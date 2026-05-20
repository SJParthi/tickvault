# Implementation Plan: QuestDB Table Cleanup — drop to 24 tables

**Status:** APPROVED
**Date:** 2026-05-20
**Approved by:** Parthiban (operator) — keep list corrected 2026-05-20:
`ticks` + 21 candle timeframe tables (1m…15m each minute, 30m, 1h, 2h,
3h, 4h, 1d) + one option-chain table + `historical_candles`. NO
`candles_1s`. Everything else dropped.

## Goal

Cut the QuestDB schema to **24 tables** — only the live data plane the
operator queries. No second-level base table, no shadow tables, no
audit/instrument/misc sprawl.

## KEEP (24)

| Table(s) | Role |
|---|---|
| `ticks` | live LTP + greeks (iv/delta/gamma/theta/vega) + day OHLC columns |
| `candles_1m` `candles_2m` `candles_3m` `candles_4m` `candles_5m` `candles_6m` `candles_7m` `candles_8m` `candles_9m` `candles_10m` `candles_11m` `candles_12m` `candles_13m` `candles_14m` `candles_15m` | 15 minute timeframes (1→15) |
| `candles_30m` `candles_1h` `candles_2h` `candles_3h` `candles_4h` `candles_1d` | 6 higher timeframes |
| `option_chain_minute_snapshot` | the one option-chain table |
| `historical_candles` | REST historical — cross-verify ground truth |

= 1 + 21 + 1 + 1 = **24**.

## Candle architecture change — NO `candles_1s` base

Today the 9 candle matviews sample from a `candles_1s` base table (a
2-tier cascade). The operator does not want `candles_1s`. Therefore:

- Each of the 21 candle timeframe tables becomes a QuestDB
  **materialized view that samples `ticks` directly**
  (`SAMPLE BY <tf>` with `first/max/min/last` OHLC + `sum` volume).
- `candles_1s` is **dropped**.
- The in-memory 29-TF cascade (`tickvault_trading::candles`), the
  `candles_1s` writer, and the 9 `candles_*_shadow` tables are all
  **removed** — superseded by direct `ticks`→matview aggregation.
- 12 sub-15m timeframes (`candles_2m,3m,4m,6m,7m,8m,9m,10m,11m,12m,13m,14m`)
  were retired in PR #517 — they are **re-created** as matviews here.

## DROP — and rip out the writer code, not just the SQL

| Group | Tables |
|---|---|
| `candles_1s` base | candles_1s |
| Candle shadow (9) | candles_1m_shadow … candles_1d_shadow |
| Audit (~20) | aggregator_seal_audit, auth_renewal_audit, bar_correction_audit, boot_audit, decision_audit, gap_fill_audit, last_tick_audit, open_price_audit, order_audit, order_update_ws_audit, orphan_position_audit, pnl_audit, self_trade_audit, selftest_audit, signal_audit, sl_replacement_audit, static_ip_audit, volume_nse_audit, ws_reconnect_audit, subscription_audit_log |
| Instrument (5) | fno_underlyings, derivative_contracts, subscribed_indices, instrument_build_metadata, index_constituents |
| Misc (6) | market_depth, previous_close, indicator_snapshots, obi_snapshots, nse_holidays, live_instance_lock |

Each dropped table also means deleting its `*_persistence.rs` module,
its boot-time DDL entry in `main.rs`, its notification-event variant(s),
its `ErrorCode` variant(s), and its ratchet tests.

## Consequences (operator-acknowledged)

1. **PR #9 cross-verify** redesigned: keeps `historical_candles` as the
   compare source, result is **Telegram-alert-only — no audit table**.
2. **`order_audit`** (SEBI 5-year order trail) dropped now (empty —
   sandbox/dry-run). HARD GATE: MUST be re-added before `dry_run=false`.

## Sequencing (serial PRs, §H — one at a time)

- [ ] **#T1** — candle re-architecture: 21 matviews sampling `ticks`
  directly (incl. re-creating the 12 sub-15m TFs); drop `candles_1s`,
  the 9 shadow tables, the in-memory 29-TF cascade + seal writer.
- [ ] **#T2** — drop ~20 audit tables + `*_audit_persistence.rs` modules
  + boot DDL + notification/ErrorCode wiring + ratchets.
- [ ] **#T3** — drop 5 instrument tables + their persistence surface.
- [ ] **#T4** — drop 6 misc tables + their persistence modules.
- [ ] **#T5** — cross-verify (PR #9) lands table-free (Telegram-only)
  against `historical_candles`.

Supersedes the original AWS-lifecycle PR #10 QuestDB-cleanup scope.

## Pre-req

PR #722 (IDX_I Quote mode) must merge to `main` first (§H serial
completion). Then #T1 starts. — **DONE: #722 merged 2026-05-20.**

## Column cleanup — audit result (folds into #T1)

Operator directive 2026-05-20: "check what columns are especially
needed for ticks / candles / historical / option chain from docs
design only — drop the rest."

Audited against `docs/dhan-ref/03-live-market-feed-websocket.md`
(Quote packet bytes 8-49 + OI packet code 5),
`docs/dhan-ref/05-historical-data.md` (columnar OHLCV) and
`docs/dhan-ref/06-option-chain.md`.

Buckets: **A = Dhan packet/REST field (KEEP)**, **B = identity /
timestamp infra (KEEP)**, **C = derived / operational EXTRA (DROP)**.

### `ticks` — 25 cols → keep 16, drop 9

| Bucket | Columns |
|---|---|
| A (Dhan Quote/OI packet) | `ltp` `open` `high` `low` `close` `volume` `oi` `avg_price` `last_trade_qty` `total_buy_qty` `total_sell_qty` |
| B (infra) | `segment` `security_id` `exchange_timestamp` `ts` `received_at` |
| **C — DROP** | `iv` `delta` `gamma` `theta` `vega` `volume_delta` `prev_day_close` `prev_day_oi` `phase` |

`prev_day_close` is redundant — Quote bytes 38-41 (`close`) already
IS previous-day close per Ticket #5525125. Greeks: indices-only
universe never carries options on the main feed → the 5 greek
columns stay empty; greeks live in `option_chain_minute_snapshot`.

### Candle TF tables (new matviews over `ticks`) — project 10 cols

| Bucket | Columns |
|---|---|
| A (OHLCV) | `open`=first `high`=max `low`=min `close`=last `volume`=sum `oi`=last |
| B (infra) | `segment` `security_id` `ts` |
| keep (verify) | `tick_count`=count — data-quality / cross-verify |
| **C — DROP** | `iv` `delta` `gamma` `theta` `vega` `volume_cum_day_at_end` `prev_day_close` `prev_day_oi` `phase` `close_pct_from_prev_day` `oi_pct_from_prev_day` `volume_pct_from_prev_day` |

### `historical_candles` — 11 cols → keep 10, drop 1

| Bucket | Columns |
|---|---|
| A (Dhan REST OHLCV) | `open` `high` `low` `close` `volume` `oi` |
| B (infra) | `segment` `timeframe` `security_id` `ts` |
| **C — DROP** | `candles_source` |

### `option_chain_minute_snapshot` — 26 cols → keep 24, drop 2

| Bucket | Columns |
|---|---|
| A (Dhan option-chain) | `strike` `ce_or_pe` `last_price` `average_price` `oi` `previous_oi` `volume` `previous_volume` `previous_close_price` `top_bid_price` `top_bid_quantity` `top_ask_price` `top_ask_quantity` `implied_volatility` `greek_delta` `greek_theta` `greek_gamma` `greek_vega` |
| B (infra) | `ts` `trading_date_ist` `underlying_symbol` `underlying_security_id` `exchange_segment` `security_id` |
| **C — DROP** | `cache_age_used_secs` `fetch_outcome` |

### Open decision (operator must confirm)

The 5 greek columns on `ticks` + candle tables: earlier the operator
said "track ltp and all greeks". But under the indices-only locked
universe NO options ride the main feed, so those columns are
always empty. Greeks of options ARE tracked in
`option_chain_minute_snapshot`. Recommendation: DROP greeks from
`ticks` + candles; keep greeks ONLY in the option-chain table.
Needs explicit operator yes before the column-drop lands in #T1.

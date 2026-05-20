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
completion). Then #T1 starts.

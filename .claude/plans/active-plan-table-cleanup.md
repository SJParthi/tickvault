# Implementation Plan: QuestDB Table Cleanup — drop to 24 tables

**Status:** APPROVED
**Date:** 2026-05-20
**Approved by:** Parthiban (operator) — keep list corrected 2026-05-20:
`ticks` + 21 candle timeframe tables (1m…15m each minute, 30m, 1h, 2h,
3h, 4h, 1d) + one option-chain table + `historical_candles`. NO
`candles_1s`. Everything else dropped.

## Per-Item Guarantee Matrix

Every item in this plan (#T1–#T5) is bound by the canonical 15-row
"100% everything" matrix and the 7-row resilience demand matrix — see
`.claude/rules/project/per-wave-guarantee-matrix.md`. All 15 + 7 rows
apply to every #T item. Each lands only inside the tested envelope,
with ratcheted regression coverage: compile-verified, ratchet-guarded,
workspace + scoped test suites green before merge.

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

## Candle architecture — RAM-FIRST (operator-locked 2026-05-20)

> **Operator lock 2026-05-20 (verbatim):** "keep everything in RAM
> memory and periodically flush to db ... for trading decision nowhere
> we can tolerate ... we cannot touch db and db is only for replay and
> historical purpose."
>
> This supersedes the earlier (WRONG) matview proposal. QuestDB
> materialized views push the candle aggregation **into the database**
> — that is a DB-on-hot-path violation of `aws-budget.md` rule 12
> (RAM-first) and operator-charter §C. **NO materialized views. EVER.**

The candle architecture is RAM-first:

| Stage | Where | Detail |
|---|---|---|
| Tick → live bar update | **RAM** | the Rust in-memory multi-timeframe aggregator updates the open bar for all 21 timeframes per tick — O(1), zero-alloc |
| Bar seal at TF boundary | **RAM** | sealed bar stays in the RAM bar cache (today + yesterday) AND is queued for async flush |
| Periodic flush | **async cold path** | a background task ILP-writes sealed bars to the 21 plain QuestDB candle tables |
| Trading decision / indicator read | **RAM ONLY** | reads the RAM bar cache — **never** `SELECT` from QuestDB |
| Replay / cross-verify / dashboards | QuestDB (cold) | the only consumers of the QuestDB candle tables |

Therefore:

- The 21 QuestDB candle tables are **plain `CREATE TABLE` tables** —
  the flush sink. **NOT** materialized views.
- The **in-memory multi-timeframe aggregator is KEPT** as the RAM
  source of truth — trimmed/extended to exactly the 21 TFs. (Earlier
  draft said "remove the in-memory cascade" — that was wrong; it is
  the heart of the RAM-first design and stays.)
- `candles_1s` is **dropped** — no QuestDB base table, no QuestDB-side
  aggregation, no cascade-over-QuestDB.
- The 9 `candles_*_shadow` tables are **dropped** — the 21 plain
  candle tables ARE the flush targets now, no separate shadow layer.
- 12 sub-15m timeframes (`candles_2m,3m,4m,6m,7m,8m,9m,10m,11m,12m,13m,14m`)
  retired in PR #517 — re-created here as RAM aggregator timeframes +
  plain flush tables.

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

- [x] **#T1** — candle re-architecture (RAM-first): 21 **plain**
  QuestDB candle tables fed by the in-memory aggregator's periodic
  async flush; KEEP + extend the RAM aggregator to the 21 TFs (incl.
  re-adding the 12 sub-15m TFs); drop `candles_1s` + the 9 shadow
  tables + any QuestDB matview chain. NO materialized views.
  Split into 3 serial sub-PRs (per `stream-resilience.md` — >3 crates):
  - [x] **#T1a** — storage+engine foundation: `TfIndex`/aggregator
    extended 9→21 TFs; 21 plain `candles_<tf>` table DDL (10-col, no
    pct, `segment` column, `DEDUP UPSERT KEYS(ts, security_id, segment)`);
    seal-writer chain re-pointed to the plain tables. Old Engine A
    (`candles_1s` + matviews) + Engine C (cascade) still run in
    parallel — nothing deleted. Workspace compiles green. (PR #731;
    405-DDL hotfix PR #733.)
  - [x] **#T1c** — column cleanup: dropped the 9 `ticks` bucket-C
    columns (`iv/delta/gamma/theta/vega`, `volume_delta`,
    `prev_day_close`, `prev_day_oi`, `phase`) via boot-time
    `ALTER ... DROP COLUMN IF EXISTS` self-heal. The prev-day /
    volume-delta / phase enrichment was KEPT — it is not dead (still
    feeds the live VOLUME-MONO-01 gate + the shared `prev_oi_cache`);
    its teardown is flagged for a separate focused PR. (this PR.)
  - [x] **#T1b** — cutover: minute/IST-midnight boundary force-seal
    task; delete Engine A (`candles_1s` + matviews) + Engine C
    (cascade); `main.rs` rewire; re-point `cross_verify` +
    `post_open_cross_check` to the plain tables; drop-legacy DDL.
    (PR #734.)
- [x] **#T2** — drop ~20 audit tables + `*_audit_persistence.rs` modules
  + boot DDL + notification/ErrorCode wiring + ratchets.
  (#T2a PR #737 — 10 dead modules; #T2b PR #738 — 9 wired modules.)
- [x] **#T3** — drop 5 instrument tables + their persistence surface.
  (PR #739.)
- [x] **#T4** — drop 6 misc tables + their persistence modules.
  (market_depth, previous_close, indicator_snapshots, obi_snapshots,
  nse_holidays, live_instance_lock — modules deleted, callers rewired,
  boot DDL trimmed.)
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

### Candle TF tables (plain tables, flushed from the RAM aggregator) — 10 cols

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

### Decision — RESOLVED (operator approved 2026-05-20)

Operator verbatim 2026-05-20: "yes go ahead dude with the table
cleanup dude and even columns drop also dude."

- Greeks columns (`iv` `delta` `gamma` `theta` `vega`): **DROP** from
  `ticks` + all candle tables. Confirmed — they are always empty under
  the 4-IDX_I locked universe (no options on the main feed). Option
  greeks remain in `option_chain_minute_snapshot`.
- Full column cleanup (every bucket-C column in the four tables above):
  **APPROVED** — folds into the relevant #T-PR.
- Table cleanup #T1–#T5: **APPROVED** — execute serially per §H.

No open decisions remain. The plan is fully unblocked.

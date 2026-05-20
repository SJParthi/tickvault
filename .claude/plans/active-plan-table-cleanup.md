# Implementation Plan: QuestDB Table Cleanup — drop to 13 tables

**Status:** APPROVED
**Date:** 2026-05-20
**Approved by:** Parthiban (operator) — verbatim: "remove the entire fucking
tables except ticks + all candle timeframe tables + one option chain table",
keep/drop list approved 2026-05-20 with `historical_candles` retained.

## Goal

Cut the QuestDB schema from ~37 tables to **13**. The operator finds the
table sprawl unmanageable; only the live data plane is wanted.

## KEEP (13)

| Table | Role |
|---|---|
| `ticks` | live LTP + greeks (iv/delta/gamma/theta/vega) + day OHLC columns |
| `candles_1s` | base candle table |
| `candles_1m` `candles_5m` `candles_15m` `candles_30m` `candles_1h` `candles_2h` `candles_3h` `candles_4h` `candles_1d` | 9 live candle timeframes |
| `option_chain_minute_snapshot` | the one option-chain table |
| `historical_candles` | REST historical — cross-verify ground truth |

## DROP (~31) — and rip out the writer code, not just the SQL

| Group | Tables |
|---|---|
| Audit (~20) | aggregator_seal_audit, auth_renewal_audit, bar_correction_audit, boot_audit, decision_audit, gap_fill_audit, last_tick_audit, open_price_audit, order_audit, order_update_ws_audit, orphan_position_audit, pnl_audit, self_trade_audit, selftest_audit, signal_audit, sl_replacement_audit, static_ip_audit, volume_nse_audit, ws_reconnect_audit, subscription_audit_log |
| Instrument (5) | fno_underlyings, derivative_contracts, subscribed_indices, instrument_build_metadata, index_constituents |
| Candle shadow (9) | candles_1m_shadow … candles_1d_shadow (Wave-6 internal scaffolding) |
| Misc (6) | market_depth, previous_close, indicator_snapshots, obi_snapshots, nse_holidays, live_instance_lock |

Each dropped table also means deleting its `*_persistence.rs` module, its
boot-time DDL `tokio::join!` entry in `main.rs`, its notification-event
variant(s), its `ErrorCode` AUDIT-NN variant(s), and its ratchet tests.

## Consequences (operator-acknowledged)

1. **PR #9 cross-verify** redesigned: keeps `historical_candles` as the
   compare source, but the result is **Telegram-alert-only — no
   `cross_verify_audit` table**.
2. **`order_audit`** (SEBI 5-year order trail) is dropped now (empty —
   sandbox/dry-run). HARD GATE: it MUST be re-added before `dry_run=false`
   / live trading. Tracked here so go-live cannot forget it.

## Sequencing (serial PRs, §H — one at a time)

This is too large for one PR. Execute as sequenced sub-PRs:

- [ ] **#T1** — drop the 9 `candles_*_shadow` tables + Wave-6 seal writer
  (`shadow_persistence.rs`, `shadow_candle_writer.rs`, seal_writer_loop,
  seal_ring, cascade shadow wiring, boot wiring, ratchets).
- [ ] **#T2** — drop the ~20 audit tables + their `*_audit_persistence.rs`
  modules + boot DDL + notification/ErrorCode wiring + ratchets.
- [ ] **#T3** — drop the 5 instrument tables + `instrument_persistence.rs`
  surface for them + boot DDL.
- [ ] **#T4** — drop the 6 misc tables (market_depth, previous_close,
  indicator_snapshots, obi_snapshots, nse_holidays, live_instance_lock)
  + their persistence modules + boot wiring.
- [ ] **#T5** — cross-verify (PR #9) lands table-free (Telegram-only)
  against `historical_candles`.

Net effect: supersedes the original AWS-lifecycle PR #10 QuestDB-cleanup
scope and folds it in.

## Pre-req

PR #722 (IDX_I Quote mode) must merge to `main` first (§H serial
completion). Then #T1 starts.

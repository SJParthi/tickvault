# Phase 0 — Lean Lock & Forensic Audit Foundation

> **Status:** IN PROGRESS (23 items merged; ~15 items remaining)
> **Authority:** `CLAUDE.md` > `.claude/rules/project/operator-charter-forever.md` > this file
> **Companion plan:** `.claude/plans/friday-may-15-mega/topic-PHASE-0-LEAN-LOCKED.md` (item-by-item status)
> **Companion architecture:** `.claude/rules/project/phase-0-architecture.md` (Item 27h, pending)

## What Phase 0 is

Phase 0 is the **Lean Lock + Forensic Audit foundation** of tickvault — the
work needed to make the system production-safe for live SEBI-regulated F&O
trading on a single Dhan account. It locks the trading universe scope to
indices + their underlyings (no full F&O chain), pins down a 60s WebSocket
budget per account, ensures every safety-critical lifecycle event lands in
a per-table SEBI-retentioned QuestDB audit row, and adds the defensive
guards that prevent silent corruption from poisoning downstream
aggregations.

Phase 0 is **NOT live trading.** Strategies are dry-run by default
(`config.strategy.dry_run = true`). The OMS, risk engine, P&L tracker,
and SL-replacement gate all exist in compute-only scaffolding form so
the audit chains can be exercised end-to-end before live capital flips
on in Phase 1.

## What Phase 0 is NOT

  * Not a refactor of `Cargo.toml` workspace deps — see `cargo-and-docker.md`.
  * Not a feature for new instrument types — index F&O + cash equities only.
  * Not a perf optimisation — DHAT zero-alloc + Criterion budgets exist
    independently in the `quality/` tree.
  * Not a UI / dashboard PR — operator-health Grafana dashboards are
    Wave 9 territory.

## The 27 plan items (lean-locked)

The canonical item list lives in
`.claude/plans/friday-may-15-mega/topic-PHASE-0-LEAN-LOCKED.md` with
inline `[x]` / `[ ]` checkboxes. This README is the merged-PR-by-PR
record so future Claude sessions can `grep` for what's done without
opening the plan file.

### DONE — merged this iteration (23 PRs)

| Item | Title | PR | What it ships |
|---|---|---|---|
| 13 | Pre-open buffer → 09:15 candle.open | #619 | Open-price source-selection primitives in `crates/common/src/open_price_source.rs` |
| 14 | REST `/marketfeed/quote.day_open` fallback at 09:14:55 IST | (commit 6e9b4ce) | Pre-open REST fallback for stocks missing from buffer |
| (test fix) | Test regression fix | #626 | One-off test fix |
| 18 | GetIpResponse extension | #627 | Static-IP boot-gate Dhan response parsing |
| 18 | Static IP boot gate | #628 | Boot-time `/v2/ip/getIP` check refuses boot if Dhan reports `ordersAllowed=false` |
| 18b | Static IP retry loop | #629 | 30 retries × 60s = 30 min propagation window before halt |
| 18c | `static_ip_audit` QuestDB table | #630 | Audit row per boot-gate outcome (Pass/Fail) |
| 19a | Instance-lock primitives | #631 | `host_id` + `lock_key` pure-function helpers |
| 19b | Async lock primitives + ErrorCode | #632 | `try_acquire`/`renew`/`release`/`AcquireOutcome` + `Resilience01DualInstanceDetected` ErrorCode |
| 19c | Heartbeat task + DualInstanceDetected event | #633 | 30s heartbeat task + Telegram event variant |
| 19d | Boot-gate wiring | #634 | Step 6a-prime in `main.rs` — refuses boot if another tickvault holds lock |
| 19e | `live_instance_lock` audit table + 3 boot INSERTs | #635 | Audit table + Acquired/AlreadyHeld/ValkeyError boot-time outcomes |
| 19f | Heartbeat audit + chained graceful shutdown | #636 | LostOwnership + GracefulRelease audit rows + shutdown-chain bridge |
| 22d | End-of-day Telegram digest at 15:31:30 IST | #637 | Once-per-day positive ping with main_feed status + token headroom |
| 21 | NaN/Inf indicator-snapshot guard | #638 | `IndicatorSnapshot::sanitize_nan_inf()` + `tv_indicator_nan_guard_fired_total` counter |
| 17a | `auth_renewal_audit` QuestDB table | #639 | Audit table for SEBI 24h JWT renewal lifecycle |
| 22e | Tick-level OHLC integrity guards | #640 | `check_tick_ohlc_integrity()` + 3 violation counters |
| 17b | TokenManager wiring for auth_renewal_audit | #641 | Success/Failed/CircuitBreakerHalt audit-row INSERTs |
| 16a | `open_price_audit` QuestDB table | #642 | Audit table for 09:15 candle's open-price source decision |
| 22a | DH-904 backoff ladder + 5s timeout pin | #643 | `compute_dh904_backoff()` + `DH904_BACKOFF_SECS` 10/20/40/80s ladder |
| 24a | `order_update_ws_audit` QuestDB table | #644 | Audit table for live order-update WebSocket events with `Source='P'` filter |
| 22f-a | `self_trade_audit` QuestDB table | #645 | SEBI wash-trade prevention 60s cooldown audit + `SELF_TRADE_COOLDOWN_SECS` constant |
| 22b-a | `sl_replacement_audit` QuestDB table | #646 | Stop-loss-leg-cancelled auto-replace forensic table (Detected/Replaced/FailedToReplace) |
| 25-a | `pnl_audit` QuestDB table (dry-run scaffolding) | #647 | P&L tracker per-instrument audit (OnFill/OnMinute/OnEod) |

### REMAINING — not merged yet

  * **8** — Gap-fill scheduler `/charts/intraday` seal-then-fetch (bar_end + 5s buffer)
  * **9** — Gap-fill DEDUP UPSERT into `candles_1m` + RAM bar cache + `gap_fill_audit`
  * **10** — Disconnect-chain 7-layer observability (5 counters + 2 gauges + 2 audit + 5 Telegram + ratchets)
  * **11** — Dual-gate market-hours fix (G1 exchange_ts + G2 wall-clock 60s grace)
  * **12** — `last_tick_audit` Telegram + `LastTickAfterBoundary` event + banned-pattern hook (persistence module already shipped, wiring pending)
  * **15** — 09:16:05 cross-check vs Dhan `/charts/intraday` 09:15 bar
  * **16b** — Open-price aggregator wiring (calls `append_open_price_audit_row` from the 09:15 seal path)
  * **17c** — `force_renewal` + `force_renewal_if_stale` audit-row wiring (optional follow-up to 17b)
  * **20** — Orphan position 15:25 IST watchdog (risk engine)
  * **22a-wire** — Engine integration of DH-904 backoff at the 7+ order-API call sites
  * **22b-b** — OMS wiring + `SlLegFailedToReplace` Critical Telegram variant
  * **22c** — Boot-success Telegram positive ping (already covered by 09:14 readiness; verification only)
  * **22f-b** — OMS pre-trade-gate wiring + `SelfTradeCooldownBlocked` Telegram variant
  * **23** — Prev-close mode mix + REST boot fetch + `PrevCloseMissingAtMarketOpen` + banned-pattern hook
  * **24b** — Order-update WS aggregator wiring + Source='N' Telegram + 7-layer observability
  * **25-b** — OMS engine OnFill/OnMinute/OnEod audit-row INSERTs
  * **25-c** — Daily-loss-threshold Telegram variant + Prom alert rule
  * **26** — `signal_audit` + `decision_audit` QuestDB tables (sampled)
  * **27b** — `docs/runbooks/aws-deployment.md`
  * **27c** — `docs/runbooks/operator-daily-startup.md`
  * **27d** — `docs/runbooks/troubleshooting.md`
  * **27e** — `docs/runbooks/kill-switch.md`
  * **27f** — `docs/runbooks/backtest-runner.md`
  * **27g** — `docs/runbooks/phase-1-monitoring-rubric.md`
  * **27h** — `.claude/rules/project/phase-0-architecture.md`

## The audit-table family — SEBI 5-year retention

Phase 0 ships **11 SEBI-retentioned QuestDB audit tables** that together
form the forensic chain regulators expect when reconstructing a session.
Every table follows the same template (see `static_ip_audit_persistence.rs`
for the canonical reference):

  * `CREATE TABLE IF NOT EXISTS` — idempotent boot-time DDL
  * Composite DEDUP UPSERT KEYS including `trading_date_ist` + the
    table's identity columns + (where applicable) an `outcome`
    discriminator so lifecycle chains keep both rows
  * Nanos-to-micros conversion at INSERT time (2026-04-28
    regression-class fix pinned by source-scan tests)
  * Typed wire-format enums for SYMBOL columns (no string typos)
  * Per-row schema-drift ratchet test pinning every column declaration
  * `sanitize_audit_string` on every STRING column for SQL-injection safety

| # | Table | Item | Purpose |
|---|---|---|---|
| 1 | `static_ip_audit` | 18c | Boot-time Dhan `/v2/ip/getIP` outcome |
| 2 | `live_instance_lock` | 19e | Dual-instance SSM lock lifecycle (was Valkey before #O4) |
| 3 | `auth_renewal_audit` | 17a | SEBI 24h JWT renewal lifecycle |
| 4 | `open_price_audit` | 16a | 09:15 candle's open-price source decision |
| 5 | `order_update_ws_audit` | 24a | Live order-update WS events with Source filter |
| 6 | `self_trade_audit` | 22f-a | Wash-trade prevention 60s cooldown lifecycle |
| 7 | `sl_replacement_audit` | 22b-a | Stop-loss-leg-cancelled auto-replace lifecycle |
| 8 | `pnl_audit` | 25-a | Per-instrument realized + unrealized P&L snapshots |
| 9 | `phase2_audit` (Wave 2) | (pre-Phase-0) | Phase 2 dispatch outcomes |
| 10 | `boot_audit` (Wave 2) | (pre-Phase-0) | Boot lifecycle outcomes |
| 11 | `selftest_audit` (Wave 2) | (pre-Phase-0) | Market-open self-test outcomes |

Plus the supporting non-audit but SEBI-relevant tables: `order_audit`
(strict 5y SEBI mandate), `ws_reconnect_audit`, `depth_rebalance_audit`,
`depth_dynamic_diff_audit`, `gap_fill_audit`, `aggregator_seal_audit`,
`last_tick_audit`, `volume_nse_audit`.

## Defensive guards shipped (non-audit)

  * **Item 21** — `IndicatorSnapshot::sanitize_nan_inf()` clamps NaN/Inf
    in 20 f64 fields, increments `tv_indicator_nan_guard_fired_total`.
  * **Item 22a** — `compute_dh904_backoff(attempts) -> Option<Duration>`
    pure-function exponential ladder (10s → 20s → 40s → 80s).
  * **Item 22e** — `check_tick_ohlc_integrity(ltp, high, low) -> u8`
    bitmask of 3 OHLC invariant violations + 3 static-label counters.
  * **Item 18 + 19** — Boot-gate refusals: static IP not whitelisted
    OR another tickvault process holds the SSM lock → boot HALTS
    with operator-readable Telegram + the failure persists to the
    relevant audit table.

## Cross-references

  * Operator charter: `.claude/rules/project/operator-charter-forever.md`
  * 22-test taxonomy: `.claude/rules/project/testing.md`
  * Live-feed purity: `.claude/rules/project/live-feed-purity.md`
  * Security-id uniqueness (I-P1-11): `.claude/rules/project/security-id-uniqueness.md`
  * Audit findings 2026-04-17: `.claude/rules/project/audit-findings-2026-04-17.md`
  * Hot-path zero-alloc: `.claude/rules/project/hot-path.md`
  * Phase 1 spec: `docs/phases/phase-1-live-trading.md`

## How to add a new audit table (template)

  1. Copy `crates/storage/src/static_ip_audit_persistence.rs` to
     `crates/storage/src/<your_audit_name>_persistence.rs`.
  2. Replace the `QUESTDB_TABLE_*` constant + `DEDUP_KEY_*` constant
     + outcome enum + DDL columns + INSERT params.
  3. Add the module to `crates/storage/src/lib.rs` (alphabetised).
  4. Wire `ensure_<your>_audit_table` into the boot-time `tokio::join!`
     block in `crates/app/src/main.rs` next to its siblings.
  5. Wire `append_<your>_audit_row` into the producing code path.
  6. Ship 12-16 unit tests covering: table-name constant, outcome
     wire-format stability, DEDUP composition (with regression guards
     for designated-timestamp + outcome-in-key + I-P1-11 composite),
     nanos→micros source-scan ratchet, DDL column-set ratchet,
     distinct labels invariant, smoke-pin for the pub fn, and 2-3
     `#[tokio::test]` error-path covering each lifecycle outcome.

## How to verify Phase 0 status

```bash
# All Phase 0 audit-table modules present
ls crates/storage/src/*_audit_persistence.rs

# All Phase 0 tests passing (scoped — see testing-scope.md)
cargo test -p tickvault-storage --lib audit
cargo test -p tickvault-core --lib auth::secret_manager
cargo test -p tickvault-app --lib

# All meta-guards green (source-scan invariants)
cargo test -p tickvault-core --lib auth::secret_manager::tests::test_

# Plan checkbox status
grep -E '^- \[[ x]\]' .claude/plans/friday-may-15-mega/topic-PHASE-0-LEAN-LOCKED.md
```

## Phase 1 prerequisites

Before flipping `dry_run = false` and going live in Phase 1:

  1. All Phase 0 items checked ✅ in
     `topic-PHASE-0-LEAN-LOCKED.md`.
  2. Item 18 (static IP) + Item 19 (instance lock) boot gates BOTH
     pass on the AWS production instance (not just dev Mac).
  3. All 11 audit tables exist in production QuestDB
     (`make doctor` confirms).
  4. Items 22a-wire / 22b-b / 22f-b OMS engine integrations all
     ratcheted by the source-scan meta-guards in
     `crates/core/src/auth/secret_manager.rs::tests`.
  5. Item 25-b/c P&L scaffolding promoted from dry-run to live
     accumulator.
  6. 27a-h documentation chain complete (operator can answer
     "what does this audit table mean?" without source diving).

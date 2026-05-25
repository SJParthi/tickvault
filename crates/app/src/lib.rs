//! Library target for the tickvault application.
//!
//! Exposes infrastructure, observability, and trading pipeline modules
//! so that `cargo llvm-cov` can instrument them through the lib target.
//! The binary entry point (`main.rs`) re-exports these via `use crate::`.

#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![deny(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]
// Phase 0.2: no dropped Result/JoinHandle/must-use values (silent error swallowing).
#![cfg_attr(not(test), deny(unused_must_use))]
#![cfg_attr(not(test), warn(clippy::let_underscore_must_use))]
#![cfg_attr(test, allow(clippy::assertions_on_constants))]
#![cfg_attr(test, allow(clippy::field_reassign_with_default))]

pub mod bar_cache_loader;
// PR #8a (2026-05-19) — Slice 1: 09:15:00 IST `DayOhlcTracker::arm_sid()`
// boot wiring per `index-day-ohlc-tracker-error-codes.md`. Closes the
// operator-locked pre-open equilibrium open-price gap.
pub mod day_ohlc_orchestrator;
// PR #6a (2026-05-19): bhavcopy_pipeline DELETED — 16:30 IST NSE bhavcopy
// cross-check retired under 4-IDX_I LOCKED_UNIVERSE (operator lock 2026-05-15).
// Bhavcopy is NSE_FNO-only; no F&O subscriptions remain to cross-check.
pub mod boot_helpers;
pub mod core_pinning;
// PR #4 (2026-05-19): depth-20 / depth-200 modules DELETED (operator-locked
// per websocket-connection-scope-lock.md — 4-IDX_I uses 1 main-feed conn
// + 1 order-update conn only).
// PR #3 (2026-05-19): `greeks_pipeline` module DELETED. Greeks
// pipeline retired alongside the indices-only universe. Option Chain
// REST overlay (PR #8) ships Dhan-computed greeks separately.
pub mod infra;
// 2026-05-09 PR 5c.5-final (Bug 3 — movers retirement): the
// `movers_pipeline` orchestrator is DELETED. Operator directive:
// "only ticks and our 9 needed candle timeframes are available".
// The 25 `movers_*` matviews + `movers_1s` base table are dropped at
// boot by `materialized_views::drop_bug3_retired_views`.
// PR #454 (2026-05-03): boot-time prev_oi cache loader. Wires
// bhavcopy → cache extraction primitives into the boot path so the
// in-memory cache (consumed by the cascade seal-time pct-stamping
// path + tick enricher) is Dhan-precise from the first tick.
pub mod prev_oi_loader;
// F2 (Wave-5 #504e follow-up) — boot-time loader for `PrevDayCache`
// so the cascade seal-time pct-stamping path (PR #520 / F1) sees
// non-zero `prev_day_close` values from QuestDB's `previous_close`
// table on cold boot.
pub mod metrics_catalog;
pub mod observability;
// 2026-05-25 — Crash-recovery REHYDRATE of the option-chain current-
// expiry cache from QuestDB's `option_chain_minute_snapshot` table.
// See `option_chain_cache_loader.rs` module docs for the 3-tier
// recovery contract.
pub mod option_chain_cache_loader;
pub mod subsystem_memory;
pub mod trading_pipeline;

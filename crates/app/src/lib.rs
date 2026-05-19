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
pub mod bhavcopy_pipeline;
pub mod boot_helpers;
pub mod boot_smoke_test;
pub mod core_pinning;
pub mod depth_20_conn_spawner;
pub mod depth_20_single_side_planner;
pub mod depth_bridge_state_writer;
pub mod depth_dynamic_pipeline;
pub mod depth_dynamic_pipeline_v2;
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
pub mod phase2_recovery;
pub mod prev_day_cache_loader;
pub mod subsystem_memory;
pub mod trading_pipeline;

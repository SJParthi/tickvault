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

pub mod bhavcopy_pipeline;
pub mod boot_helpers;
pub mod boot_smoke_test;
pub mod core_pinning;
pub mod depth_20_conn_spawner;
pub mod depth_20_single_side_planner;
pub mod depth_bridge_state_writer;
pub mod depth_dynamic_pipeline;
pub mod depth_dynamic_pipeline_v2;
pub mod greeks_pipeline;
pub mod infra;
pub mod movers_base_pipeline;
// movers_v2_pipeline DELETED in PR #450 commit 6 — V2 in-memory tracker
// superseded by canonical movers_1s + 25 mat views populated via
// movers_base_pipeline.
pub mod observability;
pub mod phase2_recovery;
pub mod trading_pipeline;

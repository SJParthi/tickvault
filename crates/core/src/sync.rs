//! Atomic types that the loom model checker can see.
//!
//! With the crate's non-default `loom` feature these are loom's atomics, so a
//! `loom::model` test drives the REAL production code through every
//! interleaving. Without it they are the std atomics, unchanged: no cost and
//! no behaviour change in a normal build.
//!
//! Audit H3 (2026-10-05, finished 2026-10-06): the core loom tests modelled
//! hand-written copies of the code, so a change to the real orderings could
//! not fail them. Three production structures now take their atomics from
//! here, and each has a loom file that drives the real methods:
//!
//! | Structure | Loom file |
//! |---|---|
//! | `websocket::pool_supervisor::GhostRegister` | `tests/loom_ghost_register.rs` |
//! | `websocket::pool_supervisor::RingByteBudget` | `tests/loom_ws_decoupling.rs` |
//! | `websocket::activity_watchdog::{ProgressProbe, note_activity}` and the counter it reads | `tests/loom_activity_watchdog.rs` |
//!
//! Everything else in the crate keeps the std atomics.
//!
//! The `loom` feature is enabled only by the loom CI lane, and only together
//! with `--test loom_*` targets: loom atomics panic outside `loom::model`, so
//! the crate's ordinary unit tests must never run with it.

#[cfg(feature = "loom")]
pub(crate) use loom::sync::atomic::{
    AtomicBool, AtomicI64, AtomicU8, AtomicU32, AtomicU64, AtomicUsize,
};

#[cfg(not(feature = "loom"))]
pub(crate) use std::sync::atomic::{
    AtomicBool, AtomicI64, AtomicU8, AtomicU32, AtomicU64, AtomicUsize,
};

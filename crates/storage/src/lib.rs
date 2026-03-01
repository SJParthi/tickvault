// Compile-time lint enforcement — defense-in-depth with CLI clippy flags
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]
#![deny(clippy::dbg_macro)]
#![warn(missing_docs)]

//! Data persistence layer — QuestDB for time-series, Valkey for caching.
//!
//! # Modules
//! - `instrument_persistence` — daily instrument snapshot to QuestDB (Block 01.1)
//!
//! # Key Modules (to be built)
//! - `questdb_writer` — ILP ingestion for tick data, SQL for orders
//! - `valkey_cache` — deadpool connection pool, state caching
//! - `recovery` — memmap2-based crash recovery
//!
//! # Boot Sequence Position
//! OMS -> **QuestDB -> Valkey** -> HTTP API

pub mod instrument_persistence;
pub mod tick_persistence;

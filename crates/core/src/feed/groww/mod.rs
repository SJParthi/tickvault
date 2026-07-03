//! Groww second feed (native Rust).
//!
//! Build order (per `.claude/plans/active-plan-groww-second-feed.md`):
//! - **PR-2 (this):** [`auth`] — obtain a Groww access token from the
//!   `api_key` + a rotating TOTP code (the prerequisite for the live feed).
//! - Later PRs: NATS connect + subscribe, protobuf tick decode → the same
//!   WAL/ring/spill/DLQ chain, 1-minute aggregation → the shared `candles_1m`
//!   table (tagged `feed='groww'`),
//!   and the live-1m vs Groww-backtest-1m exact parity check.
//!
//! Default OFF — nothing here runs unless `feeds.groww_enabled` is set.
//! Reference: `docs/groww-ref/`.

pub mod auth;
pub mod instruments;
pub mod nats;
pub mod nkey;
pub mod parity_1m;
pub mod proto;
pub mod shard_cutter;
pub mod shared_master_writer;
pub mod subjects;

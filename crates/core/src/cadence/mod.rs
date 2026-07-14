//! Broker-agnostic fetch-cadence + decision-timing scheduler (operator
//! cadence directive 2026-07-14, judge-locked design rev-8 FINAL).
//!
//! Per trading-day minute-close instant T in `[09:16:00, 15:30:00]` IST:
//! Dhan pre-fires its 3 serialized option chains at :55/:58/:02 (rate-gated
//! 1-per-3s per-underlying AND globally) and fetches 4 post-close intraday
//! spot singles at :03.0/:03.4/:03.8/:04.2 (400ms gate); Groww bursts all 7
//! requests (3 chains + 4 spots) in parallel at :00 (gate-free lane) with
//! an :00.8 verdict + sequential fallback. Each lane's decision fires the
//! INSTANT its data-complete predicate flips (event-driven, latched
//! exactly-once per (lane, cycle)); past its cutoff the lane HONEST-SKIPS
//! with a coded loud log — never a late decision, never a decision on
//! missing/stale data.
//!
//! # Modules
//! - `schedule` — pure IST minute-boundary + slot-table calculus (per
//!   broker anchor + ladder rung; boundary math reimplemented here — core
//!   cannot depend on `crates/app`; drift is pinned by mirrored vectors +
//!   const-asserts against the shared `SPOT_1M_REST_*` window constants)
//! - `gate` — pure CAS [`gate::MinSpacingGate`] in the MONOTONIC time
//!   domain (injected clock) — the structural zero-429 hard floor
//! - `ladder` — the Dhan failure ladder (rungs 0..=5, next-cycle anchor
//!   shift, step-back-one recovery) + in-cycle retry policy
//! - `executor` — the LOCKED [`executor::CadenceExecutor`] seam (this PR
//!   ships NO REST caller; [`executor::DryRunLoggingExecutor`] logs fires
//!   and returns `Err(Empty)` — never synthesizes prices)
//! - `assembly` — per-cycle per-lane data store, the data-complete
//!   predicate (3 chains + 3 underlying spots; VIX advisory), cross-source
//!   fill + provenance, and the inline moneyness resolution (consumes
//!   `tickvault_common::moneyness` — ZERO re-implemented math)
//! - `decision` — the per-lane cycle FSM, the exactly-once decision latch,
//!   [`decision::DecisionSnapshot`] and the dry-run log sink
//! - `runner` — the ONE supervised tokio task driving the slots through
//!   the gates (sleep-to-event `select!` loop; no polling tick)
//!
//! # Visibility note (honest deviation from the design's `pub(crate)`
//! markings)
//! The structural zero-429 REPLAY PROOF lives in the integration test
//! `crates/core/tests/cadence_zero_429_replay.rs`, which can only see
//! `pub` items — so `schedule` / `gate` / `ladder` / `assembly` /
//! `decision` are `pub` INTERNAL modules (not a stable API; the only
//! intended external consumers are the replay proof, the DHAT ratchet and
//! `crates/app`'s boot wiring).
//!
//! # Governance
//! Config-gated DEFAULT-OFF (`[cadence] enabled = false`), dry-run log
//! sink only, NO live orders, NO strategy wiring (§28 boundary), no new
//! WebSocket, and the existing per-minute RECORD-capture legs are
//! UNTOUCHED (this runner never publishes to their `minute_done_tx` watch
//! channels). HARD MERGE DEPENDENCY: after PR #1540
//! (`tickvault_common::moneyness` + `pipeline::chain_snapshot`).

pub mod assembly;
pub mod decision;
pub mod executor;
pub mod gate;
pub mod ladder;
pub mod runner;
pub mod schedule;

pub use executor::{
    CadenceExecutor, CadenceFetchError, ChainFetchOk, ChainFetchRequest, DryRunLoggingExecutor,
    SpotFetchRequest, SpotSnapshot, SpotTarget,
};
pub use runner::{CadenceRunnerDeps, SystemCadenceClock, spawn_supervised_cadence_runner};

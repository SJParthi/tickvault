//! Broker-agnostic fetch-cadence + decision-timing scheduler (operator
//! cadence directive 2026-07-14, judge-locked design rev-8 FINAL;
//! reshaped POST-CLOSE by the operator directive 2026-07-16).
//!
//! Per trading-day minute-close instant T in `[09:16:00, 15:30:00]` IST:
//! Dhan fires POST-CLOSE per its SHAPE ladder (2026-07-16 + the
//! same-day all-7 correction — rung 0 primary: ALL 7 requests
//! concurrent at the burst second T+1, 3 chains + 4 spots, exactly the
//! operator's "all 7 parallel at first second"; rung 1 fallback: chains
//! at T+1, ALL 4 spots at T+2), the spot seconds further split by the
//! 2026-07-15 ADAPTIVE CONCURRENCY tiers (per-second-group caps
//! 4/3/2/1, greedy overflow to later 1000ms buckets; both brokers'
//! candle endpoints are single-symbol-per-request — never one batched
//! HTTP call). The TWO-BUCKET budget model makes all-7 legal: the 4
//! spot fires pass the spot + COMBINED rolling-1000ms rings (the
//! Data-API 5/sec bucket, 4 ≤ 5); the 3 chain fires pass ONLY the
//! per-(underlying, expiry) ≥3s chain gate (the option-chain API's own
//! budget — the 3s rule is per-SAME-chain-expiry only, different
//! underlyings explicitly concurrent). Groww fires
//! per its fallback-shape ladder (2026-07-16 two-rung prescription +
//! the retained choice-3 last resort — rung 0: all 7 requests in
//! parallel at :00, gate-free lane; rung 1: :01 chains / :02 all 4
//! spots; rung 2: :01 / :02 core spots / :03 VIX alone) with a
//! last-wave+timeout verdict + sequential fallback. Each lane's
//! decision fires the INSTANT its data-complete predicate flips
//! (event-driven, latched exactly-once per (lane, cycle)); past its
//! cutoff the lane HONEST-SKIPS with a coded loud log — never a late
//! decision, never a decision on missing/stale data.
//!
//! # Modules
//! - `schedule` — pure IST minute-boundary + slot-table calculus (per
//!   broker burst offset + shape rung + concurrency step + fallback
//!   shape; boundary math reimplemented here — core cannot depend on
//!   `crates/app`; drift is pinned by mirrored vectors + const-asserts
//!   against the shared `SPOT_1M_REST_*` window constants)
//! - `gate` — pure CAS [`gate::MinSpacingGate`] (per-(underlying,
//!   expiry) chains + per-expiry waves) + the spot/COMBINED
//!   rolling-window rings inside [`gate::DhanGates`] (spot+expiry ≤ 5
//!   Dhan Data-API fires per rolling second; chains solely per-key —
//!   the two-bucket model, THE binding cadence-lane pacing per the
//!   2026-07-16 coordinator ruling) in the MONOTONIC time domain
//!   (injected clock) — the structural zero-429 hard floor
//! - `ladder` — the streak-driven shape/concurrency ladders
//!   ([`ladder::StreakLadder`]: the 2026-07-16 Dhan shape rung 0⇄1,
//!   the spot tiers, the Groww fallback shapes) + the per-second spot
//!   bucket math ([`ladder::spot_second_buckets`]) + in-cycle retry
//!   policy
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
pub mod expiry;
pub mod gate;
pub mod history_repull;
pub mod ladder;
pub mod runner;
pub mod schedule;

pub use executor::{
    CadenceExecutor, CadenceFetchError, ChainFetchOk, ChainFetchRequest, DryRunLoggingExecutor,
    ExpiryListRequest, ExpiryResolver, SpotFetchRequest, SpotSnapshot, SpotTarget,
    StubExpiryResolver,
};
pub use expiry::{
    DayLockedExpiryStore, ExpiryDate, ExpiryPolicy, RecordVerdict, UnderlyingExpiryView,
    global_expiry_store, policy_for, resolve_policy_expiry,
};
pub use gate::{DhanGates, global_dhan_gates, init_global_dhan_gates};
pub use runner::{CadenceRunnerDeps, SystemCadenceClock, spawn_supervised_cadence_runner};

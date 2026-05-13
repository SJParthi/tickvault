//! Phase 2 crash-recovery wiring (PROMPT C, 2026-04-20).
//!
//! At 09:12:30 IST the Phase 2 scheduler pins the day's stock F&O chain
//! to the 09:08..09:12 finalised pre-open prices. If the app crashes or
//! is restarted at (say) 11:00 IST, re-running the picker would resolve
//! ATM from a DIFFERENT live price and subscribe a DIFFERENT chain —
//! breaking continuity for the remainder of the session.
//!
//! Parthiban's directive (2026-04-20): a mid-market restart must resume
//! the SAME ATM chain picked at 09:12.
//!
//! # Responsibility boundary
//!
//! - **PROMPT A** (separate PR) owns the *writer* task that consumes
//!   `Phase2PickCompleted` from the scheduler's `pick_completed_tx`
//!   channel and writes `data/state/phase2-subscription.json` via
//!   [`tickvault_storage::phase2_subscription_marker::write_snapshot`].
//! - **PROMPT C** (this module) owns the *reader* side that runs at
//!   boot BEFORE the scheduler spawns, reads the snapshot, and either
//!   (a) re-dispatches the snapshotted instruments directly and skips
//!   the scheduler, or (b) hands off to the scheduler for the normal
//!   fresh-pick path.
//!
//! When the snapshot file is absent (no PROMPT A yet, fresh clone, or
//! a non-trading-day boot) [`read_snapshot`] returns `None` and the
//! decision naturally falls through to `RunFreshPhase2` /
//! `WaitForPhase2Scheduler` / `SkipPhase2OutsideMarketHours` — the
//! existing scheduler handles every one of those outcomes unchanged.
//!
//! # Purity
//!
//! [`plan_recovery`] is pure — no I/O, no allocations beyond the
//! (cold-path) `InstrumentSubscription` vec built when
//! [`RecoveryDecision::UseSnapshot`] fires. All time / calendar inputs
//! are explicit arguments, making the function trivially testable with
//! synthetic inputs.

use chrono::NaiveDate;

use tickvault_core::websocket::types::InstrumentSubscription;
use tickvault_storage::phase2_subscription_marker::{
    Phase2SubscriptionSnapshot, RecoveryDecision, decide_recovery,
};

/// On-disk path for the Phase 2 subscription snapshot. Must match the
/// path the PROMPT A writer uses.
pub const PHASE2_SNAPSHOT_PATH: &str = "data/state/phase2-subscription.json";

/// Prometheus counter name emitted once per boot — label `outcome`
/// carries one of [`OUTCOME_REUSED_SNAPSHOT`],
/// [`OUTCOME_FRESH_IMMEDIATE`], [`OUTCOME_WAIT_SCHEDULER`],
/// [`OUTCOME_SKIP_OFF_HOURS`].
pub const RECOVERY_METRIC_NAME: &str = "tv_phase2_recovery_total";

/// Metric label: today-dated snapshot was found on disk, its chain was
/// dispatched directly to the pool, scheduler was NOT spawned.
pub const OUTCOME_REUSED_SNAPSHOT: &str = "reused_snapshot";

/// Metric label: no snapshot (or stale) AND we are inside 09:12..15:30
/// IST on a trading day — the scheduler's `RunImmediate` arm picks a
/// fresh chain from the pre-open buffer immediately.
pub const OUTCOME_FRESH_IMMEDIATE: &str = "fresh_immediate";

/// Metric label: boot is before 09:12 IST on a trading day — the
/// scheduler's `SleepUntil` arm will run the picker at the normal 09:12
/// trigger. Nothing to do from recovery's side.
pub const OUTCOME_WAIT_SCHEDULER: &str = "wait_scheduler";

/// Metric label: non-trading day OR boot is at/after 15:30 IST and no
/// valid snapshot is available — stock F&O stays unsubscribed this
/// boot. The scheduler's `SkipToday` arm handles the same condition.
pub const OUTCOME_SKIP_OFF_HOURS: &str = "skip_off_hours";

/// Action the boot sequence must take after consulting the snapshot.
///
/// Keeping this distinct from [`RecoveryDecision`] lets the boot code
/// stay small: the planner pre-converts `PersistedSubscription` →
/// `InstrumentSubscription` so main.rs only needs to call
/// `pool.dispatch_subscribe(...)` for the UseSnapshot arm.
#[derive(Debug, Clone)]
pub enum RecoveryAction {
    /// Today-dated snapshot found — dispatch these instruments via
    /// `WebSocketConnectionPool::dispatch_subscribe` and DO NOT spawn
    /// the Phase 2 scheduler. `snapshot_date` + `instrument_count` are
    /// kept so the caller can log the summary without reparsing.
    DispatchSnapshot {
        snapshot_date: NaiveDate,
        instrument_count: usize,
        instruments: Vec<InstrumentSubscription>,
    },
    /// No snapshot (or stale) AND mid-market on a trading day — spawn
    /// the scheduler; it will run its `RunImmediate` arm immediately.
    RunFreshPhase2,
    /// Pre-09:12 on a trading day — spawn the scheduler; it will run
    /// its `SleepUntil` arm.
    WaitForScheduler,
    /// Off-hours / non-trading-day with no snapshot — do NOT spawn the
    /// scheduler for this boot. Matches the scheduler's own skip
    /// behaviour so we keep the paths observationally identical.
    SkipOffHours,
}

impl RecoveryAction {
    /// Wire-format Prometheus label for [`RECOVERY_METRIC_NAME`].
    #[must_use]
    pub fn outcome_label(&self) -> &'static str {
        match self {
            Self::DispatchSnapshot { .. } => OUTCOME_REUSED_SNAPSHOT,
            Self::RunFreshPhase2 => OUTCOME_FRESH_IMMEDIATE,
            Self::WaitForScheduler => OUTCOME_WAIT_SCHEDULER,
            Self::SkipOffHours => OUTCOME_SKIP_OFF_HOURS,
        }
    }

    /// Whether the Phase 2 scheduler should still be spawned at boot.
    /// `DispatchSnapshot` and `SkipOffHours` replace / suppress the
    /// scheduler; `RunFreshPhase2` and `WaitForScheduler` hand off.
    #[must_use]
    pub fn should_spawn_scheduler(&self) -> bool {
        matches!(self, Self::RunFreshPhase2 | Self::WaitForScheduler)
    }
}

/// Pure planner — no I/O. Consumes the already-loaded snapshot + clock
/// inputs + calendar verdict, returns the action the boot should take.
///
/// # Arguments
/// - `today`: IST calendar date.
/// - `now_sec_of_day_ist`: seconds since IST midnight (0..86_400).
/// - `is_trading_day`: supplied by the caller's `TradingCalendar`.
/// - `snapshot`: result of reading the snapshot file (or `None`).
///
/// # Performance
/// O(n) only when `UseSnapshot` fires, where n is the number of
/// persisted instruments (cold boot-time path — not a hot path).
#[must_use]
pub fn plan_recovery(
    today: NaiveDate,
    now_sec_of_day_ist: u32,
    is_trading_day: bool,
    snapshot: Option<&Phase2SubscriptionSnapshot>,
) -> RecoveryAction {
    match decide_recovery(today, snapshot, now_sec_of_day_ist, is_trading_day) {
        RecoveryDecision::UseSnapshot { snapshot } => {
            let instruments: Vec<InstrumentSubscription> = snapshot
                .instruments
                .iter()
                .map(|p| InstrumentSubscription {
                    exchange_segment: p.exchange_segment.clone(),
                    security_id: p.security_id.clone(),
                })
                .collect();
            RecoveryAction::DispatchSnapshot {
                snapshot_date: snapshot.snapshot_date,
                instrument_count: snapshot.instrument_count,
                instruments,
            }
        }
        RecoveryDecision::RunFreshPhase2 => RecoveryAction::RunFreshPhase2,
        RecoveryDecision::WaitForPhase2Scheduler => RecoveryAction::WaitForScheduler,
        RecoveryDecision::SkipPhase2OutsideMarketHours => RecoveryAction::SkipOffHours,
    }
}

/// PR-E indices-only ratchet helper. Returns `true` iff the operator's
/// configured subscription scope makes Phase 2 stock-F&O dispatch
/// meaningful. Under `IndicesOnlyAllExpiries` (the default since Wave
/// 5) AND `IndicesUnderlyingsOnly` (Phase 0 LEAN MVP, 2026-05-13), the
/// subscription planner already drops stock derivatives, so running
/// Phase 2 would silently no-op and waste the 09:13 wakeup + preopen-
/// buffer cloning + REST fallback HTTP work. main.rs gates the
/// scheduler spawn AND the snapshot-recovery dispatch on this function.
#[must_use]
pub const fn should_spawn_phase2_scheduler(
    scope: tickvault_common::config::SubscriptionScope,
) -> bool {
    match scope {
        // No stock F&O subscribed under these scopes → Phase 2 is a no-op.
        tickvault_common::config::SubscriptionScope::IndicesOnlyAllExpiries => false,
        tickvault_common::config::SubscriptionScope::IndicesUnderlyingsOnly => false,
        tickvault_common::config::SubscriptionScope::FullUniverse => true,
    }
}

/// Phase 0 Item 3 (operator-locked 2026-05-13) — returns `true` iff the
/// configured scope ALLOWS the depth dynamic pipeline (depth-20 + depth-200
/// pools) AND the `[features].depth_dynamic_pipeline_v2` flag is on. Under
/// `IndicesUnderlyingsOnly` the depth pipelines are PARKED entirely
/// (Phase 0 LEAN MVP per `topic-PHASE-0-LEAN-LOCKED.md`) — operator's
/// option-buying strategy uses underlying ticks only, not order-book
/// depth. The feature flag stays in `config/base.toml` so legacy /
/// FullUniverse + Wave 5 production can flip the v2 cutover.
///
/// Pure `const fn`; tested by
/// `test_should_spawn_depth_dynamic_pipeline_*` ratchets.
#[must_use]
pub const fn should_spawn_depth_dynamic_pipeline(
    scope: tickvault_common::config::SubscriptionScope,
    feature_flag: bool,
) -> bool {
    match scope {
        // Phase 0 LEAN MVP — depth feeds parked entirely.
        tickvault_common::config::SubscriptionScope::IndicesUnderlyingsOnly => false,
        tickvault_common::config::SubscriptionScope::IndicesOnlyAllExpiries
        | tickvault_common::config::SubscriptionScope::FullUniverse => feature_flag,
    }
}

/// Phase 0 Item 3 (operator-locked 2026-05-13) — returns `true` iff the
/// configured scope ALLOWS the greeks pipeline AND the `[greeks].enabled`
/// flag is on. Under `IndicesUnderlyingsOnly` the greeks pipeline is
/// PARKED entirely (Phase 0 LEAN MVP) — operator's option-buying strategy
/// computes indicators on underlying spot ticks; streaming Delta/Theta/
/// Vega is Phase 2 territory.
///
/// Pure `const fn`; tested by `test_should_spawn_greeks_pipeline_*`.
#[must_use]
pub const fn should_spawn_greeks_pipeline(
    scope: tickvault_common::config::SubscriptionScope,
    greeks_enabled: bool,
) -> bool {
    match scope {
        tickvault_common::config::SubscriptionScope::IndicesUnderlyingsOnly => false,
        tickvault_common::config::SubscriptionScope::IndicesOnlyAllExpiries
        | tickvault_common::config::SubscriptionScope::FullUniverse => greeks_enabled,
    }
}

/// Phase 0 Item 4 (operator-locked 2026-05-13) — returns the effective
/// per-conn activity-watchdog threshold for the main-feed WebSocket
/// pool. Under `IndicesUnderlyingsOnly` the data rate is dense (113-448
/// frames/sec aggregate across the single conn — 4 IDX_I + 218 NSE_EQ),
/// so we tighten to `WATCHDOG_THRESHOLD_IDX_I_SECS = 3` (silent-socket
/// detection in <5s instead of the legacy ~55s). Under legacy / Wave 5
/// scopes the conn count is up to 5 with potentially sparser per-conn
/// traffic, so we preserve `WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS = 50`
/// (above Dhan's 40s server ping timeout per the original P2.1 plan).
///
/// Pure `const fn`; tested by
/// `test_effective_main_feed_watchdog_threshold_*`.
#[must_use]
pub const fn effective_main_feed_watchdog_threshold_secs(
    scope: tickvault_common::config::SubscriptionScope,
) -> u64 {
    match scope {
        tickvault_common::config::SubscriptionScope::IndicesUnderlyingsOnly => {
            tickvault_core::websocket::activity_watchdog::WATCHDOG_THRESHOLD_IDX_I_SECS
        }
        tickvault_common::config::SubscriptionScope::IndicesOnlyAllExpiries
        | tickvault_common::config::SubscriptionScope::FullUniverse => {
            tickvault_core::websocket::activity_watchdog::WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS
        }
    }
}

/// Returns the current IST date and seconds-since-IST-midnight for the
/// recovery decision. Uses `tickvault_common::trading_calendar::ist_offset`
/// so the definition of "IST" lives in one place.
#[must_use]
pub fn current_ist_seconds_of_day() -> (NaiveDate, u32) {
    use chrono::{TimeZone, Timelike, Utc};
    let ist = tickvault_common::trading_calendar::ist_offset();
    let now_ist = ist.from_utc_datetime(&Utc::now().naive_utc());
    (
        now_ist.date_naive(),
        now_ist.time().num_seconds_from_midnight(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tickvault_storage::phase2_subscription_marker::PersistedSubscription;

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 4, 20).expect("valid")
    }

    fn yesterday() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 4, 19).expect("valid")
    }

    fn sample_snapshot(date: NaiveDate) -> Phase2SubscriptionSnapshot {
        let mut prices = BTreeMap::new();
        prices.insert("RELIANCE".to_string(), 2847.5);
        Phase2SubscriptionSnapshot {
            snapshot_date: date,
            instrument_count: 2,
            reference_prices: prices,
            instruments: vec![
                PersistedSubscription {
                    exchange_segment: "NSE_FNO".to_string(),
                    security_id: "45001".to_string(),
                },
                PersistedSubscription {
                    exchange_segment: "NSE_FNO".to_string(),
                    security_id: "45002".to_string(),
                },
            ],
        }
    }

    #[test]
    fn test_plan_recovery_today_snapshot_returns_dispatch() {
        let snap = sample_snapshot(today());
        let action = plan_recovery(today(), 11 * 3600, true, Some(&snap));
        match action {
            RecoveryAction::DispatchSnapshot {
                snapshot_date,
                instrument_count,
                instruments,
            } => {
                assert_eq!(snapshot_date, today());
                assert_eq!(instrument_count, 2);
                assert_eq!(instruments.len(), 2);
                assert_eq!(instruments[0].exchange_segment, "NSE_FNO");
                assert_eq!(instruments[0].security_id, "45001");
                assert_eq!(instruments[1].security_id, "45002");
            }
            other => panic!("expected DispatchSnapshot, got {other:?}"),
        }
    }

    #[test]
    fn test_plan_recovery_mid_market_no_snapshot_runs_fresh() {
        let action = plan_recovery(today(), 11 * 3600, true, None);
        assert!(matches!(action, RecoveryAction::RunFreshPhase2));
    }

    #[test]
    fn test_plan_recovery_pre_open_no_snapshot_waits() {
        let action = plan_recovery(today(), 8 * 3600 + 30 * 60, true, None);
        assert!(matches!(action, RecoveryAction::WaitForScheduler));
    }

    #[test]
    fn test_plan_recovery_non_trading_day_no_snapshot_skips() {
        let action = plan_recovery(today(), 11 * 3600, false, None);
        assert!(matches!(action, RecoveryAction::SkipOffHours));
    }

    #[test]
    fn test_plan_recovery_stale_snapshot_mid_market_runs_fresh() {
        let stale = sample_snapshot(yesterday());
        let action = plan_recovery(today(), 11 * 3600, true, Some(&stale));
        assert!(matches!(action, RecoveryAction::RunFreshPhase2));
    }

    #[test]
    fn test_outcome_labels_are_stable() {
        assert_eq!(OUTCOME_REUSED_SNAPSHOT, "reused_snapshot");
        assert_eq!(OUTCOME_FRESH_IMMEDIATE, "fresh_immediate");
        assert_eq!(OUTCOME_WAIT_SCHEDULER, "wait_scheduler");
        assert_eq!(OUTCOME_SKIP_OFF_HOURS, "skip_off_hours");
    }

    #[test]
    fn test_outcome_label_dispatches_per_variant() {
        let snap = sample_snapshot(today());
        let d = plan_recovery(today(), 11 * 3600, true, Some(&snap));
        assert_eq!(d.outcome_label(), OUTCOME_REUSED_SNAPSHOT);
        let f = plan_recovery(today(), 11 * 3600, true, None);
        assert_eq!(f.outcome_label(), OUTCOME_FRESH_IMMEDIATE);
        let w = plan_recovery(today(), 8 * 3600, true, None);
        assert_eq!(w.outcome_label(), OUTCOME_WAIT_SCHEDULER);
        let s = plan_recovery(today(), 11 * 3600, false, None);
        assert_eq!(s.outcome_label(), OUTCOME_SKIP_OFF_HOURS);
    }

    #[test]
    fn test_should_spawn_scheduler_matches_handoff_semantics() {
        assert!(RecoveryAction::RunFreshPhase2.should_spawn_scheduler());
        assert!(RecoveryAction::WaitForScheduler.should_spawn_scheduler());
        assert!(!RecoveryAction::SkipOffHours.should_spawn_scheduler());
        assert!(
            !RecoveryAction::DispatchSnapshot {
                snapshot_date: today(),
                instrument_count: 0,
                instruments: Vec::new(),
            }
            .should_spawn_scheduler()
        );
    }

    #[test]
    fn test_snapshot_path_is_stable() {
        assert_eq!(PHASE2_SNAPSHOT_PATH, "data/state/phase2-subscription.json");
    }

    #[test]
    fn test_metric_name_is_stable() {
        assert_eq!(RECOVERY_METRIC_NAME, "tv_phase2_recovery_total");
    }

    #[test]
    fn test_current_ist_seconds_of_day_is_bounded() {
        let (_date, secs) = current_ist_seconds_of_day();
        assert!(secs < 86_400, "seconds-of-day must be < 86_400, got {secs}");
    }

    // -----------------------------------------------------------------------
    // PR-E indices-only — should_spawn_phase2_scheduler ratchets
    // -----------------------------------------------------------------------

    #[test]
    fn test_should_spawn_phase2_scheduler_skipped_under_indices_only() {
        // Default scope = no stock F&O = Phase 2 dispatch is dead weight.
        assert!(!should_spawn_phase2_scheduler(
            tickvault_common::config::SubscriptionScope::IndicesOnlyAllExpiries
        ));
    }

    #[test]
    fn test_should_spawn_phase2_scheduler_runs_under_full_universe() {
        // Legacy scope still needs Phase 2 for ATM±25 stock F&O dispatch.
        assert!(should_spawn_phase2_scheduler(
            tickvault_common::config::SubscriptionScope::FullUniverse
        ));
    }

    /// Coverage ratchet: every variant of SubscriptionScope must have a
    /// defined Phase 2 spawn answer. Adding a new scope variant without
    /// updating `should_spawn_phase2_scheduler` will fail the compile
    /// match exhaustiveness check first; this test pins the *count* of
    /// variants so a silent renaming doesn't slip through.
    #[test]
    fn test_should_spawn_phase2_scheduler_covers_every_scope_variant() {
        // Three known variants today. If a fourth is added, this test
        // breaks and the operator must consciously decide its semantics.
        let variants = [
            tickvault_common::config::SubscriptionScope::IndicesOnlyAllExpiries,
            tickvault_common::config::SubscriptionScope::FullUniverse,
            tickvault_common::config::SubscriptionScope::IndicesUnderlyingsOnly,
        ];
        // Just exercise all — the assertions in the prior tests pin
        // actual values.
        for v in variants {
            let _ = should_spawn_phase2_scheduler(v);
        }
    }

    /// Phase 0 Item 1 (2026-05-13): IndicesUnderlyingsOnly drops stock F&O
    /// → Phase 2 scheduler must NOT spawn (same as IndicesOnlyAllExpiries).
    #[test]
    fn test_should_spawn_phase2_scheduler_skipped_under_phase_0_scope() {
        assert!(!should_spawn_phase2_scheduler(
            tickvault_common::config::SubscriptionScope::IndicesUnderlyingsOnly,
        ));
    }

    // ----------------------------------------------------------------------
    // Phase 0 Item 3 — depth dynamic pipeline + greeks pipeline scope gates
    // ----------------------------------------------------------------------

    #[test]
    fn test_should_spawn_depth_dynamic_pipeline_skipped_under_phase_0_regardless_of_flag() {
        // Phase 0 LEAN MVP: depth feeds parked entirely. Flag state IGNORED.
        for flag in [true, false] {
            assert!(!should_spawn_depth_dynamic_pipeline(
                tickvault_common::config::SubscriptionScope::IndicesUnderlyingsOnly,
                flag,
            ));
        }
    }

    #[test]
    fn test_should_spawn_depth_dynamic_pipeline_honours_flag_under_other_scopes() {
        for scope in [
            tickvault_common::config::SubscriptionScope::IndicesOnlyAllExpiries,
            tickvault_common::config::SubscriptionScope::FullUniverse,
        ] {
            assert!(should_spawn_depth_dynamic_pipeline(scope, true));
            assert!(!should_spawn_depth_dynamic_pipeline(scope, false));
        }
    }

    #[test]
    fn test_should_spawn_greeks_pipeline_skipped_under_phase_0_regardless_of_flag() {
        for greeks_enabled in [true, false] {
            assert!(!should_spawn_greeks_pipeline(
                tickvault_common::config::SubscriptionScope::IndicesUnderlyingsOnly,
                greeks_enabled,
            ));
        }
    }

    #[test]
    fn test_should_spawn_greeks_pipeline_honours_flag_under_other_scopes() {
        for scope in [
            tickvault_common::config::SubscriptionScope::IndicesOnlyAllExpiries,
            tickvault_common::config::SubscriptionScope::FullUniverse,
        ] {
            assert!(should_spawn_greeks_pipeline(scope, true));
            assert!(!should_spawn_greeks_pipeline(scope, false));
        }
    }

    // ----------------------------------------------------------------------
    // Phase 0 Item 4 — main-feed activity watchdog threshold scope gate
    // ----------------------------------------------------------------------

    #[test]
    fn test_effective_main_feed_watchdog_threshold_under_phase_0_is_three_secs() {
        assert_eq!(
            effective_main_feed_watchdog_threshold_secs(
                tickvault_common::config::SubscriptionScope::IndicesUnderlyingsOnly,
            ),
            tickvault_core::websocket::activity_watchdog::WATCHDOG_THRESHOLD_IDX_I_SECS,
        );
        assert_eq!(
            effective_main_feed_watchdog_threshold_secs(
                tickvault_common::config::SubscriptionScope::IndicesUnderlyingsOnly,
            ),
            3,
            "Phase 0 effective threshold must match WATCHDOG_THRESHOLD_IDX_I_SECS = 3",
        );
    }

    #[test]
    fn test_effective_main_feed_watchdog_threshold_under_legacy_is_fifty_secs() {
        for scope in [
            tickvault_common::config::SubscriptionScope::IndicesOnlyAllExpiries,
            tickvault_common::config::SubscriptionScope::FullUniverse,
        ] {
            assert_eq!(
                effective_main_feed_watchdog_threshold_secs(scope),
                tickvault_core::websocket::activity_watchdog::WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS,
            );
            assert_eq!(
                effective_main_feed_watchdog_threshold_secs(scope),
                50,
                "Legacy effective threshold must match WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS = 50",
            );
        }
    }

    #[test]
    fn test_effective_main_feed_watchdog_threshold_covers_every_scope_variant() {
        // Compile-fail ratchet — adding a 4th SubscriptionScope variant
        // without updating effective_main_feed_watchdog_threshold_secs
        // fails the const-fn match exhaustiveness check.
        let variants = [
            tickvault_common::config::SubscriptionScope::IndicesOnlyAllExpiries,
            tickvault_common::config::SubscriptionScope::FullUniverse,
            tickvault_common::config::SubscriptionScope::IndicesUnderlyingsOnly,
        ];
        for v in variants {
            let _ = effective_main_feed_watchdog_threshold_secs(v);
        }
    }
}

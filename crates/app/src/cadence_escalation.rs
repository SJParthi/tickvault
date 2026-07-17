//! Cadence-lane escalation edges (fix round 2026-07-17, HIGH).
//!
//! The stood-down legacy per-minute loops were the ONLY emit sites for the
//! `error!(code = "SPOT1M-01", stage = "escalation")` /
//! `error!(code = "CHAIN-02", stage = "escalation")` lines the CloudWatch
//! filters `spot1m-01-escalation` / `chain-02-escalation` scope on, and for
//! the typed Telegram pages (`Spot1mFetchDegraded` / `ChainFetchDegraded` +
//! the Groww twins). This module ports the 3-consecutive-fully-failed-
//! minutes edge onto the cadence executors, REUSING the existing
//! [`FailureEdge`] state machine, the existing ErrorCodes, and the existing
//! NotificationEvent variants — NO new variant, NO new filter, NO terraform
//! change.
//!
//! Semantics (the legacy contract, minute-bucketed executor-side):
//! - Each lane (Dhan / Groww) tallies per-leg (spot / chain) outcomes per
//!   cycle minute. A minute is FULLY FAILED when it saw ≥1 attempt and
//!   ZERO successes — and a persist failure counts as a FAILED attempt
//!   (fetch-ok-but-lost is not ok; the legacy M1 rule).
//! - A minute FINALIZES when a NEWER minute's first outcome arrives for
//!   that leg — the runner fires every in-session minute, so finalization
//!   lags one minute. Honest envelope: the session's LAST minute never
//!   finalizes (the day rolls; the edge state is run-scoped, a task
//!   respawn restarts the streak — the legacy [`FailureEdge`] envelope).
//! - Rising edge (3 consecutive fully-failed finalized minutes): ONE coded
//!   `error!` with the EXACT legacy field shape (the CW filters match on
//!   code + level + stage) + the existing typed HIGH Telegram event.
//! - Falling edge (a finalized minute with ≥1 success after a paged
//!   episode): one Info recovery event; the edge re-arms.

use std::sync::Arc;

use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;
use tickvault_core::notification::{NotificationEvent, NotificationService};
use tracing::{error, info};

pub(crate) use crate::spot_1m_rest_boot::{EdgeAction, FailureEdge, format_minute_ist_12h};

/// Runs a blocking questdb-rs ILP `flush()` (conf-pinned 5s request_timeout
/// per attempt) without pinning a tokio runtime worker: on the multi-thread
/// runtime the closure runs under `tokio::task::block_in_place` — the
/// seal-writer house pattern (`crates/storage/src/seal_writer_loop.rs`),
/// sibling tasks migrate while the sync HTTP flush blocks. Current-thread
/// runtimes (the `#[tokio::test]` harness) call directly, because
/// `block_in_place` panics there.
pub(crate) fn flush_off_worker<T>(f: impl FnOnce() -> T) -> T {
    if tokio::runtime::Handle::current().runtime_flavor()
        == tokio::runtime::RuntimeFlavor::MultiThread
    {
        tokio::task::block_in_place(f)
    } else {
        f()
    }
}

/// Which cadence leg an outcome belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EscalationLeg {
    Spot,
    Chain,
}

/// Per-leg minute-bucketed tally feeding the reused [`FailureEdge`].
#[derive(Debug, Default)]
struct LegTally {
    /// The minute (IST secs-of-day of the minute OPEN) currently
    /// accumulating.
    minute_secs: Option<u32>,
    ok: u32,
    attempts: u32,
    edge: FailureEdge,
}

impl LegTally {
    /// Record ONE target outcome (`ok` = fetch succeeded AND persisted)
    /// for the cycle minute `minute_secs`. When a NEWER minute's first
    /// outcome arrives, the PREVIOUS minute finalizes — returns
    /// `Some((finalized_minute_secs, action))`. A stale outcome for an
    /// already-rolled minute is ignored (audit-only late completion).
    fn record(&mut self, minute_secs: u32, ok: bool) -> Option<(u32, EdgeAction)> {
        let mut finalized = None;
        match self.minute_secs {
            Some(cur) if minute_secs > cur => {
                let fully_failed = self.attempts > 0 && self.ok == 0;
                let action = self.edge.record_minute(fully_failed);
                finalized = Some((cur, action));
                self.minute_secs = Some(minute_secs);
                self.ok = 0;
                self.attempts = 0;
            }
            Some(cur) if minute_secs < cur => return None,
            Some(_) => {}
            None => self.minute_secs = Some(minute_secs),
        }
        self.attempts = self.attempts.saturating_add(1);
        if ok {
            self.ok = self.ok.saturating_add(1);
        }
        finalized
    }
}

/// One broker lane's escalation state (both legs). Held behind the
/// executor's `tokio::Mutex` — cold path, a handful of fires per minute.
#[derive(Debug, Default)]
pub(crate) struct LaneEscalation {
    spot: LegTally,
    chain: LegTally,
}

impl LaneEscalation {
    /// Record one outcome; returns the finalize action for the previous
    /// minute when this outcome rolled the bucket.
    pub(crate) fn record(
        &mut self,
        leg: EscalationLeg,
        minute_secs: u32,
        ok: bool,
    ) -> Option<(u32, EdgeAction)> {
        match leg {
            EscalationLeg::Spot => self.spot.record(minute_secs, ok),
            EscalationLeg::Chain => self.chain.record(minute_secs, ok),
        }
    }
}

/// Emit the coded `error!`/`info!` + the EXISTING typed Telegram event for
/// a finalized minute's edge action. Field shapes match the legacy emit
/// sites EXACTLY (the CloudWatch `spot1m-01-escalation` /
/// `chain-02-escalation` filters scope on `$.code` + `$.level` +
/// `$.stage`; Groww lines additionally carry `feed = "groww"` per the
/// house grep-split convention). `EdgeAction::None` is a no-op.
pub(crate) fn emit_edge_action(
    feed: Feed,
    leg: EscalationLeg,
    minute_secs: u32,
    action: EdgeAction,
    notifier: Option<&Arc<NotificationService>>,
) {
    let minute_label = format_minute_ist_12h(minute_secs);
    match (feed, leg, action) {
        (_, _, EdgeAction::None) => {}
        (Feed::Dhan, EscalationLeg::Spot, EdgeAction::Page { consecutive }) => {
            error!(
                code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                stage = "escalation",
                consecutive,
                minute = %minute_label,
                "SPOT1M-01: per-minute spot fetch fully failed for consecutive \
                 minutes — paging (edge-triggered)"
            );
            if let Some(n) = notifier {
                n.notify(NotificationEvent::Spot1mFetchDegraded {
                    consecutive_failed_minutes: consecutive,
                    minute_ist: minute_label,
                });
            }
        }
        (Feed::Dhan, EscalationLeg::Spot, EdgeAction::Recover { failed_minutes }) => {
            info!(
                failed_minutes,
                minute = %minute_label,
                "cadence: Dhan per-minute spot fetch recovered after a paged episode"
            );
            if let Some(n) = notifier {
                n.notify(NotificationEvent::Spot1mFetchRecovered {
                    minute_ist: minute_label,
                    failed_minutes,
                });
            }
        }
        (Feed::Dhan, EscalationLeg::Chain, EdgeAction::Page { consecutive }) => {
            error!(
                code = ErrorCode::Chain02FetchDegraded.code_str(),
                stage = "escalation",
                consecutive,
                minute = %minute_label,
                "CHAIN-02: per-minute option-chain fetch fully failed for \
                 consecutive minutes — paging (edge-triggered)"
            );
            if let Some(n) = notifier {
                n.notify(NotificationEvent::ChainFetchDegraded {
                    consecutive_failed_minutes: consecutive,
                    minute_ist: minute_label,
                });
            }
        }
        (Feed::Dhan, EscalationLeg::Chain, EdgeAction::Recover { failed_minutes }) => {
            info!(
                failed_minutes,
                minute = %minute_label,
                "cadence: Dhan per-minute chain fetch recovered after a paged episode"
            );
            if let Some(n) = notifier {
                n.notify(NotificationEvent::ChainFetchRecovered {
                    minute_ist: minute_label,
                    failed_minutes,
                });
            }
        }
        (Feed::Groww, EscalationLeg::Spot, EdgeAction::Page { consecutive }) => {
            error!(
                code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                stage = "escalation",
                feed = "groww",
                consecutive,
                minute = %minute_label,
                "SPOT1M-01: Groww per-minute spot fetch fully failed for \
                 consecutive minutes — paging (edge-triggered)"
            );
            if let Some(n) = notifier {
                n.notify(NotificationEvent::GrowwSpot1mFetchDegraded {
                    consecutive_failed_minutes: consecutive,
                    minute_ist: minute_label,
                });
            }
        }
        (Feed::Groww, EscalationLeg::Spot, EdgeAction::Recover { failed_minutes }) => {
            info!(
                failed_minutes,
                minute = %minute_label,
                "cadence: Groww per-minute spot fetch recovered after a paged episode"
            );
            if let Some(n) = notifier {
                n.notify(NotificationEvent::GrowwSpot1mFetchRecovered {
                    minute_ist: minute_label,
                    failed_minutes,
                });
            }
        }
        (Feed::Groww, EscalationLeg::Chain, EdgeAction::Page { consecutive }) => {
            error!(
                code = ErrorCode::Chain02FetchDegraded.code_str(),
                stage = "escalation",
                feed = "groww",
                consecutive,
                minute = %minute_label,
                "CHAIN-02: Groww per-minute chain fetch fully failed for \
                 consecutive minutes — paging (edge-triggered)"
            );
            if let Some(n) = notifier {
                n.notify(NotificationEvent::GrowwChain1mFetchDegraded {
                    consecutive_failed_minutes: consecutive,
                    minute_ist: minute_label,
                });
            }
        }
        (Feed::Groww, EscalationLeg::Chain, EdgeAction::Recover { failed_minutes }) => {
            info!(
                failed_minutes,
                minute = %minute_label,
                "cadence: Groww per-minute chain fetch recovered after a paged episode"
            );
            if let Some(n) = notifier {
                n.notify(NotificationEvent::GrowwChain1mFetchRecovered {
                    minute_ist: minute_label,
                    failed_minutes,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::constants::SPOT_1M_REST_CONSECUTIVE_FAIL_PAGE_THRESHOLD;

    const M0: u32 = 33_360; // 09:16:00 IST minute open
    const fn minute(i: u32) -> u32 {
        M0 + i * 60
    }

    #[test]
    fn test_first_minute_never_finalizes_until_rollover() {
        let mut lane = LaneEscalation::default();
        assert_eq!(lane.record(EscalationLeg::Spot, minute(0), false), None);
        assert_eq!(lane.record(EscalationLeg::Spot, minute(0), false), None);
        // Rollover finalizes minute 0 (fully failed → below threshold →
        // EdgeAction::None).
        assert_eq!(
            lane.record(EscalationLeg::Spot, minute(1), false),
            Some((minute(0), EdgeAction::None))
        );
    }

    #[test]
    fn test_three_strike_rising_edge_pages_once() {
        let mut lane = LaneEscalation::default();
        let t = SPOT_1M_REST_CONSECUTIVE_FAIL_PAGE_THRESHOLD;
        // Fail minutes 0..t; each finalizes at the next minute's first
        // outcome — the PAGE fires when the t-th failed minute finalizes.
        let mut paged = 0u32;
        for i in 0..=t {
            if let Some((_, EdgeAction::Page { consecutive })) =
                lane.record(EscalationLeg::Spot, minute(i), false)
            {
                paged += 1;
                assert_eq!(consecutive, t);
                assert_eq!(i, t, "the page fires exactly when minute t-1 finalizes");
            }
        }
        assert_eq!(paged, 1, "rising edge pages exactly once");
        // Further failed minutes stay silent (already paged this episode).
        assert_eq!(
            lane.record(EscalationLeg::Spot, minute(t + 1), false),
            Some((minute(t), EdgeAction::None))
        );
    }

    #[test]
    fn test_recovery_after_paged_episode_re_arms() {
        let mut lane = LaneEscalation::default();
        let t = SPOT_1M_REST_CONSECUTIVE_FAIL_PAGE_THRESHOLD;
        for i in 0..=t {
            lane.record(EscalationLeg::Spot, minute(i), false);
        }
        // Minute t succeeds (ok outcome), finalized by minute t+1's first
        // outcome → Recover.
        let mut lane2 = LaneEscalation::default();
        for i in 0..=t {
            lane2.record(EscalationLeg::Spot, minute(i), false);
        }
        // overwrite minute t as an OK minute
        lane2.record(EscalationLeg::Spot, minute(t), true);
        let fin = lane2.record(EscalationLeg::Spot, minute(t + 1), false);
        match fin {
            Some((m, EdgeAction::Recover { failed_minutes })) => {
                assert_eq!(m, minute(t));
                assert_eq!(failed_minutes, t);
            }
            other => panic!("expected Recover, got {other:?}"),
        }
        // The edge re-armed: a fresh t-strike run pages again.
        let mut paged_again = false;
        for i in (t + 2)..=(2 * t + 2) {
            if let Some((_, EdgeAction::Page { .. })) =
                lane2.record(EscalationLeg::Spot, minute(i), false)
            {
                paged_again = true;
            }
        }
        assert!(paged_again, "edge re-arms after recovery");
    }

    #[test]
    fn test_mixed_minute_with_one_ok_is_not_fully_failed() {
        let mut lane = LaneEscalation::default();
        lane.record(EscalationLeg::Spot, minute(0), false);
        lane.record(EscalationLeg::Spot, minute(0), true);
        lane.record(EscalationLeg::Spot, minute(0), false);
        // Finalize: ok > 0 → not fully failed → None (no episode).
        assert_eq!(
            lane.record(EscalationLeg::Spot, minute(1), false),
            Some((minute(0), EdgeAction::None))
        );
    }

    #[test]
    fn test_stale_older_minute_outcome_is_ignored() {
        let mut lane = LaneEscalation::default();
        lane.record(EscalationLeg::Spot, minute(2), false);
        // A late completion for an already-rolled minute neither counts
        // nor finalizes anything.
        assert_eq!(lane.record(EscalationLeg::Spot, minute(1), true), None);
        // The current bucket is untouched: rollover still finalizes
        // minute 2 as fully failed (attempts 1, ok 0).
        assert_eq!(
            lane.record(EscalationLeg::Spot, minute(3), false),
            Some((minute(2), EdgeAction::None))
        );
    }

    #[test]
    fn test_legs_are_independent() {
        let mut lane = LaneEscalation::default();
        lane.record(EscalationLeg::Spot, minute(0), false);
        // A chain outcome for minute 1 must NOT finalize the spot bucket.
        assert_eq!(lane.record(EscalationLeg::Chain, minute(1), true), None);
        assert_eq!(
            lane.record(EscalationLeg::Spot, minute(1), true),
            Some((minute(0), EdgeAction::None))
        );
    }
}

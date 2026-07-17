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
use tickvault_common::types::SecurityId;
use tickvault_core::cadence::executor::CadenceFetchError;
use tickvault_core::notification::{NotificationEvent, NotificationService};
use tracing::{error, info};

use crate::groww_option_chain_1m_boot::{
    UnderlyingEdgeAction as GrowwUnderlyingEdgeAction,
    UnderlyingServedTracker as GrowwChainServedTracker,
};
use crate::option_chain_1m_boot::{
    UnderlyingEdgeAction as DhanUnderlyingEdgeAction,
    UnderlyingServedTracker as DhanChainServedTracker,
};
pub(crate) use crate::spot_1m_rest_boot::{EdgeAction, FailureEdge, format_minute_ist_12h};
use crate::spot_1m_rest_boot::{SidEdgeAction, SidServedTracker};

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

/// Fetch-level per-target outcome for the NOT-SERVED detectors (fix
/// round 2 2026-07-17, HIGH — the legacy per-SID / per-underlying pagers
/// restored on the cadence path). Deliberately FETCH-level, never
/// persist-gated: "is the VENDOR serving this target?" — persist failures
/// are OUR side and feed the escalation edge via the M1 gate (the legacy
/// tracker docs' exact split).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TargetOutcome {
    /// The vendor served this target's data this minute.
    Served,
    /// Failed/empty — a candidate not-served minute (counted only when
    /// ≥1 SIBLING served the same minute; a global-failure minute
    /// neither counts nor resets).
    NotServed,
    /// Auth-class failure (dead token / entitlement) — a GLOBAL token
    /// condition: the WHOLE minute becomes a tracker HOLD (the legacy
    /// no-token / auth-abort / EntitlementStop sink-skip arms).
    AuthHold,
}

/// Classify one executor result for the not-served detectors. `None` =
/// do NOT record this target at all (a per-target HOLD — the legacy
/// "missing from the minute's verdicts is simply untouched" arm):
/// `QueueDelay` is SELF-INFLICTED pacing, never a vendor verdict.
pub(crate) fn target_outcome_of<T>(result: &Result<T, CadenceFetchError>) -> Option<TargetOutcome> {
    match result {
        Ok(_) => Some(TargetOutcome::Served),
        Err(CadenceFetchError::Auth) => Some(TargetOutcome::AuthHold),
        Err(CadenceFetchError::QueueDelay) => None,
        Err(_) => Some(TargetOutcome::NotServed),
    }
}

/// Per-leg minute-bucketed PER-TARGET outcome map (the [`LegTally`]
/// pattern applied per target). Finalizes the previous minute's batch
/// when a NEWER minute's first outcome arrives; a stale older-minute
/// outcome is ignored (audit-only late completion). Same-minute repeats
/// for one target are LAST-WRITE-WINS — the runner's ONE bounded
/// in-cycle retry REPLACES the first pass for all downstream processing
/// (the §2 retry contract).
#[derive(Debug, Default)]
struct MinuteTargets {
    minute_secs: Option<u32>,
    targets: Vec<(&'static str, SecurityId, TargetOutcome)>,
}

type FinalizedTargets = (u32, Vec<(&'static str, SecurityId, TargetOutcome)>);

impl MinuteTargets {
    fn record(
        &mut self,
        minute_secs: u32,
        symbol: &'static str,
        security_id: SecurityId,
        outcome: TargetOutcome,
    ) -> Option<FinalizedTargets> {
        let mut finalized = None;
        match self.minute_secs {
            Some(cur) if minute_secs > cur => {
                finalized = Some((cur, std::mem::take(&mut self.targets)));
                self.minute_secs = Some(minute_secs);
            }
            Some(cur) if minute_secs < cur => return None,
            Some(_) => {}
            None => self.minute_secs = Some(minute_secs),
        }
        if let Some(entry) = self.targets.iter_mut().find(|(s, _, _)| *s == symbol) {
            entry.1 = security_id;
            entry.2 = outcome;
        } else {
            self.targets.push((symbol, security_id, outcome));
        }
        finalized
    }
}

/// Unified edge verdict mapped from the reused legacy trackers'
/// per-tracker action enums (structurally identical state machines).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NotServedAction {
    /// Nothing to page for this target this minute.
    None,
    /// RISING edge — page ONCE (High), latched until this target's own
    /// recovery (threshold 10 counted minutes, the legacy constants).
    Page { consecutive: u32 },
    /// FALLING edge — one Info ping; the latch re-arms.
    Recover { not_served_minutes: u32 },
}

/// One emit-ready per-target verdict for a FINALIZED minute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NotServedEmit {
    pub(crate) symbol: &'static str,
    pub(crate) security_id: SecurityId,
    /// `true` = this minute COUNTED toward the target's not-served
    /// streak (failed while ≥1 sibling served) — increments the legacy
    /// per-counted-minute counter.
    pub(crate) counted: bool,
    pub(crate) action: NotServedAction,
}

/// One broker lane's escalation state (both legs) + the reused legacy
/// not-served trackers. Held behind the executor's `tokio::Mutex` —
/// cold path, a handful of fires per minute.
#[derive(Debug)]
pub(crate) struct LaneEscalation {
    feed: Feed,
    spot: LegTally,
    chain: LegTally,
    spot_targets: MinuteTargets,
    chain_targets: MinuteTargets,
    /// Dhan lane only: the legacy per-SID spot detector (4 SIDs incl.
    /// INDIA VIX — `SPOT1M-01 stage="sid_not_served"`).
    dhan_spot_served: SidServedTracker,
    /// Dhan lane only: the legacy per-underlying chain detector
    /// (`CHAIN-02 stage="underlying_not_served"`, field-less on `feed`).
    dhan_chain_served: DhanChainServedTracker,
    /// Groww lane only: the legacy per-underlying chain detector
    /// (`CHAIN-02 stage="underlying_not_served"`, `feed = "groww"`).
    /// The Groww SPOT leg deliberately has NO per-SID detector — the
    /// legacy leg had only the VIX-specific arms (`vix_unresolved`,
    /// ported into the Groww executor's identity resolution; the
    /// sweep-time `vix_not_served` belongs to the Groww post-session
    /// sweep, a named residual).
    groww_chain_served: GrowwChainServedTracker,
}

impl LaneEscalation {
    pub(crate) fn new(feed: Feed) -> Self {
        Self {
            feed,
            spot: LegTally::default(),
            chain: LegTally::default(),
            spot_targets: MinuteTargets::default(),
            chain_targets: MinuteTargets::default(),
            dhan_spot_served: SidServedTracker::default(),
            dhan_chain_served: DhanChainServedTracker::default(),
            groww_chain_served: GrowwChainServedTracker::default(),
        }
    }

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

    /// Record one PER-TARGET fetch-level outcome for the not-served
    /// detectors; when the outcome rolls the minute bucket, the PREVIOUS
    /// minute finalizes and its per-target verdicts feed the reused
    /// legacy tracker for `(self.feed, leg)` — returning emit-ready
    /// actions. An `AuthHold` anywhere in the finalized batch HOLDs the
    /// whole minute (nothing fed, nothing reset). `(Groww, Spot)` is a
    /// structural no-op (no legacy detector existed).
    pub(crate) fn record_target(
        &mut self,
        leg: EscalationLeg,
        minute_secs: u32,
        symbol: &'static str,
        security_id: SecurityId,
        outcome: TargetOutcome,
    ) -> Option<(u32, Vec<NotServedEmit>)> {
        if matches!((self.feed, leg), (Feed::Groww, EscalationLeg::Spot)) {
            return None;
        }
        let bucket = match leg {
            EscalationLeg::Spot => &mut self.spot_targets,
            EscalationLeg::Chain => &mut self.chain_targets,
        };
        let (minute, batch) = bucket.record(minute_secs, symbol, security_id, outcome)?;
        let emits = self.finalize_not_served(leg, &batch);
        Some((minute, emits))
    }

    /// Feed one FINALIZED minute's batch into the lane's tracker and map
    /// the tracker-specific actions onto the unified emit shape.
    fn finalize_not_served(
        &mut self,
        leg: EscalationLeg,
        batch: &[(&'static str, SecurityId, TargetOutcome)],
    ) -> Vec<NotServedEmit> {
        if batch.is_empty() || batch.iter().any(|&(_, _, o)| o == TargetOutcome::AuthHold) {
            // Auth is a GLOBAL token condition — the whole minute HOLDs
            // (the legacy no-token / auth-abort / EntitlementStop arms).
            return Vec::new();
        }
        let any_served = batch.iter().any(|&(_, _, o)| o == TargetOutcome::Served);
        match (self.feed, leg) {
            (Feed::Dhan, EscalationLeg::Spot) => {
                let verdicts: Vec<(SecurityId, bool)> = batch
                    .iter()
                    .map(|&(_, sid, o)| (sid, o == TargetOutcome::Served))
                    .collect();
                let actions = self.dhan_spot_served.record_minute(&verdicts);
                batch
                    .iter()
                    .zip(actions)
                    .map(|(&(symbol, sid, o), (_, action))| NotServedEmit {
                        symbol,
                        security_id: sid,
                        counted: o != TargetOutcome::Served && any_served,
                        action: match action {
                            SidEdgeAction::None => NotServedAction::None,
                            SidEdgeAction::Page { consecutive } => {
                                NotServedAction::Page { consecutive }
                            }
                            SidEdgeAction::Recover { not_served_minutes } => {
                                NotServedAction::Recover { not_served_minutes }
                            }
                        },
                    })
                    .collect()
            }
            (Feed::Dhan, EscalationLeg::Chain) => {
                let verdicts: Vec<(&'static str, bool)> = batch
                    .iter()
                    .map(|&(symbol, _, o)| (symbol, o == TargetOutcome::Served))
                    .collect();
                let actions = self.dhan_chain_served.record_minute(&verdicts);
                batch
                    .iter()
                    .zip(actions)
                    .map(|(&(symbol, sid, o), (_, action))| NotServedEmit {
                        symbol,
                        security_id: sid,
                        counted: o != TargetOutcome::Served && any_served,
                        action: match action {
                            DhanUnderlyingEdgeAction::None => NotServedAction::None,
                            DhanUnderlyingEdgeAction::Page { consecutive } => {
                                NotServedAction::Page { consecutive }
                            }
                            DhanUnderlyingEdgeAction::Recover { not_served_minutes } => {
                                NotServedAction::Recover { not_served_minutes }
                            }
                        },
                    })
                    .collect()
            }
            (Feed::Groww, EscalationLeg::Chain) => {
                let verdicts: Vec<(&'static str, bool)> = batch
                    .iter()
                    .map(|&(symbol, _, o)| (symbol, o == TargetOutcome::Served))
                    .collect();
                let actions = self.groww_chain_served.record_minute(&verdicts);
                batch
                    .iter()
                    .zip(actions)
                    .map(|(&(symbol, sid, o), (_, action))| NotServedEmit {
                        symbol,
                        security_id: sid,
                        counted: o != TargetOutcome::Served && any_served,
                        action: match action {
                            GrowwUnderlyingEdgeAction::None => NotServedAction::None,
                            GrowwUnderlyingEdgeAction::Page { consecutive } => {
                                NotServedAction::Page { consecutive }
                            }
                            GrowwUnderlyingEdgeAction::Recover { not_served_minutes } => {
                                NotServedAction::Recover { not_served_minutes }
                            }
                        },
                    })
                    .collect()
            }
            // (Groww, Spot) is guarded in record_target; any future feed
            // has no legacy detector — nothing to emit.
            _ => Vec::new(),
        }
    }
}

/// Emit the not-served counters / coded logs / EXISTING typed Telegram
/// events for one FINALIZED minute's per-target verdicts. Field shapes
/// match the legacy emit sites EXACTLY (`SPOT1M-01
/// stage="sid_not_served"` / `CHAIN-02 stage="underlying_not_served"`;
/// Groww lines carry `feed = "groww"`; Dhan lines stay field-less per
/// the house grep-split convention). ZERO new NotificationEvent /
/// ErrorCode variants.
pub(crate) fn emit_not_served(
    feed: Feed,
    leg: EscalationLeg,
    minute_secs: u32,
    emits: &[NotServedEmit],
    notifier: Option<&Arc<NotificationService>>,
) {
    let minute_label = format_minute_ist_12h(minute_secs);
    for emit in emits {
        match (feed, leg) {
            (Feed::Dhan, EscalationLeg::Spot) => {
                if emit.counted {
                    metrics::counter!(
                        "tv_spot1m_sid_not_served_total", "symbol" => emit.symbol
                    )
                    .increment(1);
                }
                match emit.action {
                    NotServedAction::Page { consecutive } => {
                        error!(
                            code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                            stage = "sid_not_served",
                            sid = emit.security_id,
                            symbol = emit.symbol,
                            consecutive,
                            minute = %minute_label,
                            "SPOT1M-01: the vendor is not returning 1-minute candles \
                             for this index while the other indices succeed — paging \
                             once per SID (edge-latched; re-armed on this SID's \
                             recovery)"
                        );
                        if let Some(n) = notifier {
                            n.notify(NotificationEvent::Spot1mSidNotServed {
                                symbol: emit.symbol.to_string(),
                                consecutive_minutes: consecutive,
                            });
                        }
                    }
                    NotServedAction::Recover { not_served_minutes } => {
                        info!(
                            sid = emit.security_id,
                            symbol = emit.symbol,
                            not_served_minutes,
                            minute = %minute_label,
                            "cadence: this index is being served again after a \
                             paged not-served episode"
                        );
                        if let Some(n) = notifier {
                            n.notify(NotificationEvent::Spot1mSidServedRecovered {
                                symbol: emit.symbol.to_string(),
                                not_served_minutes,
                            });
                        }
                    }
                    NotServedAction::None => {}
                }
            }
            (Feed::Dhan, EscalationLeg::Chain) => {
                if emit.counted {
                    metrics::counter!(
                        "tv_chain1m_underlying_not_served_total", "underlying" => emit.symbol
                    )
                    .increment(1);
                }
                match emit.action {
                    NotServedAction::Page { consecutive } => {
                        error!(
                            code = ErrorCode::Chain02FetchDegraded.code_str(),
                            stage = "underlying_not_served",
                            underlying = emit.symbol,
                            consecutive_minutes = consecutive,
                            minute = %minute_label,
                            "CHAIN-02: Dhan is not serving this underlying's option \
                             chain while the other underlyings succeed — paging once \
                             per underlying (edge-latched; re-armed on this \
                             underlying's own recovery)"
                        );
                        if let Some(n) = notifier {
                            n.notify(NotificationEvent::Chain1mUnderlyingNotServed {
                                underlying: emit.symbol,
                                empty_minutes: consecutive,
                            });
                        }
                    }
                    NotServedAction::Recover { not_served_minutes } => {
                        info!(
                            underlying = emit.symbol,
                            not_served_minutes,
                            minute = %minute_label,
                            "cadence: this underlying's Dhan chain is being served \
                             again after a paged not-served episode"
                        );
                        if let Some(n) = notifier {
                            n.notify(NotificationEvent::Chain1mUnderlyingServedRecovered {
                                underlying: emit.symbol,
                                empty_minutes: not_served_minutes,
                            });
                        }
                    }
                    NotServedAction::None => {}
                }
            }
            (Feed::Groww, EscalationLeg::Chain) => {
                if emit.counted {
                    metrics::counter!(
                        "tv_groww_chain1m_underlying_not_served_total",
                        "underlying" => emit.symbol
                    )
                    .increment(1);
                }
                match emit.action {
                    NotServedAction::Page { consecutive } => {
                        error!(
                            code = ErrorCode::Chain02FetchDegraded.code_str(),
                            stage = "underlying_not_served",
                            feed = "groww",
                            underlying = emit.symbol,
                            consecutive_minutes = consecutive,
                            minute = %minute_label,
                            "CHAIN-02: Groww is not serving this underlying's option \
                             chain while the other underlyings succeed — paging once \
                             per underlying (edge-latched; re-armed on this \
                             underlying's own recovery)"
                        );
                        if let Some(n) = notifier {
                            n.notify(NotificationEvent::GrowwChain1mUnderlyingNotServed {
                                underlying: emit.symbol,
                                empty_minutes: consecutive,
                            });
                        }
                    }
                    NotServedAction::Recover { not_served_minutes } => {
                        info!(
                            underlying = emit.symbol,
                            not_served_minutes,
                            minute = %minute_label,
                            "cadence: this underlying's Groww chain is being served \
                             again after a paged not-served episode"
                        );
                        if let Some(n) = notifier {
                            n.notify(NotificationEvent::GrowwChain1mUnderlyingServedRecovered {
                                underlying: emit.symbol,
                                empty_minutes: not_served_minutes,
                            });
                        }
                    }
                    NotServedAction::None => {}
                }
            }
            // (Groww, Spot) never produces emits (no legacy detector);
            // future feeds likewise.
            _ => {}
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
        let mut lane = LaneEscalation::new(Feed::Dhan);
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
        let mut lane = LaneEscalation::new(Feed::Dhan);
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
        let mut lane = LaneEscalation::new(Feed::Dhan);
        let t = SPOT_1M_REST_CONSECUTIVE_FAIL_PAGE_THRESHOLD;
        for i in 0..=t {
            lane.record(EscalationLeg::Spot, minute(i), false);
        }
        // Minute t succeeds (ok outcome), finalized by minute t+1's first
        // outcome → Recover.
        let mut lane2 = LaneEscalation::new(Feed::Dhan);
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
        let mut lane = LaneEscalation::new(Feed::Dhan);
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
        let mut lane = LaneEscalation::new(Feed::Dhan);
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
        let mut lane = LaneEscalation::new(Feed::Dhan);
        lane.record(EscalationLeg::Spot, minute(0), false);
        // A chain outcome for minute 1 must NOT finalize the spot bucket.
        assert_eq!(lane.record(EscalationLeg::Chain, minute(1), true), None);
        assert_eq!(
            lane.record(EscalationLeg::Spot, minute(1), true),
            Some((minute(0), EdgeAction::None))
        );
    }

    const NS_T: u32 = tickvault_common::constants::SPOT_1M_REST_SID_NOT_SERVED_THRESHOLD;

    fn rec_spot(
        lane: &mut LaneEscalation,
        i: u32,
        symbol: &'static str,
        sid: SecurityId,
        outcome: TargetOutcome,
    ) -> Option<(u32, Vec<NotServedEmit>)> {
        lane.record_target(EscalationLeg::Spot, minute(i), symbol, sid, outcome)
    }

    fn page_for(emits: &[NotServedEmit], symbol: &str) -> Option<u32> {
        emits.iter().find_map(|e| match e.action {
            NotServedAction::Page { consecutive } if e.symbol == symbol => Some(consecutive),
            _ => None,
        })
    }

    fn recover_for(emits: &[NotServedEmit], symbol: &str) -> Option<u32> {
        emits.iter().find_map(|e| match e.action {
            NotServedAction::Recover { not_served_minutes } if e.symbol == symbol => {
                Some(not_served_minutes)
            }
            _ => None,
        })
    }

    #[test]
    fn test_target_outcome_of_taxonomy() {
        assert_eq!(
            target_outcome_of(&Ok::<(), _>(())),
            Some(TargetOutcome::Served)
        );
        assert_eq!(
            target_outcome_of::<()>(&Err(CadenceFetchError::Auth)),
            Some(TargetOutcome::AuthHold)
        );
        // Self-inflicted pacing is NEVER a vendor verdict — per-target HOLD.
        assert_eq!(
            target_outcome_of::<()>(&Err(CadenceFetchError::QueueDelay)),
            None
        );
        for err in [
            CadenceFetchError::Empty,
            CadenceFetchError::Timeout,
            CadenceFetchError::Transport,
            CadenceFetchError::Malformed,
            CadenceFetchError::RateLimited {
                retry_after_ms: None,
            },
        ] {
            assert_eq!(
                target_outcome_of::<()>(&Err(err)),
                Some(TargetOutcome::NotServed)
            );
        }
    }

    #[test]
    fn test_not_served_counts_only_with_sibling_success_and_pages_at_threshold() {
        let mut lane = LaneEscalation::new(Feed::Dhan);
        let mut pages = 0u32;
        for i in 0..=NS_T {
            let fin = rec_spot(&mut lane, i, "NIFTY", 13, TargetOutcome::Served);
            if let Some((m, emits)) = fin {
                assert_eq!(m, minute(i - 1));
                // The failing SID's minute COUNTED (a sibling served).
                let vix = emits.iter().find(|e| e.symbol == "INDIA VIX").unwrap();
                assert!(vix.counted, "sibling-ok minute must count");
                let nifty = emits.iter().find(|e| e.symbol == "NIFTY").unwrap();
                assert!(!nifty.counted, "a served SID never counts");
                if let Some(consecutive) = page_for(&emits, "INDIA VIX") {
                    pages += 1;
                    assert_eq!(consecutive, NS_T);
                    assert_eq!(
                        i, NS_T,
                        "page fires when the NS_T-th counted minute finalizes"
                    );
                }
            }
            rec_spot(&mut lane, i, "INDIA VIX", 21, TargetOutcome::NotServed);
        }
        assert_eq!(pages, 1, "rising edge pages exactly once");
    }

    #[test]
    fn test_not_served_global_failure_minutes_neither_count_nor_reset() {
        let mut lane = LaneEscalation::new(Feed::Dhan);
        let mut pages = 0u32;
        // 5 counted minutes, 3 global-failure minutes (HOLD), then more
        // counted minutes: the streak survives the hold and pages at the
        // 10th COUNTED minute.
        let mut counted = 0u32;
        for i in 0..40u32 {
            let global_fail = (5..8).contains(&i);
            let nifty_outcome = if global_fail {
                TargetOutcome::NotServed
            } else {
                TargetOutcome::Served
            };
            if let Some((_, emits)) = rec_spot(&mut lane, i, "NIFTY", 13, nifty_outcome) {
                let vix = emits.iter().find(|e| e.symbol == "INDIA VIX").unwrap();
                if vix.counted {
                    counted += 1;
                }
                if page_for(&emits, "INDIA VIX").is_some() {
                    pages += 1;
                    assert_eq!(counted, NS_T, "page fires at the 10th COUNTED minute");
                    break;
                }
            }
            rec_spot(&mut lane, i, "INDIA VIX", 21, TargetOutcome::NotServed);
        }
        assert_eq!(pages, 1);
    }

    #[test]
    fn test_not_served_auth_hold_skips_the_whole_minute() {
        let mut lane = LaneEscalation::new(Feed::Dhan);
        let mut pages = 0u32;
        let mut counted = 0u32;
        for i in 0..40u32 {
            // Minutes 3..6: the sibling hits an Auth-class failure — the
            // WHOLE minute holds (no counts for anyone, no reset).
            let nifty_outcome = if (3..6).contains(&i) {
                TargetOutcome::AuthHold
            } else {
                TargetOutcome::Served
            };
            if let Some((_, emits)) = rec_spot(&mut lane, i, "NIFTY", 13, nifty_outcome) {
                if emits.is_empty() {
                    // held minute — nothing fed
                } else {
                    let vix = emits.iter().find(|e| e.symbol == "INDIA VIX").unwrap();
                    if vix.counted {
                        counted += 1;
                    }
                    if page_for(&emits, "INDIA VIX").is_some() {
                        pages += 1;
                        assert_eq!(counted, NS_T);
                        break;
                    }
                }
            }
            rec_spot(&mut lane, i, "INDIA VIX", 21, TargetOutcome::NotServed);
        }
        assert_eq!(pages, 1, "auth-held minutes neither count nor reset");
    }

    #[test]
    fn test_not_served_recovery_re_arms_the_latch() {
        let mut lane = LaneEscalation::new(Feed::Dhan);
        // Drive to a page.
        for i in 0..=NS_T {
            rec_spot(&mut lane, i, "NIFTY", 13, TargetOutcome::Served);
            rec_spot(&mut lane, i, "INDIA VIX", 21, TargetOutcome::NotServed);
        }
        // VIX serves at minute NS_T+1 → Recover when it finalizes.
        rec_spot(&mut lane, NS_T + 1, "NIFTY", 13, TargetOutcome::Served);
        rec_spot(&mut lane, NS_T + 1, "INDIA VIX", 21, TargetOutcome::Served);
        let (_, emits) = rec_spot(&mut lane, NS_T + 2, "NIFTY", 13, TargetOutcome::Served)
            .expect("rollover finalizes");
        assert!(
            recover_for(&emits, "INDIA VIX").is_some(),
            "falling edge emits Recover"
        );
        // A fresh full streak pages AGAIN (the latch re-armed).
        rec_spot(
            &mut lane,
            NS_T + 2,
            "INDIA VIX",
            21,
            TargetOutcome::NotServed,
        );
        let mut paged_again = false;
        for i in (NS_T + 3)..(2 * NS_T + 4) {
            if let Some((_, emits)) = rec_spot(&mut lane, i, "NIFTY", 13, TargetOutcome::Served)
                && page_for(&emits, "INDIA VIX").is_some()
            {
                paged_again = true;
                break;
            }
            rec_spot(&mut lane, i, "INDIA VIX", 21, TargetOutcome::NotServed);
        }
        assert!(paged_again, "edge re-arms after recovery");
    }

    #[test]
    fn test_not_served_retry_outcome_replaces_first_pass() {
        let mut lane = LaneEscalation::new(Feed::Dhan);
        // First pass rate-limited, in-cycle retry served — the minute
        // finalizes as SERVED (the §2 retry-replaces contract).
        rec_spot(&mut lane, 0, "NIFTY", 13, TargetOutcome::Served);
        rec_spot(&mut lane, 0, "INDIA VIX", 21, TargetOutcome::NotServed);
        rec_spot(&mut lane, 0, "INDIA VIX", 21, TargetOutcome::Served);
        let (_, emits) =
            rec_spot(&mut lane, 1, "NIFTY", 13, TargetOutcome::Served).expect("finalize");
        let vix = emits.iter().find(|e| e.symbol == "INDIA VIX").unwrap();
        assert!(!vix.counted, "retry-served minute never counts");
    }

    #[test]
    fn test_groww_spot_has_no_not_served_detector() {
        let mut lane = LaneEscalation::new(Feed::Groww);
        for i in 0..40u32 {
            assert_eq!(
                lane.record_target(
                    EscalationLeg::Spot,
                    minute(i),
                    "INDIA VIX",
                    21,
                    TargetOutcome::NotServed,
                ),
                None,
                "the legacy Groww spot leg had only the VIX arms — no per-SID detector"
            );
        }
    }

    #[test]
    fn test_groww_chain_not_served_pages_via_the_groww_tracker() {
        let mut lane = LaneEscalation::new(Feed::Groww);
        let mut pages = 0u32;
        for i in 0..=NS_T {
            if let Some((_, emits)) = lane.record_target(
                EscalationLeg::Chain,
                minute(i),
                "BANKNIFTY",
                0,
                TargetOutcome::Served,
            ) && page_for(&emits, "NIFTY").is_some()
            {
                pages += 1;
            }
            lane.record_target(
                EscalationLeg::Chain,
                minute(i),
                "NIFTY",
                0,
                TargetOutcome::NotServed,
            );
        }
        assert_eq!(pages, 1, "the Groww chain lane pages at the threshold");
    }
}

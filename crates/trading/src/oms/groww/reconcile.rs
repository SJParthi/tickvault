//! Pure reconciliation primitives (design §4.9) — NO I/O, no clock, no
//! transport. The periodic reconciler task (ORD-PR-3/5) feeds these with
//! the ledger view + local order twins + the `/v1/order/list` snapshot rows
//! and acts on the returned classifications.
//!
//! Six mismatch kinds (three-input mirror of the Dhan
//! `reconciliation.rs:42-46` shape, + the HIGH-5 prior-day arm):
//! - `status_drift` — broker truth via the §4.6 machine; reconcile-sourced
//!   backward = stale-skip (2-sweep escalation);
//! - `fill_drift` — INTEGER compare (the §37.2 house precedent — never a
//!   float epsilon);
//! - `ghost_local` — locally tracked, absent at the broker: minimum-age
//!   grace (older than one full sweep interval) AND absent on 2 consecutive
//!   sweeps before ANY Expired-class action (hostile F-6b/F-13);
//! - `ghost_broker` — at the broker, unknown locally: our `TV…` reference ⇒
//!   ADOPT + audit (crash-recovered order); foreign reference = BruteX
//!   co-tenant order on the shared account ⇒ counted + IGNORED, never
//!   touched;
//! - `orphan_intent` — an open ledger intent with no tracked order ⇒ the
//!   §4.7 resolution ladder;
//! - `prior_day_needs_per_order_read` — a prior-day non-terminal local
//!   order absent from TODAY's day-scoped list ⇒ per-order read, NEVER an
//!   Expired-class stamp (HIGH-5 / doc-F1).
//!
//! Also here: the pure EOD force-terminalize classification (15:35 IST
//! sweep — Groww has NO `EXPIRED` status, post-close wire status is a day-0
//! probe row; hostile F-7) and the cross-close recovery classification
//! (`/v1/order/list` is "Day's orders" — prior-day truth must come from
//! per-order reads; unreadable ⇒ unresolved + Critical, NEVER a silent
//! Expired stamp — doc-fidelity F1).

use std::collections::{BTreeMap, BTreeSet};

use super::intent_ledger::{BootIntentClass, IntentSummary};
use super::reference_id::{self, IstDate};
use super::state::{
    ObservationSource, OrderObservation, TrackedOrderState, TransitionDecision, TransitionOutcome,
    evaluate_transition, is_fill_class, is_terminal,
};
use super::types::GrowwOrderStatus;

/// Consecutive sweeps a locally-tracked order must be ABSENT from the
/// broker snapshot before any Expired-class action (hostile F-6b).
pub const GHOST_LOCAL_CONFIRM_SWEEPS: u8 = 2;

/// The reconcile mismatch kinds (design §4.9; five original + the HIGH-5
/// prior-day arm).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MismatchKind {
    /// Broker status disagrees beyond the stale-skip envelope.
    StatusDrift,
    /// Broker fill quantity disagrees (integer compare).
    FillDrift,
    /// Locally tracked, absent at the broker past the grace rules.
    GhostLocal,
    /// Broker order unknown locally.
    GhostBroker,
    /// Open ledger intent with no tracked order.
    OrphanIntent,
    /// A PRIOR-DAY non-terminal local order absent from TODAY's day-scoped
    /// list (doc-F1: `/v1/order/list` is "Day's orders" — its absence proves
    /// NOTHING about a prior-day order). NEVER GhostLocal/Expired-class:
    /// truth must come from a per-order read (HIGH-5).
    PriorDayNeedsPerOrderRead,
}

impl MismatchKind {
    /// Stable audit/metric label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StatusDrift => "status_drift",
            Self::FillDrift => "fill_drift",
            Self::GhostLocal => "ghost_local",
            Self::GhostBroker => "ghost_broker",
            Self::OrphanIntent => "orphan_intent",
            Self::PriorDayNeedsPerOrderRead => "prior_day_needs_per_order_read",
        }
    }
}

/// A locally tracked order, as the reconciler sees it (pure view).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalOrderView {
    /// Broker-assigned order id.
    pub groww_order_id: String,
    /// Our reference id for the order's place intent.
    pub reference_id: String,
    /// The §4.6 tracked state.
    pub state: TrackedOrderState,
    /// IST trading date the order was placed on, as the 6-digit `yymmdd`
    /// block (`yy*10_000 + mm*100 + dd`) — the HIGH-5 prior-day gate input.
    pub trading_date_yymmdd: u32,
    /// When the order was locally created (epoch ms) — the min-age grace
    /// input.
    pub created_ts_ms: i64,
    /// Consecutive PRIOR sweeps this order was absent from the snapshot.
    pub missing_sweeps: u8,
}

/// One `/v1/order/list` snapshot row, reduced to the reconcile-relevant
/// subset (every field `Option` — the wire guarantees nothing).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SnapshotRow {
    /// Broker order id.
    pub groww_order_id: Option<String>,
    /// Raw status string.
    pub order_status: Option<String>,
    /// Cumulative filled quantity.
    pub filled_quantity: Option<i64>,
    /// Echoed reference id.
    pub order_reference_id: Option<String>,
}

/// Reconcile parameters (caller-resolved; no clock here).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileParams {
    /// "Now" in epoch ms (the caller's clock).
    pub now_ms: i64,
    /// The sweep interval in ms (60s active / 300s idle — config).
    pub sweep_interval_ms: i64,
    /// TODAY's IST trading date as the 6-digit `yymmdd` block — the HIGH-5
    /// prior-day gate reference (`IstDate::yymmdd_num`).
    pub today_yymmdd: u32,
}

/// Per-tracked-order reconcile outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderReconcileOutcome {
    /// The order this outcome concerns.
    pub groww_order_id: String,
    /// The §4.6 decision for the snapshot observation (`None` when the
    /// order was absent from the snapshot).
    pub decision: Option<TransitionDecision>,
    /// A mismatch to report/audit, if any.
    pub finding: Option<MismatchKind>,
    /// The new consecutive-missing-sweeps counter to store back.
    pub next_missing_sweeps: u8,
    /// Ghost-local classification detail (set only with a
    /// `GhostLocal`-related verdict).
    pub ghost_local: Option<GhostLocalVerdict>,
}

/// Ghost-local grace classification (hostile F-6b/F-13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GhostLocalVerdict {
    /// Within the grace envelope (too young, or not yet absent on
    /// [`GHOST_LOCAL_CONFIRM_SWEEPS`] consecutive sweeps) — counted, no
    /// action.
    Grace,
    /// Grace exhausted — Expired-class action may proceed (audited).
    Confirmed,
}

/// A broker-side row with no local twin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhostBrokerOutcome {
    /// The broker order id (when the row carried one).
    pub groww_order_id: Option<String>,
    /// What to do with it.
    pub action: GhostBrokerAction,
}

/// Ghost-broker disposition (design §4.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GhostBrokerAction {
    /// Carries OUR `TV…` reference — a crash-recovered order: ADOPT + audit.
    Adopt,
    /// Foreign reference (BruteX co-tenant) — counted + IGNORED, never
    /// touched.
    ForeignIgnored,
}

/// The full reconcile report (pure output).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReconcileReport {
    /// Per-tracked-order outcomes (BTreeMap: deterministic iteration).
    pub orders: BTreeMap<String, OrderReconcileOutcome>,
    /// Broker rows with no local twin.
    pub ghost_broker: Vec<GhostBrokerOutcome>,
    /// Open ledger intents with no tracked order → the resolution ladder.
    pub orphan_intents: Vec<String>,
    /// Foreign (co-tenant) rows counted this sweep.
    pub foreign_ignored: usize,
    /// Reconcile-sourced stale-snapshot skips this sweep.
    pub stale_snapshot_skips: usize,
}

/// The pure three-input reconcile (design §4.9): local twins vs the
/// snapshot rows vs the open-intent ledger view. Deterministic; no I/O.
///
/// The caller applies: legal decisions (adopt status/fill), findings
/// (audit + GROWW-ORD-04-class visibility — emits land in ORD-PR-3),
/// ghost-broker adoptions, and routes orphan intents to the ladder.
#[must_use]
pub fn reconcile_groww_orders(
    local: &[LocalOrderView],
    snapshot: &[SnapshotRow],
    open_intents: &[IntentSummary],
    params: ReconcileParams,
) -> ReconcileReport {
    let mut report = ReconcileReport::default();

    // Index the snapshot by order id (rows without an id can never match a
    // local twin; they fall through to ghost-broker handling below).
    // NOTE: keying on `groww_order_id` ASSUMES it is unique across the
    // CASH/FNO segments (Unknown — day-0 probe item); a cross-segment id
    // collision would collapse two rows into one here.
    let mut snap_by_id: BTreeMap<&str, &SnapshotRow> = BTreeMap::new();
    for row in snapshot {
        if let Some(id) = row.groww_order_id.as_deref() {
            snap_by_id.insert(id, row);
        }
    }

    // --- Tracked orders vs snapshot.
    for order in local {
        let outcome = match snap_by_id.get(order.groww_order_id.as_str()) {
            Some(row) => {
                let obs = OrderObservation {
                    status: row.order_status.as_deref().map_or_else(
                        || GrowwOrderStatus::Unknown(String::new()),
                        GrowwOrderStatus::parse,
                    ),
                    filled_quantity: row.filled_quantity,
                    source: ObservationSource::Reconcile,
                };
                let decision = evaluate_transition(&order.state, &obs);
                if decision.outcome == TransitionOutcome::StaleSnapshotSkip {
                    report.stale_snapshot_skips += 1;
                }
                let finding = classify_snapshot_finding(&decision, &order.state, row);
                OrderReconcileOutcome {
                    groww_order_id: order.groww_order_id.clone(),
                    decision: Some(decision),
                    finding,
                    next_missing_sweeps: 0, // present this sweep
                    ghost_local: None,
                }
            }
            None => {
                // Terminal local orders legitimately age out of the day
                // snapshot — never a ghost.
                if is_terminal(&order.state.status) {
                    OrderReconcileOutcome {
                        groww_order_id: order.groww_order_id.clone(),
                        decision: None,
                        finding: None,
                        next_missing_sweeps: 0,
                        ghost_local: None,
                    }
                } else if order.trading_date_yymmdd != params.today_yymmdd {
                    // HIGH-5: a PRIOR-DAY non-terminal order absent from
                    // TODAY's day-scoped list proves NOTHING (doc-F1) —
                    // NEVER GhostLocal/Expired; route to a per-order read.
                    // The missing-sweeps counter is meaningless for it and
                    // is left untouched.
                    OrderReconcileOutcome {
                        groww_order_id: order.groww_order_id.clone(),
                        decision: None,
                        finding: Some(MismatchKind::PriorDayNeedsPerOrderRead),
                        next_missing_sweeps: order.missing_sweeps,
                        ghost_local: None,
                    }
                } else {
                    let next_missing = order.missing_sweeps.saturating_add(1);
                    let verdict = classify_ghost_local(
                        params.now_ms.saturating_sub(order.created_ts_ms),
                        params.sweep_interval_ms,
                        next_missing,
                    );
                    OrderReconcileOutcome {
                        groww_order_id: order.groww_order_id.clone(),
                        decision: None,
                        finding: (verdict == GhostLocalVerdict::Confirmed)
                            .then_some(MismatchKind::GhostLocal),
                        next_missing_sweeps: next_missing,
                        ghost_local: Some(verdict),
                    }
                }
            }
        };
        // LOW: key from the outcome's own id — one clone, not two.
        report
            .orders
            .insert(outcome.groww_order_id.clone(), outcome);
    }

    // --- Snapshot rows with no local twin (ghost_broker).
    for row in snapshot {
        let known_locally = row
            .groww_order_id
            .as_deref()
            .is_some_and(|id| report.orders.contains_key(id));
        if known_locally {
            continue;
        }
        let ours = row
            .order_reference_id
            .as_deref()
            .is_some_and(reference_id::is_ours);
        // LOW: Adopt REQUIRES a broker order id — an id-less row cannot be
        // adopted (nothing to poll); our-reference-but-id-less rows are
        // ignored WITHOUT inflating the foreign counter.
        let action = if ours && row.groww_order_id.is_some() {
            GhostBrokerAction::Adopt
        } else {
            if !ours {
                report.foreign_ignored += 1;
            }
            GhostBrokerAction::ForeignIgnored
        };
        report.ghost_broker.push(GhostBrokerOutcome {
            groww_order_id: row.groww_order_id.clone(),
            action,
        });
    }

    // --- Open intents with no tracked order (orphan_intent → ladder).
    // LOW: dedup on intent_id — a caller-duplicated summary must not route
    // to the ladder twice.
    let mut orphan_seen: BTreeSet<&str> = BTreeSet::new();
    for intent in open_intents {
        let tracked = intent
            .groww_order_id
            .as_deref()
            .is_some_and(|id| report.orders.contains_key(id));
        if !tracked && orphan_seen.insert(intent.intent_id.as_str()) {
            report.orphan_intents.push(intent.intent_id.clone());
        }
    }

    report
}

/// Reset the per-order missing-sweeps counters at SESSION OPEN (HIGH-5): a
/// new day's list starts empty, so counters accumulated against yesterday's
/// snapshots must never carry into today's GhostLocal confirmation. The
/// executor calls this once at the session boundary (ORD-PR-3/5).
pub fn reset_missing_sweeps_at_session_open(orders: &mut [LocalOrderView]) {
    for o in orders {
        o.missing_sweeps = 0;
    }
}

/// Map a §4.6 decision on a reconcile observation to a mismatch finding.
fn classify_snapshot_finding(
    decision: &TransitionDecision,
    _local: &TrackedOrderState,
    _row: &SnapshotRow,
) -> Option<MismatchKind> {
    if decision.needs_reconciliation {
        // Escalated stale reconcile / illegal move: fill-driven violations
        // classify as fill drift, status-driven as status drift.
        if decision.fill_monotonicity_violation {
            return Some(MismatchKind::FillDrift);
        }
        return Some(MismatchKind::StatusDrift);
    }
    None
}

/// The ghost-local grace rule (hostile F-6b/F-13): Expired-class action
/// ONLY when the order is OLDER than one full sweep interval AND has been
/// absent on [`GHOST_LOCAL_CONFIRM_SWEEPS`] consecutive sweeps (the count
/// INCLUDING the current one).
#[must_use]
pub fn classify_ghost_local(
    age_ms: i64,
    sweep_interval_ms: i64,
    missing_sweeps_including_current: u8,
) -> GhostLocalVerdict {
    if age_ms > sweep_interval_ms && missing_sweeps_including_current >= GHOST_LOCAL_CONFIRM_SWEEPS
    {
        GhostLocalVerdict::Confirmed
    } else {
        GhostLocalVerdict::Grace
    }
}

// ---------------------------------------------------------------------------
// EOD force-terminalize (hostile F-7) + cross-close recovery (doc-F1)
// ---------------------------------------------------------------------------

/// EOD (15:35 IST) sweep classification for one tracked order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EodAction {
    /// Genuinely OPEN-class non-terminal at EOD with ZERO fill:
    /// force-terminalize as Expired-class + audit row (mode-tagged);
    /// per-order polling stops until the next session. Groww has NO
    /// `EXPIRED` wire status — this is a LOCAL classification; the
    /// post-close wire status is a day-0 probe row (F-7).
    ForceExpireLocal,
    /// OPEN-class non-terminal at EOD with `filled_quantity > 0`
    /// (R2-MEDIUM-HIGH-2): Groww has NO `PARTIAL` wire status — a partial
    /// fill presents as an open-class status carrying a positive fill
    /// count. The ORDER is Expired-class locally (it will never fill
    /// further this session), but the carried fill means a POSITION EXISTS
    /// — this arm demands the position-exists page (hostile F-10
    /// semantics; distinct from [`EodAction::FilledAwaitSettlement`],
    /// which is status-driven). NEVER a plain `ForceExpireLocal`: that
    /// would hide a real partial position.
    ForceExpireWithPartialFill {
        /// The cumulative filled quantity at EOD (the position size the
        /// F-10 page must carry).
        filled_quantity: i64,
    },
    /// Already terminal — nothing to do.
    AlreadyTerminal,
    /// Fill-class non-terminal (`EXECUTED` / `DELIVERY_AWAITED`) at EOD
    /// (HIGH-2): a POSITION EXISTS — locally terminal-WITH-FILL, carrying
    /// the position-exists page semantics of hostile F-10. NEVER
    /// force-expired: an executed order is not "expired", and stamping it
    /// Expired would hide the live position.
    FilledAwaitSettlement,
    /// `Unknown(_)` at EOD (HIGH-2): we cannot assert the order is
    /// open-class from a string we did not understand — Critical route
    /// (operator/reconcile owns it), NEVER a silent Expired stamp.
    // GROWW-ORD-07-class emit lands in ORD-PR-3 once the variants exist.
    UnknownAtEodCritical,
}

/// Classify one order for the EOD sweep — pure, total.
#[must_use]
pub fn classify_eod_action(state: &TrackedOrderState) -> EodAction {
    if is_terminal(&state.status) {
        return EodAction::AlreadyTerminal;
    }
    // Non-terminal fill class (EXECUTED / DELIVERY_AWAITED — COMPLETED is
    // terminal and already handled above).
    if is_fill_class(&state.status) {
        return EodAction::FilledAwaitSettlement;
    }
    if matches!(state.status, GrowwOrderStatus::Unknown(_)) {
        return EodAction::UnknownAtEodCritical;
    }
    // R2-MEDIUM-HIGH-2: an open-class order carrying a positive fill is a
    // PARTIAL (Groww has no PARTIAL status — the fill count is the only
    // signal); the position-exists page must ride the expiry.
    if state.filled_quantity > 0 {
        return EodAction::ForceExpireWithPartialFill {
            filled_quantity: state.filled_quantity,
        };
    }
    EodAction::ForceExpireLocal
}

/// Cross-close (next-morning boot) disposition of a replayed intent
/// (doc-fidelity F1: `/v1/order/list` is "Day's orders" — NEVER prior-day
/// truth; prior-date intents resolve per-order via status-by-reference /
/// detail, and an UNREADABLE prior-day intent routes to `unresolved` +
/// Critical — never a silent Expired stamp: a prior-day EXECUTED order is a
/// live position).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrossCloseAction {
    /// Same-day intent — the normal boot protocol (ladder before mutations).
    SameDayBootProtocol,
    /// Prior-day `recorded`-only — provably never sent; settle
    /// `resolved_not_landed(never_sent)`.
    SettleNeverSent,
    /// Prior-day open intent — resolve PER-ORDER via reference/detail reads
    /// (never the day-scoped list).
    ResolvePerOrderReads,
    /// Prior-day intent whose state cannot be read — `unresolved` +
    /// Critical; NEVER silently expired.
    // GROWW-ORD-03 emit lands in ORD-PR-3 once the variants exist.
    MarkUnresolvedCritical,
    /// Terminal — nothing to do.
    Settled,
}

/// Classify a replayed intent across the close boundary — pure.
///
/// `intent_yymmdd` / `today_yymmdd` are the 6-digit IST date blocks
/// (`yy*10_000 + mm*100 + dd`); `prior_day_readable` reports whether the
/// per-order read path answered for this intent (day-0 probe D-10 decides
/// whether it CAN — until proven, callers pass `false` and every prior-day
/// open intent routes to Critical, the fail-closed default).
#[must_use]
pub fn classify_cross_close_intent(
    intent_yymmdd: u32,
    today_yymmdd: u32,
    class: BootIntentClass,
    prior_day_readable: bool,
) -> CrossCloseAction {
    if class == BootIntentClass::Settled {
        return CrossCloseAction::Settled;
    }
    if intent_yymmdd == today_yymmdd {
        return CrossCloseAction::SameDayBootProtocol;
    }
    match class {
        BootIntentClass::NeverSent => CrossCloseAction::SettleNeverSent,
        BootIntentClass::NeedsResolution | BootIntentClass::NeedsOperator => {
            if prior_day_readable {
                CrossCloseAction::ResolvePerOrderReads
            } else {
                CrossCloseAction::MarkUnresolvedCritical
            }
        }
        BootIntentClass::Settled => CrossCloseAction::Settled,
    }
}

/// The REAL PRODUCER for [`classify_cross_close_intent`] (HIGH-4): classify
/// every replayed OPEN intent (the multi-file
/// [`super::intent_ledger::IntentLedger::classify_open_intents`] output —
/// prior-day files included since the day-rollover replay) across the close
/// boundary. Each intent's date comes from its OWN reference id
/// (`reference_id::decompose` — every intent id we journal is our generated
/// shape); a non-decomposable reference (unreachable for our own intents)
/// fail-closes to `MarkUnresolvedCritical` — we cannot prove it is same-day.
#[must_use]
pub fn classify_cross_close_open_intents(
    open: &[(IntentSummary, BootIntentClass)],
    today: IstDate,
    prior_day_readable: bool,
) -> Vec<(IntentSummary, CrossCloseAction)> {
    let today_yymmdd = today.yymmdd_num();
    open.iter()
        .map(|(summary, class)| {
            let action = match reference_id::decompose(&summary.reference_id) {
                Some(parts) => classify_cross_close_intent(
                    parts.yymmdd,
                    today_yymmdd,
                    *class,
                    prior_day_readable,
                ),
                None => CrossCloseAction::MarkUnresolvedCritical,
            };
            (summary.clone(), action)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oms::groww::intent_ledger::IntentKind;
    use crate::oms::groww::reference_id::{IstDate, generate_reference_id};

    const DATE: IstDate = IstDate {
        year: 2026,
        month: 7,
        day: 15,
    };
    const PARAMS: ReconcileParams = ReconcileParams {
        now_ms: 1_760_000_600_000,
        sweep_interval_ms: 60_000,
        today_yymmdd: 260_715,
    };

    fn local(
        id: &str,
        status: GrowwOrderStatus,
        fill: i64,
        created_ts_ms: i64,
        missing_sweeps: u8,
    ) -> LocalOrderView {
        LocalOrderView {
            groww_order_id: id.to_owned(),
            reference_id: generate_reference_id(DATE, 1, 1),
            state: TrackedOrderState::new(status, fill),
            trading_date_yymmdd: PARAMS.today_yymmdd,
            created_ts_ms,
            missing_sweeps,
        }
    }

    fn row(id: &str, status: &str, fill: i64, reference: Option<&str>) -> SnapshotRow {
        SnapshotRow {
            groww_order_id: Some(id.to_owned()),
            order_status: Some(status.to_owned()),
            filled_quantity: Some(fill),
            order_reference_id: reference.map(str::to_owned),
        }
    }

    // --- consistent snapshot: clean transition, no findings ---

    #[test]
    fn test_consistent_snapshot_yields_transition_no_finding() {
        let locals = [local("G1", GrowwOrderStatus::Open, 0, 1_760_000_000_000, 0)];
        let snap = [row("G1", "EXECUTED", 50, None)];
        let report = reconcile_groww_orders(&locals, &snap, &[], PARAMS);
        let o = &report.orders["G1"];
        assert_eq!(
            o.decision.as_ref().map(|d| d.outcome),
            Some(TransitionOutcome::Transition)
        );
        assert_eq!(o.finding, None);
        assert_eq!(o.next_missing_sweeps, 0);
        assert!(report.ghost_broker.is_empty());
    }

    // --- stale-snapshot skip (P37): first sweep skip, second escalates ---

    #[test]
    fn test_stale_snapshot_skip_then_status_drift_on_second_sweep() {
        // Local is OPEN; the snapshot lags at ACKED (backward).
        let mut lo = local("G1", GrowwOrderStatus::Open, 0, 1_760_000_000_000, 0);
        let snap = [row("G1", "ACKED", 0, None)];
        let r1 = reconcile_groww_orders(std::slice::from_ref(&lo), &snap, &[], PARAMS);
        let o1 = &r1.orders["G1"];
        assert_eq!(
            o1.decision.as_ref().map(|d| d.outcome),
            Some(TransitionOutcome::StaleSnapshotSkip)
        );
        assert_eq!(o1.finding, None, "first stale sweep is grace");
        assert_eq!(r1.stale_snapshot_skips, 1);
        // Store back the escalation counter, sweep again.
        lo.state.consecutive_stale_reconcile = o1
            .decision
            .as_ref()
            .map_or(0, |d| d.next_stale_reconcile_count);
        let r2 = reconcile_groww_orders(&[lo], &snap, &[], PARAMS);
        let o2 = &r2.orders["G1"];
        assert_eq!(o2.finding, Some(MismatchKind::StatusDrift));
    }

    // --- fill drift ---

    #[test]
    fn test_fill_decrease_escalates_to_fill_drift_after_grace() {
        let mut lo = local("G1", GrowwOrderStatus::Open, 30, 1_760_000_000_000, 0);
        lo.state.consecutive_stale_reconcile = 1; // one stale sweep already
        let snap = [row("G1", "OPEN", 20, None)]; // fill went DOWN
        let report = reconcile_groww_orders(&[lo], &snap, &[], PARAMS);
        let o = &report.orders["G1"];
        assert_eq!(o.finding, Some(MismatchKind::FillDrift));
    }

    // --- ghost_local grace (P38): min-age AND 2 consecutive sweeps ---

    #[test]
    fn test_ghost_local_grace_then_confirmed() {
        // Milliseconds-old order absent from the snapshot: GRACE (age).
        let young = local("G1", GrowwOrderStatus::Open, 0, PARAMS.now_ms - 10, 5);
        let r = reconcile_groww_orders(&[young], &[], &[], PARAMS);
        assert_eq!(r.orders["G1"].ghost_local, Some(GhostLocalVerdict::Grace));
        assert_eq!(r.orders["G1"].finding, None);

        // Old order, FIRST absent sweep: GRACE (sweep count).
        let old_first = local("G2", GrowwOrderStatus::Open, 0, PARAMS.now_ms - 600_000, 0);
        let r = reconcile_groww_orders(&[old_first], &[], &[], PARAMS);
        assert_eq!(r.orders["G2"].ghost_local, Some(GhostLocalVerdict::Grace));
        assert_eq!(r.orders["G2"].next_missing_sweeps, 1);

        // Old order, SECOND consecutive absent sweep: CONFIRMED.
        let old_second = local("G3", GrowwOrderStatus::Open, 0, PARAMS.now_ms - 600_000, 1);
        let r = reconcile_groww_orders(&[old_second], &[], &[], PARAMS);
        assert_eq!(
            r.orders["G3"].ghost_local,
            Some(GhostLocalVerdict::Confirmed)
        );
        assert_eq!(r.orders["G3"].finding, Some(MismatchKind::GhostLocal));
    }

    #[test]
    fn test_terminal_local_absent_from_snapshot_is_not_a_ghost() {
        let done = local("G1", GrowwOrderStatus::Completed, 50, 1_760_000_000_000, 3);
        let r = reconcile_groww_orders(&[done], &[], &[], PARAMS);
        assert_eq!(r.orders["G1"].finding, None);
        assert_eq!(r.orders["G1"].ghost_local, None);
        assert_eq!(r.orders["G1"].next_missing_sweeps, 0);
    }

    #[test]
    fn test_presence_resets_missing_sweeps() {
        let lo = local("G1", GrowwOrderStatus::Open, 0, 1_760_000_000_000, 1);
        let snap = [row("G1", "OPEN", 0, None)];
        let r = reconcile_groww_orders(&[lo], &snap, &[], PARAMS);
        assert_eq!(r.orders["G1"].next_missing_sweeps, 0);
    }

    // --- ghost_broker (P39/P40): ours adopted, foreign ignored ---

    #[test]
    fn test_ghost_broker_ours_adopted_foreign_ignored() {
        let ours_ref = generate_reference_id(DATE, 7, 7);
        let snap = [
            row("GX", "OPEN", 0, Some(&ours_ref)),
            row("GY", "OPEN", 0, Some("BRUTEX-01-XY")),
            row("GZ", "OPEN", 0, None),
        ];
        let r = reconcile_groww_orders(&[], &snap, &[], PARAMS);
        assert_eq!(r.ghost_broker.len(), 3);
        let by_id: BTreeMap<_, _> = r
            .ghost_broker
            .iter()
            .map(|g| (g.groww_order_id.clone().unwrap(), g.action))
            .collect();
        assert_eq!(by_id["GX"], GhostBrokerAction::Adopt);
        assert_eq!(by_id["GY"], GhostBrokerAction::ForeignIgnored);
        assert_eq!(by_id["GZ"], GhostBrokerAction::ForeignIgnored);
        assert_eq!(r.foreign_ignored, 2);
    }

    // --- orphan intents ---

    #[test]
    fn test_orphan_open_intent_routes_to_ladder() {
        let rid = generate_reference_id(DATE, 9, 9);
        let orphan = IntentSummary {
            intent_id: rid.clone(),
            reference_id: rid.clone(),
            kind: IntentKind::Place,
            last_phase: crate::oms::groww::intent_ledger::IntentPhase::Ambiguous,
            groww_order_id: None,
            last_ts_ms: 1,
        };
        // A second open intent whose order IS tracked must NOT be orphaned.
        let tracked_rid = generate_reference_id(DATE, 10, 9);
        let tracked_intent = IntentSummary {
            intent_id: tracked_rid.clone(),
            reference_id: tracked_rid,
            kind: IntentKind::Place,
            last_phase: crate::oms::groww::intent_ledger::IntentPhase::Sent,
            groww_order_id: Some("G1".to_owned()),
            last_ts_ms: 2,
        };
        let locals = [local("G1", GrowwOrderStatus::Open, 0, 1, 0)];
        let snap = [row("G1", "OPEN", 0, None)];
        let r = reconcile_groww_orders(&locals, &snap, &[orphan, tracked_intent], PARAMS);
        assert_eq!(r.orphan_intents, vec![rid]);
    }

    // --- snapshot row with missing status parses as Unknown → park ---

    #[test]
    fn test_snapshot_row_without_status_parks_never_transitions() {
        let locals = [local("G1", GrowwOrderStatus::Open, 0, 1, 0)];
        let snap = [SnapshotRow {
            groww_order_id: Some("G1".to_owned()),
            order_status: None,
            filled_quantity: Some(0),
            order_reference_id: None,
        }];
        let r = reconcile_groww_orders(&locals, &snap, &[], PARAMS);
        let d = r.orders["G1"].decision.as_ref().unwrap();
        assert_eq!(d.outcome, TransitionOutcome::Park);
        assert!(d.unknown_status);
    }

    // --- EOD classification (P29; HIGH-2 fill-class + Unknown arms) ---

    #[test]
    fn test_eod_force_expires_only_genuinely_open_class() {
        use GrowwOrderStatus as S;
        // Only genuinely OPEN-class non-terminals with ZERO fill plain-
        // force-expire (R2-MEDIUM-HIGH-2: a positive fill takes the
        // partial-fill arm — tested below).
        for s in [
            S::New,
            S::Acked,
            S::TriggerPending,
            S::Approved,
            S::Open,
            S::ModificationRequested,
            S::CancellationRequested,
        ] {
            assert_eq!(
                classify_eod_action(&TrackedOrderState::new(s.clone(), 0)),
                EodAction::ForceExpireLocal,
                "{s:?}"
            );
        }
        for s in [S::Completed, S::Rejected, S::Failed, S::Cancelled] {
            assert_eq!(
                classify_eod_action(&TrackedOrderState::new(s.clone(), 0)),
                EodAction::AlreadyTerminal,
                "{s:?}"
            );
        }
    }

    #[test]
    fn test_eod_partial_fill_open_class_never_plain_force_expires() {
        use GrowwOrderStatus as S;
        // R2-MEDIUM-HIGH-2: Groww has NO PARTIAL wire status — a partial
        // fill presents as an open-class status with filled_quantity > 0.
        // At EOD it must carry the fill + the position-exists page, never a
        // plain ForceExpireLocal (which would hide a real position).
        for s in [
            S::New,
            S::Acked,
            S::TriggerPending,
            S::Approved,
            S::Open,
            S::ModificationRequested,
            S::CancellationRequested,
        ] {
            let action = classify_eod_action(&TrackedOrderState::new(s.clone(), 30));
            assert_eq!(
                action,
                EodAction::ForceExpireWithPartialFill {
                    filled_quantity: 30
                },
                "{s:?} with fill must take the partial-fill arm"
            );
            assert_ne!(action, EodAction::ForceExpireLocal, "{s:?}");
        }
        // Fill-class remains status-driven — UNCHANGED by the fill gate.
        for s in [S::Executed, S::DeliveryAwaited] {
            assert_eq!(
                classify_eod_action(&TrackedOrderState::new(s.clone(), 30)),
                EodAction::FilledAwaitSettlement,
                "{s:?}"
            );
        }
        // Terminal with fill stays AlreadyTerminal (the F-10 page fired at
        // terminal entry, not at EOD).
        assert_eq!(
            classify_eod_action(&TrackedOrderState::new(S::Completed, 30)),
            EodAction::AlreadyTerminal
        );
        // Unknown with fill stays Critical (the stronger arm wins).
        assert_eq!(
            classify_eod_action(&TrackedOrderState::new(S::Unknown("X".to_owned()), 30)),
            EodAction::UnknownAtEodCritical
        );
        // A defensive negative fill is NOT a partial — plain force-expire.
        assert_eq!(
            classify_eod_action(&TrackedOrderState::new(S::Open, -5)),
            EodAction::ForceExpireLocal
        );
    }

    #[test]
    fn test_eod_fill_class_states_await_settlement_never_expired() {
        use GrowwOrderStatus as S;
        // HIGH-2: EXECUTED / DELIVERY_AWAITED at EOD = a position exists —
        // terminal-with-fill locally (F-10 page semantics), NEVER Expired.
        for s in [S::Executed, S::DeliveryAwaited] {
            assert_eq!(
                classify_eod_action(&TrackedOrderState::new(s.clone(), 50)),
                EodAction::FilledAwaitSettlement,
                "{s:?}"
            );
            // The classification is status-driven, not fill-count-driven
            // (EXECUTED means the fill completed even if the count field
            // never arrived).
            assert_eq!(
                classify_eod_action(&TrackedOrderState::new(s.clone(), 0)),
                EodAction::FilledAwaitSettlement,
                "{s:?}"
            );
        }
    }

    #[test]
    fn test_eod_unknown_routes_critical_never_expired() {
        use GrowwOrderStatus as S;
        // HIGH-2: Unknown(_) at EOD can never be silently expired.
        assert_eq!(
            classify_eod_action(&TrackedOrderState::new(S::Unknown("X".to_owned()), 0)),
            EodAction::UnknownAtEodCritical
        );
        assert_eq!(
            classify_eod_action(&TrackedOrderState::new(S::Unknown(String::new()), 20)),
            EodAction::UnknownAtEodCritical
        );
    }

    // --- cross-close classification (P30/P47, doc-F1) ---

    #[test]
    fn test_cross_close_prior_day_unreadable_is_critical_never_silent_expired() {
        use BootIntentClass as C;
        // Same day → normal boot protocol.
        assert_eq!(
            classify_cross_close_intent(260_715, 260_715, C::NeedsResolution, false),
            CrossCloseAction::SameDayBootProtocol
        );
        // Prior-day never-sent → settle.
        assert_eq!(
            classify_cross_close_intent(260_714, 260_715, C::NeverSent, false),
            CrossCloseAction::SettleNeverSent
        );
        // Prior-day open + readable → per-order reads (never the day list).
        assert_eq!(
            classify_cross_close_intent(260_714, 260_715, C::NeedsResolution, true),
            CrossCloseAction::ResolvePerOrderReads
        );
        // Prior-day open + UNREADABLE → Critical (the fail-closed default
        // until day-0 probe D-10 proves prior-day reads).
        assert_eq!(
            classify_cross_close_intent(260_714, 260_715, C::NeedsResolution, false),
            CrossCloseAction::MarkUnresolvedCritical
        );
        assert_eq!(
            classify_cross_close_intent(260_714, 260_715, C::NeedsOperator, false),
            CrossCloseAction::MarkUnresolvedCritical
        );
        // Settled is settled regardless of dates.
        assert_eq!(
            classify_cross_close_intent(260_714, 260_715, C::Settled, false),
            CrossCloseAction::Settled
        );
    }

    #[test]
    fn test_ghost_local_grace_boundaries_pure() {
        // Exactly one sweep interval old: NOT older-than ⇒ grace.
        assert_eq!(
            classify_ghost_local(60_000, 60_000, 2),
            GhostLocalVerdict::Grace
        );
        assert_eq!(
            classify_ghost_local(60_001, 60_000, 2),
            GhostLocalVerdict::Confirmed
        );
        assert_eq!(
            classify_ghost_local(60_001, 60_000, 1),
            GhostLocalVerdict::Grace
        );
        assert_eq!(GHOST_LOCAL_CONFIRM_SWEEPS, 2);
    }

    #[test]
    fn test_mismatch_kind_labels_stable() {
        assert_eq!(MismatchKind::StatusDrift.as_str(), "status_drift");
        assert_eq!(MismatchKind::FillDrift.as_str(), "fill_drift");
        assert_eq!(MismatchKind::GhostLocal.as_str(), "ghost_local");
        assert_eq!(MismatchKind::GhostBroker.as_str(), "ghost_broker");
        assert_eq!(MismatchKind::OrphanIntent.as_str(), "orphan_intent");
        assert_eq!(
            MismatchKind::PriorDayNeedsPerOrderRead.as_str(),
            "prior_day_needs_per_order_read"
        );
    }

    // --- HIGH-5: prior-day locals are never GhostLocal-expired ---

    #[test]
    fn test_prior_day_local_absent_from_today_list_needs_per_order_read() {
        // A PRIOR-day non-terminal local order + an EMPTY today-list: the
        // day-scoped list proves nothing — per-order read, never Expired.
        let mut lo = local(
            "G1",
            GrowwOrderStatus::Open,
            0,
            PARAMS.now_ms - 90_000_000, // ~a day old — far past min-age grace
            5,                          // even past the sweep-count grace
        );
        lo.trading_date_yymmdd = 260_714; // yesterday
        let r = reconcile_groww_orders(std::slice::from_ref(&lo), &[], &[], PARAMS);
        let o = &r.orders["G1"];
        assert_eq!(
            o.finding,
            Some(MismatchKind::PriorDayNeedsPerOrderRead),
            "prior-day absence must route to a per-order read"
        );
        assert_eq!(o.ghost_local, None, "never a GhostLocal verdict");
        assert_eq!(
            o.next_missing_sweeps, 5,
            "the day-list absence counter is meaningless for a prior-day \
             order and stays untouched"
        );
        // A prior-day TERMINAL local stays a no-op.
        let mut done = local("G2", GrowwOrderStatus::Completed, 50, 1, 3);
        done.trading_date_yymmdd = 260_714;
        let r = reconcile_groww_orders(&[done], &[], &[], PARAMS);
        assert_eq!(r.orders["G2"].finding, None);
    }

    #[test]
    fn test_missing_sweeps_reset_at_session_open() {
        // HIGH-5: counters accumulated against yesterday's snapshots must
        // never pre-confirm today's GhostLocal.
        let mut orders = vec![
            local("G1", GrowwOrderStatus::Open, 0, PARAMS.now_ms - 600_000, 1),
            local("G2", GrowwOrderStatus::Acked, 0, PARAMS.now_ms - 600_000, 7),
        ];
        reset_missing_sweeps_at_session_open(&mut orders);
        assert!(orders.iter().all(|o| o.missing_sweeps == 0));
        // After the reset, a single absent sweep is GRACE again — the full
        // 2-sweep confirmation restarts.
        let r = reconcile_groww_orders(&orders, &[], &[], PARAMS);
        assert_eq!(r.orders["G1"].ghost_local, Some(GhostLocalVerdict::Grace));
        assert_eq!(r.orders["G1"].finding, None);
        assert_eq!(r.orders["G1"].next_missing_sweeps, 1);
        assert_eq!(r.orders["G2"].ghost_local, Some(GhostLocalVerdict::Grace));
    }

    // --- HIGH-4: the cross-close producer over replayed open intents ---

    #[test]
    fn test_cross_close_producer_routes_prior_day_open_intents() {
        use crate::oms::groww::intent_ledger::IntentPhase;
        let day1 = IstDate {
            year: 2026,
            month: 7,
            day: 14,
        };
        let mk = |date: IstDate, seq: u32, phase: IntentPhase| {
            let rid = generate_reference_id(date, seq, 1);
            IntentSummary {
                intent_id: rid.clone(),
                reference_id: rid,
                kind: IntentKind::Place,
                last_phase: phase,
                groww_order_id: None,
                last_ts_ms: 1,
            }
        };
        let open = [
            (
                mk(day1, 1, IntentPhase::Recorded),
                BootIntentClass::NeverSent,
            ),
            (
                mk(day1, 2, IntentPhase::Sent),
                BootIntentClass::NeedsResolution,
            ),
            (
                mk(DATE, 3, IntentPhase::Sent),
                BootIntentClass::NeedsResolution,
            ),
        ];
        // Prior-day readability UNPROVEN (the day-0 default).
        let actions = classify_cross_close_open_intents(&open, DATE, false);
        assert_eq!(actions.len(), 3);
        assert_eq!(actions[0].1, CrossCloseAction::SettleNeverSent);
        assert_eq!(actions[1].1, CrossCloseAction::MarkUnresolvedCritical);
        assert_eq!(actions[2].1, CrossCloseAction::SameDayBootProtocol);
        // Prior-day readability PROVEN → per-order reads.
        let actions = classify_cross_close_open_intents(&open, DATE, true);
        assert_eq!(actions[1].1, CrossCloseAction::ResolvePerOrderReads);
        // A non-decomposable reference id fail-closes to Critical.
        let weird = [(
            IntentSummary {
                intent_id: "WEIRD-REF-01".to_owned(),
                reference_id: "WEIRD-REF-01".to_owned(),
                kind: IntentKind::Place,
                last_phase: IntentPhase::Sent,
                groww_order_id: None,
                last_ts_ms: 1,
            },
            BootIntentClass::NeedsResolution,
        )];
        let actions = classify_cross_close_open_intents(&weird, DATE, true);
        assert_eq!(actions[0].1, CrossCloseAction::MarkUnresolvedCritical);
    }

    // --- LOW: adopt requires an order id; orphan dedup ---

    #[test]
    fn test_ghost_broker_ours_without_order_id_is_not_adopted() {
        let ours_ref = generate_reference_id(DATE, 8, 8);
        let snap = [SnapshotRow {
            groww_order_id: None,
            order_status: Some("OPEN".to_owned()),
            filled_quantity: Some(0),
            order_reference_id: Some(ours_ref),
        }];
        let r = reconcile_groww_orders(&[], &snap, &[], PARAMS);
        assert_eq!(r.ghost_broker.len(), 1);
        assert_eq!(
            r.ghost_broker[0].action,
            GhostBrokerAction::ForeignIgnored,
            "Adopt requires Some(groww_order_id)"
        );
        assert_eq!(
            r.foreign_ignored, 0,
            "an OURS-but-id-less row never inflates the foreign counter"
        );
    }

    #[test]
    fn test_orphan_intents_deduped_on_intent_id() {
        let rid = generate_reference_id(DATE, 12, 12);
        let orphan = IntentSummary {
            intent_id: rid.clone(),
            reference_id: rid.clone(),
            kind: IntentKind::Place,
            last_phase: crate::oms::groww::intent_ledger::IntentPhase::Ambiguous,
            groww_order_id: None,
            last_ts_ms: 1,
        };
        let r = reconcile_groww_orders(&[], &[], &[orphan.clone(), orphan], PARAMS);
        assert_eq!(r.orphan_intents, vec![rid], "one ladder route per intent");
    }
}

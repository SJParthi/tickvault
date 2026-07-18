//! Groww order state machine — the rank-monotone OPEN-SET lattice
//! (design §4.6, hostile F-2 fix). ORD-PR-2 pure core: a PURE TOTAL
//! function; no I/O, no clock, no channel.
//!
//! # Why rank-monotone, not a hand-enumerated DAG
//! The status poller runs at 2s/5s/15s and SKIPS intermediate states — every
//! legal multi-hop path must therefore be a legal single observed edge
//! (NEW→COMPLETED, CANCELLATION_REQUESTED→COMPLETED, …). Legality is
//! `rank(to) > rank(from)` from a LIVE source; BACKWARD moves and moves out
//! of terminal are illegal ⇒ PARK + `needs_reconciliation` — EXCEPT backward
//! observations from the RECONCILE snapshot source, which classify
//! `stale_snapshot_skip` unless repeated on 2 consecutive sweeps (hostile
//! F-6a). No documented transition DAG exists — every rank below is Assumed
//! (`16` §4.1 note).
//!
//! # Hints never transition state
//! This module consumes PARSED statuses from REST truth reads only. The
//! Session-3 `BrokerOrderEvent` hint type NEVER appears here — a hint
//! schedules a targeted detail poll; the REST response drives this FSM
//! (design §4.11; the `hint_never_transitions_state` ratchet lands with the
//! consumer in ORD-PR-3/5).
//!
//! # Partial fills are DATA, not states
//! `filled_quantity` rides a monotonic ratchet: a decrease from a LIVE
//! source is a reconcile-mismatch marker; from the RECONCILE source it takes
//! the stale-skip rule (design §4.6).

use super::types::GrowwOrderStatus;

/// Where an observation came from — a LIVE read (status/detail poll,
/// post-mutation confirm) vs the periodic RECONCILE list snapshot (which can
/// be stale relative to per-order reads; hostile F-6a).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObservationSource {
    /// Per-order status/detail read — authoritative freshness.
    Live,
    /// Order-list reconcile sweep row — may lag live reads.
    Reconcile,
}

/// Ranks per design §4.6 (Assumed classification — no doc DAG exists).
/// Terminals (REJECTED / FAILED / CANCELLED / COMPLETED) are handled by
/// [`is_terminal`] BEFORE rank comparison; their rank here is the ∞-class.
const RANK_TERMINAL: u8 = u8::MAX;

/// Rank a KNOWN status. `None` for `Unknown` (parked before ranking).
#[must_use]
fn rank(status: &GrowwOrderStatus) -> Option<u8> {
    Some(match status {
        GrowwOrderStatus::New => 1,
        GrowwOrderStatus::Acked => 2,
        GrowwOrderStatus::Approved | GrowwOrderStatus::TriggerPending => 3,
        GrowwOrderStatus::Open => 4,
        GrowwOrderStatus::ModificationRequested | GrowwOrderStatus::CancellationRequested => 5,
        GrowwOrderStatus::Executed => 6,
        GrowwOrderStatus::DeliveryAwaited => 7,
        GrowwOrderStatus::Completed
        | GrowwOrderStatus::Rejected
        | GrowwOrderStatus::Failed
        | GrowwOrderStatus::Cancelled => RANK_TERMINAL,
        GrowwOrderStatus::Unknown(_) => return None,
    })
}

/// Terminal states — no further lifecycle transition is expected
/// (design §4.6 rank-∞ class). `Executed`/`DeliveryAwaited` are NOT terminal
/// (they legally move forward to `Completed`); `Unknown` is NOT terminal
/// (we cannot assert done-ness from a string we did not understand).
#[must_use]
pub fn is_terminal(status: &GrowwOrderStatus) -> bool {
    matches!(
        status,
        GrowwOrderStatus::Completed
            | GrowwOrderStatus::Rejected
            | GrowwOrderStatus::Failed
            | GrowwOrderStatus::Cancelled
    )
}

/// The fill-carrying execution class (`EXECUTED` / `DELIVERY_AWAITED` /
/// `COMPLETED`) — a move `CANCELLATION_REQUESTED → any of these` is the
/// cancel-lost-race arm (hostile F-2: the h4 page fires at ANY of the three
/// observation points).
#[must_use]
pub fn is_fill_class(status: &GrowwOrderStatus) -> bool {
    matches!(
        status,
        GrowwOrderStatus::Executed
            | GrowwOrderStatus::DeliveryAwaited
            | GrowwOrderStatus::Completed
    )
}

/// The tracked local state of one order (the executor's per-order twin).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedOrderState {
    /// Last adopted status.
    pub status: GrowwOrderStatus,
    /// Monotonic cumulative fill (units).
    pub filled_quantity: i64,
    /// Consecutive RECONCILE sweeps that observed a backward status or a
    /// fill decrease for this order — the 2-sweep escalation counter
    /// (hostile F-6a). Reset on any consistent observation.
    pub consecutive_stale_reconcile: u8,
}

impl TrackedOrderState {
    /// A fresh tracked order at its first adopted status.
    #[must_use]
    pub fn new(status: GrowwOrderStatus, filled_quantity: i64) -> Self {
        Self {
            status,
            filled_quantity,
            consecutive_stale_reconcile: 0,
        }
    }
}

/// One parsed observation of an order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderObservation {
    /// The parsed status (open-set — may be `Unknown`).
    pub status: GrowwOrderStatus,
    /// Observed cumulative fill, when the payload carried it.
    pub filled_quantity: Option<i64>,
    /// Transport provenance of this observation.
    pub source: ObservationSource,
}

/// The classification of one observation against the tracked state —
/// exactly ONE outcome per (status × fill × state × source) tuple (the
/// totality contract, graft 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransitionOutcome {
    /// Legal move — adopt the observed status (+ fill refresh).
    Transition,
    /// Same status — fill-field refresh only (engine.rs:611-617 mirror).
    SameStatusRefresh,
    /// No adoption: `Unknown` status, or an illegal move from a LIVE source
    /// (backward / out-of-terminal). Check
    /// [`TransitionDecision::needs_reconciliation`].
    Park,
    /// Backward/inconsistent observation from the RECONCILE source within
    /// the 2-sweep grace — counted, not escalated (hostile F-6a).
    StaleSnapshotSkip,
}

/// The full decision — outcome plus the page/audit flags the executor and
/// observability arms consume. Pure data; the emits live in ORD-PR-3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransitionDecision {
    /// The single classified outcome.
    pub outcome: TransitionOutcome,
    /// The order needs a targeted reconcile (illegal live move, escalated
    /// stale reconcile, or a live fill decrease).
    // GROWW-ORD-04 emit lands in ORD-PR-3 once the variants exist.
    pub needs_reconciliation: bool,
    /// `CANCELLATION_REQUESTED → fill-class` — the cancel lost the race
    /// (h4 page at any of the three observation points; hostile F-2).
    pub cancel_lost_race: bool,
    /// A terminal state was entered (or terminally refreshed) with
    /// `filled_quantity > 0` — the "position exists" page (hostile F-10).
    pub terminal_with_fill: bool,
    /// The observed cumulative fill DECREASED (monotonicity breach).
    pub fill_monotonicity_violation: bool,
    /// An `Unknown` status was observed — park + coalesced warn
    /// (open-set answer to O-1).
    // GROWW-ORD-07 emit lands in ORD-PR-3 once the variants exist.
    pub unknown_status: bool,
    /// The new `consecutive_stale_reconcile` counter value the caller must
    /// store back (the 2-sweep escalation state).
    pub next_stale_reconcile_count: u8,
}

impl TransitionDecision {
    /// A neutral decision skeleton.
    const fn base(outcome: TransitionOutcome) -> Self {
        Self {
            outcome,
            needs_reconciliation: false,
            cancel_lost_race: false,
            terminal_with_fill: false,
            fill_monotonicity_violation: false,
            unknown_status: false,
            next_stale_reconcile_count: 0,
        }
    }
}

/// Consecutive stale-reconcile sweeps that escalate to a reconcile mismatch
/// (hostile F-6a: "unless repeated on 2 consecutive sweeps").
pub const STALE_RECONCILE_ESCALATION_SWEEPS: u8 = 2;

/// Evaluate one observation against the tracked state — the PURE TOTAL
/// classification function (design §4.6). Never panics; every input tuple
/// lands on exactly one [`TransitionOutcome`].
///
/// The caller adopts `(status, fill)` ONLY when the outcome is `Transition`
/// or `SameStatusRefresh` (fill only, and only upward), and stores back
/// [`TransitionDecision::next_stale_reconcile_count`] in all cases.
#[must_use]
pub fn evaluate_transition(
    current: &TrackedOrderState,
    obs: &OrderObservation,
) -> TransitionDecision {
    // --- Unknown observed status: PARK, never transition (O-1 fail-closed).
    if matches!(obs.status, GrowwOrderStatus::Unknown(_)) {
        let mut d = TransitionDecision::base(TransitionOutcome::Park);
        d.unknown_status = true;
        d.next_stale_reconcile_count = current.consecutive_stale_reconcile;
        return d;
    }

    let fill_decreased = obs
        .filled_quantity
        .is_some_and(|f| f < current.filled_quantity);
    let obs_fill_positive = obs
        .filled_quantity
        .map_or(current.filled_quantity > 0, |f| f > 0);

    // --- Same status: fill-field refresh only.
    if obs.status == current.status {
        let mut d = TransitionDecision::base(TransitionOutcome::SameStatusRefresh);
        if fill_decreased {
            match obs.source {
                ObservationSource::Live => {
                    d.fill_monotonicity_violation = true;
                    d.needs_reconciliation = true;
                    d.next_stale_reconcile_count = 0;
                }
                ObservationSource::Reconcile => {
                    return stale_or_escalate(current, /* fill_violation: */ true);
                }
            }
        }
        // A terminal same-status refresh with fill > 0 re-fires the
        // "position exists" visibility (hostile F-10 / P43).
        if is_terminal(&obs.status) && obs_fill_positive {
            d.terminal_with_fill = true;
        }
        return d;
    }

    // --- The current state is being asked to CHANGE.
    // Unknown-parked current state: rank is unknowable — only reconcile-led
    // clarification may move it; classify by source.
    let Some(from_rank) = rank(&current.status) else {
        // Current is Unknown(_) — adopt any KNOWN status from either source
        // (the reconcile-clarifies path of P50); the parser guarantees `obs`
        // is known here.
        let mut d = TransitionDecision::base(TransitionOutcome::Transition);
        if is_terminal(&obs.status) && obs_fill_positive {
            d.terminal_with_fill = true;
        }
        if fill_decreased {
            d.fill_monotonicity_violation = true;
            d.needs_reconciliation = true;
        }
        return d;
    };
    // `obs.status` is known (Unknown handled above) — rank is Some.
    let Some(to_rank) = rank(&obs.status) else {
        // Unreachable by construction; keep total + fail-closed anyway.
        let mut d = TransitionDecision::base(TransitionOutcome::Park);
        d.unknown_status = true;
        d.next_stale_reconcile_count = current.consecutive_stale_reconcile;
        return d;
    };

    // --- Moves out of terminal are illegal (rank-∞ class).
    if is_terminal(&current.status) {
        return match obs.source {
            ObservationSource::Live => {
                let mut d = TransitionDecision::base(TransitionOutcome::Park);
                d.needs_reconciliation = true;
                // MEDIUM-6: a live-source Park never resets the stale-sweep
                // counter — only a successful adoption does.
                d.next_stale_reconcile_count = current.consecutive_stale_reconcile;
                d
            }
            ObservationSource::Reconcile => stale_or_escalate(current, fill_decreased),
        };
    }

    // --- Modify-request overlay resolution (enumerated special arm):
    // MODIFICATION_REQUESTED legally RESOLVES back to a resting state when
    // the modify applies (the order re-rests). Without this arm every
    // successful modify would page as an illegal backward move. Deviation
    // from the strict rank text of §4.6 — recorded in the PR notes.
    //
    // CANCELLATION_REQUESTED gets NO such arm (adversarial HIGH-1): a
    // resting status observed under a pending cancel is either a stale
    // snapshot or a cancel-reject — ADOPTING it would leave the order in a
    // resting state and DISARM the cancel-lost-race tripwire if EXECUTED
    // arrives later. It therefore PARKS with `needs_reconciliation` from a
    // LIVE source (the stale-skip rule from RECONCILE); the order STAYS in
    // CancellationRequested, so the race arm still fires, and a genuine
    // cancel-reject surfaces via reconciliation then resolves on the next
    // terminal status.
    let overlay_resolution = matches!(current.status, GrowwOrderStatus::ModificationRequested)
        && matches!(
            obs.status,
            GrowwOrderStatus::Acked
                | GrowwOrderStatus::Approved
                | GrowwOrderStatus::TriggerPending
                | GrowwOrderStatus::Open
        );

    // --- Sticky CANCELLATION_REQUESTED (R2-HIGH-1): NO LATERAL adoption
    // OUT of a pending cancel. CancelReq → ModReq shares rank 5, so the
    // generic lateral rule would adopt it — and the ModReq overlay arm
    // would then legally resolve back to a resting state, disarming the
    // cancel-lost-race tripwire over TWO hops (the mask the one-hop HIGH-1
    // fix above did not cover). There is NO legitimate CancelReq → ModReq
    // flow: the ledger's `MutationInFlight` invariant refuses a modify
    // while a cancel intent is open, so the observation is a stale
    // snapshot. Only rank-6+ (fill-class) and terminal statuses exit
    // CancellationRequested; this routes to the same PARK / stale-skip
    // arms as a strictly-backward move.
    let cancel_req_sticky_lateral =
        matches!(current.status, GrowwOrderStatus::CancellationRequested)
            && matches!(obs.status, GrowwOrderStatus::ModificationRequested);

    // --- Forward (or lateral within the same rank tier, e.g.
    // MODIFICATION_REQUESTED → CANCELLATION_REQUESTED — the legitimate
    // direction — or APPROVED ↔ TRIGGER_PENDING) is legal; strictly
    // backward is not, and neither is the sticky-CancelReq lateral above.
    if (to_rank >= from_rank || overlay_resolution) && !cancel_req_sticky_lateral {
        let mut d = TransitionDecision::base(TransitionOutcome::Transition);
        if matches!(current.status, GrowwOrderStatus::CancellationRequested)
            && is_fill_class(&obs.status)
        {
            d.cancel_lost_race = true;
        }
        if is_terminal(&obs.status) && obs_fill_positive {
            d.terminal_with_fill = true;
        }
        if fill_decreased {
            match obs.source {
                ObservationSource::Live => {
                    d.fill_monotonicity_violation = true;
                    d.needs_reconciliation = true;
                }
                ObservationSource::Reconcile => {
                    return stale_or_escalate(current, true);
                }
            }
        }
        return d;
    }

    // --- Strictly backward (incl. CANCELLATION_REQUESTED → resting,
    // HIGH-1) or the sticky-CancelReq lateral (CANCELLATION_REQUESTED →
    // MODIFICATION_REQUESTED, R2-HIGH-1).
    match obs.source {
        ObservationSource::Live => {
            let mut d = TransitionDecision::base(TransitionOutcome::Park);
            d.needs_reconciliation = true;
            if fill_decreased {
                d.fill_monotonicity_violation = true;
            }
            // MEDIUM-6: a live-source Park PRESERVES the stale-sweep counter
            // — only a successful adoption resets it, so an alternating
            // stale-reconcile / live-park sequence still escalates.
            d.next_stale_reconcile_count = current.consecutive_stale_reconcile;
            d
        }
        ObservationSource::Reconcile => stale_or_escalate(current, fill_decreased),
    }
}

/// The reconcile-source stale-vs-escalate rule (hostile F-6a): the first
/// inconsistent sweep is a counted skip; the
/// [`STALE_RECONCILE_ESCALATION_SWEEPS`]-th consecutive one escalates to
/// `needs_reconciliation`.
fn stale_or_escalate(current: &TrackedOrderState, fill_violation: bool) -> TransitionDecision {
    let next = current.consecutive_stale_reconcile.saturating_add(1);
    let mut d = TransitionDecision::base(TransitionOutcome::StaleSnapshotSkip);
    d.next_stale_reconcile_count = next;
    if next >= STALE_RECONCILE_ESCALATION_SWEEPS {
        d.needs_reconciliation = true;
        d.fill_monotonicity_violation = fill_violation;
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Every KNOWN status (12 annexure + OPEN).
    fn known_statuses() -> Vec<GrowwOrderStatus> {
        use GrowwOrderStatus as S;
        vec![
            S::New,
            S::Acked,
            S::TriggerPending,
            S::Approved,
            S::Open,
            S::ModificationRequested,
            S::CancellationRequested,
            S::Executed,
            S::DeliveryAwaited,
            S::Completed,
            S::Rejected,
            S::Failed,
            S::Cancelled,
        ]
    }

    fn all_statuses() -> Vec<GrowwOrderStatus> {
        let mut v = known_statuses();
        v.push(GrowwOrderStatus::Unknown("WEIRD_2027".to_owned()));
        v
    }

    fn tracked(status: GrowwOrderStatus, fill: i64) -> TrackedOrderState {
        TrackedOrderState::new(status, fill)
    }

    fn obs(
        status: GrowwOrderStatus,
        fill: Option<i64>,
        source: ObservationSource,
    ) -> OrderObservation {
        OrderObservation {
            status,
            filled_quantity: fill,
            source,
        }
    }

    // --- terminal / fill-class pins ---

    #[test]
    fn test_terminal_set_is_exactly_the_four() {
        use GrowwOrderStatus as S;
        for s in known_statuses() {
            let want = matches!(s, S::Completed | S::Rejected | S::Failed | S::Cancelled);
            assert_eq!(is_terminal(&s), want, "{s:?}");
        }
        assert!(!is_terminal(&S::Unknown("X".to_owned())));
    }

    #[test]
    fn test_fill_class_pin() {
        use GrowwOrderStatus as S;
        assert!(is_fill_class(&S::Executed));
        assert!(is_fill_class(&S::DeliveryAwaited));
        assert!(is_fill_class(&S::Completed));
        assert!(!is_fill_class(&S::Cancelled));
        assert!(!is_fill_class(&S::Open));
    }

    // --- poll-skip proof: forward multi-hop as a single edge ---

    #[test]
    fn test_poll_skip_forward_moves_are_legal_single_edges() {
        use GrowwOrderStatus as S;
        for (from, to) in [
            (S::New, S::Completed),                   // full skip
            (S::New, S::Executed),                    // fast fill
            (S::Acked, S::DeliveryAwaited),           // graft-3 arm
            (S::Approved, S::DeliveryAwaited),        // graft-3 arm
            (S::Open, S::Completed),                  // graft-3 arm
            (S::CancellationRequested, S::Cancelled), // cancel landed
            (S::TriggerPending, S::Open),             // trigger fired
            (S::ModificationRequested, S::Executed),  // filled mid-modify
        ] {
            let d = evaluate_transition(
                &tracked(from.clone(), 0),
                &obs(to.clone(), Some(0), ObservationSource::Live),
            );
            assert_eq!(
                d.outcome,
                TransitionOutcome::Transition,
                "{from:?} -> {to:?} must be a legal single edge"
            );
            assert!(!d.needs_reconciliation, "{from:?} -> {to:?}");
        }
    }

    // --- cancel-lost-race fires at ANY of the three fill-class points (F-2) ---

    #[test]
    fn test_cancel_lost_race_at_all_three_observation_points() {
        use GrowwOrderStatus as S;
        for to in [S::Executed, S::DeliveryAwaited, S::Completed] {
            let d = evaluate_transition(
                &tracked(S::CancellationRequested, 0),
                &obs(to.clone(), Some(50), ObservationSource::Live),
            );
            assert_eq!(d.outcome, TransitionOutcome::Transition);
            assert!(d.cancel_lost_race, "race flag must fire on {to:?}");
        }
        // Cancel that LANDS is not a race.
        let d = evaluate_transition(
            &tracked(S::CancellationRequested, 0),
            &obs(S::Cancelled, Some(0), ObservationSource::Live),
        );
        assert!(!d.cancel_lost_race);
    }

    // --- terminal-with-fill (F-10): entry AND same-status refresh ---

    #[test]
    fn test_terminal_with_fill_pages_on_entry_and_refresh() {
        use GrowwOrderStatus as S;
        // Entry into CANCELLED with a partial fill (P7).
        let d = evaluate_transition(
            &tracked(S::CancellationRequested, 20),
            &obs(S::Cancelled, Some(20), ObservationSource::Live),
        );
        assert!(d.terminal_with_fill);
        // Terminal same-status refresh with fill (P43).
        let d = evaluate_transition(
            &tracked(S::Cancelled, 20),
            &obs(S::Cancelled, Some(20), ObservationSource::Live),
        );
        assert_eq!(d.outcome, TransitionOutcome::SameStatusRefresh);
        assert!(d.terminal_with_fill);
        // Terminal entry with ZERO fill does not page.
        let d = evaluate_transition(
            &tracked(S::Open, 0),
            &obs(S::Cancelled, Some(0), ObservationSource::Live),
        );
        assert!(!d.terminal_with_fill);
        // Absent observed fill falls back to the tracked fill.
        let d = evaluate_transition(
            &tracked(S::Open, 20),
            &obs(S::Cancelled, None, ObservationSource::Live),
        );
        assert!(d.terminal_with_fill);
    }

    // --- backward + out-of-terminal ---

    #[test]
    fn test_backward_from_live_parks_and_flags() {
        use GrowwOrderStatus as S;
        let d = evaluate_transition(
            &tracked(S::Open, 0),
            &obs(S::Acked, Some(0), ObservationSource::Live),
        );
        assert_eq!(d.outcome, TransitionOutcome::Park);
        assert!(d.needs_reconciliation);
    }

    #[test]
    fn test_out_of_terminal_from_live_parks() {
        use GrowwOrderStatus as S;
        for term in [S::Completed, S::Rejected, S::Failed, S::Cancelled] {
            let d = evaluate_transition(
                &tracked(term.clone(), 0),
                &obs(S::Open, Some(0), ObservationSource::Live),
            );
            assert_eq!(d.outcome, TransitionOutcome::Park, "out of {term:?}");
            assert!(d.needs_reconciliation);
        }
    }

    // --- stale-snapshot skip + 2-sweep escalation (F-6a) ---

    #[test]
    fn test_reconcile_backward_is_stale_skip_then_escalates_on_second_sweep() {
        use GrowwOrderStatus as S;
        let mut state = tracked(S::Open, 10);
        // Sweep 1: backward from reconcile — counted skip, no escalation.
        let d1 = evaluate_transition(
            &state,
            &obs(S::Acked, Some(10), ObservationSource::Reconcile),
        );
        assert_eq!(d1.outcome, TransitionOutcome::StaleSnapshotSkip);
        assert!(!d1.needs_reconciliation);
        assert_eq!(d1.next_stale_reconcile_count, 1);
        state.consecutive_stale_reconcile = d1.next_stale_reconcile_count;
        // Sweep 2: repeated — escalates.
        let d2 = evaluate_transition(
            &state,
            &obs(S::Acked, Some(10), ObservationSource::Reconcile),
        );
        assert_eq!(d2.outcome, TransitionOutcome::StaleSnapshotSkip);
        assert!(d2.needs_reconciliation);
        assert_eq!(d2.next_stale_reconcile_count, 2);
    }

    #[test]
    fn test_consistent_observation_resets_stale_counter() {
        use GrowwOrderStatus as S;
        let state = TrackedOrderState {
            status: S::Open,
            filled_quantity: 0,
            consecutive_stale_reconcile: 1,
        };
        let d = evaluate_transition(
            &state,
            &obs(S::Executed, Some(50), ObservationSource::Reconcile),
        );
        assert_eq!(d.outcome, TransitionOutcome::Transition);
        assert_eq!(d.next_stale_reconcile_count, 0);
    }

    #[test]
    fn test_live_park_preserves_stale_reconcile_counter() {
        use GrowwOrderStatus as S;
        // MEDIUM-6: an alternating stale-reconcile / live-park sequence must
        // still escalate at STALE_RECONCILE_ESCALATION_SWEEPS stale sweeps —
        // a live Park never resets the counter (only a successful adoption
        // does).
        let mut state = tracked(S::Open, 10);
        // Sweep 1: backward from reconcile — counted skip.
        let d1 = evaluate_transition(
            &state,
            &obs(S::Acked, Some(10), ObservationSource::Reconcile),
        );
        assert_eq!(d1.outcome, TransitionOutcome::StaleSnapshotSkip);
        assert_eq!(d1.next_stale_reconcile_count, 1);
        state.consecutive_stale_reconcile = d1.next_stale_reconcile_count;
        // Interleaved LIVE backward park — must PRESERVE the counter.
        let d2 = evaluate_transition(&state, &obs(S::Acked, Some(10), ObservationSource::Live));
        assert_eq!(d2.outcome, TransitionOutcome::Park);
        assert_eq!(
            d2.next_stale_reconcile_count, 1,
            "live park must preserve the stale counter"
        );
        state.consecutive_stale_reconcile = d2.next_stale_reconcile_count;
        // Sweep 2: the second stale reconcile still escalates.
        let d3 = evaluate_transition(
            &state,
            &obs(S::Acked, Some(10), ObservationSource::Reconcile),
        );
        assert_eq!(d3.outcome, TransitionOutcome::StaleSnapshotSkip);
        assert!(
            d3.needs_reconciliation,
            "escalation must survive the interleaved live park"
        );
        assert_eq!(d3.next_stale_reconcile_count, 2);
        // Out-of-terminal live park preserves the counter too.
        let term = TrackedOrderState {
            status: S::Completed,
            filled_quantity: 0,
            consecutive_stale_reconcile: 1,
        };
        let d4 = evaluate_transition(&term, &obs(S::Open, Some(0), ObservationSource::Live));
        assert_eq!(d4.outcome, TransitionOutcome::Park);
        assert_eq!(d4.next_stale_reconcile_count, 1);
    }

    // --- fill monotonicity (P44) ---

    #[test]
    fn test_fill_decrease_live_flags_violation() {
        use GrowwOrderStatus as S;
        // Same-status live decrease.
        let d = evaluate_transition(
            &tracked(S::Open, 30),
            &obs(S::Open, Some(20), ObservationSource::Live),
        );
        assert_eq!(d.outcome, TransitionOutcome::SameStatusRefresh);
        assert!(d.fill_monotonicity_violation);
        assert!(d.needs_reconciliation);
        // Forward-move live decrease is also flagged.
        let d = evaluate_transition(
            &tracked(S::Open, 30),
            &obs(S::Executed, Some(10), ObservationSource::Live),
        );
        assert_eq!(d.outcome, TransitionOutcome::Transition);
        assert!(d.fill_monotonicity_violation);
        assert!(d.needs_reconciliation);
    }

    #[test]
    fn test_fill_decrease_reconcile_takes_stale_skip_rule() {
        use GrowwOrderStatus as S;
        let d = evaluate_transition(
            &tracked(S::Open, 30),
            &obs(S::Open, Some(20), ObservationSource::Reconcile),
        );
        assert_eq!(d.outcome, TransitionOutcome::StaleSnapshotSkip);
        assert!(!d.needs_reconciliation);
        assert_eq!(d.next_stale_reconcile_count, 1);
    }

    #[test]
    fn test_fill_increase_is_clean_refresh() {
        use GrowwOrderStatus as S;
        let d = evaluate_transition(
            &tracked(S::Open, 10),
            &obs(S::Open, Some(20), ObservationSource::Live),
        );
        assert_eq!(d.outcome, TransitionOutcome::SameStatusRefresh);
        assert!(!d.fill_monotonicity_violation);
        assert!(!d.needs_reconciliation);
    }

    // --- Unknown handling (O-1) ---

    #[test]
    fn test_unknown_observation_parks_never_transitions() {
        use GrowwOrderStatus as S;
        for source in [ObservationSource::Live, ObservationSource::Reconcile] {
            let d = evaluate_transition(
                &tracked(S::Open, 0),
                &obs(S::Unknown("MYSTERY".to_owned()), Some(50), source),
            );
            assert_eq!(d.outcome, TransitionOutcome::Park);
            assert!(d.unknown_status);
            assert!(!d.needs_reconciliation);
        }
    }

    #[test]
    fn test_unknown_parked_current_state_adopts_known_clarification() {
        use GrowwOrderStatus as S;
        let d = evaluate_transition(
            &tracked(S::Unknown("MYSTERY".to_owned()), 0),
            &obs(S::Cancelled, Some(0), ObservationSource::Reconcile),
        );
        assert_eq!(d.outcome, TransitionOutcome::Transition);
        // Terminal-with-fill still audited from a parked state.
        let d = evaluate_transition(
            &tracked(S::Unknown("MYSTERY".to_owned()), 0),
            &obs(S::Executed, Some(50), ObservationSource::Live),
        );
        assert_eq!(d.outcome, TransitionOutcome::Transition);
    }

    // --- overlay-resolution special arm (recorded deviation; HIGH-1:
    //     ModificationRequested ONLY) ---

    #[test]
    fn test_modify_request_resolves_back_to_resting_state() {
        use GrowwOrderStatus as S;
        // MODIFICATION_REQUESTED completing back to OPEN is a legal arm —
        // otherwise every successful modify would page GROWW-ORD-04.
        let d = evaluate_transition(
            &tracked(S::ModificationRequested, 0),
            &obs(S::Open, Some(0), ObservationSource::Live),
        );
        assert_eq!(d.outcome, TransitionOutcome::Transition);
        assert!(!d.needs_reconciliation);
        // Lateral within the overlay tier: modify-requested → cancel-requested.
        let d = evaluate_transition(
            &tracked(S::ModificationRequested, 0),
            &obs(S::CancellationRequested, Some(0), ObservationSource::Live),
        );
        assert_eq!(d.outcome, TransitionOutcome::Transition);
        // But an overlay resolving back to NEW (rank 1, below the resting
        // tier) stays illegal.
        let d = evaluate_transition(
            &tracked(S::ModificationRequested, 0),
            &obs(S::New, Some(0), ObservationSource::Live),
        );
        assert_eq!(d.outcome, TransitionOutcome::Park);
        assert!(d.needs_reconciliation);
    }

    #[test]
    fn test_cancel_request_never_resolves_back_to_resting_state() {
        use GrowwOrderStatus as S;
        // HIGH-1: CancellationRequested → any resting status is a PARK with
        // needs_reconciliation (live) — never adopted, so the cancel-lost-
        // race tripwire stays armed.
        for resting in [S::Acked, S::Approved, S::TriggerPending, S::Open] {
            let d = evaluate_transition(
                &tracked(S::CancellationRequested, 0),
                &obs(resting.clone(), Some(0), ObservationSource::Live),
            );
            assert_eq!(
                d.outcome,
                TransitionOutcome::Park,
                "CancelReq -> {resting:?} must PARK"
            );
            assert!(d.needs_reconciliation, "CancelReq -> {resting:?}");
        }
        // From the RECONCILE source the stale-skip rule owns it.
        let d = evaluate_transition(
            &tracked(S::CancellationRequested, 0),
            &obs(S::Open, Some(0), ObservationSource::Reconcile),
        );
        assert_eq!(d.outcome, TransitionOutcome::StaleSnapshotSkip);
    }

    #[test]
    fn test_adversarial_cancel_req_stale_open_then_executed_fires_race() {
        use GrowwOrderStatus as S;
        // HIGH-1 adversarial sequence: CancelReq → OPEN (stale) → EXECUTED.
        let state = tracked(S::CancellationRequested, 0);
        // The stale OPEN parks — the caller does NOT adopt, so the order
        // stays in CancellationRequested.
        let d1 = evaluate_transition(&state, &obs(S::Open, Some(0), ObservationSource::Live));
        assert_eq!(d1.outcome, TransitionOutcome::Park);
        assert!(d1.needs_reconciliation);
        // EXECUTED then arrives — the race arm MUST still fire (the pre-fix
        // adoption of OPEN would have silently disarmed it).
        let d2 = evaluate_transition(&state, &obs(S::Executed, Some(50), ObservationSource::Live));
        assert_eq!(d2.outcome, TransitionOutcome::Transition);
        assert!(d2.cancel_lost_race, "race must survive the stale OPEN park");
    }

    #[test]
    fn test_cancel_request_sticky_no_lateral_exit_to_modification_requested() {
        use GrowwOrderStatus as S;
        // R2-HIGH-1: CancelReq → ModReq (same rank tier) must NEVER be
        // adopted — from LIVE it parks with needs_reconciliation…
        let d = evaluate_transition(
            &tracked(S::CancellationRequested, 0),
            &obs(S::ModificationRequested, Some(0), ObservationSource::Live),
        );
        assert_eq!(d.outcome, TransitionOutcome::Park);
        assert!(d.needs_reconciliation);
        // …and from RECONCILE the stale-skip rule owns it.
        let d = evaluate_transition(
            &tracked(S::CancellationRequested, 0),
            &obs(
                S::ModificationRequested,
                Some(0),
                ObservationSource::Reconcile,
            ),
        );
        assert_eq!(d.outcome, TransitionOutcome::StaleSnapshotSkip);
        // The LEGITIMATE direction (ModReq → CancelReq) stays legal.
        let d = evaluate_transition(
            &tracked(S::ModificationRequested, 0),
            &obs(S::CancellationRequested, Some(0), ObservationSource::Live),
        );
        assert_eq!(d.outcome, TransitionOutcome::Transition);
    }

    #[test]
    fn test_adversarial_two_hop_lateral_mask_cannot_disarm_cancel_race() {
        use GrowwOrderStatus as S;
        // R2-HIGH-1 adversarial 4-step sequence:
        // Open → ModReq → CancelReq → ModReq(stale) → Open(stale) → EXECUTED.
        // Pre-fix, the stale ModReq was adopted laterally (rank 5 == 5) and
        // the overlay arm then adopted the stale Open — silently disarming
        // the cancel-lost-race tripwire before EXECUTED arrived.
        let mut state = tracked(S::Open, 0);
        // Step 1: a legitimate modify request.
        let d = evaluate_transition(
            &state,
            &obs(S::ModificationRequested, Some(0), ObservationSource::Live),
        );
        assert_eq!(d.outcome, TransitionOutcome::Transition);
        state.status = S::ModificationRequested;
        // Step 2: a legitimate cancel supersedes (ModReq → CancelReq).
        let d = evaluate_transition(
            &state,
            &obs(S::CancellationRequested, Some(0), ObservationSource::Live),
        );
        assert_eq!(d.outcome, TransitionOutcome::Transition);
        state.status = S::CancellationRequested;
        // Step 3: a STALE ModReq observation — must PARK; the state never
        // leaves CancellationRequested.
        let d = evaluate_transition(
            &state,
            &obs(S::ModificationRequested, Some(0), ObservationSource::Live),
        );
        assert_eq!(d.outcome, TransitionOutcome::Park, "stale ModReq must park");
        assert!(d.needs_reconciliation);
        // Step 4: a STALE Open observation — still parks (one-hop HIGH-1).
        let d = evaluate_transition(&state, &obs(S::Open, Some(0), ObservationSource::Live));
        assert_eq!(d.outcome, TransitionOutcome::Park, "stale Open must park");
        assert!(d.needs_reconciliation);
        // Step 5: EXECUTED arrives — the race arm MUST fire (the two-hop
        // mask would have left the tracked state at Open and missed it).
        let d = evaluate_transition(&state, &obs(S::Executed, Some(50), ObservationSource::Live));
        assert_eq!(d.outcome, TransitionOutcome::Transition);
        assert!(
            d.cancel_lost_race,
            "cancel_lost_race must survive the 2-hop lateral mask"
        );
    }

    // --- THE table-driven TOTALITY test (graft 6): every
    //     (status × fill × current-state × source) lands on exactly one
    //     outcome, and the classification is internally consistent.
    //     Totality pin note: the stale-counter dimension deliberately
    //     samples {0, 1}; the ≥2 escalated region is covered by the
    //     dedicated escalation tests
    //     (`test_reconcile_backward_is_stale_skip_then_escalates_on_second_sweep`,
    //     `test_live_park_preserves_stale_reconcile_counter`). ---

    #[test]
    fn test_totality_every_status_fill_state_source_tuple_classified() {
        let fills: [(i64, Option<i64>); 7] = [
            (0, Some(0)),   // no fill
            (10, Some(5)),  // decrease (violation class)
            (10, Some(10)), // flat partial
            (10, Some(50)), // increase to full
            (10, None),     // MEDIUM-7: absent observed fill
            (0, Some(-5)),  // MEDIUM-7: negative observed fill (decrease)
            (-5, Some(-5)), // MEDIUM-7: negative tracked, flat
        ];
        let mut checked = 0_u32;
        for current_status in all_statuses() {
            for obs_status in all_statuses() {
                for (cur_fill, obs_fill) in fills {
                    for source in [ObservationSource::Live, ObservationSource::Reconcile] {
                        for stale in [0_u8, 1] {
                            let state = TrackedOrderState {
                                status: current_status.clone(),
                                filled_quantity: cur_fill,
                                consecutive_stale_reconcile: stale,
                            };
                            let o = obs(obs_status.clone(), obs_fill, source);
                            let d = evaluate_transition(&state, &o);
                            checked += 1;

                            // Exactly-one-outcome consistency pins:
                            match d.outcome {
                                TransitionOutcome::SameStatusRefresh => {
                                    assert_eq!(o.status, state.status);
                                }
                                TransitionOutcome::Transition => {
                                    assert_ne!(o.status, state.status);
                                    assert!(
                                        !matches!(o.status, GrowwOrderStatus::Unknown(_)),
                                        "Unknown must never be adopted"
                                    );
                                }
                                TransitionOutcome::StaleSnapshotSkip => {
                                    assert_eq!(
                                        source,
                                        ObservationSource::Reconcile,
                                        "stale-skip is reconcile-only"
                                    );
                                    assert!(d.next_stale_reconcile_count > stale);
                                }
                                TransitionOutcome::Park => {
                                    // Park = unknown obs, or illegal live move,
                                    // or (unreachable) fail-closed arm.
                                    assert!(
                                        d.unknown_status || d.needs_reconciliation,
                                        "park must carry a reason: {state:?} {o:?}"
                                    );
                                }
                            }
                            // Unknown observed status is ALWAYS a park.
                            if matches!(o.status, GrowwOrderStatus::Unknown(_)) {
                                assert_eq!(d.outcome, TransitionOutcome::Park);
                                assert!(d.unknown_status);
                            }
                            // cancel_lost_race only ever fires from
                            // CANCELLATION_REQUESTED into the fill class.
                            if d.cancel_lost_race {
                                assert_eq!(state.status, GrowwOrderStatus::CancellationRequested);
                                assert!(is_fill_class(&o.status));
                            }
                            // terminal_with_fill implies a terminal status
                            // was involved on the observed side.
                            if d.terminal_with_fill {
                                assert!(is_terminal(&o.status));
                            }
                        }
                    }
                }
            }
        }
        // 14 statuses × 14 statuses × 7 fills × 2 sources × 2 stale = 10976.
        assert_eq!(checked, 14 * 14 * 7 * 2 * 2);
    }

    // --- proptest: the classifier is total + never panics on arbitrary
    //     status strings (incl. case variance) and arbitrary fills ---

    proptest! {
        #[test]
        fn prop_parse_status_never_panics_and_classifier_total(
            raw_current in "\\PC{0,24}",
            raw_obs in "\\PC{0,24}",
            cur_fill in -1000_i64..1_000_000,
            obs_fill in proptest::option::of(-1000_i64..1_000_000),
            live in proptest::bool::ANY,
            stale in 0_u8..4,
        ) {
            let state = TrackedOrderState {
                status: GrowwOrderStatus::parse(&raw_current),
                filled_quantity: cur_fill,
                consecutive_stale_reconcile: stale,
            };
            let o = OrderObservation {
                status: GrowwOrderStatus::parse(&raw_obs),
                filled_quantity: obs_fill,
                source: if live { ObservationSource::Live } else { ObservationSource::Reconcile },
            };
            let d = evaluate_transition(&state, &o);
            // Unknown obs never transitions (hints/garbage can never move state).
            if matches!(o.status, GrowwOrderStatus::Unknown(_)) {
                prop_assert_eq!(d.outcome, TransitionOutcome::Park);
            }
        }

        /// Case-insensitivity: parsing any casing of a known value yields a
        /// known (non-Unknown) status.
        #[test]
        fn prop_parse_status_case_insensitive(idx in 0_usize..13, upper in proptest::bool::ANY) {
            let known = [
                "NEW", "ACKED", "TRIGGER_PENDING", "APPROVED", "OPEN",
                "MODIFICATION_REQUESTED", "CANCELLATION_REQUESTED", "EXECUTED",
                "DELIVERY_AWAITED", "COMPLETED", "REJECTED", "FAILED", "CANCELLED",
            ];
            let raw = if upper {
                known[idx].to_ascii_uppercase()
            } else {
                known[idx].to_ascii_lowercase()
            };
            let parsed = GrowwOrderStatus::parse(&raw);
            prop_assert!(!matches!(parsed, GrowwOrderStatus::Unknown(_)), "{}", raw);
        }
    }
}

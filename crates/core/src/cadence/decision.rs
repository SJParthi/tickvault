//! Event-driven per-lane decide/skip: the cycle FSM (design §7), the
//! exactly-once decision latch, [`DecisionSnapshot`] and the DRY-RUN log
//! sink (structured `info!` + counters — no strategy wiring, §28
//! boundary).

use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;
use tracing::{error, info};

use super::assembly::{MoneynessFold, SpotProvenance};
use crate::pipeline::chain_snapshot::ChainUnderlying;

/// The per-lane per-cycle FSM states (design §7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CadenceState {
    /// Waiting for the cycle's anchor (or off-session / lane disabled).
    Idle,
    /// The anchor was reached; no fetch dispatched yet.
    Armed,
    /// ≥1 fetch dispatched; assembling.
    Fetching,
    /// The predicate completed on own-broker data only.
    Complete,
    /// The predicate completed via cross-fill / chain-embedded / degraded
    /// provenance.
    DegradedComplete,
    /// The cycle was honest-skipped (cutoff / both sources dead / all
    /// Unknown).
    Skipped,
    /// The decision was emitted (own data).
    Decided,
    /// The decision was emitted, stamped degraded.
    DecidedDegraded,
}

/// The FSM inputs (design §7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CadenceEvent {
    /// The cycle anchor was reached with the session open and the lane
    /// enabled.
    AnchorReached,
    /// Off-session or lane disabled at the anchor.
    OffSessionOrDisabled,
    /// The first fetch of the cycle was dispatched.
    FirstFetchDispatched,
    /// The data-complete predicate flipped true on own-broker data only.
    PredicateCompleteOwn,
    /// The predicate flipped true via degraded provenance.
    PredicateCompleteDegraded,
    /// The lane's cutoff elapsed while incomplete.
    CutoffElapsed,
    /// Both brokers are dead for this cycle.
    BothSourcesDead,
    /// The decision snapshot was emitted.
    DecisionEmitted,
    /// Rollover to the next minute boundary.
    Rollover,
    /// Shutdown notified — drop the cycle, no partial emit.
    Shutdown,
}

/// Pure, total-over-legal-inputs FSM transition — `None` = ILLEGAL
/// (design §7; the runner debug-asserts legality and refuses the move).
#[must_use]
pub fn next_cadence_state(state: CadenceState, event: CadenceEvent) -> Option<CadenceState> {
    use CadenceEvent as E;
    use CadenceState as S;
    // Shutdown from ANY state drops the cycle (no partial emit).
    if event == E::Shutdown {
        return Some(S::Idle);
    }
    match (state, event) {
        (S::Idle, E::AnchorReached) => Some(S::Armed),
        (S::Idle, E::OffSessionOrDisabled) => Some(S::Idle),
        (S::Armed, E::FirstFetchDispatched) => Some(S::Fetching),
        // An armed lane whose cutoff fires before any dispatch (e.g. the
        // lane found nothing to fetch) still resolves honestly.
        (S::Armed | S::Fetching, E::CutoffElapsed | E::BothSourcesDead) => Some(S::Skipped),
        (S::Fetching, E::PredicateCompleteOwn) => Some(S::Complete),
        (S::Fetching, E::PredicateCompleteDegraded) => Some(S::DegradedComplete),
        (S::Complete, E::DecisionEmitted) => Some(S::Decided),
        (S::DegradedComplete, E::DecisionEmitted) => Some(S::DecidedDegraded),
        (S::Decided | S::DecidedDegraded | S::Skipped, E::Rollover) => Some(S::Idle),
        _ => None,
    }
}

/// Why a lane's decision was honest-skipped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// The lane was incomplete at its staleness cutoff.
    Cutoff,
    /// Both brokers produced nothing usable this cycle.
    BothSourcesDead,
    /// The predicate completed but every underlying folded all-Unknown
    /// (invalid spots ⇒ unusable moneyness, design §6).
    AllUnknown,
}

impl SkipReason {
    /// Stable stage label (matches the CADENCE-02 stage taxonomy).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cutoff => "cutoff",
            Self::BothSourcesDead => "both_sources_dead",
            Self::AllUnknown => "all_unknown",
        }
    }
}

/// The decision outcome carried on a [`DecisionSnapshot`] — Decided XOR
/// Skipped, exactly one per (lane, cycle).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecisionOutcome {
    /// Decided on own-broker data.
    Decided,
    /// Decided via degraded provenance (cross-fill / chain-embedded /
    /// pre-close↔post-close mix) — stamped, never hidden.
    DecidedDegraded,
    /// Honest-skipped with the carried reason.
    Skipped(SkipReason),
}

impl DecisionOutcome {
    /// Stable counter label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Decided => "decided",
            Self::DecidedDegraded => "decided_degraded",
            Self::Skipped(_) => "skipped",
        }
    }
}

/// One lane's per-cycle decision snapshot — the dry-run deliverable
/// (design §5 "Fire rule": emitted the INSTANT the predicate flips,
/// latched on (lane, cycle_minute), exactly-once).
#[derive(Clone, Copy, Debug)]
pub struct DecisionSnapshot {
    /// The deciding lane.
    pub lane: Feed,
    /// The decided minute (MINUTE-OPEN, IST secs-of-day).
    pub cycle_minute_ist: u32,
    /// The outcome (Decided XOR Skipped).
    pub outcome: DecisionOutcome,
    /// TRUE when the advisory VIX spot was absent.
    pub vix_missing: bool,
    /// TRUE for the session's last cycle (T = 15:30:00) — kept in
    /// dry-run, log-only (design §1 "Day edges").
    pub post_close: bool,
    /// Decision latency: emit instant − T, ms (negative only for Dhan's
    /// theoretical pre-close completion — not reachable with post-close
    /// spots).
    pub latency_ms: i64,
    /// Per-underlying moneyness folds (NIFTY / BANKNIFTY / SENSEX order).
    pub moneyness: [MoneynessFold; ChainUnderlying::COUNT],
    /// Per-underlying spot provenance (None = unresolved at skip time).
    pub spot_provenance: [Option<SpotProvenance>; ChainUnderlying::COUNT],
}

/// The exactly-once decision latch: at most ONE outcome per
/// (lane, cycle_minute) — Decided XOR Skipped (design §5).
#[derive(Debug, Default)]
pub struct DecisionLatch {
    /// Last latched cycle minute per feed index (None = never).
    latched: [Option<u32>; Feed::COUNT],
}

impl DecisionLatch {
    /// A fresh latch (nothing decided).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Attempt to latch (lane, cycle_minute). TRUE exactly once per pair;
    /// FALSE for any repeat (the caller must NOT emit).
    #[must_use]
    pub fn try_latch(&mut self, lane: Feed, cycle_minute_ist: u32) -> bool {
        let slot = &mut self.latched[lane.index()];
        if *slot == Some(cycle_minute_ist) {
            return false;
        }
        *slot = Some(cycle_minute_ist);
        true
    }
}

/// Emit one decision snapshot to the DRY-RUN sink: structured `info!` +
/// `tv_cadence_decision_total{lane,outcome}` (+ the latency histogram);
/// a skip additionally fires the coded CADENCE-02 `error!` (Rule 11 — a
/// skip is never rendered OK). NO strategy wiring.
///
/// `dry_run` (verifier F10, dated 2026-07-15): when the wiring runs
/// DRY-RUN executors (every fetch structurally returns `Empty`), every
/// cycle's skips are the EXPECTED SHAPE — ~1,500 High `error!` lines/day
/// of pure noise. A dry-run skip therefore logs at `info!` with a
/// `dry_run = true` field (counters unchanged — the trend survives);
/// REAL executor wirings keep the coded `error!`.
pub fn emit_decision(snapshot: &DecisionSnapshot, dry_run: bool) {
    metrics::counter!(
        "tv_cadence_decision_total",
        "lane" => snapshot.lane.as_str(),
        "outcome" => snapshot.outcome.as_str()
    )
    .increment(1);
    // APPROVED: bounded lane latency (≤ cutoff ms) — precision loss is nil.
    #[allow(clippy::cast_precision_loss)]
    metrics::histogram!(
        "tv_cadence_decision_latency_ms",
        "lane" => snapshot.lane.as_str()
    )
    .record(snapshot.latency_ms as f64);

    let unknown_total: u32 = snapshot.moneyness.iter().map(|f| f.unknown).sum();
    let rows_total: u32 = snapshot.moneyness.iter().map(|f| f.rows).sum();
    match snapshot.outcome {
        DecisionOutcome::Skipped(reason) if dry_run => {
            info!(
                dry_run = true,
                stage = reason.as_str(),
                lane = snapshot.lane.as_str(),
                cycle_minute_ist = snapshot.cycle_minute_ist,
                latency_ms = snapshot.latency_ms,
                post_close = snapshot.post_close,
                "cadence lane skip under DRY-RUN executors (expected \
                 shape — every dry-run fetch returns Empty; F10 demotion)"
            );
        }
        DecisionOutcome::Skipped(reason) => {
            error!(
                code = ErrorCode::Cadence02DecisionSkipped.code_str(),
                stage = reason.as_str(),
                lane = snapshot.lane.as_str(),
                cycle_minute_ist = snapshot.cycle_minute_ist,
                latency_ms = snapshot.latency_ms,
                post_close = snapshot.post_close,
                "CADENCE-02: lane decision honest-skipped — never a late \
                 decision, never a decision on missing/stale data"
            );
        }
        DecisionOutcome::Decided | DecisionOutcome::DecidedDegraded => {
            info!(
                lane = snapshot.lane.as_str(),
                outcome = snapshot.outcome.as_str(),
                cycle_minute_ist = snapshot.cycle_minute_ist,
                latency_ms = snapshot.latency_ms,
                vix_missing = snapshot.vix_missing,
                post_close = snapshot.post_close,
                rows = rows_total,
                unknown = unknown_total,
                "cadence decision (dry-run sink — no strategy wiring)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_next_cadence_state_full_table_and_illegal_moves() {
        use CadenceEvent as E;
        use CadenceState as S;
        // The legal table, row by row (design §7).
        assert_eq!(
            next_cadence_state(S::Idle, E::AnchorReached),
            Some(S::Armed)
        );
        assert_eq!(
            next_cadence_state(S::Idle, E::OffSessionOrDisabled),
            Some(S::Idle)
        );
        assert_eq!(
            next_cadence_state(S::Armed, E::FirstFetchDispatched),
            Some(S::Fetching)
        );
        assert_eq!(
            next_cadence_state(S::Fetching, E::PredicateCompleteOwn),
            Some(S::Complete)
        );
        assert_eq!(
            next_cadence_state(S::Fetching, E::PredicateCompleteDegraded),
            Some(S::DegradedComplete)
        );
        assert_eq!(
            next_cadence_state(S::Fetching, E::CutoffElapsed),
            Some(S::Skipped)
        );
        assert_eq!(
            next_cadence_state(S::Fetching, E::BothSourcesDead),
            Some(S::Skipped)
        );
        assert_eq!(
            next_cadence_state(S::Complete, E::DecisionEmitted),
            Some(S::Decided)
        );
        assert_eq!(
            next_cadence_state(S::DegradedComplete, E::DecisionEmitted),
            Some(S::DecidedDegraded)
        );
        for terminal in [S::Decided, S::DecidedDegraded, S::Skipped] {
            assert_eq!(next_cadence_state(terminal, E::Rollover), Some(S::Idle));
        }
        // Shutdown from ANY state drops the cycle.
        for s in [
            S::Idle,
            S::Armed,
            S::Fetching,
            S::Complete,
            S::DegradedComplete,
            S::Skipped,
            S::Decided,
            S::DecidedDegraded,
        ] {
            assert_eq!(next_cadence_state(s, E::Shutdown), Some(S::Idle));
        }
        // Illegal moves are None — never a silent wrong state.
        assert_eq!(next_cadence_state(S::Idle, E::DecisionEmitted), None);
        assert_eq!(
            next_cadence_state(S::Decided, E::PredicateCompleteOwn),
            None
        );
        assert_eq!(next_cadence_state(S::Skipped, E::CutoffElapsed), None);
        assert_eq!(next_cadence_state(S::Complete, E::Rollover), None);
    }

    #[test]
    fn test_cadence_decision_latch_one_per_lane_per_cycle() {
        let mut latch = DecisionLatch::new();
        // Exactly once per (lane, cycle).
        assert!(latch.try_latch(Feed::Dhan, 35_940));
        assert!(!latch.try_latch(Feed::Dhan, 35_940), "no double decision");
        // The OTHER lane's latch is independent for the same cycle.
        assert!(latch.try_latch(Feed::Groww, 35_940));
        assert!(!latch.try_latch(Feed::Groww, 35_940));
        // The next cycle re-arms both lanes.
        assert!(latch.try_latch(Feed::Dhan, 36_000));
        assert!(latch.try_latch(Feed::Groww, 36_000));
    }

    #[test]
    fn test_cadence_decision_try_latch_fresh_and_day_rollover_semantics() {
        // A fresh latch admits the first minute of a new day even though
        // its numeric value is SMALLER than yesterday's last latched
        // minute (cycle minutes are day-scoped seconds-of-day; within a
        // day the runner's strictly-after-last boundary selection makes
        // repeats impossible).
        let mut latch = DecisionLatch::new();
        assert!(latch.try_latch(Feed::Dhan, 55_740)); // yesterday 15:29
        assert!(latch.try_latch(Feed::Dhan, 33_300)); // today 09:15
        assert!(
            !latch.try_latch(Feed::Dhan, 33_300),
            "same-pair repeat refused"
        );
    }

    #[test]
    fn ratchet_runner_day_start_reset_recreates_decision_latch() {
        // CAD-NEW-1 / CONC-1 (hostile-review round 1, 2026-07-15): cycle
        // minutes are BARE minute-of-day values that recur every day. A
        // latch slot frozen across the IST day flip (parked lanes via
        // /api/feeds, a midnight suspend's Abandoned cycle) collides with
        // the same minute-of-day the next day: try_latch refuses, the
        // (lane, minute) gets ZERO Decided/Skipped outcomes (an
        // exactly-once hole in the zero direction) and the should-never
        // double_latch + illegal_fsm_move channels fire for a routine
        // pattern. The runner's day-start reset block MUST therefore
        // re-create the latch alongside the ladders.
        let src = include_str!("runner.rs");
        let start = src
            .find("Day-start reset")
            .expect("runner day-start reset block present");
        let window = &src[start..start.saturating_add(3_000).min(src.len())];
        assert!(
            window.contains("latch = DecisionLatch::new()"),
            "the runner's day-start reset block must reset the \
             DecisionLatch (CAD-NEW-1/CONC-1)"
        );
    }

    #[test]
    fn test_emit_decision_skip_reason_and_outcome_labels_stable() {
        // Stable wire labels (counters + CADENCE-02 stage taxonomy).
        assert_eq!(SkipReason::Cutoff.as_str(), "cutoff");
        assert_eq!(SkipReason::BothSourcesDead.as_str(), "both_sources_dead");
        assert_eq!(SkipReason::AllUnknown.as_str(), "all_unknown");
        assert_eq!(DecisionOutcome::Decided.as_str(), "decided");
        assert_eq!(
            DecisionOutcome::DecidedDegraded.as_str(),
            "decided_degraded"
        );
        assert_eq!(
            DecisionOutcome::Skipped(SkipReason::Cutoff).as_str(),
            "skipped"
        );
        // emit_decision is total over both arms AND both dry_run modes
        // (smoke — sink is logs+counters only; no panic, no strategy
        // side effects). F10: dry_run=true demotes the skip to info!;
        // dry_run=false keeps the coded CADENCE-02 error!.
        let snap = DecisionSnapshot {
            lane: Feed::Groww,
            cycle_minute_ist: 35_940,
            outcome: DecisionOutcome::Skipped(SkipReason::BothSourcesDead),
            vix_missing: true,
            post_close: false,
            latency_ms: 6_000,
            moneyness: [MoneynessFold::default(); ChainUnderlying::COUNT],
            spot_provenance: [None; ChainUnderlying::COUNT],
        };
        emit_decision(&snap, false);
        emit_decision(&snap, true);
        let decided = DecisionSnapshot {
            outcome: DecisionOutcome::Decided,
            ..snap
        };
        emit_decision(&decided, false);
        emit_decision(&decided, true);
    }
}

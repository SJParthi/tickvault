//! The ADAPTIVE ladders (operator directives 2026-07-15 + 2026-07-16) +
//! the in-cycle retry policy.
//!
//! All three ladders share ONE primitive — the streak-driven
//! [`StreakLadder`]: degrade one step after
//! `concurrency_degrade_after_dirty_cycles` (default 2, Assumed — flagged
//! for operator confirm) CONSECUTIVE dirty cycles, recover one step after
//! `concurrency_recover_after_clean_cycles` (default 3, Assumed)
//! consecutive fully-clean cycles — never permanently stuck from a
//! one-off 429. Every ladder is day-scoped (reset at day start / boot).
//!
//! - **Dhan SHAPE ladder** (2026-07-16 — replaces the retired pre-close
//!   `:55 → :50` anchor-shift ladder): rung 0 (primary) = second 1 →
//!   ALL 7 requests concurrent (3 chains + 4 spots — the operator's
//!   same-day correction; the interim 5+2 packing is SUPERSEDED, §0b of
//!   the rule file); rung 1 (fallback) = second 1 → 3 chains, second 2
//!   → all 4 spots. Both rungs are fully POST-close — no pre-fire
//!   exists anymore.
//! - **Dhan spot-concurrency ladder** — tier steps 0..=3 encode the
//!   per-second spot capacity `4 → 3 → 2 → 1`; the shape rung's
//!   second-buckets and the tier capacity compose via
//!   [`spot_second_buckets`] (greedy per-second-group fill — degradation
//!   happens WITHIN each second group, spilling overflow to later
//!   1000ms-spaced seconds).
//! - **Groww fallback-shape ladder** — the three-choice ladder
//!   (2026-07-15): choice 1 = `:00` all-7-parallel (the operator's
//!   primary); choice 2 = `:01` chains / `:02` ALL 4 spots (the
//!   operator's 2026-07-16 split fallback); choice 3 (last resort,
//!   BEYOND the operator's two-rung prescription — a more-conservative
//!   final bounded degrade, dated note in the rule file) = `:01` chains
//!   / `:02` core spots / `:03` VIX alone.

use super::executor::CadenceFetchError;

/// Highest Dhan SHAPE step — step 1 = the operator's split fallback
/// (chains second 1, all spots second 2). Step 0 is the ALL-7 concurrent
/// burst (2026-07-16 directive + same-day correction).
pub const DHAN_SHAPE_MAX_STEP: u8 = 1;

/// Per-IST-day cap on Dhan shape-ladder RE-ENTRIES to rung 0 (RS1(b),
/// 2026-07-16 — the termination BELT for the UNVERIFIED-LIVE chain-bucket
/// exemption, rule file §0b): if the wire enforces ONE Data-API bucket,
/// every recovered all-7 burst 429s again and the streak ladder would
/// oscillate 0⇄1 ALL DAY (recover after 3 clean rung-1 cycles → 429 → 2
/// dirty → demote → repeat), re-armed every morning by the day-start
/// reset — and `ladder_exhausted` never fires because rung 1 stays clean.
/// After this many same-day re-entries to rung 0, the NEXT demotion is
/// FINAL for the IST day: the ladder HOLDS rung 1 (the split fallback)
/// until the day-start reset re-arms it, and the runner emits ONE
/// edge-latched CADENCE-01 `rung0_reentry_cap_latched` log for the day.
pub const CADENCE_DHAN_RUNG0_REENTRY_CAP_PER_DAY: u32 = 3;

/// Highest Dhan spot-concurrency step — step 3 = fully sequential 1×4.
pub const SPOT_CONCURRENCY_MAX_STEP: u8 = 3;

/// Highest Groww fallback-shape step — step 2 = the three-second split
/// (choice 3, the LAST resort).
pub const GROWW_SHAPE_MAX_STEP: u8 = 2;

/// A streak-driven adaptive ladder (shared primitive of the Dhan shape
/// ladder, the Dhan spot-concurrency ladder and the Groww fallback-shape
/// ladder): degrade ONE step after N consecutive dirty cycles, recover
/// one step after M consecutive clean cycles. Steps are clamped to
/// `[min_step, max_step]` at every transition.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreakLadder {
    /// Current step (0 = the most concurrent shape).
    pub step: u8,
    /// Consecutive dirty cycles at the current step.
    dirty_streak: u32,
    /// Consecutive fully-clean cycles at the current step.
    clean_streak: u32,
}

/// A [`StreakLadder::advance`] transition (None = no step change).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreakShift {
    /// Stepped DOWN the shape ladder (step + 1, less concurrent).
    Degraded,
    /// Stepped back UP (step − 1, more concurrent).
    Recovered,
}

impl StreakLadder {
    /// A ladder starting at `min_step` (the structural floor — e.g. a
    /// `spot_window_cap` below the full group size floors the Dhan spot
    /// step via [`min_spot_step_for_cap`]).
    #[must_use]
    // TEST-EXEMPT: covered by the StreakLadder unit tests (floor-start arms; guard name-pattern mismatch).
    pub fn starting_at(min_step: u8) -> Self {
        Self {
            step: min_step,
            dirty_streak: 0,
            clean_streak: 0,
        }
    }

    /// Fold one whole-cycle verdict: `dirty` = ≥1 rate-limited outcome on
    /// the ladder's legs this cycle. Degrades one step after
    /// `degrade_after` CONSECUTIVE dirty cycles ("continuous", never a
    /// one-off); recovers one step after `recover_after` consecutive
    /// fully-clean cycles. A dirty cycle resets the clean streak and vice
    /// versa; a shift resets both streaks (the next shift needs a fresh
    /// full streak at the NEW step). Pure state fold — no I/O.
    pub fn advance(
        &mut self,
        dirty: bool,
        degrade_after: u32,
        recover_after: u32,
        min_step: u8,
        max_step: u8,
    ) -> Option<StreakShift> {
        // Defensive clamp (config floors can only move between boots; the
        // in-memory step must never sit outside the legal band).
        self.step = self.step.clamp(min_step, max_step);
        if dirty {
            self.clean_streak = 0;
            self.dirty_streak = self.dirty_streak.saturating_add(1);
            if self.dirty_streak >= degrade_after && self.step < max_step {
                self.step = self.step.saturating_add(1);
                self.dirty_streak = 0;
                return Some(StreakShift::Degraded);
            }
            None
        } else {
            self.dirty_streak = 0;
            self.clean_streak = self.clean_streak.saturating_add(1);
            if self.clean_streak >= recover_after && self.step > min_step {
                self.step = self.step.saturating_sub(1);
                self.clean_streak = 0;
                return Some(StreakShift::Recovered);
            }
            None
        }
    }
}

/// Day-scoped bookkeeping for [`CADENCE_DHAN_RUNG0_REENTRY_CAP_PER_DAY`]
/// (RS1(b), 2026-07-16). Pure state fold — no I/O; the runner resets it
/// (alongside the ladders) at the IST day start, which re-arms the cap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DhanRung0ReentryCap {
    /// Recoveries back to rung 0 observed this IST day.
    reentries_today: u32,
    /// The cap latched — rung 1 is held for the rest of the session.
    latched: bool,
}

impl DhanRung0ReentryCap {
    /// Fold one Dhan shape-ladder shift. Returns `true` exactly ONCE per
    /// IST day — at the demotion that follows the cap-th re-entry (the
    /// moment the hold becomes binding) — so the caller can emit the
    /// edge-latched `rung0_reentry_cap_latched` log. Below the cap this
    /// is pure bookkeeping and behavior is unchanged.
    pub fn record_shift(&mut self, shift: StreakShift) -> bool {
        match shift {
            StreakShift::Recovered => {
                self.reentries_today = self.reentries_today.saturating_add(1);
                false
            }
            StreakShift::Degraded => {
                if self.reentries_today >= CADENCE_DHAN_RUNG0_REENTRY_CAP_PER_DAY && !self.latched {
                    self.latched = true;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// The EFFECTIVE min step for the Dhan shape ladder: rung 1
    /// ([`DHAN_SHAPE_MAX_STEP`]) once latched — recovery back to rung 0
    /// is refused for the rest of the IST day — else 0. Passing this as
    /// `min_step` into [`StreakLadder::advance`] is clamp-safe: the
    /// latch only fires ON a demotion, so the ladder already sits AT
    /// rung 1 when the floor rises (the defensive clamp is a no-op).
    #[must_use]
    pub fn min_step(&self) -> u8 {
        if self.latched { DHAN_SHAPE_MAX_STEP } else { 0 }
    }
}

/// The Dhan spot SECOND-BUCKET assignment (2026-07-16): which
/// 1000ms-spaced second bucket (0-based from the burst second) spot
/// target `target_idx` (`SpotTarget::ALL` order:
/// NIFTY/BANKNIFTY/SENSEX/INDIA VIX) fires in, composed from the SHAPE
/// rung and the spot-concurrency TIER step.
///
/// - Shape rung 0 (the operator's ALL-7 primary — correction
///   2026-07-16: *"for dhan also as the primary all 7 parallel at first
///   second"*; the interim 5+2 packing is SUPERSEDED, recorded in the
///   rule file's §0b) bases ALL 4 spots in bucket 0 — the burst second,
///   alongside the 3 concurrent chains.
/// - Shape rung ≥1 (split fallback — chains second 1, spots second 2)
///   bases ALL 4 spots in bucket 1.
/// - The tier step caps the per-bucket SPOT count at `4 − step`
///   (clamped ≥1): degradation happens WITHIN each second group, greedy
///   overflow spilling to the NEXT 1000ms bucket (per-second-group tier
///   math, coordinator addendum item 4).
///
/// Budget safety (the TWO-BUCKET model, operator correction 2026-07-16 —
/// the chain-bucket EXEMPTION is **Assumed / UNVERIFIED-LIVE**, RS1
/// honesty marker in the rule file §0b): the burst second is 3 chains +
/// ≤4 spots. The 4 spot fires sit in the Data-API 5/sec bucket (4 ≤ 5);
/// the 3 chain fires are gated ONLY by the option-chain API's
/// per-(underlying, expiry) rule (different underlyings explicitly
/// concurrent per Dhan's documented rule) — but whether Dhan ALSO counts
/// chain fires against the Data-API 5/sec bucket is UNVERIFIED
/// (counter-evidence: `dhan/api-introduction.md` rule 10 lists Option
/// Chain under Data APIs 5/sec, and the §8 grant math historically
/// counted chains + spots jointly). The 2026-07-16 15:35 IST post-market
/// wire probe is the first live evidence; if the wire enforces ONE
/// bucket, the RateLimited-armed shape ladder demotes and the per-day
/// rung-0 re-entry cap ([`CADENCE_DHAN_RUNG0_REENTRY_CAP_PER_DAY`])
/// bounds the oscillation. Assignments are non-decreasing in
/// `target_idx`. Pure, zero-alloc.
#[must_use]
pub fn spot_second_buckets(dhan_shape: u8, spot_step: u8) -> [usize; 4] {
    let base: [usize; 4] = if dhan_shape == 0 {
        [0, 0, 0, 0]
    } else {
        [1, 1, 1, 1]
    };
    // Per-bucket spot capacity at this tier: 4 → 3 → 2 → 1 (clamped ≥1).
    let cap = usize::from(4u8.saturating_sub(spot_step.min(3)).max(1));
    // Max reachable bucket: base 1 + 3 overflow steps = 4 (< 8).
    let mut counts = [0usize; 8];
    let mut out = [0usize; 4];
    for (k, slot) in out.iter_mut().enumerate() {
        let mut b = base[k];
        while counts[b] >= cap {
            b += 1;
        }
        counts[b] += 1;
        *slot = b;
    }
    out
}

/// The STRUCTURAL concurrency-step floor for a configured
/// `spot_window_cap`: no second-bucket's SPOT count may exceed the
/// rolling-window cap (a cap of 4..=5 admits the full step-0 group;
/// 3 → step 1; 2 → step 2; 1 → fully sequential). Pure.
#[must_use]
// TEST-EXEMPT: covered by the spot-cap floor arms of the StreakLadder unit tests (guard name-pattern mismatch).
pub fn min_spot_step_for_cap(cap: u32) -> u8 {
    match cap {
        0 | 1 => 3, // 0 is unreachable post-validate; fail-closed sequential
        2 => 2,
        3 => 1,
        _ => 0,
    }
}

/// The Groww fallback-shape WAVE indices `(chains, core_spots, vix_spot)`
/// — each a 0-based multiple of `CADENCE_GROWW_WAVE_STEP_MS` past the
/// burst anchor. Choice 1 (step 0): all at `:00`; choice 2 (step 1):
/// chains `:01`, ALL 4 spots `:02`; choice 3 (step 2, last resort):
/// chains `:01`, core spots `:02`, VIX alone `:03` (deliberately lowest
/// priority — context-only). Steps past the max clamp to choice 3. Pure.
#[must_use]
pub const fn groww_wave_indices(shape: u8) -> (usize, usize, usize) {
    match shape {
        0 => (0, 0, 0),
        1 => (1, 2, 2),
        _ => (1, 2, 3),
    }
}

/// Does this fetch failure ARM the ladders? (operator correction
/// 2026-07-16, verbatim: *"see that too instantly dont commit — one and
/// only when you tried that multiple times and gets rate limited alone
/// alone fallback"*.)
///
/// - `RateLimited` (a 429) is the SOLE arming class — the fallback shape
///   exists to relieve broker rate pressure and nothing else.
/// - `Timeout` / `Transport` do NOT arm (operator correction 2026-07-16
///   — supersedes the interim "both buckets" reading that armed them):
///   a slow/flaky vendor is not rate pressure; reshaping the burst
///   cannot fix it. Both still degrade the lane loudly via CADENCE-01.
/// - `Empty` (Dhan spot 200-with-zero-candles) does NOT arm: a shape
///   degrade cannot fix a vendor serving-lag saga — arming on the 14-day
///   200-empty class would flap the shape every minute (judge ruling,
///   design §0).
/// - `Auth` / `Malformed` do NOT arm: neither is a pacing signal
///   (reshaping the burst cannot fix a dead token or a schema drift);
///   both still degrade the lane loudly via CADENCE-01.
/// - `QueueDelay` does NOT arm (verifier F1(iii), dated 2026-07-15): a
///   nominal gate deferral is SELF-INFLICTED pacing — the scheduler's
///   own composition with its gates — not a vendor signal; arming on it
///   would let our own defense-in-depth walk the shape down forever.
#[must_use]
pub fn failure_arms_ladder(err: &CadenceFetchError) -> bool {
    matches!(err, CadenceFetchError::RateLimited { .. })
}

/// Per-class in-cycle retry budget (the 2026-07-20 amendment eligibility
/// matrix): Malformed => 0 (never retried); Empty on a SPOT leg => the
/// native ladder cap (CADENCE_NATIVE_RETRY_MAX_ATTEMPTS, never below
/// retry_max); everything else — including Empty on the CHAIN leg and
/// RateLimited — => the legacy retry_max.
#[must_use]
pub fn late_retry_budget(err: &CadenceFetchError, leg_is_chain: bool, retry_max: u32) -> u32 {
    match err {
        CadenceFetchError::Malformed => 0,
        CadenceFetchError::Empty if !leg_is_chain => {
            retry_max.max(tickvault_common::constants::CADENCE_NATIVE_RETRY_MAX_ATTEMPTS as u32)
        }
        _ => retry_max,
    }
}

/// May a failed request be retried IN-CYCLE? (design §3(b); RateLimited
/// arm reversed by the operator's 2026-07-16 correction.)
///
/// - A `RateLimited` leg KEEPS its bounded in-cycle retry — the retry
///   (through the gates, after the per-key spacing) is one of the
///   operator's "tried that multiple times" attempts; only the
///   multi-attempt rate-limited streak demotes the shape. (The interim
///   "429 kills the leg for the cycle" behavior is REVERSED — recorded
///   in the rule file's §0b.)
/// - At most `retry_max` retries per failed request (default 1 — one
///   retry per leg per cycle, never more).
/// - The retry must LAND: the gate's earliest-allowed fire instant plus
///   the p95 latency allowance must sit at/before the lane's cutoff —
///   otherwise the retry could only produce a late (discarded) response.
#[must_use]
pub fn may_retry_in_cycle(
    err: &CadenceFetchError,
    leg_is_chain: bool,
    retries_used: u32,
    retry_max: u32,
    earliest_fire_ms: i64,
    latency_allowance_ms: i64,
    lane_cutoff_abs_ms: i64,
) -> bool {
    if retries_used >= late_retry_budget(err, leg_is_chain, retry_max) {
        return false;
    }
    earliest_fire_ms.saturating_add(latency_allowance_ms) <= lane_cutoff_abs_ms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dhan_shape_ladder_rung0_rung1_transitions_under_streak_rules() {
        // 2026-07-16: the Dhan SHAPE ladder replaces the retired anchor
        // ladder — rung 0 (all-7 primary) ⇄ rung 1 (split fallback), under
        // the SAME 2-dirty / 3-clean streak thresholds the Groww shape
        // ladder uses.
        let mut l = StreakLadder::starting_at(0);
        // One-off arming failure never degrades…
        assert_eq!(l.advance(true, 2, 3, 0, DHAN_SHAPE_MAX_STEP), None);
        assert_eq!(l.step, 0);
        // …a clean cycle resets the dirty streak…
        assert_eq!(l.advance(false, 2, 3, 0, DHAN_SHAPE_MAX_STEP), None);
        assert_eq!(l.advance(true, 2, 3, 0, DHAN_SHAPE_MAX_STEP), None);
        assert_eq!(l.step, 0, "dirty-clean-dirty is never 'continuous'");
        // …2 CONSECUTIVE dirty cycles degrade rung 0 → rung 1.
        assert_eq!(
            l.advance(true, 2, 3, 0, DHAN_SHAPE_MAX_STEP),
            Some(StreakShift::Degraded)
        );
        assert_eq!(l.step, 1);
        // Dirty AT the max rung holds (there is no rung 2 — the shape
        // ladder tops out at the operator's split fallback).
        for _ in 0..4 {
            assert_eq!(l.advance(true, 2, 3, 0, DHAN_SHAPE_MAX_STEP), None);
        }
        assert_eq!(l.step, DHAN_SHAPE_MAX_STEP);
        // 3 CONSECUTIVE clean cycles recover rung 1 → rung 0.
        assert_eq!(l.advance(false, 2, 3, 0, DHAN_SHAPE_MAX_STEP), None);
        assert_eq!(l.advance(false, 2, 3, 0, DHAN_SHAPE_MAX_STEP), None);
        assert_eq!(
            l.advance(false, 2, 3, 0, DHAN_SHAPE_MAX_STEP),
            Some(StreakShift::Recovered)
        );
        assert_eq!(l.step, 0);
        // Clean at rung 0 stays home (never underflows).
        for _ in 0..5 {
            assert_eq!(l.advance(false, 2, 3, 0, DHAN_SHAPE_MAX_STEP), None);
        }
        assert_eq!(l.step, 0);
    }

    #[test]
    fn test_ladder_rate_limited_sole_arming_class_with_bounded_retry() {
        // Operator correction 2026-07-16 ("tried that multiple times and
        // gets rate limited alone alone fallback"): RateLimited is the
        // SOLE arming class…
        let rate_limited = CadenceFetchError::RateLimited {
            retry_after_ms: Some(2_000),
        };
        assert!(failure_arms_ladder(&rate_limited));
        // …and a rate-limited leg KEEPS its ONE bounded in-cycle retry
        // (through the gates, after the per-key spacing) — the retry is
        // one of the operator's "multiple attempts"; the interim
        // "429 kills the leg for the cycle" behavior is REVERSED.
        assert!(may_retry_in_cycle(
            &rate_limited,
            true,
            0,
            1,
            5_000,
            1_500,
            60_000
        ));
        // One retry per leg per cycle, never more.
        assert!(!may_retry_in_cycle(
            &rate_limited,
            true,
            1,
            1,
            5_000,
            1_500,
            60_000
        ));
        // Timeout / Transport (incl. 5xx at the seam) NEVER arm — they
        // degrade the lane loudly via CADENCE-01 but never reshape.
        assert!(!failure_arms_ladder(&CadenceFetchError::Timeout));
        assert!(!failure_arms_ladder(&CadenceFetchError::Transport));
        // Auth / Malformed degrade loudly but do not reshape the burst.
        assert!(!failure_arms_ladder(&CadenceFetchError::Auth));
        assert!(!failure_arms_ladder(&CadenceFetchError::Malformed));
    }

    #[test]
    fn test_ladder_spot_200_empty_does_not_arm() {
        // Assumed-rule pin (design §0 flagged deviation): a 200-empty spot
        // must NOT degrade the shape — a reshape cannot fix the vendor
        // serving-lag saga it would react to.
        assert!(!failure_arms_ladder(&CadenceFetchError::Empty));
        // An Empty IS still retryable in-cycle (one gated attempt).
        assert!(may_retry_in_cycle(
            &CadenceFetchError::Empty,
            true,
            0,
            1,
            5_000,
            1_500,
            15_000
        ));
    }

    #[test]
    fn test_may_retry_in_cycle_respects_gate_and_cutoff() {
        let e = CadenceFetchError::Transport;
        // Budget exhausted → no retry.
        assert!(!may_retry_in_cycle(&e, true, 1, 1, 5_000, 1_500, 60_000));
        // Gate's earliest instant + latency allowance past the cutoff →
        // the retry could only be a late response → refused.
        assert!(!may_retry_in_cycle(&e, true, 0, 1, 14_000, 1_500, 15_000));
        // Landing exactly AT the cutoff is admitted (inclusive).
        assert!(may_retry_in_cycle(&e, true, 0, 1, 13_500, 1_500, 15_000));
        // Room + budget → retry through the gates.
        assert!(may_retry_in_cycle(&e, true, 0, 1, 5_000, 1_500, 15_000));
    }

    #[test]
    fn test_cadence_ladder_failure_arms_ladder_is_total() {
        // failure_arms_ladder is total over the 7-variant error enum —
        // exactly ONE arms (RateLimited — the operator's 2026-07-16
        // rate-limit-only correction), 6 do not (a new variant must pick
        // a side here).
        let arms = [
            failure_arms_ladder(&CadenceFetchError::RateLimited {
                retry_after_ms: None,
            }),
            failure_arms_ladder(&CadenceFetchError::Timeout),
            failure_arms_ladder(&CadenceFetchError::Transport),
            failure_arms_ladder(&CadenceFetchError::Empty),
            failure_arms_ladder(&CadenceFetchError::QueueDelay),
            failure_arms_ladder(&CadenceFetchError::Auth),
            failure_arms_ladder(&CadenceFetchError::Malformed),
        ];
        assert_eq!(arms.iter().filter(|a| **a).count(), 1);
        assert_eq!(arms, [true, false, false, false, false, false, false]);
    }

    #[test]
    fn test_ladder_queue_delay_is_non_arming_but_retryable() {
        // F1(iii), 2026-07-15: a nominal gate deferral is SELF-INFLICTED
        // pacing — it must NEVER degrade the shape…
        assert!(!failure_arms_ladder(&CadenceFetchError::QueueDelay));
        // …but it IS retryable in-cycle (the pacing spill clears within
        // the window; one more gated attempt is legitimate).
        assert!(may_retry_in_cycle(
            &CadenceFetchError::QueueDelay,
            true,
            0,
            1,
            5_000,
            1_500,
            15_000
        ));
    }

    /// Drive one full demote-then-recover oscillation of the Dhan shape
    /// ladder through the cap: 2 dirty cycles (Degraded), then 3 clean
    /// cycles (Recovered). Returns the latch verdicts of the two shifts.
    fn oscillate_once(l: &mut StreakLadder, cap: &mut DhanRung0ReentryCap) -> (bool, bool) {
        assert_eq!(
            l.advance(true, 2, 3, cap.min_step(), DHAN_SHAPE_MAX_STEP),
            None
        );
        let demote = l
            .advance(true, 2, 3, cap.min_step(), DHAN_SHAPE_MAX_STEP)
            .expect("2 consecutive dirty cycles must demote");
        assert_eq!(demote, StreakShift::Degraded);
        let demote_latched = cap.record_shift(demote);
        assert_eq!(
            l.advance(false, 2, 3, cap.min_step(), DHAN_SHAPE_MAX_STEP),
            None
        );
        assert_eq!(
            l.advance(false, 2, 3, cap.min_step(), DHAN_SHAPE_MAX_STEP),
            None
        );
        let recover = l
            .advance(false, 2, 3, cap.min_step(), DHAN_SHAPE_MAX_STEP)
            .expect("3 consecutive clean cycles below the cap must recover");
        assert_eq!(recover, StreakShift::Recovered);
        let recover_latched = cap.record_shift(recover);
        (demote_latched, recover_latched)
    }

    #[test]
    fn test_dhan_rung0_reentry_cap_record_shift_below_cap_behavior_unchanged() {
        // RS1(b) test (iii): below the cap, oscillation behavior is
        // byte-identical to the uncapped ladder — 3 full demote-recover
        // oscillations complete, min_step stays 0, the latch never fires.
        let mut l = StreakLadder::starting_at(0);
        let mut cap = DhanRung0ReentryCap::default();
        for _ in 0..usize::try_from(CADENCE_DHAN_RUNG0_REENTRY_CAP_PER_DAY).unwrap() {
            let (demote_latched, recover_latched) = oscillate_once(&mut l, &mut cap);
            assert!(!demote_latched, "no latch below the cap");
            assert!(!recover_latched, "a recovery never latches");
            assert_eq!(l.step, 0, "recovery back to rung 0 admitted below the cap");
            assert_eq!(cap.min_step(), 0, "floor unchanged below the cap");
        }
    }

    #[test]
    fn test_dhan_rung0_reentry_cap_min_step_holds_rung1_and_latches_once() {
        // RS1(b) test (i): 3 demote-recover oscillations, then the 4th
        // demotion latches the cap EXACTLY ONCE — the 4th recovery
        // attempt is refused and the ladder holds rung 1 all session.
        let mut l = StreakLadder::starting_at(0);
        let mut cap = DhanRung0ReentryCap::default();
        for _ in 0..usize::try_from(CADENCE_DHAN_RUNG0_REENTRY_CAP_PER_DAY).unwrap() {
            oscillate_once(&mut l, &mut cap);
        }
        // The 4th demotion (2 consecutive dirty cycles) latches the cap.
        assert_eq!(
            l.advance(true, 2, 3, cap.min_step(), DHAN_SHAPE_MAX_STEP),
            None
        );
        let demote = l
            .advance(true, 2, 3, cap.min_step(), DHAN_SHAPE_MAX_STEP)
            .expect("the 4th demotion still happens");
        assert_eq!(demote, StreakShift::Degraded);
        assert!(
            cap.record_shift(demote),
            "the cap-exhausting demotion latches"
        );
        assert_eq!(
            cap.min_step(),
            DHAN_SHAPE_MAX_STEP,
            "rung 1 is the floor now"
        );
        // The 4th recovery attempt is REFUSED — any run of clean cycles
        // holds rung 1 for the rest of the session.
        for _ in 0..10 {
            assert_eq!(
                l.advance(false, 2, 3, cap.min_step(), DHAN_SHAPE_MAX_STEP),
                None,
                "recovery to rung 0 refused after the cap latched"
            );
            assert_eq!(l.step, DHAN_SHAPE_MAX_STEP, "rung held at 1");
        }
        // The latch fires exactly once: further dirty cycles at the held
        // rung produce NO shift (step pinned), so record_shift is never
        // fed another Degraded — and even a hypothetical one is a no-op.
        for _ in 0..4 {
            assert_eq!(
                l.advance(true, 2, 3, cap.min_step(), DHAN_SHAPE_MAX_STEP),
                None
            );
        }
        assert!(
            !cap.record_shift(StreakShift::Degraded),
            "the latch never fires a second time in the same day"
        );
    }

    #[test]
    fn test_dhan_rung0_reentry_cap_day_start_reset_rearms() {
        // RS1(b) test (ii): the IST day-start reset (the runner assigns
        // fresh `Default`/`starting_at(0)` values, mirrored here) re-arms
        // the cap — the next day oscillates freely below the cap again.
        let mut l = StreakLadder::starting_at(0);
        let mut cap = DhanRung0ReentryCap::default();
        for _ in 0..usize::try_from(CADENCE_DHAN_RUNG0_REENTRY_CAP_PER_DAY).unwrap() {
            oscillate_once(&mut l, &mut cap);
        }
        assert_eq!(
            l.advance(true, 2, 3, cap.min_step(), DHAN_SHAPE_MAX_STEP),
            None
        );
        let demote = l
            .advance(true, 2, 3, cap.min_step(), DHAN_SHAPE_MAX_STEP)
            .expect("the cap-exhausting demotion");
        assert!(cap.record_shift(demote));
        assert_eq!(cap.min_step(), DHAN_SHAPE_MAX_STEP);
        // Day-start reset (the runner's exact reset statements).
        l = StreakLadder::starting_at(0);
        cap = DhanRung0ReentryCap::default();
        assert_eq!(cap.min_step(), 0, "fresh day: floor re-armed to rung 0");
        let (demote_latched, recover_latched) = oscillate_once(&mut l, &mut cap);
        assert!(!demote_latched && !recover_latched);
        assert_eq!(l.step, 0, "fresh day: oscillation admitted again");
    }

    #[test]
    fn test_dhan_shape_ladder_alternating_pattern_never_degrades() {
        // 2026-07-16 transform of the retired F7 amplitude-1 pin: the old
        // anchor ladder shifted on ANY single failing cycle (amplitude-1
        // oscillation under an alternating pattern). The shape ladder's
        // streak thresholds SUBSUME that concern — a perfectly
        // alternating fail/clean minute pattern never reaches 2
        // consecutive dirty cycles, so the shape holds rung 0 forever.
        let mut l = StreakLadder::starting_at(0);
        for _ in 0..8 {
            assert_eq!(l.advance(true, 2, 3, 0, DHAN_SHAPE_MAX_STEP), None);
            assert_eq!(l.step, 0, "one dirty cycle never reshapes");
            assert_eq!(l.advance(false, 2, 3, 0, DHAN_SHAPE_MAX_STEP), None);
            assert_eq!(l.step, 0);
        }
    }

    #[test]
    fn test_spot_concurrency_ladder_degrades_after_2_dirty_recovers_after_3_clean() {
        // The operator's exact rules (defaults 2/3, Assumed): degrade ONE
        // step after 2 CONSECUTIVE dirty cycles; recover one step after 3
        // consecutive fully-clean cycles at the current step.
        let mut l = StreakLadder::starting_at(0);
        // One-off 429 never degrades…
        assert_eq!(l.advance(true, 2, 3, 0, SPOT_CONCURRENCY_MAX_STEP), None);
        assert_eq!(l.step, 0);
        // …a clean cycle resets the dirty streak…
        assert_eq!(l.advance(false, 2, 3, 0, SPOT_CONCURRENCY_MAX_STEP), None);
        assert_eq!(l.advance(true, 2, 3, 0, SPOT_CONCURRENCY_MAX_STEP), None);
        assert_eq!(l.step, 0, "dirty-clean-dirty is never 'continuous'");
        // …2 CONSECUTIVE dirty cycles degrade exactly one step.
        assert_eq!(
            l.advance(true, 2, 3, 0, SPOT_CONCURRENCY_MAX_STEP),
            Some(StreakShift::Degraded)
        );
        assert_eq!(l.step, 1);
        // Walk the full ladder 1 → 2 → 3 (each needing a fresh 2-streak).
        for expected in [2u8, 3] {
            assert_eq!(l.advance(true, 2, 3, 0, SPOT_CONCURRENCY_MAX_STEP), None);
            assert_eq!(
                l.advance(true, 2, 3, 0, SPOT_CONCURRENCY_MAX_STEP),
                Some(StreakShift::Degraded)
            );
            assert_eq!(l.step, expected);
        }
        // Dirty AT the max step holds the step (never past sequential).
        assert_eq!(l.advance(true, 2, 3, 0, SPOT_CONCURRENCY_MAX_STEP), None);
        assert_eq!(l.advance(true, 2, 3, 0, SPOT_CONCURRENCY_MAX_STEP), None);
        assert_eq!(l.step, 3);
        // Recovery: 2 clean cycles are NOT enough; a dirty cycle resets
        // the clean streak; 3 CONSECUTIVE clean cycles climb one step.
        assert_eq!(l.advance(false, 2, 3, 0, SPOT_CONCURRENCY_MAX_STEP), None);
        assert_eq!(l.advance(false, 2, 3, 0, SPOT_CONCURRENCY_MAX_STEP), None);
        assert_eq!(l.advance(true, 2, 3, 0, SPOT_CONCURRENCY_MAX_STEP), None);
        assert_eq!(l.step, 3, "a dirty cycle resets the clean streak");
        for expected in [2u8, 1, 0] {
            assert_eq!(l.advance(false, 2, 3, 0, SPOT_CONCURRENCY_MAX_STEP), None);
            assert_eq!(l.advance(false, 2, 3, 0, SPOT_CONCURRENCY_MAX_STEP), None);
            assert_eq!(
                l.advance(false, 2, 3, 0, SPOT_CONCURRENCY_MAX_STEP),
                Some(StreakShift::Recovered)
            );
            assert_eq!(l.step, expected);
        }
        // Clean at step 0 stays home (never climbs past the floor).
        for _ in 0..5 {
            assert_eq!(l.advance(false, 2, 3, 0, SPOT_CONCURRENCY_MAX_STEP), None);
        }
        assert_eq!(l.step, 0);
    }

    #[test]
    fn test_groww_shape_ladder_all_choice_transitions() {
        // The coordinator-required tier-transition matrix (2026-07-15):
        // choice 1→2, 2→3, 3→2, 2→1 — steps 0→1, 1→2, 2→1, 1→0 — under
        // the shared 2-dirty / 3-clean rules.
        let mut l = StreakLadder::starting_at(0);
        // 1 → 2.
        assert_eq!(l.advance(true, 2, 3, 0, GROWW_SHAPE_MAX_STEP), None);
        assert_eq!(
            l.advance(true, 2, 3, 0, GROWW_SHAPE_MAX_STEP),
            Some(StreakShift::Degraded)
        );
        assert_eq!(l.step, 1);
        // 2 → 3 (the LAST resort — the ladder tops out here).
        assert_eq!(l.advance(true, 2, 3, 0, GROWW_SHAPE_MAX_STEP), None);
        assert_eq!(
            l.advance(true, 2, 3, 0, GROWW_SHAPE_MAX_STEP),
            Some(StreakShift::Degraded)
        );
        assert_eq!(l.step, 2);
        assert_eq!(l.advance(true, 2, 3, 0, GROWW_SHAPE_MAX_STEP), None);
        assert_eq!(l.advance(true, 2, 3, 0, GROWW_SHAPE_MAX_STEP), None);
        assert_eq!(l.step, GROWW_SHAPE_MAX_STEP, "never past choice 3");
        // 3 → 2.
        assert_eq!(l.advance(false, 2, 3, 0, GROWW_SHAPE_MAX_STEP), None);
        assert_eq!(l.advance(false, 2, 3, 0, GROWW_SHAPE_MAX_STEP), None);
        assert_eq!(
            l.advance(false, 2, 3, 0, GROWW_SHAPE_MAX_STEP),
            Some(StreakShift::Recovered)
        );
        assert_eq!(l.step, 1);
        // 2 → 1 (back to the all-7 burst; never stuck from a one-off).
        assert_eq!(l.advance(false, 2, 3, 0, GROWW_SHAPE_MAX_STEP), None);
        assert_eq!(l.advance(false, 2, 3, 0, GROWW_SHAPE_MAX_STEP), None);
        assert_eq!(
            l.advance(false, 2, 3, 0, GROWW_SHAPE_MAX_STEP),
            Some(StreakShift::Recovered)
        );
        assert_eq!(l.step, 0);
    }

    #[test]
    fn test_spot_second_buckets_encodes_rung_and_tier_groupings() {
        // Rung 0 (the operator's ALL-7 primary, 2026-07-16 correction):
        // every spot shares the burst second with the 3 chains.
        assert_eq!(spot_second_buckets(0, 0), [0, 0, 0, 0]);
        // Tier degradation happens WITHIN the burst group, greedy
        // overflow spilling to the next 1000ms buckets.
        assert_eq!(spot_second_buckets(0, 1), [0, 0, 0, 1]);
        assert_eq!(spot_second_buckets(0, 2), [0, 0, 1, 1]);
        // Rung 0 fully sequential (tier 3): one spot per second, greedy
        // spill forward — [T+1, T+2, T+3, T+4] relative buckets.
        assert_eq!(spot_second_buckets(0, 3), [0, 1, 2, 3]);
        // Rung 1 (split fallback): all 4 spots in second 2 at tier 0;
        // tier degradation happens WITHIN the second group, spilling
        // overflow to later 1000ms buckets.
        assert_eq!(spot_second_buckets(1, 0), [1, 1, 1, 1]);
        assert_eq!(spot_second_buckets(1, 1), [1, 1, 1, 2]);
        assert_eq!(spot_second_buckets(1, 2), [1, 1, 2, 2]);
        assert_eq!(spot_second_buckets(1, 3), [1, 2, 3, 4]);
        // Tier steps past the max clamp to fully sequential.
        assert_eq!(spot_second_buckets(0, 9), [0, 1, 2, 3]);
        assert_eq!(spot_second_buckets(9, 9), [1, 2, 3, 4]);
        // Two-bucket budget safety (2026-07-16 correction): the burst
        // second holds ≤4 SPOTS — the Data-API bucket (4 ≤ 5); the 3
        // concurrent chains sit in the option-chain API's OWN
        // per-(underlying, expiry) budget and never count here.
        for shape in 0..=1u8 {
            for step in 0..=SPOT_CONCURRENCY_MAX_STEP {
                let buckets = spot_second_buckets(shape, step);
                let burst_spots = buckets.iter().filter(|b| **b == 0).count();
                assert!(burst_spots <= 4, "shape {shape} step {step}");
                // Assignments are non-decreasing in target order.
                for w in buckets.windows(2) {
                    assert!(w[0] <= w[1]);
                }
            }
        }
        // The structural floor per window cap: no BUCKET's spot count may
        // exceed the cap, at either shape rung.
        assert_eq!(min_spot_step_for_cap(5), 0);
        assert_eq!(min_spot_step_for_cap(4), 0);
        assert_eq!(min_spot_step_for_cap(3), 1);
        assert_eq!(min_spot_step_for_cap(2), 2);
        assert_eq!(min_spot_step_for_cap(1), 3);
        assert_eq!(min_spot_step_for_cap(0), 3, "fail-closed sequential");
        for cap in 1..=5u32 {
            let floor = min_spot_step_for_cap(cap);
            for shape in 0..=DHAN_SHAPE_MAX_STEP {
                for step in floor..=SPOT_CONCURRENCY_MAX_STEP {
                    let buckets = spot_second_buckets(shape, step);
                    for bucket in 0..=4usize {
                        let size = buckets.iter().filter(|b| **b == bucket).count();
                        assert!(
                            size as u32 <= cap,
                            "cap {cap} shape {shape} step {step} bucket {bucket} size {size}"
                        );
                    }
                }
            }
        }
        // A floored ladder starts AND stays at/above its floor.
        let mut l = StreakLadder::starting_at(min_spot_step_for_cap(2));
        assert_eq!(l.step, 2);
        for _ in 0..10 {
            l.advance(
                false,
                2,
                3,
                min_spot_step_for_cap(2),
                SPOT_CONCURRENCY_MAX_STEP,
            );
        }
        assert_eq!(l.step, 2, "recovery never climbs above the cap floor");
    }

    #[test]
    fn test_groww_wave_indices_encode_three_choice_shapes() {
        // Choice 1 (step 0): :00 all-7-parallel.
        assert_eq!(groww_wave_indices(0), (0, 0, 0));
        // Choice 2 (step 1): :01 chains, :02 ALL 4 spots (VIX included).
        assert_eq!(groww_wave_indices(1), (1, 2, 2));
        // Choice 3 (step 2, last resort): :01 chains, :02 core spots,
        // :03 VIX alone (deliberately lowest priority).
        assert_eq!(groww_wave_indices(2), (1, 2, 3));
        // Clamp past the max = choice 3.
        assert_eq!(groww_wave_indices(7), (1, 2, 3));
    }

    #[test]
    fn test_late_retry_eligibility_set_pinned() {
        use tickvault_common::constants::CADENCE_NATIVE_RETRY_MAX_ATTEMPTS;
        let retry_max = 1u32;
        // Malformed: never retried, either leg.
        assert_eq!(
            late_retry_budget(&CadenceFetchError::Malformed, false, retry_max),
            0
        );
        assert_eq!(
            late_retry_budget(&CadenceFetchError::Malformed, true, retry_max),
            0
        );
        // Empty on a SPOT leg: the native ladder cap.
        assert_eq!(
            late_retry_budget(&CadenceFetchError::Empty, false, retry_max),
            CADENCE_NATIVE_RETRY_MAX_ATTEMPTS as u32
        );
        // Empty on the CHAIN leg: legacy budget.
        assert_eq!(
            late_retry_budget(&CadenceFetchError::Empty, true, retry_max),
            1
        );
        // RateLimited: legacy budget both legs (429 is budget-spend, never budget-grow).
        let rl = CadenceFetchError::RateLimited {
            retry_after_ms: None,
        };
        assert_eq!(late_retry_budget(&rl, false, retry_max), retry_max);
        assert_eq!(late_retry_budget(&rl, true, retry_max), retry_max);
        // Remaining classes: legacy budget.
        assert_eq!(
            late_retry_budget(&CadenceFetchError::Timeout, false, retry_max),
            retry_max
        );
        assert_eq!(
            late_retry_budget(&CadenceFetchError::QueueDelay, false, retry_max),
            retry_max
        );
    }
}

//! Pure IST minute-boundary + slot-table calculus (design §1).
//!
//! REIMPLEMENTED here (core cannot depend on `crates/app`, where the
//! proven boundary calculus lives in `spot_1m_rest_boot.rs`); drift is
//! pinned two ways: compile-time const-asserts against the SHARED
//! `SPOT_1M_REST_*` window constants in `tickvault_common::constants`,
//! plus mirrored hand-typed test vectors matching the app module's
//! literals (`test_cadence_schedule_boundary_vectors_mirror_spot_1m_rest`).
//!
//! Everything here is a pure function of (boundary, rung, config) — no
//! clock, no I/O; the runner + the replay proof share it, so the proven
//! schedule and the executed schedule cannot diverge.

use tickvault_common::config::CadenceConfig;
use tickvault_common::constants::{
    SPOT_1M_REST_FIRST_FIRE_SECS_OF_DAY_IST, SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST,
};

use crate::pipeline::chain_snapshot::ChainUnderlying;

/// First cadence cycle boundary of the session: T = 09:16:00 IST (the
/// minute 09:15 closes there). Mirrors the record-capture legs' window.
pub const CADENCE_FIRST_CYCLE_BOUNDARY_SECS_OF_DAY_IST: u32 = 9 * 3600 + 16 * 60;

/// Last cadence cycle boundary of the session: T = 15:30:00 IST (the
/// minute 15:29 closes there; its event decisions are stamped
/// `post_close=true` — kept in dry-run, design §1 "Day edges").
pub const CADENCE_LAST_CYCLE_BOUNDARY_SECS_OF_DAY_IST: u32 = 15 * 3600 + 30 * 60;

// Drift pins: the cadence window IS the record-capture legs' window (the
// same [09:16:00, 15:30:00] inclusive boundary marks) — a change to either
// side fails the build here.
const _: () = assert!(
    CADENCE_FIRST_CYCLE_BOUNDARY_SECS_OF_DAY_IST == SPOT_1M_REST_FIRST_FIRE_SECS_OF_DAY_IST,
    "cadence first boundary must mirror the spot_1m_rest first fire (09:16:00 IST)"
);
const _: () = assert!(
    CADENCE_LAST_CYCLE_BOUNDARY_SECS_OF_DAY_IST == SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST,
    "cadence last boundary must mirror the spot_1m_rest last fire (15:30:00 IST)"
);

/// p95 REST latency allowance used by the in-cycle retry admission test
/// (`ladder::may_retry_in_cycle`): a retry is admitted only when the
/// gate's earliest fire + this allowance lands at/before the lane cutoff.
/// Live evidence: ~200ms Groww RTT, sub-second Dhan chain fetches (the
/// 2026-07-13 probe measured 0.2s); 1500ms is the conservative bound the
/// design also uses for the Groww per-request timeout.
pub const CADENCE_RETRY_LATENCY_ALLOWANCE_MS: i64 = 1_500;

/// Cross-fill freshness floor (design §5): a foreign snapshot is valid
/// for the borrowing lane only when fetched at/after T − this — the
/// window deliberately spans Dhan's PRE-close :55 chain and Groww's
/// POST-close :00 burst.
pub const CADENCE_CROSS_FILL_FRESHNESS_FLOOR_MS: i64 = 5_000;

/// Number of Dhan chain in-cycle retry slots (≤1 per failed underlying,
/// ≤3 total — design §1 retry row).
pub const CADENCE_CHAIN_RETRY_SLOTS: usize = ChainUnderlying::COUNT;

/// Milliseconds per second (readability of the ms-of-day math).
const MS_PER_SEC: i64 = 1_000;

/// Seconds per cadence cycle (one exchange minute).
const SECS_PER_CYCLE: u32 = 60;

/// The next cycle boundary at-or-after `now_secs_of_day` on the IST
/// seconds-of-day domain — the SAME calculus as the app's
/// `next_minute_close_fire` (boundaries are the exact minute marks
/// `[09:16:00, 15:30:00]` INCLUSIVE; `None` once today's window is past).
/// Pure.
#[must_use]
pub fn next_cycle_boundary(now_secs_of_day: u32) -> Option<u32> {
    if now_secs_of_day <= CADENCE_FIRST_CYCLE_BOUNDARY_SECS_OF_DAY_IST {
        return Some(CADENCE_FIRST_CYCLE_BOUNDARY_SECS_OF_DAY_IST);
    }
    let next_boundary = now_secs_of_day
        .div_ceil(SECS_PER_CYCLE)
        .saturating_mul(SECS_PER_CYCLE);
    (next_boundary <= CADENCE_LAST_CYCLE_BOUNDARY_SECS_OF_DAY_IST).then_some(next_boundary)
}

/// NO-MID-CYCLE-JOIN (design §4 case 4 — the restart-safety structural
/// rule): a booting/waking process arms only at a boundary whose EARLIEST
/// pre-fire instant (the Dhan chain anchor at the current rung) is still
/// strictly in the future — it never joins a cycle whose pre-close fires
/// have already begun. Also enforces strictly-after-`last_boundary` so an
/// instant-completing cycle can never re-select its own boundary (the
/// spot_1m_rest H1-fix mirror). Pure.
#[must_use]
pub fn next_joinable_boundary(
    now_ms_of_day: i64,
    last_boundary: Option<u32>,
    rung: u8,
    cfg: &CadenceConfig,
) -> Option<u32> {
    let earliest_prefire_offset_ms = earliest_prefire_offset_ms(rung, cfg);
    // Horizon: strictly after the last completed boundary.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    // APPROVED: ms-of-day / 1000 is within [0, 86_400) — fits u32.
    let mut candidate_from = (now_ms_of_day.max(0) / MS_PER_SEC) as u32;
    if let Some(lb) = last_boundary {
        candidate_from = candidate_from.max(lb.saturating_add(1));
    }
    let mut candidate = next_cycle_boundary(candidate_from)?;
    loop {
        let anchor_ms = i64::from(candidate) * MS_PER_SEC + earliest_prefire_offset_ms;
        if anchor_ms > now_ms_of_day {
            return Some(candidate);
        }
        // This boundary's pre-fires already began — skip to the next.
        candidate = next_cycle_boundary(candidate.saturating_add(1))?;
    }
}

/// The earliest pre-fire offset (ms, negative = pre-close) of a cycle at
/// `rung` — the first Dhan chain slot after the ladder shift. Pure.
#[must_use]
pub fn earliest_prefire_offset_ms(rung: u8, cfg: &CadenceConfig) -> i64 {
    let shift = i64::from(rung).saturating_mul(cfg.dhan_ladder_step_ms);
    cfg.dhan_chain_offsets_ms
        .first()
        .copied()
        .unwrap_or(0)
        .saturating_sub(shift)
}

/// Is `boundary_secs_of_day` inside the session window
/// `[09:16:00, 15:30:00]` INCLUSIVE? Pure.
#[must_use]
pub fn boundary_in_window(boundary_secs_of_day: u32) -> bool {
    (CADENCE_FIRST_CYCLE_BOUNDARY_SECS_OF_DAY_IST..=CADENCE_LAST_CYCLE_BOUNDARY_SECS_OF_DAY_IST)
        .contains(&boundary_secs_of_day)
}

/// One cycle's full slot table (all instants are ABSOLUTE IST
/// milliseconds-of-day TARGETS — the gates are the hard floor).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CycleSlots {
    /// The minute-close boundary T, IST seconds-of-day.
    pub boundary_secs_of_day: u32,
    /// The decided minute's MINUTE-OPEN (T − 60), IST seconds-of-day.
    pub cycle_minute_ist: u32,
    /// T as absolute IST ms-of-day.
    pub boundary_ms: i64,
    /// The ladder rung this table was built at.
    pub rung: u8,
    /// Dhan chain primaries (NIFTY / BANKNIFTY / SENSEX order — one slot
    /// per [`ChainUnderlying`], shifted wholesale by −1000·rung ms; the
    /// pairwise gaps are shift-invariant).
    pub dhan_chain_slots_ms: [i64; ChainUnderlying::COUNT],
    /// Dhan chain in-cycle retry grid: `chain_min_spacing_ms`-stepped
    /// after the rung's LAST primary (rung 0 ⇒ :05/:08/:11; rung 5 ⇒
    /// :00/:03/:06).
    pub dhan_chain_retry_slots_ms: [i64; CADENCE_CHAIN_RETRY_SLOTS],
    /// Dhan spot singles (NIFTY / BANKNIFTY / SENSEX / INDIA VIX order),
    /// post-close-clamped: `max(T + spot_start − 1000·rung, T +
    /// spot_min_post_close)` then `spacing`-stepped.
    pub dhan_spot_slots_ms: [i64; 4],
    /// Groww 7-parallel burst instant (T + anchor offset).
    pub groww_burst_ms: i64,
    /// Groww burst-failure verdict instant (burst + burst timeout).
    pub groww_verdict_ms: i64,
    /// Dhan lane staleness cutoff (absolute).
    pub dhan_cutoff_ms: i64,
    /// Groww lane staleness cutoff (absolute).
    pub groww_cutoff_ms: i64,
    /// TRUE for the session's LAST cycle (T = 15:30:00) — its event
    /// decisions are stamped `post_close=true`.
    pub post_close: bool,
}

/// Build the full slot table for the cycle closing at
/// `boundary_secs_of_day`, at ladder `rung`. Pure — shared by the runner
/// AND the replay proof so the proven and executed schedules cannot
/// diverge.
#[must_use]
pub fn build_cycle_slots(boundary_secs_of_day: u32, rung: u8, cfg: &CadenceConfig) -> CycleSlots {
    let t_ms = i64::from(boundary_secs_of_day) * MS_PER_SEC;
    let shift = i64::from(rung).saturating_mul(cfg.dhan_ladder_step_ms);

    let mut dhan_chain_slots_ms = [0_i64; ChainUnderlying::COUNT];
    for (i, slot) in dhan_chain_slots_ms.iter_mut().enumerate() {
        let offset = cfg.dhan_chain_offsets_ms.get(i).copied().unwrap_or(0);
        *slot = t_ms.saturating_add(offset).saturating_sub(shift);
    }
    let last_primary = dhan_chain_slots_ms[ChainUnderlying::COUNT - 1];
    let mut dhan_chain_retry_slots_ms = [0_i64; CADENCE_CHAIN_RETRY_SLOTS];
    for (j, slot) in dhan_chain_retry_slots_ms.iter_mut().enumerate() {
        // APPROVED: j < 3 — the cast is safe.
        #[allow(clippy::cast_possible_wrap)]
        let step = (j as i64 + 1).saturating_mul(cfg.chain_min_spacing_ms);
        *slot = last_primary.saturating_add(step);
    }

    let spot_base = t_ms
        .saturating_add(cfg.dhan_spot_start_offset_ms)
        .saturating_sub(shift)
        .max(t_ms.saturating_add(cfg.spot_min_post_close_ms));
    let mut dhan_spot_slots_ms = [0_i64; 4];
    for (k, slot) in dhan_spot_slots_ms.iter_mut().enumerate() {
        // APPROVED: k < 4 — the cast is safe.
        #[allow(clippy::cast_possible_wrap)]
        let step = (k as i64).saturating_mul(cfg.dhan_spot_spacing_ms);
        *slot = spot_base.saturating_add(step);
    }

    let groww_burst_ms = t_ms.saturating_add(cfg.groww_anchor_offset_ms);
    CycleSlots {
        boundary_secs_of_day,
        cycle_minute_ist: boundary_secs_of_day.saturating_sub(SECS_PER_CYCLE),
        boundary_ms: t_ms,
        rung,
        dhan_chain_slots_ms,
        dhan_chain_retry_slots_ms,
        dhan_spot_slots_ms,
        groww_burst_ms,
        groww_verdict_ms: groww_burst_ms.saturating_add(cfg.groww_burst_timeout_ms),
        dhan_cutoff_ms: t_ms.saturating_add(cfg.dhan_lane_cutoff_ms),
        groww_cutoff_ms: t_ms.saturating_add(cfg.groww_lane_cutoff_ms),
        post_close: boundary_secs_of_day == CADENCE_LAST_CYCLE_BOUNDARY_SECS_OF_DAY_IST,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> CadenceConfig {
        CadenceConfig::default()
    }

    /// Helper: ms-of-day for HH:MM:SS.mmm literals.
    fn ms(h: i64, m: i64, s: i64, milli: i64) -> i64 {
        ((h * 3600 + m * 60 + s) * 1_000) + milli
    }

    #[test]
    fn test_cadence_schedule_rung0_slots_match_operator_table() {
        // T = 10:00:00 — a mid-session boundary; rung 0 = the operator's
        // nominal table (design §1).
        let slots = build_cycle_slots(10 * 3600, 0, &cfg());
        assert_eq!(slots.cycle_minute_ist, 10 * 3600 - 60);
        // Chains :55.0 / :58.0 / :02.0 (pre-close NIFTY/BANKNIFTY, SENSEX
        // post-close).
        assert_eq!(
            slots.dhan_chain_slots_ms,
            [ms(9, 59, 55, 0), ms(9, 59, 58, 0), ms(10, 0, 2, 0)]
        );
        // Chain retries :05 / :08 / :11 (3s-stepped after the last
        // primary).
        assert_eq!(
            slots.dhan_chain_retry_slots_ms,
            [ms(10, 0, 5, 0), ms(10, 0, 8, 0), ms(10, 0, 11, 0)]
        );
        // Spots :03.0 / :03.4 / :03.8 / :04.2 (the operator's own example
        // instants — 400ms spacing).
        assert_eq!(
            slots.dhan_spot_slots_ms,
            [
                ms(10, 0, 3, 0),
                ms(10, 0, 3, 400),
                ms(10, 0, 3, 800),
                ms(10, 0, 4, 200)
            ]
        );
        // Groww burst at T+0, verdict at T+800.
        assert_eq!(slots.groww_burst_ms, ms(10, 0, 0, 0));
        assert_eq!(slots.groww_verdict_ms, ms(10, 0, 0, 800));
        // Cutoffs :06 / :15.
        assert_eq!(slots.groww_cutoff_ms, ms(10, 0, 6, 0));
        assert_eq!(slots.dhan_cutoff_ms, ms(10, 0, 15, 0));
        assert!(!slots.post_close);
    }

    #[test]
    fn test_cadence_schedule_rung_shift_preserves_chain_gaps() {
        // The wholesale −1000·r shift keeps the pairwise 3.0s/4.0s gaps
        // shift-invariant on EVERY rung, and the retry grid stays 3s-
        // stepped after the rung's last primary (design §1 rung map).
        let c = cfg();
        for rung in 0..=5u8 {
            let slots = build_cycle_slots(10 * 3600, rung, &c);
            let [n, b, s] = slots.dhan_chain_slots_ms;
            assert_eq!(b - n, 3_000, "rung {rung}: NIFTY→BANKNIFTY gap");
            assert_eq!(s - b, 4_000, "rung {rung}: BANKNIFTY→SENSEX gap");
            // The rung-r NIFTY anchor is exactly 1000·r earlier.
            assert_eq!(n, ms(9, 59, 55, 0) - i64::from(rung) * 1_000);
            // Retry grid: last primary + 3/6/9s.
            assert_eq!(
                slots.dhan_chain_retry_slots_ms,
                [s + 3_000, s + 6_000, s + 9_000]
            );
        }
        // Rung 5 spot-check against the design's literal row:
        // chains :50.0/:53.0/:57.0, retries :00/:03/:06.
        let r5 = build_cycle_slots(10 * 3600, 5, &c);
        assert_eq!(
            r5.dhan_chain_slots_ms,
            [ms(9, 59, 50, 0), ms(9, 59, 53, 0), ms(9, 59, 57, 0)]
        );
        assert_eq!(
            r5.dhan_chain_retry_slots_ms,
            [ms(10, 0, 0, 0), ms(10, 0, 3, 0), ms(10, 0, 6, 0)]
        );
    }

    #[test]
    fn test_cadence_schedule_spot_clamp_never_pre_close() {
        let c = cfg();
        // Rungs 0..=2 shift the spot base normally (:03 → :02 → :01)…
        assert_eq!(
            build_cycle_slots(10 * 3600, 1, &c).dhan_spot_slots_ms[0],
            ms(10, 0, 2, 0)
        );
        assert_eq!(
            build_cycle_slots(10 * 3600, 2, &c).dhan_spot_slots_ms[0],
            ms(10, 0, 1, 0)
        );
        // …rungs 3..=5 CLAMP at T+300 (the just-closed candle cannot
        // exist pre-close): :00.3 :00.7 :01.1 :01.5 per the design table.
        for rung in 3..=5u8 {
            let slots = build_cycle_slots(10 * 3600, rung, &c);
            assert_eq!(
                slots.dhan_spot_slots_ms,
                [
                    ms(10, 0, 0, 300),
                    ms(10, 0, 0, 700),
                    ms(10, 0, 1, 100),
                    ms(10, 0, 1, 500)
                ],
                "rung {rung} must clamp at T+300"
            );
            // Never pre-close, on ANY rung.
            for slot in slots.dhan_spot_slots_ms {
                assert!(slot >= slots.boundary_ms + 300);
            }
        }
    }

    #[test]
    fn test_cadence_schedule_boundary_vectors_mirror_spot_1m_rest() {
        // Mirrored hand-typed vectors pinning equality with the app
        // module's literals (crates/app/src/spot_1m_rest_boot.rs
        // `next_minute_close_fire` semantics + the shared window
        // constants 33_360 / 55_800 — also const-asserted above).
        assert_eq!(CADENCE_FIRST_CYCLE_BOUNDARY_SECS_OF_DAY_IST, 33_360); // 09:16:00
        assert_eq!(CADENCE_LAST_CYCLE_BOUNDARY_SECS_OF_DAY_IST, 55_800); // 15:30:00
        // Pre-window (midnight, 09:00, exactly 09:16) → the first fire.
        assert_eq!(next_cycle_boundary(0), Some(33_360));
        assert_eq!(next_cycle_boundary(9 * 3600), Some(33_360));
        assert_eq!(next_cycle_boundary(33_360), Some(33_360));
        // Mid-window: 09:16:01 → 09:17:00; 10:29:30 → 10:30:00;
        // an exact boundary selects itself.
        assert_eq!(next_cycle_boundary(33_361), Some(33_420));
        assert_eq!(next_cycle_boundary(10 * 3600 + 29 * 60 + 30), Some(37_800));
        assert_eq!(next_cycle_boundary(37_800), Some(37_800));
        // The last mark is INCLUSIVE; one second past it → None.
        assert_eq!(next_cycle_boundary(55_800), Some(55_800));
        assert_eq!(next_cycle_boundary(55_801), None);
        assert_eq!(next_cycle_boundary(86_399), None);
    }

    #[test]
    fn test_cadence_schedule_window_09_16_to_15_30_inclusive() {
        assert!(!boundary_in_window(33_300)); // 09:15:00 — before the window
        assert!(boundary_in_window(33_360)); // 09:16:00 — first
        assert!(boundary_in_window(45_000)); // mid-session
        assert!(boundary_in_window(55_800)); // 15:30:00 — last, inclusive
        assert!(!boundary_in_window(55_860)); // 15:31:00 — past
        // The last cycle carries the post_close stamp.
        assert!(build_cycle_slots(55_800, 0, &cfg()).post_close);
        assert!(!build_cycle_slots(55_740, 0, &cfg()).post_close);
    }

    #[test]
    fn test_cadence_schedule_next_joinable_boundary_no_mid_cycle_join() {
        let c = cfg();
        // 09:59:56.0 — the 10:00:00 cycle's :55 pre-fire ALREADY began
        // (rung 0 anchor = 09:59:55) → a booting process must skip to
        // 10:01:00.
        let now = ms_of(9, 59, 56, 0);
        assert_eq!(next_joinable_boundary(now, None, 0, &c), Some(36_060));
        // 09:59:54.0 — the anchor is still in the future → 10:00:00 joins.
        let now = ms_of(9, 59, 54, 0);
        assert_eq!(next_joinable_boundary(now, None, 0, &c), Some(36_000));
        // Rung 5 widens the pre-fire to :50 — 09:59:51 must skip.
        let now = ms_of(9, 59, 51, 0);
        assert_eq!(next_joinable_boundary(now, None, 5, &c), Some(36_060));
        // Strictly-after-last: an instant-completing cycle never
        // re-selects its own boundary.
        let now = ms_of(10, 0, 20, 0);
        assert_eq!(
            next_joinable_boundary(now, Some(36_000), 0, &c),
            Some(36_060)
        );
        // Past the session window → None.
        let now = ms_of(15, 30, 1, 0);
        assert_eq!(next_joinable_boundary(now, Some(55_800), 0, &c), None);
    }

    /// Helper alias for the joinable-boundary vectors.
    fn ms_of(h: i64, m: i64, s: i64, milli: i64) -> i64 {
        ((h * 3600 + m * 60 + s) * 1_000) + milli
    }

    #[test]
    fn test_cadence_schedule_earliest_prefire_offset_ms_tracks_rung() {
        let c = cfg();
        assert_eq!(earliest_prefire_offset_ms(0, &c), -5_000);
        assert_eq!(earliest_prefire_offset_ms(5, &c), -10_000);
    }

    #[test]
    fn test_cadence_schedule_next_cycle_boundary_boundary_in_window_build_cycle_slots_agree() {
        // Consistency sweep: EVERY boundary next_cycle_boundary can emit
        // is inside the inclusive window, and build_cycle_slots stamps it
        // with the matching minute-open + boundary_ms + post_close flag.
        let c = cfg();
        let mut boundary = next_cycle_boundary(0);
        let mut seen = 0_u32;
        while let Some(b) = boundary {
            assert!(boundary_in_window(b), "emitted boundary {b} in window");
            let slots = build_cycle_slots(b, 0, &c);
            assert_eq!(slots.boundary_secs_of_day, b);
            assert_eq!(slots.cycle_minute_ist, b - 60);
            assert_eq!(slots.boundary_ms, i64::from(b) * 1_000);
            assert_eq!(
                slots.post_close,
                b == CADENCE_LAST_CYCLE_BOUNDARY_SECS_OF_DAY_IST
            );
            seen += 1;
            boundary = next_cycle_boundary(b + 1);
        }
        // [09:16:00, 15:30:00] inclusive = 375 minute closes.
        assert_eq!(seen, 375);
    }
}

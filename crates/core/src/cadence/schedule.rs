//! Pure IST minute-boundary + slot-table calculus (design §1; reshaped by
//! the 2026-07-16 operator directive — ALL fires are POST-close now).
//!
//! REIMPLEMENTED here (core cannot depend on `crates/app`, where the
//! proven boundary calculus lives in `spot_1m_rest_boot.rs`); drift is
//! pinned two ways: compile-time const-asserts against the SHARED
//! `SPOT_1M_REST_*` window constants in `tickvault_common::constants`,
//! plus mirrored hand-typed test vectors matching the app module's
//! literals (`test_cadence_schedule_boundary_vectors_mirror_spot_1m_rest`).
//!
//! Everything here is a pure function of (boundary, shape, config) — no
//! clock, no I/O; the runner + the replay proof share it, so the proven
//! schedule and the executed schedule cannot diverge.

use tickvault_common::config::CadenceConfig;
use tickvault_common::constants::{
    CADENCE_GROWW_WAVE_STEP_MS, CADENCE_SPOT_WINDOW_MS, SPOT_1M_REST_FIRST_FIRE_SECS_OF_DAY_IST,
    SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST,
};

use super::ladder::{groww_wave_indices, spot_second_buckets};
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
/// for the borrowing lane only when fetched at/after T − this. Since the
/// 2026-07-16 reshape ALL fires on both lanes are POST-close (Dhan burst
/// second 1–2, Groww waves :00–:03), so every same-cycle completion
/// trivially passes; the floor is a belt against cross-CYCLE staleness
/// (the retired pre-close schedule needed a lender-aware widening —
/// CADENCE-XFILL-RUNG-1, 2026-07-15 — which is retired with it).
pub const CADENCE_CROSS_FILL_FRESHNESS_FLOOR_MS: i64 = 5_000;

/// Number of Dhan chain in-cycle retry slots (≤1 per failed underlying,
/// ≤3 total — design §1 retry row).
pub const CADENCE_CHAIN_RETRY_SLOTS: usize = ChainUnderlying::COUNT;

/// Milliseconds per second (readability of the ms-of-day math).
const MS_PER_SEC: i64 = 1_000;

/// Seconds per cadence cycle (one exchange minute).
const SECS_PER_CYCLE: u32 = 60;

/// R1 (2026-07-15): the mid-minute anchor for IN-SESSION expiry retry
/// waves — seconds-of-minute 30, maximally far from BOTH brokers' burst
/// region (the 2026-07-16 shape packs every fire into T+0..≈T+5s: the
/// Groww :00–:03 waves and the Dhan second-1/second-2 burst + the
/// in-cycle retry grid ≤ the T+15s cutoff). A free-running 60s retry
/// interval was near phase-locked to the minute: an expiry fire landing
/// in the burst window + routine ≥1ms wake jitter put 6 Dhan fires in
/// one rolling second, so a NOMINAL fire deferred — a false
/// `gate_deferred_nominal` should-never page EVERY minute of a vendor
/// outage (the collision class is documented at the gate level by
/// `test_cadence_gate_expiry_invasion_tolerance_and_cap5_backstop` —
/// the 2026-07-16 two-bucket re-derivation of the retired
/// burst-window-defers pin: invasion-tolerant at the default cap 4,
/// colliding again at cap 5, where this :30 anchor stays the fix).
pub const CADENCE_EXPIRY_WAVE_MID_MINUTE_ANCHOR_MS: i64 = 30_000;

/// The next expiry-retry-wave fire instant (ms-of-day) — pure
/// sleep-to-instant math (R1, 2026-07-15). Outside the cycle-burst era
/// (`anchor_mid_minute = false`: pre-market boot-phase waves,
/// non-trading days, post-session) the wave keeps the plain configured
/// cadence (`now + interval` — no cycle bursts exist to collide with).
/// In the burst era it snaps FORWARD to the next
/// [`CADENCE_EXPIRY_WAVE_MID_MINUTE_ANCHOR_MS`] (:30-of-minute) anchor:
/// a configured interval slower than the per-minute anchor grid honors
/// whole-minute head-room first (a 120s interval fires every second
/// anchor), and the returned instant is always STRICTLY after `now`.
/// The L2 expiry gate stays the backstop either way — this fn only
/// moves the waves OUT of the burst window so the backstop never has
/// to defer a nominal fire.
#[must_use]
// TEST-EXEMPT: covered by test_cadence_expiry_wave_anchor_mid_minute_band_never_burst_window (guard name-pattern mismatch).
pub fn next_expiry_wave_instant_ms(
    now_ms_of_day: i64,
    interval_ms: i64,
    anchor_mid_minute: bool,
) -> i64 {
    let interval = interval_ms.max(1);
    if !anchor_mid_minute {
        return now_ms_of_day.saturating_add(interval);
    }
    const MINUTE_MS: i64 = 60 * MS_PER_SEC;
    // Whole-minute head beyond the per-minute anchor cadence: slower
    // configured intervals SKIP anchors, they never fire between them.
    let head = (interval - MINUTE_MS).max(0);
    let earliest = now_ms_of_day.saturating_add(head);
    // The first :30 anchor STRICTLY after `earliest`.
    let k = (earliest - CADENCE_EXPIRY_WAVE_MID_MINUTE_ANCHOR_MS).div_euclid(MINUTE_MS) + 1;
    k.saturating_mul(MINUTE_MS)
        .saturating_add(CADENCE_EXPIRY_WAVE_MID_MINUTE_ANCHOR_MS)
}

/// Should the sleep between expiry-resolution retry waves anchor at the
/// mid-minute instant? True on a trading day, before session end (the
/// last cycle boundary), whenever the PLAIN `now + interval` target
/// would reach the era look-ahead window (one minute before the first
/// cycle boundary). That single condition subsumes the pre-R3-F1 "now
/// within one minute of the first boundary" look-ahead (`interval` ≥
/// 1ms means the target is strictly after `now`) AND clamps the
/// transitional wave (R3-F1 belt (b), 2026-07-15): validation bounds
/// `expiry_retry_interval_ms` ≤ 60s (belt (a)), but even under future
/// validation drift the LAST pre-era wake of a >60s interval must not
/// sleep the plain interval straight into the first session cycle's
/// burst window (65s @ 09:14:58 → a plain wake at 09:16:03 = inside the
/// first burst's fire region → one false `gate_deferred_nominal` page at
/// session entry). Boot-phase / non-trading / post-session wakes keep
/// the plain configured cadence — no cycle bursts exist to collide
/// with. Pure.
#[must_use]
pub fn expiry_wave_anchor_active(now_ms_of_day: i64, interval_ms: i64, trading_day: bool) -> bool {
    if !trading_day {
        return false;
    }
    const MINUTE_MS: i64 = 60 * MS_PER_SEC;
    let last_ms = i64::from(CADENCE_LAST_CYCLE_BOUNDARY_SECS_OF_DAY_IST) * MS_PER_SEC;
    if now_ms_of_day >= last_ms {
        return false;
    }
    let era_lookahead_ms =
        i64::from(CADENCE_FIRST_CYCLE_BOUNDARY_SECS_OF_DAY_IST) * MS_PER_SEC - MINUTE_MS;
    now_ms_of_day.saturating_add(interval_ms.max(1)) >= era_lookahead_ms
}

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
/// fire instant (all fires are POST-close since 2026-07-16 — the smaller
/// of the Groww burst anchor and the Dhan burst second) is still strictly
/// in the future — it never joins a cycle whose fires have already begun.
/// Also enforces strictly-after-`last_boundary` so an instant-completing
/// cycle can never re-select its own boundary (the spot_1m_rest H1-fix
/// mirror). Pure.
#[must_use]
pub fn next_joinable_boundary(
    now_ms_of_day: i64,
    last_boundary: Option<u32>,
    cfg: &CadenceConfig,
) -> Option<u32> {
    let earliest_offset_ms = cfg.groww_anchor_offset_ms.min(cfg.dhan_burst_offset_ms);
    // Horizon: strictly after the last completed boundary.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    // APPROVED: ms-of-day / 1000 is within [0, 86_400) — fits u32.
    let mut candidate_from = (now_ms_of_day.max(0) / MS_PER_SEC) as u32;
    if let Some(lb) = last_boundary {
        candidate_from = candidate_from.max(lb.saturating_add(1));
    }
    let mut candidate = next_cycle_boundary(candidate_from)?;
    loop {
        let anchor_ms = i64::from(candidate) * MS_PER_SEC + earliest_offset_ms;
        if anchor_ms > now_ms_of_day {
            return Some(candidate);
        }
        // This boundary's fires already began — skip to the next.
        candidate = next_cycle_boundary(candidate.saturating_add(1))?;
    }
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
    /// The Dhan SHAPE rung this table was built at (2026-07-16: 0 = the
    /// ALL-7 concurrent primary; 1 = the split fallback).
    pub dhan_shape: u8,
    /// Dhan chain slots (NIFTY / BANKNIFTY / SENSEX order) — ALL THREE at
    /// the burst second (T + `dhan_burst_offset_ms`): the 2026-07-16
    /// directive makes different underlyings explicitly CONCURRENT (the
    /// broker's 3s rule is per-(underlying, expiry) only).
    pub dhan_chain_slots_ms: [i64; ChainUnderlying::COUNT],
    /// Dhan chain in-cycle retry grid: ALL slots at burst +
    /// `chain_min_spacing_ms` (T+4s nominal) — each failed underlying's
    /// retry is a DIFFERENT (underlying, expiry) key, so the retries are
    /// concurrent too; the per-key gate admits each at exactly +3s.
    pub dhan_chain_retry_slots_ms: [i64; CADENCE_CHAIN_RETRY_SLOTS],
    /// The Dhan spot-concurrency ladder step this table was built at
    /// (2026-07-15: 0 = 4-per-second … 3 = fully sequential).
    pub spot_step: u8,
    /// Dhan spot slots (NIFTY / BANKNIFTY / SENSEX / INDIA VIX order),
    /// per the SECOND-BUCKET assignment `spot_second_buckets(dhan_shape,
    /// spot_step)`: shape 0 bases ALL 4 spots in the burst second beside
    /// the 3 chains (base `[0, 0, 0, 0]` — the operator's "all 7 parallel
    /// at first second"; supersedes the interim 5+2 base wording,
    /// 2026-07-16 all-7 correction); shape 1 bases all 4 in second 2; the
    /// tier step caps per-second spot counts, greedy overflow spilling to
    /// later 1000ms buckets. Base clamped ≥ T + `spot_min_post_close_ms`
    /// (the just-closed candle cannot exist pre-close).
    pub dhan_spot_slots_ms: [i64; 4],
    /// The Groww fallback-shape ladder step this table was built at
    /// (2026-07-15 three-choice ladder: 0 = `:00` all-7 burst;
    /// 1 = `:01` chains / `:02` all 4 spots; 2 = `:01` / `:02` core
    /// spots / `:03` VIX alone).
    pub groww_shape: u8,
    /// Groww chain-wave instant (all 3 chains in parallel).
    pub groww_chain_wave_ms: i64,
    /// Groww core-spot-wave instant (NIFTY/BANKNIFTY/SENSEX spots).
    pub groww_spot_wave_ms: i64,
    /// Groww VIX-spot-wave instant (equals the spot wave except at
    /// choice 3, where VIX fires alone last — context-only priority).
    pub groww_vix_wave_ms: i64,
    /// Groww wave-failure verdict instant (LAST wave + burst timeout).
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
/// `boundary_secs_of_day`, at Dhan shape rung `dhan_shape`,
/// spot-concurrency `spot_step` and Groww fallback-shape `groww_shape`
/// (2026-07-16 shape). Pure — shared by the runner AND the replay proof
/// so the proven and executed schedules cannot diverge.
#[must_use]
pub fn build_cycle_slots(
    boundary_secs_of_day: u32,
    dhan_shape: u8,
    spot_step: u8,
    groww_shape: u8,
    cfg: &CadenceConfig,
) -> CycleSlots {
    let t_ms = i64::from(boundary_secs_of_day) * MS_PER_SEC;

    // The Dhan burst second: 3 chains, ALL concurrent (2026-07-16 — the
    // 3s rule is per-(underlying, expiry); different underlyings fire
    // together).
    let burst_ms = t_ms.saturating_add(cfg.dhan_burst_offset_ms);
    let dhan_chain_slots_ms = [burst_ms; ChainUnderlying::COUNT];
    // Retries: each failed underlying re-fires at burst + spacing — the
    // per-(underlying, expiry) gate admits each at exactly +3s
    // (inclusive boundary), and the combined window holds ≤4 fires there
    // (3 chain retries + ≤1 spot-grid fire) — never past the cap.
    let retry_ms = burst_ms.saturating_add(cfg.chain_min_spacing_ms);
    let dhan_chain_retry_slots_ms = [retry_ms; CADENCE_CHAIN_RETRY_SLOTS];

    // Spot slots: second buckets from (shape rung × tier step), the base
    // clamped post-close (structurally inert at the default burst
    // offset — validate() pins burst ≥ spot_min_post_close).
    let spot_base = burst_ms.max(t_ms.saturating_add(cfg.spot_min_post_close_ms));
    let buckets = spot_second_buckets(dhan_shape, spot_step);
    let mut dhan_spot_slots_ms = [0_i64; 4];
    for (k, slot) in dhan_spot_slots_ms.iter_mut().enumerate() {
        // APPROVED: bucket index ≤ 4 — the cast is safe.
        #[allow(clippy::cast_possible_wrap)]
        let bucket_offset = (buckets[k] as i64).saturating_mul(CADENCE_SPOT_WINDOW_MS);
        *slot = spot_base.saturating_add(bucket_offset);
    }

    let groww_anchor_ms = t_ms.saturating_add(cfg.groww_anchor_offset_ms);
    let (chain_wave, spot_wave, vix_wave) = groww_wave_indices(groww_shape);
    let wave_ms = |wave: usize| -> i64 {
        // APPROVED: wave indices are 0..=3 — the cast is safe.
        #[allow(clippy::cast_possible_wrap)]
        groww_anchor_ms.saturating_add((wave as i64).saturating_mul(CADENCE_GROWW_WAVE_STEP_MS))
    };
    let groww_chain_wave_ms = wave_ms(chain_wave);
    let groww_spot_wave_ms = wave_ms(spot_wave);
    let groww_vix_wave_ms = wave_ms(vix_wave);
    let last_wave_ms = groww_chain_wave_ms
        .max(groww_spot_wave_ms)
        .max(groww_vix_wave_ms);
    CycleSlots {
        boundary_secs_of_day,
        cycle_minute_ist: boundary_secs_of_day.saturating_sub(SECS_PER_CYCLE),
        boundary_ms: t_ms,
        dhan_shape,
        dhan_chain_slots_ms,
        dhan_chain_retry_slots_ms,
        spot_step,
        dhan_spot_slots_ms,
        groww_shape,
        groww_chain_wave_ms,
        groww_spot_wave_ms,
        groww_vix_wave_ms,
        groww_verdict_ms: last_wave_ms.saturating_add(cfg.groww_burst_timeout_ms),
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
    fn test_cadence_expiry_wave_anchor_mid_minute_band_never_burst_window() {
        // R1 (2026-07-15): every IN-SESSION expiry retry wave lands at
        // seconds-of-minute 30 EXACTLY — therefore NEVER inside the
        // burst region (2026-07-16 shape: Groww waves :00–:03, Dhan
        // burst seconds 1–2, retry grid ≤ the :15 cutoff) — for
        // arbitrary wake phases and configured intervals. The exact
        // ==30 pin IS the band assertion (any band check after it would
        // be dead code — R3 nit, 2026-07-15).
        for interval in [1_i64, 5_000, 60_000, 90_000, 120_000, 300_000] {
            let mut now = ms(9, 16, 0, 0);
            while now < ms(9, 26, 0, 0) {
                let at = next_expiry_wave_instant_ms(now, interval, true);
                assert!(at > now, "the anchor is always strictly after now");
                let secs_of_minute = (at % 60_000) / 1_000;
                assert_eq!(
                    secs_of_minute, 30,
                    "in-session waves fire at :30 exactly (interval {interval}, now {now})"
                );
                // A slower-than-per-minute interval is honored to within
                // one anchor-grid minute (waves skip anchors, never fire
                // between them).
                assert!(at - now >= (interval - 60_000).max(0));
                now += 777; // sweep every wake phase across the minute
            }
        }
        // Two consecutive in-session waves at the default 60s interval
        // are exactly one anchor (one minute) apart.
        let first = next_expiry_wave_instant_ms(ms(10, 0, 30, 0), 60_000, true);
        assert_eq!(first, ms(10, 1, 30, 0));
        assert_eq!(
            next_expiry_wave_instant_ms(first, 60_000, true),
            ms(10, 2, 30, 0)
        );
        // Boot-phase / non-trading waves keep the plain configured
        // cadence — no cycle bursts exist to collide with.
        assert_eq!(next_expiry_wave_instant_ms(1_000, 60_000, false), 61_000);
        assert_eq!(next_expiry_wave_instant_ms(1_000, 0, false), 1_001);
    }

    /// R3-F1 belt (b), 2026-07-15: the transitional-wave clamp. A
    /// config-legal-under-drift interval > 60s used to escape the
    /// runner's fixed one-minute look-ahead: the LAST pre-era wake
    /// (≤ 09:14:59) slept the PLAIN interval and landed ONE
    /// transitional wave inside the FIRST session cycle's burst window
    /// (65s @ 09:14:58 → 09:16:03 = inside the first burst's fire
    /// region → one false `gate_deferred_nominal` page at session
    /// entry). The predicate now keys on where the PLAIN target would
    /// LAND, so the first in-era wake snaps to a :30 anchor instead.
    #[test]
    fn test_expiry_wave_anchor_active_clamps_first_in_era_wake_to_anchor() {
        let now = ms(9, 14, 58, 0);
        let interval = 65_000_i64;
        // The bug shape: the plain target lands inside the first session
        // cycle's burst window (boundary 09:16:00 + fires within ~T+5s).
        assert_eq!(now + interval, ms(9, 16, 3, 0));
        // The clamp turns the anchor ON for this wake…
        assert!(expiry_wave_anchor_active(now, interval, true));
        // …and the anchored instant is a :30 anchor, honoring the
        // whole-minute head-room of the >60s interval, OUTSIDE any
        // burst window.
        assert_eq!(
            next_expiry_wave_instant_ms(now, interval, true),
            ms(9, 15, 30, 0)
        );
        // The pre-existing one-minute look-ahead is preserved (subsumed
        // by the plain-target condition).
        assert!(expiry_wave_anchor_active(ms(9, 15, 30, 0), 60_000, true));
        // Far pre-era wakes keep the plain cadence (target far short of
        // the era look-ahead window).
        assert!(!expiry_wave_anchor_active(ms(8, 0, 0, 0), 60_000, true));
        // Non-trading days never anchor.
        assert!(!expiry_wave_anchor_active(now, interval, false));
        // Session-over wakes keep the plain cadence.
        assert!(!expiry_wave_anchor_active(ms(15, 30, 0, 0), 60_000, true));
        // Boundary edge: the era look-ahead opens exactly one minute
        // before the first cycle boundary (plain target at 09:15:00).
        assert!(expiry_wave_anchor_active(ms(9, 14, 0, 0), 60_000, true));
        assert!(!expiry_wave_anchor_active(ms(9, 13, 59, 999), 60_000, true));
    }

    #[test]
    fn test_cadence_schedule_rung0_slots_match_operator_table() {
        // T = 10:00:00 — a mid-session boundary; shape 0 / step 0 /
        // groww 0 = the operator's 2026-07-16 primary table (same-day
        // correction: "all 7 parallel at first second"): second 1 =
        // 3 chains + ALL 4 spots concurrent — the spots sit in the
        // Data-API bucket (4 ≤ 5), the chains in the option-chain API's
        // own per-(underlying, expiry) budget (two-bucket model).
        let slots = build_cycle_slots(10 * 3600, 0, 0, 0, &cfg());
        assert_eq!(slots.cycle_minute_ist, 10 * 3600 - 60);
        assert_eq!(slots.dhan_shape, 0);
        // Chains: ALL THREE concurrent at the burst second T+1.0
        // (different underlyings are explicitly concurrent — the 3s rule
        // is per-(underlying, expiry) only).
        assert_eq!(slots.dhan_chain_slots_ms, [ms(10, 0, 1, 0); 3]);
        // Chain retries: all at burst + 3s = T+4.0 (per-key gates admit
        // each at exactly +3s; chains never touch the Data-API ring).
        assert_eq!(slots.dhan_chain_retry_slots_ms, [ms(10, 0, 4, 0); 3]);
        // Spots: ALL FOUR share the burst second with the 3 chains —
        // the operator's all-7 primary.
        assert_eq!(slots.spot_step, 0);
        assert_eq!(slots.dhan_spot_slots_ms, [ms(10, 0, 1, 0); 4]);
        // Groww shape 0 (choice 1): all waves at T+0, verdict at T+800.
        assert_eq!(slots.groww_shape, 0);
        assert_eq!(slots.groww_chain_wave_ms, ms(10, 0, 0, 0));
        assert_eq!(slots.groww_spot_wave_ms, ms(10, 0, 0, 0));
        assert_eq!(slots.groww_vix_wave_ms, ms(10, 0, 0, 0));
        assert_eq!(slots.groww_verdict_ms, ms(10, 0, 0, 800));
        // Cutoffs :06 / :15.
        assert_eq!(slots.groww_cutoff_ms, ms(10, 0, 6, 0));
        assert_eq!(slots.dhan_cutoff_ms, ms(10, 0, 15, 0));
        assert!(!slots.post_close);
    }

    #[test]
    fn test_cadence_schedule_rung1_split_fallback_slots() {
        // Shape rung 1 (the operator's 2026-07-16 fallback): second 1 =
        // 3 chains only; second 2 = ALL 4 spots.
        let slots = build_cycle_slots(10 * 3600, 1, 0, 0, &cfg());
        assert_eq!(slots.dhan_shape, 1);
        assert_eq!(slots.dhan_chain_slots_ms, [ms(10, 0, 1, 0); 3]);
        assert_eq!(slots.dhan_chain_retry_slots_ms, [ms(10, 0, 4, 0); 3]);
        assert_eq!(slots.dhan_spot_slots_ms, [ms(10, 0, 2, 0); 4]);
        // The chain slots are IDENTICAL across rungs — the shape ladder
        // reshapes only the SPOT packing (chains are per-key-gated, not
        // rescheduled).
        let s0 = build_cycle_slots(10 * 3600, 0, 0, 0, &cfg());
        assert_eq!(s0.dhan_chain_slots_ms, slots.dhan_chain_slots_ms);
        assert_eq!(
            s0.dhan_chain_retry_slots_ms,
            slots.dhan_chain_retry_slots_ms
        );
    }

    #[test]
    fn test_cadence_schedule_spot_concurrency_groupings_per_step() {
        // The 2026-07-15 spot-concurrency tiers compose with the
        // 2026-07-16 shape rungs via the per-second-group bucket fill.
        let c = cfg();
        // Shape 0: tier degradation happens WITHIN the burst group,
        // greedy overflow spilling to the next 1000ms buckets; tier 3
        // spills to singles T+1/2/3/4.
        let s1 = build_cycle_slots(10 * 3600, 0, 1, 0, &c);
        assert_eq!(
            s1.dhan_spot_slots_ms,
            [
                ms(10, 0, 1, 0),
                ms(10, 0, 1, 0),
                ms(10, 0, 1, 0),
                ms(10, 0, 2, 0)
            ]
        );
        let s2 = build_cycle_slots(10 * 3600, 0, 2, 0, &c);
        assert_eq!(
            s2.dhan_spot_slots_ms,
            [
                ms(10, 0, 1, 0),
                ms(10, 0, 1, 0),
                ms(10, 0, 2, 0),
                ms(10, 0, 2, 0)
            ]
        );
        let s3 = build_cycle_slots(10 * 3600, 0, 3, 0, &c);
        assert_eq!(
            s3.dhan_spot_slots_ms,
            [
                ms(10, 0, 1, 0),
                ms(10, 0, 2, 0),
                ms(10, 0, 3, 0),
                ms(10, 0, 4, 0)
            ]
        );
        // Shape 1: tier degradation happens WITHIN second 2, spilling
        // overflow to later seconds (per-second-group tier math).
        let r1s1 = build_cycle_slots(10 * 3600, 1, 1, 0, &c);
        assert_eq!(
            r1s1.dhan_spot_slots_ms,
            [
                ms(10, 0, 2, 0),
                ms(10, 0, 2, 0),
                ms(10, 0, 2, 0),
                ms(10, 0, 3, 0)
            ]
        );
        let r1s2 = build_cycle_slots(10 * 3600, 1, 2, 0, &c);
        assert_eq!(
            r1s2.dhan_spot_slots_ms,
            [
                ms(10, 0, 2, 0),
                ms(10, 0, 2, 0),
                ms(10, 0, 3, 0),
                ms(10, 0, 3, 0)
            ]
        );
        let r1s3 = build_cycle_slots(10 * 3600, 1, 3, 0, &c);
        assert_eq!(
            r1s3.dhan_spot_slots_ms,
            [
                ms(10, 0, 2, 0),
                ms(10, 0, 3, 0),
                ms(10, 0, 4, 0),
                ms(10, 0, 5, 0)
            ]
        );
        // Even the LAST single of the deepest shape × tier + a full
        // retry round (window-stepped appends) sits inside the :15 Dhan
        // cutoff.
        assert!(r1s3.dhan_spot_slots_ms[3] + 4 * 1_000 <= r1s3.dhan_cutoff_ms);
    }

    #[test]
    fn test_cadence_schedule_groww_three_choice_wave_instants_no_overlap() {
        // The coordinator-relayed three-choice ladder (2026-07-15).
        let c = cfg();
        // Choice 2 (shape 1): :01 chains, :02 ALL 4 spots (VIX included);
        // verdict = last wave + 800.
        let s1 = build_cycle_slots(10 * 3600, 0, 0, 1, &c);
        assert_eq!(s1.groww_shape, 1);
        assert_eq!(s1.groww_chain_wave_ms, ms(10, 0, 1, 0));
        assert_eq!(s1.groww_spot_wave_ms, ms(10, 0, 2, 0));
        assert_eq!(s1.groww_vix_wave_ms, ms(10, 0, 2, 0));
        assert_eq!(s1.groww_verdict_ms, ms(10, 0, 2, 800));
        // Choice 3 (shape 2, LAST resort): :01 chains, :02 core spots,
        // :03 VIX alone.
        let s2 = build_cycle_slots(10 * 3600, 0, 0, 2, &c);
        assert_eq!(s2.groww_chain_wave_ms, ms(10, 0, 1, 0));
        assert_eq!(s2.groww_spot_wave_ms, ms(10, 0, 2, 0));
        assert_eq!(s2.groww_vix_wave_ms, ms(10, 0, 3, 0));
        assert_eq!(s2.groww_verdict_ms, ms(10, 0, 3, 800));
        // NO-OVERLAP-INTO-NEXT-BURST: for EVERY shape, the last wave, the
        // verdict, the cutoff AND the worst-case fully-sequential 7-leg
        // fallback tail end strictly before the NEXT minute's burst
        // anchor (the config validate() pins the same bound at boot).
        let next_burst_ms = i64::from(10 * 3600 + 60) * 1_000 + c.groww_anchor_offset_ms;
        for shape in 0..=2u8 {
            let s = build_cycle_slots(10 * 3600, 0, 0, shape, &c);
            let worst_tail = s.groww_verdict_ms + 7 * c.groww_request_timeout_ms;
            assert!(s.groww_vix_wave_ms < next_burst_ms, "shape {shape} wave");
            assert!(
                s.groww_verdict_ms < s.groww_cutoff_ms,
                "shape {shape} verdict"
            );
            assert!(s.groww_cutoff_ms < next_burst_ms, "shape {shape} cutoff");
            assert!(worst_tail < next_burst_ms, "shape {shape} fallback tail");
        }
    }

    #[test]
    fn test_cadence_schedule_burst_packing_never_exceeds_broker_cap() {
        // 2026-07-16 cap reconciliation (two-bucket model, same-day
        // all-7 correction): at EVERY (shape × tier) permutation, no
        // single second carries more than 4 SPOT fires — the Data-API
        // bucket (4 ≤ the broker's 5/sec cap, with one slot of expiry
        // headroom). The 3 concurrent chains sit in the option-chain
        // API's OWN per-(underlying, expiry) budget and never count
        // against the Data-API bucket.
        let c = cfg();
        for shape in 0..=1u8 {
            for step in 0..=3u8 {
                let s = build_cycle_slots(10 * 3600, shape, step, 0, &c);
                let burst = s.dhan_chain_slots_ms[0];
                for probe in 0..=5i64 {
                    let second = burst + probe * 1_000;
                    let spots = s
                        .dhan_spot_slots_ms
                        .iter()
                        .filter(|m| **m == second)
                        .count();
                    assert!(
                        spots <= 4,
                        "shape {shape} step {step} second {second}: {spots} spots"
                    );
                }
                // Every chain fires concurrently at the burst instant.
                assert_eq!(s.dhan_chain_slots_ms, [burst; 3]);
                // All fires are POST-close — no pre-fire exists anymore.
                for slot in s
                    .dhan_chain_slots_ms
                    .iter()
                    .chain(s.dhan_spot_slots_ms.iter())
                {
                    assert!(*slot > s.boundary_ms, "post-close only");
                }
            }
        }
    }

    #[test]
    fn test_cadence_schedule_spot_clamp_never_pre_close() {
        // The post-close clamp is structural at the default config
        // (burst 1000 ≥ min_post_close 300) but must hold under raw
        // config drift: a burst offset BELOW the spot floor clamps the
        // spot base (chains keep the raw burst — they are live
        // snapshots, not just-closed candles).
        let mut c = cfg();
        c.dhan_burst_offset_ms = 100;
        let s = build_cycle_slots(10 * 3600, 0, 0, 0, &c);
        assert_eq!(s.dhan_chain_slots_ms, [ms(10, 0, 0, 100); 3]);
        assert_eq!(s.dhan_spot_slots_ms, [ms(10, 0, 0, 300); 4]);
        // Never pre-close, on ANY shape and ANY concurrency step.
        for shape in 0..=1u8 {
            for step in 0..=3u8 {
                for slot in build_cycle_slots(10 * 3600, shape, step, 0, &c).dhan_spot_slots_ms {
                    assert!(slot >= s.boundary_ms + 300);
                }
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
        assert!(build_cycle_slots(55_800, 0, 0, 0, &cfg()).post_close);
        assert!(!build_cycle_slots(55_740, 0, 0, 0, &cfg()).post_close);
    }

    #[test]
    fn test_cadence_schedule_next_joinable_boundary_no_mid_cycle_join() {
        let c = cfg();
        // All fires are POST-close (2026-07-16): the earliest fire of
        // the 10:00:00 cycle is the Groww burst at T+0 exactly — a boot
        // AT 10:00:00.000 must skip to 10:01:00…
        let now = ms_of(10, 0, 0, 0);
        assert_eq!(next_joinable_boundary(now, None, &c), Some(36_060));
        // …while a boot 1ms earlier still joins 10:00:00.
        let now = ms_of(9, 59, 59, 999);
        assert_eq!(next_joinable_boundary(now, None, &c), Some(36_000));
        // No pre-close fire exists anymore: even :56 of the previous
        // minute joins the imminent boundary (the retired :55 pre-fire
        // would have forced a skip here).
        let now = ms_of(9, 59, 56, 0);
        assert_eq!(next_joinable_boundary(now, None, &c), Some(36_000));
        // Strictly-after-last: an instant-completing cycle never
        // re-selects its own boundary.
        let now = ms_of(10, 0, 20, 0);
        assert_eq!(next_joinable_boundary(now, Some(36_000), &c), Some(36_060));
        // Past the session window → None.
        let now = ms_of(15, 30, 1, 0);
        assert_eq!(next_joinable_boundary(now, Some(55_800), &c), None);
    }

    /// Helper alias for the joinable-boundary vectors.
    fn ms_of(h: i64, m: i64, s: i64, milli: i64) -> i64 {
        ((h * 3600 + m * 60 + s) * 1_000) + milli
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
            let slots = build_cycle_slots(b, 0, 0, 0, &c);
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

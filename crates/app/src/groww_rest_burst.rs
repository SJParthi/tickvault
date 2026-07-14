//! Groww REST burst auto-ladder — the SHARED tier/demotion state + wave
//! schedule for the per-minute Groww REST legs (operator approval
//! 2026-07-14, relayed via the coordinator session: *"approved and go
//! ahead with the recommendation"*; contract:
//! `no-rest-except-live-feed-2026-06-27.md` §9.7 +
//! `groww-second-feed-scope-2026-06-19.md` §38.9; stage strings:
//! `rest-1m-pipeline-error-codes.md`).
//!
//! ## The ladder (one shared state, three consumer legs)
//! - Configured tier (`[groww_rest_burst] tier`): `two_wave` (SHIPPED
//!   DEFAULT — 3 chain requests at close+300 ms, 4 spot at close+1,350 ms;
//!   the > 1 s wave separation keeps every rolling second single-wave) or
//!   `seven_concurrent` (the operator preference, PROBE-GATED — all 7 at
//!   close+300 ms).
//! - AUTO-DEMOTE (a LADDER since the 2026-07-14 fix round — review
//!   MEDIUM-1): any Groww-leg HTTP 429 steps one session-scoped
//!   `AtomicU8` demotion level DOWN the shape ladder
//!   `seven_concurrent → two_wave → two_wave_staggered →
//!   two_wave_sequential` (each wave request slot of the SEQUENTIAL
//!   shape starts a full per-slot budget after the previous — the
//!   pre-Stage-1 one-at-a-time pacing shape). Every level increment is
//!   its own edge (returns `true` exactly once per level) so the caller
//!   emits ONE coded warn per level (SPOT1M-01 / CHAIN-02
//!   `stage="burst_demoted"` — the leg that saw the 429 owns the
//!   error-code routing) and the shared counter
//!   `tv_groww_rest_burst_demoted_total{level}` increments here. A 429
//!   at the ladder floor is a no-op (no further shape exists). Boot
//!   resets to the configured tier (process state).
//! - WARM-UP: at minute boundary − [`GROWW_REST_WARMUP_LEAD_SECS`] each
//!   enabled leg sends ONE lightweight UNAUTHENTICATED GET on its own
//!   long-lived client (response discarded — the 401/400 reply still
//!   re-establishes an idle-closed TLS/H2 connection), bounded by
//!   [`GROWW_REST_WARMUP_TIMEOUT_SECS`] so a fire can never start late.
//!   Deliberately NO Authorization header and NO token-cache touch: an
//!   SSM read failure seconds before the fire would burn the ≥ 60 s
//!   token re-read pacing floor and turn the fire into a no_token miss.
//!
//! Cold path only — one atomic load per fire; zero hot-path involvement.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use tracing::debug;

use tickvault_common::config::GrowwRestBurstTier;
use tickvault_common::constants::{
    GROWW_REST_BURST_INTRA_WAVE_STAGGER_MS, GROWW_REST_WARMUP_TIMEOUT_SECS,
    GROWW_SPOT_1M_FIRE_DELAY_MS, GROWW_SPOT_1M_TWO_WAVE_FIRE_DELAY_MS,
};

/// The EFFECTIVE burst shape a fire runs under — the configured tier
/// folded with the session demotion flag (pure mapping:
/// [`effective_burst_tier`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectiveBurstTier {
    /// All 7 requests at close+300 ms (probe-gated configured tier, not
    /// demoted).
    SevenConcurrent,
    /// Chain wave at close+300 ms, spot wave at close+1,350 ms.
    TwoWave,
    /// two_wave with a per-slot 350 ms intra-wave stagger (the first
    /// demoted two_wave shape).
    TwoWaveStaggered,
    /// two_wave fired FULLY SEQUENTIALLY within each wave: slot k starts
    /// a whole per-slot budget after slot k−1, so requests can never
    /// overlap (the pre-Stage-1 one-at-a-time pacing shape — the ladder
    /// FLOOR, review MEDIUM-1 2026-07-14).
    TwoWaveSequential,
}

impl EffectiveBurstTier {
    /// Static metric-label value (`tv_groww_rest_burst_tier_total{tier}`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SevenConcurrent => "seven_concurrent",
            Self::TwoWave => "two_wave",
            Self::TwoWaveStaggered => "two_wave_staggered",
            Self::TwoWaveSequential => "two_wave_sequential",
        }
    }
}

/// The ordered demotion ladder — each live 429 steps the session one
/// shape DOWN this list (never up).
const BURST_SHAPE_LADDER: [EffectiveBurstTier; 4] = [
    EffectiveBurstTier::SevenConcurrent,
    EffectiveBurstTier::TwoWave,
    EffectiveBurstTier::TwoWaveStaggered,
    EffectiveBurstTier::TwoWaveSequential,
];

/// A configured tier's START index on [`BURST_SHAPE_LADDER`]. Pure.
const fn ladder_start_index(configured: GrowwRestBurstTier) -> usize {
    match configured {
        GrowwRestBurstTier::SevenConcurrent => 0,
        GrowwRestBurstTier::TwoWave => 1,
    }
}

/// Fold (configured tier, session demotion level) → the effective burst
/// shape: index `start + level` on the ladder, saturating at the
/// sequential FLOOR (review MEDIUM-1 2026-07-14 — the pre-fix single
/// `AtomicBool` stopped at the staggered shape; a 429 while already
/// staggered now drops to fully-sequential-within-wave). Pure.
#[must_use]
pub fn effective_burst_tier(
    configured: GrowwRestBurstTier,
    demotion_level: u8,
) -> EffectiveBurstTier {
    let idx = ladder_start_index(configured).saturating_add(demotion_level as usize);
    BURST_SHAPE_LADDER[idx.min(BURST_SHAPE_LADDER.len() - 1)]
}

/// The SPOT wave's post-boundary fire delay (ms) under an effective tier:
/// `seven_concurrent` fires with the chain wave at close+300 ms; both
/// two_wave shapes fire at close+1,350 ms. (The CHAIN wave is always
/// [`GROWW_CHAIN_1M_FIRE_DELAY_MS`].) Pure.
#[must_use]
pub fn spot_wave_fire_delay_ms(tier: EffectiveBurstTier) -> u64 {
    match tier {
        EffectiveBurstTier::SevenConcurrent => GROWW_SPOT_1M_FIRE_DELAY_MS,
        EffectiveBurstTier::TwoWave
        | EffectiveBurstTier::TwoWaveStaggered
        | EffectiveBurstTier::TwoWaveSequential => GROWW_SPOT_1M_TWO_WAVE_FIRE_DELAY_MS,
    }
}

/// Per-request intra-wave stagger (ms) for request slot `slot` of a wave:
/// 0 under the concurrent shapes; slot k × 350 ms under the STAGGERED
/// demotion shape (the first-attempt shift propagates into every re-poll
/// rung wave, since rung offsets are measured from each target's own
/// first attempt); slot k × `sequential_slot_span_ms` under the
/// SEQUENTIAL floor shape (the caller passes its leg's whole per-slot
/// hard budget, so slots can never overlap — the ladder/request is
/// budget-bounded inside the span; review MEDIUM-1 2026-07-14). Pure.
#[must_use]
pub fn intra_wave_stagger_ms(
    tier: EffectiveBurstTier,
    slot: usize,
    sequential_slot_span_ms: u64,
) -> u64 {
    match tier {
        EffectiveBurstTier::TwoWaveStaggered => {
            (slot as u64).saturating_mul(GROWW_REST_BURST_INTRA_WAVE_STAGGER_MS)
        }
        EffectiveBurstTier::TwoWaveSequential => {
            (slot as u64).saturating_mul(sequential_slot_span_ms)
        }
        EffectiveBurstTier::SevenConcurrent | EffectiveBurstTier::TwoWave => 0,
    }
}

/// Remaining sleep (ms) from `now_ms_of_day` to the instant
/// `fire_secs_of_day × 1000 + delay_ms` — MILLISECOND-precise wake math
/// (CRITICAL-1 fix 2026-07-14): the pre-fix else-branch summed a
/// whole-second boundary gap with the wave delay, so the actual wake was
/// `fire + frac(now) + delay` and the two legs' independent fractional
/// offsets could collapse the load-bearing 1,050 ms wave separation to
/// ~51 ms (one rolling second holding 11 req/s SOLO). Computing the
/// target from the ms clock keeps the separation EXACT regardless of
/// warm-up config and after overruns. Negative `delay_ms` targets a
/// pre-boundary instant (the warm-up lead). Pure.
#[must_use]
pub fn wave_sleep_from_now_ms(fire_secs_of_day: u32, delay_ms: i64, now_ms_of_day: i64) -> u64 {
    let target_ms = i64::from(fire_secs_of_day)
        .saturating_mul(1_000)
        .saturating_add(delay_ms);
    u64::try_from(target_ms.saturating_sub(now_ms_of_day)).unwrap_or(0)
}

/// The shared session burst state: the configured tier (boot value) + the
/// session-scoped demotion LEVEL + the warm-up toggle. One `Arc` is shared
/// by the spot, chain and contract legs (main.rs creates it once).
#[derive(Debug)]
pub struct GrowwRestBurstState {
    configured: GrowwRestBurstTier,
    demotion_level: AtomicU8,
    warm_up: bool,
}

impl GrowwRestBurstState {
    /// Build the shared state from the boot config. A restart therefore
    /// RESETS any session demotion to the configured tier (§9.7).
    #[must_use]
    pub fn new(configured: GrowwRestBurstTier, warm_up: bool) -> Arc<Self> {
        Arc::new(Self {
            configured,
            demotion_level: AtomicU8::new(0),
            warm_up,
        })
    }

    /// The effective burst shape for the NEXT fire.
    #[must_use]
    pub fn effective_tier(&self) -> EffectiveBurstTier {
        effective_burst_tier(self.configured, self.demotion_level.load(Ordering::Relaxed))
    }

    /// Record a live HTTP 429 on any Groww REST leg. Steps the session
    /// ONE shape down the demotion ladder (review MEDIUM-1 2026-07-14:
    /// a 429 while already staggered drops to fully-sequential). Returns
    /// `true` on each LEVEL EDGE (exactly once per level) so the calling
    /// leg emits its ONE coded warn per level (`stage="burst_demoted"`);
    /// the shared counter increments here on the same edge with the NEW
    /// level as a static label. A 429 at the sequential floor is a
    /// no-op (`false` — no further shape exists; the leg's own 429
    /// counter still records it).
    pub fn note_rate_limited(&self) -> bool {
        let start = ladder_start_index(self.configured);
        let floor = BURST_SHAPE_LADDER.len() - 1;
        let stepped =
            self.demotion_level
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |level| {
                    // Step only while the effective shape can still change.
                    // The +1 cannot overflow: level is bounded by the
                    // 4-entry ladder (< 3 whenever the guard passes).
                    ((start + level as usize) < floor).then_some(level + 1)
                });
        if let Ok(previous) = stepped {
            // The CAS's own previous value names THIS edge's new level —
            // race-safe against a concurrent further step.
            let level_label = match previous + 1 {
                1 => "1",
                2 => "2",
                _ => "3",
            };
            metrics::counter!("tv_groww_rest_burst_demoted_total", "level" => level_label)
                .increment(1);
        }
        stepped.is_ok()
    }

    /// Is the pre-boundary warm-up enabled?
    #[must_use]
    pub fn warm_up_enabled(&self) -> bool {
        self.warm_up
    }
}

/// Send ONE unauthenticated warm-up GET on `client` (response discarded)
/// — best-effort TLS/H2 re-establishment before the critical window.
/// Bounded by [`GROWW_REST_WARMUP_TIMEOUT_SECS`] regardless of the
/// client's own timeout; NEVER coded/paged (warm-up failure is expected
/// under network trouble and the fire's own ladder is the loud path).
/// Counted per leg: `tv_groww_rest_warmup_total{leg, outcome}`.
pub async fn send_warmup_get(
    client: &reqwest::Client,
    url: &str,
    query: &[(&'static str, String)],
    leg: &'static str,
) {
    let request = client.get(url).query(query).send();
    let outcome =
        match tokio::time::timeout(Duration::from_secs(GROWW_REST_WARMUP_TIMEOUT_SECS), request)
            .await
        {
            Ok(Ok(resp)) => {
                // Any status (401/400 expected — no Authorization header by
                // design) means the connection is warm; the body is dropped.
                debug!(
                    leg,
                    status = resp.status().as_u16(),
                    "groww_rest_burst: warm-up GET completed (connection warmed)"
                );
                "sent"
            }
            Ok(Err(err)) => {
                // Redaction-exempt (review LOW 2026-07-14): no
                // Authorization header is sent and no body is read — a
                // reqwest transport error here can only carry the public
                // static endpoint URL, never a credential.
                debug!(
                    leg,
                    ?err,
                    "groww_rest_burst: warm-up GET transport error (best-effort)"
                );
                "error"
            }
            Err(_elapsed) => {
                debug!(leg, "groww_rest_burst: warm-up GET timed out (best-effort)");
                "timeout"
            }
        };
    metrics::counter!("tv_groww_rest_warmup_total", "leg" => leg, "outcome" => outcome)
        .increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The (configured, demotion level) → effective mapping walks the
    /// §9.7 ladder: seven → two_wave → staggered → sequential; two_wave
    /// starts one rung down; every level past the floor saturates.
    #[test]
    fn test_effective_tier_mapping() {
        assert_eq!(
            effective_burst_tier(GrowwRestBurstTier::SevenConcurrent, 0),
            EffectiveBurstTier::SevenConcurrent
        );
        assert_eq!(
            effective_burst_tier(GrowwRestBurstTier::SevenConcurrent, 1),
            EffectiveBurstTier::TwoWave
        );
        assert_eq!(
            effective_burst_tier(GrowwRestBurstTier::SevenConcurrent, 2),
            EffectiveBurstTier::TwoWaveStaggered
        );
        assert_eq!(
            effective_burst_tier(GrowwRestBurstTier::SevenConcurrent, 3),
            EffectiveBurstTier::TwoWaveSequential
        );
        assert_eq!(
            effective_burst_tier(GrowwRestBurstTier::TwoWave, 0),
            EffectiveBurstTier::TwoWave
        );
        assert_eq!(
            effective_burst_tier(GrowwRestBurstTier::TwoWave, 1),
            EffectiveBurstTier::TwoWaveStaggered
        );
        assert_eq!(
            effective_burst_tier(GrowwRestBurstTier::TwoWave, 2),
            EffectiveBurstTier::TwoWaveSequential
        );
        // Saturation past the floor (both configured tiers).
        assert_eq!(
            effective_burst_tier(GrowwRestBurstTier::TwoWave, u8::MAX),
            EffectiveBurstTier::TwoWaveSequential
        );
        assert_eq!(
            effective_burst_tier(GrowwRestBurstTier::SevenConcurrent, u8::MAX),
            EffectiveBurstTier::TwoWaveSequential
        );
    }

    /// The spot wave delay per effective tier: 300 ms under seven, the
    /// 1,350 ms two_wave delay under both two_wave shapes.
    #[test]
    fn test_spot_fire_delay_per_tier() {
        assert_eq!(
            spot_wave_fire_delay_ms(EffectiveBurstTier::SevenConcurrent),
            GROWW_SPOT_1M_FIRE_DELAY_MS
        );
        assert_eq!(
            spot_wave_fire_delay_ms(EffectiveBurstTier::TwoWave),
            GROWW_SPOT_1M_TWO_WAVE_FIRE_DELAY_MS
        );
        assert_eq!(
            spot_wave_fire_delay_ms(EffectiveBurstTier::TwoWaveStaggered),
            GROWW_SPOT_1M_TWO_WAVE_FIRE_DELAY_MS
        );
        assert_eq!(
            spot_wave_fire_delay_ms(EffectiveBurstTier::TwoWaveSequential),
            GROWW_SPOT_1M_TWO_WAVE_FIRE_DELAY_MS
        );
        // The chain wave anchor referenced by the docs stays 300 ms.
        assert_eq!(
            tickvault_common::constants::GROWW_CHAIN_1M_FIRE_DELAY_MS,
            300
        );
    }

    /// Stagger slots: zero under the concurrent shapes; 350 ms steps
    /// under the staggered shape; whole-slot-span steps under the
    /// sequential floor.
    #[test]
    fn test_intra_wave_stagger_slots() {
        const SPAN: u64 = 11_000;
        for slot in 0..4 {
            assert_eq!(
                intra_wave_stagger_ms(EffectiveBurstTier::SevenConcurrent, slot, SPAN),
                0
            );
            assert_eq!(
                intra_wave_stagger_ms(EffectiveBurstTier::TwoWave, slot, SPAN),
                0
            );
            assert_eq!(
                intra_wave_stagger_ms(EffectiveBurstTier::TwoWaveStaggered, slot, SPAN),
                slot as u64 * GROWW_REST_BURST_INTRA_WAVE_STAGGER_MS
            );
            assert_eq!(
                intra_wave_stagger_ms(EffectiveBurstTier::TwoWaveSequential, slot, SPAN),
                slot as u64 * SPAN
            );
        }
    }

    /// The demotion LADDER: each 429 edge steps one shape down (once per
    /// level), the floor is a no-op, and a fresh state (a restart) is
    /// undemoted.
    #[test]
    fn test_demote_edge_fires_once() {
        let state = GrowwRestBurstState::new(GrowwRestBurstTier::SevenConcurrent, false);
        assert_eq!(state.effective_tier(), EffectiveBurstTier::SevenConcurrent);
        assert!(state.note_rate_limited(), "first 429 is a demotion edge");
        assert_eq!(state.effective_tier(), EffectiveBurstTier::TwoWave);
        assert!(state.note_rate_limited(), "second 429 steps to staggered");
        assert_eq!(state.effective_tier(), EffectiveBurstTier::TwoWaveStaggered);
        assert!(state.note_rate_limited(), "third 429 steps to sequential");
        assert_eq!(
            state.effective_tier(),
            EffectiveBurstTier::TwoWaveSequential
        );
        assert!(
            !state.note_rate_limited(),
            "a 429 at the sequential floor is not an edge"
        );
        assert_eq!(
            state.effective_tier(),
            EffectiveBurstTier::TwoWaveSequential
        );

        let two = GrowwRestBurstState::new(GrowwRestBurstTier::TwoWave, true);
        assert!(two.warm_up_enabled());
        assert!(two.note_rate_limited());
        assert_eq!(two.effective_tier(), EffectiveBurstTier::TwoWaveStaggered);
        assert!(
            two.note_rate_limited(),
            "already-staggered 429 → sequential"
        );
        assert_eq!(two.effective_tier(), EffectiveBurstTier::TwoWaveSequential);
        assert!(!two.note_rate_limited());

        // A fresh state (restart semantics) resets to the configured tier.
        let fresh = GrowwRestBurstState::new(GrowwRestBurstTier::SevenConcurrent, false);
        assert_eq!(fresh.effective_tier(), EffectiveBurstTier::SevenConcurrent);
        assert!(!fresh.warm_up_enabled());
    }

    /// Metric labels are the four static strings.
    #[test]
    fn test_tier_labels_are_static() {
        assert_eq!(
            EffectiveBurstTier::SevenConcurrent.as_str(),
            "seven_concurrent"
        );
        assert_eq!(EffectiveBurstTier::TwoWave.as_str(), "two_wave");
        assert_eq!(
            EffectiveBurstTier::TwoWaveStaggered.as_str(),
            "two_wave_staggered"
        );
        assert_eq!(
            EffectiveBurstTier::TwoWaveSequential.as_str(),
            "two_wave_sequential"
        );
    }

    /// CRITICAL-1 (2026-07-14): the wave wake instant is computed from a
    /// MILLISECOND clock — `fire × 1000 + delay − now_ms`, floored at 0 —
    /// so the actual wake carries NO whole-second fractional drift and
    /// the 1,050 ms two_wave separation holds exactly.
    #[test]
    fn test_wave_sleep_from_now_ms_is_millisecond_precise() {
        // 09:16:00 fire, spot two_wave delay, now = 09:15:59.432 →
        // target 33_360_000 + 1_350; sleep = 1_918 ms exactly.
        assert_eq!(wave_sleep_from_now_ms(33_360, 1_350, 33_359_432), 1_918);
        // The chain leg's wake off the SAME now differs by exactly the
        // 1,050 ms delay difference — the separation is now-independent.
        assert_eq!(wave_sleep_from_now_ms(33_360, 300, 33_359_432), 868);
        for now_ms in [33_355_000_i64, 33_359_001, 33_359_999] {
            let spot = wave_sleep_from_now_ms(33_360, 1_350, now_ms);
            let chain = wave_sleep_from_now_ms(33_360, 300, now_ms);
            assert_eq!(spot - chain, 1_050);
        }
        // Already past the target → 0 (fire immediately), never underflow.
        assert_eq!(wave_sleep_from_now_ms(33_360, 300, 33_361_000), 0);
        // Negative delay targets the pre-boundary warm-up instant.
        assert_eq!(wave_sleep_from_now_ms(33_360, -4_000, 33_355_500), 500);
        assert_eq!(wave_sleep_from_now_ms(33_360, -4_000, 33_356_500), 0);
    }
}

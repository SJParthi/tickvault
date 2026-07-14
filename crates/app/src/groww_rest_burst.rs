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
//! - AUTO-DEMOTE: any Groww-leg HTTP 429 flips one session-scoped
//!   `AtomicBool` — `seven_concurrent` demotes to plain `two_wave`;
//!   `two_wave` demotes to STAGGERED two_wave (each wave request slot
//!   starts `slot × GROWW_REST_BURST_INTRA_WAVE_STAGGER_MS` into the
//!   wave). The demotion edge returns `true` exactly once so the caller
//!   emits ONE coded warn (SPOT1M-01 / CHAIN-02 `stage="burst_demoted"` —
//!   the leg that saw the 429 owns the error-code routing) and the shared
//!   counter `tv_groww_rest_burst_demoted_total` increments here. Boot
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
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// two_wave with a per-slot 350 ms intra-wave stagger (the demoted
    /// two_wave shape).
    TwoWaveStaggered,
}

impl EffectiveBurstTier {
    /// Static metric-label value (`tv_groww_rest_burst_tier_total{tier}`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SevenConcurrent => "seven_concurrent",
            Self::TwoWave => "two_wave",
            Self::TwoWaveStaggered => "two_wave_staggered",
        }
    }
}

/// Fold (configured tier, demoted?) → the effective burst shape.
/// `seven_concurrent` demotes to plain `two_wave`; `two_wave` demotes to
/// the staggered shape (one session-scoped demotion level by design —
/// the §9.7 single-`AtomicBool` contract). Pure.
#[must_use]
pub fn effective_burst_tier(configured: GrowwRestBurstTier, demoted: bool) -> EffectiveBurstTier {
    match (configured, demoted) {
        (GrowwRestBurstTier::SevenConcurrent, false) => EffectiveBurstTier::SevenConcurrent,
        (GrowwRestBurstTier::SevenConcurrent, true) | (GrowwRestBurstTier::TwoWave, false) => {
            EffectiveBurstTier::TwoWave
        }
        (GrowwRestBurstTier::TwoWave, true) => EffectiveBurstTier::TwoWaveStaggered,
    }
}

/// The SPOT wave's post-boundary fire delay (ms) under an effective tier:
/// `seven_concurrent` fires with the chain wave at close+300 ms; both
/// two_wave shapes fire at close+1,350 ms. (The CHAIN wave is always
/// [`GROWW_CHAIN_1M_FIRE_DELAY_MS`].) Pure.
#[must_use]
pub fn spot_wave_fire_delay_ms(tier: EffectiveBurstTier) -> u64 {
    match tier {
        EffectiveBurstTier::SevenConcurrent => GROWW_SPOT_1M_FIRE_DELAY_MS,
        EffectiveBurstTier::TwoWave | EffectiveBurstTier::TwoWaveStaggered => {
            GROWW_SPOT_1M_TWO_WAVE_FIRE_DELAY_MS
        }
    }
}

/// Per-request intra-wave stagger (ms) for request slot `slot` of a wave:
/// 0 everywhere except the DEMOTED two_wave shape, where slot k starts
/// k × 350 ms into its wave (the ladder rung offsets are measured from
/// each target's own first attempt, so the stagger propagates into every
/// re-poll rung wave too). Pure.
#[must_use]
pub fn intra_wave_stagger_ms(tier: EffectiveBurstTier, slot: usize) -> u64 {
    match tier {
        EffectiveBurstTier::TwoWaveStaggered => {
            (slot as u64).saturating_mul(GROWW_REST_BURST_INTRA_WAVE_STAGGER_MS)
        }
        EffectiveBurstTier::SevenConcurrent | EffectiveBurstTier::TwoWave => 0,
    }
}

/// The shared session burst state: the configured tier (boot value) + the
/// session-scoped demotion flag + the warm-up toggle. One `Arc` is shared
/// by the spot, chain and contract legs (main.rs creates it once).
#[derive(Debug)]
pub struct GrowwRestBurstState {
    configured: GrowwRestBurstTier,
    demoted: AtomicBool,
    warm_up: bool,
}

impl GrowwRestBurstState {
    /// Build the shared state from the boot config. A restart therefore
    /// RESETS any session demotion to the configured tier (§9.7).
    #[must_use]
    pub fn new(configured: GrowwRestBurstTier, warm_up: bool) -> Arc<Self> {
        Arc::new(Self {
            configured,
            demoted: AtomicBool::new(false),
            warm_up,
        })
    }

    /// The effective burst shape for the NEXT fire.
    #[must_use]
    pub fn effective_tier(&self) -> EffectiveBurstTier {
        effective_burst_tier(self.configured, self.demoted.load(Ordering::Relaxed))
    }

    /// Is the pre-boundary warm-up enabled?
    #[must_use]
    pub fn warm_up_enabled(&self) -> bool {
        self.warm_up
    }

    /// Record a live HTTP 429 on any Groww REST leg. Returns `true` on
    /// the DEMOTION EDGE (exactly once per session) so the calling leg
    /// emits its ONE coded warn (`stage="burst_demoted"`); the shared
    /// counter increments here on the same edge.
    pub fn note_rate_limited(&self) -> bool {
        let newly_demoted = !self.demoted.swap(true, Ordering::Relaxed);
        if newly_demoted {
            metrics::counter!("tv_groww_rest_burst_demoted_total").increment(1);
        }
        newly_demoted
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

    /// The (configured, demoted) → effective mapping is total and matches
    /// the §9.7 contract: seven demotes to plain two_wave; two_wave
    /// demotes to the staggered shape.
    #[test]
    fn test_effective_tier_mapping() {
        assert_eq!(
            effective_burst_tier(GrowwRestBurstTier::SevenConcurrent, false),
            EffectiveBurstTier::SevenConcurrent
        );
        assert_eq!(
            effective_burst_tier(GrowwRestBurstTier::SevenConcurrent, true),
            EffectiveBurstTier::TwoWave
        );
        assert_eq!(
            effective_burst_tier(GrowwRestBurstTier::TwoWave, false),
            EffectiveBurstTier::TwoWave
        );
        assert_eq!(
            effective_burst_tier(GrowwRestBurstTier::TwoWave, true),
            EffectiveBurstTier::TwoWaveStaggered
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
        // The chain wave anchor referenced by the docs stays 300 ms.
        assert_eq!(
            tickvault_common::constants::GROWW_CHAIN_1M_FIRE_DELAY_MS,
            300
        );
    }

    /// Stagger slots: zero except under the demoted two_wave shape.
    #[test]
    fn test_intra_wave_stagger_slots() {
        for slot in 0..4 {
            assert_eq!(
                intra_wave_stagger_ms(EffectiveBurstTier::SevenConcurrent, slot),
                0
            );
            assert_eq!(intra_wave_stagger_ms(EffectiveBurstTier::TwoWave, slot), 0);
            assert_eq!(
                intra_wave_stagger_ms(EffectiveBurstTier::TwoWaveStaggered, slot),
                slot as u64 * GROWW_REST_BURST_INTRA_WAVE_STAGGER_MS
            );
        }
    }

    /// The demotion edge fires exactly once per session; the state then
    /// reports the demoted shape; a fresh state (a restart) is undemoted.
    #[test]
    fn test_demote_edge_fires_once() {
        let state = GrowwRestBurstState::new(GrowwRestBurstTier::SevenConcurrent, false);
        assert_eq!(state.effective_tier(), EffectiveBurstTier::SevenConcurrent);
        assert!(state.note_rate_limited(), "first 429 is the demotion edge");
        assert!(!state.note_rate_limited(), "second 429 is not an edge");
        assert_eq!(state.effective_tier(), EffectiveBurstTier::TwoWave);

        let two = GrowwRestBurstState::new(GrowwRestBurstTier::TwoWave, true);
        assert!(two.warm_up_enabled());
        assert!(two.note_rate_limited());
        assert_eq!(two.effective_tier(), EffectiveBurstTier::TwoWaveStaggered);

        // A fresh state (restart semantics) resets to the configured tier.
        let fresh = GrowwRestBurstState::new(GrowwRestBurstTier::SevenConcurrent, false);
        assert_eq!(fresh.effective_tier(), EffectiveBurstTier::SevenConcurrent);
        assert!(!fresh.warm_up_enabled());
    }

    /// Metric labels are the three static strings.
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
    }
}

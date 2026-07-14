//! Pure CAS minimum-spacing gates — the STRUCTURAL zero-429 hard floor.
//!
//! Every Dhan cadence fire (primary, retry, any ladder rung) must pass its
//! gate BEFORE dispatch. Gates live in the MONOTONIC millisecond domain
//! (injected by the caller — `tokio::time::Instant`-derived in production,
//! a `SimClock` in the replay proof), so a wall-clock step/regression can
//! never compress spacing: the wall clock only picks TARGET instants, the
//! monotonic gates are the floor (judge ruling, design §0 "Gate time
//! domain").
//!
//! The Groww lane consults NO gates BY CONSTRUCTION — `schedule`'s Groww
//! slots carry no gate parameter and the runner's Groww arms never touch
//! [`DhanGates`] (compile-time gate-free, design §4).

use std::sync::atomic::{AtomicI64, Ordering};

use crate::pipeline::chain_snapshot::ChainUnderlying;

/// Verdict of a gate acquisition attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateVerdict {
    /// The fire is authorized; the gate advanced to `now + spacing`.
    Acquired,
    /// The fire is NOT authorized yet — retry at/after the carried
    /// monotonic instant (a DEFERRAL, never a violation: the caller
    /// re-schedules, it never fires ungated).
    RetryAtMs(i64),
}

/// A minimum-spacing fire-authorization gate: at most one `Acquired` per
/// `spacing_ms` window, enforced by a CAS loop over one `AtomicI64`
/// (`next_allowed_ms`, monotonic domain). Panic-free saturating
/// arithmetic; zero allocation; O(1) per acquire (bounded CAS retries only
/// under contention, and the cadence runner is single-tasked per gate).
#[derive(Debug)]
pub struct MinSpacingGate {
    /// The earliest monotonic instant (ms) the NEXT fire may be
    /// authorized. `i64::MIN` = never fired (first acquire always passes).
    next_allowed_ms: AtomicI64,
    /// The enforced minimum spacing between authorizations, ms.
    spacing_ms: i64,
}

impl MinSpacingGate {
    /// A fresh gate: the first acquire always passes.
    #[must_use]
    pub fn new(spacing_ms: i64) -> Self {
        Self {
            next_allowed_ms: AtomicI64::new(i64::MIN),
            spacing_ms,
        }
    }

    /// The enforced spacing, ms.
    // TEST-EXEMPT: trivial field accessor consumed by the replay-proof assertions.
    #[must_use]
    pub fn spacing_ms(&self) -> i64 {
        self.spacing_ms
    }

    /// Attempt to authorize a fire at monotonic instant
    /// `now_monotonic_ms`. Exactly one caller wins each spacing window;
    /// a losing/early caller receives the deferral instant.
    #[must_use]
    pub fn try_acquire(&self, now_monotonic_ms: i64) -> GateVerdict {
        loop {
            let cur = self.next_allowed_ms.load(Ordering::Acquire);
            if now_monotonic_ms < cur {
                return GateVerdict::RetryAtMs(cur);
            }
            let next = now_monotonic_ms.saturating_add(self.spacing_ms);
            if self
                .next_allowed_ms
                .compare_exchange(cur, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return GateVerdict::Acquired;
            }
        }
    }

    /// Conservative boot re-seed (design §0 "Restart safety",
    /// belt-and-braces beside the structural no-mid-cycle-join rule): a
    /// booting process seeds `next_allowed = now + spacing`, wasting at
    /// most one slot — never risking a violation against fires the OLD
    /// process may have issued just before dying (the monotonic domain
    /// does not survive a restart, so the fresh gate cannot know them).
    pub fn reseed(&self, now_monotonic_ms: i64) {
        self.next_allowed_ms.store(
            now_monotonic_ms.saturating_add(self.spacing_ms),
            Ordering::Release,
        );
    }
}

/// The Dhan lane's gate set (design §4): one gate per chain underlying +
/// one GLOBAL chain gate (the strictest interpretation of Dhan's
/// 1-unique-request-per-3s option-chain rule) + the spot singles gate.
#[derive(Debug)]
pub struct DhanGates {
    /// Per-underlying chain gates, indexed by [`ChainUnderlying::index`].
    chain_per_underlying: [MinSpacingGate; ChainUnderlying::COUNT],
    /// The global chain gate — EVERY chain fire (any underlying, primary
    /// or retry) must also pass this.
    chain_global: MinSpacingGate,
    /// The Dhan spot singles gate (default 400ms = 2.5 rps; the shared
    /// `dhan_data_api_limiter` at the executor seam remains the SECOND
    /// live floor — both must pass).
    spot: MinSpacingGate,
}

impl DhanGates {
    /// Build the gate set from the validated `[cadence]` spacings.
    #[must_use]
    pub fn new(chain_spacing_ms: i64, spot_spacing_ms: i64) -> Self {
        Self {
            chain_per_underlying: std::array::from_fn(|_| MinSpacingGate::new(chain_spacing_ms)),
            chain_global: MinSpacingGate::new(chain_spacing_ms),
            spot: MinSpacingGate::new(spot_spacing_ms),
        }
    }

    /// Authorize a chain fire for `underlying`: per-underlying gate FIRST,
    /// then the GLOBAL gate. The schedule serializes chains ≥3s apart, so
    /// the global acquire cannot fail after the per-underlying acquire
    /// succeeded on nominal slots (asserted by the replay proof); if it
    /// ever does, the per-underlying slot was conservatively consumed and
    /// the fire DEFERS — a deferral, never a violation (design §4). The
    /// carried retry instant is the MAX of both constraints (the consumed
    /// per-underlying slot now binds at `now + spacing`), so the caller's
    /// next wake cannot arrive before it can actually pass.
    #[must_use]
    pub fn try_acquire_chain(
        &self,
        underlying: ChainUnderlying,
        now_monotonic_ms: i64,
    ) -> GateVerdict {
        match self.chain_per_underlying[underlying.index()].try_acquire(now_monotonic_ms) {
            GateVerdict::Acquired => {
                match self.chain_global.try_acquire(now_monotonic_ms) {
                    GateVerdict::Acquired => GateVerdict::Acquired,
                    GateVerdict::RetryAtMs(global_at) => {
                        // The per-underlying slot was conservatively
                        // consumed above: it now binds at now + spacing.
                        let per_ul_at = now_monotonic_ms.saturating_add(
                            self.chain_per_underlying[underlying.index()].spacing_ms(),
                        );
                        GateVerdict::RetryAtMs(global_at.max(per_ul_at))
                    }
                }
            }
            defer @ GateVerdict::RetryAtMs(_) => defer,
        }
    }

    /// Authorize a Dhan spot fire.
    #[must_use]
    pub fn try_acquire_spot(&self, now_monotonic_ms: i64) -> GateVerdict {
        self.spot.try_acquire(now_monotonic_ms)
    }

    /// Conservative boot re-seed of EVERY gate (see
    /// [`MinSpacingGate::reseed`]).
    pub fn reseed_all(&self, now_monotonic_ms: i64) {
        for g in &self.chain_per_underlying {
            g.reseed(now_monotonic_ms);
        }
        self.chain_global.reseed(now_monotonic_ms);
        self.spot.reseed(now_monotonic_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cadence_gate_min_spacing_acquire_and_defer() {
        let gate = MinSpacingGate::new(3_000);
        // First acquire always passes.
        assert_eq!(gate.try_acquire(10_000), GateVerdict::Acquired);
        // Inside the window → deferral carrying the exact retry instant.
        assert_eq!(gate.try_acquire(12_999), GateVerdict::RetryAtMs(13_000));
        // Exactly at the boundary → authorized (spacing is inclusive).
        assert_eq!(gate.try_acquire(13_000), GateVerdict::Acquired);
        // The window advanced from the AUTHORIZED instant, not the target.
        assert_eq!(gate.try_acquire(15_500), GateVerdict::RetryAtMs(16_000));
        assert_eq!(gate.spacing_ms(), 3_000);
    }

    #[test]
    fn test_cadence_gate_monotonic_immune_to_wall_regression() {
        // The gate NEVER sees a wall clock — callers feed it monotonic ms.
        // A wall-clock regression re-picks TARGETS but the monotonic input
        // can only move forward; feeding an (impossible) earlier monotonic
        // instant is still refused, proving spacing cannot be compressed
        // by any clock step.
        let gate = MinSpacingGate::new(3_000);
        assert_eq!(gate.try_acquire(50_000), GateVerdict::Acquired);
        // Simulated "wall regressed 40s" mapped to an earlier instant:
        // still inside the monotonic window → deferred, never acquired.
        assert_eq!(gate.try_acquire(10_000), GateVerdict::RetryAtMs(53_000));
        assert_eq!(gate.try_acquire(52_999), GateVerdict::RetryAtMs(53_000));
        assert_eq!(gate.try_acquire(53_000), GateVerdict::Acquired);
    }

    #[test]
    fn test_cadence_gate_boot_reseed_conservative() {
        let gates = DhanGates::new(3_000, 400);
        gates.reseed_all(100_000);
        // Immediately post-boot NOTHING is authorized — one full spacing
        // must elapse first (the conservative belt-and-braces).
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Nifty, 100_000),
            GateVerdict::RetryAtMs(103_000)
        );
        assert_eq!(
            gates.try_acquire_spot(100_100),
            GateVerdict::RetryAtMs(100_400)
        );
        // After one spacing everything flows again.
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Nifty, 103_000),
            GateVerdict::Acquired
        );
        assert_eq!(gates.try_acquire_spot(100_400), GateVerdict::Acquired);
    }

    #[test]
    fn test_cadence_gate_chain_acquire_is_per_underlying_then_global() {
        let gates = DhanGates::new(3_000, 400);
        // NIFTY at t=0 passes both its own and the global gate.
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Nifty, 0),
            GateVerdict::Acquired
        );
        // BANKNIFTY 1s later: its per-underlying gate is fresh (slot
        // conservatively consumed → binds at 4s), but the GLOBAL gate
        // defers at 3s — the carried instant is the MAX (4s), so the
        // caller's next wake can actually pass.
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Banknifty, 1_000),
            GateVerdict::RetryAtMs(4_000)
        );
        // At the max boundary it flows (per-UL 4s ok, global 3s ok).
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Banknifty, 4_000),
            GateVerdict::Acquired
        );
        // NIFTY 500ms after the BANKNIFTY fire: NIFTY's own gate is clear
        // (last NIFTY fire was t=0), but the GLOBAL gate advanced to 7s;
        // NIFTY's slot is conservatively consumed → binds at 7.5s = max.
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Nifty, 4_500),
            GateVerdict::RetryAtMs(7_500)
        );
    }

    #[test]
    fn test_cadence_gate_try_acquire_chain_try_acquire_spot_independent_spacing_ms() {
        // The spot gate and the chain gates are INDEPENDENT floors: a
        // chain fire never consumes a spot slot and vice versa.
        let gates = DhanGates::new(3_000, 400);
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Sensex, 1_000),
            GateVerdict::Acquired
        );
        assert_eq!(gates.try_acquire_spot(1_010), GateVerdict::Acquired);
        assert_eq!(gates.try_acquire_spot(1_200), GateVerdict::RetryAtMs(1_410));
        assert_eq!(gates.try_acquire_spot(1_410), GateVerdict::Acquired);
        // spacing_ms accessor reports the configured floor.
        assert_eq!(MinSpacingGate::new(400).spacing_ms(), 400);
    }

    #[test]
    fn test_cadence_gate_reseed_all_wastes_at_most_one_slot() {
        // reseed_all (boot / restart) defers every gate exactly one full
        // spacing from the reseed instant — never more, never less.
        let gates = DhanGates::new(3_000, 400);
        // Consume slots so a reseed must OVERWRITE live state, not just
        // initialize fresh gates.
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Nifty, 500),
            GateVerdict::Acquired
        );
        assert_eq!(gates.try_acquire_spot(500), GateVerdict::Acquired);
        gates.reseed_all(10_000);
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Nifty, 12_999),
            GateVerdict::RetryAtMs(13_000)
        );
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Nifty, 13_000),
            GateVerdict::Acquired
        );
        assert_eq!(
            gates.try_acquire_spot(10_399),
            GateVerdict::RetryAtMs(10_400)
        );
        assert_eq!(gates.try_acquire_spot(10_400), GateVerdict::Acquired);
    }
}

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

use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};

use tickvault_common::constants::{CADENCE_SPOT_WINDOW_CAP_CEILING, CADENCE_SPOT_WINDOW_MS};

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

/// A ROLLING-WINDOW fire-authorization gate (operator spot-concurrency
/// ladder gate change, 2026-07-15): at most `cap` authorizations in ANY
/// sliding `window_ms` window, enforced over a fixed-size ring of the
/// last `cap` authorization instants (cap ≤ 5 — the Dhan Data-API 5/sec
/// hard cap — so the ring is O(1) fixed-size). Same injected-clock
/// MONOTONIC-domain design as [`MinSpacingGate`]: the wall clock only
/// picks TARGET instants, this gate is the structural floor. The ring
/// lives behind an uncontended `Mutex` (the cadence runner is
/// single-tasked per gate; the lock is poison-recovering and cold-path —
/// a handful of acquires per minute).
///
/// Boundary convention mirrors [`MinSpacingGate`]: an authorization at
/// EXACTLY `oldest + window_ms` passes (the oldest instant falls out of
/// the half-open window `(now − window_ms, now]` at that instant).
#[derive(Debug)]
pub struct RollingWindowGate {
    /// The last `cap` authorization instants, oldest-first ring.
    ring: Mutex<WindowRing>,
    /// The sliding window length, ms.
    window_ms: i64,
}

/// The fixed-size authorization ring (cap ≤ [`CADENCE_SPOT_WINDOW_CAP_CEILING`]).
#[derive(Debug)]
struct WindowRing {
    /// Authorization instants (monotonic ms), ring-indexed.
    slots: [i64; CADENCE_SPOT_WINDOW_CAP_CEILING as usize],
    /// The index of the OLDEST retained instant.
    head: usize,
    /// How many instants are retained (≤ cap).
    len: usize,
    /// The enforced cap (1..=5, validated at boot).
    cap: usize,
}

impl RollingWindowGate {
    /// A fresh gate: the first `cap` acquires always pass. `cap` is
    /// clamped to `1..=CADENCE_SPOT_WINDOW_CAP_CEILING` (validated at
    /// boot; the clamp is fail-closed defense).
    #[must_use]
    pub fn new(window_ms: i64, cap: u32) -> Self {
        let cap = cap.clamp(1, CADENCE_SPOT_WINDOW_CAP_CEILING) as usize;
        Self {
            ring: Mutex::new(WindowRing {
                slots: [i64::MIN; CADENCE_SPOT_WINDOW_CAP_CEILING as usize],
                head: 0,
                len: 0,
                cap,
            }),
            window_ms,
        }
    }

    /// The sliding window length, ms.
    // TEST-EXEMPT: trivial field accessor consumed by the replay-proof assertions.
    #[must_use]
    pub fn window_ms(&self) -> i64 {
        self.window_ms
    }

    /// The enforced cap.
    // TEST-EXEMPT: trivial field accessor consumed by the replay-proof assertions.
    #[must_use]
    pub fn cap(&self) -> usize {
        self.ring
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cap
    }

    /// Attempt to authorize a fire at monotonic instant
    /// `now_monotonic_ms`: authorized iff fewer than `cap` prior
    /// authorizations sit inside `(now − window_ms, now]` — equivalently,
    /// iff the cap-th most recent authorization is at/older than
    /// `now − window_ms`. A denial carries the earliest admissible
    /// instant (`oldest_retained + window_ms`) — a DEFERRAL, never a
    /// violation.
    #[must_use]
    pub fn try_acquire(&self, now_monotonic_ms: i64) -> GateVerdict {
        let mut ring = self
            .ring
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if ring.len == ring.cap {
            let oldest = ring.slots[ring.head];
            let earliest = oldest.saturating_add(self.window_ms);
            if now_monotonic_ms < earliest {
                return GateVerdict::RetryAtMs(earliest);
            }
            // The oldest instant falls out of the window; overwrite it.
            let head = ring.head;
            ring.slots[head] = now_monotonic_ms;
            ring.head = (head + 1) % ring.cap;
        } else {
            let idx = (ring.head + ring.len) % ring.cap;
            ring.slots[idx] = now_monotonic_ms;
            ring.len += 1;
        }
        GateVerdict::Acquired
    }

    /// Conservative boot re-seed (the [`MinSpacingGate::reseed`] mirror):
    /// pretend a FULL window of authorizations just happened at `now` —
    /// nothing is authorized for one whole window, wasting at most one
    /// window against fires the OLD process may have issued just before
    /// dying.
    pub fn reseed(&self, now_monotonic_ms: i64) {
        let mut ring = self
            .ring
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cap = ring.cap;
        for slot in &mut ring.slots[..cap] {
            *slot = now_monotonic_ms;
        }
        ring.head = 0;
        ring.len = cap;
    }
}

/// The Dhan lane's gate set (design §4): one gate per chain underlying +
/// one GLOBAL chain gate (the strictest interpretation of Dhan's
/// 1-unique-request-per-3s option-chain rule) + the spot ROLLING-WINDOW
/// gate.
#[derive(Debug)]
pub struct DhanGates {
    /// Per-underlying chain gates, indexed by [`ChainUnderlying::index`].
    chain_per_underlying: [MinSpacingGate; ChainUnderlying::COUNT],
    /// The global chain gate — EVERY chain fire (any underlying, primary
    /// or retry) must also pass this.
    chain_global: MinSpacingGate,
    /// The Dhan spot ROLLING-1000ms-WINDOW gate (2026-07-15 gate change:
    /// the concurrency ladder's step-0 group fires 4 spots at ONE
    /// instant, which a min-spacing gate cannot admit — the window gate
    /// is the structural ceiling instead: ≤ `spot_window_cap` in ANY
    /// sliding 1000ms window, default 4, hard cap 5 = the Dhan Data-API
    /// per-second budget). The shared `dhan_data_api_limiter` at the
    /// executor seam remains the SECOND live floor — it will smooth a
    /// 4-simultaneous burst to ~3/sec (its default target rps); the
    /// scheduler's window gate is the structural ceiling and the shared
    /// limiter is defense-in-depth (composition documented, never
    /// fought).
    spot: RollingWindowGate,
}

impl DhanGates {
    /// Build the gate set from the validated `[cadence]` spacings + the
    /// spot window cap.
    #[must_use]
    pub fn new(chain_spacing_ms: i64, spot_window_cap: u32) -> Self {
        Self {
            chain_per_underlying: std::array::from_fn(|_| MinSpacingGate::new(chain_spacing_ms)),
            chain_global: MinSpacingGate::new(chain_spacing_ms),
            spot: RollingWindowGate::new(CADENCE_SPOT_WINDOW_MS, spot_window_cap),
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
        let gates = DhanGates::new(3_000, 4);
        gates.reseed_all(100_000);
        // Immediately post-boot NOTHING is authorized — one full spacing
        // (chains) / one full window (spots) must elapse first (the
        // conservative belt-and-braces).
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Nifty, 100_000),
            GateVerdict::RetryAtMs(103_000)
        );
        assert_eq!(
            gates.try_acquire_spot(100_100),
            GateVerdict::RetryAtMs(101_000)
        );
        // After one spacing/window everything flows again.
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Nifty, 103_000),
            GateVerdict::Acquired
        );
        assert_eq!(gates.try_acquire_spot(101_000), GateVerdict::Acquired);
    }

    #[test]
    fn test_cadence_gate_rolling_window_cap_and_boundary() {
        // Cap 4 in ANY sliding 1000ms window: a full simultaneous step-0
        // group (4 at one instant) is authorized; the 5th in the same
        // window defers to the earliest admissible instant.
        let gate = RollingWindowGate::new(1_000, 4);
        assert_eq!(gate.window_ms(), 1_000);
        assert_eq!(gate.cap(), 4);
        for _ in 0..4 {
            assert_eq!(gate.try_acquire(10_000), GateVerdict::Acquired);
        }
        assert_eq!(gate.try_acquire(10_000), GateVerdict::RetryAtMs(11_000));
        assert_eq!(gate.try_acquire(10_999), GateVerdict::RetryAtMs(11_000));
        // EXACTLY at oldest + window the oldest falls out (inclusive
        // boundary — the MinSpacingGate convention).
        assert_eq!(gate.try_acquire(11_000), GateVerdict::Acquired);
        // The window slides per-instant: each later acquire ages one more
        // 10_000 auth out of ITS window…
        assert_eq!(gate.try_acquire(11_100), GateVerdict::Acquired);
        assert_eq!(gate.try_acquire(11_200), GateVerdict::Acquired);
        assert_eq!(gate.try_acquire(11_300), GateVerdict::Acquired);
        // …until the retained set is {11_000, 11_100, 11_200, 11_300}:
        // the window (10_400, 11_400] already holds 4 → deny until the
        // oldest retained (11_000) ages out at 12_000.
        assert_eq!(gate.try_acquire(11_400), GateVerdict::RetryAtMs(12_000));
        assert_eq!(gate.try_acquire(12_000), GateVerdict::Acquired);
    }

    #[test]
    fn test_cadence_gate_rolling_window_monotonic_immune_and_reseed() {
        // The window gate never sees a wall clock — a regressed monotonic
        // input (impossible in production; defensive) is still refused
        // while the ring holds newer instants.
        let gate = RollingWindowGate::new(1_000, 2);
        assert_eq!(gate.try_acquire(50_000), GateVerdict::Acquired);
        assert_eq!(gate.try_acquire(50_000), GateVerdict::Acquired);
        assert_eq!(gate.try_acquire(10_000), GateVerdict::RetryAtMs(51_000));
        assert_eq!(gate.try_acquire(50_999), GateVerdict::RetryAtMs(51_000));
        assert_eq!(gate.try_acquire(51_000), GateVerdict::Acquired);
        // Reseed fills the whole window conservatively — nothing admits
        // for one full window, then the full cap flows again.
        gate.reseed(80_000);
        assert_eq!(gate.try_acquire(80_999), GateVerdict::RetryAtMs(81_000));
        assert_eq!(gate.try_acquire(81_000), GateVerdict::Acquired);
        assert_eq!(gate.try_acquire(81_000), GateVerdict::Acquired);
        assert_eq!(gate.try_acquire(81_000), GateVerdict::RetryAtMs(82_000));
        // The cap clamp is fail-closed (0 → 1; 9 → the 5/sec ceiling).
        assert_eq!(RollingWindowGate::new(1_000, 0).cap(), 1);
        assert_eq!(RollingWindowGate::new(1_000, 9).cap(), 5);
    }

    #[test]
    fn test_cadence_gate_chain_acquire_is_per_underlying_then_global() {
        let gates = DhanGates::new(3_000, 4);
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
        // The spot window gate and the chain gates are INDEPENDENT
        // floors: a chain fire never consumes a spot slot and vice versa.
        let gates = DhanGates::new(3_000, 2);
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Sensex, 1_000),
            GateVerdict::Acquired
        );
        assert_eq!(gates.try_acquire_spot(1_010), GateVerdict::Acquired);
        assert_eq!(gates.try_acquire_spot(1_200), GateVerdict::Acquired);
        assert_eq!(gates.try_acquire_spot(1_500), GateVerdict::RetryAtMs(2_010));
        assert_eq!(gates.try_acquire_spot(2_010), GateVerdict::Acquired);
        // spacing_ms accessor reports the configured floor.
        assert_eq!(MinSpacingGate::new(400).spacing_ms(), 400);
    }

    #[test]
    fn test_cadence_gate_reseed_all_wastes_at_most_one_slot() {
        // reseed_all (boot / restart) defers every gate exactly one full
        // spacing/window from the reseed instant — never more, never less.
        let gates = DhanGates::new(3_000, 4);
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
            gates.try_acquire_spot(10_999),
            GateVerdict::RetryAtMs(11_000)
        );
        assert_eq!(gates.try_acquire_spot(11_000), GateVerdict::Acquired);
    }
}

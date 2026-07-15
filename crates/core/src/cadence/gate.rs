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

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

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

/// A fixed-size ROLLING-WINDOW authorization ring (operator
/// spot-concurrency ladder gate change, 2026-07-15; combined-budget
/// rework, verifier L1 2026-07-15): at most `cap` authorizations in ANY
/// sliding window, over a ring of the last `cap` authorization instants
/// (cap ≤ 5 — the Dhan Data-API 5/sec hard cap — so the ring is O(1)
/// fixed-size). Same injected-clock MONOTONIC-domain design as
/// [`MinSpacingGate`]: the wall clock only picks TARGET instants, the
/// window rings are the structural floor. Rings live behind ONE
/// poison-recovering `Mutex` inside [`DhanGates`] so a MULTI-ring
/// admission (spot + combined) is checked-then-recorded ATOMICALLY —
/// both-or-neither, no phantom consumption (cold path — a handful of
/// acquires per minute).
///
/// Boundary convention mirrors [`MinSpacingGate`]: an authorization at
/// EXACTLY `oldest + window_ms` passes (the oldest instant falls out of
/// the half-open window `(now − window_ms, now]` at that instant).
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

impl WindowRing {
    /// A fresh ring: the first `cap` records always admit. `cap` is
    /// clamped to `1..=CADENCE_SPOT_WINDOW_CAP_CEILING` (validated at
    /// boot; the clamp is fail-closed defense).
    fn new(cap: u32) -> Self {
        let cap = cap.clamp(1, CADENCE_SPOT_WINDOW_CAP_CEILING) as usize;
        Self {
            slots: [i64::MIN; CADENCE_SPOT_WINDOW_CAP_CEILING as usize],
            head: 0,
            len: 0,
            cap,
        }
    }

    /// Is a fire at `now` admissible WITHOUT recording it? Admissible
    /// iff fewer than `cap` prior authorizations sit inside
    /// `(now − window_ms, now]`. A denial carries the earliest
    /// admissible instant (`oldest_retained + window_ms`) — a DEFERRAL,
    /// never a violation.
    fn admissible(&self, now_monotonic_ms: i64, window_ms: i64) -> Result<(), i64> {
        if self.len == self.cap {
            let oldest = self.slots[self.head];
            let earliest = oldest.saturating_add(window_ms);
            if now_monotonic_ms < earliest {
                return Err(earliest);
            }
        }
        Ok(())
    }

    /// Record an AUTHORIZED fire at `now` (callers check [`Self::admissible`]
    /// first, under the same lock — atomic both-or-neither).
    fn record(&mut self, now_monotonic_ms: i64) {
        if self.len == self.cap {
            // The oldest instant falls out of the window; overwrite it.
            let head = self.head;
            self.slots[head] = now_monotonic_ms;
            self.head = (head + 1) % self.cap;
        } else {
            let idx = (self.head + self.len) % self.cap;
            self.slots[idx] = now_monotonic_ms;
            self.len += 1;
        }
    }

    /// Conservative boot re-seed (the [`MinSpacingGate::reseed`] mirror):
    /// pretend a FULL window of authorizations just happened at `now` —
    /// nothing admits for one whole window, wasting at most one window
    /// against fires the OLD process may have issued just before dying.
    fn fill(&mut self, now_monotonic_ms: i64) {
        let cap = self.cap;
        for slot in &mut self.slots[..cap] {
            *slot = now_monotonic_ms;
        }
        self.head = 0;
        self.len = cap;
    }
}

/// The two rolling-window rings, under ONE lock so multi-ring
/// admissions are atomic (no phantom consumption, verifier L1
/// 2026-07-15).
#[derive(Debug)]
struct DhanWindows {
    /// The Dhan SPOT window (cap = the configured `spot_window_cap`,
    /// default 4).
    spot: WindowRing,
    /// The COMBINED Dhan per-second budget (verifier L1, 2026-07-15):
    /// EVERY Dhan cadence fire — chain, spot AND expiry-list — records
    /// here, cap = [`CADENCE_SPOT_WINDOW_CAP_CEILING`] (5 = Dhan's
    /// Data-API per-second hard budget). Pre-L1 the spot window and the
    /// chain spacing gates were independent, so a chain fire + a full
    /// spot group could land 5 (or, at `spot_window_cap = 5`, 6) Dhan
    /// requests in one rolling second — zero headroom vs Dhan's 5/sec.
    combined: WindowRing,
}

/// One-expiry-fire-per-rolling-second spacing (verifier L2, 2026-07-15):
/// bounds the expiry-resolution wave to ≤1 fire per window so the
/// on-time nominal schedule's worst window (4 spots, or 1 chain) plus
/// one expiry fire stays ≤ the combined 5/sec budget — an expiry wave
/// can never gate-defer a nominal cycle slot.
const CADENCE_EXPIRY_FIRE_SPACING_MS: i64 = CADENCE_SPOT_WINDOW_MS;

/// The Dhan lane's gate set (design §4): one gate per chain underlying +
/// one GLOBAL chain gate (the strictest interpretation of Dhan's
/// 1-unique-request-per-3s option-chain rule) + the spot ROLLING-WINDOW
/// gate + the COMBINED chain+spot+expiry per-second budget (verifier L1,
/// 2026-07-15) + the expiry-fire spacing gate (verifier L2, 2026-07-15).
#[derive(Debug)]
pub struct DhanGates {
    /// Per-underlying chain gates, indexed by [`ChainUnderlying::index`].
    chain_per_underlying: [MinSpacingGate; ChainUnderlying::COUNT],
    /// The global chain gate — EVERY chain fire (any underlying, primary
    /// or retry) must also pass this.
    chain_global: MinSpacingGate,
    /// The expiry-list fire spacing gate (verifier L2, 2026-07-15): ≤1
    /// Dhan expiry-list fire per rolling second, so a resolution wave
    /// can never crowd a nominal cycle window out of the combined
    /// budget. Deferral = the resolver sleeps to the instant or retries
    /// next wave — never an ungated fire.
    expiry_spacing: MinSpacingGate,
    /// The Dhan SPOT rolling-1000ms window + the COMBINED per-second
    /// budget, under ONE lock (2026-07-15 gate change + verifier L1):
    /// the concurrency ladder's step-0 group fires 4 spots at ONE
    /// instant, which a min-spacing gate cannot admit — the SPOT window
    /// ring is the structural ceiling instead (≤ `spot_window_cap` in
    /// ANY sliding 1000ms window, default 4, hard cap 5); the COMBINED
    /// ring additionally caps chain+spot+expiry Dhan fires at 5 per
    /// rolling second (Dhan's Data-API hard budget — pre-L1 a chain
    /// fire + a full spot group could jointly hit 5-6/sec with zero
    /// headroom). The shared `dhan_data_api_limiter` at the executor
    /// seam remains the SECOND live floor — it will smooth a
    /// 4-simultaneous burst to ~3/sec (its default target rps); the
    /// scheduler's window gates are the structural ceiling and the
    /// shared limiter is defense-in-depth (composition documented,
    /// never fought).
    ///
    /// HONEST COMPOSITION NOTE (verifier F6, dated 2026-07-15): "never
    /// queues" holds at the DEFAULT composition only (window cap 4 vs the
    /// limiter's 3 rps target leaves headroom inside a nominal cycle). At
    /// the limiter's 2 rps FLOOR (a 429-storm step-down), a
    /// gate-authorized 4-spot group CAN briefly queue inside the shared
    /// limiter — bounded (≤ ~1s of pacing spill, the limiter smooths, it
    /// never rejects) and harmless (a limiter-queue deferral is
    /// SELF-INFLICTED pacing, typed `CadenceFetchError::QueueDelay`,
    /// NON-ARMING for the ladders). Stated plainly so the composition is
    /// never mistaken for a violation.
    windows: Mutex<DhanWindows>,
    /// Per-(underlying, expiry) chain fire stamps (verifier F1(i), dated
    /// 2026-07-15): recorded on a FINAL `Acquired` when the caller knows
    /// the expiry it is firing for, consulted BEFORE the spacing gates on
    /// later expiry-known fires. The per-underlying 3s gate is ALWAYS
    /// enforced and SUBSUMES this map on the nominal single-expiry path
    /// (one expiry per underlying per day) — the map exists so a future
    /// multi-expiry composition still honors Dhan's
    /// 1-unique-request-per-3s PER (underlying, expiry) KEY rule, and so
    /// an expiry-LESS fire (boot-degraded lane) is strictly MORE
    /// conservative (it passes the per-underlying gate, which bounds
    /// every expiry of that underlying at once). Cold path: a Mutex'd
    /// HashMap touched a handful of times per minute.
    chain_expiry_stamps: Mutex<HashMap<(usize, u32), i64>>,
}

impl DhanGates {
    /// Build the gate set from the validated `[cadence]` spacings + the
    /// spot window cap.
    #[must_use]
    pub fn new(chain_spacing_ms: i64, spot_window_cap: u32) -> Self {
        Self {
            chain_per_underlying: std::array::from_fn(|_| MinSpacingGate::new(chain_spacing_ms)),
            chain_global: MinSpacingGate::new(chain_spacing_ms),
            expiry_spacing: MinSpacingGate::new(CADENCE_EXPIRY_FIRE_SPACING_MS),
            windows: Mutex::new(DhanWindows {
                spot: WindowRing::new(spot_window_cap),
                combined: WindowRing::new(CADENCE_SPOT_WINDOW_CAP_CEILING),
            }),
            chain_expiry_stamps: Mutex::new(HashMap::new()),
        }
    }

    /// Authorize a chain fire for `underlying` (optionally keyed to a
    /// resolved `expiry_yyyymmdd`): the per-(underlying, expiry) stamp is
    /// consulted FIRST when the expiry is known (F1(i), 2026-07-15), then
    /// the per-underlying gate, then the GLOBAL gate. The schedule
    /// serializes chains ≥3s apart, so the global acquire cannot fail
    /// after the per-underlying acquire succeeded on nominal slots
    /// (asserted by the replay proof); if it ever does, the
    /// per-underlying slot was conservatively consumed and the fire
    /// DEFERS — a deferral, never a violation (design §4). The carried
    /// retry instant is the MAX of both constraints (the consumed
    /// per-underlying slot now binds at `now + spacing`), so the caller's
    /// next wake cannot arrive before it can actually pass. On a FINAL
    /// `Acquired` with a known expiry, the (underlying, expiry) stamp is
    /// recorded. An expiry-less fire (`None` — boot-degraded lane)
    /// consults NO stamp but still passes the per-underlying gate, which
    /// is strictly MORE conservative (it spaces ALL expiries of the
    /// underlying at once — subsumption, pinned by
    /// `test_cadence_gate_expiryless_fire_subsumes_expiry_stamp`).
    #[must_use]
    pub fn try_acquire_chain(
        &self,
        underlying: ChainUnderlying,
        expiry_yyyymmdd: Option<u32>,
        now_monotonic_ms: i64,
    ) -> GateVerdict {
        let spacing = self.chain_per_underlying[underlying.index()].spacing_ms();
        if let Some(expiry) = expiry_yyyymmdd {
            let stamps = self
                .chain_expiry_stamps
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(&last) = stamps.get(&(underlying.index(), expiry)) {
                let earliest = last.saturating_add(spacing);
                if now_monotonic_ms < earliest {
                    return GateVerdict::RetryAtMs(earliest);
                }
            }
        }
        let verdict =
            match self.chain_per_underlying[underlying.index()].try_acquire(now_monotonic_ms) {
                GateVerdict::Acquired => {
                    match self.chain_global.try_acquire(now_monotonic_ms) {
                        GateVerdict::Acquired => {
                            // The COMBINED per-second budget (verifier
                            // L1, 2026-07-15): checked+recorded
                            // ATOMICALLY under the windows lock. A
                            // combined denial leaves the (already
                            // consumed) spacing slots binding at
                            // now + spacing — the carried instant is the
                            // MAX so the caller's next wake passes both.
                            let mut w = self
                                .windows
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            match w
                                .combined
                                .admissible(now_monotonic_ms, CADENCE_SPOT_WINDOW_MS)
                            {
                                Ok(()) => {
                                    w.combined.record(now_monotonic_ms);
                                    GateVerdict::Acquired
                                }
                                Err(combined_at) => {
                                    let per_ul_at = now_monotonic_ms.saturating_add(spacing);
                                    GateVerdict::RetryAtMs(combined_at.max(per_ul_at))
                                }
                            }
                        }
                        GateVerdict::RetryAtMs(global_at) => {
                            // The per-underlying slot was conservatively
                            // consumed above: it now binds at now + spacing.
                            let per_ul_at = now_monotonic_ms.saturating_add(spacing);
                            GateVerdict::RetryAtMs(global_at.max(per_ul_at))
                        }
                    }
                }
                defer @ GateVerdict::RetryAtMs(_) => defer,
            };
        if verdict == GateVerdict::Acquired
            && let Some(expiry) = expiry_yyyymmdd
        {
            self.chain_expiry_stamps
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert((underlying.index(), expiry), now_monotonic_ms);
        }
        verdict
    }

    /// The recorded per-(underlying, expiry) authorization stamp, if any
    /// (monotonic ms of the last expiry-keyed `Acquired`).
    #[must_use]
    // TEST-EXEMPT: covered by test_cadence_gate_expiry_stamp_recorded_and_consulted (guard name-pattern mismatch).
    pub fn chain_expiry_stamp(
        &self,
        underlying: ChainUnderlying,
        expiry_yyyymmdd: u32,
    ) -> Option<i64> {
        self.chain_expiry_stamps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(underlying.index(), expiry_yyyymmdd))
            .copied()
    }

    /// Authorize a Dhan spot fire: the SPOT window AND the COMBINED
    /// per-second budget (verifier L1, 2026-07-15), checked-then-recorded
    /// ATOMICALLY under one lock — both-or-neither, no phantom
    /// consumption. A denial carries the MAX of the failing rings'
    /// earliest-admissible instants.
    #[must_use]
    pub fn try_acquire_spot(&self, now_monotonic_ms: i64) -> GateVerdict {
        let mut w = self
            .windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let spot_check = w.spot.admissible(now_monotonic_ms, CADENCE_SPOT_WINDOW_MS);
        let combined_check = w
            .combined
            .admissible(now_monotonic_ms, CADENCE_SPOT_WINDOW_MS);
        match (spot_check, combined_check) {
            (Ok(()), Ok(())) => {
                w.spot.record(now_monotonic_ms);
                w.combined.record(now_monotonic_ms);
                GateVerdict::Acquired
            }
            (Err(a), Ok(())) | (Ok(()), Err(a)) => GateVerdict::RetryAtMs(a),
            (Err(a), Err(b)) => GateVerdict::RetryAtMs(a.max(b)),
        }
    }

    /// Authorize a Dhan EXPIRY-LIST fire (verifier L2, 2026-07-15): the
    /// COMBINED per-second budget + the 1-per-rolling-second expiry
    /// spacing. Nothing is consumed on a denial (the combined ring is
    /// only recorded AFTER the spacing gate acquires, all under the
    /// windows lock — atomic vs the chain/spot recorders), so the
    /// resolver can freely skip a deferred fire to its next wave.
    #[must_use]
    pub fn try_acquire_expiry(&self, now_monotonic_ms: i64) -> GateVerdict {
        let mut w = self
            .windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Err(at) = w
            .combined
            .admissible(now_monotonic_ms, CADENCE_SPOT_WINDOW_MS)
        {
            return GateVerdict::RetryAtMs(at);
        }
        match self.expiry_spacing.try_acquire(now_monotonic_ms) {
            GateVerdict::Acquired => {
                w.combined.record(now_monotonic_ms);
                GateVerdict::Acquired
            }
            defer @ GateVerdict::RetryAtMs(_) => defer,
        }
    }

    /// Conservative boot re-seed of EVERY gate (see
    /// [`MinSpacingGate::reseed`]) — the per-(underlying, expiry) stamps
    /// are CLEARED (a respawned runner creates a fresh monotonic domain;
    /// stale-domain stamps would defer/admit against meaningless
    /// instants, and the reseeded spacing gates already refuse everything
    /// for one full spacing anyway).
    pub fn reseed_all(&self, now_monotonic_ms: i64) {
        for g in &self.chain_per_underlying {
            g.reseed(now_monotonic_ms);
        }
        self.chain_global.reseed(now_monotonic_ms);
        self.expiry_spacing.reseed(now_monotonic_ms);
        {
            let mut w = self
                .windows
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            w.spot.fill(now_monotonic_ms);
            w.combined.fill(now_monotonic_ms);
        }
        self.chain_expiry_stamps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

/// The PROCESS-GLOBAL Dhan gate registry (verifier F1(ii), dated
/// 2026-07-15): every production Dhan cadence fire — the runner today,
/// any future real executor composition — must record/consult ONE shared
/// gate set, so two accidental runner instances (or a future
/// executor-side fast path) can never each hold private gates and jointly
/// exceed Dhan's budget. Initialized once by the boot wiring
/// ([`init_global_dhan_gates`]); tests inject their OWN isolated
/// [`DhanGates`] through the runner deps and never touch this handle.
static GLOBAL_DHAN_GATES: OnceLock<Arc<DhanGates>> = OnceLock::new();

/// Initialize-or-fetch the process-global Dhan gate set. First caller's
/// spacings win (the boot wiring runs once, guarded by the
/// once-per-process spawn latch); later callers receive the existing set
/// unchanged — deliberately first-write-wins, mirroring the
/// `global_expiry_store` / OnceLock house pattern.
// TEST-EXEMPT: covered by test_cadence_gate_global_handle_first_write_wins_and_shared (guard name-pattern mismatch).
pub fn init_global_dhan_gates(
    chain_spacing_ms: i64,
    spot_window_cap: u32,
) -> &'static Arc<DhanGates> {
    GLOBAL_DHAN_GATES.get_or_init(|| Arc::new(DhanGates::new(chain_spacing_ms, spot_window_cap)))
}

/// The process-global Dhan gate registry, initializing at the
/// conservative defaults (3s chain spacing floor, window cap 4) if the
/// boot wiring has not seeded it yet — fail-closed: the defaults are the
/// STRICTEST legal composition, so an early caller can only be MORE
/// conservative than the configured set.
#[must_use]
// TEST-EXEMPT: covered by test_cadence_gate_global_handle_first_write_wins_and_shared (guard name-pattern mismatch).
pub fn global_dhan_gates() -> &'static Arc<DhanGates> {
    init_global_dhan_gates(
        tickvault_common::constants::CADENCE_CHAIN_MIN_SPACING_FLOOR_MS,
        4,
    )
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
            gates.try_acquire_chain(ChainUnderlying::Nifty, None, 100_000),
            GateVerdict::RetryAtMs(103_000)
        );
        assert_eq!(
            gates.try_acquire_spot(100_100),
            GateVerdict::RetryAtMs(101_000)
        );
        // After one spacing/window everything flows again.
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Nifty, None, 103_000),
            GateVerdict::Acquired
        );
        assert_eq!(gates.try_acquire_spot(101_000), GateVerdict::Acquired);
    }

    #[test]
    fn test_cadence_gate_rolling_window_cap_and_boundary() {
        // Cap 4 in ANY sliding 1000ms window (through the spot path —
        // the combined ring's cap-5 headroom never binds a pure-spot
        // sequence at cap 4): a full simultaneous step-0 group (4 at one
        // instant) is authorized; the 5th in the same window defers to
        // the earliest admissible instant.
        let gates = DhanGates::new(3_000, 4);
        for _ in 0..4 {
            assert_eq!(gates.try_acquire_spot(10_000), GateVerdict::Acquired);
        }
        assert_eq!(
            gates.try_acquire_spot(10_000),
            GateVerdict::RetryAtMs(11_000)
        );
        assert_eq!(
            gates.try_acquire_spot(10_999),
            GateVerdict::RetryAtMs(11_000)
        );
        // EXACTLY at oldest + window the oldest falls out (inclusive
        // boundary — the MinSpacingGate convention).
        assert_eq!(gates.try_acquire_spot(11_000), GateVerdict::Acquired);
        // The window slides per-instant: each later acquire ages one more
        // 10_000 auth out of ITS window…
        assert_eq!(gates.try_acquire_spot(11_100), GateVerdict::Acquired);
        assert_eq!(gates.try_acquire_spot(11_200), GateVerdict::Acquired);
        assert_eq!(gates.try_acquire_spot(11_300), GateVerdict::Acquired);
        // …until the retained set is {11_000, 11_100, 11_200, 11_300}:
        // the window (10_400, 11_400] already holds 4 → deny until the
        // oldest retained (11_000) ages out at 12_000.
        assert_eq!(
            gates.try_acquire_spot(11_400),
            GateVerdict::RetryAtMs(12_000)
        );
        assert_eq!(gates.try_acquire_spot(12_000), GateVerdict::Acquired);
    }

    #[test]
    fn test_cadence_gate_rolling_window_monotonic_immune_and_reseed() {
        // The window rings never see a wall clock — a regressed monotonic
        // input (impossible in production; defensive) is still refused
        // while the ring holds newer instants.
        let gates = DhanGates::new(3_000, 2);
        assert_eq!(gates.try_acquire_spot(50_000), GateVerdict::Acquired);
        assert_eq!(gates.try_acquire_spot(50_000), GateVerdict::Acquired);
        assert_eq!(
            gates.try_acquire_spot(10_000),
            GateVerdict::RetryAtMs(51_000)
        );
        assert_eq!(
            gates.try_acquire_spot(50_999),
            GateVerdict::RetryAtMs(51_000)
        );
        assert_eq!(gates.try_acquire_spot(51_000), GateVerdict::Acquired);
        // Reseed fills the whole window conservatively — nothing admits
        // for one full window, then the full cap flows again.
        gates.reseed_all(80_000);
        assert_eq!(
            gates.try_acquire_spot(80_999),
            GateVerdict::RetryAtMs(81_000)
        );
        assert_eq!(gates.try_acquire_spot(81_000), GateVerdict::Acquired);
        assert_eq!(gates.try_acquire_spot(81_000), GateVerdict::Acquired);
        assert_eq!(
            gates.try_acquire_spot(81_000),
            GateVerdict::RetryAtMs(82_000)
        );
        // The cap clamp is fail-closed: 0 → 1 (second same-instant spot
        // refused)…
        let clamped_low = DhanGates::new(3_000, 0);
        assert_eq!(clamped_low.try_acquire_spot(5_000), GateVerdict::Acquired);
        assert_eq!(
            clamped_low.try_acquire_spot(5_000),
            GateVerdict::RetryAtMs(6_000)
        );
        // …and 9 → the 5/sec ceiling (the 6th same-instant spot refused
        // by BOTH the clamped spot ring and the combined budget).
        let clamped_high = DhanGates::new(3_000, 9);
        for _ in 0..5 {
            assert_eq!(clamped_high.try_acquire_spot(5_000), GateVerdict::Acquired);
        }
        assert_eq!(
            clamped_high.try_acquire_spot(5_000),
            GateVerdict::RetryAtMs(6_000)
        );
    }

    #[test]
    fn test_cadence_gate_chain_acquire_is_per_underlying_then_global() {
        let gates = DhanGates::new(3_000, 4);
        // NIFTY at t=0 passes both its own and the global gate.
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Nifty, None, 0),
            GateVerdict::Acquired
        );
        // BANKNIFTY 1s later: its per-underlying gate is fresh (slot
        // conservatively consumed → binds at 4s), but the GLOBAL gate
        // defers at 3s — the carried instant is the MAX (4s), so the
        // caller's next wake can actually pass.
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Banknifty, None, 1_000),
            GateVerdict::RetryAtMs(4_000)
        );
        // At the max boundary it flows (per-UL 4s ok, global 3s ok).
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Banknifty, None, 4_000),
            GateVerdict::Acquired
        );
        // NIFTY 500ms after the BANKNIFTY fire: NIFTY's own gate is clear
        // (last NIFTY fire was t=0), but the GLOBAL gate advanced to 7s;
        // NIFTY's slot is conservatively consumed → binds at 7.5s = max.
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Nifty, None, 4_500),
            GateVerdict::RetryAtMs(7_500)
        );
    }

    #[test]
    fn test_cadence_gate_try_acquire_chain_try_acquire_spot_independent_spacing_ms() {
        // The SPECIFIC gates stay independent floors (a chain fire never
        // consumes a SPOT-ring slot and vice versa) — but since L1
        // (2026-07-15) both share the COMBINED 5-per-rolling-second
        // budget, which never binds this low-volume sequence.
        let gates = DhanGates::new(3_000, 2);
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Sensex, None, 1_000),
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
    fn test_cadence_gate_combined_window_refuses_sixth_dhan_fire_per_rolling_second() {
        // Verifier L1 (2026-07-15), INVERTED from the demonstrating
        // hostile test: pre-fix a chain fire + a validation-legal
        // spot_window_cap=5 group put SIX Dhan fires inside one rolling
        // 1000ms window (Dhan's Data-API budget is 5/sec). Post-fix the
        // COMBINED ring refuses the 6th — a DEFERRAL to the instant the
        // chain fire ages out, never a violation.
        let gates = DhanGates::new(3_000, 5);
        let t = 1_000_000_i64;
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Sensex, None, t + 2_001),
            GateVerdict::Acquired
        );
        for _ in 0..4 {
            assert_eq!(gates.try_acquire_spot(t + 3_000), GateVerdict::Acquired);
        }
        // The 5th spot is legal per the SPOT ring (cap 5) but would be
        // the 6th Dhan fire inside (t+2_001, t+3_001) — the combined
        // budget defers it to EXACTLY the instant the chain fire ages
        // out of the window (nothing consumed on the denial).
        assert_eq!(
            gates.try_acquire_spot(t + 3_000),
            GateVerdict::RetryAtMs(t + 3_001)
        );
        assert_eq!(gates.try_acquire_spot(t + 3_001), GateVerdict::Acquired);
        // The combined budget binds the CHAIN direction too: 4 spots +
        // an expiry fire fill the window; a chain fire inside it is
        // deferred (spacing slots conservatively consumed → the carried
        // instant is the MAX so the next wake passes everything).
        let gates2 = DhanGates::new(3_000, 4);
        assert_eq!(gates2.try_acquire_expiry(t), GateVerdict::Acquired);
        for _ in 0..4 {
            assert_eq!(gates2.try_acquire_spot(t + 100), GateVerdict::Acquired);
        }
        assert_eq!(
            gates2.try_acquire_chain(ChainUnderlying::Nifty, None, t + 500),
            GateVerdict::RetryAtMs(t + 3_500)
        );
        assert_eq!(
            gates2.try_acquire_chain(ChainUnderlying::Nifty, None, t + 3_500),
            GateVerdict::Acquired
        );
    }

    #[test]
    fn test_cadence_gate_combined_window_admits_nominal_chain_then_spot_group() {
        // Nominal-schedule feasibility pin (L1): the locked schedule's
        // chain :02 fire + the FULL step-0 spot group at :03 (exactly one
        // window later) pass the combined gate with ZERO deferrals — the
        // chain fire sits at EXACTLY oldest+window when the group fires
        // (the inclusive boundary), so the combined budget never defers
        // an on-time nominal slot.
        let gates = DhanGates::new(3_000, 4);
        let t = 36_000_000_i64; // 10:00:00 as a monotonic stand-in
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Sensex, None, t + 2_000),
            GateVerdict::Acquired
        );
        for _ in 0..4 {
            assert_eq!(gates.try_acquire_spot(t + 3_000), GateVerdict::Acquired);
        }
    }

    #[test]
    fn test_cadence_gate_try_acquire_expiry_shares_combined_window_and_spacing() {
        // Verifier L2 (2026-07-15): expiry-list fires ride the SAME
        // combined per-second budget + their own 1-per-rolling-second
        // spacing.
        let gates = DhanGates::new(3_000, 4);
        // First fire passes; a second inside the spacing window defers
        // on the expiry spacing (nothing consumed on the denial).
        assert_eq!(gates.try_acquire_expiry(10_000), GateVerdict::Acquired);
        assert_eq!(
            gates.try_acquire_expiry(10_500),
            GateVerdict::RetryAtMs(11_000)
        );
        assert_eq!(gates.try_acquire_expiry(11_000), GateVerdict::Acquired);
        // A full combined window (4 spots + 1 chain across one second)
        // defers an expiry fire to the instant the oldest fire ages out.
        let gates2 = DhanGates::new(3_000, 4);
        assert_eq!(
            gates2.try_acquire_chain(ChainUnderlying::Banknifty, None, 20_000),
            GateVerdict::Acquired
        );
        for _ in 0..4 {
            assert_eq!(gates2.try_acquire_spot(20_400), GateVerdict::Acquired);
        }
        assert_eq!(
            gates2.try_acquire_expiry(20_800),
            GateVerdict::RetryAtMs(21_000)
        );
        assert_eq!(gates2.try_acquire_expiry(21_000), GateVerdict::Acquired);
        // An expiry fire COUNTS toward the combined budget for later
        // spot fires too (the reverse direction): after the expiry fire
        // at 21_000 the window (20_400, 21_400] holds 4 spots + 1 expiry
        // = 5 → a spot at 21_100 is deferred until the 20_400 group ages
        // out.
        assert_eq!(
            gates2.try_acquire_spot(21_100),
            GateVerdict::RetryAtMs(21_400)
        );
        assert_eq!(gates2.try_acquire_spot(21_400), GateVerdict::Acquired);
        // reseed_all covers the expiry spacing gate too.
        gates2.reseed_all(50_000);
        assert_eq!(
            gates2.try_acquire_expiry(50_500),
            GateVerdict::RetryAtMs(51_000)
        );
        assert_eq!(gates2.try_acquire_expiry(51_000), GateVerdict::Acquired);
    }

    #[test]
    fn test_cadence_gate_reseed_all_wastes_at_most_one_slot() {
        // reseed_all (boot / restart) defers every gate exactly one full
        // spacing/window from the reseed instant — never more, never less.
        let gates = DhanGates::new(3_000, 4);
        // Consume slots so a reseed must OVERWRITE live state, not just
        // initialize fresh gates.
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Nifty, None, 500),
            GateVerdict::Acquired
        );
        assert_eq!(gates.try_acquire_spot(500), GateVerdict::Acquired);
        gates.reseed_all(10_000);
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Nifty, None, 12_999),
            GateVerdict::RetryAtMs(13_000)
        );
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Nifty, None, 13_000),
            GateVerdict::Acquired
        );
        assert_eq!(
            gates.try_acquire_spot(10_999),
            GateVerdict::RetryAtMs(11_000)
        );
        assert_eq!(gates.try_acquire_spot(11_000), GateVerdict::Acquired);
        // reseed_all ALSO clears the expiry stamps (fresh monotonic
        // domain on a respawn — stale-domain stamps are meaningless).
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Sensex, Some(20_260_730), 20_000),
            GateVerdict::Acquired
        );
        assert!(
            gates
                .chain_expiry_stamp(ChainUnderlying::Sensex, 20_260_730)
                .is_some()
        );
        gates.reseed_all(30_000);
        assert!(
            gates
                .chain_expiry_stamp(ChainUnderlying::Sensex, 20_260_730)
                .is_none()
        );
    }

    #[test]
    fn test_cadence_gate_expiry_stamp_recorded_and_consulted() {
        // F1(i), 2026-07-15: an expiry-keyed Acquired records the
        // (underlying, expiry) stamp; a later expiry-keyed fire for the
        // SAME key defers on the stamp even if (hypothetically) the
        // spacing gates were bypassed — the stamp is consulted FIRST.
        let gates = DhanGates::new(3_000, 4);
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Nifty, Some(20_260_716), 10_000),
            GateVerdict::Acquired
        );
        assert_eq!(
            gates.chain_expiry_stamp(ChainUnderlying::Nifty, 20_260_716),
            Some(10_000)
        );
        // Same key inside the window → deferral carries the stamp bound.
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Nifty, Some(20_260_716), 11_000),
            GateVerdict::RetryAtMs(13_000)
        );
        // A DIFFERENT expiry of the same underlying is NOT deferred by
        // the stamp — but the per-underlying gate still binds (13s), so
        // the composed verdict is a deferral (subsumption in the other
        // direction: the per-underlying gate is always enforced).
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Nifty, Some(20_260_723), 11_000),
            GateVerdict::RetryAtMs(13_000)
        );
        // A deferred fire never records a stamp for its key.
        assert_eq!(
            gates.chain_expiry_stamp(ChainUnderlying::Nifty, 20_260_723),
            None
        );
        // At/after the boundary the same key flows again and re-stamps.
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Nifty, Some(20_260_716), 13_000),
            GateVerdict::Acquired
        );
        assert_eq!(
            gates.chain_expiry_stamp(ChainUnderlying::Nifty, 20_260_716),
            Some(13_000)
        );
    }

    #[test]
    fn test_cadence_gate_expiryless_fire_subsumes_expiry_stamp() {
        // F1(i) SUBSUMPTION, 2026-07-15: an expiry-LESS fire (None — the
        // boot-degraded lane) records no stamp, but the ALWAYS-enforced
        // per-underlying gate bounds EVERY expiry of that underlying at
        // once — so two expiry-less fires intended for DIFFERENT expiries
        // sharing one underlying can never be closer than the
        // per-(underlying, expiry) rule would allow for either key. The
        // expiry-less path is therefore strictly MORE conservative.
        let gates = DhanGates::new(3_000, 4);
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Banknifty, None, 5_000),
            GateVerdict::Acquired
        );
        // No stamp was recorded for ANY expiry key…
        assert_eq!(
            gates.chain_expiry_stamp(ChainUnderlying::Banknifty, 20_260_729),
            None
        );
        // …yet a second expiry-less fire (intended for a DIFFERENT
        // expiry) is still refused inside the window: the per-underlying
        // gate subsumes the per-key rule.
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Banknifty, None, 7_999),
            GateVerdict::RetryAtMs(8_000)
        );
        // An expiry-KEYED fire after an expiry-less fire is equally
        // bounded by the per-underlying gate (no stamp needed).
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Banknifty, Some(20_260_729), 7_500),
            GateVerdict::RetryAtMs(8_000)
        );
        assert_eq!(
            gates.try_acquire_chain(ChainUnderlying::Banknifty, Some(20_260_729), 8_000),
            GateVerdict::Acquired
        );
    }

    #[test]
    fn test_cadence_gate_global_handle_first_write_wins_and_shared() {
        // F1(ii), 2026-07-15: the process-global registry hands every
        // caller the SAME Arc; a later init with different knobs does NOT
        // replace it (first-write-wins, OnceLock semantics).
        let a = init_global_dhan_gates(3_000, 4);
        let b = global_dhan_gates();
        let c = init_global_dhan_gates(9_999, 1);
        assert!(Arc::ptr_eq(a, b));
        assert!(Arc::ptr_eq(a, c));
        // Shared state is visible across handles: a fire through one
        // handle defers the same fire through another.
        let now = 1_000_000;
        let first = a.try_acquire_chain(ChainUnderlying::Sensex, None, now);
        // (Tolerate a prior test-process user of the global handle — the
        // verdict either acquires fresh or defers; EITHER way the second
        // handle must observe the SAME gate state.)
        match first {
            GateVerdict::Acquired => {
                assert_eq!(
                    b.try_acquire_chain(ChainUnderlying::Sensex, None, now),
                    GateVerdict::RetryAtMs(now + 3_000)
                );
            }
            GateVerdict::RetryAtMs(at) => {
                assert_eq!(
                    b.try_acquire_chain(ChainUnderlying::Sensex, None, now),
                    GateVerdict::RetryAtMs(at)
                );
            }
        }
    }
}

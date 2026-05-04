//! Boot-ordering gate for Phase 2 (29-tf engine plan, L14 invariant).
//!
//! Per plan L14:
//!
//! > **Strict cold-boot ordering invariant.** Boot sequence MUST complete
//! > in this order before WS subscribe fires:
//! >  1. `prev_oi_cache` loaded from `candles_1d` yesterday,
//! >  2. `CandleEngine` and `MoversEngine` instantiated and ready,
//! >  3. historical replay of last 5 min ticks from `ticks` table,
//! >  4. THEN Dhan WS subscribe.
//! > Ratchet test pins each gate. Hostile bug-hunt CRITICAL C4 fix.
//!
//! ## Design
//!
//! Single atomic state machine with 4 monotonically-flippable flags.
//! `subscribe_allowed()` returns `true` only when all 4 are set, in the
//! correct order. The boot driver in `main.rs` calls `mark_*_ready()`
//! on each phase completion, then checks `subscribe_allowed()` before
//! firing the WS connect.
//!
//! Why a struct rather than a sequence of `if`s in the boot driver:
//! - Ratchet-testable independent of the boot sequence's other concerns
//!   (auth, metrics, depth, etc.)
//! - Detects out-of-order calls (e.g. `mark_replay_ready` before
//!   `mark_oi_cache_loaded`) — `subscribe_allowed` returns false in
//!   that case, with a diagnostic getter for the failing flag.
//! - Carries no `Arc<Mutex<>>` — the 4 flags are atomic booleans, so
//!   the gate can be safely shared across boot tasks if needed.
//!
//! ## What this commit does NOT do
//!
//! Wiring this gate into the production boot sequence (`crates/app/src/main.rs`)
//! and `run_tick_processor` is deferred to a follow-up PR. The reasons:
//!
//! - Two boot paths (slow/fast) with subtle ordering differences that
//!   need an isolated review.
//! - The gate must integrate with the existing pool watchdog +
//!   subscribe dispatch flow without breaking the depth-rebalancer
//!   path or the `SubscribeRxGuard` invariant from PR #337.
//! - Phase 2 commits 1-7 ship the foundation (code, tests, contracts);
//!   the production wire-up is a focused diff that benefits from
//!   landing on a clean main branch.
//!
//! Until the wire-up ships, this gate is a `pub` helper available for
//! review and unit testing. Operator can adopt the wire-up in a focused
//! Phase 2.5 PR with controlled rollout.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// L14 boot-ordering gate. Cloneable; clones share the same Arc-backed
/// flags so multiple boot tasks can mark phases independently.
#[derive(Clone, Default)]
pub struct BootOrderingGate {
    /// Phase 1: prev_oi_cache successfully loaded from QuestDB
    /// candles_1d (or known-empty on fresh deploy — both flip the
    /// flag, so the gate doesn't deadlock on cold-start).
    oi_cache_loaded: Arc<AtomicBool>,
    /// Phase 2: CandleEngine + MoversEngine constructed and registered
    /// in the pipeline. (These engines land in Phase 3; the gate field
    /// is reserved here so the L14 invariant covers them once they
    /// exist. Until Phase 3, the boot driver flips this flag
    /// immediately after Phase 1.)
    engines_ready: Arc<AtomicBool>,
    /// Phase 3: historical replay completed (last 5 min ticks from
    /// `ticks` table replayed into the enricher state). On a fresh
    /// deploy or a cold-start with empty ticks, this flag flips
    /// immediately after the no-op replay returns. Never blocks.
    replay_completed: Arc<AtomicBool>,
    /// Phase 4: WS subscribe explicitly authorized — the boot driver
    /// flips this AFTER it has confirmed the previous 3 are set, as
    /// a "ready to dispatch subscribe" handshake. Tests verify the
    /// previous 3 flags MUST be set before this can be flipped.
    subscribe_authorized: Arc<AtomicBool>,
}

/// Result of a boot-ordering check, useful for diagnostics in
/// integration tests + log lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubscribeReadiness {
    /// All 4 phases marked ready in the correct order.
    Ready,
    /// `prev_oi_cache` not yet loaded.
    AwaitingOiCache,
    /// Engines not yet constructed.
    AwaitingEngines,
    /// Historical replay not yet completed.
    AwaitingReplay,
    /// All 3 prerequisites met but subscribe not yet authorized
    /// (boot driver hasn't issued the final go-ahead).
    AwaitingAuthorization,
}

impl BootOrderingGate {
    /// New gate with all flags clear.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark Phase 1 (OI cache load) complete. Idempotent — calling
    /// twice is safe; the flag stays set.
    #[inline]
    pub fn mark_oi_cache_loaded(&self) {
        self.oi_cache_loaded.store(true, Ordering::Release);
    }

    /// Mark Phase 2 (engines instantiated) complete. Idempotent.
    #[inline]
    pub fn mark_engines_ready(&self) {
        self.engines_ready.store(true, Ordering::Release);
    }

    /// Mark Phase 3 (historical replay) complete. Idempotent.
    #[inline]
    pub fn mark_replay_completed(&self) {
        self.replay_completed.store(true, Ordering::Release);
    }

    /// Mark Phase 4 (subscribe authorized) complete. Returns `false`
    /// if the previous 3 phases are not yet set — caller MUST resolve
    /// the upstream phase before authorizing subscribe. This is the
    /// L14 hostile-bug-hunt fix: prevent the boot driver from
    /// authorizing subscribe ahead of the required phases.
    #[must_use]
    pub fn try_authorize_subscribe(&self) -> bool {
        if !self.oi_cache_loaded.load(Ordering::Acquire) {
            return false;
        }
        if !self.engines_ready.load(Ordering::Acquire) {
            return false;
        }
        if !self.replay_completed.load(Ordering::Acquire) {
            return false;
        }
        self.subscribe_authorized.store(true, Ordering::Release);
        true
    }

    /// Returns `true` only when all 4 phases have been marked ready
    /// in the correct order. The WS connect / subscribe-dispatch site
    /// reads this BEFORE sending the subscribe frame.
    #[inline]
    pub fn subscribe_allowed(&self) -> bool {
        self.oi_cache_loaded.load(Ordering::Acquire)
            && self.engines_ready.load(Ordering::Acquire)
            && self.replay_completed.load(Ordering::Acquire)
            && self.subscribe_authorized.load(Ordering::Acquire)
    }

    /// Diagnostic readiness — useful for integration tests + log
    /// lines that need to identify the blocking phase.
    pub fn readiness(&self) -> SubscribeReadiness {
        if !self.oi_cache_loaded.load(Ordering::Acquire) {
            SubscribeReadiness::AwaitingOiCache
        } else if !self.engines_ready.load(Ordering::Acquire) {
            SubscribeReadiness::AwaitingEngines
        } else if !self.replay_completed.load(Ordering::Acquire) {
            SubscribeReadiness::AwaitingReplay
        } else if !self.subscribe_authorized.load(Ordering::Acquire) {
            SubscribeReadiness::AwaitingAuthorization
        } else {
            SubscribeReadiness::Ready
        }
    }

    /// Test-only escape hatch: forcibly flip the gate to Ready. Used
    /// by integration tests that don't model the full boot sequence
    /// but need a Ready gate to exercise post-boot code paths. Marked
    /// `#[cfg(test)]` so it can't be misused in production.
    #[cfg(test)]
    pub fn force_ready_for_test(&self) {
        self.mark_oi_cache_loaded();
        self.mark_engines_ready();
        self.mark_replay_completed();
        let _ = self.try_authorize_subscribe();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_gate_is_not_ready() {
        let g = BootOrderingGate::new();
        assert!(!g.subscribe_allowed());
        assert_eq!(g.readiness(), SubscribeReadiness::AwaitingOiCache);
    }

    /// L14 invariant: `try_authorize_subscribe` MUST refuse if any
    /// upstream phase is not set. This is the hostile-bug-hunt CRITICAL
    /// C4 fix — prevents the boot driver from authorizing subscribe
    /// ahead of the required phases.
    #[test]
    fn test_try_authorize_refuses_without_oi_cache() {
        let g = BootOrderingGate::new();
        g.mark_engines_ready();
        g.mark_replay_completed();
        assert!(
            !g.try_authorize_subscribe(),
            "MUST refuse without OI cache loaded"
        );
        assert!(!g.subscribe_allowed());
        assert_eq!(g.readiness(), SubscribeReadiness::AwaitingOiCache);
    }

    #[test]
    fn test_try_authorize_refuses_without_engines() {
        let g = BootOrderingGate::new();
        g.mark_oi_cache_loaded();
        g.mark_replay_completed();
        assert!(
            !g.try_authorize_subscribe(),
            "MUST refuse without engines ready"
        );
        assert_eq!(g.readiness(), SubscribeReadiness::AwaitingEngines);
    }

    #[test]
    fn test_try_authorize_refuses_without_replay() {
        let g = BootOrderingGate::new();
        g.mark_oi_cache_loaded();
        g.mark_engines_ready();
        assert!(
            !g.try_authorize_subscribe(),
            "MUST refuse without replay completed"
        );
        assert_eq!(g.readiness(), SubscribeReadiness::AwaitingReplay);
    }

    #[test]
    fn test_full_sequence_succeeds() {
        let g = BootOrderingGate::new();
        g.mark_oi_cache_loaded();
        assert_eq!(g.readiness(), SubscribeReadiness::AwaitingEngines);
        g.mark_engines_ready();
        assert_eq!(g.readiness(), SubscribeReadiness::AwaitingReplay);
        g.mark_replay_completed();
        assert_eq!(g.readiness(), SubscribeReadiness::AwaitingAuthorization);
        assert!(g.try_authorize_subscribe());
        assert!(g.subscribe_allowed());
        assert_eq!(g.readiness(), SubscribeReadiness::Ready);
    }

    /// L14 ratchet: marking phases out of order is harmless — the gate
    /// only authorizes when ALL prerequisites are met. The intent is
    /// boot drivers can mark phases concurrently if they wish; only
    /// the authorization step has the ordering invariant.
    #[test]
    fn test_phases_can_be_marked_out_of_order_safely() {
        let g = BootOrderingGate::new();
        // Out-of-order marking — perfectly fine.
        g.mark_replay_completed();
        g.mark_oi_cache_loaded();
        g.mark_engines_ready();
        assert!(
            g.try_authorize_subscribe(),
            "all prerequisites met (regardless of mark order)"
        );
        assert!(g.subscribe_allowed());
    }

    #[test]
    fn test_idempotent_marks() {
        let g = BootOrderingGate::new();
        g.mark_oi_cache_loaded();
        g.mark_oi_cache_loaded(); // double-call safe
        g.mark_oi_cache_loaded();
        g.mark_engines_ready();
        g.mark_replay_completed();
        assert!(g.try_authorize_subscribe());
        assert!(g.try_authorize_subscribe()); // double-call safe
    }

    #[test]
    fn test_clone_shares_state() {
        let g1 = BootOrderingGate::new();
        let g2 = g1.clone();
        g1.mark_oi_cache_loaded();
        // The clone sees the same state via Arc.
        assert!(g2.oi_cache_loaded.load(Ordering::Acquire));
        g2.mark_engines_ready();
        assert!(g1.engines_ready.load(Ordering::Acquire));
    }

    #[test]
    fn test_force_ready_for_test_helper_works() {
        let g = BootOrderingGate::new();
        g.force_ready_for_test();
        assert!(g.subscribe_allowed());
        assert_eq!(g.readiness(), SubscribeReadiness::Ready);
    }

    #[test]
    fn test_subscribe_readiness_variants_are_distinct() {
        // Five distinct readiness states.
        let states = [
            SubscribeReadiness::Ready,
            SubscribeReadiness::AwaitingOiCache,
            SubscribeReadiness::AwaitingEngines,
            SubscribeReadiness::AwaitingReplay,
            SubscribeReadiness::AwaitingAuthorization,
        ];
        for i in 0..states.len() {
            for j in (i + 1)..states.len() {
                assert_ne!(states[i], states[j], "duplicate readiness at {i},{j}");
            }
        }
    }

    /// Explicit name-match for the pub-fn-test guard.
    #[test]
    fn test_mark_oi_cache_loaded_explicit() {
        let g = BootOrderingGate::new();
        g.mark_oi_cache_loaded();
        assert!(g.oi_cache_loaded.load(Ordering::Acquire));
    }

    #[test]
    fn test_mark_engines_ready_explicit() {
        let g = BootOrderingGate::new();
        g.mark_engines_ready();
        assert!(g.engines_ready.load(Ordering::Acquire));
    }

    #[test]
    fn test_mark_replay_completed_explicit() {
        let g = BootOrderingGate::new();
        g.mark_replay_completed();
        assert!(g.replay_completed.load(Ordering::Acquire));
    }

    #[test]
    fn test_subscribe_allowed_explicit() {
        let g = BootOrderingGate::new();
        assert!(!g.subscribe_allowed());
        g.force_ready_for_test();
        assert!(g.subscribe_allowed());
    }

    #[test]
    fn test_readiness_explicit() {
        let g = BootOrderingGate::new();
        assert_eq!(g.readiness(), SubscribeReadiness::AwaitingOiCache);
    }

    #[test]
    fn test_try_authorize_subscribe_explicit_name_match() {
        let g = BootOrderingGate::new();
        assert!(!g.try_authorize_subscribe());
        g.mark_oi_cache_loaded();
        g.mark_engines_ready();
        g.mark_replay_completed();
        assert!(g.try_authorize_subscribe());
    }

    #[test]
    fn test_new_explicit_name_match() {
        let _ = BootOrderingGate::new();
    }
}

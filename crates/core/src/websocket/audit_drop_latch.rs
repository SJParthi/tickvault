//! Edge latch for a forensic-row drop EPISODE (2026-09-02, audit finding 9).
//!
//! # The gap this closes
//!
//! The socket lifecycle audit path (`WalRingSink::on_lifecycle` →
//! `spawn_live_feed_lifecycle_audit` → `run_ws_event_audit_consumer`) is two
//! bounded `try_send`s deep. Both drop arms COUNTED a lost row
//! (`tv_ws_event_audit_dropped_total{reason}`) and neither LOGGED one, so a
//! wedged consumer cost every socket-lifecycle row of a session with a
//! counter as the only trace — a counter that is not EMF-selected and sits on
//! no dashboard. The order-update socket's own drop arm has logged at
//! `error!` since 2026-07-05 (`ws_audit_loud_drops_guard`); the live-feed
//! forwarder never inherited that, because it was written after the guard.
//!
//! # Why a latch and not an `error!` per drop
//!
//! A full channel is a STATE, not an event. Sixteen sockets reconnecting
//! against a stalled consumer would emit hundreds of drops in a second, and a
//! log line per drop is a flood carrying no new information after the first.
//! The house rule for a structural condition is one page per EPISODE
//! (`stall_scan_is_starved`, the RISK-GAP-03 edge latch): say it ONCE on the
//! rising edge, count every occurrence, and re-arm when a success proves the
//! path is open again.
//!
//! O(1), one atomic per call, no allocation — this sits beside a `try_send`
//! on the socket-event path and must cost nothing that path would notice.

use std::sync::atomic::{AtomicBool, Ordering};

/// One drop episode's edge state. `false` = the path is healthy (or has
/// never dropped), `true` = a drop episode is open.
#[derive(Debug, Default)]
pub struct DropLatch(AtomicBool);

impl DropLatch {
    /// A latch in the healthy state.
    #[must_use]
    pub const fn new() -> Self {
        Self(AtomicBool::new(false))
    }

    /// Record a drop. Returns `true` ONLY on the first drop of an episode —
    /// the rising edge the caller should log. Every later drop in the same
    /// episode returns `false` (count it, do not log it).
    #[must_use]
    // TEST-EXEMPT: pinned by the five audit_drop_latch::tests cases (first drop loud, repeat drops quiet, re-arm on ok)
    pub fn on_drop(&self) -> bool {
        !self.0.swap(true, Ordering::AcqRel)
    }

    /// Record a success. Returns `true` when this success CLOSES an open
    /// episode — the falling edge the caller may log as a recovery. A
    /// success on an already-healthy latch returns `false`.
    #[must_use]
    // TEST-EXEMPT: pinned by the five audit_drop_latch::tests cases (re-arm on ok closes the episode)
    pub fn on_ok(&self) -> bool {
        self.0.swap(false, Ordering::AcqRel)
    }

    /// Whether a drop episode is currently open.
    #[must_use]
    // TEST-EXEMPT: read-only accessor over the same AtomicBool the on_drop/on_ok tests drive
    pub fn is_dropping(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::DropLatch;

    #[test]
    fn a_fresh_latch_is_healthy_and_a_success_on_it_is_not_a_recovery() {
        let latch = DropLatch::new();
        assert!(!latch.is_dropping());
        assert!(
            !latch.on_ok(),
            "a success with no open episode is routine, never a recovery line"
        );
        assert!(!latch.is_dropping());
    }

    #[test]
    fn the_first_drop_of_an_episode_is_the_only_loud_one() {
        let latch = DropLatch::new();
        assert!(latch.on_drop(), "rising edge — log this one");
        assert!(latch.is_dropping());
        for _ in 0..1_000 {
            assert!(
                !latch.on_drop(),
                "every later drop in the same episode is counted, not logged"
            );
        }
        assert!(latch.is_dropping());
    }

    #[test]
    fn a_success_closes_the_episode_exactly_once() {
        let latch = DropLatch::new();
        assert!(latch.on_drop());
        assert!(latch.on_ok(), "falling edge — the recovery line");
        assert!(!latch.is_dropping());
        assert!(!latch.on_ok(), "a second success is routine again");
    }

    #[test]
    fn the_latch_re_arms_so_a_second_episode_is_loud_again() {
        let latch = DropLatch::new();
        assert!(latch.on_drop());
        assert!(!latch.on_drop());
        assert!(latch.on_ok());
        assert!(
            latch.on_drop(),
            "after a recovery the NEXT drop is a new episode and must page again"
        );
        assert!(!latch.on_drop());
    }

    #[test]
    fn default_is_the_healthy_state() {
        let latch = DropLatch::default();
        assert!(!latch.is_dropping());
        assert!(latch.on_drop());
    }
}

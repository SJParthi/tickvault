//! Main-feed losses the live paper fill tap must hear about (plan item LF-6b).
//!
//! The live paper fill chain builds its candles from the ticks the frame drain
//! folds live. Two kinds of main-feed loss never reach that drain and leave no
//! gap the chain could see on its own, so they are published here for the
//! tap (`dhan_feed_stack::live_fill_tap`), which folds them into the
//! `drop_seq` every tapped tick carries:
//!
//! * a frame shed before the drain: the socket-to-drain ring was full
//!   ([`note`] from `WalRingSink::note_ring_shed`), or the ring and the WAL
//!   both refused it ([`note`] just before each `FrameSinkOutcome::WalDropped`
//!   return of `WalRingSink::accept`). Read with
//!   [`main_feed_live_shed_total`];
//! * a main-feed socket that was delivering and is now down (closed, waiting
//!   out a redial, dialling, re-subscribing, or parked). Its instruments get no
//!   ticks while the OTHER sockets keep the stream busy, so no global silence
//!   ever shows, and Dhan does not replay the downtime. Read with
//!   [`main_feed_socket_loss_state`]: the low 32 bits are the mask of main-feed
//!   slots down now (by global connection index), the high 32 bits count every
//!   change of that mask. The tap counts a loss on every change and, while
//!   the mask is not 0, once per second of receipt time, so the chain's
//!   untrusted stretch runs without a hole from the close to the redial.
//!
//! # Complexity
//!
//! [`note`]: one branch and at most one relaxed add, shed arms only.
//! [`ConnectionSupervisor::on_event`]: the state machine's own O(1) step plus,
//! for a main-feed slot whose phase moved into or out of "delivering", one
//! compare-and-swap loop on one word (cold: once per close, dial or
//! re-subscribe, never per frame). The two readers are one relaxed load each.
//! No allocation anywhere.
//!
//! **Honest limits:** a slot whose global index is 32 or more is not tracked
//! (the authorized main-feed sockets are indexes 0 to 4); a socket that was
//! never live (its first dial failed) is not counted as down, because its
//! instruments never fed a chain candle; the shutdown park marks every live
//! main-feed socket down, which taints nothing a running session reads.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use super::{ConnEvent, ConnPhase, ConnectionSupervisor, DhanEndpointType, SupervisorAction};

/// Main-feed frames the live drain never folded live (see the module docs).
static MAIN_FEED_LIVE_SHED: AtomicU64 = AtomicU64::new(0);

/// Main-feed slots down now (low 32 bits) and the count of mask changes
/// (high 32 bits). See the module docs.
static MAIN_FEED_SOCKETS_DOWN: AtomicU64 = AtomicU64::new(0);

/// Counts one main-feed frame that will not reach the live drain. O(1).
#[inline]
pub(super) fn note(main_feed: bool) {
    if main_feed {
        MAIN_FEED_LIVE_SHED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Main-feed frames shed before the live drain over this process's life
/// (ring sheds and WAL-and-ring refusals). One relaxed load, O(1), no
/// allocation; read once per tick by the live paper fill tap.
#[must_use]
#[inline]
pub fn main_feed_live_shed_total() -> u64 {
    MAIN_FEED_LIVE_SHED.load(Ordering::Relaxed)
}

/// The main-feed socket loss word: down-slot mask (low 32 bits) and mask
/// change count (high 32 bits). One relaxed load, O(1), no allocation; read
/// once per tick by the live paper fill tap.
#[must_use]
#[inline]
pub fn main_feed_socket_loss_state() -> u64 {
    MAIN_FEED_SOCKETS_DOWN.load(Ordering::Relaxed)
}

/// Whether a phase means the socket is subscribed and its instruments flow.
#[inline]
const fn delivering(phase: ConnPhase) -> bool {
    matches!(phase, ConnPhase::Subscribing | ConnPhase::Live)
}

/// The next loss word after slot `index` moved from `was` to `now`, or `None`
/// when the mask does not change. Pure, O(1).
///
/// A slot goes down when it leaves "delivering" (Subscribing or Live) and
/// comes back when it reaches Live (subscribe acked or first frame).
#[must_use]
pub(super) fn next_loss_word(word: u64, index: u8, was: ConnPhase, now: ConnPhase) -> Option<u64> {
    let bit = 1u32.checked_shl(u32::from(index))?;
    // APPROVED: deliberate split of the packed word into its two halves.
    #[allow(clippy::cast_possible_truncation)]
    let mask = word as u32;
    let changes = word >> 32;
    let next = if delivering(was) && !delivering(now) {
        mask | bit
    } else if now == ConnPhase::Live && was != ConnPhase::Live {
        mask & !bit
    } else {
        return None;
    };
    if next == mask {
        return None;
    }
    let changes = changes.wrapping_add(1) & u64::from(u32::MAX);
    Some((changes << 32) | u64::from(next))
}

/// Applies one main-feed phase move to the process-wide loss word. Cold.
fn note_phase(index: u8, was: ConnPhase, now: ConnPhase) {
    let _moved = MAIN_FEED_SOCKETS_DOWN.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |w| {
        next_loss_word(w, index, was, now)
    });
}

impl ConnectionSupervisor {
    /// Feeds one event through the state machine and returns the action the
    /// shell must take. Total, allocation-free, O(1). A main-feed slot that
    /// stops or starts delivering is published for the live paper fill tap
    /// (see the module docs).
    pub fn on_event(&mut self, event: ConnEvent, now: Instant) -> SupervisorAction {
        let was = self.phase;
        let action = self.on_event_inner(event, now);
        if self.slot.endpoint == DhanEndpointType::MainFeed && was != self.phase {
            note_phase(self.slot.global_index, was, self.phase);
        }
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::websocket::pool_budget::{ConnectionSlot, DhanAccount};

    fn main_feed(global_index: u8, now: Instant) -> ConnectionSupervisor {
        ConnectionSupervisor::new(
            ConnectionSlot {
                account: DhanAccount::Primary,
                endpoint: DhanEndpointType::MainFeed,
                pool_index: global_index,
                global_index,
            },
            now,
        )
    }

    fn live(s: &mut ConnectionSupervisor, now: Instant) {
        let _ = s.on_event(ConnEvent::BeginDial, now);
        let _ = s.on_event(ConnEvent::DialSucceeded, now);
        let _ = s.on_event(ConnEvent::SubscribeAcked, now);
        assert_eq!(s.phase(), ConnPhase::Live);
    }

    fn down(index: u8) -> bool {
        main_feed_socket_loss_state() & (1u64 << index) != 0
    }

    /// LF-6b review finding 1: one of two live main-feed sockets closes while
    /// the other keeps delivering. The closed slot reads down (and every
    /// change moves the change count) until it is live again; the sibling's
    /// frames change nothing.
    #[test]
    fn test_one_main_feed_socket_down_while_its_sibling_delivers_main_feed_socket_loss_state() {
        let now = Instant::now();
        // Slots 29 and 30: no other test in this crate dials them.
        let mut a = main_feed(29, now);
        let mut b = main_feed(30, now);
        live(&mut a, now);
        live(&mut b, now);
        assert!(!down(29) && !down(30));
        let changes = main_feed_socket_loss_state() >> 32;
        let _ = a.on_event(ConnEvent::Disconnected { code: None }, now);
        assert!(down(29), "a delivering socket that closes is down");
        assert!(!down(30));
        assert_ne!(main_feed_socket_loss_state() >> 32, changes);
        for _ in 0..3 {
            let _ = b.on_event(ConnEvent::FrameReceived, now);
        }
        assert!(down(29), "the sibling's frames do not bring it back");
        // Redial, re-subscribe: down until live again.
        let _ = a.on_event(ConnEvent::BeginDial, now);
        let _ = a.on_event(ConnEvent::DialSucceeded, now);
        assert!(down(29), "dialled but not yet delivering");
        let _ = a.on_event(ConnEvent::SubscribeAcked, now);
        assert!(!down(29), "live again");
        // A depth socket never touches the word.
        let before = main_feed_socket_loss_state() & (1u64 << 31);
        let mut d = ConnectionSupervisor::new(
            ConnectionSlot {
                account: DhanAccount::Primary,
                endpoint: DhanEndpointType::Depth20,
                pool_index: 0,
                global_index: 31,
            },
            now,
        );
        live(&mut d, now);
        let _ = d.on_event(ConnEvent::Disconnected { code: None }, now);
        assert_eq!(main_feed_socket_loss_state() & (1u64 << 31), before);
    }

    #[test]
    fn test_next_loss_word_sets_on_leave_clears_on_live_and_counts_changes() {
        let w = next_loss_word(0, 2, ConnPhase::Live, ConnPhase::Backoff).expect("down");
        assert_eq!(w, (1u64 << 32) | 4);
        assert_eq!(
            next_loss_word(w, 2, ConnPhase::Subscribing, ConnPhase::Backoff),
            None,
            "already down: no change"
        );
        assert_eq!(
            next_loss_word(w, 2, ConnPhase::Backoff, ConnPhase::Dialing),
            None
        );
        assert_eq!(
            next_loss_word(w, 2, ConnPhase::Subscribing, ConnPhase::Live),
            Some(2u64 << 32)
        );
        assert_eq!(
            next_loss_word(0, 2, ConnPhase::Dialing, ConnPhase::Subscribing),
            None,
            "a first dial is never down"
        );
        assert_eq!(
            next_loss_word(0, 40, ConnPhase::Live, ConnPhase::Backoff),
            None
        );
        assert_eq!(
            next_loss_word(u64::MAX << 32, 0, ConnPhase::Live, ConnPhase::Parked),
            Some(1),
            "the change count wraps at 32 bits"
        );
    }

    /// Only MAIN-FEED sheds move the live-fill shed total, and every
    /// WAL-and-ring refusal counts just before it returns.
    #[test]
    fn test_main_feed_live_shed_total_counts_main_feed_sheds_only() {
        let before = main_feed_live_shed_total();
        note(false);
        note(true);
        note(true);
        assert!(main_feed_live_shed_total() >= before + 2);
        let src = include_str!("../pool_supervisor.rs");
        let production = src.split(concat!("#[cfg(", "test)]")).next().unwrap_or(src);
        // Each `WalDropped` return is preceded by the count, on the line
        // directly above it.
        let returns = production
            .matches("return FrameSinkOutcome::WalDropped;")
            .count();
        assert_eq!(returns, 3, "the three WAL-and-ring refusal arms");
        let counted = production
            .lines()
            .zip(production.lines().skip(1))
            .filter(|(a, b)| {
                a.trim() == "live_shed::note(self.endpoint == DhanEndpointType::MainFeed);"
                    && b.trim() == "return FrameSinkOutcome::WalDropped;"
            })
            .count();
        assert_eq!(
            counted, returns,
            "every WAL-and-ring refusal must count for the live fill tap"
        );
        assert!(
            production.contains("live_shed::note(self.endpoint == DhanEndpointType::MainFeed);")
        );
    }
}

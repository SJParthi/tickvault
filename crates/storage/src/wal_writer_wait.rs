//! How the WAL writer thread waits for its next record (audit PR M / M4,
//! 2026-10-26).
//!
//! # The cost this removes
//!
//! The socket read task hands every frame to the WAL writer with a crossbeam
//! `try_send` ([`crate::ws_frame_spill::WsFrameSpill::append_with_seq_at`]).
//! Until this module the writer waited in `recv_timeout`, which REGISTERS the
//! writer as a sleeping receiver. A `try_send` that finds a registered
//! receiver takes the channel's waker mutex and unparks it: a futex wake
//! syscall, on the read task, on almost every frame, because at the live rate
//! (≈ 2,600 frames a second, one every ≈ 400 µs) the writer finishes each
//! batch long before the next frame and goes back to sleep.
//!
//! Measured (2026-10-26):
//!
//! * live, `tv_hot_path_stage_ns_bucket{stage="socket_to_wal"}`, 400 per-minute
//!   scrapes 09:00–15:40 IST: p50 ≤ 4.1 µs, p99 ≤ 32.8 µs, 8,843 of
//!   63,390,377 frames over the 100 µs budget, a > 1 ms frame in 352 of 400
//!   minutes;
//! * the harness `crates/core/tests/socket_to_wal_tail_harness.rs` (x86 dev
//!   container, frames 400 µs apart, 20,000 per run): the WAL hand-off ALONE
//!   with a parked writer cost p50 11.0 µs, p99 40 µs, max 4.0 ms, against a
//!   few hundred ns when the writer is not asleep. The wake is the cost.
//!
//! # What the writer does instead
//!
//! While frames are flowing it does not register: it polls the channel with
//! `try_recv` and sleeps [`WAL_WRITER_HOT_POLL_INTERVAL`] between empty polls.
//! A `try_send` that finds no registered receiver is a few atomics: no lock,
//! no syscall. Only after [`WAL_WRITER_HOT_POLL_WINDOW`] with nothing at all
//! (pre-open, after the close, a dead feed) does it fall back to the
//! registered `recv_timeout`, so an idle process costs nothing and only the
//! first frame after a lull pays one wake.
//!
//! # What it costs
//!
//! * A frame reaches the WAL file up to one poll interval (1 ms, plus timer
//!   slack) later than before. Nothing waits on that: the frame reaches the
//!   drain through the ring independently, and the WAL's own power-loss
//!   window is the sync interval (about a second), not this.
//! * While frames flow the writer wakes at most once a millisecond, where it
//!   was woken once per frame (≈ 2,600 a second) before.
//! * The contract of the old call is kept exactly: `Timeout` only after a
//!   whole `stop_poll` with nothing received, `Disconnected` when every
//!   sender is gone (pinned below).
//!
//! # What it does NOT remove
//!
//! The ring hand-off wakes the frame drain (`frame_ring` `Notify`) when the
//! drain is parked; the same harness measured that at p50 9.7 µs, max 1.4 ms.
//! That wake is what keeps a tick's fold latency low, so it is kept; see the
//! `socket_to_wal` row in CLAUDE.md's O(1) table. Neither does this touch
//! preemption of the reader thread by other threads on its core.

use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, TryRecvError};

/// Sleep between empty polls while frames are flowing.
pub const WAL_WRITER_HOT_POLL_INTERVAL: Duration = Duration::from_millis(1); // APPROVED: this IS the named constant the rule asks for

/// How long the writer keeps polling after its last record before it
/// registers a blocking wait. Fifty polls: far longer than any in-session gap
/// between frames, short enough that an idle process stops polling at once.
pub const WAL_WRITER_HOT_POLL_WINDOW: Duration = Duration::from_millis(50); // APPROVED: this IS the named constant the rule asks for

/// The writer's next move while its channel is empty, `waited` into a wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleStep {
    /// Sleep this long without registering, then poll again.
    Poll(Duration),
    /// Register and block for at most this long.
    Block(Duration),
    /// `stop_poll` has passed with nothing received.
    TimedOut,
}

/// Pure: the step for an empty channel after `waited` of a `stop_poll` wait.
#[must_use]
pub fn idle_step(waited: Duration, stop_poll: Duration) -> IdleStep {
    if waited >= stop_poll {
        IdleStep::TimedOut
    } else if waited < WAL_WRITER_HOT_POLL_WINDOW {
        IdleStep::Poll(WAL_WRITER_HOT_POLL_INTERVAL.min(stop_poll - waited))
    } else {
        IdleStep::Block(stop_poll - waited)
    }
}

/// Waits for the writer's next record with the same contract as
/// `rx.recv_timeout(stop_poll)`, without registering as a sleeping receiver
/// for the first [`WAL_WRITER_HOT_POLL_WINDOW`] of the wait.
///
/// Runs on the WAL writer THREAD only, never on a socket read task. O(1) per
/// poll, no allocation.
///
/// # Errors
/// `Timeout` when nothing arrived for `stop_poll`; `Disconnected` when every
/// sender is gone and the channel is empty.
pub fn next_record<T>(rx: &Receiver<T>, stop_poll: Duration) -> Result<T, RecvTimeoutError> {
    let started = Instant::now();
    loop {
        match rx.try_recv() {
            Ok(record) => return Ok(record),
            Err(TryRecvError::Disconnected) => return Err(RecvTimeoutError::Disconnected),
            Err(TryRecvError::Empty) => {}
        }
        match idle_step(started.elapsed(), stop_poll) {
            IdleStep::Poll(nap) => thread::sleep(nap),
            IdleStep::Block(rest) => return rx.recv_timeout(rest),
            IdleStep::TimedOut => return Err(RecvTimeoutError::Timeout),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STOP: Duration = Duration::from_millis(200);

    #[test]
    fn test_idle_step_polls_inside_the_hot_window() {
        assert_eq!(
            idle_step(Duration::ZERO, STOP),
            IdleStep::Poll(WAL_WRITER_HOT_POLL_INTERVAL)
        );
        assert_eq!(
            idle_step(Duration::from_millis(49), STOP),
            IdleStep::Poll(WAL_WRITER_HOT_POLL_INTERVAL)
        );
    }

    #[test]
    fn test_idle_step_blocks_for_the_rest_after_the_hot_window() {
        assert_eq!(
            idle_step(WAL_WRITER_HOT_POLL_WINDOW, STOP),
            IdleStep::Block(Duration::from_millis(150))
        );
        assert_eq!(
            idle_step(Duration::from_millis(199), STOP),
            IdleStep::Block(Duration::from_millis(1))
        );
    }

    #[test]
    fn test_idle_step_times_out_at_and_past_stop_poll() {
        assert_eq!(idle_step(STOP, STOP), IdleStep::TimedOut);
        assert_eq!(idle_step(Duration::from_secs(9), STOP), IdleStep::TimedOut);
    }

    #[test]
    fn test_idle_step_never_naps_past_a_short_stop_poll() {
        let short = Duration::from_micros(300);
        assert_eq!(
            idle_step(Duration::from_micros(100), short),
            IdleStep::Poll(Duration::from_micros(200))
        );
    }

    // Regression: 2026-10-26 (audit PR M / M4) — the writer waited in a
    // registered `recv_timeout`, so every frame that found it asleep paid a
    // futex wake on the socket read task. The writer loop must wait through
    // `next_record`, never a bare `recv_timeout` (bite-checked by restoring
    // the old call: this test fails).
    #[test]
    fn test_regression_wal_writer_loop_waits_through_next_record() {
        let src = include_str!("ws_frame_spill.rs");
        let from = src.find("fn writer_loop(").expect("writer_loop");
        let len = src[from..]
            .find("fn open_segment_resilient")
            .expect("writer_loop is followed by open_segment_resilient");
        let body = &src[from..from + len];
        assert!(
            body.contains("wal_writer_wait::next_record(rx, WAL_WRITER_STOP_POLL)"),
            "writer_loop must wait through wal_writer_wait::next_record"
        );
        assert!(
            !body.contains("rx.recv_timeout("),
            "a registered recv_timeout in writer_loop puts a futex wake on every frame"
        );
    }

    // While frames flow the wait never registers, so the record is taken by a
    // poll within one interval of the send although nothing unparked the waiter.
    #[test]
    fn test_next_record_takes_a_record_by_polling_inside_the_hot_window() {
        let (tx, rx) = crossbeam_channel::bounded::<u32>(4);
        let waiter = thread::spawn(move || next_record(&rx, STOP));
        // Inside the hot window the waiter is between polls, never registered.
        thread::sleep(Duration::from_millis(10));
        // crossbeam exposes no "is a receiver registered" probe; the closest
        // observable is that the record is taken by a POLL, i.e. within one
        // poll interval of the send even though nothing unparked the waiter.
        tx.send(7).expect("send");
        assert_eq!(waiter.join().expect("join"), Ok(7));
        // And the wait used the polling arm: inside the window `idle_step`
        // never asks to block.
        for ms in 0..50 {
            assert!(matches!(
                idle_step(Duration::from_millis(ms), STOP),
                IdleStep::Poll(_)
            ));
        }
    }

    #[test]
    fn test_next_record_returns_a_queued_record_at_once() {
        let (tx, rx) = crossbeam_channel::bounded::<u32>(4);
        tx.send(1).expect("send");
        let t0 = Instant::now();
        assert_eq!(next_record(&rx, STOP), Ok(1));
        assert!(t0.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn test_next_record_times_out_only_after_stop_poll() {
        let (_tx, rx) = crossbeam_channel::bounded::<u32>(4);
        let t0 = Instant::now();
        assert_eq!(
            next_record(&rx, Duration::from_millis(80)),
            Err(RecvTimeoutError::Timeout)
        );
        assert!(t0.elapsed() >= Duration::from_millis(80));
    }

    #[test]
    fn test_next_record_reports_disconnected_when_senders_are_gone() {
        let (tx, rx) = crossbeam_channel::bounded::<u32>(4);
        drop(tx);
        assert_eq!(next_record(&rx, STOP), Err(RecvTimeoutError::Disconnected));
    }

    #[test]
    fn test_next_record_takes_a_record_sent_during_the_blocking_arm() {
        let (tx, rx) = crossbeam_channel::bounded::<u32>(4);
        let waiter = thread::spawn(move || next_record(&rx, Duration::from_secs(5)));
        // Past the hot window: the waiter is registered and must be woken.
        thread::sleep(WAL_WRITER_HOT_POLL_WINDOW + Duration::from_millis(40));
        let t0 = Instant::now();
        tx.send(9).expect("send");
        assert_eq!(waiter.join().expect("join"), Ok(9));
        assert!(t0.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn test_next_record_disconnect_during_the_blocking_arm() {
        let (tx, rx) = crossbeam_channel::bounded::<u32>(4);
        let waiter = thread::spawn(move || next_record(&rx, Duration::from_secs(5)));
        thread::sleep(WAL_WRITER_HOT_POLL_WINDOW + Duration::from_millis(40));
        drop(tx);
        assert_eq!(
            waiter.join().expect("join"),
            Err(RecvTimeoutError::Disconnected)
        );
    }
}

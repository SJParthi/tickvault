//! Loom model of the REAL activity-watchdog progress check (audit H3,
//! rewritten 2026-10-06).
//!
//! Until 2026-10-06 this file modelled a hand-written copy: an `AtomicBool`
//! standing in for `tokio::sync::Notify` and a compare-and-swap "fire at most
//! once" guard that the production watchdog never had (it fires by calling
//! `notify_one` and returning, so it cannot fire twice). A change to the real
//! code could not fail it. Those three models are deleted.
//!
//! What crosses threads in production is one counter: the order-update read
//! loop bumps it per frame (`note_activity`) and the watchdog task reads it
//! once per poll (`ProgressProbe::advanced`, the exact call inside
//! `ActivityWatchdog::run`). Under the crate's `loom` feature that counter is
//! loom's `AtomicU64` (`crate::sync`), so this file drives those two real
//! functions through every interleaving loom explores.
//!
//! Not modelled, stated plainly: the tokio timer and `Notify` (loom cannot run
//! a tokio runtime). The fire path is covered by the paused-clock unit tests in
//! `activity_watchdog.rs` (`watchdog_fires_on_sustained_silence_past_threshold`
//! and its two siblings).
//!
//! What it proves, in every interleaving:
//!
//! - a poll never reports progress that was not made: the advances a probe
//!   reports are at most the frames recorded;
//! - once the reader has stopped (joined), one poll catches up to EVERY frame,
//!   and the poll after it reports no progress: frames counted once are never
//!   counted again (a probe that re-reported them would hold off the watchdog
//!   on a dead socket);
//! - both outcomes of the race are explored: a concurrent poll sometimes sees
//!   the frames and sometimes does not (anti-vacuity, see the loom note).
//!
//! Run with:
//!   cargo test -p tickvault-core --features loom --test loom_activity_watchdog
//!
//! PATTERN MODEL (audit H3, 2026-10-05): this file models the pattern, not
//! production code, so a change to the real code cannot fail it. The real
//! `ActivityWatchdog::run` is a tokio task (an interval timer and a `Notify`),
//! which loom cannot run; the only state it shares with the reader is one
//! `Relaxed` counter. `real_code_loom_guard.rs` caps such files and may only
//! shrink.

#[cfg(feature = "loom")]
mod loom_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use loom::sync::Arc;
    use loom::sync::atomic::AtomicU64;
    use loom::thread;
    use tickvault_core::websocket::activity_watchdog::{ProgressProbe, note_activity};

    /// Frames the reader records in each model.
    const FRAMES: u64 = 2;

    /// The reader records frames on its own thread while the watchdog polls.
    ///
    /// The polls run on the MAIN thread, the shape `loom_ghost_register.rs`
    /// measured as necessary there: with its take on a spawned thread loom
    /// explored a single execution (Assumed cause: loom 0.7 tracks only the
    /// last access to each atomic). Measured 2026-10-06 for THIS model: the
    /// mirror shape (polls spawned, frames on main) also explored both
    /// outcomes, in 195 executions, so the limitation does not bite here; the
    /// shape is kept for consistency, and the anti-vacuity asserts below fail
    /// if a loom change ever collapses this model to one outcome.
    #[test]
    fn a_poll_never_reports_progress_it_did_not_see_and_never_counts_a_frame_twice() {
        static POLL_SAW_PROGRESS: AtomicUsize = AtomicUsize::new(0);
        static POLL_SAW_NONE: AtomicUsize = AtomicUsize::new(0);

        loom::model(|| {
            let counter = Arc::new(AtomicU64::new(0));
            let mut probe = ProgressProbe::start(&counter);

            let reader_counter = Arc::clone(&counter);
            let reader = thread::spawn(move || {
                for _ in 0..FRAMES {
                    note_activity(&reader_counter);
                }
            });

            // The watchdog polls three times while the reader records two
            // frames, so "advances <= frames" is not true by construction.
            let mut advances = 0_u64;
            for _ in 0..3 {
                if probe.advanced(&counter) {
                    advances += 1;
                    POLL_SAW_PROGRESS.fetch_add(1, Ordering::Relaxed);
                } else {
                    POLL_SAW_NONE.fetch_add(1, Ordering::Relaxed);
                }
                // Coherence on one atomic: a later poll never sees less.
                assert!(probe.last_seen() <= FRAMES);
            }
            assert!(
                advances <= FRAMES,
                "{advances} advances for {FRAMES} frames"
            );

            reader.join().expect("reader thread");

            // The join orders every bump before this poll: it must catch up
            // to all of them in one step...
            let before = probe.last_seen();
            let caught_up = probe.advanced(&counter);
            assert_eq!(probe.last_seen(), FRAMES, "probe missed a frame");
            // ...reporting progress exactly when the racing polls had not
            // already reached the final value...
            assert_eq!(caught_up, before != FRAMES);
            // ...and with no new frame, no poll may report progress again.
            assert!(
                !probe.advanced(&counter),
                "a frame already counted read as progress a second time"
            );
            assert!(!probe.advanced(&counter));
        });

        // ANTI-VACUITY: loom must have explored both outcomes of a racing poll.
        assert!(
            POLL_SAW_PROGRESS.load(Ordering::Relaxed) > 0,
            "no execution let a racing poll see a frame"
        );
        assert!(
            POLL_SAW_NONE.load(Ordering::Relaxed) > 0,
            "no execution let a racing poll miss the frames"
        );
    }

    /// No frame, no progress: a silent reader never moves the probe, in any
    /// interleaving of the probe's start and its polls.
    #[test]
    fn a_silent_connection_never_reads_as_progress() {
        loom::model(|| {
            let counter = Arc::new(AtomicU64::new(0));
            let poll_counter = Arc::clone(&counter);
            let watchdog = thread::spawn(move || {
                let mut probe = ProgressProbe::start(&poll_counter);
                (probe.advanced(&poll_counter), probe.advanced(&poll_counter))
            });
            let (first, second) = watchdog.join().expect("watchdog thread");
            assert!(!first && !second, "progress reported on a silent socket");
        });
    }
}

// Standard (non-loom) stress test on the same real functions, so the file
// runs in the ordinary suite too.
#[cfg(not(feature = "loom"))]
mod std_tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::thread;

    use tickvault_core::websocket::activity_watchdog::{ProgressProbe, note_activity};

    /// A reader records 100,000 frames while the watchdog polls in a loop.
    /// The probe never runs ahead of the frames, and after the reader stops
    /// one poll catches up to the exact total and the next reports nothing.
    #[test]
    fn stress_probe_tracks_a_busy_reader_exactly() {
        const FRAMES: u64 = 100_000;
        let counter = Arc::new(AtomicU64::new(0));
        let reader_counter = Arc::clone(&counter);
        let reader = thread::spawn(move || {
            for _ in 0..FRAMES {
                note_activity(&reader_counter);
            }
        });
        let mut probe = ProgressProbe::start(&counter);
        let mut previous = probe.last_seen();
        while !reader.is_finished() {
            if probe.advanced(&counter) {
                assert!(probe.last_seen() > previous, "the probe went backwards");
                previous = probe.last_seen();
            }
            assert!(probe.last_seen() <= FRAMES);
        }
        reader.join().expect("reader thread");
        let _ = probe.advanced(&counter);
        assert_eq!(probe.last_seen(), FRAMES);
        assert!(!probe.advanced(&counter));
    }
}

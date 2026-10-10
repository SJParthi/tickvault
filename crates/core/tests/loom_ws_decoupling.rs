//! Loom model of the REAL frame-ring budget the socket readers share (audit
//! H3, rewritten 2026-10-06).
//!
//! Until 2026-10-06 this file modelled a hand-written copy of the WS -> WAL
//! hand-off: a `Mutex<Vec<u8>>` as the WAL and a `Mutex<Option<u8>>` as a
//! capacity-one channel. Neither exists in production (the WAL is the storage
//! crate's queued writer, the ring a bounded `tokio::sync::mpsc` the readers
//! `try_send` into, and the reader takes no lock), so a change to the real
//! code could not fail it. Those models are deleted.
//!
//! The lock-free structure every socket reader shares on that hand-off is the
//! ring's per-class budget, `RingByteBudget`: each reader reserves bytes and a
//! slot before `try_send` (`WalRingSink::accept`), and the drain releases them
//! as frames leave the ring. Under the crate's `loom` feature its two counters
//! are loom's `AtomicUsize` (`crate::sync`), so this file drives the exact
//! `try_reserve_detailed` and `release` methods through every interleaving.
//!
//! Not modelled, stated plainly: the tokio channel and the WAL writer (loom
//! cannot run them). That the reader never awaits on either is pinned by the
//! `FrameSink::accept` signature (no `async`) and the pool supervisor tests.
//!
//! What it proves, in every interleaving:
//!
//! - concurrent readers never reserve past the byte cap or the slot cap;
//! - a reservation refused for bytes hands its slot back (a leak would shrink
//!   the class's slot budget a frame at a time until it refused everything);
//! - a reservation racing the drain's release ends with the counters exactly
//!   matching the frames that hold a reservation;
//! - both outcomes of each race are explored (anti-vacuity, see the loom note).
//!
//! Run with:
//!   cargo test -p tickvault-core --features loom --test loom_ws_decoupling
//!
//! PATTERN MODEL (audit H3, 2026-10-05): this file models the pattern, not
//! production code, so a change to the real code cannot fail it. The real
//! path is a tokio `mpsc` frame ring plus the WAL writer thread, which loom
//! cannot run. `real_code_loom_guard.rs` caps such files and may only shrink.

#[cfg(feature = "loom")]
mod loom_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use loom::sync::Arc;
    use loom::thread;
    use tickvault_core::websocket::pool_supervisor::{RingByteBudget, RingReserve};

    /// Two readers race for a byte budget only one of them fits in.
    ///
    /// The second reservation runs on the MAIN thread, the shape
    /// `loom_ghost_register.rs` measured as necessary there (with its take on
    /// a spawned thread loom explored one execution; Assumed cause: loom 0.7
    /// tracks only the last access to each atomic). Measured 2026-10-06 for
    /// THIS model: with both reservations on spawned threads loom still
    /// explored both winners (both are compare-and-swap loops), so the
    /// limitation does not bite here; the anti-vacuity asserts below fail if
    /// a loom change ever collapses it to one winner.
    #[test]
    fn two_readers_never_reserve_past_the_byte_cap_and_the_loser_returns_its_slot() {
        static SPAWNED_WON: AtomicUsize = AtomicUsize::new(0);
        static MAIN_WON: AtomicUsize = AtomicUsize::new(0);

        loom::model(|| {
            // 10 bytes, 6-byte frames: exactly one fits.
            let budget = Arc::new(RingByteBudget::with_slot_cap(10, 4));
            let reader_budget = Arc::clone(&budget);
            let reader = thread::spawn(move || reader_budget.try_reserve_detailed(6));
            let main = budget.try_reserve_detailed(6);
            let spawned = reader.join().expect("reader thread");

            let granted = [spawned, main]
                .iter()
                .filter(|r| **r == RingReserve::Granted)
                .count();
            assert_eq!(granted, 1, "spawned {spawned:?}, main {main:?}");
            for outcome in [spawned, main] {
                assert!(
                    matches!(outcome, RingReserve::Granted | RingReserve::BytesFull),
                    "unexpected {outcome:?}"
                );
            }
            assert_eq!(budget.resident(), 6, "bytes held past the one grant");
            assert_eq!(
                budget.slots_resident(),
                1,
                "the refused reservation kept its slot"
            );
            if spawned == RingReserve::Granted {
                SPAWNED_WON.fetch_add(1, Ordering::Relaxed);
            } else {
                MAIN_WON.fetch_add(1, Ordering::Relaxed);
            }

            // The drain releases the one frame: the class is empty again.
            budget.release(6);
            assert_eq!(budget.resident(), 0);
            assert_eq!(budget.slots_resident(), 0);
        });

        assert!(
            SPAWNED_WON.load(Ordering::Relaxed) > 0,
            "no execution let the spawned reader win"
        );
        assert!(
            MAIN_WON.load(Ordering::Relaxed) > 0,
            "no execution let the main reader win"
        );
    }

    /// Two readers race for the last frame slot; bytes are plentiful.
    #[test]
    fn two_readers_never_reserve_past_the_slot_cap() {
        static SPAWNED_WON: AtomicUsize = AtomicUsize::new(0);
        static MAIN_WON: AtomicUsize = AtomicUsize::new(0);

        loom::model(|| {
            let budget = Arc::new(RingByteBudget::with_slot_cap(1_000, 1));
            let reader_budget = Arc::clone(&budget);
            let reader = thread::spawn(move || reader_budget.try_reserve_detailed(5));
            let main = budget.try_reserve_detailed(7);
            let spawned = reader.join().expect("reader thread");

            match (spawned, main) {
                (RingReserve::Granted, RingReserve::SlotsFull) => {
                    assert_eq!(budget.resident(), 5);
                    SPAWNED_WON.fetch_add(1, Ordering::Relaxed);
                }
                (RingReserve::SlotsFull, RingReserve::Granted) => {
                    assert_eq!(budget.resident(), 7);
                    MAIN_WON.fetch_add(1, Ordering::Relaxed);
                }
                other => panic!("slot cap 1 broken: {other:?}"),
            }
            assert_eq!(budget.slots_resident(), 1);
        });

        assert!(SPAWNED_WON.load(Ordering::Relaxed) > 0);
        assert!(MAIN_WON.load(Ordering::Relaxed) > 0);
    }

    /// A reader reserves while the drain releases the frame that fills the
    /// budget. Whichever wins, the counters end equal to what is held.
    #[test]
    fn a_reserve_racing_the_drain_release_leaves_exact_counters() {
        static GRANTED: AtomicUsize = AtomicUsize::new(0);
        static REFUSED: AtomicUsize = AtomicUsize::new(0);

        loom::model(|| {
            let budget = Arc::new(RingByteBudget::with_slot_cap(10, 4));
            // One 6-byte frame already sits in the ring.
            assert_eq!(budget.try_reserve_detailed(6), RingReserve::Granted);

            let drain_budget = Arc::clone(&budget);
            let drain = thread::spawn(move || drain_budget.release(6));

            let reserved = budget.try_reserve_detailed(6);
            drain.join().expect("drain thread");

            match reserved {
                RingReserve::Granted => {
                    assert_eq!(budget.resident(), 6);
                    assert_eq!(budget.slots_resident(), 1);
                    GRANTED.fetch_add(1, Ordering::Relaxed);
                }
                RingReserve::BytesFull => {
                    assert_eq!(budget.resident(), 0);
                    assert_eq!(
                        budget.slots_resident(),
                        0,
                        "the refused reservation kept its slot"
                    );
                    REFUSED.fetch_add(1, Ordering::Relaxed);
                }
                RingReserve::SlotsFull => panic!("slot cap 4 cannot be full"),
            }
        });

        assert!(
            GRANTED.load(Ordering::Relaxed) > 0,
            "no execution let the release land first"
        );
        assert!(
            REFUSED.load(Ordering::Relaxed) > 0,
            "no execution let the reserve land first"
        );
    }
}

// Standard (non-loom) stress test on the same real budget, so the file runs
// in the ordinary suite too.
#[cfg(not(feature = "loom"))]
mod std_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    use tickvault_core::websocket::pool_supervisor::{RingByteBudget, RingReserve};

    /// Eight readers reserve and release 10,000 frames each against a budget
    /// far smaller than the demand. Every granted reservation sees the
    /// budget within its caps, and once every frame is released the class
    /// holds nothing: no byte and no slot leaked through a refusal.
    #[test]
    fn stress_readers_and_releases_never_breach_or_leak_the_budget() {
        const CAP: usize = 64;
        const SLOTS: usize = 3;
        let budget = Arc::new(RingByteBudget::with_slot_cap(CAP, SLOTS));
        let granted = Arc::new(AtomicUsize::new(0));
        let refused = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::with_capacity(8);
        for reader in 0..8_usize {
            let budget = Arc::clone(&budget);
            let granted = Arc::clone(&granted);
            let refused = Arc::clone(&refused);
            handles.push(thread::spawn(move || {
                for i in 0..10_000_usize {
                    let len = 1 + (reader * 7 + i) % 40;
                    match budget.try_reserve_detailed(len) {
                        RingReserve::Granted => {
                            assert!(budget.resident() <= CAP);
                            assert!(budget.slots_resident() <= SLOTS);
                            granted.fetch_add(1, Ordering::Relaxed);
                            budget.release(len);
                        }
                        RingReserve::BytesFull | RingReserve::SlotsFull => {
                            refused.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }));
        }
        for h in handles {
            h.join().expect("reader thread");
        }
        assert_eq!(budget.resident(), 0, "bytes leaked");
        assert_eq!(budget.slots_resident(), 0, "slots leaked");
        assert_eq!(
            granted.load(Ordering::Relaxed) + refused.load(Ordering::Relaxed),
            80_000
        );
        assert!(granted.load(Ordering::Relaxed) > 0);
    }
}

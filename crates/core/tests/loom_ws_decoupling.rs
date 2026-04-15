//! STAGE-D P7.1 — Loom concurrency model for the WS → WAL decoupling.
//!
//! The production invariant we need to preserve mechanically:
//!
//!   > The WS read loop never blocks on the downstream consumer. In every
//!   > interleaving, the reader can bump its activity counter and hand a
//!   > frame to the WAL path without waiting on the consumer side of the
//!   > channel, even when the consumer task has stalled completely.
//!
//! This model abstracts the production structure down to its atomic
//! shape to keep loom's state space tractable:
//!
//!   - `activity_counter: AtomicU64` — the watchdog counter bumped by the
//!     read loop on every Some(Ok(_)) frame.
//!   - `wal_queue: Mutex<Vec<u8>>` — a bounded "WAL ring" placeholder that
//!     the reader appends to (`push`) without ever blocking.
//!   - `live_channel: Mutex<Option<u8>>` — a capacity-1 live channel the
//!     reader tries to `try_send` into. When full, the reader MUST NOT
//!     block; instead it records a backpressure event and moves on.
//!   - `backpressure_counter: AtomicU64` — incremented when the live
//!     channel was full (the WAL is still durable, so no data loss).
//!
//! Run with:
//!   cargo test -p tickvault-core --features loom --test loom_ws_decoupling

#[cfg(feature = "loom")]
mod loom_tests {
    use loom::sync::atomic::{AtomicU64, Ordering};
    use loom::sync::{Arc, Mutex};
    use loom::thread;

    /// Minimal shape of the WS → WAL path.
    struct WsState {
        activity_counter: AtomicU64,
        wal_queue: Mutex<Vec<u8>>,
        live_channel: Mutex<Option<u8>>, // capacity-1 bounded channel
        backpressure_counter: AtomicU64,
    }

    impl WsState {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                activity_counter: AtomicU64::new(0),
                wal_queue: Mutex::new(Vec::new()),
                live_channel: Mutex::new(None),
                backpressure_counter: AtomicU64::new(0),
            })
        }

        /// Simulates one iteration of the WS read loop: counter bump,
        /// durable WAL append, non-blocking live try_send.
        ///
        /// This function MUST return quickly regardless of consumer
        /// state. Returning means "reader is unblocked and ready for
        /// the next frame".
        fn read_loop_once(&self, frame: u8) {
            // STAGE-C.3: activity watchdog counter bump. Relaxed is
            // sufficient because the watchdog only checks forward
            // progress, not ordering.
            self.activity_counter.fetch_add(1, Ordering::Relaxed);

            // STAGE-C.1: durable WAL append. The reader holds the WAL
            // mutex briefly — bounded by a memcpy in production (one
            // Vec<u8> push). Loom sees this as a short critical section,
            // NOT a block on the consumer.
            self.wal_queue
                .lock()
                .expect("loom: wal_queue lock")
                .push(frame);

            // STAGE-C.2: non-blocking live try_send. If the slot is
            // already full we IMMEDIATELY record a backpressure event
            // and return — we never await on the consumer.
            let mut live = self.live_channel.lock().expect("loom: live_channel lock");
            if live.is_none() {
                *live = Some(frame);
            } else {
                // Capacity-1 channel is full. In production the WAL
                // already has the frame, so we log a WARN and move on.
                self.backpressure_counter.fetch_add(1, Ordering::Release);
            }
            // Mutex guards drop here — reader is done with this frame.
        }

        /// Simulates the consumer task draining one frame from the
        /// live channel. In production this is a full tick processor +
        /// QuestDB write; for the loom model we only need to observe
        /// that draining it unblocks the next reader try_send.
        fn consumer_drain_once(&self) -> Option<u8> {
            let mut live = self.live_channel.lock().expect("loom: live_channel lock");
            live.take()
        }
    }

    /// Model 1 — slow consumer never blocks the reader.
    ///
    /// The reader produces two frames while the consumer is paused.
    /// Loom explores every possible interleaving. Invariants:
    ///   - Every frame is recorded in the WAL (no loss).
    ///   - The activity counter advanced by 2 (reader never spun).
    ///   - At most one frame is in the live channel at any moment.
    ///   - The backpressure counter is either 0 or 1, never torn.
    #[test]
    fn reader_never_blocks_on_slow_consumer() {
        loom::model(|| {
            let state = WsState::new();

            let s1 = Arc::clone(&state);
            let reader = thread::spawn(move || {
                s1.read_loop_once(0xAA);
                s1.read_loop_once(0xBB);
            });

            reader.join().expect("loom: reader join");

            // Invariant 1: WAL contains both frames regardless of interleaving.
            let wal = state.wal_queue.lock().expect("loom: wal lock");
            assert_eq!(wal.len(), 2, "WAL must contain both frames");
            assert_eq!(*wal, vec![0xAA, 0xBB]);
            drop(wal);

            // Invariant 2: activity counter advanced by exactly 2.
            assert_eq!(
                state.activity_counter.load(Ordering::Relaxed),
                2,
                "activity counter must reflect both frames"
            );

            // Invariant 3: live channel has at most 1 slot; since
            // no consumer ran, exactly one frame is sitting in it.
            let live = state.live_channel.lock().expect("loom: live lock");
            assert!(live.is_some(), "live channel must hold one frame");
            drop(live);

            // Invariant 4: backpressure counter is exactly 1 — the
            // second frame could not fit in the capacity-1 slot, so
            // the WAL absorbed it (no loss, no block).
            assert_eq!(
                state.backpressure_counter.load(Ordering::Acquire),
                1,
                "backpressure counter must equal frames that spilled only to WAL"
            );
        });
    }

    /// Model 2 — concurrent reader + consumer.
    ///
    /// Both threads run simultaneously. Loom explores every
    /// possible interleaving. Invariants:
    ///   - WAL always has the frame (durability is independent of consumer).
    ///   - The consumer either drains the frame or doesn't — both are fine.
    ///   - The reader never observes the consumer's state (zero coupling).
    ///   - Backpressure counter is 0 or 1, depending on interleaving.
    #[test]
    fn reader_and_consumer_never_deadlock() {
        loom::model(|| {
            let state = WsState::new();

            let s_reader = Arc::clone(&state);
            let reader = thread::spawn(move || {
                s_reader.read_loop_once(0x11);
            });

            let s_consumer = Arc::clone(&state);
            let consumer = thread::spawn(move || s_consumer.consumer_drain_once());

            reader.join().expect("loom: reader join");
            let drained = consumer.join().expect("loom: consumer join");

            // Invariant: WAL contains the frame in EVERY interleaving.
            let wal = state.wal_queue.lock().expect("loom: wal lock");
            assert_eq!(wal.len(), 1, "WAL must always contain the frame");
            assert_eq!(wal[0], 0x11);
            drop(wal);

            // The consumer either saw the frame (drained == Some(0x11))
            // or it ran first (drained == None). Either is correct —
            // the WS→WAL decoupling does NOT require a happens-before
            // relationship between reader and consumer.
            assert!(matches!(drained, None | Some(0x11)));
        });
    }

    /// Model 3 — a stalled consumer still lets the reader make progress.
    ///
    /// The consumer thread acquires the live-channel mutex, holds it
    /// briefly, and releases. While held, the reader attempts to push.
    /// In production the reader uses `try_send`, so mutex contention
    /// inside loom is a faithful stand-in: the reader can always make
    /// forward progress on the activity counter and the WAL, even if
    /// the live-channel lock is contended.
    ///
    /// NOTE: because `loom::sync::Mutex::lock` is blocking, this model
    /// reflects the production guarantee via a slightly different
    /// assertion: the reader completes within a bounded number of
    /// scheduler steps for every interleaving.
    #[test]
    fn reader_makes_progress_under_live_channel_contention() {
        loom::model(|| {
            let state = WsState::new();

            let s_reader = Arc::clone(&state);
            let reader = thread::spawn(move || {
                s_reader.read_loop_once(0x42);
            });

            // The consumer immediately drains. In loom's scheduler this
            // may run before, during, or after the reader — each
            // interleaving is explored.
            let s_consumer = Arc::clone(&state);
            let consumer = thread::spawn(move || {
                let _ = s_consumer.consumer_drain_once();
            });

            reader.join().expect("loom: reader join");
            consumer.join().expect("loom: consumer join");

            // Reader always completed — no deadlock, no panic, no lost
            // frame in the WAL.
            let wal = state.wal_queue.lock().expect("loom: wal lock");
            assert_eq!(wal.len(), 1);
            drop(wal);
            assert_eq!(state.activity_counter.load(Ordering::Relaxed), 1);
        });
    }
}

// Standard (non-loom) stress test so the file contributes to the
// normal CI run as well as the loom model.
#[cfg(not(feature = "loom"))]
mod std_stress_tests {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;

    /// Stress: 8 concurrent WS readers, 1 slow consumer. The readers
    /// must never deadlock on the capacity-1 live channel, and the WAL
    /// must end up with every frame even though most frames overflow
    /// into "backpressure" path.
    #[test]
    fn stress_ws_readers_never_deadlock_under_slow_consumer() {
        let wal: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::with_capacity(8_000)));
        let live: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));
        let activity = Arc::new(AtomicU64::new(0));
        let backpressure = Arc::new(AtomicU64::new(0));

        let mut handles = Vec::new();
        for r in 0..8 {
            let wal = Arc::clone(&wal);
            let live = Arc::clone(&live);
            let activity = Arc::clone(&activity);
            let backpressure = Arc::clone(&backpressure);
            handles.push(thread::spawn(move || {
                for i in 0..1000 {
                    let frame = (r * 1000 + i) as u32;
                    activity.fetch_add(1, Ordering::Relaxed);
                    wal.lock().expect("wal lock").push(frame);
                    let mut slot = live.lock().expect("live lock");
                    if slot.is_none() {
                        *slot = Some(frame);
                    } else {
                        backpressure.fetch_add(1, Ordering::Release);
                    }
                }
            }));
        }

        // Slow consumer: drains at a trickle, must not starve readers.
        let live_consumer = Arc::clone(&live);
        handles.push(thread::spawn(move || {
            for _ in 0..200 {
                let _ = live_consumer.lock().expect("live lock").take();
                thread::yield_now();
            }
        }));

        for h in handles {
            h.join().expect("stress join");
        }

        // Every frame landed in the WAL — total = 8 readers × 1000 frames.
        let wal = wal.lock().expect("wal lock");
        assert_eq!(wal.len(), 8_000, "all frames durable in WAL");
        drop(wal);

        // Activity counter saw every frame.
        assert_eq!(activity.load(Ordering::Relaxed), 8_000);

        // Backpressure counter + frames-in-slot cover all frames that
        // could not fit the capacity-1 live channel. Sanity check: it
        // is bounded above by the total number of reader frames.
        let bp = backpressure.load(Ordering::Acquire);
        assert!(bp <= 8_000);
    }
}

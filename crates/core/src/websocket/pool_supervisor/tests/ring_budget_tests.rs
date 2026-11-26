//! Byte and slot reservation behavior of the live frame ring.

use super::*;

#[test]
fn test_ring_byte_budget_cap_reports_the_configured_ceiling() {
    // `cap()` is what the lane's boot line reports as `ring_max_bytes`.
    // It exists so the operator sees the queue's size in the unit that
    // actually runs out — reporting only the 65,536 frame count is how a
    // 16 GiB ceiling hid behind a number that looks modest.
    let b = RingByteBudget::new(256 * 1024 * 1024);
    assert_eq!(b.cap(), 256 * 1024 * 1024);
    assert_eq!(b.resident(), 0, "a fresh budget holds nothing");
    assert!(b.try_reserve(1_000));
    assert_eq!(
        b.cap(),
        256 * 1024 * 1024,
        "cap is the CEILING and must not move as frames come and go — only \
         resident() tracks occupancy"
    );
}

#[test]
fn test_try_reserve_never_exceeds_the_cap() {
    let b = RingByteBudget::new(100);
    assert!(b.try_reserve(60));
    assert_eq!(b.resident(), 60);
    assert!(b.try_reserve(40), "exactly filling the cap must be allowed");
    assert_eq!(b.resident(), 100);
    assert!(!b.try_reserve(1), "one byte past the cap must be refused");
    assert_eq!(
        b.resident(),
        100,
        "a REFUSED reserve must not move the counter — a fetch_add-then-check \
         would have left the overshoot behind, which is the bound failing to bound"
    );
}

#[test]
fn test_try_reserve_refuses_a_frame_larger_than_the_whole_cap() {
    let b = RingByteBudget::new(1_024);
    assert!(
        !b.try_reserve(4_096),
        "a frame bigger than the entire budget cannot be admitted — admitting it \
         would mean the budget does not bound"
    );
    assert_eq!(b.resident(), 0);
    // And the budget is still usable afterwards: one oversized frame must
    // not poison it for the frames that follow.
    assert!(b.try_reserve(512));
    assert_eq!(b.resident(), 512);
}

#[test]
fn test_ring_byte_budget_release_saturates_and_never_wraps() {
    let b = RingByteBudget::new(1_000);
    assert!(b.try_reserve(100));
    // Over-release: a bookkeeping bug, deliberately made harmless. Wrapping
    // to usize::MAX here would refuse every subsequent frame forever —
    // turning a counting error into a total feed outage.
    b.release(10_000);
    assert_eq!(b.resident(), 0, "release must saturate at zero, not wrap");
    assert!(
        b.try_reserve(1_000),
        "the budget must still admit frames after an over-release"
    );
}

#[test]
fn test_wal_ring_sink_returns_the_reservation_when_the_count_bound_refuses() {
    // The leak this pins: the byte reserve is taken BEFORE `try_send`, so a
    // frame refused by the COUNT bound has a reservation nothing downstream
    // will ever release. Without the release on that path the budget
    // ratchets down on every count-full frame until it refuses everything —
    // a slow strangulation that would present as the feed dying for no
    // visible reason, long after the burst that caused it.
    let dir = std::env::temp_dir().join(format!(
        "tv-ring-budget-leak-{}-{}",
        std::process::id(),
        next_frame_seq()
    ));
    let spill =
        std::sync::Arc::new(WsFrameSpill::new(&dir).expect("WAL must open under a fresh temp dir"));
    // Capacity 1: the second frame is refused by COUNT, not by bytes.
    let (tx, _rx) = crate::websocket::frame_ring::frame_ring::<CapturedFrame>(1);
    let budget = std::sync::Arc::new(RingByteBudget::new(1_000_000));
    let sink = WalRingSink::new(
        std::sync::Arc::clone(&spill),
        tx,
        std::sync::Arc::clone(&budget),
        WsType::LiveFeed,
        DhanEndpointType::MainFeed,
        0,
    );

    assert_eq!(
        sink.accept(Bytes::from_static(b"0123456789")),
        FrameSinkOutcome::Captured
    );
    assert_eq!(budget.resident(), 10);

    for _ in 0..50 {
        assert_eq!(
            sink.accept(Bytes::from_static(b"0123456789")),
            FrameSinkOutcome::RingFull
        );
    }
    assert_eq!(
        budget.resident(),
        10,
        "only the ONE frame actually sitting in the ring may hold a reservation; \
         50 count-refused frames must each have given theirs back"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_wal_ring_sink_refuses_on_the_byte_bound_before_the_count_bound() {
    // The whole point of the second bound: a ring with plenty of SLOTS free
    // still refuses when those slots would hold too many BYTES.
    let dir = std::env::temp_dir().join(format!(
        "tv-ring-budget-bytes-{}-{}",
        std::process::id(),
        next_frame_seq()
    ));
    let spill =
        std::sync::Arc::new(WsFrameSpill::new(&dir).expect("WAL must open under a fresh temp dir"));
    // 64 slots, but only 25 bytes of budget: the count bound cannot bind.
    let (tx, mut rx) = crate::websocket::frame_ring::frame_ring::<CapturedFrame>(64);
    let budget = std::sync::Arc::new(RingByteBudget::new(25));
    let sink = WalRingSink::new(
        std::sync::Arc::clone(&spill),
        tx,
        std::sync::Arc::clone(&budget),
        WsType::LiveFeed,
        DhanEndpointType::MainFeed,
        0,
    );

    for _ in 0..2 {
        assert_eq!(
            sink.accept(Bytes::from_static(b"0123456789")),
            FrameSinkOutcome::Captured
        );
    }
    assert_eq!(
        sink.accept(Bytes::from_static(b"0123456789")),
        FrameSinkOutcome::RingFull,
        "20 of 25 bytes are resident and 62 slots are free — the BYTE bound must \
         refuse the third frame, which is the bound the count alone never gave"
    );

    // Draining releases, and the sink accepts again — proving this is
    // backpressure, not a latch.
    let taken = rx.try_recv().expect("a captured frame must be published");
    budget.release(taken.bytes.len());
    assert_eq!(
        sink.accept(Bytes::from_static(b"0123456789")),
        FrameSinkOutcome::Captured,
        "releasing on drain must re-open the budget"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn receiver_drop_refunds_each_class_and_accounts_for_abandoned_wal_and_live_only_frames() {
    let recorder = InPlaceCounters::default();
    let dir = wal_dir("cancel-buffered-mixed");
    let spill = std::sync::Arc::new(WsFrameSpill::new(&dir).expect("open WAL"));
    metrics::with_local_recorder(&recorder, || {
        let (tx, mut rx) = crate::websocket::frame_ring::frame_ring::<CapturedFrame>(6);
        let weak = tx.downgrade();
        let sinks = [
            DhanEndpointType::MainFeed,
            DhanEndpointType::Depth20,
            DhanEndpointType::Depth200,
        ]
        .into_iter()
        .enumerate()
        .map(|(i, endpoint)| {
            let budget = std::sync::Arc::new(RingByteBudget::with_slot_cap(16, 2));
            let sink = WalRingSink::new(
                std::sync::Arc::clone(&spill),
                tx.clone(),
                std::sync::Arc::clone(&budget),
                WsType::LiveFeed,
                endpoint,
                i as u8,
            );
            (sink, budget)
        })
        .collect::<Vec<_>>();
        let frame = Bytes::from_static(b"12345678");
        assert_eq!(sinks[0].0.accept(frame.clone()), FrameSinkOutcome::Captured);
        let taken = rx.try_recv().expect("normal dequeue");
        sinks[0].1.release(taken.bytes.len());
        for (sink, _) in &sinks {
            assert_eq!(sink.accept(frame.clone()), FrameSinkOutcome::Captured);
        }
        assert_eq!(spill.shutdown(std::time::Duration::from_secs(5)), 0);
        for (sink, budget) in &sinks {
            assert_eq!(
                sink.accept(frame.clone()),
                FrameSinkOutcome::CapturedLiveOnly
            );
            assert_eq!(budget.slots_resident(), 2);
        }
        drop(rx);
        for (sink, budget) in &sinks {
            assert_eq!((budget.slots_resident(), budget.resident()), (0, 0));
            assert_eq!(sink.accept(frame.clone()), FrameSinkOutcome::WalDropped);
        }
        assert!(weak.upgrade().is_some());
    });
    let counts = recorder.counts.lock().expect("counters");
    assert_eq!(
        counts.get("tv_dhan_feed_shutdown_ring_deferred_total"),
        Some(&3)
    );
    assert_eq!(
        counts.get("tv_ticks_lost_total,source=shutdown_ring_unbacked"),
        Some(&3)
    );
    drop(counts);
    assert_eq!(
        tickvault_storage::ws_frame_spill::replay_all(&dir)
            .expect("replay")
            .len(),
        4
    );
    std::fs::remove_dir_all(dir).expect("cleanup");
}

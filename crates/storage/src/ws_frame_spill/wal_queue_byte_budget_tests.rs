//! WAL capture-queue byte-budget tests, moved verbatim out of
//! `ws_frame_spill.rs` on 2026-10-26 (plan item K2) so the parent stays under
//! its size pin.

use super::*;

/// The whole point: the queue is bounded in BYTES, not only in records.
///
/// The old bound was 524,288 records with no length check, so the same
/// queue was worth 12 MiB of ticker packets or ~256 GiB of depth-200
/// frames. This pins the arithmetic that makes the second case impossible.
#[test]
fn the_budget_is_a_fraction_of_the_host_clamped_at_both_ends() {
    // Production: r8g.xlarge, 32 GiB. 1/16 = 2 GiB, exactly the ceiling.
    let prod = 32 * 1024 * 1024 * 1024;
    assert_eq!(
        wal_queue_budget_from_ceiling(Some(prod)),
        WAL_QUEUE_MAX_BYTES_CEILING,
        "32 GiB host must resolve to the 2 GiB ceiling"
    );

    // A 4 GiB dev container: 1/16 = 256 MiB, exactly the floor.
    let dev = 4 * 1024 * 1024 * 1024;
    assert_eq!(
        wal_queue_budget_from_ceiling(Some(dev)),
        WAL_QUEUE_MIN_BYTES,
        "4 GiB host must resolve to the 256 MiB floor"
    );

    // A tiny host must NOT resolve below the floor.
    assert_eq!(
        wal_queue_budget_from_ceiling(Some(64 * 1024 * 1024)),
        WAL_QUEUE_MIN_BYTES,
        "a host smaller than the floor still gets the floor"
    );

    // A host between the two scales with it rather than clamping.
    let mid = 16 * 1024 * 1024 * 1024;
    assert_eq!(
        wal_queue_budget_from_ceiling(Some(mid)),
        mid / WAL_QUEUE_RAM_FRACTION_DIVISOR,
        "a host between floor and ceiling scales with the host"
    );
}

/// An unreadable host takes the FLOOR, never the ceiling.
///
/// Guessing large on an unknown host guesses toward the OOM this budget
/// exists to prevent, so the direction of the fallback is load-bearing.
#[test]
fn an_unknown_host_falls_back_to_the_floor_not_the_ceiling() {
    assert_eq!(wal_queue_budget_from_ceiling(None), WAL_QUEUE_MIN_BYTES);
    assert!(
        wal_queue_budget_from_ceiling(None) < WAL_QUEUE_MAX_BYTES_CEILING,
        "the unknown-host fallback must be the SMALL end"
    );
}

/// The bound must never be so tight that the ORDINARY tick mix hits it.
///
/// A Full packet is 162 bytes. The record bound (524,288) must still be
/// what binds for that mix, or this change would convert a working feed
/// into a dropping one — the exact regression a byte bound could cause.
#[test]
fn the_record_bound_still_binds_first_for_the_ordinary_tick_mix() {
    let full_packet_bytes = 162_u64;
    let bytes_at_record_capacity = full_packet_bytes * SPILL_CHANNEL_CAPACITY as u64;
    assert!(
        bytes_at_record_capacity < WAL_QUEUE_MIN_BYTES,
        "a full channel of 162-byte packets is {bytes_at_record_capacity} bytes, which must \
         stay under even the FLOOR ({WAL_QUEUE_MIN_BYTES}) — otherwise the byte bound would \
         start refusing ordinary ticks"
    );
}

/// And it must be tight enough that the pathological mix CANNOT reach the
/// host's memory. This is the failure the change exists to prevent.
#[test]
fn a_full_channel_of_max_frames_cannot_exceed_the_budget() {
    // The largest frame the depth-200 socket accepts.
    let depth_200_max = 512_u64 * 1024;
    let unbounded_worst_case = depth_200_max * SPILL_CHANNEL_CAPACITY as u64;
    let host = 32_u64 * 1024 * 1024 * 1024;
    assert!(
        unbounded_worst_case > host,
        "sanity: the record-only bound really does exceed a 32 GiB host \
         ({unbounded_worst_case} bytes) — this is the defect being fixed"
    );
    let budget = wal_queue_budget_from_ceiling(Some(host));
    assert!(
        budget < host,
        "the byte budget must sit strictly under the host it protects"
    );
    // The budget is what now bounds the pathological mix, not the record count.
    assert!(
        budget < unbounded_worst_case,
        "the byte budget must be the binding limit for max-size frames"
    );
}

/// A refused frame must RELEASE its reservation, or the budget ratchets
/// down to zero and a permanently-empty queue refuses everything.
///
/// `new_with_dead_writer_for_test` drops the receiver, so every send takes the
/// `Disconnected` arm — which is one of the two arms that must release.
#[test]
fn a_refused_frame_releases_its_byte_reservation() {
    let spill = WsFrameSpill::new_with_dead_writer_for_test();
    for _ in 0..64 {
        assert_eq!(
            spill.append(WsType::LiveFeed, vec![0_u8; 4096]),
            AppendOutcome::Dropped,
            "no writer exists, so every append must be refused"
        );
    }
    assert_eq!(
        spill.queued_bytes.load(Ordering::Relaxed),
        0,
        "every refused frame must have released its reservation; a leak here \
         would make the budget shrink monotonically until nothing is accepted"
    );
}

/// Hold the connected writer between reads and observe which bound rejects
/// further frames. The accepted frame stays in the queue while refusals occur.
fn stalled_writer_refusals(record_capacity: usize, byte_budget: u64) -> (u64, u64) {
    let (mut spill, reader) = WsFrameSpill::new_with_held_writer_for_test(record_capacity);
    spill.queue_budget_bytes = byte_budget;
    let record_full = Arc::new(metrics::atomics::AtomicU64::new(0));
    let bytes_full = Arc::new(metrics::atomics::AtomicU64::new(0));
    let idx = ws_type_index(WsType::LiveFeed);
    spill.drop_counters.ticks_lost_channel_full[idx] =
        metrics::Counter::from_arc(Arc::clone(&record_full));
    spill.drop_counters.ticks_lost_bytes_full[idx] =
        metrics::Counter::from_arc(Arc::clone(&bytes_full));

    let payload = Bytes::from_static(b"12345678");
    for (cycle, first_id) in [10, 20].into_iter().enumerate() {
        assert_eq!(
            spill.append_with_seq(WsType::LiveFeed, payload.clone(), first_id),
            AppendOutcome::Spilled
        );
        for refused_id in first_id + 1..=first_id + 3 {
            assert_eq!(
                spill.append_with_seq(WsType::LiveFeed, payload.clone(), refused_id),
                AppendOutcome::Dropped
            );
            assert_eq!(
                spill.queued_bytes.load(Ordering::Relaxed),
                payload.len() as u64
            );
            assert_eq!(spill.queued_records(), 1);
        }
        assert_eq!(spill.drop_critical_count(), 3 * (cycle as u64 + 1));
        let stored = reader.try_recv().expect("first frame remains readable");
        assert_eq!(stored.frame_seq, first_id);
        assert_eq!(stored.frame, payload);
        // Release the held reader's byte reservation as the writer does on dequeue.
        assert_eq!(
            spill
                .queued_bytes
                .fetch_sub(stored.frame.len() as u64, Ordering::Relaxed),
            payload.len() as u64
        );
        assert!(reader.is_empty());
    }
    let directory = spill.wal_dir.clone();
    drop(spill);
    std::fs::remove_dir_all(directory).unwrap();
    (
        record_full.load(Ordering::Relaxed),
        bytes_full.load(Ordering::Relaxed),
    )
}

#[test]
fn test_drop_counter_increments_when_channel_full() {
    // A second 8-byte frame fits the byte budget. The single record slot is full.
    assert_eq!(stalled_writer_refusals(1, 16), (6, 0));
}

#[test]
fn the_byte_limit_refuses_while_a_record_slot_is_free() {
    assert_eq!(stalled_writer_refusals(2, 8), (0, 6));
}

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

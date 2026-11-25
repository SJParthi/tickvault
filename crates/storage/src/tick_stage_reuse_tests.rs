//! PF-4 review round 1 (2026-11-06): batch reuse on the rescue path, name
//! pins kept on the drain, the park counted on capacity, and the unbacked
//! mark across a ninth-table cut. A child of `tick_persistence`, so it reads
//! the writer's private state directly.

use super::*;
use crate::candle_contract_labels::{
    CandleContractLabels, TEST_PUBLISH_LOCK, publish_candle_contract_labels,
};
use std::sync::Arc;

fn lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_PUBLISH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn scratch_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = std::env::temp_dir().join(format!("tv-pf4-reuse-{tag}-{nanos}"));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// A Full-shaped NSE_FNO tick inside the session window.
fn tick(security_id: u64) -> ParsedTick {
    // 2026-10-29 10:00:00 IST, IST epoch seconds.
    let ltt = 1_793_268_000_u32;
    ParsedTick {
        security_id,
        exchange_segment_code: 2,
        last_traded_price: 245.35,
        last_trade_quantity: 75,
        exchange_timestamp: ltt,
        received_at_nanos: (i64::from(ltt) - 19_800) * 1_000_000_000,
        volume: 1_250_000,
        day_open: 238.05,
        day_close: 240.95,
        day_high: 251.2,
        day_low: 236.6,
        ..ParsedTick::default()
    }
}

/// Publishes a one-entry name table naming `security_id` (NSE_FNO) `name`.
fn publish(security_id: u64, name: &str) {
    let mut labels = CandleContractLabels::default();
    labels.insert(
        (i64::try_from(security_id).expect("fits"), "NSE_FNO"),
        Arc::from(name),
    );
    assert!(publish_candle_contract_labels(labels) > 0);
}

/// Plays the rescue thread for every queued batch.
fn settle(sink: &TickRescueSink, rx: &std::sync::mpsc::Receiver<RescueBatch>) {
    while let Ok(batch) = rx.try_recv() {
        sink.rescue(&batch);
        sink.recycle(batch);
    }
}

/// Item 1: a split writer's rescue path reuses the batch the rescue thread
/// hands back, cleared (no rows, no pins), instead of reserving a fresh one.
#[test]
fn test_recycle_a_split_writer_reuses_the_batch_its_rescue_thread_returned() {
    let _g = lock();
    publish(41_001, "REUSE-NAME");
    let dir = scratch_dir("recycle");
    let w = TickWriter::for_test(Feed::Dhan).with_spill_dir_for_test(dir.clone());
    let (mut w, _wsink, _wrx) = w.split_for_offload();
    let (rsink, rrx) = w.split_rescue_offload();

    w.append_tick_with_seq(&tick(41_001), 0).expect("append");
    assert_eq!(w.discard_pending(), 1, "handed to the rescue thread");
    let batch = rrx.try_recv().expect("queued");
    assert_eq!(batch.staged.pin_count(), 1, "the batch carries its pin");
    let spent = batch.staged.as_ptr_for_test();
    rsink.rescue(&batch);
    rsink.recycle(batch);

    w.append_tick_with_seq(&tick(41_001), 0).expect("append");
    assert_eq!(w.discard_pending(), 1);
    assert_eq!(
        w.staged.as_ptr_for_test(),
        spent,
        "the next batch is the one the rescue thread returned"
    );
    assert!(w.staged.is_empty(), "returned with no rows");
    assert_eq!(
        w.staged.pin_count(),
        0,
        "its pins were dropped off the drain"
    );
    assert!(w.staged.capacity() >= crate::tick_stage::TICK_STAGED_ROWS_PRESIZE);

    settle(&rsink, &rrx);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `recycle` drops (on the rescue thread) a batch over the producer cut and
/// one with no lane to return to, and never hands back a non-empty batch.
#[test]
fn test_recycle_drops_an_oversized_batch_and_works_with_no_lane() {
    let mut w = TickWriter::for_test(Feed::Dhan);
    let (sink, _rx) = w.split_rescue_offload();
    let oversized = RescueBatch {
        staged: crate::tick_stage::StagedBatch::with_capacity_for_test(MAX_PRODUCER_ROWS + 1),
        rows: 0,
        min_seq: 0,
        max_seq: 0,
        unbacked: false,
        floor: None,
    };
    sink.recycle(oversized);
    let lane = w
        .spare_buffers
        .as_ref()
        .expect("the rescue split opens the lane");
    assert!(
        lane.try_recv().is_err(),
        "an oversized batch is not returned"
    );

    let lone = TickRescueSink {
        spill_dir: PathBuf::new(),
        feed: Feed::Dhan,
        spill_sync: None,
        scratch: std::cell::RefCell::new(TickRowBuffer::new(TickIlpEncoder::Library)),
        spare_return: None,
    };
    lone.recycle(RescueBatch {
        staged: crate::tick_stage::StagedBatch::with_capacity_for_test(16),
        rows: 0,
        min_seq: 0,
        max_seq: 0,
        unbacked: false,
        floor: None,
    });
}

/// Item 2: on the drain, a split writer clears the rows of a deferred or
/// inline-spilled batch but keeps its name pins (they leave with the batch's
/// next hand-off); an unsplit writer drops them.
#[test]
fn test_clear_rows_on_the_drain_keeps_the_pins_of_a_split_writer() {
    let _g = lock();
    publish(41_002, "KEEP-PINS");
    for defer in [true, false] {
        let dir = scratch_dir("keep-pins");
        let mut w = TickWriter::for_test(Feed::Dhan).with_spill_dir_for_test(dir.clone());
        let (rsink, rrx) = w.split_rescue_offload();
        w.wal_defer_check = if defer {
            |_, _, _| true
        } else {
            |_, _, _| false
        };
        // Fill the rescue queue with WAL-backed rows (never parked).
        for seq in 1..=RESCUE_QUEUE_DEPTH as i64 {
            w.append_tick_with_seq(&tick(41_002), seq).expect("append");
            assert_eq!(w.discard_pending(), 1);
        }
        w.append_tick_with_seq(&tick(41_002), 50).expect("append");
        assert_eq!(w.staged.pin_count(), 1);
        assert_eq!(w.discard_pending(), 1, "deferred or spilled inline");
        assert!(w.staged.is_empty(), "defer={defer}: the rows are gone");
        assert_eq!(
            w.staged.pin_count(),
            1,
            "defer={defer}: the pins stay until the batch leaves the drain"
        );
        settle(&rsink, &rrx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Unsplit (tests, cold synchronous paths): no other thread, dropped here.
    let dir = scratch_dir("unsplit-pins");
    let mut w = TickWriter::for_test(Feed::Dhan).with_spill_dir_for_test(dir.clone());
    w.append_tick_with_seq(&tick(41_002), 0).expect("append");
    assert_eq!(w.discard_pending(), 1, "spilled inline");
    assert_eq!(w.staged.pin_count(), 0, "an unsplit writer drops them");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Item 2, the one remaining drain-side release: a batch emptied on the
/// drain with all eight pin slots full meets a ninth table while the writer
/// queue is full. The stale pins are dropped and the new row pins the NINTH
/// table (never a stale slot).
#[test]
fn test_regression_a_ninth_table_after_a_deferral_pins_the_right_table() {
    let _g = lock();
    let dir = scratch_dir("ninth-after-defer");
    let w = TickWriter::for_test(Feed::Dhan).with_spill_dir_for_test(dir.clone());
    let (mut w, _wsink, _wrx) = w.split_for_offload();
    let (rsink, rrx) = w.split_rescue_offload();
    w.wal_defer_check = |_, _, _| true;
    publish(41_003, "TABLE-0");
    let mut seq = 0_i64;
    // Fill the writer queue (nothing drains it).
    for _ in 0..FLUSH_QUEUE_DEPTH {
        seq += 1;
        w.append_tick_with_seq(&tick(41_003), seq).expect("append");
        w.flush().expect("hand off");
    }
    // Fill the rescue queue.
    for _ in 0..RESCUE_QUEUE_DEPTH {
        seq += 1;
        w.append_tick_with_seq(&tick(41_003), seq).expect("append");
        assert_eq!(w.discard_pending(), 1);
    }
    // Eight distinct tables, one row each: every pin slot used.
    for t in 1..=crate::tick_stage::TICK_STAGE_LABEL_PINS {
        publish(41_003, &format!("TABLE-{t}"));
        seq += 1;
        w.append_tick_with_seq(&tick(41_003), seq).expect("append");
    }
    assert_eq!(
        w.staged.pin_count(),
        crate::tick_stage::TICK_STAGE_LABEL_PINS
    );
    // Rescue queue full, rows WAL-backed: deferred, rows cleared, pins kept.
    assert_eq!(
        w.discard_pending(),
        crate::tick_stage::TICK_STAGE_LABEL_PINS
    );
    assert!(w.staged.is_empty());
    assert_eq!(
        w.staged.pin_count(),
        crate::tick_stage::TICK_STAGE_LABEL_PINS
    );

    publish(41_003, "NINTH-NAME");
    seq += 1;
    w.append_tick_with_seq(&tick(41_003), seq).expect("append");
    assert_eq!(w.staged.pin_count(), 1, "only the ninth table is pinned");
    assert_eq!(w.staged.len(), 1);
    let text = String::from_utf8(
        w.staged
            .encode_readonly(Feed::Dhan, TickIlpEncoder::Library),
    )
    .expect("utf8");
    assert!(text.contains("contract=NINTH-NAME"), "{text}");

    settle(&rsink, &rrx);
    publish_candle_contract_labels(CandleContractLabels::default());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Optional item: a ninth-table cut on the first row of a frame marked
/// unbacked hands the old batch off, and the new batch (which holds that
/// frame's rows) keeps the mark.
#[test]
fn test_regression_a_ninth_table_cut_keeps_the_unbacked_mark() {
    let _g = lock();
    let (mut w, _wsink, _wrx) = TickWriter::for_test(Feed::Dhan).split_for_offload();
    let mut seq = 0_i64;
    for t in 0..crate::tick_stage::TICK_STAGE_LABEL_PINS {
        publish(41_004, &format!("UNBACKED-{t}"));
        seq += 1;
        w.append_tick_with_seq(&tick(41_004), seq).expect("append");
    }
    assert!(!w.pending_unbacked());
    // The next frame's WAL append was refused: marked before its rows.
    w.mark_pending_unbacked();
    publish(41_004, "UNBACKED-NINTH");
    seq += 1;
    w.append_tick_with_seq(&tick(41_004), seq).expect("append");
    assert_eq!(w.pending(), 1, "the old batch was handed off by the cut");
    assert!(
        w.pending_unbacked(),
        "the frame staged after the cut keeps its unbacked mark"
    );
    publish_candle_contract_labels(CandleContractLabels::default());
}

/// Item 3: the park is counted on each batch's capacity, so its gauge and
/// bound are the RAM it really holds, not the rows it carries.
#[test]
fn test_park_counts_each_batch_capacity_not_its_rows() {
    let dir = scratch_dir("park-capacity");
    let mut w = TickWriter::for_test(Feed::Dhan).with_spill_dir_for_test(dir.clone());
    let (rsink, rrx) = w.split_rescue_offload();
    for _ in 0..RESCUE_QUEUE_DEPTH {
        w.append_tick_with_seq(&tick(41_005), 0).expect("append");
        assert_eq!(w.discard_pending(), 1);
    }
    // Unbacked rows meet a full queue: parked.
    w.append_tick_with_seq(&tick(41_005), 0).expect("append");
    let capacity = w.staged.capacity();
    assert_eq!(w.discard_pending(), 1);
    assert_eq!(w.parked_rescue.len(), 1, "parked");
    assert_eq!(
        w.parked_rescue_slots, capacity,
        "one row parked, its whole capacity counted"
    );
    assert!(capacity >= crate::tick_stage::TICK_STAGED_ROWS_PRESIZE);
    // Eight pre-sized batches fill the bound exactly.
    assert_eq!(
        PARKED_RESCUE_MAX_ROWS,
        PARKED_RESCUE_MAX_BATCHES * crate::tick_stage::TICK_STAGED_ROWS_PRESIZE
    );
    // A batch bigger than the room left does not fit, whatever its rows.
    let big = RescueBatch {
        staged: crate::tick_stage::StagedBatch::with_capacity_for_test(
            PARKED_RESCUE_MAX_ROWS - capacity + 1,
        ),
        rows: 1,
        min_seq: 0,
        max_seq: 0,
        unbacked: true,
        floor: None,
    };
    assert!(!w.park_rescue_fits(&big));
    loop {
        settle(&rsink, &rrx);
        if w.parked_rescue.is_empty() {
            break;
        }
        let _ = w.flush();
    }
    assert_eq!(w.parked_rescue_slots, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

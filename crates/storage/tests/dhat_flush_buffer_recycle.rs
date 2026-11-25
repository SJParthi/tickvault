//! DHAT ratchet for ILP buffer RECYCLING on the offloaded depth flush path
//! (2026-09-22, plan item 44g).
//!
//! ## What this pins
//!
//! Before item 44g every hand-off swapped the producer's ILP buffer for a
//! fresh `Buffer::new`, so the NEXT batch regrew its buffer from zero — several
//! reallocations per flush, on the frame-drain task, several times a second.
//! Now the writer thread hands each spent buffer back over a bounded lane and
//! the producer reuses it after the next successful hand-off.
//!
//! This gate drives N full flush cycles — append a batch, hand it off, receive
//! it on the "writer" side, return the spent buffer — and asserts the whole
//! window allocates FEWER than N blocks. Without recycling every cycle grows a
//! fresh buffer, so the window costs at least N blocks and the gate fails.
//!
//! ## Why the sink's `write` is not in the loop
//!
//! `for_test` has no ILP sender, so `DepthWriterSink::write` would take the
//! rescue arm and write a spill FILE every cycle — measuring file I/O, not the
//! recycle. `write` calls `return_spare_buffer` as its last step (pinned by the
//! in-file round-trip test `a_written_depth_buffer_round_trips_back_to_the_producer_empty`),
//! so the loop drives that step directly.
//!
//! CI ENFORCEMENT: deliberately UN-gated (no `#![cfg(feature = "dhat")]`), the
//! house pattern; the Coverage lane skips it by the `dhat_` name prefix.
//!
//! Run: `cargo test -p tickvault-storage --test dhat_flush_buffer_recycle`

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use tickvault_common::feed::Feed;
use tickvault_storage::depth_persistence::{
    DEPTH_KIND_20, DEPTH_SIDE_ASK, DEPTH_SIDE_BID, DepthRow, DepthWriter,
};

mod dhat_support;

/// Flush cycles inside the measured window.
const CYCLES: u64 = 200;
/// Rows per cycle — one depth-20 update (20 levels x 2 sides).
const ROWS_PER_CYCLE: i64 = 40;
const BUDGET_BYTES: u64 = 1024;
const BUDGET_BLOCKS: u64 = 8;

fn row(level: i64, side: &'static str, capture_seq: i64) -> DepthRow {
    DepthRow {
        security_id: 13,
        segment: "NSE_FNO",
        depth_kind: DEPTH_KIND_20,
        side,
        level,
        price: 25_000.5 + level as f64,
        quantity: 100 * level,
        orders: level,
        capture_seq,
        // 09:16:40 IST — inside the session window, so rows are APPENDED
        // rather than refused (see the non-vacuity assertion below).
        ts_nanos: 1_779_355_000_000_000_000,
    }
}

#[test]
fn dhat_offloaded_depth_flush_recycles_its_buffer_instead_of_regrowing() {
    let (mut writer, sink, rx) = DepthWriter::for_test(Feed::Dhan).split_for_offload();
    let mut seq: i64 = 1;
    let mut landed_rows: usize = 0;

    let cycle = |writer: &mut DepthWriter, seq: &mut i64, landed: &mut usize| {
        for level in 1..=ROWS_PER_CYCLE {
            let side = if level % 2 == 0 {
                DEPTH_SIDE_BID
            } else {
                DEPTH_SIDE_ASK
            };
            let _ = writer.append_row(&row((level % 20) + 1, side, *seq));
            *seq += 1;
        }
        let _ = writer.flush();
        if let Ok(mut batch) = rx.try_recv() {
            *landed += batch.rows();
            sink.return_spare_buffer(&mut batch);
        }
    };

    // Warm-up OUTSIDE the window: the first two cycles grow the two buffers
    // that then alternate for the rest of the run, and initialise the
    // process-global watermark.
    for _ in 0..4 {
        cycle(&mut writer, &mut seq, &mut landed_rows);
    }
    let warm_landed = landed_rows;

    let _profiler = dhat::Profiler::builder().testing().build();

    let (bytes, blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || {
            for _ in 0..CYCLES {
                cycle(&mut writer, &mut seq, &mut landed_rows);
            }
        },
    );

    assert!(
        blocks < CYCLES && bytes <= BUDGET_BYTES && blocks <= BUDGET_BLOCKS,
        "{CYCLES} offloaded depth flush cycles allocated {bytes} bytes / {blocks} blocks \
         (budget {BUDGET_BYTES} B / {BUDGET_BLOCKS} blocks, and always fewer blocks than \
         cycles). A count at or above the cycle count means each batch is regrowing its \
         ILP buffer from zero again — the writer thread's spent buffer is no longer \
         reaching the producer (DepthWriterSink::return_spare_buffer / \
         DepthWriter::install_spare_buffer)."
    );

    // NON-VACUITY: the window must actually have handed rows off. A fixture
    // whose rows were refused by the session-window gate would measure an
    // empty loop and pass for the wrong reason.
    assert!(
        landed_rows > warm_landed,
        "VACUOUS: no batch was handed off inside the measured window"
    );
}

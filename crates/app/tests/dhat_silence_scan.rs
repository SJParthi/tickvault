//! DHAT zero-allocation gate for the drain's 30 s silence arm with the
//! subscribe-reconcile book attached (2026-11-17).
//!
//! The arm scans every tracked instrument and, since the slot-cell change,
//! judges each one's subscription through `ReconcileSweep::note_at_slot` (a
//! dense per-slot cell) instead of a hash probe. This gate builds the
//! production shape: 20,000 stock-option contracts, each past its warm-up and
//! `Live`, the book attached with `with_subscribe_reconcile` as the lane does,
//! one warm-up scan to fill the cells, then 1,000 profiled
//! `scan_silence_named` calls. Budget: 0 blocks, 0 bytes.
//!
//! Non-vacuity: the detector tracks all 20,000, the book expects all 20,000,
//! and the book's fast-path counter (`SubscribeReconcile::fast_path_hits`)
//! rises by exactly 20,000 per profiled scan, so every report took the fast
//! path. Valid cells alone would not prove it: the probe path re-fills the
//! same cells and allocates nothing either (2026-11-17 review).
//!
//! One heap case per binary (DHAT's profiler is process-global).
//! Run: `cargo test -p tickvault-app --test dhat_silence_scan`

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::sync::Arc;

use tickvault_app::dhan_feed_stack::{LiveIngest, SilentInstrument, WORST_SILENT_NAMED};
use tickvault_app::subscribe_reconcile::SubscribeReconcile;
use tickvault_common::feed::Feed;
use tickvault_common::tick_types::ParsedTick;
use tickvault_common::types::ExchangeSegment;
use tickvault_core::websocket::pool_supervisor::SubscribeInstrument;
use tickvault_storage::tick_persistence::TickWriter;

const INSTRUMENTS: u64 = 20_000;
const SCANS: usize = 1_000;
/// Ticks per instrument: 9 ticks give 8 intervals, the detector's warm-up, so
/// every instrument reads `Live`.
const TICKS: u64 = 9;
/// Detector capacity: the authorized ceiling, as the live lane builds it.
const DETECTOR_CAPACITY: usize = 25_000;
/// An exchange stamp inside a trading session (epoch seconds).
const SESSION_EPOCH_SECS: u32 = 1_786_800_000;
/// The detector's receive clock at the first tick.
const BASE_MILLIS: u64 = 1_000_000;
/// `NSE_FNO` segment byte.
const NSE_FNO_CODE: u8 = 2;

#[test]
fn silence_scan_with_reconcile_slot_cells_does_not_allocate() {
    let (book, _rounds) = SubscribeReconcile::new(false);
    let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8)
        .with_detector_capacity(DETECTOR_CAPACITY)
        .with_subscribe_reconcile(Arc::clone(&book));
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let list: Vec<SubscribeInstrument> = (0..INSTRUMENTS)
        .map(|i| SubscribeInstrument {
            security_id: 60_000 + i,
            segment: ExchangeSegment::NseFno,
        })
        .collect();
    for chunk in list.chunks(5_000) {
        book.record_subscribed(&tx, chunk);
    }
    let mut frame = 0u64;
    for instrument in &list {
        assert!(ingest.seed(instrument.security_id, instrument.segment, BASE_MILLIS));
        for k in 0..TICKS {
            let tick = ParsedTick {
                security_id: instrument.security_id,
                exchange_segment_code: NSE_FNO_CODE,
                last_traded_price: 100.0,
                exchange_timestamp: SESSION_EPOCH_SECS + k as u32,
                volume: 10 + k as u32,
                ..Default::default()
            };
            frame += 1;
            let _ = ingest.ingest_tick_at(&tick, frame, 0, BASE_MILLIS + k * 1_000);
        }
    }
    assert_eq!(ingest.tracked_instruments(), INSTRUMENTS as usize);
    assert_eq!(book.expected_len(), INSTRUMENTS as usize);

    let now = BASE_MILLIS + TICKS * 1_000;
    let mut worst = [SilentInstrument::EMPTY; WORST_SILENT_NAMED];
    // Warm-up, outside the profile: the first scan probes and fills the cells.
    assert_eq!(ingest.scan_silence_named(now, &mut worst), (0, 0, 0));
    assert_eq!(
        book.cached_cells(),
        INSTRUMENTS as usize,
        "the warm-up scan must cache every instrument"
    );

    let hits_before = book.fast_path_hits();
    let profiler = dhat::Profiler::builder().testing().build();
    let mut quiet = 0u64;
    for _ in 0..SCANS {
        let (silent, never, named) = ingest.scan_silence_named(now, &mut worst);
        quiet += silent + never + named as u64;
    }
    let stats = dhat::HeapStats::get();
    drop(profiler);

    assert_eq!(quiet, 0, "every instrument is Live");
    assert_eq!(
        book.fast_path_hits() - hits_before,
        SCANS as u64 * INSTRUMENTS,
        "every profiled report took the fast path"
    );
    assert_eq!(book.cached_cells(), INSTRUMENTS as usize);
    assert_eq!(
        (stats.total_blocks, stats.total_bytes),
        (0, 0),
        "{SCANS} silence scans over {INSTRUMENTS} instruments allocated {} blocks / {} bytes; \
         the arm must not allocate (find it, do not raise the budget)",
        stats.total_blocks,
        stats.total_bytes
    );
}

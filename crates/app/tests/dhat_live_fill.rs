//! LF-6b DHAT gate: the live paper fill chain's per-tick paths allocate
//! nothing.
//!
//! * (A) `LiveFillTap::offer`, the drain's half: accepted and ring-full arms,
//!   each with its upstream-shed load (one relaxed atomic), 0 blocks over
//!   20,000 calls.
//! * (B) the frame drain with the tap ARMED: the same 10,000-tick workload as
//!   `dhat_live_ingest_seam::frame_drain_seam_does_not_allocate_per_tick`,
//!   held to that test's 256-block ceiling (the drain's own row-buffer growth,
//!   not per tick), and every tick must reach the tap (non-vacuity).
//! * (C) the chain thread's per-tick and per-pass work (`on_tap`, `on_pass`:
//!   candle book, release queue, strategy `on_bar`, 1 s seconds, and the
//!   repeat-quote path), 0 blocks
//!   over 10,000 ticks and 20,000 passes. The engine is the trading crate's
//!   `ReferenceDouble` TEST DOUBLE on a flat price, so it asks for no order
//!   (order rows allocate by design; they are per order, not per tick).
//!
//! One profiler per process (dhat constraint), so one `#[test]`.
//!
//! Run: `cargo test -p tickvault-app --test dhat_live_fill`

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::collections::HashMap;

use tickvault_app::dhan_feed_stack::live_fill_tap::TapTick;
use tickvault_app::dhan_feed_stack::live_fill_tap::{LiveFillTap, live_fill_tap_ring};
use tickvault_app::dhan_feed_stack::{LiveIngest, counters, drain_main_feed_frame};
use tickvault_app::live_fill::chain::{ChainParams, ChainSetup, ContractSpec, LiveFillChain};
use tickvault_common::feed::Feed;
use tickvault_common::tick_types::ParsedTick;
use tickvault_core::websocket::pool_budget::DhanEndpointType;
use tickvault_core::websocket::pool_supervisor::CapturedFrame;
use tickvault_storage::tick_persistence::TickWriter;
use tickvault_trading::brutex_host::InstrumentKey;
use tickvault_trading::brutex_host::engine::ReferenceDouble;

mod dhat_support;

/// 13:20:00 IST on a trading day (see `dhat_live_ingest_seam.rs`).
const SESSION_EPOCH_SECS: u32 = 1_786_800_000;
const SESSION_RECEIPT_NANOS: i64 = (SESSION_EPOCH_SECS as i64 - 19_800) * 1_000_000_000;
const DRAIN_MAX_BLOCKS: u64 = 256;
const NS: i64 = 1_000_000_000;
/// IST midnight of the chain's day (a multiple of 86,400).
const DAY: i64 = 1_779_321_600;

fn quote_packet(security_id: u32, ltp: f32, ltt: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 50];
    buf[0] = 4; // quote
    buf[1..3].copy_from_slice(&50u16.to_le_bytes());
    buf[3] = 0; // IDX_I
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    buf[8..12].copy_from_slice(&ltp.to_le_bytes());
    buf[14..18].copy_from_slice(&ltt.to_le_bytes());
    buf
}

fn frame(seq: u64, n: u64) -> CapturedFrame {
    let mut bytes = Vec::with_capacity(200);
    for i in 0..4u32 {
        bytes.extend_from_slice(&quote_packet(
            13 + i,
            100.0 + (n % 97) as f32 * 0.05,
            SESSION_EPOCH_SECS + u32::try_from(n / 4).expect("fits"),
        ));
    }
    CapturedFrame {
        seq,
        endpoint: DhanEndpointType::MainFeed,
        connection_index: (n % 4) as u8,
        received_at: std::time::Instant::now(),
        received_at_nanos: tickvault_storage::ws_frame_spill::receipt_nanos_from(
            std::time::Instant::now(),
        ),
        wal_backed: true,
        bytes: bytes.into(),
    }
}

fn receipt_nanos(n: u64) -> i64 {
    SESSION_RECEIPT_NANOS + i64::try_from(n / 4).expect("fits") * NS
}

fn chain() -> LiveFillChain<ReferenceDouble> {
    let key = InstrumentKey {
        security_id: 2885,
        exchange_segment: 1,
    };
    let recipe = serde_json::from_str(
        r#"{"schema":"tickvault-strategy-recipe/1","name":"toy","version":1,"status":"approved","approved_by":"test","approved_at_ist":"2026-11-12T08:00:00+05:30","brutex_commit":"0000000000000000000000000000000000000000","timeframes":["1m"],"settings":{},"backtest":{}}"#,
    )
    .expect("recipe");
    let mut contracts = HashMap::new();
    contracts.insert(
        key,
        ContractSpec {
            tick_paise: 5,
            lot_size: 1,
            symbol: "X".to_owned(),
        },
    );
    let setup = ChainSetup {
        recipe,
        snapshot: None,
        engine_frames_secs: vec![60],
        universe: vec![key],
        contracts,
    };
    let params = ChainParams {
        wait_ms: 1_000,
        open_wait_ms: 2_500,
        band_bps: 1_000,
        participation_bps: 2_500,
        max_fill_seconds: 60,
    };
    LiveFillChain::new(ReferenceDouble::new(), setup, params, (DAY + 33_000) * NS).expect("chain")
}

fn tap_at(secs: i64, cum: u32) -> TapTick {
    TapTick {
        security_id: 2885,
        receipt_ist_ns: secs * NS + 300_000_000,
        ltp: 100.0,
        exchange_secs: u32::try_from(secs).expect("fits"),
        cum_volume: cum,
        best_bid: 99.95,
        best_ask: 100.05,
        best_bid_qty: 10,
        best_ask_qty: 10,
        drop_seq: 0,
        segment: 1,
        new_trade: true,
    }
}

#[test]
fn dhat_live_fill_tap_drain_and_chain_per_tick_paths_allocate_nothing() {
    // ---------------- fixtures, all before the profiler ----------------
    let (accept_tx, mut accept_rx) = live_fill_tap_ring(32_768);
    let mut accept_tap = LiveFillTap::new(accept_tx);
    let (full_tx, _full_rx) = live_fill_tap_ring(1);
    let mut full_tap = LiveFillTap::new(full_tx);
    let parsed = ParsedTick {
        security_id: 7,
        exchange_segment_code: 2,
        last_traded_price: 10.0,
        exchange_timestamp: SESSION_EPOCH_SECS,
        received_at_nanos: SESSION_RECEIPT_NANOS,
        volume: 100,
        ..ParsedTick::default()
    };
    // Warm: first send on each ring, the counter registration.
    assert!(accept_tap.offer(&parsed, None, true));
    let _ = accept_rx.try_recv();
    let _ = full_tap.offer(&parsed, None, true);
    let _ = full_tap.offer(&parsed, None, true);

    const WARMUP_FRAMES: u64 = 4;
    const FRAMES: u64 = 2_500;
    let frames: Vec<CapturedFrame> = (WARMUP_FRAMES..WARMUP_FRAMES + FRAMES)
        .map(|n| frame(100 + n, n))
        .collect();
    let (drain_tx, mut drain_rx) = live_fill_tap_ring(16_384);
    let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8)
        .with_live_fill_tap(LiveFillTap::new(drain_tx));
    ingest
        .spot_prices()
        .reset_for_trading_day(tickvault_app::spot_price_store::ist_day_of(
            SESSION_EPOCH_SECS,
        ));
    let c = counters();
    for n in 0..WARMUP_FRAMES {
        let out = drain_main_feed_frame(&mut ingest, &frame(n, n), receipt_nanos(n), 1_000, c);
        assert_eq!(out.folded, 4);
    }
    while drain_rx.try_recv().is_some() {}

    let mut chain = chain();
    // Warm the chain over two minutes: bars opened, closed, released, the
    // engine's slot inserted, the release queue sorted.
    let open = DAY + 33_300;
    let mut cum: u32 = 0;
    let mut sec = open;
    let step = |chain: &mut LiveFillChain<ReferenceDouble>, sec: &mut i64, cum: &mut u32| {
        *cum = cum.wrapping_add(10);
        chain.on_tap(&tap_at(*sec, *cum));
        // The same packet again as a repeat quote (no new trade): the
        // quote-only path (finding 2) is gated too.
        let mut repeat = tap_at(*sec, *cum);
        repeat.new_trade = false;
        chain.on_tap(&repeat);
        let t = *sec * NS + 500_000_000;
        chain.on_pass(t, t - DAY * NS).expect("pass");
        let t2 = (*sec + 1) * NS;
        chain.on_pass(t2, t2 - DAY * NS).expect("pass");
        *sec += 1;
    };
    for _ in 0..180 {
        step(&mut chain, &mut sec, &mut cum);
    }
    assert!(
        chain.counters().bars_released > 100,
        "warm-up released bars"
    );
    assert_eq!(chain.counters().intents, 0, "a flat price asks for nothing");

    let _profiler = dhat::Profiler::builder().testing().build();

    // ---------------- (A) the tap ----------------
    let (a_bytes, a_blocks) = dhat_support::measure_with_phantom_retry(
        0,
        0,
        || {},
        || {
            for _ in 0..10_000 {
                std::hint::black_box(accept_tap.offer(&parsed, None, true));
                std::hint::black_box(full_tap.offer(&parsed, None, true));
            }
            while accept_rx.try_recv().is_some() {}
        },
    );
    assert_eq!(
        (a_bytes, a_blocks),
        (0, 0),
        "LiveFillTap::offer allocated {a_bytes} bytes / {a_blocks} blocks over 20,000 calls"
    );

    // ---------------- (B) the drain with the tap armed ----------------
    let before = dhat::HeapStats::get();
    let mut folded = 0u64;
    for (n, f) in (WARMUP_FRAMES..).zip(&frames) {
        folded += drain_main_feed_frame(&mut ingest, f, receipt_nanos(n), 1_000 + n, c).folded;
    }
    let after = dhat::HeapStats::get();
    let drain_blocks = after.total_blocks - before.total_blocks;
    assert_eq!(folded, FRAMES * 4);
    assert!(
        drain_blocks <= DRAIN_MAX_BLOCKS,
        "the frame drain with the live-fill tap armed allocated {drain_blocks} blocks over \
         {} ticks (ceiling {DRAIN_MAX_BLOCKS}, the drain's own gate)",
        FRAMES * 4
    );
    let mut tapped = 0u64;
    while drain_rx.try_recv().is_some() {
        tapped += 1;
    }
    assert_eq!(tapped, FRAMES * 4, "every folded live tick reached the tap");
    assert_eq!(ingest.live_fill_tap_dropped(), Some(0));

    // ---------------- (C) the chain's per-tick and per-pass work ----------------
    let (c_bytes, c_blocks) = dhat_support::measure_with_phantom_retry(
        0,
        0,
        || {},
        || {
            for _ in 0..10_000 {
                step(&mut chain, &mut sec, &mut cum);
            }
        },
    );
    assert_eq!(
        (c_bytes, c_blocks),
        (0, 0),
        "the live-fill chain allocated {c_bytes} bytes / {c_blocks} blocks over 10,000 ticks \
         and 20,000 passes"
    );
    assert_eq!(chain.counters().intents, 0);
    assert!(chain.counters().bars_released > 10_000);
}

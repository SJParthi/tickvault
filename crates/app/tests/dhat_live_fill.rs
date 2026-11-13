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
//!   candle book, release queue, strategy `on_bar`, 1 s seconds, the
//!   repeat-quote path, and since LF-9 the older-trade path, late ticks, a
//!   dropped tick, a moved out-of-band loss total, a feed gap, a clock step,
//!   a release-queue overflow and backlogged / lagging passes), 0 blocks over
//!   10,000 ticks and 20,000 passes. The engine is the trading crate's
//!   `ReferenceDouble` TEST DOUBLE on a flat price, so it asks for no order.
//! * (D, LF-9 finding 5) the same work with HELD ORDERS: a price that turns
//!   every minute makes the double ask for an order each minute; each order
//!   is held, fills through its window (or is refused by a dropped tick, a
//!   moved loss total, a feed gap or a clock step that touches it), and its
//!   Execute's position report is applied, with older trades, late ticks,
//!   backlogged passes, marks through the command ring (read in production
//!   mode: never busy-polled, so each send takes the wake path) and a
//!   drained-behind stream along the way. Every step that moves no order
//!   allocates 0 blocks; a step that decides or finishes an order allocates
//!   only its rows (order rows allocate by design, per order, never per
//!   tick), at most `ORDER_STEP_MAX_BLOCKS` per order counted.
//! * (E, LF-9 finding 5) the thread's once-a-second counter publish
//!   (`CounterPublisher`, handles resolved once) and the command ring's
//!   producer side as production runs it (LF-9 hardening): (E1) 2,000 Marks
//!   each waking a consumer task PARKED in `recv()` on another thread (the
//!   `notify_one` path), (E2) a ring nobody drains filled to its 64 slots
//!   and refused past them (`send_output`'s full branch): 0 blocks.
//!
//! One profiler per process (dhat constraint), so one `#[test]`.
//!
//! Run: `cargo test -p tickvault-app --test dhat_live_fill`

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::collections::HashMap;

use tickvault_app::dhan_feed_stack::live_fill_tap::TapTick;
use tickvault_app::dhan_feed_stack::live_fill_tap::{LiveFillTap, TapVolume, live_fill_tap_ring};
use tickvault_app::dhan_feed_stack::{LiveIngest, counters, drain_main_feed_frame};
use tickvault_app::live_fill::chain::REASON_LATE_FILL_PRICE;
use tickvault_app::live_fill::chain::{
    ChainOutput, ChainParams, ChainSetup, ContractSpec, LiveFillChain, PaperCmd,
};
use tickvault_app::live_fill::taint::TaintReason;
use tickvault_app::live_fill::{CounterPublisher, SendDrops, paper_rows, send_output};
use tickvault_common::feed::Feed;
use tickvault_common::tick_types::ParsedTick;
use tickvault_core::websocket::frame_ring::frame_ring;
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
/// Blocks one order's rows may take in the step that decides or finishes it
/// (its Decision / Built / Reject rows or Execute: owned text fields and
/// boxes). Per order, never per tick.
const ORDER_STEP_MAX_BLOCKS: u64 = 32;

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
    chain_with_lot(1)
}

/// Security ids of the extra keys (C) drives (LF-9 review, second pass):
/// an illiquid instrument whose dropped volume is discarded, one first tapped
/// after a loss inside the profiled phase, and one whose counter restart the
/// tap drops.
const ILLIQUID: u64 = 777;
const LATE_FIRST: u64 = 778;
const RESTARTS: u64 = 779;

/// The chain with every instrument's lot at `lot` shares.
fn chain_with_lot(lot: u32) -> LiveFillChain<ReferenceDouble> {
    let key = InstrumentKey {
        security_id: 2885,
        exchange_segment: 1,
    };
    let keys = [key.security_id, ILLIQUID, LATE_FIRST, RESTARTS].map(|security_id| InstrumentKey {
        security_id,
        exchange_segment: 1,
    });
    let recipe = serde_json::from_str(
        r#"{"schema":"tickvault-strategy-recipe/1","name":"toy","version":1,"status":"approved","approved_by":"test","approved_at_ist":"2026-11-12T08:00:00+05:30","brutex_commit":"0000000000000000000000000000000000000000","timeframes":["1m"],"settings":{},"backtest":{}}"#,
    )
    .expect("recipe");
    let mut contracts = HashMap::new();
    for k in keys {
        contracts.insert(
            k,
            ContractSpec {
                tick_paise: 5,
                lot_size: lot,
                symbol: "X".to_owned(),
            },
        );
    }
    let setup = ChainSetup {
        recipe,
        snapshot: None,
        engine_frames_secs: vec![60],
        universe: keys.to_vec(),
        contracts,
        trading_day: DAY / 86_400,
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

/// [`tap_at`] for instrument `security_id` with `drop_seq`.
fn tap_for(security_id: u64, secs: i64, cum: u32, drop_seq: u32) -> TapTick {
    let mut t = tap_at(secs, cum);
    t.security_id = security_id;
    t.drop_seq = drop_seq;
    t
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
        older_trade: false,
        volume_delta: 10,
        first_bucket_volume_delta: 10,
        cumulative_restarted: false,
        stale_packet: false,
        fold_seed: false,
        fold_gap_reseed: false,
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
    assert!(accept_tap.offer(&parsed, None, true, false, TapVolume::default()));
    let _ = accept_rx.try_recv();
    let _ = full_tap.offer(&parsed, None, true, false, TapVolume::default());
    let _ = full_tap.offer(&parsed, None, true, false, TapVolume::default());

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
    // LF-9 finding 5: the rarer per-tick and per-pass paths, interleaved
    // with the steady step: an older trade in the open bucket, a late tick
    // for a closed one, a dropped tick, a moved loss total, a feed gap, a
    // clock step, a release-queue overflow, backlogged and lagging passes.
    let mut loss_total: u64 = 0;
    let mut drop_seq: u32 = 0;
    let mut rare_calls: u64 = 0;
    let mut illiquid_cum: u32 = 1_000;
    let mut rare =
        |chain: &mut LiveFillChain<ReferenceDouble>, sec: &mut i64, cum: &mut u32, k: u64| {
            let s = *sec;
            rare_calls += 1;
            // LF-9 review (second pass), inside the profiled phase only (the
            // warm-up makes 9 calls): an instrument first tapped after a
            // loss, its fold delta below its cumulative (marked unknown), and
            // a counter restart on a tick the tap dropped (the next tick, below
            // the last tapped cumulative, carries it).
            match rare_calls {
                30 => chain.on_tap(&tap_for(LATE_FIRST, s, 5_000, drop_seq)),
                31 => {
                    let mut first = tap_for(RESTARTS, s, 3_000_000_000, drop_seq);
                    first.fold_seed = true;
                    chain.on_tap(&first);
                }
                32 => {
                    drop_seq = drop_seq.wrapping_add(1);
                    chain.on_tap(&tap_for(RESTARTS, s, 1_000, drop_seq));
                }
                _ => {}
            }
            let mut older = tap_at(s, cum.wrapping_sub(5));
            older.new_trade = false;
            older.older_trade = true;
            older.volume_delta = 0;
            older.first_bucket_volume_delta = 0;
            chain.on_tap(&older);
            let mut late = tap_at(s - 130, *cum);
            late.receipt_ist_ns = s * NS + 400_000_000;
            late.volume_delta = 0;
            late.first_bucket_volume_delta = 0;
            chain.on_tap(&late);
            match k % 6 {
                0 => {
                    // LF-9 review (second pass): the dropped-volume paths.
                    // The illiquid instrument trades, then the tap loses a
                    // tick of it the fold counted (100,000 shares, seen on its
                    // next repeat quote after the loss); its trade two
                    // minutes on, outside the drop's stretch, has them
                    // discarded and its day volume marked unknown.
                    // Its trade carries the steady stream's loss count (0), so
                    // it reads no new loss of its own.
                    illiquid_cum = illiquid_cum.wrapping_add(10);
                    chain.on_tap(&tap_for(ILLIQUID, s, illiquid_cum, 0));
                    drop_seq = drop_seq.wrapping_add(1);
                    illiquid_cum = illiquid_cum.wrapping_add(100_000);
                    let mut q = tap_for(ILLIQUID, s, illiquid_cum, drop_seq);
                    q.new_trade = false;
                    q.volume_delta = 0;
                    q.first_bucket_volume_delta = 0;
                    chain.on_tap(&q);
                    // The steady instrument's tick after the loss carries 1
                    // share of a dropped tick: added to its (tainted) bar.
                    *cum = cum.wrapping_add(11);
                    let mut t = tap_at(s, *cum);
                    t.drop_seq = drop_seq;
                    chain.on_tap(&t);
                }
                1 => {
                    loss_total += 1;
                    drop_seq = drop_seq.wrapping_add(1);
                    chain.note_tap_loss(loss_total);
                }
                2 => {
                    // Three silent seconds: a feed gap from the passes.
                    for g in 1..=3 {
                        let t = (s + g) * NS;
                        chain.on_pass(t, t - DAY * NS).expect("pass");
                    }
                    *sec += 3;
                }
                3 => {
                    // A wall step of one second against the monotonic clock.
                    let t = s * NS + 700_000_000;
                    chain.on_pass(t, t - DAY * NS - NS).expect("pass");
                }
                4 => {
                    // Forty seconds of ticks with no pass: the release queue
                    // overflows (8 bars a key, 4 keys), then backlogged passes.
                    for q in 0..40 {
                        *cum = cum.wrapping_add(10);
                        chain.on_tap(&tap_at(*sec + q, *cum));
                    }
                    *sec += 40;
                    let t = *sec * NS;
                    chain.on_pass_bounded(t, t - DAY * NS, true).expect("pass");
                }
                _ => {
                    // A tick whose receipt trails the wall: the drain is behind.
                    *cum = cum.wrapping_add(10);
                    let mut t = tap_at(s, *cum);
                    t.receipt_ist_ns = s * NS - 1_500_000_000;
                    chain.on_tap(&t);
                    let w = s * NS + 600_000_000;
                    chain.on_pass_bounded(w, w - DAY * NS, false).expect("pass");
                }
            }
        };
    for i in 0..180u64 {
        step(&mut chain, &mut sec, &mut cum);
        if i % 20 == 0 {
            rare(&mut chain, &mut sec, &mut cum, i / 20);
        }
    }
    assert!(
        chain.counters().bars_released > 100,
        "warm-up released bars"
    );
    assert_eq!(chain.counters().intents, 0, "a flat price asks for nothing");

    let marked_before = chain.counters().volume_unknown_marked;
    let _profiler = dhat::Profiler::builder().testing().build();

    // ---------------- (A) the tap ----------------
    let (a_bytes, a_blocks) = dhat_support::measure_with_phantom_retry(
        0,
        0,
        || {},
        || {
            for _ in 0..10_000 {
                std::hint::black_box(accept_tap.offer(
                    &parsed,
                    None,
                    true,
                    false,
                    TapVolume::default(),
                ));
                std::hint::black_box(full_tap.offer(
                    &parsed,
                    None,
                    true,
                    false,
                    TapVolume::default(),
                ));
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
            for i in 0..10_000u64 {
                step(&mut chain, &mut sec, &mut cum);
                if i % 20 == 0 {
                    rare(&mut chain, &mut sec, &mut cum, i / 20);
                }
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
    // Non-vacuity: every rare path ran.
    assert!(chain.counters().late_ticks > 100, "{:?}", chain.counters());
    assert!(
        chain.counters().bars_lost_queue_full > 0,
        "{:?}",
        chain.counters()
    );
    assert!(chain.counters().clock_steps > 0);
    let taints = chain.taints_added();
    assert!(
        taints[0] > 0 && taints[1] > 0 && taints[2] > 0,
        "{taints:?}"
    );
    // LF-9 review (second pass): the dropped-volume paths ran under the
    // profiler. Only the illiquid instrument's kept volume (100,000 shares
    // at a time) is ever discarded, so the steady instrument's (1 at a
    // time, under 100,000 in all) was never discarded, so it was carried
    // onto its tainted bar; two instruments were marked unknown inside the profiled phase (a
    // first tick after a loss, a dropped counter restart).
    let cc = chain.counters();
    assert!(
        cc.missed_volume_discarded > 0 && cc.missed_volume_discarded.is_multiple_of(100_000),
        "{cc:?}"
    );
    assert!(
        cc.volume_unknown_marked >= marked_before + 2,
        "{marked_before} -> {cc:?}"
    );

    // ---------------- (D) held orders ----------------
    let mut chain = held_order_chain();
    // Production mode (LF-9 hardening): the receiver is never busy-polled,
    // so every Mark takes `try_send`'s wake path (`notify_one`, no waiter
    // here: it stores a permit). (E1) measures the wake of a parked task.
    let (cmd_tx, mut cmd_rx) = frame_ring::<PaperCmd>(64);
    assert!(!cmd_rx.is_busy_polling());
    let (ev_tx, _ev_rx) = tokio::sync::mpsc::channel(1);
    let mut drops = SendDrops::default();
    let mut d = OrderDay::new();
    // Warm (before the profiler counts): two minutes, the engine's slots.
    for _ in 0..120 {
        d.second(&mut chain, &cmd_tx, &ev_tx, &mut drops, &mut cmd_rx);
    }
    let mut zero_steps = 0u64;
    let mut order_steps = 0u64;
    let mut worst_zero_step = 0u64;
    let mut worst_order_step_per_order = 0u64;
    for _ in 0..(60 * 60) {
        let moved_before = d.order_moves(&chain);
        let before = dhat::HeapStats::get();
        d.second(&mut chain, &cmd_tx, &ev_tx, &mut drops, &mut cmd_rx);
        let after = dhat::HeapStats::get();
        let blocks = after.total_blocks - before.total_blocks;
        let moved = d.order_moves(&chain) - moved_before;
        if moved == 0 {
            zero_steps += 1;
            worst_zero_step = worst_zero_step.max(blocks);
        } else {
            order_steps += 1;
            worst_order_step_per_order = worst_order_step_per_order.max(blocks.div_ceil(moved));
        }
    }
    let c = chain.counters();
    assert_eq!(
        worst_zero_step, 0,
        "a step that moved no order allocated {worst_zero_step} blocks ({c:?})"
    );
    assert!(
        worst_order_step_per_order <= ORDER_STEP_MAX_BLOCKS,
        "an order step allocated {worst_order_step_per_order} blocks per order (ceiling \
         {ORDER_STEP_MAX_BLOCKS})"
    );
    // Non-vacuity: orders were held, filled, sent and reported; held orders
    // were refused by each taint kind the driver makes (a dropped tick or a
    // moved loss total, a feed gap, a clock step) and by a late fill price,
    // counted from the chain's own Reject rows (LF-9 review, second pass:
    // the driver's own counts said only that it injected them); marks went
    // through the command ring. NOT covered here: an exit waiting for an
    // earlier held order of its instrument (`PendingBook::collect_ready`,
    // one O(64) scan, no allocation), which this one-frame strategy never
    // produces; its order is pinned by the fill_window and chain tests.
    assert!(
        zero_steps > 2_000 && order_steps > 20,
        "{zero_steps} / {order_steps}"
    );
    assert!(c.held > 20 && c.sent > 5, "{c:?}");
    assert!(c.position_reports > 5, "{c:?}");
    assert!(c.marks > 0 && d.marks_sent > 0, "{c:?}");
    assert!(d.tainted_by.iter().all(|n| *n > 0), "{:?}", d.tainted_by);
    let t = chain.taints_added();
    assert!(
        t[0] > 0 && t[1] > 0 && t[2] > 0,
        "every taint kind reached a held order: {t:?}"
    );
    assert!(
        c.refused > 0,
        "held orders were refused by those taints: {c:?}"
    );
    assert!(c.late_ticks > 0);
    for (i, r) in TaintReason::ALL.iter().enumerate() {
        if *r != TaintReason::ChainStart {
            assert!(
                d.refused_by[i] > 0,
                "a held order was refused {}: {:?}",
                r.as_str(),
                d.refused_by
            );
        }
    }
    assert!(
        d.late_fill_prints > 0 && d.late_fill_refused > 0,
        "a late print past the LIMIT refused a held order: {} prints, {} refused",
        d.late_fill_prints,
        d.late_fill_refused
    );
    eprintln!(
        "dhat_live_fill (D): {zero_steps} order-free steps at 0 blocks; {order_steps} order \
         steps, worst {worst_order_step_per_order} blocks per order; {c:?}"
    );

    // ---------------- (E) the counter publish and the command ring ----------------
    // The ring is read the way production reads it (LF-9 hardening): the
    // order runtime AWAITS `FrameRingReceiver::recv`, which clears the
    // busy-poll flag when it parks, so every Mark the chain thread sends
    // takes `try_send`'s wake path (`notify_one`) and wakes a task parked on
    // ANOTHER thread. A consumer thread runs a current-thread tokio runtime
    // parked in `recv()`, built before the profiler counts (its blocks
    // count too: dhat sees every thread). (E2) then fills a second ring that
    // nobody drains to its 64 slots, so `send_output`'s full branch (drop
    // counted, logged at powers of two) runs as well.
    let mut publisher = CounterPublisher::new(chain.counters(), chain.taints_added());
    let (wake_tx, mut wake_rx) = frame_ring::<PaperCmd>(64);
    assert!(
        !wake_rx.is_busy_polling(),
        "production mode: never busy-polled"
    );
    let consumed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let parked_ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let consumer = {
        let consumed = std::sync::Arc::clone(&consumed);
        let parked_ready = std::sync::Arc::clone(&parked_ready);
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("runtime");
            rt.block_on(async move {
                parked_ready.store(true, std::sync::atomic::Ordering::Release);
                while wake_rx.recv().await.is_some() {
                    consumed.fetch_add(1, std::sync::atomic::Ordering::Release);
                }
            });
        })
    };
    while !parked_ready.load(std::sync::atomic::Ordering::Acquire) {
        std::thread::yield_now();
    }
    // Warm the consumer's park / wake once before the profiler counts.
    let mark = || {
        ChainOutput::Cmd(PaperCmd::Mark {
            security_id: 2885,
            segment: tickvault_common::types::ExchangeSegment::NseEquity,
            price: 100.0,
        })
    };
    let _ = send_output(mark(), &wake_tx, &ev_tx, &mut drops);
    while consumed.load(std::sync::atomic::Ordering::Acquire) < 1 {
        std::thread::yield_now();
    }
    let (full_tx, mut full_rx) = frame_ring::<PaperCmd>(64);
    let cmds_dropped_before = drops.cmds;
    let mut sent_wake = 1usize;
    let (e_bytes, e_blocks) = dhat_support::measure_with_phantom_retry(
        0,
        0,
        || {},
        || {
            let mut c = chain.counters();
            let mut t = chain.taints_added();
            for i in 0..10_000u64 {
                c.refused += 1;
                c.late_ticks += 2;
                t[(i % 4) as usize] += 1;
                publisher.publish(c, t);
            }
            // (E1) each Mark wakes the consumer parked on its own thread.
            for _ in 0..2_000 {
                let _ = send_output(mark(), &wake_tx, &ev_tx, &mut drops);
                sent_wake += 1;
                while consumed.load(std::sync::atomic::Ordering::Acquire) < sent_wake {
                    std::hint::spin_loop();
                }
            }
            // (E2) a ring nobody drains: 64 accepted, the rest refused.
            for _ in 0..200 {
                for _ in 0..80 {
                    let _ = send_output(mark(), &full_tx, &ev_tx, &mut drops);
                }
                while full_rx.try_recv().is_some() {}
            }
        },
    );
    drop(wake_tx);
    consumer.join().expect("consumer");
    assert_eq!(
        (e_bytes, e_blocks),
        (0, 0),
        "the counter publish or the command ring allocated {e_bytes} bytes / {e_blocks} blocks"
    );
    assert!(
        consumed.load(std::sync::atomic::Ordering::Acquire) >= 2_001,
        "the parked consumer took every woken Mark"
    );
    assert!(
        drops.cmds - cmds_dropped_before >= 200 * 16,
        "the full branch ran: {} drops",
        drops.cmds - cmds_dropped_before
    );
}

/// A chain whose double asks for an order every minute (the price turns).
/// Lots of 100 shares against 10 shares a second of participation, so an
/// order is held for about ten seconds (its seconds settle one by one while
/// it waits, which the late fill price below needs).
fn held_order_chain() -> LiveFillChain<ReferenceDouble> {
    chain_with_lot(100)
}

/// Drives the held-order day one second at a time (see (D)).
struct OrderDay {
    sec: i64,
    cum: u32,
    drop_seq: u32,
    loss_total: u64,
    /// Orders refused by a dropped tick, a moved loss total, a feed gap and
    /// a clock step.
    tainted_by: [u64; 4],
    /// Held orders refused (Reject rows of `seq` 2), by [`TaintReason::ALL`]
    /// order, read from the chain's own rows (LF-9 review, second pass).
    refused_by: [u64; TaintReason::COUNT],
    /// Held orders refused `skip:late_fill_price`.
    late_fill_refused: u64,
    /// Late prints past a held order's LIMIT the driver sent.
    late_fill_prints: u64,
    marks_sent: u64,
    held_lots: i32,
    out: Vec<ChainOutput>,
}

impl OrderDay {
    fn new() -> Self {
        Self {
            sec: DAY + 33_300,
            cum: 0,
            drop_seq: 0,
            loss_total: 0,
            tainted_by: [0; 4],
            refused_by: [0; TaintReason::COUNT],
            late_fill_refused: 0,
            late_fill_prints: 0,
            marks_sent: 0,
            held_lots: 0,
            out: Vec::with_capacity(4_096),
        }
    }

    /// Orders decided or finished so far.
    fn order_moves(&self, chain: &LiveFillChain<ReferenceDouble>) -> u64 {
        let c = chain.counters();
        c.held + c.refused + c.sent + c.late_price_skipped
    }

    fn second(
        &mut self,
        chain: &mut LiveFillChain<ReferenceDouble>,
        cmd_tx: &tickvault_core::websocket::frame_ring::FrameRingSender<PaperCmd>,
        ev_tx: &tokio::sync::mpsc::Sender<
            tickvault_storage::live_order_events_persistence::LiveOrderEvent,
        >,
        drops: &mut SendDrops,
        cmd_rx: &mut tickvault_core::websocket::frame_ring::FrameRingReceiver<PaperCmd>,
    ) {
        let s = self.sec;
        let minute = (s - (DAY + 33_300)).div_euclid(60);
        let in_min = (s - (DAY + 33_300)).rem_euclid(60);
        // The price turns every minute: the double asks for an entry on a
        // rise and an exit on a fall.
        let px = if minute % 2 == 0 { 100.0 } else { 100.5 };
        // Which taint touches this minute's held order: one every twelve
        // minutes, in turn (a drop taints up to six minutes, so the minutes
        // between hold, fill and report clean orders). Minute 8 of each
        // twelve, never one right after a drained-behind minute (a multiple
        // of three, below): that minute's stretch refuses the next order
        // when it is decided, so with minute 10 no order was ever held when
        // the taint came and no held order was refused by it (LF-9 review,
        // second pass).
        let taint = if minute % 12 == 8 {
            ((minute / 12) % 4) as usize
        } else {
            usize::MAX
        };
        if taint == 2 && (3..6).contains(&in_min) {
            // A feed gap: three silent seconds, passes only.
            self.pass(chain, s, 0, false);
            self.drain(chain, cmd_tx, ev_tx, drops, cmd_rx);
            if in_min == 5 {
                self.tainted_by[2] += 1;
            }
            self.sec += 1;
            return;
        }
        self.cum = self.cum.wrapping_add(40);
        let mut t = tap_at(s, self.cum);
        t.ltp = px;
        t.volume_delta = 40;
        t.first_bucket_volume_delta = 40;
        if taint == 0 && in_min == 4 {
            self.drop_seq = self.drop_seq.wrapping_add(1);
            self.tainted_by[0] += 1;
        }
        t.drop_seq = self.drop_seq;
        if in_min == 10 && minute % 3 == 0 {
            // The drain is behind: this receipt trails the wall.
            t.receipt_ist_ns = s * NS - 1_500_000_000;
        }
        chain.on_tap(&t);
        let mut older = t;
        older.new_trade = false;
        older.older_trade = true;
        older.volume_delta = 0;
        older.first_bucket_volume_delta = 0;
        chain.on_tap(&older);
        if (minute % 12 == 5 || minute % 12 == 6) && in_min == 8 {
            // A late print in a settled second of the held order (decided
            // from the last minute's candle: an exit after a fall on an odd
            // minute, an entry after a rise on an even one) past its LIMIT:
            // `skip:late_fill_price` (LF-9 review, second pass).
            let mut late = tap_at(s - 4, self.cum);
            late.ltp = if minute % 2 == 1 { 70.0 } else { 130.0 };
            late.receipt_ist_ns = s * NS + 350_000_000;
            late.volume_delta = 0;
            late.first_bucket_volume_delta = 0;
            chain.on_tap(&late);
            self.late_fill_prints += 1;
        }
        if in_min == 20 {
            let mut late = tap_at(s - 90, self.cum);
            late.ltp = px;
            late.receipt_ist_ns = s * NS + 350_000_000;
            late.volume_delta = 0;
            late.first_bucket_volume_delta = 0;
            chain.on_tap(&late);
        }
        if taint == 1 && in_min == 4 {
            self.loss_total += 1;
            self.drop_seq = self.drop_seq.wrapping_add(1);
            chain.note_tap_loss(self.loss_total);
            self.tainted_by[1] += 1;
        }
        let step = if taint == 3 && in_min == 4 {
            self.tainted_by[3] += 1;
            NS
        } else {
            0
        };
        self.pass(chain, s, step, in_min % 15 == 7);
        self.drain(chain, cmd_tx, ev_tx, drops, cmd_rx);
        self.sec += 1;
    }

    fn pass(
        &mut self,
        chain: &mut LiveFillChain<ReferenceDouble>,
        s: i64,
        step: i64,
        backlog: bool,
    ) {
        let t = s * NS + 500_000_000;
        chain
            .on_pass_bounded(t, t - DAY * NS - step, backlog)
            .expect("pass");
        let t2 = (s + 1) * NS;
        chain.on_pass(t2, t2 - DAY * NS - step).expect("pass");
    }

    /// Marks go through the command ring (left to back up for a few
    /// seconds); every Execute's position report comes back as the paper
    /// runtime would book it (all of it).
    fn drain(
        &mut self,
        chain: &mut LiveFillChain<ReferenceDouble>,
        cmd_tx: &tickvault_core::websocket::frame_ring::FrameRingSender<PaperCmd>,
        ev_tx: &tokio::sync::mpsc::Sender<
            tickvault_storage::live_order_events_persistence::LiveOrderEvent,
        >,
        drops: &mut SendDrops,
        cmd_rx: &mut tickvault_core::websocket::frame_ring::FrameRingReceiver<PaperCmd>,
    ) {
        let out = &mut self.out;
        chain.drain_outputs(|o| out.push(o));
        let mut reports = [None; 8];
        let mut n = 0usize;
        for o in self.out.drain(..) {
            match o {
                ChainOutput::Cmd(PaperCmd::Mark { .. }) => {
                    self.marks_sent += 1;
                    let _ = send_output(o, cmd_tx, ev_tx, drops);
                }
                ChainOutput::Cmd(PaperCmd::Execute(x)) => {
                    let lot = i32::try_from(x.lot_size.max(1)).unwrap_or(1);
                    let lots = i32::try_from(x.filled_qty).unwrap_or(0) / lot;
                    self.held_lots += match x.side {
                        tickvault_trading::brutex_host::engine::Side::Buy => lots,
                        tickvault_trading::brutex_host::engine::Side::Sell => -lots,
                    };
                    if n < reports.len() {
                        reports[n] = Some(paper_rows::position_report(&x, self.held_lots));
                        n += 1;
                    }
                }
                ChainOutput::Event(e) => {
                    if e.seq == 2
                        && let Some(why) = e.reason.as_deref()
                    {
                        if why == REASON_LATE_FILL_PRICE {
                            self.late_fill_refused += 1;
                        }
                        for (i, r) in TaintReason::ALL.iter().enumerate() {
                            if why == r.as_str() {
                                self.refused_by[i] += 1;
                            }
                        }
                    }
                }
            }
        }
        for r in reports.iter().take(n).flatten() {
            chain.on_position_report(r);
        }
        if self.sec % 5 == 0 {
            while cmd_rx.try_recv().is_some() {}
        }
    }
}

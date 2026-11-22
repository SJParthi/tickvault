//! LF-6b: Criterion benchmarks for the live paper fill chain's per-tick paths.
//!
//! * `live_fill/tap_accept` — the drain's tap, accepted: one `offer` (build
//!   the 48-byte tick, `try_send` into the pre-allocated ring, no wake) plus
//!   the consumer's `try_recv` that keeps the ring from filling.
//! * `live_fill/tap_full` — the drop arm: a ring held full, `offer` counts
//!   the drop.
//! * `live_fill/tap_accept_wake_armed` — `tap_accept` with the chain
//!   thread's wake armed (§28.6, 2026-11-13): one more `bool` store; the
//!   wake itself runs once per frame (`flush_wake`), not here. Budget key
//!   `live_fill_tap` (substring-matches all three).
//! * `live_fill/wake_edge` — the idle-to-busy edge (§28.6 review): one
//!   `offer` plus the frame's `flush_wake` while a real chain thread is
//!   parked and asleep, so it pays the swap and the futex wake. In steady
//!   state this is at most one per FRAME, not rare. Timed with `iter_custom`
//!   (only the `offer` and `flush_wake`). NOT GATED: its
//!   name matches no budget key (a syscall is not held to the 100 ns tap
//!   budget), and it runs only when `TV_BENCH_WAKE_EDGE` is set, because
//!   `scripts/bench-gate.sh` applies its 5% regression arm to EVERY Criterion
//!   bench it finds, budgeted or not, and one futex wake plus a context
//!   switch on a shared runner swings far more than 5% (about 800 ns at
//!   10%). The nightly and post-merge runs never set it. Read the number:
//!   `TV_BENCH_WAKE_EDGE=1 cargo bench -p tickvault-app --bench live_fill -- wake_edge`.
//! * `live_fill/chain_on_tap` — one tick through the chain thread's
//!   `on_tap` (stream watch, candle book, instrument probe, engine `on_tick`)
//!   on 500 instruments × (1 s, 60 s), plus one pass per 500 ticks. Budget
//!   key `live_fill_chain`, per tick (500 per iteration, `[elements]`).
//!   The engine is the trading crate's `ReferenceDouble` TEST DOUBLE on a
//!   flat price (it asks for no order).
//!
//! Companion zero-alloc evidence: `tests/dhat_live_fill.rs`.
//!
//! Run: `cargo bench -p tickvault-app --bench live_fill`

use std::collections::HashMap;
use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

use tickvault_app::dhan_feed_stack::live_fill_tap::{
    ChainWake, LiveFillTap, TapTick, TapVolume, live_fill_tap_ring,
};
use tickvault_app::live_fill::chain::{ChainParams, ChainSetup, ContractSpec, LiveFillChain};
use tickvault_common::tick_types::ParsedTick;
use tickvault_trading::brutex_host::InstrumentKey;
use tickvault_trading::brutex_host::engine::ReferenceDouble;

const NS: i64 = 1_000_000_000;
/// IST midnight (a multiple of 86,400).
const DAY: i64 = 1_779_321_600;
const KEYS: u64 = 500;

fn bench_tap(c: &mut Criterion) {
    let parsed = ParsedTick {
        security_id: 7,
        exchange_segment_code: 2,
        last_traded_price: 10.0,
        exchange_timestamp: 1_786_800_000,
        received_at_nanos: 1,
        volume: 100,
        ..ParsedTick::default()
    };
    let (tx, mut rx) = live_fill_tap_ring(1_024);
    let mut tap = LiveFillTap::new(tx);
    c.bench_function("live_fill/tap_accept", |b| {
        b.iter(|| {
            black_box(tap.offer(black_box(&parsed), None, true, false, TapVolume::default()));
            black_box(rx.try_recv());
        });
    });
    // §28.6: the same accept with the chain thread's wake armed, the
    // common per-tick case: one more `bool` store (the wake is per frame).
    let (armed_tx, mut armed_rx) = live_fill_tap_ring(1_024);
    let mut armed = LiveFillTap::new(armed_tx).with_wake(std::sync::Arc::new(ChainWake::new()));
    c.bench_function("live_fill/tap_accept_wake_armed", |b| {
        b.iter(|| {
            black_box(armed.offer(black_box(&parsed), None, true, false, TapVolume::default()));
            black_box(armed_rx.try_recv());
        });
    });
    // Not gated (see the module header): run only on request, so the
    // nightly's regression arm never compares it.
    if std::env::var_os(WAKE_EDGE_ENV).is_some() {
        bench_wake_edge(c, &parsed);
    }
    let (full_tx, _full_rx) = live_fill_tap_ring(1);
    let mut full = LiveFillTap::new(full_tx);
    let _ = full.offer(&parsed, None, true, false, TapVolume::default());
    c.bench_function("live_fill/tap_full", |b| {
        b.iter(|| {
            black_box(full.offer(black_box(&parsed), None, true, false, TapVolume::default()))
        });
    });
}

/// Set (to anything) to run `live_fill/wake_edge`; unset in CI.
const WAKE_EDGE_ENV: &str = "TV_BENCH_WAKE_EDGE";

/// The idle-to-busy edge: a helper thread drains the ring and parks; each
/// timed `offer` finds it parked and asleep and pays the unpark syscall.
fn bench_wake_edge(c: &mut Criterion, parsed: &ParsedTick) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    let (tx, mut rx) = live_fill_tap_ring(1_024);
    let wake = Arc::new(ChainWake::new());
    let mut tap = LiveFillTap::new(tx).with_wake(Arc::clone(&wake));
    let stop = Arc::new(AtomicBool::new(false));
    let (w, s) = (Arc::clone(&wake), Arc::clone(&stop));
    let helper = std::thread::spawn(move || {
        w.register_current_thread();
        while !s.load(Ordering::Relaxed) {
            while rx.try_recv().is_some() {}
            w.park_unless(|| rx.is_empty(), Duration::from_millis(50));
        }
    });
    c.bench_function("live_fill/wake_edge", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                // Wait until the helper declared itself parked, then give it
                // time to fall asleep in the kernel.
                while !wake.is_parked() {
                    std::hint::spin_loop();
                }
                let settle = Instant::now();
                while settle.elapsed() < Duration::from_micros(30) {
                    std::hint::spin_loop();
                }
                let t0 = Instant::now();
                black_box(tap.offer(black_box(parsed), None, true, false, TapVolume::default()));
                tap.flush_wake();
                total += t0.elapsed();
            }
            total
        });
    });
    stop.store(true, Ordering::Relaxed);
    wake.wake();
    let _ = tap.offer(parsed, None, true, false, TapVolume::default());
    tap.flush_wake();
    let _ = helper.join();
}

fn chain() -> LiveFillChain<ReferenceDouble> {
    let universe: Vec<InstrumentKey> = (1..=KEYS)
        .map(|id| InstrumentKey {
            security_id: id,
            exchange_segment: 2,
        })
        .collect();
    let recipe = serde_json::from_str(
        r#"{"schema":"tickvault-strategy-recipe/1","name":"bench","version":1,"status":"approved","approved_by":"bench","approved_at_ist":"2026-11-12T08:00:00+05:30","brutex_commit":"0000000000000000000000000000000000000000","timeframes":["1m"],"settings":{},"backtest":{}}"#,
    )
    .expect("recipe");
    let contracts: HashMap<InstrumentKey, ContractSpec> = universe
        .iter()
        .map(|k| {
            (
                *k,
                ContractSpec {
                    tick_paise: 5,
                    lot_size: 1,
                    symbol: "X".to_owned(),
                },
            )
        })
        .collect();
    let setup = ChainSetup {
        recipe,
        snapshot: None,
        engine_frames_secs: vec![60],
        universe,
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

fn bench_chain(c: &mut Criterion) {
    let mut chain = chain();
    let mut sec = DAY + 33_300;
    let mut cum: u32 = 0;
    c.bench_function("live_fill/chain_on_tap", |b| {
        b.iter(|| {
            cum = cum.wrapping_add(10);
            for id in 1..=KEYS {
                chain.on_tap(black_box(&TapTick {
                    security_id: id,
                    receipt_ist_ns: sec * NS + 300_000_000,
                    ltp: 100.0,
                    exchange_secs: u32::try_from(sec).unwrap_or(u32::MAX),
                    cum_volume: cum,
                    best_bid: 99.95,
                    best_ask: 100.05,
                    best_bid_qty: 10,
                    best_ask_qty: 10,
                    drop_seq: 0,
                    segment: 2,
                    new_trade: true,
                    older_trade: false,
                    volume_delta: 10,
                    first_bucket_volume_delta: 10,
                    cumulative_restarted: false,
                    stale_packet: false,
                    fold_seed: false,
                    fold_gap_reseed: false,
                }));
            }
            let t = sec * NS + 500_000_000;
            chain.on_pass(t, t - DAY * NS).expect("pass");
            sec += 1;
            // Stay inside one IST day: a day roll reconfigures (cold).
            if sec >= DAY + 86_000 {
                sec = DAY + 33_300;
                chain = self::chain();
            }
        });
    });
}

criterion_group!(benches, bench_tap, bench_chain);
criterion_main!(benches);

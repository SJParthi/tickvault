//! LF-6b: Criterion benchmarks for the live paper fill chain's per-tick paths.
//!
//! * `live_fill/tap_accept` — the drain's tap, accepted: one `offer` (build
//!   the 48-byte tick, `try_send` into the pre-allocated ring, no wake) plus
//!   the consumer's `try_recv` that keeps the ring from filling.
//! * `live_fill/tap_full` — the drop arm: a ring held full, `offer` counts
//!   the drop.
//!   Budget key `live_fill_tap` (substring-matches both).
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

use tickvault_app::dhan_feed_stack::live_fill_tap::{LiveFillTap, TapTick, live_fill_tap_ring};
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
            black_box(tap.offer(black_box(&parsed), None, true));
            black_box(rx.try_recv());
        });
    });
    let (full_tx, _full_rx) = live_fill_tap_ring(1);
    let mut full = LiveFillTap::new(full_tx);
    let _ = full.offer(&parsed, None, true);
    c.bench_function("live_fill/tap_full", |b| {
        b.iter(|| black_box(full.offer(black_box(&parsed), None, true)));
    });
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

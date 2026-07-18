//! Benchmark: per-row moneyness classification + RAM chain-snapshot read.
//!
//! Operator directive 2026-07-14: moneyness is O(1) time + O(1) space +
//! zero allocation — this bench is the latency half of the proof (the
//! allocation half is `crates/core/tests/dhat_moneyness.rs`).
//!
//! Budget: `moneyness = 50` ns in `quality/benchmark-budgets.toml` — the
//! same tier as `token_handle/load` (the arc-swap hot-read class the
//! snapshot read is) and `registry_get`.
//!
//! - `moneyness/classify` — the per-row two-step-API classify (one 2-arm
//!   label parse + three integer compares).
//! - `moneyness/snapshot_read` — one lock-free arc-swap load of the
//!   process-global chain snapshot (the future decision surface).

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use tickvault_common::feed::Feed;
use tickvault_common::moneyness::{Moneyness, OptionLeg, classify_moneyness_paise};
use tickvault_core::pipeline::chain_snapshot::{
    ChainMoneynessSnapshot, ChainUnderlying, SnapshotRow, load_chain_snapshot,
    publish_chain_snapshot,
};

fn bench_classify(c: &mut Criterion) {
    // The 2026-04-21 live-capture anchor pair: NIFTY spot 24536.40,
    // grid ATM 24550 (paise).
    c.bench_function("moneyness/classify", |b| {
        b.iter(|| {
            let m = classify_moneyness_paise(
                black_box("CE"),
                black_box(2_450_000),
                black_box(2_453_640),
                black_box(2_455_000),
            );
            black_box(m);
        });
    });
}

fn bench_snapshot_read(c: &mut Criterion) {
    // One-time publish of a worst-case-sized snapshot (400 strikes × 2
    // legs = 800 rows) OUTSIDE the measured region — the read latency
    // must be independent of the row count (pointer load, no scan).
    let rows: Vec<SnapshotRow> = (0..800_i64)
        .map(|i| SnapshotRow {
            strike_paise: 2_000_000 + (i / 2) * 5_000,
            ltp_paise: 10_000,
            leg: if i % 2 == 0 {
                OptionLeg::Ce
            } else {
                OptionLeg::Pe
            },
            moneyness: Moneyness::Otm,
        })
        .collect();
    publish_chain_snapshot(ChainMoneynessSnapshot {
        feed: Feed::Dhan,
        underlying: ChainUnderlying::Nifty,
        minute_ts_ist_nanos: 1_770_000_900_000_000_000,
        fetched_at_ist_nanos: 1_770_000_901_500_000_000,
        underlying_spot: 24_536.40,
        underlying_spot_paise: 2_453_640,
        atm_strike_paise: 2_455_000,
        expiry_ist_nanos: 1_770_508_800_000_000_000,
        spot_missing: false,
        rows,
    });
    // Warm the arc-swap hazard/debt slot for this thread (one-time).
    let _warm = load_chain_snapshot(Feed::Dhan, ChainUnderlying::Nifty);

    c.bench_function("moneyness/snapshot_read", |b| {
        b.iter(|| {
            let guard =
                load_chain_snapshot(black_box(Feed::Dhan), black_box(ChainUnderlying::Nifty));
            black_box(guard.atm_strike_paise);
        });
    });
}

criterion_group!(benches, bench_classify, bench_snapshot_read);
criterion_main!(benches);

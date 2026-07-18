//! Moneyness (2026-07-14) — DHAT zero-alloc test for the per-row
//! classify (`tickvault_common::moneyness::classify_moneyness_paise`) and
//! the RAM chain-snapshot read (`chain_snapshot::load_chain_snapshot`).
//!
//! The operator contract: O(1) time + O(1) space + ZERO allocation for
//! both the per-row classification and the future decision-surface read.
//! The one-time snapshot publish (the caller-built rows Vec + one Arc) is
//! the documented cold-path cost and happens OUTSIDE the profiler window
//! below. A regression that adds `.clone()`, `format!()`, or a locking
//! map to either path would blow the budget immediately across 10,000 +
//! 10,000 calls.
//!
//! Budget: ≤ 1 KiB / ≤ 8 blocks across the whole measured region — the
//! same class as `dhat_feed_presence.rs` (the
//! loop delta itself must be 0 allocations; the budget headroom absorbs
//! only cross-thread phantom noise per dhat_support).
//!
//! CI ENFORCEMENT: deliberately UN-gated (no `#![cfg(feature = "dhat")]`)
//! — the house pattern — so the normal Test (core) nextest lane runs it on
//! every PR; the Coverage lane skips it BY TEST NAME (the `dhat_` prefix).
//!
//! Run: `cargo test -p tickvault-core --test dhat_moneyness`

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use tickvault_common::feed::Feed;
use tickvault_common::moneyness::{Moneyness, OptionLeg, classify_moneyness_paise};
use tickvault_core::pipeline::chain_snapshot::{
    ChainMoneynessSnapshot, ChainUnderlying, SnapshotRow, load_chain_snapshot,
    publish_chain_snapshot,
};

mod dhat_support;

#[test]
fn dhat_moneyness_classify_and_snapshot_read_zero_alloc() {
    // One-time publish of a worst-case-sized snapshot (400 strikes × 2
    // legs = 800 rows) OUTSIDE the profiler window — the documented
    // cold-path allocation (rows Vec + one Arc).
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
    // Warm both paths once (the arc-swap hazard/debt slot for this thread
    // is a one-time lazy init, not a per-call cost).
    let warm = load_chain_snapshot(Feed::Dhan, ChainUnderlying::Nifty);
    std::hint::black_box(warm.atm_strike_paise);
    std::hint::black_box(classify_moneyness_paise(
        "CE", 2_450_000, 2_453_640, 2_455_000,
    ));

    let _profiler = dhat::Profiler::builder().testing().build();

    const BUDGET_BYTES: u64 = 1024;
    const BUDGET_BLOCKS: u64 = 8;

    let (new_bytes, new_blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || {
            // 10,000 per-row classifies across both legs + walking strikes.
            for i in 0..10_000_i64 {
                let strike_paise = 2_400_000 + (i % 100) * 5_000;
                let leg = if i % 2 == 0 { "CE" } else { "PE" };
                let m = classify_moneyness_paise(
                    std::hint::black_box(leg),
                    std::hint::black_box(strike_paise),
                    std::hint::black_box(2_453_640),
                    std::hint::black_box(2_455_000),
                );
                std::hint::black_box(m);
            }
            // 10,000 lock-free snapshot reads (the decision surface).
            for i in 0..10_000_u32 {
                let guard = load_chain_snapshot(
                    std::hint::black_box(Feed::Dhan),
                    std::hint::black_box(ChainUnderlying::Nifty),
                );
                std::hint::black_box(guard.atm_strike_paise);
                std::hint::black_box(guard.rows.len());
                std::hint::black_box(i);
            }
        },
    );

    assert!(
        new_bytes <= BUDGET_BYTES && new_blocks <= BUDGET_BLOCKS,
        "moneyness hot path allocated: {new_bytes} bytes / {new_blocks} \
         blocks across 10,000 classifies + 10,000 snapshot reads (budget \
         {BUDGET_BYTES} B / {BUDGET_BLOCKS} blocks) — an allocation or a \
         lock entered classify_moneyness_paise or load_chain_snapshot"
    );
}

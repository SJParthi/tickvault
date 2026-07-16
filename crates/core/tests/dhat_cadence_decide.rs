//! Cadence (2026-07-14) — DHAT zero-alloc test for the event-driven
//! decide pass (design §6/§11): per pass, ONE `atm_strike_paise` anchor
//! resolution per underlying + `load_chain_snapshot` × 6 slots (both
//! feeds × 3 underlyings) + the `fold_chain_moneyness` per-row classify
//! fold — the exact read path the cadence decision finalize runs the
//! instant a lane's predicate completes.
//!
//! The one-time registry publishes (6 caller-built rows Vecs + one Arc
//! each) are the documented cold-path cost and happen OUTSIDE the
//! profiler window. A regression that adds `.clone()`, `format!()`, or a
//! locking map to the decide path would blow the budget immediately
//! across 10,000 passes.
//!
//! Budget: ≤ 8 KiB / ≤ 16 blocks across the whole measured region — the
//! loop delta itself must be 0 allocations; the headroom absorbs only
//! cross-thread phantom noise per dhat_support (the 2026-07-09
//! roaming-phantom lesson; wider than dhat_moneyness's because this
//! region runs ~60× more per-row work per pass).
//!
//! CI ENFORCEMENT: deliberately UN-gated (no `#![cfg(feature = "dhat")]`)
//! — the house pattern — so the normal Test (core) nextest lane runs it
//! on every PR; the Coverage lane skips it BY TEST NAME (the `dhat_`
//! prefix). NOTE for the ci.yml dhat drift list: add
//! `dhat_cadence_decide` alongside `dhat_moneyness` when the list is next
//! regenerated.
//!
//! Run: `cargo test -p tickvault-core --test dhat_cadence_decide`

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use tickvault_common::feed::Feed;
use tickvault_common::moneyness::{Moneyness, OptionLeg, atm_strike_paise, strike_step_paise};
use tickvault_core::cadence::assembly::fold_chain_moneyness;
use tickvault_core::pipeline::chain_snapshot::{
    ChainMoneynessSnapshot, ChainUnderlying, SnapshotRow, publish_chain_snapshot,
};

mod dhat_support;

/// Per-underlying anchor fixtures (spot paise on/near the #1540 grid).
const SPOTS_PAISE: [i64; 3] = [2_453_640, 5_102_500, 8_100_200];

fn seed_slot(feed: Feed, underlying: ChainUnderlying, spot_paise: i64) {
    let rows: Vec<SnapshotRow> = (0..100_i64)
        .map(|i| SnapshotRow {
            strike_paise: spot_paise - 250_000 + (i / 2) * 5_000,
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
        feed,
        underlying,
        minute_ts_ist_nanos: 1_770_000_900_000_000_000,
        fetched_at_ist_nanos: 1_770_000_901_500_000_000,
        underlying_spot: 0.0,
        underlying_spot_paise: spot_paise,
        atm_strike_paise: 0,
        expiry_ist_nanos: 1_770_508_800_000_000_000,
        spot_missing: false,
        rows,
    });
}

#[test]
fn dhat_cadence_decide_pass_zero_alloc() {
    // Pre-seed ALL 6 registry slots OUTSIDE the profiler window (the
    // documented cold-path publish allocation).
    for feed in [Feed::Dhan, Feed::Groww] {
        for (i, u) in ChainUnderlying::ALL.iter().enumerate() {
            seed_slot(feed, *u, SPOTS_PAISE[i]);
        }
    }
    // Warm the paths once (arc-swap per-thread lazy init is one-time).
    let warm = fold_chain_moneyness(
        Feed::Dhan,
        ChainUnderlying::Nifty,
        SPOTS_PAISE[0],
        2_455_000,
    );
    std::hint::black_box(warm.rows);

    let _profiler = dhat::Profiler::builder().testing().build();

    const BUDGET_BYTES: u64 = 8 * 1024;
    const BUDGET_BLOCKS: u64 = 16;

    let (new_bytes, new_blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || {
            // 10,000 decide passes: per pass, anchor resolution per
            // underlying (strike_step_paise → atm_strike_paise) + the
            // 6-slot load+fold the decision finalize runs.
            for pass in 0..10_000_u32 {
                for feed in [Feed::Dhan, Feed::Groww] {
                    for (i, u) in ChainUnderlying::ALL.iter().enumerate() {
                        let spot = std::hint::black_box(SPOTS_PAISE[i]);
                        let atm = strike_step_paise(u.as_str())
                            .and_then(|step| atm_strike_paise(spot, step))
                            .unwrap_or(0);
                        let fold = fold_chain_moneyness(
                            std::hint::black_box(feed),
                            std::hint::black_box(*u),
                            spot,
                            std::hint::black_box(atm),
                        );
                        std::hint::black_box(fold.rows);
                        std::hint::black_box(fold.unknown);
                    }
                }
                std::hint::black_box(pass);
            }
        },
    );

    assert!(
        new_bytes <= BUDGET_BYTES && new_blocks <= BUDGET_BLOCKS,
        "cadence decide path allocated: {new_bytes} bytes / {new_blocks} \
         blocks across 10,000 six-slot decide passes (budget \
         {BUDGET_BYTES} B / {BUDGET_BLOCKS} blocks) — an allocation or a \
         lock entered the anchor/load/fold decide chain"
    );
}

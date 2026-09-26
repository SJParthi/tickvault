//! Audit PR4 (2026-09-26) — DHAT zero-alloc test for the SLICED top-volume
//! sweep: the O(1) swap the drain's cadence arm now pays
//! (`VolumeLeaderboard::begin_sweep`) and the bounded steps the idle arm
//! runs after it (`sweep_step`, `SliceSort::step`, `GainerWalk::step`).
//!
//! Every buffer is sized once, outside the measured window. Inside it, a
//! thousand full sweeps — trades, swap, collect, sort, gainer walk — must
//! allocate nothing: the swap exchanges two pre-sized vectors, the sort merges
//! through a pre-sized second buffer, and the walk reuses its memo.
//!
//! Budget: ≤ 1 KiB / ≤ 8 blocks across 1,000 sweeps — the house class.
//!
//! Scope, stated so it is not read as more: this covers Collect, Sort and the
//! gainer walk. It does NOT drive the Publish phase (the candidate lists are
//! published as fresh `Arc`s, once per job) or the Project phase (rows stream
//! into the writer's pre-sized staging vector through `project_snapshot_with`,
//! which needs a `LiveIngest` to exercise).
//!
//! CI ENFORCEMENT: un-gated, so the normal Test (app) lane runs it; the
//! Coverage lane skips it by the `dhat_` name prefix.
//!
//! Run: `cargo test -p tickvault-app --test dhat_top_volume_sweep`

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use tickvault_app::top_volume_sweep::{SliceSort, TOP_VOLUME_SWEEP_STEP_ROWS};
use tickvault_app::volume_leaderboard::{
    GainerVerdict, GainerWalk, MAX_TRACKED_CONTRACTS, OptionFamily, RankedContract,
    VolumeLeaderboard, board_order,
};
use tickvault_common::types::ExchangeSegment;
use tickvault_storage::top_volume_rank_persistence::SnapshotCadence;

mod dhat_support;

const BUDGET_BYTES: u64 = 1024;
const BUDGET_BLOCKS: u64 = 8;
const CONTRACTS: u64 = 2_000;

fn contract(id: u64, volume: u32) -> RankedContract {
    RankedContract {
        security_id: id,
        segment: ExchangeSegment::NseFno,
        underlying_id: id % 50,
        volume,
        window_lots_milli: 0,
        delta_units: 0,
        lot_size: 1,
    }
}

#[test]
fn dhat_top_volume_sweep_begin_and_steps_zero_allocation() {
    let mut lb = VolumeLeaderboard::new();
    for id in 0..CONTRACTS {
        let _ = lb.observe(contract(id, 1_000), OptionFamily::Stock);
    }
    let mut rows: Vec<RankedContract> = Vec::with_capacity(MAX_TRACKED_CONTRACTS);
    let mut sorter = SliceSort::with_capacity(MAX_TRACKED_CONTRACTS);
    let mut walk = GainerWalk::with_capacity(300);
    let mut round: u32 = 0;
    let mut sweep = |lb: &mut VolumeLeaderboard,
                     rows: &mut Vec<RankedContract>,
                     sorter: &mut SliceSort<RankedContract>,
                     walk: &mut GainerWalk,
                     round: &mut u32| {
        *round += 1;
        for id in 0..CONTRACTS {
            let volume = 1_000 + *round * (1 + (id as u32 % 97));
            let _ = lb.observe(contract(id, volume), OptionFamily::Stock);
        }
        rows.clear();
        let _ = lb.begin_sweep(OptionFamily::Stock, SnapshotCadence::OneSecond);
        while !lb.sweep_step(
            OptionFamily::Stock,
            SnapshotCadence::OneSecond,
            TOP_VOLUME_SWEEP_STEP_ROWS,
            |c| Some(c.lot_size.max(1)),
            |_| true,
            rows,
        ) {}
        sorter.reset();
        while !sorter.step(rows, TOP_VOLUME_SWEEP_STEP_ROWS, board_order) {}
        walk.reset(300);
        while !walk.step(rows, TOP_VOLUME_SWEEP_STEP_ROWS, |u| {
            if u % 2 == 0 {
                GainerVerdict::Gainer
            } else {
                GainerVerdict::NotGainer
            }
        }) {}
        std::hint::black_box(walk.gainers().len());
    };
    // Warm: the first sweep sizes the gainer memo.
    sweep(&mut lb, &mut rows, &mut sorter, &mut walk, &mut round);
    assert_eq!(rows.len(), CONTRACTS as usize, "every contract traded");

    let _profiler = dhat::Profiler::builder().testing().build();
    let (bytes, blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || {
            for _ in 0..1_000 {
                sweep(&mut lb, &mut rows, &mut sorter, &mut walk, &mut round);
            }
        },
    );
    assert!(
        bytes <= BUDGET_BYTES && blocks <= BUDGET_BLOCKS,
        "the sliced top-volume sweep allocated {bytes} bytes / {blocks} blocks over 1,000 \
         sweeps (budget {BUDGET_BYTES} B / {BUDGET_BLOCKS} blocks) — the swap, the steps and \
         the sliced sort must reuse their pre-sized buffers"
    );
    assert_eq!(rows.len(), CONTRACTS as usize);
    assert!(rows.windows(2).all(|w| board_order(&w[0], &w[1]).is_le()));
}

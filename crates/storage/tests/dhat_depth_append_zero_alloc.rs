//! DHAT zero-alloc ratchet for `DepthWriter::append_row` — the highest-frequency
//! persistence call in the entire live lane.
//!
//! ## Why this specific function needs its own budget
//!
//! Arithmetic first, because it is the whole argument. Depth-20 emits a FIXED
//! 20 levels per side, so one instrument update is 40 rows; depth-200 emits up
//! to 200 levels per side, so one update is up to 400 rows. Across the
//! authorized pools that is roughly **10,200 rows per update cycle** — two
//! orders of magnitude above the tick path's one-row-per-packet, against the
//! same drain task and the same per-frame budget.
//!
//! A single heap allocation per row therefore costs ~10,200 allocations per
//! update cycle. On the tick path the same mistake costs one. That asymmetry is
//! why this file exists separately from the tick ratchets rather than being
//! assumed covered by them.
//!
//! ## Why it was missing until 2026-08-15
//!
//! A test-quality audit of the depth vertical found DHAT coverage stopped at
//! the PARSER: `crates/core/tests/dhat_depth_packet_zero_alloc.rs` measures
//! `parse_depth_packet` + `split_depth_frame`, and nothing measured the half
//! that actually writes. The parse half allocates nothing because it borrows a
//! caller-owned `DepthLevelBuffer`; the persist half is where a `format!` or a
//! `to_string()` would plausibly be added under pressure, because that is where
//! the labels live.
//!
//! ## The specific regression this pins
//!
//! `append_row` writes four SYMBOL columns — `segment`, `depth_kind`, `side`,
//! `feed` — every one of which is a `&'static str` from a closed set, routed
//! through `sanitize_ilp_symbol`. That helper returns `Cow::Borrowed` for clean
//! input, so no `String` is built. Two ways that breaks:
//!
//!   - a future column takes an OWNED `String` (e.g. a formatted instrument
//!     label), which allocates per ROW rather than per frame;
//!   - `sanitize_ilp_symbol` starts returning `Cow::Owned` for input it
//!     currently passes through, silently converting every symbol write into an
//!     allocation.
//!
//! Neither shows up in a row count, a test assertion, or a code review diff of
//! the call site. A budget that fails the build is the only thing that catches
//! either.
//!
//! Budget: <= 1 KiB / <= 8 blocks across 10,000 appends — the same class as
//! `dhat_ws_lag.rs` and `dhat_token_handle.rs`. Note the ratio: 10,000 appends
//! at ONE allocation each would be 10,000 blocks, ~1,250x the budget, so a
//! per-row regression cannot squeak under it.
//!
//! CI ENFORCEMENT: deliberately UN-gated (no `#![cfg(feature = "dhat")]`), the
//! house pattern, so the normal Test (storage) lane runs it on every PR; the
//! Coverage lane skips it by the `dhat_` name prefix.
//!
//! Run: `cargo test -p tickvault-storage --test dhat_depth_append_zero_alloc`

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use tickvault_common::feed::Feed;
use tickvault_storage::depth_persistence::{
    DEPTH_KIND_20, DEPTH_KIND_200, DEPTH_SIDE_ASK, DEPTH_SIDE_BID, DepthRow, DepthWriter,
};

mod dhat_support;

const BUDGET_BYTES: u64 = 1024;
const BUDGET_BLOCKS: u64 = 8;

fn row(level: i64, depth_kind: &'static str, side: &'static str) -> DepthRow {
    DepthRow {
        security_id: 13,
        segment: "IDX_I",
        depth_kind,
        side,
        level,
        price: 25_000.5 + level as f64,
        quantity: 100 * level,
        orders: level,
        capture_seq: 556_007_424 + level,
        ts_nanos: 1_779_355_000_000_000_000,
    }
}

#[test]
fn dhat_depth_append_row_is_zero_allocation_across_every_label_combination() {
    // ONE profiler per process (dhat constraint; house pattern is one #[test]
    // per dhat file). All four label combinations are measured in the same
    // window, because the four SYMBOL writes are exactly what could regress
    // and covering only one of them would leave three free to.
    let mut writer = DepthWriter::for_test(Feed::Dhan);

    // Warm-up OUTSIDE the measured window. The ILP `Buffer` grows to its
    // steady-state capacity on first use; measuring that would pin one-time
    // buffer growth instead of the per-row cost this ratchet is about. 400
    // rows is one full depth-200 side, so the buffer reaches the capacity a
    // real update needs before the window opens.
    for i in 1..=400_i64 {
        writer
            .append_row(&row(i, DEPTH_KIND_200, DEPTH_SIDE_BID))
            .expect("warm-up append"); // APPROVED: test-only
    }

    let _profiler = dhat::Profiler::builder().testing().build();

    let (bytes, blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || {
            for i in 0..10_000_i64 {
                // Cycle every (depth_kind, side) pair, so a regression in ANY
                // of the four symbol writes lands inside the budget.
                let (kind, side) = match i % 4 {
                    0 => (DEPTH_KIND_20, DEPTH_SIDE_BID),
                    1 => (DEPTH_KIND_20, DEPTH_SIDE_ASK),
                    2 => (DEPTH_KIND_200, DEPTH_SIDE_BID),
                    _ => (DEPTH_KIND_200, DEPTH_SIDE_ASK),
                };
                // `level` cycles 1..=200 rather than growing without bound, so
                // the ILP buffer stays at its warmed capacity and the numbers
                // stay in the same digit-width band throughout.
                let _ = writer.append_row(&row((i % 200) + 1, kind, side));
                std::hint::black_box(i);
            }
        },
    );

    assert!(
        bytes <= BUDGET_BYTES && blocks <= BUDGET_BLOCKS,
        "DepthWriter::append_row allocated {bytes} bytes / {blocks} blocks over 10,000 \
         appends (budget {BUDGET_BYTES} B / {BUDGET_BLOCKS} blocks). At depth-200's 400 \
         rows per instrument update this is the highest-frequency persistence call in the \
         lane, so ONE allocation per row is ~10,200 allocations per update cycle. The \
         regressions this pins: a column that takes an owned String, or \
         `sanitize_ilp_symbol` returning Cow::Owned for input it currently passes through. \
         Keep every symbol a &'static str from a closed set."
    );
}

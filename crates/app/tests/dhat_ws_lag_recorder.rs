//! DHAT zero-alloc ratchet for the per-TICK WebSocket lag tap
//! (`dhan_feed_stack::record_ws_lag`) WITH THE PRODUCTION RECORDER INSTALLED.
//!
//! ## Why a second DHAT file for the same function
//!
//! `dhat_ws_lag.rs` runs with NO metrics recorder installed. With no recorder
//! every `metrics` handle is a no-op, so that test proves only that
//! `record_ws_lag` builds no label per call. It cannot see what the RECORDER
//! does with a sample — and the production recorder
//! (`metrics-exporter-prometheus` 0.18) stores every histogram sample in a
//! `metrics_util::storage::AtomicBucket`, a lock-free list of 64-slot blocks
//! (`BLOCK_SIZE = 64` on a 64-bit target) that allocates a fresh block each
//! time the tail fills (`AtomicBucket::push` → `Owned::new(Block::new())`).
//! The blocks are freed only when the exporter drains them at a scrape. So a
//! per-tick `Histogram::record` allocated about once every 64 ticks, on the
//! per-packet path, invisibly to the no-recorder test (audit finding M6).
//!
//! This file installs the SAME recorder type production installs
//! (`PrometheusBuilder::build_recorder`, same `_ms` bucket override) and
//! measures 10,000 calls per arm. Against the histogram form it fails at
//! ~156 blocks per arm (10,000 / 64); the fixed atomic bucket array
//! (`WsLagSlot` in `dhan_feed_stack.rs`) must allocate nothing.
//!
//! That the published lines match what a real histogram rendered is pinned
//! separately, by the unit test
//! `ws_lag_slot_renders_the_lines_a_prometheus_histogram_rendered`.
//!
//! CI ENFORCEMENT: un-gated, house pattern; the Coverage lane skips it by the
//! `dhat_` name prefix.
//!
//! Run: `cargo test -p tickvault-app --test dhat_ws_lag_recorder`

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};
use tickvault_app::dhan_feed_stack::record_ws_lag;
use tickvault_common::tick_types::ParsedTick;

mod dhat_support;

const BUDGET_BYTES: u64 = 1024;
const BUDGET_BLOCKS: u64 = 8;

const PLAUSIBLE_LTT_SECS: u32 = 1_700_000_000;
const IMPLAUSIBLE_LTT_SECS: u32 = 1;

/// The `_ms` bucket set production installs (`observability.rs`,
/// `API_MS_HISTOGRAM_BUCKETS`). Repeated here because the reference histogram
/// below must be built the way production built the old one; a drift between
/// the two is caught by `ws_lag_bucket_bounds_match_the_ms_histogram_buckets`.
const PROD_MS_BUCKETS: &[f64] = &[
    1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1_000.0, 2_500.0, 5_000.0, 10_000.0, 30_000.0,
    60_000.0,
];

fn tick(exchange_timestamp: u32) -> ParsedTick {
    ParsedTick {
        security_id: 13,
        exchange_timestamp,
        last_traded_price: 25_000.5,
        ..Default::default()
    }
}

fn received_after(exchange_timestamp: u32, lag_ms: i64) -> i64 {
    let utc_secs = i64::from(exchange_timestamp)
        - i64::from(tickvault_common::constants::IST_UTC_OFFSET_SECONDS);
    utc_secs * 1_000_000_000 + lag_ms * 1_000_000
}

#[test]
fn dhat_record_ws_lag_with_the_production_recorder_is_zero_allocation() {
    // Install the production recorder type BEFORE the first call, so every
    // handle `record_ws_lag` resolves is a real one (a handle resolved before
    // a recorder exists is a no-op forever).
    let recorder = PrometheusBuilder::new()
        .set_buckets_for_metric(Matcher::Suffix("_ms".to_owned()), PROD_MS_BUCKETS)
        .expect("valid bucket set")
        .build_recorder();
    metrics::set_global_recorder(recorder).expect("no recorder installed yet");

    let measured = tick(PLAUSIBLE_LTT_SECS);
    let recv = received_after(PLAUSIBLE_LTT_SECS, 250);
    let early_recv = recv - 2_000_000_000;
    let implausible = tick(IMPLAUSIBLE_LTT_SECS);

    // Warm-up outside every window: handle resolution and per-thread epoch
    // registration are one-time costs by design.
    for connection_index in 0..30_u8 {
        record_ws_lag(connection_index, &measured, recv);
        record_ws_lag(connection_index, &measured, early_recv);
        record_ws_lag(connection_index, &implausible, recv);
    }

    let _profiler = dhat::Profiler::builder().testing().build();

    let (m_bytes, m_blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || {
            for i in 0..10_000_u32 {
                record_ws_lag((i % 30) as u8, &measured, recv);
                std::hint::black_box(i);
            }
        },
    );
    assert!(
        m_bytes <= BUDGET_BYTES && m_blocks <= BUDGET_BLOCKS,
        "record_ws_lag MEASURED arm allocated {m_bytes} B / {m_blocks} blocks over 10,000 \
         calls WITH the production recorder (budget {BUDGET_BYTES} B / {BUDGET_BLOCKS} \
         blocks). A `metrics::Histogram::record` on this path allocates a 64-slot \
         AtomicBucket block about every 64 samples; record into a `WsLagSlot` instead."
    );

    let (c_bytes, c_blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || {
            for i in 0..10_000_u32 {
                record_ws_lag((i % 30) as u8, &measured, early_recv);
                std::hint::black_box(i);
            }
        },
    );
    assert!(
        c_bytes <= BUDGET_BYTES && c_blocks <= BUDGET_BLOCKS,
        "record_ws_lag CLAMPED-NEGATIVE arm allocated {c_bytes} B / {c_blocks} blocks over \
         10,000 calls WITH the production recorder (budget {BUDGET_BYTES} B / \
         {BUDGET_BLOCKS} blocks)."
    );

    let (x_bytes, x_blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || {
            for i in 0..10_000_u32 {
                record_ws_lag((i % 30) as u8, &implausible, recv);
                std::hint::black_box(i);
            }
        },
    );
    assert!(
        x_bytes <= BUDGET_BYTES && x_blocks <= BUDGET_BLOCKS,
        "record_ws_lag IMPLAUSIBLE-LTT arm allocated {x_bytes} B / {x_blocks} blocks over \
         10,000 calls WITH the production recorder (budget {BUDGET_BYTES} B / \
         {BUDGET_BLOCKS} blocks)."
    );
}

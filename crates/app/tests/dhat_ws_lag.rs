//! DHAT zero-alloc ratchet for the per-TICK WebSocket lag tap
//! (`dhan_feed_stack::record_ws_lag`).
//!
//! ## Why this test exists, specifically
//!
//! It exists because the function it measures SHIPPED BROKEN. The first cut
//! of `record_ws_lag` built its metric label with
//! `connection_index.to_string()`, which allocates a `String` — and, because a
//! non-literal label value drops the `metrics::histogram!` macro to its
//! `vec![Label::new(..)]` arm, a `Vec` as well. Two heap allocations per tick,
//! on the per-packet path, at the ~5,000 packet/sec envelope: roughly 36
//! million allocations an hour, on the exact path `dhan_feed_stack`'s own docs
//! promise is allocation-free.
//!
//! What makes that worth a dedicated ratchet rather than a code review note:
//! the warning was ALREADY THERE. `DrainCounters`, eight hundred lines above
//! the defect in the same file, exists solely to avoid this and says so in its
//! doc comment; `pool_supervisor.rs` and `parser/dispatcher.rs` repeat it. The
//! defect landed anyway, and the drain's complexity doc was edited the same day
//! while still asserting "No heap allocation in steady state". Three correct
//! comments did not stop it. A budget that fails the build will.
//!
//! ## What is measured
//!
//! Three arms, because the function has three exits and only one of them is
//! the happy path — a ratchet that covered only the measured arm would let the
//! two error arms regress silently, and error arms are exactly where a
//! `format!` gets added under pressure:
//!
//!   - **Measured**: a plausible timestamp with a real, non-negative lag.
//!     Records into the per-connection histogram handle.
//!   - **Clamped-negative**: whole-second vendor truncation makes a fast tick
//!     compute negative. Records 0.0 AND increments a counter — two handle
//!     lookups, still zero allocation.
//!   - **Implausible**: an LTT below the plausibility floor. Counter only.
//!
//! Out-of-range connection indices are included deliberately: the fold into
//! the last slot must not allocate a fresh key, since a hostile or
//! misconfigured index is precisely when the cheap path matters most.
//!
//! Budget: <= 1 KiB / <= 8 blocks across 10,000 calls per arm — the same class
//! as `dhat_mark_forward.rs` and `dhat_token_handle.rs`.
//!
//! CI ENFORCEMENT: deliberately UN-gated (no `#![cfg(feature = "dhat")]`), the
//! house pattern, so the normal Test (app) lane runs it on every PR; the
//! Coverage lane skips it by the `dhat_` name prefix.
//!
//! Run: `cargo test -p tickvault-app --test dhat_ws_lag`

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use tickvault_app::dhan_feed_stack::record_ws_lag;
use tickvault_common::tick_types::ParsedTick;

mod dhat_support;

const BUDGET_BYTES: u64 = 1024;
const BUDGET_BLOCKS: u64 = 8;

/// Inside the plausibility band the fold enforces, so the tick is measured
/// rather than excluded.
const PLAUSIBLE_LTT_SECS: u32 = 1_700_000_000;

/// Below the floor — the excluded arm.
const IMPLAUSIBLE_LTT_SECS: u32 = 1;

fn tick(exchange_timestamp: u32) -> ParsedTick {
    ParsedTick {
        security_id: 13,
        exchange_timestamp,
        last_traded_price: 25_000.5,
        ..Default::default()
    }
}

/// Receive instant for a tick that arrived ~250 ms after its exchange stamp.
fn received_late(exchange_timestamp: u32) -> i64 {
    // The vendor stamp is IST epoch seconds; the receive stamp is UTC nanos.
    // Building it from the same constant the function subtracts keeps this
    // test honest about the offset instead of hardcoding a magic number.
    let utc_secs = i64::from(exchange_timestamp)
        - i64::from(tickvault_common::constants::IST_UTC_OFFSET_SECONDS);
    utc_secs * 1_000_000_000 + 250_000_000
}

#[test]
fn dhat_record_ws_lag_all_arms_zero_allocation() {
    // ONE profiler per process (dhat constraint; house pattern is one #[test]
    // per dhat file) — all three arms measured sequentially under it.

    let measured = tick(PLAUSIBLE_LTT_SECS);
    let recv = received_late(PLAUSIBLE_LTT_SECS);
    // A receive instant EARLIER than the stamp: the clamped-negative arm that
    // whole-second vendor truncation produces on genuinely fast deliveries.
    let early_recv = recv - 2_000_000_000;
    let implausible = tick(IMPLAUSIBLE_LTT_SECS);

    // Warm-up OUTSIDE every measured window: the handle set is resolved on
    // first use (a `OnceLock`), and the metrics recorder registers each key
    // once. Both are one-time costs by design; measuring them would pin
    // lazy-init noise instead of the per-tick cost this ratchet is about.
    for connection_index in 0..20_u8 {
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
                // Cycles across all 16 slots AND past them, so the fold into
                // the last slot is inside the budget too.
                record_ws_lag((i % 20) as u8, &measured, recv);
                std::hint::black_box(i);
            }
        },
    );
    assert!(
        m_bytes <= BUDGET_BYTES && m_blocks <= BUDGET_BLOCKS,
        "record_ws_lag MEASURED arm allocated {m_bytes} bytes / {m_blocks} blocks over \
         10,000 calls (budget {BUDGET_BYTES} B / {BUDGET_BLOCKS} blocks). The regression \
         this pins is a label built per call — `to_string()` on the connection index, or \
         any `metrics::histogram!` with a non-literal label value, which drops the macro \
         to its allocating `vec![Label::new(..)]` arm. Resolve the handle once instead; \
         see WsLagHandles in dhan_feed_stack.rs."
    );

    let (c_bytes, c_blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || {
            for i in 0..10_000_u32 {
                record_ws_lag((i % 20) as u8, &measured, early_recv);
                std::hint::black_box(i);
            }
        },
    );
    assert!(
        c_bytes <= BUDGET_BYTES && c_blocks <= BUDGET_BLOCKS,
        "record_ws_lag CLAMPED-NEGATIVE arm allocated {c_bytes} bytes / {c_blocks} blocks \
         over 10,000 calls (budget {BUDGET_BYTES} B / {BUDGET_BLOCKS} blocks). This arm \
         touches TWO handles (histogram + excluded counter) and is the one a vendor \
         sending whole-second timestamps produces most often on a fast link — it must be \
         as cheap as the happy path, not merely correct."
    );

    let (x_bytes, x_blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || {
            for i in 0..10_000_u32 {
                record_ws_lag((i % 20) as u8, &implausible, recv);
                std::hint::black_box(i);
            }
        },
    );
    assert!(
        x_bytes <= BUDGET_BYTES && x_blocks <= BUDGET_BLOCKS,
        "record_ws_lag IMPLAUSIBLE-LTT arm allocated {x_bytes} bytes / {x_blocks} blocks \
         over 10,000 calls (budget {BUDGET_BYTES} B / {BUDGET_BLOCKS} blocks). A garbage \
         vendor timestamp arrives in BURSTS, so the exclusion path is the one most likely \
         to be hot during exactly the incident an operator is trying to diagnose."
    );
}

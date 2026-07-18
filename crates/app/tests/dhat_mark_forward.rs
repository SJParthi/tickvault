//! Order-runtime dry-run PR (2026-07-14) — DHAT zero-alloc test for the
//! mark-forward tap (`order_runtime::MarkForwarder::mark_forward`), re-homed
//! 2026-07-16 to the Groww per-minute REST legs (spot + contract own-fire
//! persist-confirm choke points) after the live bridge retired with #1581.
//!
//! The tap contract (cold per-minute seam today; the budgets below were
//! proven at the per-tick seam and are kept as ratchets):
//!   - DISARMED (the dominant path — no positions, no pending paper orders):
//!     ONE `Relaxed` atomic load and nothing else — ZERO heap allocation.
//!   - ARMED + channel FULL (backpressure drop arm): the bounded `try_send`
//!     of a 16-byte `Copy` struct fails without blocking; the drop counter
//!     is a static-key `metrics::counter!` — ZERO heap allocation after the
//!     one-time recorder/key warm-up. HP-5 honesty: this zero-alloc proof is
//!     measured against THIS test process's NO-OP recorder; under the
//!     production prometheus recorder each uncached `counter!` does a
//!     sharded registry read-lock lookup (no alloc post-registration, but
//!     not lock-free) — bounded to the drop arm only. Caching the Counter
//!     handle at construction was evaluated and REJECTED: the forwarder is
//!     built in main.rs BEFORE `observability::init_metrics` installs the
//!     recorder, so a construction-time handle would permanently bind the
//!     no-op recorder (the feed-stall first-sample-baseline trap class).
//!
//! HONEST COVERAGE: the armed+ACCEPTED arm is deliberately NOT in the DHAT
//! window — tokio's mpsc grows/reuses its 32-slot block list as the queue
//! depth changes, so accepted sends have an AMORTIZED (block-reuse) alloc
//! profile that is nondeterministic per-call; its latency is budgeted by the
//! Criterion bench (`benches/order_gate.rs`) instead. Both arms measured
//! here are strictly zero-alloc per call.
//!
//! Budget: ≤ 1 KiB / ≤ 8 blocks across 10,000 calls — same class as
//! `dhat_token_handle.rs` (core).
//!
//! CI ENFORCEMENT: deliberately UN-gated (no `#![cfg(feature = "dhat")]`) —
//! the house pattern — so the normal Test (app) nextest lane runs it on
//! every PR; the Coverage lane skips it BY TEST NAME (the `dhat_` prefix).
//!
//! Run: `cargo test -p tickvault-app --test dhat_mark_forward`

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tickvault_app::order_runtime::MarkForwarder;

mod dhat_support;

const BUDGET_BYTES: u64 = 1024;
const BUDGET_BLOCKS: u64 = 8;

#[test]
fn dhat_mark_forward_hot_arms_zero_allocation() {
    // ONE profiler per process (dhat constraint; house pattern = one #[test]
    // per dhat file) — both hot arms measured sequentially under it.

    // ---- Arm A: DISARMED (the dominant per-mark path) -----------------
    // One-time construction OUTSIDE the measured windows.
    let (disarmed_tx, _disarmed_rx) = tokio::sync::mpsc::channel(64);
    let disarmed = MarkForwarder {
        marks_wanted: Arc::new(AtomicBool::new(false)),
        tx: disarmed_tx,
    };
    // Warm once (nothing lazy on this arm, but keep the house shape).
    disarmed.mark_forward(13, 0, 25_000.0);

    // ---- Arm B: ARMED + channel FULL (backpressure drop arm) ----------
    // Tiny channel, FILLED during warm-up: every measured try_send takes
    // the Full/drop arm (counted, never blocking). The metrics recorder +
    // the static counter key warm up on the first drop, outside the window.
    let (armed_tx, armed_rx) = tokio::sync::mpsc::channel(4);
    let armed = MarkForwarder {
        marks_wanted: Arc::new(AtomicBool::new(true)),
        tx: armed_tx,
    };
    for i in 0..5_u32 {
        armed.mark_forward(u64::from(i), 0, 1.0);
    }
    assert!(
        armed.marks_wanted.load(Ordering::Relaxed),
        "gate must still be armed"
    );

    let _profiler = dhat::Profiler::builder().testing().build();

    let (a_bytes, a_blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || {
            for i in 0..10_000_u32 {
                disarmed.mark_forward(u64::from(i % 4), (i % 3) as u8, 100.5);
                std::hint::black_box(i);
            }
        },
    );
    assert!(
        a_bytes <= BUDGET_BYTES && a_blocks <= BUDGET_BLOCKS,
        "mark_forward DISARMED arm allocated {a_bytes} bytes / {a_blocks} blocks \
         over 10,000 calls (budget {BUDGET_BYTES} B / {BUDGET_BLOCKS} blocks) — the \
         mark gate must stay a single Relaxed load"
    );

    let (b_bytes, b_blocks) = dhat_support::measure_with_phantom_retry(
        BUDGET_BYTES,
        BUDGET_BLOCKS,
        || {},
        || {
            for i in 0..10_000_u32 {
                armed.mark_forward(u64::from(i % 4), (i % 3) as u8, 100.5);
                std::hint::black_box(i);
            }
        },
    );
    assert!(
        b_bytes <= BUDGET_BYTES && b_blocks <= BUDGET_BLOCKS,
        "mark_forward ARMED+FULL drop arm allocated {b_bytes} bytes / {b_blocks} \
         blocks over 10,000 calls (budget {BUDGET_BYTES} B / {BUDGET_BLOCKS} blocks) \
         — the backpressure drop must stay alloc-free (counter key is static)"
    );

    drop(armed_rx);
}

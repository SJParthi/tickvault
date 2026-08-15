//! DHAT zero-allocation gate for the **per-tick fold seam** —
//! `LiveIngest::ingest_tick_at`, the function every live tick passes through.
//!
//! # Why this test exists (plan Item 1)
//!
//! The 2026-08-14 audit found a heap allocation on this exact path: a
//! `to_string()` plus a `Vec` build inside the per-tick lag recorder, executing
//! once for **every tick on every socket**. It was removed in #1752.
//!
//! What matters more than that fix is *why nobody had caught it*. The workspace
//! has twelve DHAT gates, and **not one of them covered this seam.** They cover
//! packet decode, the aggregator fold, the registry, the reader — the pieces on
//! either side — while the app-side drain that joins them was measured by
//! nothing. An allocation could be reintroduced here tomorrow and every gate in
//! the repository would stay green.
//!
//! A fix without a gate is a fix with a shelf life. This is the gate.
//!
//! # Why the seam is measured rather than the whole drain
//!
//! `run_frame_drain` is `async` and owns a channel, a byte budget and a socket
//! supervisor; driving it under DHAT would measure tokio's block-list growth
//! (amortised and nondeterministic per call — the same reason
//! `dhat_mark_forward.rs` excludes its accepted-send arm) rather than our own
//! per-tick cost. `ingest_tick_at` is the part that runs per TICK rather than
//! per frame, it is where the removed allocation lived, and it is
//! deterministic. Measuring it is a real bound; measuring the whole loop would
//! be a noisy one.
//!
//! # Budget — and why it counts BLOCKS, not bytes
//!
//! The first draft of this gate budgeted bytes and failed immediately at ~1 MB
//! for 10,000 folds. That was the gate being wrong, not the code: this seam
//! appends each tick to the database row buffer, so **bytes are SUPPOSED to
//! grow with tick count**. A byte ceiling here would either be permanently red
//! or so loose it proved nothing.
//!
//! Blocks are the honest signal. The same 10,000 folds took **276 blocks** —
//! amortised growth (a handful of buffer doublings), not one allocation per
//! tick. A reintroduced per-tick `to_string()` would add ~10,000 blocks and be
//! impossible to miss, while adding only ~100 KB of bytes, which would have
//! hidden comfortably inside a byte budget.
//!
//! So: **≤ 512 blocks across 10,000 folds.** That is ~20× headroom over
//! today's 276 and still ~20× below a genuine per-tick regression. Bytes are
//! recorded in the failure message for diagnosis but are deliberately NOT
//! asserted.
//!
//! CI ENFORCEMENT: deliberately UN-gated (no `#![cfg(feature = "dhat")]`) — the
//! house pattern — so the normal Test (app) lane runs it on every PR, while the
//! Coverage lane skips it by the `dhat_` name prefix (coverage instrumentation
//! perturbs allocation counts).
//!
//! Run: `cargo test -p tickvault-app --test dhat_live_ingest_seam`

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use tickvault_app::dhan_feed_stack::LiveIngest;
use tickvault_common::feed::Feed;
use tickvault_common::tick_types::ParsedTick;
use tickvault_storage::tick_persistence::TickWriter;

/// Folds this many ticks inside the measured window.
const FOLDS: u64 = 10_000;

/// Ceilings. Deliberately generous in absolute terms and tight in SCALING
/// terms: 10,000 folds may not cost 10,000 allocations.
const MAX_BLOCKS: u64 = 512;

/// A tick shaped like a real Quote packet for one of the index instruments.
fn tick(security_id: u64, price: f32, exchange_ts: u32) -> ParsedTick {
    ParsedTick {
        security_id,
        exchange_segment_code: 0, // IDX_I
        last_traded_price: price,
        exchange_timestamp: exchange_ts,
        ..Default::default()
    }
}

#[test]
fn ingest_tick_seam_does_not_allocate_per_tick() {
    let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8);

    // WARM-UP, OUTSIDE the measured window. First touch of an instrument
    // allocates its aggregator slot and registers its metric keys — a real
    // one-time cost that must not be mistaken for a per-tick one. Measuring it
    // would make this gate fail for the wrong reason and teach the next reader
    // to raise the budget instead of finding the leak.
    for i in 0..4u64 {
        let _ = ingest.ingest_tick_at(&tick(13 + i, 100.0, 1_000), i as u64, 0, 1_000);
    }

    let profiler = dhat::Profiler::builder().testing().build();

    for n in 0..FOLDS {
        // Vary price and timestamp so the fold does real work — a constant
        // tick could be short-circuited and would prove nothing.
        let t = tick(
            13 + (n % 4),
            100.0 + (n % 97) as f32 * 0.05,
            1_000 + (n % 600) as u32,
        );
        let _ = ingest.ingest_tick_at(&t, n, (n % 3) as u32, 1_000 + n);
    }

    let stats = dhat::HeapStats::get();
    drop(profiler);

    assert!(
        stats.total_blocks <= MAX_BLOCKS,
        "PER-TICK ALLOCATION ON THE FOLD SEAM.\n\n\
         {} folds allocated {} blocks ({} bytes); the budget is {MAX_BLOCKS} \
         blocks.\n\n\
         Blocks, not bytes, because this seam appends every tick to the database \
         row buffer — bytes are SUPPOSED to grow with tick count. A block count \
         approaching the fold count means one allocation PER TICK, which is the \
         exact defect the 2026-08-14 audit found here: a `to_string()` and a \
         `Vec` build running once per tick per socket, on a seam no DHAT gate \
         covered.\n\n\
         If you are reading this, find the allocation — do NOT raise the \
         ceiling. This cost scales with tick volume, and at 4,565 instruments it \
         scales hard.",
        FOLDS,
        stats.total_blocks,
        stats.total_bytes
    );
}

#[test]
fn gate_is_not_vacuous() {
    // The budget above is an UPPER bound, which a loop that never ran would
    // satisfy trivially. This proves the seam does real work.
    //
    // It asserts on tracked state rather than on the returned outcome
    // deliberately. `IngestOutcome` has several non-`Folded` arms whose exact
    // preconditions (slot seeding, sequence representability) are the fold's
    // business, not this gate's; pinning one of them here would couple an
    // allocation test to fold semantics and break on unrelated changes. What
    // this gate needs to know is narrower and more durable: that ticks reached
    // the seam and it recorded them.
    let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8);
    assert_eq!(
        ingest.tracked_instruments(),
        0,
        "a fresh ingest must track nothing — otherwise the delta below proves nothing"
    );

    for i in 0..4u64 {
        let _ = ingest.ingest_tick_at(&tick(13 + i, 100.0 + i as f32, 1_000), i, 0, 1_000);
    }

    assert!(
        ingest.tracked_instruments() > 0,
        "after folding ticks for 4 instruments the seam tracks none — the ticks \
         are not reaching it, and the allocation budget above would pass while \
         measuring an empty loop"
    );
}

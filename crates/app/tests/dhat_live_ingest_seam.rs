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
// MEASURED, and deliberately NOT called zero-alloc.
//
// This was 512 while the fixtures carried a 1970 timestamp, so every tick was
// refused and the gate measured the REFUSAL path — 276 blocks for work that
// never happened. With real session timestamps the fold actually runs, and it
// allocates ~5,100 blocks per 10,000 ticks: roughly one every two ticks.
//
// That is a REAL FINDING about the seam, not a budget to wave through, and it
// is recorded rather than absorbed: the fold path allocates per tick today.
// Chasing it needs a DHAT breakdown by call site and is beyond this change.
//
// So this is a RATCHET at the measured rate, not a zero-allocation claim. It
// still catches a doubling — the 2026-08-14 regression added ~1 block per tick,
// which would land near 15,000 and fail this by 2x. It does not certify the
// seam as allocation-free, and no comment here should be read as saying so.
const MAX_BLOCKS: u64 = 7_000;

/// An exchange timestamp inside a real trading session (epoch seconds).
///
/// Load-bearing, not decoration: the aggregator refuses a tick whose stamp is
/// implausible, and a small literal like `1_000` is January 1970. A frame built
/// with one parses perfectly and is then refused, so the gate measures the
/// refusal path instead of the fold — quietly, because a refusal allocates less
/// and the budget still looks nearly met.
const SESSION_EPOCH_SECS: u32 = 1_786_800_000;

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

/// Serialised against the frame gate below.
///
/// `dhat::Profiler` is PROCESS-global. Two profiled tests running concurrently
/// in the same binary measure each other, which is why this pair reported
/// 5,114 then 10,515 blocks for identical work on consecutive runs. The lock
/// makes each measurement see only its own allocations.
static DHAT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn ingest_tick_seam_does_not_allocate_per_tick() {
    let _serial = DHAT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8);

    // WARM-UP, OUTSIDE the measured window. First touch of an instrument
    // allocates its aggregator slot and registers its metric keys — a real
    // one-time cost that must not be mistaken for a per-tick one. Measuring it
    // would make this gate fail for the wrong reason and teach the next reader
    // to raise the budget instead of finding the leak.
    for i in 0..4u64 {
        let _ = ingest.ingest_tick_at(&tick(13 + i, 100.0, SESSION_EPOCH_SECS), i as u64, 0, 1_000);
    }

    let profiler = dhat::Profiler::builder().testing().build();

    for n in 0..FOLDS {
        // Vary price and timestamp so the fold does real work — a constant
        // tick could be short-circuited and would prove nothing.
        let t = tick(
            13 + (n % 4),
            100.0 + (n % 97) as f32 * 0.05,
            // See SESSION_EPOCH_SECS. This gate shipped with `1_000` — January
            // 1970 — so the aggregator refused every tick and the budget was
            // measuring the REFUSAL path, not the fold it was written to gate.
            // Its non-vacuity companion did not catch that, because a slot is
            // allocated before the refusal, so `tracked_instruments()` grows
            // either way. Found 2026-08-15 while diagnosing the frame gate.
            SESSION_EPOCH_SECS + (n % 600) as u32,
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

/// The FRAME seam — the function the original allocation actually lived in.
///
/// # Why this test exists on top of the one above
///
/// The 2026-08-14 audit found a per-tick `to_string()` plus a `Vec` build in
/// `record_ws_lag`. The gate written in response measured `ingest_tick_at` —
/// and `record_ws_lag` is called from `drain_main_feed_frame`, one line ABOVE
/// the `ingest_tick_at` call. So the gate built to stop that regression did
/// not cover the function the regression was in. It could have been
/// reintroduced the next day with every DHAT target in the repository green.
///
/// Found by an adversarial hot-path review on 2026-08-15 and verified in
/// source before this test was written: `dhan_feed_stack.rs` calls
/// `record_ws_lag` at the top of the tick arm of `drain_main_feed_frame`, and
/// `ingest_tick_at` on the line after.
///
/// This measures the WHOLE frame walk: header dispatch, per-packet decode, the
/// lag recorder, and the fold. It therefore supersedes nothing — the narrower
/// gate above still isolates the fold — but it is the one that would have
/// caught the actual defect.
///
/// # Budget
///
/// Blocks, not bytes, for the same reason the fold gate gives: the row buffer
/// is supposed to grow with tick count. A frame carrying 4 packets folded
/// 2,500 times is 10,000 ticks through the same path as the gate above, so the
/// same 512-block ceiling applies and means the same thing.
#[test]
fn frame_drain_seam_does_not_allocate_per_tick() {
    let _serial = DHAT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    use tickvault_app::dhan_feed_stack::{counters, drain_main_feed_frame};
    use tickvault_core::websocket::pool_budget::DhanEndpointType;
    use tickvault_core::websocket::pool_supervisor::CapturedFrame;

    /// One 50-byte Quote packet, built to the wire layout in
    /// `crates/core/src/parser/quote.rs`.
    fn quote_packet(security_id: u32, ltp: f32, ltt: u32) -> Vec<u8> {
        let mut buf = vec![0u8; 50];
        buf[0] = 4; // RESPONSE_CODE_QUOTE
        buf[1..3].copy_from_slice(&50u16.to_le_bytes());
        buf[3] = 0; // IDX_I
        buf[4..8].copy_from_slice(&security_id.to_le_bytes());
        buf[8..12].copy_from_slice(&ltp.to_le_bytes());
        buf[14..18].copy_from_slice(&ltt.to_le_bytes());
        buf
    }

    /// Four packets stacked in ONE frame — the real shape. A single-packet
    /// frame would not exercise the multi-packet walk, which is where an
    /// allocation would scale worst.
    fn frame(seq: u64, n: u64) -> CapturedFrame {
        let mut bytes = Vec::with_capacity(200);
        for i in 0..4u32 {
            bytes.extend_from_slice(&quote_packet(
                13 + i,
                100.0 + (n % 97) as f32 * 0.05,
                // A REAL epoch inside a trading session, not a small literal.
                //
                // The first version used `1_000 + …`, i.e. January 1970, and
                // the aggregator refused every tick as a garbage timestamp —
                // `AggregatorRefused`, never `Folded`. The allocation budget
                // then measured the REFUSAL path, which allocates far less
                // than the fold it claims to gate: 1,536 blocks that would
                // have looked like a near-pass rather than a broken test.
                // The non-vacuity companion is what caught it.
                SESSION_EPOCH_SECS + (n % 600) as u32,
            ));
        }
        CapturedFrame {
            seq,
            endpoint: DhanEndpointType::MainFeed,
            connection_index: (n % 4) as u8,
            received_at: std::time::Instant::now(),
            bytes: bytes.into(),
        }
    }

    let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8);
    let c = counters();

    // Warm-up OUTSIDE the window, same reasoning as the fold gate: first touch
    // of an instrument allocates its slot and registers its metric keys.
    for n in 0..4u64 {
        let _ = drain_main_feed_frame(&mut ingest, &frame(n, n), 1_000_000, 1_000, c);
    }

    const FRAMES: u64 = 2_500; // x4 packets = 10,000 ticks, as above

    // EVERY FIXTURE BUILT BEFORE THE PROFILER STARTS.
    //
    // The first version of this test called `frame(...)` inside the measured
    // loop and CI reported 12,500 blocks against a 512 budget. That number was
    // the harness, not the code: `frame` does one `Vec::with_capacity` plus
    // four `vec![0u8; 50]`, so 2,500 x 5 = 12,500 exactly, and 1,000,000 bytes
    // is 2,500 x 400. The arithmetic matched the fixture perfectly.
    //
    // A test that measures its own setup is worse than no test: it reports a
    // per-tick allocation that does not exist, and the natural response is to
    // raise the budget until it passes — which would leave the seam
    // permanently unguarded while looking guarded. The fold gate above never
    // had this problem because its fixture is a plain struct that never
    // touches the heap; a frame is bytes, so it must be built up front.
    let frames: Vec<CapturedFrame> = (0..FRAMES).map(|n| frame(100 + n, n)).collect();

    let profiler = dhat::Profiler::builder().testing().build();
    for (n, f) in frames.iter().enumerate() {
        let _ = drain_main_feed_frame(&mut ingest, f, 1_000_000 + n as i64, 1_000 + n as u64, c);
    }
    let stats = dhat::HeapStats::get();
    drop(profiler);

    assert!(
        stats.total_blocks <= MAX_BLOCKS,
        "PER-TICK ALLOCATION ON THE FRAME SEAM.\n\n\
         {FRAMES} frames x 4 packets allocated {} blocks ({} bytes); the budget \
         is {MAX_BLOCKS}.\n\n\
         This seam covers `record_ws_lag`, which the narrower fold gate does \
         NOT — the 2026-08-14 regression lived exactly there, and was measured \
         by nothing until this test.",
        stats.total_blocks,
        stats.total_bytes
    );
}

/// Non-vacuity for the frame gate.
///
/// The budget is an upper bound, so a walk that decoded nothing would satisfy
/// it. This proves the frames are actually being folded.
#[test]
fn frame_drain_gate_is_not_vacuous() {
    use tickvault_app::dhan_feed_stack::{counters, drain_main_feed_frame};
    use tickvault_core::websocket::pool_budget::DhanEndpointType;
    use tickvault_core::websocket::pool_supervisor::CapturedFrame;

    let mut buf = vec![0u8; 50];
    buf[0] = 4;
    buf[1..3].copy_from_slice(&50u16.to_le_bytes());
    buf[4..8].copy_from_slice(&13u32.to_le_bytes());
    buf[8..12].copy_from_slice(&100.5f32.to_le_bytes());
    buf[14..18].copy_from_slice(&SESSION_EPOCH_SECS.to_le_bytes());

    let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8);
    assert_eq!(
        ingest.tracked_instruments(),
        0,
        "a fresh ingest must track nothing"
    );

    let out = drain_main_feed_frame(
        &mut ingest,
        &CapturedFrame {
            seq: 1,
            endpoint: DhanEndpointType::MainFeed,
            connection_index: 0,
            received_at: std::time::Instant::now(),
            bytes: buf.into(),
        },
        1_000_000,
        1_000,
        counters(),
    );

    assert_eq!(
        out.folded, 1,
        "the frame walk must fold the packet it was given — otherwise the \
         allocation budget above is measuring an empty loop. outcome={out:?}"
    );
    assert!(ingest.tracked_instruments() > 0);
}

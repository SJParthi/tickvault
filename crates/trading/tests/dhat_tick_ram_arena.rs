//! DHAT allocation gate for the raw-tick RAM arena.
//!
//! # Why this exists
//!
//! `tick_ram_arena.rs` opens with the claim *"Zero heap allocation after
//! construction."* Until now the only thing checking it was
//! `append_never_reallocates_the_arena`, which reads `arena.capacity()` — it
//! watches ONE of the four collections and cannot see the head table, the
//! count table, or the slot map. An adversarial review flagged that gap: a
//! zero-allocation claim in a header, with no mechanical proof, is exactly the
//! shape this repository's own rules forbid.
//!
//! # What this proves, and what it does not
//!
//! PROVES: `append`, `latest`, `walk_back` and the counter accessors perform
//! ZERO heap allocations in steady state — including the first append for a
//! NEW instrument, which is the interesting half, because that path pushes to
//! two vectors and inserts into a hash map. It is allocation-free only
//! because all three are pre-sized to the slot ceiling at construction, and
//! nothing outside a test asserts that they stay pre-sized.
//!
//! DOES NOT PROVE the arena is cheap, correct, or wired. It is not wired —
//! the store has no production caller yet (its module header says so), and
//! this gate exists so that the day it IS wired, the allocation claim is
//! already nailed down rather than being taken on trust at the moment it
//! starts running on the frame drain.
//!
//! ONE profiler per test binary: `dhat::Profiler` is process-global and
//! panics if a second is built while one is live, so every measured region
//! lives in a single test (the `dhat_multi_tf_fold.rs` convention).

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use tickvault_common::types::ExchangeSegment;
use tickvault_trading::in_mem::tick_ram_arena::{ArenaKey, TickRamArena, TickSample};

/// Record capacity of the fixture arena.
///
/// Named rather than repeated so the construction and the between-attempt
/// reset can never drift apart — a reset to a SMALLER arena would reintroduce
/// the `ArenaFull` this fixture was fixed for, and would look like a passing
/// change.
const ARENA_CAPACITY: usize = 120_000;

mod dhat_support;

fn key(id: u64) -> ArenaKey {
    (id, ExchangeSegment::NseFno)
}

fn sample(ts: u32, ltp: f32) -> TickSample {
    TickSample {
        exchange_timestamp: ts,
        last_traded_price: ltp,
        last_trade_quantity: 1,
        volume: 100,
        open_interest: 0,
        total_buy_quantity: 0,
        total_sell_quantity: 0,
    }
}

#[test]
fn dhat_arena_append_and_read_are_zero_alloc_including_new_instruments() {
    let _profiler = dhat::Profiler::builder().testing().build();

    // Construction allocates — deliberately, and OUTSIDE the measured region.
    // That is the whole design: one up-front allocation so no append can ever
    // trigger a reallocation mid-session on the frame drain.
    //
    // Behind a `RefCell` so the re-attempt `stabilize` below can REPLACE it.
    // Both closures handed to `measure_with_phantom_retry` need `&mut` access
    // and Rust will not let two closures hold that at once; the cell is the
    // narrowest way to give the reset closure the same arena the workload
    // uses. Every borrow is short-lived and they never overlap — `stabilize`
    // runs strictly between attempts.
    let arena = std::cell::RefCell::new(TickRamArena::with_capacity(ARENA_CAPACITY));

    // Warm ONE instrument outside the measurement so the steady-state loop
    // below is measuring steady state and not first-touch.
    arena
        .borrow_mut()
        .append(key(1), sample(1_000, 100.0))
        .expect("warm append must land");

    let (_, allocs) = dhat_support::measure_with_phantom_retry(
        0,
        0,
        // RESET between attempts. Without this the test could not survive its
        // own retry, and the retry is not hypothetical — the helper exists
        // because GitHub's 2-core runners land a documented ~900 B / 4-block
        // cross-thread phantom inside the measurement window.
        //
        // The workload below appends roughly 75,000 records per attempt
        // (50,000 steady-state + 5,000 first-touch + the ~20,000 of the
        // refusal arm that land before the SLOT ceiling) into an arena of
        // ARENA_CAPACITY. A second attempt on the same arena therefore passes
        // the record ceiling part-way through and the steady-state append
        // returns `ArenaFull` — which surfaces as an `expect` panic, not as a
        // budget failure, so it reads like a broken arena rather than an
        // exhausted fixture. Observed on 2026-09-01.
        //
        // Rebuilding here allocates, deliberately: `stabilize` runs BEFORE the
        // `HeapStats` snapshot is taken, so it is outside every measurement
        // window and cannot mask a real regression. That is exactly what the
        // helper's own doc reserves this parameter for.
        || {
            let mut a = arena.borrow_mut();
            *a = TickRamArena::with_capacity(ARENA_CAPACITY);
            a.append(key(1), sample(1_000, 100.0))
                .expect("warm append must land after reset");
        },
        || {
            let mut arena = arena.borrow_mut();
            // (a) STEADY STATE — 50,000 appends to an already-known
            // instrument. A `.clone()`, `format!` or `Vec` in `append` would
            // allocate at least once per iteration and be unmissable.
            for i in 0..50_000u32 {
                arena
                    .append(key(1), sample(1_000 + i, 100.0 + (i % 97) as f32))
                    .expect("steady-state append must land");
            }

            // (b) NEW INSTRUMENTS — 5,000 first-touch appends. This is the
            // path that pushes to `heads`, pushes to `counts` and inserts
            // into `slot_of`. It is allocation-free ONLY because all three
            // are pre-sized to the 25,000 slot ceiling at construction; if a
            // future edit drops a `with_capacity`, this is the arm that
            // catches it, and nothing else would.
            for id in 2..5_002u64 {
                arena
                    .append(key(id), sample(2_000, 250.0))
                    .expect("first-touch append must land");
            }

            // (c) READS — `latest` is one hash probe plus two index reads,
            // and `walk_back` returns a LAZY iterator. Collecting it would
            // allocate in the test rather than in the store, so the walk is
            // consumed by folding instead.
            let mut seen = 0usize;
            let mut last = 0.0f32;
            for id in 1..1_000u64 {
                if let Some(t) = arena.latest(key(id)) {
                    last += t.last_traded_price;
                }
                for tick in arena.walk_back(key(id)).take(8) {
                    seen += 1;
                    last += tick.last_traded_price;
                }
            }
            assert!(
                seen > 0 && last > 0.0,
                "the read arm must actually read — otherwise this test passes \
                 vacuously while measuring nothing"
            );

            // (d) REFUSAL PATH — a slot-exhausted refusal must not allocate
            // either. The arena is far from full, so drive the SLOT ceiling
            // by asking for instruments past it.
            let mut refused = 0usize;
            for id in 100_000..130_000u64 {
                if arena.append(key(id), sample(3_000, 10.0)).is_err() {
                    refused += 1;
                }
            }
            assert!(
                refused > 0,
                "the refusal arm must actually refuse — the slot ceiling is \
                 25,000 and this loop asks for 30,000 new instruments"
            );
        },
    );

    assert_eq!(
        allocs, 0,
        "TickRamArena must be zero-alloc after construction — for steady-state \
         appends, first-touch appends, reads and refusals alike. Got {allocs} \
         allocation blocks. The module header states this claim; if the claim \
         is no longer true, change the header, do not raise this budget."
    );
}

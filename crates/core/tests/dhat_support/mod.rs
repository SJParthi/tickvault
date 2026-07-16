//! Shared bounded re-measure helper for the DHAT zero-alloc ratchet tests.
//!
//! NOT a test target: this file lives in a `tests/` SUBDIRECTORY, so cargo
//! compiles it only when a dhat test binary declares `mod dhat_support;`.
//! One copy exists per crate with dhat tests (core / storage / trading) —
//! integration-test binaries cannot share code across crates without a
//! dedicated helper crate, and the file is deliberately tiny.
//!
//! ## Why retries exist (2026-07-09 — the roaming-phantom CI flake)
//!
//! `dhat::HeapStats` is PROCESS-GLOBAL: the before/after snapshots around a
//! measured workload also count allocations made concurrently by any other
//! thread — the libtest harness machinery, and (in two tests) the test's
//! own background worker. On GitHub's 2-core runners a one-time concurrent
//! allocation burst (observed: EXACTLY 4 blocks / ~900 B) lands inside the
//! measurement window nondeterministically, failing a DIFFERENT dhat test
//! on each attempt while the same SHA passes on re-run:
//!   - main run 29043027170: dhat_instrument_registry "left: 4, right: 0"
//!     ("4 blocks over 4000 operations") — siblings green microseconds
//!     earlier in the same job;
//!   - PR #1458 run 29045589653: needed 3 attempts (different test each
//!     time); PR #1459 flaked once more. Local runs: consistently green.
//!
//! ## Why a retry CANNOT mask a real hot-path allocation
//!
//! The retry re-executes the IDENTICAL workload and re-applies the caller's
//! UNCHANGED budget each attempt:
//!   - a per-iteration regression (`.clone()` / `format!()` / `Vec::new()`
//!     in the measured loop) allocates >= the iteration count (1,000-50,000
//!     blocks in these tests) on EVERY attempt — all attempts blow the
//!     budget and the caller's bespoke assertion fires on the final values;
//!   - a per-workload allocation above budget likewise recurs every attempt;
//!   - only allocations that do NOT recur under identical re-execution are
//!     absorbed: one-time lazy init (the long-sanctioned warm-up semantic —
//!     every test here already warms up explicitly) and cross-thread
//!     phantom noise (the flake this helper exists to absorb).
//!
//! Budgets are RATCHETS and are not changed by this helper.

/// Measure `workload` between `dhat::HeapStats` snapshots, retrying up to
/// [`DHAT_MEASURE_ATTEMPTS`] times while the measured delta exceeds the
/// given budget. `stabilize` runs before every RE-attempt (not before the
/// first) so stateful workloads can restore their steady-state invariant
/// (pass `|| {}` when not needed).
///
/// Returns the FINAL attempt's `(bytes_delta, blocks_delta)` — the caller
/// keeps its own bespoke assertion, which fires only if EVERY attempt
/// exceeded the budget.
pub fn measure_with_phantom_retry(
    budget_bytes: u64,
    budget_blocks: u64,
    mut stabilize: impl FnMut(),
    mut workload: impl FnMut(),
) -> (u64, u64) {
    const DHAT_MEASURE_ATTEMPTS: u32 = 3;

    let (mut bytes, mut blocks) = (u64::MAX, u64::MAX);
    for attempt in 1..=DHAT_MEASURE_ATTEMPTS {
        if attempt > 1 {
            stabilize();
        }
        let before = dhat::HeapStats::get();
        workload();
        let after = dhat::HeapStats::get();
        bytes = after.total_bytes.saturating_sub(before.total_bytes);
        blocks = after.total_blocks.saturating_sub(before.total_blocks);
        if bytes <= budget_bytes && blocks <= budget_blocks {
            if attempt > 1 {
                eprintln!(
                    "dhat_support: attempt {attempt}/{DHAT_MEASURE_ATTEMPTS} clean \
                     ({bytes} B / {blocks} blocks) — earlier attempt absorbed a \
                     one-time/cross-thread phantom (see dhat_support/mod.rs)"
                );
            }
            return (bytes, blocks);
        }
        eprintln!(
            "dhat_support: attempt {attempt}/{DHAT_MEASURE_ATTEMPTS} measured \
             {bytes} B / {blocks} blocks (budget {budget_bytes} B / {budget_blocks} \
             blocks) — retrying; a real per-iteration allocation recurs on every attempt"
        );
    }
    (bytes, blocks)
}

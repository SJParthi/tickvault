//! Ratchet: the bench-gate per-element normalization for BATCH benchmarks.
//!
//! 2026-06-02 load+chaos proof run found a unit bug: `scripts/bench-gate.sh`
//! compared a 100-tick BATCH median (`pipeline/batch_100_mixed`, ~470ns)
//! against the PER-TICK budget (`pipeline = 100ns`), so the latency gate
//! failed by construction even though per-tick latency was ~4.7ns (≈20×
//! under budget). The fix: `quality/benchmark-budgets.toml` declares an
//! `[elements]` section (elements-per-iteration for batch benches) and the
//! gate divides the median by that count before comparing.
//!
//! This guard pins both halves so the unit bug can't silently return.

use std::fs;
use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .expect("workspace root above crates/common") // APPROVED: test
}

fn read(rel: &str) -> String {
    let p = workspace_root().join(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())) // APPROVED: test
}

#[test]
fn budgets_toml_declares_elements_for_batch_benches() {
    let toml = read("quality/benchmark-budgets.toml");
    assert!(
        toml.contains("[elements]"),
        "benchmark-budgets.toml MUST declare an [elements] section so the \
         bench-gate normalizes batch-bench medians to per-element before \
         comparing to per-element budgets (2026-06-02 unit-bug fix)."
    );
    // The two pipeline batch benches each process 100 elements per iter.
    for needle in ["pipeline_batch_100_mixed", "pipeline_burst_100_ticker"] {
        assert!(
            toml.contains(needle),
            "[elements] MUST list {needle} with its per-iteration element count \
             — otherwise the gate compares its 100-tick batch median to the \
             per-tick budget and fails by construction."
        );
    }
}

#[test]
fn bench_gate_divides_median_by_element_count() {
    let gate = read("scripts/bench-gate.sh");
    assert!(
        gate.contains("[elements]") && gate.contains("per_elem_ns"),
        "bench-gate.sh MUST parse [elements] and compute a per-element value \
         (per_elem_ns) before the budget comparison — restoring the raw-median \
         comparison reintroduces the batch-vs-per-tick unit bug."
    );
    assert!(
        gate.contains("median_ns / n_elements"),
        "bench-gate.sh MUST divide the batch median by its element count."
    );
}

// RETIRED (stage-2 dead-WS sweep, 2026-07-17):
// `budgets_toml_pins_compute_only_10us_gate` pinned the B6
// `composite_quote_tick_compute_only = 10000` budget row — its Criterion
// bench (crates/core/benches/full_tick_processing.rs) measured the deleted
// tick chain and was deleted with it, so the row was removed from
// benchmark-budgets.toml (dated comment there). The surviving hot-path
// budgets (dispatch_frame/pipeline/moneyness — feed_presence retired
// 2026-07-18 with the presence registry) keep their own
// bench-gate enforcement (the feed_lag key retired 2026-07-17 with the dead
// Dhan-lag ring/publisher chain — dashboard tidy).

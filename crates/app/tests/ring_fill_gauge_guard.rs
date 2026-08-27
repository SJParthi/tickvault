//! Ring OCCUPANCY: we shipped the denominator and withheld the numerator.
//!
//! The frame ring is bounded twice — by frame COUNT and by BYTES
//! (`RingByteBudget`). `tv_dhan_feed_ring_max_bytes` has been EMF-selected
//! since 2026-08-15, so the CAPACITY reaches CloudWatch while the OCCUPANCY
//! did not. `RingByteBudget::resident()` existed the whole time with call
//! sites only in its own unit tests.
//!
//! Paired with the dwell gauge, the two answer different halves of one
//! question and neither answers it alone: dwell is how far behind the drain is
//! in TIME, fill is how full the ring is in BYTES. Both climbing is a drain
//! that cannot keep up; dwell flat while fill climbs is large frames rather
//! than a slow drain — a different problem with a different fix.

use tickvault_app::dhan_feed_stack::budget_fill_pct;

#[test]
fn an_empty_budget_reads_zero_and_a_full_one_reads_one_hundred() {
    assert!(
        budget_fill_pct(0, 1_000).abs() < f64::EPSILON,
        "empty is 0%"
    );
    assert!(
        (budget_fill_pct(1_000, 1_000) - 100.0).abs() < f64::EPSILON,
        "full is 100%"
    );
    assert!(
        (budget_fill_pct(250, 1_000) - 25.0).abs() < f64::EPSILON,
        "a quarter full is 25%"
    );
}

/// A zero capacity must NOT produce NaN.
///
/// The combination is unreachable in production — both caps derive from a
/// non-zero host figure — but a NaN reaching a gauge is silently unchartable,
/// and "the chart went blank" is a worse failure than "the chart read zero":
/// blank looks like the app died, and would send an operator to diagnose the
/// wrong thing during an incident.
#[test]
fn a_zero_capacity_reads_zero_and_never_nan() {
    let pct = budget_fill_pct(0, 0);
    assert!(pct.is_finite(), "must never be NaN or infinite, got {pct}");
    assert!(pct.abs() < f64::EPSILON, "must read 0.0, got {pct}");

    let pct = budget_fill_pct(500, 0);
    assert!(pct.is_finite(), "must never be NaN or infinite, got {pct}");
}

/// Over-capacity clamps to 100 rather than exceeding it.
///
/// `RingByteBudget` uses a CAS loop precisely so `resident` can never exceed
/// `cap`, so this should be unreachable — which is why it is worth pinning.
/// A gauge reading 340% would look like a unit bug and get dismissed, at the
/// exact moment the ring is in trouble.
#[test]
fn over_capacity_clamps_to_one_hundred_rather_than_reading_absurd() {
    let pct = budget_fill_pct(3_400, 1_000);
    assert!(
        (pct - 100.0).abs() < f64::EPSILON,
        "must clamp to 100%, got {pct}"
    );
}

#[test]
fn the_worst_of_two_pools_is_what_surfaces() {
    // Main feed nearly empty, depth nearly full: the gauge must report the
    // pool in trouble. Reporting the average, or the larger pool by capacity,
    // would read healthy while one side is about to refuse frames.
    let main = budget_fill_pct(10, 1_000); //  1%
    let depth = budget_fill_pct(950, 1_000); // 95%
    assert!(
        (main.max(depth) - 95.0).abs() < f64::EPSILON,
        "the worst pool must surface"
    );
}

#[test]
fn the_gauges_are_published_and_read_the_real_budgets() {
    let src = include_str!("../src/dhan_feed_stack.rs");
    let production = src.split("#[cfg(test)]\nmod ").next().unwrap_or(src);
    assert!(
        production.contains("metrics::gauge!(RING_RESIDENT_PCT_GAUGE)"),
        "the selected worst-of-two gauge must be published"
    );
    // It must read the LIVE budgets, not a constant. Publishing `cap()` as if
    // it were occupancy would produce a permanently-100% chart that reads as a
    // crisis and is in fact a bug.
    assert!(
        production.contains("main_feed_budget.resident()")
            && production.contains("depth_budget.resident()"),
        "both budgets' live occupancy must be read"
    );
    assert!(
        production.contains("main_pct.max(depth_pct)"),
        "the selected gauge must carry the WORST pool, not the first or the \
         average — an average reads healthy while one pool is about to refuse"
    );
    // Per-pool detail exists but must NOT share the selected name, or every
    // pool becomes a paid CloudWatch dimension.
    assert!(
        production.contains("tv_dhan_feed_ring_resident_pct_by_pool"),
        "per-pool detail must be published under its own unselected name"
    );
}

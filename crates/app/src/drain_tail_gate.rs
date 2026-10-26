//! The tail gate for the main-feed drain bench (plan item K6, 2026-10-26).
//!
//! `scripts/bench-gate.sh` compares Criterion's MEDIAN against the budget in
//! `quality/benchmark-budgets.toml`. A drain that is 1 µs almost always and
//! 1 ms once in a hundred frames passes that check. This module is the second
//! check, run by `benches/feed_drain.rs` after Criterion: the drain's per-frame
//! p99, divided by the ticks in the frame, must also be at or under the same
//! per-tick budget (times the CI hardware multiplier the median arm uses). A
//! p99 over it makes the bench run exit non-zero, which fails the bench job.
//!
//! The budget is read from the TOML, never restated here, so the median and
//! the p99 arms can not drift apart. Nothing is added to the TOML, so its key
//! count is unchanged.
//!
//! # Complexity
//! Cold: the bench calls it once per run. [`tail_verdict`] is O(1) on an
//! already-sorted slice; [`budget_from_toml`] is O(lines of the TOML).

/// The budget key the main-feed drain bench is measured against
/// (`feed_drain/main_feed_frame_4_quotes`), in nanoseconds per tick.
pub const MAIN_FEED_DRAIN_BUDGET_KEY: &str = "feed_drain_main_feed";

/// The environment variable the bench workflow already uses to scale the
/// absolute budgets on slower CI runners (`.github/workflows/bench.yml`).
pub const BUDGET_MULTIPLIER_ENV: &str = "BENCH_BUDGET_MULTIPLIER";

/// The largest multiplier accepted. Past it the "budget" stops meaning
/// anything, so a larger value is refused rather than honoured.
pub const MAX_BUDGET_MULTIPLIER: f64 = 10.0;

/// Reads `key = <integer>` from the `[budgets]` table of the budgets TOML.
/// `None` when the table or key is missing or the value is not a positive
/// integer: the caller fails the run, it never guesses a budget.
#[must_use]
pub fn budget_from_toml(text: &str, key: &str) -> Option<u64> {
    let mut in_budgets = false;
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') {
            in_budgets = line == "[budgets]";
            continue;
        }
        if !in_budgets {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if name.trim() == key {
            return value.trim().parse::<u64>().ok().filter(|v| *v > 0);
        }
    }
    None
}

/// The budget multiplier from the environment value: 1.0 when unset; `None`
/// when set but not a finite number in `[1.0, MAX_BUDGET_MULTIPLIER]` (a
/// multiplier below 1 would tighten the budget, which is not what the
/// variable is for).
#[must_use]
pub fn budget_multiplier(value: Option<&str>) -> Option<f64> {
    match value {
        None => Some(1.0),
        Some(v) => v
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|m| m.is_finite() && (1.0..=MAX_BUDGET_MULTIPLIER).contains(m)),
    }
}

/// The sample at quantile `num / den` of an ascending-sorted slice: the
/// value at 1-based rank `ceil(len × num / den)`. `None` for an empty slice.
#[must_use]
pub fn quantile_of_sorted(sorted: &[u64], num: usize, den: usize) -> Option<u64> {
    if sorted.is_empty() || den == 0 {
        return None;
    }
    let rank = sorted
        .len()
        .saturating_mul(num)
        .div_ceil(den)
        .clamp(1, sorted.len());
    sorted.get(rank - 1).copied()
}

/// What the tail check found.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TailVerdict {
    /// p99 per frame, nanoseconds.
    pub p99_ns_per_frame: u64,
    /// p99 per tick (`p99_ns_per_frame / ticks_per_frame`), nanoseconds.
    pub p99_ns_per_tick: f64,
    /// The ceiling it was compared with: budget × multiplier, ns per tick.
    pub ceiling_ns_per_tick: f64,
    /// `p99_ns_per_tick <= ceiling_ns_per_tick`.
    pub within: bool,
}

/// Compares the p99 of `sorted` (per-frame nanoseconds, ascending) with the
/// per-tick budget. `None` for no samples or a frame of zero ticks.
#[must_use]
pub fn tail_verdict(
    sorted: &[u64],
    ticks_per_frame: u64,
    budget_ns_per_tick: u64,
    multiplier: f64,
) -> Option<TailVerdict> {
    if ticks_per_frame == 0 {
        return None;
    }
    let p99 = quantile_of_sorted(sorted, 99, 100)?;
    // u64 -> f64 is exact below 2^53 ns (about 104 days), far above any frame.
    let (p99_f, ticks_f, budget_f) = (
        p99 as f64,
        ticks_per_frame as f64,
        budget_ns_per_tick as f64,
    );
    let p99_ns_per_tick = p99_f / ticks_f;
    let ceiling_ns_per_tick = budget_f * multiplier;
    Some(TailVerdict {
        p99_ns_per_frame: p99,
        p99_ns_per_tick,
        ceiling_ns_per_tick,
        within: p99_ns_per_tick <= ceiling_ns_per_tick,
    })
}

/// The gate's own check, run by the bench before it measures: a synthetic
/// sample set whose p99 is one nanosecond per tick over the ceiling must
/// fail, and one exactly at it must pass. `Err` names what went wrong; the
/// bench exits non-zero on it, so a gate that can not fail never runs.
pub fn self_test(budget_ns_per_tick: u64, ticks_per_frame: u64) -> Result<(), String> {
    let at = budget_ns_per_tick.saturating_mul(ticks_per_frame);
    let over = at.saturating_add(ticks_per_frame);
    // 100 samples: 98 fast, then the p99 sample and one above it.
    let build = |p99: u64| -> Vec<u64> {
        let mut v = vec![1u64; 98];
        v.push(p99);
        v.push(p99.saturating_mul(2));
        v
    };
    match tail_verdict(&build(over), ticks_per_frame, budget_ns_per_tick, 1.0) {
        Some(v) if !v.within => {}
        other => {
            return Err(format!(
                "an over-budget p99 did not fail the gate: {other:?}"
            ));
        }
    }
    match tail_verdict(&build(at), ticks_per_frame, budget_ns_per_tick, 1.0) {
        Some(v) if v.within => Ok(()),
        other => Err(format!(
            "a p99 exactly at the budget failed the gate: {other:?}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOML: &str = "max_regression_pct = 5.0\n[budgets]\n\
        feed_drain_main_feed     = 2500         # <2.5us per tick\n\
        feed_drain_depth = 1500\n[elements]\nfeed_drain_main_feed = 4\n";

    #[test]
    fn test_budget_from_toml_reads_the_budgets_table_only() {
        assert_eq!(
            budget_from_toml(TOML, MAIN_FEED_DRAIN_BUDGET_KEY),
            Some(2500)
        );
        assert_eq!(budget_from_toml(TOML, "feed_drain_depth"), Some(1500));
        assert_eq!(budget_from_toml(TOML, "missing"), None);
        assert_eq!(
            budget_from_toml(
                "[elements]\nfeed_drain_main_feed = 4\n",
                MAIN_FEED_DRAIN_BUDGET_KEY
            ),
            None
        );
        assert_eq!(
            budget_from_toml(
                "[budgets]\nfeed_drain_main_feed = 0\n",
                MAIN_FEED_DRAIN_BUDGET_KEY
            ),
            None
        );
        assert_eq!(
            budget_from_toml(
                "[budgets]\nfeed_drain_main_feed = x\n",
                MAIN_FEED_DRAIN_BUDGET_KEY
            ),
            None
        );
    }

    #[test]
    fn test_budget_from_toml_reads_the_real_budget_file() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../quality/benchmark-budgets.toml"
        );
        let text = std::fs::read_to_string(path).expect("budgets TOML is in the repo");
        assert!(budget_from_toml(&text, MAIN_FEED_DRAIN_BUDGET_KEY).is_some());
    }

    #[test]
    fn test_budget_multiplier_defaults_and_refuses_nonsense() {
        assert_eq!(budget_multiplier(None), Some(1.0));
        assert_eq!(budget_multiplier(Some("2.5")), Some(2.5));
        assert_eq!(budget_multiplier(Some(" 1 ")), Some(1.0));
        for bad in ["0.5", "abc", "NaN", "inf", "11", "-2"] {
            assert_eq!(budget_multiplier(Some(bad)), None, "{bad}");
        }
    }

    #[test]
    fn test_quantile_of_sorted_ranks() {
        assert_eq!(quantile_of_sorted(&[], 99, 100), None);
        assert_eq!(quantile_of_sorted(&[7], 99, 100), Some(7));
        let v: Vec<u64> = (1..=1_000).collect();
        assert_eq!(quantile_of_sorted(&v, 1, 2), Some(500));
        assert_eq!(quantile_of_sorted(&v, 99, 100), Some(990));
        assert_eq!(quantile_of_sorted(&v, 999, 1_000), Some(999));
        assert_eq!(quantile_of_sorted(&v, 1, 0), None);
    }

    #[test]
    fn test_tail_verdict_fails_a_synthetic_over_budget_p99() {
        // 4 ticks a frame, 2,500 ns a tick: the ceiling is 10,000 ns a frame.
        let mut v = vec![3_000u64; 98];
        v.push(10_004);
        v.push(50_000);
        let verdict = tail_verdict(&v, 4, 2_500, 1.0).expect("samples");
        assert_eq!(verdict.p99_ns_per_frame, 10_004);
        assert!(
            !verdict.within,
            "10,004 ns / 4 ticks is over 2,500 ns a tick"
        );
        // The CI multiplier lifts the ceiling, the median arm's rule.
        assert!(tail_verdict(&v, 4, 2_500, 2.5).expect("samples").within);
        assert_eq!(tail_verdict(&v, 0, 2_500, 1.0), None);
        assert_eq!(tail_verdict(&[], 4, 2_500, 1.0), None);
    }

    #[test]
    fn test_self_test_passes_for_the_real_gate() {
        assert_eq!(self_test(2_500, 4), Ok(()));
        assert_eq!(self_test(1, 1), Ok(()));
    }
}

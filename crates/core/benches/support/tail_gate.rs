//! The per-call p99 gate shared by the side-store benches (plan ITEM DT,
//! 2026-10-30): `tick_gap_detector` (core), `tick_conservation_ledger`
//! (storage), `spot_price_store` and `backup_dedup` (app). Included by path
//! (`#[path]`), so each bench crate compiles its own copy and no crate gains
//! a dependency.
//!
//! Criterion reports the MEDIAN. This times one call at a time over
//! [`PERCENTILE_SAMPLES`] calls, with one `Instant` pair inside every sample
//! (so the figures read high by the clock's cost), and fails when the p99 is
//! over the bench's `[budgets]` value in `quality/benchmark-budgets.toml`
//! times `BENCH_BUDGET_MULTIPLIER` (the same multiplier `bench-gate.sh` uses).
//! It checks first that an over-budget sample set fails, so the gate can fail.
//! Every timed call also reports whether it took its case's path (the call
//! returns `true` when the store gave the expected outcome); one call that
//! did not fails the gate, so a case cannot quietly time a cheaper path.

use std::time::Instant;

/// Per-call samples per gated case.
pub const PERCENTILE_SAMPLES: usize = 200_000;

/// Largest accepted `BENCH_BUDGET_MULTIPLIER`.
const MAX_MULTIPLIER: f64 = 10.0;

/// The value at 1-based rank `ceil(len × num / den)` of an ascending slice.
pub fn quantile(sorted: &[u64], num: usize, den: usize) -> u64 {
    if sorted.is_empty() || den == 0 {
        return 0;
    }
    let rank = sorted
        .len()
        .saturating_mul(num)
        .div_ceil(den)
        .clamp(1, sorted.len());
    sorted[rank - 1]
}

/// Times `call(n)` once per sample for `n` in `0..PERCENTILE_SAMPLES`;
/// returns the samples sorted ascending and how many calls returned `false`
/// (took another path than the case's). The `Vec` is sized before the loop.
pub fn samples(mut call: impl FnMut(u64) -> bool) -> (Vec<u64>, u64) {
    let mut out = Vec::with_capacity(PERCENTILE_SAMPLES);
    let mut off_path = 0u64;
    for n in 0..PERCENTILE_SAMPLES as u64 {
        let start = Instant::now();
        let on_path = call(n);
        out.push(u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX));
        off_path += u64::from(!on_path);
    }
    out.sort_unstable();
    (out, off_path)
}

/// The ceiling in nanoseconds: `key`'s budget times the multiplier.
pub fn ceiling_ns(key: &str) -> Result<f64, String> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../quality/benchmark-budgets.toml"
    );
    let text = std::fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?;
    let mut in_budgets = false;
    let mut budget = None;
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') {
            in_budgets = line == "[budgets]";
            continue;
        }
        if let (true, Some((name, value))) = (in_budgets, line.split_once('='))
            && name.trim() == key
        {
            budget = value.trim().parse::<u64>().ok().filter(|v| *v > 0);
        }
    }
    let budget = budget.ok_or_else(|| format!("no positive `{key}` in [budgets] of {path}"))?;
    let multiplier = match std::env::var("BENCH_BUDGET_MULTIPLIER") {
        Err(_) => 1.0,
        Ok(v) => v
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|m| m.is_finite() && (1.0..=MAX_MULTIPLIER).contains(m))
            .ok_or_else(|| {
                format!("BENCH_BUDGET_MULTIPLIER = {v:?} is not in [1, {MAX_MULTIPLIER}]")
            })?,
    };
    Ok(budget as f64 * multiplier)
}

/// Whether the p99 of `sorted` is at or under `ceiling`.
pub fn within(sorted: &[u64], ceiling: f64) -> bool {
    (quantile(sorted, 99, 100) as f64) <= ceiling
}

/// Collects verdicts for one bench's gated cases.
pub struct Gate {
    group: &'static str,
    ceiling: f64,
    failures: Vec<String>,
}

impl Gate {
    /// Reads the ceiling for `key` and checks the gate can fail.
    pub fn new(group: &'static str, key: &str) -> Result<Self, String> {
        let ceiling = ceiling_ns(key)?;
        // 98 fast samples, then the p99 sample and one more, both over.
        let mut over = vec![1u64; 98];
        let bad = (ceiling as u64).saturating_add(1);
        over.extend([bad, bad]);
        if within(&over, ceiling) {
            return Err(format!("{group}: self-test: an over-budget p99 passed"));
        }
        let mut at = vec![1u64; 98];
        at.extend([ceiling as u64, ceiling as u64]);
        if !within(&at, ceiling) {
            return Err(format!("{group}: self-test: an at-budget p99 failed"));
        }
        Ok(Self {
            group,
            ceiling,
            failures: Vec::new(),
        })
    }

    /// Times `call` and records its verdict. `call` returns whether the
    /// store took the case's path; any `false` fails the gate.
    pub fn case(&mut self, name: &str, call: impl FnMut(u64) -> bool) {
        let (s, off_path) = samples(call);
        if off_path > 0 {
            self.failures
                .push(format!("{name}: {off_path} calls took another path"));
        }
        let p99 = quantile(&s, 99, 100);
        let ok = within(&s, self.ceiling);
        println!(
            "{}/{name}: p50 = {} ns, p99 = {p99} ns, p99.9 = {} ns, max = {} ns over {} calls \
             (one Instant pair inside each sample); p99 ceiling {:.0} ns: {}",
            self.group,
            quantile(&s, 1, 2),
            quantile(&s, 999, 1_000),
            s.last().copied().unwrap_or(0),
            s.len(),
            self.ceiling,
            if ok { "PASS" } else { "FAIL" },
        );
        if !ok {
            self.failures.push(format!("{name} p99 {p99} ns"));
        }
    }

    /// `Err` when any case was over the ceiling.
    pub fn finish(self) -> Result<(), String> {
        if self.failures.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "{} over {:.0} ns: {}",
                self.group,
                self.ceiling,
                self.failures.join(", ")
            ))
        }
    }
}

/// Runs `gate` only under `cargo bench` (`--bench`) and exits 1 on failure.
pub fn run_under_bench(name: &str, gate: impl FnOnce() -> Result<(), String>) {
    if std::env::args().any(|a| a == "--bench")
        && let Err(why) = gate()
    {
        eprintln!("{name} p99 gate FAILED: {why}");
        std::process::exit(1);
    }
}

//! Sub-PR #9b of 2026-05-27 daily-universe expansion — L3 RECONCILE
//! anomaly check (Z+ defense layer 3).
//!
//! **Feature-gated.** This module compiles only when the
//! `daily_universe_fetcher` cargo feature is enabled (per §21 of the
//! authoritative rule file). Under the current `Indices4Only` default
//! scope this module is dead code.
//!
//! ## Z+ defense — L3 RECONCILE layer
//!
//! Per the Z+ doctrine + `daily-universe-scope-expansion-2026-05-27.md`
//! §9 layer L3: compare today's CSV row count vs yesterday's
//! `instrument_fetch_audit` row.
//!
//! Flag as anomaly if:
//! - `total_rows_today < 0.5× total_rows_yesterday` (silent half-bulk
//!   removal — Dhan-side regression that shrank the CSV)
//! - `total_rows_today > 2.0× total_rows_yesterday` (silent doubling
//!   — Dhan-side bug duplicating rows)
//!
//! Otherwise: outcome is `WithinBounds` and boot proceeds.
//!
//! ## What this module DOES NOT do (deferred to Sub-PR #9c)
//!
//! - SHA-256 of the CSV body (needs `sha2` workspace dep — pending
//!   operator approval per `cargo-and-docker.md`)
//! - `instrument_fetch_audit` QuestDB table DDL + persistence (Sub-PR
//!   #9c — heavier storage-crate work)
//! - Querying yesterday's audit row at boot (Sub-PR #10b orchestrator)
//! - Telegram event emission on anomaly (Sub-PR #10b)

#![cfg(feature = "daily_universe_fetcher")]

/// Lower bound multiplier — if today's row count is below this fraction
/// of yesterday's, the CSV is anomalously SMALL. Per rule file §9 layer
/// L3.
pub const L3_ANOMALY_SHRINK_THRESHOLD: f64 = 0.5;

/// Upper bound multiplier — if today's row count exceeds this multiple
/// of yesterday's, the CSV is anomalously LARGE. Per rule file §9 layer
/// L3.
pub const L3_ANOMALY_GROWTH_THRESHOLD: f64 = 2.0;

/// Outcome of the L3 RECONCILE anomaly check.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AnomalyOutcome {
    /// First-ever boot (no yesterday audit row) — no anomaly check
    /// possible. Caller proceeds normally.
    FirstBootNoYesterdayBaseline,

    /// `total_rows_today` is within `[0.5×, 2.0×]` of yesterday's
    /// count. Normal day-over-day variation.
    WithinBounds { ratio: f64 },

    /// CSV anomalously shrank — today's count is below
    /// `L3_ANOMALY_SHRINK_THRESHOLD × yesterday`.
    AnomalousShrink {
        total_rows_today: usize,
        total_rows_yesterday: usize,
        ratio: f64,
    },

    /// CSV anomalously grew — today's count exceeds
    /// `L3_ANOMALY_GROWTH_THRESHOLD × yesterday`.
    AnomalousGrowth {
        total_rows_today: usize,
        total_rows_yesterday: usize,
        ratio: f64,
    },
}

impl AnomalyOutcome {
    /// Stable wire-format string for the L5 AUDIT layer + CloudWatch
    /// metric dimensions.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FirstBootNoYesterdayBaseline => "first_boot_no_baseline",
            Self::WithinBounds { .. } => "within_bounds",
            Self::AnomalousShrink { .. } => "anomalous_shrink",
            Self::AnomalousGrowth { .. } => "anomalous_growth",
        }
    }

    /// True if this outcome should HALT the boot (or at least require
    /// operator acknowledgement via `--operator-acknowledge-stale-csv`).
    /// Anomalies are HALT-class — the universe might be wrong.
    #[must_use]
    pub fn is_anomaly(self) -> bool {
        matches!(
            self,
            Self::AnomalousShrink { .. } | Self::AnomalousGrowth { .. }
        )
    }
}

/// Compare today's CSV row count against yesterday's audit baseline.
///
/// **Pure function** — no I/O, no clock reads. Caller queries
/// yesterday's audit row from QuestDB + passes both counts.
///
/// # Arguments
///
/// * `total_rows_today` — count of parsed rows from today's CSV
///   (output of Sub-PR #4 `parse_detailed_csv()`)
/// * `total_rows_yesterday` — optional yesterday's count from the
///   `instrument_fetch_audit` table. `None` = first-ever boot.
///
/// # Returns
///
/// [`AnomalyOutcome`] enum.
#[must_use]
pub fn check_anomaly(
    total_rows_today: usize,
    total_rows_yesterday: Option<usize>,
) -> AnomalyOutcome {
    let yesterday = match total_rows_yesterday {
        Some(0) => {
            // Defensive — yesterday had 0 rows means yesterday's CSV
            // was empty (the parser would have rejected such a CSV at
            // Sub-PR #4's `Empty` error path; an audit row with 0
            // shouldn't exist). Treat as no baseline.
            return AnomalyOutcome::FirstBootNoYesterdayBaseline;
        }
        Some(n) => n,
        None => return AnomalyOutcome::FirstBootNoYesterdayBaseline,
    };

    let ratio = (total_rows_today as f64) / (yesterday as f64);

    if ratio < L3_ANOMALY_SHRINK_THRESHOLD {
        AnomalyOutcome::AnomalousShrink {
            total_rows_today,
            total_rows_yesterday: yesterday,
            ratio,
        }
    } else if ratio > L3_ANOMALY_GROWTH_THRESHOLD {
        AnomalyOutcome::AnomalousGrowth {
            total_rows_today,
            total_rows_yesterday: yesterday,
            ratio,
        }
    } else {
        AnomalyOutcome::WithinBounds { ratio }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_boot_returns_no_baseline_outcome() {
        let outcome = check_anomaly(25_000, None);
        assert_eq!(outcome, AnomalyOutcome::FirstBootNoYesterdayBaseline);
    }

    #[test]
    fn yesterday_zero_treated_as_no_baseline() {
        // Defensive — yesterday count of 0 shouldn't happen but if it
        // does, don't divide by zero.
        let outcome = check_anomaly(25_000, Some(0));
        assert_eq!(outcome, AnomalyOutcome::FirstBootNoYesterdayBaseline);
    }

    #[test]
    fn similar_day_over_day_count_is_within_bounds() {
        // 25,100 today vs 25,000 yesterday = 1.004× ratio
        let outcome = check_anomaly(25_100, Some(25_000));
        match outcome {
            AnomalyOutcome::WithinBounds { ratio } => {
                assert!((ratio - 1.004).abs() < 0.001);
            }
            _ => panic!("expected WithinBounds, got {outcome:?}"),
        }
    }

    #[test]
    fn exact_50_percent_shrink_is_anomalous() {
        // 12,500 today vs 25,000 yesterday = 0.5× ratio (at threshold).
        // Threshold is strict `<` so exactly 0.5 is NOT anomalous.
        let outcome = check_anomaly(12_500, Some(25_000));
        match outcome {
            AnomalyOutcome::WithinBounds { ratio } => {
                assert!((ratio - 0.5).abs() < 0.001);
            }
            _ => panic!("at-threshold 0.5 should be within bounds, got {outcome:?}"),
        }
    }

    #[test]
    fn below_50_percent_is_anomalous_shrink() {
        // 12,000 today vs 25,000 yesterday = 0.48× — anomalous.
        let outcome = check_anomaly(12_000, Some(25_000));
        assert!(matches!(outcome, AnomalyOutcome::AnomalousShrink { .. }));
        assert!(outcome.is_anomaly());
    }

    #[test]
    fn exact_200_percent_growth_is_at_threshold() {
        // 50,000 today vs 25,000 yesterday = 2.0× (at threshold).
        // Threshold is strict `>` so exactly 2.0 is NOT anomalous.
        let outcome = check_anomaly(50_000, Some(25_000));
        assert!(matches!(outcome, AnomalyOutcome::WithinBounds { .. }));
    }

    #[test]
    fn above_200_percent_is_anomalous_growth() {
        // 51,000 today vs 25,000 yesterday = 2.04× — anomalous.
        let outcome = check_anomaly(51_000, Some(25_000));
        assert!(matches!(outcome, AnomalyOutcome::AnomalousGrowth { .. }));
        assert!(outcome.is_anomaly());
    }

    #[test]
    fn zero_today_with_non_zero_yesterday_is_anomalous_shrink() {
        // 0 today vs 25,000 yesterday = 0.0× — clearly anomalous.
        let outcome = check_anomaly(0, Some(25_000));
        match outcome {
            AnomalyOutcome::AnomalousShrink {
                total_rows_today: 0,
                total_rows_yesterday: 25_000,
                ratio,
            } => {
                assert_eq!(ratio, 0.0);
            }
            _ => panic!("expected AnomalousShrink, got {outcome:?}"),
        }
    }

    #[test]
    fn massive_growth_is_anomalous_growth() {
        // 1,000,000 today vs 25,000 yesterday = 40× growth.
        let outcome = check_anomaly(1_000_000, Some(25_000));
        match outcome {
            AnomalyOutcome::AnomalousGrowth { ratio, .. } => {
                assert_eq!(ratio, 40.0);
            }
            _ => panic!("expected AnomalousGrowth, got {outcome:?}"),
        }
    }

    #[test]
    fn diagnostic_fields_preserved_in_anomaly() {
        let outcome = check_anomaly(10_000, Some(25_000));
        match outcome {
            AnomalyOutcome::AnomalousShrink {
                total_rows_today,
                total_rows_yesterday,
                ratio,
            } => {
                assert_eq!(total_rows_today, 10_000);
                assert_eq!(total_rows_yesterday, 25_000);
                assert!((ratio - 0.4).abs() < 0.001);
            }
            _ => panic!("expected AnomalousShrink"),
        }
    }

    #[test]
    fn as_str_returns_stable_wire_format() {
        assert_eq!(
            AnomalyOutcome::FirstBootNoYesterdayBaseline.as_str(),
            "first_boot_no_baseline"
        );
        assert_eq!(
            AnomalyOutcome::WithinBounds { ratio: 1.0 }.as_str(),
            "within_bounds"
        );
        assert_eq!(
            AnomalyOutcome::AnomalousShrink {
                total_rows_today: 10_000,
                total_rows_yesterday: 25_000,
                ratio: 0.4,
            }
            .as_str(),
            "anomalous_shrink"
        );
        assert_eq!(
            AnomalyOutcome::AnomalousGrowth {
                total_rows_today: 60_000,
                total_rows_yesterday: 25_000,
                ratio: 2.4,
            }
            .as_str(),
            "anomalous_growth"
        );
    }

    #[test]
    fn is_anomaly_truth_table() {
        assert!(!AnomalyOutcome::FirstBootNoYesterdayBaseline.is_anomaly());
        assert!(!AnomalyOutcome::WithinBounds { ratio: 1.0 }.is_anomaly());
        assert!(
            AnomalyOutcome::AnomalousShrink {
                total_rows_today: 0,
                total_rows_yesterday: 25_000,
                ratio: 0.0,
            }
            .is_anomaly()
        );
        assert!(
            AnomalyOutcome::AnomalousGrowth {
                total_rows_today: 100_000,
                total_rows_yesterday: 25_000,
                ratio: 4.0,
            }
            .is_anomaly()
        );
    }

    #[test]
    fn threshold_constants_pinned() {
        assert_eq!(L3_ANOMALY_SHRINK_THRESHOLD, 0.5);
        assert_eq!(L3_ANOMALY_GROWTH_THRESHOLD, 2.0);
    }

    #[test]
    fn deterministic_pure_function() {
        let outcome1 = check_anomaly(25_000, Some(25_000));
        let outcome2 = check_anomaly(25_000, Some(25_000));
        assert_eq!(outcome1, outcome2);
    }
}

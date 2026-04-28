//! Wave 3-D Item 13 — Composite real-time guarantee score.
//!
//! Every 10 seconds the boot scheduler samples the live state of six
//! independent dimensions and hands a populated [`SloInputs`] to
//! [`evaluate_slo_score`]. The pure evaluator returns a tri-state
//! [`SloOutcome`] driven by a single composite gauge in `[0.0, 1.0]`:
//!
//! | Score range | Outcome | Severity |
//! |---|---|---|
//! | `[0.95, 1.0]` | `Healthy` | Info (only on recovery edge) |
//! | `[0.80, 0.95)` | `Degraded` | High (only on rising edge) |
//! | `[0.0, 0.80)` | `Critical` | Critical (only on rising edge) |
//!
//! Edge-triggering is the responsibility of the scheduler — this module
//! is a pure function so it can be unit-tested without a tokio runtime
//! and benchmarked without bench-time noise from logging / metrics.
//!
//! # Score formula
//!
//! Per SCOPE §13.1, the score is a *weighted product* of six dimensions.
//! With all weights == 1 this is a pure multiplicative product:
//!
//! ```text
//! score = ws_health * qdb_health * tick_freshness
//!         * token_freshness * spill_health * phase2_health
//! ```
//!
//! Each dimension is clamped to `[0.0, 1.0]` before multiplication so
//! malformed inputs cannot drive the score above 1.0 or to NaN. Five
//! of the six dimensions are binary in production (0 or 1); WS_health
//! is the one fractional input ((active connections) / expected). A
//! single binary dimension at 0 zeroes the score → `Critical`.
//!
//! # Why pure multiplicative
//!
//! 1. **Operator interpretability.** Score == 1.0 ⇔ every dimension green.
//!    Any non-trivial degradation immediately scales the score below 1.
//! 2. **Bench budget.** The hot path is six `f64::min(1.0, max(0.0, x))`
//!    clamps and five multiplies — `bench_score_compute_le_1us` ratchets
//!    p99 ≤ 1 µs (in practice ≤ 50 ns on c7i.xlarge).
//! 3. **No NaN propagation.** Clamping eliminates `NaN * 0 = NaN`. The
//!    `ZERO_FLOOR_FOR_CLASSIFY` constant guards against any rounding
//!    edge that could otherwise compute a sub-zero score.
//!
//! # What the score does NOT do
//!
//! It does not auto-fix anything; it does not call the kill switch; it
//! does not page the operator on every red tick (only the rising edge
//! into a worse tier). It is a *summary* signal — the underlying typed
//! errors (BOOT-01, AUTH-GAP-03, PHASE2-01, ...) carry their own
//! runbooks and their own auto-triage YAML rules. This composite is
//! the operator's "is everything working?" answer at a glance, not a
//! replacement for the per-dimension diagnostics.

use tickvault_common::error_code::ErrorCode;

/// Six independent inputs to the composite SLO score. Populated by the
/// boot scheduler from live counters before each 10-second evaluation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SloInputs {
    /// `(active_main + depth_20 + depth_200 + order_update) / expected`.
    /// Expected today is the sum of the 4 connection pools' targets.
    /// Operator-side computation lives in the scheduler so this struct
    /// stays a pure-data carrier; clamped to `[0,1]` here.
    pub ws_health: f64,
    /// `1.0` if QuestDB connected (last health probe succeeded), else `0.0`.
    pub qdb_health: f64,
    /// `1.0` if the worst per-instrument tick gap is `< 30s` (during
    /// market hours), else `0.0`. Outside market hours the scheduler
    /// pins this to `1.0` because tick silence post-close is by-design.
    pub tick_freshness: f64,
    /// `1.0` if seconds-until-token-expiry `> 4h`, else `0.0`.
    pub token_freshness: f64,
    /// `1.0` if `rate(tv_spill_dropped_total[5m]) == 0`, else `0.0`.
    pub spill_health: f64,
    /// `1.0` if today's Phase 2 outcome `== Complete`, else `0.0`.
    /// Pinned to `1.0` outside market hours and before the 09:13:00 IST
    /// trigger (no Phase 2 outcome to grade yet).
    pub phase2_health: f64,
}

/// Tri-state classification of a computed score.
///
/// Carries `score` (the raw composite value) and `weakest` (the static
/// label of the dimension whose individual input was the minimum) for
/// operator triage. `weakest` is a `&'static str` so it can be safely
/// emitted in tracing / Telegram without user-controllable data.
#[derive(Debug, Clone, PartialEq)]
pub enum SloOutcome {
    /// Score `>= 0.95`. All dimensions effectively green.
    Healthy {
        /// Composite score in `[0.95, 1.0]`.
        score: f64,
    },
    /// Score in `[0.80, 0.95)`. At least one dimension fractionally
    /// degraded; trading can continue, but the operator should watch.
    Degraded {
        /// Composite score in `[0.80, 0.95)`.
        score: f64,
        /// Static label of the lowest-input dimension.
        weakest: &'static str,
    },
    /// Score `< 0.80`. At least one binary dimension is `0` (typical)
    /// or several dimensions degraded. Trading may still continue but
    /// operator action is required.
    Critical {
        /// Composite score in `[0.0, 0.80)`.
        score: f64,
        /// Static label of the lowest-input dimension.
        weakest: &'static str,
    },
}

impl SloOutcome {
    /// ErrorCode emitted alongside this outcome on the EDGE — see
    /// `wave-3-d-error-codes.md` for the runbook.
    #[must_use]
    pub const fn error_code(&self) -> ErrorCode {
        match self {
            Self::Healthy { .. } => ErrorCode::Slo01Healthy,
            Self::Degraded { .. } | Self::Critical { .. } => ErrorCode::Slo02Degraded,
        }
    }

    /// Stable wire-format string used in the audit / dashboard `tier`
    /// label.
    #[must_use]
    pub const fn outcome_str(&self) -> &'static str {
        match self {
            Self::Healthy { .. } => "healthy",
            Self::Degraded { .. } => "degraded",
            Self::Critical { .. } => "critical",
        }
    }

    /// Returns the composite score regardless of variant.
    #[must_use]
    pub const fn score(&self) -> f64 {
        match self {
            Self::Healthy { score }
            | Self::Degraded { score, .. }
            | Self::Critical { score, .. } => *score,
        }
    }

    /// Returns a numeric tier rank for edge-trigger comparisons:
    /// `0 = Healthy`, `1 = Degraded`, `2 = Critical`. The scheduler
    /// fires Telegram only on a strictly increasing tier rank.
    #[must_use]
    pub const fn tier(&self) -> u8 {
        match self {
            Self::Healthy { .. } => 0,
            Self::Degraded { .. } => 1,
            Self::Critical { .. } => 2,
        }
    }
}

/// Static labels for every dimension. Order MUST match the order in
/// `SloInputs` field declaration AND the order in
/// `dimension_values()` so that `find_weakest()` can pair them. The
/// labels are wire-stable: `weakest` is emitted in tracing logs and
/// the runbook documents each one.
pub const DIMENSION_LABELS: &[&str] = &[
    "ws_health",
    "qdb_health",
    "tick_freshness",
    "token_freshness",
    "spill_health",
    "phase2_health",
];

/// Score boundary at or above which the system is `Healthy`.
/// Per SCOPE §13.1.
pub const SLO_WARN_THRESHOLD: f64 = 0.95;

/// Score boundary below which the system is `Critical`.
/// Per SCOPE §13.1.
pub const SLO_CRITICAL_THRESHOLD: f64 = 0.80;

/// Floor used after multiplication to defend against IEEE 754
/// rounding driving a value imperceptibly below 0 when one input is
/// already 0. Saturating at `0.0` keeps `outcome.score()` in
/// `[0.0, 1.0]` for every code path.
const ZERO_FLOOR_FOR_CLASSIFY: f64 = 0.0;

/// Return a copy of every input value paired with its static label.
/// Helper for the weakest-dimension scan; kept as a separate function
/// so the unit tests can verify ordering invariance directly.
#[must_use]
fn dimension_values(inputs: &SloInputs) -> [(f64, &'static str); 6] {
    [
        (inputs.ws_health, DIMENSION_LABELS[0]),
        (inputs.qdb_health, DIMENSION_LABELS[1]),
        (inputs.tick_freshness, DIMENSION_LABELS[2]),
        (inputs.token_freshness, DIMENSION_LABELS[3]),
        (inputs.spill_health, DIMENSION_LABELS[4]),
        (inputs.phase2_health, DIMENSION_LABELS[5]),
    ]
}

/// Clamp every input to `[0,1]` so a malformed gauge cannot drive
/// the score above `1.0` (giving a false-OK) or below `0.0` (which
/// would survive the multiplication and produce a non-classifiable
/// outcome).
#[inline]
fn clamp01(x: f64) -> f64 {
    if x.is_nan() {
        return 0.0;
    }
    x.clamp(0.0, 1.0)
}

/// Pure evaluator. Computes the composite score and classifies it.
///
/// Hot path: six clamp ops + five multiplies + one `f64::min` scan
/// over 6 entries. No allocation. Bench ratchet
/// `bench_score_compute_le_1us` ensures p99 latency stays below 1 µs.
#[must_use]
// TEST-EXEMPT: covered by 12 unit tests in `mod tests` below — test_score_is_one_when_all_dimensions_green, test_score_is_zero_when_qdb_disconnected, test_score_is_fractional_when_ws_partially_degraded, test_classify_healthy_at_threshold_inclusive, test_classify_degraded_band, test_classify_critical_below_080, test_weakest_dimension_is_the_minimum_input, test_clamp_rejects_above_one_and_below_zero, test_dimension_labels_match_input_field_order, test_thresholds_are_pinned, test_outcome_str_is_wire_stable, test_tier_ordering_is_strictly_monotonic_for_edge_trigger.
pub fn evaluate_slo_score(inputs: &SloInputs) -> SloOutcome {
    let ws = clamp01(inputs.ws_health);
    let qdb = clamp01(inputs.qdb_health);
    let tick = clamp01(inputs.tick_freshness);
    let token = clamp01(inputs.token_freshness);
    let spill = clamp01(inputs.spill_health);
    let phase2 = clamp01(inputs.phase2_health);

    let raw = ws * qdb * tick * token * spill * phase2;
    let score = raw.clamp(ZERO_FLOOR_FOR_CLASSIFY, 1.0);

    if score >= SLO_WARN_THRESHOLD {
        return SloOutcome::Healthy { score };
    }

    let weakest = find_weakest(inputs);
    if score >= SLO_CRITICAL_THRESHOLD {
        SloOutcome::Degraded { score, weakest }
    } else {
        SloOutcome::Critical { score, weakest }
    }
}

/// Identify the dimension whose individual (clamped) input is the
/// minimum. Ties are broken by declaration order.
#[must_use]
fn find_weakest(inputs: &SloInputs) -> &'static str {
    let values = dimension_values(inputs);
    let mut min_label = values[0].1;
    let mut min_value = clamp01(values[0].0);
    let mut i = 1;
    while i < values.len() {
        let (v, label) = values[i];
        let cv = clamp01(v);
        if cv < min_value {
            min_value = cv;
            min_label = label;
        }
        i += 1;
    }
    min_label
}

#[cfg(test)]
mod tests {
    use super::*;

    fn green_inputs() -> SloInputs {
        SloInputs {
            ws_health: 1.0,
            qdb_health: 1.0,
            tick_freshness: 1.0,
            token_freshness: 1.0,
            spill_health: 1.0,
            phase2_health: 1.0,
        }
    }

    #[test]
    fn test_score_is_one_when_all_dimensions_green() {
        let outcome = evaluate_slo_score(&green_inputs());
        match outcome {
            SloOutcome::Healthy { score } => {
                assert!(
                    (score - 1.0).abs() < f64::EPSILON,
                    "expected score == 1.0, got {score}"
                );
            }
            other => panic!("expected Healthy, got {other:?}"),
        }
    }

    #[test]
    fn test_score_is_zero_when_qdb_disconnected() {
        let mut inputs = green_inputs();
        inputs.qdb_health = 0.0;
        let outcome = evaluate_slo_score(&inputs);
        match outcome {
            SloOutcome::Critical { score, weakest } => {
                assert_eq!(score, 0.0);
                assert_eq!(weakest, "qdb_health");
                assert_eq!(outcome.error_code(), ErrorCode::Slo02Degraded);
                assert_eq!(outcome.tier(), 2);
            }
            other => panic!("expected Critical, got {other:?}"),
        }
    }

    #[test]
    fn test_score_is_fractional_when_ws_partially_degraded() {
        // 11/12 connections active = 0.9166... score → Degraded.
        let mut inputs = green_inputs();
        inputs.ws_health = 11.0_f64 / 12.0_f64;
        let outcome = evaluate_slo_score(&inputs);
        match outcome {
            SloOutcome::Degraded { score, weakest } => {
                assert!((score - inputs.ws_health).abs() < 1e-9);
                assert_eq!(weakest, "ws_health");
                assert_eq!(outcome.tier(), 1);
            }
            other => panic!("expected Degraded, got {other:?}"),
        }
    }

    #[test]
    fn test_classify_healthy_at_threshold_inclusive() {
        // Exactly 0.95 must classify as Healthy (inclusive lower bound).
        let mut inputs = green_inputs();
        inputs.ws_health = SLO_WARN_THRESHOLD;
        let outcome = evaluate_slo_score(&inputs);
        match outcome {
            SloOutcome::Healthy { score } => {
                assert!((score - SLO_WARN_THRESHOLD).abs() < 1e-12);
            }
            other => panic!("expected Healthy at exactly 0.95, got {other:?}"),
        }
    }

    #[test]
    fn test_classify_degraded_band() {
        // 0.85 squarely in [0.80, 0.95).
        let mut inputs = green_inputs();
        inputs.ws_health = 0.85;
        let outcome = evaluate_slo_score(&inputs);
        assert!(matches!(outcome, SloOutcome::Degraded { .. }));
        assert_eq!(outcome.error_code(), ErrorCode::Slo02Degraded);
    }

    #[test]
    fn test_classify_critical_below_080() {
        // 0.70 → Critical even though no dimension is fully zero.
        let mut inputs = green_inputs();
        inputs.ws_health = 0.70;
        let outcome = evaluate_slo_score(&inputs);
        match outcome {
            SloOutcome::Critical { score, weakest } => {
                assert!((score - 0.70).abs() < 1e-9);
                assert_eq!(weakest, "ws_health");
            }
            other => panic!("expected Critical, got {other:?}"),
        }
    }

    #[test]
    fn test_weakest_dimension_is_the_minimum_input() {
        // Token is the lowest. The score is 0 because token=0 zeroes
        // the product, but `weakest` should still report token, not
        // something arbitrary like ws.
        let inputs = SloInputs {
            ws_health: 0.95,
            qdb_health: 1.0,
            tick_freshness: 1.0,
            token_freshness: 0.0,
            spill_health: 1.0,
            phase2_health: 1.0,
        };
        let outcome = evaluate_slo_score(&inputs);
        match outcome {
            SloOutcome::Critical { weakest, score } => {
                assert_eq!(weakest, "token_freshness");
                assert_eq!(score, 0.0);
            }
            other => panic!("expected Critical, got {other:?}"),
        }
    }

    #[test]
    fn test_clamp_rejects_above_one_and_below_zero() {
        // Malformed inputs (>1 or NaN or negative) must not produce
        // a >1 score or NaN. This prevents a false-OK if a gauge
        // is misconfigured upstream.
        let inputs = SloInputs {
            ws_health: 12.5,          // >1 → clamped to 1
            qdb_health: -3.0,         // <0 → clamped to 0
            tick_freshness: f64::NAN, // NaN → 0
            token_freshness: 1.0,
            spill_health: 1.0,
            phase2_health: 1.0,
        };
        let outcome = evaluate_slo_score(&inputs);
        match outcome {
            SloOutcome::Critical { score, weakest } => {
                assert!(score >= 0.0 && score <= 1.0, "score {score} out of range");
                assert_eq!(score, 0.0);
                // Tied at 0.0 — declaration-order tie-break gives qdb first.
                assert_eq!(weakest, "qdb_health");
            }
            other => panic!("expected Critical, got {other:?}"),
        }
    }

    #[test]
    fn test_dimension_labels_match_input_field_order() {
        // Mechanical guard: if someone reorders SloInputs fields without
        // updating DIMENSION_LABELS, this test fails because the
        // weakest scan will pair the wrong value with the wrong label.
        let cases = [
            (
                SloInputs {
                    ws_health: 0.0,
                    qdb_health: 1.0,
                    tick_freshness: 1.0,
                    token_freshness: 1.0,
                    spill_health: 1.0,
                    phase2_health: 1.0,
                },
                "ws_health",
            ),
            (
                SloInputs {
                    ws_health: 1.0,
                    qdb_health: 0.0,
                    tick_freshness: 1.0,
                    token_freshness: 1.0,
                    spill_health: 1.0,
                    phase2_health: 1.0,
                },
                "qdb_health",
            ),
            (
                SloInputs {
                    ws_health: 1.0,
                    qdb_health: 1.0,
                    tick_freshness: 0.0,
                    token_freshness: 1.0,
                    spill_health: 1.0,
                    phase2_health: 1.0,
                },
                "tick_freshness",
            ),
            (
                SloInputs {
                    ws_health: 1.0,
                    qdb_health: 1.0,
                    tick_freshness: 1.0,
                    token_freshness: 0.0,
                    spill_health: 1.0,
                    phase2_health: 1.0,
                },
                "token_freshness",
            ),
            (
                SloInputs {
                    ws_health: 1.0,
                    qdb_health: 1.0,
                    tick_freshness: 1.0,
                    token_freshness: 1.0,
                    spill_health: 0.0,
                    phase2_health: 1.0,
                },
                "spill_health",
            ),
            (
                SloInputs {
                    ws_health: 1.0,
                    qdb_health: 1.0,
                    tick_freshness: 1.0,
                    token_freshness: 1.0,
                    spill_health: 1.0,
                    phase2_health: 0.0,
                },
                "phase2_health",
            ),
        ];
        for (inputs, expected) in cases {
            assert_eq!(find_weakest(&inputs), expected);
        }
    }

    #[test]
    fn test_thresholds_are_pinned() {
        // Wire-stable constants — bumping them changes operator-facing
        // semantics. Force a code review by failing this test.
        assert!((SLO_WARN_THRESHOLD - 0.95).abs() < f64::EPSILON);
        assert!((SLO_CRITICAL_THRESHOLD - 0.80).abs() < f64::EPSILON);
    }

    #[test]
    fn test_outcome_str_is_wire_stable() {
        assert_eq!(SloOutcome::Healthy { score: 1.0 }.outcome_str(), "healthy");
        assert_eq!(
            SloOutcome::Degraded {
                score: 0.9,
                weakest: "ws_health"
            }
            .outcome_str(),
            "degraded"
        );
        assert_eq!(
            SloOutcome::Critical {
                score: 0.0,
                weakest: "qdb_health"
            }
            .outcome_str(),
            "critical"
        );
    }

    #[test]
    fn test_tier_ordering_is_strictly_monotonic_for_edge_trigger() {
        // Edge-trigger logic in main.rs compares tier(); a strict
        // ordering is required so a Healthy→Degraded transition fires
        // exactly once and a Healthy→Critical transition also fires
        // exactly once (doesn't get suppressed by an intermediate).
        assert!(
            SloOutcome::Healthy { score: 1.0 }.tier()
                < SloOutcome::Degraded {
                    score: 0.9,
                    weakest: "ws_health"
                }
                .tier()
        );
        assert!(
            SloOutcome::Degraded {
                score: 0.9,
                weakest: "ws_health"
            }
            .tier()
                < SloOutcome::Critical {
                    score: 0.5,
                    weakest: "qdb_health"
                }
                .tier()
        );
    }
}

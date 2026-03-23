//! Parameter calibration to match Dhan's exact Greeks computation.
//!
//! Dhan computes Greeks server-side with specific (r, q, day_count) parameters.
//! This module finds those exact parameters by grid search over observed data.
//!
//! # Strategy
//! 1. Feed Dhan's IV directly into our BS formulas
//! 2. Try every (r, q, day_count) combination
//! 3. Score = sum of squared differences across all strikes
//! 4. Best combination = Dhan's parameters
//!
//! # Performance
//! Grid size: 5 rates × 4 div_yields × 3 day_counts = 60 combinations.
//! Each combination evaluates all strikes. Total ~60 × N_strikes × O(1) = O(N).

use super::black_scholes::{OptionSide, compute_greeks_from_iv};

// ---------------------------------------------------------------------------
// Calibration Input
// ---------------------------------------------------------------------------

/// A single observation from Dhan's option chain for calibration.
#[derive(Debug, Clone, Copy)]
pub struct CalibrationSample {
    /// Call or Put.
    pub side: OptionSide,
    /// Underlying spot price.
    pub spot: f64,
    /// Strike price.
    pub strike: f64,
    /// Calendar days to expiry (NOT years — we convert using day_count candidate).
    pub days_to_expiry: i64,
    /// Option market price (LTP).
    pub market_price: f64,
    /// Dhan's implied volatility (decimal, e.g., 0.15 = 15%).
    pub dhan_iv: f64,
    /// Dhan's delta.
    pub dhan_delta: f64,
    /// Dhan's gamma.
    pub dhan_gamma: f64,
    /// Dhan's theta (per day).
    pub dhan_theta: f64,
    /// Dhan's vega (per 1% IV).
    pub dhan_vega: f64,
}

// ---------------------------------------------------------------------------
// Calibration Result
// ---------------------------------------------------------------------------

/// Best-fit parameters found by calibration.
#[derive(Debug, Clone, Copy)]
pub struct CalibrationResult {
    /// Best-fit risk-free rate (annualized).
    pub risk_free_rate: f64,
    /// Best-fit dividend yield (annualized).
    pub dividend_yield: f64,
    /// Best-fit day count divisor for theta (365.0, 365.25, or 252.0).
    pub day_count: f64,
    /// Total squared error at best-fit parameters.
    pub total_error: f64,
    /// Number of samples used.
    pub sample_count: usize,
    /// Mean squared error per sample per Greek.
    pub mean_error: f64,
}

// ---------------------------------------------------------------------------
// Grid Values
// ---------------------------------------------------------------------------

/// Risk-free rate candidates to test.
const RATE_GRID: &[f64] = &[0.0, 0.05, 0.06, 0.065, 0.068, 0.07, 0.075];

/// Dividend yield candidates to test.
const DIV_GRID: &[f64] = &[0.0, 0.005, 0.01, 0.012, 0.015];

/// Day count candidates to test.
const DAY_COUNT_GRID: &[f64] = &[365.0, 365.25, 252.0];

/// Epsilon for "exact match" — values within this are considered identical.
pub const EXACT_MATCH_EPSILON: f64 = 1e-4;

// ---------------------------------------------------------------------------
// Calibration Function
// ---------------------------------------------------------------------------

/// Calibrates (r, q, day_count) by grid search over observed Dhan data.
///
/// Finds the parameter combination that minimizes total squared error
/// between our Greeks (computed from Dhan's IV) and Dhan's Greeks.
///
/// # Returns
/// `Some(CalibrationResult)` if samples are non-empty, `None` if empty.
///
/// # Performance
/// O(|RATE_GRID| × |DIV_GRID| × |DAY_COUNT_GRID| × N_samples) = O(105 × N).
pub fn calibrate_parameters(samples: &[CalibrationSample]) -> Option<CalibrationResult> {
    if samples.is_empty() {
        return None;
    }

    let mut best_error = f64::MAX;
    let mut best_r = 0.0;
    let mut best_q = 0.0;
    let mut best_dc = 365.0;

    for &r in RATE_GRID {
        for &q in DIV_GRID {
            for &dc in DAY_COUNT_GRID {
                let error = compute_total_error(samples, r, q, dc);
                if error < best_error {
                    best_error = error;
                    best_r = r;
                    best_q = q;
                    best_dc = dc;
                }
            }
        }
    }

    // 4 Greeks per sample (delta, gamma, theta, vega).
    let n = samples.len() as f64;
    let mean_error = best_error / (n * 4.0);

    Some(CalibrationResult {
        risk_free_rate: best_r,
        dividend_yield: best_q,
        day_count: best_dc,
        total_error: best_error,
        sample_count: samples.len(),
        mean_error,
    })
}

/// Checks if calibration result represents an exact match (within epsilon).
pub fn is_exact_match(result: &CalibrationResult) -> bool {
    // Mean squared error per Greek per sample should be near zero.
    result.mean_error < EXACT_MATCH_EPSILON
}

// ---------------------------------------------------------------------------
// Internal
// ---------------------------------------------------------------------------

/// Computes total squared error for a given (r, q, day_count) across all samples.
fn compute_total_error(samples: &[CalibrationSample], rate: f64, div: f64, day_count: f64) -> f64 {
    let mut total = 0.0;

    for s in samples {
        // Skip invalid samples.
        if s.days_to_expiry <= 0 || s.dhan_iv <= 0.0 || s.market_price <= 0.01 {
            continue;
        }

        // Convert days to years using candidate day_count.
        let time = s.days_to_expiry as f64 / day_count;

        let our = compute_greeks_from_iv(
            s.side,
            s.spot,
            s.strike,
            time,
            rate,
            div,
            s.dhan_iv,
            s.market_price,
            day_count,
        );

        // Squared differences for each Greek.
        let d_delta = our.delta - s.dhan_delta;
        let d_gamma = our.gamma - s.dhan_gamma;
        let d_theta = our.theta - s.dhan_theta;
        let d_vega = our.vega - s.dhan_vega;

        total += d_delta * d_delta + d_gamma * d_gamma + d_theta * d_theta + d_vega * d_vega;
    }

    total
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;
    use crate::greeks::black_scholes::{self, OptionSide};

    #[test]
    fn test_calibrate_empty_returns_none() {
        assert!(calibrate_parameters(&[]).is_none());
    }

    #[test]
    fn test_calibrate_finds_known_parameters() {
        // Generate synthetic data using known parameters.
        let known_r = 0.065;
        let known_q = 0.0;
        let known_dc = 365.0;

        let spot = 23000.0;
        let days = 7;
        let time = days as f64 / known_dc;

        // Generate samples at various strikes using our BS with known params.
        let strikes = [22500.0, 22800.0, 23000.0, 23200.0, 23500.0];
        let mut samples = Vec::new();

        for &strike in &strikes {
            for &side in &[OptionSide::Call, OptionSide::Put] {
                let iv = 0.15; // 15% IV
                let market_price =
                    black_scholes::bs_price(side, spot, strike, time, known_r, known_q, iv);
                if market_price < 0.01 {
                    continue;
                }
                let greeks = black_scholes::compute_greeks_from_iv(
                    side,
                    spot,
                    strike,
                    time,
                    known_r,
                    known_q,
                    iv,
                    market_price,
                    known_dc,
                );

                samples.push(CalibrationSample {
                    side,
                    spot,
                    strike,
                    days_to_expiry: days,
                    market_price,
                    dhan_iv: iv,
                    dhan_delta: greeks.delta,
                    dhan_gamma: greeks.gamma,
                    dhan_theta: greeks.theta,
                    dhan_vega: greeks.vega,
                });
            }
        }

        let result = calibrate_parameters(&samples).unwrap();

        assert!(
            (result.risk_free_rate - known_r).abs() < f64::EPSILON,
            "rate: expected {known_r}, got {}",
            result.risk_free_rate
        );
        assert!(
            (result.dividend_yield - known_q).abs() < f64::EPSILON,
            "div: expected {known_q}, got {}",
            result.dividend_yield
        );
        assert!(
            (result.day_count - known_dc).abs() < f64::EPSILON,
            "day_count: expected {known_dc}, got {}",
            result.day_count
        );
        assert!(
            is_exact_match(&result),
            "should be exact match: mean_error={}",
            result.mean_error
        );
    }

    #[test]
    fn test_calibrate_zero_error_with_correct_params() {
        // If we feed our own Greeks as "Dhan's", calibration MUST find zero error.
        let r = 0.07;
        let q = 0.012;
        let dc = 365.25;

        let spot = 45000.0; // BANKNIFTY-like
        let days = 3;
        let time = days as f64 / dc;
        let iv = 0.20;

        let price = black_scholes::bs_call_price(spot, 45000.0, time, r, q, iv);
        let greeks = black_scholes::compute_greeks_from_iv(
            OptionSide::Call,
            spot,
            45000.0,
            time,
            r,
            q,
            iv,
            price,
            dc,
        );

        let samples = vec![CalibrationSample {
            side: OptionSide::Call,
            spot,
            strike: 45000.0,
            days_to_expiry: days,
            market_price: price,
            dhan_iv: iv,
            dhan_delta: greeks.delta,
            dhan_gamma: greeks.gamma,
            dhan_theta: greeks.theta,
            dhan_vega: greeks.vega,
        }];

        let result = calibrate_parameters(&samples).unwrap();
        assert!(
            result.total_error < 1e-20,
            "total error should be ~0: got {}",
            result.total_error
        );
    }

    #[test]
    fn test_calibrate_detects_wrong_rate() {
        // Generate with r=0.065, calibrate should NOT pick r=0.05.
        let known_r = 0.065;
        let known_q = 0.0;
        let known_dc = 365.0;

        let spot = 23000.0;
        let days = 14;
        let time = days as f64 / known_dc;
        let iv = 0.18;

        let strikes = [22000.0, 22500.0, 23000.0, 23500.0, 24000.0];
        let mut samples = Vec::new();

        for &strike in &strikes {
            let price = black_scholes::bs_call_price(spot, strike, time, known_r, known_q, iv);
            if price < 0.01 {
                continue;
            }
            let greeks = black_scholes::compute_greeks_from_iv(
                OptionSide::Call,
                spot,
                strike,
                time,
                known_r,
                known_q,
                iv,
                price,
                known_dc,
            );
            samples.push(CalibrationSample {
                side: OptionSide::Call,
                spot,
                strike,
                days_to_expiry: days,
                market_price: price,
                dhan_iv: iv,
                dhan_delta: greeks.delta,
                dhan_gamma: greeks.gamma,
                dhan_theta: greeks.theta,
                dhan_vega: greeks.vega,
            });
        }

        let result = calibrate_parameters(&samples).unwrap();
        assert!(
            (result.risk_free_rate - known_r).abs() < f64::EPSILON,
            "should find r={known_r}, got {}",
            result.risk_free_rate
        );
    }

    #[test]
    fn test_calibrate_detects_dividend_yield() {
        // Generate with q=0.01, calibrate should find it.
        let known_r = 0.065;
        let known_q = 0.01;
        let known_dc = 365.0;

        let spot = 23000.0;
        let days = 30;
        let time = days as f64 / known_dc;
        let iv = 0.16;

        let strikes = [22000.0, 22500.0, 23000.0, 23500.0, 24000.0];
        let mut samples = Vec::new();

        for &strike in &strikes {
            for &side in &[OptionSide::Call, OptionSide::Put] {
                let price = black_scholes::bs_price(side, spot, strike, time, known_r, known_q, iv);
                if price < 0.01 {
                    continue;
                }
                let greeks = black_scholes::compute_greeks_from_iv(
                    side, spot, strike, time, known_r, known_q, iv, price, known_dc,
                );
                samples.push(CalibrationSample {
                    side,
                    spot,
                    strike,
                    days_to_expiry: days,
                    market_price: price,
                    dhan_iv: iv,
                    dhan_delta: greeks.delta,
                    dhan_gamma: greeks.gamma,
                    dhan_theta: greeks.theta,
                    dhan_vega: greeks.vega,
                });
            }
        }

        let result = calibrate_parameters(&samples).unwrap();
        assert!(
            (result.dividend_yield - known_q).abs() < f64::EPSILON,
            "should find q={known_q}, got {}",
            result.dividend_yield
        );
    }

    #[test]
    fn test_calibrate_detects_day_count_convention() {
        // Generate with dc=252, calibrate should find it (not 365).
        let known_r = 0.065;
        let known_q = 0.0;
        let known_dc = 252.0;

        let spot = 23000.0;
        let days = 10;
        let time = days as f64 / known_dc;
        let iv = 0.15;

        let strikes = [22500.0, 22800.0, 23000.0, 23200.0, 23500.0];
        let mut samples = Vec::new();

        for &strike in &strikes {
            for &side in &[OptionSide::Call, OptionSide::Put] {
                let price = black_scholes::bs_price(side, spot, strike, time, known_r, known_q, iv);
                if price < 0.01 {
                    continue;
                }
                let greeks = black_scholes::compute_greeks_from_iv(
                    side, spot, strike, time, known_r, known_q, iv, price, known_dc,
                );
                samples.push(CalibrationSample {
                    side,
                    spot,
                    strike,
                    days_to_expiry: days,
                    market_price: price,
                    dhan_iv: iv,
                    dhan_delta: greeks.delta,
                    dhan_gamma: greeks.gamma,
                    dhan_theta: greeks.theta,
                    dhan_vega: greeks.vega,
                });
            }
        }

        let result = calibrate_parameters(&samples).unwrap();
        assert!(
            (result.day_count - known_dc).abs() < f64::EPSILON,
            "should find dc={known_dc}, got {}",
            result.day_count
        );
    }

    /// Helper: runs calibration for a specific `days_to_expiry` value and returns
    /// the best result along with a per-combo error table.
    fn calibrate_with_day_count(
        base_samples: &[(OptionSide, f64, f64, f64, f64, f64, f64, f64, f64)],
        days_to_expiry: i64,
    ) -> (Option<CalibrationResult>, Vec<(f64, f64, f64, f64)>) {
        let samples: Vec<CalibrationSample> = base_samples
            .iter()
            .map(
                |&(
                    side,
                    spot,
                    strike,
                    market_price,
                    dhan_iv,
                    dhan_delta,
                    dhan_gamma,
                    dhan_theta,
                    dhan_vega,
                )| {
                    CalibrationSample {
                        side,
                        spot,
                        strike,
                        days_to_expiry,
                        market_price,
                        dhan_iv,
                        dhan_delta,
                        dhan_gamma,
                        dhan_theta,
                        dhan_vega,
                    }
                },
            )
            .collect();

        // Collect per-combo errors for reporting.
        let mut combo_errors: Vec<(f64, f64, f64, f64)> = Vec::new();
        for &r in RATE_GRID {
            for &q in DIV_GRID {
                for &dc in DAY_COUNT_GRID {
                    let err = compute_total_error(&samples, r, q, dc);
                    combo_errors.push((r, q, dc, err));
                }
            }
        }

        let result = calibrate_parameters(&samples);
        (result, combo_errors)
    }

    /// Tests calibration with both days_to_expiry = 1 (exclusive) and 2 (inclusive/Dhan display).
    ///
    /// Uses synthetic data generated from known BS parameters with a specific day count.
    /// The test proves that calibration CORRECTLY identifies the day count convention
    /// and that using the wrong day count produces measurable error.
    ///
    /// Real-world scenario: March 23 (Sunday) → March 24 (Monday) expiry.
    /// - Exclusive count: expiry - today = 1 calendar day
    /// - Inclusive count (Dhan display): 2 "days to expiry"
    #[test]
    fn test_dual_day_count_calibration_exclusive_vs_inclusive() {
        // Use realistic NIFTY-like parameters.
        let spot = 23100.0;

        // --- Scenario A: Dhan uses days_to_expiry=1 internally (exclusive) ---
        // Generate "Dhan's Greeks" using days=1 and known params.
        let gen_r = 0.07;
        let gen_q = 0.0;
        let gen_dc = 365.0;
        let gen_days: i64 = 1;
        let gen_time = gen_days as f64 / gen_dc;

        let strikes = [
            22800.0, 22900.0, 23000.0, 23100.0, 23200.0, 23300.0, 23400.0,
        ];
        let iv_val = 0.12; // 12% IV — typical short-dated NIFTY

        let mut base_samples_1day: Vec<(OptionSide, f64, f64, f64, f64, f64, f64, f64, f64)> =
            Vec::new();
        for &strike in &strikes {
            for &side in &[OptionSide::Call, OptionSide::Put] {
                let price =
                    black_scholes::bs_price(side, spot, strike, gen_time, gen_r, gen_q, iv_val);
                if price < 0.01 {
                    continue;
                }
                let g = black_scholes::compute_greeks_from_iv(
                    side, spot, strike, gen_time, gen_r, gen_q, iv_val, price, gen_dc,
                );
                base_samples_1day.push((
                    side, spot, strike, price, iv_val, g.delta, g.gamma, g.theta, g.vega,
                ));
            }
        }

        // Calibrate with days_to_expiry=1 (correct: should find exact match).
        let (result_1d_correct, _) = calibrate_with_day_count(&base_samples_1day, 1);
        let r1 = result_1d_correct.unwrap();
        assert!(
            is_exact_match(&r1),
            "days=1 with data generated at days=1 should be exact match (mean_error={})",
            r1.mean_error
        );
        assert!(
            (r1.risk_free_rate - gen_r).abs() < f64::EPSILON,
            "should find r={gen_r}, got {}",
            r1.risk_free_rate
        );

        // Calibrate with days_to_expiry=2 (WRONG: should NOT find exact match).
        let (result_2d_wrong, _) = calibrate_with_day_count(&base_samples_1day, 2);
        let r2 = result_2d_wrong.unwrap();
        assert!(
            r2.mean_error > r1.mean_error,
            "Wrong day count (2 vs 1) should produce larger error: err_2d={} vs err_1d={}",
            r2.mean_error,
            r1.mean_error
        );

        // --- Scenario B: Dhan uses days_to_expiry=2 internally (inclusive) ---
        let gen_days_2: i64 = 2;
        let gen_time_2 = gen_days_2 as f64 / gen_dc;

        let mut base_samples_2day: Vec<(OptionSide, f64, f64, f64, f64, f64, f64, f64, f64)> =
            Vec::new();
        for &strike in &strikes {
            for &side in &[OptionSide::Call, OptionSide::Put] {
                let price =
                    black_scholes::bs_price(side, spot, strike, gen_time_2, gen_r, gen_q, iv_val);
                if price < 0.01 {
                    continue;
                }
                let g = black_scholes::compute_greeks_from_iv(
                    side, spot, strike, gen_time_2, gen_r, gen_q, iv_val, price, gen_dc,
                );
                base_samples_2day.push((
                    side, spot, strike, price, iv_val, g.delta, g.gamma, g.theta, g.vega,
                ));
            }
        }

        // Calibrate with days_to_expiry=2 (correct: should find exact match).
        let (result_2d_correct, _) = calibrate_with_day_count(&base_samples_2day, 2);
        let r3 = result_2d_correct.unwrap();
        assert!(
            is_exact_match(&r3),
            "days=2 with data generated at days=2 should be exact match (mean_error={})",
            r3.mean_error
        );

        // Calibrate with days_to_expiry=1 (WRONG for this data).
        let (result_1d_wrong, _) = calibrate_with_day_count(&base_samples_2day, 1);
        let r4 = result_1d_wrong.unwrap();
        assert!(
            r4.mean_error > r3.mean_error,
            "Wrong day count (1 vs 2) should produce larger error: err_1d={} vs err_2d={}",
            r4.mean_error,
            r3.mean_error
        );
    }

    /// Exhaustive grid search across BOTH day counts for every (r, q, dc) combination.
    ///
    /// This test generates synthetic "Dhan data" with known parameters (r=0.07, q=0.0, dc=365.0,
    /// days=2 — simulating Dhan's inclusive count) and verifies that the calibration:
    /// 1. Finds the exact parameters when using the correct day count
    /// 2. Cannot find an exact match when using the wrong day count
    /// 3. Reports per-combination errors for diagnostic comparison
    #[test]
    fn test_exhaustive_grid_both_day_counts() {
        let spot = 23100.0;
        let true_r = 0.07;
        let true_q = 0.0;
        let true_dc = 365.0;
        let true_days: i64 = 2; // Dhan's inclusive count convention

        let strikes = [
            22700.0, 22800.0, 22900.0, 23000.0, 23100.0, 23200.0, 23300.0, 23400.0, 23500.0,
        ];
        let iv_val = 0.14;
        let time = true_days as f64 / true_dc;

        let mut base_samples: Vec<(OptionSide, f64, f64, f64, f64, f64, f64, f64, f64)> =
            Vec::new();
        for &strike in &strikes {
            for &side in &[OptionSide::Call, OptionSide::Put] {
                let price =
                    black_scholes::bs_price(side, spot, strike, time, true_r, true_q, iv_val);
                if price < 0.01 {
                    continue;
                }
                let g = black_scholes::compute_greeks_from_iv(
                    side, spot, strike, time, true_r, true_q, iv_val, price, true_dc,
                );
                base_samples.push((
                    side, spot, strike, price, iv_val, g.delta, g.gamma, g.theta, g.vega,
                ));
            }
        }

        // Test with days_to_expiry=1 (exclusive — WRONG for this data)
        let (result_1, combos_1) = calibrate_with_day_count(&base_samples, 1);
        let best_1 = result_1.unwrap();

        // Test with days_to_expiry=2 (inclusive — CORRECT for this data)
        let (result_2, combos_2) = calibrate_with_day_count(&base_samples, 2);
        let best_2 = result_2.unwrap();

        // The correct day count (2) should yield exact match.
        assert!(
            is_exact_match(&best_2),
            "days=2 (correct) should yield exact match: mean_error={}",
            best_2.mean_error
        );
        assert!(
            (best_2.risk_free_rate - true_r).abs() < f64::EPSILON,
            "days=2: rate should be {true_r}, got {}",
            best_2.risk_free_rate
        );
        assert!(
            (best_2.dividend_yield - true_q).abs() < f64::EPSILON,
            "days=2: div_yield should be {true_q}, got {}",
            best_2.dividend_yield
        );
        assert!(
            (best_2.day_count - true_dc).abs() < f64::EPSILON,
            "days=2: day_count should be {true_dc}, got {}",
            best_2.day_count
        );

        // The wrong day count (1) should NOT yield exact match.
        assert!(
            !is_exact_match(&best_1) || best_1.mean_error > best_2.mean_error,
            "days=1 (wrong) should NOT yield exact match or should have higher error. \
             best_1 mean_error={}, best_2 mean_error={}",
            best_1.mean_error,
            best_2.mean_error
        );

        // Verify the best combo from wrong day count has non-trivial error.
        let min_error_wrong = combos_1.iter().map(|c| c.3).fold(f64::MAX, f64::min);
        let min_error_right = combos_2.iter().map(|c| c.3).fold(f64::MAX, f64::min);
        assert!(
            min_error_right < min_error_wrong,
            "Best error with correct day count ({min_error_right}) should be less than \
             best error with wrong day count ({min_error_wrong})"
        );
    }

    /// Verifies that short-dated options (1 vs 2 days) produce measurably different
    /// Greeks, proving that the day count convention MATTERS for calibration accuracy.
    #[test]
    fn test_day_count_sensitivity_short_dated() {
        let spot = 23100.0;
        let strike = 23100.0; // ATM
        let r = 0.07;
        let q = 0.0;
        let dc = 365.0;
        let iv = 0.12;

        // 1 day to expiry
        let time_1d = 1.0 / dc;
        let g1 = black_scholes::compute_greeks_from_iv(
            OptionSide::Call,
            spot,
            strike,
            time_1d,
            r,
            q,
            iv,
            black_scholes::bs_call_price(spot, strike, time_1d, r, q, iv),
            dc,
        );

        // 2 days to expiry
        let time_2d = 2.0 / dc;
        let g2 = black_scholes::compute_greeks_from_iv(
            OptionSide::Call,
            spot,
            strike,
            time_2d,
            r,
            q,
            iv,
            black_scholes::bs_call_price(spot, strike, time_2d, r, q, iv),
            dc,
        );

        // Theta should differ significantly (roughly 1.4x for sqrt(2) effect).
        let theta_ratio = g1.theta / g2.theta;
        assert!(
            theta_ratio > 1.2,
            "1-day theta ({}) should be significantly more negative than 2-day theta ({}), ratio={}",
            g1.theta,
            g2.theta,
            theta_ratio
        );

        // Delta should be closer to 0.5 for ATM, but differ between 1d and 2d.
        let delta_diff = (g1.delta - g2.delta).abs();
        assert!(
            delta_diff > 0.001,
            "ATM deltas should differ between 1d ({}) and 2d ({})",
            g1.delta,
            g2.delta
        );

        // Gamma should be higher for shorter-dated (more curvature).
        assert!(
            g1.gamma > g2.gamma,
            "1-day gamma ({}) should exceed 2-day gamma ({})",
            g1.gamma,
            g2.gamma
        );

        // Vega should be lower for shorter-dated.
        assert!(
            g1.vega < g2.vega,
            "1-day vega ({}) should be less than 2-day vega ({})",
            g1.vega,
            g2.vega
        );
    }

    #[test]
    fn test_is_exact_match_threshold() {
        let exact = CalibrationResult {
            risk_free_rate: 0.065,
            dividend_yield: 0.0,
            day_count: 365.0,
            total_error: 1e-20,
            sample_count: 10,
            mean_error: 1e-21 / 40.0,
        };
        assert!(is_exact_match(&exact));

        let not_exact = CalibrationResult {
            risk_free_rate: 0.065,
            dividend_yield: 0.0,
            day_count: 365.0,
            total_error: 1.0,
            sample_count: 10,
            mean_error: 0.025,
        };
        assert!(!is_exact_match(&not_exact));
    }
}

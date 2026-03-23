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

//! Black-Scholes option pricing model for European options (NSE style).
//!
//! Provides:
//! - `bs_call_price()` / `bs_put_price()` — theoretical option price
//! - `iv_solve()` — implied volatility from market price (Newton-Raphson)
//! - `delta()`, `gamma()`, `theta()`, `vega()` — first-order Greeks
//! - `OptionGreeks` — all Greeks computed in one pass (O(1))
//!
//! # Performance
//! All functions are O(1) — pure `f64` arithmetic, zero allocation.
//! IV solver converges in 5-15 Newton-Raphson iterations (~1μs).
//!
//! # Precision
//! Normal CDF uses Abramowitz & Stegun approximation (error < 7.5e-8).
//! IV solver uses tolerance of 1e-8 with max 50 iterations.

use std::f64::consts::PI;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// IV solver convergence tolerance.
const IV_TOLERANCE: f64 = 1e-8;

/// Maximum Newton-Raphson iterations for IV solver.
const IV_MAX_ITERATIONS: u32 = 50;

/// Initial IV guess for Newton-Raphson (30% annualized — typical for NIFTY).
const IV_INITIAL_GUESS: f64 = 0.30;

/// Minimum IV bound (prevents negative/zero volatility).
const IV_MIN: f64 = 0.001;

/// Maximum IV bound (500% — extreme but mathematically valid).
const IV_MAX: f64 = 5.0;

/// Minimum time to expiry (prevents division by zero). ~1 minute in years.
const MIN_TIME_TO_EXPIRY: f64 = 1.0 / (365.25 * 24.0 * 60.0);

// ---------------------------------------------------------------------------
// Option Type
// ---------------------------------------------------------------------------

/// European option type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionSide {
    /// Call option — right to buy.
    Call,
    /// Put option — right to sell.
    Put,
}

// ---------------------------------------------------------------------------
// Greeks Result
// ---------------------------------------------------------------------------

/// All Greeks computed in one pass from Black-Scholes.
///
/// `Copy` for zero-allocation on hot path.
#[derive(Debug, Clone, Copy)]
pub struct OptionGreeks {
    /// Implied volatility (annualized, e.g., 0.30 = 30%).
    pub iv: f64,
    /// Rate of change of option price w.r.t. underlying price.
    /// CE: [0, 1], PE: [-1, 0].
    pub delta: f64,
    /// Rate of change of delta w.r.t. underlying price.
    /// Always positive. Highest for ATM options.
    pub gamma: f64,
    /// Daily time decay (negative for long options).
    /// Expressed as price change per calendar day.
    pub theta: f64,
    /// Sensitivity to 1% change in IV.
    /// Always positive.
    pub vega: f64,
    /// Black-Scholes theoretical price.
    pub bs_price: f64,
    /// Intrinsic value: max(S-K, 0) for CE, max(K-S, 0) for PE.
    pub intrinsic: f64,
    /// Extrinsic (time) value: market_price - intrinsic.
    pub extrinsic: f64,
}

impl Default for OptionGreeks {
    fn default() -> Self {
        Self {
            iv: 0.0,
            delta: 0.0,
            gamma: 0.0,
            theta: 0.0,
            vega: 0.0,
            bs_price: 0.0,
            intrinsic: 0.0,
            extrinsic: 0.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Normal Distribution Functions (Abramowitz & Stegun)
// ---------------------------------------------------------------------------

/// Standard normal probability density function: φ(x) = e^(-x²/2) / √(2π).
///
/// O(1), pure math.
fn normal_pdf(x: f64) -> f64 {
    (-0.5 * x * x).exp() / (2.0 * PI).sqrt()
}

/// Standard normal cumulative distribution function: Φ(x) = P(Z ≤ x).
///
/// Uses Hart's approximation (1968) via rational polynomial.
/// Maximum absolute error: < 7.5e-8.
///
/// O(1), pure math, no allocation.
fn normal_cdf(x: f64) -> f64 {
    // Handle extreme values to avoid NaN/overflow.
    if x < -10.0 {
        return 0.0;
    }
    if x > 10.0 {
        return 1.0;
    }

    // Use symmetry: Φ(x) = 1 - Φ(-x) for negative x.
    let (k, abs_x) = if x < 0.0 { (1.0, -x) } else { (0.0, x) };

    // Rational approximation constants (Abramowitz & Stegun 26.2.17 applied
    // to the complementary error function, then converted to normal CDF).
    const A1: f64 = 0.319381530;
    const A2: f64 = -0.356563782;
    const A3: f64 = 1.781477937;
    const A4: f64 = -1.821255978;
    const A5: f64 = 1.330274429;
    const P: f64 = 0.2316419;

    let t = 1.0 / (1.0 + P * abs_x);
    let pdf = normal_pdf(abs_x);
    let poly = t * (A1 + t * (A2 + t * (A3 + t * (A4 + t * A5))));
    let cdf = 1.0 - pdf * poly;

    // If x was negative, use symmetry: Φ(-x) = 1 - Φ(x).
    cdf + k * (1.0 - 2.0 * cdf)
}

// ---------------------------------------------------------------------------
// Black-Scholes Core (d1, d2)
// ---------------------------------------------------------------------------

/// Computes d1 and d2 for Black-Scholes formula.
///
/// d1 = (ln(S/K) + (r - q + σ²/2) × T) / (σ × √T)
/// d2 = d1 - σ × √T
///
/// Returns (d1, d2). Caller must ensure T > 0 and sigma > 0.
#[inline]
fn compute_d1_d2(spot: f64, strike: f64, time: f64, rate: f64, div: f64, sigma: f64) -> (f64, f64) {
    let sqrt_t = time.sqrt();
    let sigma_sqrt_t = sigma * sqrt_t;
    let d1 = ((spot / strike).ln() + (rate - div + 0.5 * sigma * sigma) * time) / sigma_sqrt_t;
    let d2 = d1 - sigma_sqrt_t;
    (d1, d2)
}

// ---------------------------------------------------------------------------
// Black-Scholes Pricing
// ---------------------------------------------------------------------------

/// Black-Scholes price for a European call option.
///
/// # Arguments
/// * `spot` — Current underlying price (e.g., NIFTY 23114.50)
/// * `strike` — Option strike price (e.g., 23000.00)
/// * `time` — Time to expiry in years (e.g., 3/365.25 = 0.00821)
/// * `rate` — Risk-free rate (annualized, e.g., 0.065 = 6.5%)
/// * `div` — Dividend yield (annualized, e.g., 0.012 = 1.2%)
/// * `sigma` — Volatility (annualized, e.g., 0.30 = 30%)
pub fn bs_call_price(spot: f64, strike: f64, time: f64, rate: f64, div: f64, sigma: f64) -> f64 {
    let t = time.max(MIN_TIME_TO_EXPIRY);
    let (d1, d2) = compute_d1_d2(spot, strike, t, rate, div, sigma);
    spot * (-div * t).exp() * normal_cdf(d1) - strike * (-rate * t).exp() * normal_cdf(d2)
}

/// Black-Scholes price for a European put option.
pub fn bs_put_price(spot: f64, strike: f64, time: f64, rate: f64, div: f64, sigma: f64) -> f64 {
    let t = time.max(MIN_TIME_TO_EXPIRY);
    let (d1, d2) = compute_d1_d2(spot, strike, t, rate, div, sigma);
    strike * (-rate * t).exp() * normal_cdf(-d2) - spot * (-div * t).exp() * normal_cdf(-d1)
}

/// Black-Scholes price for either call or put.
pub fn bs_price(
    side: OptionSide,
    spot: f64,
    strike: f64,
    time: f64,
    rate: f64,
    div: f64,
    sigma: f64,
) -> f64 {
    match side {
        OptionSide::Call => bs_call_price(spot, strike, time, rate, div, sigma),
        OptionSide::Put => bs_put_price(spot, strike, time, rate, div, sigma),
    }
}

// ---------------------------------------------------------------------------
// Vega (needed by IV solver)
// ---------------------------------------------------------------------------

/// Vega: sensitivity of option price to 1% change in volatility.
///
/// vega = S × e^(-qT) × φ(d1) × √T / 100
///
/// Divided by 100 so the unit is "price change per 1% IV move".
fn vega_raw(spot: f64, strike: f64, time: f64, rate: f64, div: f64, sigma: f64) -> f64 {
    let t = time.max(MIN_TIME_TO_EXPIRY);
    let (d1, _) = compute_d1_d2(spot, strike, t, rate, div, sigma);
    spot * (-div * t).exp() * normal_pdf(d1) * t.sqrt() / 100.0
}

// ---------------------------------------------------------------------------
// IV Solver (Newton-Raphson)
// ---------------------------------------------------------------------------

/// Solves for implied volatility using Newton-Raphson iteration.
///
/// Given a market price, finds the σ that makes BS_price(σ) = market_price.
///
/// # Returns
/// `Some(iv)` if converged within tolerance, `None` if failed.
///
/// # Performance
/// O(1) — typically 5-15 iterations, max 50. Each iteration is O(1) math.
pub fn iv_solve(
    side: OptionSide,
    spot: f64,
    strike: f64,
    time: f64,
    rate: f64,
    div: f64,
    market_price: f64,
) -> Option<f64> {
    // Sanity checks — prevent nonsensical inputs.
    if spot <= 0.0 || strike <= 0.0 || time <= 0.0 || market_price <= 0.0 {
        return None;
    }

    // Intrinsic value check — market price must exceed intrinsic.
    let intrinsic = match side {
        OptionSide::Call => (spot * (-div * time).exp() - strike * (-rate * time).exp()).max(0.0),
        OptionSide::Put => (strike * (-rate * time).exp() - spot * (-div * time).exp()).max(0.0),
    };
    if market_price < intrinsic * 0.99 {
        // Price below intrinsic — arbitrage condition, IV undefined.
        return None;
    }

    let mut sigma = IV_INITIAL_GUESS;

    for _ in 0..IV_MAX_ITERATIONS {
        let price = bs_price(side, spot, strike, time, rate, div, sigma);
        let diff = price - market_price;

        if diff.abs() < IV_TOLERANCE {
            return Some(sigma);
        }

        // Vega for Newton-Raphson step (same for call and put).
        let v = vega_raw(spot, strike, time, rate, div, sigma) * 100.0;
        if v.abs() < 1e-15 {
            // Vega too small — can't converge (deep ITM/OTM with near-zero time).
            return None;
        }

        sigma -= diff / v;

        // Clamp to valid range.
        sigma = sigma.clamp(IV_MIN, IV_MAX);
    }

    // Failed to converge.
    None
}

// ---------------------------------------------------------------------------
// Greeks (all in one pass)
// ---------------------------------------------------------------------------

/// Computes all Greeks for a single option contract in O(1).
///
/// # Arguments
/// * `side` — Call or Put
/// * `spot` — Underlying price (live tick LTP)
/// * `strike` — Strike price (from instrument master)
/// * `time` — Time to expiry in years
/// * `rate` — Risk-free rate (annualized)
/// * `div` — Dividend yield (annualized)
/// * `market_price` — Current option LTP (from live tick)
///
/// # Returns
/// `Some(OptionGreeks)` if IV converges, `None` otherwise.
pub fn compute_greeks(
    side: OptionSide,
    spot: f64,
    strike: f64,
    time: f64,
    rate: f64,
    div: f64,
    market_price: f64,
) -> Option<OptionGreeks> {
    let iv = iv_solve(side, spot, strike, time, rate, div, market_price)?;
    let t = time.max(MIN_TIME_TO_EXPIRY);
    let (d1, d2) = compute_d1_d2(spot, strike, t, rate, div, iv);

    let exp_qt = (-div * t).exp();
    let exp_rt = (-rate * t).exp();
    let pdf_d1 = normal_pdf(d1);
    let sqrt_t = t.sqrt();

    // Delta
    let delta = match side {
        OptionSide::Call => exp_qt * normal_cdf(d1),
        OptionSide::Put => exp_qt * (normal_cdf(d1) - 1.0),
    };

    // Gamma (same for call and put)
    let gamma = exp_qt * pdf_d1 / (spot * iv * sqrt_t);

    // Theta (per calendar day, not per year)
    let theta_annual = match side {
        OptionSide::Call => {
            -spot * iv * exp_qt * pdf_d1 / (2.0 * sqrt_t) - rate * strike * exp_rt * normal_cdf(d2)
                + div * spot * exp_qt * normal_cdf(d1)
        }
        OptionSide::Put => {
            -spot * iv * exp_qt * pdf_d1 / (2.0 * sqrt_t) + rate * strike * exp_rt * normal_cdf(-d2)
                - div * spot * exp_qt * normal_cdf(-d1)
        }
    };
    let theta = theta_annual / 365.25; // Per calendar day

    // Vega (per 1% IV change)
    let vega = spot * exp_qt * pdf_d1 * sqrt_t / 100.0;

    // BS theoretical price
    let bs_theoretical = bs_price(side, spot, strike, t, rate, div, iv);

    // Intrinsic & extrinsic
    let intrinsic = match side {
        OptionSide::Call => (spot - strike).max(0.0),
        OptionSide::Put => (strike - spot).max(0.0),
    };
    let extrinsic = (market_price - intrinsic).max(0.0);

    Some(OptionGreeks {
        iv,
        delta,
        gamma,
        theta,
        vega,
        bs_price: bs_theoretical,
        intrinsic,
        extrinsic,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

    // Known Black-Scholes values for validation.
    // Source: Hull's "Options, Futures, and Other Derivatives" + online calculators.
    //
    // S=100, K=100, T=1, r=5%, q=0%, σ=20%
    // Call = 10.4506, Put = 5.5735
    // Delta(CE) = 0.6368, Delta(PE) = -0.3632
    // Gamma = 0.01876, Theta(daily CE) = -0.01757, Vega = 0.3752

    const S: f64 = 100.0;
    const K: f64 = 100.0;
    const T: f64 = 1.0;
    const R: f64 = 0.05;
    const Q: f64 = 0.0;
    const SIGMA: f64 = 0.20;

    // --- Normal CDF tests ---

    #[test]
    fn test_normal_cdf_zero() {
        let result = normal_cdf(0.0);
        assert!(
            (result - 0.5).abs() < 1e-7,
            "Φ(0) should be 0.5, got {result}"
        );
    }

    #[test]
    fn test_normal_cdf_positive() {
        let result = normal_cdf(1.0);
        assert!(
            (result - 0.8413).abs() < 0.001,
            "Φ(1) should be ~0.8413, got {result}"
        );
    }

    #[test]
    fn test_normal_cdf_negative() {
        let result = normal_cdf(-1.0);
        assert!(
            (result - 0.1587).abs() < 0.001,
            "Φ(-1) should be ~0.1587, got {result}"
        );
    }

    #[test]
    fn test_normal_cdf_symmetry() {
        for x in [0.5, 1.0, 1.5, 2.0, 3.0] {
            let sum = normal_cdf(x) + normal_cdf(-x);
            assert!(
                (sum - 1.0).abs() < 1e-7,
                "Φ(x) + Φ(-x) should be 1.0 for x={x}"
            );
        }
    }

    #[test]
    fn test_normal_cdf_extreme_values() {
        assert!(normal_cdf(-15.0) < 1e-15, "Φ(-15) should be ~0");
        assert!((normal_cdf(15.0) - 1.0).abs() < 1e-15, "Φ(15) should be ~1");
    }

    #[test]
    fn test_normal_pdf_at_zero() {
        let expected = 1.0 / (2.0 * PI).sqrt();
        assert!((normal_pdf(0.0) - expected).abs() < 1e-10);
    }

    // --- BS pricing tests (Hull reference values) ---

    #[test]
    fn test_bs_call_price_atm() {
        let price = bs_call_price(S, K, T, R, Q, SIGMA);
        assert!(
            (price - 10.4506).abs() < 0.01,
            "ATM call should be ~10.45, got {price}"
        );
    }

    #[test]
    fn test_bs_put_price_atm() {
        let price = bs_put_price(S, K, T, R, Q, SIGMA);
        assert!(
            (price - 5.5735).abs() < 0.01,
            "ATM put should be ~5.57, got {price}"
        );
    }

    #[test]
    fn test_put_call_parity() {
        // C - P = S*e^(-qT) - K*e^(-rT)
        let call = bs_call_price(S, K, T, R, Q, SIGMA);
        let put = bs_put_price(S, K, T, R, Q, SIGMA);
        let parity = S * (-Q * T).exp() - K * (-R * T).exp();
        assert!(
            (call - put - parity).abs() < 0.001,
            "Put-call parity violated: C-P={}, expected {parity}",
            call - put
        );
    }

    #[test]
    fn test_bs_deep_itm_call() {
        // S=150, K=100 → deep ITM call ≈ intrinsic + small time value
        let price = bs_call_price(150.0, 100.0, T, R, Q, SIGMA);
        assert!(
            price > 50.0,
            "deep ITM call should be > intrinsic (50), got {price}"
        );
    }

    #[test]
    fn test_bs_deep_otm_call() {
        // S=50, K=100 → deep OTM call ≈ very small
        let price = bs_call_price(50.0, 100.0, T, R, Q, SIGMA);
        assert!(price < 1.0, "deep OTM call should be < 1.0, got {price}");
    }

    // --- IV solver tests ---

    #[test]
    fn test_iv_solve_call_roundtrip() {
        let price = bs_call_price(S, K, T, R, Q, SIGMA);
        let iv = iv_solve(OptionSide::Call, S, K, T, R, Q, price).unwrap();
        assert!(
            (iv - SIGMA).abs() < 0.001,
            "IV should roundtrip to {SIGMA}, got {iv}"
        );
    }

    #[test]
    fn test_iv_solve_put_roundtrip() {
        let price = bs_put_price(S, K, T, R, Q, SIGMA);
        let iv = iv_solve(OptionSide::Put, S, K, T, R, Q, price).unwrap();
        assert!(
            (iv - SIGMA).abs() < 0.001,
            "IV should roundtrip to {SIGMA}, got {iv}"
        );
    }

    #[test]
    fn test_iv_solve_high_volatility() {
        let sigma = 0.80; // 80%
        let price = bs_call_price(S, K, T, R, Q, sigma);
        let iv = iv_solve(OptionSide::Call, S, K, T, R, Q, price).unwrap();
        assert!(
            (iv - sigma).abs() < 0.01,
            "IV should roundtrip to {sigma}, got {iv}"
        );
    }

    #[test]
    fn test_iv_solve_short_expiry() {
        // 3 days to expiry (typical for weekly options)
        let t = 3.0 / 365.25;
        let price = bs_call_price(23114.0, 23000.0, t, 0.065, 0.012, 0.30);
        let iv = iv_solve(OptionSide::Call, 23114.0, 23000.0, t, 0.065, 0.012, price).unwrap();
        assert!(
            (iv - 0.30).abs() < 0.01,
            "IV should be ~0.30 for NIFTY-like option, got {iv}"
        );
    }

    #[test]
    fn test_iv_solve_returns_none_for_zero_price() {
        assert!(iv_solve(OptionSide::Call, S, K, T, R, Q, 0.0).is_none());
    }

    #[test]
    fn test_iv_solve_returns_none_for_negative_inputs() {
        assert!(iv_solve(OptionSide::Call, -100.0, K, T, R, Q, 10.0).is_none());
        assert!(iv_solve(OptionSide::Call, S, -100.0, T, R, Q, 10.0).is_none());
        assert!(iv_solve(OptionSide::Call, S, K, -1.0, R, Q, 10.0).is_none());
    }

    // --- Greeks tests (Hull reference values) ---

    #[test]
    fn test_compute_greeks_atm_call() {
        let price = bs_call_price(S, K, T, R, Q, SIGMA);
        let greeks = compute_greeks(OptionSide::Call, S, K, T, R, Q, price).unwrap();

        assert!((greeks.iv - 0.20).abs() < 0.001, "IV: {}", greeks.iv);
        assert!(
            (greeks.delta - 0.6368).abs() < 0.01,
            "Delta: {}",
            greeks.delta
        );
        assert!(
            (greeks.gamma - 0.01876).abs() < 0.002,
            "Gamma: {}",
            greeks.gamma
        );
        assert!(
            greeks.theta < 0.0,
            "Theta should be negative for long call: {}",
            greeks.theta
        );
        assert!(
            greeks.vega > 0.0,
            "Vega should be positive: {}",
            greeks.vega
        );
    }

    #[test]
    fn test_compute_greeks_atm_put() {
        let price = bs_put_price(S, K, T, R, Q, SIGMA);
        let greeks = compute_greeks(OptionSide::Put, S, K, T, R, Q, price).unwrap();

        assert!(
            (greeks.delta - (-0.3632)).abs() < 0.01,
            "Put delta: {}",
            greeks.delta
        );
        assert!(
            greeks.gamma > 0.0,
            "Gamma always positive: {}",
            greeks.gamma
        );
        assert!(greeks.theta < 0.0, "Theta negative: {}", greeks.theta);
    }

    #[test]
    fn test_call_put_delta_sum() {
        // |delta_call| + |delta_put| ≈ 1.0 (for same strike/expiry)
        let call_price = bs_call_price(S, K, T, R, Q, SIGMA);
        let put_price = bs_put_price(S, K, T, R, Q, SIGMA);
        let call_greeks = compute_greeks(OptionSide::Call, S, K, T, R, Q, call_price).unwrap();
        let put_greeks = compute_greeks(OptionSide::Put, S, K, T, R, Q, put_price).unwrap();

        let delta_sum = call_greeks.delta + put_greeks.delta.abs();
        assert!(
            (delta_sum - 1.0).abs() < 0.05,
            "|delta_CE| + |delta_PE| should be ~1.0, got {delta_sum}"
        );
    }

    #[test]
    fn test_gamma_same_for_call_and_put() {
        let call_price = bs_call_price(S, K, T, R, Q, SIGMA);
        let put_price = bs_put_price(S, K, T, R, Q, SIGMA);
        let call_greeks = compute_greeks(OptionSide::Call, S, K, T, R, Q, call_price).unwrap();
        let put_greeks = compute_greeks(OptionSide::Put, S, K, T, R, Q, put_price).unwrap();

        assert!(
            (call_greeks.gamma - put_greeks.gamma).abs() < 0.001,
            "Gamma should be same for CE and PE at same strike"
        );
    }

    #[test]
    fn test_vega_same_for_call_and_put() {
        let call_price = bs_call_price(S, K, T, R, Q, SIGMA);
        let put_price = bs_put_price(S, K, T, R, Q, SIGMA);
        let call_greeks = compute_greeks(OptionSide::Call, S, K, T, R, Q, call_price).unwrap();
        let put_greeks = compute_greeks(OptionSide::Put, S, K, T, R, Q, put_price).unwrap();

        assert!(
            (call_greeks.vega - put_greeks.vega).abs() < 0.001,
            "Vega should be same for CE and PE at same strike"
        );
    }

    #[test]
    fn test_nifty_like_option() {
        // Real-world NIFTY-like: S=23114, K=23000, T=3 days, r=6.5%, q=1.2%, σ=30%
        let spot = 23114.50;
        let strike = 23000.0;
        let time = 3.0 / 365.25;
        let rate = 0.065;
        let div = 0.012;
        let sigma = 0.2952; // ATM IV ~29.52% from screenshot

        let call_price = bs_call_price(spot, strike, time, rate, div, sigma);
        // ITM by 114 points + time value → realistic range 100-500.
        assert!(
            call_price > 100.0 && call_price < 500.0,
            "NIFTY 23000 CE with 3 days should be 100-500, got {call_price}"
        );

        let greeks =
            compute_greeks(OptionSide::Call, spot, strike, time, rate, div, call_price).unwrap();
        assert!(
            greeks.delta > 0.4 && greeks.delta < 0.8,
            "Delta: {}",
            greeks.delta
        );
        assert!(greeks.gamma > 0.0, "Gamma: {}", greeks.gamma);
        assert!(greeks.theta < 0.0, "Theta: {}", greeks.theta);
        assert!(greeks.vega > 0.0, "Vega: {}", greeks.vega);
    }

    #[test]
    fn test_intrinsic_extrinsic_itm_call() {
        // ITM call: S=110, K=100
        let price = bs_call_price(110.0, K, T, R, Q, SIGMA);
        let greeks = compute_greeks(OptionSide::Call, 110.0, K, T, R, Q, price).unwrap();
        assert!(
            (greeks.intrinsic - 10.0).abs() < 0.01,
            "Intrinsic: {}",
            greeks.intrinsic
        );
        assert!(
            greeks.extrinsic > 0.0,
            "Extrinsic should be positive: {}",
            greeks.extrinsic
        );
    }

    #[test]
    fn test_intrinsic_extrinsic_otm_call() {
        // OTM call: S=90, K=100
        let price = bs_call_price(90.0, K, T, R, Q, SIGMA);
        let greeks = compute_greeks(OptionSide::Call, 90.0, K, T, R, Q, price).unwrap();
        assert!(
            (greeks.intrinsic - 0.0).abs() < 0.01,
            "OTM intrinsic should be 0"
        );
        assert!(
            (greeks.extrinsic - price).abs() < 0.01,
            "OTM extrinsic should equal price"
        );
    }

    // --- Edge cases ---

    #[test]
    fn test_compute_greeks_returns_none_for_zero_price() {
        assert!(compute_greeks(OptionSide::Call, S, K, T, R, Q, 0.0).is_none());
    }

    #[test]
    fn test_bs_price_near_zero_time() {
        // ITM call at expiry → intrinsic value
        let price = bs_call_price(110.0, 100.0, 0.0001, R, Q, SIGMA);
        assert!(
            (price - 10.0).abs() < 0.5,
            "Near-expiry ITM call should be ~intrinsic (10), got {price}"
        );
    }

    #[test]
    fn test_option_side_eq() {
        assert_eq!(OptionSide::Call, OptionSide::Call);
        assert_ne!(OptionSide::Call, OptionSide::Put);
    }

    #[test]
    fn test_option_greeks_default() {
        let g = OptionGreeks::default();
        assert_eq!(g.iv, 0.0);
        assert_eq!(g.delta, 0.0);
    }

    #[test]
    fn test_option_greeks_is_copy() {
        let g = OptionGreeks::default();
        let g2 = g; // Copy
        assert_eq!(g.iv, g2.iv);
    }

    #[test]
    fn test_bs_price_call_delegates_correctly() {
        let direct = bs_call_price(S, K, T, R, Q, SIGMA);
        let via_wrapper = bs_price(OptionSide::Call, S, K, T, R, Q, SIGMA);
        assert!((direct - via_wrapper).abs() < f64::EPSILON);
    }

    #[test]
    fn test_bs_price_put_delegates_correctly() {
        let direct = bs_put_price(S, K, T, R, Q, SIGMA);
        let via_wrapper = bs_price(OptionSide::Put, S, K, T, R, Q, SIGMA);
        assert!((direct - via_wrapper).abs() < f64::EPSILON);
    }

    #[test]
    fn test_iv_solve_various_strikes() {
        // Test IV convergence across different moneyness levels.
        for strike in [80.0, 90.0, 100.0, 110.0, 120.0] {
            let price = bs_call_price(S, strike, T, R, Q, SIGMA);
            if price > 0.01 {
                let iv = iv_solve(OptionSide::Call, S, strike, T, R, Q, price);
                assert!(iv.is_some(), "IV should converge for strike={strike}");
                assert!(
                    (iv.unwrap() - SIGMA).abs() < 0.01,
                    "IV roundtrip failed for strike={strike}: got {}",
                    iv.unwrap()
                );
            }
        }
    }

    // ===================================================================
    // COMPREHENSIVE VERIFICATION SUITE
    // Mathematical invariants, real-world values, edge cases, stress tests
    // ===================================================================

    // --- Mathematical Invariants (must ALWAYS hold) ---

    #[test]
    fn test_invariant_put_call_parity_sweep() {
        // C - P = S*e^(-qT) - K*e^(-rT) must hold for ALL valid inputs.
        for s in [50.0, 80.0, 100.0, 120.0, 200.0] {
            for k in [50.0, 80.0, 100.0, 120.0, 200.0] {
                for t in [0.01, 0.1, 0.5, 1.0, 2.0] {
                    for sigma in [0.10, 0.30, 0.50, 1.0] {
                        let call = bs_call_price(s, k, t, R, Q, sigma);
                        let put = bs_put_price(s, k, t, R, Q, sigma);
                        let parity = s * (-Q * t).exp() - k * (-R * t).exp();
                        let diff = (call - put - parity).abs();
                        assert!(
                            diff < 0.001,
                            "Parity violated: S={s} K={k} T={t} σ={sigma} diff={diff}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn test_invariant_call_price_always_positive() {
        for s in [50.0, 100.0, 200.0] {
            for k in [50.0, 100.0, 200.0] {
                for t in [0.01, 0.5, 1.0, 5.0] {
                    for sigma in [0.05, 0.20, 0.50, 1.0] {
                        let p = bs_call_price(s, k, t, R, Q, sigma);
                        assert!(p >= 0.0, "Call >= 0: S={s} K={k} T={t} σ={sigma} got {p}");
                    }
                }
            }
        }
    }

    #[test]
    fn test_invariant_put_price_always_positive() {
        for s in [50.0, 100.0, 200.0] {
            for k in [50.0, 100.0, 200.0] {
                for t in [0.01, 0.5, 1.0, 5.0] {
                    for sigma in [0.05, 0.20, 0.50, 1.0] {
                        let p = bs_put_price(s, k, t, R, Q, sigma);
                        assert!(p >= 0.0, "Put >= 0: S={s} K={k} T={t} σ={sigma} got {p}");
                    }
                }
            }
        }
    }

    #[test]
    fn test_invariant_call_delta_bounded() {
        for s in [50.0, 100.0, 200.0] {
            for k in [50.0, 100.0, 200.0] {
                for t in [0.01, 0.1, 1.0] {
                    let price = bs_call_price(s, k, t, R, Q, 0.30);
                    if let Some(g) = compute_greeks(OptionSide::Call, s, k, t, R, Q, price) {
                        assert!(
                            g.delta >= -0.01 && g.delta <= 1.01,
                            "Call delta ∈ [0,1]: S={s} K={k} T={t} got {}",
                            g.delta
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn test_invariant_put_delta_bounded() {
        for s in [50.0, 100.0, 200.0] {
            for k in [50.0, 100.0, 200.0] {
                for t in [0.01, 0.1, 1.0] {
                    let price = bs_put_price(s, k, t, R, Q, 0.30);
                    if let Some(g) = compute_greeks(OptionSide::Put, s, k, t, R, Q, price) {
                        assert!(
                            g.delta >= -1.01 && g.delta <= 0.01,
                            "Put delta ∈ [-1,0]: S={s} K={k} T={t} got {}",
                            g.delta
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn test_invariant_gamma_always_positive() {
        for s in [50.0, 100.0, 200.0] {
            for k in [50.0, 100.0, 200.0] {
                let price = bs_call_price(s, k, T, R, Q, 0.30);
                if let Some(g) = compute_greeks(OptionSide::Call, s, k, T, R, Q, price) {
                    assert!(g.gamma >= 0.0, "Gamma >= 0: S={s} K={k} got {}", g.gamma);
                }
            }
        }
    }

    #[test]
    fn test_invariant_vega_always_positive() {
        for s in [50.0, 100.0, 200.0] {
            for k in [50.0, 100.0, 200.0] {
                let price = bs_call_price(s, k, T, R, Q, 0.30);
                if let Some(g) = compute_greeks(OptionSide::Call, s, k, T, R, Q, price) {
                    assert!(g.vega >= 0.0, "Vega >= 0: S={s} K={k} got {}", g.vega);
                }
            }
        }
    }

    #[test]
    fn test_invariant_call_monotone_increasing_in_spot() {
        let mut prev = 0.0;
        for s in (50..=150).step_by(5) {
            let p = bs_call_price(s as f64, K, T, R, Q, SIGMA);
            assert!(
                p >= prev,
                "Call must increase with S: S={s} price={p} prev={prev}"
            );
            prev = p;
        }
    }

    #[test]
    fn test_invariant_put_monotone_decreasing_in_spot() {
        let mut prev = f64::MAX;
        for s in (50..=150).step_by(5) {
            let p = bs_put_price(s as f64, K, T, R, Q, SIGMA);
            assert!(
                p <= prev,
                "Put must decrease with S: S={s} price={p} prev={prev}"
            );
            prev = p;
        }
    }

    #[test]
    fn test_invariant_price_increases_with_volatility() {
        let mut prev_c = 0.0;
        let mut prev_p = 0.0;
        for sigma_pct in (5..=100).step_by(5) {
            let sigma = sigma_pct as f64 / 100.0;
            let c = bs_call_price(S, K, T, R, Q, sigma);
            let p = bs_put_price(S, K, T, R, Q, sigma);
            assert!(c >= prev_c, "Call increases with σ: σ={sigma} c={c}");
            assert!(p >= prev_p, "Put increases with σ: σ={sigma} p={p}");
            prev_c = c;
            prev_p = p;
        }
    }

    #[test]
    fn test_invariant_call_bounded_by_spot() {
        for s in [50.0, 100.0, 200.0] {
            for k in [50.0, 100.0, 200.0] {
                let p = bs_call_price(s, k, T, R, Q, SIGMA);
                assert!(p <= s * 1.01, "Call <= S: S={s} K={k} price={p}");
            }
        }
    }

    // --- IV Solver Stress Tests ---

    #[test]
    fn test_iv_roundtrip_all_volatilities() {
        for sigma_pct in (5..=200).step_by(5) {
            let sigma = sigma_pct as f64 / 100.0;
            let price = bs_call_price(S, K, T, R, Q, sigma);
            if price > 0.01 {
                let iv = iv_solve(OptionSide::Call, S, K, T, R, Q, price);
                assert!(iv.is_some(), "IV converge for σ={}%", sigma_pct);
                assert!(
                    (iv.unwrap() - sigma).abs() < 0.005,
                    "IV roundtrip σ={}%: got {}",
                    sigma_pct,
                    iv.unwrap()
                );
            }
        }
    }

    #[test]
    fn test_iv_roundtrip_all_moneyness_call() {
        for strike in (60..=140).step_by(5) {
            let k = strike as f64;
            let price = bs_call_price(S, k, T, R, Q, SIGMA);
            if price > 0.01 {
                let iv = iv_solve(OptionSide::Call, S, k, T, R, Q, price);
                assert!(iv.is_some(), "Call IV converge K={k}");
                assert!(
                    (iv.unwrap() - SIGMA).abs() < 0.005,
                    "Call IV K={k}: got {}",
                    iv.unwrap()
                );
            }
        }
    }

    #[test]
    fn test_iv_roundtrip_all_moneyness_put() {
        for strike in (60..=140).step_by(5) {
            let k = strike as f64;
            let price = bs_put_price(S, k, T, R, Q, SIGMA);
            if price > 0.01 {
                let iv = iv_solve(OptionSide::Put, S, k, T, R, Q, price);
                assert!(iv.is_some(), "Put IV converge K={k}");
                assert!(
                    (iv.unwrap() - SIGMA).abs() < 0.005,
                    "Put IV K={k}: got {}",
                    iv.unwrap()
                );
            }
        }
    }

    #[test]
    fn test_iv_roundtrip_short_expiries() {
        for days in 1..=7 {
            let t = days as f64 / 365.25;
            let price = bs_call_price(S, K, t, R, Q, SIGMA);
            if price > 0.01 {
                let iv = iv_solve(OptionSide::Call, S, K, t, R, Q, price);
                assert!(iv.is_some(), "IV converge T={days}d");
                assert!(
                    (iv.unwrap() - SIGMA).abs() < 0.01,
                    "IV T={days}d: got {}",
                    iv.unwrap()
                );
            }
        }
    }

    // --- Real-World NIFTY Values (from Dhan screenshot) ---

    #[test]
    fn test_real_nifty_greeks_direction() {
        // NIFTY 50 spot=23114.50, 3 days to expiry, r=6.5%, q=1.2%
        let spot = 23114.50;
        let time = 3.0 / 365.25;
        let rate = 0.065;
        let div = 0.012;

        // ITM call (23000 CE) — delta should be > 0.5
        let ce_itm = 296.50;
        if let Some(g) = compute_greeks(OptionSide::Call, spot, 23000.0, time, rate, div, ce_itm) {
            assert!(g.delta > 0.5, "ITM CE delta > 0.5: got {}", g.delta);
            assert!(g.theta < 0.0, "Theta negative: {}", g.theta);
            assert!(g.gamma > 0.0, "Gamma positive: {}", g.gamma);
            assert!(g.vega > 0.0, "Vega positive: {}", g.vega);
            assert!(g.iv > 0.0 && g.iv < 1.0, "IV reasonable: {}", g.iv);
        }

        // OTM put (23000 PE) — delta should be between -0.5 and 0
        let pe_otm = 173.75;
        if let Some(g) = compute_greeks(OptionSide::Put, spot, 23000.0, time, rate, div, pe_otm) {
            assert!(
                g.delta < 0.0 && g.delta > -1.0,
                "OTM PE delta ∈ (-1,0): {}",
                g.delta
            );
        }
    }

    #[test]
    fn test_real_nifty_gamma_highest_atm() {
        let spot = 23114.50;
        let time = 3.0 / 365.25;
        let rate = 0.065;
        let div = 0.012;

        let atm = bs_call_price(spot, 23100.0, time, rate, div, 0.30);
        let itm = bs_call_price(spot, 22700.0, time, rate, div, 0.30);
        let otm = bs_call_price(spot, 23500.0, time, rate, div, 0.30);

        let ga = compute_greeks(OptionSide::Call, spot, 23100.0, time, rate, div, atm).unwrap();
        let gi = compute_greeks(OptionSide::Call, spot, 22700.0, time, rate, div, itm).unwrap();
        let go = compute_greeks(OptionSide::Call, spot, 23500.0, time, rate, div, otm).unwrap();

        assert!(ga.gamma > gi.gamma, "ATM gamma > ITM gamma");
        assert!(ga.gamma > go.gamma, "ATM gamma > OTM gamma");
    }

    // --- Numerical Stability ---

    #[test]
    fn test_edge_very_small_time() {
        let t = 1.0 / (365.25 * 24.0 * 60.0); // 1 minute
        let c = bs_call_price(110.0, 100.0, t, R, Q, SIGMA);
        assert!(c > 9.0 && c < 11.0, "Near-expiry ITM ≈ intrinsic: got {c}");
    }

    #[test]
    fn test_edge_extreme_volatility() {
        let c = bs_call_price(S, K, T, R, Q, 3.0); // 300%
        assert!(
            c.is_finite() && c > 0.0,
            "300% vol finite positive: got {c}"
        );
    }

    #[test]
    fn test_edge_very_low_volatility() {
        let c = bs_call_price(110.0, 100.0, T, R, Q, 0.001);
        let intrinsic = 110.0 - 100.0 * (-R * T).exp();
        assert!(
            (c - intrinsic).abs() < 1.0,
            "Low vol ITM ≈ intrinsic PV: got {c}"
        );
    }

    #[test]
    fn test_edge_deep_itm_1000() {
        let c = bs_call_price(1000.0, 100.0, T, R, Q, SIGMA);
        assert!(c > 895.0, "Deep ITM (1000/100): got {c}");
    }

    #[test]
    fn test_edge_deep_otm_tiny() {
        let c = bs_call_price(10.0, 1000.0, T, R, Q, SIGMA);
        assert!(c < 0.001, "Deep OTM near zero: got {c}");
    }

    #[test]
    fn test_edge_zero_rate() {
        let c = bs_call_price(S, K, T, 0.0, Q, SIGMA);
        assert!(c > 0.0 && c.is_finite(), "Zero rate valid: got {c}");
    }

    #[test]
    fn test_normal_cdf_precision_table() {
        // Standard normal CDF table values (4 decimal precision).
        let cases = [
            (-3.0, 0.0013),
            (-2.0, 0.0228),
            (-1.0, 0.1587),
            (-0.5, 0.3085),
            (0.0, 0.5000),
            (0.5, 0.6915),
            (1.0, 0.8413),
            (2.0, 0.9772),
            (3.0, 0.9987),
        ];
        for (x, expected) in cases {
            let got = normal_cdf(x);
            assert!(
                (got - expected).abs() < 0.001,
                "Φ({x}) = {expected}, got {got}"
            );
        }
    }

    // --- Greeks Consistency: call/put gamma/vega match ---

    #[test]
    fn test_greeks_call_put_gamma_vega_same_sweep() {
        for k in [80.0, 90.0, 100.0, 110.0, 120.0] {
            let cp = bs_call_price(S, k, T, R, Q, SIGMA);
            let pp = bs_put_price(S, k, T, R, Q, SIGMA);
            if let (Some(cg), Some(pg)) = (
                compute_greeks(OptionSide::Call, S, k, T, R, Q, cp),
                compute_greeks(OptionSide::Put, S, k, T, R, Q, pp),
            ) {
                assert!(
                    (cg.gamma - pg.gamma).abs() < 0.0001,
                    "Gamma K={k}: CE={} PE={}",
                    cg.gamma,
                    pg.gamma
                );
                assert!(
                    (cg.vega - pg.vega).abs() < 0.001,
                    "Vega K={k}: CE={} PE={}",
                    cg.vega,
                    pg.vega
                );
            }
        }
    }

    #[test]
    fn test_greeks_delta_call_minus_put() {
        // delta_call - delta_put = e^(-qT)
        for k in [80.0, 90.0, 100.0, 110.0, 120.0] {
            let cp = bs_call_price(S, k, T, R, Q, SIGMA);
            let pp = bs_put_price(S, k, T, R, Q, SIGMA);
            if let (Some(cg), Some(pg)) = (
                compute_greeks(OptionSide::Call, S, k, T, R, Q, cp),
                compute_greeks(OptionSide::Put, S, k, T, R, Q, pp),
            ) {
                let expected = (-Q * T).exp();
                let actual = cg.delta - pg.delta;
                assert!(
                    (actual - expected).abs() < 0.02,
                    "delta_CE - delta_PE = e^(-qT): K={k} got {actual} expected {expected}"
                );
            }
        }
    }
}

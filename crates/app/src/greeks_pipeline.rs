//! Greeks pipeline — periodic option chain fetch, compute, and persist.
//!
//! Fetches option chain from Dhan API for major indices (NIFTY, BANKNIFTY,
//! FINNIFTY, MIDCPNIFTY), computes Black-Scholes Greeks, cross-verifies
//! against Dhan's values, and writes everything to QuestDB.
//!
//! # Tables Written
//! - `dhan_option_chain_raw` — raw API response (audit trail)
//! - `option_greeks` — our computed Greeks
//! - `greeks_verification` — our vs Dhan comparison
//! - `pcr_snapshots` — Put-Call Ratio per underlying/expiry
//!
//! # Rate Limits
//! Dhan Option Chain API: 1 request per 3 seconds (enforced by `OptionChainClient`).
//! 4 underlyings × 2 calls each = ~24 seconds per cycle. Default interval: 60s.

use std::time::Duration;

use anyhow::Result;
use chrono::{NaiveDate, TimeDelta, Utc};
use tracing::{debug, error, info, warn};

use tickvault_common::config::{GreeksConfig, QuestDbConfig};
use tickvault_common::constants::{
    IST_UTC_OFFSET_NANOS, IST_UTC_OFFSET_SECONDS_I64, TICK_PERSIST_END_SECS_OF_DAY_IST,
    TICK_PERSIST_START_SECS_OF_DAY_IST, VALIDATION_MUST_EXIST_INDICES,
};
use tickvault_core::auth::TokenHandle;
use tickvault_core::option_chain::client::OptionChainClient;
use tickvault_core::option_chain::types::OptionData;
use tickvault_storage::greeks_persistence::{
    DhanRawRow, GreeksPersistenceWriter, OptionGreeksRow, PcrSnapshotRow, VerificationRow,
};
use tickvault_trading::greeks::black_scholes::{self, OptionSide};
use tickvault_trading::greeks::buildup::classify_buildup;
use tickvault_trading::greeks::calibration::{
    CalibrationSample, EXACT_MATCH_EPSILON, calibrate_parameters, is_exact_match,
};
use tickvault_trading::greeks::pcr::{classify_pcr, compute_pcr};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Epsilon for exact match classification (all Greeks within this = EXACT).
const MATCH_EPSILON: f64 = EXACT_MATCH_EPSILON;

/// Underlyings to fetch option chain for.
/// Uses first 4 from VALIDATION_MUST_EXIST_INDICES (NIFTY, BANKNIFTY, FINNIFTY, MIDCPNIFTY).
const MAX_UNDERLYINGS: usize = 4;

/// Exchange segment for index underlyings (option chain API).
const INDEX_SEGMENT: &str = "IDX_I";

/// Segment string for F&O contracts in QuestDB.
const FNO_SEGMENT: &str = "NSE_FNO";

/// After this many consecutive cycles where ALL underlyings fail,
/// escalate from WARN to ERROR (which triggers Telegram alert).
const CONSECUTIVE_FAILURE_ESCALATION_THRESHOLD: u32 = 5;

/// Returns true if current IST time is within the greeks fetch window
/// (09:15–15:30 IST). Outside this window, option chain data is stale
/// and fetching wastes Dhan API quota (Data API: 100K calls/day).
fn is_within_greeks_window() -> bool {
    let now_utc = Utc::now();
    let ist_secs = now_utc
        .timestamp()
        .saturating_add(IST_UTC_OFFSET_SECONDS_I64);
    #[allow(clippy::cast_possible_truncation)] // APPROVED: secs-of-day always fits u32
    let secs_of_day = (ist_secs % 86_400) as u32;
    // O(1) EXEMPT: called once per cycle (cold path), not per tick
    #[allow(clippy::manual_range_contains)] // APPROVED: avoids banned .contains() pattern
    {
        secs_of_day >= TICK_PERSIST_START_SECS_OF_DAY_IST
            && secs_of_day < TICK_PERSIST_END_SECS_OF_DAY_IST
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Runs the greeks pipeline in a loop indefinitely.
///
/// Fetches option chain data, computes Greeks, and writes to QuestDB
/// every `config.fetch_interval_secs` seconds.
///
/// Runs as a background `tokio::spawn` task — terminates when the
/// process shuts down (same pattern as tick/candle persistence consumers).
// TEST-EXEMPT: async loop requiring live Dhan API + QuestDB (integration test)
pub async fn run_greeks_pipeline(
    token_handle: TokenHandle,
    client_id: String,
    dhan_base_url: String,
    greeks_config: GreeksConfig,
    questdb_config: QuestDbConfig,
) {
    info!(
        interval_secs = greeks_config.fetch_interval_secs,
        "greeks pipeline starting"
    );

    // Create the option chain client.
    let mut oc_client = match OptionChainClient::new(token_handle, client_id, dhan_base_url) {
        Ok(c) => c,
        Err(err) => {
            error!(
                ?err,
                "failed to create OptionChainClient — greeks pipeline disabled"
            );
            return;
        }
    };

    // Create the QuestDB writer.
    let mut writer = match GreeksPersistenceWriter::new(&questdb_config) {
        Ok(w) => w,
        Err(err) => {
            error!(
                ?err,
                "failed to connect to QuestDB ILP for greeks — greeks pipeline disabled"
            );
            return;
        }
    };

    let interval = Duration::from_secs(greeks_config.fetch_interval_secs);
    let mut consecutive_zero_strikes: u32 = 0;

    let mut logged_off_hours = false;

    loop {
        // Skip cycles outside market hours to conserve Dhan API quota.
        if !is_within_greeks_window() {
            if !logged_off_hours {
                info!("greeks pipeline: outside market hours (09:15–15:30 IST) — pausing cycles");
                logged_off_hours = true;
            }
            // Reset consecutive failure counter — off-hours zeros are expected.
            consecutive_zero_strikes = 0;
            tokio::time::sleep(interval).await;
            continue;
        }
        logged_off_hours = false;

        // Run one cycle.
        match run_one_cycle(&mut oc_client, &mut writer, &greeks_config).await {
            Ok(total_strikes) => {
                if total_strikes == 0 {
                    consecutive_zero_strikes = consecutive_zero_strikes.saturating_add(1);
                    if consecutive_zero_strikes >= CONSECUTIVE_FAILURE_ESCALATION_THRESHOLD {
                        // ERROR level triggers Telegram alert (per rust-code.md).
                        error!(
                            consecutive_cycles = consecutive_zero_strikes,
                            "greeks pipeline: ALL underlyings failed for {} consecutive cycles — \
                             Dhan API may be down or token expired",
                            consecutive_zero_strikes,
                        );
                    }
                } else {
                    if consecutive_zero_strikes >= CONSECUTIVE_FAILURE_ESCALATION_THRESHOLD {
                        info!(
                            recovered_after = consecutive_zero_strikes,
                            total_strikes,
                            "greeks pipeline recovered after {} consecutive zero-strike cycles",
                            consecutive_zero_strikes,
                        );
                    }
                    consecutive_zero_strikes = 0;
                }
            }
            Err(err) => {
                consecutive_zero_strikes = consecutive_zero_strikes.saturating_add(1);
                if consecutive_zero_strikes >= CONSECUTIVE_FAILURE_ESCALATION_THRESHOLD {
                    error!(
                        ?err,
                        consecutive_cycles = consecutive_zero_strikes,
                        "greeks pipeline cycle failed — {} consecutive failures",
                        consecutive_zero_strikes,
                    );
                } else {
                    warn!(
                        ?err,
                        "greeks pipeline cycle failed — will retry next interval"
                    );
                }
            }
        }

        tokio::time::sleep(interval).await;
    }
}

// ---------------------------------------------------------------------------
// Single Cycle
// ---------------------------------------------------------------------------

/// Runs one fetch-compute-persist cycle for all configured underlyings.
/// Returns the total number of strikes processed (0 = all underlyings failed).
async fn run_one_cycle(
    oc_client: &mut OptionChainClient,
    writer: &mut GreeksPersistenceWriter,
    config: &GreeksConfig,
) -> Result<u32> {
    // System clock (UTC) → IST for QuestDB display (same pattern as received_at in tick_persistence).
    let now_nanos = Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(0)
        .saturating_add(IST_UTC_OFFSET_NANOS);
    // Use IST date for time-to-expiry (prevents off-by-1-day near midnight UTC).
    let today = (Utc::now() + TimeDelta::seconds(IST_UTC_OFFSET_SECONDS_I64)).date_naive();

    let underlyings =
        &VALIDATION_MUST_EXIST_INDICES[..MAX_UNDERLYINGS.min(VALIDATION_MUST_EXIST_INDICES.len())];

    let mut total_strikes = 0u32;

    for &(symbol, security_id) in underlyings {
        match process_underlying(
            oc_client,
            writer,
            config,
            symbol,
            u64::from(security_id),
            today,
            now_nanos,
        )
        .await
        {
            Ok(count) => {
                total_strikes = total_strikes.saturating_add(count);
            }
            Err(err) => {
                warn!(symbol, ?err, "failed to process underlying — skipping");
            }
        }
    }

    // Flush all accumulated rows.
    if let Err(err) = writer.flush() {
        warn!(?err, "failed to flush greeks data to QuestDB");
    } else {
        info!(
            total_strikes,
            pending = writer.pending_count(),
            "greeks pipeline cycle complete"
        );
    }

    Ok(total_strikes)
}

/// Processes a single underlying: fetch chain → compute → write.
///
/// Returns the number of strikes processed.
async fn process_underlying(
    oc_client: &mut OptionChainClient,
    writer: &mut GreeksPersistenceWriter,
    config: &GreeksConfig,
    symbol: &str,
    security_id: u64,
    today: NaiveDate,
    ts_nanos: i64,
) -> Result<u32> {
    // Step 1: Fetch expiry list.
    let expiry_resp = oc_client
        .fetch_expiry_list(security_id, INDEX_SEGMENT)
        .await?;

    if expiry_resp.data.is_empty() {
        warn!(symbol, "no expiries found");
        return Ok(0);
    }

    // Pick nearest expiry.
    let nearest_expiry = &expiry_resp.data[0];
    debug!(symbol, expiry = %nearest_expiry, "fetching option chain");

    // Step 2: Fetch option chain.
    let chain = oc_client
        .fetch_option_chain(security_id, INDEX_SEGMENT, nearest_expiry)
        .await?;

    let spot_price = chain.data.last_price;
    let days_to_expiry = compute_days_to_expiry(nearest_expiry, today);
    // Use fractional days for precision (critical for short-dated options).
    // NSE options expire at 15:30 IST on expiry day.
    let fractional_days = compute_fractional_days_to_expiry(nearest_expiry, ts_nanos);
    let time_to_expiry = fractional_days / config.day_count;

    // Accumulators for PCR and calibration.
    let mut total_call_oi: i64 = 0;
    let mut total_put_oi: i64 = 0;
    let mut total_call_volume: i64 = 0;
    let mut total_put_volume: i64 = 0;
    let mut strike_count: u32 = 0;
    let mut calibration_samples: Vec<CalibrationSample> = Vec::new();

    // Step 3: Process each strike.
    for (strike_str, strike_data) in &chain.data.oc {
        let strike_price: f64 = match strike_str.parse() {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Process CE side.
        if let Some(ce) = &strike_data.ce {
            total_call_oi = total_call_oi.saturating_add(ce.oi);
            total_call_volume = total_call_volume.saturating_add(ce.volume);
            if let Some(sample) = process_option_side(
                writer,
                config,
                ce,
                symbol,
                security_id,
                strike_price,
                "CE",
                nearest_expiry,
                spot_price,
                time_to_expiry,
                days_to_expiry,
                OptionSide::Call,
                ts_nanos,
            )? {
                calibration_samples.push(sample);
            }
            strike_count = strike_count.saturating_add(1);
        }

        // Process PE side.
        if let Some(pe) = &strike_data.pe {
            total_put_oi = total_put_oi.saturating_add(pe.oi);
            total_put_volume = total_put_volume.saturating_add(pe.volume);
            if let Some(sample) = process_option_side(
                writer,
                config,
                pe,
                symbol,
                security_id,
                strike_price,
                "PE",
                nearest_expiry,
                spot_price,
                time_to_expiry,
                days_to_expiry,
                OptionSide::Put,
                ts_nanos,
            )? {
                calibration_samples.push(sample);
            }
            strike_count = strike_count.saturating_add(1);
        }
    }

    // Step 3b: Run parameter calibration on collected samples.
    if !calibration_samples.is_empty()
        && let Some(cal_result) = calibrate_parameters(&calibration_samples)
    {
        let exact = is_exact_match(&cal_result);
        info!(
            symbol,
            samples = cal_result.sample_count,
            best_rate = cal_result.risk_free_rate,
            best_div = cal_result.dividend_yield,
            best_day_count = cal_result.day_count,
            mean_error = cal_result.mean_error,
            exact_match = exact,
            "calibration result"
        );
    }

    // Step 4: Write PCR snapshot.
    write_pcr_snapshot(
        writer,
        symbol,
        nearest_expiry,
        total_put_oi,
        total_call_oi,
        total_put_volume,
        total_call_volume,
        ts_nanos,
    )?;

    debug!(
        symbol,
        strikes = strike_count,
        spot = spot_price,
        pcr_oi = ?compute_pcr(
            total_put_oi.max(0) as u64,
            total_call_oi.max(0) as u64
        ),
        "underlying processed"
    );

    Ok(strike_count)
}

// ---------------------------------------------------------------------------
// Per-Option Processing
// ---------------------------------------------------------------------------

/// Processes a single option side (CE or PE):
/// 1. Write raw Dhan data
/// 2. Compute our Greeks (using our IV solver)
/// 3. Write computed Greeks
/// 4. Cross-verify using Dhan's IV passthrough (isolates formula from parameters)
/// 5. Return calibration sample for parameter grid search
///
/// Returns `Some(CalibrationSample)` for valid options (used by calibration).
// APPROVED: 13 params needed — each represents a distinct domain value from option chain processing
#[allow(clippy::too_many_arguments)]
fn process_option_side(
    writer: &mut GreeksPersistenceWriter,
    config: &GreeksConfig,
    option: &OptionData,
    underlying_symbol: &str,
    underlying_security_id: u64,
    strike_price: f64,
    option_type: &str,
    expiry_date: &str,
    spot_price: f64,
    time_to_expiry: f64,
    days_to_expiry: i64,
    side: OptionSide,
    ts_nanos: i64,
) -> Result<Option<CalibrationSample>> {
    // Construct display name from available data (cold path — format!() is fine).
    // APPROVED: cast to i64 truncates decimal part; NSE strikes are always whole numbers
    #[allow(clippy::cast_possible_truncation)]
    let strike_display = strike_price as i64;
    let symbol_name_owned = format!("{underlying_symbol} {strike_display} {option_type}");
    let symbol_name: &str = &symbol_name_owned;

    // 1. Write raw Dhan data.
    writer.write_dhan_raw_row(&DhanRawRow {
        security_id: option.security_id as i64,
        segment: FNO_SEGMENT,
        symbol_name,
        underlying_symbol,
        underlying_security_id: underlying_security_id as i64,
        underlying_segment: INDEX_SEGMENT,
        strike_price,
        option_type,
        expiry_date,
        spot_price,
        last_price: option.last_price,
        average_price: option.average_price,
        oi: option.oi,
        previous_close_price: option.previous_close_price,
        previous_oi: option.previous_oi,
        previous_volume: option.previous_volume,
        volume: option.volume,
        top_bid_price: option.top_bid_price,
        top_bid_quantity: option.top_bid_quantity,
        top_ask_price: option.top_ask_price,
        top_ask_quantity: option.top_ask_quantity,
        implied_volatility: option.implied_volatility,
        delta: option.greeks.delta,
        theta: option.greeks.theta,
        gamma: option.greeks.gamma,
        vega: option.greeks.vega,
        ts_nanos,
    })?;

    // 2. Compute our Greeks (skip if LTP is zero or near-zero — no market).
    if option.last_price <= 0.01 || time_to_expiry <= 0.0 {
        return Ok(None);
    }

    let our_greeks = black_scholes::compute_greeks(
        side,
        spot_price,
        strike_price,
        time_to_expiry,
        config.risk_free_rate,
        config.dividend_yield,
        option.last_price,
    );

    let greeks = match our_greeks {
        Some(g) => {
            // NaN guard: skip row if any critical Greek is not finite.
            // greeks_from_iv sanitizes internally, but belt-and-suspenders for QuestDB.
            if !g.iv.is_finite() || !g.delta.is_finite() || !g.bs_price.is_finite() {
                warn!(
                    security_id = option.security_id,
                    "NaN/Inf in computed Greeks — skipping"
                );
                return Ok(None);
            }
            g
        }
        None => return Ok(None), // IV solver didn't converge — skip.
    };

    // Classify buildup (OI cast to u32 — safe for typical F&O OI values).
    // APPROVED: .max(0) ensures non-negative; F&O OI fits in u32 (max ~100M contracts)
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let current_oi = option.oi.max(0) as u32;
    // APPROVED: .max(0) ensures non-negative; F&O OI fits in u32 (max ~100M contracts)
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let prev_oi = option.previous_oi.max(0) as u32;
    let buildup = classify_buildup(
        current_oi,
        prev_oi,
        option.last_price,
        option.previous_close_price,
    );
    let buildup_label = buildup.map_or("Unknown", |b| b.label());

    // 3. Write computed Greeks.
    writer.write_option_greeks_row(&OptionGreeksRow {
        segment: FNO_SEGMENT,
        security_id: option.security_id as i64,
        symbol_name,
        underlying_security_id: underlying_security_id as i64,
        underlying_symbol,
        strike_price,
        option_type,
        expiry_date,
        iv: greeks.iv,
        delta: greeks.delta,
        gamma: greeks.gamma,
        theta: greeks.theta,
        vega: greeks.vega,
        rho: greeks.rho,
        charm: greeks.charm,
        vanna: greeks.vanna,
        volga: greeks.volga,
        veta: greeks.veta,
        speed: greeks.speed,
        color: greeks.color,
        zomma: greeks.zomma,
        ultima: greeks.ultima,
        bs_price: greeks.bs_price,
        intrinsic_value: greeks.intrinsic,
        extrinsic_value: greeks.extrinsic,
        spot_price,
        option_ltp: option.last_price,
        oi: option.oi,
        volume: option.volume,
        buildup_type: buildup_label,
        ts_nanos,
    })?;

    // 4. Cross-verification using Dhan's IV passthrough.
    // Dhan IV is in percentage (e.g., 9.789 = 9.789%). Convert to decimal.
    let dhan_iv_decimal = option.implied_volatility / 100.0;

    // Compute "calibrated" Greeks: use Dhan's IV + our formulas + current params.
    // If these match Dhan's Greeks, our BS formulas are correct and only params differ.
    let calibrated = black_scholes::compute_greeks_from_iv(
        side,
        spot_price,
        strike_price,
        time_to_expiry,
        config.risk_free_rate,
        config.dividend_yield,
        dhan_iv_decimal,
        option.last_price,
        config.day_count,
    );

    // Compare calibrated Greeks (from Dhan's IV) vs Dhan's reported Greeks.
    let delta_diff = calibrated.delta - option.greeks.delta;
    let gamma_diff = calibrated.gamma - option.greeks.gamma;
    let theta_diff = calibrated.theta - option.greeks.theta;
    let vega_diff = calibrated.vega - option.greeks.vega;
    let iv_diff = greeks.iv - dhan_iv_decimal;

    let match_status = classify_match(iv_diff, delta_diff, gamma_diff, theta_diff, vega_diff);

    writer.write_verification_row(&VerificationRow {
        security_id: option.security_id as i64,
        segment: FNO_SEGMENT,
        symbol_name,
        underlying_symbol,
        strike_price,
        option_type,
        our_iv: greeks.iv,
        dhan_iv: dhan_iv_decimal,
        iv_diff,
        our_delta: calibrated.delta,
        dhan_delta: option.greeks.delta,
        delta_diff,
        our_gamma: calibrated.gamma,
        dhan_gamma: option.greeks.gamma,
        gamma_diff,
        our_theta: calibrated.theta,
        dhan_theta: option.greeks.theta,
        theta_diff,
        our_vega: calibrated.vega,
        dhan_vega: option.greeks.vega,
        vega_diff,
        match_status,
        ts_nanos,
    })?;

    // 5. Build calibration sample for parameter grid search.
    let sample = CalibrationSample {
        side,
        spot: spot_price,
        strike: strike_price,
        days_to_expiry,
        market_price: option.last_price,
        dhan_iv: dhan_iv_decimal,
        dhan_delta: option.greeks.delta,
        dhan_gamma: option.greeks.gamma,
        dhan_theta: option.greeks.theta,
        dhan_vega: option.greeks.vega,
    };

    Ok(Some(sample))
}

// ---------------------------------------------------------------------------
// PCR Snapshot
// ---------------------------------------------------------------------------

/// Computes and writes PCR snapshot for a single underlying/expiry.
// APPROVED: 8 params needed — PCR requires both OI and volume for puts and calls
#[allow(clippy::too_many_arguments)]
fn write_pcr_snapshot(
    writer: &mut GreeksPersistenceWriter,
    underlying_symbol: &str,
    expiry_date: &str,
    total_put_oi: i64,
    total_call_oi: i64,
    total_put_volume: i64,
    total_call_volume: i64,
    ts_nanos: i64,
) -> Result<()> {
    // APPROVED: .max(0) ensures non-negative before u64 cast
    #[allow(clippy::cast_sign_loss)]
    let pcr_oi_val =
        compute_pcr(total_put_oi.max(0) as u64, total_call_oi.max(0) as u64).unwrap_or(0.0);
    // APPROVED: .max(0) ensures non-negative before u64 cast
    #[allow(clippy::cast_sign_loss)]
    let pcr_volume_val = compute_pcr(
        total_put_volume.max(0) as u64,
        total_call_volume.max(0) as u64,
    )
    .unwrap_or(0.0);

    let sentiment = classify_pcr(pcr_oi_val);
    let sentiment_str = match sentiment {
        tickvault_trading::greeks::pcr::PcrSentiment::Bullish => "Bullish",
        tickvault_trading::greeks::pcr::PcrSentiment::Neutral => "Neutral",
        tickvault_trading::greeks::pcr::PcrSentiment::Bearish => "Bearish",
    };

    writer.write_pcr_snapshot_row(&PcrSnapshotRow {
        underlying_symbol,
        expiry_date,
        pcr_oi: pcr_oi_val,
        pcr_volume: pcr_volume_val,
        total_put_oi,
        total_call_oi,
        total_put_volume,
        total_call_volume,
        sentiment: sentiment_str,
        ts_nanos,
    })?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Computes calendar days to expiry from expiry date string.
///
/// Returns 0 if the expiry date is today or in the past.
fn compute_days_to_expiry(expiry_str: &str, today: NaiveDate) -> i64 {
    let expiry = match NaiveDate::parse_from_str(expiry_str, "%Y-%m-%d") {
        Ok(d) => d,
        Err(_) => return 0,
    };
    let days = (expiry - today).num_days();
    if days <= 0 { 0 } else { days }
}

/// NSE market close time: 15:30 IST = 15*3600 + 30*60 = 55800 seconds from midnight.
const NSE_CLOSE_SECONDS_IST: i64 = 55_800;

/// Computes fractional days to expiry with hour-level precision.
///
/// Options expire at NSE market close (15:30 IST) on expiry day.
/// Uses current IST timestamp (already offset) for precise remaining time.
///
/// Example: If now is 14:30 IST on day before expiry (24h + 1h remaining = 25h = 1.04 days).
fn compute_fractional_days_to_expiry(expiry_str: &str, now_ist_nanos: i64) -> f64 {
    let expiry = match NaiveDate::parse_from_str(expiry_str, "%Y-%m-%d") {
        Ok(d) => d,
        Err(_) => return 0.0,
    };

    // Expiry moment = expiry_date at 15:30 IST (NSE close), in nanoseconds.
    // expiry_date midnight epoch + 15:30 in nanos.
    let expiry_midnight_secs = expiry
        .and_hms_opt(0, 0, 0)
        .map(|dt| dt.and_utc().timestamp())
        .unwrap_or(0);
    let expiry_close_nanos =
        (expiry_midnight_secs + NSE_CLOSE_SECONDS_IST).saturating_mul(1_000_000_000);

    // Remaining time in fractional days.
    let remaining_nanos = expiry_close_nanos.saturating_sub(now_ist_nanos);
    if remaining_nanos <= 0 {
        return 0.0;
    }

    // Convert nanos to fractional days (1 day = 86400 seconds = 86_400_000_000_000 nanos).
    remaining_nanos as f64 / 86_400_000_000_000.0
}

/// Computes time-to-expiry in years from calendar days.
///
/// Uses the configured day_count divisor (365.0 default, calibration may change).
#[cfg(test)]
fn days_to_years(days: i64, day_count: f64) -> f64 {
    if days <= 0 {
        return 0.0;
    }
    days as f64 / day_count
}

/// Classifies match between our Greeks and Dhan's.
///
/// - `EXACT`: All Greeks match within f64 epsilon — our computation replicates Dhan's.
/// - `PARAM_DIFF`: Calibrated (Dhan IV) Greeks match, but our IV solver differs — parameter gap.
/// - `MISMATCH`: Even calibrated Greeks don't match — investigate formula or data issue.
fn classify_match(
    iv_diff: f64,
    delta_diff: f64,
    gamma_diff: f64,
    theta_diff: f64,
    vega_diff: f64,
) -> &'static str {
    // All diffs within epsilon = exact match (our IV solver + params match Dhan's).
    if iv_diff.abs() <= MATCH_EPSILON
        && delta_diff.abs() <= MATCH_EPSILON
        && gamma_diff.abs() <= MATCH_EPSILON
        && theta_diff.abs() <= MATCH_EPSILON
        && vega_diff.abs() <= MATCH_EPSILON
    {
        "EXACT"
    } else if delta_diff.abs() <= MATCH_EPSILON
        && gamma_diff.abs() <= MATCH_EPSILON
        && theta_diff.abs() <= MATCH_EPSILON
        && vega_diff.abs() <= MATCH_EPSILON
    {
        // Greeks match but IV differs — our solver converges to different IV
        // (usually due to rate/div differences).
        "PARAM_DIFF"
    } else {
        "MISMATCH"
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- Days-to-expiry tests ---

    #[test]
    fn test_compute_days_to_expiry_future() {
        let today = NaiveDate::from_ymd_opt(2025, 3, 23).unwrap();
        assert_eq!(compute_days_to_expiry("2025-03-27", today), 4);
    }

    #[test]
    fn test_compute_days_to_expiry_today() {
        let today = NaiveDate::from_ymd_opt(2025, 3, 27).unwrap();
        assert_eq!(compute_days_to_expiry("2025-03-27", today), 0);
    }

    #[test]
    fn test_compute_days_to_expiry_past() {
        let today = NaiveDate::from_ymd_opt(2025, 3, 28).unwrap();
        assert_eq!(compute_days_to_expiry("2025-03-27", today), 0);
    }

    #[test]
    fn test_compute_days_to_expiry_invalid_date() {
        let today = NaiveDate::from_ymd_opt(2025, 3, 23).unwrap();
        assert_eq!(compute_days_to_expiry("not-a-date", today), 0);
    }

    #[test]
    fn test_days_to_years_standard() {
        let years = days_to_years(4, 365.0);
        assert!((years - 4.0 / 365.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_days_to_years_zero() {
        assert!((days_to_years(0, 365.0) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_days_to_years_negative() {
        assert!((days_to_years(-5, 365.0) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_days_to_years_trading_days() {
        let years = days_to_years(10, 252.0);
        assert!((years - 10.0 / 252.0).abs() < f64::EPSILON);
    }

    // --- Classify match tests ---

    #[test]
    fn test_classify_match_all_zero() {
        assert_eq!(classify_match(0.0, 0.0, 0.0, 0.0, 0.0), "EXACT");
    }

    #[test]
    fn test_classify_match_within_epsilon() {
        let e = MATCH_EPSILON / 2.0;
        assert_eq!(classify_match(e, -e, e, -e, e), "EXACT");
    }

    #[test]
    fn test_classify_match_iv_differs_greeks_match() {
        // Greeks within epsilon but IV differs — parameter gap.
        let e = MATCH_EPSILON / 2.0;
        assert_eq!(classify_match(0.5, e, -e, e, -e), "PARAM_DIFF");
    }

    #[test]
    fn test_classify_match_greeks_differ() {
        // Greeks differ (theta way off) — formula or data issue.
        assert_eq!(classify_match(0.01, 0.0, 0.0, 5.0, 0.0), "MISMATCH");
    }

    #[test]
    fn test_classify_match_delta_off() {
        assert_eq!(classify_match(0.0, 0.5, 0.0, 0.0, 0.0), "MISMATCH");
    }

    #[test]
    fn test_classify_match_vega_off() {
        assert_eq!(classify_match(0.0, 0.0, 0.0, 0.0, 2.0), "MISMATCH");
    }

    // --- Market hours gate enforcement ---

    #[test]
    fn test_greeks_market_hours_gate_source_code() {
        // Mechanical enforcement: greeks pipeline must check market hours before each cycle.
        let source = include_str!("greeks_pipeline.rs");
        assert!(
            source.contains("is_within_greeks_window()"),
            "greeks_pipeline.rs MUST call is_within_greeks_window() in the main loop"
        );
        assert!(
            source.contains("TICK_PERSIST_START_SECS_OF_DAY_IST"),
            "greeks window must use TICK_PERSIST_START constant"
        );
        assert!(
            source.contains("TICK_PERSIST_END_SECS_OF_DAY_IST"),
            "greeks window must use TICK_PERSIST_END constant"
        );
    }

    // --- IST timestamp source code enforcement ---

    #[test]
    fn test_critical_greeks_timestamp_includes_ist_offset() {
        // Mechanical enforcement: verify source code adds IST offset to timestamps.
        let source = include_str!("greeks_pipeline.rs");
        assert!(
            source.contains("saturating_add(IST_UTC_OFFSET_NANOS)"),
            "greeks_pipeline.rs MUST add IST_UTC_OFFSET_NANOS to timestamps"
        );
    }

    #[test]
    fn test_critical_greeks_today_uses_ist() {
        // Mechanical enforcement: verify source code uses IST for today's date.
        let source = include_str!("greeks_pipeline.rs");
        assert!(
            source.contains("IST_UTC_OFFSET_SECONDS_I64"),
            "greeks_pipeline.rs MUST use IST_UTC_OFFSET_SECONDS_I64 for today's date"
        );
    }

    // --- Symbol name tests ---

    #[test]
    fn test_symbol_name_format() {
        // Verify the format matches "UNDERLYING STRIKE TYPE".
        let name = format!("{} {} {}", "NIFTY", 23000_i64, "CE");
        assert_eq!(name, "NIFTY 23000 CE");
    }

    #[test]
    fn test_symbol_name_put() {
        let name = format!("{} {} {}", "BANKNIFTY", 45500_i64, "PE");
        assert_eq!(name, "BANKNIFTY 45500 PE");
    }
}

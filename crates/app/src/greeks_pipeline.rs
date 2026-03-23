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
use chrono::{NaiveDate, Utc};
use tracing::{debug, error, info, warn};

use dhan_live_trader_common::config::{GreeksConfig, QuestDbConfig};
use dhan_live_trader_common::constants::VALIDATION_MUST_EXIST_INDICES;
use dhan_live_trader_core::auth::TokenHandle;
use dhan_live_trader_core::option_chain::client::OptionChainClient;
use dhan_live_trader_core::option_chain::types::OptionData;
use dhan_live_trader_storage::greeks_persistence::{
    DhanRawRow, GreeksPersistenceWriter, OptionGreeksRow, PcrSnapshotRow, VerificationRow,
};
use dhan_live_trader_trading::greeks::black_scholes::{self, OptionSide};
use dhan_live_trader_trading::greeks::buildup::classify_buildup;
use dhan_live_trader_trading::greeks::pcr::{classify_pcr, compute_pcr};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// IV difference threshold for "MATCH" classification (absolute).
const IV_MATCH_THRESHOLD: f64 = 0.05;

/// Delta/Gamma/Theta/Vega difference threshold for "MATCH" (absolute).
const GREEK_MATCH_THRESHOLD: f64 = 0.1;

/// Underlyings to fetch option chain for.
/// Uses first 4 from VALIDATION_MUST_EXIST_INDICES (NIFTY, BANKNIFTY, FINNIFTY, MIDCPNIFTY).
const MAX_UNDERLYINGS: usize = 4;

/// Exchange segment for index underlyings (option chain API).
const INDEX_SEGMENT: &str = "IDX_I";

/// Segment string for F&O contracts in QuestDB.
const FNO_SEGMENT: &str = "NSE_FNO";

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

    loop {
        // Run one cycle.
        if let Err(err) = run_one_cycle(&mut oc_client, &mut writer, &greeks_config).await {
            warn!(
                ?err,
                "greeks pipeline cycle failed — will retry next interval"
            );
        }

        tokio::time::sleep(interval).await;
    }
}

// ---------------------------------------------------------------------------
// Single Cycle
// ---------------------------------------------------------------------------

/// Runs one fetch-compute-persist cycle for all configured underlyings.
async fn run_one_cycle(
    oc_client: &mut OptionChainClient,
    writer: &mut GreeksPersistenceWriter,
    config: &GreeksConfig,
) -> Result<()> {
    let now_nanos = Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let today = Utc::now().date_naive();

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

    Ok(())
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
    let time_to_expiry = compute_time_to_expiry(nearest_expiry, today);

    // Accumulators for PCR.
    let mut total_call_oi: i64 = 0;
    let mut total_put_oi: i64 = 0;
    let mut total_call_volume: i64 = 0;
    let mut total_put_volume: i64 = 0;
    let mut strike_count: u32 = 0;

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
            process_option_side(
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
                OptionSide::Call,
                ts_nanos,
            )?;
            strike_count = strike_count.saturating_add(1);
        }

        // Process PE side.
        if let Some(pe) = &strike_data.pe {
            total_put_oi = total_put_oi.saturating_add(pe.oi);
            total_put_volume = total_put_volume.saturating_add(pe.volume);
            process_option_side(
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
                OptionSide::Put,
                ts_nanos,
            )?;
            strike_count = strike_count.saturating_add(1);
        }
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
/// 2. Compute our Greeks
/// 3. Write computed Greeks
/// 4. Write cross-verification
// APPROVED: 12 params needed — each represents a distinct domain value from option chain processing
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
    side: OptionSide,
    ts_nanos: i64,
) -> Result<()> {
    let symbol_name = ""; // Not available from option chain API response

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
        return Ok(());
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
        Some(g) => g,
        None => return Ok(()), // IV solver didn't converge — skip.
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

    // 4. Cross-verification.
    // Dhan IV is in percentage (e.g., 9.789 = 9.789%). Our IV is decimal (0.098).
    let dhan_iv_decimal = option.implied_volatility / 100.0;
    let iv_diff = greeks.iv - dhan_iv_decimal;
    let delta_diff = greeks.delta - option.greeks.delta;
    let gamma_diff = greeks.gamma - option.greeks.gamma;
    let theta_diff = greeks.theta - option.greeks.theta;
    let vega_diff = greeks.vega - option.greeks.vega;

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
        our_delta: greeks.delta,
        dhan_delta: option.greeks.delta,
        delta_diff,
        our_gamma: greeks.gamma,
        dhan_gamma: option.greeks.gamma,
        gamma_diff,
        our_theta: greeks.theta,
        dhan_theta: option.greeks.theta,
        theta_diff,
        our_vega: greeks.vega,
        dhan_vega: option.greeks.vega,
        vega_diff,
        match_status,
        ts_nanos,
    })?;

    Ok(())
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
        dhan_live_trader_trading::greeks::pcr::PcrSentiment::Bullish => "Bullish",
        dhan_live_trader_trading::greeks::pcr::PcrSentiment::Neutral => "Neutral",
        dhan_live_trader_trading::greeks::pcr::PcrSentiment::Bearish => "Bearish",
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

/// Computes time-to-expiry in years from expiry date string.
///
/// Returns 0.0 if the expiry date is today or in the past.
fn compute_time_to_expiry(expiry_str: &str, today: NaiveDate) -> f64 {
    let expiry = match NaiveDate::parse_from_str(expiry_str, "%Y-%m-%d") {
        Ok(d) => d,
        Err(_) => return 0.0,
    };
    let days = (expiry - today).num_days();
    if days <= 0 {
        return 0.0;
    }
    days as f64 / 365.25
}

/// Classifies the overall match between our Greeks and Dhan's.
fn classify_match(
    iv_diff: f64,
    delta_diff: f64,
    gamma_diff: f64,
    theta_diff: f64,
    vega_diff: f64,
) -> &'static str {
    if iv_diff.abs() <= IV_MATCH_THRESHOLD
        && delta_diff.abs() <= GREEK_MATCH_THRESHOLD
        && gamma_diff.abs() <= GREEK_MATCH_THRESHOLD
        && theta_diff.abs() <= GREEK_MATCH_THRESHOLD
        && vega_diff.abs() <= GREEK_MATCH_THRESHOLD
    {
        "MATCH"
    } else if iv_diff.abs() <= IV_MATCH_THRESHOLD {
        "PARTIAL"
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

    #[test]
    fn test_compute_time_to_expiry_future() {
        let today = NaiveDate::from_ymd_opt(2025, 3, 23).unwrap();
        let tte = compute_time_to_expiry("2025-03-27", today);
        // 4 days / 365.25
        assert!((tte - 4.0 / 365.25).abs() < 0.001);
    }

    #[test]
    fn test_compute_time_to_expiry_today() {
        let today = NaiveDate::from_ymd_opt(2025, 3, 27).unwrap();
        let tte = compute_time_to_expiry("2025-03-27", today);
        assert!((tte - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_compute_time_to_expiry_past() {
        let today = NaiveDate::from_ymd_opt(2025, 3, 28).unwrap();
        let tte = compute_time_to_expiry("2025-03-27", today);
        assert!((tte - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_compute_time_to_expiry_invalid_date() {
        let today = NaiveDate::from_ymd_opt(2025, 3, 23).unwrap();
        let tte = compute_time_to_expiry("not-a-date", today);
        assert!((tte - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_classify_match_exact() {
        assert_eq!(classify_match(0.0, 0.0, 0.0, 0.0, 0.0), "MATCH");
    }

    #[test]
    fn test_classify_match_within_threshold() {
        assert_eq!(classify_match(0.04, 0.05, 0.01, -0.05, 0.08), "MATCH");
    }

    #[test]
    fn test_classify_match_partial() {
        // IV matches but delta is off.
        assert_eq!(classify_match(0.01, 0.5, 0.0, 0.0, 0.0), "PARTIAL");
    }

    #[test]
    fn test_classify_match_mismatch() {
        // IV is way off.
        assert_eq!(classify_match(0.2, 0.0, 0.0, 0.0, 0.0), "MISMATCH");
    }
}

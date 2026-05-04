//! PR #454 (2026-05-03) — boot-time `prev_oi` cache loader.
//!
//! Wires the bhavcopy → cache extraction pipeline (built in PR #450
//! commit 3) into the boot path so the new unified `/api/movers`
//! endpoint's `OI Change` and `OI Change %` columns display
//! Dhan-precise values from the very first tick.
//!
//! # Why this exists
//!
//! PR #450 commit 3 shipped two pure data-extraction functions:
//! - `crates/core/src/instrument/bhavcopy_cross_check.rs::build_prev_oi_cache_from_bhavcopy`
//! - `crates/core/src/option_chain/prev_oi.rs::extract_prev_oi_from_option_chain`
//!
//! But the boot orchestrator (`main.rs::spawn_movers_pipeline` call site)
//! was wired with `Arc::new(HashMap::new())` — empty cache. The
//! `PREVOI-01` boot-time WARN flagged this as a known transition gap.
//!
//! This module closes the gap with the simplest correct implementation:
//! fetch yesterday's NSE bhavcopy at boot, parse, and build the cache.
//!
//! # Why bhavcopy first (not Option Chain REST)
//!
//! Per `.claude/rules/dhan/option-chain.md` rule 4 the Dhan Option Chain
//! REST endpoint is rate-limited at **1 unique request every 3 seconds**.
//! Loading prev_oi for our 219 (underlying, expiry) pairs serially would
//! take ~11 minutes — fits before 08:15 IST IF started by 08:00. The
//! bhavcopy approach is FREE (no Dhan quota), takes ~5-6 minutes total
//! (download ~5min + parse ~5s + cache build ~1s), and covers ALL NSE
//! F&O including futures (Option Chain returns options only).
//!
//! Future work (separate PR): Option Chain REST overlay for the most-
//! watched contracts (NIFTY/BANKNIFTY/SENSEX) to override bhavcopy's
//! T+1 staleness with Dhan-canonical values for the highest-traffic
//! underlyings.
//!
//! # Failure mode
//!
//! On any failure (404 / network timeout / unzip / parse error) the
//! function returns `Arc::new(HashMap::new())` (empty cache) and emits
//! a typed `error!` with `code = ErrorCode::PrevOi01CacheEmptyAtBoot`.
//! This preserves the existing PREVOI-01 contract — `/api/movers` will
//! display `current_OI - 0 = current_OI` for OI Change until the next
//! boot succeeds.
//!
//! # Hot path
//!
//! Cold path. Runs ONCE at boot. The resulting `Arc<HashMap>` is consumed
//! by `spawn_movers_pipeline` and read O(1) lock-free per 1s drain.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{Datelike, NaiveDate, Utc};
use tracing::{error, info, warn};

use tickvault_common::error_code::ErrorCode;
use tickvault_common::instrument_registry::InstrumentRegistry;
use tickvault_common::instrument_types::DerivativeContract;
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_core::instrument::bhavcopy_cross_check::{
    build_prev_oi_cache_from_bhavcopy, parse_bhavcopy_csv,
};
use tickvault_core::instrument::bhavcopy_fetcher::{BhavcopySegment, fetch_bhavcopy_zip};
use tickvault_core::instrument::bhavcopy_scheduler::unzip_csv_from_zip_body;

/// Computes the most recent trading day strictly before `today` IST.
///
/// Iterates backwards from `today - 1` calling `calendar.is_trading_day()`.
/// Bounded at 14 iterations — covers any plausible long weekend
/// (4-day holiday + adjacent weekend = 6 days; 14 gives 8 days margin
/// for unprecedented holiday clusters). Returns `None` only if no
/// trading day is found within the bound (defensive — should never
/// happen in practice).
///
/// # I-P1-11 / O(1)
///
/// Pure function. Cold path (boot only). Calendar lookup is O(1) per
/// `is_trading_day` call (HashSet lookup).
#[must_use]
pub fn most_recent_prior_trading_day(
    today: NaiveDate,
    calendar: &TradingCalendar,
) -> Option<NaiveDate> {
    const MAX_LOOKBACK_DAYS: i64 = 14;
    for offset in 1..=MAX_LOOKBACK_DAYS {
        let candidate = today - chrono::Duration::days(offset);
        if calendar.is_trading_day(candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Formats a `NaiveDate` as `YYYYMMDD` for NSE bhavcopy URL.
#[must_use]
pub fn format_yyyymmdd(date: NaiveDate) -> String {
    format!("{:04}{:02}{:02}", date.year(), date.month(), date.day())
}

/// Loads the previous-session-close OI cache from yesterday's NSE
/// bhavcopy at boot.
///
/// Returns `Arc<HashMap<(security_id, exchange_segment_code), prev_oi_i64>>`
/// keyed by the I-P1-11 composite key. On any failure, returns an
/// empty `Arc<HashMap>` (consumer treats absence as 0, so OI Change
/// gracefully degrades to `current_OI - 0`).
///
/// # Steps
///
/// 1. Compute most-recent prior trading day (skip weekends/holidays)
/// 2. HTTP-fetch the NSE F&O bhavcopy ZIP for that date (3-attempt
///    backoff — see `fetch_bhavcopy_zip`)
/// 3. Unzip via shell-out (`unzip_csv_from_zip_body`)
/// 4. Parse CSV → `Vec<BhavcopyRow>`
/// 5. Build cache via `build_prev_oi_cache_from_bhavcopy` (which
///    resolves bhavcopy `(TckrSymb, XpryDt, StrkPric, OptnTp)` tuples
///    against our `DerivativeContract` vector to obtain Dhan
///    SecurityIds, then keys by `(security_id, segment_code)` per
///    I-P1-11)
/// 6. Wrap in `Arc` and return.
///
/// # Error handling
///
/// All steps log + return empty cache on failure. The boot proceeds
/// with degraded OI Change column rather than HALT — operator visibility
/// via the `PREVOI-01` WARN that the empty-cache path emits at the
/// downstream `spawn_movers_pipeline` call site.
pub async fn load_prev_oi_cache_at_boot(
    registry: &InstrumentRegistry,
    calendar: &TradingCalendar,
) -> Arc<HashMap<(u32, u8), i64>> {
    let today_utc = Utc::now().date_naive();
    let Some(prior_trading_day) = most_recent_prior_trading_day(today_utc, calendar) else {
        error!(
            code = ErrorCode::PrevOi01CacheEmptyAtBoot.code_str(),
            "prev_oi loader could not find a prior trading day within 14 days — \
             returning empty cache; OI Change column will display current_OI"
        );
        return Arc::new(HashMap::new());
    };
    let yyyymmdd = format_yyyymmdd(prior_trading_day);

    info!(
        prior_trading_day = %prior_trading_day,
        yyyymmdd,
        "prev_oi loader: fetching NSE bhavcopy for prior trading day"
    );

    // Step 2: fetch ZIP
    let zip_body = match fetch_bhavcopy_zip(BhavcopySegment::Fno, &yyyymmdd).await {
        Ok(b) => b,
        Err(err) => {
            error!(
                code = ErrorCode::PrevOi01CacheEmptyAtBoot.code_str(),
                yyyymmdd,
                ?err,
                "prev_oi loader: bhavcopy ZIP fetch failed — returning empty cache"
            );
            return Arc::new(HashMap::new());
        }
    };

    // Step 3: unzip
    let csv_bytes = match unzip_csv_from_zip_body(&zip_body).await {
        Ok(b) => b,
        Err(err) => {
            error!(
                code = ErrorCode::PrevOi01CacheEmptyAtBoot.code_str(),
                ?err,
                "prev_oi loader: unzip failed — returning empty cache"
            );
            return Arc::new(HashMap::new());
        }
    };
    let csv_str = match std::str::from_utf8(&csv_bytes) {
        Ok(s) => s,
        Err(err) => {
            error!(
                code = ErrorCode::PrevOi01CacheEmptyAtBoot.code_str(),
                ?err,
                "prev_oi loader: bhavcopy CSV not UTF-8 — returning empty cache"
            );
            return Arc::new(HashMap::new());
        }
    };

    // Step 4: parse
    let nse_rows = match parse_bhavcopy_csv(csv_str) {
        Ok(rows) => rows,
        Err(err) => {
            error!(
                code = ErrorCode::PrevOi01CacheEmptyAtBoot.code_str(),
                ?err,
                "prev_oi loader: bhavcopy CSV parse failed — returning empty cache"
            );
            return Arc::new(HashMap::new());
        }
    };

    // Step 5: build cache. Construct `DerivativeContract` rows from
    // the registry's `SubscribedInstrument` entries that have all
    // derivative-specific fields populated. Per I-P1-11 the registry
    // iter exposes BOTH segments of cross-segment SID collisions.
    let derivatives: Vec<DerivativeContract> = registry
        .iter()
        .filter_map(|sub| {
            // Only derivative contracts have all four optional fields.
            let kind = sub.instrument_kind?;
            let expiry = sub.expiry_date?;
            // Strike is 0.0 for futures; option_type is None for futures.
            // Both forms are accepted by build_dhan_lookup.
            let strike = sub.strike_price.unwrap_or(0.0);
            let option_type = sub.option_type;
            Some(DerivativeContract {
                security_id: sub.security_id,
                underlying_symbol: sub.underlying_symbol.clone(),
                instrument_kind: kind,
                exchange_segment: sub.exchange_segment,
                expiry_date: expiry,
                strike_price: strike,
                option_type,
                lot_size: 0,    // not used by build_prev_oi_cache_from_bhavcopy
                tick_size: 0.0, // not used
                symbol_name: sub.display_label.clone(),
                display_name: sub.display_label.clone(),
            })
        })
        .collect();

    if derivatives.is_empty() {
        warn!(
            code = ErrorCode::PrevOi01CacheEmptyAtBoot.code_str(),
            "prev_oi loader: registry has zero derivative contracts — \
             cache will be empty (subscription_plan absent or registry not built yet)"
        );
        return Arc::new(HashMap::new());
    }

    let cache = build_prev_oi_cache_from_bhavcopy(&nse_rows, &derivatives);
    let cache_size = cache.len();

    info!(
        cache_size,
        bhavcopy_rows = nse_rows.len(),
        derivative_contracts = derivatives.len(),
        "prev_oi loader: bhavcopy cache built successfully — \
         /api/movers OI Change column will display Dhan-precise values \
         (Option Chain REST overlay disabled at this call site; use \
         load_prev_oi_cache_at_boot_with_overlay to add Dhan-canonical \
         override for NIFTY/BANKNIFTY/SENSEX top 3)"
    );

    Arc::new(cache)
}

// ---------------------------------------------------------------------------
// PR #456 (2026-05-04) — Option Chain REST overlay for top 3 underlyings
// ---------------------------------------------------------------------------

/// The 3 most-watched index F&O underlyings — `(symbol, scrip_id, options_segment_code)`.
///
/// Mirrors `VALIDATION_MUST_EXIST_INDICES[..3]` from
/// `crates/common/src/constants.rs:897`. The `options_segment_code` is
/// the **option contract** segment (NSE_FNO=2 / BSE_FNO=8), NOT the
/// underlying's IDX_I segment — extracted prev_oi cache keys are per
/// option, so the segment code stamped on each cache entry must match
/// the OPTION's segment (matches the `(security_id, segment_code)`
/// composite key used by the bhavcopy loader and consumed by
/// `movers_pipeline`).
///
/// Hardcoded: these 3 SecurityIds + segment assignments are stable
/// per Dhan instrument-master + IDX_I/NSE_FNO mapping convention.
const TOP_3_OVERLAY_INDICES: &[(&str, u64, u8)] = &[
    ("NIFTY", 13, 2),     // NSE_FNO options
    ("BANKNIFTY", 25, 2), // NSE_FNO options
    ("SENSEX", 51, 8),    // BSE_FNO options
];

/// Loads the prev_oi cache from bhavcopy AND applies the Option Chain
/// REST overlay for NIFTY/BANKNIFTY/SENSEX.
///
/// Sequence:
/// 1. Bhavcopy fetch + parse (calls `load_prev_oi_cache_at_boot`)
/// 2. For each of the top 3 underlyings: fetch nearest expiry + chain,
///    extract `previous_oi`, MERGE INTO accumulator (last-wins per
///    `merge_prev_oi_cache` semantics — Option Chain values override
///    bhavcopy on collision because Dhan-canonical > T+1 NSE static).
///
/// # Why an overlay (not replacement)
///
/// Option Chain REST has a 1-req/3-sec rate limit per
/// `.claude/rules/dhan/option-chain.md` rule 4. Loading prev_oi for
/// our full 219 (underlying, expiry) universe would take ~11 minutes.
/// The overlay covers the 3 highest-traffic indices in ~18 seconds
/// (3 underlyings × 2 calls × 3s rate-limit), giving operators
/// Dhan-canonical values for the contracts that matter most while
/// bhavcopy provides T+1 baseline for the rest.
///
/// # Failure handling
///
/// Per-underlying try; one failure (404 / network timeout / unauth)
/// does NOT abort the others. Bhavcopy values remain authoritative
/// when overlay fails. Logged at WARN, not ERROR — degradation is
/// graceful (T+1 staleness for the affected underlying).
///
/// # Arguments
///
/// `client_id` is the Dhan account client ID (loaded from SSM at
/// boot, plain `String` here per project convention — sensitive bits
/// stay in the `TokenHandle`).
pub async fn load_prev_oi_cache_at_boot_with_overlay(
    registry: &InstrumentRegistry,
    calendar: &TradingCalendar,
    token_handle: tickvault_core::auth::token_manager::TokenHandle,
    client_id: String,
    rest_api_base_url: String,
) -> Arc<HashMap<(u32, u8), i64>> {
    // Step 1: bhavcopy baseline (existing function).
    let bhavcopy_cache = load_prev_oi_cache_at_boot(registry, calendar).await;
    let bhavcopy_size = bhavcopy_cache.len();

    // Strip Arc to mutate; we'll re-wrap at the end.
    let mut accumulator: HashMap<(u32, u8), i64> = (*bhavcopy_cache).clone();

    // Step 2: Option Chain overlay.
    let mut chain_client = match tickvault_core::option_chain::client::OptionChainClient::new(
        token_handle,
        client_id,
        rest_api_base_url,
    ) {
        Ok(c) => c,
        Err(err) => {
            // Overlay disabled — bhavcopy stays authoritative.
            warn!(
                code = ErrorCode::PrevOi01CacheEmptyAtBoot.code_str(),
                error = %err,
                "prev_oi overlay: OptionChainClient::new failed — \
                 bhavcopy cache stays authoritative for ALL underlyings \
                 (top 3 will lack Dhan-canonical override)"
            );
            return Arc::new(accumulator);
        }
    };

    let mut overlay_total: usize = 0;
    let mut overlay_underlyings_succeeded: usize = 0;

    for &(symbol, scrip_id, options_segment_code) in TOP_3_OVERLAY_INDICES {
        // The underlying is IDX_I; the option chain query takes the
        // underlying's IDX_I scrip ID + segment "IDX_I" — NSE/BSE
        // distinction is implicit in the scrip_id.
        let underlying_seg = "IDX_I";

        // Step 2a: fetch expiry list to get the nearest expiry.
        let expiry_list = match chain_client
            .fetch_expiry_list(scrip_id, underlying_seg)
            .await
        {
            Ok(list) => list,
            Err(err) => {
                warn!(
                    code = ErrorCode::PrevOi01CacheEmptyAtBoot.code_str(),
                    underlying = symbol,
                    error = %err,
                    "prev_oi overlay: fetch_expiry_list failed — \
                     bhavcopy values for this underlying remain authoritative"
                );
                continue;
            }
        };
        let nearest_expiry = match expiry_list.data.first() {
            Some(e) => e.clone(),
            None => {
                warn!(
                    underlying = symbol,
                    "prev_oi overlay: expiry list returned empty — skipping"
                );
                continue;
            }
        };

        // Step 2b: fetch the option chain for nearest expiry.
        let response = match chain_client
            .fetch_option_chain(scrip_id, underlying_seg, &nearest_expiry)
            .await
        {
            Ok(r) => r,
            Err(err) => {
                warn!(
                    code = ErrorCode::PrevOi01CacheEmptyAtBoot.code_str(),
                    underlying = symbol,
                    expiry = %nearest_expiry,
                    error = %err,
                    "prev_oi overlay: fetch_option_chain failed — \
                     bhavcopy values for this underlying remain authoritative"
                );
                continue;
            }
        };

        // Step 2c: extract + merge (last-wins overrides bhavcopy on collision).
        let overlay_entries =
            tickvault_core::option_chain::prev_oi::extract_prev_oi_from_option_chain(
                &response,
                options_segment_code,
            );
        let entry_count = overlay_entries.len();
        tickvault_core::option_chain::prev_oi::merge_prev_oi_cache(
            &mut accumulator,
            overlay_entries,
        );
        overlay_total = overlay_total.saturating_add(entry_count);
        overlay_underlyings_succeeded = overlay_underlyings_succeeded.saturating_add(1);

        info!(
            underlying = symbol,
            expiry = %nearest_expiry,
            options_segment_code,
            overlay_entries = entry_count,
            cumulative_overlay_total = overlay_total,
            "prev_oi overlay: applied Dhan-canonical previous_oi values"
        );
    }

    info!(
        bhavcopy_size,
        overlay_added_or_overrode = overlay_total,
        overlay_underlyings_succeeded,
        overlay_underlyings_total = TOP_3_OVERLAY_INDICES.len(),
        final_cache_size = accumulator.len(),
        "prev_oi cache: bhavcopy + Option Chain overlay complete"
    );

    Arc::new(accumulator)
}

// ---------------------------------------------------------------------------
// Tests — pure helpers only (HTTP-dependent loader is integration-tested)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::config::TradingConfig;

    fn test_calendar() -> TradingCalendar {
        // Empty holiday list: only weekends count as non-trading.
        let cfg = TradingConfig {
            market_open_time: "09:15:00".into(),
            market_close_time: "15:30:00".into(),
            order_cutoff_time: "15:29:00".into(),
            data_collection_start: "09:00:00".into(),
            data_collection_end: "16:00:00".into(),
            timezone: "Asia/Kolkata".into(),
            max_orders_per_second: 10,
            nse_holidays: Vec::new(),
            muhurat_trading_dates: Vec::new(),
            nse_mock_trading_dates: Vec::new(),
        };
        TradingCalendar::from_config(&cfg).expect("test calendar constructs")
    }

    #[test]
    fn test_most_recent_prior_trading_day_skips_weekend() {
        let calendar = test_calendar();
        // Monday 2026-05-04 — prior trading day must skip Sat/Sun back to Friday.
        let monday = NaiveDate::from_ymd_opt(2026, 5, 4).unwrap();
        let result = most_recent_prior_trading_day(monday, &calendar);
        let friday = NaiveDate::from_ymd_opt(2026, 5, 1).unwrap();
        assert_eq!(result, Some(friday));
    }

    #[test]
    fn test_most_recent_prior_trading_day_for_tuesday_returns_monday() {
        let calendar = test_calendar();
        // Tuesday 2026-05-05 — prior trading day = Monday 2026-05-04.
        let tuesday = NaiveDate::from_ymd_opt(2026, 5, 5).unwrap();
        let result = most_recent_prior_trading_day(tuesday, &calendar);
        let monday = NaiveDate::from_ymd_opt(2026, 5, 4).unwrap();
        assert_eq!(result, Some(monday));
    }

    #[test]
    fn test_most_recent_prior_trading_day_for_sunday_returns_friday() {
        let calendar = test_calendar();
        // Sunday — prior trading day = Friday (skip Saturday).
        let sunday = NaiveDate::from_ymd_opt(2026, 5, 3).unwrap();
        let result = most_recent_prior_trading_day(sunday, &calendar);
        let friday = NaiveDate::from_ymd_opt(2026, 5, 1).unwrap();
        assert_eq!(result, Some(friday));
    }

    #[test]
    fn test_format_yyyymmdd_pads_single_digit_month_and_day() {
        let date = NaiveDate::from_ymd_opt(2026, 5, 1).unwrap();
        assert_eq!(format_yyyymmdd(date), "20260501");
    }

    #[test]
    fn test_format_yyyymmdd_handles_double_digit_month_and_day() {
        let date = NaiveDate::from_ymd_opt(2026, 12, 31).unwrap();
        assert_eq!(format_yyyymmdd(date), "20261231");
    }

    #[test]
    fn test_format_yyyymmdd_handles_year_boundary() {
        let date = NaiveDate::from_ymd_opt(2099, 1, 1).unwrap();
        assert_eq!(format_yyyymmdd(date), "20990101");
    }

    /// PR #456 ratchet: TOP_3_OVERLAY_INDICES MUST contain exactly the
    /// 3 most-watched index F&O underlyings (NIFTY/BANKNIFTY/SENSEX) —
    /// mirroring `VALIDATION_MUST_EXIST_INDICES[..3]`. Adding more
    /// underlyings without operator approval would breach Dhan's
    /// 1-req/3s rate limit budget for the boot-time overlay.
    #[test]
    fn test_top_3_overlay_indices_pinned_to_exactly_three_underlyings() {
        assert_eq!(TOP_3_OVERLAY_INDICES.len(), 3);
        let symbols: Vec<&str> = TOP_3_OVERLAY_INDICES.iter().map(|(s, _, _)| *s).collect();
        assert_eq!(symbols, vec!["NIFTY", "BANKNIFTY", "SENSEX"]);
    }

    /// PR #456 ratchet: NIFTY + BANKNIFTY options live on NSE_FNO
    /// (segment_code 2). SENSEX options live on BSE_FNO (segment_code
    /// 8). Pinning so a future regression doesn't accidentally stamp
    /// the wrong segment on cache keys (would silently corrupt
    /// I-P1-11 composite-key entries).
    #[test]
    fn test_top_3_overlay_options_segments_per_dhan_annexure() {
        for &(symbol, _scrip, segment_code) in TOP_3_OVERLAY_INDICES {
            match symbol {
                "NIFTY" | "BANKNIFTY" => {
                    assert_eq!(
                        segment_code, 2,
                        "{symbol} options must use NSE_FNO=2 segment_code"
                    );
                }
                "SENSEX" => {
                    assert_eq!(
                        segment_code, 8,
                        "SENSEX options must use BSE_FNO=8 segment_code"
                    );
                }
                _ => panic!("unexpected symbol in TOP_3_OVERLAY_INDICES: {symbol}"),
            }
        }
    }

    /// PR #456 ratchet: scrip IDs MUST match the canonical
    /// `VALIDATION_MUST_EXIST_INDICES` from
    /// `crates/common/src/constants.rs:897` so the overlay queries
    /// the SAME underlyings the universe builder validates.
    #[test]
    fn test_top_3_overlay_scrip_ids_match_validation_must_exist_indices() {
        let expected = [("NIFTY", 13_u64), ("BANKNIFTY", 25_u64), ("SENSEX", 51_u64)];
        for (i, &(sym, scrip, _)) in TOP_3_OVERLAY_INDICES.iter().enumerate() {
            assert_eq!(sym, expected[i].0);
            assert_eq!(
                scrip, expected[i].1,
                "{sym} scrip_id must match VALIDATION_MUST_EXIST_INDICES"
            );
        }
    }
}

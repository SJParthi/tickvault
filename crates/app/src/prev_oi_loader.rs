//! Boot-time `prev_oi` cache loader (Option Chain REST overlay only).
//!
//! Builds the previous-session-close OI cache consumed by the unified
//! `/api/movers` endpoint and the tick enricher's `OI Change` / `OI
//! Change %` columns.
//!
//! # 2026-05-09 simplification — bhavcopy step retired
//!
//! Up to PR #454 / #456 the boot path fetched yesterday's NSE F&O
//! bhavcopy ZIP, shelled out to the macOS `unzip` binary, parsed the
//! CSV, then layered an Option Chain REST overlay for
//! NIFTY/BANKNIFTY/SENSEX on top. Two reasons motivated dropping the
//! bhavcopy step:
//!
//! 1. **macOS Info-ZIP 6.00 (April 2009) is unreliable on stdin pipes.**
//!    Live boot 2026-05-09 20:57:24 IST emitted `PREVOI-01: write to
//!    unzip stdin failed (Broken pipe)`. The shell-out was a recurring
//!    failure mode with no clean fix short of pulling in a Rust ZIP
//!    crate (operator declined: "why zip jsut avoid that").
//! 2. **Wave-5 indices-only scope makes bhavcopy redundant.** The
//!    overlay covers NIFTY/BANKNIFTY/SENSEX in ~18s and that is the
//!    full universe under `[subscription] scope = "indices_only_…"`.
//!    Live evidence from the same boot: `bhavcopy_size=0,
//!    overlay_added_or_overrode=866, final_cache_size=866` —
//!    overlay alone produced complete coverage.
//!
//! The function preserves its public name (`load_prev_oi_cache_at_boot_with_overlay`)
//! and signature shape but no longer touches `bhavcopy_fetcher`,
//! `bhavcopy_scheduler::unzip_csv_from_zip_body`, or
//! `bhavcopy_cross_check::build_prev_oi_cache_from_bhavcopy`. Those
//! still ship for the 16:30 IST `bhavcopy_pipeline.rs` cycle (operator
//! hold).
//!
//! # Failure mode
//!
//! On any per-underlying overlay failure (rate-limit / network / auth)
//! the function logs a WARN and continues with the next underlying. If
//! all 3 fail the returned cache is empty; downstream consumers treat
//! absence as 0 (`current_OI - 0 = current_OI`).
//!
//! # Hot path
//!
//! Cold path. Runs ONCE at boot. The resulting `Arc<HashMap>` is consumed
//! by the unified movers pipeline and read O(1) lock-free per drain.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{Datelike, NaiveDate};
use tracing::{info, warn};

use tickvault_common::error_code::ErrorCode;
use tickvault_common::trading_calendar::TradingCalendar;

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

// ---------------------------------------------------------------------------
// Option Chain REST overlay (sole source post 2026-05-09 simplification)
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

/// Loads the prev_oi cache solely from the Dhan Option Chain REST
/// overlay for NIFTY/BANKNIFTY/SENSEX.
///
/// Sequence:
/// 1. Start with empty accumulator.
/// 2. For each of the top 3 underlyings: fetch nearest expiry + chain,
///    extract `previous_oi`, MERGE INTO accumulator (last-wins per
///    `merge_prev_oi_cache` semantics).
///
/// # Why overlay-only (no bhavcopy)
///
/// Wave-5 indices-only scope subscribes only NIFTY/BANKNIFTY/SENSEX
/// derivatives — exactly the 3 underlyings the overlay covers. The
/// bhavcopy fetch + macOS `unzip` shell-out (retired 2026-05-09 per
/// operator directive "why zip jsut avoid that") added complexity
/// and a recurring `PREVOI-01` failure mode without expanding coverage.
///
/// # Failure handling
///
/// Per-underlying try; one failure (404 / network timeout / unauth)
/// does NOT abort the others. Logged at WARN — degradation is
/// graceful (cache misses fall back to `current_OI - 0`).
///
/// # Arguments
///
/// `client_id` is the Dhan account client ID (loaded from SSM at
/// boot, plain `String` here per project convention — sensitive bits
/// stay in the `TokenHandle`).
pub async fn load_prev_oi_cache_at_boot_with_overlay(
    token_handle: tickvault_core::auth::token_manager::TokenHandle,
    client_id: String,
    rest_api_base_url: String,
) -> Arc<HashMap<(u32, u8), i64>> {
    let mut accumulator: HashMap<(u32, u8), i64> = HashMap::new();

    // Option Chain REST overlay — sole prev_oi source.
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
        overlay_added = overlay_total,
        overlay_underlyings_succeeded,
        overlay_underlyings_total = TOP_3_OVERLAY_INDICES.len(),
        final_cache_size = accumulator.len(),
        "prev_oi cache: Option Chain overlay complete (bhavcopy step retired 2026-05-09)"
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

    /// 2026-05-09 ratchet: the boot-time prev_oi loader MUST NOT
    /// reference the bhavcopy fetch / unzip / parse pipeline. The
    /// macOS Info-ZIP 6.00 stdin-pipe failure mode (`PREVOI-01:
    /// Broken pipe`) is retired by construction — re-introducing
    /// these symbols re-introduces the bug. Operator directive
    /// 2026-05-09: "why zip jsut avoid that".
    ///
    /// Source-scan guard. Reads the module's own source file and
    /// asserts none of the bhavcopy-step symbols appear anywhere
    /// outside the historical doc comment.
    #[test]
    fn test_prev_oi_loader_source_does_not_reference_bhavcopy_pipeline() {
        // `CARGO_MANIFEST_DIR` is the absolute path to the crate root
        // (`crates/app/`); join with the source-relative path of THIS
        // file. Avoids the cwd-vs-workspace-root mismatch between
        // `cargo test -p tickvault-app` (cwd = crate root) and
        // `cargo test --workspace` (cwd = workspace root).
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let path = format!("{manifest_dir}/src/prev_oi_loader.rs");
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("prev_oi_loader.rs readable at {path}: {e}"));
        // Build needles at runtime via fragment concatenation so this
        // test file itself does NOT contain the joined banned-symbol
        // literals (otherwise the scan would trip on its own source).
        let needles: [String; 5] = [
            format!("{}{}{}", "unzip", "_csv_from_zip_body", "("),
            format!("{}{}{}", "fetch", "_bhavcopy_zip", "("),
            format!("{}{}{}", "build_prev_oi_cache", "_from_bhavcopy", "("),
            format!("{}{}{}", "parse", "_bhavcopy_csv", "("),
            format!("{}{}", "Bhavcopy", "Segment::Fno"),
        ];
        for needle in &needles {
            assert!(
                !src.contains(needle),
                "prev_oi_loader.rs MUST NOT reference `{needle}` — \
                 the boot-time bhavcopy step is retired (2026-05-09). \
                 Re-introducing it brings back the macOS unzip stdin-pipe \
                 PREVOI-01 broken-pipe failure mode."
            );
        }
    }
}

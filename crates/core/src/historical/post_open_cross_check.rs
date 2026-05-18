//! Phase 0 Item 15 — 09:16:05 IST post-open cross-check primitives.
//!
//! At 09:16:05 IST every trading day (per
//! `OPEN_PRICE_CROSS_CHECK_TIME_SECS_OF_DAY_IST` = 33_365), this module
//! provides the pure-function comparator + clock helpers + outcome
//! types that the scheduler (sub-deliverable 4) drives.
//!
//! The cross-check compares our `candles_1m_shadow` 09:15 row against
//! Dhan REST `/v2/charts/intraday` 09:15 bar for all 222 SIDs (Phase 0
//! universe: 4 IDX_I + 218 NSE_EQ). Any OHLCV mismatch outside the
//! 1-paisa tolerance triggers a `bar_correction_audit` row and a
//! REPLACE-write to `candles_1m_shadow` + `historical_candles` (mirror)
//! per Items 15+28+29 hostile-agent C1 architecture verdict.
//!
//! **This module is pure logic only — NO I/O.** The scheduler in
//! sub-deliverable 4 wires the REST fetch (via existing
//! `candle_fetcher.rs::fetch_historical_candles`) + audit writes + the
//! strategy gate flip. Keeping the comparator pure means every
//! mismatch class is unit-testable without QuestDB / Dhan / network.

use tickvault_common::constants::{IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY};
use tickvault_common::open_price_source::OPEN_PRICE_CROSS_CHECK_TIME_SECS_OF_DAY_IST;

/// Price tolerance for OHLC comparison. Dhan REST returns 2-decimal
/// prices; our aggregator stores f64 via `f32_to_f64_clean`. Tolerance
/// of 0.01 (1 paisa) accommodates rounding without false-negatives.
///
/// Index values (NIFTY/SENSEX) carry more decimals; the same 0.01
/// tolerance translates to ~0.0001% drift at 25,000 INR — safely below
/// any meaningful strategy-level error.
pub const PRICE_TOLERANCE_RUPEES: f64 = 0.01;

/// Tolerance for integer fields (volume, OI). Dhan REST + our aggregator
/// both store these as exact integers; ANY delta is a real mismatch.
pub const VOLUME_TOLERANCE_UNITS: i64 = 0;

/// One field-level mismatch between our local bar and Dhan's bar.
/// Caller bundles these into the `bar_correction_audit` row.
#[derive(Debug, Clone, PartialEq)]
pub struct BarMismatch {
    /// Numeric SecurityId (composite key part 1 per I-P1-11).
    pub security_id: u32,
    /// Exchange segment byte code (composite key part 2 per I-P1-11).
    /// `IDX_I=0`, `NSE_EQ=1`, `NSE_FNO=2`, etc.
    pub exchange_segment: u8,
    /// Trading symbol for operator-readable Telegram payload.
    /// Allocated string — caller MUST sanitize via
    /// `sanitize_audit_string` before writing to ILP.
    pub trading_symbol: String,
    /// Which OHLCV field disagreed. Stable wire-format label —
    /// `open` / `high` / `low` / `close` / `volume`.
    pub field_label: &'static str,
    /// Our `candles_1m_shadow` value.
    pub local_value: f64,
    /// Dhan REST `/v2/charts/intraday` value.
    pub dhan_value: f64,
}

/// Local snapshot of our `candles_1m_shadow` row for the bar being
/// cross-checked. Caller reads this from QuestDB before invoking
/// `compare_bar`.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalBar {
    pub security_id: u32,
    pub exchange_segment: u8,
    pub trading_symbol: String,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: i64,
}

/// Dhan REST `/v2/charts/intraday` 1m bar response — projection of
/// `HistoricalCandle` to OHLCV-only. Caller extracts this from the
/// columnar parallel-array response.
#[derive(Debug, Clone, PartialEq)]
pub struct DhanBar {
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: i64,
}

/// Pure-function comparator. Returns the FULL list of field-level
/// mismatches (caller decides whether one mismatch = full correction
/// or per-field). Empty Vec = perfect match within tolerance.
///
/// Comparison is field-by-field with `PRICE_TOLERANCE_RUPEES` (0.01)
/// for OHLC and `VOLUME_TOLERANCE_UNITS` (0) for volume.
#[must_use]
pub fn compare_bar(local: &LocalBar, dhan: &DhanBar) -> Vec<BarMismatch> {
    let mut mismatches = Vec::new();
    push_if_price_mismatch(&mut mismatches, local, "open", local.open, dhan.open);
    push_if_price_mismatch(&mut mismatches, local, "high", local.high, dhan.high);
    push_if_price_mismatch(&mut mismatches, local, "low", local.low, dhan.low);
    push_if_price_mismatch(&mut mismatches, local, "close", local.close, dhan.close);
    if (local.volume - dhan.volume).abs() > VOLUME_TOLERANCE_UNITS {
        mismatches.push(BarMismatch {
            security_id: local.security_id,
            exchange_segment: local.exchange_segment,
            trading_symbol: local.trading_symbol.clone(),
            field_label: "volume",
            // APPROVED: integer-to-double cast for audit-row display only;
            // exact integer values stored in separate audit columns.
            local_value: local.volume as f64,
            dhan_value: dhan.volume as f64,
        });
    }
    mismatches
}

#[inline]
fn push_if_price_mismatch(
    mismatches: &mut Vec<BarMismatch>,
    local: &LocalBar,
    field_label: &'static str,
    local_value: f64,
    dhan_value: f64,
) {
    if (local_value - dhan_value).abs() > PRICE_TOLERANCE_RUPEES {
        mismatches.push(BarMismatch {
            security_id: local.security_id,
            exchange_segment: local.exchange_segment,
            trading_symbol: local.trading_symbol.clone(),
            field_label,
            local_value,
            dhan_value,
        });
    }
}

/// Outcome of one cross-check pass across the whole 222-SID universe.
/// Drives the strategy gate decision in sub-deliverable 4:
///
///   * `AllMatch` → flip strategy gate to `true`, allow trading
///   * `Corrected` → strategy gate stays `true` (corrections wrote
///     authoritative Dhan values to shadow + historical_candles)
///   * `InconclusiveLowCoverage` → strategy gate stays `false`; Dhan
///     REST returned data for fewer than the false-OK threshold
///     (per audit-findings Rule 11)
///   * `Failed` → strategy gate stays `false`; Dhan REST hard-error
#[derive(Debug, Clone, PartialEq)]
pub enum CrossCheckOutcome {
    /// All `compared_count` bars matched Dhan within tolerance.
    /// Strategy gate may open.
    AllMatch { compared_count: usize },
    /// `mismatches.len()` bars disagreed and were corrected by writing
    /// Dhan's authoritative values. Strategy gate may open with
    /// operator-visible Telegram summary.
    Corrected {
        compared_count: usize,
        mismatches_count: usize,
    },
    /// Dhan REST returned data for fewer SIDs than the
    /// `MIN_COMPARED_COUNT_FOR_PASS` threshold. Cross-check cannot
    /// vouch for the universe; strategy gate stays CLOSED.
    InconclusiveLowCoverage {
        compared_count: usize,
        expected_count: usize,
    },
    /// Hard failure (network, auth, all 222 fetches errored). Strategy
    /// gate stays CLOSED; operator must inspect.
    Failed { reason: String },
}

/// Minimum compared-SID count below which the cross-check pass is
/// declared inconclusive (audit-findings Rule 11 — no false-OK signals).
///
/// At 90% of the Phase 0 universe (200 of 222 SIDs), we accept up to
/// 22 illiquid stragglers with no Dhan data. Below this threshold,
/// Dhan REST is presumed degraded and the strategy gate stays CLOSED.
pub const MIN_COMPARED_COUNT_FOR_PASS: usize = 200;

/// Pure-function classifier for cross-check outcomes given the raw
/// counts from a completed pass.
#[must_use]
pub fn classify_outcome(
    compared_count: usize,
    expected_count: usize,
    mismatches_count: usize,
    rest_error_count: usize,
) -> CrossCheckOutcome {
    if rest_error_count == expected_count {
        return CrossCheckOutcome::Failed {
            reason: format!("all {expected_count} Dhan REST fetches failed"),
        };
    }
    if compared_count < MIN_COMPARED_COUNT_FOR_PASS {
        return CrossCheckOutcome::InconclusiveLowCoverage {
            compared_count,
            expected_count,
        };
    }
    if mismatches_count == 0 {
        CrossCheckOutcome::AllMatch { compared_count }
    } else {
        CrossCheckOutcome::Corrected {
            compared_count,
            mismatches_count,
        }
    }
}

/// Pure-function clock helper: seconds remaining until the next
/// 09:16:05 IST boundary, given the current IST seconds-of-day.
/// Returns `0` only when `now == 09:16:05` exactly. If past 09:16:05,
/// returns the duration until tomorrow's 09:16:05.
#[must_use]
pub fn seconds_until_post_open_cross_check_ist(now_secs_of_day_ist: u32) -> u32 {
    if now_secs_of_day_ist <= OPEN_PRICE_CROSS_CHECK_TIME_SECS_OF_DAY_IST {
        OPEN_PRICE_CROSS_CHECK_TIME_SECS_OF_DAY_IST - now_secs_of_day_ist
    } else {
        SECONDS_PER_DAY - now_secs_of_day_ist + OPEN_PRICE_CROSS_CHECK_TIME_SECS_OF_DAY_IST
    }
}

/// Convert a `chrono::Utc::now()`-style UTC timestamp into IST
/// seconds-of-day. Lifted out so the scheduler + tests share the
/// same arithmetic.
#[must_use]
pub fn ist_seconds_of_day_from_utc(now_utc_secs: i64) -> u32 {
    let ist = now_utc_secs.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
    // APPROVED: rem_euclid produces a value in [0, 86399]; cast to u32
    // is lossless given the SECONDS_PER_DAY = 86_400 upper bound.
    ist.rem_euclid(i64::from(SECONDS_PER_DAY)) as u32
}

/// Returns `true` if the cross-check should run TODAY at boot time.
/// `false` when the wall clock has already crossed past 09:16:05 IST
/// AND today's catch-up window has elapsed (caller passes the boot
/// catch-up budget — e.g. 1 hour past the boundary).
///
/// At boot exactly at 09:30 IST (14 min late), this returns `true`
/// for the `BootCatchUp` audit outcome; at 11:30 IST (over 2 hours
/// late) returns `false` — operator must use the 15:31 post-market
/// cross-verify to catch up.
#[must_use]
pub fn should_run_boot_catch_up(now_secs_of_day_ist: u32, catch_up_budget_secs: u32) -> bool {
    if now_secs_of_day_ist < OPEN_PRICE_CROSS_CHECK_TIME_SECS_OF_DAY_IST {
        // Not yet 09:16:05 — normal scheduler path, no catch-up needed.
        return false;
    }
    let elapsed = now_secs_of_day_ist.saturating_sub(OPEN_PRICE_CROSS_CHECK_TIME_SECS_OF_DAY_IST);
    elapsed <= catch_up_budget_secs
}

/// Default boot catch-up budget. 1 hour past 09:16:05 IST (i.e. up
/// to 10:16:05 IST) the boot path can still run a catch-up
/// cross-check. Beyond that, defer to the 15:31 post-market run.
pub const BOOT_CATCH_UP_BUDGET_SECS: u32 = 3600;

#[cfg(test)]
mod tests {
    use super::*;

    fn make_local() -> LocalBar {
        LocalBar {
            security_id: 51,
            exchange_segment: 0, // IDX_I
            trading_symbol: "SENSEX".to_string(),
            open: 74807.97,
            high: 74900.50,
            low: 74429.77,
            close: 74429.77,
            volume: 0, // IDX_I has no volume
        }
    }

    fn make_dhan_match() -> DhanBar {
        DhanBar {
            open: 74807.97,
            high: 74900.50,
            low: 74429.77,
            close: 74429.77,
            volume: 0,
        }
    }

    // ---- compare_bar pure-function tests ----

    #[test]
    fn test_compare_bar_perfect_match_returns_empty_vec() {
        let local = make_local();
        let dhan = make_dhan_match();
        let mismatches = compare_bar(&local, &dhan);
        assert!(
            mismatches.is_empty(),
            "perfect match must return empty mismatches"
        );
    }

    #[test]
    fn test_compare_bar_within_price_tolerance_returns_empty_vec() {
        // 0.005 delta < PRICE_TOLERANCE_RUPEES (0.01) — should match.
        let local = make_local();
        let dhan = DhanBar {
            open: 74807.975,
            high: 74900.505,
            low: 74429.775,
            close: 74429.775,
            volume: 0,
        };
        let mismatches = compare_bar(&local, &dhan);
        assert!(
            mismatches.is_empty(),
            "0.005 < tolerance must match: got {mismatches:?}"
        );
    }

    #[test]
    fn test_compare_bar_open_mismatch_detected() {
        // User's actual SENSEX 2026-05-18 bug: our open = 74670.64,
        // Dhan REST open = 74807.97 — 137 rupee delta, well past 0.01.
        let local = LocalBar {
            open: 74670.64,
            ..make_local()
        };
        let dhan = make_dhan_match();
        let mismatches = compare_bar(&local, &dhan);
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].field_label, "open");
        assert!((mismatches[0].local_value - 74670.64).abs() < f64::EPSILON);
        assert!((mismatches[0].dhan_value - 74807.97).abs() < f64::EPSILON);
        assert_eq!(mismatches[0].security_id, 51);
        assert_eq!(mismatches[0].exchange_segment, 0);
        assert_eq!(mismatches[0].trading_symbol, "SENSEX");
    }

    #[test]
    fn test_compare_bar_all_fields_mismatch() {
        let local = make_local();
        let dhan = DhanBar {
            open: local.open + 1.0,
            high: local.high + 1.0,
            low: local.low + 1.0,
            close: local.close + 1.0,
            volume: 100,
        };
        let mismatches = compare_bar(&local, &dhan);
        // 4 price fields + volume = 5 mismatches.
        assert_eq!(mismatches.len(), 5);
        let labels: Vec<&'static str> = mismatches.iter().map(|m| m.field_label).collect();
        assert!(labels.contains(&"open"));
        assert!(labels.contains(&"high"));
        assert!(labels.contains(&"low"));
        assert!(labels.contains(&"close"));
        assert!(labels.contains(&"volume"));
    }

    #[test]
    fn test_compare_bar_volume_zero_tolerance() {
        // Volume off by exactly 1 — must flag (tolerance is 0).
        let local = make_local();
        let dhan = DhanBar {
            volume: 1,
            ..make_dhan_match()
        };
        let mismatches = compare_bar(&local, &dhan);
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].field_label, "volume");
    }

    #[test]
    fn test_compare_bar_carries_composite_key_per_i_p1_11() {
        let local = make_local();
        let dhan = DhanBar {
            open: 74808.50, // mismatch
            ..make_dhan_match()
        };
        let mismatches = compare_bar(&local, &dhan);
        // I-P1-11 ratchet: every mismatch MUST carry
        // (security_id, exchange_segment) composite key.
        assert_eq!(mismatches[0].security_id, 51);
        assert_eq!(mismatches[0].exchange_segment, 0);
    }

    // ---- classify_outcome pure-function tests ----

    #[test]
    fn test_classify_outcome_all_match() {
        let outcome = classify_outcome(222, 222, 0, 0);
        assert_eq!(
            outcome,
            CrossCheckOutcome::AllMatch {
                compared_count: 222
            }
        );
    }

    #[test]
    fn test_classify_outcome_corrected() {
        let outcome = classify_outcome(222, 222, 5, 0);
        assert_eq!(
            outcome,
            CrossCheckOutcome::Corrected {
                compared_count: 222,
                mismatches_count: 5,
            }
        );
    }

    #[test]
    fn test_classify_outcome_low_coverage_below_threshold() {
        // 199 compared < MIN_COMPARED_COUNT_FOR_PASS (200) → inconclusive.
        let outcome = classify_outcome(199, 222, 0, 23);
        assert_eq!(
            outcome,
            CrossCheckOutcome::InconclusiveLowCoverage {
                compared_count: 199,
                expected_count: 222,
            }
        );
    }

    #[test]
    fn test_classify_outcome_at_threshold_passes() {
        // 200 compared == MIN_COMPARED_COUNT_FOR_PASS → just barely passes.
        let outcome = classify_outcome(200, 222, 0, 22);
        assert_eq!(
            outcome,
            CrossCheckOutcome::AllMatch {
                compared_count: 200
            }
        );
    }

    #[test]
    fn test_classify_outcome_failed_when_all_rest_errors() {
        let outcome = classify_outcome(0, 222, 0, 222);
        assert!(matches!(outcome, CrossCheckOutcome::Failed { .. }));
    }

    #[test]
    fn test_classify_outcome_failed_reason_includes_count() {
        let outcome = classify_outcome(0, 222, 0, 222);
        if let CrossCheckOutcome::Failed { reason } = outcome {
            assert!(reason.contains("222"));
        } else {
            panic!("expected Failed");
        }
    }

    // ---- clock helper tests ----

    #[test]
    fn test_seconds_until_post_open_cross_check_ist_at_midnight() {
        // 00:00:00 IST → 09:16:05 IST = 9 * 3600 + 16 * 60 + 5 = 33_365.
        assert_eq!(seconds_until_post_open_cross_check_ist(0), 33_365);
    }

    #[test]
    fn test_seconds_until_post_open_cross_check_ist_at_market_open() {
        // 09:15:00 IST → 09:16:05 IST = 65 seconds.
        let now = 9 * 3600 + 15 * 60;
        assert_eq!(seconds_until_post_open_cross_check_ist(now), 65);
    }

    #[test]
    fn test_seconds_until_post_open_cross_check_ist_exactly_at_boundary() {
        // At exactly 09:16:05 IST → 0 (fire immediately).
        assert_eq!(seconds_until_post_open_cross_check_ist(33_365), 0);
    }

    #[test]
    fn test_seconds_until_post_open_cross_check_ist_after_boundary_rolls() {
        // 15:30:00 IST → next 09:16:05 IST tomorrow.
        // = SECONDS_PER_DAY - 55_800 + 33_365 = 86_400 - 22_435 = 63_965.
        let now = 15 * 3600 + 30 * 60;
        assert_eq!(seconds_until_post_open_cross_check_ist(now), 63_965);
    }

    #[test]
    fn test_ist_seconds_of_day_from_utc_at_utc_midnight() {
        // UTC 00:00 → IST 05:30 = 5*3600 + 30*60 = 19_800.
        assert_eq!(ist_seconds_of_day_from_utc(0), 19_800);
    }

    #[test]
    fn test_ist_seconds_of_day_from_utc_wraps_across_utc_midnight() {
        // UTC 19:30 → IST 01:00 next day = 3_600.
        let utc = 19 * 3600 + 30 * 60;
        assert_eq!(ist_seconds_of_day_from_utc(utc), 3_600);
    }

    // ---- boot catch-up tests ----

    #[test]
    fn test_should_run_boot_catch_up_before_boundary_returns_false() {
        // 09:00:00 IST is before 09:16:05 — normal scheduler path.
        let now = 9 * 3600;
        assert!(!should_run_boot_catch_up(now, BOOT_CATCH_UP_BUDGET_SECS));
    }

    #[test]
    fn test_should_run_boot_catch_up_exactly_at_boundary_returns_true() {
        // At 09:16:05 IST exactly, elapsed = 0, within budget.
        assert!(should_run_boot_catch_up(33_365, BOOT_CATCH_UP_BUDGET_SECS));
    }

    #[test]
    fn test_should_run_boot_catch_up_within_one_hour_returns_true() {
        // 09:30 IST (~14 min past 09:16:05) — within 1h budget.
        let now = 9 * 3600 + 30 * 60;
        assert!(should_run_boot_catch_up(now, BOOT_CATCH_UP_BUDGET_SECS));
    }

    #[test]
    fn test_should_run_boot_catch_up_past_budget_returns_false() {
        // 11:30 IST (~2h 14m past 09:16:05) — beyond 1h budget.
        let now = 11 * 3600 + 30 * 60;
        assert!(!should_run_boot_catch_up(now, BOOT_CATCH_UP_BUDGET_SECS));
    }

    #[test]
    fn test_boot_catch_up_budget_constant() {
        assert_eq!(BOOT_CATCH_UP_BUDGET_SECS, 3600);
    }

    // ---- tolerance constants ----

    #[test]
    fn test_price_tolerance_constant() {
        assert!((PRICE_TOLERANCE_RUPEES - 0.01).abs() < f64::EPSILON);
    }

    #[test]
    fn test_volume_tolerance_constant() {
        assert_eq!(VOLUME_TOLERANCE_UNITS, 0);
    }

    #[test]
    fn test_min_compared_count_for_pass_constant() {
        assert_eq!(MIN_COMPARED_COUNT_FOR_PASS, 200);
    }
}

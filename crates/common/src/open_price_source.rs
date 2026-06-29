//! Phase 0 Item 13 — open-price source-selection primitives (2026-05-13).
//!
//! Per-instrument classification of where the 09:15 1m candle's
//! `open` price should come from. Plan §9 of `topic-PHASE-0-LEAN-LOCKED.md`
//! locks the per-class rules:
//!
//! | Class | Primary | Fallback chain |
//! |---|---|---|
//! | IndexWithPreopenBuffer (NIFTY, BANKNIFTY) | Pre-open buffer last slot | First WS tick + `OPEN-PRICE-WARN` |
//! | IndexNoPreopen (SENSEX-BSE, INDIA VIX) | First WS tick (acceptable) | — |
//! | StockWithPreopen (NSE_EQ F&O) | Pre-open buffer last slot | REST `/v2/marketfeed/quote.day_open` → first WS tick + warn |
//!
//! The bug this fixes: today we treat the FIRST WS tick after 09:15:00
//! as the OPEN price for the 09:15 1m candle. WRONG — NSE's OFFICIAL
//! OPEN is the pre-open call-auction equilibrium price frozen at
//! 09:08:00 IST. The first post-09:15 trade is just the first POST-OPEN
//! trade, not the OPEN. Per-stock drift ~₹2-5 silently shifts gap-up/
//! gap-down logic away from NSE truth.
//!
//! This module is pure logic — IO (REST fetch, WS tick capture,
//! aggregator init) lands in follow-up PRs.

use crate::constants::INDIA_VIX_SECURITY_ID;

/// NIFTY index value `security_id` on the IDX_I segment. Pinned by
/// `subscription_planner.rs::FULL_CHAIN_INDEX_SYMBOLS` (verified at
/// CSV ingest time per `test_india_vix_security_id_constant_pinned_at_21`
/// pattern; this constant mirrors the same idea for NIFTY).
pub const NIFTY_SECURITY_ID: crate::types::SecurityId = 13;

/// BANKNIFTY index value `security_id` on the IDX_I segment.
pub const BANKNIFTY_SECURITY_ID: crate::types::SecurityId = 25;

/// SENSEX index value `security_id` on the IDX_I segment. BSE-derived;
/// our NSE pre-open buffer does NOT capture it, so the fallback is the
/// first WS tick (with a cross-verify flag).
pub const SENSEX_SECURITY_ID: crate::types::SecurityId = 51;

/// Pre-open call-auction equilibrium freeze time per NSE. Captured by
/// the pre-open buffer's last slot (09:08–09:15). Plan §9.
pub const PREOPEN_FREEZE_TIME_SECS_OF_DAY_IST: u32 = 9 * 3600 + 8 * 60;

/// Time at which the aggregator reads the pre-open buffer to set the
/// 09:15 candle's `open`. Just before the seal so we capture the
/// final buffer state. Plan §9.
pub const PREOPEN_BUFFER_READ_TIME_SECS_OF_DAY_IST: u32 = 9 * 3600 + 14 * 60 + 59;

/// Time at which the REST `/v2/marketfeed/quote.day_open` fallback is
/// invoked (for NSE_EQ stocks whose pre-open buffer is empty at this
/// time). Plan §9: 09:14:55 IST gives the REST a few seconds to
/// complete before the 09:15:00 seal.
pub const OPEN_PRICE_REST_FALLBACK_TIME_SECS_OF_DAY_IST: u32 = 9 * 3600 + 14 * 60 + 55;

/// Time at which the 09:15 bar is cross-checked against Dhan's
/// `/v2/charts/intraday` historical response (plan §9). 65 seconds
/// after the seal: 60s bar end + 5s buffer (matches
/// `GAP_FILL_POST_SEAL_BUFFER_SECS`).
pub const OPEN_PRICE_CROSS_CHECK_TIME_SECS_OF_DAY_IST: u32 = 9 * 3600 + 16 * 60 + 5;

/// Per-class classification of an instrument's 09:15-open source rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenSourceClass {
    /// NIFTY (13), BANKNIFTY (25) — IDX_I with NSE-captured pre-open
    /// buffer.
    IndexWithPreopenBuffer,
    /// SENSEX (51) — BSE-derived; INDIA VIX (21) — option-chain-derived.
    /// Neither has an NSE pre-open buffer entry; first WS tick is the
    /// best we can do (cross-verified at 09:16:05 against Dhan REST).
    IndexNoPreopen,
    /// NSE_EQ — any cash-equity F&O underlying. Pre-open buffer is
    /// expected; REST `/marketfeed/quote.day_open` is the secondary
    /// fallback; first WS tick is the last-resort fallback.
    StockWithPreopen,
}

/// Resolved source of the 09:15 candle's `open` price for a single
/// instrument, given the available inputs at seal time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenSource {
    /// Ideal — read from the pre-open buffer's last slot. NSE's
    /// official equilibrium price.
    PreopenBuffer,
    /// Secondary — REST `/v2/marketfeed/quote.day_open` (NSE_EQ only).
    RestQuoteDayOpen,
    /// Last resort — first WebSocket tick after 09:15:00. Triggers
    /// `OPEN-PRICE-WARN` Telegram for IndexWithPreopenBuffer +
    /// StockWithPreopen classes; acceptable (no warn) for
    /// IndexNoPreopen.
    FirstWsTick,
}

/// Phase 0 Item 13 — classify an instrument by its 09:15-open source
/// rules. Pure function, exhaustively defined for the Phase 0
/// universe (4 IDX_I + ~218 NSE_EQ). For unknown SecurityIds on IDX_I
/// (sectoral display indices etc.) the result is `IndexNoPreopen` —
/// those instruments fall back to first WS tick with no warning
/// (acceptable since they're not on the trading hot path).
#[inline]
#[must_use]
pub const fn classify_instrument_for_open_source(
    security_id: crate::types::SecurityId,
    is_idx_i: bool,
) -> OpenSourceClass {
    if is_idx_i {
        if security_id == NIFTY_SECURITY_ID || security_id == BANKNIFTY_SECURITY_ID {
            return OpenSourceClass::IndexWithPreopenBuffer;
        }
        // SENSEX (BSE), INDIA VIX (computed), other display indices —
        // no NSE pre-open buffer entry; first WS tick is canonical.
        // Includes INDIA_VIX_SECURITY_ID and SENSEX_SECURITY_ID by
        // construction. (Mentioned for cross-reference; const-fn match
        // is exhaustive in the `if` above.)
        let _ = INDIA_VIX_SECURITY_ID;
        let _ = SENSEX_SECURITY_ID;
        return OpenSourceClass::IndexNoPreopen;
    }
    // NSE_EQ — the F&O underlying universe.
    OpenSourceClass::StockWithPreopen
}

/// Phase 0 Item 13 — given the instrument's class and what inputs are
/// available at seal time, return the resolved source the aggregator
/// MUST use for the 09:15 candle's `open` field.
///
/// Fallback chain matches plan §9 row table exactly.
#[inline]
#[must_use]
pub const fn select_open_source(
    class: OpenSourceClass,
    has_preopen_buffer_entry: bool,
    has_rest_quote_fallback: bool,
) -> OpenSource {
    match class {
        OpenSourceClass::IndexWithPreopenBuffer => {
            if has_preopen_buffer_entry {
                OpenSource::PreopenBuffer
            } else {
                OpenSource::FirstWsTick
            }
        }
        OpenSourceClass::StockWithPreopen => {
            if has_preopen_buffer_entry {
                OpenSource::PreopenBuffer
            } else if has_rest_quote_fallback {
                OpenSource::RestQuoteDayOpen
            } else {
                OpenSource::FirstWsTick
            }
        }
        OpenSourceClass::IndexNoPreopen => OpenSource::FirstWsTick,
    }
}

/// Phase 0 Item 13 — should the aggregator emit an `OPEN-PRICE-WARN`
/// Telegram when the resolved source is `FirstWsTick`? Per plan §9:
///   * `IndexWithPreopenBuffer` → YES (NSE provides pre-open; falling
///     back to first tick means our buffer missed)
///   * `StockWithPreopen` → YES (same rationale)
///   * `IndexNoPreopen` → NO (acceptable by design — VIX/BSE indices
///     have no NSE pre-open)
#[inline]
#[must_use]
pub const fn should_warn_on_first_ws_tick_fallback(class: OpenSourceClass) -> bool {
    matches!(
        class,
        OpenSourceClass::IndexWithPreopenBuffer | OpenSourceClass::StockWithPreopen
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // SecurityId constant pins
    // -----------------------------------------------------------------------

    #[test]
    fn test_nifty_security_id_pinned_at_13() {
        assert_eq!(NIFTY_SECURITY_ID, 13);
    }

    #[test]
    fn test_banknifty_security_id_pinned_at_25() {
        assert_eq!(BANKNIFTY_SECURITY_ID, 25);
    }

    #[test]
    fn test_sensex_security_id_pinned_at_51() {
        assert_eq!(SENSEX_SECURITY_ID, 51);
    }

    // -----------------------------------------------------------------------
    // Time constants — Plan §9
    // -----------------------------------------------------------------------

    #[test]
    fn test_preopen_freeze_time_at_0908() {
        // 9h * 3600 + 8m * 60 = 32_880 secs-of-day.
        assert_eq!(PREOPEN_FREEZE_TIME_SECS_OF_DAY_IST, 32_880);
    }

    #[test]
    fn test_preopen_buffer_read_time_at_091459() {
        // 9h * 3600 + 14m * 60 + 59s = 33_299.
        assert_eq!(PREOPEN_BUFFER_READ_TIME_SECS_OF_DAY_IST, 33_299);
    }

    #[test]
    fn test_open_price_rest_fallback_time_at_091455() {
        // 9h * 3600 + 14m * 60 + 55s = 33_295.
        assert_eq!(OPEN_PRICE_REST_FALLBACK_TIME_SECS_OF_DAY_IST, 33_295);
    }

    #[test]
    fn test_open_price_cross_check_time_at_091605() {
        // 9h * 3600 + 16m * 60 + 5s = 33_365.
        assert_eq!(OPEN_PRICE_CROSS_CHECK_TIME_SECS_OF_DAY_IST, 33_365);
    }

    #[test]
    fn test_time_ordering_makes_sense() {
        // The four pinned times MUST be strictly ordered.
        // freeze < REST fallback < buffer read < cross-check.
        assert!(
            PREOPEN_FREEZE_TIME_SECS_OF_DAY_IST < OPEN_PRICE_REST_FALLBACK_TIME_SECS_OF_DAY_IST
        );
        assert!(
            OPEN_PRICE_REST_FALLBACK_TIME_SECS_OF_DAY_IST
                < PREOPEN_BUFFER_READ_TIME_SECS_OF_DAY_IST
        );
        assert!(
            PREOPEN_BUFFER_READ_TIME_SECS_OF_DAY_IST < OPEN_PRICE_CROSS_CHECK_TIME_SECS_OF_DAY_IST
        );
    }

    // -----------------------------------------------------------------------
    // Classification — Plan §9 row table
    // -----------------------------------------------------------------------

    #[test]
    fn test_classify_nifty_is_index_with_preopen_buffer() {
        assert_eq!(
            classify_instrument_for_open_source(NIFTY_SECURITY_ID, true),
            OpenSourceClass::IndexWithPreopenBuffer,
        );
    }

    #[test]
    fn test_classify_banknifty_is_index_with_preopen_buffer() {
        assert_eq!(
            classify_instrument_for_open_source(BANKNIFTY_SECURITY_ID, true),
            OpenSourceClass::IndexWithPreopenBuffer,
        );
    }

    #[test]
    fn test_classify_sensex_is_index_no_preopen() {
        // BSE-derived; our NSE pre-open buffer doesn't capture it.
        assert_eq!(
            classify_instrument_for_open_source(SENSEX_SECURITY_ID, true),
            OpenSourceClass::IndexNoPreopen,
        );
    }

    #[test]
    fn test_classify_india_vix_is_index_no_preopen() {
        // VIX is option-chain-derived; no pre-open buffer entry.
        assert_eq!(
            classify_instrument_for_open_source(INDIA_VIX_SECURITY_ID, true),
            OpenSourceClass::IndexNoPreopen,
        );
    }

    #[test]
    fn test_classify_nse_eq_is_stock_with_preopen() {
        // RELIANCE = 2885 (example F&O underlying).
        assert_eq!(
            classify_instrument_for_open_source(2885, false),
            OpenSourceClass::StockWithPreopen,
        );
    }

    #[test]
    fn test_classify_unknown_idx_i_is_index_no_preopen() {
        // Sectoral display indices fall through to IndexNoPreopen.
        assert_eq!(
            classify_instrument_for_open_source(14, true), // NIFTY AUTO
            OpenSourceClass::IndexNoPreopen,
        );
    }

    // -----------------------------------------------------------------------
    // select_open_source — Plan §9 fallback chain
    // -----------------------------------------------------------------------

    #[test]
    fn test_select_open_source_index_with_preopen_prefers_buffer() {
        assert_eq!(
            select_open_source(OpenSourceClass::IndexWithPreopenBuffer, true, false),
            OpenSource::PreopenBuffer,
        );
    }

    #[test]
    fn test_select_open_source_index_with_preopen_falls_back_to_first_tick() {
        // No REST fallback for indices — buffer-empty goes straight to
        // first tick (with warn).
        assert_eq!(
            select_open_source(OpenSourceClass::IndexWithPreopenBuffer, false, false),
            OpenSource::FirstWsTick,
        );
        // REST availability is IRRELEVANT for indices — no quote.day_open
        // endpoint applies.
        assert_eq!(
            select_open_source(OpenSourceClass::IndexWithPreopenBuffer, false, true),
            OpenSource::FirstWsTick,
        );
    }

    #[test]
    fn test_select_open_source_stock_full_fallback_chain() {
        // 1. Buffer available → buffer wins.
        assert_eq!(
            select_open_source(OpenSourceClass::StockWithPreopen, true, true),
            OpenSource::PreopenBuffer,
        );
        // 2. Buffer missing, REST available → REST.
        assert_eq!(
            select_open_source(OpenSourceClass::StockWithPreopen, false, true),
            OpenSource::RestQuoteDayOpen,
        );
        // 3. Both missing → first tick (with warn).
        assert_eq!(
            select_open_source(OpenSourceClass::StockWithPreopen, false, false),
            OpenSource::FirstWsTick,
        );
    }

    #[test]
    fn test_select_open_source_index_no_preopen_always_first_tick() {
        // SENSEX, INDIA VIX, sectoral indices — first tick is by-design.
        for has_buf in [true, false] {
            for has_rest in [true, false] {
                assert_eq!(
                    select_open_source(OpenSourceClass::IndexNoPreopen, has_buf, has_rest),
                    OpenSource::FirstWsTick,
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // Telegram warn decision — Plan §9
    // -----------------------------------------------------------------------

    #[test]
    fn test_should_warn_on_first_ws_tick_fallback_yes_for_index_with_preopen() {
        assert!(should_warn_on_first_ws_tick_fallback(
            OpenSourceClass::IndexWithPreopenBuffer
        ));
    }

    #[test]
    fn test_should_warn_on_first_ws_tick_fallback_yes_for_stock() {
        assert!(should_warn_on_first_ws_tick_fallback(
            OpenSourceClass::StockWithPreopen
        ));
    }

    #[test]
    fn test_should_warn_on_first_ws_tick_fallback_no_for_index_no_preopen() {
        // SENSEX / INDIA VIX — first-tick is acceptable, NO warn.
        assert!(!should_warn_on_first_ws_tick_fallback(
            OpenSourceClass::IndexNoPreopen
        ));
    }
}

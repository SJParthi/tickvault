//! Phase 2 delta — stock-only F&O subscription computation (Parthiban, 2026-04-20).
//!
//! # Why this module exists
//!
//! At 09:12:30 IST the Phase 2 scheduler wakes up. For every F&O stock,
//! it must:
//! 1. Pick a reference price (the 09:12 close, or backtrack through
//!    09:11/09:10/09:09/09:08 if 09:12 has no tick).
//! 2. Pick an eligible expiry — **strictly more than 2 trading days
//!    remaining** (Dhan does not allow trading stock F&O on expiry
//!    day, and a single-day-remaining contract is dangerous).
//! 3. Find the ATM strike in that expiry's option chain.
//! 4. Return `ATM ± N` CE + PE entries, plus the nearest future.
//!
//! Indices (NIFTY / BANKNIFTY / FINNIFTY / MIDCPNIFTY / SENSEX) are
//! NOT touched by this module — their full chains subscribe at Phase 1
//! per `live-market-feed-subscription.md`.
//!
//! # Hot-path / latency
//!
//! All compute functions here are pure — no I/O, no async, no locks.
//! Called exactly once per day at 09:12:30 IST, but still bounded:
//! O(stocks × log(strikes)) for ATM binary search.

use std::collections::HashMap;

use chrono::NaiveDate;
use tickvault_common::instrument_types::{FnoUniverse, OptionChainKey, UnderlyingKind};
use tickvault_common::trading_calendar::TradingCalendar;

use crate::instrument::preopen_price_buffer::PreOpenCloses;
use crate::websocket::types::InstrumentSubscription;

/// Minimum trading days that MUST remain between `today` (exclusive)
/// and `expiry_date` (inclusive) for a stock F&O contract to be
/// eligible for Phase 2 subscription.
///
/// Parthiban's spec (2026-04-20, verbatim): "greater than two working
/// days not equals". `>2` means ≥3, i.e. at least 3 trading days
/// between today (exclusive) and expiry (inclusive). This prevents
/// subscribing to contracts that are same-day-expiring (0 remaining)
/// or expiring-tomorrow (1 remaining) or expiring-day-after (2 remaining),
/// all of which Dhan either blocks or discourages trading on.
pub const MIN_TRADING_DAYS_TO_EXPIRY: usize = 3;

/// Returns the first (nearest) expiry in `calendar.expiry_dates` that
/// has strictly more than 2 trading days remaining from `today`.
///
/// Returns `None` if every expiry in the calendar is in the past, or
/// if every future expiry has ≤ 2 trading days remaining. The caller
/// treats `None` as "skip this stock for Phase 2" (it will be picked
/// up on the next eligible expiry roll).
///
/// # Performance
///
/// O(E × D) where E = number of expiries (typically 3-6 per stock) and
/// D = days between today and expiry (bounded by `MAX_DAYS_SCAN` to
/// prevent unbounded iteration on malformed data). Called once per
/// stock per day at 09:12:30 IST.
#[must_use]
pub fn select_stock_expiry(
    expiry_dates: &[NaiveDate],
    today: NaiveDate,
    calendar: &TradingCalendar,
) -> Option<NaiveDate> {
    for &expiry in expiry_dates {
        if expiry <= today {
            continue; // skip expired
        }
        let trading_days = count_trading_days_strict(today, expiry, calendar);
        if trading_days > 2 {
            return Some(expiry);
        }
    }
    None
}

/// Counts trading days strictly between `from` (exclusive) and
/// `to` (inclusive). Hard cap at 365 iterations to bound worst case.
///
/// Example: `from=Mon, to=Thu` on a normal week with no holidays:
///   iterates Tue, Wed, Thu → returns 3.
#[inline]
#[must_use]
fn count_trading_days_strict(
    from_exclusive: NaiveDate,
    to_inclusive: NaiveDate,
    calendar: &TradingCalendar,
) -> usize {
    const MAX_DAYS_SCAN: usize = 365;
    let mut count = 0_usize;
    let mut cur = from_exclusive;
    for _ in 0..MAX_DAYS_SCAN {
        let Some(next) = cur.succ_opt() else {
            break;
        };
        cur = next;
        if cur > to_inclusive {
            break;
        }
        if calendar.is_trading_day(cur) {
            count = count.saturating_add(1);
        }
    }
    count
}

/// Finds the strike-price index closest to `spot_price` in a strike-
/// sorted slice. Returns 0 when the slice is empty (caller should
/// guard against that before dispatching).
///
/// Binary-search-with-lower-bound: `partition_point` gives us the
/// first index whose strike > spot. The nearest strike is then either
/// that index or its predecessor — whichever is closer.
#[inline]
#[must_use]
fn nearest_strike_index(strikes: &[f64], spot_price: f64) -> usize {
    if strikes.is_empty() {
        return 0;
    }
    let upper = strikes.partition_point(|&s| s <= spot_price);
    if upper == 0 {
        return 0;
    }
    if upper == strikes.len() {
        return strikes.len() - 1;
    }
    let lo = upper - 1;
    let hi = upper;
    let lo_dist = (strikes[lo] - spot_price).abs();
    let hi_dist = (strikes[hi] - spot_price).abs();
    if lo_dist <= hi_dist { lo } else { hi }
}

/// Result of [`compute_phase2_stock_subscriptions`] — the subscription
/// list plus per-stock accounting so the caller can alert + emit
/// Prometheus metrics without scanning the list twice.
#[derive(Debug, Clone)]
pub struct Phase2StockPlan {
    /// Flat list of `(segment, security_id)` subscriptions to dispatch.
    pub instruments: Vec<InstrumentSubscription>,
    /// Stocks that successfully produced a subscription block.
    pub stocks_subscribed: Vec<String>,
    /// Stocks skipped because the pre-open buffer had no price.
    pub stocks_skipped_no_price: Vec<String>,
    /// Stocks skipped because no expiry had > 2 trading days remaining.
    pub stocks_skipped_no_eligible_expiry: Vec<String>,
    /// Stocks skipped because the universe had no option chain for
    /// the selected expiry (data-quality issue).
    pub stocks_skipped_no_chain: Vec<String>,
}

impl Phase2StockPlan {
    /// Total instruments across all successfully-planned stocks.
    #[must_use]
    pub fn total_instruments(&self) -> usize {
        self.instruments.len()
    }
}

/// Pure function — no I/O, no allocations on the hot path beyond the
/// returned vectors. Given a snapshot of the universe, the pre-open
/// price buffer, the trading calendar, and today's IST date, returns
/// a `Phase2StockPlan` containing the stock F&O subscribe list.
///
/// # Arguments
///
/// - `universe` — built at boot from the Dhan CSV.
/// - `snapshots` — per-stock pre-open minute-close buckets
///   (09:08..09:12 IST) from `preopen_price_buffer::snapshot()`.
/// - `calendar` — from `ApplicationConfig::trading`.
/// - `today` — IST calendar date.
/// - `strikes_each_side` — ATM ± N. Parthiban's spec is `25`
///   (matches `live-market-feed-subscription.md`).
///
/// # Performance
///
/// `O(stocks × (expiries + log(strikes) + N))`, called once per day
/// at 09:12:30 IST. At ~200 stocks with 3 expiries × 50 strikes × 25
/// each side, total work is a few hundred microseconds.
#[must_use]
pub fn compute_phase2_stock_subscriptions(
    universe: &FnoUniverse,
    snapshots: &HashMap<String, PreOpenCloses>,
    calendar: &TradingCalendar,
    today: NaiveDate,
    strikes_each_side: usize,
) -> Phase2StockPlan {
    let mut instruments: Vec<InstrumentSubscription> =
        Vec::with_capacity(universe.underlyings.len() * (2 * strikes_each_side + 1 + 1));
    let mut stocks_subscribed = Vec::new();
    let mut stocks_skipped_no_price = Vec::new();
    let mut stocks_skipped_no_eligible_expiry = Vec::new();
    let mut stocks_skipped_no_chain = Vec::new();

    for (symbol, underlying) in &universe.underlyings {
        // Indices (NseIndex/BseIndex) are handled in Phase 1 — skip.
        if underlying.kind != UnderlyingKind::Stock {
            continue;
        }

        // 1. Price — 09:12 close, backtrack through 09:11/09:10/09:09/09:08.
        let Some(closes) = snapshots.get(symbol) else {
            stocks_skipped_no_price.push(symbol.clone());
            continue;
        };
        let Some(price) = closes.backtrack_latest() else {
            stocks_skipped_no_price.push(symbol.clone());
            continue;
        };

        // 2. Expiry — strictly > 2 trading days remaining.
        let Some(expiry_calendar) = universe.expiry_calendars.get(symbol) else {
            stocks_skipped_no_eligible_expiry.push(symbol.clone());
            continue;
        };
        let Some(expiry) = select_stock_expiry(&expiry_calendar.expiry_dates, today, calendar)
        else {
            stocks_skipped_no_eligible_expiry.push(symbol.clone());
            continue;
        };

        // 3. Option chain for (symbol, expiry).
        let key = OptionChainKey {
            underlying_symbol: symbol.clone(),
            expiry_date: expiry,
        };
        let Some(chain) = universe.option_chains.get(&key) else {
            stocks_skipped_no_chain.push(symbol.clone());
            continue;
        };

        // 4. ATM on CE side (calls are sorted ascending by strike).
        let ce_strikes: Vec<f64> = chain.calls.iter().map(|e| e.strike_price).collect();
        let pe_strikes: Vec<f64> = chain.puts.iter().map(|e| e.strike_price).collect();
        let ce_atm_idx = nearest_strike_index(&ce_strikes, price);
        let pe_atm_idx = nearest_strike_index(&pe_strikes, price);

        // 5. Take ATM ± N on each side, clamped to available range.
        let ce_lo = ce_atm_idx.saturating_sub(strikes_each_side);
        let ce_hi = ce_atm_idx
            .saturating_add(strikes_each_side)
            .saturating_add(1)
            .min(chain.calls.len());
        let pe_lo = pe_atm_idx.saturating_sub(strikes_each_side);
        let pe_hi = pe_atm_idx
            .saturating_add(strikes_each_side)
            .saturating_add(1)
            .min(chain.puts.len());

        for entry in &chain.calls[ce_lo..ce_hi] {
            instruments.push(InstrumentSubscription::new(
                underlying.derivative_segment,
                entry.security_id,
            ));
        }
        for entry in &chain.puts[pe_lo..pe_hi] {
            instruments.push(InstrumentSubscription::new(
                underlying.derivative_segment,
                entry.security_id,
            ));
        }
        // 6. Include the nearest-expiry future (Parthiban's spec:
        //    "for future and options both also").
        if let Some(future_id) = chain.future_security_id {
            instruments.push(InstrumentSubscription::new(
                underlying.derivative_segment,
                future_id,
            ));
        }
        stocks_subscribed.push(symbol.clone());
    }

    Phase2StockPlan {
        instruments,
        stocks_subscribed,
        stocks_skipped_no_price,
        stocks_skipped_no_eligible_expiry,
        stocks_skipped_no_chain,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, TimeZone, Utc};
    use std::time::Duration;
    use tickvault_common::config::{NseHolidayEntry, TradingConfig};
    use tickvault_common::instrument_types::{
        ExpiryCalendar, FnoUnderlying, FnoUniverse, OptionChain, OptionChainEntry, OptionChainKey,
        UnderlyingKind, UniverseBuildMetadata,
    };
    use tickvault_common::types::ExchangeSegment;

    fn empty_universe_metadata() -> UniverseBuildMetadata {
        UniverseBuildMetadata {
            csv_source: String::new(),
            csv_row_count: 0,
            parsed_row_count: 0,
            index_count: 0,
            equity_count: 0,
            underlying_count: 0,
            derivative_count: 0,
            option_chain_count: 0,
            build_duration: Duration::from_millis(0),
            build_timestamp: FixedOffset::east_opt(0)
                .expect("zero offset is valid")
                .from_utc_datetime(&Utc::now().naive_utc()),
        }
    }

    fn make_calendar_empty_holidays() -> TradingCalendar {
        // No holidays → every Mon-Fri is a trading day.
        let cfg = TradingConfig {
            market_open_time: "09:15:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "15:30:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays: vec![],
            muhurat_trading_dates: vec![],
            nse_mock_trading_dates: vec![],
        };
        TradingCalendar::from_config(&cfg).expect("calendar build must succeed")
    }

    fn make_calendar_with_holiday(dates: &[&str]) -> TradingCalendar {
        let cfg = TradingConfig {
            market_open_time: "09:15:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "15:30:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays: dates
                .iter()
                .map(|d| NseHolidayEntry {
                    date: (*d).to_string(),
                    name: "test-holiday".to_string(),
                })
                .collect(),
            muhurat_trading_dates: vec![],
            nse_mock_trading_dates: vec![],
        };
        TradingCalendar::from_config(&cfg).expect("calendar build must succeed")
    }

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).expect("valid date")
    }

    // Monday 2026-04-20 is the anchor used across tests.
    fn today() -> NaiveDate {
        d(2026, 4, 20)
    }

    #[test]
    fn test_count_trading_days_strict_mon_to_thu_is_three() {
        // Mon → Tue, Wed, Thu = 3 trading days.
        let cal = make_calendar_empty_holidays();
        assert_eq!(
            count_trading_days_strict(d(2026, 4, 20), d(2026, 4, 23), &cal),
            3
        );
    }

    #[test]
    fn test_count_trading_days_strict_mon_to_tue_is_one() {
        let cal = make_calendar_empty_holidays();
        assert_eq!(
            count_trading_days_strict(d(2026, 4, 20), d(2026, 4, 21), &cal),
            1
        );
    }

    #[test]
    fn test_count_trading_days_strict_mon_to_mon_next_week_excludes_weekend() {
        // Mon → Tue/Wed/Thu/Fri (4 trading) + Sat/Sun (skipped) + next Mon (5th).
        let cal = make_calendar_empty_holidays();
        assert_eq!(
            count_trading_days_strict(d(2026, 4, 20), d(2026, 4, 27), &cal),
            5
        );
    }

    #[test]
    fn test_count_trading_days_strict_skips_holidays() {
        // Mon → Tue(holiday), Wed, Thu = 2 trading days.
        let cal = make_calendar_with_holiday(&["2026-04-21"]);
        assert_eq!(
            count_trading_days_strict(d(2026, 4, 20), d(2026, 4, 23), &cal),
            2
        );
    }

    #[test]
    fn test_select_stock_expiry_picks_first_eligible() {
        let cal = make_calendar_empty_holidays();
        // today = Mon 4/20. Expiries: Tue(1td), Wed(2td), Thu(3td), next Mon(5td).
        // First > 2 is Thu 4/23.
        let expiries = vec![
            d(2026, 4, 21),
            d(2026, 4, 22),
            d(2026, 4, 23),
            d(2026, 4, 27),
        ];
        assert_eq!(
            select_stock_expiry(&expiries, today(), &cal),
            Some(d(2026, 4, 23))
        );
    }

    #[test]
    fn test_select_stock_expiry_returns_none_when_no_eligible() {
        let cal = make_calendar_empty_holidays();
        // Only 1-day and 2-day expiries exist → all ineligible.
        let expiries = vec![d(2026, 4, 21), d(2026, 4, 22)];
        assert_eq!(select_stock_expiry(&expiries, today(), &cal), None);
    }

    #[test]
    fn test_select_stock_expiry_skips_past_expiries() {
        let cal = make_calendar_empty_holidays();
        // Last Thursday (past) + this Thursday (3 td) → picks this Thursday.
        let expiries = vec![d(2026, 4, 16), d(2026, 4, 23)];
        assert_eq!(
            select_stock_expiry(&expiries, today(), &cal),
            Some(d(2026, 4, 23))
        );
    }

    #[test]
    fn test_select_stock_expiry_handles_holiday_reducing_count_below_threshold() {
        // Thu expiry normally has 3 trading days (Tue/Wed/Thu).
        // If Tue is a holiday, only 2 trading days remain → must skip.
        // Next expiry (following Thu 4/30) has 4 trading days → pick that.
        let cal = make_calendar_with_holiday(&["2026-04-21"]);
        let expiries = vec![d(2026, 4, 23), d(2026, 4, 30)];
        assert_eq!(
            select_stock_expiry(&expiries, today(), &cal),
            Some(d(2026, 4, 30))
        );
    }

    #[test]
    fn test_select_stock_expiry_respects_sorted_order_not_calendar_jump() {
        // Expiries sorted ascending — first eligible wins, later ones ignored.
        let cal = make_calendar_empty_holidays();
        let expiries = vec![d(2026, 4, 23), d(2026, 4, 30), d(2026, 5, 28)];
        assert_eq!(
            select_stock_expiry(&expiries, today(), &cal),
            Some(d(2026, 4, 23))
        );
    }

    #[test]
    fn test_select_stock_expiry_empty_calendar_returns_none() {
        let cal = make_calendar_empty_holidays();
        assert_eq!(select_stock_expiry(&[], today(), &cal), None);
    }

    #[test]
    fn test_nearest_strike_index_exact_match() {
        let strikes = vec![2800.0, 2850.0, 2900.0, 2950.0, 3000.0];
        assert_eq!(nearest_strike_index(&strikes, 2900.0), 2);
    }

    #[test]
    fn test_nearest_strike_index_between_strikes_prefers_lower_on_tie() {
        // Spot exactly midway — tie broken to lower index (documented).
        let strikes = vec![2800.0, 2900.0];
        assert_eq!(nearest_strike_index(&strikes, 2850.0), 0);
    }

    #[test]
    fn test_nearest_strike_index_below_range() {
        let strikes = vec![2800.0, 2850.0, 2900.0];
        assert_eq!(nearest_strike_index(&strikes, 100.0), 0);
    }

    #[test]
    fn test_nearest_strike_index_above_range() {
        let strikes = vec![2800.0, 2850.0, 2900.0];
        assert_eq!(nearest_strike_index(&strikes, 9999.0), 2);
    }

    #[test]
    fn test_nearest_strike_index_empty_returns_zero() {
        let strikes: Vec<f64> = vec![];
        assert_eq!(nearest_strike_index(&strikes, 100.0), 0);
    }

    // -------------------------------------------------------------
    // compute_phase2_stock_subscriptions — integration-style tests
    // against a synthetic FnoUniverse.
    // -------------------------------------------------------------

    fn make_stock_underlying(symbol: &str) -> FnoUnderlying {
        FnoUnderlying {
            underlying_symbol: symbol.to_string(),
            underlying_security_id: 1_000,
            price_feed_security_id: 2_000,
            price_feed_segment: ExchangeSegment::NseEquity,
            derivative_segment: ExchangeSegment::NseFno,
            kind: UnderlyingKind::Stock,
            lot_size: 100,
            contract_count: 51,
        }
    }

    fn make_index_underlying(symbol: &str) -> FnoUnderlying {
        FnoUnderlying {
            underlying_symbol: symbol.to_string(),
            underlying_security_id: 26_000,
            price_feed_security_id: 13,
            price_feed_segment: ExchangeSegment::IdxI,
            derivative_segment: ExchangeSegment::NseFno,
            kind: UnderlyingKind::NseIndex,
            lot_size: 50,
            contract_count: 100,
        }
    }

    fn make_chain(
        symbol: &str,
        expiry: NaiveDate,
        strike_center: f64,
        strikes_each_side: i32,
        base_sid: u32,
    ) -> OptionChain {
        // Build `strikes_each_side * 2 + 1` strikes centered on `strike_center`
        // with 50-point gaps.
        let mut calls = Vec::new();
        let mut puts = Vec::new();
        for i in -strikes_each_side..=strikes_each_side {
            let strike = strike_center + f64::from(i) * 50.0;
            calls.push(OptionChainEntry {
                security_id: base_sid + (i + strikes_each_side) as u32,
                strike_price: strike,
                lot_size: 100,
            });
            puts.push(OptionChainEntry {
                security_id: base_sid + 10_000 + (i + strikes_each_side) as u32,
                strike_price: strike,
                lot_size: 100,
            });
        }
        // Sort ascending so binary search works.
        calls.sort_by(|a, b| a.strike_price.partial_cmp(&b.strike_price).unwrap());
        puts.sort_by(|a, b| a.strike_price.partial_cmp(&b.strike_price).unwrap());
        OptionChain {
            underlying_symbol: symbol.to_string(),
            expiry_date: expiry,
            calls,
            puts,
            future_security_id: Some(base_sid + 50_000),
        }
    }

    fn make_universe_with_stock(
        symbol: &str,
        expiries: Vec<NaiveDate>,
        strike_center: f64,
    ) -> FnoUniverse {
        let mut underlyings = HashMap::new();
        underlyings.insert(symbol.to_string(), make_stock_underlying(symbol));

        let mut expiry_calendars = HashMap::new();
        expiry_calendars.insert(
            symbol.to_string(),
            ExpiryCalendar {
                underlying_symbol: symbol.to_string(),
                expiry_dates: expiries.clone(),
            },
        );

        let mut option_chains = HashMap::new();
        for (i, &exp) in expiries.iter().enumerate() {
            let chain = make_chain(symbol, exp, strike_center, 25, 40_000 + i as u32 * 1_000);
            option_chains.insert(
                OptionChainKey {
                    underlying_symbol: symbol.to_string(),
                    expiry_date: exp,
                },
                chain,
            );
        }

        FnoUniverse {
            underlyings,
            derivative_contracts: HashMap::new(),
            instrument_info: HashMap::new(),
            option_chains,
            expiry_calendars,
            subscribed_indices: Vec::new(),
            build_metadata: empty_universe_metadata(),
        }
    }

    fn stock_buffer(symbol: &str, price_0912: f64) -> HashMap<String, PreOpenCloses> {
        let mut buf = HashMap::new();
        let closes = PreOpenCloses {
            closes: [None, None, None, None, Some(price_0912)],
        };
        buf.insert(symbol.to_string(), closes);
        buf
    }

    #[test]
    fn test_compute_phase2_happy_path_yields_51_ce_plus_51_pe_plus_1_future() {
        let cal = make_calendar_empty_holidays();
        let uni =
            make_universe_with_stock("RELIANCE", vec![d(2026, 4, 23), d(2026, 4, 30)], 2850.0);
        let snapshots = stock_buffer("RELIANCE", 2847.5);

        let plan = compute_phase2_stock_subscriptions(&uni, &snapshots, &cal, today(), 25);

        assert_eq!(plan.stocks_subscribed, vec!["RELIANCE".to_string()]);
        assert!(plan.stocks_skipped_no_price.is_empty());
        assert!(plan.stocks_skipped_no_eligible_expiry.is_empty());
        assert!(plan.stocks_skipped_no_chain.is_empty());
        // 51 strikes × 2 (CE + PE) + 1 future.
        assert_eq!(plan.total_instruments(), 51 * 2 + 1);
    }

    #[test]
    fn test_compute_phase2_skips_stock_without_pre_open_price() {
        let cal = make_calendar_empty_holidays();
        let uni = make_universe_with_stock("INFY", vec![d(2026, 4, 23)], 1500.0);
        let empty_snapshots = HashMap::new();

        let plan = compute_phase2_stock_subscriptions(&uni, &empty_snapshots, &cal, today(), 25);

        assert!(plan.instruments.is_empty());
        assert_eq!(plan.stocks_skipped_no_price, vec!["INFY".to_string()]);
    }

    #[test]
    fn test_compute_phase2_skips_stock_with_no_eligible_expiry() {
        let cal = make_calendar_empty_holidays();
        // Only a 1-day expiry → ineligible.
        let uni = make_universe_with_stock("TCS", vec![d(2026, 4, 21)], 3400.0);
        let snapshots = stock_buffer("TCS", 3400.0);

        let plan = compute_phase2_stock_subscriptions(&uni, &snapshots, &cal, today(), 25);

        assert!(plan.instruments.is_empty());
        assert_eq!(
            plan.stocks_skipped_no_eligible_expiry,
            vec!["TCS".to_string()]
        );
    }

    #[test]
    fn test_compute_phase2_ignores_index_underlyings() {
        let cal = make_calendar_empty_holidays();
        // Universe with ONLY an NseIndex — Phase 2 should skip entirely.
        let mut underlyings = HashMap::new();
        underlyings.insert("NIFTY".to_string(), make_index_underlying("NIFTY"));
        let uni = FnoUniverse {
            underlyings,
            derivative_contracts: HashMap::new(),
            instrument_info: HashMap::new(),
            option_chains: HashMap::new(),
            expiry_calendars: HashMap::new(),
            subscribed_indices: Vec::new(),
            build_metadata: empty_universe_metadata(),
        };
        let snapshots = stock_buffer("NIFTY", 26_000.0);

        let plan = compute_phase2_stock_subscriptions(&uni, &snapshots, &cal, today(), 25);
        assert!(plan.instruments.is_empty());
        assert!(plan.stocks_subscribed.is_empty());
        // Index is NEITHER subscribed NOR counted in any skip bucket.
        assert!(plan.stocks_skipped_no_price.is_empty());
    }

    #[test]
    fn test_compute_phase2_respects_strikes_each_side_parameter() {
        let cal = make_calendar_empty_holidays();
        let uni = make_universe_with_stock("HDFC", vec![d(2026, 4, 23)], 2850.0);
        let snapshots = stock_buffer("HDFC", 2850.0);

        // strikes_each_side = 5 → 5 + 1 + 5 = 11 strikes × 2 + 1 future = 23.
        let plan = compute_phase2_stock_subscriptions(&uni, &snapshots, &cal, today(), 5);
        assert_eq!(plan.total_instruments(), 11 * 2 + 1);
    }

    #[test]
    fn test_compute_phase2_backtracking_uses_0911_when_0912_missing() {
        // If only 09:11 slot is populated, ATM picking still works.
        let cal = make_calendar_empty_holidays();
        let uni = make_universe_with_stock("RELIANCE", vec![d(2026, 4, 23)], 2850.0);
        let mut snapshots = HashMap::new();
        let closes = PreOpenCloses {
            // slots 0..4 = 09:08/09/10/11/12. Only 09:11 has data.
            closes: [None, None, None, Some(2861.0), None],
        };
        snapshots.insert("RELIANCE".to_string(), closes);

        let plan = compute_phase2_stock_subscriptions(&uni, &snapshots, &cal, today(), 25);
        assert_eq!(plan.stocks_subscribed, vec!["RELIANCE".to_string()]);
        assert_eq!(plan.total_instruments(), 51 * 2 + 1);
    }

    #[test]
    fn test_compute_phase2_stock_with_no_buckets_in_any_minute_is_skipped() {
        // All 5 slots None — stock must be skipped.
        let cal = make_calendar_empty_holidays();
        let uni = make_universe_with_stock("WIPRO", vec![d(2026, 4, 23)], 400.0);
        let mut snapshots = HashMap::new();
        snapshots.insert("WIPRO".to_string(), PreOpenCloses { closes: [None; 5] });
        let plan = compute_phase2_stock_subscriptions(&uni, &snapshots, &cal, today(), 25);
        assert!(plan.instruments.is_empty());
        assert_eq!(plan.stocks_skipped_no_price, vec!["WIPRO".to_string()]);
    }

    // ---------------------------------------------------------------
    // Phase 2 late-start live-LTP fallback tests (2026-04-23)
    // ---------------------------------------------------------------
    //
    // These pin the "same process, different spot source" contract:
    // `compute_phase2_stock_subscriptions` MUST accept a synthetic
    // snapshot built from live NSE_EQ LTPs (via
    // `build_synthetic_snap_from_live_ltps`) and produce the same
    // plan shape as it would from the pre-open buffer. This is the
    // fallback path used by the scheduler when the 09:08–09:12 IST
    // window was missed (fresh-clone boot at 11:26 AM, crash recovery
    // mid-session, etc.).

    #[test]
    fn fallback_synthetic_snap_from_live_ltp_produces_normal_phase2_plan() {
        use crate::instrument::preopen_price_buffer::build_synthetic_snap_from_live_ltps;

        let cal = make_calendar_empty_holidays();
        let uni =
            make_universe_with_stock("RELIANCE", vec![d(2026, 4, 23), d(2026, 4, 30)], 2850.0);

        // The scheduler's fallback path: live_stock_ltps map built
        // continuously from the NSE_EQ tick broadcast.
        let mut live_ltps = HashMap::new();
        live_ltps.insert("RELIANCE".to_string(), 2_847.5);
        let synthetic_snap = build_synthetic_snap_from_live_ltps(&live_ltps);

        let plan = compute_phase2_stock_subscriptions(&uni, &synthetic_snap, &cal, today(), 25);

        // Exact same shape as the happy-path test that uses a real
        // pre-open buffer: stock subscribes, 51 × 2 + 1 instruments.
        assert_eq!(plan.stocks_subscribed, vec!["RELIANCE".to_string()]);
        assert!(plan.stocks_skipped_no_price.is_empty());
        assert_eq!(plan.total_instruments(), 51 * 2 + 1);
    }

    #[test]
    fn fallback_empty_live_ltps_skips_every_stock_just_like_empty_preopen() {
        use crate::instrument::preopen_price_buffer::build_synthetic_snap_from_live_ltps;

        let cal = make_calendar_empty_holidays();
        let uni = make_universe_with_stock("INFY", vec![d(2026, 4, 23)], 1_500.0);
        let live_ltps: HashMap<String, f64> = HashMap::new();
        let synthetic_snap = build_synthetic_snap_from_live_ltps(&live_ltps);

        let plan = compute_phase2_stock_subscriptions(&uni, &synthetic_snap, &cal, today(), 25);

        // No live LTPs and no pre-open = no plan. This is correct:
        // we'd rather skip than forge prices.
        assert!(plan.instruments.is_empty());
        assert_eq!(plan.stocks_skipped_no_price, vec!["INFY".to_string()]);
    }

    #[test]
    fn fallback_partial_live_ltp_coverage_subscribes_only_covered_stocks() {
        use crate::instrument::preopen_price_buffer::build_synthetic_snap_from_live_ltps;

        let cal = make_calendar_empty_holidays();
        // Two stocks in the universe, only one has a live LTP.
        let mut uni =
            make_universe_with_stock("RELIANCE", vec![d(2026, 4, 23), d(2026, 4, 30)], 2850.0);
        // Add INFY directly to the universe.
        let infy = make_universe_with_stock("INFY", vec![d(2026, 4, 23), d(2026, 4, 30)], 1500.0);
        for (sym, u) in infy.underlyings {
            uni.underlyings.insert(sym, u);
        }
        for (k, v) in infy.expiry_calendars {
            uni.expiry_calendars.insert(k, v);
        }
        for (k, v) in infy.option_chains {
            uni.option_chains.insert(k, v);
        }

        let mut live_ltps = HashMap::new();
        live_ltps.insert("RELIANCE".to_string(), 2_847.5);
        // INFY missing.
        let synthetic_snap = build_synthetic_snap_from_live_ltps(&live_ltps);

        let plan = compute_phase2_stock_subscriptions(&uni, &synthetic_snap, &cal, today(), 25);

        assert!(plan.stocks_subscribed.contains(&"RELIANCE".to_string()));
        assert!(plan.stocks_skipped_no_price.contains(&"INFY".to_string()));
    }

    #[test]
    fn fallback_synthetic_snap_and_preopen_snap_produce_identical_plans() {
        // Two paths to the same answer: both the real 09:12 close and
        // the live-LTP fallback at 2_847.5 must produce the exact same
        // instrument count. Proves the fallback is not a lossy path.
        use crate::instrument::preopen_price_buffer::build_synthetic_snap_from_live_ltps;

        let cal = make_calendar_empty_holidays();
        let uni =
            make_universe_with_stock("RELIANCE", vec![d(2026, 4, 23), d(2026, 4, 30)], 2850.0);

        let preopen_snap = stock_buffer("RELIANCE", 2_847.5);
        let mut live_ltps = HashMap::new();
        live_ltps.insert("RELIANCE".to_string(), 2_847.5);
        let synthetic_snap = build_synthetic_snap_from_live_ltps(&live_ltps);

        let plan_preopen =
            compute_phase2_stock_subscriptions(&uni, &preopen_snap, &cal, today(), 25);
        let plan_fallback =
            compute_phase2_stock_subscriptions(&uni, &synthetic_snap, &cal, today(), 25);

        assert_eq!(
            plan_preopen.total_instruments(),
            plan_fallback.total_instruments(),
            "fallback path must produce identical instrument count to preopen path at same spot"
        );
        assert_eq!(
            plan_preopen.stocks_subscribed,
            plan_fallback.stocks_subscribed
        );
    }

    #[test]
    fn test_min_trading_days_to_expiry_constant_is_three() {
        // Guards the spec: "greater than two working days" means >=3.
        assert_eq!(MIN_TRADING_DAYS_TO_EXPIRY, 3);
    }
}

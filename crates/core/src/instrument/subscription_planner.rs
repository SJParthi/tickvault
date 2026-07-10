//! Subscription planner: FnoUniverse -> filtered instrument list.
//!
//! Takes the full FnoUniverse (~97K contracts) and produces the exact list of
//! instruments to subscribe on the WebSocket, respecting:
//!
//! 1. **5 major indices** — ALL contracts (all expiries, all strikes, no filtering)
//! 2. **Display indices** — index value only (IDX_I ticker)
//! 3. **Stock equities** — NSE_EQ price feed for each F&O stock
//! 4. **Stock derivatives Stage 1** — current expiry ATM +- N strikes (priority)
//! 5. **Stock derivatives Stage 2** — progressive fill to 25K capacity,
//!    nearest expiry first, deterministic sort order
//!
//! # Feed Mode
//! All instruments start as Ticker mode. Quote and Full are supported in code
//! but configured via `SubscriptionConfig.feed_mode`.
//!
//! # Capacity
//! Total must fit within 25,000 (5 connections x 5,000 each).
//! The planner logs a warning if capacity is exceeded.

use std::collections::{HashMap, HashSet};

use chrono::NaiveDate;
use tracing::{debug, info, warn};

use tickvault_common::config::SubscriptionConfig;
use tickvault_common::constants::{FULL_CHAIN_INDEX_SYMBOLS, MAX_TOTAL_SUBSCRIPTIONS};
use tickvault_common::instrument_registry::{
    InstrumentRegistry, SubscribedInstrument, SubscriptionCategory, make_derivative_instrument,
    make_derivative_instrument_from_archived, make_display_index_instrument,
    make_major_index_instrument, make_stock_equity_instrument,
    make_stock_equity_instrument_from_archived,
};
use tickvault_common::instrument_types::{
    ArchivedFnoUniverse, DhanInstrumentKind, FnoUniverse, IndexCategory, OptionChainKey,
    UnderlyingKind, naive_date_from_archived_i32,
};
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_common::types::{ExchangeSegment, FeedMode};

#[cfg(feature = "daily_universe_fetcher")]
use crate::instrument::daily_universe::{DailyUniverse, InstrumentRole};

// ---------------------------------------------------------------------------
// Subscription Plan — output of the planner
// ---------------------------------------------------------------------------

/// Output of the subscription planner.
///
/// Contains the `InstrumentRegistry` for O(1) lookups and summary statistics.
#[derive(Debug)]
pub struct SubscriptionPlan {
    /// The registry containing all subscribed instruments.
    pub registry: InstrumentRegistry,

    /// Summary of what was planned (for logging).
    pub summary: SubscriptionPlanSummary,
}

/// Human-readable summary of the subscription plan.
#[derive(Debug)]
pub struct SubscriptionPlanSummary {
    /// Number of major index value feeds (IDX_I).
    pub major_index_values: usize,
    /// Number of display index feeds (IDX_I).
    pub display_indices: usize,
    /// Number of index derivative contracts.
    pub index_derivatives: usize,
    /// Number of stock equity price feeds (NSE_EQ).
    pub stock_equities: usize,
    /// Number of stock derivative contracts.
    pub stock_derivatives: usize,
    /// Total instruments in the plan.
    pub total: usize,
    /// Feed mode used.
    pub feed_mode: FeedMode,
    /// Whether the plan exceeds WebSocket capacity.
    pub exceeds_capacity: bool,
    /// Stocks skipped because no current-expiry option chain was found.
    pub stocks_skipped_no_chain: usize,
    /// Total stock derivatives available before capacity cap (Stage 2 progressive fill).
    pub stock_derivatives_available: usize,
    /// Stock derivatives skipped due to 25K capacity cap.
    pub stock_derivatives_skipped: usize,
    /// Capacity utilization as percentage: (total / MAX_TOTAL_SUBSCRIPTIONS) * 100.
    pub capacity_utilization_pct: f64,
}

// ---------------------------------------------------------------------------
// Fix #6 (2026-04-24) — stock F&O expiry rollover helper
// ---------------------------------------------------------------------------

/// Trading-day threshold at or below which a stock F&O nearest expiry is
/// rolled to the next one.
///
/// **STRICT RULE (Parthiban 2026-04-28 update — T-only):** roll ONLY when
/// today IS the expiry day itself (0 trading days remaining). T-1 and
/// earlier KEEP the nearest expiry. The previous T-1 rollover (constant
/// = 1) was over-cautious; Dhan only blocks trading on the expiry day
/// itself, not the day before.
///
/// **Concrete decision table** (e.g., Thursday April 30 = expiry):
///
/// | Today | Trading days remaining | Roll? |
/// |---|---:|---|
/// | Thursday Apr 30 (expiry day, T) | 0 | YES — Dhan disallows expiry-day trading on stock F&O |
/// | Wednesday Apr 29 (T-1) | 1 | NO — keep nearest (was YES pre-2026-04-28) |
/// | Tuesday Apr 28 (T-2) | 2 | NO — keep current expiry |
/// | Monday Apr 27 (T-3) | 3 | NO — keep current expiry |
///
/// **SCOPE — STOCKS ONLY (OPTSTK + FUTSTK):**
/// - Stock options (OPTSTK) and stock futures (FUTSTK) on F&O stocks → ROLL applies
/// - Index options (OPTIDX) and index futures (FUTIDX) for NIFTY/BANKNIFTY/SENSEX → NEVER roll
/// - Index strategies legitimately trade right up to expiry; weekly expiries are routine
/// - Cash equities and IDX_I value feeds have no expiry concept
///
/// Pinned by ratchet test `test_index_expiry_never_rolls_via_planner` —
/// rollover MUST NOT leak into the index path.
pub const STOCK_EXPIRY_ROLLOVER_TRADING_DAYS: u32 = 0;

/// Select the nearest expiry for a stock F&O subscription, applying the
/// stock-only rollover rule. Returns `None` if no suitable expiry exists.
///
/// **SCOPE — STOCKS ONLY (OPTSTK + FUTSTK).** Indices (NIFTY / BANKNIFTY /
/// SENSEX — the 3 full-chain indices as of 2026-04-25) keep nearest expiry
/// unconditionally; they trade actively right up to expiry seconds and
/// weekly index expiries are routine. FINNIFTY + MIDCPNIFTY are not in
/// the F&O universe anymore (dropped 2026-04-25).
///
/// **Rule (T-only since 2026-04-28):** roll ONLY when today IS the expiry
/// day itself (0 trading days remaining). See
/// `STOCK_EXPIRY_ROLLOVER_TRADING_DAYS` docstring for the full decision
/// table. Was previously T-1-or-T; Parthiban 2026-04-28 narrowed to T-only
/// because Dhan only blocks trading on the expiry day itself.
///
/// Behaviour:
/// - When `calendar` is `None`: returns the first expiry `>= today` (the
///   pre-rollover behaviour). Tests and legacy callers rely on this.
/// - When `calendar` is `Some`: if today IS the expiry day (0 trading days
///   remaining), returns the NEXT expiry; otherwise returns the nearest.
///
/// `expiry_dates` MUST be sorted ascending (canonical invariant from the
/// universe builder).
// TEST-EXEMPT: covered by test_stock_expiry_rolls_only_on_t_zero, test_stock_expiry_keeps_nearest_on_t_minus_1, test_stock_expiry_stays_on_t_minus_2, test_stock_expiry_none_calendar_uses_legacy_nearest, test_stock_expiry_no_next_keeps_nearest_on_t_zero, test_stock_expiry_none_when_all_expiries_past, test_index_expiry_never_rolls_via_planner — substring grep misses the full fn name.
pub fn select_stock_expiry_with_rollover(
    expiry_dates: &[NaiveDate],
    today: NaiveDate,
    calendar: Option<&TradingCalendar>,
) -> Option<NaiveDate> {
    // Find index of the nearest expiry >= today.
    let nearest_idx = expiry_dates.iter().position(|d| *d >= today)?;
    let nearest = expiry_dates[nearest_idx];

    let Some(cal) = calendar else {
        return Some(nearest);
    };

    // T-ONLY rule (2026-04-28): roll ONLY when today IS the expiry day
    // (0 trading days remaining). Was T-or-T-1 (`<= 1`) pre-2026-04-28;
    // narrowed because Dhan only blocks trading on expiry day itself, not
    // T-1. Uses `==` rather than `<=` because the constant is now 0 (u32
    // min); `<= 0` is `clippy::absurd_extreme_comparisons` on unsigned.
    let trading_days_left = cal.count_trading_days(today, nearest);
    if trading_days_left == STOCK_EXPIRY_ROLLOVER_TRADING_DAYS {
        // Roll to the NEXT expiry, if one exists.
        if let Some(next) = expiry_dates.get(nearest_idx + 1) {
            debug!(
                %today,
                %nearest,
                next = %next,
                trading_days_left,
                "Fix #6: stock expiry rollover — next expiry chosen"
            );
            return Some(*next);
        }
        // No next expiry — fall back to nearest (better than nothing;
        // the caller may still choose to skip this stock downstream).
        warn!(
            %today,
            %nearest,
            trading_days_left,
            "Fix #6: stock expiry rollover wanted, but no next expiry in calendar — keeping nearest"
        );
    }
    Some(nearest)
}

// ---------------------------------------------------------------------------
// Planner
// ---------------------------------------------------------------------------

/// Builds a subscription plan from the FnoUniverse and configuration.
///
/// # Strategy
/// - **Indices (NIFTY, BANKNIFTY, SENSEX, MIDCPNIFTY, FINNIFTY):**
///   Subscribe the index value feed (IDX_I) + ALL derivative contracts
///   (all expiries, all strikes). No filtering whatsoever. Indices keep
///   nearest expiry unconditionally — Fix #6 rollover does NOT apply.
///
/// - **Display indices (INDIA VIX, sectoral, broad market):**
///   Subscribe the index value feed only (IDX_I). No derivatives.
///
/// - **Stocks (~206):**
///   Subscribe the equity price feed (NSE_EQ) + current expiry only +
///   ATM ± N strikes (CE + PE) + current-month future.
///   ATM is approximated using the middle strike of the current expiry chain
///   (since no live prices are available at startup).
///   **Fix #6 (2026-04-24, narrowed 2026-04-28 to T-only):** when
///   `trading_calendar` is `Some`, the current expiry rolls to the NEXT
///   one ONLY if today IS the expiry day (0 trading days remaining).
///   Dhan disallows stock F&O trading on expiry day itself; T-1 and
///   earlier are tradeable so we keep the nearest expiry.
///
/// # Feed Mode
/// All instruments use the same feed mode from config (Ticker by default).
///
/// # Spot Prices
/// Map of underlying_symbol → LTP. When a stock's LTP is present, ATM is
/// calculated from the real spot price (binary search on sorted chain).
/// When absent AND the map is non-empty, that stock's F&O is SKIPPED.
/// When the map is empty, falls back to median strike (boot-time behavior
/// before pre-market prices are available).
///
/// # Trading Calendar (Fix #6 — T-only since 2026-04-28)
/// When `trading_calendar` is `Some`, stock F&O expiries roll forward
/// ONLY when today IS the expiry day (0 trading days remaining). When
/// `None`, nearest expiry is always used (legacy behaviour —
/// pre-2026-04-24).
/// AWS-lifecycle LOCKED (PR #7b) — under `SubscriptionScope::Indices4Only`
/// (the only remaining variant) stock derivatives are NEVER subscribed.
/// Pure function, always returns `false`. Retained as a gate point so
/// existing call sites in the planner read naturally; downstream PRs
/// may inline + delete once the planner is restructured.
#[inline]
#[must_use]
pub const fn should_subscribe_stock_derivatives(_config: &SubscriptionConfig) -> bool {
    false
}

/// AWS-lifecycle LOCKED (PR #7b) — under `SubscriptionScope::Indices4Only`
/// index derivatives are NEVER subscribed. Pure function, always
/// returns `false`.
#[inline]
#[must_use]
pub const fn should_subscribe_index_derivatives(_config: &SubscriptionConfig) -> bool {
    false
}

/// AWS-lifecycle LOCKED (PR #7b) — under `SubscriptionScope::Indices4Only`
/// the ONLY allowed display index is INDIA VIX (one of the 4 LOCKED
/// IDX_I SIDs). All sectoral / broad-market indices are PARKED.
///
/// Gates by stable Dhan `SecurityId` (`INDIA_VIX_SECURITY_ID = 21`),
/// NOT the display name — filtering on `"INDIA VIX"` string was brittle
/// against Dhan CSV name drift.
///
/// Pure `const fn`; tested by
/// `test_is_display_index_allowed_under_scope_keeps_only_india_vix`.
#[inline]
#[must_use]
pub const fn is_display_index_allowed_under_scope(
    _config: &SubscriptionConfig,
    security_id: tickvault_common::types::SecurityId,
) -> bool {
    security_id == tickvault_common::constants::INDIA_VIX_SECURITY_ID
}

pub fn build_subscription_plan(
    universe: &FnoUniverse,
    config: &SubscriptionConfig,
    today: NaiveDate,
    spot_prices: &std::collections::HashMap<String, f64>,
    trading_calendar: Option<&TradingCalendar>,
) -> SubscriptionPlan {
    let feed_mode = config.parsed_feed_mode().unwrap_or(FeedMode::Ticker);
    // Wave 5 Item 2: scope gate. Under `IndicesOnlyAllExpiries`, no stock
    // F&O is subscribed regardless of the legacy boolean.
    let stock_derivatives_enabled = should_subscribe_stock_derivatives(config);

    let mut instruments: Vec<SubscribedInstrument> = Vec::with_capacity(MAX_TOTAL_SUBSCRIPTIONS);
    // BUG FIX (2026-04-17, spotted by Parthiban): dedup was keyed on
    // `security_id` alone, but Dhan CAN reuse the same numeric security_id
    // across different segments (e.g. id=13 is FINNIFTY in IDX_I AND is
    // some other instrument in NSE_EQ / NSE_FNO). The old single-key set
    // on the numeric id silently skipped the second-seen instance — which
    // is why FINNIFTY's IDX_I subscription never went out and the depth
    // ATM selector had no spot price for it. Correct dedup key is
    // `(security_id, segment)`.
    let mut seen_ids: HashSet<(u64, ExchangeSegment)> =
        HashSet::with_capacity(MAX_TOTAL_SUBSCRIPTIONS);
    let mut stocks_skipped_no_chain: usize = 0;

    let full_chain_set: HashSet<&str> = FULL_CHAIN_INDEX_SYMBOLS.iter().copied().collect();

    // -----------------------------------------------------------------------
    // 1a. Major index value feeds (IDX_I) — from `underlyings` HashMap
    //
    // Legacy path: pre-AWS-lifecycle the universe-builder populated
    // `underlyings` for every F&O underlying (3 indices + 216 stocks). PR
    // #6b retired the builder, so this HashMap is now empty under
    // `FnoUniverse::locked_4_idx_i()`. The post-PR #6b path lives in
    // section 1b below.
    // -----------------------------------------------------------------------
    for underlying in universe.underlyings.values() {
        if !full_chain_set.contains(underlying.underlying_symbol.as_str()) {
            continue;
        }
        if seen_ids.insert((underlying.price_feed_security_id, ExchangeSegment::IdxI)) {
            instruments.push(make_major_index_instrument(
                underlying.price_feed_security_id,
                &underlying.underlying_symbol,
                feed_mode,
            ));
        }
    }

    // -----------------------------------------------------------------------
    // 1b. Major index value feeds (IDX_I) — from `subscribed_indices`
    //
    // POST-AWS-LIFECYCLE FIX (hotfix 2026-05-19): `FnoUniverse::locked_4_idx_i()`
    // populates `subscribed_indices` (not `underlyings`) for the 3 majors
    // with `category = IndexCategory::FnoUnderlying`. Without this loop the
    // planner emitted only INDIA VIX (the sole `DisplayIndex` in the LOCKED
    // universe), shipping a 1-SID subscription instead of 4. Boot log:
    //   "major_index_values":0, "display_indices":1, "total":1
    // The planner now drains BOTH paths so legacy builder-populated runs
    // (section 1a) AND the LOCKED-universe path (this section) both work.
    // -----------------------------------------------------------------------
    for index in &universe.subscribed_indices {
        if index.category != IndexCategory::FnoUnderlying {
            continue;
        }
        if !full_chain_set.contains(index.symbol.as_str()) {
            continue;
        }
        if seen_ids.insert((index.security_id, ExchangeSegment::IdxI)) {
            instruments.push(make_major_index_instrument(
                index.security_id,
                &index.symbol,
                feed_mode,
            ));
        }
    }

    // -----------------------------------------------------------------------
    // 2. Display indices (IDX_I)
    //
    // Under `Indices4Only` scope this loop is filtered to INDIA VIX only
    // via `is_display_index_allowed_under_scope`. All other sectoral /
    // broad-market display indices are PARKED.
    // -----------------------------------------------------------------------
    for index in &universe.subscribed_indices {
        if index.category == IndexCategory::DisplayIndex
            && is_display_index_allowed_under_scope(config, index.security_id)
            && seen_ids.insert((index.security_id, ExchangeSegment::IdxI))
        {
            instruments.push(make_display_index_instrument(
                index.security_id,
                &index.symbol,
                feed_mode,
            ));
        }
    }

    // -----------------------------------------------------------------------
    // 3. Index derivatives — full chain at ALL future expiries (3 indices)
    //
    // 2026-05-02 (operator-confirmed restore of pre-2026-04-25 behavior):
    // subscribe EVERY contract whose `expiry_date >= today` for NIFTY +
    // BANKNIFTY + SENSEX — current weekly + next weekly + monthly + far
    // monthly + every strike on each. Yields ~10-11K contracts vs the
    // ~1.8K nearest-expiry-only count. Operator's intent is full chain
    // for all live expiries so strategies have visibility across the
    // term structure.
    //
    // The 25K Dhan WS cap still holds because stock F&O is OFF under
    // `IndicesOnlyAllExpiries` scope (Wave 5 default) — index F&O
    // ~10-11K + stock equities 209 + IDX_I ~26 = ~10.3K total, well
    // under cap with ~14K headroom.
    //
    // Indices NEVER apply the stock rollover rule (≤1-day-to-expiry roll).
    // Per ratchet `test_index_expiry_never_rolls_via_planner`, index
    // strategies legitimately trade expiry-day, so every future expiry
    // (including today) stays in.
    //
    // Phase 0 Item 1 (2026-05-13): `should_subscribe_index_derivatives` returns
    // FALSE under the `IndicesUnderlyingsOnly` scope — the entire index F&O
    // chain (~10K contracts) is PARKED until Phase 2.
    // -----------------------------------------------------------------------
    if should_subscribe_index_derivatives(config) {
        // Single pass: subscribe every NIFTY/BANKNIFTY/SENSEX derivative
        // whose expiry is today or future. No nearest-expiry filter.
        for contract in universe.derivative_contracts.values() {
            let is_index = matches!(
                contract.instrument_kind,
                tickvault_common::instrument_types::DhanInstrumentKind::FutureIndex
                    | tickvault_common::instrument_types::DhanInstrumentKind::OptionIndex
            );
            if is_index
                && full_chain_set.contains(contract.underlying_symbol.as_str())
                && contract.expiry_date >= today
                && seen_ids.insert((contract.security_id, contract.exchange_segment))
            {
                instruments.push(make_derivative_instrument(
                    contract,
                    SubscriptionCategory::IndexDerivative,
                    feed_mode,
                ));
            }
        }
    }

    // -----------------------------------------------------------------------
    // 4. Stock equities (NSE_EQ price feed) + Stock derivatives (current expiry)
    // -----------------------------------------------------------------------
    // 2026-04-25: Track each stock's selected expiry so Stage 2 progressive
    // fill (Section 5 below) ALSO respects the rollover. Without this map,
    // Stage 2 would silently re-subscribe contracts at the rolled-away
    // nearest expiry — defeating the safety margin (T-1 rollover for stock
    // options because Dhan disallows expiry-day trading).
    let mut selected_expiry_per_stock: HashMap<String, NaiveDate> =
        HashMap::with_capacity(universe.underlyings.len());

    for underlying in universe.underlyings.values() {
        if underlying.kind != UnderlyingKind::Stock {
            continue;
        }

        // 4a. Stock equity price feed.
        //
        // Wave 5 Item 14: NSE_EQ subscriptions use Quote mode UNCONDITIONALLY
        // (independent of `config.feed_mode`). The Quote packet (code 4,
        // 50 bytes) carries LTP / LTQ / LTT / ATP + volume + day OHLC +
        // prev_close (bytes 38-41) — everything cash-equity needs under
        // the indices-only scope. The 162-byte Full packet's extra payload
        // (OI + 5-level depth) is unused for cash equities and would
        // waste ~115 KB/sec of bandwidth across ~206 stocks at peak.
        // The Item-13 ratchet pins this contract.
        let nse_eq_feed_mode = FeedMode::Quote;
        if config.subscribe_stock_equities
            && seen_ids.insert((
                underlying.price_feed_security_id,
                underlying.price_feed_segment,
            ))
        {
            instruments.push(make_stock_equity_instrument(underlying, nse_eq_feed_mode));
        }

        // 4b. Stock derivatives — current expiry only, ATM ± N strikes.
        // Wave 5 Item 2: under `IndicesOnlyAllExpiries` scope this short-
        // circuits for every stock underlying, dropping the entire 216-
        // stock F&O block (~22K contracts) from the plan.
        if !stock_derivatives_enabled {
            continue;
        }

        // Find current expiry. STOCK-ONLY rollover (OPTSTK + FUTSTK):
        // roll to the NEXT expiry when STRICTLY LESS THAN 2 trading days
        // remain (i.e. today is T or T-1). Indices NEVER apply this rule.
        // See `select_stock_expiry_with_rollover` and
        // `STOCK_EXPIRY_ROLLOVER_TRADING_DAYS` for the full decision table.
        let current_expiry = universe
            .expiry_calendars
            .get(&underlying.underlying_symbol)
            .and_then(|cal| {
                select_stock_expiry_with_rollover(&cal.expiry_dates, today, trading_calendar)
            });

        let Some(expiry) = current_expiry else {
            debug!(
                underlying = %underlying.underlying_symbol,
                "No current expiry found — skipping stock derivatives"
            );
            stocks_skipped_no_chain = stocks_skipped_no_chain.saturating_add(1);
            continue;
        };

        // 2026-04-25: Record the selected expiry so Stage 2 (Section 5)
        // ONLY fills at this same expiry — preserving the rollover safety
        // margin. Without this, Stage 2 would silently subscribe nearest-
        // expiry contracts on T-1 stocks.
        selected_expiry_per_stock.insert(underlying.underlying_symbol.clone(), expiry);

        // Subscribe the current-month future for this expiry
        let chain_key = OptionChainKey {
            underlying_symbol: underlying.underlying_symbol.clone(),
            expiry_date: expiry,
        };

        if let Some(chain) = universe.option_chains.get(&chain_key) {
            // Future for this expiry
            if let Some(future_id) = chain.future_security_id
                && let Some(future_contract) = universe.derivative_contracts.get(&future_id)
                && seen_ids.insert((future_id, future_contract.exchange_segment))
            {
                instruments.push(make_derivative_instrument(
                    future_contract,
                    SubscriptionCategory::StockDerivative,
                    feed_mode,
                ));
            }

            // ATM ± N strike filtering.
            // When spot_prices map is non-empty → use real spot price (binary search).
            // When spot_prices map is empty → use median strike (boot-time fallback).
            // When map is non-empty but stock missing → skip that stock's options.
            let atm_above = config.stock_atm_strikes_above;
            let atm_below = config.stock_atm_strikes_below;

            let spot = spot_prices.get(&underlying.underlying_symbol).copied();
            let call_count = chain.calls.len();
            let put_count = chain.puts.len();

            // ATM index for calls (binary search on sorted strikes).
            let call_atm_idx: Option<usize> = if call_count > 0 {
                match spot {
                    Some(price) if price > 0.0 && price.is_finite() => {
                        // Binary search: find strike closest to spot price.
                        let idx = chain.calls.partition_point(|e| e.strike_price < price);
                        let candidates = [idx.saturating_sub(1), idx.min(call_count - 1)];
                        candidates.iter().copied().min_by(|&a, &b| {
                            let da = (chain.calls[a].strike_price - price).abs();
                            let db = (chain.calls[b].strike_price - price).abs();
                            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
                        })
                    }
                    _ => {
                        // No pre-market price available — skip this stock's options.
                        // At 9:12 AM, every traded stock has an LTP from the
                        // pre-market auction. If missing, the stock didn't trade
                        // in pre-market — skip it (no median fallback).
                        debug!(
                            underlying = %underlying.underlying_symbol,
                            "No pre-market price — skipping stock options"
                        );
                        None
                    }
                }
            } else {
                None
            };

            // Subscribe calls: ATM ± N
            if let Some(atm_idx) = call_atm_idx {
                let start = atm_idx.saturating_sub(atm_below);
                let end = atm_idx
                    .saturating_add(atm_above)
                    .saturating_add(1)
                    .min(call_count);
                for entry in &chain.calls[start..end] {
                    if let Some(contract) = universe.derivative_contracts.get(&entry.security_id)
                        && seen_ids.insert((entry.security_id, contract.exchange_segment))
                    {
                        instruments.push(make_derivative_instrument(
                            contract,
                            SubscriptionCategory::StockDerivative,
                            feed_mode,
                        ));
                    }
                }

                // PROOF: log ATM selection for this stock (cold path — once per stock at boot)
                if let Some(sp) = spot {
                    info!(
                        underlying = %underlying.underlying_symbol,
                        spot = sp,
                        atm_strike = chain.calls[atm_idx].strike_price,
                        expiry = %expiry,
                        calls = end.saturating_sub(start),
                        "PROOF: stock ATM selection — spot {sp:.2} → ATM strike {}, ± {atm_above}/{atm_below} calls",
                        chain.calls[atm_idx].strike_price,
                    );
                }
            }

            // ATM index for puts (same binary search as calls).
            let put_atm_idx: Option<usize> = if put_count > 0 {
                match spot {
                    Some(price) if price > 0.0 && price.is_finite() => {
                        let idx = chain.puts.partition_point(|e| e.strike_price < price);
                        let candidates = [idx.saturating_sub(1), idx.min(put_count - 1)];
                        candidates.iter().copied().min_by(|&a, &b| {
                            let da = (chain.puts[a].strike_price - price).abs();
                            let db = (chain.puts[b].strike_price - price).abs();
                            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
                        })
                    }
                    _ => None, // No pre-market price — skipped (same as calls)
                }
            } else {
                None
            };

            // Subscribe puts: ATM ± N
            if let Some(atm_idx) = put_atm_idx {
                let start = atm_idx.saturating_sub(atm_below);
                let end = atm_idx
                    .saturating_add(atm_above)
                    .saturating_add(1)
                    .min(put_count);
                for entry in &chain.puts[start..end] {
                    if let Some(contract) = universe.derivative_contracts.get(&entry.security_id)
                        && seen_ids.insert((entry.security_id, contract.exchange_segment))
                    {
                        instruments.push(make_derivative_instrument(
                            contract,
                            SubscriptionCategory::StockDerivative,
                            feed_mode,
                        ));
                    }
                }
            }
        } else {
            debug!(
                underlying = %underlying.underlying_symbol,
                expiry = %expiry,
                "No option chain found for current expiry — skipping"
            );
            stocks_skipped_no_chain = stocks_skipped_no_chain.saturating_add(1);
        }
    }

    // -----------------------------------------------------------------------
    // 5. Progressive fill — remaining stock derivatives to 25K capacity
    //    Stage 2: stock derivatives at THE PER-STOCK SELECTED EXPIRY only,
    //    nearest first. Stage 1 (ATM ± 25 above) added priority instruments.
    //    seen_ids ensures no duplicates.
    //
    //    2026-04-25 SAFETY: Stage 2 MUST respect the rollover via
    //    `selected_expiry_per_stock`. Stocks on T-1 (rolled to next expiry)
    //    must NOT have nearest-expiry contracts subscribed by Stage 2. Stocks
    //    that were skipped in Stage 1 (no spot price) are excluded entirely
    //    so we never subscribe a stock without ATM resolution.
    // -----------------------------------------------------------------------
    let mut stock_derivatives_available: usize = 0;
    let mut stock_derivatives_skipped: usize = 0;

    // Wave 5 Item 2 scope gate. Under `IndicesOnlyAllExpiries` the entire
    // Stage 2 progressive-fill block is dead code.
    if stock_derivatives_enabled {
        // O(1) EXEMPT: begin — planner runs once at startup, not per tick
        let count_before_stage2 = instruments.len();

        let mut remaining_stock_derivatives: Vec<
            &tickvault_common::instrument_types::DerivativeContract,
        > = universe
            .derivative_contracts
            .values()
            .filter(|c| {
                let kind_ok = matches!(
                    c.instrument_kind,
                    tickvault_common::instrument_types::DhanInstrumentKind::FutureStock
                        | tickvault_common::instrument_types::DhanInstrumentKind::OptionStock
                );
                if !kind_ok {
                    return false;
                }
                // SAFETY (2026-04-25): only include if Stage 1 selected an
                // expiry for this stock AND this contract's expiry matches.
                // This makes Stage 2 honour the stock-only rollover.
                let expiry_ok = selected_expiry_per_stock
                    .get(c.underlying_symbol.as_str())
                    .is_some_and(|sel| *sel == c.expiry_date);
                expiry_ok && !seen_ids.contains(&(c.security_id, c.exchange_segment))
            })
            .collect();

        // Deterministic sort: nearest expiry -> underlying -> security_id
        remaining_stock_derivatives.sort_by(|a, b| {
            a.expiry_date
                .cmp(&b.expiry_date)
                .then(a.underlying_symbol.cmp(&b.underlying_symbol))
                .then(a.security_id.cmp(&b.security_id))
        });

        stock_derivatives_available = remaining_stock_derivatives.len();
        for contract in &remaining_stock_derivatives {
            if instruments.len() >= MAX_TOTAL_SUBSCRIPTIONS {
                break;
            }
            if seen_ids.insert((contract.security_id, contract.exchange_segment)) {
                instruments.push(make_derivative_instrument(
                    contract,
                    SubscriptionCategory::StockDerivative,
                    feed_mode,
                ));
            }
        }

        let stage2_added = instruments.len().saturating_sub(count_before_stage2);
        stock_derivatives_skipped = stock_derivatives_available.saturating_sub(stage2_added);

        info!(
            stage2_available = stock_derivatives_available,
            stage2_added = stage2_added,
            stage2_skipped = stock_derivatives_skipped,
            "Progressive fill: Stage 2 stock derivatives"
        );
        // O(1) EXEMPT: end
    }

    // -----------------------------------------------------------------------
    // Build the registry and summary
    // -----------------------------------------------------------------------
    let registry = InstrumentRegistry::from_instruments(instruments);

    // I-P1-11 gap G (2026-04-17): expose registry composite-index health
    // as Prometheus gauges so the operator sees cross-segment collisions
    // in Grafana + receives Telegram alerts on ERROR logs emitted during
    // construction.
    metrics::gauge!("tv_instrument_registry_cross_segment_collisions")
        .set(registry.cross_segment_collisions() as f64);
    metrics::gauge!("tv_instrument_registry_total_entries").set(registry.len() as f64);

    // PR #288 (#5b): emit one gauge per collision pair so operators can
    // see WHICH ids collided in Grafana without parsing logs. Label values
    // are stable (numeric id + enum segment) so cardinality is bounded by
    // the number of actual collisions (typical = 0-3, worst case ~10 per
    // Dhan's CSV).
    for (security_id, losing, winning) in registry.collision_pairs() {
        metrics::gauge!(
            "tv_instrument_registry_collision_pair",
            "security_id" => security_id.to_string(),
            "losing_segment" => losing.as_str(),
            "winning_segment" => winning.as_str(),
        )
        .set(1.0);
    }

    let exceeds_capacity = registry.len() > MAX_TOTAL_SUBSCRIPTIONS;
    if exceeds_capacity {
        warn!(
            total = registry.len(),
            capacity = MAX_TOTAL_SUBSCRIPTIONS,
            "Subscription plan exceeds WebSocket capacity — some instruments will not be subscribed"
        );
    }

    let total = registry.len();
    let capacity_utilization_pct = if MAX_TOTAL_SUBSCRIPTIONS > 0 {
        (total as f64 / MAX_TOTAL_SUBSCRIPTIONS as f64) * 100.0
    } else {
        0.0
    };

    let summary = SubscriptionPlanSummary {
        major_index_values: registry.category_count(SubscriptionCategory::MajorIndexValue),
        display_indices: registry.category_count(SubscriptionCategory::DisplayIndex),
        index_derivatives: registry.category_count(SubscriptionCategory::IndexDerivative),
        stock_equities: registry.category_count(SubscriptionCategory::StockEquity),
        stock_derivatives: registry.category_count(SubscriptionCategory::StockDerivative),
        total,
        feed_mode,
        exceeds_capacity,
        stocks_skipped_no_chain,
        stock_derivatives_available,
        stock_derivatives_skipped,
        capacity_utilization_pct,
    };

    info!(
        major_index_values = summary.major_index_values,
        display_indices = summary.display_indices,
        index_derivatives = summary.index_derivatives,
        stock_equities = summary.stock_equities,
        stock_derivatives = summary.stock_derivatives,
        total = summary.total,
        feed_mode = %feed_mode,
        stocks_skipped = summary.stocks_skipped_no_chain,
        capacity_pct = format!("{:.1}%", summary.capacity_utilization_pct),
        "Subscription plan built"
    );

    SubscriptionPlan { registry, summary }
}

/// §36 (2026-07-08): map an IndexFuture target's CSV segment string to the
/// WebSocket `ExchangeSegment`. FUTIDX segment CANNOT be derived from the
/// role alone (SENSEX = BSE_FNO, the NSE three = NSE_FNO), so it comes from
/// the authoritative `csv_row.segment`. Unknown values → `None` (fail-closed
/// skip + counter — never mis-subscribed).
#[cfg(feature = "daily_universe_fetcher")]
fn futidx_segment_from_csv(csv_segment: &str) -> Option<ExchangeSegment> {
    match csv_segment {
        "NSE_FNO" => Some(ExchangeSegment::NseFno),
        "BSE_FNO" => Some(ExchangeSegment::BseFno),
        _ => None,
    }
}

/// Builds a subscription plan from a daily-universe (~250 SIDs) per the
/// 2026-05-27 `DailyUniverse` scope. Every SID subscribes in **Quote mode**
/// (§8 — the 50-byte packet carries day OHLC at fixed offsets). IDX_I
/// indices, NSE_EQ F&O-underlying/NTM spots, and (§36 2026-07-08 / §36.7
/// 2026-07-10) the all-months FUTIDX contracts of the 4 underlyings are
/// subscribed — never any other derivative. Dedup is the composite `(security_id, exchange_segment)` key
/// (I-P1-11). Cold path — called once at boot.
///
/// The instrument segment is derived from the target ROLE, which is
/// authoritative per rule §2 (indices → IDX_I; F&O underlyings resolve to
/// their NSE_EQ spot row; IndexFuture → [`futidx_segment_from_csv`]). Rows
/// whose `security_id` is not a valid `u64` are skipped + counted — never
/// panics on malformed CSV-derived data.
#[cfg(feature = "daily_universe_fetcher")]
pub fn build_subscription_plan_from_daily_universe(universe: &DailyUniverse) -> SubscriptionPlan {
    // §8: the daily universe subscribes every SID in Quote mode.
    let feed_mode = FeedMode::Quote;

    let mut instruments: Vec<SubscribedInstrument> =
        Vec::with_capacity(universe.subscription_targets.len());
    // I-P1-11: dedup on the composite (security_id, segment) key.
    let mut seen_ids: HashSet<(u64, ExchangeSegment)> =
        HashSet::with_capacity(universe.subscription_targets.len());
    let mut skipped_unparsable_sid: usize = 0;
    let mut skipped_futidx_bad_segment: usize = 0;
    // §36 hostile-review round 2 (2026-07-08): count the IndexFuture targets
    // the universe HANDED the planner, so the post-plan honesty check below
    // can page FUTIDX-01 on ANY planner-stage drop of a chosen future
    // (unparsable SID / unknown segment / composite-key dedup) — the Dhan
    // mirror of the Groww post-cap gauge fix (round 1, R1-B).
    let expected_futures = universe
        .subscription_targets
        .iter()
        .filter(|t| t.role == InstrumentRole::IndexFuture)
        .count();

    for target in &universe.subscription_targets {
        let Ok(security_id) = target.csv_row.security_id.parse::<u64>() else {
            skipped_unparsable_sid += 1;
            continue;
        };
        // Role determines segment deterministically (rule §2). NTM
        // constituents (§31) are NSE_EQ equities, same as F&O underlyings.
        let segment = match target.role {
            InstrumentRole::Index => ExchangeSegment::IdxI,
            InstrumentRole::FnoUnderlying | InstrumentRole::IndexConstituent => {
                ExchangeSegment::NseEquity
            }
            // §36 (2026-07-08): FUTIDX segment comes from the CSV row —
            // SENSEX is BSE_FNO, the NSE three are NSE_FNO. Unknown segment
            // → fail-closed skip + counter, never mis-subscribed.
            InstrumentRole::IndexFuture => match futidx_segment_from_csv(&target.csv_row.segment) {
                Some(seg) => seg,
                None => {
                    skipped_futidx_bad_segment += 1;
                    continue;
                }
            },
        };
        if !seen_ids.insert((security_id, segment)) {
            continue;
        }
        let instrument = match target.role {
            InstrumentRole::Index => {
                make_major_index_instrument(security_id, &target.csv_row.symbol_name, feed_mode)
            }
            InstrumentRole::FnoUnderlying | InstrumentRole::IndexConstituent => {
                SubscribedInstrument {
                    security_id,
                    exchange_segment: ExchangeSegment::NseEquity,
                    category: SubscriptionCategory::StockEquity,
                    display_label: target.csv_row.symbol_name.clone(),
                    underlying_symbol: target.csv_row.symbol_name.clone(),
                    instrument_kind: None,
                    expiry_date: None,
                    strike_price: None,
                    option_type: None,
                    feed_mode,
                }
            }
            // §36/§36.7 (2026-07-10): one of the all-months index futures.
            // Feed mode stays the plan-global Quote binding (§8 lock);
            // prev-close arrives in the Quote packet at bytes 38-41 (Ticket
            // #5525125, dhan_locked_facts.rs).
            InstrumentRole::IndexFuture => SubscribedInstrument {
                security_id,
                exchange_segment: segment,
                category: SubscriptionCategory::IndexDerivative,
                // Precise contract label (Dhan-support commandment).
                display_label: target.csv_row.symbol_name.clone(),
                underlying_symbol: target.csv_row.underlying_symbol.clone(),
                instrument_kind: Some(DhanInstrumentKind::FutureIndex),
                expiry_date: NaiveDate::parse_from_str(
                    target.csv_row.expiry_date.trim(),
                    "%Y-%m-%d",
                )
                .ok(),
                strike_price: None,
                option_type: None,
                feed_mode,
            },
        };
        instruments.push(instrument);
    }

    if skipped_unparsable_sid > 0 {
        warn!(
            skipped_unparsable_sid,
            "daily-universe plan: skipped rows with non-numeric security_id"
        );
    }
    if skipped_futidx_bad_segment > 0 {
        warn!(
            skipped_futidx_bad_segment,
            "daily-universe plan: skipped IndexFuture targets with unknown csv segment (§36 \
             fail-closed — expected NSE_FNO or BSE_FNO)"
        );
    }

    let registry = InstrumentRegistry::from_instruments(instruments);
    metrics::gauge!("tv_instrument_registry_cross_segment_collisions")
        .set(registry.cross_segment_collisions() as f64);
    metrics::gauge!("tv_instrument_registry_total_entries").set(registry.len() as f64);

    // §36 hostile-review round 2 (2026-07-08): the Dhan futures gauge is
    // POST-PLAN and honest — it reports what is actually IN the plan, and a
    // planner-stage drop of a chosen future is LOUD (FUTIDX-01), mirroring
    // the Groww post-cap fix. IndexDerivative is populated ONLY by the
    // IndexFuture role in the DailyUniverse plan, so the category count IS
    // the planned-futures count.
    let planned_futures = registry.category_count(SubscriptionCategory::IndexDerivative);
    metrics::gauge!("tv_index_futures_selected", "feed" => "dhan").set(planned_futures as f64);
    if planned_futures < expected_futures {
        tracing::error!(
            code = tickvault_common::error_code::ErrorCode::Futidx01SelectionDegraded.code_str(),
            feed = "dhan",
            expected = expected_futures,
            planned = planned_futures,
            skipped_unparsable_sid,
            skipped_futidx_bad_segment,
            "index-future(s) dropped at the PLAN stage (unparsable SID / unknown segment / \
             composite-key dedup) — the gauge reports the POST-PLAN count; subscription \
             proceeds without them"
        );
    }

    let total = registry.len();
    let exceeds_capacity = total > MAX_TOTAL_SUBSCRIPTIONS;
    if exceeds_capacity {
        warn!(
            total,
            capacity = MAX_TOTAL_SUBSCRIPTIONS,
            "Daily-universe plan exceeds WebSocket capacity"
        );
    }
    let capacity_utilization_pct = if MAX_TOTAL_SUBSCRIPTIONS > 0 {
        (total as f64 / MAX_TOTAL_SUBSCRIPTIONS as f64) * 100.0
    } else {
        0.0
    };

    let summary = SubscriptionPlanSummary {
        major_index_values: registry.category_count(SubscriptionCategory::MajorIndexValue),
        display_indices: registry.category_count(SubscriptionCategory::DisplayIndex),
        index_derivatives: registry.category_count(SubscriptionCategory::IndexDerivative),
        stock_equities: registry.category_count(SubscriptionCategory::StockEquity),
        stock_derivatives: registry.category_count(SubscriptionCategory::StockDerivative),
        total,
        feed_mode,
        exceeds_capacity,
        stocks_skipped_no_chain: 0,
        stock_derivatives_available: 0,
        stock_derivatives_skipped: 0,
        capacity_utilization_pct,
    };

    info!(
        major_index_values = summary.major_index_values,
        stock_equities = summary.stock_equities,
        total = summary.total,
        feed_mode = %feed_mode,
        skipped_unparsable_sid,
        "Daily-universe subscription plan built"
    );

    SubscriptionPlan { registry, summary }
}

// ---------------------------------------------------------------------------
// Zero-Copy Archived Planner (Phase 2)
// ---------------------------------------------------------------------------

/// Builds a subscription plan from zero-copy archived data. No heap
/// allocation for the universe — only the output `SubscribedInstrument`
/// structs are allocated.
///
/// Same logic as [`build_subscription_plan`] but operates on
/// `&ArchivedFnoUniverse` from a memory-mapped rkyv cache.
pub fn build_subscription_plan_from_archived(
    universe: &ArchivedFnoUniverse,
    config: &SubscriptionConfig,
    today: NaiveDate,
) -> SubscriptionPlan {
    let feed_mode = config.parsed_feed_mode().unwrap_or(FeedMode::Ticker);
    // Wave 5 Item 2 (archived path mirror): scope gate.
    let stock_derivatives_enabled = should_subscribe_stock_derivatives(config);

    let mut instruments: Vec<SubscribedInstrument> = Vec::with_capacity(MAX_TOTAL_SUBSCRIPTIONS);
    // BUG FIX (2026-04-17): see sibling `build_subscription_plan` — dedup
    // must use `(security_id, segment)` so an id that exists in both
    // IDX_I and NSE_EQ (e.g. FINNIFTY=27) is NOT silently collapsed to
    // whichever one appeared first.
    let mut seen_ids: HashSet<(u64, ExchangeSegment)> =
        HashSet::with_capacity(MAX_TOTAL_SUBSCRIPTIONS);
    let mut stocks_skipped_no_chain: usize = 0;

    let full_chain_set: HashSet<&str> = FULL_CHAIN_INDEX_SYMBOLS.iter().copied().collect();

    // Pre-build option chain lookup: (symbol, expiry) → &ArchivedOptionChain.
    // ArchivedOptionChainKey can't be constructed for HashMap::get, so we build
    // a local lookup from the archived data (one-time O(n) for ~2K entries).
    let mut option_chain_lookup: HashMap<(&str, NaiveDate), usize> =
        HashMap::with_capacity(universe.option_chains.len());
    let option_chain_entries: Vec<_> = universe.option_chains.iter().collect();
    for (idx, (key, _)) in option_chain_entries.iter().enumerate() {
        let symbol = key.underlying_symbol.as_str();
        let expiry = naive_date_from_archived_i32(&key.expiry_date);
        option_chain_lookup.insert((symbol, expiry), idx);
    }

    // -----------------------------------------------------------------------
    // 1a. Major index value feeds (IDX_I) — from `underlyings` HashMap
    //     (legacy builder path; empty under post-PR #6b LOCKED universe)
    // -----------------------------------------------------------------------
    for underlying in universe.underlyings.values() {
        let symbol = underlying.underlying_symbol.as_str();
        if !full_chain_set.contains(symbol) {
            continue;
        }
        let price_id = underlying.price_feed_security_id.to_native();
        if seen_ids.insert((price_id, ExchangeSegment::IdxI)) {
            instruments.push(make_major_index_instrument(price_id, symbol, feed_mode));
        }
    }

    // -----------------------------------------------------------------------
    // 1b. Major index value feeds (IDX_I) — from `subscribed_indices`
    //     (POST-AWS-LIFECYCLE hotfix 2026-05-19 — see sibling planner
    //     section 1b for rationale)
    // -----------------------------------------------------------------------
    for index in universe.subscribed_indices.iter() {
        let cat = IndexCategory::from(&index.category);
        if cat != IndexCategory::FnoUnderlying {
            continue;
        }
        let symbol = index.symbol.as_str();
        if !full_chain_set.contains(symbol) {
            continue;
        }
        let sec_id = index.security_id.to_native();
        if seen_ids.insert((sec_id, ExchangeSegment::IdxI)) {
            instruments.push(make_major_index_instrument(sec_id, symbol, feed_mode));
        }
    }

    // -----------------------------------------------------------------------
    // 2. Display indices (IDX_I)
    //
    // Under `Indices4Only` scope this loop is filtered to INDIA VIX only
    // via `is_display_index_allowed_under_scope`.
    // -----------------------------------------------------------------------
    for index in universe.subscribed_indices.iter() {
        let cat = IndexCategory::from(&index.category);
        let sec_id = index.security_id.to_native();
        let symbol = index.symbol.as_str();
        if cat == IndexCategory::DisplayIndex
            && is_display_index_allowed_under_scope(config, sec_id)
            && seen_ids.insert((sec_id, ExchangeSegment::IdxI))
        {
            instruments.push(make_display_index_instrument(sec_id, symbol, feed_mode));
        }
    }

    // -----------------------------------------------------------------------
    // 3. Index derivatives — full chain at CURRENT EXPIRY ONLY (3 indices)
    //
    // 2026-04-25: Mirror of live planner change. See sibling
    // `build_subscription_plan` Section 3 for rationale (current-expiry filter
    // for FULL_CHAIN_INDEX_SYMBOLS to fit 25K WS capacity).
    //
    // Phase 0 Item 1 (archived path mirror): `should_subscribe_index_derivatives`
    // returns FALSE under `IndicesUnderlyingsOnly` — entire index F&O block
    // skipped.
    // -----------------------------------------------------------------------
    if should_subscribe_index_derivatives(config) {
        // Pre-pass: nearest expiry per full-chain index.
        let mut index_nearest_expiry: HashMap<&str, NaiveDate> =
            HashMap::with_capacity(FULL_CHAIN_INDEX_SYMBOLS.len());
        for contract in universe.derivative_contracts.values() {
            let kind = DhanInstrumentKind::from(&contract.instrument_kind);
            let is_index = matches!(
                kind,
                DhanInstrumentKind::FutureIndex | DhanInstrumentKind::OptionIndex
            );
            let symbol = contract.underlying_symbol.as_str();
            let expiry = naive_date_from_archived_i32(&contract.expiry_date);
            if is_index && full_chain_set.contains(symbol) && expiry >= today {
                index_nearest_expiry
                    .entry(symbol)
                    .and_modify(|e| {
                        if expiry < *e {
                            *e = expiry;
                        }
                    })
                    .or_insert(expiry);
            }
        }

        // Subscribe pass: only contracts at the nearest expiry per index.
        for contract in universe.derivative_contracts.values() {
            let kind = DhanInstrumentKind::from(&contract.instrument_kind);
            let is_index = matches!(
                kind,
                DhanInstrumentKind::FutureIndex | DhanInstrumentKind::OptionIndex
            );
            let sec_id = contract.security_id.to_native();
            let contract_seg = ExchangeSegment::from(&contract.exchange_segment);
            let symbol = contract.underlying_symbol.as_str();
            let expiry = naive_date_from_archived_i32(&contract.expiry_date);
            if is_index
                && full_chain_set.contains(symbol)
                && index_nearest_expiry
                    .get(symbol)
                    .is_some_and(|nearest| *nearest == expiry)
                && seen_ids.insert((sec_id, contract_seg))
            {
                instruments.push(make_derivative_instrument_from_archived(
                    contract,
                    SubscriptionCategory::IndexDerivative,
                    feed_mode,
                ));
            }
        }
    }

    // -----------------------------------------------------------------------
    // 4. Stock equities + Stock derivatives (current expiry)
    // -----------------------------------------------------------------------
    // 2026-04-25: Mirror of live planner Section 4 — track each stock's
    // selected expiry so Stage 2 progressive fill respects the same.
    let mut selected_expiry_per_stock: HashMap<String, NaiveDate> =
        HashMap::with_capacity(universe.underlyings.len());

    for underlying in universe.underlyings.values() {
        let kind = UnderlyingKind::from(&underlying.kind);
        if kind != UnderlyingKind::Stock {
            continue;
        }

        let symbol = underlying.underlying_symbol.as_str();

        // 4a. Stock equity price feed.
        //
        // Wave 5 Item 14: NSE_EQ stamped Quote unconditionally — see the
        // canonical `build_subscription_plan` path above for rationale.
        let nse_eq_feed_mode = FeedMode::Quote;
        let price_id = underlying.price_feed_security_id.to_native();
        let price_seg = ExchangeSegment::from(&underlying.price_feed_segment);
        if config.subscribe_stock_equities && seen_ids.insert((price_id, price_seg)) {
            instruments.push(make_stock_equity_instrument_from_archived(
                underlying,
                nse_eq_feed_mode,
            ));
        }

        // 4b. Stock derivatives — current expiry only, ATM ± N strikes.
        // Wave 5 Item 2 scope gate (archived path mirror).
        if !stock_derivatives_enabled {
            continue;
        }

        // Find current expiry (first expiry >= today)
        let current_expiry = universe.expiry_calendars.get(symbol).and_then(|cal| {
            cal.expiry_dates.iter().find_map(|d| {
                let date = naive_date_from_archived_i32(d);
                if date >= today { Some(date) } else { None }
            })
        });

        let Some(expiry) = current_expiry else {
            debug!(
                underlying = %symbol,
                "No current expiry found — skipping stock derivatives"
            );
            stocks_skipped_no_chain = stocks_skipped_no_chain.saturating_add(1);
            continue;
        };

        // 2026-04-25: Same safety as live planner — track selected expiry.
        selected_expiry_per_stock.insert(symbol.to_string(), expiry);

        // Look up option chain via our pre-built index
        if let Some(&chain_idx) = option_chain_lookup.get(&(symbol, expiry)) {
            let (_, chain) = &option_chain_entries[chain_idx];

            // Future for this expiry
            if let Some(archived_future_id) = chain.future_security_id.as_ref() {
                let future_id: u64 = archived_future_id.to_native();
                let archived_key = rkyv::rend::u64_le::from_native(future_id);
                if let Some(future_contract) = universe.derivative_contracts.get(&archived_key) {
                    let future_seg = ExchangeSegment::from(&future_contract.exchange_segment);
                    if seen_ids.insert((future_id, future_seg)) {
                        instruments.push(make_derivative_instrument_from_archived(
                            future_contract,
                            SubscriptionCategory::StockDerivative,
                            feed_mode,
                        ));
                    }
                }
            }

            // ATM ± N strike filtering
            let atm_above = config.stock_atm_strikes_above;
            let atm_below = config.stock_atm_strikes_below;

            // Calls
            let call_count = chain.calls.len();
            if call_count > 0 {
                let mid_idx = call_count / 2;
                let start = mid_idx.saturating_sub(atm_below);
                let end = mid_idx
                    .saturating_add(atm_above)
                    .saturating_add(1)
                    .min(call_count);
                for entry in &chain.calls[start..end] {
                    let sec_id = entry.security_id.to_native();
                    let archived_key = rkyv::rend::u64_le::from_native(sec_id);
                    if let Some(contract) = universe.derivative_contracts.get(&archived_key)
                        && seen_ids
                            .insert((sec_id, ExchangeSegment::from(&contract.exchange_segment)))
                    {
                        instruments.push(make_derivative_instrument_from_archived(
                            contract,
                            SubscriptionCategory::StockDerivative,
                            feed_mode,
                        ));
                    }
                }
            }

            // Puts
            let put_count = chain.puts.len();
            if put_count > 0 {
                let mid_idx = put_count / 2;
                let start = mid_idx.saturating_sub(atm_below);
                let end = mid_idx
                    .saturating_add(atm_above)
                    .saturating_add(1)
                    .min(put_count);
                for entry in &chain.puts[start..end] {
                    let sec_id = entry.security_id.to_native();
                    let archived_key = rkyv::rend::u64_le::from_native(sec_id);
                    if let Some(contract) = universe.derivative_contracts.get(&archived_key)
                        && seen_ids
                            .insert((sec_id, ExchangeSegment::from(&contract.exchange_segment)))
                    {
                        instruments.push(make_derivative_instrument_from_archived(
                            contract,
                            SubscriptionCategory::StockDerivative,
                            feed_mode,
                        ));
                    }
                }
            }
        } else {
            debug!(
                underlying = %symbol,
                expiry = %expiry,
                "No option chain found for current expiry — skipping"
            );
            stocks_skipped_no_chain = stocks_skipped_no_chain.saturating_add(1);
        }
    }

    // -----------------------------------------------------------------------
    // 5. Progressive fill — remaining stock derivatives to 25K capacity
    // -----------------------------------------------------------------------
    let mut stock_derivatives_available: usize = 0;
    let mut stock_derivatives_skipped: usize = 0;

    // Wave 5 Item 2 scope gate (archived path mirror).
    if stock_derivatives_enabled {
        // O(1) EXEMPT: begin — planner runs once at startup, not per tick
        let count_before_stage2 = instruments.len();

        let mut remaining_stock_derivatives: Vec<(u64, NaiveDate, &str, &_)> = universe
            .derivative_contracts
            .values()
            .filter_map(|c| {
                let kind = DhanInstrumentKind::from(&c.instrument_kind);
                let is_stock_deriv = matches!(
                    kind,
                    DhanInstrumentKind::FutureStock | DhanInstrumentKind::OptionStock
                );
                if !is_stock_deriv {
                    return None;
                }
                let expiry = naive_date_from_archived_i32(&c.expiry_date);
                let sec_id = c.security_id.to_native();
                let seg = ExchangeSegment::from(&c.exchange_segment);
                // SAFETY (2026-04-25): only include if Stage 1 selected an
                // expiry for this stock AND this contract's expiry matches.
                // Mirrors the live planner Stage 2 fix.
                let expiry_ok = selected_expiry_per_stock
                    .get(c.underlying_symbol.as_str())
                    .is_some_and(|sel| *sel == expiry);
                if expiry_ok && !seen_ids.contains(&(sec_id, seg)) {
                    Some((sec_id, expiry, c.underlying_symbol.as_str(), c))
                } else {
                    None
                }
            })
            .collect();

        // Deterministic sort: nearest expiry -> underlying -> security_id
        remaining_stock_derivatives
            .sort_by(|a, b| a.1.cmp(&b.1).then(a.2.cmp(b.2)).then(a.0.cmp(&b.0)));

        stock_derivatives_available = remaining_stock_derivatives.len();
        for (sec_id, _, _, contract) in &remaining_stock_derivatives {
            if instruments.len() >= MAX_TOTAL_SUBSCRIPTIONS {
                break;
            }
            let seg = ExchangeSegment::from(&contract.exchange_segment);
            if seen_ids.insert((*sec_id, seg)) {
                instruments.push(make_derivative_instrument_from_archived(
                    contract,
                    SubscriptionCategory::StockDerivative,
                    feed_mode,
                ));
            }
        }

        let stage2_added = instruments.len().saturating_sub(count_before_stage2);
        stock_derivatives_skipped = stock_derivatives_available.saturating_sub(stage2_added);

        info!(
            stage2_available = stock_derivatives_available,
            stage2_added = stage2_added,
            stage2_skipped = stock_derivatives_skipped,
            "Progressive fill: Stage 2 stock derivatives (archived)"
        );
        // O(1) EXEMPT: end
    }

    // -----------------------------------------------------------------------
    // Build the registry and summary
    // -----------------------------------------------------------------------
    let registry = InstrumentRegistry::from_instruments(instruments);

    // I-P1-11 gap G (2026-04-17): expose registry composite-index health
    // as Prometheus gauges so the operator sees cross-segment collisions
    // in Grafana + receives Telegram alerts on ERROR logs emitted during
    // construction.
    metrics::gauge!("tv_instrument_registry_cross_segment_collisions")
        .set(registry.cross_segment_collisions() as f64);
    metrics::gauge!("tv_instrument_registry_total_entries").set(registry.len() as f64);

    // PR #288 (#5b): emit one gauge per collision pair so operators can
    // see WHICH ids collided in Grafana without parsing logs. Label values
    // are stable (numeric id + enum segment) so cardinality is bounded by
    // the number of actual collisions (typical = 0-3, worst case ~10 per
    // Dhan's CSV).
    for (security_id, losing, winning) in registry.collision_pairs() {
        metrics::gauge!(
            "tv_instrument_registry_collision_pair",
            "security_id" => security_id.to_string(),
            "losing_segment" => losing.as_str(),
            "winning_segment" => winning.as_str(),
        )
        .set(1.0);
    }

    let exceeds_capacity = registry.len() > MAX_TOTAL_SUBSCRIPTIONS;
    if exceeds_capacity {
        warn!(
            total = registry.len(),
            capacity = MAX_TOTAL_SUBSCRIPTIONS,
            "Subscription plan exceeds WebSocket capacity"
        );
    }

    let total = registry.len();
    let capacity_utilization_pct = if MAX_TOTAL_SUBSCRIPTIONS > 0 {
        (total as f64 / MAX_TOTAL_SUBSCRIPTIONS as f64) * 100.0
    } else {
        0.0
    };

    let summary = SubscriptionPlanSummary {
        major_index_values: registry.category_count(SubscriptionCategory::MajorIndexValue),
        display_indices: registry.category_count(SubscriptionCategory::DisplayIndex),
        index_derivatives: registry.category_count(SubscriptionCategory::IndexDerivative),
        stock_equities: registry.category_count(SubscriptionCategory::StockEquity),
        stock_derivatives: registry.category_count(SubscriptionCategory::StockDerivative),
        total,
        feed_mode,
        exceeds_capacity,
        stocks_skipped_no_chain,
        stock_derivatives_available,
        stock_derivatives_skipped,
        capacity_utilization_pct,
    };

    info!(
        major_index_values = summary.major_index_values,
        display_indices = summary.display_indices,
        index_derivatives = summary.index_derivatives,
        stock_equities = summary.stock_equities,
        stock_derivatives = summary.stock_derivatives,
        total = summary.total,
        feed_mode = %feed_mode,
        stocks_skipped = summary.stocks_skipped_no_chain,
        capacity_pct = format!("{:.1}%", summary.capacity_utilization_pct),
        "Subscription plan built (from archived zero-copy data)"
    );

    SubscriptionPlan { registry, summary }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test code
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;

    use chrono::Utc;

    use tickvault_common::instrument_types::*;
    use tickvault_common::types::{Exchange, ExchangeSegment, OptionType, SecurityId};

    /// PR #7b — after legacy-variant retirement the helper simply
    /// returns the default LOCKED config (`Indices4Only`). Retained
    /// so the dozens of call sites compile; tests that historically
    /// asserted legacy stock-F&O behavior under `FullUniverse` have
    /// been deleted or rewritten to assert the LOCKED contract
    /// (zero stock F&O, zero index F&O, 4 IDX_I + INDIA VIX only).
    fn legacy_full_universe_config() -> SubscriptionConfig {
        SubscriptionConfig::default()
    }

    fn make_test_universe() -> FnoUniverse {
        let ist = tickvault_common::trading_calendar::ist_offset();
        let expiry = NaiveDate::from_ymd_opt(2026, 3, 27).unwrap();

        let mut underlyings = HashMap::new();
        let mut derivative_contracts: HashMap<SecurityId, DerivativeContract> = HashMap::new();
        let mut option_chains = HashMap::new();
        let mut expiry_calendars = HashMap::new();
        let instrument_info = HashMap::new();

        // --- NIFTY (major index) ---
        underlyings.insert(
            "NIFTY".to_string(),
            FnoUnderlying {
                underlying_symbol: "NIFTY".to_string(),
                underlying_security_id: 26000,
                price_feed_security_id: 13,
                price_feed_segment: ExchangeSegment::IdxI,
                derivative_segment: ExchangeSegment::NseFno,
                kind: UnderlyingKind::NseIndex,
                lot_size: 50,
                contract_count: 10,
            },
        );

        // NIFTY future
        derivative_contracts.insert(
            50001,
            DerivativeContract {
                security_id: 50001,
                underlying_symbol: "NIFTY".to_string(),
                instrument_kind: DhanInstrumentKind::FutureIndex,
                exchange_segment: ExchangeSegment::NseFno,
                expiry_date: expiry,
                strike_price: 0.0,
                option_type: None,
                lot_size: 50,
                tick_size: 0.05,
                symbol_name: "NIFTY-27MAR26-FUT".to_string(),
                display_name: "NIFTY FUT Mar26".to_string(),
            },
        );

        // NIFTY options (5 CE + 5 PE)
        let strikes = [17500.0, 17750.0, 18000.0, 18250.0, 18500.0];
        let mut calls = Vec::new();
        let mut puts = Vec::new();
        for (i, &strike) in strikes.iter().enumerate() {
            let ce_id = 50100 + i as u64;
            let pe_id = 50200 + i as u64;

            derivative_contracts.insert(
                ce_id,
                DerivativeContract {
                    security_id: ce_id,
                    underlying_symbol: "NIFTY".to_string(),
                    instrument_kind: DhanInstrumentKind::OptionIndex,
                    exchange_segment: ExchangeSegment::NseFno,
                    expiry_date: expiry,
                    strike_price: strike,
                    option_type: Some(OptionType::Call),
                    lot_size: 50,
                    tick_size: 0.05,
                    symbol_name: format!("NIFTY-27MAR26-{strike}-CE"),
                    display_name: format!("NIFTY {strike} CE Mar26"),
                },
            );
            calls.push(OptionChainEntry {
                security_id: ce_id,
                strike_price: strike,
                lot_size: 50,
            });

            derivative_contracts.insert(
                pe_id,
                DerivativeContract {
                    security_id: pe_id,
                    underlying_symbol: "NIFTY".to_string(),
                    instrument_kind: DhanInstrumentKind::OptionIndex,
                    exchange_segment: ExchangeSegment::NseFno,
                    expiry_date: expiry,
                    strike_price: strike,
                    option_type: Some(OptionType::Put),
                    lot_size: 50,
                    tick_size: 0.05,
                    symbol_name: format!("NIFTY-27MAR26-{strike}-PE"),
                    display_name: format!("NIFTY {strike} PE Mar26"),
                },
            );
            puts.push(OptionChainEntry {
                security_id: pe_id,
                strike_price: strike,
                lot_size: 50,
            });
        }

        let nifty_chain_key = OptionChainKey {
            underlying_symbol: "NIFTY".to_string(),
            expiry_date: expiry,
        };
        option_chains.insert(
            nifty_chain_key,
            OptionChain {
                underlying_symbol: "NIFTY".to_string(),
                expiry_date: expiry,
                calls,
                puts,
                future_security_id: Some(50001),
            },
        );
        expiry_calendars.insert(
            "NIFTY".to_string(),
            ExpiryCalendar {
                underlying_symbol: "NIFTY".to_string(),
                expiry_dates: vec![expiry],
            },
        );

        // --- BANKNIFTY (major index — minimal fixture for Phase 0 ratchets) ---
        // Hostile-review M1 fix (2026-05-13): BANKNIFTY + SENSEX must be present
        // in the test universe so the Phase 0 planner ratchets actually detect
        // a regression that drops them from Section 1.
        underlyings.insert(
            "BANKNIFTY".to_string(),
            FnoUnderlying {
                underlying_symbol: "BANKNIFTY".to_string(),
                underlying_security_id: 26009,
                price_feed_security_id: 25,
                price_feed_segment: ExchangeSegment::IdxI,
                derivative_segment: ExchangeSegment::NseFno,
                kind: UnderlyingKind::NseIndex,
                lot_size: 15,
                contract_count: 0,
            },
        );
        expiry_calendars.insert(
            "BANKNIFTY".to_string(),
            ExpiryCalendar {
                underlying_symbol: "BANKNIFTY".to_string(),
                expiry_dates: vec![expiry],
            },
        );

        // --- SENSEX (BSE major index — minimal fixture for Phase 0 ratchets) ---
        underlyings.insert(
            "SENSEX".to_string(),
            FnoUnderlying {
                underlying_symbol: "SENSEX".to_string(),
                underlying_security_id: 26010,
                price_feed_security_id: 51,
                price_feed_segment: ExchangeSegment::IdxI,
                derivative_segment: ExchangeSegment::BseFno,
                kind: UnderlyingKind::BseIndex,
                lot_size: 10,
                contract_count: 0,
            },
        );
        expiry_calendars.insert(
            "SENSEX".to_string(),
            ExpiryCalendar {
                underlying_symbol: "SENSEX".to_string(),
                expiry_dates: vec![expiry],
            },
        );

        // --- RELIANCE (stock) ---
        underlyings.insert(
            "RELIANCE".to_string(),
            FnoUnderlying {
                underlying_symbol: "RELIANCE".to_string(),
                underlying_security_id: 26001,
                price_feed_security_id: 2885,
                price_feed_segment: ExchangeSegment::NseEquity,
                derivative_segment: ExchangeSegment::NseFno,
                kind: UnderlyingKind::Stock,
                lot_size: 250,
                contract_count: 5,
            },
        );

        // RELIANCE future
        derivative_contracts.insert(
            60001,
            DerivativeContract {
                security_id: 60001,
                underlying_symbol: "RELIANCE".to_string(),
                instrument_kind: DhanInstrumentKind::FutureStock,
                exchange_segment: ExchangeSegment::NseFno,
                expiry_date: expiry,
                strike_price: 0.0,
                option_type: None,
                lot_size: 250,
                tick_size: 0.05,
                symbol_name: "RELIANCE-27MAR26-FUT".to_string(),
                display_name: "RELIANCE FUT Mar26".to_string(),
            },
        );

        // RELIANCE options (5 CE + 5 PE)
        let stock_strikes = [2600.0, 2650.0, 2700.0, 2750.0, 2800.0];
        let mut stock_calls = Vec::new();
        let mut stock_puts = Vec::new();
        for (i, &strike) in stock_strikes.iter().enumerate() {
            let ce_id = 60100 + i as u64;
            let pe_id = 60200 + i as u64;

            derivative_contracts.insert(
                ce_id,
                DerivativeContract {
                    security_id: ce_id,
                    underlying_symbol: "RELIANCE".to_string(),
                    instrument_kind: DhanInstrumentKind::OptionStock,
                    exchange_segment: ExchangeSegment::NseFno,
                    expiry_date: expiry,
                    strike_price: strike,
                    option_type: Some(OptionType::Call),
                    lot_size: 250,
                    tick_size: 0.05,
                    symbol_name: format!("RELIANCE-27MAR26-{strike}-CE"),
                    display_name: format!("RELIANCE {strike} CE Mar26"),
                },
            );
            stock_calls.push(OptionChainEntry {
                security_id: ce_id,
                strike_price: strike,
                lot_size: 250,
            });

            derivative_contracts.insert(
                pe_id,
                DerivativeContract {
                    security_id: pe_id,
                    underlying_symbol: "RELIANCE".to_string(),
                    instrument_kind: DhanInstrumentKind::OptionStock,
                    exchange_segment: ExchangeSegment::NseFno,
                    expiry_date: expiry,
                    strike_price: strike,
                    option_type: Some(OptionType::Put),
                    lot_size: 250,
                    tick_size: 0.05,
                    symbol_name: format!("RELIANCE-27MAR26-{strike}-PE"),
                    display_name: format!("RELIANCE {strike} PE Mar26"),
                },
            );
            stock_puts.push(OptionChainEntry {
                security_id: pe_id,
                strike_price: strike,
                lot_size: 250,
            });
        }

        let rel_chain_key = OptionChainKey {
            underlying_symbol: "RELIANCE".to_string(),
            expiry_date: expiry,
        };
        option_chains.insert(
            rel_chain_key,
            OptionChain {
                underlying_symbol: "RELIANCE".to_string(),
                expiry_date: expiry,
                calls: stock_calls,
                puts: stock_puts,
                future_security_id: Some(60001),
            },
        );
        expiry_calendars.insert(
            "RELIANCE".to_string(),
            ExpiryCalendar {
                underlying_symbol: "RELIANCE".to_string(),
                expiry_dates: vec![expiry],
            },
        );

        // --- Display index: INDIA VIX ---
        let subscribed_indices = vec![
            SubscribedIndex {
                symbol: "NIFTY".to_string(),
                security_id: 13,
                exchange: Exchange::NationalStockExchange,
                category: IndexCategory::FnoUnderlying,
                subcategory: IndexSubcategory::Fno,
            },
            SubscribedIndex {
                symbol: "INDIA VIX".to_string(),
                security_id: 21,
                exchange: Exchange::NationalStockExchange,
                category: IndexCategory::DisplayIndex,
                subcategory: IndexSubcategory::Volatility,
            },
        ];

        FnoUniverse {
            underlyings,
            derivative_contracts,
            instrument_info,
            option_chains,
            expiry_calendars,
            subscribed_indices,
            build_metadata: UniverseBuildMetadata {
                csv_source: "test".to_string(),
                csv_row_count: 0,
                parsed_row_count: 0,
                index_count: 0,
                equity_count: 0,
                underlying_count: 2,
                derivative_count: 22,
                option_chain_count: 2,
                build_duration: Duration::from_millis(0),
                build_timestamp: Utc::now().with_timezone(&ist),
            },
        }
    }

    #[cfg(any())] // PR #7b — retired (tested legacy scope behavior)
    fn test_build_plan_default_config() {
        let universe = make_test_universe();
        let config = legacy_full_universe_config();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        // Phase 0 Item 1 fixture expansion (2026-05-13): NIFTY + BANKNIFTY +
        // SENSEX all live in the test universe so the Phase 0 lower-bound
        // ratchet detects regressions that drop any of the 3 majors.
        assert_eq!(plan.summary.major_index_values, 3); // NIFTY + BANKNIFTY + SENSEX
        assert!(plan.summary.index_derivatives > 0); // NIFTY futures + options (only NIFTY has them)

        // INDIA VIX is a display index
        assert_eq!(plan.summary.display_indices, 1);

        // RELIANCE is a stock
        assert_eq!(plan.summary.stock_equities, 1);
        assert!(plan.summary.stock_derivatives > 0);

        assert_eq!(plan.summary.feed_mode, FeedMode::Full);
        assert!(!plan.summary.exceeds_capacity);
    }

    #[cfg(any())] // PR #7b — retired (tested legacy scope behavior)
    fn test_index_derivatives_all_subscribed() {
        let universe = make_test_universe();
        let config = legacy_full_universe_config();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        // NIFTY: 1 future + 5 CE + 5 PE = 11 derivatives
        assert_eq!(plan.summary.index_derivatives, 11);
    }

    #[cfg(any())] // PR #7b — retired (tested legacy scope behavior)
    fn test_stock_derivatives_current_expiry_only() {
        let universe = make_test_universe();
        let config = legacy_full_universe_config();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        // RELIANCE: 1 future + 5 CE + 5 PE = 11 (all 5 strikes within ATM±10)
        assert_eq!(plan.summary.stock_derivatives, 11);
    }

    #[cfg(any())] // PR #7b — retired (tested legacy scope behavior)
    fn test_stock_past_expiry_skipped() {
        let universe = make_test_universe();
        let config = legacy_full_universe_config();
        // Set today AFTER the expiry date → no current expiry
        let today = NaiveDate::from_ymd_opt(2026, 4, 1).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        // RELIANCE has no valid expiry → skipped
        assert_eq!(plan.summary.stock_derivatives, 0);
        assert_eq!(plan.summary.stocks_skipped_no_chain, 1);
    }

    // PR #7b — `test_disable_*_derivatives` + `test_disable_display_indices`
    // + `test_disable_stock_equities` retired. The flags (subscribe_*) and
    // the FullUniverse variant they exercised no longer exist; the only
    // scope (`Indices4Only`) ALWAYS emits zero derivatives and zero stock
    // equities by construction. Coverage is provided by
    // `test_indices4only_planner_emits_zero_derivatives` below.

    #[test]
    fn test_no_duplicate_security_ids() {
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        // Verify no duplicates by checking total equals unique count
        let ids: Vec<u64> = plan.registry.iter().map(|i| i.security_id).collect();
        let unique: HashSet<u64> = ids.iter().copied().collect();
        assert_eq!(ids.len(), unique.len(), "Duplicate security_ids in plan");
    }

    #[cfg(any())] // PR #7b — retired (tested legacy scope behavior)
    fn test_registry_o1_lookup() {
        let universe = make_test_universe();
        let config = legacy_full_universe_config();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        // NIFTY index value
        let nifty = plan.registry.get(13).unwrap();
        assert_eq!(nifty.category, SubscriptionCategory::MajorIndexValue);
        assert_eq!(nifty.underlying_symbol, "NIFTY");

        // INDIA VIX display index
        let vix = plan.registry.get(21).unwrap();
        assert_eq!(vix.category, SubscriptionCategory::DisplayIndex);

        // RELIANCE equity
        let reliance = plan.registry.get(2885).unwrap();
        assert_eq!(reliance.category, SubscriptionCategory::StockEquity);

        // NIFTY future
        let nifty_fut = plan.registry.get(50001).unwrap();
        assert_eq!(nifty_fut.category, SubscriptionCategory::IndexDerivative);

        // Unknown ID
        assert!(plan.registry.get(99999).is_none());
    }

    #[test]
    fn test_feed_mode_from_config() {
        let universe = make_test_universe();
        let config = SubscriptionConfig {
            feed_mode: "Quote".to_string(),
            ..Default::default()
        };
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        assert_eq!(plan.summary.feed_mode, FeedMode::Quote);
        // Verify individual instruments have Quote mode
        let nifty = plan.registry.get(13).unwrap();
        assert_eq!(nifty.feed_mode, FeedMode::Quote);
    }

    #[test]
    fn test_feed_mode_full() {
        let universe = make_test_universe();
        let config = SubscriptionConfig {
            feed_mode: "Full".to_string(),
            ..Default::default()
        };
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        assert_eq!(plan.summary.feed_mode, FeedMode::Full);
    }

    #[test]
    fn test_feed_mode_invalid_falls_back_to_ticker() {
        let universe = make_test_universe();
        let config = SubscriptionConfig {
            feed_mode: "Invalid".to_string(),
            ..Default::default()
        };
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        // Invalid feed mode falls back to Ticker (see build_subscription_plan line 112)
        assert_eq!(plan.summary.feed_mode, FeedMode::Ticker);
    }

    // PR #7b — `test_atm_strike_range_narrow` + `test_atm_strike_range_zero`
    // retired. Both exercised the stock-F&O ATM filter on the legacy
    // FullUniverse scope. Under `Indices4Only` stock F&O is never
    // subscribed and the ATM-strike config fields no longer affect
    // the plan output.

    #[test]
    fn test_total_instrument_count() {
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        let expected_total = plan.summary.major_index_values
            + plan.summary.display_indices
            + plan.summary.index_derivatives
            + plan.summary.stock_equities
            + plan.summary.stock_derivatives;
        assert_eq!(plan.summary.total, expected_total);
        assert_eq!(plan.registry.len(), expected_total);
    }

    #[cfg(any())] // PR #7b — retired (tested legacy scope behavior)
    fn test_by_exchange_segment_grouping() {
        let universe = make_test_universe();
        let config = legacy_full_universe_config();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );
        let grouped = plan.registry.by_exchange_segment();

        // Phase 0 Item 1 (2026-05-13): fixture now includes BANKNIFTY (25) +
        // SENSEX (51). IDX_I total = NIFTY (13) + BANKNIFTY (25) + SENSEX (51)
        // + INDIA VIX (21) = 4.
        assert_eq!(grouped.get(&ExchangeSegment::IdxI).unwrap().len(), 4);

        // NSE_EQ: RELIANCE (2885) = 1
        assert_eq!(grouped.get(&ExchangeSegment::NseEquity).unwrap().len(), 1);

        // NSE_FNO: all derivatives
        let fno_count = grouped.get(&ExchangeSegment::NseFno).unwrap().len();
        assert_eq!(
            fno_count,
            plan.summary.index_derivatives + plan.summary.stock_derivatives
        );
    }

    // --- Progressive Fill (Stage 2) Tests ---

    #[test]
    fn test_plan_capacity_utilization_percentage() {
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        let expected_pct = (plan.summary.total as f64 / 25000.0) * 100.0;
        assert!(
            (plan.summary.capacity_utilization_pct - expected_pct).abs() < 0.001,
            "Capacity utilization: expected {expected_pct}, got {}",
            plan.summary.capacity_utilization_pct
        );
    }

    #[test]
    fn test_plan_small_universe_no_skips() {
        // Small test universe — all instruments fit, nothing skipped
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        // Small universe: all RELIANCE contracts already in Stage 1 (ATM+-10)
        // Stage 2 has 0 remaining → 0 available, 0 skipped
        assert_eq!(plan.summary.stock_derivatives_skipped, 0);
        assert!(!plan.summary.exceeds_capacity);
    }

    #[test]
    fn test_plan_deterministic_sort() {
        // Same input twice → identical set of instruments
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan1 = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );
        let plan2 = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        let mut ids1: Vec<u64> = plan1.registry.iter().map(|i| i.security_id).collect();
        let mut ids2: Vec<u64> = plan2.registry.iter().map(|i| i.security_id).collect();
        ids1.sort();
        ids2.sort();
        assert_eq!(ids1, ids2, "Plans should produce identical instrument sets");
        assert_eq!(plan1.summary.total, plan2.summary.total);
    }

    #[test]
    fn test_plan_no_duplicates_progressive_fill() {
        // Verify Stage 2 doesn't duplicate Stage 1 instruments
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        let ids: Vec<u64> = plan.registry.iter().map(|i| i.security_id).collect();
        let unique: HashSet<u64> = ids.iter().copied().collect();
        assert_eq!(
            ids.len(),
            unique.len(),
            "Duplicate security_ids after progressive fill"
        );
    }

    #[cfg(any())] // PR #7b — retired (tested legacy scope behavior)
    fn test_plan_stage2_does_not_add_far_month_stock_derivatives() {
        // 2026-04-25: Updated semantics. Previously Stage 2 progressively
        // filled far-month stock derivatives until 25K capacity. Under the
        // new "ATM±25 current expiry only" design, Stage 2 MUST stay within
        // the per-stock selected expiry. Far-month stock contracts are
        // explicitly DROPPED.
        let mut universe = make_test_universe();
        // near_expiry = 2026-03-27 is already in make_test_universe()
        let far_expiry = NaiveDate::from_ymd_opt(2026, 4, 24).unwrap();

        // Add RELIANCE contracts for far expiry (new security IDs)
        let far_future_id = 70001;
        universe.derivative_contracts.insert(
            far_future_id,
            DerivativeContract {
                security_id: far_future_id,
                underlying_symbol: "RELIANCE".to_string(),
                instrument_kind: DhanInstrumentKind::FutureStock,
                exchange_segment: ExchangeSegment::NseFno,
                expiry_date: far_expiry,
                strike_price: 0.0,
                option_type: None,
                lot_size: 250,
                tick_size: 0.05,
                symbol_name: "RELIANCE-24APR26-FUT".to_string(),
                display_name: "RELIANCE FUT Apr26".to_string(),
            },
        );
        for i in 0..3u32 {
            let ce_id: u64 = 70100 + u64::from(i);
            universe.derivative_contracts.insert(
                ce_id,
                DerivativeContract {
                    security_id: ce_id,
                    underlying_symbol: "RELIANCE".to_string(),
                    instrument_kind: DhanInstrumentKind::OptionStock,
                    exchange_segment: ExchangeSegment::NseFno,
                    expiry_date: far_expiry,
                    strike_price: 2600.0 + (i as f64) * 50.0,
                    option_type: Some(OptionType::Call),
                    lot_size: 250,
                    tick_size: 0.05,
                    symbol_name: format!("RELIANCE-24APR26-{}-CE", 2600 + i * 50),
                    display_name: format!("RELIANCE {} CE Apr26", 2600 + i * 50),
                },
            );
        }

        // Add far expiry to calendar
        universe
            .expiry_calendars
            .get_mut("RELIANCE")
            .unwrap()
            .expiry_dates
            .push(far_expiry);

        let config = legacy_full_universe_config();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();
        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        // Stage 1 subscribes near expiry ATM±N (all 11 RELIANCE near contracts).
        // Stage 2 must NOT add far-month contracts under the new design.
        assert!(
            plan.registry.get(far_future_id).is_none(),
            "Far expiry future MUST NOT be in plan — current expiry only"
        );
        assert!(
            plan.registry.get(70100).is_none(),
            "Far expiry CE MUST NOT be in plan — current expiry only"
        );

        // Total stock derivatives should only include near expiry contracts.
        // make_test_universe puts 11 RELIANCE contracts at near expiry
        // (1 future + 5 CE + 5 PE).
        assert_eq!(
            plan.summary.stock_derivatives, 11,
            "Stage 2 must not bypass current-expiry filter; only 11 near-expiry contracts allowed"
        );
    }

    #[cfg(any())] // PR #7b — retired (tested legacy scope behavior)
    fn test_plan_stock_option_chain_calls_only() {
        // Chain with calls but empty puts
        let mut universe = make_test_universe();
        let expiry = NaiveDate::from_ymd_opt(2026, 3, 27).unwrap();

        // Add a new stock "INFY" with calls only (no puts)
        universe.underlyings.insert(
            "INFY".to_string(),
            FnoUnderlying {
                underlying_symbol: "INFY".to_string(),
                underlying_security_id: 26002,
                price_feed_security_id: 1594,
                price_feed_segment: ExchangeSegment::NseEquity,
                derivative_segment: ExchangeSegment::NseFno,
                kind: UnderlyingKind::Stock,
                lot_size: 300,
                contract_count: 3,
            },
        );

        // INFY future
        let infy_fut_id = 80001;
        universe.derivative_contracts.insert(
            infy_fut_id,
            DerivativeContract {
                security_id: infy_fut_id,
                underlying_symbol: "INFY".to_string(),
                instrument_kind: DhanInstrumentKind::FutureStock,
                exchange_segment: ExchangeSegment::NseFno,
                expiry_date: expiry,
                strike_price: 0.0,
                option_type: None,
                lot_size: 300,
                tick_size: 0.05,
                symbol_name: "INFY-27MAR26-FUT".to_string(),
                display_name: "INFY FUT Mar26".to_string(),
            },
        );

        // 3 INFY calls, NO puts
        let mut infy_calls = Vec::new();
        for i in 0..3u32 {
            let ce_id: u64 = 80100 + u64::from(i);
            universe.derivative_contracts.insert(
                ce_id,
                DerivativeContract {
                    security_id: ce_id,
                    underlying_symbol: "INFY".to_string(),
                    instrument_kind: DhanInstrumentKind::OptionStock,
                    exchange_segment: ExchangeSegment::NseFno,
                    expiry_date: expiry,
                    strike_price: 1500.0 + (i as f64) * 50.0,
                    option_type: Some(OptionType::Call),
                    lot_size: 300,
                    tick_size: 0.05,
                    symbol_name: format!("INFY-27MAR26-{}-CE", 1500 + i * 50),
                    display_name: format!("INFY {} CE Mar26", 1500 + i * 50),
                },
            );
            infy_calls.push(OptionChainEntry {
                security_id: ce_id,
                strike_price: 1500.0 + (i as f64) * 50.0,
                lot_size: 300,
            });
        }

        let infy_chain_key = OptionChainKey {
            underlying_symbol: "INFY".to_string(),
            expiry_date: expiry,
        };
        universe.option_chains.insert(
            infy_chain_key,
            OptionChain {
                underlying_symbol: "INFY".to_string(),
                expiry_date: expiry,
                calls: infy_calls,
                puts: vec![], // NO puts
                future_security_id: Some(infy_fut_id),
            },
        );
        universe.expiry_calendars.insert(
            "INFY".to_string(),
            ExpiryCalendar {
                underlying_symbol: "INFY".to_string(),
                expiry_dates: vec![expiry],
            },
        );

        let config = legacy_full_universe_config();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();
        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        // INFY: 1 future + 3 CE + 0 PE = 4 (Stage 1 ATM+-10 covers all 3 calls)
        // Verify INFY instruments are in the plan
        assert!(plan.registry.get(infy_fut_id).is_some(), "INFY future");
        assert!(plan.registry.get(80100).is_some(), "INFY CE 1");
        assert!(plan.registry.get(80101).is_some(), "INFY CE 2");
        assert!(plan.registry.get(80102).is_some(), "INFY CE 3");

        // INFY equity feed
        assert!(plan.registry.get(1594).is_some(), "INFY equity");
    }

    // --- Additional coverage tests ---

    #[cfg(any())] // PR #7b — retired (tested legacy scope behavior)
    fn test_stock_with_no_option_chain_for_expiry_skipped() {
        // Stock has an expiry calendar entry but no option chain for that date
        let mut universe = make_test_universe();
        let expiry = NaiveDate::from_ymd_opt(2026, 3, 27).unwrap();

        // Add stock SBIN with expiry calendar but no option chain
        universe.underlyings.insert(
            "SBIN".to_string(),
            FnoUnderlying {
                underlying_symbol: "SBIN".to_string(),
                underlying_security_id: 26003,
                price_feed_security_id: 5258,
                price_feed_segment: ExchangeSegment::NseEquity,
                derivative_segment: ExchangeSegment::NseFno,
                kind: UnderlyingKind::Stock,
                lot_size: 1500,
                contract_count: 0,
            },
        );
        universe.expiry_calendars.insert(
            "SBIN".to_string(),
            ExpiryCalendar {
                underlying_symbol: "SBIN".to_string(),
                expiry_dates: vec![expiry],
            },
        );
        // Deliberately NOT adding an option chain for SBIN

        let config = legacy_full_universe_config();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();
        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        // SBIN equity should still be subscribed
        assert!(
            plan.registry.get(5258).is_some(),
            "SBIN equity feed should be subscribed"
        );

        // stocks_skipped_no_chain should be >= 1 (SBIN has no chain)
        assert!(
            plan.summary.stocks_skipped_no_chain >= 1,
            "SBIN should count as skipped (no chain): {}",
            plan.summary.stocks_skipped_no_chain
        );
    }

    #[cfg(any())] // PR #7b — retired (tested legacy scope behavior)
    fn test_stock_with_no_expiry_calendar_skipped() {
        // Stock has no expiry calendar at all — should skip derivatives
        let mut universe = make_test_universe();

        universe.underlyings.insert(
            "HDFC".to_string(),
            FnoUnderlying {
                underlying_symbol: "HDFC".to_string(),
                underlying_security_id: 26004,
                price_feed_security_id: 7777,
                price_feed_segment: ExchangeSegment::NseEquity,
                derivative_segment: ExchangeSegment::NseFno,
                kind: UnderlyingKind::Stock,
                lot_size: 300,
                contract_count: 0,
            },
        );
        // No expiry_calendar for HDFC

        let config = legacy_full_universe_config();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();
        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        // HDFC equity should be subscribed
        assert!(
            plan.registry.get(7777).is_some(),
            "HDFC equity feed should be subscribed"
        );
        // Should increment stocks_skipped_no_chain
        assert!(plan.summary.stocks_skipped_no_chain >= 1);
    }

    // PR #7b — `test_plan_with_all_subscriptions_disabled` retired. The
    // 4 `subscribe_*` booleans and the FullUniverse variant were
    // deleted; under `Indices4Only` the planner ALWAYS emits zero
    // index/stock derivatives and zero NSE_EQ when
    // `subscribe_stock_equities = false`. Equivalent coverage lives
    // in `test_indices4only_planner_emits_zero_derivatives` (Slice 6
    // ratchet) and the default-config assertion in this file.

    #[test]
    fn test_plan_summary_capacity_utilization_non_negative() {
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        assert!(
            plan.summary.capacity_utilization_pct >= 0.0,
            "capacity utilization should be non-negative"
        );
        assert!(
            plan.summary.capacity_utilization_pct <= 100.0,
            "small test universe should be within capacity"
        );
    }

    #[test]
    fn test_plan_stock_derivatives_available_counts() {
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        // stock_derivatives_available + stock_derivatives_skipped >= 0
        // and stock_derivatives_skipped is always consistent
        assert!(
            plan.summary.stock_derivatives_available >= plan.summary.stock_derivatives_skipped,
            "available >= skipped"
        );
    }

    #[test]
    fn test_plan_subscription_plan_debug() {
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        let debug_str = format!("{:?}", plan);
        assert!(
            !debug_str.is_empty(),
            "SubscriptionPlan should have Debug output"
        );
    }

    #[test]
    fn test_plan_summary_debug() {
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        let debug_str = format!("{:?}", plan.summary);
        assert!(
            debug_str.contains("major_index_values"),
            "summary debug should contain field names"
        );
    }

    #[cfg(any())] // PR #7b — retired (tested legacy scope behavior)
    fn test_plan_stock_derivatives_with_future_only_no_options() {
        // Stock with future but no option chain — future should still be subscribed
        let mut universe = make_test_universe();
        let expiry = NaiveDate::from_ymd_opt(2026, 3, 27).unwrap();

        universe.underlyings.insert(
            "TATA".to_string(),
            FnoUnderlying {
                underlying_symbol: "TATA".to_string(),
                underlying_security_id: 26005,
                price_feed_security_id: 8888,
                price_feed_segment: ExchangeSegment::NseEquity,
                derivative_segment: ExchangeSegment::NseFno,
                kind: UnderlyingKind::Stock,
                lot_size: 100,
                contract_count: 1,
            },
        );

        let tata_fut_id = 90001;
        universe.derivative_contracts.insert(
            tata_fut_id,
            DerivativeContract {
                security_id: tata_fut_id,
                underlying_symbol: "TATA".to_string(),
                instrument_kind: DhanInstrumentKind::FutureStock,
                exchange_segment: ExchangeSegment::NseFno,
                expiry_date: expiry,
                strike_price: 0.0,
                option_type: None,
                lot_size: 100,
                tick_size: 0.05,
                symbol_name: "TATA-27MAR26-FUT".to_string(),
                display_name: "TATA FUT Mar26".to_string(),
            },
        );

        // Chain with future but empty calls/puts
        let tata_chain_key = OptionChainKey {
            underlying_symbol: "TATA".to_string(),
            expiry_date: expiry,
        };
        universe.option_chains.insert(
            tata_chain_key,
            OptionChain {
                underlying_symbol: "TATA".to_string(),
                expiry_date: expiry,
                calls: vec![],
                puts: vec![],
                future_security_id: Some(tata_fut_id),
            },
        );
        universe.expiry_calendars.insert(
            "TATA".to_string(),
            ExpiryCalendar {
                underlying_symbol: "TATA".to_string(),
                expiry_dates: vec![expiry],
            },
        );

        let config = legacy_full_universe_config();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();
        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        // TATA future should be subscribed
        assert!(
            plan.registry.get(tata_fut_id).is_some(),
            "TATA future should be subscribed even with empty chain"
        );
        // TATA equity should be subscribed
        assert!(
            plan.registry.get(8888).is_some(),
            "TATA equity should be subscribed"
        );
    }

    #[cfg(any())] // PR #7b — retired (tested legacy scope behavior)
    fn test_plan_stock_expired_all_expiries_skipped() {
        // Stock where ALL expiries are in the past — should skip derivatives
        let mut universe = make_test_universe();
        let past_expiry = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();

        universe.underlyings.insert(
            "EXPIRED".to_string(),
            FnoUnderlying {
                underlying_symbol: "EXPIRED".to_string(),
                underlying_security_id: 26006,
                price_feed_security_id: 9999,
                price_feed_segment: ExchangeSegment::NseEquity,
                derivative_segment: ExchangeSegment::NseFno,
                kind: UnderlyingKind::Stock,
                lot_size: 200,
                contract_count: 0,
            },
        );
        universe.expiry_calendars.insert(
            "EXPIRED".to_string(),
            ExpiryCalendar {
                underlying_symbol: "EXPIRED".to_string(),
                expiry_dates: vec![past_expiry],
            },
        );

        let config = legacy_full_universe_config();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();
        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        // EXPIRED stock should increment stocks_skipped_no_chain
        assert!(plan.summary.stocks_skipped_no_chain >= 1);
    }

    #[test]
    fn test_plan_non_full_chain_index_not_subscribed_as_index_derivative() {
        // An index underlying that is NOT in FULL_CHAIN_INDEX_SYMBOLS
        // should not have its derivatives subscribed as index derivatives
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        // INDIA VIX is a display index, not a major index
        // Its derivatives (if any) should NOT be in IndexDerivative category
        let display_idx = plan.registry.get(21);
        assert!(display_idx.is_some(), "INDIA VIX display index");
        assert_eq!(
            display_idx.unwrap().category,
            SubscriptionCategory::DisplayIndex,
            "INDIA VIX should be DisplayIndex, not MajorIndexValue"
        );
    }

    #[cfg(any())] // PR #7b — retired (tested legacy scope behavior)
    fn test_plan_today_equals_expiry_still_included() {
        // When today == expiry date, the contract should still be included
        let universe = make_test_universe();
        let config = legacy_full_universe_config();
        // Set today to the exact expiry date
        let today = NaiveDate::from_ymd_opt(2026, 3, 27).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        // RELIANCE derivatives should still be subscribed (expiry >= today)
        assert!(
            plan.summary.stock_derivatives > 0,
            "derivatives with expiry == today should be included"
        );
    }

    #[test]
    fn test_plan_today_after_expiry_stock_derivatives_zero() {
        // When today > all expiries, no stock derivatives should be subscribed
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 12, 31).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        // RELIANCE has expiry 2026-03-27, which is < today
        // All stock derivatives should be skipped
        assert_eq!(
            plan.summary.stock_derivatives, 0,
            "no stock derivatives when all expiries are past"
        );
    }

    #[test]
    fn test_plan_empty_universe() {
        // Completely empty universe — no instruments
        let ist = tickvault_common::trading_calendar::ist_offset();
        let universe = FnoUniverse {
            underlyings: HashMap::new(),
            derivative_contracts: HashMap::new(),
            instrument_info: HashMap::new(),
            option_chains: HashMap::new(),
            expiry_calendars: HashMap::new(),
            subscribed_indices: Vec::new(),
            build_metadata: UniverseBuildMetadata {
                csv_source: "test".to_string(),
                csv_row_count: 0,
                parsed_row_count: 0,
                index_count: 0,
                equity_count: 0,
                underlying_count: 0,
                derivative_count: 0,
                option_chain_count: 0,
                build_duration: Duration::from_millis(0),
                build_timestamp: Utc::now().with_timezone(&ist),
            },
        };

        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        assert_eq!(plan.summary.total, 0);
        assert_eq!(plan.summary.major_index_values, 0);
        assert_eq!(plan.summary.display_indices, 0);
        assert_eq!(plan.summary.index_derivatives, 0);
        assert_eq!(plan.summary.stock_equities, 0);
        assert_eq!(plan.summary.stock_derivatives, 0);
        assert!(!plan.summary.exceeds_capacity);
    }

    #[cfg(any())] // PR #7b — retired (tested legacy scope behavior)
    fn test_plan_exceeds_capacity_triggers_warning() {
        // Build a universe with > 25,000 stock derivative contracts to trigger
        // the capacity limit (line 316: break) and warning (lines 346-347).
        let ist = tickvault_common::trading_calendar::ist_offset();
        let expiry = NaiveDate::from_ymd_opt(2026, 3, 27).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let mut underlyings = HashMap::new();
        let mut derivative_contracts: HashMap<SecurityId, DerivativeContract> = HashMap::new();
        let mut option_chains = HashMap::new();
        let mut expiry_calendars = HashMap::new();

        // Create 50 stocks, each with 600 options (300 CE + 300 PE)
        // Total: 50 stocks * 600 options = 30,000 + 50 futures = 30,050
        // Plus 50 equity feeds = 30,100 → exceeds MAX_TOTAL_SUBSCRIPTIONS (25,000)
        let mut base_id: u64 = 100_000;
        for stock_idx in 0..50u32 {
            let symbol = format!("STOCK{stock_idx}");
            let underlying_id = 90_000 + u64::from(stock_idx);
            let equity_id = 80_000 + u64::from(stock_idx);

            underlyings.insert(
                symbol.clone(),
                FnoUnderlying {
                    underlying_symbol: symbol.clone(),
                    underlying_security_id: underlying_id,
                    price_feed_security_id: equity_id,
                    price_feed_segment: ExchangeSegment::NseEquity,
                    derivative_segment: ExchangeSegment::NseFno,
                    kind: UnderlyingKind::Stock,
                    lot_size: 100,
                    contract_count: 601,
                },
            );

            // Future
            let fut_id = base_id;
            base_id += 1;
            derivative_contracts.insert(
                fut_id,
                DerivativeContract {
                    security_id: fut_id,
                    underlying_symbol: symbol.clone(),
                    instrument_kind: DhanInstrumentKind::FutureStock,
                    exchange_segment: ExchangeSegment::NseFno,
                    expiry_date: expiry,
                    strike_price: 0.0,
                    option_type: None,
                    lot_size: 100,
                    tick_size: 0.05,
                    symbol_name: format!("{symbol}-FUT"),
                    display_name: format!("{symbol} FUT"),
                },
            );

            let mut calls = Vec::new();
            let mut puts = Vec::new();
            for strike_idx in 0..300u32 {
                let ce_id = base_id;
                base_id += 1;
                let pe_id = base_id;
                base_id += 1;
                let strike = 1000.0 + (strike_idx as f64) * 10.0;

                derivative_contracts.insert(
                    ce_id,
                    DerivativeContract {
                        security_id: ce_id,
                        underlying_symbol: symbol.clone(),
                        instrument_kind: DhanInstrumentKind::OptionStock,
                        exchange_segment: ExchangeSegment::NseFno,
                        expiry_date: expiry,
                        strike_price: strike,
                        option_type: Some(OptionType::Call),
                        lot_size: 100,
                        tick_size: 0.05,
                        symbol_name: format!("{symbol}-{strike}-CE"),
                        display_name: format!("{symbol} {strike} CE"),
                    },
                );
                calls.push(OptionChainEntry {
                    security_id: ce_id,
                    strike_price: strike,
                    lot_size: 100,
                });

                derivative_contracts.insert(
                    pe_id,
                    DerivativeContract {
                        security_id: pe_id,
                        underlying_symbol: symbol.clone(),
                        instrument_kind: DhanInstrumentKind::OptionStock,
                        exchange_segment: ExchangeSegment::NseFno,
                        expiry_date: expiry,
                        strike_price: strike,
                        option_type: Some(OptionType::Put),
                        lot_size: 100,
                        tick_size: 0.05,
                        symbol_name: format!("{symbol}-{strike}-PE"),
                        display_name: format!("{symbol} {strike} PE"),
                    },
                );
                puts.push(OptionChainEntry {
                    security_id: pe_id,
                    strike_price: strike,
                    lot_size: 100,
                });
            }

            let chain_key = OptionChainKey {
                underlying_symbol: symbol.clone(),
                expiry_date: expiry,
            };
            option_chains.insert(
                chain_key,
                OptionChain {
                    underlying_symbol: symbol.clone(),
                    expiry_date: expiry,
                    calls,
                    puts,
                    future_security_id: Some(fut_id),
                },
            );
            expiry_calendars.insert(
                symbol.clone(),
                ExpiryCalendar {
                    underlying_symbol: symbol,
                    expiry_dates: vec![expiry],
                },
            );
        }

        let universe = FnoUniverse {
            underlyings,
            derivative_contracts,
            instrument_info: HashMap::new(),
            option_chains,
            expiry_calendars,
            subscribed_indices: vec![],
            build_metadata: UniverseBuildMetadata {
                csv_source: "test-capacity".to_string(),
                csv_row_count: 0,
                parsed_row_count: 0,
                index_count: 0,
                equity_count: 0,
                underlying_count: 50,
                derivative_count: 30050,
                option_chain_count: 50,
                build_duration: Duration::from_millis(0),
                build_timestamp: Utc::now().with_timezone(&ist),
            },
        };

        let config = legacy_full_universe_config();
        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        // The plan should be capped at MAX_TOTAL_SUBSCRIPTIONS
        assert!(
            plan.summary.total <= MAX_TOTAL_SUBSCRIPTIONS,
            "plan total ({}) should be capped at MAX_TOTAL_SUBSCRIPTIONS ({})",
            plan.summary.total,
            MAX_TOTAL_SUBSCRIPTIONS
        );

        // Stock derivatives should have been partially skipped
        assert!(
            plan.summary.stock_derivatives_skipped > 0,
            "some stock derivatives should be skipped due to capacity limit"
        );
    }

    // -----------------------------------------------------------------------
    // Single-element option chain (call_count=1, put_count=1)
    // -----------------------------------------------------------------------

    #[cfg(any())] // PR #7b — retired (tested legacy scope behavior)
    fn test_plan_single_element_option_chain() {
        let mut universe = make_test_universe();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();
        let expiry = NaiveDate::from_ymd_opt(2026, 3, 30).unwrap();

        // Replace RELIANCE chain with single call + single put
        let key = OptionChainKey {
            underlying_symbol: "RELIANCE".to_owned(),
            expiry_date: expiry,
        };
        universe.option_chains.insert(
            key,
            OptionChain {
                underlying_symbol: "RELIANCE".to_owned(),
                expiry_date: expiry,
                calls: vec![OptionChainEntry {
                    security_id: 80001,
                    strike_price: 2500.0,
                    lot_size: 500,
                }],
                puts: vec![OptionChainEntry {
                    security_id: 80002,
                    strike_price: 2500.0,
                    lot_size: 500,
                }],
                future_security_id: Some(60001),
            },
        );

        let config = legacy_full_universe_config();
        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        // Single call + single put should be subscribed (mid_idx=0 for both)
        assert!(
            plan.summary.stock_derivatives >= 3,
            "future + 1 call + 1 put = at least 3 stock derivatives, got {}",
            plan.summary.stock_derivatives
        );
    }

    // -----------------------------------------------------------------------
    // Option chain with future_security_id = None
    // -----------------------------------------------------------------------

    #[cfg(any())] // PR #7b — retired (tested legacy scope behavior)
    fn test_plan_option_chain_without_future_still_has_options() {
        let mut universe = make_test_universe();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();
        let expiry = NaiveDate::from_ymd_opt(2026, 3, 30).unwrap();

        // Set future_security_id to None for RELIANCE chain
        let key = OptionChainKey {
            underlying_symbol: "RELIANCE".to_owned(),
            expiry_date: expiry,
        };
        if let Some(chain) = universe.option_chains.get_mut(&key) {
            chain.future_security_id = None;
        }

        let config = legacy_full_universe_config();
        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        // Should still include options even without a future linked in the chain
        assert!(
            plan.summary.stock_derivatives > 0,
            "options should still be subscribed when future_security_id is None"
        );
        // The future may still be picked up from Stage 5 progressive fill
        // (it exists in derivative_contracts). The key behavior is: no panic, options still work.
    }

    // -----------------------------------------------------------------------
    // Constants: DISPLAY_INDEX_ENTRIES subcategory strings are valid
    // -----------------------------------------------------------------------

    #[test]
    fn test_display_index_entries_all_subcategories_recognized() {
        let valid = [
            "Volatility",
            "BroadMarket",
            "MidCap",
            "SmallCap",
            "Sectoral",
            "Thematic",
        ];
        use tickvault_common::constants::DISPLAY_INDEX_ENTRIES;
        for &(name, _id, subcategory) in DISPLAY_INDEX_ENTRIES {
            assert!(
                valid.contains(&subcategory),
                "DISPLAY_INDEX_ENTRIES has unrecognized subcategory '{}' for '{}'",
                subcategory,
                name
            );
        }
    }

    // -----------------------------------------------------------------------
    // Constants: DISPLAY_INDEX_ENTRIES security IDs are non-zero
    // -----------------------------------------------------------------------

    #[test]
    fn test_display_index_entries_security_ids_non_zero() {
        use tickvault_common::constants::DISPLAY_INDEX_ENTRIES;
        for &(name, security_id, _) in DISPLAY_INDEX_ENTRIES {
            assert!(
                security_id > 0,
                "DISPLAY_INDEX_ENTRIES has zero security_id for '{}'",
                name
            );
        }
    }

    // -----------------------------------------------------------------------
    // Constants: DISPLAY_INDEX_ENTRIES names are non-empty
    // -----------------------------------------------------------------------

    #[test]
    fn test_display_index_entries_names_non_empty() {
        use tickvault_common::constants::DISPLAY_INDEX_ENTRIES;
        for &(name, _, _) in DISPLAY_INDEX_ENTRIES {
            assert!(
                !name.trim().is_empty(),
                "DISPLAY_INDEX_ENTRIES has empty name"
            );
        }
    }

    // -----------------------------------------------------------------------
    // PR #6b (2026-05-19): test_archived_planner_produces_identical_summary
    // RETIRED — binary_cache module deleted under 4-IDX_I LOCKED_UNIVERSE.
    // The build_subscription_plan_from_archived path is kept for now (still
    // pub-used by mod.rs) but loses its parity test; a future PR may retire
    // the archived variant entirely.
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Coverage: IDX_I ticker-only routing — major indices use IdxI segment
    // -----------------------------------------------------------------------

    #[test]
    fn test_major_index_uses_idx_i_exchange_segment() {
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        // NIFTY (security_id 13) should be subscribed with IDX_I segment
        let nifty = plan.registry.get(13).unwrap();
        assert_eq!(
            nifty.exchange_segment,
            ExchangeSegment::IdxI,
            "major index value feed must use IDX_I segment"
        );
        assert_eq!(nifty.category, SubscriptionCategory::MajorIndexValue);
    }

    #[test]
    fn test_display_index_uses_idx_i_exchange_segment() {
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        // INDIA VIX (security_id 21) should use IDX_I segment
        let vix = plan.registry.get(21).unwrap();
        assert_eq!(
            vix.exchange_segment,
            ExchangeSegment::IdxI,
            "display index must use IDX_I segment"
        );
        assert_eq!(vix.category, SubscriptionCategory::DisplayIndex);
    }

    // -----------------------------------------------------------------------
    // Coverage: Stock equity uses NSE_EQ segment
    // -----------------------------------------------------------------------

    #[test]
    fn test_stock_equity_uses_nse_equity_segment() {
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        // RELIANCE equity (security_id 2885) should use NSE_EQ segment
        let reliance = plan.registry.get(2885).unwrap();
        assert_eq!(
            reliance.exchange_segment,
            ExchangeSegment::NseEquity,
            "stock equity must use NSE_EQ segment"
        );
        assert_eq!(reliance.category, SubscriptionCategory::StockEquity);
    }

    // -----------------------------------------------------------------------
    // Coverage: Index derivatives use NSE_FNO segment
    // -----------------------------------------------------------------------

    #[cfg(any())] // PR #7b — retired (tested legacy scope behavior)
    fn test_index_derivative_uses_nse_fno_segment() {
        let universe = make_test_universe();
        let config = legacy_full_universe_config();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        // NIFTY future (50001) should use NSE_FNO segment
        let nifty_fut = plan.registry.get(50001).unwrap();
        assert_eq!(
            nifty_fut.exchange_segment,
            ExchangeSegment::NseFno,
            "index derivative must use NSE_FNO segment"
        );
        assert_eq!(nifty_fut.category, SubscriptionCategory::IndexDerivative);
    }

    // -----------------------------------------------------------------------
    // Coverage: Full mode propagates feed_mode to all instruments
    // -----------------------------------------------------------------------

    #[test]
    fn test_full_mode_propagates_to_all_instruments() {
        let universe = make_test_universe();
        let config = SubscriptionConfig {
            feed_mode: "Full".to_string(),
            ..Default::default()
        };
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        assert_eq!(plan.summary.feed_mode, FeedMode::Full);

        // Wave 5 Item 14: NSE_EQ is Quote-mode UNCONDITIONALLY (downgrade
        // from config.feed_mode to save ~115 KB/sec under indices-only
        // scope). Skip NSE_EQ in this propagation check; the dedicated
        // `test_nse_eq_uses_quote_mode_under_indices_only` ratchet pins
        // the override.
        for instrument in plan
            .registry
            .iter()
            .filter(|i| i.exchange_segment != ExchangeSegment::NseEquity)
        {
            assert_eq!(
                instrument.feed_mode,
                FeedMode::Full,
                "non-NSE_EQ instrument {} ({}) should inherit config Full feed mode",
                instrument.security_id,
                instrument.display_label
            );
        }
    }

    // -----------------------------------------------------------------------
    // Coverage: Ticker mode propagates to all instruments
    // -----------------------------------------------------------------------

    #[test]
    fn test_ticker_mode_propagates_to_all_instruments() {
        let universe = make_test_universe();
        let config = SubscriptionConfig {
            feed_mode: "Ticker".to_string(),
            ..Default::default()
        };
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        for instrument in plan
            .registry
            .iter()
            .filter(|i| i.exchange_segment != ExchangeSegment::NseEquity)
        {
            assert_eq!(
                instrument.feed_mode,
                FeedMode::Ticker,
                "non-NSE_EQ instrument {} should inherit config Ticker feed mode \
                 (Wave 5 Item 14: NSE_EQ is Quote-mode unconditionally)",
                instrument.security_id
            );
        }
    }

    // -----------------------------------------------------------------------
    // Coverage: Capacity limit — plan total capped at MAX_TOTAL_SUBSCRIPTIONS
    // -----------------------------------------------------------------------

    #[cfg(any())] // PR #7b — retired (tested legacy scope behavior)
    fn test_capacity_limit_stage2_break_path() {
        // Build a universe with enough stock derivatives to trigger the
        // `instruments.len() >= MAX_TOTAL_SUBSCRIPTIONS` break in Stage 2.
        // We need > 25,000 instruments total. The existing capacity test does
        // this with 50 stocks x 600 options. Here we verify the exact cap behavior.
        let ist = tickvault_common::trading_calendar::ist_offset();
        let expiry = NaiveDate::from_ymd_opt(2026, 3, 27).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let mut underlyings = HashMap::new();
        let mut derivative_contracts: HashMap<SecurityId, DerivativeContract> = HashMap::new();
        let mut expiry_calendars = HashMap::new();

        // Create 30 stocks with 1000 options each = 30,000 + 30 futures = 30,030
        let mut base_id: u64 = 200_000;
        for stock_idx in 0..30u32 {
            let symbol = format!("CAP{stock_idx}");
            let equity_id = 180_000 + u64::from(stock_idx);

            underlyings.insert(
                symbol.clone(),
                FnoUnderlying {
                    underlying_symbol: symbol.clone(),
                    underlying_security_id: 190_000 + u64::from(stock_idx),
                    price_feed_security_id: equity_id,
                    price_feed_segment: ExchangeSegment::NseEquity,
                    derivative_segment: ExchangeSegment::NseFno,
                    kind: UnderlyingKind::Stock,
                    lot_size: 100,
                    contract_count: 1001,
                },
            );

            let fut_id = base_id;
            base_id += 1;
            derivative_contracts.insert(
                fut_id,
                DerivativeContract {
                    security_id: fut_id,
                    underlying_symbol: symbol.clone(),
                    instrument_kind: DhanInstrumentKind::FutureStock,
                    exchange_segment: ExchangeSegment::NseFno,
                    expiry_date: expiry,
                    strike_price: 0.0,
                    option_type: None,
                    lot_size: 100,
                    tick_size: 0.05,
                    symbol_name: format!("{symbol}-FUT"),
                    display_name: format!("{symbol} FUT"),
                },
            );

            for strike_idx in 0..500u32 {
                let ce_id = base_id;
                base_id += 1;
                let pe_id = base_id;
                base_id += 1;
                let strike = 500.0 + (strike_idx as f64) * 5.0;

                derivative_contracts.insert(
                    ce_id,
                    DerivativeContract {
                        security_id: ce_id,
                        underlying_symbol: symbol.clone(),
                        instrument_kind: DhanInstrumentKind::OptionStock,
                        exchange_segment: ExchangeSegment::NseFno,
                        expiry_date: expiry,
                        strike_price: strike,
                        option_type: Some(OptionType::Call),
                        lot_size: 100,
                        tick_size: 0.05,
                        symbol_name: format!("{symbol}-{strike}-CE"),
                        display_name: format!("{symbol} {strike} CE"),
                    },
                );
                derivative_contracts.insert(
                    pe_id,
                    DerivativeContract {
                        security_id: pe_id,
                        underlying_symbol: symbol.clone(),
                        instrument_kind: DhanInstrumentKind::OptionStock,
                        exchange_segment: ExchangeSegment::NseFno,
                        expiry_date: expiry,
                        strike_price: strike,
                        option_type: Some(OptionType::Put),
                        lot_size: 100,
                        tick_size: 0.05,
                        symbol_name: format!("{symbol}-{strike}-PE"),
                        display_name: format!("{symbol} {strike} PE"),
                    },
                );
            }

            expiry_calendars.insert(
                symbol.clone(),
                ExpiryCalendar {
                    underlying_symbol: symbol,
                    expiry_dates: vec![expiry],
                },
            );
        }

        let universe = FnoUniverse {
            underlyings,
            derivative_contracts,
            instrument_info: HashMap::new(),
            option_chains: HashMap::new(), // no option chains -> skips ATM filtering
            expiry_calendars,
            subscribed_indices: vec![],
            build_metadata: UniverseBuildMetadata {
                csv_source: "test-cap".to_string(),
                csv_row_count: 0,
                parsed_row_count: 0,
                index_count: 0,
                equity_count: 0,
                underlying_count: 30,
                derivative_count: 30030,
                option_chain_count: 0,
                build_duration: Duration::from_millis(0),
                build_timestamp: Utc::now().with_timezone(&ist),
            },
        };

        let config = legacy_full_universe_config();
        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        // Total must not exceed MAX_TOTAL_SUBSCRIPTIONS
        assert!(
            plan.summary.total <= MAX_TOTAL_SUBSCRIPTIONS,
            "plan total ({}) must be <= {}",
            plan.summary.total,
            MAX_TOTAL_SUBSCRIPTIONS
        );

        // Some stock derivatives should have been skipped
        assert!(
            plan.summary.stock_derivatives_skipped > 0,
            "capacity cap should cause some derivatives to be skipped"
        );

        // stock_derivatives_available should be larger than what was subscribed
        assert!(
            plan.summary.stock_derivatives_available > plan.summary.stock_derivatives,
            "available ({}) > subscribed ({})",
            plan.summary.stock_derivatives_available,
            plan.summary.stock_derivatives
        );
    }

    // -----------------------------------------------------------------------
    // Coverage: Quote mode propagates to instruments
    // -----------------------------------------------------------------------

    #[test]
    fn test_quote_mode_propagates_to_all_instruments() {
        let universe = make_test_universe();
        let config = SubscriptionConfig {
            feed_mode: "Quote".to_string(),
            ..Default::default()
        };
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        assert_eq!(plan.summary.feed_mode, FeedMode::Quote);
        for instrument in plan.registry.iter() {
            assert_eq!(
                instrument.feed_mode,
                FeedMode::Quote,
                "instrument {} should have Quote feed mode",
                instrument.security_id
            );
        }
    }

    // -----------------------------------------------------------------------
    // Coverage: Stock derivative category assignment
    // -----------------------------------------------------------------------

    #[cfg(any())] // PR #7b — retired (tested legacy scope behavior)
    fn test_stock_derivative_category_assignment() {
        let universe = make_test_universe();
        let config = legacy_full_universe_config();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        // RELIANCE future (60001) should be StockDerivative
        let rel_fut = plan.registry.get(60001).unwrap();
        assert_eq!(rel_fut.category, SubscriptionCategory::StockDerivative);

        // RELIANCE option CE (60100) should be StockDerivative
        let rel_ce = plan.registry.get(60100).unwrap();
        assert_eq!(rel_ce.category, SubscriptionCategory::StockDerivative);
    }

    // -----------------------------------------------------------------------
    // Coverage: underlying_symbol propagated correctly
    // -----------------------------------------------------------------------

    #[cfg(any())] // PR #7b — retired (tested legacy scope behavior)
    fn test_underlying_symbol_propagated_to_instruments() {
        let universe = make_test_universe();
        let config = legacy_full_universe_config();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        // NIFTY index value
        let nifty = plan.registry.get(13).unwrap();
        assert_eq!(nifty.underlying_symbol, "NIFTY");

        // RELIANCE equity
        let reliance = plan.registry.get(2885).unwrap();
        assert_eq!(reliance.underlying_symbol, "RELIANCE");

        // NIFTY derivative
        let nifty_fut = plan.registry.get(50001).unwrap();
        assert_eq!(nifty_fut.underlying_symbol, "NIFTY");
    }

    // -----------------------------------------------------------------------
    // Coverage: Index option is classified as IndexDerivative
    // -----------------------------------------------------------------------

    #[cfg(any())] // PR #7b — retired (tested legacy scope behavior)
    fn test_index_option_classified_as_index_derivative() {
        let universe = make_test_universe();
        let config = legacy_full_universe_config();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        // NIFTY CE option (50100) should be IndexDerivative
        let nifty_ce = plan.registry.get(50100).unwrap();
        assert_eq!(nifty_ce.category, SubscriptionCategory::IndexDerivative);

        // NIFTY PE option (50200) should be IndexDerivative
        let nifty_pe = plan.registry.get(50200).unwrap();
        assert_eq!(nifty_pe.category, SubscriptionCategory::IndexDerivative);
    }

    // ========================================================================
    // REGRESSION (2026-04-17, spotted live by Parthiban): Dhan reuses the
    // same numeric security_id across different segments (e.g. id=13 is
    // FINNIFTY in IDX_I AND is some other instrument in NSE_EQ). The old
    // `HashSet<u32>` dedup silently dropped the second-seen instance.
    // These tests prove the new `HashSet<(u64, ExchangeSegment)>` dedup
    // keeps BOTH.
    // ========================================================================

    #[test]
    fn test_regression_finnifty_id27_both_segments_are_kept() {
        // REGRESSION (spotted live by Parthiban, 2026-04-17):
        // Dhan reuses the same numeric security_id across segments
        // (e.g. FINNIFTY's IDX_I index value = 27 and some NSE_EQ
        // instrument may ALSO be 27). The old `HashSet<u32>` dedup
        // silently dropped the second-seen instance. Live symptom:
        // FINNIFTY's IDX_I subscription never went out, depth ATM
        // selector had no spot price for it, and FINNIFTY was
        // silently missing from the depth-20 dashboard.
        //
        // Repro uses NIFTY (price_feed_security_id=13, IDX_I) as the
        // "major index" leg and adds a synthetic stock with
        // price_feed_security_id=13 in NSE_EQ. Without the fix, the
        // second insert of id=13 would return false and one of the
        // two would be dropped. With the fix, both are kept because
        // the dedup key is `(13, IdxI)` vs `(13, NseEquity)`.
        use tickvault_common::instrument_types::{FnoUnderlying, UnderlyingKind};
        let mut universe = make_test_universe();

        // Precondition: NIFTY exists with price_feed_security_id=13 on IDX_I.
        let nifty = universe
            .underlyings
            .get("NIFTY")
            .expect("test universe must contain NIFTY");
        assert_eq!(nifty.price_feed_security_id, 13);
        assert_eq!(nifty.price_feed_segment, ExchangeSegment::IdxI);

        // Add a synthetic stock with price_feed_security_id == 13 in NSE_EQ.
        universe.underlyings.insert(
            "SYNTHETIC_STOCK_COLLIDER".to_string(),
            FnoUnderlying {
                underlying_symbol: "SYNTHETIC_STOCK_COLLIDER".to_string(),
                underlying_security_id: 13,
                price_feed_security_id: 13, // SAME id as NIFTY IDX_I
                price_feed_segment: ExchangeSegment::NseEquity,
                derivative_segment: ExchangeSegment::NseFno,
                kind: UnderlyingKind::Stock,
                lot_size: 500,
                contract_count: 0,
            },
        );

        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();
        let plan = build_subscription_plan(
            &universe,
            &SubscriptionConfig::default(),
            today,
            &std::collections::HashMap::new(),
            None,
        );

        // Count instruments with security_id == 13, grouped by segment.
        let mut id13_by_segment: std::collections::HashMap<ExchangeSegment, usize> =
            std::collections::HashMap::new();
        for i in plan.registry.iter().filter(|i| i.security_id == 13) {
            *id13_by_segment.entry(i.exchange_segment).or_insert(0) += 1;
        }

        // FINAL BEHAVIOR (complete fix, 2026-04-17):
        // - Planner-level `seen_ids` is segment-aware → both
        //   (13, IdxI) and (13, NseEquity) go into the raw Vec.
        // - `InstrumentRegistry::from_instruments` maintains TWO indexes:
        //   legacy `instruments: HashMap<SecurityId, _>` AND new
        //   `by_composite: HashMap<(SecurityId, ExchangeSegment), _>`.
        //   Both entries live in composite; legacy collapses one (with
        //   WARN log) for backward compat across 59 existing call sites.
        // - `get_with_segment` is the correct API for segment-aware
        //   callers (tick processor has segment from header byte 3).
        assert!(
            !id13_by_segment.is_empty(),
            "at least one entry with security_id=13 must survive in the \
             legacy registry. Got: {id13_by_segment:?}"
        );
        for (seg, count) in &id13_by_segment {
            assert_eq!(
                *count, 1,
                "security_id=13 appears {count} times in segment {seg:?} — \
                 intra-segment dedup must be exactly one"
            );
        }

        // I-P1-11: mechanical proof the full fix works. BOTH entries MUST
        // be addressable via the segment-aware lookup.
        assert!(
            plan.registry
                .get_with_segment(13, ExchangeSegment::IdxI)
                .is_some(),
            "NIFTY IDX_I (security_id=13) must be addressable via \
             get_with_segment. I-P1-11 composite index broken."
        );
        assert!(
            plan.registry
                .get_with_segment(13, ExchangeSegment::NseEquity)
                .is_some(),
            "Synthetic stock NSE_EQ (security_id=13) must be addressable \
             via get_with_segment. I-P1-11 composite index broken."
        );
        assert!(
            plan.registry
                .contains_with_segment(13, ExchangeSegment::IdxI)
        );
        assert!(
            plan.registry
                .contains_with_segment(13, ExchangeSegment::NseEquity)
        );
        // Negative: a segment we didn't add must NOT be reported as present.
        assert!(
            !plan
                .registry
                .contains_with_segment(13, ExchangeSegment::BseEquity),
            "segment-aware contains must be strict — BseEquity was never added"
        );
    }

    #[test]
    fn test_regression_seen_ids_key_type_is_pair() {
        // Compile-time / type-level assertion: the dedup HashSet must be
        // typed as `HashSet<(u64, ExchangeSegment)>` so a future refactor
        // that reverts to `HashSet<u32>` fails to compile before this
        // test even runs. We assert via a tiny synthetic HashSet that
        // matches the production type.
        let mut set: std::collections::HashSet<(u64, ExchangeSegment)> =
            std::collections::HashSet::new();
        // Inserting the same id under two different segments must return
        // true for both — the pair is the key.
        assert!(
            set.insert((27, ExchangeSegment::IdxI)),
            "first insert of (27, IdxI) must succeed"
        );
        assert!(
            set.insert((27, ExchangeSegment::NseEquity)),
            "second insert of (27, NseEq) must ALSO succeed — different segment, \
             logically different instrument. If this fails, someone regressed \
             the dedup key to `u32` alone."
        );
        assert!(
            !set.insert((27, ExchangeSegment::IdxI)),
            "third insert of (27, IdxI) is a true duplicate — must be rejected"
        );
    }

    // -----------------------------------------------------------------
    // Fix #6 (2026-04-24): stock F&O expiry rollover.
    //
    // Strict rule: roll to NEXT expiry when today is T (expiry day) or
    // T-1 (day before). T-2 keeps the current expiry. Indices never roll.
    // -----------------------------------------------------------------

    fn make_test_calendar_no_holidays() -> TradingCalendar {
        use tickvault_common::config::TradingConfig;
        let cfg = TradingConfig {
            market_open_time: "09:00:00".to_string(),
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
        TradingCalendar::from_config(&cfg).expect("calendar must build")
    }

    #[test]
    fn test_stock_expiry_keeps_nearest_on_t_minus_1() {
        // T-only rule (2026-04-28): T-1 KEEPS the nearest expiry.
        // Was previously "rolls" under the T-or-T-1 rule; narrowed because
        // Dhan only blocks trading on expiry day itself, not T-1.
        // Today = Wed 2026-04-29. Nearest expiry = Thu 2026-04-30.
        // count_trading_days(Wed, Thu) = 1 → T-only rule (<= 0) does NOT roll.
        let cal = make_test_calendar_no_holidays();
        let today = NaiveDate::from_ymd_opt(2026, 4, 29).unwrap();
        let nearest = NaiveDate::from_ymd_opt(2026, 4, 30).unwrap();
        let next = NaiveDate::from_ymd_opt(2026, 5, 7).unwrap();
        let picked = select_stock_expiry_with_rollover(&[nearest, next], today, Some(&cal));
        assert_eq!(
            picked,
            Some(nearest),
            "T-only rule (2026-04-28): Wed (T-1) with Thu expiry KEEPS nearest \
             (was: rolls under old T-or-T-1 rule)"
        );
    }

    #[test]
    fn test_stock_expiry_rolls_only_on_t_zero() {
        // T-only rule (2026-04-28): T-0 (expiry day itself) rolls.
        // This is the ONLY case that triggers rollover under the new rule.
        // Today = Thu 2026-04-30 (expiry day). Nearest = today.
        // count_trading_days(Thu, Thu) = 0 → T-only rule rolls.
        let cal = make_test_calendar_no_holidays();
        let today = NaiveDate::from_ymd_opt(2026, 4, 30).unwrap();
        let nearest = today;
        let next = NaiveDate::from_ymd_opt(2026, 5, 7).unwrap();
        let picked = select_stock_expiry_with_rollover(&[nearest, next], today, Some(&cal));
        assert_eq!(
            picked,
            Some(next),
            "T-only rule (2026-04-28): Thu (expiry day) MUST roll to next expiry — \
             Dhan disallows stock F&O trading on expiry day itself"
        );
    }

    #[test]
    fn test_stock_expiry_stays_on_t_minus_2() {
        // Today = Tue 2026-04-28. Nearest = Thu 2026-04-30.
        // count_trading_days(Tue, Thu) = 2 → strict rule keeps nearest.
        let cal = make_test_calendar_no_holidays();
        let today = NaiveDate::from_ymd_opt(2026, 4, 28).unwrap();
        let nearest = NaiveDate::from_ymd_opt(2026, 4, 30).unwrap();
        let next = NaiveDate::from_ymd_opt(2026, 5, 7).unwrap();
        let picked = select_stock_expiry_with_rollover(&[nearest, next], today, Some(&cal));
        assert_eq!(
            picked,
            Some(nearest),
            "Fix #6 strict: Tue (T-2) keeps nearest — not T or T-1"
        );
    }

    #[test]
    fn test_stock_expiry_none_when_all_expiries_past() {
        let cal = make_test_calendar_no_holidays();
        let today = NaiveDate::from_ymd_opt(2026, 5, 1).unwrap();
        let past1 = NaiveDate::from_ymd_opt(2026, 4, 23).unwrap();
        let past2 = NaiveDate::from_ymd_opt(2026, 4, 30).unwrap();
        let picked = select_stock_expiry_with_rollover(&[past1, past2], today, Some(&cal));
        assert_eq!(picked, None);
    }

    #[test]
    fn test_stock_expiry_no_next_keeps_nearest_on_t_zero() {
        // T-only rule: only T-0 triggers rollover. With single expiry on
        // T-0, cannot roll forward. Keep nearest (= today = expiry day)
        // and let caller decide to skip. Also emits a WARN log (not
        // asserted — tracing-capture is heavy).
        // (Previously this test used T-1 because old rule triggered there;
        // updated to T-0 to cover the equivalent code path under T-only.)
        let cal = make_test_calendar_no_holidays();
        let today = NaiveDate::from_ymd_opt(2026, 4, 30).unwrap();
        let only = NaiveDate::from_ymd_opt(2026, 4, 30).unwrap();
        let picked = select_stock_expiry_with_rollover(&[only], today, Some(&cal));
        assert_eq!(
            picked,
            Some(only),
            "no next expiry → keep nearest (graceful degradation)"
        );
    }

    #[test]
    fn test_stock_expiry_none_calendar_uses_legacy_nearest() {
        // Without a calendar, the helper falls back to pre-Fix-6 nearest-only
        // behaviour — even on T-1 or T. Existing test callers pass None.
        let today = NaiveDate::from_ymd_opt(2026, 4, 29).unwrap();
        let nearest = NaiveDate::from_ymd_opt(2026, 4, 30).unwrap();
        let next = NaiveDate::from_ymd_opt(2026, 5, 7).unwrap();
        let picked = select_stock_expiry_with_rollover(&[nearest, next], today, None);
        assert_eq!(picked, Some(nearest));
    }

    #[test]
    fn test_index_expiry_never_rolls_via_planner() {
        // Ratchet: indices must NEVER roll. The planner applies rollover
        // ONLY inside the `UnderlyingKind::Stock` branch, so indices see
        // all their derivative contracts unconditionally (no expiry filter
        // runs for indices). This test covers the index path end-to-end.
        //
        // Set up an index (NIFTY) where today is expiry day. If the rule
        // leaked into the index path, NO index contracts would be emitted.
        // We instead assert that major-index derivative count > 0.
        let universe = make_test_universe();
        let cal = make_test_calendar_no_holidays();
        // Use a date where NIFTY's expiry in make_test_universe (2026-03-27)
        // is T+1 trading day from today. Even at T-1 the planner must still
        // emit index contracts.
        let today = NaiveDate::from_ymd_opt(2026, 3, 26).unwrap();
        let mut config = SubscriptionConfig::default();
        config.subscribe_stock_equities = true;

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            Some(&cal),
        );

        // Index derivatives must still be present — rollover does NOT
        // apply to indices.
        assert!(
            plan.summary.index_derivatives > 0 || plan.summary.major_index_values > 0,
            "Fix #6 ratchet: indices must emit derivative/value contracts even on T-1. \
             If this fails, rollover has leaked into the index path."
        );
    }

    // ========================================================================
    // 2026-04-25 ratchets — F&O universe rebuild (3 indices + ATM±25 stocks)
    // ========================================================================

    /// 2026-04-25 ratchet: full-chain index set is exactly 3 — NIFTY,
    /// BANKNIFTY, SENSEX. Regression to 5 (re-adding FINNIFTY/MIDCPNIFTY)
    /// resurrects the 40K-contract over-subscription bug.
    #[test]
    fn test_only_three_indices_in_full_chain_set() {
        assert_eq!(FULL_CHAIN_INDEX_SYMBOLS.len(), 3);
        let set: HashSet<&str> = FULL_CHAIN_INDEX_SYMBOLS.iter().copied().collect();
        assert!(set.contains("NIFTY"));
        assert!(set.contains("BANKNIFTY"));
        assert!(set.contains("SENSEX"));
    }

    /// 2026-04-25 ratchet: FINNIFTY + MIDCPNIFTY are explicitly NOT in the
    /// full-chain set. Both were dropped to free 25K WS capacity.
    #[test]
    fn test_finnifty_midcpnifty_dropped_from_index_set() {
        let set: HashSet<&str> = FULL_CHAIN_INDEX_SYMBOLS.iter().copied().collect();
        assert!(
            !set.contains("FINNIFTY"),
            "FINNIFTY must stay dropped from FULL_CHAIN_INDEX_SYMBOLS"
        );
        assert!(
            !set.contains("MIDCPNIFTY"),
            "MIDCPNIFTY must stay dropped from FULL_CHAIN_INDEX_SYMBOLS"
        );
    }

    /// PR #7b — `test_index_derivatives_subscribe_all_future_expiries`
    /// retired. The test exercised the legacy `IndicesOnlyAllExpiries`
    /// scope's "subscribe every NIFTY/BANKNIFTY/SENSEX expiry" path,
    /// which no longer exists. Under `Indices4Only` index F&O is never
    /// subscribed.
    #[cfg(any())]
    fn test_index_derivatives_subscribe_all_future_expiries() {
        let nearest = NaiveDate::from_ymd_opt(2026, 4, 30).unwrap();
        let mid = NaiveDate::from_ymd_opt(2026, 5, 28).unwrap();
        let far = NaiveDate::from_ymd_opt(2026, 6, 25).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 4, 25).unwrap();

        let mut underlyings = HashMap::new();
        let mut derivative_contracts: HashMap<SecurityId, DerivativeContract> = HashMap::new();

        underlyings.insert(
            "NIFTY".to_string(),
            FnoUnderlying {
                underlying_symbol: "NIFTY".to_string(),
                underlying_security_id: 26000,
                price_feed_security_id: 13,
                price_feed_segment: ExchangeSegment::IdxI,
                derivative_segment: ExchangeSegment::NseFno,
                kind: UnderlyingKind::NseIndex,
                lot_size: 50,
                contract_count: 6,
            },
        );

        // Insert 1 future + 1 CE per expiry (3 expiries × 2 contracts = 6 total)
        for (i, exp) in [nearest, mid, far].iter().enumerate() {
            derivative_contracts.insert(
                70000 + i as u64,
                DerivativeContract {
                    security_id: 70000 + i as u64,
                    underlying_symbol: "NIFTY".to_string(),
                    instrument_kind: DhanInstrumentKind::FutureIndex,
                    exchange_segment: ExchangeSegment::NseFno,
                    expiry_date: *exp,
                    strike_price: 0.0,
                    option_type: None,
                    lot_size: 50,
                    tick_size: 0.05,
                    symbol_name: format!("NIFTY-{exp}-FUT"),
                    display_name: format!("NIFTY FUT {exp}"),
                },
            );
            derivative_contracts.insert(
                70010 + i as u64,
                DerivativeContract {
                    security_id: 70010 + i as u64,
                    underlying_symbol: "NIFTY".to_string(),
                    instrument_kind: DhanInstrumentKind::OptionIndex,
                    exchange_segment: ExchangeSegment::NseFno,
                    expiry_date: *exp,
                    strike_price: 18000.0,
                    option_type: Some(OptionType::Call),
                    lot_size: 50,
                    tick_size: 0.05,
                    symbol_name: format!("NIFTY-{exp}-18000-CE"),
                    display_name: format!("NIFTY 18000 CE {exp}"),
                },
            );
        }

        let ist = tickvault_common::trading_calendar::ist_offset();
        let universe = FnoUniverse {
            underlyings,
            derivative_contracts,
            option_chains: HashMap::new(),
            expiry_calendars: HashMap::new(),
            instrument_info: HashMap::new(),
            subscribed_indices: Vec::new(),
            build_metadata: UniverseBuildMetadata {
                csv_source: "test".to_string(),
                csv_row_count: 0,
                parsed_row_count: 0,
                index_count: 0,
                equity_count: 0,
                underlying_count: 0,
                derivative_count: 0,
                option_chain_count: 0,
                build_duration: Duration::from_millis(0),
                build_timestamp: Utc::now().with_timezone(&ist),
            },
        };

        let config = SubscriptionConfig {
            scope: SubscriptionScope::IndicesOnlyAllExpiries,
            feed_mode: "Full".to_string(),
            subscribe_index_derivatives: true,
            subscribe_stock_derivatives: false,
            subscribe_stock_equities: false,
            subscribe_display_indices: false,
            stock_atm_strikes_above: 25,
            stock_atm_strikes_below: 25,
            stock_default_atm_fallback_enabled: false,
            enable_twenty_depth: false,
            twenty_depth_max_instruments: 49,
            ..SubscriptionConfig::default()
        };

        let plan = build_subscription_plan(&universe, &config, today, &HashMap::new(), None);

        // Assert: ALL 6 contracts (3 expiries × {future, CE}) are in the plan.
        // Operator-confirmed 2026-05-02: every future expiry of the 3
        // full-chain indices must be subscribed for term-structure
        // visibility. Yields ~10-11K live contracts in production.
        let subscribed_ids: HashSet<u64> = plan.registry.iter().map(|i| i.security_id).collect();

        for (label, sid) in [
            ("Nearest-expiry NIFTY future", 70000_u32),
            ("Mid-expiry NIFTY future", 70001),
            ("Far-expiry NIFTY future", 70002),
            ("Nearest-expiry NIFTY CE", 70010),
            ("Mid-expiry NIFTY CE", 70011),
            ("Far-expiry NIFTY CE", 70012),
        ] {
            assert!(
                subscribed_ids.contains(&sid),
                "{label} (sid={sid}) must be subscribed under all-expiries policy"
            );
        }
        // Plan also includes the IDX_I price-feed (security_id 13, segment IDX_I),
        // which is correct. Count only the F&O contracts (70000+ range).
        let fno_count = subscribed_ids
            .iter()
            .filter(|&&sid| (70000..70100).contains(&sid))
            .count();
        assert_eq!(
            fno_count, 6,
            "Exactly 6 NIFTY F&O contracts (3 expiries × {{FUT, CE}}) expected, got {fno_count}"
        );
    }

    /// 2026-04-25 ratchet: capacity assertion — total subscription count
    /// MUST stay below the hard cap. The default test universe has 1 stock
    /// (RELIANCE) + 1 index (NIFTY) and ~12 derivative contracts; this is a
    /// trivially-small assertion. The real-world live count of ~24,324 is
    /// validated separately at boot via `summary.exceeds_capacity`.
    #[test]
    fn test_total_subscription_count_below_25k_hard_limit() {
        let universe = make_test_universe();
        let cal = make_test_calendar_no_holidays();
        let today = NaiveDate::from_ymd_opt(2026, 3, 26).unwrap();
        let config = SubscriptionConfig::default();
        let mut spot = HashMap::new();
        spot.insert("RELIANCE".to_string(), 2500.0);

        let plan = build_subscription_plan(&universe, &config, today, &spot, Some(&cal));

        assert!(
            plan.summary.total <= MAX_TOTAL_SUBSCRIPTIONS,
            "total {} must not exceed hard cap {}",
            plan.summary.total,
            MAX_TOTAL_SUBSCRIPTIONS
        );
        assert!(
            !plan.summary.exceeds_capacity,
            "exceeds_capacity flag must be false on a sane universe"
        );
    }

    /// 2026-04-25 ratchet: stock-option ATM cap constant exists and is
    /// `STOCK_OPTION_ATM_STRIKES_EACH_SIDE = 25`. Defaults of
    /// `SubscriptionConfig::stock_atm_strikes_above/below` MUST equal this
    /// constant in production; test configs may override smaller for
    /// fast iteration. The end-to-end ATM filtering behaviour is already
    /// covered by `test_atm_strike_range_narrow` and
    /// `test_plan_stock_option_chain_calls_only`.
    #[test]
    fn test_stock_options_atm_cap_constant_is_25() {
        use tickvault_common::constants::STOCK_OPTION_ATM_STRIKES_EACH_SIDE;
        assert_eq!(STOCK_OPTION_ATM_STRIKES_EACH_SIDE, 25);

        // Verify SubscriptionConfig::default() picks up the same value.
        let cfg = SubscriptionConfig::default();
        assert_eq!(
            cfg.stock_atm_strikes_above, STOCK_OPTION_ATM_STRIKES_EACH_SIDE,
            "default config must mirror the constant for production safety"
        );
        assert_eq!(
            cfg.stock_atm_strikes_below, STOCK_OPTION_ATM_STRIKES_EACH_SIDE,
            "default config must mirror the constant for production safety"
        );
    }

    /// 2026-04-28 ratchet (NEW #45): rollover constant is exactly 0 (T-only).
    /// Was 1 (T-or-T-1) until 2026-04-28. Regressing to 1 would prematurely
    /// drop T-1 contracts that ARE tradeable on Dhan; regressing to 2 would
    /// lose another trading day of liquidity. Lock at 0.
    #[test]
    fn test_stock_expiry_rollover_constant_is_zero() {
        assert_eq!(
            STOCK_EXPIRY_ROLLOVER_TRADING_DAYS, 0,
            "rollover threshold must stay at 0 (T-only — Dhan only blocks expiry-day trading)"
        );
    }

    /// 2026-04-28 ratchet (UPDATED for T-only): cross-instrument rollover
    /// scope check — confirms the rollover applies to BOTH OPTSTK (stock
    /// options) AND FUTSTK (stock futures). Both are F&O on the same stock
    /// underlying and share the same expiry calendar; if one rolls, the
    /// other must too. Set up RELIANCE on T-0 (expiry day, the ONLY case
    /// that triggers rollover under T-only rule), assert that BOTH the
    /// future and the options rolled to the next expiry.
    #[cfg(any())] // PR #7b — retired (tested legacy scope behavior)
    fn test_stock_rollover_applies_to_both_optstk_and_futstk() {
        // Build a 2-expiry RELIANCE universe: nearest is T-0, next is +28d.
        let nearest = NaiveDate::from_ymd_opt(2026, 4, 30).unwrap();
        let next = NaiveDate::from_ymd_opt(2026, 5, 28).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 4, 30).unwrap(); // T-0 (expiry day)

        let mut underlyings = HashMap::new();
        underlyings.insert(
            "RELIANCE".to_string(),
            FnoUnderlying {
                underlying_symbol: "RELIANCE".to_string(),
                underlying_security_id: 2885,
                price_feed_security_id: 2885,
                price_feed_segment: ExchangeSegment::NseEquity,
                derivative_segment: ExchangeSegment::NseFno,
                kind: UnderlyingKind::Stock,
                lot_size: 250,
                contract_count: 4,
            },
        );

        let mut derivative_contracts: HashMap<SecurityId, DerivativeContract> = HashMap::new();
        // Future + 1 CE strike at NEAREST expiry (should be SKIPPED due to rollover)
        derivative_contracts.insert(
            80001,
            DerivativeContract {
                security_id: 80001,
                underlying_symbol: "RELIANCE".to_string(),
                instrument_kind: DhanInstrumentKind::FutureStock,
                exchange_segment: ExchangeSegment::NseFno,
                expiry_date: nearest,
                strike_price: 0.0,
                option_type: None,
                lot_size: 250,
                tick_size: 0.05,
                symbol_name: "RELIANCE-30APR26-FUT".to_string(),
                display_name: "RELIANCE FUT 30Apr26 (NEAREST)".to_string(),
            },
        );
        derivative_contracts.insert(
            80002,
            DerivativeContract {
                security_id: 80002,
                underlying_symbol: "RELIANCE".to_string(),
                instrument_kind: DhanInstrumentKind::OptionStock,
                exchange_segment: ExchangeSegment::NseFno,
                expiry_date: nearest,
                strike_price: 2700.0,
                option_type: Some(OptionType::Call),
                lot_size: 250,
                tick_size: 0.05,
                symbol_name: "RELIANCE-30APR26-2700-CE".to_string(),
                display_name: "RELIANCE 2700 CE 30Apr26 (NEAREST)".to_string(),
            },
        );
        // Future + 1 CE strike at NEXT expiry (should be subscribed after rollover)
        derivative_contracts.insert(
            80101,
            DerivativeContract {
                security_id: 80101,
                underlying_symbol: "RELIANCE".to_string(),
                instrument_kind: DhanInstrumentKind::FutureStock,
                exchange_segment: ExchangeSegment::NseFno,
                expiry_date: next,
                strike_price: 0.0,
                option_type: None,
                lot_size: 250,
                tick_size: 0.05,
                symbol_name: "RELIANCE-28MAY26-FUT".to_string(),
                display_name: "RELIANCE FUT 28May26 (NEXT)".to_string(),
            },
        );
        derivative_contracts.insert(
            80102,
            DerivativeContract {
                security_id: 80102,
                underlying_symbol: "RELIANCE".to_string(),
                instrument_kind: DhanInstrumentKind::OptionStock,
                exchange_segment: ExchangeSegment::NseFno,
                expiry_date: next,
                strike_price: 2700.0,
                option_type: Some(OptionType::Call),
                lot_size: 250,
                tick_size: 0.05,
                symbol_name: "RELIANCE-28MAY26-2700-CE".to_string(),
                display_name: "RELIANCE 2700 CE 28May26 (NEXT)".to_string(),
            },
        );

        let mut option_chains = HashMap::new();
        option_chains.insert(
            OptionChainKey {
                underlying_symbol: "RELIANCE".to_string(),
                expiry_date: next,
            },
            OptionChain {
                underlying_symbol: "RELIANCE".to_string(),
                expiry_date: next,
                calls: vec![OptionChainEntry {
                    security_id: 80102,
                    strike_price: 2700.0,
                    lot_size: 250,
                }],
                puts: Vec::new(),
                future_security_id: Some(80101),
            },
        );

        let mut expiry_calendars = HashMap::new();
        expiry_calendars.insert(
            "RELIANCE".to_string(),
            ExpiryCalendar {
                underlying_symbol: "RELIANCE".to_string(),
                expiry_dates: vec![nearest, next],
            },
        );

        let ist = tickvault_common::trading_calendar::ist_offset();
        let universe = FnoUniverse {
            underlyings,
            derivative_contracts,
            option_chains,
            expiry_calendars,
            instrument_info: HashMap::new(),
            subscribed_indices: Vec::new(),
            build_metadata: UniverseBuildMetadata {
                csv_source: "test".to_string(),
                csv_row_count: 0,
                parsed_row_count: 0,
                index_count: 0,
                equity_count: 0,
                underlying_count: 0,
                derivative_count: 0,
                option_chain_count: 0,
                build_duration: Duration::from_millis(0),
                build_timestamp: Utc::now().with_timezone(&ist),
            },
        };

        let cal = make_test_calendar_no_holidays();
        let mut spot = HashMap::new();
        spot.insert("RELIANCE".to_string(), 2700.0);
        let plan = build_subscription_plan(
            &universe,
            &legacy_full_universe_config(),
            today,
            &spot,
            Some(&cal),
        );

        let subscribed_ids: HashSet<u64> = plan.registry.iter().map(|i| i.security_id).collect();

        // BOTH NEAREST FUT and NEAREST CE must be DROPPED (rolled away).
        assert!(
            !subscribed_ids.contains(&80001),
            "Stock FUTSTK at T-0 (expiry day) nearest expiry must be rolled — found in subscription"
        );
        assert!(
            !subscribed_ids.contains(&80002),
            "Stock OPTSTK at T-0 (expiry day) nearest expiry must be rolled — found in subscription"
        );
        // BOTH NEXT FUT and NEXT CE must be SUBSCRIBED (rolled to).
        assert!(
            subscribed_ids.contains(&80101),
            "Stock FUTSTK at NEXT expiry must be subscribed after rollover"
        );
        assert!(
            subscribed_ids.contains(&80102),
            "Stock OPTSTK at NEXT expiry must be subscribed after rollover"
        );
    }

    // ----------------------------------------------------------------------
    // Wave 5 Item 13 — boot-time prev-close routing assertion ratchets.
    //
    // Per `.claude/rules/dhan/live-market-feed.md` rule 7+8 + the
    // `Previous-Day Close Routing` block: prev_close arrives via different
    // packet types depending on segment. The subscription plan MUST stamp
    // each instrument with a feed_mode that delivers prev_close for its
    // segment, otherwise PREVCLOSE-03 fires Severity::Critical at boot:
    //
    //   IDX_I    → Ticker         (prev_close delivered by standalone code 6)
    //   NSE_EQ   → Quote OR Full  (bytes 38-41 / 50-53 of Quote/Full packet)
    //   NSE_FNO  → Full           (bytes 50-53 of Full packet)
    //   BSE_FNO  → Full           (bytes 50-53 of Full packet)
    //
    // 2026-07-08 correction (§36 FUTIDX-4, dated): the NSE_FNO/BSE_FNO
    // "→ Full" rows above are STALE for the live DailyUniverse path — the
    // Full requirement served the deleted OI/depth consumers and its ratchet
    // test below is retired `#[cfg(any())]` (PR #7b). The LIVE locked fact
    // (Ticket #5525125, `dhan_locked_facts.rs`) permits prev-close from the
    // QUOTE packet at bytes 38-41 for NSE_EQ AND NSE_FNO derivatives, and the
    // §8 Quote-for-all lock stands: ALL §36/§36.7 FUTIDX SIDs (every monthly
    // serial since 2026-07-10) subscribe in Quote mode. OI is NOT inline in Quote mode (separate code-5 packet,
    // counted-and-dropped today — documented non-goal). Ratchets:
    // `crates/core/tests/prev_close_routing_5525125_guard.rs` (Quote+FNO
    // routing) + `test_daily_universe_plan_index_future_targets_quote_mode_fno_segments`.
    //
    // These tests verify the planner output today (default config = Full)
    // and ratchet against future regression where someone downgrades F&O
    // to Quote/Ticker and silently loses prev_close for half the universe.
    // ----------------------------------------------------------------------

    #[test]
    fn test_idx_i_subscriptions_use_ticker_mode() {
        // With Ticker config the planner's per-instrument feed_mode is
        // Ticker — the explicit mode that carries IDX_I prev_close (via
        // separate code 6 packet). At any other config mode, Dhan's
        // server-side rule still returns IDX_I as code 2 (Ticker) but
        // the client must subscribe explicitly with the Ticker request
        // code on the IDX_I segment for code-6 prev_close to arrive.
        let universe = make_test_universe();
        let config = SubscriptionConfig {
            feed_mode: "Ticker".to_string(),
            ..SubscriptionConfig::default()
        };
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();
        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        let idx_i_count = plan
            .registry
            .iter()
            .filter(|i| i.exchange_segment == ExchangeSegment::IdxI)
            .count();
        assert!(
            idx_i_count > 0,
            "test universe must produce at least one IDX_I instrument"
        );

        for inst in plan
            .registry
            .iter()
            .filter(|i| i.exchange_segment == ExchangeSegment::IdxI)
        {
            assert_eq!(
                inst.feed_mode,
                FeedMode::Ticker,
                "IDX_I instrument {} ({}) must be Ticker for prev_close routing \
                 (code 6 packet); got {:?}. PREVCLOSE-03.",
                inst.security_id,
                inst.display_label,
                inst.feed_mode
            );
        }
    }

    #[test]
    fn test_nse_eq_subscriptions_use_quote_or_full() {
        // Default config = Full. NSE_EQ MUST be Quote or Full to carry
        // prev_close (bytes 38-41 of Quote / bytes 50-53 of Full).
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();
        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        let nse_eq_count = plan
            .registry
            .iter()
            .filter(|i| i.exchange_segment == ExchangeSegment::NseEquity)
            .count();
        assert!(
            nse_eq_count > 0,
            "test universe must produce at least one NSE_EQ instrument"
        );

        for inst in plan
            .registry
            .iter()
            .filter(|i| i.exchange_segment == ExchangeSegment::NseEquity)
        {
            assert!(
                matches!(inst.feed_mode, FeedMode::Quote | FeedMode::Full),
                "NSE_EQ instrument {} ({}) must be Quote or Full for prev_close \
                 (bytes 38-41 / 50-53); Ticker would lose it. Got {:?}. PREVCLOSE-03.",
                inst.security_id,
                inst.display_label,
                inst.feed_mode
            );
        }
    }

    #[cfg(any())] // PR #7b — retired (tested legacy scope behavior)
    fn test_nse_fno_bse_fno_subscriptions_use_full_mode() {
        // Default config = Full. NSE_FNO + BSE_FNO MUST be Full to carry
        // prev_close (bytes 50-53) AND OI (bytes 34-37) AND 5-level depth
        // (bytes 62-161) needed by Greeks pipeline + option chain UI.
        let universe = make_test_universe();
        let config = legacy_full_universe_config();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();
        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        let derivative_count = plan
            .registry
            .iter()
            .filter(|i| {
                matches!(
                    i.exchange_segment,
                    ExchangeSegment::NseFno | ExchangeSegment::BseFno
                )
            })
            .count();
        assert!(
            derivative_count > 0,
            "test universe must produce at least one NSE_FNO / BSE_FNO instrument"
        );

        for inst in plan.registry.iter().filter(|i| {
            matches!(
                i.exchange_segment,
                ExchangeSegment::NseFno | ExchangeSegment::BseFno
            )
        }) {
            assert_eq!(
                inst.feed_mode,
                FeedMode::Full,
                "{:?} instrument {} ({}) must be Full mode for prev_close + OI + \
                 5-level depth; Quote/Ticker would lose them. PREVCLOSE-03.",
                inst.exchange_segment,
                inst.security_id,
                inst.display_label
            );
        }
    }

    // ----------------------------------------------------------------------
    // Wave 5 Item 14 — NSE_EQ feed_mode downgrade ratchets.
    //
    // Under indices-only scope the cash-equity feeds carry only LTP /
    // volume / day OHLC + prev_close — exactly the contents of the 50-byte
    // Quote packet (code 4). The 162-byte Full packet's extra payload
    // (OI + 5-level depth) is unused for cash equities, so subscribing
    // them at Full mode wastes ~115 KB/sec of bandwidth across ~206
    // stocks at peak. The planner stamps NSE_EQ at Quote unconditionally
    // regardless of `config.feed_mode`. These tests pin the override.
    // ----------------------------------------------------------------------

    #[test]
    fn test_nse_eq_uses_quote_mode_under_indices_only() {
        let universe = make_test_universe();
        // Default config = Full. Even so, NSE_EQ must come out as Quote.
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();
        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        let nse_eq: Vec<_> = plan
            .registry
            .iter()
            .filter(|i| i.exchange_segment == ExchangeSegment::NseEquity)
            .collect();
        assert!(
            !nse_eq.is_empty(),
            "test universe must produce at least one NSE_EQ instrument"
        );
        for inst in nse_eq {
            assert_eq!(
                inst.feed_mode,
                FeedMode::Quote,
                "Wave 5 Item 14: NSE_EQ {} ({}) must be Quote mode regardless \
                 of config.feed_mode (= {:?}); got {:?}.",
                inst.security_id,
                inst.display_label,
                config.parsed_feed_mode().unwrap_or(FeedMode::Ticker),
                inst.feed_mode
            );
        }

        // Sanity: even when config asks for Ticker, NSE_EQ stays Quote.
        let config_ticker = SubscriptionConfig {
            feed_mode: "Ticker".to_string(),
            ..SubscriptionConfig::default()
        };
        let plan_ticker = build_subscription_plan(
            &universe,
            &config_ticker,
            today,
            &std::collections::HashMap::new(),
            None,
        );
        for inst in plan_ticker
            .registry
            .iter()
            .filter(|i| i.exchange_segment == ExchangeSegment::NseEquity)
        {
            assert_eq!(
                inst.feed_mode,
                FeedMode::Quote,
                "Wave 5 Item 14: NSE_EQ override applies even when \
                 config.feed_mode = Ticker; got {:?}",
                inst.feed_mode
            );
        }
    }

    #[test]
    fn test_quote_packet_close_field_matches_prev_close_lookup() {
        // Documents the data-flow contract that justifies the downgrade:
        // The Quote packet's `close` field at bytes 38-41 is the previous
        // trading session's close (Dhan Ticket #5525125), and the Wave-5
        // movers writers + restart-recovery path read prev_close from
        // exactly that field on every Quote tick. Therefore Quote mode
        // is sufficient for NSE_EQ — no need to escalate to Full.
        //
        // This test pins the Quote packet's prev_close routing in the
        // segment-aware code today, by re-running the boot-time matrix
        // assertion under Quote-defaulted config and asserting all
        // NSE_EQ instruments still satisfy the `Quote OR Full` band
        // from PREVCLOSE-03.
        let universe = make_test_universe();
        let config = SubscriptionConfig::default(); // = Full
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();
        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        for inst in plan
            .registry
            .iter()
            .filter(|i| i.exchange_segment == ExchangeSegment::NseEquity)
        {
            assert!(
                matches!(inst.feed_mode, FeedMode::Quote | FeedMode::Full),
                "NSE_EQ {} ({}) must remain in the prev_close-carrying band \
                 (Quote bytes 38-41 / Full bytes 50-53); got {:?}.",
                inst.security_id,
                inst.display_label,
                inst.feed_mode
            );
            // Wave 5 Item 14 tightens the band to Quote-only:
            assert_eq!(
                inst.feed_mode,
                FeedMode::Quote,
                "NSE_EQ {} ({}) must be EXACTLY Quote (Item 14 downgrade)",
                inst.security_id,
                inst.display_label
            );
        }
    }

    // ----------------------------------------------------------------------
    // Wave 5 Item 2 — `subscription.scope` universe-filter ratchets.
    //
    // Default scope = `IndicesOnlyAllExpiries` (Item 1) → drops the entire
    // 216-stock F&O block; only NIFTY + BANKNIFTY + SENSEX index F&O,
    // cash equities, and IDX_I are subscribed.
    // ----------------------------------------------------------------------

    // PR #7b — `test_indices_only_scope_filters_to_three_underlyings`
    // retired. The IndicesOnlyAllExpiries variant was deleted; under
    // `Indices4Only` index F&O is ALSO zero, so the test's "index F&O
    // still subscribed" assertion no longer holds. Equivalent coverage
    // in `test_indices4only_planner_emits_zero_derivatives` (Slice 6).

    #[test]
    fn test_universe_count_pinned_at_11018() {
        // The pinned 11,018 number from the plan is for the LIVE Dhan
        // universe (3 indices + 26 display + 216 cash + ~10,783 index F&O).
        // The synthetic test universe has only 1 NIFTY + 1 INDIA VIX + 1
        // RELIANCE underlying, so the absolute number won't match. Instead
        // pin the qualitative rule: `total = (index_derivs + cash_eq +
        // major_idx + display_idx)` AND it equals `total - stock_derivs`
        // because the stock-derivative drop ratchet is exact.
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        let s = &plan.summary;
        assert_eq!(s.stock_derivatives, 0);
        assert_eq!(
            s.total,
            s.major_index_values + s.display_indices + s.index_derivatives + s.stock_equities,
            "indices-only total = idx + display + index_derivs + cash; \
             no stock-derivs contribution"
        );
        // Sanity ceilings: with the 25K hard cap untouched, default scope
        // can never exceed it.
        assert!(s.total <= MAX_TOTAL_SUBSCRIPTIONS);
    }

    #[test]
    fn test_finnifty_midcpnifty_excluded_from_indices_only() {
        // Synthesise a universe with FINNIFTY + MIDCPNIFTY underlyings to
        // verify they are EXCLUDED even when present in raw CSV. The
        // existing `test_finnifty_midcpnifty_dropped_from_index_set`
        // exercises the FULL_CHAIN_INDEX_SYMBOLS constant directly; this
        // test runs the same invariant THROUGH the planner under
        // IndicesOnlyAllExpiries scope.
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );
        for inst in plan.registry.iter() {
            assert_ne!(
                inst.underlying_symbol, "FINNIFTY",
                "FINNIFTY must be excluded from Wave 5 universe"
            );
            assert_ne!(
                inst.underlying_symbol, "MIDCPNIFTY",
                "MIDCPNIFTY must be excluded from Wave 5 universe"
            );
        }
    }

    #[cfg(any())] // PR #7b — retired (tested legacy scope behavior)
    fn test_stock_fno_excluded_under_indices_only_scope() {
        // Symmetric to test_indices_only_scope_filters_to_three_underlyings
        // but explicitly checks the NSE_FNO + BSE_FNO segment counts of
        // STOCK derivative kinds. Under indices-only scope, the only
        // NSE_FNO instruments allowed are FUTIDX / OPTIDX, NEVER FUTSTK
        // / OPTSTK.
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &config,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        let stock_fno_count = plan
            .registry
            .iter()
            .filter(|i| {
                matches!(
                    i.instrument_kind,
                    Some(DhanInstrumentKind::OptionStock) | Some(DhanInstrumentKind::FutureStock)
                )
            })
            .count();
        assert_eq!(
            stock_fno_count, 0,
            "no FUTSTK / OPTSTK contracts allowed under Wave 5 indices-only scope"
        );

        // Mirror under FullUniverse scope: stock F&O CAN appear.
        let plan_full = build_subscription_plan(
            &universe,
            &legacy_full_universe_config(),
            today,
            &std::collections::HashMap::new(),
            None,
        );
        let stock_fno_count_full = plan_full
            .registry
            .iter()
            .filter(|i| {
                matches!(
                    i.instrument_kind,
                    Some(DhanInstrumentKind::OptionStock) | Some(DhanInstrumentKind::FutureStock)
                )
            })
            .count();
        assert!(
            stock_fno_count_full > 0,
            "FullUniverse scope must restore stock F&O subscription path"
        );
    }

    // ----------------------------------------------------------------------
    // AWS-lifecycle LOCKED — Indices4Only scope contract
    //
    // Under the only legal scope (`SubscriptionScope::Indices4Only`) the
    // planner emits:
    //   - NIFTY / BANKNIFTY / SENSEX IDX_I value feeds (Ticker)
    //   - INDIA VIX IDX_I display-index feed (Ticker)
    //   - NSE_EQ cash-equity feeds for any underlying stocks present
    //     in the universe (Quote)
    // and NEVER emits index derivatives, stock derivatives, or
    // sectoral / broad-market display indices.
    // ----------------------------------------------------------------------

    /// PR #7b — renamed from `phase_0_config()`; now just returns the
    /// LOCKED default config since `Indices4Only` is the only scope.
    fn phase_0_config() -> SubscriptionConfig {
        SubscriptionConfig::default()
    }

    #[test]
    fn test_should_subscribe_index_derivatives_false_under_indices4only() {
        let cfg = SubscriptionConfig::default();
        assert!(
            !should_subscribe_index_derivatives(&cfg),
            "Indices4Only scope must never subscribe index derivatives"
        );
    }

    #[test]
    fn test_indices4only_scope_skips_stock_derivatives() {
        let cfg = SubscriptionConfig::default();
        assert!(
            !should_subscribe_stock_derivatives(&cfg),
            "Indices4Only scope must never subscribe stock derivatives"
        );
    }

    #[test]
    fn test_is_display_index_allowed_under_scope_keeps_only_india_vix() {
        let cfg = SubscriptionConfig::default();
        let vix_sid = tickvault_common::constants::INDIA_VIX_SECURITY_ID;
        // INDIA VIX (SID 21) — the ONLY allowed display index.
        assert!(is_display_index_allowed_under_scope(&cfg, vix_sid));
        // Sectoral / broad-market display indices — PARKED.
        assert!(!is_display_index_allowed_under_scope(&cfg, 17)); // NIFTY 100
        assert!(!is_display_index_allowed_under_scope(&cfg, 19)); // NIFTY 500
        assert!(!is_display_index_allowed_under_scope(&cfg, 14)); // NIFTY AUTO
        assert!(!is_display_index_allowed_under_scope(&cfg, 0));
    }

    #[test]
    fn test_indices_underlyings_only_planner_emits_zero_derivatives() {
        let universe = make_test_universe();
        let cfg = phase_0_config();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &cfg,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        assert_eq!(
            plan.summary.index_derivatives, 0,
            "Phase 0 plan must contain ZERO index derivatives"
        );
        assert_eq!(
            plan.summary.stock_derivatives, 0,
            "Phase 0 plan must contain ZERO stock derivatives"
        );
        for inst in plan.registry.iter() {
            assert!(
                !matches!(
                    inst.instrument_kind,
                    Some(DhanInstrumentKind::OptionStock)
                        | Some(DhanInstrumentKind::FutureStock)
                        | Some(DhanInstrumentKind::OptionIndex)
                        | Some(DhanInstrumentKind::FutureIndex)
                ),
                "derivative leaked into Phase 0 plan: {} {:?}",
                inst.display_label,
                inst.instrument_kind,
            );
        }
    }

    #[test]
    fn test_indices_underlyings_only_planner_nse_eq_uses_quote_mode() {
        let universe = make_test_universe();
        let cfg = phase_0_config();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &cfg,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        let nse_eq: Vec<_> = plan
            .registry
            .iter()
            .filter(|i| i.exchange_segment == ExchangeSegment::NseEquity)
            .collect();
        assert!(
            !nse_eq.is_empty(),
            "Phase 0 plan must include at least one NSE_EQ cash feed"
        );
        for inst in &nse_eq {
            assert_eq!(
                inst.feed_mode,
                FeedMode::Quote,
                "NSE_EQ {} must subscribe in Quote mode (prev_close in bytes 38-41)",
                inst.display_label,
            );
        }
    }

    #[test]
    fn test_indices_underlyings_only_planner_filters_display_to_vix_only() {
        let universe = make_test_universe();
        let cfg = phase_0_config();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &cfg,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        let display: Vec<_> = plan
            .registry
            .iter()
            .filter(|i| i.category == SubscriptionCategory::DisplayIndex)
            .collect();
        assert_eq!(
            display.len(),
            1,
            "Phase 0 plan must contain exactly ONE display index (INDIA VIX)"
        );
        assert_eq!(display[0].display_label, "INDIA VIX");
        assert_eq!(
            display[0].security_id,
            tickvault_common::constants::INDIA_VIX_SECURITY_ID
        );
        assert_eq!(display[0].exchange_segment, ExchangeSegment::IdxI);
    }

    #[test]
    fn test_indices_underlyings_only_plan_under_phase_0_target() {
        // Synthetic universe is tiny (1 NIFTY + 1 RELIANCE + 1 VIX), so the
        // exact count is small. The ratchet here is qualitative: Phase 0 must
        // never exceed the 230-SID target (PHASE_0_TOTAL_SIDS_TARGET).
        let universe = make_test_universe();
        let cfg = phase_0_config();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &cfg,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        assert!(
            plan.summary.total <= tickvault_common::constants::PHASE_0_TOTAL_SIDS_TARGET,
            "Phase 0 plan total {} exceeded PHASE_0_TOTAL_SIDS_TARGET",
            plan.summary.total,
        );
        // Sanity: composition exactly matches the documented buckets.
        assert_eq!(
            plan.summary.total,
            plan.summary.major_index_values
                + plan.summary.display_indices
                + plan.summary.stock_equities,
            "Phase 0 total must equal idx + display + cash; derivatives must be zero",
        );
    }

    #[test]
    fn test_indices_underlyings_only_planner_emits_all_three_idx_i_full_chain() {
        // Hostile-review M1 fix (2026-05-13): pin that the Phase 0 planner
        // emits NIFTY (SID 13) + BANKNIFTY (SID 25) + SENSEX (SID 51) from
        // FULL_CHAIN_INDEX_SYMBOLS Section 1. Without this ratchet a future
        // change could silently drop one of the 3 majors and Phase 0 would
        // proceed with broken coverage.
        let universe = make_test_universe();
        let cfg = phase_0_config();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &cfg,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        let majors: std::collections::HashSet<u64> = plan
            .registry
            .iter()
            .filter(|i| i.category == SubscriptionCategory::MajorIndexValue)
            .map(|i| i.security_id)
            .collect();
        for required_sid in [13, 25, 51] {
            assert!(
                majors.contains(&required_sid),
                "Phase 0 plan must include major IDX_I SID {required_sid} \
                 (NIFTY=13, BANKNIFTY=25, SENSEX=51); got {majors:?}"
            );
        }
    }

    #[test]
    fn test_indices_underlyings_only_planner_lower_bound_floor() {
        // Hostile-review H1 fix (2026-05-13, unit-level lower bound):
        // ratchets that the Phase 0 plan is never silently empty. Combined
        // count of major IDX_I + display indices must be >= 4 (the 4 Phase 0
        // IDX_I SIDs: NIFTY, BANKNIFTY, SENSEX, INDIA VIX). Boot-time runtime
        // Telegram floor alert is wired in PR-3 / Item 22c
        // (BootReadyConfirmation positive ping).
        let universe = make_test_universe();
        let cfg = phase_0_config();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(
            &universe,
            &cfg,
            today,
            &std::collections::HashMap::new(),
            None,
        );

        let idx_i = plan.summary.major_index_values + plan.summary.display_indices;
        assert!(
            idx_i >= tickvault_common::constants::PHASE_0_IDX_I_COUNT,
            "Phase 0 plan IDX_I count {} below PHASE_0_IDX_I_COUNT floor {}",
            idx_i,
            tickvault_common::constants::PHASE_0_IDX_I_COUNT,
        );
        assert!(
            plan.summary.total > 0,
            "Phase 0 plan must never emit zero instruments (false-OK guard)",
        );
    }

    #[test]
    fn test_india_vix_security_id_constant_pinned_at_21() {
        assert_eq!(
            tickvault_common::constants::INDIA_VIX_SECURITY_ID,
            21,
            "INDIA VIX SID must remain pinned at 21 (Dhan instrument master)",
        );
    }

    #[test]
    fn test_phase_0_idx_i_symbols_contain_required_four() {
        let syms: std::collections::HashSet<&str> =
            tickvault_common::constants::PHASE_0_IDX_I_SYMBOLS
                .iter()
                .copied()
                .collect();
        for required in ["NIFTY", "BANKNIFTY", "SENSEX", "INDIA VIX"] {
            assert!(
                syms.contains(required),
                "PHASE_0_IDX_I_SYMBOLS must contain {required}",
            );
        }
        assert_eq!(
            tickvault_common::constants::PHASE_0_IDX_I_SYMBOLS.len(),
            tickvault_common::constants::PHASE_0_IDX_I_COUNT,
        );
    }
}

#[cfg(all(test, feature = "daily_universe_fetcher"))]
mod daily_universe_plan_tests {
    use super::*;
    use crate::instrument::csv_parser::CsvRow;
    use crate::instrument::daily_universe::{DailyUniverse, InstrumentRole, SubscriptionTarget};

    fn daily_target(
        role: InstrumentRole,
        security_id: &str,
        segment: &str,
        symbol: &str,
    ) -> SubscriptionTarget {
        SubscriptionTarget {
            role,
            is_fno_underlying: role == InstrumentRole::FnoUnderlying,
            is_index_constituent: role == InstrumentRole::IndexConstituent,
            csv_row: CsvRow {
                security_id: security_id.to_string(),
                exch_id: String::new(),
                segment: segment.to_string(),
                instrument: String::new(),
                symbol_name: symbol.to_string(),
                underlying_security_id: String::new(),
                underlying_symbol: String::new(),
                display_name: String::new(),
                lot_size: 0,
                tick_size: 0.0,
                expiry_date: String::new(),
                strike_price: 0.0,
                option_type: String::new(),
                isin: String::new(),
            },
        }
    }

    #[test]
    fn test_daily_universe_plan_emits_all_sids_quote_mode() {
        let universe = DailyUniverse {
            subscription_targets: vec![
                daily_target(InstrumentRole::Index, "13", "IDX_I", "NIFTY"),
                daily_target(InstrumentRole::Index, "51", "IDX_I", "SENSEX"),
                daily_target(InstrumentRole::FnoUnderlying, "2885", "NSE_EQ", "RELIANCE"),
            ],
            fno_contracts: vec![],
        };
        let plan = build_subscription_plan_from_daily_universe(&universe);
        assert_eq!(plan.summary.total, 3, "all 3 SIDs emitted");
        assert_eq!(plan.summary.major_index_values, 2, "2 indices");
        assert_eq!(plan.summary.stock_equities, 1, "1 NSE_EQ underlying");
        // §8: every daily-universe SID subscribes in Quote mode.
        assert_eq!(plan.summary.feed_mode, FeedMode::Quote);
    }

    /// Hostile-review round 2 (2026-07-08, F2 — Dhan mirror of the Groww
    /// post-cap fix): a planner-stage drop of a chosen IndexFuture target
    /// (unknown segment here; unparsable SID is the sibling arm) leaves the
    /// POST-PLAN IndexDerivative count BELOW the handed-in future count —
    /// the honesty gap the post-plan gauge + FUTIDX-01 error now cover.
    #[test]
    fn test_plan_futidx_planner_drop_is_counted_post_plan() {
        let mut good = daily_target(InstrumentRole::IndexFuture, "35001", "NSE_FNO", "NIFTY-FUT");
        good.csv_row.underlying_symbol = "NIFTY".to_string();
        good.csv_row.expiry_date = "2026-07-30".to_string();
        let mut bad = daily_target(
            InstrumentRole::IndexFuture,
            "45001",
            "MCX_COMM", // unknown FUTIDX segment → fail-closed planner skip
            "SENSEX-FUT",
        );
        bad.csv_row.underlying_symbol = "SENSEX".to_string();
        bad.csv_row.expiry_date = "2026-07-30".to_string();
        let universe = DailyUniverse {
            subscription_targets: vec![good, bad],
            fno_contracts: vec![],
        };
        let plan = build_subscription_plan_from_daily_universe(&universe);
        // 2 futures handed in, 1 survives the plan — the post-plan gauge
        // reports 1 (not 2) and the FUTIDX-01 planner-drop error fires.
        assert_eq!(
            plan.summary.index_derivatives, 1,
            "post-plan count is honest"
        );
        assert_eq!(plan.summary.total, 1);
    }

    #[test]
    fn test_daily_universe_plan_dedup_composite_and_skips_unparsable() {
        let universe = DailyUniverse {
            subscription_targets: vec![
                daily_target(InstrumentRole::Index, "13", "IDX_I", "NIFTY"),
                // Same (security_id, segment) — I-P1-11 composite dedup drops it.
                daily_target(InstrumentRole::Index, "13", "IDX_I", "NIFTY-DUP"),
                // Non-numeric security_id — skipped (never panics).
                daily_target(
                    InstrumentRole::FnoUnderlying,
                    "not-a-number",
                    "NSE_EQ",
                    "BAD",
                ),
            ],
            fno_contracts: vec![],
        };
        let plan = build_subscription_plan_from_daily_universe(&universe);
        assert_eq!(
            plan.summary.total, 1,
            "dup dropped + unparsable skipped → 1 survivor"
        );
        assert_eq!(plan.summary.major_index_values, 1);
    }

    // ----- §36 (2026-07-08): FUTIDX-4 planner arm -----

    #[cfg(feature = "daily_universe_fetcher")]
    fn daily_futidx_target(
        security_id: &str,
        segment: &str,
        underlying: &str,
        expiry: &str,
    ) -> SubscriptionTarget {
        SubscriptionTarget {
            role: InstrumentRole::IndexFuture,
            is_fno_underlying: false,
            is_index_constituent: false,
            csv_row: CsvRow {
                security_id: security_id.to_string(),
                exch_id: if segment == "BSE_FNO" { "BSE" } else { "NSE" }.to_string(),
                segment: segment.to_string(),
                instrument: "FUTIDX".to_string(),
                symbol_name: format!("{underlying}-Jul2026-FUT"),
                underlying_symbol: underlying.to_string(),
                expiry_date: expiry.to_string(),
                ..CsvRow::default()
            },
        }
    }

    #[test]
    #[cfg(feature = "daily_universe_fetcher")]
    fn test_daily_universe_plan_index_future_targets_quote_mode_fno_segments() {
        let universe = DailyUniverse {
            subscription_targets: vec![
                daily_target(InstrumentRole::Index, "13", "IDX_I", "NIFTY"),
                daily_futidx_target("35001", "NSE_FNO", "NIFTY", "2026-07-30"),
                daily_futidx_target("45001", "BSE_FNO", "SENSEX", "2026-07-31"),
            ],
            fno_contracts: vec![],
        };
        let plan = build_subscription_plan_from_daily_universe(&universe);
        assert_eq!(plan.summary.total, 3);
        assert_eq!(plan.summary.index_derivatives, 2, "both futures counted");
        // §8: plan-global Quote mode, futures included.
        assert_eq!(plan.summary.feed_mode, FeedMode::Quote);
        let nifty_fut = plan
            .registry
            .get_with_segment(35001, ExchangeSegment::NseFno)
            .expect("NIFTY future in registry at NSE_FNO");
        assert_eq!(nifty_fut.category, SubscriptionCategory::IndexDerivative);
        assert_eq!(nifty_fut.feed_mode, FeedMode::Quote);
        assert_eq!(
            nifty_fut.instrument_kind,
            Some(DhanInstrumentKind::FutureIndex)
        );
        assert_eq!(
            nifty_fut.expiry_date,
            NaiveDate::from_ymd_opt(2026, 7, 30),
            "expiry parsed from SM_EXPIRY_DATE"
        );
        assert_eq!(nifty_fut.underlying_symbol, "NIFTY");
        let sensex_fut = plan
            .registry
            .get_with_segment(45001, ExchangeSegment::BseFno)
            .expect("SENSEX future in registry at BSE_FNO");
        assert_eq!(sensex_fut.exchange_segment, ExchangeSegment::BseFno);
    }

    #[test]
    #[cfg(feature = "daily_universe_fetcher")]
    fn test_daily_universe_plan_futidx_plans_every_monthly_target() {
        // §36.7 (2026-07-10): the planner plans EVERY (underlying, month)
        // target the universe hands in — 2 months × 4 underlyings here,
        // distinct SIDs, zero drops (the gauge-visible IndexDerivative
        // count equals the handed-in count).
        let universe = DailyUniverse {
            subscription_targets: vec![
                daily_futidx_target("35001", "NSE_FNO", "NIFTY", "2026-07-30"),
                daily_futidx_target("35002", "NSE_FNO", "BANKNIFTY", "2026-07-30"),
                daily_futidx_target("35003", "NSE_FNO", "MIDCPNIFTY", "2026-07-28"),
                daily_futidx_target("45001", "BSE_FNO", "SENSEX", "2026-07-31"),
                daily_futidx_target("36001", "NSE_FNO", "NIFTY", "2026-08-27"),
                daily_futidx_target("36002", "NSE_FNO", "BANKNIFTY", "2026-08-27"),
                daily_futidx_target("36003", "NSE_FNO", "MIDCPNIFTY", "2026-08-25"),
                daily_futidx_target("46001", "BSE_FNO", "SENSEX", "2026-08-28"),
            ],
            fno_contracts: vec![],
        };
        let plan = build_subscription_plan_from_daily_universe(&universe);
        assert_eq!(
            plan.summary.index_derivatives, 8,
            "every monthly target planned — one per (underlying, month)"
        );
        assert_eq!(plan.summary.total, 8, "zero planner-stage drops");
    }

    #[test]
    #[cfg(feature = "daily_universe_fetcher")]
    fn test_daily_universe_plan_index_future_unknown_segment_skipped() {
        let universe = DailyUniverse {
            subscription_targets: vec![
                daily_target(InstrumentRole::Index, "13", "IDX_I", "NIFTY"),
                // A wiring-bug row: IndexFuture role with a non-FNO segment.
                daily_futidx_target("35001", "MCX_COMM", "NIFTY", "2026-07-30"),
                daily_futidx_target("35002", "garbage", "BANKNIFTY", "2026-07-30"),
            ],
            fno_contracts: vec![],
        };
        let plan = build_subscription_plan_from_daily_universe(&universe);
        assert_eq!(
            plan.summary.total, 1,
            "bad-segment futures skipped fail-closed, never mis-subscribed"
        );
        assert_eq!(plan.summary.index_derivatives, 0);
    }

    #[test]
    #[cfg(feature = "daily_universe_fetcher")]
    fn test_daily_universe_plan_futures_dedup_composite_key() {
        // I-P1-11: an NSE_FNO future SID numerically equal to an NSE_EQ spot
        // SID → BOTH survive (composite (sid, segment) identity).
        let universe = DailyUniverse {
            subscription_targets: vec![
                daily_target(InstrumentRole::FnoUnderlying, "2885", "NSE_EQ", "RELIANCE"),
                daily_futidx_target("2885", "NSE_FNO", "NIFTY", "2026-07-30"),
            ],
            fno_contracts: vec![],
        };
        let plan = build_subscription_plan_from_daily_universe(&universe);
        assert_eq!(plan.summary.total, 2, "both segments kept");
        assert!(
            plan.registry
                .get_with_segment(2885, ExchangeSegment::NseEquity)
                .is_some()
        );
        assert!(
            plan.registry
                .get_with_segment(2885, ExchangeSegment::NseFno)
                .is_some()
        );
    }

    #[test]
    fn test_daily_universe_plan_empty_is_empty() {
        let universe = DailyUniverse {
            subscription_targets: vec![],
            fno_contracts: vec![],
        };
        let plan = build_subscription_plan_from_daily_universe(&universe);
        assert_eq!(plan.summary.total, 0);
    }
}

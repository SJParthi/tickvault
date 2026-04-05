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

use dhan_live_trader_common::config::SubscriptionConfig;
use dhan_live_trader_common::constants::{FULL_CHAIN_INDEX_SYMBOLS, MAX_TOTAL_SUBSCRIPTIONS};
use dhan_live_trader_common::instrument_registry::{
    InstrumentRegistry, SubscribedInstrument, SubscriptionCategory, make_derivative_instrument,
    make_derivative_instrument_from_archived, make_display_index_instrument,
    make_major_index_instrument, make_stock_equity_instrument,
    make_stock_equity_instrument_from_archived,
};
use dhan_live_trader_common::instrument_types::{
    ArchivedFnoUniverse, DhanInstrumentKind, FnoUniverse, IndexCategory, OptionChainKey,
    UnderlyingKind, naive_date_from_archived_i32,
};
use dhan_live_trader_common::types::FeedMode;

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
// Planner
// ---------------------------------------------------------------------------

/// Builds a subscription plan from the FnoUniverse and configuration.
///
/// # Strategy
/// - **Indices (NIFTY, BANKNIFTY, SENSEX, MIDCPNIFTY, FINNIFTY):**
///   Subscribe the index value feed (IDX_I) + ALL derivative contracts
///   (all expiries, all strikes). No filtering whatsoever.
///
/// - **Display indices (INDIA VIX, sectoral, broad market):**
///   Subscribe the index value feed only (IDX_I). No derivatives.
///
/// - **Stocks (~206):**
///   Subscribe the equity price feed (NSE_EQ) + current expiry only +
///   ATM ± N strikes (CE + PE) + current-month future.
///   ATM is approximated using the middle strike of the current expiry chain
///   (since no live prices are available at startup).
///
/// # Feed Mode
/// All instruments use the same feed mode from config (Ticker by default).
pub fn build_subscription_plan(
    universe: &FnoUniverse,
    config: &SubscriptionConfig,
    today: NaiveDate,
) -> SubscriptionPlan {
    let feed_mode = config.parsed_feed_mode().unwrap_or(FeedMode::Ticker);

    let mut instruments: Vec<SubscribedInstrument> = Vec::with_capacity(MAX_TOTAL_SUBSCRIPTIONS);
    let mut seen_ids: HashSet<u32> = HashSet::with_capacity(MAX_TOTAL_SUBSCRIPTIONS);
    let mut stocks_skipped_no_chain: usize = 0;

    let full_chain_set: HashSet<&str> = FULL_CHAIN_INDEX_SYMBOLS.iter().copied().collect();

    // -----------------------------------------------------------------------
    // 1. Major index value feeds (IDX_I) — 5 symbols
    // -----------------------------------------------------------------------
    for underlying in universe.underlyings.values() {
        if !full_chain_set.contains(underlying.underlying_symbol.as_str()) {
            continue;
        }
        // Index value feed (IDX_I segment)
        if seen_ids.insert(underlying.price_feed_security_id) {
            instruments.push(make_major_index_instrument(
                underlying.price_feed_security_id,
                &underlying.underlying_symbol,
                feed_mode,
            ));
        }
    }

    // -----------------------------------------------------------------------
    // 2. Display indices (IDX_I) — 23 symbols
    // -----------------------------------------------------------------------
    if config.subscribe_display_indices {
        for index in &universe.subscribed_indices {
            if index.category == IndexCategory::DisplayIndex && seen_ids.insert(index.security_id) {
                instruments.push(make_display_index_instrument(
                    index.security_id,
                    &index.symbol,
                    feed_mode,
                ));
            }
        }
    }

    // -----------------------------------------------------------------------
    // 3. Index derivatives — ALL contracts for 5 major indices (no filtering)
    // -----------------------------------------------------------------------
    if config.subscribe_index_derivatives {
        for contract in universe.derivative_contracts.values() {
            let is_index = matches!(
                contract.instrument_kind,
                dhan_live_trader_common::instrument_types::DhanInstrumentKind::FutureIndex
                    | dhan_live_trader_common::instrument_types::DhanInstrumentKind::OptionIndex
            );
            if is_index
                && full_chain_set.contains(contract.underlying_symbol.as_str())
                && seen_ids.insert(contract.security_id)
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
    for underlying in universe.underlyings.values() {
        if underlying.kind != UnderlyingKind::Stock {
            continue;
        }

        // 4a. Stock equity price feed
        if config.subscribe_stock_equities && seen_ids.insert(underlying.price_feed_security_id) {
            instruments.push(make_stock_equity_instrument(underlying, feed_mode));
        }

        // 4b. Stock derivatives — current expiry only, ATM ± N strikes
        if !config.subscribe_stock_derivatives {
            continue;
        }

        // Find current expiry (first expiry >= today)
        let current_expiry = universe
            .expiry_calendars
            .get(&underlying.underlying_symbol)
            .and_then(|cal| cal.expiry_dates.iter().find(|d| **d >= today).copied());

        let Some(expiry) = current_expiry else {
            debug!(
                underlying = %underlying.underlying_symbol,
                "No current expiry found — skipping stock derivatives"
            );
            stocks_skipped_no_chain = stocks_skipped_no_chain.saturating_add(1);
            continue;
        };

        // Subscribe the current-month future for this expiry
        let chain_key = OptionChainKey {
            underlying_symbol: underlying.underlying_symbol.clone(),
            expiry_date: expiry,
        };

        if let Some(chain) = universe.option_chains.get(&chain_key) {
            // Future for this expiry
            if let Some(future_id) = chain.future_security_id
                && let Some(future_contract) = universe.derivative_contracts.get(&future_id)
                && seen_ids.insert(future_id)
            {
                instruments.push(make_derivative_instrument(
                    future_contract,
                    SubscriptionCategory::StockDerivative,
                    feed_mode,
                ));
            }

            // ATM ± N strike filtering
            // At startup we don't have live prices, so use the middle strike
            // of the chain as ATM approximation.
            let atm_above = config.stock_atm_strikes_above;
            let atm_below = config.stock_atm_strikes_below;

            // Calls — sorted ascending by strike
            let call_count = chain.calls.len();
            if call_count > 0 {
                let mid_idx = call_count / 2;
                let start = mid_idx.saturating_sub(atm_below);
                let end = mid_idx
                    .saturating_add(atm_above)
                    .saturating_add(1)
                    .min(call_count);
                for entry in &chain.calls[start..end] {
                    if let Some(contract) = universe.derivative_contracts.get(&entry.security_id)
                        && seen_ids.insert(entry.security_id)
                    {
                        instruments.push(make_derivative_instrument(
                            contract,
                            SubscriptionCategory::StockDerivative,
                            feed_mode,
                        ));
                    }
                }
            }

            // Puts — sorted ascending by strike
            let put_count = chain.puts.len();
            if put_count > 0 {
                let mid_idx = put_count / 2;
                let start = mid_idx.saturating_sub(atm_below);
                let end = mid_idx
                    .saturating_add(atm_above)
                    .saturating_add(1)
                    .min(put_count);
                for entry in &chain.puts[start..end] {
                    if let Some(contract) = universe.derivative_contracts.get(&entry.security_id)
                        && seen_ids.insert(entry.security_id)
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
    //    Stage 2: ALL stock derivatives with expiry >= today, nearest first.
    //    Stage 1 (ATM+-N above) already added priority instruments.
    //    seen_ids ensures no duplicates.
    // -----------------------------------------------------------------------
    let mut stock_derivatives_available: usize = 0;
    let mut stock_derivatives_skipped: usize = 0;

    if config.subscribe_stock_derivatives {
        // O(1) EXEMPT: begin — planner runs once at startup, not per tick
        let count_before_stage2 = instruments.len();

        let mut remaining_stock_derivatives: Vec<
            &dhan_live_trader_common::instrument_types::DerivativeContract,
        > = universe
            .derivative_contracts
            .values()
            .filter(|c| {
                matches!(
                    c.instrument_kind,
                    dhan_live_trader_common::instrument_types::DhanInstrumentKind::FutureStock
                        | dhan_live_trader_common::instrument_types::DhanInstrumentKind::OptionStock
                ) && c.expiry_date >= today
                    && !seen_ids.contains(&c.security_id)
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
            if seen_ids.insert(contract.security_id) {
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

    let mut instruments: Vec<SubscribedInstrument> = Vec::with_capacity(MAX_TOTAL_SUBSCRIPTIONS);
    let mut seen_ids: HashSet<u32> = HashSet::with_capacity(MAX_TOTAL_SUBSCRIPTIONS);
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
    // 1. Major index value feeds (IDX_I)
    // -----------------------------------------------------------------------
    for underlying in universe.underlyings.values() {
        let symbol = underlying.underlying_symbol.as_str();
        if !full_chain_set.contains(symbol) {
            continue;
        }
        let price_id = underlying.price_feed_security_id.to_native();
        if seen_ids.insert(price_id) {
            instruments.push(make_major_index_instrument(price_id, symbol, feed_mode));
        }
    }

    // -----------------------------------------------------------------------
    // 2. Display indices (IDX_I)
    // -----------------------------------------------------------------------
    if config.subscribe_display_indices {
        for index in universe.subscribed_indices.iter() {
            let cat = IndexCategory::from(&index.category);
            let sec_id = index.security_id.to_native();
            if cat == IndexCategory::DisplayIndex && seen_ids.insert(sec_id) {
                instruments.push(make_display_index_instrument(
                    sec_id,
                    index.symbol.as_str(),
                    feed_mode,
                ));
            }
        }
    }

    // -----------------------------------------------------------------------
    // 3. Index derivatives — ALL contracts for major indices
    // -----------------------------------------------------------------------
    if config.subscribe_index_derivatives {
        for contract in universe.derivative_contracts.values() {
            let kind = DhanInstrumentKind::from(&contract.instrument_kind);
            let is_index = matches!(
                kind,
                DhanInstrumentKind::FutureIndex | DhanInstrumentKind::OptionIndex
            );
            let sec_id = contract.security_id.to_native();
            if is_index
                && full_chain_set.contains(contract.underlying_symbol.as_str())
                && seen_ids.insert(sec_id)
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
    for underlying in universe.underlyings.values() {
        let kind = UnderlyingKind::from(&underlying.kind);
        if kind != UnderlyingKind::Stock {
            continue;
        }

        let symbol = underlying.underlying_symbol.as_str();

        // 4a. Stock equity price feed
        let price_id = underlying.price_feed_security_id.to_native();
        if config.subscribe_stock_equities && seen_ids.insert(price_id) {
            instruments.push(make_stock_equity_instrument_from_archived(
                underlying, feed_mode,
            ));
        }

        // 4b. Stock derivatives — current expiry only, ATM ± N strikes
        if !config.subscribe_stock_derivatives {
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

        // Look up option chain via our pre-built index
        if let Some(&chain_idx) = option_chain_lookup.get(&(symbol, expiry)) {
            let (_, chain) = &option_chain_entries[chain_idx];

            // Future for this expiry
            if let Some(archived_future_id) = chain.future_security_id.as_ref() {
                let future_id: u32 = archived_future_id.to_native();
                if seen_ids.insert(future_id) {
                    // Look up the future contract in derivative_contracts
                    let archived_key = rkyv::rend::u32_le::from_native(future_id);
                    if let Some(future_contract) = universe.derivative_contracts.get(&archived_key)
                    {
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
                    let archived_key = rkyv::rend::u32_le::from_native(sec_id);
                    if let Some(contract) = universe.derivative_contracts.get(&archived_key)
                        && seen_ids.insert(sec_id)
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
                    let archived_key = rkyv::rend::u32_le::from_native(sec_id);
                    if let Some(contract) = universe.derivative_contracts.get(&archived_key)
                        && seen_ids.insert(sec_id)
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

    if config.subscribe_stock_derivatives {
        // O(1) EXEMPT: begin — planner runs once at startup, not per tick
        let count_before_stage2 = instruments.len();

        let mut remaining_stock_derivatives: Vec<(u32, NaiveDate, &str, &_)> = universe
            .derivative_contracts
            .values()
            .filter_map(|c| {
                let kind = DhanInstrumentKind::from(&c.instrument_kind);
                let is_stock_deriv = matches!(
                    kind,
                    DhanInstrumentKind::FutureStock | DhanInstrumentKind::OptionStock
                );
                let expiry = naive_date_from_archived_i32(&c.expiry_date);
                let sec_id = c.security_id.to_native();
                if is_stock_deriv && expiry >= today && !seen_ids.contains(&sec_id) {
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
            if seen_ids.insert(*sec_id) {
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

    use dhan_live_trader_common::instrument_types::*;
    use dhan_live_trader_common::types::{Exchange, ExchangeSegment, OptionType, SecurityId};

    /// Builds a minimal FnoUniverse for testing.
    fn make_test_universe() -> FnoUniverse {
        let ist = dhan_live_trader_common::trading_calendar::ist_offset();
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
            let ce_id = 50100 + i as u32;
            let pe_id = 50200 + i as u32;

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
            let ce_id = 60100 + i as u32;
            let pe_id = 60200 + i as u32;

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

    #[test]
    fn test_build_plan_default_config() {
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(&universe, &config, today);

        // NIFTY is a major index → its IDX_I feed + ALL derivatives subscribed
        assert_eq!(plan.summary.major_index_values, 1); // NIFTY (only 1 of the 5 is in test universe)
        assert!(plan.summary.index_derivatives > 0); // NIFTY futures + options

        // INDIA VIX is a display index
        assert_eq!(plan.summary.display_indices, 1);

        // RELIANCE is a stock
        assert_eq!(plan.summary.stock_equities, 1);
        assert!(plan.summary.stock_derivatives > 0);

        assert_eq!(plan.summary.feed_mode, FeedMode::Full);
        assert!(!plan.summary.exceeds_capacity);
    }

    #[test]
    fn test_index_derivatives_all_subscribed() {
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(&universe, &config, today);

        // NIFTY: 1 future + 5 CE + 5 PE = 11 derivatives
        assert_eq!(plan.summary.index_derivatives, 11);
    }

    #[test]
    fn test_stock_derivatives_current_expiry_only() {
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(&universe, &config, today);

        // RELIANCE: 1 future + 5 CE + 5 PE = 11 (all 5 strikes within ATM±10)
        assert_eq!(plan.summary.stock_derivatives, 11);
    }

    #[test]
    fn test_stock_past_expiry_skipped() {
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        // Set today AFTER the expiry date → no current expiry
        let today = NaiveDate::from_ymd_opt(2026, 4, 1).unwrap();

        let plan = build_subscription_plan(&universe, &config, today);

        // RELIANCE has no valid expiry → skipped
        assert_eq!(plan.summary.stock_derivatives, 0);
        assert_eq!(plan.summary.stocks_skipped_no_chain, 1);
    }

    #[test]
    fn test_disable_stock_derivatives() {
        let universe = make_test_universe();
        let config = SubscriptionConfig {
            subscribe_stock_derivatives: false,
            ..Default::default()
        };
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(&universe, &config, today);

        assert_eq!(plan.summary.stock_derivatives, 0);
        assert_eq!(plan.summary.stock_equities, 1); // Equity feed still subscribed
    }

    #[test]
    fn test_disable_display_indices() {
        let universe = make_test_universe();
        let config = SubscriptionConfig {
            subscribe_display_indices: false,
            ..Default::default()
        };
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(&universe, &config, today);

        assert_eq!(plan.summary.display_indices, 0);
    }

    #[test]
    fn test_disable_index_derivatives() {
        let universe = make_test_universe();
        let config = SubscriptionConfig {
            subscribe_index_derivatives: false,
            ..Default::default()
        };
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(&universe, &config, today);

        assert_eq!(plan.summary.index_derivatives, 0);
        assert_eq!(plan.summary.major_index_values, 1); // Index value feed still subscribed
    }

    #[test]
    fn test_disable_stock_equities() {
        let universe = make_test_universe();
        let config = SubscriptionConfig {
            subscribe_stock_equities: false,
            ..Default::default()
        };
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(&universe, &config, today);

        assert_eq!(plan.summary.stock_equities, 0);
        // Stock derivatives still subscribed
        assert!(plan.summary.stock_derivatives > 0);
    }

    #[test]
    fn test_no_duplicate_security_ids() {
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(&universe, &config, today);

        // Verify no duplicates by checking total equals unique count
        let ids: Vec<u32> = plan.registry.iter().map(|i| i.security_id).collect();
        let unique: HashSet<u32> = ids.iter().copied().collect();
        assert_eq!(ids.len(), unique.len(), "Duplicate security_ids in plan");
    }

    #[test]
    fn test_registry_o1_lookup() {
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(&universe, &config, today);

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

        let plan = build_subscription_plan(&universe, &config, today);

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

        let plan = build_subscription_plan(&universe, &config, today);

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

        let plan = build_subscription_plan(&universe, &config, today);

        // Invalid feed mode falls back to Ticker (see build_subscription_plan line 112)
        assert_eq!(plan.summary.feed_mode, FeedMode::Ticker);
    }

    #[test]
    fn test_atm_strike_range_narrow() {
        let universe = make_test_universe();
        let config = SubscriptionConfig {
            stock_atm_strikes_above: 1,
            stock_atm_strikes_below: 1,
            ..Default::default()
        };
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(&universe, &config, today);

        // Stage 1: RELIANCE 1 future + 3 CE (mid+-1) + 3 PE (mid+-1) = 7
        // Stage 2: remaining 4 CE + 4 PE = 8 (progressive fill adds the rest)
        // Total stock derivatives: 7 + 4 = 11 (all RELIANCE)
        // But actually Stage 2 adds whatever wasn't in Stage 1
        assert_eq!(plan.summary.stock_derivatives, 11);
    }

    #[test]
    fn test_atm_strike_range_zero() {
        let universe = make_test_universe();
        let config = SubscriptionConfig {
            stock_atm_strikes_above: 0,
            stock_atm_strikes_below: 0,
            ..Default::default()
        };
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(&universe, &config, today);

        // Stage 1: RELIANCE 1 future + 1 CE (ATM only) + 1 PE (ATM only) = 3
        // Stage 2: remaining 4 CE + 4 PE = 8 (progressive fill adds the rest)
        // Total stock derivatives: 3 + 8 = 11 (all RELIANCE)
        assert_eq!(plan.summary.stock_derivatives, 11);
    }

    #[test]
    fn test_total_instrument_count() {
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(&universe, &config, today);

        let expected_total = plan.summary.major_index_values
            + plan.summary.display_indices
            + plan.summary.index_derivatives
            + plan.summary.stock_equities
            + plan.summary.stock_derivatives;
        assert_eq!(plan.summary.total, expected_total);
        assert_eq!(plan.registry.len(), expected_total);
    }

    #[test]
    fn test_by_exchange_segment_grouping() {
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(&universe, &config, today);
        let grouped = plan.registry.by_exchange_segment();

        // IDX_I: NIFTY value (13) + INDIA VIX (21) = 2
        assert_eq!(grouped.get(&ExchangeSegment::IdxI).unwrap().len(), 2);

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

        let plan = build_subscription_plan(&universe, &config, today);

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

        let plan = build_subscription_plan(&universe, &config, today);

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

        let plan1 = build_subscription_plan(&universe, &config, today);
        let plan2 = build_subscription_plan(&universe, &config, today);

        let mut ids1: Vec<u32> = plan1.registry.iter().map(|i| i.security_id).collect();
        let mut ids2: Vec<u32> = plan2.registry.iter().map(|i| i.security_id).collect();
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

        let plan = build_subscription_plan(&universe, &config, today);

        let ids: Vec<u32> = plan.registry.iter().map(|i| i.security_id).collect();
        let unique: HashSet<u32> = ids.iter().copied().collect();
        assert_eq!(
            ids.len(),
            unique.len(),
            "Duplicate security_ids after progressive fill"
        );
    }

    #[test]
    fn test_plan_stage2_nearest_expiry_prioritized() {
        // Universe with 2 expiry dates — Stage 2 should fill nearest first
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
            let ce_id = 70100 + i;
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

        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();
        let plan = build_subscription_plan(&universe, &config, today);

        // Stage 1 subscribes near expiry ATM+-N (all 11 RELIANCE near contracts).
        // Stage 2 should add far expiry contracts (future + 3 CE = 4).
        // Verify far expiry contracts are present in plan.
        assert!(
            plan.registry.get(far_future_id).is_some(),
            "Far expiry future should be in plan via Stage 2"
        );
        assert!(
            plan.registry.get(70100).is_some(),
            "Far expiry CE should be in plan via Stage 2"
        );

        // Total stock derivatives should include both near (11) + far (4) = 15
        assert_eq!(plan.summary.stock_derivatives, 15);
    }

    #[test]
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
            let ce_id = 80100 + i;
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

        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();
        let plan = build_subscription_plan(&universe, &config, today);

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

    #[test]
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

        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();
        let plan = build_subscription_plan(&universe, &config, today);

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

    #[test]
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

        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();
        let plan = build_subscription_plan(&universe, &config, today);

        // HDFC equity should be subscribed
        assert!(
            plan.registry.get(7777).is_some(),
            "HDFC equity feed should be subscribed"
        );
        // Should increment stocks_skipped_no_chain
        assert!(plan.summary.stocks_skipped_no_chain >= 1);
    }

    #[test]
    fn test_plan_with_all_subscriptions_disabled() {
        let universe = make_test_universe();
        let config = SubscriptionConfig {
            subscribe_index_derivatives: false,
            subscribe_display_indices: false,
            subscribe_stock_equities: false,
            subscribe_stock_derivatives: false,
            ..Default::default()
        };
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(&universe, &config, today);

        // Only major index values should remain
        assert_eq!(plan.summary.display_indices, 0);
        assert_eq!(plan.summary.index_derivatives, 0);
        assert_eq!(plan.summary.stock_equities, 0);
        assert_eq!(plan.summary.stock_derivatives, 0);
        // Major index values are always subscribed
        assert!(plan.summary.major_index_values > 0);
    }

    #[test]
    fn test_plan_summary_capacity_utilization_non_negative() {
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(&universe, &config, today);

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

        let plan = build_subscription_plan(&universe, &config, today);

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

        let plan = build_subscription_plan(&universe, &config, today);

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

        let plan = build_subscription_plan(&universe, &config, today);

        let debug_str = format!("{:?}", plan.summary);
        assert!(
            debug_str.contains("major_index_values"),
            "summary debug should contain field names"
        );
    }

    #[test]
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

        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();
        let plan = build_subscription_plan(&universe, &config, today);

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

    #[test]
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

        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();
        let plan = build_subscription_plan(&universe, &config, today);

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

        let plan = build_subscription_plan(&universe, &config, today);

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

    #[test]
    fn test_plan_today_equals_expiry_still_included() {
        // When today == expiry date, the contract should still be included
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        // Set today to the exact expiry date
        let today = NaiveDate::from_ymd_opt(2026, 3, 27).unwrap();

        let plan = build_subscription_plan(&universe, &config, today);

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

        let plan = build_subscription_plan(&universe, &config, today);

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
        let ist = dhan_live_trader_common::trading_calendar::ist_offset();
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

        let plan = build_subscription_plan(&universe, &config, today);

        assert_eq!(plan.summary.total, 0);
        assert_eq!(plan.summary.major_index_values, 0);
        assert_eq!(plan.summary.display_indices, 0);
        assert_eq!(plan.summary.index_derivatives, 0);
        assert_eq!(plan.summary.stock_equities, 0);
        assert_eq!(plan.summary.stock_derivatives, 0);
        assert!(!plan.summary.exceeds_capacity);
    }

    #[test]
    fn test_plan_exceeds_capacity_triggers_warning() {
        // Build a universe with > 25,000 stock derivative contracts to trigger
        // the capacity limit (line 316: break) and warning (lines 346-347).
        let ist = dhan_live_trader_common::trading_calendar::ist_offset();
        let expiry = NaiveDate::from_ymd_opt(2026, 3, 27).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let mut underlyings = HashMap::new();
        let mut derivative_contracts: HashMap<SecurityId, DerivativeContract> = HashMap::new();
        let mut option_chains = HashMap::new();
        let mut expiry_calendars = HashMap::new();

        // Create 50 stocks, each with 600 options (300 CE + 300 PE)
        // Total: 50 stocks * 600 options = 30,000 + 50 futures = 30,050
        // Plus 50 equity feeds = 30,100 → exceeds MAX_TOTAL_SUBSCRIPTIONS (25,000)
        let mut base_id: u32 = 100_000;
        for stock_idx in 0..50u32 {
            let symbol = format!("STOCK{stock_idx}");
            let underlying_id = 90_000 + stock_idx;
            let equity_id = 80_000 + stock_idx;

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

        let config = SubscriptionConfig::default();
        let plan = build_subscription_plan(&universe, &config, today);

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

    #[test]
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

        let config = SubscriptionConfig::default();
        let plan = build_subscription_plan(&universe, &config, today);

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

    #[test]
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

        let config = SubscriptionConfig::default();
        let plan = build_subscription_plan(&universe, &config, today);

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
        use dhan_live_trader_common::constants::DISPLAY_INDEX_ENTRIES;
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
        use dhan_live_trader_common::constants::DISPLAY_INDEX_ENTRIES;
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
        use dhan_live_trader_common::constants::DISPLAY_INDEX_ENTRIES;
        for &(name, _, _) in DISPLAY_INDEX_ENTRIES {
            assert!(
                !name.trim().is_empty(),
                "DISPLAY_INDEX_ENTRIES has empty name"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Archived vs non-archived planner parity
    // -----------------------------------------------------------------------

    #[test]
    fn test_archived_planner_produces_identical_summary() {
        use crate::instrument::binary_cache::{MappedUniverse, write_binary_cache};

        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap(); // APPROVED: test constant

        // Build plan from owned types
        let plan_owned = build_subscription_plan(&universe, &config, today);

        // Serialize → load as archived → build plan from archived types
        let dir = std::env::temp_dir().join(format!(
            "dlt-test-planner-parity-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let cache_dir = dir.to_str().unwrap(); // APPROVED: test-only path
        write_binary_cache(&universe, cache_dir).unwrap(); // APPROVED: test assertion

        let mapped = MappedUniverse::load(cache_dir).unwrap().unwrap(); // APPROVED: test assertion
        let archived = mapped.archived();
        let plan_archived = build_subscription_plan_from_archived(archived, &config, today);

        // Summaries must be identical
        assert_eq!(
            plan_owned.summary.major_index_values, plan_archived.summary.major_index_values,
            "major_index_values mismatch"
        );
        assert_eq!(
            plan_owned.summary.display_indices, plan_archived.summary.display_indices,
            "display_indices mismatch"
        );
        assert_eq!(
            plan_owned.summary.index_derivatives, plan_archived.summary.index_derivatives,
            "index_derivatives mismatch"
        );
        assert_eq!(
            plan_owned.summary.stock_equities, plan_archived.summary.stock_equities,
            "stock_equities mismatch"
        );
        assert_eq!(
            plan_owned.summary.stock_derivatives, plan_archived.summary.stock_derivatives,
            "stock_derivatives mismatch"
        );
        assert_eq!(
            plan_owned.summary.total, plan_archived.summary.total,
            "total mismatch"
        );
        assert_eq!(
            plan_owned.summary.feed_mode, plan_archived.summary.feed_mode,
            "feed_mode mismatch"
        );

        // Registry security IDs must be identical sets
        let mut ids_owned: Vec<u32> = plan_owned.registry.iter().map(|i| i.security_id).collect();
        let mut ids_archived: Vec<u32> = plan_archived
            .registry
            .iter()
            .map(|i| i.security_id)
            .collect();
        ids_owned.sort();
        ids_archived.sort();
        assert_eq!(
            ids_owned, ids_archived,
            "archived planner produced different security_id set"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Coverage: IDX_I ticker-only routing — major indices use IdxI segment
    // -----------------------------------------------------------------------

    #[test]
    fn test_major_index_uses_idx_i_exchange_segment() {
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(&universe, &config, today);

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

        let plan = build_subscription_plan(&universe, &config, today);

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

        let plan = build_subscription_plan(&universe, &config, today);

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

    #[test]
    fn test_index_derivative_uses_nse_fno_segment() {
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(&universe, &config, today);

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

        let plan = build_subscription_plan(&universe, &config, today);

        assert_eq!(plan.summary.feed_mode, FeedMode::Full);

        // Verify every subscribed instrument has Full feed mode
        for instrument in plan.registry.iter() {
            assert_eq!(
                instrument.feed_mode,
                FeedMode::Full,
                "instrument {} ({}) should have Full feed mode",
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

        let plan = build_subscription_plan(&universe, &config, today);

        for instrument in plan.registry.iter() {
            assert_eq!(
                instrument.feed_mode,
                FeedMode::Ticker,
                "instrument {} should have Ticker feed mode",
                instrument.security_id
            );
        }
    }

    // -----------------------------------------------------------------------
    // Coverage: Capacity limit — plan total capped at MAX_TOTAL_SUBSCRIPTIONS
    // -----------------------------------------------------------------------

    #[test]
    fn test_capacity_limit_stage2_break_path() {
        // Build a universe with enough stock derivatives to trigger the
        // `instruments.len() >= MAX_TOTAL_SUBSCRIPTIONS` break in Stage 2.
        // We need > 25,000 instruments total. The existing capacity test does
        // this with 50 stocks x 600 options. Here we verify the exact cap behavior.
        let ist = dhan_live_trader_common::trading_calendar::ist_offset();
        let expiry = NaiveDate::from_ymd_opt(2026, 3, 27).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let mut underlyings = HashMap::new();
        let mut derivative_contracts: HashMap<SecurityId, DerivativeContract> = HashMap::new();
        let mut expiry_calendars = HashMap::new();

        // Create 30 stocks with 1000 options each = 30,000 + 30 futures = 30,030
        let mut base_id: u32 = 200_000;
        for stock_idx in 0..30u32 {
            let symbol = format!("CAP{stock_idx}");
            let equity_id = 180_000 + stock_idx;

            underlyings.insert(
                symbol.clone(),
                FnoUnderlying {
                    underlying_symbol: symbol.clone(),
                    underlying_security_id: 190_000 + stock_idx,
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

        let config = SubscriptionConfig::default();
        let plan = build_subscription_plan(&universe, &config, today);

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

        let plan = build_subscription_plan(&universe, &config, today);

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

    #[test]
    fn test_stock_derivative_category_assignment() {
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(&universe, &config, today);

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

    #[test]
    fn test_underlying_symbol_propagated_to_instruments() {
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(&universe, &config, today);

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

    #[test]
    fn test_index_option_classified_as_index_derivative() {
        let universe = make_test_universe();
        let config = SubscriptionConfig::default();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(&universe, &config, today);

        // NIFTY CE option (50100) should be IndexDerivative
        let nifty_ce = plan.registry.get(50100).unwrap();
        assert_eq!(nifty_ce.category, SubscriptionCategory::IndexDerivative);

        // NIFTY PE option (50200) should be IndexDerivative
        let nifty_pe = plan.registry.get(50200).unwrap();
        assert_eq!(nifty_pe.category, SubscriptionCategory::IndexDerivative);
    }
}

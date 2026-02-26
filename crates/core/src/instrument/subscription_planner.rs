//! Subscription planner: FnoUniverse → filtered instrument list.
//!
//! Takes the full FnoUniverse (~97K contracts) and produces the exact list of
//! instruments to subscribe on the WebSocket, respecting:
//!
//! 1. **5 major indices** — ALL contracts (all expiries, all strikes, no filtering)
//! 2. **Display indices** — index value only (IDX_I ticker)
//! 3. **Stock equities** — NSE_EQ price feed for each F&O stock
//! 4. **Stock derivatives** — current expiry only, ATM ± N strikes
//!
//! # Feed Mode
//! All instruments start as Ticker mode. Quote and Full are supported in code
//! but configured via `SubscriptionConfig.feed_mode`.
//!
//! # Capacity
//! Total must fit within 25,000 (5 connections × 5,000 each).
//! The planner logs a warning if capacity is exceeded.

use std::collections::HashSet;

use chrono::NaiveDate;
use tracing::{debug, info, warn};

use dhan_live_trader_common::config::SubscriptionConfig;
use dhan_live_trader_common::constants::{FULL_CHAIN_INDEX_SYMBOLS, MAX_TOTAL_SUBSCRIPTIONS};
use dhan_live_trader_common::instrument_registry::{
    InstrumentRegistry, SubscribedInstrument, SubscriptionCategory, make_derivative_instrument,
    make_display_index_instrument, make_major_index_instrument, make_stock_equity_instrument,
};
use dhan_live_trader_common::instrument_types::{
    FnoUniverse, IndexCategory, OptionChainKey, UnderlyingKind,
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
            stocks_skipped_no_chain += 1;
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
                let end = (mid_idx + atm_above + 1).min(call_count);
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
                let end = (mid_idx + atm_above + 1).min(put_count);
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
            stocks_skipped_no_chain += 1;
        }
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

    let summary = SubscriptionPlanSummary {
        major_index_values: registry.category_count(SubscriptionCategory::MajorIndexValue),
        display_indices: registry.category_count(SubscriptionCategory::DisplayIndex),
        index_derivatives: registry.category_count(SubscriptionCategory::IndexDerivative),
        stock_equities: registry.category_count(SubscriptionCategory::StockEquity),
        stock_derivatives: registry.category_count(SubscriptionCategory::StockDerivative),
        total: registry.len(),
        feed_mode,
        exceeds_capacity,
        stocks_skipped_no_chain,
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
        "Subscription plan built"
    );

    SubscriptionPlan { registry, summary }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;

    use chrono::{FixedOffset, Utc};

    use dhan_live_trader_common::instrument_types::*;
    use dhan_live_trader_common::types::{Exchange, ExchangeSegment, OptionType, SecurityId};

    /// Builds a minimal FnoUniverse for testing.
    fn make_test_universe() -> FnoUniverse {
        let ist = FixedOffset::east_opt(19_800).unwrap();
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

        assert_eq!(plan.summary.feed_mode, FeedMode::Ticker);
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
        let mut config = SubscriptionConfig::default();
        config.subscribe_stock_derivatives = false;
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(&universe, &config, today);

        assert_eq!(plan.summary.stock_derivatives, 0);
        assert_eq!(plan.summary.stock_equities, 1); // Equity feed still subscribed
    }

    #[test]
    fn test_disable_display_indices() {
        let universe = make_test_universe();
        let mut config = SubscriptionConfig::default();
        config.subscribe_display_indices = false;
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(&universe, &config, today);

        assert_eq!(plan.summary.display_indices, 0);
    }

    #[test]
    fn test_disable_index_derivatives() {
        let universe = make_test_universe();
        let mut config = SubscriptionConfig::default();
        config.subscribe_index_derivatives = false;
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(&universe, &config, today);

        assert_eq!(plan.summary.index_derivatives, 0);
        assert_eq!(plan.summary.major_index_values, 1); // Index value feed still subscribed
    }

    #[test]
    fn test_disable_stock_equities() {
        let universe = make_test_universe();
        let mut config = SubscriptionConfig::default();
        config.subscribe_stock_equities = false;
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
        let mut config = SubscriptionConfig::default();
        config.feed_mode = "Quote".to_string();
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
        let mut config = SubscriptionConfig::default();
        config.feed_mode = "Full".to_string();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(&universe, &config, today);

        assert_eq!(plan.summary.feed_mode, FeedMode::Full);
    }

    #[test]
    fn test_feed_mode_invalid_falls_back_to_ticker() {
        let universe = make_test_universe();
        let mut config = SubscriptionConfig::default();
        config.feed_mode = "Invalid".to_string();
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(&universe, &config, today);

        // Invalid feed mode falls back to Ticker
        assert_eq!(plan.summary.feed_mode, FeedMode::Ticker);
    }

    #[test]
    fn test_atm_strike_range_narrow() {
        let universe = make_test_universe();
        let mut config = SubscriptionConfig::default();
        config.stock_atm_strikes_above = 1;
        config.stock_atm_strikes_below = 1;
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(&universe, &config, today);

        // RELIANCE: 1 future + 3 CE (mid±1) + 3 PE (mid±1) = 7
        assert_eq!(plan.summary.stock_derivatives, 7);
    }

    #[test]
    fn test_atm_strike_range_zero() {
        let universe = make_test_universe();
        let mut config = SubscriptionConfig::default();
        config.stock_atm_strikes_above = 0;
        config.stock_atm_strikes_below = 0;
        let today = NaiveDate::from_ymd_opt(2026, 3, 15).unwrap();

        let plan = build_subscription_plan(&universe, &config, today);

        // RELIANCE: 1 future + 1 CE (ATM only) + 1 PE (ATM only) = 3
        assert_eq!(plan.summary.stock_derivatives, 3);
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
}

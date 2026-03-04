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

        let stage2_added = instruments.len() - count_before_stage2;
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

        // Invalid feed mode falls back to Ticker
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
        let ist = FixedOffset::east_opt(5 * 3600 + 30 * 60).unwrap();
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
        let ist = FixedOffset::east_opt(19_800).unwrap();
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
}

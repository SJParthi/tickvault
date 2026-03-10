//! O(1) instrument registry for subscribed instruments.
//!
//! Maps `security_id` → full instrument metadata for every subscribed instrument.
//! Built once at startup from `FnoUniverse`, shared via `Arc` across the pipeline.
//!
//! # Performance
//! All lookups are O(1) via `HashMap`. The registry is immutable after construction
//! — no locks, no contention, no allocation on the hot path.
//!
//! # Subscribed Instrument Categories
//! 1. **5 major index values** (IDX_I): NIFTY, BANKNIFTY, SENSEX, MIDCPNIFTY, FINNIFTY
//! 2. **Index derivatives** (NSE_FNO/BSE_FNO): ALL expiries, ALL strikes for 5 major indices
//! 3. **Display indices** (IDX_I): INDIA VIX, sectoral, broad market (23 total)
//! 4. **Stock equities** (NSE_EQ): price feeds for ~206 F&O stocks
//! 5. **Stock derivatives** (NSE_FNO): ATM ± N current expiry (priority) + progressive fill nearest-expiry-first to 25K capacity

use std::collections::HashMap;
use std::fmt;

use chrono::NaiveDate;

use crate::instrument_types::{
    ArchivedDerivativeContract, ArchivedFnoUnderlying, DerivativeContract, DhanInstrumentKind,
    FnoUnderlying, naive_date_from_archived_i32,
};
use crate::types::{ExchangeSegment, FeedMode, OptionType, SecurityId};

// ---------------------------------------------------------------------------
// Subscribed Instrument — what we know about each security_id
// ---------------------------------------------------------------------------

/// Category of a subscribed instrument for logging and diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SubscriptionCategory {
    /// Major index value feed (IDX_I segment, e.g., NIFTY 50 = 13).
    MajorIndexValue,
    /// Display-only index (IDX_I segment, e.g., INDIA VIX = 21).
    DisplayIndex,
    /// Index derivative contract (futures or options on major indices).
    IndexDerivative,
    /// Stock equity price feed (NSE_EQ segment, e.g., RELIANCE = 2885).
    StockEquity,
    /// Stock derivative contract (futures or options on stocks, current expiry).
    StockDerivative,
}

impl SubscriptionCategory {
    /// Returns the canonical string for logging and metrics.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::MajorIndexValue => "MajorIndexValue",
            Self::DisplayIndex => "DisplayIndex",
            Self::IndexDerivative => "IndexDerivative",
            Self::StockEquity => "StockEquity",
            Self::StockDerivative => "StockDerivative",
        }
    }
}

impl fmt::Display for SubscriptionCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Metadata for a single subscribed instrument.
///
/// Provides O(1) enrichment when a tick arrives: given `security_id`,
/// instantly know what this instrument is, its underlying, strike, expiry, etc.
#[derive(Debug, Clone)]
pub struct SubscribedInstrument {
    /// The security ID (same as the HashMap key, stored here for convenience).
    pub security_id: SecurityId,

    /// Exchange segment for WebSocket subscription JSON.
    pub exchange_segment: ExchangeSegment,

    /// Which category this instrument belongs to.
    pub category: SubscriptionCategory,

    /// Human-readable label for logging (e.g., "NIFTY", "RELIANCE 18000 CE Mar26").
    pub display_label: String,

    /// Underlying symbol (e.g., "NIFTY", "RELIANCE"). Same as symbol for indices/equities.
    pub underlying_symbol: String,

    /// Instrument kind for derivatives. `None` for indices and equities.
    pub instrument_kind: Option<DhanInstrumentKind>,

    /// Expiry date for derivatives. `None` for indices and equities.
    pub expiry_date: Option<NaiveDate>,

    /// Strike price for options. `None` for futures, indices, equities.
    pub strike_price: Option<f64>,

    /// Option type for options. `None` for futures, indices, equities.
    pub option_type: Option<OptionType>,

    /// Feed mode this instrument is subscribed with.
    pub feed_mode: FeedMode,
}

// ---------------------------------------------------------------------------
// Instrument Registry
// ---------------------------------------------------------------------------

/// Immutable O(1) registry of all subscribed instruments.
///
/// Built once at startup, shared via `Arc<InstrumentRegistry>` across the pipeline.
/// Every tick from the WebSocket can be enriched in O(1) by looking up `security_id`.
#[derive(Clone)]
pub struct InstrumentRegistry {
    /// O(1) lookup: security_id → instrument metadata.
    instruments: HashMap<SecurityId, SubscribedInstrument>,

    /// Summary counts per category for logging.
    category_counts: HashMap<SubscriptionCategory, usize>,

    /// Total subscribed instrument count.
    total_count: usize,
}

impl InstrumentRegistry {
    /// Creates an empty registry (used in tests or offline mode).
    pub fn empty() -> Self {
        Self {
            instruments: HashMap::new(),
            category_counts: HashMap::new(),
            total_count: 0,
        }
    }

    /// Creates a registry from a pre-built instrument map.
    pub fn from_instruments(instruments: Vec<SubscribedInstrument>) -> Self {
        let mut map = HashMap::with_capacity(instruments.len());

        for instrument in instruments {
            map.insert(instrument.security_id, instrument);
        }

        // Count categories from the deduplicated map (handles duplicate security_ids correctly).
        let mut category_counts: HashMap<SubscriptionCategory, usize> = HashMap::new();
        for instrument in map.values() {
            let count = category_counts.entry(instrument.category).or_insert(0);
            *count = count.saturating_add(1);
        }

        let total_count = map.len();

        Self {
            instruments: map,
            category_counts,
            total_count,
        }
    }

    /// O(1) lookup: returns instrument metadata for a security_id, or `None` if not subscribed.
    #[inline]
    pub fn get(&self, security_id: SecurityId) -> Option<&SubscribedInstrument> {
        self.instruments.get(&security_id)
    }

    /// Returns `true` if this security_id is in the registry.
    #[inline]
    pub fn contains(&self, security_id: SecurityId) -> bool {
        self.instruments.contains_key(&security_id)
    }

    /// Total number of subscribed instruments.
    #[inline]
    pub fn len(&self) -> usize {
        self.total_count
    }

    /// Returns `true` if the registry is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.total_count == 0
    }

    /// Returns the count for a specific category.
    pub fn category_count(&self, category: SubscriptionCategory) -> usize {
        self.category_counts.get(&category).copied().unwrap_or(0)
    }

    /// Returns a reference to all category counts.
    pub fn category_counts(&self) -> &HashMap<SubscriptionCategory, usize> {
        &self.category_counts
    }

    /// Returns an iterator over all subscribed instruments.
    pub fn iter(&self) -> impl Iterator<Item = &SubscribedInstrument> {
        self.instruments.values()
    }

    /// Returns all security_ids grouped by exchange segment.
    ///
    /// Used by the subscription planner to build WebSocket subscription messages.
    pub fn by_exchange_segment(&self) -> HashMap<ExchangeSegment, Vec<SecurityId>> {
        let mut grouped: HashMap<ExchangeSegment, Vec<SecurityId>> = HashMap::new();
        for instrument in self.instruments.values() {
            grouped
                .entry(instrument.exchange_segment)
                .or_default()
                .push(instrument.security_id);
        }
        grouped
    }
}

impl fmt::Debug for InstrumentRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InstrumentRegistry")
            .field("total_count", &self.total_count)
            .field("category_counts", &self.category_counts)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Builder helpers — convert FnoUniverse entities to SubscribedInstrument
// ---------------------------------------------------------------------------

/// Creates a `SubscribedInstrument` for a major index value feed.
pub fn make_major_index_instrument(
    security_id: SecurityId,
    symbol: &str,
    feed_mode: FeedMode,
) -> SubscribedInstrument {
    SubscribedInstrument {
        security_id,
        exchange_segment: ExchangeSegment::IdxI,
        category: SubscriptionCategory::MajorIndexValue,
        display_label: symbol.to_string(),
        underlying_symbol: symbol.to_string(),
        instrument_kind: None,
        expiry_date: None,
        strike_price: None,
        option_type: None,
        feed_mode,
    }
}

/// Creates a `SubscribedInstrument` for a display-only index.
pub fn make_display_index_instrument(
    security_id: SecurityId,
    symbol: &str,
    feed_mode: FeedMode,
) -> SubscribedInstrument {
    SubscribedInstrument {
        security_id,
        exchange_segment: ExchangeSegment::IdxI,
        category: SubscriptionCategory::DisplayIndex,
        display_label: symbol.to_string(),
        underlying_symbol: symbol.to_string(),
        instrument_kind: None,
        expiry_date: None,
        strike_price: None,
        option_type: None,
        feed_mode,
    }
}

/// Creates a `SubscribedInstrument` for a stock equity price feed.
pub fn make_stock_equity_instrument(
    underlying: &FnoUnderlying,
    feed_mode: FeedMode,
) -> SubscribedInstrument {
    SubscribedInstrument {
        security_id: underlying.price_feed_security_id,
        exchange_segment: underlying.price_feed_segment,
        category: SubscriptionCategory::StockEquity,
        display_label: underlying.underlying_symbol.clone(),
        underlying_symbol: underlying.underlying_symbol.clone(),
        instrument_kind: None,
        expiry_date: None,
        strike_price: None,
        option_type: None,
        feed_mode,
    }
}

/// Creates a `SubscribedInstrument` from a derivative contract.
pub fn make_derivative_instrument(
    contract: &DerivativeContract,
    category: SubscriptionCategory,
    feed_mode: FeedMode,
) -> SubscribedInstrument {
    SubscribedInstrument {
        security_id: contract.security_id,
        exchange_segment: contract.exchange_segment,
        category,
        display_label: contract.display_name.clone(),
        underlying_symbol: contract.underlying_symbol.clone(),
        instrument_kind: Some(contract.instrument_kind),
        expiry_date: Some(contract.expiry_date),
        strike_price: if contract.strike_price > 0.0 {
            Some(contract.strike_price)
        } else {
            None
        },
        option_type: contract.option_type,
        feed_mode,
    }
}

// ---------------------------------------------------------------------------
// Builder helpers — zero-copy archived types → SubscribedInstrument
// ---------------------------------------------------------------------------

/// Creates a `SubscribedInstrument` for a stock equity price feed from an archived underlying.
pub fn make_stock_equity_instrument_from_archived(
    underlying: &ArchivedFnoUnderlying,
    feed_mode: FeedMode,
) -> SubscribedInstrument {
    SubscribedInstrument {
        security_id: underlying.price_feed_security_id.to_native(),
        exchange_segment: ExchangeSegment::from(&underlying.price_feed_segment),
        category: SubscriptionCategory::StockEquity,
        display_label: underlying.underlying_symbol.as_str().to_owned(),
        underlying_symbol: underlying.underlying_symbol.as_str().to_owned(),
        instrument_kind: None,
        expiry_date: None,
        strike_price: None,
        option_type: None,
        feed_mode,
    }
}

/// Creates a `SubscribedInstrument` from an archived derivative contract.
pub fn make_derivative_instrument_from_archived(
    contract: &ArchivedDerivativeContract,
    category: SubscriptionCategory,
    feed_mode: FeedMode,
) -> SubscribedInstrument {
    let strike = contract.strike_price.to_native();
    SubscribedInstrument {
        security_id: contract.security_id.to_native(),
        exchange_segment: ExchangeSegment::from(&contract.exchange_segment),
        category,
        display_label: contract.display_name.as_str().to_owned(),
        underlying_symbol: contract.underlying_symbol.as_str().to_owned(),
        instrument_kind: Some(DhanInstrumentKind::from(&contract.instrument_kind)),
        expiry_date: Some(naive_date_from_archived_i32(&contract.expiry_date)),
        strike_price: if strike > 0.0 { Some(strike) } else { None },
        option_type: contract.option_type.as_ref().map(OptionType::from),
        feed_mode,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_instrument(
        security_id: SecurityId,
        category: SubscriptionCategory,
    ) -> SubscribedInstrument {
        SubscribedInstrument {
            security_id,
            exchange_segment: ExchangeSegment::IdxI,
            category,
            display_label: format!("TEST_{security_id}"),
            underlying_symbol: "TEST".to_string(),
            instrument_kind: None,
            expiry_date: None,
            strike_price: None,
            option_type: None,
            feed_mode: FeedMode::Ticker,
        }
    }

    #[test]
    fn test_empty_registry() {
        let registry = InstrumentRegistry::empty();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(registry.get(13).is_none());
        assert!(!registry.contains(13));
    }

    #[test]
    fn test_from_instruments_single() {
        let instruments = vec![sample_instrument(13, SubscriptionCategory::MajorIndexValue)];
        let registry = InstrumentRegistry::from_instruments(instruments);
        assert_eq!(registry.len(), 1);
        assert!(!registry.is_empty());
        assert!(registry.contains(13));
        assert!(!registry.contains(99));
        let inst = registry.get(13).unwrap();
        assert_eq!(inst.security_id, 13);
        assert_eq!(inst.category, SubscriptionCategory::MajorIndexValue);
    }

    #[test]
    fn test_from_instruments_multiple_categories() {
        let instruments = vec![
            sample_instrument(13, SubscriptionCategory::MajorIndexValue),
            sample_instrument(25, SubscriptionCategory::MajorIndexValue),
            sample_instrument(21, SubscriptionCategory::DisplayIndex),
            sample_instrument(2885, SubscriptionCategory::StockEquity),
            sample_instrument(50000, SubscriptionCategory::IndexDerivative),
            sample_instrument(60000, SubscriptionCategory::StockDerivative),
        ];
        let registry = InstrumentRegistry::from_instruments(instruments);
        assert_eq!(registry.len(), 6);
        assert_eq!(
            registry.category_count(SubscriptionCategory::MajorIndexValue),
            2
        );
        assert_eq!(
            registry.category_count(SubscriptionCategory::DisplayIndex),
            1
        );
        assert_eq!(
            registry.category_count(SubscriptionCategory::StockEquity),
            1
        );
        assert_eq!(
            registry.category_count(SubscriptionCategory::IndexDerivative),
            1
        );
        assert_eq!(
            registry.category_count(SubscriptionCategory::StockDerivative),
            1
        );
    }

    #[test]
    fn test_category_count_missing_returns_zero() {
        let instruments = vec![sample_instrument(13, SubscriptionCategory::MajorIndexValue)];
        let registry = InstrumentRegistry::from_instruments(instruments);
        assert_eq!(
            registry.category_count(SubscriptionCategory::StockDerivative),
            0
        );
    }

    #[test]
    fn test_by_exchange_segment() {
        let mut inst1 = sample_instrument(13, SubscriptionCategory::MajorIndexValue);
        inst1.exchange_segment = ExchangeSegment::IdxI;
        let mut inst2 = sample_instrument(2885, SubscriptionCategory::StockEquity);
        inst2.exchange_segment = ExchangeSegment::NseEquity;
        let mut inst3 = sample_instrument(50000, SubscriptionCategory::IndexDerivative);
        inst3.exchange_segment = ExchangeSegment::NseFno;

        let registry = InstrumentRegistry::from_instruments(vec![inst1, inst2, inst3]);
        let grouped = registry.by_exchange_segment();
        assert_eq!(grouped.get(&ExchangeSegment::IdxI).unwrap().len(), 1);
        assert_eq!(grouped.get(&ExchangeSegment::NseEquity).unwrap().len(), 1);
        assert_eq!(grouped.get(&ExchangeSegment::NseFno).unwrap().len(), 1);
        assert!(!grouped.contains_key(&ExchangeSegment::BseFno));
    }

    #[test]
    fn test_iter_returns_all() {
        let instruments = vec![
            sample_instrument(1, SubscriptionCategory::MajorIndexValue),
            sample_instrument(2, SubscriptionCategory::DisplayIndex),
            sample_instrument(3, SubscriptionCategory::StockEquity),
        ];
        let registry = InstrumentRegistry::from_instruments(instruments);
        let collected: Vec<SecurityId> = registry.iter().map(|i| i.security_id).collect();
        assert_eq!(collected.len(), 3);
        assert!(collected.contains(&1));
        assert!(collected.contains(&2));
        assert!(collected.contains(&3));
    }

    #[test]
    fn test_make_major_index_instrument() {
        let inst = make_major_index_instrument(13, "NIFTY", FeedMode::Ticker);
        assert_eq!(inst.security_id, 13);
        assert_eq!(inst.exchange_segment, ExchangeSegment::IdxI);
        assert_eq!(inst.category, SubscriptionCategory::MajorIndexValue);
        assert_eq!(inst.display_label, "NIFTY");
        assert_eq!(inst.underlying_symbol, "NIFTY");
        assert!(inst.instrument_kind.is_none());
        assert!(inst.expiry_date.is_none());
        assert!(inst.strike_price.is_none());
        assert!(inst.option_type.is_none());
        assert_eq!(inst.feed_mode, FeedMode::Ticker);
    }

    #[test]
    fn test_make_display_index_instrument() {
        let inst = make_display_index_instrument(21, "INDIA VIX", FeedMode::Ticker);
        assert_eq!(inst.category, SubscriptionCategory::DisplayIndex);
        assert_eq!(inst.display_label, "INDIA VIX");
    }

    #[test]
    fn test_make_stock_equity_instrument() {
        let underlying = FnoUnderlying {
            underlying_symbol: "RELIANCE".to_string(),
            underlying_security_id: 26000,
            price_feed_security_id: 2885,
            price_feed_segment: ExchangeSegment::NseEquity,
            derivative_segment: ExchangeSegment::NseFno,
            kind: crate::instrument_types::UnderlyingKind::Stock,
            lot_size: 250,
            contract_count: 100,
        };
        let inst = make_stock_equity_instrument(&underlying, FeedMode::Ticker);
        assert_eq!(inst.security_id, 2885);
        assert_eq!(inst.exchange_segment, ExchangeSegment::NseEquity);
        assert_eq!(inst.category, SubscriptionCategory::StockEquity);
        assert_eq!(inst.underlying_symbol, "RELIANCE");
    }

    #[test]
    fn test_make_derivative_instrument_option() {
        let contract = DerivativeContract {
            security_id: 52432,
            underlying_symbol: "NIFTY".to_string(),
            instrument_kind: DhanInstrumentKind::OptionIndex,
            exchange_segment: ExchangeSegment::NseFno,
            expiry_date: NaiveDate::from_ymd_opt(2026, 3, 27).unwrap(),
            strike_price: 18000.0,
            option_type: Some(OptionType::Call),
            lot_size: 50,
            tick_size: 0.05,
            symbol_name: "NIFTY-27MAR26-18000-CE".to_string(),
            display_name: "NIFTY 18000 CE Mar26".to_string(),
        };
        let inst = make_derivative_instrument(
            &contract,
            SubscriptionCategory::IndexDerivative,
            FeedMode::Ticker,
        );
        assert_eq!(inst.security_id, 52432);
        assert_eq!(inst.exchange_segment, ExchangeSegment::NseFno);
        assert_eq!(inst.category, SubscriptionCategory::IndexDerivative);
        assert_eq!(inst.underlying_symbol, "NIFTY");
        assert_eq!(inst.instrument_kind, Some(DhanInstrumentKind::OptionIndex));
        assert_eq!(
            inst.expiry_date,
            Some(NaiveDate::from_ymd_opt(2026, 3, 27).unwrap())
        );
        assert_eq!(inst.strike_price, Some(18000.0));
        assert_eq!(inst.option_type, Some(OptionType::Call));
    }

    #[test]
    fn test_make_derivative_instrument_future_no_strike() {
        let contract = DerivativeContract {
            security_id: 99999,
            underlying_symbol: "RELIANCE".to_string(),
            instrument_kind: DhanInstrumentKind::FutureStock,
            exchange_segment: ExchangeSegment::NseFno,
            expiry_date: NaiveDate::from_ymd_opt(2026, 3, 27).unwrap(),
            strike_price: 0.0,
            option_type: None,
            lot_size: 250,
            tick_size: 0.05,
            symbol_name: "RELIANCE-27MAR26-FUT".to_string(),
            display_name: "RELIANCE FUT Mar26".to_string(),
        };
        let inst = make_derivative_instrument(
            &contract,
            SubscriptionCategory::StockDerivative,
            FeedMode::Ticker,
        );
        assert!(inst.strike_price.is_none()); // 0.0 → None
        assert!(inst.option_type.is_none());
    }

    #[test]
    fn test_subscription_category_as_str() {
        assert_eq!(
            SubscriptionCategory::MajorIndexValue.as_str(),
            "MajorIndexValue"
        );
        assert_eq!(SubscriptionCategory::DisplayIndex.as_str(), "DisplayIndex");
        assert_eq!(
            SubscriptionCategory::IndexDerivative.as_str(),
            "IndexDerivative"
        );
        assert_eq!(SubscriptionCategory::StockEquity.as_str(), "StockEquity");
        assert_eq!(
            SubscriptionCategory::StockDerivative.as_str(),
            "StockDerivative"
        );
    }

    #[test]
    fn test_subscription_category_display() {
        assert_eq!(
            format!("{}", SubscriptionCategory::MajorIndexValue),
            "MajorIndexValue"
        );
    }

    #[test]
    fn test_registry_debug_format() {
        let registry = InstrumentRegistry::from_instruments(vec![sample_instrument(
            13,
            SubscriptionCategory::MajorIndexValue,
        )]);
        let debug = format!("{:?}", registry);
        assert!(debug.contains("InstrumentRegistry"));
        assert!(debug.contains("total_count: 1"));
    }

    #[test]
    fn test_duplicate_security_id_last_wins() {
        // If same security_id appears twice, the last one wins (HashMap behavior).
        let inst1 = sample_instrument(13, SubscriptionCategory::MajorIndexValue);
        let inst2 = sample_instrument(13, SubscriptionCategory::DisplayIndex);
        let registry = InstrumentRegistry::from_instruments(vec![inst1, inst2]);
        // HashMap insert order: last wins
        assert_eq!(registry.len(), 1);
        assert_eq!(
            registry.get(13).unwrap().category,
            SubscriptionCategory::DisplayIndex
        );
    }

    #[test]
    fn test_category_counts_reference() {
        let instruments = vec![
            sample_instrument(1, SubscriptionCategory::MajorIndexValue),
            sample_instrument(2, SubscriptionCategory::MajorIndexValue),
            sample_instrument(3, SubscriptionCategory::StockEquity),
        ];
        let registry = InstrumentRegistry::from_instruments(instruments);
        let counts = registry.category_counts();
        assert_eq!(counts.len(), 2);
        assert_eq!(
            *counts.get(&SubscriptionCategory::MajorIndexValue).unwrap(),
            2
        );
        assert_eq!(*counts.get(&SubscriptionCategory::StockEquity).unwrap(), 1);
    }
}

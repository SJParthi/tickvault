//! Domain types for the instrument universe.
//!
//! Block 01 produces these types. Every downstream block (WebSocket, OMS,
//! storage, API) consumes them. Types live in `common` so all crates
//! can depend on them without importing `core`.

use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use chrono::{DateTime, FixedOffset, NaiveDate};
use serde::{Deserialize, Serialize};

use crate::types::{Exchange, ExchangeSegment, OptionType, SecurityId};

// ---------------------------------------------------------------------------
// Underlying Classification
// ---------------------------------------------------------------------------

/// Classification of an F&O underlying.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UnderlyingKind {
    /// NSE index with F&O derivatives (e.g., NIFTY, BANKNIFTY).
    NseIndex,
    /// BSE index with F&O derivatives (e.g., SENSEX, BANKEX).
    BseIndex,
    /// Individual stock with F&O derivatives (e.g., RELIANCE, HDFCBANK).
    Stock,
}

impl UnderlyingKind {
    /// Returns the canonical string representation for storage and display.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NseIndex => "NseIndex",
            Self::BseIndex => "BseIndex",
            Self::Stock => "Stock",
        }
    }
}

impl fmt::Display for UnderlyingKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Classification of a derivative instrument as it appears in the Dhan CSV.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DhanInstrumentKind {
    /// Index future (FUTIDX).
    FutureIndex,
    /// Stock future (FUTSTK).
    FutureStock,
    /// Index option (OPTIDX).
    OptionIndex,
    /// Stock option (OPTSTK).
    OptionStock,
}

impl DhanInstrumentKind {
    /// Returns the canonical string representation for storage and display.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FutureIndex => "FutureIndex",
            Self::FutureStock => "FutureStock",
            Self::OptionIndex => "OptionIndex",
            Self::OptionStock => "OptionStock",
        }
    }
}

impl fmt::Display for DhanInstrumentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Subscribed Index (8 F&O + 23 Display = 31 total)
// ---------------------------------------------------------------------------

/// Category of a subscribed index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum IndexCategory {
    /// F&O underlying index (has derivatives, e.g., NIFTY, BANKNIFTY, SENSEX).
    FnoUnderlying,
    /// Display-only index (market dashboard, no derivatives, e.g., INDIA VIX).
    DisplayIndex,
}

impl IndexCategory {
    /// Returns the canonical string representation for storage and display.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FnoUnderlying => "FnoUnderlying",
            Self::DisplayIndex => "DisplayIndex",
        }
    }
}

impl fmt::Display for IndexCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Subcategory of a display index for dashboard grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum IndexSubcategory {
    /// Volatility indices (e.g., INDIA VIX).
    Volatility,
    /// Broad market indices (e.g., NIFTY 100, NIFTY 500).
    BroadMarket,
    /// Mid cap indices (e.g., NIFTYMCAP50).
    MidCap,
    /// Small cap indices (e.g., NIFTY SMALLCAP 50).
    SmallCap,
    /// Sectoral indices (e.g., NIFTY AUTO, NIFTYIT).
    Sectoral,
    /// Thematic indices (e.g., NIFTY CONSUMPTION).
    Thematic,
    /// F&O index (category is FnoUnderlying, subcategory is Fno).
    Fno,
}

impl IndexSubcategory {
    /// Returns the canonical string representation for storage and display.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Volatility => "Volatility",
            Self::BroadMarket => "BroadMarket",
            Self::MidCap => "MidCap",
            Self::SmallCap => "SmallCap",
            Self::Sectoral => "Sectoral",
            Self::Thematic => "Thematic",
            Self::Fno => "Fno",
        }
    }
}

impl fmt::Display for IndexSubcategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A subscribed index — one of the 31 documented indices.
///
/// 8 F&O indices (NIFTY, BANKNIFTY, etc.) + 23 display indices (VIX, sectoral, etc.).
/// All subscribed on IDX_I exchange segment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscribedIndex {
    /// Index symbol (e.g., "NIFTY", "INDIA VIX", "NIFTY AUTO").
    pub symbol: String,

    /// IDX_I security ID for WebSocket subscription.
    pub security_id: SecurityId,

    /// Exchange (NSE or BSE).
    pub exchange: Exchange,

    /// Category: FnoUnderlying or DisplayIndex.
    pub category: IndexCategory,

    /// Subcategory for dashboard grouping.
    pub subcategory: IndexSubcategory,
}

// ---------------------------------------------------------------------------
// F&O Underlying
// ---------------------------------------------------------------------------

/// A single underlying in the F&O universe.
///
/// One entry per unique `UNDERLYING_SYMBOL` that has derivatives.
/// Contains everything needed to subscribe to the underlying's live price
/// feed and to look up its derivative contracts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FnoUnderlying {
    /// Underlying symbol (e.g., "NIFTY", "RELIANCE").
    pub underlying_symbol: String,

    /// Security ID from the CSV `UNDERLYING_SECURITY_ID` column.
    /// For indices, this is the "phantom" FNO ID (e.g., NIFTY = 26000),
    /// NOT the IDX_I price feed ID.
    pub underlying_security_id: SecurityId,

    /// Security ID for live price feed subscription.
    /// For indices: IDX_I ID (e.g., NIFTY = 13).
    /// For stocks: NSE_EQ ID (e.g., RELIANCE = 2885).
    pub price_feed_security_id: SecurityId,

    /// Exchange segment for the live price feed.
    /// IdxI for indices, NseEquity for stocks.
    pub price_feed_segment: ExchangeSegment,

    /// Exchange segment for derivatives.
    /// NseFno or BseFno.
    pub derivative_segment: ExchangeSegment,

    /// Classification of this underlying.
    pub kind: UnderlyingKind,

    /// Contract lot size.
    pub lot_size: u32,

    /// Total number of derivative contracts for this underlying.
    pub contract_count: usize,
}

// ---------------------------------------------------------------------------
// Derivative Contract
// ---------------------------------------------------------------------------

/// A single derivative contract (future or option).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DerivativeContract {
    /// This contract's own security ID (unique across all contracts).
    pub security_id: SecurityId,

    /// Symbol of the underlying (e.g., "NIFTY", "RELIANCE").
    pub underlying_symbol: String,

    /// Kind of derivative instrument.
    pub instrument_kind: DhanInstrumentKind,

    /// Exchange segment (NseFno or BseFno).
    pub exchange_segment: ExchangeSegment,

    /// Expiry date of this contract.
    pub expiry_date: NaiveDate,

    /// Strike price. 0.0 for futures.
    pub strike_price: f64,

    /// Option type. `None` for futures.
    pub option_type: Option<OptionType>,

    /// Contract lot size.
    pub lot_size: u32,

    /// Minimum price movement.
    pub tick_size: f64,

    /// Full symbol from the CSV (e.g., "NIFTY-Mar2026-18000-CE").
    pub symbol_name: String,

    /// Human-readable display name.
    pub display_name: String,
}

// ---------------------------------------------------------------------------
// Option Chain
// ---------------------------------------------------------------------------

/// A single option chain: all contracts for one (underlying, expiry).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptionChain {
    /// Underlying symbol.
    pub underlying_symbol: String,

    /// Expiry date for this chain.
    pub expiry_date: NaiveDate,

    /// Call options sorted by strike price ascending.
    pub calls: Vec<OptionChainEntry>,

    /// Put options sorted by strike price ascending.
    pub puts: Vec<OptionChainEntry>,

    /// Corresponding future security ID for this expiry, if one exists.
    pub future_security_id: Option<SecurityId>,
}

/// One strike in an option chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptionChainEntry {
    /// Security ID of this option contract.
    pub security_id: SecurityId,

    /// Strike price.
    pub strike_price: f64,

    /// Contract lot size.
    pub lot_size: u32,
}

// ---------------------------------------------------------------------------
// Option Chain Key
// ---------------------------------------------------------------------------

/// Composite key for option chain lookups.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OptionChainKey {
    /// Underlying symbol.
    pub underlying_symbol: String,

    /// Expiry date.
    pub expiry_date: NaiveDate,
}

// ---------------------------------------------------------------------------
// Expiry Calendar
// ---------------------------------------------------------------------------

/// Expiry calendar for a single underlying.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpiryCalendar {
    /// Underlying symbol.
    pub underlying_symbol: String,

    /// Sorted expiry dates (ascending), all >= today at build time.
    pub expiry_dates: Vec<NaiveDate>,
}

// ---------------------------------------------------------------------------
// Instrument Info (global lookup)
// ---------------------------------------------------------------------------

/// Generic instrument info for any security ID.
///
/// Used by the WebSocket binary parser to decode what a security ID
/// represents when receiving tick data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InstrumentInfo {
    /// An index (IDX_I segment).
    Index {
        security_id: SecurityId,
        symbol: String,
        exchange: Exchange,
    },
    /// An equity stock (NSE_EQ segment).
    Equity {
        security_id: SecurityId,
        symbol: String,
    },
    /// A derivative contract (NSE_FNO or BSE_FNO segment).
    Derivative {
        security_id: SecurityId,
        underlying_symbol: String,
        instrument_kind: DhanInstrumentKind,
        exchange_segment: ExchangeSegment,
        expiry_date: NaiveDate,
        strike_price: f64,
        option_type: Option<OptionType>,
    },
}

// ---------------------------------------------------------------------------
// Build Metadata
// ---------------------------------------------------------------------------

/// Metadata about the universe build for logging and monitoring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UniverseBuildMetadata {
    /// Which CSV URL was used (primary or fallback).
    pub csv_source: String,

    /// Total rows in the raw CSV file.
    pub csv_row_count: usize,

    /// Rows remaining after (NSE I/E/D + BSE I/D) filtering.
    pub parsed_row_count: usize,

    /// Pass 1 result: number of indices found.
    pub index_count: usize,

    /// Pass 2 result: number of equities found.
    pub equity_count: usize,

    /// Pass 3 result: number of F&O underlyings found.
    pub underlying_count: usize,

    /// Pass 5 result: number of derivative contracts.
    pub derivative_count: usize,

    /// Pass 5 result: number of unique option chains.
    pub option_chain_count: usize,

    /// Total universe build duration.
    #[serde(with = "duration_serde")]
    pub build_duration: Duration,

    /// Timestamp when the build completed (IST).
    pub build_timestamp: DateTime<FixedOffset>,
}

/// Serde helper for `Duration` (stored as milliseconds).
mod duration_serde {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
        duration.as_millis().serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
        let millis = u64::deserialize(deserializer)?;
        Ok(Duration::from_millis(millis))
    }
}

// ---------------------------------------------------------------------------
// F&O Universe (the single output artifact)
// ---------------------------------------------------------------------------

/// The complete F&O universe produced by Block 01.
///
/// This is the single artifact consumed by all downstream blocks:
/// WebSocket subscription, binary parser decoding, OMS order validation,
/// storage persistence, and API endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FnoUniverse {
    /// All F&O underlyings, keyed by underlying symbol.
    pub underlyings: HashMap<String, FnoUnderlying>,

    /// All derivative contracts, keyed by their own security ID.
    pub derivative_contracts: HashMap<SecurityId, DerivativeContract>,

    /// Global lookup: any security ID to what it represents.
    /// Covers indices, equities, AND derivatives.
    pub instrument_info: HashMap<SecurityId, InstrumentInfo>,

    /// Option chains grouped by (underlying symbol, expiry date).
    pub option_chains: HashMap<OptionChainKey, OptionChain>,

    /// Expiry calendars per underlying symbol.
    pub expiry_calendars: HashMap<String, ExpiryCalendar>,

    /// All 31 subscribed indices (8 F&O + 23 Display), keyed by security ID.
    pub subscribed_indices: Vec<SubscribedIndex>,

    /// Build metadata for diagnostics.
    pub build_metadata: UniverseBuildMetadata,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- UnderlyingKind ---

    #[test]
    fn test_underlying_kind_as_str_all_variants() {
        assert_eq!(UnderlyingKind::NseIndex.as_str(), "NseIndex");
        assert_eq!(UnderlyingKind::BseIndex.as_str(), "BseIndex");
        assert_eq!(UnderlyingKind::Stock.as_str(), "Stock");
    }

    #[test]
    fn test_underlying_kind_display() {
        assert_eq!(format!("{}", UnderlyingKind::NseIndex), "NseIndex");
        assert_eq!(format!("{}", UnderlyingKind::Stock), "Stock");
    }

    #[test]
    fn test_underlying_kind_serde_roundtrip() {
        let original = UnderlyingKind::BseIndex;
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: UnderlyingKind = serde_json::from_str(&json).unwrap();
        assert_eq!(original, deserialized);
    }

    // --- DhanInstrumentKind ---

    #[test]
    fn test_dhan_instrument_kind_as_str_all_variants() {
        assert_eq!(DhanInstrumentKind::FutureIndex.as_str(), "FutureIndex");
        assert_eq!(DhanInstrumentKind::FutureStock.as_str(), "FutureStock");
        assert_eq!(DhanInstrumentKind::OptionIndex.as_str(), "OptionIndex");
        assert_eq!(DhanInstrumentKind::OptionStock.as_str(), "OptionStock");
    }

    #[test]
    fn test_dhan_instrument_kind_display() {
        assert_eq!(
            format!("{}", DhanInstrumentKind::FutureIndex),
            "FutureIndex"
        );
        assert_eq!(
            format!("{}", DhanInstrumentKind::OptionStock),
            "OptionStock"
        );
    }

    #[test]
    fn test_dhan_instrument_kind_serde_roundtrip() {
        let original = DhanInstrumentKind::OptionIndex;
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: DhanInstrumentKind = serde_json::from_str(&json).unwrap();
        assert_eq!(original, deserialized);
    }

    // --- IndexCategory ---

    #[test]
    fn test_index_category_as_str_all_variants() {
        assert_eq!(IndexCategory::FnoUnderlying.as_str(), "FnoUnderlying");
        assert_eq!(IndexCategory::DisplayIndex.as_str(), "DisplayIndex");
    }

    #[test]
    fn test_index_category_display() {
        assert_eq!(format!("{}", IndexCategory::FnoUnderlying), "FnoUnderlying");
        assert_eq!(format!("{}", IndexCategory::DisplayIndex), "DisplayIndex");
    }

    #[test]
    fn test_index_category_serde_roundtrip() {
        let original = IndexCategory::DisplayIndex;
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: IndexCategory = serde_json::from_str(&json).unwrap();
        assert_eq!(original, deserialized);
    }

    // --- IndexSubcategory ---

    #[test]
    fn test_index_subcategory_as_str_all_variants() {
        assert_eq!(IndexSubcategory::Volatility.as_str(), "Volatility");
        assert_eq!(IndexSubcategory::BroadMarket.as_str(), "BroadMarket");
        assert_eq!(IndexSubcategory::MidCap.as_str(), "MidCap");
        assert_eq!(IndexSubcategory::SmallCap.as_str(), "SmallCap");
        assert_eq!(IndexSubcategory::Sectoral.as_str(), "Sectoral");
        assert_eq!(IndexSubcategory::Thematic.as_str(), "Thematic");
        assert_eq!(IndexSubcategory::Fno.as_str(), "Fno");
    }

    #[test]
    fn test_index_subcategory_display() {
        assert_eq!(format!("{}", IndexSubcategory::Volatility), "Volatility");
        assert_eq!(format!("{}", IndexSubcategory::Fno), "Fno");
    }

    #[test]
    fn test_index_subcategory_serde_roundtrip() {
        let original = IndexSubcategory::Sectoral;
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: IndexSubcategory = serde_json::from_str(&json).unwrap();
        assert_eq!(original, deserialized);
    }

    // --- OptionChainKey equality and hash ---

    #[test]
    fn test_option_chain_key_equality() {
        let key1 = OptionChainKey {
            underlying_symbol: "NIFTY".to_string(),
            expiry_date: chrono::NaiveDate::from_ymd_opt(2026, 3, 27).unwrap(),
        };
        let key2 = OptionChainKey {
            underlying_symbol: "NIFTY".to_string(),
            expiry_date: chrono::NaiveDate::from_ymd_opt(2026, 3, 27).unwrap(),
        };
        assert_eq!(key1, key2);
    }

    #[test]
    fn test_option_chain_key_inequality_symbol() {
        let key1 = OptionChainKey {
            underlying_symbol: "NIFTY".to_string(),
            expiry_date: chrono::NaiveDate::from_ymd_opt(2026, 3, 27).unwrap(),
        };
        let key2 = OptionChainKey {
            underlying_symbol: "BANKNIFTY".to_string(),
            expiry_date: chrono::NaiveDate::from_ymd_opt(2026, 3, 27).unwrap(),
        };
        assert_ne!(key1, key2);
    }

    #[test]
    fn test_option_chain_key_inequality_expiry() {
        let key1 = OptionChainKey {
            underlying_symbol: "NIFTY".to_string(),
            expiry_date: chrono::NaiveDate::from_ymd_opt(2026, 3, 27).unwrap(),
        };
        let key2 = OptionChainKey {
            underlying_symbol: "NIFTY".to_string(),
            expiry_date: chrono::NaiveDate::from_ymd_opt(2026, 4, 30).unwrap(),
        };
        assert_ne!(key1, key2);
    }

    #[test]
    fn test_option_chain_key_hash_map_usage() {
        use std::collections::HashMap;
        let key = OptionChainKey {
            underlying_symbol: "NIFTY".to_string(),
            expiry_date: chrono::NaiveDate::from_ymd_opt(2026, 3, 27).unwrap(),
        };
        let mut map = HashMap::new();
        map.insert(key.clone(), 42);
        assert_eq!(map.get(&key), Some(&42));
    }

    // --- Duration serde helper ---

    #[test]
    fn test_build_metadata_duration_serde_roundtrip() {
        use chrono::{FixedOffset, Utc};
        let ist = FixedOffset::east_opt(19_800).unwrap();
        let metadata = UniverseBuildMetadata {
            csv_source: "primary".to_string(),
            csv_row_count: 100,
            parsed_row_count: 50,
            index_count: 10,
            equity_count: 20,
            underlying_count: 5,
            derivative_count: 100,
            option_chain_count: 10,
            build_duration: std::time::Duration::from_millis(1234),
            build_timestamp: Utc::now().with_timezone(&ist),
        };

        let json = serde_json::to_string(&metadata).unwrap();
        let deserialized: UniverseBuildMetadata = serde_json::from_str(&json).unwrap();

        assert_eq!(
            deserialized.build_duration,
            std::time::Duration::from_millis(1234)
        );
        assert_eq!(deserialized.csv_source, "primary");
        assert_eq!(deserialized.csv_row_count, 100);
    }
}

//! Domain types for the instrument universe.
//!
//! Block 01 produces these types. Every downstream block (WebSocket, OMS,
//! storage, API) consumes them. Types live in `common` so all crates
//! can depend on them without importing `core`.

use std::collections::HashMap;
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

    /// Build metadata for diagnostics.
    pub build_metadata: UniverseBuildMetadata,
}

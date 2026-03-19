//! Domain types for the instrument universe.
//!
//! Block 01 produces these types. Every downstream block (WebSocket, OMS,
//! storage, API) consumes them. Types live in `common` so all crates
//! can depend on them without importing `core`.

use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::types::{Exchange, ExchangeSegment, OptionType, SecurityId};

// ---------------------------------------------------------------------------
// Underlying Classification
// ---------------------------------------------------------------------------

/// Classification of an F&O underlying.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[rkyv(compare(PartialEq))]
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
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[rkyv(compare(PartialEq))]
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
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[rkyv(compare(PartialEq))]
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
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[rkyv(compare(PartialEq))]
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

// ---------------------------------------------------------------------------
// Archived → Owned Conversions (for zero-copy rkyv access)
// ---------------------------------------------------------------------------

impl From<&ArchivedUnderlyingKind> for UnderlyingKind {
    fn from(archived: &ArchivedUnderlyingKind) -> Self {
        match archived {
            ArchivedUnderlyingKind::NseIndex => Self::NseIndex,
            ArchivedUnderlyingKind::BseIndex => Self::BseIndex,
            ArchivedUnderlyingKind::Stock => Self::Stock,
        }
    }
}

impl From<&ArchivedDhanInstrumentKind> for DhanInstrumentKind {
    fn from(archived: &ArchivedDhanInstrumentKind) -> Self {
        match archived {
            ArchivedDhanInstrumentKind::FutureIndex => Self::FutureIndex,
            ArchivedDhanInstrumentKind::FutureStock => Self::FutureStock,
            ArchivedDhanInstrumentKind::OptionIndex => Self::OptionIndex,
            ArchivedDhanInstrumentKind::OptionStock => Self::OptionStock,
        }
    }
}

impl From<&ArchivedIndexCategory> for IndexCategory {
    fn from(archived: &ArchivedIndexCategory) -> Self {
        match archived {
            ArchivedIndexCategory::FnoUnderlying => Self::FnoUnderlying,
            ArchivedIndexCategory::DisplayIndex => Self::DisplayIndex,
        }
    }
}

/// Reconstruct a `NaiveDate` from an archived rkyv `i32_le` (days from CE).
///
/// # Safety contract
/// The caller must ensure the `i32_le` value was produced by [`NaiveDateAsI32`]
/// serialization of a valid `NaiveDate`.
#[allow(clippy::expect_used)] // APPROVED: archived value validated at cache write time
pub fn naive_date_from_archived_i32(days: &rkyv::rend::i32_le) -> NaiveDate {
    NaiveDate::from_num_days_from_ce_opt((*days).into())
        .expect("valid days-from-CE in validated rkyv archive") // APPROVED: archived value validated at cache write time
}

/// A subscribed index — one of the 31 documented indices.
///
/// 8 F&O indices (NIFTY, BANKNIFTY, etc.) + 23 display indices (VIX, sectoral, etc.).
/// All subscribed on IDX_I exchange segment.
#[derive(
    Debug, Clone, Serialize, Deserialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize,
)]
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
#[derive(
    Debug, Clone, Serialize, Deserialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize,
)]
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
#[derive(
    Debug, Clone, Serialize, Deserialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize,
)]
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
    #[rkyv(with = NaiveDateAsI32)]
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
#[derive(
    Debug, Clone, Serialize, Deserialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize,
)]
pub struct OptionChain {
    /// Underlying symbol.
    pub underlying_symbol: String,

    /// Expiry date for this chain.
    #[rkyv(with = NaiveDateAsI32)]
    pub expiry_date: NaiveDate,

    /// Call options sorted by strike price ascending.
    pub calls: Vec<OptionChainEntry>,

    /// Put options sorted by strike price ascending.
    pub puts: Vec<OptionChainEntry>,

    /// Corresponding future security ID for this expiry, if one exists.
    pub future_security_id: Option<SecurityId>,
}

/// One strike in an option chain.
#[derive(
    Debug, Clone, Serialize, Deserialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize,
)]
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
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[rkyv(derive(Hash, PartialEq, Eq))]
pub struct OptionChainKey {
    /// Underlying symbol.
    pub underlying_symbol: String,

    /// Expiry date.
    #[rkyv(with = NaiveDateAsI32)]
    pub expiry_date: NaiveDate,
}

// ---------------------------------------------------------------------------
// Expiry Calendar
// ---------------------------------------------------------------------------

/// Expiry calendar for a single underlying.
#[derive(
    Debug, Clone, Serialize, Deserialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize,
)]
pub struct ExpiryCalendar {
    /// Underlying symbol.
    pub underlying_symbol: String,

    /// Sorted expiry dates (ascending), all >= today at build time.
    #[rkyv(with = rkyv::with::Map<NaiveDateAsI32>)]
    pub expiry_dates: Vec<NaiveDate>,
}

// ---------------------------------------------------------------------------
// Instrument Info (global lookup)
// ---------------------------------------------------------------------------

/// Generic instrument info for any security ID.
///
/// Used by the WebSocket binary parser to decode what a security ID
/// represents when receiving tick data.
#[derive(
    Debug, Clone, Serialize, Deserialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize,
)]
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
        #[rkyv(with = NaiveDateAsI32)]
        expiry_date: NaiveDate,
        strike_price: f64,
        option_type: Option<OptionType>,
    },
}

// ---------------------------------------------------------------------------
// Build Metadata
// ---------------------------------------------------------------------------

/// Metadata about the universe build for logging and monitoring.
#[derive(
    Debug, Clone, Serialize, Deserialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize,
)]
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
    #[rkyv(with = DurationAsMillis)]
    pub build_duration: Duration,

    /// Timestamp when the build completed (IST).
    #[rkyv(with = DateTimeFixedOffsetAsI64)]
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

/// rkyv wrapper for `NaiveDate` — stored as `i32` (days from CE epoch).
///
/// chrono's rkyv-64 feature uses rkyv 0.7; we use rkyv 0.8. This bridges the gap.
/// Used via `#[rkyv(with = NaiveDateAsI32)]`.
pub struct NaiveDateAsI32;

impl rkyv::with::ArchiveWith<NaiveDate> for NaiveDateAsI32 {
    type Archived = rkyv::rend::i32_le;
    type Resolver = ();

    fn resolve_with(
        field: &NaiveDate,
        _resolver: Self::Resolver,
        out: rkyv::Place<Self::Archived>,
    ) {
        let value = rkyv::rend::i32_le::from_native(field.num_days_from_ce());
        rkyv::Archive::resolve(&value, (), out);
    }
}

impl<S: rkyv::rancor::Fallible + ?Sized> rkyv::with::SerializeWith<NaiveDate, S>
    for NaiveDateAsI32
{
    fn serialize_with(
        _field: &NaiveDate,
        _serializer: &mut S,
    ) -> Result<Self::Resolver, <S as rkyv::rancor::Fallible>::Error> {
        Ok(())
    }
}

impl<D: rkyv::rancor::Fallible + ?Sized>
    rkyv::with::DeserializeWith<rkyv::rend::i32_le, NaiveDate, D> for NaiveDateAsI32
where
    <D as rkyv::rancor::Fallible>::Error: rkyv::rancor::Source,
{
    fn deserialize_with(
        archived: &rkyv::rend::i32_le,
        _deserializer: &mut D,
    ) -> Result<NaiveDate, <D as rkyv::rancor::Fallible>::Error> {
        NaiveDate::from_num_days_from_ce_opt((*archived).into())
            .ok_or_else(|| rkyv::rancor::Source::new(NaiveDateRkyvError))
    }
}

/// Error type for invalid NaiveDate deserialization from rkyv.
#[derive(Debug)]
struct NaiveDateRkyvError;

impl fmt::Display for NaiveDateRkyvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid NaiveDate days-from-CE value in rkyv archive")
    }
}

impl std::error::Error for NaiveDateRkyvError {}

/// rkyv wrapper for `DateTime<FixedOffset>` — stored as `(i64, i32)` (timestamp secs + UTC offset secs).
///
/// Used via `#[rkyv(with = DateTimeFixedOffsetAsI64)]`.
pub struct DateTimeFixedOffsetAsI64;

/// Archived form of `DateTime<FixedOffset>`: timestamp seconds + UTC offset seconds.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[rkyv(compare(PartialEq))]
pub struct ArchivedDateTimeFixedOffset {
    pub timestamp_secs: i64,
    pub offset_secs: i32,
}

impl rkyv::with::ArchiveWith<DateTime<FixedOffset>> for DateTimeFixedOffsetAsI64 {
    type Archived = <ArchivedDateTimeFixedOffset as rkyv::Archive>::Archived;
    type Resolver = <ArchivedDateTimeFixedOffset as rkyv::Archive>::Resolver;

    fn resolve_with(
        field: &DateTime<FixedOffset>,
        resolver: Self::Resolver,
        out: rkyv::Place<Self::Archived>,
    ) {
        let proxy = ArchivedDateTimeFixedOffset {
            timestamp_secs: field.timestamp(),
            offset_secs: field.offset().local_minus_utc(),
        };
        rkyv::Archive::resolve(&proxy, resolver, out);
    }
}

impl<S: rkyv::rancor::Fallible + rkyv::ser::Writer + ?Sized>
    rkyv::with::SerializeWith<DateTime<FixedOffset>, S> for DateTimeFixedOffsetAsI64
{
    fn serialize_with(
        field: &DateTime<FixedOffset>,
        serializer: &mut S,
    ) -> Result<Self::Resolver, <S as rkyv::rancor::Fallible>::Error> {
        let proxy = ArchivedDateTimeFixedOffset {
            timestamp_secs: field.timestamp(),
            offset_secs: field.offset().local_minus_utc(),
        };
        rkyv::Serialize::serialize(&proxy, serializer)
    }
}

impl<D: rkyv::rancor::Fallible + ?Sized>
    rkyv::with::DeserializeWith<
        <ArchivedDateTimeFixedOffset as rkyv::Archive>::Archived,
        DateTime<FixedOffset>,
        D,
    > for DateTimeFixedOffsetAsI64
where
    <D as rkyv::rancor::Fallible>::Error: rkyv::rancor::Source,
{
    fn deserialize_with(
        archived: &<ArchivedDateTimeFixedOffset as rkyv::Archive>::Archived,
        deserializer: &mut D,
    ) -> Result<DateTime<FixedOffset>, <D as rkyv::rancor::Fallible>::Error> {
        let proxy: ArchivedDateTimeFixedOffset =
            rkyv::Deserialize::deserialize(archived, deserializer)?;
        let offset = FixedOffset::east_opt(proxy.offset_secs)
            .ok_or_else(|| rkyv::rancor::Source::new(DateTimeRkyvError))?;
        DateTime::from_timestamp(proxy.timestamp_secs, 0)
            .map(|dt| dt.with_timezone(&offset))
            .ok_or_else(|| rkyv::rancor::Source::new(DateTimeRkyvError))
    }
}

/// Error type for invalid DateTime deserialization from rkyv.
#[derive(Debug)]
struct DateTimeRkyvError;

impl fmt::Display for DateTimeRkyvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid DateTime<FixedOffset> value in rkyv archive")
    }
}

impl std::error::Error for DateTimeRkyvError {}

/// rkyv wrapper for `Duration` — stored as `u64` milliseconds.
///
/// Mirrors `duration_serde` above. Used via `#[rkyv(with = DurationAsMillis)]`.
pub struct DurationAsMillis;

impl rkyv::with::ArchiveWith<Duration> for DurationAsMillis {
    type Archived = rkyv::rend::u64_le;
    type Resolver = ();

    fn resolve_with(field: &Duration, _resolver: Self::Resolver, out: rkyv::Place<Self::Archived>) {
        #[allow(clippy::arithmetic_side_effects)]
        // APPROVED: Duration::as_millis() truncation u128→u64 is safe — won't overflow until year 584M+
        let millis = field.as_millis() as u64;
        let value = rkyv::rend::u64_le::from_native(millis);
        rkyv::Archive::resolve(&value, (), out);
    }
}

impl<S: rkyv::rancor::Fallible + ?Sized> rkyv::with::SerializeWith<Duration, S>
    for DurationAsMillis
{
    fn serialize_with(
        _field: &Duration,
        _serializer: &mut S,
    ) -> Result<Self::Resolver, <S as rkyv::rancor::Fallible>::Error> {
        Ok(())
    }
}

impl<D: rkyv::rancor::Fallible + ?Sized>
    rkyv::with::DeserializeWith<rkyv::rend::u64_le, Duration, D> for DurationAsMillis
{
    fn deserialize_with(
        archived: &rkyv::rend::u64_le,
        _deserializer: &mut D,
    ) -> Result<Duration, <D as rkyv::rancor::Fallible>::Error> {
        Ok(Duration::from_millis((*archived).into()))
    }
}

// ---------------------------------------------------------------------------
// Index Constituency (niftyindices.com data)
// ---------------------------------------------------------------------------

/// A single constituent stock within an NSE index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexConstituent {
    /// Index name (e.g., "Nifty 50", "Nifty Bank").
    pub index_name: String,
    /// Stock symbol (e.g., "RELIANCE", "HDFCBANK").
    pub symbol: String,
    /// ISIN code (e.g., "INE002A01018").
    pub isin: String,
    /// Weight in the index (percentage, 0.0 if not available).
    pub weight: f64,
    /// Industry/sector classification from the CSV.
    pub sector: String,
    /// Date when this constituency data was last downloaded.
    pub last_updated: NaiveDate,
}

/// Bidirectional index ↔ stock mapping built from niftyindices.com CSVs.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IndexConstituencyMap {
    /// Forward map: index name → list of constituent stocks.
    pub index_to_constituents: HashMap<String, Vec<IndexConstituent>>,
    /// Reverse map: stock symbol → list of index names containing this stock.
    pub stock_to_indices: HashMap<String, Vec<String>>,
    /// Build metadata for diagnostics.
    pub build_metadata: ConstituencyBuildMetadata,
}

impl IndexConstituencyMap {
    /// O(1) forward lookup: get all constituents of an index.
    pub fn get_constituents(&self, index_name: &str) -> Option<&[IndexConstituent]> {
        self.index_to_constituents
            .get(index_name)
            .map(|v| v.as_slice())
    }

    /// O(1) reverse lookup: get all indices containing a stock.
    pub fn get_indices_for_stock(&self, symbol: &str) -> Option<&[String]> {
        self.stock_to_indices.get(symbol).map(|v| v.as_slice())
    }

    /// Number of indices in the map.
    pub fn index_count(&self) -> usize {
        self.index_to_constituents.len()
    }

    /// Number of unique stocks across all indices.
    pub fn stock_count(&self) -> usize {
        self.stock_to_indices.len()
    }

    /// Sorted list of all index names.
    pub fn all_index_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self
            .index_to_constituents
            .keys()
            .map(|s| s.as_str())
            .collect();
        names.sort_unstable();
        names
    }

    /// Quick membership check for a stock symbol.
    pub fn contains_stock(&self, symbol: &str) -> bool {
        self.stock_to_indices.contains_key(symbol)
    }

    /// Quick membership check for an index name.
    pub fn contains_index(&self, index_name: &str) -> bool {
        self.index_to_constituents.contains_key(index_name)
    }
}

/// Build metadata for index constituency download and parsing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConstituencyBuildMetadata {
    /// Total time spent downloading and parsing CSVs.
    #[serde(with = "duration_serde")]
    pub download_duration: Duration,
    /// Number of indices successfully downloaded.
    pub indices_downloaded: usize,
    /// Number of indices that failed to download.
    pub indices_failed: usize,
    /// Number of unique stocks across all indices.
    pub unique_stocks: usize,
    /// Total number of (index, stock) mappings.
    pub total_mappings: usize,
    /// Timestamp when the build completed (IST).
    pub build_timestamp: DateTime<FixedOffset>,
}

impl Default for ConstituencyBuildMetadata {
    fn default() -> Self {
        #[allow(clippy::expect_used)] // APPROVED: compile-time provable — 19800 always valid
        let ist = FixedOffset::east_opt(crate::constants::IST_UTC_OFFSET_SECONDS)
            .expect("IST offset 19800s is always valid"); // APPROVED: compile-time provable constant
        Self {
            download_duration: Duration::default(),
            indices_downloaded: 0,
            indices_failed: 0,
            unique_stocks: 0,
            total_mappings: 0,
            build_timestamp: Utc::now().with_timezone(&ist),
        }
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
#[derive(
    Debug, Clone, Serialize, Deserialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize,
)]
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

impl FnoUniverse {
    /// O(1) lookup: symbol → equity/index price feed security ID.
    ///
    /// For stocks: "RELIANCE" → 2885 (NSE_EQ ID).
    /// For indices: "NIFTY" → 13 (IDX_I ID).
    /// Returns `None` if the symbol is not an F&O underlying.
    pub fn symbol_to_security_id(&self, symbol: &str) -> Option<SecurityId> {
        self.underlyings
            .get(symbol)
            .map(|u| u.price_feed_security_id)
    }

    /// O(1) lookup: symbol → underlying details (lot size, segment, kind, etc.).
    pub fn get_underlying(&self, symbol: &str) -> Option<&FnoUnderlying> {
        self.underlyings.get(symbol)
    }

    /// O(n) lookup: symbol → all derivative contract security IDs for that underlying.
    /// Returns futures + options contracts. Empty vec if symbol not found.
    pub fn derivative_security_ids_for_symbol(&self, symbol: &str) -> Vec<SecurityId> {
        self.derivative_contracts
            .values()
            .filter(|c| c.underlying_symbol == symbol)
            .map(|c| c.security_id)
            .collect()
    }

    /// O(1) lookup: security_id → symbol string.
    /// Works for indices, equities, and derivatives (returns underlying_symbol for derivatives).
    pub fn security_id_to_symbol(&self, security_id: SecurityId) -> Option<&str> {
        self.instrument_info
            .get(&security_id)
            .map(|info| match info {
                InstrumentInfo::Index { symbol, .. } => symbol.as_str(),
                InstrumentInfo::Equity { symbol, .. } => symbol.as_str(),
                InstrumentInfo::Derivative {
                    underlying_symbol, ..
                } => underlying_symbol.as_str(),
            })
    }

    /// O(1) lookup: index symbol → index security ID from subscribed_indices.
    /// "NIFTY" → 13, "BANKNIFTY" → 29.
    pub fn index_symbol_to_security_id(&self, symbol: &str) -> Option<SecurityId> {
        self.subscribed_indices
            .iter()
            .find(|idx| idx.symbol == symbol)
            .map(|idx| idx.security_id)
    }
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

    // =====================================================================
    // FnoUniverse method tests — exhaustive coverage
    // =====================================================================

    fn make_test_build_metadata() -> UniverseBuildMetadata {
        use chrono::{FixedOffset, Utc};
        let ist = FixedOffset::east_opt(19_800).unwrap();
        UniverseBuildMetadata {
            csv_source: "test".to_string(),
            csv_row_count: 0,
            parsed_row_count: 0,
            index_count: 0,
            equity_count: 0,
            underlying_count: 0,
            derivative_count: 0,
            option_chain_count: 0,
            build_duration: std::time::Duration::ZERO,
            build_timestamp: Utc::now().with_timezone(&ist),
        }
    }

    fn make_test_fno_universe() -> FnoUniverse {
        use crate::types::{Exchange, ExchangeSegment, OptionType};

        let mut underlyings = HashMap::new();
        underlyings.insert(
            "RELIANCE".to_string(),
            FnoUnderlying {
                underlying_symbol: "RELIANCE".to_string(),
                underlying_security_id: 26000,
                price_feed_security_id: 2885,
                price_feed_segment: ExchangeSegment::NseEquity,
                derivative_segment: ExchangeSegment::NseFno,
                kind: UnderlyingKind::Stock,
                lot_size: 250,
                contract_count: 3,
            },
        );
        underlyings.insert(
            "NIFTY".to_string(),
            FnoUnderlying {
                underlying_symbol: "NIFTY".to_string(),
                underlying_security_id: 26000,
                price_feed_security_id: 13,
                price_feed_segment: ExchangeSegment::IdxI,
                derivative_segment: ExchangeSegment::NseFno,
                kind: UnderlyingKind::NseIndex,
                lot_size: 25,
                contract_count: 2,
            },
        );
        underlyings.insert(
            "HDFCBANK".to_string(),
            FnoUnderlying {
                underlying_symbol: "HDFCBANK".to_string(),
                underlying_security_id: 27000,
                price_feed_security_id: 1333,
                price_feed_segment: ExchangeSegment::NseEquity,
                derivative_segment: ExchangeSegment::NseFno,
                kind: UnderlyingKind::Stock,
                lot_size: 550,
                contract_count: 1,
            },
        );

        let mut derivative_contracts = HashMap::new();
        derivative_contracts.insert(
            49001,
            DerivativeContract {
                security_id: 49001,
                underlying_symbol: "RELIANCE".to_string(),
                instrument_kind: DhanInstrumentKind::FutureStock,
                exchange_segment: ExchangeSegment::NseFno,
                expiry_date: chrono::NaiveDate::from_ymd_opt(2026, 3, 27).unwrap(),
                strike_price: 0.0,
                option_type: None,
                lot_size: 250,
                tick_size: 0.05,
                symbol_name: "RELIANCE-Mar2026-FUT".to_string(),
                display_name: "RELIANCE Mar2026 Future".to_string(),
            },
        );
        derivative_contracts.insert(
            49002,
            DerivativeContract {
                security_id: 49002,
                underlying_symbol: "RELIANCE".to_string(),
                instrument_kind: DhanInstrumentKind::OptionStock,
                exchange_segment: ExchangeSegment::NseFno,
                expiry_date: chrono::NaiveDate::from_ymd_opt(2026, 3, 27).unwrap(),
                strike_price: 3000.0,
                option_type: Some(OptionType::Call),
                lot_size: 250,
                tick_size: 0.05,
                symbol_name: "RELIANCE-Mar2026-3000-CE".to_string(),
                display_name: "RELIANCE Mar2026 3000 CE".to_string(),
            },
        );
        derivative_contracts.insert(
            49003,
            DerivativeContract {
                security_id: 49003,
                underlying_symbol: "RELIANCE".to_string(),
                instrument_kind: DhanInstrumentKind::OptionStock,
                exchange_segment: ExchangeSegment::NseFno,
                expiry_date: chrono::NaiveDate::from_ymd_opt(2026, 3, 27).unwrap(),
                strike_price: 3000.0,
                option_type: Some(OptionType::Put),
                lot_size: 250,
                tick_size: 0.05,
                symbol_name: "RELIANCE-Mar2026-3000-PE".to_string(),
                display_name: "RELIANCE Mar2026 3000 PE".to_string(),
            },
        );
        derivative_contracts.insert(
            50001,
            DerivativeContract {
                security_id: 50001,
                underlying_symbol: "NIFTY".to_string(),
                instrument_kind: DhanInstrumentKind::FutureIndex,
                exchange_segment: ExchangeSegment::NseFno,
                expiry_date: chrono::NaiveDate::from_ymd_opt(2026, 3, 27).unwrap(),
                strike_price: 0.0,
                option_type: None,
                lot_size: 25,
                tick_size: 0.05,
                symbol_name: "NIFTY-Mar2026-FUT".to_string(),
                display_name: "NIFTY Mar2026 Future".to_string(),
            },
        );
        derivative_contracts.insert(
            50002,
            DerivativeContract {
                security_id: 50002,
                underlying_symbol: "NIFTY".to_string(),
                instrument_kind: DhanInstrumentKind::OptionIndex,
                exchange_segment: ExchangeSegment::NseFno,
                expiry_date: chrono::NaiveDate::from_ymd_opt(2026, 3, 27).unwrap(),
                strike_price: 25000.0,
                option_type: Some(OptionType::Call),
                lot_size: 25,
                tick_size: 0.05,
                symbol_name: "NIFTY-Mar2026-25000-CE".to_string(),
                display_name: "NIFTY Mar2026 25000 CE".to_string(),
            },
        );
        derivative_contracts.insert(
            51001,
            DerivativeContract {
                security_id: 51001,
                underlying_symbol: "HDFCBANK".to_string(),
                instrument_kind: DhanInstrumentKind::FutureStock,
                exchange_segment: ExchangeSegment::NseFno,
                expiry_date: chrono::NaiveDate::from_ymd_opt(2026, 3, 27).unwrap(),
                strike_price: 0.0,
                option_type: None,
                lot_size: 550,
                tick_size: 0.05,
                symbol_name: "HDFCBANK-Mar2026-FUT".to_string(),
                display_name: "HDFCBANK Mar2026 Future".to_string(),
            },
        );

        let mut instrument_info = HashMap::new();
        instrument_info.insert(
            13,
            InstrumentInfo::Index {
                security_id: 13,
                symbol: "NIFTY".to_string(),
                exchange: Exchange::NationalStockExchange,
            },
        );
        instrument_info.insert(
            2885,
            InstrumentInfo::Equity {
                security_id: 2885,
                symbol: "RELIANCE".to_string(),
            },
        );
        instrument_info.insert(
            1333,
            InstrumentInfo::Equity {
                security_id: 1333,
                symbol: "HDFCBANK".to_string(),
            },
        );
        instrument_info.insert(
            49001,
            InstrumentInfo::Derivative {
                security_id: 49001,
                underlying_symbol: "RELIANCE".to_string(),
                instrument_kind: DhanInstrumentKind::FutureStock,
                exchange_segment: ExchangeSegment::NseFno,
                expiry_date: chrono::NaiveDate::from_ymd_opt(2026, 3, 27).unwrap(),
                strike_price: 0.0,
                option_type: None,
            },
        );
        instrument_info.insert(
            50002,
            InstrumentInfo::Derivative {
                security_id: 50002,
                underlying_symbol: "NIFTY".to_string(),
                instrument_kind: DhanInstrumentKind::OptionIndex,
                exchange_segment: ExchangeSegment::NseFno,
                expiry_date: chrono::NaiveDate::from_ymd_opt(2026, 3, 27).unwrap(),
                strike_price: 25000.0,
                option_type: Some(OptionType::Call),
            },
        );

        let subscribed_indices = vec![
            SubscribedIndex {
                symbol: "NIFTY".to_string(),
                security_id: 13,
                exchange: Exchange::NationalStockExchange,
                category: IndexCategory::FnoUnderlying,
                subcategory: IndexSubcategory::Fno,
            },
            SubscribedIndex {
                symbol: "BANKNIFTY".to_string(),
                security_id: 29,
                exchange: Exchange::NationalStockExchange,
                category: IndexCategory::FnoUnderlying,
                subcategory: IndexSubcategory::Fno,
            },
            SubscribedIndex {
                symbol: "INDIA VIX".to_string(),
                security_id: 25,
                exchange: Exchange::NationalStockExchange,
                category: IndexCategory::DisplayIndex,
                subcategory: IndexSubcategory::Volatility,
            },
        ];

        FnoUniverse {
            underlyings,
            derivative_contracts,
            instrument_info,
            option_chains: HashMap::new(),
            expiry_calendars: HashMap::new(),
            subscribed_indices,
            build_metadata: make_test_build_metadata(),
        }
    }

    // --- symbol_to_security_id ---

    #[test]
    fn test_symbol_to_security_id_stock() {
        let universe = make_test_fno_universe();
        assert_eq!(universe.symbol_to_security_id("RELIANCE"), Some(2885));
        assert_eq!(universe.symbol_to_security_id("HDFCBANK"), Some(1333));
    }

    #[test]
    fn test_symbol_to_security_id_index() {
        let universe = make_test_fno_universe();
        assert_eq!(universe.symbol_to_security_id("NIFTY"), Some(13));
    }

    #[test]
    fn test_symbol_to_security_id_unknown_returns_none() {
        let universe = make_test_fno_universe();
        assert_eq!(universe.symbol_to_security_id("NONEXISTENT"), None);
        assert_eq!(universe.symbol_to_security_id(""), None);
    }

    #[test]
    fn test_symbol_to_security_id_case_sensitive() {
        let universe = make_test_fno_universe();
        assert!(universe.symbol_to_security_id("Reliance").is_none());
        assert!(universe.symbol_to_security_id("nifty").is_none());
        assert!(universe.symbol_to_security_id("RELIANCE").is_some());
    }

    #[test]
    fn test_symbol_to_security_id_empty_universe() {
        let universe = FnoUniverse {
            underlyings: HashMap::new(),
            derivative_contracts: HashMap::new(),
            instrument_info: HashMap::new(),
            option_chains: HashMap::new(),
            expiry_calendars: HashMap::new(),
            subscribed_indices: Vec::new(),
            build_metadata: make_test_build_metadata(),
        };
        assert_eq!(universe.symbol_to_security_id("RELIANCE"), None);
    }

    // --- security_id_to_symbol ---

    #[test]
    fn test_security_id_to_symbol_equity() {
        let universe = make_test_fno_universe();
        assert_eq!(universe.security_id_to_symbol(2885), Some("RELIANCE"));
        assert_eq!(universe.security_id_to_symbol(1333), Some("HDFCBANK"));
    }

    #[test]
    fn test_security_id_to_symbol_index() {
        let universe = make_test_fno_universe();
        assert_eq!(universe.security_id_to_symbol(13), Some("NIFTY"));
    }

    #[test]
    fn test_security_id_to_symbol_derivative_returns_underlying() {
        let universe = make_test_fno_universe();
        assert_eq!(universe.security_id_to_symbol(49001), Some("RELIANCE"));
        assert_eq!(universe.security_id_to_symbol(50002), Some("NIFTY"));
    }

    #[test]
    fn test_security_id_to_symbol_unknown_returns_none() {
        let universe = make_test_fno_universe();
        assert_eq!(universe.security_id_to_symbol(99999), None);
        assert_eq!(universe.security_id_to_symbol(0), None);
    }

    // --- Roundtrip: symbol → security_id → symbol ---

    #[test]
    fn test_symbol_security_id_roundtrip() {
        let universe = make_test_fno_universe();
        for symbol in &["RELIANCE", "HDFCBANK", "NIFTY"] {
            let sec_id = universe
                .symbol_to_security_id(symbol)
                .unwrap_or_else(|| panic!("{symbol} should have security_id"));
            let back = universe
                .security_id_to_symbol(sec_id)
                .unwrap_or_else(|| panic!("security_id {sec_id} should resolve"));
            assert_eq!(*symbol, back, "roundtrip failed for {symbol}");
        }
    }

    // --- get_underlying ---

    #[test]
    fn test_get_underlying_returns_full_details() {
        let universe = make_test_fno_universe();
        let u = universe.get_underlying("RELIANCE").unwrap();
        assert_eq!(u.underlying_symbol, "RELIANCE");
        assert_eq!(u.price_feed_security_id, 2885);
        assert_eq!(u.lot_size, 250);
        assert_eq!(u.kind, UnderlyingKind::Stock);
        assert_eq!(u.contract_count, 3);
    }

    #[test]
    fn test_get_underlying_none_for_unknown() {
        let universe = make_test_fno_universe();
        assert!(universe.get_underlying("TCS").is_none());
    }

    // --- derivative_security_ids_for_symbol ---

    #[test]
    fn test_derivative_ids_for_stock() {
        let universe = make_test_fno_universe();
        let mut ids = universe.derivative_security_ids_for_symbol("RELIANCE");
        ids.sort();
        assert_eq!(ids, vec![49001, 49002, 49003]);
    }

    #[test]
    fn test_derivative_ids_for_index() {
        let universe = make_test_fno_universe();
        let mut ids = universe.derivative_security_ids_for_symbol("NIFTY");
        ids.sort();
        assert_eq!(ids, vec![50001, 50002]);
    }

    #[test]
    fn test_derivative_ids_unknown_symbol_empty() {
        let universe = make_test_fno_universe();
        assert!(
            universe
                .derivative_security_ids_for_symbol("NONEXISTENT")
                .is_empty()
        );
    }

    #[test]
    fn test_derivative_ids_no_cross_contamination() {
        let universe = make_test_fno_universe();
        let reliance_ids = universe.derivative_security_ids_for_symbol("RELIANCE");
        assert!(!reliance_ids.contains(&50001)); // NIFTY future
        assert!(!reliance_ids.contains(&51001)); // HDFCBANK future
    }

    // --- index_symbol_to_security_id ---

    #[test]
    fn test_index_symbol_to_security_id_known() {
        let universe = make_test_fno_universe();
        assert_eq!(universe.index_symbol_to_security_id("NIFTY"), Some(13));
        assert_eq!(universe.index_symbol_to_security_id("BANKNIFTY"), Some(29));
        assert_eq!(universe.index_symbol_to_security_id("INDIA VIX"), Some(25));
    }

    #[test]
    fn test_index_symbol_to_security_id_unknown() {
        let universe = make_test_fno_universe();
        assert_eq!(universe.index_symbol_to_security_id("NONEXISTENT"), None);
    }

    // --- Idempotency: repeated calls give same results ---

    #[test]
    fn test_idempotent_lookups_100_iterations() {
        let universe = make_test_fno_universe();
        for _ in 0..100 {
            assert_eq!(universe.symbol_to_security_id("RELIANCE"), Some(2885));
            assert_eq!(universe.security_id_to_symbol(2885), Some("RELIANCE"));
            assert_eq!(universe.index_symbol_to_security_id("NIFTY"), Some(13));
        }
    }

    // --- Cross-day security_id reuse scenario ---

    #[test]
    fn test_cross_day_security_id_reuse() {
        // Day 1: 49001 = RELIANCE FUT
        let day1 = make_test_fno_universe();
        assert_eq!(day1.security_id_to_symbol(49001), Some("RELIANCE"));

        // Day 2: 49001 reused for HDFCBANK FUT (post-expiry reassignment)
        let mut day2 = make_test_fno_universe();
        day2.instrument_info.insert(
            49001,
            InstrumentInfo::Derivative {
                security_id: 49001,
                underlying_symbol: "HDFCBANK".to_string(),
                instrument_kind: DhanInstrumentKind::FutureStock,
                exchange_segment: ExchangeSegment::NseFno,
                expiry_date: chrono::NaiveDate::from_ymd_opt(2026, 4, 24).unwrap(),
                strike_price: 0.0,
                option_type: None,
            },
        );
        // Same security_id, different symbol on different day
        assert_eq!(day1.security_id_to_symbol(49001), Some("RELIANCE"));
        assert_eq!(day2.security_id_to_symbol(49001), Some("HDFCBANK"));
    }

    // --- Structural invariants ---

    #[test]
    fn test_all_underlyings_have_nonzero_price_feed_id() {
        let universe = make_test_fno_universe();
        for (symbol, u) in &universe.underlyings {
            assert!(
                u.price_feed_security_id > 0,
                "{symbol} has zero price_feed_security_id"
            );
        }
    }

    #[test]
    fn test_all_derivatives_reference_valid_underlying() {
        let universe = make_test_fno_universe();
        for (sec_id, contract) in &universe.derivative_contracts {
            assert!(
                universe
                    .underlyings
                    .contains_key(&contract.underlying_symbol),
                "derivative {sec_id} references unknown underlying: {}",
                contract.underlying_symbol
            );
        }
    }

    #[test]
    fn test_instrument_info_no_duplicate_security_ids() {
        let universe = make_test_fno_universe();
        let count = universe.instrument_info.len();
        let unique: std::collections::HashSet<_> = universe.instrument_info.keys().collect();
        assert_eq!(count, unique.len());
    }

    // --- Duplicate insertion into HashMap: last wins ---

    #[test]
    fn test_duplicate_security_id_in_instrument_info_last_wins() {
        let mut info = HashMap::new();
        info.insert(
            2885_u32,
            InstrumentInfo::Equity {
                security_id: 2885,
                symbol: "RELIANCE".to_string(),
            },
        );
        info.insert(
            2885_u32,
            InstrumentInfo::Equity {
                security_id: 2885,
                symbol: "CHANGED".to_string(),
            },
        );
        assert_eq!(info.len(), 1);
        if let InstrumentInfo::Equity { symbol, .. } = &info[&2885] {
            assert_eq!(symbol, "CHANGED");
        }
    }
}

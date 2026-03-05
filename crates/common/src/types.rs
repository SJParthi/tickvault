//! Core domain types shared across all crates.
//!
//! These types represent the fundamental building blocks of the trading system.
//! They are used by every downstream module — WebSocket, parser, OMS, storage.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Exchange identifier.
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
pub enum Exchange {
    /// National Stock Exchange of India.
    NationalStockExchange,
    /// Bombay Stock Exchange.
    BombayStockExchange,
}

impl Exchange {
    /// Returns the canonical string representation for storage and display.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NationalStockExchange => "NSE",
            Self::BombayStockExchange => "BSE",
        }
    }
}

impl fmt::Display for Exchange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Exchange segment for subscription and routing.
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
pub enum ExchangeSegment {
    /// Index segment (both NSE and BSE indices).
    IdxI,
    /// NSE Equity segment.
    NseEquity,
    /// NSE Futures & Options segment.
    NseFno,
    /// BSE Equity segment.
    BseEquity,
    /// BSE Futures & Options segment.
    BseFno,
    /// MCX Commodity segment.
    McxComm,
}

impl ExchangeSegment {
    /// Returns the canonical string representation for storage and display.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::IdxI => "IDX_I",
            Self::NseEquity => "NSE_EQ",
            Self::NseFno => "NSE_FNO",
            Self::BseEquity => "BSE_EQ",
            Self::BseFno => "BSE_FNO",
            Self::McxComm => "MCX_COMM",
        }
    }
}

impl fmt::Display for ExchangeSegment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// WebSocket feed mode determining the data granularity.
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
pub enum FeedMode {
    /// Compact price data (16 bytes: header + LTP + LTT).
    Ticker,
    /// Price + volume + OHLC (50 bytes).
    Quote,
    /// Full: quote + OI + 5-level market depth (162 bytes).
    Full,
}

impl FeedMode {
    /// Returns the canonical string representation for storage and display.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ticker => "Ticker",
            Self::Quote => "Quote",
            Self::Full => "Full",
        }
    }
}

impl fmt::Display for FeedMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Instrument type classification.
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
pub enum InstrumentType {
    /// Index (e.g., NIFTY, BANKNIFTY).
    Index,
    /// Equity stock (e.g., RELIANCE, HDFCBANK).
    Equity,
    /// Futures contract.
    Future,
    /// Options contract.
    Option,
}

impl InstrumentType {
    /// Returns the canonical string representation for storage and display.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Index => "Index",
            Self::Equity => "Equity",
            Self::Future => "Future",
            Self::Option => "Option",
        }
    }
}

impl fmt::Display for InstrumentType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Option type for derivatives.
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
pub enum OptionType {
    /// Call option.
    Call,
    /// Put option.
    Put,
}

impl OptionType {
    /// Returns the canonical string representation for storage and display.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Call => "CE",
            Self::Put => "PE",
        }
    }
}

impl fmt::Display for OptionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Unique identifier for a security in the Dhan system.
pub type SecurityId = u32;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- Exchange ---

    #[test]
    fn test_exchange_as_str() {
        assert_eq!(Exchange::NationalStockExchange.as_str(), "NSE");
        assert_eq!(Exchange::BombayStockExchange.as_str(), "BSE");
    }

    #[test]
    fn test_exchange_display() {
        assert_eq!(format!("{}", Exchange::NationalStockExchange), "NSE");
        assert_eq!(format!("{}", Exchange::BombayStockExchange), "BSE");
    }

    #[test]
    fn test_exchange_debug() {
        let debug = format!("{:?}", Exchange::NationalStockExchange);
        assert!(debug.contains("NationalStockExchange"));
    }

    #[test]
    fn test_exchange_clone_eq() {
        let a = Exchange::NationalStockExchange;
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn test_exchange_serde_roundtrip() {
        let original = Exchange::NationalStockExchange;
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: Exchange = serde_json::from_str(&json).unwrap();
        assert_eq!(original, deserialized);
    }

    // --- ExchangeSegment ---

    #[test]
    fn test_exchange_segment_as_str_all_variants() {
        assert_eq!(ExchangeSegment::IdxI.as_str(), "IDX_I");
        assert_eq!(ExchangeSegment::NseEquity.as_str(), "NSE_EQ");
        assert_eq!(ExchangeSegment::NseFno.as_str(), "NSE_FNO");
        assert_eq!(ExchangeSegment::BseEquity.as_str(), "BSE_EQ");
        assert_eq!(ExchangeSegment::BseFno.as_str(), "BSE_FNO");
        assert_eq!(ExchangeSegment::McxComm.as_str(), "MCX_COMM");
    }

    #[test]
    fn test_exchange_segment_display() {
        assert_eq!(format!("{}", ExchangeSegment::NseFno), "NSE_FNO");
        assert_eq!(format!("{}", ExchangeSegment::BseFno), "BSE_FNO");
    }

    #[test]
    fn test_exchange_segment_serde_roundtrip() {
        let original = ExchangeSegment::McxComm;
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: ExchangeSegment = serde_json::from_str(&json).unwrap();
        assert_eq!(original, deserialized);
    }

    // --- FeedMode ---

    #[test]
    fn test_feed_mode_as_str_all_variants() {
        assert_eq!(FeedMode::Ticker.as_str(), "Ticker");
        assert_eq!(FeedMode::Quote.as_str(), "Quote");
        assert_eq!(FeedMode::Full.as_str(), "Full");
    }

    #[test]
    fn test_feed_mode_display() {
        assert_eq!(format!("{}", FeedMode::Ticker), "Ticker");
        assert_eq!(format!("{}", FeedMode::Full), "Full");
    }

    #[test]
    fn test_feed_mode_serde_roundtrip() {
        let original = FeedMode::Quote;
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: FeedMode = serde_json::from_str(&json).unwrap();
        assert_eq!(original, deserialized);
    }

    // --- InstrumentType ---

    #[test]
    fn test_instrument_type_as_str_all_variants() {
        assert_eq!(InstrumentType::Index.as_str(), "Index");
        assert_eq!(InstrumentType::Equity.as_str(), "Equity");
        assert_eq!(InstrumentType::Future.as_str(), "Future");
        assert_eq!(InstrumentType::Option.as_str(), "Option");
    }

    #[test]
    fn test_instrument_type_display() {
        assert_eq!(format!("{}", InstrumentType::Future), "Future");
        assert_eq!(format!("{}", InstrumentType::Option), "Option");
    }

    #[test]
    fn test_instrument_type_serde_roundtrip() {
        let original = InstrumentType::Equity;
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: InstrumentType = serde_json::from_str(&json).unwrap();
        assert_eq!(original, deserialized);
    }

    // --- OptionType ---

    #[test]
    fn test_option_type_as_str() {
        assert_eq!(OptionType::Call.as_str(), "CE");
        assert_eq!(OptionType::Put.as_str(), "PE");
    }

    #[test]
    fn test_option_type_display() {
        assert_eq!(format!("{}", OptionType::Call), "CE");
        assert_eq!(format!("{}", OptionType::Put), "PE");
    }

    #[test]
    fn test_option_type_serde_roundtrip() {
        let original = OptionType::Put;
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: OptionType = serde_json::from_str(&json).unwrap();
        assert_eq!(original, deserialized);
    }

    // --- Hash consistency (used as HashMap keys) ---

    #[test]
    fn test_exchange_segment_hash_consistency() {
        use std::collections::HashMap;
        let mut map = HashMap::new();
        map.insert(ExchangeSegment::NseFno, "nse_fno");
        map.insert(ExchangeSegment::BseFno, "bse_fno");
        assert_eq!(map.get(&ExchangeSegment::NseFno), Some(&"nse_fno"));
        assert_eq!(map.get(&ExchangeSegment::BseFno), Some(&"bse_fno"));
        assert_eq!(map.get(&ExchangeSegment::IdxI), None);
    }
}

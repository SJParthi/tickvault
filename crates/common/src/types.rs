//! Core domain types shared across all crates.
//!
//! These types represent the fundamental building blocks of the trading system.
//! They are used by every downstream module — WebSocket, parser, OMS, storage.

use serde::{Deserialize, Serialize};

/// Exchange identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Exchange {
    /// National Stock Exchange of India.
    NationalStockExchange,
    /// Bombay Stock Exchange.
    BombayStockExchange,
}

/// Exchange segment for subscription and routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

/// WebSocket feed mode determining the data granularity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FeedMode {
    /// Compact price data (25 bytes).
    Ticker,
    /// Price + best bid/ask (51 bytes).
    Quote,
    /// Full market depth with OI (162 bytes).
    Full,
}

/// Instrument type classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

/// Option type for derivatives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OptionType {
    /// Call option.
    Call,
    /// Put option.
    Put,
}

/// Unique identifier for a security in the Dhan system.
pub type SecurityId = u32;

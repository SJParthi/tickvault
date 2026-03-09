//! Tick and market depth types shared across the pipeline.
//!
//! These types flow from the binary parser through the tick processor
//! to QuestDB persistence. They must be Copy for zero-allocation on the hot path.

// ---------------------------------------------------------------------------
// Parsed Tick — output of binary frame parser
// ---------------------------------------------------------------------------

/// A fully parsed tick from the Dhan WebSocket V2 binary protocol.
///
/// All price fields are f32 in rupees (NOT paise — no division needed).
/// This struct is `Copy` for zero-allocation on the hot path.
#[derive(Debug, Clone, Copy)]
pub struct ParsedTick {
    /// Dhan security identifier.
    pub security_id: u32,
    /// Binary exchange segment code (0=IDX, 1=NSE_EQ, 2=NSE_FNO, etc.).
    pub exchange_segment_code: u8,
    /// Last traded price in rupees (f32).
    pub last_traded_price: f32,
    /// Last trade quantity (from Quote/Full packets; 0 for Ticker).
    pub last_trade_quantity: u16,
    /// Exchange timestamp as IST-naive epoch seconds (Dhan sends IST clock as if UTC).
    pub exchange_timestamp: u32,
    /// Local receive timestamp in nanoseconds since Unix epoch (for latency measurement).
    pub received_at_nanos: i64,
    /// Average traded price (from Quote/Full; 0.0 for Ticker).
    pub average_traded_price: f32,
    /// Cumulative day volume (from Quote/Full; 0 for Ticker).
    pub volume: u32,
    /// Total sell quantity (from Quote/Full; 0 for Ticker).
    pub total_sell_quantity: u32,
    /// Total buy quantity (from Quote/Full; 0 for Ticker).
    pub total_buy_quantity: u32,
    /// Day open price (from Quote/Full; 0.0 for Ticker).
    pub day_open: f32,
    /// Previous close price (from Quote/Full; 0.0 for Ticker).
    pub day_close: f32,
    /// Day high price (from Quote/Full; 0.0 for Ticker).
    pub day_high: f32,
    /// Day low price (from Quote/Full; 0.0 for Ticker).
    pub day_low: f32,
    /// Open interest (from Full packet; 0 for Ticker/Quote).
    pub open_interest: u32,
    /// OI day high (from Full packet; 0 for Ticker/Quote).
    pub oi_day_high: u32,
    /// OI day low (from Full packet; 0 for Ticker/Quote).
    pub oi_day_low: u32,
}

impl Default for ParsedTick {
    fn default() -> Self {
        Self {
            security_id: 0,
            exchange_segment_code: 0,
            last_traded_price: 0.0,
            last_trade_quantity: 0,
            exchange_timestamp: 0,
            received_at_nanos: 0,
            average_traded_price: 0.0,
            volume: 0,
            total_sell_quantity: 0,
            total_buy_quantity: 0,
            day_open: 0.0,
            day_close: 0.0,
            day_high: 0.0,
            day_low: 0.0,
            open_interest: 0,
            oi_day_high: 0,
            oi_day_low: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Market Depth Level — from Full packet (5 levels × 20 bytes)
// ---------------------------------------------------------------------------

/// A single level of market depth from the Full/MarketDepth packet.
///
/// Dhan sends 5 levels per packet. This struct is `Copy` + `Default` for stack allocation.
///
/// Dhan SDK format per level: `<IIHHff>` (u32, u32, u16, u16, f32, f32).
/// All quantity/order fields are unsigned per the Dhan binary protocol.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MarketDepthLevel {
    /// Bid quantity at this level (Dhan wire: u32).
    pub bid_quantity: u32,
    /// Ask quantity at this level (Dhan wire: u32).
    pub ask_quantity: u32,
    /// Number of bid orders at this level (Dhan wire: u16).
    pub bid_orders: u16,
    /// Number of ask orders at this level (Dhan wire: u16).
    pub ask_orders: u16,
    /// Bid price at this level (rupees, Dhan wire: f32).
    pub bid_price: f32,
    /// Ask price at this level (rupees, Dhan wire: f32).
    pub ask_price: f32,
}

// ---------------------------------------------------------------------------
// Deep Depth Level — from 20-level / 200-level depth feeds
// ---------------------------------------------------------------------------

/// A single level of market depth from the 20-level or 200-level depth feed.
///
/// These feeds use f64 prices (unlike the standard 5-level feed which uses f32)
/// and send bid/ask sides as separate packets. Each level contains
/// price, quantity, and order count for one side only.
///
/// Wire format per level: price(f64 LE, 8 bytes) + quantity(u32 LE, 4 bytes) + orders(u32 LE, 4 bytes) = 16 bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DeepDepthLevel {
    /// Price at this level (rupees, f64 — higher precision than 5-level f32).
    pub price: f64,
    /// Quantity at this level.
    pub quantity: u32,
    /// Number of orders at this level.
    pub orders: u32,
}

// ---------------------------------------------------------------------------
// Historical Candle — 1-minute OHLCV from Dhan intraday API
// ---------------------------------------------------------------------------

/// A single 1-minute OHLCV candle from Dhan's historical intraday API.
///
/// Used for cross-verification against live tick data and for backfill.
/// Timestamps are UTC epoch seconds (converted from IST-naive by the fetcher).
#[derive(Debug, Clone, Copy)]
pub struct HistoricalCandle {
    /// Candle open timestamp as UTC epoch seconds.
    pub timestamp_utc_secs: i64,
    /// Dhan security identifier.
    pub security_id: u32,
    /// Exchange segment code (matches `ParsedTick::exchange_segment_code`).
    pub exchange_segment_code: u8,
    /// Open price in rupees.
    pub open: f64,
    /// High price in rupees.
    pub high: f64,
    /// Low price in rupees.
    pub low: f64,
    /// Close price in rupees.
    pub close: f64,
    /// Volume for this candle interval.
    pub volume: i64,
    /// Open interest (F&O instruments only; 0 for equity).
    pub open_interest: i64,
}

/// Response from Dhan's intraday charts API.
///
/// Each field is a parallel array — index N across all arrays forms one candle.
/// Dhan V2 REST API returns timestamps as IST-naive epoch seconds — the IST
/// clock time encoded as if UTC. Same convention as the WebSocket binary feed.
///
/// Note: Dhan sometimes returns integer fields (volume, open_interest) as floats
/// (e.g., `105600.0` instead of `105600`). The `deserialize_f64_as_i64_vec`
/// deserializer handles both forms by truncating floats to i64.
#[derive(Debug, serde::Deserialize)]
pub struct DhanIntradayResponse {
    /// Opening prices per candle.
    pub open: Vec<f64>,
    /// High prices per candle.
    pub high: Vec<f64>,
    /// Low prices per candle.
    pub low: Vec<f64>,
    /// Closing prices per candle.
    pub close: Vec<f64>,
    /// Volume per candle (Dhan may return as int or float).
    #[serde(deserialize_with = "deserialize_f64_as_i64_vec")]
    pub volume: Vec<i64>,
    /// Timestamps as IST-naive epoch seconds from Dhan V2 REST API.
    /// Dhan may return as int or float.
    #[serde(deserialize_with = "deserialize_f64_as_i64_vec")]
    pub timestamp: Vec<i64>,
    /// Open interest per candle (present when `oi: true` in request).
    /// Dhan may return as int or float.
    #[serde(default, deserialize_with = "deserialize_f64_as_i64_vec_or_default")]
    pub open_interest: Vec<i64>,
}

/// Deserializes a JSON array of numbers (int or float) into `Vec<i64>`.
///
/// Dhan API inconsistently returns some integer fields as floats
/// (e.g., volume `105600.0` instead of `105600`). This handles both.
fn deserialize_f64_as_i64_vec<'de, D>(deserializer: D) -> Result<Vec<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let values: Vec<f64> = serde::Deserialize::deserialize(deserializer)?;
    Ok(values.into_iter().map(|v| v as i64).collect())
}

/// Same as `deserialize_f64_as_i64_vec` but defaults to empty vec if absent.
fn deserialize_f64_as_i64_vec_or_default<'de, D>(deserializer: D) -> Result<Vec<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_f64_as_i64_vec(deserializer)
}

impl DhanIntradayResponse {
    /// Returns the number of candles in this response.
    pub fn len(&self) -> usize {
        self.timestamp.len()
    }

    /// Returns true if the response contains no candles.
    pub fn is_empty(&self) -> bool {
        self.timestamp.is_empty()
    }

    /// Validates that all parallel arrays have the same length.
    pub fn is_consistent(&self) -> bool {
        let n = self.timestamp.len();
        self.open.len() == n
            && self.high.len() == n
            && self.low.len() == n
            && self.close.len() == n
            && self.volume.len() == n
            && (self.open_interest.is_empty() || self.open_interest.len() == n)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- ParsedTick ---

    #[test]
    fn test_parsed_tick_default_all_zeros() {
        let tick = ParsedTick::default();
        assert_eq!(tick.security_id, 0);
        assert_eq!(tick.last_traded_price, 0.0);
        assert_eq!(tick.volume, 0);
        assert_eq!(tick.exchange_timestamp, 0);
        assert_eq!(tick.received_at_nanos, 0);
    }

    #[test]
    fn test_parsed_tick_is_copy() {
        let tick = ParsedTick {
            security_id: 42,
            last_traded_price: 100.5,
            ..Default::default()
        };
        let copy = tick; // Copy, not move
        assert_eq!(tick.security_id, copy.security_id);
        assert_eq!(tick.last_traded_price, copy.last_traded_price);
    }

    // --- MarketDepthLevel ---

    #[test]
    fn test_market_depth_level_default() {
        let level = MarketDepthLevel::default();
        assert_eq!(level.bid_quantity, 0);
        assert_eq!(level.ask_quantity, 0);
        assert_eq!(level.bid_orders, 0);
        assert_eq!(level.ask_orders, 0);
        assert_eq!(level.bid_price, 0.0);
        assert_eq!(level.ask_price, 0.0);
    }

    // --- DeepDepthLevel ---

    #[test]
    fn test_deep_depth_level_default() {
        let level = DeepDepthLevel::default();
        assert_eq!(level.price, 0.0);
        assert_eq!(level.quantity, 0);
        assert_eq!(level.orders, 0);
    }

    #[test]
    fn test_deep_depth_level_is_copy() {
        let level = DeepDepthLevel {
            price: 24500.50,
            quantity: 1000,
            orders: 42,
        };
        let copy = level; // Copy, not move
        assert_eq!(level.price, copy.price);
        assert_eq!(level.quantity, copy.quantity);
        assert_eq!(level.orders, copy.orders);
    }

    #[test]
    fn test_deep_depth_level_f64_precision() {
        let level = DeepDepthLevel {
            price: 24500.123456789,
            quantity: 1,
            orders: 1,
        };
        assert!((level.price - 24500.123456789).abs() < 1e-9);
    }

    // --- DhanIntradayResponse ---

    #[test]
    fn test_intraday_response_empty() {
        let resp = DhanIntradayResponse {
            open: vec![],
            high: vec![],
            low: vec![],
            close: vec![],
            volume: vec![],
            timestamp: vec![],
            open_interest: vec![],
        };
        assert!(resp.is_empty());
        assert_eq!(resp.len(), 0);
        assert!(resp.is_consistent());
    }

    #[test]
    fn test_intraday_response_consistent() {
        let resp = DhanIntradayResponse {
            open: vec![100.0, 101.0],
            high: vec![102.0, 103.0],
            low: vec![99.0, 100.0],
            close: vec![101.0, 102.0],
            volume: vec![1000, 2000],
            timestamp: vec![1700000000, 1700000060],
            open_interest: vec![5000, 6000],
        };
        assert!(!resp.is_empty());
        assert_eq!(resp.len(), 2);
        assert!(resp.is_consistent());
    }

    #[test]
    fn test_intraday_response_inconsistent_lengths() {
        let resp = DhanIntradayResponse {
            open: vec![100.0],
            high: vec![102.0, 103.0],
            low: vec![99.0],
            close: vec![101.0],
            volume: vec![1000],
            timestamp: vec![1700000000],
            open_interest: vec![],
        };
        assert!(!resp.is_consistent());
    }

    #[test]
    fn test_intraday_response_oi_optional() {
        let resp = DhanIntradayResponse {
            open: vec![100.0],
            high: vec![102.0],
            low: vec![99.0],
            close: vec![101.0],
            volume: vec![1000],
            timestamp: vec![1700000000],
            open_interest: vec![], // No OI — still consistent
        };
        assert!(resp.is_consistent());
    }

    // --- HistoricalCandle ---

    #[test]
    fn test_historical_candle_is_copy() {
        let candle = HistoricalCandle {
            timestamp_utc_secs: 1700000000,
            security_id: 42,
            exchange_segment_code: 2,
            open: 100.0,
            high: 102.0,
            low: 99.0,
            close: 101.0,
            volume: 1000,
            open_interest: 5000,
        };
        let copy = candle;
        assert_eq!(candle.security_id, copy.security_id);
        assert_eq!(candle.timestamp_utc_secs, copy.timestamp_utc_secs);
    }
}

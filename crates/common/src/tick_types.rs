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
    /// Security identifier (u64 — holds both Dhan's 4-byte wire id and Groww's
    /// native exchange_token, which sets bit 62 for indices).
    pub security_id: u64,
    /// Binary exchange segment code (0=IDX, 1=NSE_EQ, 2=NSE_FNO, etc.).
    pub exchange_segment_code: u8,
    /// Last traded price in rupees (f32).
    pub last_traded_price: f32,
    /// Last trade quantity (from Quote/Full packets; 0 for Ticker).
    pub last_trade_quantity: u16,
    /// Exchange timestamp as IST epoch seconds from Dhan WebSocket.
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
    /// Implied volatility (annualized decimal, e.g., 0.30 = 30%). f64::NAN for non-F&O.
    pub iv: f64,
    /// Delta (rate of option price change w.r.t. underlying). f64::NAN for non-F&O.
    pub delta: f64,
    /// Gamma (rate of delta change w.r.t. underlying). f64::NAN for non-F&O.
    pub gamma: f64,
    /// Theta (daily time decay). f64::NAN for non-F&O.
    pub theta: f64,
    /// Vega (sensitivity per 1% vol change). f64::NAN for non-F&O.
    pub vega: f64,
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
            iv: f64::NAN,
            delta: f64::NAN,
            gamma: f64::NAN,
            theta: f64::NAN,
            vega: f64::NAN,
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
// Deep Depth Level — input type for the OBI (Order Book Imbalance) indicator
// ---------------------------------------------------------------------------
//
// PR #4 (2026-05-19) retired the depth-20 + depth-200 WebSocket pipelines.
// `DeepDepthLevel` is preserved as the input type for the OBI indicator
// (`crates/trading/src/indicator/obi.rs`), which the 4-IDX_I LOCKED_UNIVERSE
// strategy can still compute from any future depth source (e.g. 5-level
// depth from the main feed Full packet, after a thin adaptor).

/// A single level of market depth with f64 prices + u32 quantity + u32 orders.
/// 16-byte wire layout: price(f64 LE, 8) + quantity(u32 LE, 4) + orders(u32 LE, 4).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DeepDepthLevel {
    /// Price at this level (rupees, f64).
    pub price: f64,
    /// Quantity at this level.
    pub quantity: u32,
    /// Number of orders at this level.
    pub orders: u32,
}

// ---------------------------------------------------------------------------
// Option Greeks — computed from Black-Scholes in trading crate
// ---------------------------------------------------------------------------

/// Computed option Greeks snapshot for a single contract.
///
/// Lives in `common` so storage, core, and api crates can reference it
/// without depending on the trading crate. The trading crate's
/// `greeks::black_scholes::compute_greeks()` produces these values.
///
/// `Copy` for zero-allocation on hot path. All values are f64 for precision.
#[derive(Debug, Clone, Copy)]
pub struct OptionGreeksSnapshot {
    /// Implied volatility (annualized, e.g., 0.30 = 30%).
    pub iv: f64,
    /// Rate of change of option price w.r.t. underlying price.
    /// CE: [0, 1], PE: [-1, 0].
    pub delta: f64,
    /// Rate of change of delta w.r.t. underlying price.
    /// Always positive. Highest for ATM options.
    pub gamma: f64,
    /// Daily time decay (negative for long options).
    pub theta: f64,
    /// Sensitivity to 1% change in IV. Always positive.
    pub vega: f64,
    /// Black-Scholes theoretical price.
    pub bs_price: f64,
    /// Intrinsic value: max(S-K, 0) for CE, max(K-S, 0) for PE.
    pub intrinsic: f64,
    /// Extrinsic (time) value: market_price - intrinsic.
    pub extrinsic: f64,
    /// Put-Call Ratio for the underlying at this snapshot time.
    /// NaN or 0.0 if not computed.
    pub pcr: f64,
}

impl Default for OptionGreeksSnapshot {
    fn default() -> Self {
        Self {
            iv: 0.0,
            delta: 0.0,
            gamma: 0.0,
            theta: 0.0,
            vega: 0.0,
            bs_price: 0.0,
            intrinsic: 0.0,
            extrinsic: 0.0,
            pcr: 0.0,
        }
    }
}

// ---------------------------------------------------------------------------
// GreeksEnricher — trait for inline Greeks computation on the hot path
// ---------------------------------------------------------------------------

/// Trait for enriching ticks with Greeks data on the hot path.
///
/// Implemented in `crates/trading` (InlineGreeksComputer) and injected into
/// `crates/core` tick_processor via generic `<G: GreeksEnricher>` (monomorphized, no vtable).
///
/// # Contract
/// - `enrich()` is called once per valid tick, BEFORE persistence and broadcast.
/// - For index/equity ticks: updates internal underlying LTP cache, no Greeks.
/// - For F&O option ticks: mutates `tick.iv`, `tick.delta`, `tick.gamma`,
///   `tick.theta`, `tick.vega` in-place. Leaves them as NAN if computation fails.
/// - Must be O(1) per tick (HashMap lookups + Jaeckel IV solver).
/// - Must not allocate on the hot path (all maps pre-allocated).
pub trait GreeksEnricher: Send {
    /// Enrich a parsed tick with Greeks. Mutates Greeks fields in place.
    fn enrich(&mut self, tick: &mut ParsedTick);
}

/// No-op implementation of `GreeksEnricher` for when Greeks are disabled.
/// Used as the concrete type parameter when `greeks_enricher = None`.
/// Zero-size type — compiler eliminates all related code completely.
pub struct NoopGreeksEnricher;

impl GreeksEnricher for NoopGreeksEnricher {
    #[inline(always)]
    fn enrich(&mut self, _tick: &mut ParsedTick) {}
}

/// Response from Dhan's intraday charts API.
///
/// Each field is a parallel array — index N across all arrays forms one candle.
/// Dhan V2 REST API returns timestamps as standard UTC epoch seconds.
/// Stored as-is in QuestDB; Grafana converts to IST via Asia/Kolkata timezone.
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
    /// Timestamps as UTC epoch seconds from Dhan V2 REST API.
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

/// Response from Dhan's daily charts API (`/charts/historical`).
///
/// Same columnar parallel array format as `DhanIntradayResponse`.
/// Daily timestamps represent IST midnight as UTC epoch seconds.
#[derive(Debug, serde::Deserialize)]
pub struct DhanDailyResponse {
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
    /// Timestamps as UTC epoch seconds from Dhan V2 REST API.
    #[serde(deserialize_with = "deserialize_f64_as_i64_vec")]
    pub timestamp: Vec<i64>,
    /// Open interest per candle (present when `oi: true` in request).
    #[serde(default, deserialize_with = "deserialize_f64_as_i64_vec_or_default")]
    pub open_interest: Vec<i64>,
}

impl DhanDailyResponse {
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

    #[test]
    fn test_daily_response_consistent() {
        let resp = DhanDailyResponse {
            open: vec![100.0, 101.0],
            high: vec![102.0, 103.0],
            low: vec![99.0, 100.0],
            close: vec![101.0, 102.0],
            volume: vec![100_000, 200_000],
            timestamp: vec![1700000000, 1700086400],
            open_interest: vec![],
        };
        assert!(!resp.is_empty());
        assert_eq!(resp.len(), 2);
        assert!(resp.is_consistent());
    }

    // -----------------------------------------------------------------------
    // DhanIntradayResponse deserialization (JSON → struct)
    // -----------------------------------------------------------------------

    #[test]
    fn test_intraday_response_full_json_deserialize() {
        let json = r#"{
            "open": [100.0, 101.0],
            "high": [102.0, 103.0],
            "low": [99.0, 100.0],
            "close": [101.0, 102.0],
            "volume": [1000.0, 2000.0],
            "timestamp": [1700000000.0, 1700000060.0],
            "open_interest": [5000.0, 6000.0]
        }"#;
        let resp: DhanIntradayResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.len(), 2);
        assert!(resp.is_consistent());
        assert_eq!(resp.volume, vec![1000, 2000]);
        assert_eq!(resp.timestamp, vec![1700000000, 1700000060]);
        assert_eq!(resp.open_interest, vec![5000, 6000]);
    }

    #[test]
    fn test_intraday_response_volume_as_float_truncated_to_i64() {
        // Dhan sometimes returns volume as float with .0
        let json = r#"{
            "open": [100.0],
            "high": [102.0],
            "low": [99.0],
            "close": [101.0],
            "volume": [1234.0],
            "timestamp": [1700000000.0]
        }"#;
        let resp: DhanIntradayResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.volume[0], 1234);
    }

    #[test]
    fn test_intraday_response_missing_oi_defaults_empty() {
        let json = r#"{
            "open": [100.0],
            "high": [102.0],
            "low": [99.0],
            "close": [101.0],
            "volume": [1000.0],
            "timestamp": [1700000000.0]
        }"#;
        let resp: DhanIntradayResponse = serde_json::from_str(json).unwrap();
        assert!(resp.open_interest.is_empty());
    }

    #[test]
    fn test_intraday_response_empty_arrays() {
        let json = r#"{
            "open": [],
            "high": [],
            "low": [],
            "close": [],
            "volume": [],
            "timestamp": []
        }"#;
        let resp: DhanIntradayResponse = serde_json::from_str(json).unwrap();
        assert!(resp.is_empty());
        assert!(resp.is_consistent());
    }

    // -----------------------------------------------------------------------
    // MarketDepthLevel edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_market_depth_level_equality() {
        let a = MarketDepthLevel {
            bid_quantity: 100,
            ask_quantity: 200,
            bid_orders: 5,
            ask_orders: 10,
            bid_price: 245.5,
            ask_price: 246.0,
        };
        let b = a;
        assert_eq!(a, b);
    }

    // -----------------------------------------------------------------------
    // ParsedTick field access
    // -----------------------------------------------------------------------

    #[test]
    fn test_parsed_tick_all_fields_settable() {
        let tick = ParsedTick {
            security_id: 52432,
            exchange_segment_code: 2,
            last_traded_price: 245.5,
            last_trade_quantity: 75,
            exchange_timestamp: 1740556500,
            received_at_nanos: 1_740_556_500_123_456_789,
            average_traded_price: 244.0,
            volume: 50000,
            total_sell_quantity: 25000,
            total_buy_quantity: 25000,
            day_open: 242.0,
            day_close: 240.0,
            day_high: 248.0,
            day_low: 238.0,
            open_interest: 120000,
            oi_day_high: 130000,
            oi_day_low: 110000,
            iv: f64::NAN,
            delta: f64::NAN,
            gamma: f64::NAN,
            theta: f64::NAN,
            vega: f64::NAN,
        };
        assert_eq!(tick.security_id, 52432);
        assert_eq!(tick.exchange_segment_code, 2);
        assert!((tick.last_traded_price - 245.5).abs() < f32::EPSILON);
    }

    // -----------------------------------------------------------------------
    // Coverage: DhanDailyResponse::is_consistent — non-empty OI mismatch
    // (line 276: open_interest.len() == n branch)
    // -----------------------------------------------------------------------

    #[test]
    fn test_daily_response_inconsistent_oi_length() {
        let resp = DhanDailyResponse {
            open: vec![100.0, 101.0],
            high: vec![102.0, 103.0],
            low: vec![99.0, 100.0],
            close: vec![101.0, 102.0],
            volume: vec![1000, 2000],
            timestamp: vec![1700000000, 1700086400],
            open_interest: vec![5000], // 1 element vs 2 candles — inconsistent
        };
        assert!(!resp.is_consistent());
    }

    #[test]
    fn test_daily_response_consistent_with_oi() {
        let resp = DhanDailyResponse {
            open: vec![100.0],
            high: vec![102.0],
            low: vec![99.0],
            close: vec![101.0],
            volume: vec![1000],
            timestamp: vec![1700000000],
            open_interest: vec![5000], // Same length as other arrays
        };
        assert!(resp.is_consistent());
    }

    // -----------------------------------------------------------------------
    // Coverage: deserialize_f64_as_i64_vec error path (line 197)
    // -----------------------------------------------------------------------

    #[test]
    fn test_intraday_response_invalid_volume_type_fails() {
        // volume array contains a string instead of numbers — exercises the
        // deserialization error path of deserialize_f64_as_i64_vec.
        let json = r#"{
            "open": [100.0],
            "high": [102.0],
            "low": [99.0],
            "close": [101.0],
            "volume": ["not_a_number"],
            "timestamp": [1700000000.0]
        }"#;
        let result: Result<DhanIntradayResponse, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Coverage: DhanIntradayResponse::is_consistent — non-empty OI mismatch
    // (line 228: open_interest.len() == n when OI present but wrong length)
    // -----------------------------------------------------------------------

    #[test]
    fn test_intraday_response_inconsistent_oi_length() {
        let resp = DhanIntradayResponse {
            open: vec![100.0, 101.0],
            high: vec![102.0, 103.0],
            low: vec![99.0, 100.0],
            close: vec![101.0, 102.0],
            volume: vec![1000, 2000],
            timestamp: vec![1700000000, 1700000060],
            open_interest: vec![5000], // 1 element vs 2 candles
        };
        assert!(!resp.is_consistent());
    }

    // --- OptionGreeksSnapshot ---

    #[test]
    fn test_option_greeks_snapshot_default_all_zeros() {
        let g = OptionGreeksSnapshot::default();
        assert_eq!(g.iv, 0.0);
        assert_eq!(g.delta, 0.0);
        assert_eq!(g.gamma, 0.0);
        assert_eq!(g.theta, 0.0);
        assert_eq!(g.vega, 0.0);
        assert_eq!(g.bs_price, 0.0);
        assert_eq!(g.intrinsic, 0.0);
        assert_eq!(g.extrinsic, 0.0);
        assert_eq!(g.pcr, 0.0);
    }

    #[test]
    fn test_option_greeks_snapshot_is_copy() {
        let g = OptionGreeksSnapshot {
            iv: 0.25,
            delta: 0.55,
            gamma: 0.0013,
            theta: -15.0,
            vega: 12.5,
            bs_price: 350.0,
            intrinsic: 200.0,
            extrinsic: 150.0,
            pcr: 1.2,
        };
        let copy = g; // Copy, not move
        assert_eq!(g.iv, copy.iv);
        assert_eq!(g.delta, copy.delta);
        assert_eq!(g.pcr, copy.pcr);
    }

    // --- DhanDailyResponse deserialization from JSON ---

    #[test]
    fn test_daily_response_full_json_deserialize() {
        let json = r#"{
            "open": [100.0, 101.0],
            "high": [102.0, 103.0],
            "low": [99.0, 100.0],
            "close": [101.0, 102.0],
            "volume": [100000.0, 200000.0],
            "timestamp": [1700000000.0, 1700086400.0],
            "open_interest": [5000.0, 6000.0]
        }"#;
        let resp: DhanDailyResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.len(), 2);
        assert!(resp.is_consistent());
        assert_eq!(resp.volume, vec![100000, 200000]);
        assert_eq!(resp.timestamp, vec![1700000000, 1700086400]);
        assert_eq!(resp.open_interest, vec![5000, 6000]);
    }

    #[test]
    fn test_daily_response_empty_json_deserialize() {
        let json = r#"{
            "open": [],
            "high": [],
            "low": [],
            "close": [],
            "volume": [],
            "timestamp": []
        }"#;
        let resp: DhanDailyResponse = serde_json::from_str(json).unwrap();
        assert!(resp.is_empty());
        assert_eq!(resp.len(), 0);
        assert!(resp.is_consistent());
    }

    #[test]
    fn test_daily_response_missing_oi_defaults_empty() {
        let json = r#"{
            "open": [100.0],
            "high": [102.0],
            "low": [99.0],
            "close": [101.0],
            "volume": [1000.0],
            "timestamp": [1700000000.0]
        }"#;
        let resp: DhanDailyResponse = serde_json::from_str(json).unwrap();
        assert!(resp.open_interest.is_empty());
        assert!(resp.is_consistent());
    }

    #[test]
    fn test_daily_response_inconsistent_open_length() {
        let resp = DhanDailyResponse {
            open: vec![100.0], // 1 vs 2
            high: vec![102.0, 103.0],
            low: vec![99.0, 100.0],
            close: vec![101.0, 102.0],
            volume: vec![1000, 2000],
            timestamp: vec![1700000000, 1700086400],
            open_interest: vec![],
        };
        assert!(!resp.is_consistent());
    }

    #[test]
    fn test_daily_response_inconsistent_close_length() {
        let resp = DhanDailyResponse {
            open: vec![100.0, 101.0],
            high: vec![102.0, 103.0],
            low: vec![99.0, 100.0],
            close: vec![101.0], // 1 vs 2
            volume: vec![1000, 2000],
            timestamp: vec![1700000000, 1700086400],
            open_interest: vec![],
        };
        assert!(!resp.is_consistent());
    }

    #[test]
    fn test_daily_response_inconsistent_volume_length() {
        let resp = DhanDailyResponse {
            open: vec![100.0, 101.0],
            high: vec![102.0, 103.0],
            low: vec![99.0, 100.0],
            close: vec![101.0, 102.0],
            volume: vec![1000], // 1 vs 2
            timestamp: vec![1700000000, 1700086400],
            open_interest: vec![],
        };
        assert!(!resp.is_consistent());
    }

    // --- DhanIntradayResponse more edge cases ---

    #[test]
    fn test_intraday_response_inconsistent_close_length() {
        let resp = DhanIntradayResponse {
            open: vec![100.0, 101.0],
            high: vec![102.0, 103.0],
            low: vec![99.0, 100.0],
            close: vec![101.0], // 1 vs 2
            volume: vec![1000, 2000],
            timestamp: vec![1700000000, 1700000060],
            open_interest: vec![],
        };
        assert!(!resp.is_consistent());
    }

    #[test]
    fn test_intraday_response_inconsistent_volume_length() {
        let resp = DhanIntradayResponse {
            open: vec![100.0, 101.0],
            high: vec![102.0, 103.0],
            low: vec![99.0, 100.0],
            close: vec![101.0, 102.0],
            volume: vec![1000], // 1 vs 2
            timestamp: vec![1700000000, 1700000060],
            open_interest: vec![],
        };
        assert!(!resp.is_consistent());
    }

    #[test]
    fn test_intraday_response_inconsistent_low_length() {
        let resp = DhanIntradayResponse {
            open: vec![100.0, 101.0],
            high: vec![102.0, 103.0],
            low: vec![99.0], // 1 vs 2
            close: vec![101.0, 102.0],
            volume: vec![1000, 2000],
            timestamp: vec![1700000000, 1700000060],
            open_interest: vec![],
        };
        assert!(!resp.is_consistent());
    }

    // --- DhanDailyResponse inconsistent low/high lengths ---

    #[test]
    fn test_daily_response_inconsistent_low_length() {
        let resp = DhanDailyResponse {
            open: vec![100.0, 101.0],
            high: vec![102.0, 103.0],
            low: vec![99.0], // 1 vs 2
            close: vec![101.0, 102.0],
            volume: vec![1000, 2000],
            timestamp: vec![1700000000, 1700086400],
            open_interest: vec![],
        };
        assert!(!resp.is_consistent());
    }

    #[test]
    fn test_daily_response_inconsistent_high_length() {
        let resp = DhanDailyResponse {
            open: vec![100.0, 101.0],
            high: vec![102.0], // 1 vs 2
            low: vec![99.0, 100.0],
            close: vec![101.0, 102.0],
            volume: vec![1000, 2000],
            timestamp: vec![1700000000, 1700086400],
            open_interest: vec![],
        };
        assert!(!resp.is_consistent());
    }

    // --- ParsedTick Debug impl ---

    #[test]
    fn test_parsed_tick_debug_output() {
        let tick = ParsedTick {
            security_id: 42,
            last_traded_price: 100.5,
            ..Default::default()
        };
        let debug = format!("{:?}", tick);
        assert!(debug.contains("ParsedTick"));
        assert!(debug.contains("42"));
    }

    // --- MarketDepthLevel Debug impl ---

    #[test]
    fn test_market_depth_level_debug_output() {
        let level = MarketDepthLevel {
            bid_quantity: 100,
            ask_quantity: 200,
            bid_orders: 5,
            ask_orders: 10,
            bid_price: 245.5,
            ask_price: 246.0,
        };
        let debug = format!("{:?}", level);
        assert!(debug.contains("MarketDepthLevel"));
    }

    // --- OptionGreeksSnapshot Debug impl ---

    #[test]
    fn test_option_greeks_snapshot_debug_output() {
        let g = OptionGreeksSnapshot::default();
        let debug = format!("{:?}", g);
        assert!(debug.contains("OptionGreeksSnapshot"));
    }

    #[test]
    fn test_option_greeks_snapshot_typical_atm_call() {
        let g = OptionGreeksSnapshot {
            iv: 0.20,
            delta: 0.53,
            gamma: 0.00132,
            theta: -15.15,
            vega: 12.18,
            bs_price: 340.0,
            intrinsic: 100.0,
            extrinsic: 240.0,
            pcr: 0.85,
        };
        // ATM call: delta near 0.5
        assert!(g.delta > 0.4 && g.delta < 0.6);
        // Theta always negative for long options
        assert!(g.theta < 0.0);
        // Vega always positive
        assert!(g.vega > 0.0);
    }

    #[test]
    fn test_noop_greeks_enricher_is_zero_side_effect() {
        // Covers `impl GreeksEnricher for NoopGreeksEnricher::enrich`
        // (tick_types.rs:217-220). The no-op enricher must not mutate
        // any stable field of the ParsedTick it is given.
        let mut enricher = NoopGreeksEnricher;
        let mut tick = ParsedTick {
            security_id: 1333,
            last_traded_price: 100.5,
            exchange_timestamp: 1_700_000_000,
            volume: 42,
            ..ParsedTick::default()
        };
        enricher.enrich(&mut tick);
        assert_eq!(tick.security_id, 1333);
        assert!((tick.last_traded_price - 100.5).abs() < f32::EPSILON);
        assert_eq!(tick.exchange_timestamp, 1_700_000_000);
        assert_eq!(tick.volume, 42);
        // IV/delta/etc. stay NaN — no enrichment happened.
        assert!(tick.iv.is_nan());
        assert!(tick.delta.is_nan());
    }
}

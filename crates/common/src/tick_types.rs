//! Tick, candle, and chart interval types shared across the pipeline.
//!
//! These types flow from the binary parser through the candle aggregator
//! to the API server and frontend. They must be Copy where possible
//! for zero-allocation on the hot path.

use std::fmt;

use serde::{Deserialize, Serialize};

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
    /// Exchange timestamp as epoch seconds (UTC).
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

/// A single level of market depth from the Full packet.
///
/// Dhan sends 5 levels. This struct is `Copy` + `Default` for stack allocation.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MarketDepthLevel {
    /// Bid quantity at this level.
    pub bid_quantity: i32,
    /// Ask quantity at this level.
    pub ask_quantity: i32,
    /// Number of bid orders at this level.
    pub bid_orders: i16,
    /// Number of ask orders at this level.
    pub ask_orders: i16,
    /// Bid price at this level (rupees).
    pub bid_price: f32,
    /// Ask price at this level (rupees).
    pub ask_price: f32,
}

// ---------------------------------------------------------------------------
// Candle — OHLCV output of candle aggregator
// ---------------------------------------------------------------------------

/// A finalized OHLCV candle for a specific security and timeframe.
///
/// Volume is delta (not cumulative) — computed as `end_cumulative - start_cumulative`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Candle {
    /// Candle open time as Unix epoch seconds (UTC).
    pub timestamp: i64,
    /// Security identifier.
    pub security_id: u32,
    /// Open price.
    pub open: f64,
    /// High price.
    pub high: f64,
    /// Low price.
    pub low: f64,
    /// Close price.
    pub close: f64,
    /// Volume delta for this candle period.
    pub volume: u32,
    /// Open interest at candle close.
    pub open_interest: u32,
    /// Number of ticks received during this candle.
    pub tick_count: u32,
}

// ---------------------------------------------------------------------------
// Timeframe — time-based candle intervals
// ---------------------------------------------------------------------------

/// Time-based candle interval.
///
/// Covers all standard TradingView intervals plus custom.
/// Each variant maps to a fixed number of seconds for IST boundary alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Timeframe {
    // Seconds (6 standard)
    /// 1-second candles.
    S1,
    /// 5-second candles.
    S5,
    /// 10-second candles.
    S10,
    /// 15-second candles.
    S15,
    /// 30-second candles.
    S30,
    /// 45-second candles.
    S45,

    // Minutes (10 standard)
    /// 1-minute candles.
    M1,
    /// 2-minute candles.
    M2,
    /// 3-minute candles.
    M3,
    /// 4-minute candles.
    M4,
    /// 5-minute candles.
    M5,
    /// 10-minute candles.
    M10,
    /// 15-minute candles.
    M15,
    /// 25-minute candles.
    M25,
    /// 30-minute candles.
    M30,
    /// 45-minute candles.
    M45,

    // Hours (4 standard)
    /// 1-hour candles.
    H1,
    /// 2-hour candles.
    H2,
    /// 3-hour candles.
    H3,
    /// 4-hour candles.
    H4,

    // Day+ (7 standard)
    /// Daily candles.
    D1,
    /// 5-day candles.
    D5,
    /// Weekly candles.
    W1,
    /// Monthly candles.
    Mo1,
    /// 3-month (quarterly) candles.
    Mo3,
    /// 6-month candles.
    Mo6,
    /// Yearly candles.
    Y1,
}

/// Number of standard time-based timeframes.
pub const STANDARD_TIMEFRAME_COUNT: usize = 27;

impl Timeframe {
    /// Returns the interval duration in seconds.
    ///
    /// For Day+ intervals, uses approximate seconds (trading use only).
    pub fn as_seconds(&self) -> i64 {
        match self {
            Self::S1 => 1,
            Self::S5 => 5,
            Self::S10 => 10,
            Self::S15 => 15,
            Self::S30 => 30,
            Self::S45 => 45,
            Self::M1 => 60,
            Self::M2 => 120,
            Self::M3 => 180,
            Self::M4 => 240,
            Self::M5 => 300,
            Self::M10 => 600,
            Self::M15 => 900,
            Self::M25 => 1500,
            Self::M30 => 1800,
            Self::M45 => 2700,
            Self::H1 => 3600,
            Self::H2 => 7200,
            Self::H3 => 10800,
            Self::H4 => 14400,
            Self::D1 => 86400,
            Self::D5 => 432_000,
            Self::W1 => 604_800,
            Self::Mo1 => 2_592_000,  // 30 days
            Self::Mo3 => 7_776_000,  // 90 days
            Self::Mo6 => 15_552_000, // 180 days
            Self::Y1 => 31_536_000,  // 365 days
        }
    }

    /// Returns the canonical string representation (matches TradingView labels).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::S1 => "1s",
            Self::S5 => "5s",
            Self::S10 => "10s",
            Self::S15 => "15s",
            Self::S30 => "30s",
            Self::S45 => "45s",
            Self::M1 => "1m",
            Self::M2 => "2m",
            Self::M3 => "3m",
            Self::M4 => "4m",
            Self::M5 => "5m",
            Self::M10 => "10m",
            Self::M15 => "15m",
            Self::M25 => "25m",
            Self::M30 => "30m",
            Self::M45 => "45m",
            Self::H1 => "1h",
            Self::H2 => "2h",
            Self::H3 => "3h",
            Self::H4 => "4h",
            Self::D1 => "1D",
            Self::D5 => "5D",
            Self::W1 => "1W",
            Self::Mo1 => "1M",
            Self::Mo3 => "3M",
            Self::Mo6 => "6M",
            Self::Y1 => "1Y",
        }
    }

    /// Parses a string label into a Timeframe.
    ///
    /// Returns `None` for unrecognized strings.
    pub fn from_label(s: &str) -> Option<Self> {
        match s {
            "1s" => Some(Self::S1),
            "5s" => Some(Self::S5),
            "10s" => Some(Self::S10),
            "15s" => Some(Self::S15),
            "30s" => Some(Self::S30),
            "45s" => Some(Self::S45),
            "1m" => Some(Self::M1),
            "2m" => Some(Self::M2),
            "3m" => Some(Self::M3),
            "4m" => Some(Self::M4),
            "5m" => Some(Self::M5),
            "10m" => Some(Self::M10),
            "15m" => Some(Self::M15),
            "25m" => Some(Self::M25),
            "30m" => Some(Self::M30),
            "45m" => Some(Self::M45),
            "1h" => Some(Self::H1),
            "2h" => Some(Self::H2),
            "3h" => Some(Self::H3),
            "4h" => Some(Self::H4),
            "1D" => Some(Self::D1),
            "5D" => Some(Self::D5),
            "1W" => Some(Self::W1),
            "1M" => Some(Self::Mo1),
            "3M" => Some(Self::Mo3),
            "6M" => Some(Self::Mo6),
            "1Y" => Some(Self::Y1),
            _ => None,
        }
    }

    /// Returns all 27 standard timeframes.
    pub fn all_standard() -> &'static [Timeframe] {
        &[
            Self::S1,
            Self::S5,
            Self::S10,
            Self::S15,
            Self::S30,
            Self::S45,
            Self::M1,
            Self::M2,
            Self::M3,
            Self::M4,
            Self::M5,
            Self::M10,
            Self::M15,
            Self::M25,
            Self::M30,
            Self::M45,
            Self::H1,
            Self::H2,
            Self::H3,
            Self::H4,
            Self::D1,
            Self::D5,
            Self::W1,
            Self::Mo1,
            Self::Mo3,
            Self::Mo6,
            Self::Y1,
        ]
    }
}

impl fmt::Display for Timeframe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// TickInterval — tick-count-based candle intervals
// ---------------------------------------------------------------------------

/// Tick-count-based candle interval.
///
/// Candle closes after receiving N ticks for a given security_id.
/// Matches TradingView's tick-based chart intervals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TickInterval {
    /// 1-tick candles (every tick is its own candle).
    T1,
    /// 10-tick candles.
    T10,
    /// 100-tick candles.
    T100,
    /// 1000-tick candles.
    T1000,
    /// Custom tick count.
    Custom(u32),
}

impl TickInterval {
    /// Returns the number of ticks per candle.
    pub fn tick_count(&self) -> u32 {
        match self {
            Self::T1 => 1,
            Self::T10 => 10,
            Self::T100 => 100,
            Self::T1000 => 1000,
            Self::Custom(n) => *n,
        }
    }

    /// Returns the canonical string label.
    pub fn as_str(&self) -> String {
        match self {
            Self::T1 => "1T".to_string(),
            Self::T10 => "10T".to_string(),
            Self::T100 => "100T".to_string(),
            Self::T1000 => "1000T".to_string(),
            Self::Custom(n) => format!("{n}T"),
        }
    }

    /// Parses a string label into a TickInterval.
    ///
    /// Format: `<number>T` (e.g., "10T", "500T").
    /// Returns `None` for invalid input.
    pub fn from_label(s: &str) -> Option<Self> {
        let s = s.trim();
        if !s.ends_with('T') && !s.ends_with('t') {
            return None;
        }
        let num_str = &s[..s.len() - 1];
        let n: u32 = num_str.parse().ok()?;
        if n == 0 {
            return None;
        }
        Some(match n {
            1 => Self::T1,
            10 => Self::T10,
            100 => Self::T100,
            1000 => Self::T1000,
            _ => Self::Custom(n),
        })
    }

    /// Returns the 4 standard tick intervals.
    pub fn all_standard() -> &'static [TickInterval] {
        &[Self::T1, Self::T10, Self::T100, Self::T1000]
    }
}

impl fmt::Display for TickInterval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ---------------------------------------------------------------------------
// ChartInterval — unified interval type (time or tick)
// ---------------------------------------------------------------------------

/// Unified chart interval encompassing both time-based and tick-based intervals.
///
/// The API and frontend send/receive this type to specify chart resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChartInterval {
    /// Time-based interval (candle closes on wall-clock boundary).
    Time(Timeframe),
    /// Tick-count-based interval (candle closes after N ticks).
    Tick(TickInterval),
    /// Custom time interval specified in seconds.
    CustomTime(i64),
}

impl ChartInterval {
    /// Parses a string label into a ChartInterval.
    ///
    /// Tries tick format first (`<N>T`), then standard timeframes,
    /// then custom time format (`<N>s`, `<N>m`, `<N>h`).
    pub fn from_label(s: &str) -> Option<Self> {
        // Try tick-based first
        if let Some(tick) = TickInterval::from_label(s) {
            return Some(Self::Tick(tick));
        }

        // Try standard timeframe
        if let Some(tf) = Timeframe::from_label(s) {
            return Some(Self::Time(tf));
        }

        // Try custom time (e.g., "7m", "90s", "6h")
        let s = s.trim();
        if s.len() < 2 {
            return None;
        }
        let (num_str, unit) = s.split_at(s.len() - 1);
        let n: i64 = num_str.parse().ok()?;
        if n <= 0 {
            return None;
        }
        let seconds = match unit {
            "s" => n,
            "m" => n * 60,
            "h" => n * 3600,
            _ => return None,
        };
        Some(Self::CustomTime(seconds))
    }

    /// Returns the string label for this interval.
    pub fn as_str(&self) -> String {
        match self {
            Self::Time(tf) => tf.as_str().to_string(),
            Self::Tick(ti) => ti.as_str(),
            Self::CustomTime(secs) => {
                if *secs % 3600 == 0 {
                    format!("{}h", secs / 3600)
                } else if *secs % 60 == 0 {
                    format!("{}m", secs / 60)
                } else {
                    format!("{secs}s")
                }
            }
        }
    }
}

impl fmt::Display for ChartInterval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ---------------------------------------------------------------------------
// CandleBroadcastMessage — sent over broadcast channel
// ---------------------------------------------------------------------------

/// Message sent via `tokio::sync::broadcast` when a candle is finalized.
///
/// Includes the interval label so subscribers can filter by timeframe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandleBroadcastMessage {
    /// The interval that produced this candle (e.g., "5m", "100T").
    pub interval: String,
    /// The finalized candle data.
    pub candle: Candle,
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
        assert_eq!(tick.open_interest, 0);
    }

    #[test]
    fn test_parsed_tick_is_copy() {
        let tick = ParsedTick {
            security_id: 13,
            exchange_segment_code: 2,
            last_traded_price: 24500.50,
            ..Default::default()
        };
        let tick2 = tick; // Copy, not move
        assert_eq!(tick.security_id, tick2.security_id);
        assert_eq!(tick.last_traded_price, tick2.last_traded_price);
    }

    // --- MarketDepthLevel ---

    #[test]
    fn test_market_depth_level_default() {
        let level = MarketDepthLevel::default();
        assert_eq!(level.bid_quantity, 0);
        assert_eq!(level.ask_quantity, 0);
        assert_eq!(level.bid_price, 0.0);
        assert_eq!(level.ask_price, 0.0);
    }

    // --- Candle ---

    #[test]
    fn test_candle_serde_roundtrip() {
        let candle = Candle {
            timestamp: 1740556500,
            security_id: 13,
            open: 24500.0,
            high: 24510.0,
            low: 24490.0,
            close: 24505.0,
            volume: 12345,
            open_interest: 0,
            tick_count: 50,
        };
        let json = serde_json::to_string(&candle).unwrap();
        let deserialized: Candle = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.timestamp, candle.timestamp);
        assert_eq!(deserialized.open, candle.open);
        assert_eq!(deserialized.volume, candle.volume);
    }

    // --- Timeframe ---

    #[test]
    fn test_timeframe_all_standard_count() {
        assert_eq!(Timeframe::all_standard().len(), STANDARD_TIMEFRAME_COUNT);
    }

    #[test]
    fn test_timeframe_as_seconds_seconds_group() {
        assert_eq!(Timeframe::S1.as_seconds(), 1);
        assert_eq!(Timeframe::S5.as_seconds(), 5);
        assert_eq!(Timeframe::S10.as_seconds(), 10);
        assert_eq!(Timeframe::S15.as_seconds(), 15);
        assert_eq!(Timeframe::S30.as_seconds(), 30);
        assert_eq!(Timeframe::S45.as_seconds(), 45);
    }

    #[test]
    fn test_timeframe_as_seconds_minutes_group() {
        assert_eq!(Timeframe::M1.as_seconds(), 60);
        assert_eq!(Timeframe::M2.as_seconds(), 120);
        assert_eq!(Timeframe::M3.as_seconds(), 180);
        assert_eq!(Timeframe::M4.as_seconds(), 240);
        assert_eq!(Timeframe::M5.as_seconds(), 300);
        assert_eq!(Timeframe::M10.as_seconds(), 600);
        assert_eq!(Timeframe::M15.as_seconds(), 900);
        assert_eq!(Timeframe::M25.as_seconds(), 1500);
        assert_eq!(Timeframe::M30.as_seconds(), 1800);
        assert_eq!(Timeframe::M45.as_seconds(), 2700);
    }

    #[test]
    fn test_timeframe_as_seconds_hours_group() {
        assert_eq!(Timeframe::H1.as_seconds(), 3600);
        assert_eq!(Timeframe::H2.as_seconds(), 7200);
        assert_eq!(Timeframe::H3.as_seconds(), 10800);
        assert_eq!(Timeframe::H4.as_seconds(), 14400);
    }

    #[test]
    fn test_timeframe_as_str_roundtrip_all() {
        for tf in Timeframe::all_standard() {
            let label = tf.as_str();
            let parsed = Timeframe::from_label(label);
            assert_eq!(parsed, Some(*tf), "roundtrip failed for {label}");
        }
    }

    #[test]
    fn test_timeframe_from_label_unknown_returns_none() {
        assert_eq!(Timeframe::from_label("7m"), None);
        assert_eq!(Timeframe::from_label("99s"), None);
        assert_eq!(Timeframe::from_label(""), None);
        assert_eq!(Timeframe::from_label("abc"), None);
    }

    #[test]
    fn test_timeframe_display() {
        assert_eq!(format!("{}", Timeframe::M5), "5m");
        assert_eq!(format!("{}", Timeframe::H1), "1h");
        assert_eq!(format!("{}", Timeframe::D1), "1D");
    }

    #[test]
    fn test_timeframe_serde_roundtrip() {
        let tf = Timeframe::M15;
        let json = serde_json::to_string(&tf).unwrap();
        let deserialized: Timeframe = serde_json::from_str(&json).unwrap();
        assert_eq!(tf, deserialized);
    }

    // --- TickInterval ---

    #[test]
    fn test_tick_interval_tick_count() {
        assert_eq!(TickInterval::T1.tick_count(), 1);
        assert_eq!(TickInterval::T10.tick_count(), 10);
        assert_eq!(TickInterval::T100.tick_count(), 100);
        assert_eq!(TickInterval::T1000.tick_count(), 1000);
        assert_eq!(TickInterval::Custom(500).tick_count(), 500);
    }

    #[test]
    fn test_tick_interval_as_str_roundtrip() {
        for ti in TickInterval::all_standard() {
            let label = ti.as_str();
            let parsed = TickInterval::from_label(&label);
            assert_eq!(parsed, Some(*ti), "roundtrip failed for {label}");
        }
    }

    #[test]
    fn test_tick_interval_custom_roundtrip() {
        let ti = TickInterval::Custom(50);
        let label = ti.as_str();
        assert_eq!(label, "50T");
        let parsed = TickInterval::from_label(&label).unwrap();
        assert_eq!(parsed, TickInterval::Custom(50));
    }

    #[test]
    fn test_tick_interval_from_label_invalid() {
        assert_eq!(TickInterval::from_label("0T"), None); // zero not allowed
        assert_eq!(TickInterval::from_label("T"), None); // no number
        assert_eq!(TickInterval::from_label("abc"), None); // no T suffix
        assert_eq!(TickInterval::from_label(""), None);
        assert_eq!(TickInterval::from_label("-5T"), None); // negative
    }

    #[test]
    fn test_tick_interval_case_insensitive_suffix() {
        assert_eq!(TickInterval::from_label("10t"), Some(TickInterval::T10));
        assert_eq!(TickInterval::from_label("100T"), Some(TickInterval::T100));
    }

    #[test]
    fn test_tick_interval_display() {
        assert_eq!(format!("{}", TickInterval::T100), "100T");
        assert_eq!(format!("{}", TickInterval::Custom(42)), "42T");
    }

    // --- ChartInterval ---

    #[test]
    fn test_chart_interval_from_label_tick() {
        let ci = ChartInterval::from_label("100T").unwrap();
        assert_eq!(ci, ChartInterval::Tick(TickInterval::T100));
    }

    #[test]
    fn test_chart_interval_from_label_standard_time() {
        let ci = ChartInterval::from_label("5m").unwrap();
        assert_eq!(ci, ChartInterval::Time(Timeframe::M5));
    }

    #[test]
    fn test_chart_interval_from_label_custom_time_minutes() {
        let ci = ChartInterval::from_label("7m").unwrap();
        assert_eq!(ci, ChartInterval::CustomTime(420));
    }

    #[test]
    fn test_chart_interval_from_label_custom_time_seconds() {
        let ci = ChartInterval::from_label("90s").unwrap();
        assert_eq!(ci, ChartInterval::CustomTime(90));
    }

    #[test]
    fn test_chart_interval_from_label_custom_time_hours() {
        let ci = ChartInterval::from_label("6h").unwrap();
        assert_eq!(ci, ChartInterval::CustomTime(21600));
    }

    #[test]
    fn test_chart_interval_from_label_invalid() {
        assert_eq!(ChartInterval::from_label(""), None);
        assert_eq!(ChartInterval::from_label("x"), None);
        assert_eq!(ChartInterval::from_label("0m"), None); // zero not allowed
    }

    #[test]
    fn test_chart_interval_as_str() {
        assert_eq!(ChartInterval::Time(Timeframe::M5).as_str(), "5m");
        assert_eq!(ChartInterval::Tick(TickInterval::T100).as_str(), "100T");
        assert_eq!(ChartInterval::CustomTime(420).as_str(), "7m");
        assert_eq!(ChartInterval::CustomTime(90).as_str(), "90s");
        assert_eq!(ChartInterval::CustomTime(7200).as_str(), "2h");
    }

    #[test]
    fn test_chart_interval_display() {
        assert_eq!(format!("{}", ChartInterval::Time(Timeframe::H1)), "1h");
    }

    // --- CandleBroadcastMessage ---

    #[test]
    fn test_candle_broadcast_message_serde() {
        let msg = CandleBroadcastMessage {
            interval: "5m".to_string(),
            candle: Candle {
                timestamp: 1740556500,
                security_id: 13,
                open: 24500.0,
                high: 24510.0,
                low: 24490.0,
                close: 24505.0,
                volume: 12345,
                open_interest: 0,
                tick_count: 50,
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: CandleBroadcastMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.interval, "5m");
        assert_eq!(deserialized.candle.security_id, 13);
    }
}

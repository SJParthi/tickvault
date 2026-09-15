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
    /// Cumulative day volume. Interpret only when `volume_present` is true.
    pub volume: u32,
    /// Whether this observation contains the cumulative-volume field.
    /// A Quote/Full carrying zero is present; a Ticker has no such field.
    /// Programmatic construction defaults to present for compatibility;
    /// every wire decoder must set this field explicitly.
    pub volume_present: bool,
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
            volume_present: true,
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

/// One source observation, preserving absence separately from a real zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeObservation {
    Missing,
    Cumulative(u64),
}

impl ParsedTick {
    /// An explicit wider counter is itself a present observation.
    #[inline]
    #[must_use]
    pub fn volume_observation(&self, wider_counter: Option<u64>) -> VolumeObservation {
        match wider_counter {
            Some(value) => VolumeObservation::Cumulative(value),
            None if self.volume_present => VolumeObservation::Cumulative(u64::from(self.volume)),
            None => VolumeObservation::Missing,
        }
    }
}

/// Quality of a quantity attributed under the configured candle clock.
/// Eligibility establishes the local calculation contract, not that a retail
/// source delivered every exchange trade or supplied aggressor-side labels.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VolumeQuality {
    pub baseline_known: bool,
    pub counter_ambiguous: bool,
    pub volume_missing: bool,
    pub attribution_uncertain: bool,
}

impl VolumeQuality {
    #[inline]
    #[must_use]
    pub const fn is_eligible(self) -> bool {
        self.baseline_known
            && !self.counter_ambiguous
            && !self.volume_missing
            && !self.attribution_uncertain
    }
}

/// Result of a single constant-work cumulative-counter observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeAssessment {
    /// Previous accepted counter, or this observation when establishing a baseline.
    pub baseline: u64,
    /// Accepted monotonic counter on the current session axis.
    pub cumulative: u64,
    pub increment: u64,
    pub baseline_seeded: bool,
    pub session_changed: bool,
    pub quality: VolumeQuality,
}

/// Shared, presence-aware cumulative-volume policy. A decrease without an
/// explicit new session is ambiguous: it never lowers the accepted counter,
/// and subsequent observations cannot silently certify the affected session.
/// Magnitude, repeated packets and reaching an old high do not prove a reset.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VolumeCounter {
    session_day: Option<u32>,
    last: Option<u64>,
    ambiguous: bool,
    /// Unlike `last`, this survives an administrative same-session reset.
    /// Missing Ticker observations do not consume first-trade evidence.
    has_seen_counter: bool,
}

impl VolumeCounter {
    #[inline]
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// A same-session administrative flush may forget its quantity baseline,
    /// but it cannot turn an unresolved counter epoch into known data.
    #[inline]
    pub fn reset_baseline(&mut self) {
        self.last = None;
    }

    #[inline]
    #[must_use]
    pub const fn cumulative(&self) -> Option<u64> {
        self.last
    }

    #[inline]
    #[must_use]
    pub const fn session_day(&self) -> Option<u32> {
        self.session_day
    }

    #[inline]
    #[must_use]
    pub const fn is_ambiguous(&self) -> bool {
        self.ambiguous
    }

    /// The caller validates the session against its trusted receipt/replay clock.
    /// Older-session input is refused here as a second defence and cannot reset
    /// either the accepted axis or its ambiguity flag.
    #[inline]
    pub fn observe(
        &mut self,
        session_day: u32,
        observation: VolumeObservation,
    ) -> VolumeAssessment {
        self.observe_with_opening_trade(session_day, observation, None)
    }

    /// Observe a counter with optional, caller-validated first-trade evidence.
    ///
    /// The caller must only provide a quantity for a current-session opening
    /// second whose trusted receipt is in that same second. This counter also
    /// requires positive cumulative == that trade's quantity and no accepted
    /// counter yet in this session. A same-session reset after a counter, missing
    /// field, duplicate or unresolved counter epoch can never mint a prefix.
    /// All other observations retain the ordinary baseline-only behavior.
    #[inline]
    pub fn observe_with_opening_trade(
        &mut self,
        session_day: u32,
        observation: VolumeObservation,
        opening_trade_quantity: Option<u16>,
    ) -> VolumeAssessment {
        let old_session = self.session_day;
        let session_changed = old_session.is_some_and(|old| session_day > old);
        if old_session.is_some_and(|old| session_day < old) {
            let value = self.last.unwrap_or(0);
            return VolumeAssessment {
                baseline: value,
                cumulative: value,
                increment: 0,
                baseline_seeded: false,
                session_changed: false,
                quality: VolumeQuality {
                    baseline_known: self.last.is_some(),
                    counter_ambiguous: true,
                    volume_missing: matches!(observation, VolumeObservation::Missing),
                    attribution_uncertain: true,
                },
            };
        }
        if old_session.is_none() || session_changed {
            self.session_day = Some(session_day);
            self.last = None;
            self.ambiguous = false;
            self.has_seen_counter = false;
        }
        let mut quality = VolumeQuality {
            baseline_known: self.last.is_some(),
            counter_ambiguous: self.ambiguous,
            volume_missing: false,
            attribution_uncertain: false,
        };
        let VolumeObservation::Cumulative(value) = observation else {
            quality.volume_missing = true;
            let current = self.last.unwrap_or(0);
            return VolumeAssessment {
                baseline: current,
                cumulative: current,
                increment: 0,
                baseline_seeded: false,
                session_changed,
                quality,
            };
        };
        let Some(previous) = self.last else {
            let opening_origin_proven = !self.has_seen_counter
                && !self.ambiguous
                && opening_trade_quantity
                    .is_some_and(|quantity| quantity > 0 && value == u64::from(quantity));
            self.last = Some(value);
            self.has_seen_counter = true;
            if opening_origin_proven {
                quality.baseline_known = true;
            }
            return VolumeAssessment {
                baseline: if opening_origin_proven { 0 } else { value },
                cumulative: value,
                increment: if opening_origin_proven { value } else { 0 },
                baseline_seeded: true,
                session_changed,
                quality,
            };
        };
        if value < previous {
            self.ambiguous = true;
            quality.counter_ambiguous = true;
        } else {
            self.last = Some(value);
        }
        let cumulative = self.last.unwrap_or(previous);
        VolumeAssessment {
            baseline: previous,
            cumulative,
            increment: cumulative.saturating_sub(previous),
            baseline_seeded: false,
            session_changed,
            quality,
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

// Dead-code batch 2 (2026-07-18): `OptionGreeksSnapshot`, `GreeksEnricher` +
// `NoopGreeksEnricher`, and `DhanDailyResponse` DELETED — zero production
// callers (the generic tick-processor Greeks consumer died in stage 2; the
// prev-day daily fetch died in PR-C3). `DhanIntradayResponse` stays (pinned
// by common/tests/schema_validation.rs).

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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_counter_never_seeds_but_a_present_zero_does() {
        let mut counter = VolumeCounter::default();
        let missing = counter.observe(20_000, VolumeObservation::Missing);
        assert!(!missing.quality.baseline_known);
        assert!(missing.quality.volume_missing);
        assert_eq!(counter.cumulative(), None);
        let seeded = counter.observe(20_000, VolumeObservation::Cumulative(0));
        assert!(seeded.baseline_seeded);
        assert_eq!(counter.cumulative(), Some(0));
        assert_eq!(
            counter
                .observe(20_000, VolumeObservation::Cumulative(100))
                .increment,
            100
        );
        assert_eq!(
            counter
                .observe(20_000, VolumeObservation::Cumulative(300))
                .increment,
            200
        );
    }

    #[test]
    fn proved_opening_trade_counts_once_and_only_on_a_new_session() {
        for quantity in [1_u16, 600, u16::MAX] {
            let value = u64::from(quantity);
            let mut counter = VolumeCounter::default();
            let first = counter.observe_with_opening_trade(
                20_000,
                VolumeObservation::Cumulative(value),
                Some(quantity),
            );
            assert_eq!(first.baseline, 0);
            assert_eq!(first.increment, value);
            assert!(first.baseline_seeded);
            assert!(first.quality.is_eligible());
            let duplicate = counter.observe_with_opening_trade(
                20_000,
                VolumeObservation::Cumulative(value),
                Some(quantity),
            );
            assert_eq!(duplicate.increment, 0);
            assert!(!duplicate.baseline_seeded);

            counter.reset_baseline();
            let after_flush = counter.observe_with_opening_trade(
                20_000,
                VolumeObservation::Cumulative(value),
                Some(quantity),
            );
            assert_eq!(after_flush.baseline, value);
            assert_eq!(after_flush.increment, 0);
            assert!(!after_flush.quality.baseline_known);

            let tomorrow = counter.observe_with_opening_trade(
                20_001,
                VolumeObservation::Cumulative(value),
                Some(quantity),
            );
            assert!(tomorrow.session_changed);
            assert_eq!(tomorrow.baseline, 0);
            assert_eq!(tomorrow.increment, value);
        }
    }

    #[test]
    fn opening_hint_cannot_create_volume_without_matching_present_quantity() {
        for (observation, hint) in [
            (VolumeObservation::Missing, Some(600)),
            (VolumeObservation::Cumulative(600), None),
            (VolumeObservation::Cumulative(1_200), Some(600)),
            (VolumeObservation::Cumulative(0), Some(0)),
            (VolumeObservation::Cumulative(u64::MAX), Some(u16::MAX)),
        ] {
            let mut counter = VolumeCounter::default();
            let result = counter.observe_with_opening_trade(20_000, observation, hint);
            assert_eq!(result.increment, 0);
            assert!(!result.quality.baseline_known);
        }
    }

    #[test]
    fn missing_price_only_observation_does_not_consume_the_first_counter_evidence() {
        let mut counter = VolumeCounter::default();
        counter.observe(20_000, VolumeObservation::Missing);
        let first_counter = counter.observe_with_opening_trade(
            20_000,
            VolumeObservation::Cumulative(600),
            Some(600),
        );
        assert_eq!(first_counter.baseline, 0);
        assert_eq!(first_counter.increment, 600);
        assert!(first_counter.quality.baseline_known);
    }

    #[test]
    fn opening_hint_cannot_clear_same_session_ambiguity_or_reopen_an_old_session() {
        let mut counter = VolumeCounter::default();
        counter.observe(20_000, VolumeObservation::Cumulative(1_200));
        let regression = counter.observe_with_opening_trade(
            20_000,
            VolumeObservation::Cumulative(600),
            Some(600),
        );
        assert_eq!(regression.increment, 0);
        assert!(regression.quality.counter_ambiguous);
        counter.reset_baseline();
        let restart = counter.observe_with_opening_trade(
            20_000,
            VolumeObservation::Cumulative(600),
            Some(600),
        );
        assert_eq!(restart.increment, 0);
        assert!(restart.quality.counter_ambiguous);
        let old = counter.observe_with_opening_trade(
            19_999,
            VolumeObservation::Cumulative(600),
            Some(600),
        );
        assert_eq!(old.increment, 0);
        assert!(old.quality.attribution_uncertain);
        assert_eq!(counter.session_day(), Some(20_000));
    }

    #[test]
    fn missing_observation_cannot_trigger_a_large_counter_reset() {
        let mut counter = VolumeCounter::default();
        counter.observe(20_000, VolumeObservation::Cumulative(3_000_000_000));
        assert_eq!(
            counter
                .observe(20_000, VolumeObservation::Cumulative(3_000_000_100))
                .increment,
            100
        );
        counter.observe(20_000, VolumeObservation::Missing);
        let last = counter.observe(20_000, VolumeObservation::Cumulative(3_000_000_200));
        assert_eq!(last.increment, 100);
        assert!(last.quality.is_eligible());
    }

    #[test]
    fn ambiguous_decreases_never_reseed_after_a_packet_count_or_recovery() {
        for high in [1_000_u64, 1 << 31, u64::MAX] {
            let mut counter = VolumeCounter::default();
            counter.observe(20_000, VolumeObservation::Cumulative(high));
            for _ in 0..64 {
                let lower = counter.observe(20_000, VolumeObservation::Cumulative(10));
                assert!(lower.quality.counter_ambiguous);
                assert_eq!(lower.increment, 0);
                assert_eq!(lower.cumulative, high);
            }
            let recovery = counter.observe(20_000, VolumeObservation::Cumulative(high));
            assert_eq!(recovery.increment, 0);
            assert!(!recovery.quality.is_eligible());
            let new_session = counter.observe(20_001, VolumeObservation::Cumulative(0));
            assert!(new_session.session_changed);
            assert!(new_session.baseline_seeded);
            assert_eq!(new_session.increment, 0);
            assert!(!new_session.quality.counter_ambiguous);
            let next = counter.observe(20_001, VolumeObservation::Cumulative(50));
            assert_eq!(next.increment, 50);
            assert!(next.quality.is_eligible());
        }
    }

    #[test]
    fn old_session_and_u64_limits_cannot_corrupt_the_accepted_axis() {
        let mut counter = VolumeCounter::default();
        counter.observe(20_000, VolumeObservation::Cumulative(0));
        let highest = counter.observe(20_000, VolumeObservation::Cumulative(u64::MAX));
        assert_eq!(highest.increment, u64::MAX);
        let stale = counter.observe(19_999, VolumeObservation::Cumulative(0));
        assert!(stale.quality.attribution_uncertain);
        assert_eq!(counter.cumulative(), Some(u64::MAX));
        assert_eq!(counter.session_day(), Some(20_000));
        assert_eq!(
            counter
                .observe(20_000, VolumeObservation::Cumulative(u64::MAX))
                .increment,
            0
        );
    }

    #[test]
    fn an_administrative_flush_cannot_clear_same_session_counter_ambiguity() {
        let mut counter = VolumeCounter::default();
        counter.observe(20_000, VolumeObservation::Cumulative(1_000));
        counter.observe(20_000, VolumeObservation::Cumulative(100));
        counter.reset_baseline();
        let seeded = counter.observe(20_000, VolumeObservation::Cumulative(200));
        assert!(seeded.baseline_seeded);
        assert!(seeded.quality.counter_ambiguous);
        assert!(
            !counter
                .observe(20_000, VolumeObservation::Cumulative(300))
                .quality
                .is_eligible()
        );
        assert!(
            counter
                .observe(20_001, VolumeObservation::Cumulative(0))
                .session_changed
        );
        assert!(
            counter
                .observe(20_001, VolumeObservation::Cumulative(50))
                .quality
                .is_eligible()
        );
    }

    #[test]
    fn parsed_volume_presence_survives_an_explicit_wider_override() {
        let tick = ParsedTick {
            volume_present: false,
            ..ParsedTick::default()
        };
        assert_eq!(tick.volume_observation(None), VolumeObservation::Missing);
        assert_eq!(
            tick.volume_observation(Some(0)),
            VolumeObservation::Cumulative(0)
        );
        assert_eq!(
            tick.volume_observation(Some(u64::MAX)),
            VolumeObservation::Cumulative(u64::MAX)
        );
    }

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
            volume_present: true,
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
}

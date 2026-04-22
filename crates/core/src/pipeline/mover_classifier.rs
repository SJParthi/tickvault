//! Mover bucket + category classification (plan item A, 2026-04-22).
//!
//! Classifies each live tick into one of SIX mutually-exclusive movers
//! buckets so the downstream tracker can maintain **independent top-N
//! leaderboards** per bucket instead of mixing derivative types.
//!
//! # The 6 buckets
//!
//! | Bucket              | Segment          | Category                | Instrument kind |
//! |---------------------|------------------|-------------------------|-----------------|
//! | `Indices`           | `IDX_I`          | MajorIndexValue / DisplayIndex | — |
//! | `Stocks`            | `NSE_EQ`/`BSE_EQ`| StockEquity             | — |
//! | `IndexFutures`      | `NSE_FNO`        | IndexDerivative         | FutureIndex (FUTIDX) |
//! | `StockFutures`      | `NSE_FNO`        | StockDerivative         | FutureStock (FUTSTK) |
//! | `IndexOptions`      | `NSE_FNO`        | IndexDerivative         | OptionIndex (OPTIDX) |
//! | `StockOptions`      | `NSE_FNO`        | StockDerivative         | OptionStock (OPTSTK) |
//!
//! The classifier delegates to `InstrumentRegistry::get_with_segment`
//! which is I-P1-11 compliant (segment-aware lookup). An instrument
//! missing from the registry returns `None` — caller must skip such
//! ticks from ranking (they should never reach the tracker in prod,
//! but defensive classification is O(1) and cheap).
//!
//! # Performance
//!
//! `classify_instrument` = single `get_with_segment` call + one match.
//! O(1), < 50 ns in the hot path per the `instrument_registry` budget.
//! The result is expected to be cached inside the tracker's
//! `SecurityState` so the classifier runs at most once per instrument
//! per session.

use tickvault_common::instrument_registry::{InstrumentRegistry, SubscriptionCategory};
use tickvault_common::instrument_types::DhanInstrumentKind;
use tickvault_common::types::{ExchangeSegment, SecurityId};

// ---------------------------------------------------------------------------
// MoverBucket
// ---------------------------------------------------------------------------

/// One of the six top-level movers buckets. Each bucket has an
/// independently-ranked top-N leaderboard; values never cross buckets.
///
/// The `as_str()` wire format is persisted to QuestDB
/// (`top_movers.bucket SYMBOL` column) and emitted as a Prometheus
/// label. Once shipped, **DO NOT RENAME** — only add new variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MoverBucket {
    /// IDX_I — NIFTY 50, BANKNIFTY, INDIA VIX, sector indices, etc.
    Indices,
    /// NSE_EQ / BSE_EQ — RELIANCE, TCS, INFY, etc.
    Stocks,
    /// NSE_FNO / FUTIDX — NIFTY FUT Apr26, BANKNIFTY FUT Apr26, etc.
    IndexFutures,
    /// NSE_FNO / FUTSTK — RELIANCE FUT Apr26, TCS FUT Apr26, etc.
    StockFutures,
    /// NSE_FNO / OPTIDX — NIFTY 25000 CE, BANKNIFTY 50000 PE, etc.
    IndexOptions,
    /// NSE_FNO / OPTSTK — RELIANCE 3000 CE, TCS 4000 PE, etc.
    StockOptions,
}

impl MoverBucket {
    /// Stable wire-format string for QuestDB `bucket SYMBOL` column and
    /// Prometheus `bucket=` label. DO NOT RENAME — values are persisted
    /// across the SEBI 5-year retention window.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Indices => "indices",
            Self::Stocks => "stocks",
            Self::IndexFutures => "index_futures",
            Self::StockFutures => "stock_futures",
            Self::IndexOptions => "index_options",
            Self::StockOptions => "stock_options",
        }
    }

    /// Returns true for buckets whose instruments have open interest
    /// (derivatives). Used by `MoverCategory::is_applicable_to` to
    /// gate OI-based rankings.
    #[must_use]
    pub const fn has_open_interest(&self) -> bool {
        matches!(
            self,
            Self::IndexFutures | Self::StockFutures | Self::IndexOptions | Self::StockOptions
        )
    }

    /// Returns all six variants in canonical order for iteration in
    /// snapshot construction and tests.
    #[must_use]
    pub const fn all() -> [Self; 6] {
        [
            Self::Indices,
            Self::Stocks,
            Self::IndexFutures,
            Self::StockFutures,
            Self::IndexOptions,
            Self::StockOptions,
        ]
    }
}

impl std::fmt::Display for MoverBucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// MoverCategory
// ---------------------------------------------------------------------------

/// A single ranking dimension within a bucket. Seven categories total.
///
/// Every bucket supports `Gainers`/`Losers`/`MostActive`. The four
/// OI-dependent categories (`TopOi`, `OiBuildup`, `OiUnwind`,
/// `TopValue`) are only applicable to derivative buckets
/// (`MoverBucket::has_open_interest() == true`). The API layer returns
/// HTTP 405 Method Not Allowed when an OI category is requested for
/// `Indices` or `Stocks`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MoverCategory {
    /// Top-N by % change descending (positive only).
    Gainers,
    /// Top-N by % change ascending (negative only).
    Losers,
    /// Top-N by traded volume descending.
    MostActive,
    /// Top-N by absolute open interest descending. Derivatives only.
    TopOi,
    /// Top-N by OI change % descending (positive only). Derivatives only.
    OiBuildup,
    /// Top-N by OI change % ascending (negative only). Derivatives only.
    OiUnwind,
    /// Top-N by traded value (LTP × volume) descending. Derivatives only.
    TopValue,
}

impl MoverCategory {
    /// Stable wire-format string for QuestDB `rank_category SYMBOL`
    /// column and `?category=` HTTP query parameter. DO NOT RENAME.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Gainers => "gainers",
            Self::Losers => "losers",
            Self::MostActive => "most_active",
            Self::TopOi => "top_oi",
            Self::OiBuildup => "oi_buildup",
            Self::OiUnwind => "oi_unwind",
            Self::TopValue => "top_value",
        }
    }

    /// Returns true if this category is meaningful for the given
    /// bucket. OI-based categories are only applicable to derivative
    /// buckets; price-based categories apply to all buckets.
    #[must_use]
    pub const fn is_applicable_to(&self, bucket: MoverBucket) -> bool {
        match self {
            // Price/volume categories apply to every bucket.
            Self::Gainers | Self::Losers | Self::MostActive => true,
            // OI-based categories only apply to derivative buckets.
            Self::TopOi | Self::OiBuildup | Self::OiUnwind | Self::TopValue => {
                bucket.has_open_interest()
            }
        }
    }

    /// Parse an HTTP `?category=` query-parameter value. Returns `None`
    /// for unknown strings so the handler can respond with 400 and a
    /// list of valid values.
    #[must_use]
    pub fn from_str_slice(value: &str) -> Option<Self> {
        match value {
            "gainers" => Some(Self::Gainers),
            "losers" => Some(Self::Losers),
            "most_active" => Some(Self::MostActive),
            "top_oi" => Some(Self::TopOi),
            "oi_buildup" => Some(Self::OiBuildup),
            "oi_unwind" => Some(Self::OiUnwind),
            "top_value" => Some(Self::TopValue),
            _ => None,
        }
    }

    /// All seven variants in canonical order.
    #[must_use]
    pub const fn all() -> [Self; 7] {
        [
            Self::Gainers,
            Self::Losers,
            Self::MostActive,
            Self::TopOi,
            Self::OiBuildup,
            Self::OiUnwind,
            Self::TopValue,
        ]
    }
}

impl std::fmt::Display for MoverCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Classifier
// ---------------------------------------------------------------------------

/// Resolve `(security_id, segment)` into its `MoverBucket`.
///
/// Uses `InstrumentRegistry::get_with_segment` (I-P1-11 segment-aware).
/// Returns `None` if:
/// - the instrument is not in the registry (caller should drop the tick); or
/// - the instrument is a derivative with `instrument_kind = None` (malformed — never expected in prod).
///
/// # Performance
///
/// O(1) — one `HashMap::get` + one enum match. No allocation.
#[inline]
#[must_use]
pub fn classify_instrument(
    registry: &InstrumentRegistry,
    security_id: SecurityId,
    segment: ExchangeSegment,
) -> Option<MoverBucket> {
    let instrument = registry.get_with_segment(security_id, segment)?;
    match instrument.category {
        SubscriptionCategory::MajorIndexValue | SubscriptionCategory::DisplayIndex => {
            Some(MoverBucket::Indices)
        }
        SubscriptionCategory::StockEquity => Some(MoverBucket::Stocks),
        SubscriptionCategory::IndexDerivative => match instrument.instrument_kind? {
            DhanInstrumentKind::FutureIndex => Some(MoverBucket::IndexFutures),
            DhanInstrumentKind::OptionIndex => Some(MoverBucket::IndexOptions),
            // Index derivative MUST NOT carry a stock kind; treat as
            // malformed universe and drop the tick.
            DhanInstrumentKind::FutureStock | DhanInstrumentKind::OptionStock => None,
        },
        SubscriptionCategory::StockDerivative => match instrument.instrument_kind? {
            DhanInstrumentKind::FutureStock => Some(MoverBucket::StockFutures),
            DhanInstrumentKind::OptionStock => Some(MoverBucket::StockOptions),
            // Stock derivative MUST NOT carry an index kind; treat as
            // malformed universe and drop the tick.
            DhanInstrumentKind::FutureIndex | DhanInstrumentKind::OptionIndex => None,
        },
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use tickvault_common::instrument_registry::{
        InstrumentRegistry, SubscribedInstrument, SubscriptionCategory,
    };
    use tickvault_common::types::{FeedMode, OptionType};

    // ---- MoverBucket tests ------------------------------------------------

    #[test]
    fn test_mover_bucket_as_str_is_stable() {
        assert_eq!(MoverBucket::Indices.as_str(), "indices");
        assert_eq!(MoverBucket::Stocks.as_str(), "stocks");
        assert_eq!(MoverBucket::IndexFutures.as_str(), "index_futures");
        assert_eq!(MoverBucket::StockFutures.as_str(), "stock_futures");
        assert_eq!(MoverBucket::IndexOptions.as_str(), "index_options");
        assert_eq!(MoverBucket::StockOptions.as_str(), "stock_options");
    }

    #[test]
    fn test_mover_bucket_all_strings_unique() {
        let mut seen = std::collections::HashSet::new();
        for b in MoverBucket::all() {
            assert!(seen.insert(b.as_str()), "duplicate as_str() for {b:?}");
        }
        assert_eq!(seen.len(), 6);
    }

    #[test]
    fn test_mover_bucket_has_open_interest_is_true_for_derivatives() {
        assert!(!MoverBucket::Indices.has_open_interest());
        assert!(!MoverBucket::Stocks.has_open_interest());
        assert!(MoverBucket::IndexFutures.has_open_interest());
        assert!(MoverBucket::StockFutures.has_open_interest());
        assert!(MoverBucket::IndexOptions.has_open_interest());
        assert!(MoverBucket::StockOptions.has_open_interest());
    }

    #[test]
    fn test_mover_bucket_display_matches_as_str() {
        assert_eq!(
            format!("{}", MoverBucket::IndexOptions),
            MoverBucket::IndexOptions.as_str()
        );
    }

    #[test]
    fn test_mover_bucket_all_returns_six_variants() {
        assert_eq!(MoverBucket::all().len(), 6);
    }

    // ---- MoverCategory tests ---------------------------------------------

    #[test]
    fn test_mover_category_as_str_is_stable() {
        assert_eq!(MoverCategory::Gainers.as_str(), "gainers");
        assert_eq!(MoverCategory::Losers.as_str(), "losers");
        assert_eq!(MoverCategory::MostActive.as_str(), "most_active");
        assert_eq!(MoverCategory::TopOi.as_str(), "top_oi");
        assert_eq!(MoverCategory::OiBuildup.as_str(), "oi_buildup");
        assert_eq!(MoverCategory::OiUnwind.as_str(), "oi_unwind");
        assert_eq!(MoverCategory::TopValue.as_str(), "top_value");
    }

    #[test]
    fn test_mover_category_from_str_roundtrip() {
        for c in MoverCategory::all() {
            let parsed = MoverCategory::from_str_slice(c.as_str()).expect("known");
            assert_eq!(parsed, c);
        }
    }

    #[test]
    fn test_mover_category_from_str_unknown_is_none() {
        assert!(MoverCategory::from_str_slice("").is_none());
        assert!(MoverCategory::from_str_slice("Gainers").is_none()); // case-sensitive
        assert!(MoverCategory::from_str_slice("nonsense").is_none());
    }

    #[test]
    fn test_mover_category_price_categories_apply_to_every_bucket() {
        for bucket in MoverBucket::all() {
            assert!(MoverCategory::Gainers.is_applicable_to(bucket));
            assert!(MoverCategory::Losers.is_applicable_to(bucket));
            assert!(MoverCategory::MostActive.is_applicable_to(bucket));
        }
    }

    #[test]
    fn test_mover_category_oi_categories_apply_only_to_derivatives() {
        let oi_cats = [
            MoverCategory::TopOi,
            MoverCategory::OiBuildup,
            MoverCategory::OiUnwind,
            MoverCategory::TopValue,
        ];
        for cat in oi_cats {
            assert!(
                !cat.is_applicable_to(MoverBucket::Indices),
                "{cat:?} must NOT apply to Indices"
            );
            assert!(
                !cat.is_applicable_to(MoverBucket::Stocks),
                "{cat:?} must NOT apply to Stocks"
            );
            assert!(cat.is_applicable_to(MoverBucket::IndexFutures));
            assert!(cat.is_applicable_to(MoverBucket::StockFutures));
            assert!(cat.is_applicable_to(MoverBucket::IndexOptions));
            assert!(cat.is_applicable_to(MoverBucket::StockOptions));
        }
    }

    #[test]
    fn test_mover_category_all_strings_unique() {
        let mut seen = std::collections::HashSet::new();
        for c in MoverCategory::all() {
            assert!(seen.insert(c.as_str()), "duplicate as_str() for {c:?}");
        }
        assert_eq!(seen.len(), 7);
    }

    // ---- classify_instrument tests ---------------------------------------

    /// Build a registry containing instruments covering every bucket.
    fn make_registry_covering_all_buckets() -> InstrumentRegistry {
        let instruments = vec![
            // Indices: MajorIndexValue (NIFTY IDX_I = 13)
            SubscribedInstrument {
                security_id: 13,
                exchange_segment: ExchangeSegment::IdxI,
                category: SubscriptionCategory::MajorIndexValue,
                display_label: "NIFTY".to_string(),
                underlying_symbol: "NIFTY".to_string(),
                instrument_kind: None,
                expiry_date: None,
                strike_price: None,
                option_type: None,
                feed_mode: FeedMode::Ticker,
            },
            // Indices: DisplayIndex (INDIA VIX IDX_I = 21)
            SubscribedInstrument {
                security_id: 21,
                exchange_segment: ExchangeSegment::IdxI,
                category: SubscriptionCategory::DisplayIndex,
                display_label: "INDIA VIX".to_string(),
                underlying_symbol: "INDIAVIX".to_string(),
                instrument_kind: None,
                expiry_date: None,
                strike_price: None,
                option_type: None,
                feed_mode: FeedMode::Ticker,
            },
            // Stocks: StockEquity (RELIANCE NSE_EQ = 2885)
            SubscribedInstrument {
                security_id: 2885,
                exchange_segment: ExchangeSegment::NseEquity,
                category: SubscriptionCategory::StockEquity,
                display_label: "RELIANCE".to_string(),
                underlying_symbol: "RELIANCE".to_string(),
                instrument_kind: None,
                expiry_date: None,
                strike_price: None,
                option_type: None,
                feed_mode: FeedMode::Quote,
            },
            // IndexFutures: IndexDerivative + FutureIndex (NIFTY FUT = 50001)
            SubscribedInstrument {
                security_id: 50001,
                exchange_segment: ExchangeSegment::NseFno,
                category: SubscriptionCategory::IndexDerivative,
                display_label: "NIFTY FUT Apr26".to_string(),
                underlying_symbol: "NIFTY".to_string(),
                instrument_kind: Some(DhanInstrumentKind::FutureIndex),
                expiry_date: NaiveDate::from_ymd_opt(2026, 4, 28),
                strike_price: None,
                option_type: None,
                feed_mode: FeedMode::Full,
            },
            // StockFutures: StockDerivative + FutureStock (RELIANCE FUT = 60001)
            SubscribedInstrument {
                security_id: 60001,
                exchange_segment: ExchangeSegment::NseFno,
                category: SubscriptionCategory::StockDerivative,
                display_label: "RELIANCE FUT Apr26".to_string(),
                underlying_symbol: "RELIANCE".to_string(),
                instrument_kind: Some(DhanInstrumentKind::FutureStock),
                expiry_date: NaiveDate::from_ymd_opt(2026, 4, 28),
                strike_price: None,
                option_type: None,
                feed_mode: FeedMode::Full,
            },
            // IndexOptions: IndexDerivative + OptionIndex (NIFTY 25000 CE = 70001)
            SubscribedInstrument {
                security_id: 70001,
                exchange_segment: ExchangeSegment::NseFno,
                category: SubscriptionCategory::IndexDerivative,
                display_label: "NIFTY 25000 CE Apr26".to_string(),
                underlying_symbol: "NIFTY".to_string(),
                instrument_kind: Some(DhanInstrumentKind::OptionIndex),
                expiry_date: NaiveDate::from_ymd_opt(2026, 4, 28),
                strike_price: Some(25000.0),
                option_type: Some(OptionType::Call),
                feed_mode: FeedMode::Full,
            },
            // StockOptions: StockDerivative + OptionStock (RELIANCE 3000 CE = 80001)
            SubscribedInstrument {
                security_id: 80001,
                exchange_segment: ExchangeSegment::NseFno,
                category: SubscriptionCategory::StockDerivative,
                display_label: "RELIANCE 3000 CE Apr26".to_string(),
                underlying_symbol: "RELIANCE".to_string(),
                instrument_kind: Some(DhanInstrumentKind::OptionStock),
                expiry_date: NaiveDate::from_ymd_opt(2026, 4, 28),
                strike_price: Some(3000.0),
                option_type: Some(OptionType::Call),
                feed_mode: FeedMode::Full,
            },
        ];
        InstrumentRegistry::from_instruments(instruments)
    }

    #[test]
    fn test_classify_major_index_value_returns_indices() {
        let reg = make_registry_covering_all_buckets();
        assert_eq!(
            classify_instrument(&reg, 13, ExchangeSegment::IdxI),
            Some(MoverBucket::Indices)
        );
    }

    #[test]
    fn test_classify_display_index_returns_indices() {
        let reg = make_registry_covering_all_buckets();
        assert_eq!(
            classify_instrument(&reg, 21, ExchangeSegment::IdxI),
            Some(MoverBucket::Indices)
        );
    }

    #[test]
    fn test_classify_stock_equity_returns_stocks() {
        let reg = make_registry_covering_all_buckets();
        assert_eq!(
            classify_instrument(&reg, 2885, ExchangeSegment::NseEquity),
            Some(MoverBucket::Stocks)
        );
    }

    #[test]
    fn test_classify_nifty_future_returns_index_futures() {
        let reg = make_registry_covering_all_buckets();
        assert_eq!(
            classify_instrument(&reg, 50001, ExchangeSegment::NseFno),
            Some(MoverBucket::IndexFutures)
        );
    }

    #[test]
    fn test_classify_reliance_future_returns_stock_futures() {
        let reg = make_registry_covering_all_buckets();
        assert_eq!(
            classify_instrument(&reg, 60001, ExchangeSegment::NseFno),
            Some(MoverBucket::StockFutures)
        );
    }

    #[test]
    fn test_classify_nifty_option_returns_index_options() {
        let reg = make_registry_covering_all_buckets();
        assert_eq!(
            classify_instrument(&reg, 70001, ExchangeSegment::NseFno),
            Some(MoverBucket::IndexOptions)
        );
    }

    #[test]
    fn test_classify_reliance_option_returns_stock_options() {
        let reg = make_registry_covering_all_buckets();
        assert_eq!(
            classify_instrument(&reg, 80001, ExchangeSegment::NseFno),
            Some(MoverBucket::StockOptions)
        );
    }

    #[test]
    fn test_classify_unknown_security_id_returns_none() {
        let reg = make_registry_covering_all_buckets();
        assert_eq!(
            classify_instrument(&reg, 99999, ExchangeSegment::NseFno),
            None
        );
    }

    #[test]
    fn test_classify_known_id_wrong_segment_returns_none() {
        // RELIANCE has id=2885 on NSE_EQ. Same id on IDX_I is a different
        // (possibly missing) instrument per I-P1-11 — must NOT collide.
        let reg = make_registry_covering_all_buckets();
        assert_eq!(classify_instrument(&reg, 2885, ExchangeSegment::IdxI), None);
    }

    /// Umbrella test that names `classify_instrument` literally so the
    /// pub-fn-test-guard matches. Covers every bucket in one pass.
    #[test]
    fn test_classify_instrument_routes_all_six_buckets() {
        let reg = make_registry_covering_all_buckets();
        let expected: &[(u32, ExchangeSegment, MoverBucket)] = &[
            (13, ExchangeSegment::IdxI, MoverBucket::Indices),
            (21, ExchangeSegment::IdxI, MoverBucket::Indices),
            (2885, ExchangeSegment::NseEquity, MoverBucket::Stocks),
            (50001, ExchangeSegment::NseFno, MoverBucket::IndexFutures),
            (60001, ExchangeSegment::NseFno, MoverBucket::StockFutures),
            (70001, ExchangeSegment::NseFno, MoverBucket::IndexOptions),
            (80001, ExchangeSegment::NseFno, MoverBucket::StockOptions),
        ];
        for &(sid, seg, want) in expected {
            assert_eq!(
                classify_instrument(&reg, sid, seg),
                Some(want),
                "classify_instrument({sid}, {seg:?}) expected {want:?}"
            );
        }
    }

    /// Umbrella test naming `is_applicable_to` literally.
    #[test]
    fn test_is_applicable_to_gates_oi_only_on_derivative_buckets() {
        for bucket in MoverBucket::all() {
            let oi_applicable = MoverCategory::TopOi.is_applicable_to(bucket);
            assert_eq!(oi_applicable, bucket.has_open_interest());
        }
    }

    /// Umbrella test naming `from_str_slice` literally.
    #[test]
    fn test_from_str_slice_parses_every_canonical_category_string() {
        for cat in MoverCategory::all() {
            let round = MoverCategory::from_str_slice(cat.as_str());
            assert_eq!(round, Some(cat));
        }
    }
}

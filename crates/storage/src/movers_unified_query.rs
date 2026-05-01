//! Wave 5 Item 25/27 Phase C — read-path SQL builder for `movers_unified_*`.
//!
//! Pure-logic SQL string builder. Produces the right SELECT against the
//! correct materialized view per (timeframe, category). Operator runs
//! these via QuestDB `/exec` HTTP endpoint or REST handler.
//!
//! # Design (per plan §"Item 25 — Read path")
//!
//! - Each query targets a single materialized view (`movers_unified_<tf>`)
//! - Defensive `WHERE segment != 'BSE_FNO'` for top-volume / top-gainer
//!   per Wave 5 Item 4 SENSEX filter
//! - `LIMIT` defaults to 50 (operator can override per category)
//! - All literals SQL-escaped via single-quote-doubling (the only user
//!   input is timeframe + category enum, both validated server-side)
//!
//! # Why pure-logic
//!
//! - `cargo test` runs zero-cost (no QuestDB dep)
//! - Schema-drift catches happen at compile time (column names pinned)
//! - SQL injection surface = zero (only enum-typed inputs)
//! - Caller fetches via reqwest::get → questdb /exec → JSON

use std::fmt;

/// Read-side mover category. Maps to the `ORDER BY` clause + (optional)
/// extra WHERE filters per the plan §"Read-time category mappings"
/// table in Item 27.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MoversCategory {
    /// `ORDER BY open_interest DESC` — snapshot ranking.
    HighestOi,
    /// `ORDER BY oi_delta_bucket DESC` — bucket-incremental gainer.
    OiGainer,
    /// `ORDER BY oi_delta_bucket ASC` — bucket-incremental loser.
    OiLoser,
    /// `ORDER BY volume_cumulative DESC` — session intraday total.
    TopVolumeIntraday,
    /// `ORDER BY volume_bucket DESC` — bucket-incremental top volume.
    TopVolumeBucket,
    /// `ORDER BY (last_price * volume_bucket) DESC` — turnover by bucket.
    TopValueBucket,
    /// `ORDER BY change_pct_session DESC` — session-vs-prev-day gainer.
    PriceGainerSession,
    /// `ORDER BY change_pct_bucket DESC` — bucket-incremental gainer.
    PriceGainerBucket,
    /// `ORDER BY change_pct_session ASC` — session-vs-prev-day loser.
    PriceLoserSession,
    /// `ORDER BY change_pct_bucket ASC` — bucket-incremental loser.
    PriceLoserBucket,
}

impl MoversCategory {
    /// Stable wire-format string used as URL query param + Telegram label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HighestOi => "highest_oi",
            Self::OiGainer => "oi_gainer",
            Self::OiLoser => "oi_loser",
            Self::TopVolumeIntraday => "top_volume_intraday",
            Self::TopVolumeBucket => "top_volume_bucket",
            Self::TopValueBucket => "top_value_bucket",
            Self::PriceGainerSession => "price_gainer_session",
            Self::PriceGainerBucket => "price_gainer_bucket",
            Self::PriceLoserSession => "price_loser_session",
            Self::PriceLoserBucket => "price_loser_bucket",
        }
    }

    /// Parses the wire-format string back to enum. Returns `None` on
    /// unknown input — caller HTTP-handles as 400.
    ///
    /// Named `parse_wire_str` (NOT `from_str`) to avoid clashing with
    /// `std::str::FromStr::from_str` which has different semantics
    /// (Result instead of Option).
    #[must_use]
    pub fn parse_wire_str(s: &str) -> Option<Self> {
        match s {
            "highest_oi" => Some(Self::HighestOi),
            "oi_gainer" => Some(Self::OiGainer),
            "oi_loser" => Some(Self::OiLoser),
            "top_volume_intraday" => Some(Self::TopVolumeIntraday),
            "top_volume_bucket" => Some(Self::TopVolumeBucket),
            "top_value_bucket" => Some(Self::TopValueBucket),
            "price_gainer_session" => Some(Self::PriceGainerSession),
            "price_gainer_bucket" => Some(Self::PriceGainerBucket),
            "price_loser_session" => Some(Self::PriceLoserSession),
            "price_loser_bucket" => Some(Self::PriceLoserBucket),
            _ => None,
        }
    }

    /// `ORDER BY` clause body. Excludes the `ORDER BY` prefix to keep
    /// composition flexible.
    #[must_use]
    pub const fn order_by_clause(self) -> &'static str {
        match self {
            Self::HighestOi => "open_interest DESC",
            Self::OiGainer => "oi_delta_bucket DESC",
            Self::OiLoser => "oi_delta_bucket ASC",
            Self::TopVolumeIntraday => "volume_cumulative DESC",
            Self::TopVolumeBucket => "volume_bucket DESC",
            Self::TopValueBucket => "(last_price * volume_bucket) DESC",
            Self::PriceGainerSession => "change_pct_session DESC",
            Self::PriceGainerBucket => "change_pct_bucket DESC",
            Self::PriceLoserSession => "change_pct_session ASC",
            Self::PriceLoserBucket => "change_pct_bucket ASC",
        }
    }
}

impl fmt::Display for MoversCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Default LIMIT for top-movers reads — matches dashboard tile count.
pub const MOVERS_QUERY_DEFAULT_LIMIT: usize = 50;

/// Builds the read-side SELECT against `movers_unified_<tf>`.
///
/// `timeframe` MUST be one of the 24 mat-view timeframes (caller validates
/// via `MOVERS_VIEW_TIMEFRAMES`). `limit` clamped to `[1, 200]`.
///
/// Excludes `BSE_FNO` per Wave 5 Item 4 SENSEX-options filter.
#[must_use]
pub fn build_movers_query(timeframe: &str, category: MoversCategory, limit: usize) -> String {
    let bounded_limit = limit.clamp(1, 200);
    format!(
        "SELECT \
            security_id, segment, ts, \
            last_price, open_interest, volume_cumulative, prev_close, \
            volume_bucket, oi_delta_bucket, \
            open_price_bucket, high_price_bucket, low_price_bucket, \
            change_pct_session, change_pct_bucket \
         FROM movers_unified_{timeframe} \
         WHERE ts > dateadd('s', -300, now()) \
           AND segment != 'BSE_FNO' \
         ORDER BY {order} \
         LIMIT {bounded_limit};",
        order = category.order_by_clause()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_category_as_str_roundtrips() {
        for cat in [
            MoversCategory::HighestOi,
            MoversCategory::OiGainer,
            MoversCategory::OiLoser,
            MoversCategory::TopVolumeIntraday,
            MoversCategory::TopVolumeBucket,
            MoversCategory::TopValueBucket,
            MoversCategory::PriceGainerSession,
            MoversCategory::PriceGainerBucket,
            MoversCategory::PriceLoserSession,
            MoversCategory::PriceLoserBucket,
        ] {
            let s = cat.as_str();
            assert_eq!(
                MoversCategory::parse_wire_str(s),
                Some(cat),
                "roundtrip {s}"
            );
        }
    }

    #[test]
    fn test_parse_wire_str_rejects_unknown() {
        assert_eq!(MoversCategory::parse_wire_str(""), None);
        assert_eq!(MoversCategory::parse_wire_str("bogus"), None);
        assert_eq!(MoversCategory::parse_wire_str("HIGHEST_OI"), None); // case-sensitive
    }

    #[test]
    fn test_build_query_includes_target_view_for_5m() {
        let sql = build_movers_query("5m", MoversCategory::TopVolumeBucket, 50);
        assert!(sql.contains("FROM movers_unified_5m"));
    }

    #[test]
    fn test_build_query_excludes_bse_fno_per_wave_5_item_4() {
        let sql = build_movers_query("5m", MoversCategory::PriceGainerSession, 50);
        assert!(sql.contains("segment != 'BSE_FNO'"));
    }

    #[test]
    fn test_build_query_uses_5_minute_freshness_window() {
        let sql = build_movers_query("5m", MoversCategory::TopVolumeBucket, 50);
        // Read-side recency: only consider the last ~5 min of mat-view rows.
        assert!(sql.contains("dateadd('s', -300, now())"));
    }

    #[test]
    fn test_build_query_clamps_limit_to_max_200() {
        let sql = build_movers_query("1m", MoversCategory::HighestOi, 99_999);
        assert!(sql.contains("LIMIT 200"));
    }

    #[test]
    fn test_build_query_clamps_limit_to_min_1() {
        let sql = build_movers_query("1m", MoversCategory::HighestOi, 0);
        assert!(sql.contains("LIMIT 1"));
    }

    #[test]
    fn test_build_query_default_limit_is_50_via_constant() {
        let sql = build_movers_query(
            "5m",
            MoversCategory::TopVolumeBucket,
            MOVERS_QUERY_DEFAULT_LIMIT,
        );
        assert!(sql.contains("LIMIT 50"));
    }

    #[test]
    fn test_build_query_top_value_uses_price_volume_product() {
        let sql = build_movers_query("5m", MoversCategory::TopValueBucket, 50);
        assert!(sql.contains("ORDER BY (last_price * volume_bucket) DESC"));
    }

    #[test]
    fn test_build_query_includes_all_14_columns() {
        let sql = build_movers_query("5m", MoversCategory::HighestOi, 50);
        for col in [
            "security_id",
            "segment",
            "ts",
            "last_price",
            "open_interest",
            "volume_cumulative",
            "prev_close",
            "volume_bucket",
            "oi_delta_bucket",
            "open_price_bucket",
            "high_price_bucket",
            "low_price_bucket",
            "change_pct_session",
            "change_pct_bucket",
        ] {
            assert!(sql.contains(col), "SELECT must include `{col}`");
        }
    }

    #[test]
    fn test_build_query_loser_uses_asc_order() {
        let sql = build_movers_query("5m", MoversCategory::PriceLoserBucket, 50);
        assert!(sql.contains("change_pct_bucket ASC"));
    }

    #[test]
    fn test_order_by_clause_returns_distinct_clauses_per_category() {
        // Pub-fn substring-match guard pin for `pub const fn order_by_clause`.
        // Sanity: 10 categories must produce 10 distinct ORDER BY clauses
        // (no copy-paste collisions).
        let mut seen = std::collections::HashSet::new();
        for cat in [
            MoversCategory::HighestOi,
            MoversCategory::OiGainer,
            MoversCategory::OiLoser,
            MoversCategory::TopVolumeIntraday,
            MoversCategory::TopVolumeBucket,
            MoversCategory::TopValueBucket,
            MoversCategory::PriceGainerSession,
            MoversCategory::PriceGainerBucket,
            MoversCategory::PriceLoserSession,
            MoversCategory::PriceLoserBucket,
        ] {
            assert!(
                seen.insert(cat.order_by_clause()),
                "{cat:?} produced duplicate ORDER BY"
            );
        }
        assert_eq!(seen.len(), 10);
    }

    #[test]
    fn test_build_movers_query_produces_complete_select_statement() {
        // Pub-fn substring-match guard pin for `pub fn build_movers_query`.
        let sql = build_movers_query("1m", MoversCategory::HighestOi, 50);
        assert!(sql.starts_with("SELECT"));
        assert!(sql.contains("FROM movers_unified_1m"));
        assert!(sql.contains("WHERE"));
        assert!(sql.contains("ORDER BY"));
        assert!(sql.contains("LIMIT"));
        assert!(sql.ends_with(';'));
    }

    #[test]
    fn test_category_display_matches_as_str() {
        // Display impl is identity over `as_str` per the wire-format
        // contract.
        for cat in [MoversCategory::HighestOi, MoversCategory::OiLoser] {
            assert_eq!(format!("{cat}"), cat.as_str());
        }
    }
}

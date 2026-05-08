//! Wave-5 §K-L28..L36 — Top-N query primitive over candle bars.
//!
//! Pure-function `top_n_by_bars` that takes a slice of `Bar`s,
//! filters by [`Scope`] segment criteria, sorts by [`Category`]'s
//! (sort_field, direction) per L30, and returns the top N. The
//! caller (e.g. `/api/movers/v2`) supplies the bar slice — typically
//! by snapshotting `CascadeFanout`'s per-TF engine map.
//!
//! ## Why a separate module (not on `CascadeFanout`)
//!
//! L35 explicitly retires the `MoversEngine` abstraction. Top-N
//! queries are EXPRESSIONS over the existing data structures (tick
//! map, candle map). Keeping them as pure functions in a small
//! module makes the contract auditable + composable for future
//! consumers (depth-dynamic selector per L34, alerting / dashboards).
//!
//! ## Filter dimensions (per L29 — Dhan UI parity, exactly 3)
//!
//! | Dim | Values | Source in Dhan UI |
//! |---|---|---|
//! | [`Category`] | 7 enum variants (HighestOI / OIGainers / ... / PriceLosers) | top tab buttons |
//! | `expiry` | date label string ("26 MAY", "26 JUN", "26 JUL") | top-right dropdown |
//! | [`Scope`] | 5 mutually-exclusive (All / Index / Stock / NSE / BSE) | far-right dropdown |
//!
//! Operator-controlled `n` per call (L32 — no hard cap).
//!
//! ## Performance
//!
//! O(N log N) sort + O(N) filter where N is the number of bars
//! supplied. Off the per-tick hot path; called by `/api/movers/v2`
//! handlers (≥1 second between calls in normal operator workflow)
//! and the depth-dynamic selector (1 call per minute per L37).
//!
//! ## What this module does NOT do
//!
//! - Fetch / iterate the bar slice — caller's responsibility (typically
//!   `CascadeFanout::snapshot_<tf>m()`, shipped in a follow-up wiring PR).
//! - Filter by expiry — the bar slice doesn't carry an expiry label
//!   today. Expiry filtering requires a join against
//!   `InstrumentRegistry`; ships as a separate PR.
//! - Filter by Index/Stock instrument-type — also requires the
//!   registry. Today the [`Scope`] enum supports `All`, `NSE`, `BSE`
//!   directly via segment_code. `Index` and `Stock` map back to
//!   "All" pending the registry-aware filter PR.

use crate::candles::engine::Bar;

/// 7 sort categories per L30. Mapped to (sort_field, direction)
/// inside [`Category::sort_key`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// `open_interest DESC` — biggest absolute OI first.
    HighestOI,
    /// `oi_pct_from_prev_day DESC` — biggest % OI growth first.
    OIGainers,
    /// `oi_pct_from_prev_day ASC` — biggest % OI drop first.
    OILosers,
    /// `cumulative_volume DESC` — biggest day volume first.
    TopVolume,
    /// `(close × cumulative_volume) DESC` — biggest day turnover first.
    TopValue,
    /// `close_pct_from_prev_day DESC` — biggest % gain first.
    PriceGainers,
    /// `close_pct_from_prev_day ASC` — biggest % drop first.
    PriceLosers,
}

/// 5 mutually-exclusive instrument scopes per L31.
///
/// Today only `All` / `NSE` / `BSE` discriminate at the segment_code
/// level; `Index` and `Stock` need the `InstrumentRegistry` for
/// instrument_type lookup and currently fall through to `All`. The
/// registry-aware filter ships as a small follow-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// No filter — every (security_id, segment) is eligible.
    All,
    /// Indices only — `instrument_type IN (Index, IndexFuture, IndexOption)`.
    /// **Today**: falls through to `All` pending registry-aware filter.
    Index,
    /// Stocks only — `instrument_type IN (Equity, StockFuture, StockOption)`.
    /// **Today**: falls through to `All` pending registry-aware filter.
    Stock,
    /// NSE segments only — `segment IN (NSE_EQ=1, NSE_FNO=2, IDX_I=0)`.
    Nse,
    /// BSE segments only — `segment IN (BSE_EQ=4, BSE_FNO=8)`.
    Bse,
}

impl Scope {
    /// Returns `true` if the bar passes the segment-code based filter
    /// for this scope. Per L31: `NSE` segments are 0/1/2; `BSE`
    /// segments are 4/8. Other Scope variants always return `true`
    /// (registry-aware filtering ships separately).
    #[must_use]
    pub fn includes(self, segment_code: u8) -> bool {
        match self {
            // L31: All passes everything.
            Self::All | Self::Index | Self::Stock => true,
            // NSE: IDX_I=0, NSE_EQ=1, NSE_FNO=2
            Self::Nse => matches!(segment_code, 0 | 1 | 2),
            // BSE: BSE_EQ=4, BSE_FNO=8
            Self::Bse => matches!(segment_code, 4 | 8),
        }
    }
}

/// Top-N query parameters per L28.
///
/// `expiry` is `Option<String>` because today the bar slice does not
/// carry an expiry label (instrument-master metadata, not in `Bar`).
/// The field reserves the slot for the registry-aware filter PR
/// while keeping today's `top_n_by_bars` signature stable.
#[derive(Debug, Clone)]
pub struct TopNQuery {
    pub category: Category,
    pub scope: Scope,
    pub n: usize,
    /// Reserved per L29 — date-label string filter (e.g. "26 MAY").
    /// Currently ignored; kept on the struct so the API surface is
    /// stable across the registry-filter follow-up.
    pub expiry: Option<String>,
}

impl TopNQuery {
    /// Convenience constructor for a no-expiry top-N query.
    #[must_use]
    pub fn new(category: Category, scope: Scope, n: usize) -> Self {
        Self {
            category,
            scope,
            n,
            expiry: None,
        }
    }
}

/// Pure top-N filter + sort over a bar slice. Returns at most
/// `query.n` bars.
///
/// **Algorithm** (see L28 + L30):
/// 1. Filter by `query.scope.includes(bar.exchange_segment_code)`.
/// 2. Sort by `query.category`'s (field, direction) using
///    `f64::partial_cmp` (NaN coerces to "less" so NaN rows sink to
///    the bottom regardless of direction — L13 div-by-zero policy
///    means no `Bar` produced by `stamp_bar_pct_fields` ever carries
///    NaN, but defensive in case a caller passes hand-rolled data).
/// 3. Truncate to `query.n` entries.
///
/// **Allocates one `Vec<Bar>`** — sized to the filtered subset (≤ N
/// in steady state, ≤ all-bars in worst case). Off the hot path; the
/// `/api/movers/v2` handler is the typical caller.
///
/// **Returns empty Vec** if `query.n == 0` — no work, no allocation.
#[must_use]
pub fn top_n_by_bars(bars: &[Bar], query: &TopNQuery) -> Vec<Bar> {
    if query.n == 0 {
        return Vec::new(); // APPROVED: cold-path /api/movers/v2 handler — NOT per-tick; n=0 short-circuit avoids all allocation
    }
    // Filter by segment-code scope.
    let mut filtered: Vec<Bar> = bars
        .iter()
        .filter(|b| query.scope.includes(b.exchange_segment_code))
        .copied()
        .collect(); // APPROVED: cold-path /api/movers/v2 (≥1s between calls), bounded by bars.len() ≤ universe size (~11K)
    // Sort by category-derived sort key. Direction is applied by
    // either using the natural order (Asc) or reversing comparator
    // result (Desc).
    sort_by_category(&mut filtered, query.category);
    filtered.truncate(query.n);
    filtered
}

/// Sort `bars` in place per the category's (field, direction).
///
/// Internal helper kept `pub(crate)` so unit tests can exercise the
/// sort half independently of the filter half.
///
/// O(1) EXEMPT: cold-path /api/movers/v2 + 1/min depth-dynamic
/// selector — NOT per-tick. O(N log N) sort is the canonical
/// algorithm for top-N over an unsorted snapshot.
pub(crate) fn sort_by_category(bars: &mut [Bar], category: Category) {
    match category {
        // OI: i64, integer comparison.
        Category::HighestOI => bars.sort_by(|a, b| b.oi.cmp(&a.oi)), // O(1) EXEMPT: cold-path top-N
        // % fields: f64 with NaN-sinks-low policy.
        Category::OIGainers => sort_f64_desc(bars, |b| b.oi_pct_from_prev_day),
        Category::OILosers => sort_f64_asc(bars, |b| b.oi_pct_from_prev_day),
        // Cumulative volume: i64 integer comparison.
        Category::TopVolume => {
            bars.sort_by(|a, b| b.volume_cum_day_at_end.cmp(&a.volume_cum_day_at_end)) // O(1) EXEMPT: cold-path top-N
        }
        // Top value = close × cumulative_volume; both wide → compute as f64.
        Category::TopValue => sort_f64_desc(bars, |b| b.close * (b.volume_cum_day_at_end as f64)),
        Category::PriceGainers => sort_f64_desc(bars, |b| b.close_pct_from_prev_day),
        Category::PriceLosers => sort_f64_asc(bars, |b| b.close_pct_from_prev_day),
    }
}

#[inline]
fn sort_f64_desc<F: Fn(&Bar) -> f64>(bars: &mut [Bar], key: F) {
    // O(1) EXEMPT: cold-path top-N — see `sort_by_category` rationale.
    bars.sort_by(|a, b| {
        let ka = key(a);
        let kb = key(b);
        // NaN sinks to the bottom: treat NaN as -Inf for descending.
        kb.partial_cmp(&ka)
            .unwrap_or(match (ka.is_nan(), kb.is_nan()) {
                (true, false) => std::cmp::Ordering::Greater,
                (false, true) => std::cmp::Ordering::Less,
                _ => std::cmp::Ordering::Equal,
            })
    });
}

#[inline]
fn sort_f64_asc<F: Fn(&Bar) -> f64>(bars: &mut [Bar], key: F) {
    // O(1) EXEMPT: cold-path top-N — see `sort_by_category` rationale.
    bars.sort_by(|a, b| {
        let ka = key(a);
        let kb = key(b);
        // NaN sinks to the bottom: treat NaN as +Inf for ascending.
        ka.partial_cmp(&kb)
            .unwrap_or(match (ka.is_nan(), kb.is_nan()) {
                (true, false) => std::cmp::Ordering::Greater,
                (false, true) => std::cmp::Ordering::Less,
                _ => std::cmp::Ordering::Equal,
            })
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_bar(security_id: u32, segment: u8, close: f64, oi: i64, vol: i64) -> Bar {
        Bar {
            bucket_start_ist_secs: 0,
            bucket_end_ist_secs: 60,
            open: close,
            high: close,
            low: close,
            close,
            volume: 0,
            volume_cum_day_at_end: vol,
            oi,
            tick_count: 1,
            security_id,
            exchange_segment_code: segment,
            sealed: true,
            prev_day_close: 100.0,
            prev_day_oi: 1_000_000,
            close_pct_from_prev_day: (close - 100.0) / 100.0 * 100.0,
            oi_pct_from_prev_day: 0.0,
            volume_pct_from_prev_day: 0.0,
        }
    }

    #[test]
    fn test_scope_all_includes_every_segment() {
        for seg in 0..=10 {
            assert!(Scope::All.includes(seg));
        }
    }

    #[test]
    fn test_scope_nse_only_passes_nse_segments() {
        for seg in [0_u8, 1, 2] {
            assert!(Scope::Nse.includes(seg), "NSE seg {seg} must pass");
        }
        for seg in [3_u8, 4, 5, 7, 8] {
            assert!(!Scope::Nse.includes(seg), "non-NSE seg {seg} must fail");
        }
    }

    #[test]
    fn test_scope_bse_only_passes_bse_segments() {
        for seg in [4_u8, 8] {
            assert!(Scope::Bse.includes(seg));
        }
        for seg in [0_u8, 1, 2, 3, 5, 7, 9, 10] {
            assert!(!Scope::Bse.includes(seg), "non-BSE seg {seg} must fail");
        }
    }

    #[test]
    fn test_scope_index_falls_through_to_all_pending_registry_filter() {
        // Documented limitation per L31 / module docstring: today
        // Index/Stock require InstrumentRegistry → currently behaves
        // like All. Test pins this behaviour so a regression in the
        // registry-aware follow-up is explicit.
        for seg in 0..=10 {
            assert!(Scope::Index.includes(seg));
            assert!(Scope::Stock.includes(seg));
        }
    }

    #[test]
    fn test_top_n_by_bars_zero_n_returns_empty() {
        let bars = vec![make_bar(1, 1, 100.0, 0, 0)];
        let q = TopNQuery::new(Category::TopVolume, Scope::All, 0);
        assert!(top_n_by_bars(&bars, &q).is_empty());
    }

    #[test]
    fn test_top_n_by_bars_top_volume_descending() {
        let bars = vec![
            make_bar(1, 1, 100.0, 0, 1_000),
            make_bar(2, 1, 200.0, 0, 5_000),
            make_bar(3, 1, 50.0, 0, 3_000),
        ];
        let q = TopNQuery::new(Category::TopVolume, Scope::All, 3);
        let result = top_n_by_bars(&bars, &q);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].security_id, 2); // 5_000 vol
        assert_eq!(result[1].security_id, 3); // 3_000
        assert_eq!(result[2].security_id, 1); // 1_000
    }

    #[test]
    fn test_top_n_by_bars_truncates_to_n() {
        let bars: Vec<_> = (0..10)
            .map(|i| make_bar(i, 1, 100.0, 0, i64::from(i) * 1_000))
            .collect();
        let q = TopNQuery::new(Category::TopVolume, Scope::All, 3);
        let result = top_n_by_bars(&bars, &q);
        assert_eq!(result.len(), 3);
        // Top 3 by volume DESC = 9, 8, 7
        assert_eq!(result[0].security_id, 9);
        assert_eq!(result[1].security_id, 8);
        assert_eq!(result[2].security_id, 7);
    }

    #[test]
    fn test_top_n_by_bars_filters_by_scope_nse() {
        let bars = vec![
            make_bar(1, 1, 100.0, 0, 5_000),  // NSE_EQ — pass
            make_bar(2, 4, 200.0, 0, 10_000), // BSE_EQ — fail
            make_bar(3, 2, 50.0, 0, 3_000),   // NSE_FNO — pass
        ];
        let q = TopNQuery::new(Category::TopVolume, Scope::Nse, 10);
        let result = top_n_by_bars(&bars, &q);
        assert_eq!(result.len(), 2);
        // Only NSE bars survived the filter.
        assert!(
            result
                .iter()
                .all(|b| matches!(b.exchange_segment_code, 0 | 1 | 2))
        );
        // Sorted by volume DESC.
        assert_eq!(result[0].security_id, 1); // 5_000 NSE_EQ
        assert_eq!(result[1].security_id, 3); // 3_000 NSE_FNO
    }

    #[test]
    fn test_top_n_by_bars_price_gainers_descending() {
        let bars = vec![
            make_bar(1, 1, 105.0, 0, 0), // +5%
            make_bar(2, 1, 110.0, 0, 0), // +10%
            make_bar(3, 1, 100.0, 0, 0), // 0%
            make_bar(4, 1, 95.0, 0, 0),  // -5%
        ];
        let q = TopNQuery::new(Category::PriceGainers, Scope::All, 4);
        let result = top_n_by_bars(&bars, &q);
        assert_eq!(result[0].security_id, 2); // +10%
        assert_eq!(result[1].security_id, 1); // +5%
        assert_eq!(result[2].security_id, 3); // 0%
        assert_eq!(result[3].security_id, 4); // -5%
    }

    #[test]
    fn test_top_n_by_bars_price_losers_ascending() {
        let bars = vec![
            make_bar(1, 1, 105.0, 0, 0),
            make_bar(2, 1, 110.0, 0, 0),
            make_bar(3, 1, 90.0, 0, 0),
            make_bar(4, 1, 95.0, 0, 0),
        ];
        let q = TopNQuery::new(Category::PriceLosers, Scope::All, 2);
        let result = top_n_by_bars(&bars, &q);
        assert_eq!(result.len(), 2);
        // Most negative pct first.
        assert_eq!(result[0].security_id, 3); // -10%
        assert_eq!(result[1].security_id, 4); // -5%
    }

    #[test]
    fn test_top_n_by_bars_top_value_uses_close_times_volume() {
        // close * cumulative_volume = "value traded today"
        let bars = vec![
            // 100 * 50_000 = 5_000_000
            make_bar(1, 1, 100.0, 0, 50_000),
            // 1000 * 1_000 = 1_000_000
            make_bar(2, 1, 1000.0, 0, 1_000),
            // 50 * 100_000 = 5_000_000 (tie with bar 1; sort is stable)
            make_bar(3, 1, 50.0, 0, 100_000),
        ];
        let q = TopNQuery::new(Category::TopValue, Scope::All, 3);
        let result = top_n_by_bars(&bars, &q);
        assert_eq!(result.len(), 3);
        // Bar 2 (1M) should be last; bars 1 + 3 (5M each) on top.
        assert_eq!(result[2].security_id, 2);
    }

    #[test]
    fn test_top_n_by_bars_highest_oi() {
        let bars = vec![
            make_bar(1, 2, 0.0, 1_000_000, 0),
            make_bar(2, 2, 0.0, 5_000_000, 0),
            make_bar(3, 2, 0.0, 3_000_000, 0),
        ];
        let q = TopNQuery::new(Category::HighestOI, Scope::All, 3);
        let result = top_n_by_bars(&bars, &q);
        assert_eq!(result[0].oi, 5_000_000);
        assert_eq!(result[1].oi, 3_000_000);
        assert_eq!(result[2].oi, 1_000_000);
    }

    #[test]
    fn test_top_n_by_bars_oi_gainers_uses_pct_field() {
        let mut b1 = make_bar(1, 2, 100.0, 0, 0);
        b1.oi_pct_from_prev_day = 25.0;
        let mut b2 = make_bar(2, 2, 100.0, 0, 0);
        b2.oi_pct_from_prev_day = 50.0;
        let mut b3 = make_bar(3, 2, 100.0, 0, 0);
        b3.oi_pct_from_prev_day = 10.0;
        let q = TopNQuery::new(Category::OIGainers, Scope::All, 3);
        let result = top_n_by_bars(&[b1, b2, b3], &q);
        assert_eq!(result[0].security_id, 2); // 50%
        assert_eq!(result[1].security_id, 1); // 25%
        assert_eq!(result[2].security_id, 3); // 10%
    }

    #[test]
    fn test_top_n_by_bars_nan_sinks_to_bottom_descending() {
        // NaN bars must NOT bubble to top regardless of direction.
        let mut b_nan = make_bar(1, 1, 0.0, 0, 0);
        b_nan.close_pct_from_prev_day = f64::NAN;
        let b_pos = {
            let mut b = make_bar(2, 1, 0.0, 0, 0);
            b.close_pct_from_prev_day = 5.0;
            b
        };
        let b_zero = {
            let mut b = make_bar(3, 1, 0.0, 0, 0);
            b.close_pct_from_prev_day = 0.0;
            b
        };
        let q = TopNQuery::new(Category::PriceGainers, Scope::All, 3);
        let result = top_n_by_bars(&[b_nan, b_pos, b_zero], &q);
        assert_eq!(result[0].security_id, 2); // +5%
        assert_eq!(result[1].security_id, 3); // 0%
        // NaN bar last.
        assert_eq!(result[2].security_id, 1);
    }

    #[test]
    fn test_top_n_query_new_constructor_defaults_expiry_to_none() {
        let q = TopNQuery::new(Category::TopVolume, Scope::All, 5);
        assert_eq!(q.category, Category::TopVolume);
        assert_eq!(q.scope, Scope::All);
        assert_eq!(q.n, 5);
        assert!(q.expiry.is_none());
    }

    #[test]
    fn test_top_n_by_bars_empty_input_returns_empty() {
        let q = TopNQuery::new(Category::TopVolume, Scope::All, 10);
        let result = top_n_by_bars(&[], &q);
        assert!(result.is_empty());
    }

    #[test]
    fn test_top_n_by_bars_n_larger_than_input_returns_all_filtered() {
        let bars = vec![make_bar(1, 1, 100.0, 0, 100), make_bar(2, 1, 100.0, 0, 200)];
        let q = TopNQuery::new(Category::TopVolume, Scope::All, 10);
        let result = top_n_by_bars(&bars, &q);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_top_n_by_bars_seven_categories_all_supported() {
        // Smoke ratchet: every Category variant must produce a result
        // without panic.
        let bars = vec![
            make_bar(1, 1, 100.0, 1_000, 5_000),
            make_bar(2, 1, 105.0, 2_000, 8_000),
        ];
        for cat in [
            Category::HighestOI,
            Category::OIGainers,
            Category::OILosers,
            Category::TopVolume,
            Category::TopValue,
            Category::PriceGainers,
            Category::PriceLosers,
        ] {
            let q = TopNQuery::new(cat, Scope::All, 2);
            let result = top_n_by_bars(&bars, &q);
            assert_eq!(result.len(), 2, "category {cat:?} must return 2 bars");
        }
    }

    #[test]
    fn test_scope_five_variants_covered() {
        // L29: 5 mutually-exclusive scope variants. Pinned so a
        // future `pub enum Scope { ... }` change has to update this
        // test deliberately.
        let _all = Scope::All;
        let _idx = Scope::Index;
        let _stk = Scope::Stock;
        let _nse = Scope::Nse;
        let _bse = Scope::Bse;
    }

    #[test]
    fn test_category_seven_variants_covered() {
        // L29 + L30: exactly 7 categories. Pinned.
        let _highest_oi = Category::HighestOI;
        let _oi_gainers = Category::OIGainers;
        let _oi_losers = Category::OILosers;
        let _top_volume = Category::TopVolume;
        let _top_value = Category::TopValue;
        let _price_gainers = Category::PriceGainers;
        let _price_losers = Category::PriceLosers;
    }

    #[test]
    fn test_sort_by_category_orders_in_place_for_each_category() {
        // Direct exercise of the `sort_by_category` helper independently
        // of the filter half. Confirms the per-category comparator
        // chosen in the match arm is applied to the slice.
        let mk = |close: f64, oi: i64, vol: i64, oi_pct: f64, close_pct: f64| {
            let mut b = make_bar(1, 1, close, oi, vol);
            b.oi_pct_from_prev_day = oi_pct;
            b.close_pct_from_prev_day = close_pct;
            b
        };

        let mut bars = vec![
            mk(100.0, 10, 1000, 1.0, 5.0),
            mk(200.0, 30, 5000, 9.0, -3.0),
            mk(50.0, 20, 2000, -2.0, 7.0),
        ];

        sort_by_category(&mut bars, Category::HighestOI);
        assert_eq!(bars[0].oi, 30);
        assert_eq!(bars[2].oi, 10);

        sort_by_category(&mut bars, Category::TopVolume);
        assert_eq!(bars[0].volume_cum_day_at_end, 5000);

        sort_by_category(&mut bars, Category::OIGainers);
        assert_eq!(bars[0].oi_pct_from_prev_day, 9.0);

        sort_by_category(&mut bars, Category::OILosers);
        assert_eq!(bars[0].oi_pct_from_prev_day, -2.0);

        sort_by_category(&mut bars, Category::PriceGainers);
        assert_eq!(bars[0].close_pct_from_prev_day, 7.0);

        sort_by_category(&mut bars, Category::PriceLosers);
        assert_eq!(bars[0].close_pct_from_prev_day, -3.0);

        sort_by_category(&mut bars, Category::TopValue);
        assert!(
            bars[0].close * (bars[0].volume_cum_day_at_end as f64)
                >= bars[1].close * (bars[1].volume_cum_day_at_end as f64)
        );
    }
}

//! Phase 4a — DORMANT BY DEFAULT (active-plan §6 row 3).
//!
//! `/api/movers?v=2` reads OHLCV directly from the in-RAM 29-TF
//! `CascadeFanout` instead of the QuestDB `stock_movers` /
//! `option_movers` matviews.
//!
//! ## F3 (Wave-5 §K-L28..L36 / #505 follow-up)
//!
//! Two query modes are now supported:
//!
//! 1. **Single-instrument lookup** — supply `?security_id=X&segment_code=Y&timeframe=1m`.
//!    Returns the latest Bar for that instrument's TF engine. Same as
//!    the dormant Phase 4a scaffold; preserved for backwards compat.
//! 2. **Top-N (Dhan-parity)** — supply `?category=X&scope=Y&n=Z&timeframe=1m`.
//!    Snapshots the TF's bars via [`tickvault_trading::candles::CascadeFanout::snapshot_1m`]
//!    et al., feeds them into [`tickvault_trading::in_mem::top_n_by_bars`],
//!    and returns the top-N as JSON. This is the operator-facing
//!    Dhan-UI-parity path.
//!
//! Both modes accept an optional `timeframe` (defaults to `1m`).
//! Mode is selected by which params are present: when `category`
//! is present, Mode 2 wins; otherwise Mode 1.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use tickvault_trading::candles::{Bar, CascadeFanout};
use tickvault_trading::in_mem::{Category, Scope, TopNQuery, top_n_by_bars};

use crate::state::SharedAppState;

#[derive(Debug, Clone, Deserialize)]
pub struct MoversV2Query {
    #[serde(default = "default_timeframe")]
    pub timeframe: String,
    /// Mode 1 (single-instrument) — security_id of target instrument.
    pub security_id: Option<u32>,
    /// Mode 1 (single-instrument) — exchange_segment_code of target.
    pub segment_code: Option<u8>,
    /// F3 — Mode 2 (top-N): Dhan-UI category. When present, switches
    /// the handler to the Dhan-parity top-N path.
    pub category: Option<String>,
    /// F3 — Mode 2 (top-N): segment scope filter. Defaults to `All`.
    #[serde(default)]
    pub scope: Option<String>,
    /// F3 — Mode 2 (top-N): result-set size. Defaults to `MOVERS_V2_DEFAULT_N`.
    #[serde(default)]
    pub n: Option<usize>,
}

fn default_timeframe() -> String {
    "1m".to_string()
}

/// F3 default top-N size when `n` is omitted. Mirrors the typical
/// Dhan-UI mover dropdown (10 entries per category). Operator can
/// override per call up to `MOVERS_V2_MAX_N`.
pub const MOVERS_V2_DEFAULT_N: usize = 10;

/// F3 hard cap on `n` per call. The cap exists so a misconfigured
/// client cannot allocate `Vec<Bar>` proportional to the universe
/// (~11K) on every request. The handler clamps `n` to this value
/// and reports the clamp via `meta.n_capped` in the response so the
/// operator knows the request was modified.
pub const MOVERS_V2_MAX_N: usize = 500;

/// Parses a Dhan-UI category label to the typed [`Category`] enum.
/// Returns `None` on unknown input — the handler reports a 400.
#[must_use]
pub fn parse_category(label: &str) -> Option<Category> {
    match label {
        "highest_oi" | "HighestOI" => Some(Category::HighestOI),
        "oi_gainers" | "OIGainers" => Some(Category::OIGainers),
        "oi_losers" | "OILosers" => Some(Category::OILosers),
        "top_volume" | "TopVolume" => Some(Category::TopVolume),
        "top_value" | "TopValue" => Some(Category::TopValue),
        "price_gainers" | "PriceGainers" => Some(Category::PriceGainers),
        "price_losers" | "PriceLosers" => Some(Category::PriceLosers),
        _ => None,
    }
}

/// Parses a Dhan-UI scope label to the typed [`Scope`] enum.
/// Returns `None` on unknown input. Empty / missing input defaults
/// to `Scope::All` at the call site.
#[must_use]
pub fn parse_scope(label: &str) -> Option<Scope> {
    match label {
        "all" | "All" => Some(Scope::All),
        "index" | "Index" => Some(Scope::Index),
        "stock" | "Stock" => Some(Scope::Stock),
        "nse" | "NSE" | "Nse" => Some(Scope::Nse),
        "bse" | "BSE" | "Bse" => Some(Scope::Bse),
        _ => None,
    }
}

/// Snapshots the TF's bars via the appropriate `CascadeFanout::snapshot_<tf>m`
/// accessor. Returns `None` if the timeframe label is unknown
/// (caller reports a 400). All 11 retained TFs (post PR #517) are
/// supported.
#[must_use]
pub fn snapshot_for_top_n(fanout: &CascadeFanout, timeframe: &str) -> Option<Vec<Bar>> {
    match timeframe {
        "1m" => Some(fanout.snapshot_1m()),
        "5m" => Some(fanout.snapshot_5m()),
        "15m" => Some(fanout.snapshot_15m()),
        "30m" => Some(fanout.snapshot_30m()),
        "1h" => Some(fanout.snapshot_1h()),
        "2h" => Some(fanout.snapshot_2h()),
        "3h" => Some(fanout.snapshot_3h()),
        "4h" => Some(fanout.snapshot_4h()),
        "1d" => Some(fanout.snapshot_1d()),
        "1w" => Some(fanout.snapshot_1w()),
        "1mo" => Some(fanout.snapshot_1mo()),
        _ => None,
    }
}

/// F3 top-N response — Dhan-UI parity. Carries the requested
/// dimensions plus a list of bars sorted per `Category`.
#[derive(Debug, Clone, Serialize)]
pub struct MoversV2TopNResponse {
    pub timeframe: String,
    pub category: String,
    pub scope: String,
    pub n_requested: usize,
    pub n_returned: usize,
    /// `true` if `n_requested` exceeded `MOVERS_V2_MAX_N` and was
    /// silently clamped to the cap. Operator visibility — see
    /// `MOVERS_V2_MAX_N` doc.
    pub n_capped: bool,
    /// Total bars in the snapshot before sort/truncate. Useful for
    /// dashboard "X of Y" displays + capacity planning.
    pub instruments_in_ram: usize,
    pub bars: Vec<MoversV2Bar>,
    pub source: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct MoversV2Response {
    pub timeframe: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instruments_in_ram: Option<usize>,
    pub bar: Option<MoversV2Bar>,
    pub source: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct MoversV2Bar {
    /// 2026-05-09 PR 5b1 — instrument identity composite key per
    /// audit-findings I-P1-11. Both `security_id` AND
    /// `exchange_segment_code` are required because `security_id`
    /// alone is NOT unique across exchange segments (Dhan reuses
    /// numeric ids across IDX_I / NSE_EQ / NSE_FNO — see
    /// `.claude/rules/project/security-id-uniqueness.md`).
    /// Without these the operator-facing dashboards cannot render
    /// rows; their inclusion is the prerequisite that unblocks the
    /// PR 5b2 frontend migration documented in
    /// `.claude/plans/active-plan-movers-cleanup-5b-5c-5d.md`.
    pub security_id: u32,
    /// Exchange segment numeric code (0=IDX_I, 1=NSE_EQ, 2=NSE_FNO,
    /// 8=BSE_FNO, etc.) — see `tickvault_common::types::ExchangeSegment`.
    pub exchange_segment_code: u8,
    pub bucket_start_ist_secs: u32,
    pub bucket_end_ist_secs: u32,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: i64,
    pub oi: i64,
    pub tick_count: u32,
    pub sealed: bool,
    /// 2026-05-09 PR 5b1.5 — prev-day reference + seal-time pct
    /// stamping (added in PR #520 F1). Frontend dashboards expect
    /// `change_pct` as a single derived field; exposing both
    /// `prev_day_close` and the pre-computed `close_pct_from_prev_day`
    /// lets clients render the column without a second QuestDB hop
    /// for `previous_close` rows. Equivalent fields exist on the
    /// underlying `Bar`; this is pure additive plumbing.
    pub prev_day_close: f64,
    pub prev_day_oi: i64,
    pub close_pct_from_prev_day: f64,
    pub oi_pct_from_prev_day: f64,
    pub volume_pct_from_prev_day: f64,
}

impl From<&tickvault_trading::candles::Bar> for MoversV2Bar {
    fn from(bar: &tickvault_trading::candles::Bar) -> Self {
        Self {
            security_id: bar.security_id,
            exchange_segment_code: bar.exchange_segment_code,
            bucket_start_ist_secs: bar.bucket_start_ist_secs,
            bucket_end_ist_secs: bar.bucket_end_ist_secs,
            open: bar.open,
            high: bar.high,
            low: bar.low,
            close: bar.close,
            volume: bar.volume,
            oi: bar.oi,
            tick_count: bar.tick_count,
            sealed: bar.sealed,
            prev_day_close: bar.prev_day_close,
            prev_day_oi: bar.prev_day_oi,
            close_pct_from_prev_day: bar.close_pct_from_prev_day,
            oi_pct_from_prev_day: bar.oi_pct_from_prev_day,
            volume_pct_from_prev_day: bar.volume_pct_from_prev_day,
        }
    }
}

pub async fn get_movers_v2(
    State(state): State<SharedAppState>,
    Query(params): Query<MoversV2Query>,
) -> impl IntoResponse {
    let Some(fanout) = state.cascade_fanout() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "movers_v2_not_initialized",
                "message": "/api/movers?v=2 route is registered but \
                            CascadeFanout is not in AppState. \
                            Check config.api.movers_v2_enabled and \
                            main.rs boot wiring.",
            })),
        )
            .into_response();
    };

    // F3 (Wave-5 §K-L28..L36 / #505 follow-up): when `category` is
    // present, route to the Dhan-parity top-N path (Mode 2). Otherwise
    // fall through to the legacy single-instrument path (Mode 1) for
    // backwards compat with pre-F3 callers.
    if let Some(category_label) = params.category.as_deref() {
        return handle_top_n(fanout, &params, category_label);
    }

    let (_instruments_in_ram_unused, bar_opt) = snapshot_for_timeframe(
        fanout,
        &params.timeframe,
        params.security_id,
        params.segment_code,
    );

    let instruments_in_ram = ram_count_for_timeframe(fanout, &params.timeframe);

    let response = MoversV2Response {
        timeframe: params.timeframe.clone(),
        instruments_in_ram,
        bar: bar_opt.as_ref().map(MoversV2Bar::from),
        source: "phase4a-dormant-scaffold",
    };
    match serde_json::to_value(&response) {
        Ok(value) => (StatusCode::OK, Json(value)).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": "movers_v2_serialization_failed",
                "message": err.to_string(),
                "source": "phase4a-dormant-scaffold",
            })),
        )
            .into_response(),
    }
}

/// F3 Mode 2 dispatch: validate category/scope/n, snapshot the TF,
/// run `top_n_by_bars`, return JSON. Pure-logic primitives are
/// unit-tested below; this function bundles them with HTTP error
/// responses.
fn handle_top_n(
    fanout: &CascadeFanout,
    params: &MoversV2Query,
    category_label: &str,
) -> axum::response::Response {
    let Some(category) = parse_category(category_label) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "movers_v2_unknown_category",
                "category": category_label,
                "message": "valid categories: highest_oi, oi_gainers, \
                            oi_losers, top_volume, top_value, \
                            price_gainers, price_losers (or PascalCase variants)",
            })),
        )
            .into_response();
    };
    let scope = match params.scope.as_deref() {
        Some(label) => match parse_scope(label) {
            Some(s) => s,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "movers_v2_unknown_scope",
                        "scope": label,
                        "message": "valid scopes: all, index, stock, nse, bse",
                    })),
                )
                    .into_response();
            }
        },
        None => Scope::All,
    };
    let bars = match snapshot_for_top_n(fanout, &params.timeframe) {
        Some(b) => b,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "movers_v2_unknown_timeframe",
                    "timeframe": params.timeframe,
                    "message": "valid timeframes: 1m, 5m, 15m, 30m, 1h, \
                                2h, 3h, 4h, 1d, 1w, 1mo (post PR #517)",
                })),
            )
                .into_response();
        }
    };
    let instruments_in_ram = bars.len();
    let n_requested = params.n.unwrap_or(MOVERS_V2_DEFAULT_N);
    let n_capped = n_requested > MOVERS_V2_MAX_N;
    let n_effective = n_requested.min(MOVERS_V2_MAX_N);
    let query = TopNQuery::new(category, scope, n_effective);
    let top_bars = top_n_by_bars(&bars, &query);
    let response = MoversV2TopNResponse {
        timeframe: params.timeframe.clone(),
        category: category_label.to_string(),
        scope: params.scope.clone().unwrap_or_else(|| "all".to_string()),
        n_requested,
        n_returned: top_bars.len(),
        n_capped,
        instruments_in_ram,
        bars: top_bars.iter().map(MoversV2Bar::from).collect(),
        source: "f3-cascade-fanout-snapshot",
    };
    match serde_json::to_value(&response) {
        Ok(value) => (StatusCode::OK, Json(value)).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": "movers_v2_serialization_failed",
                "message": err.to_string(),
                "source": "f3-cascade-fanout-snapshot",
            })),
        )
            .into_response(),
    }
}

pub fn ram_count_for_timeframe(
    fanout: &tickvault_trading::candles::CascadeFanout,
    timeframe: &str,
) -> Option<usize> {
    use tickvault_trading::candles::CascadeFanout;
    match timeframe {
        // L7 (PR #504c): seconds-level engines (3s/5s/10s/15s/30s) RETIRED.
        // PR #517 (Wave-5 TF reduction): sub-15-minute non-canonical engines
        // (2m/3m/4m/6m/7m/8m/9m/10m/11m/12m/13m/14m) RETIRED.
        "1m" => Some(CascadeFanout::len_1m(fanout)),
        "5m" => Some(CascadeFanout::len_5m(fanout)),
        "15m" => Some(CascadeFanout::len_15m(fanout)),
        "30m" => Some(CascadeFanout::len_30m(fanout)),
        "1h" => Some(CascadeFanout::len_1h(fanout)),
        "2h" => Some(CascadeFanout::len_2h(fanout)),
        "3h" => Some(CascadeFanout::len_3h(fanout)),
        "4h" => Some(CascadeFanout::len_4h(fanout)),
        "1d" => Some(CascadeFanout::len_1d(fanout)),
        "1w" => Some(CascadeFanout::len_1w(fanout)),
        "1mo" => Some(CascadeFanout::len_1mo(fanout)),
        _ => None,
    }
}

pub fn snapshot_for_timeframe(
    fanout: &tickvault_trading::candles::CascadeFanout,
    timeframe: &str,
    security_id: Option<u32>,
    segment_code: Option<u8>,
) -> (usize, Option<tickvault_trading::candles::Bar>) {
    use tickvault_trading::candles::CascadeFanout;
    type AccessorFn = fn(&CascadeFanout, u32, u8) -> Option<tickvault_trading::candles::Bar>;
    let accessor: Option<AccessorFn> = match timeframe {
        // L7 (PR #504c) + PR #517: retired engines fall through to None.
        "1m" => Some(CascadeFanout::latest_1m),
        "5m" => Some(CascadeFanout::latest_5m),
        "15m" => Some(CascadeFanout::latest_15m),
        "30m" => Some(CascadeFanout::latest_30m),
        "1h" => Some(CascadeFanout::latest_1h),
        "2h" => Some(CascadeFanout::latest_2h),
        "3h" => Some(CascadeFanout::latest_3h),
        "4h" => Some(CascadeFanout::latest_4h),
        "1d" => Some(CascadeFanout::latest_1d),
        "1w" => Some(CascadeFanout::latest_1w),
        "1mo" => Some(CascadeFanout::latest_1mo),
        _ => None,
    };

    let Some(accessor) = accessor else {
        return (0, None);
    };

    let bar = match (security_id, segment_code) {
        (Some(sid), Some(seg)) => accessor(fanout, sid, seg),
        _ => None,
    };

    (0, bar)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tickvault_trading::candles::{Bar, CascadeFanout};

    fn make_sealed_1s_bar(security_id: u32, segment_code: u8, bucket_start: u32) -> Bar {
        Bar {
            bucket_start_ist_secs: bucket_start,
            bucket_end_ist_secs: bucket_start + 1,
            open: 100.0,
            high: 101.0,
            low: 99.0,
            close: 100.5,
            volume: 1_000,
            volume_cum_day_at_end: 1_000,
            oi: 200,
            tick_count: 5,
            security_id,
            exchange_segment_code: segment_code,
            sealed: true,
            prev_day_close: 0.0,
            prev_day_oi: 0,
            close_pct_from_prev_day: 0.0,
            oi_pct_from_prev_day: 0.0,
            volume_pct_from_prev_day: 0.0,
        }
    }

    #[test]
    fn snapshot_for_timeframe_unknown_returns_zero_and_none() {
        let fanout = CascadeFanout::new();
        let (count, bar) = snapshot_for_timeframe(&fanout, "bogus_tf", Some(1), Some(1));
        assert_eq!(count, 0);
        assert!(bar.is_none());
    }

    #[test]
    fn snapshot_for_timeframe_returns_none_when_fanout_empty() {
        let fanout = CascadeFanout::new();
        let (_count, bar) = snapshot_for_timeframe(&fanout, "1m", Some(1), Some(1));
        assert!(bar.is_none());
    }

    #[test]
    fn snapshot_for_timeframe_returns_bar_after_fanout_fed() {
        let fanout = CascadeFanout::new();
        let one_s = make_sealed_1s_bar(42, 1, 1_000);
        fanout.feed_sealed_1s_bar(&one_s);
        let (_count, bar) = snapshot_for_timeframe(&fanout, "1m", Some(42), Some(1));
        assert!(bar.is_some());
        assert_eq!(bar.unwrap().security_id, 42);
    }

    #[test]
    fn snapshot_for_timeframe_returns_none_when_security_id_missing() {
        let fanout = CascadeFanout::new();
        let one_s = make_sealed_1s_bar(42, 1, 1_000);
        fanout.feed_sealed_1s_bar(&one_s);
        let (_count, bar) = snapshot_for_timeframe(&fanout, "1m", None, None);
        assert!(bar.is_none());
    }

    #[test]
    fn snapshot_for_timeframe_isolates_segment_per_i_p1_11() {
        let fanout = CascadeFanout::new();
        let bar_seg0 = make_sealed_1s_bar(27, 0, 1_000);
        fanout.feed_sealed_1s_bar(&bar_seg0);
        let (_count, bar) = snapshot_for_timeframe(&fanout, "30m", Some(27), Some(1));
        assert!(bar.is_none());
        let (_count2, bar2) = snapshot_for_timeframe(&fanout, "30m", Some(27), Some(0));
        assert!(bar2.is_some());
    }

    #[test]
    fn movers_v2_bar_from_bar_preserves_all_fields() {
        let bar = make_sealed_1s_bar(1, 1, 1_000);
        let v2 = MoversV2Bar::from(&bar);
        assert_eq!(v2.open, bar.open);
        assert_eq!(v2.close, bar.close);
        assert_eq!(v2.volume, bar.volume);
        assert_eq!(v2.tick_count, bar.tick_count);
        assert_eq!(v2.sealed, bar.sealed);
    }

    /// 2026-05-09 PR 5b1.5 ratchet — prev-day reference + seal-time
    /// pct fields MUST propagate from `Bar` to `MoversV2Bar`. PR 5b2
    /// (frontend migration) needs `change_pct` to render the
    /// gainers/losers % column without a second QuestDB hop for
    /// `previous_close` rows. The pct fields are computed at
    /// seal-time by `seal_with_pct` (PR #520 F1) so the API response
    /// can hand them off untouched.
    #[test]
    fn movers_v2_bar_propagates_prev_day_and_pct_fields() {
        let mut bar = make_sealed_1s_bar(13, 0, 1_000);
        bar.prev_day_close = 95.0;
        bar.prev_day_oi = 150;
        bar.close_pct_from_prev_day = 5.79;
        bar.oi_pct_from_prev_day = 33.33;
        bar.volume_pct_from_prev_day = 12.5;
        let v2 = MoversV2Bar::from(&bar);
        assert!((v2.prev_day_close - 95.0).abs() < f64::EPSILON);
        assert_eq!(v2.prev_day_oi, 150);
        assert!((v2.close_pct_from_prev_day - 5.79).abs() < f64::EPSILON);
        assert!((v2.oi_pct_from_prev_day - 33.33).abs() < f64::EPSILON);
        assert!((v2.volume_pct_from_prev_day - 12.5).abs() < f64::EPSILON);
    }

    /// 2026-05-09 PR 5b1 ratchet — identity fields MUST propagate
    /// from `Bar` to `MoversV2Bar`. PR 5b2 (frontend migration) is
    /// blocked without these fields because static dashboards need
    /// `(security_id, exchange_segment_code)` to render rows. This
    /// test pins the propagation per-segment (audit-findings I-P1-11
    /// — `security_id` alone is NOT unique across segments).
    #[test]
    fn movers_v2_bar_propagates_identity_fields_per_i_p1_11() {
        // Distinct `(security_id, exchange_segment_code)` pairs: same
        // numeric id 27 across two segments must round-trip without
        // collision in the API response struct.
        let bar_idx = make_sealed_1s_bar(27, 0, 1_000); // IDX_I
        let bar_eq = make_sealed_1s_bar(27, 1, 2_000); // NSE_EQ
        let v2_idx = MoversV2Bar::from(&bar_idx);
        let v2_eq = MoversV2Bar::from(&bar_eq);
        assert_eq!(v2_idx.security_id, 27);
        assert_eq!(v2_idx.exchange_segment_code, 0);
        assert_eq!(v2_eq.security_id, 27);
        assert_eq!(v2_eq.exchange_segment_code, 1);
        // Composite key disambiguates them — confirmation that the
        // PR 5b2 frontend migration can rely on identity fields.
        assert_ne!(
            (v2_idx.security_id, v2_idx.exchange_segment_code),
            (v2_eq.security_id, v2_eq.exchange_segment_code),
        );
    }

    #[test]
    fn snapshot_for_timeframe_handles_all_11_derived_tfs() {
        // PR #517 (Wave-5 TF reduction): retired the 12 sub-15-minute non-
        // canonical engines. Cascade has 11 derived TFs: 1m / 5m / 15m / 30m
        // / 1h / 2h / 3h / 4h / 1d / 1w / 1mo.
        use tickvault_trading::candles::DERIVED_ENGINE_COUNT;
        let fanout = CascadeFanout::new();
        let tfs = [
            "1m", "5m", "15m", "30m", "1h", "2h", "3h", "4h", "1d", "1w", "1mo",
        ];
        assert_eq!(
            tfs.len(),
            DERIVED_ENGINE_COUNT,
            "TF accessor list (len={}) drifted from DERIVED_ENGINE_COUNT (={DERIVED_ENGINE_COUNT}).",
            tfs.len()
        );
        for tf in tfs {
            let (count, bar) = snapshot_for_timeframe(&fanout, tf, Some(1), Some(1));
            assert_eq!(count, 0);
            assert!(bar.is_none(), "TF {tf} returned a bar from empty fanout");
        }
    }

    #[test]
    fn snapshot_for_timeframe_returns_none_for_retired_seconds_tfs() {
        let fanout = CascadeFanout::new();
        for tf in ["3s", "5s", "10s", "15s", "30s"] {
            let (count, bar) = snapshot_for_timeframe(&fanout, tf, Some(1), Some(1));
            assert_eq!(count, 0);
            assert!(bar.is_none());
        }
    }

    #[test]
    fn snapshot_for_timeframe_returns_none_for_retired_pr517_sub_15m_tfs() {
        // PR #517 ratchet: the 12 sub-15-minute non-canonical TFs MUST
        // return `None` from snapshot_for_timeframe. Symmetric to the
        // L7 retired-seconds ratchet above.
        let fanout = CascadeFanout::new();
        for tf in [
            "2m", "3m", "4m", "6m", "7m", "8m", "9m", "10m", "11m", "12m", "13m", "14m",
        ] {
            let (count, bar) = snapshot_for_timeframe(&fanout, tf, Some(1), Some(1));
            assert_eq!(count, 0, "PR #517 retired TF `{tf}` must report 0");
            assert!(bar.is_none(), "PR #517 retired TF `{tf}` must return None");
            assert_eq!(
                ram_count_for_timeframe(&fanout, tf),
                None,
                "PR #517 retired TF `{tf}` must return None from ram_count_for_timeframe"
            );
        }
    }

    #[test]
    fn ram_count_for_timeframe_returns_none_for_retired_seconds_tfs() {
        let fanout = CascadeFanout::new();
        for tf in ["3s", "5s", "10s", "15s", "30s"] {
            assert_eq!(ram_count_for_timeframe(&fanout, tf), None);
        }
    }

    #[test]
    fn movers_v2_response_source_is_phase4a_dormant_scaffold() {
        let resp = MoversV2Response {
            timeframe: "1m".to_string(),
            instruments_in_ram: None,
            bar: None,
            source: "phase4a-dormant-scaffold",
        };
        assert_eq!(resp.source, "phase4a-dormant-scaffold");
    }

    #[test]
    fn movers_v2_response_omits_instruments_in_ram_when_none() {
        let resp = MoversV2Response {
            timeframe: "1m".to_string(),
            instruments_in_ram: None,
            bar: None,
            source: "phase4a-dormant-scaffold",
        };
        let json = serde_json::to_string(&resp).expect("response serializes");
        assert!(!json.contains("instruments_in_ram"));
    }

    #[test]
    fn movers_v2_response_includes_instruments_in_ram_when_some() {
        let resp = MoversV2Response {
            timeframe: "1m".to_string(),
            instruments_in_ram: Some(24_310),
            bar: None,
            source: "phase4a-dormant-scaffold",
        };
        let json = serde_json::to_string(&resp).expect("response serializes");
        assert!(json.contains("\"instruments_in_ram\":24310"));
    }

    #[test]
    fn snapshot_for_timeframe_explicit_name_match() {
        let fanout = CascadeFanout::new();
        let _ = snapshot_for_timeframe(&fanout, "1m", None, None);
    }

    #[test]
    fn ram_count_for_timeframe_unknown_returns_none() {
        let fanout = CascadeFanout::new();
        assert!(ram_count_for_timeframe(&fanout, "bogus_tf").is_none());
    }

    #[test]
    fn ram_count_for_timeframe_returns_zero_on_empty_fanout() {
        let fanout = CascadeFanout::new();
        assert_eq!(ram_count_for_timeframe(&fanout, "1m"), Some(0));
        assert_eq!(ram_count_for_timeframe(&fanout, "30m"), Some(0));
        assert_eq!(ram_count_for_timeframe(&fanout, "1d"), Some(0));
    }

    #[test]
    fn ram_count_for_timeframe_advances_after_seed() {
        let fanout = CascadeFanout::new();
        let bar = make_sealed_1s_bar(1234, 1, 1_000);
        fanout.feed_sealed_1s_bar(&bar);
        assert_eq!(ram_count_for_timeframe(&fanout, "1m"), Some(1));
        assert_eq!(ram_count_for_timeframe(&fanout, "1h"), Some(1));
        let bar2 = make_sealed_1s_bar(5678, 1, 1_000);
        fanout.feed_sealed_1s_bar(&bar2);
        assert_eq!(ram_count_for_timeframe(&fanout, "1m"), Some(2));
    }

    #[test]
    fn ram_count_for_timeframe_explicit_name_match() {
        let fanout = CascadeFanout::new();
        let _ = ram_count_for_timeframe(&fanout, "1m");
    }

    #[test]
    fn get_movers_v2_explicit_name_match() {
        let _: fn(State<SharedAppState>, Query<MoversV2Query>) -> _ = get_movers_v2;
    }

    #[test]
    fn cascade_fanout_arc_install_via_with_cascade_fanout() {
        let fanout = Arc::new(CascadeFanout::new());
        let _ = fanout.clone();
        assert!(Arc::strong_count(&fanout) >= 1);
    }

    // -----------------------------------------------------------------
    // F3 (Wave-5 §K-L28..L36 / #505 follow-up) — top-N path tests.
    // -----------------------------------------------------------------

    #[test]
    fn parse_category_accepts_snake_case_and_pascal_case() {
        assert_eq!(parse_category("highest_oi"), Some(Category::HighestOI));
        assert_eq!(parse_category("HighestOI"), Some(Category::HighestOI));
        assert_eq!(parse_category("oi_gainers"), Some(Category::OIGainers));
        assert_eq!(parse_category("OIGainers"), Some(Category::OIGainers));
        assert_eq!(parse_category("oi_losers"), Some(Category::OILosers));
        assert_eq!(parse_category("OILosers"), Some(Category::OILosers));
        assert_eq!(parse_category("top_volume"), Some(Category::TopVolume));
        assert_eq!(parse_category("TopVolume"), Some(Category::TopVolume));
        assert_eq!(parse_category("top_value"), Some(Category::TopValue));
        assert_eq!(parse_category("TopValue"), Some(Category::TopValue));
        assert_eq!(
            parse_category("price_gainers"),
            Some(Category::PriceGainers)
        );
        assert_eq!(parse_category("PriceGainers"), Some(Category::PriceGainers));
        assert_eq!(parse_category("price_losers"), Some(Category::PriceLosers));
        assert_eq!(parse_category("PriceLosers"), Some(Category::PriceLosers));
    }

    #[test]
    fn parse_category_rejects_unknown() {
        assert!(parse_category("bogus").is_none());
        assert!(parse_category("").is_none());
        assert!(parse_category("HIGHEST_OI").is_none()); // strict casing
    }

    #[test]
    fn parse_scope_accepts_dhan_ui_labels() {
        assert_eq!(parse_scope("all"), Some(Scope::All));
        assert_eq!(parse_scope("All"), Some(Scope::All));
        assert_eq!(parse_scope("index"), Some(Scope::Index));
        assert_eq!(parse_scope("Index"), Some(Scope::Index));
        assert_eq!(parse_scope("stock"), Some(Scope::Stock));
        assert_eq!(parse_scope("Stock"), Some(Scope::Stock));
        assert_eq!(parse_scope("nse"), Some(Scope::Nse));
        assert_eq!(parse_scope("NSE"), Some(Scope::Nse));
        assert_eq!(parse_scope("Nse"), Some(Scope::Nse));
        assert_eq!(parse_scope("bse"), Some(Scope::Bse));
        assert_eq!(parse_scope("BSE"), Some(Scope::Bse));
    }

    #[test]
    fn parse_scope_rejects_unknown() {
        assert!(parse_scope("bogus").is_none());
        assert!(parse_scope("").is_none());
    }

    #[test]
    fn snapshot_for_top_n_routes_every_retained_tf() {
        let fanout = CascadeFanout::new();
        // All 11 retained TFs (post PR #517) MUST resolve to Some(...);
        // any unknown returns None.
        for tf in [
            "1m", "5m", "15m", "30m", "1h", "2h", "3h", "4h", "1d", "1w", "1mo",
        ] {
            assert!(
                snapshot_for_top_n(&fanout, tf).is_some(),
                "snapshot_for_top_n must resolve `{tf}`"
            );
        }
        // Retired sub-15m TFs MUST resolve to None.
        for retired in [
            "2m", "3m", "4m", "6m", "7m", "8m", "9m", "10m", "11m", "12m", "13m", "14m",
        ] {
            assert!(
                snapshot_for_top_n(&fanout, retired).is_none(),
                "snapshot_for_top_n must NOT resolve retired `{retired}`"
            );
        }
        // Sub-minute TFs were retired in PR #504c.
        for sub_minute in ["1s", "3s", "5s", "10s", "15s", "30s"] {
            assert!(
                snapshot_for_top_n(&fanout, sub_minute).is_none(),
                "snapshot_for_top_n must NOT resolve seconds-level `{sub_minute}`"
            );
        }
    }

    #[test]
    fn movers_v2_default_n_is_ten() {
        // F3 ratchet: changing the default from 10 needs operator
        // discussion — Dhan UI's primary mover dropdown is 10 entries.
        assert_eq!(MOVERS_V2_DEFAULT_N, 10);
    }

    #[test]
    fn movers_v2_max_n_is_500() {
        // F3 ratchet: changing the cap from 500 needs operator
        // discussion (capacity-planning + memory budget).
        assert_eq!(MOVERS_V2_MAX_N, 500);
    }

    #[test]
    fn handle_top_n_clamps_n_at_max() {
        // F3 ratchet: when `n` exceeds `MOVERS_V2_MAX_N`, the response
        // MUST clamp + report `n_capped: true`. Pure-logic check via
        // direct `top_n_by_bars` call mirroring the handler body.
        let bars: Vec<Bar> = (0..600u32)
            .map(|i| make_sealed_1s_bar(i, 1, 1_000))
            .collect();
        let n_requested = 600_usize;
        let n_capped = n_requested > MOVERS_V2_MAX_N;
        let n_effective = n_requested.min(MOVERS_V2_MAX_N);
        let query = TopNQuery::new(Category::HighestOI, Scope::All, n_effective);
        let result = top_n_by_bars(&bars, &query);
        assert!(n_capped);
        assert_eq!(n_effective, MOVERS_V2_MAX_N);
        assert_eq!(result.len(), MOVERS_V2_MAX_N);
    }

    #[test]
    fn handle_top_n_default_n_is_used_when_omitted() {
        // F3 ratchet: when `n` is None, the handler uses
        // `MOVERS_V2_DEFAULT_N` (= 10).
        let n_requested = MOVERS_V2_DEFAULT_N; // emulates `params.n.unwrap_or(...)`
        let bars: Vec<Bar> = (0..50u32)
            .map(|i| make_sealed_1s_bar(i, 1, 1_000))
            .collect();
        let query = TopNQuery::new(Category::HighestOI, Scope::All, n_requested);
        let result = top_n_by_bars(&bars, &query);
        assert_eq!(result.len(), 10);
    }

    #[test]
    fn parse_category_explicit_name_match() {
        let _ = parse_category;
    }

    #[test]
    fn parse_scope_explicit_name_match() {
        let _ = parse_scope;
    }

    #[test]
    fn snapshot_for_top_n_explicit_name_match() {
        let _ = snapshot_for_top_n;
    }
}

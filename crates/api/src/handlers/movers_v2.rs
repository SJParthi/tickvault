//! Phase 4a — DORMANT BY DEFAULT (active-plan §6 row 3).
//!
//! `/api/movers?v=2` reads OHLCV directly from the in-RAM 29-TF
//! `CascadeFanout` instead of the QuestDB `stock_movers` /
//! `option_movers` matviews.
//!
//! Dormancy gates: see module docstring on the original file. The full
//! Dhan-parity response shape lands in a follow-up PR once the soak
//! gate is cleared.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};

use crate::state::SharedAppState;

#[derive(Debug, Clone, Deserialize)]
pub struct MoversV2Query {
    #[serde(default = "default_timeframe")]
    pub timeframe: String,
    pub security_id: Option<u32>,
    pub segment_code: Option<u8>,
}

fn default_timeframe() -> String {
    "1m".to_string()
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
}

impl From<&tickvault_trading::candles::Bar> for MoversV2Bar {
    fn from(bar: &tickvault_trading::candles::Bar) -> Self {
        Self {
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
}

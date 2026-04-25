//! Plan item D1 (2026-04-22) — unified `/api/movers` handler backed by the
//! 6-bucket `MoversTrackerV2` snapshot.
//!
//! Supersedes `/api/top-movers` (stocks + indices only) and
//! `/api/option-movers` (mixed NSE_FNO). Those remain live as back-compat
//! shims (plan item D2) until front-end consumers migrate.
//!
//! # Query parameters
//!
//! - `bucket` — optional filter: `indices` | `stocks` | `index_futures` |
//!   `stock_futures` | `index_options` | `stock_options`. Default = all six.
//! - `category` — optional filter: `gainers` | `losers` | `most_active` |
//!   `top_oi` | `oi_buildup` | `oi_unwind` | `top_value`. Default = all
//!   applicable for the requested bucket (3 for price buckets, 7 for
//!   derivative buckets).
//! - `top` — integer 1..=20 (default 20). Trims each returned list.
//!
//! # Validation rules
//!
//! - Invalid `bucket` / `category` values → 400 with the list of valid values.
//! - Requesting a derivative-only category (`top_oi`, `oi_buildup`,
//!   `oi_unwind`, `top_value`) on a price bucket (`indices`, `stocks`) →
//!   405 Method Not Allowed.
//! - `top=0` or `top>20` → 400.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use tickvault_core::pipeline::mover_classifier::{MoverBucket, MoverCategory};
use tickvault_core::pipeline::top_movers::{DerivativeBucket, MoverEntryV2, PriceBucket};

use crate::state::SharedAppState;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum `top` value allowed. Mirrors `MOVERS_V2_TOP_N` in the tracker.
const MAX_TOP_N: usize = 20;

// ---------------------------------------------------------------------------
// Query parameters
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct MoversQuery {
    #[serde(default)]
    pub bucket: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub top: Option<usize>,
}

// ---------------------------------------------------------------------------
// Response shape
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Default)]
pub struct MoversV2Response {
    /// IST time the snapshot was emitted (epoch seconds). `0` when no
    /// snapshot is available yet.
    pub snapshot_at_ist_secs: i64,
    /// `true` when the tracker has produced at least one snapshot.
    pub available: bool,
    /// Map keyed by bucket wire-string (see `MoverBucket::as_str()`).
    /// Each value is a [`BucketResponse`] containing up to 7 category arrays.
    pub buckets: std::collections::BTreeMap<String, BucketResponse>,
    /// Total instruments currently tracked across all buckets.
    pub total_tracked: usize,
}

#[derive(Debug, Serialize, Default)]
pub struct BucketResponse {
    pub tracked: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gainers: Option<Vec<MoverEntryV2>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub losers: Option<Vec<MoverEntryV2>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub most_active: Option<Vec<MoverEntryV2>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_oi: Option<Vec<MoverEntryV2>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oi_buildup: Option<Vec<MoverEntryV2>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oi_unwind: Option<Vec<MoverEntryV2>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_value: Option<Vec<MoverEntryV2>>,
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

fn parse_bucket(s: &str) -> Option<MoverBucket> {
    match s {
        "indices" => Some(MoverBucket::Indices),
        "stocks" => Some(MoverBucket::Stocks),
        "index_futures" => Some(MoverBucket::IndexFutures),
        "stock_futures" => Some(MoverBucket::StockFutures),
        "index_options" => Some(MoverBucket::IndexOptions),
        "stock_options" => Some(MoverBucket::StockOptions),
        _ => None,
    }
}

fn parse_category(s: &str) -> Option<MoverCategory> {
    match s {
        "gainers" => Some(MoverCategory::Gainers),
        "losers" => Some(MoverCategory::Losers),
        "most_active" => Some(MoverCategory::MostActive),
        "top_oi" => Some(MoverCategory::TopOi),
        "oi_buildup" => Some(MoverCategory::OiBuildup),
        "oi_unwind" => Some(MoverCategory::OiUnwind),
        "top_value" => Some(MoverCategory::TopValue),
        _ => None,
    }
}

fn valid_buckets() -> Vec<String> {
    MoverBucket::all()
        .iter()
        .map(|b| b.as_str().to_string())
        .collect()
}

fn valid_categories() -> Vec<String> {
    MoverCategory::all()
        .iter()
        .map(|c| c.as_str().to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Trim + project helpers
// ---------------------------------------------------------------------------

fn trim(list: &[MoverEntryV2], top: usize) -> Vec<MoverEntryV2> {
    list.iter().take(top).copied().collect()
}

fn project_price(
    bucket: &PriceBucket,
    top: usize,
    only_category: Option<MoverCategory>,
) -> BucketResponse {
    let include = |cat: MoverCategory| only_category.is_none_or(|filter| filter == cat);
    BucketResponse {
        tracked: bucket.tracked,
        gainers: include(MoverCategory::Gainers).then(|| trim(&bucket.gainers, top)),
        losers: include(MoverCategory::Losers).then(|| trim(&bucket.losers, top)),
        most_active: include(MoverCategory::MostActive).then(|| trim(&bucket.most_active, top)),
        top_oi: None,
        oi_buildup: None,
        oi_unwind: None,
        top_value: None,
    }
}

fn project_derivative(
    bucket: &DerivativeBucket,
    top: usize,
    only_category: Option<MoverCategory>,
) -> BucketResponse {
    let include = |cat: MoverCategory| only_category.is_none_or(|filter| filter == cat);
    BucketResponse {
        tracked: bucket.tracked,
        gainers: include(MoverCategory::Gainers).then(|| trim(&bucket.gainers, top)),
        losers: include(MoverCategory::Losers).then(|| trim(&bucket.losers, top)),
        most_active: include(MoverCategory::MostActive).then(|| trim(&bucket.most_active, top)),
        top_oi: include(MoverCategory::TopOi).then(|| trim(&bucket.top_oi, top)),
        oi_buildup: include(MoverCategory::OiBuildup).then(|| trim(&bucket.oi_buildup, top)),
        oi_unwind: include(MoverCategory::OiUnwind).then(|| trim(&bucket.oi_unwind, top)),
        top_value: include(MoverCategory::TopValue).then(|| trim(&bucket.top_value, top)),
    }
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// `GET /api/movers` — returns the latest V2 movers snapshot.
// TEST-EXEMPT: covered by 6 handler tests — test_handler_invalid_bucket_returns_400_with_valid_list et al
pub async fn get_movers_v2(
    State(state): State<SharedAppState>,
    Query(params): Query<MoversQuery>,
) -> Response {
    // ---- Validate `bucket` ----
    let bucket_filter = match params.bucket.as_deref() {
        None | Some("") => None,
        Some(s) => match parse_bucket(s) {
            Some(b) => Some(b),
            None => {
                let body = ErrorBody {
                    error: format!("invalid bucket `{s}`"),
                    valid: Some(valid_buckets()),
                };
                return (StatusCode::BAD_REQUEST, Json(body)).into_response();
            }
        },
    };

    // ---- Validate `category` ----
    let category_filter = match params.category.as_deref() {
        None | Some("") => None,
        Some(s) => match parse_category(s) {
            Some(c) => Some(c),
            None => {
                let body = ErrorBody {
                    error: format!("invalid category `{s}`"),
                    valid: Some(valid_categories()),
                };
                return (StatusCode::BAD_REQUEST, Json(body)).into_response();
            }
        },
    };

    // ---- Bucket/category compatibility (405) ----
    if let (Some(b), Some(c)) = (bucket_filter, category_filter)
        && !c.is_applicable_to(b)
    {
        let body = ErrorBody {
            error: format!(
                "category `{}` is not applicable to bucket `{}`",
                c.as_str(),
                b.as_str()
            ),
            valid: None,
        };
        return (StatusCode::METHOD_NOT_ALLOWED, Json(body)).into_response();
    }

    // ---- Validate `top` ----
    let top = match params.top {
        Some(0) => {
            let body = ErrorBody {
                error: "top must be >= 1".to_string(),
                valid: None,
            };
            return (StatusCode::BAD_REQUEST, Json(body)).into_response();
        }
        Some(n) if n > MAX_TOP_N => {
            let body = ErrorBody {
                error: format!("top must be <= {MAX_TOP_N}"),
                valid: None,
            };
            return (StatusCode::BAD_REQUEST, Json(body)).into_response();
        }
        Some(n) => n,
        None => MAX_TOP_N,
    };

    // ---- Read the snapshot (cheap shared lock) ----
    let handle = state.movers_snapshot_v2();
    let guard = match handle.read() {
        Ok(g) => g,
        Err(_) => {
            return Json(MoversV2Response::default()).into_response();
        }
    };
    let snapshot = match guard.as_ref() {
        Some(s) => s,
        None => {
            return Json(MoversV2Response::default()).into_response();
        }
    };

    // ---- Project per bucket filter ----
    let mut buckets = std::collections::BTreeMap::new();
    let include_bucket = |b: MoverBucket| bucket_filter.is_none_or(|filter| filter == b);

    if include_bucket(MoverBucket::Indices) {
        buckets.insert(
            MoverBucket::Indices.as_str().to_string(),
            project_price(&snapshot.indices, top, category_filter),
        );
    }
    if include_bucket(MoverBucket::Stocks) {
        buckets.insert(
            MoverBucket::Stocks.as_str().to_string(),
            project_price(&snapshot.stocks, top, category_filter),
        );
    }
    if include_bucket(MoverBucket::IndexFutures) {
        buckets.insert(
            MoverBucket::IndexFutures.as_str().to_string(),
            project_derivative(&snapshot.index_futures, top, category_filter),
        );
    }
    if include_bucket(MoverBucket::StockFutures) {
        buckets.insert(
            MoverBucket::StockFutures.as_str().to_string(),
            project_derivative(&snapshot.stock_futures, top, category_filter),
        );
    }
    if include_bucket(MoverBucket::IndexOptions) {
        buckets.insert(
            MoverBucket::IndexOptions.as_str().to_string(),
            project_derivative(&snapshot.index_options, top, category_filter),
        );
    }
    if include_bucket(MoverBucket::StockOptions) {
        buckets.insert(
            MoverBucket::StockOptions.as_str().to_string(),
            project_derivative(&snapshot.stock_options, top, category_filter),
        );
    }

    let body = MoversV2Response {
        snapshot_at_ist_secs: snapshot.snapshot_at_ist_secs,
        available: true,
        total_tracked: snapshot.total_tracked,
        buckets,
    };
    Json(body).into_response()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_bucket_known_values() {
        assert_eq!(parse_bucket("indices"), Some(MoverBucket::Indices));
        assert_eq!(parse_bucket("stocks"), Some(MoverBucket::Stocks));
        assert_eq!(
            parse_bucket("index_futures"),
            Some(MoverBucket::IndexFutures)
        );
        assert_eq!(
            parse_bucket("stock_futures"),
            Some(MoverBucket::StockFutures)
        );
        assert_eq!(
            parse_bucket("index_options"),
            Some(MoverBucket::IndexOptions)
        );
        assert_eq!(
            parse_bucket("stock_options"),
            Some(MoverBucket::StockOptions)
        );
        assert_eq!(parse_bucket("garbage"), None);
        assert_eq!(parse_bucket("INDICES"), None); // case-sensitive
        assert_eq!(parse_bucket(""), None);
    }

    #[test]
    fn test_parse_category_known_values() {
        assert_eq!(parse_category("gainers"), Some(MoverCategory::Gainers));
        assert_eq!(parse_category("losers"), Some(MoverCategory::Losers));
        assert_eq!(
            parse_category("most_active"),
            Some(MoverCategory::MostActive)
        );
        assert_eq!(parse_category("top_oi"), Some(MoverCategory::TopOi));
        assert_eq!(parse_category("oi_buildup"), Some(MoverCategory::OiBuildup));
        assert_eq!(parse_category("oi_unwind"), Some(MoverCategory::OiUnwind));
        assert_eq!(parse_category("top_value"), Some(MoverCategory::TopValue));
        assert_eq!(parse_category("xxx"), None);
    }

    #[test]
    fn test_valid_buckets_contains_all_six() {
        let bs = valid_buckets();
        assert_eq!(bs.len(), 6);
        for wire in [
            "indices",
            "stocks",
            "index_futures",
            "stock_futures",
            "index_options",
            "stock_options",
        ] {
            assert!(bs.iter().any(|b| b == wire), "missing bucket `{wire}`");
        }
    }

    #[test]
    fn test_valid_categories_contains_all_seven() {
        let cs = valid_categories();
        assert_eq!(cs.len(), 7);
        for wire in [
            "gainers",
            "losers",
            "most_active",
            "top_oi",
            "oi_buildup",
            "oi_unwind",
            "top_value",
        ] {
            assert!(cs.iter().any(|c| c == wire), "missing category `{wire}`");
        }
    }

    fn sample_entry() -> MoverEntryV2 {
        MoverEntryV2 {
            security_id: 1,
            exchange_segment_code: 0,
            last_traded_price: 100.0,
            prev_close: 95.0,
            change_pct: 5.26,
            volume: 1_000,
            open_interest: 0,
            prev_open_interest: 0,
            oi_change_pct: 0.0,
            value: 100_000.0,
        }
    }

    #[test]
    fn test_project_price_no_filter_returns_all_three_categories() {
        let mut bucket = PriceBucket::default();
        bucket.tracked = 5;
        for _ in 0..3 {
            bucket.gainers.push(sample_entry());
            bucket.losers.push(sample_entry());
            bucket.most_active.push(sample_entry());
        }
        let response = project_price(&bucket, 20, None);
        assert_eq!(response.tracked, 5);
        assert!(response.gainers.is_some());
        assert!(response.losers.is_some());
        assert!(response.most_active.is_some());
        // Price buckets NEVER emit derivative categories — those are None.
        assert!(response.top_oi.is_none());
        assert!(response.oi_buildup.is_none());
        assert!(response.oi_unwind.is_none());
        assert!(response.top_value.is_none());
    }

    #[test]
    fn test_project_price_gainers_only_filter() {
        let mut bucket = PriceBucket::default();
        bucket.tracked = 1;
        bucket.gainers.push(sample_entry());
        bucket.losers.push(sample_entry());
        bucket.most_active.push(sample_entry());
        let response = project_price(&bucket, 20, Some(MoverCategory::Gainers));
        assert!(response.gainers.is_some());
        assert!(response.losers.is_none());
        assert!(response.most_active.is_none());
    }

    #[test]
    fn test_project_derivative_no_filter_returns_all_seven_categories() {
        let mut bucket = DerivativeBucket::default();
        bucket.tracked = 7;
        bucket.gainers.push(sample_entry());
        bucket.losers.push(sample_entry());
        bucket.most_active.push(sample_entry());
        bucket.top_oi.push(sample_entry());
        bucket.oi_buildup.push(sample_entry());
        bucket.oi_unwind.push(sample_entry());
        bucket.top_value.push(sample_entry());
        let response = project_derivative(&bucket, 20, None);
        assert_eq!(response.tracked, 7);
        assert!(response.gainers.is_some());
        assert!(response.losers.is_some());
        assert!(response.most_active.is_some());
        assert!(response.top_oi.is_some());
        assert!(response.oi_buildup.is_some());
        assert!(response.oi_unwind.is_some());
        assert!(response.top_value.is_some());
    }

    #[test]
    fn test_project_derivative_top_oi_filter() {
        let mut bucket = DerivativeBucket::default();
        bucket.tracked = 1;
        bucket.gainers.push(sample_entry());
        bucket.top_oi.push(sample_entry());
        let response = project_derivative(&bucket, 20, Some(MoverCategory::TopOi));
        assert!(response.gainers.is_none());
        assert!(response.top_oi.is_some());
    }

    #[test]
    fn test_trim_respects_top_n_bound() {
        let list: Vec<MoverEntryV2> = (0..30).map(|_| sample_entry()).collect();
        let trimmed = trim(&list, 5);
        assert_eq!(trimmed.len(), 5);
        let trimmed_all = trim(&list, 100);
        assert_eq!(trimmed_all.len(), 30); // take() caps at available
    }

    // ----- End-to-end routing via the router -----

    fn mk_state() -> SharedAppState {
        use std::sync::{Arc, RwLock};
        let state = SharedAppState::new(
            tickvault_common::config::QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port: 1,
                pg_port: 1,
                ilp_port: 1,
            },
            tickvault_common::config::DhanConfig {
                websocket_url: "wss://test".to_string(),
                order_update_websocket_url: "wss://test".to_string(),
                rest_api_base_url: "https://test".to_string(),
                auth_base_url: "https://test".to_string(),
                instrument_csv_url: "https://test".to_string(),
                instrument_csv_fallback_url: "https://test".to_string(),
                max_instruments_per_connection: 5000,
                max_websocket_connections: 5,
                sandbox_base_url: String::new(),
            },
            tickvault_common::config::InstrumentConfig {
                daily_download_time: "08:55:00".to_string(),
                csv_cache_directory: "/tmp/tv-cache".to_string(),
                csv_cache_filename: "instruments.csv".to_string(),
                csv_download_timeout_secs: 120,
                build_window_start: "08:25:00".to_string(),
                build_window_end: "08:55:00".to_string(),
            },
            Arc::new(RwLock::new(None)),
            Arc::new(RwLock::new(None)),
            Arc::new(crate::state::SystemHealthStatus::new()),
        );
        state
    }

    async fn call(uri: &str, state: SharedAppState) -> (axum::http::StatusCode, String) {
        use axum::Router;
        use axum::body::Body;
        use axum::http::Request;
        use axum::routing::get;
        use tower::ServiceExt;

        let router = Router::new()
            .route("/api/movers", get(get_movers_v2))
            .with_state(state);
        let response = router
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .expect("valid request"),
            )
            .await
            .expect("router should respond");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body readable");
        let text = String::from_utf8_lossy(&bytes).to_string();
        (status, text)
    }

    #[tokio::test]
    async fn test_handler_invalid_bucket_returns_400_with_valid_list() {
        let state = mk_state();
        let (status, body) = call("/api/movers?bucket=garbage", state).await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert!(body.contains("invalid bucket"), "body={body}");
        assert!(body.contains("indices"), "valid list missing: {body}");
    }

    #[tokio::test]
    async fn test_handler_invalid_category_returns_400_with_valid_list() {
        let state = mk_state();
        let (status, body) = call("/api/movers?category=xxx", state).await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert!(body.contains("invalid category"), "body={body}");
        assert!(body.contains("top_oi"), "valid list missing: {body}");
    }

    #[tokio::test]
    async fn test_handler_top_oi_on_indices_returns_405() {
        let state = mk_state();
        let (status, body) = call("/api/movers?bucket=indices&category=top_oi", state).await;
        assert_eq!(status, axum::http::StatusCode::METHOD_NOT_ALLOWED);
        assert!(body.contains("not applicable"));
    }

    #[tokio::test]
    async fn test_handler_top_too_large_returns_400() {
        let state = mk_state();
        let (status, body) = call("/api/movers?top=21", state).await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert!(body.contains("top"), "body={body}");
    }

    #[tokio::test]
    async fn test_handler_top_zero_returns_400() {
        let state = mk_state();
        let (status, _) = call("/api/movers?top=0", state).await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_handler_empty_snapshot_returns_200_with_available_false() {
        let state = mk_state();
        let (status, body) = call("/api/movers", state).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert!(body.contains("\"available\":false"), "body={body}");
    }

    #[tokio::test]
    async fn test_handler_populated_snapshot_returns_200_with_buckets() {
        use std::sync::{Arc, RwLock};
        use tickvault_core::pipeline::top_movers::MoversSnapshotV2;

        let state = mk_state();
        let mut snapshot = MoversSnapshotV2::default();
        snapshot.snapshot_at_ist_secs = 1_800_000_000;
        snapshot.total_tracked = 1;
        snapshot.indices.tracked = 1;
        snapshot.indices.gainers.push(sample_entry());
        let handle = Arc::new(RwLock::new(Some(snapshot)));
        let state = state.with_movers_snapshot_v2(handle);

        let (status, body) = call("/api/movers?bucket=indices", state).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert!(body.contains("\"available\":true"), "body={body}");
        assert!(body.contains("\"indices\""), "body missing indices: {body}");
        // Filter-one-bucket behaviour: stocks must NOT appear.
        assert!(
            !body.contains("\"stocks\""),
            "stocks should be filtered: {body}"
        );
    }
}

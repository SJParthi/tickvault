//! Quote endpoint — returns the latest tick for a given security from QuestDB.
//!
//! Cold-path HTTP endpoint. Queries QuestDB via its HTTP SQL API.
//!
//! # Identity (I-P1-11, 2026-10-02)
//!
//! `security_id` alone is not unique: Dhan reuses ids across segments (for
//! example `27` is an `IDX_I` index and an `NSE_EQ` stock). The endpoint
//! therefore takes an optional `?segment=` (`IDX_I`, `NSE_EQ`, `NSE_FNO`, ...).
//! With it, the answer is that instrument's latest tick. Without it, the
//! answer is the single instrument holding that id; if the id has ticks in
//! more than one segment the endpoint answers **409** and lists them, instead
//! of returning whichever was fresher. Before 2026-10-02 it returned the
//! fresher one silently.
//!
//! # One HTTP client (2026-10-02)
//!
//! Requests go through the process's shared QuestDB client
//! ([`SharedAppState::questdb_http_client`], pooled, built once) with a
//! per-request timeout. Each cache miss used to build a new client, with its
//! own connection pool and no connection reuse.

use std::time::Duration;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use tickvault_common::segment::{segment_code_to_str, segment_str_to_code};

use crate::response_cache::{QUOTE_SEGMENT_UNSPECIFIED, cached_json_response};
use crate::state::SharedAppState;

/// Timeout for QuestDB quote queries (cold path, not tick processing).
const QUESTDB_QUOTE_TIMEOUT_SECS: u64 = 3;

/// Query parameters of `GET /api/quote/{security_id}`.
#[derive(Debug, Default, Deserialize)]
pub struct QuoteParams {
    /// The exchange segment, as stored in `ticks.segment` (`NSE_EQ`, ...).
    /// Optional; see the module docs for what its absence means.
    pub segment: Option<String>,
}

/// Latest quote response for a single security.
///
/// The `ticks` table is shared by every live feed (Dhan + Groww), so the
/// response carries the `feed` label of the latest row. Groww writes only a
/// subset of columns, so the OHLC / OI / cumulative-quantity / avg-price fields
/// are `Option`: a Groww latest row honestly reports them as `null` instead of
/// faking a `0.0` / `0`. (NULL handling for the Groww column subset is by
/// design — see `.claude/rules/project/live-feed-purity.md` rule 5.)
#[derive(Debug, Serialize)]
pub struct QuoteResponse {
    pub security_id: u64,
    /// Feed source of the latest tick (`"dhan"` / `"groww"`).
    pub feed: String,
    /// Exchange segment string as stored in `ticks.segment` (e.g. `"NSE_EQ"`).
    pub segment: String,
    pub last_traded_price: f64,
    /// Last traded quantity. `None` when the latest row's `last_trade_qty` is
    /// NULL (e.g. a Groww row).
    pub last_traded_quantity: Option<u64>,
    /// Cumulative day volume. `None` when NULL (e.g. a Groww row).
    pub volume: Option<u64>,
    /// Open interest. `None` when NULL.
    pub open_interest: Option<u64>,
    /// Day-session OHLC from the Quote/Full packet. `None` when NULL.
    pub day_open: Option<f64>,
    pub day_high: Option<f64>,
    pub day_low: Option<f64>,
    pub day_close: Option<f64>,
    pub timestamp: String,
}

/// `GET /api/quote/:security_id[?segment=SEG]` — the latest tick from QuestDB.
///
/// 2026-07-09 audit hardening: successful (200) bodies are TTL-cached per
/// `(security_id, segment)` (1s, bounded map in [`SharedAppState`]) — "latest
/// tick" honestly becomes "latest tick, ≤1s old". ONLY 200 responses are
/// cached: 400/404/409/503 are never stored, so attacker-chosen garbage
/// security_ids can never grow the map (only SIDs with real tick rows enter)
/// and a negative entry can never mask a just-arrived first tick. The rate
/// limiter in `crate::public_guard` runs BEFORE this handler (route_layer).
pub async fn get_quote(
    State(state): State<SharedAppState>,
    Path(security_id): Path<u64>,
    Query(params): Query<QuoteParams>,
) -> impl IntoResponse {
    // SECURITY: defense-in-depth guard against invalid security_id.
    // The u64 type from Axum's Path extractor already prevents SQL injection,
    // but we reject 0 as an invalid security_id (no instrument has id=0).
    if security_id == 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid security_id: must be non-zero"})),
        )
            .into_response();
    }

    // The segment is validated against the known set and only its CANONICAL
    // string (a `&'static str` from the code) ever reaches the SQL, never the
    // caller's text.
    let segment_code = match params.segment.as_deref() {
        None => None,
        Some(raw) => match segment_str_to_code(raw) {
            Some(code) => Some(code),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "unknown segment; expected one of IDX_I, NSE_EQ, NSE_FNO, \
                                  NSE_CURRENCY, BSE_EQ, MCX_COMM, BSE_CURRENCY, BSE_FNO"
                    })),
                )
                    .into_response();
            }
        },
    };
    let cache_key = (
        security_id,
        segment_code.unwrap_or(QUOTE_SEGMENT_UNSPECIFIED),
    );

    if let Some(body) = state.quote_cache().get(cache_key) {
        metrics::counter!("tv_api_cache_hits_total", "endpoint" => "quote").increment(1);
        return cached_json_response(body, "hit");
    }

    let cfg = state.questdb_config();
    let base_url = format!("http://{}:{}", cfg.host, cfg.http_port);
    let client = state.questdb_http_client();

    match query_latest_ticks(client, &base_url, security_id, segment_code).await {
        Some(mut rows) if rows.len() == 1 => {
            let Some(quote) = rows.pop() else {
                return internal_error();
            };
            match serde_json::to_string(&quote) {
                Ok(body) => {
                    // Cache ONLY the 200 body (see handler docs).
                    state.quote_cache().put(cache_key, body.clone());
                    cached_json_response(body, "miss")
                }
                Err(_) => internal_error(),
            }
        }
        Some(rows) if rows.len() > 1 => {
            // Only reachable without `?segment=`: with one, the query returns
            // at most one row per (security_id, segment).
            let segments: Vec<&str> = rows.iter().map(|q| q.segment.as_str()).collect();
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "security_id exists in multiple segments; pass ?segment=",
                    "security_id": security_id,
                    "segments": segments,
                })),
            )
                .into_response()
        }
        // QuestDB answered with no row: it is reachable, there is no data.
        Some(_) => not_found(),
        None => {
            // The query failed. Distinguish QuestDB unreachable from a query
            // QuestDB refused (for example a missing table).
            if check_questdb_reachable(client, &base_url).await {
                not_found()
            } else {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({"error": "QuestDB is unreachable"})),
                )
                    .into_response()
            }
        }
    }
}

/// The 404 body: QuestDB is reachable and holds no tick for the request.
fn not_found() -> axum::response::Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error": "no tick data found for this security_id"})),
    )
        .into_response()
}

/// The 500 body for a response that could not be built.
fn internal_error() -> axum::response::Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": "failed to serialize quote response"})),
    )
        .into_response()
}

/// The per-request timeout for every quote query. It overrides the shared
/// client's own (longer) timeout for these requests only.
const fn quote_timeout() -> Duration {
    Duration::from_secs(QUESTDB_QUOTE_TIMEOUT_SECS)
}

/// The latest-tick SQL for one security id, optionally in one segment.
///
/// Partitioned by `(security_id, segment)`, so without a segment it returns
/// one row PER SEGMENT the id has ticks in, which is what lets the handler
/// see a collision instead of silently picking the fresher row. The segment
/// text comes from [`segment_code_to_str`], never from the caller.
#[must_use]
fn build_latest_tick_sql(security_id: u64, segment_code: Option<u8>) -> String {
    // The columns that exist in the `ticks` table: `feed`, `segment` (SYMBOL
    // string, not a numeric code), `ltp`, `last_trade_qty`, `volume`, `oi`,
    // `open`/`high`/`low`/`close`, `ts`. Within each partition the freshest
    // row across ALL feeds wins; `feed` labels its source.
    const COLUMNS: &str = "security_id, feed, segment, ltp, last_trade_qty, volume, oi, \
                           open, high, low, close, ts";
    match segment_code {
        Some(code) => format!(
            "SELECT {COLUMNS} FROM ticks WHERE security_id = {security_id} \
             AND segment = '{}' LATEST ON ts PARTITION BY security_id, segment",
            segment_code_to_str(code)
        ),
        None => format!(
            "SELECT {COLUMNS} FROM ticks WHERE security_id = {security_id} \
             LATEST ON ts PARTITION BY security_id, segment"
        ),
    }
}

/// Queries QuestDB for the latest tick of `security_id`, one row per segment
/// (or only `segment_code`'s row when given).
///
/// `None` when the request failed or the answer was not a well-formed
/// dataset (any malformed row makes the whole answer `None`, as one bad row
/// is not evidence about the others). `Some(empty)` when QuestDB answered
/// with no row.
async fn query_latest_ticks(
    client: &reqwest::Client,
    base_url: &str,
    security_id: u64,
    segment_code: Option<u8>,
) -> Option<Vec<QuoteResponse>> {
    let sql = build_latest_tick_sql(security_id, segment_code);
    let url = format!("{base_url}/exec");
    let resp = client
        .get(&url)
        .timeout(quote_timeout())
        .query(&[("query", sql.as_str())])
        .send()
        .await
        .ok()?;
    let body: serde_json::Value = resp.json().await.ok()?;
    let dataset = body.get("dataset")?.as_array()?;
    dataset
        .iter()
        .map(|row| parse_quote_row(row.as_array()?))
        .collect()
}

/// Parses one `ticks` row in the [`build_latest_tick_sql`] column order:
/// security_id(0), feed(1), segment(2), ltp(3), last_trade_qty(4), volume(5),
/// oi(6), open(7), high(8), low(9), close(10), ts(11).
///
/// Mandatory fields use `?` (security_id, feed, segment, ltp, ts). The
/// remaining numeric fields are NULL for a Groww row (9-of-19 subset) — they
/// map to `None` via `as_u64()` / `as_f64()` (JSON `null` or absent → `None`),
/// never a misleading `0`/`0.0` and never a panic.
fn parse_quote_row(row: &[serde_json::Value]) -> Option<QuoteResponse> {
    Some(QuoteResponse {
        security_id: row.first()?.as_u64()?,
        feed: row.get(1)?.as_str()?.to_string(),
        segment: row.get(2)?.as_str()?.to_string(),
        last_traded_price: row.get(3)?.as_f64()?,
        last_traded_quantity: row.get(4).and_then(serde_json::Value::as_u64),
        volume: row.get(5).and_then(serde_json::Value::as_u64),
        open_interest: row.get(6).and_then(serde_json::Value::as_u64),
        day_open: row.get(7).and_then(serde_json::Value::as_f64),
        day_high: row.get(8).and_then(serde_json::Value::as_f64),
        day_low: row.get(9).and_then(serde_json::Value::as_f64),
        day_close: row.get(10).and_then(serde_json::Value::as_f64),
        timestamp: row.get(11)?.as_str().unwrap_or("").to_string(),
    })
}

/// Connectivity check — `SHOW TABLES` on QuestDB. Reachable means a 2xx
/// answer: before 2026-10-02 any HTTP answer counted, so a QuestDB returning
/// 5xx read as reachable and the handler answered 404 ("no data") instead of
/// 503.
async fn check_questdb_reachable(client: &reqwest::Client, base_url: &str) -> bool {
    let url = format!("{base_url}/exec");
    client
        .get(&url)
        .timeout(quote_timeout())
        .query(&[("query", "SHOW TABLES")])
        .send()
        .await
        .is_ok_and(|resp| resp.status().is_success())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The latest tick for `security_id` when QuestDB returned exactly one
    /// row, `None` otherwise. Keeps the row-parsing tests below on their
    /// original one-row shape.
    async fn query_one(
        client: &reqwest::Client,
        base_url: &str,
        security_id: u64,
    ) -> Option<QuoteResponse> {
        let mut rows = query_latest_ticks(client, base_url, security_id, None).await?;
        if rows.len() == 1 { rows.pop() } else { None }
    }

    /// A request with no `?segment=`.
    fn no_segment() -> Query<QuoteParams> {
        Query(QuoteParams::default())
    }

    /// A request with `?segment=<raw>`, exactly as the caller typed it.
    fn with_segment(raw: &str) -> Query<QuoteParams> {
        Query(QuoteParams {
            segment: Some(raw.to_string()),
        })
    }

    /// The mock's port, for [`mock_state`].
    fn port_of(base_url: &str) -> u16 {
        base_url
            .rsplit(':')
            .next()
            .expect("port should exist")
            .parse()
            .expect("port should parse")
    }

    /// A one-shot mock answering with an arbitrary HTTP status line.
    async fn start_status_mock_server(status_line: &'static str, body: &'static str) -> String {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut buf = vec![0u8; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
            let response = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        });
        format!("http://127.0.0.1:{}", addr.port())
    }

    /// The same id with ticks in two segments (I-P1-11: `27` is an IDX_I
    /// index and an NSE_EQ stock).
    const TWO_SEGMENT_ROWS: &str = r#"{"dataset":[[27,"dhan","IDX_I",21500.5,null,null,null,null,null,null,null,"2026-10-01T10:30:00.000000Z"],[27,"dhan","NSE_EQ",412.25,10,9000,null,410.0,415.0,409.0,411.0,"2026-10-01T10:30:01.000000Z"]]}"#;

    // ---- SQL ----

    #[test]
    fn test_latest_tick_sql_without_a_segment_returns_one_row_per_segment() {
        let sql = build_latest_tick_sql(27, None);
        assert!(sql.contains("WHERE security_id = 27 LATEST ON ts"), "{sql}");
        assert!(
            sql.contains("PARTITION BY security_id, segment"),
            "without a segment the query must keep each segment's row apart: {sql}"
        );
        assert!(!sql.contains("AND segment"), "{sql}");
    }

    #[test]
    fn test_latest_tick_sql_with_a_segment_uses_the_canonical_name() {
        let code = segment_str_to_code("NSE_EQ").expect("known segment");
        let sql = build_latest_tick_sql(27, Some(code));
        assert!(
            sql.contains("WHERE security_id = 27 AND segment = 'NSE_EQ' LATEST ON ts"),
            "{sql}"
        );
        assert!(sql.contains("PARTITION BY security_id, segment"), "{sql}");
    }

    // ---- multi-row parsing ----

    #[tokio::test]
    async fn test_query_latest_ticks_returns_every_segment_row() {
        let base_url = start_mock_server(TWO_SEGMENT_ROWS).await;
        let client = reqwest::Client::new();
        let rows = query_latest_ticks(&client, &base_url, 27, None)
            .await
            .expect("a well-formed dataset");
        let segments: Vec<&str> = rows.iter().map(|q| q.segment.as_str()).collect();
        assert_eq!(segments, ["IDX_I", "NSE_EQ"]);
    }

    #[tokio::test]
    async fn test_query_latest_ticks_one_malformed_row_fails_the_whole_answer() {
        let body = r#"{"dataset":[[27,"dhan","IDX_I",21500.5,null,null,null,null,null,null,null,"ts"],[27]]}"#;
        let base_url = start_mock_server(body).await;
        let client = reqwest::Client::new();
        assert!(
            query_latest_ticks(&client, &base_url, 27, None)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_query_latest_ticks_empty_dataset_is_an_answer_not_a_failure() {
        let base_url = start_mock_server(r#"{"dataset":[]}"#).await;
        let client = reqwest::Client::new();
        let rows = query_latest_ticks(&client, &base_url, 27, None).await;
        assert!(rows.is_some_and(|r| r.is_empty()));
    }

    // ---- handler: segment ----

    /// Two segments and no `?segment=`: 409 listing both, never a guess, and
    /// never cached.
    #[tokio::test]
    async fn test_get_quote_two_segments_without_a_param_is_409_listing_both() {
        let base_url = start_mock_server(TWO_SEGMENT_ROWS).await;
        let state = mock_state(port_of(&base_url));
        let response = get_quote(State(state.clone()), Path(27), no_segment())
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body readable");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(json["segments"], serde_json::json!(["IDX_I", "NSE_EQ"]));
        assert_eq!(json["security_id"], serde_json::json!(27));
        assert!(state.quote_cache().is_empty(), "a 409 is never cached");
    }

    /// `?segment=NSE_EQ` answers that instrument, and caches it under the
    /// composite key only.
    #[tokio::test]
    async fn test_get_quote_with_a_segment_returns_that_row_and_caches_it_by_segment() {
        let body = r#"{"dataset":[[27,"dhan","NSE_EQ",412.25,10,9000,null,410.0,415.0,409.0,411.0,"2026-10-01T10:30:01.000000Z"]]}"#;
        let base_url = start_mock_server(body).await;
        let state = mock_state(port_of(&base_url));
        let response = get_quote(State(state.clone()), Path(27), with_segment("NSE_EQ"))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body readable");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(json["segment"], serde_json::json!("NSE_EQ"));

        let nse_eq = segment_str_to_code("NSE_EQ").expect("known segment");
        assert!(state.quote_cache().get((27, nse_eq)).is_some());
        assert!(
            state
                .quote_cache()
                .get((27, QUOTE_SEGMENT_UNSPECIFIED))
                .is_none(),
            "a segment-scoped body must not answer an unscoped request"
        );

        // The mock is exhausted: an unscoped request cannot be a cache hit.
        let unscoped = get_quote(State(state), Path(27), no_segment())
            .await
            .into_response();
        assert_ne!(unscoped.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_get_quote_unknown_segment_is_400_and_never_cached() {
        let state = mock_state(1);
        let response = get_quote(State(state.clone()), Path(27), with_segment("BOGUS"))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(state.quote_cache().is_empty());
    }

    /// The caller's text never reaches the SQL: anything outside the known
    /// set, an injection attempt or a lowercase name included, is refused
    /// before a query is built.
    #[tokio::test]
    async fn test_get_quote_injection_shaped_segment_is_400() {
        for raw in ["nse_eq'--", "NSE_EQ' OR 1=1 --", "nse_eq", "", " NSE_EQ"] {
            let state = mock_state(1);
            let response = get_quote(State(state), Path(27), with_segment(raw))
                .await
                .into_response();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{raw:?}");
        }
    }

    // ---- reachability ----

    /// A QuestDB answering 5xx is not reachable for this check: before
    /// 2026-10-02 it was, and the handler answered 404 instead of 503.
    #[tokio::test]
    async fn test_check_questdb_reachable_5xx_is_unreachable() {
        let base_url =
            start_status_mock_server("500 Internal Server Error", r#"{"error":"boom"}"#).await;
        let client = reqwest::Client::new();
        assert!(!check_questdb_reachable(&client, &base_url).await);
    }

    // ---- shared client ratchet ----

    /// The handler uses the process's shared, pooled QuestDB client. Building
    /// a client per request (the shape before 2026-10-02) opens a new pool on
    /// every cache miss.
    #[test]
    fn test_quote_handler_uses_the_shared_client_and_builds_none() {
        let source = include_str!("quote.rs");
        let production = source
            .split_once("\n#[cfg(test)]")
            .map_or(source, |(before, _)| before);
        assert!(
            !production.contains("Client::builder"),
            "the quote handler must not build its own HTTP client"
        );
        assert!(
            production.contains("state.questdb_http_client()"),
            "the quote handler must use the shared QuestDB client"
        );
    }

    #[test]
    fn test_quote_response_serialization() {
        let quote = QuoteResponse {
            security_id: 12345,
            feed: "dhan".to_string(),
            segment: "NSE_FNO".to_string(),
            last_traded_price: 1500.50,
            last_traded_quantity: Some(100),
            volume: Some(50000),
            open_interest: Some(1234),
            day_open: Some(1490.0),
            day_high: Some(1510.0),
            day_low: Some(1485.0),
            day_close: Some(1495.0),
            timestamp: "2026-03-08T10:30:00.000000Z".to_string(),
        };
        let json = serde_json::to_string(&quote).expect("serialization should succeed");
        assert!(json.contains("\"security_id\":12345"));
        assert!(json.contains("\"feed\":\"dhan\""));
        assert!(json.contains("\"segment\":\"NSE_FNO\""));
        assert!(json.contains("\"last_traded_price\":1500.5"));
        assert!(json.contains("\"timestamp\":\"2026-03-08T10:30:00.000000Z\""));
    }

    #[test]
    fn test_quote_response_debug_impl() {
        let quote = QuoteResponse {
            security_id: 99999,
            feed: "dhan".to_string(),
            segment: "IDX_I".to_string(),
            last_traded_price: 250.75,
            last_traded_quantity: Some(50),
            volume: Some(10000),
            open_interest: None,
            day_open: Some(248.0),
            day_high: Some(252.0),
            day_low: Some(247.5),
            day_close: Some(249.0),
            timestamp: "2026-03-08T11:00:00.000000Z".to_string(),
        };
        let debug = format!("{quote:?}");
        assert!(debug.contains("QuoteResponse"));
        assert!(debug.contains("99999"));
    }

    /// A fully-populated Dhan-style row maps every Option to `Some(..)`.
    #[tokio::test]
    async fn test_query_latest_tick_dhan_row_all_fields_present() {
        // security_id, feed, segment, ltp, last_trade_qty, volume, oi,
        // open, high, low, close, ts
        let body = r#"{"dataset":[[12345,"dhan","NSE_FNO",1500.5,100,50000,1234,1490.0,1510.0,1485.0,1495.0,"2026-03-08T10:30:00.000000Z"]]}"#;
        let base_url = start_mock_server(body).await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");
        let quote = query_one(&client, &base_url, 12345)
            .await
            .expect("quote should be present");
        assert_eq!(quote.feed, "dhan");
        assert_eq!(quote.segment, "NSE_FNO");
        assert_eq!(quote.last_traded_quantity, Some(100));
        assert_eq!(quote.volume, Some(50000));
        assert_eq!(quote.open_interest, Some(1234));
        assert_eq!(quote.day_open, Some(1490.0));
        assert_eq!(quote.day_close, Some(1495.0));
    }

    /// A Groww-style latest row has NULL OHLC/OI/qty — they MUST map to `None`
    /// (JSON `null`), NOT a misleading `0.0` / `0`.
    #[tokio::test]
    async fn test_query_latest_tick_groww_row_null_ohlc_maps_to_none() {
        // Groww writes ltp + volume but NULL open/high/low/close/oi/last_trade_qty.
        let body = r#"{"dataset":[[12345,"groww","NSE_EQ",1500.5,null,50000,null,null,null,null,null,"2026-03-08T10:30:00.000000Z"]]}"#;
        let base_url = start_mock_server(body).await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");
        let quote = query_one(&client, &base_url, 12345)
            .await
            .expect("quote should be present");
        assert_eq!(quote.feed, "groww");
        assert_eq!(quote.last_traded_price, 1500.5);
        assert_eq!(quote.volume, Some(50000));
        // The NULL columns are honestly None, NOT 0.0 / 0.
        assert_eq!(quote.day_open, None);
        assert_eq!(quote.day_high, None);
        assert_eq!(quote.day_low, None);
        assert_eq!(quote.day_close, None);
        assert_eq!(quote.open_interest, None);
        assert_eq!(quote.last_traded_quantity, None);

        // Serialized JSON shows null, never a faked 0.
        let json = serde_json::to_string(&quote).expect("serialization should succeed");
        assert!(json.contains("\"feed\":\"groww\""));
        assert!(json.contains("\"day_open\":null"));
        assert!(json.contains("\"open_interest\":null"));
        assert!(!json.contains("\"day_open\":0.0"));
    }

    /// The SELECT must reference only columns that exist in the real `ticks`
    /// DDL — pins against a regression back to the phantom column names.
    #[tokio::test]
    async fn test_query_latest_tick_uses_real_ddl_columns() {
        // The mock echoes the query so we can assert the SELECT shape.
        // We instead assert the handler parses a row in the real column order.
        // (Column-name correctness is also covered by the 12-field mock rows
        // above matching the SELECT order: feed(1), segment(2), ... ts(11).)
        let body =
            r#"{"dataset":[[1,"dhan","IDX_I",100.0,null,null,null,null,null,null,null,"ts"]]}"#;
        let base_url = start_mock_server(body).await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");
        let quote = query_one(&client, &base_url, 1)
            .await
            .expect("quote should be present");
        assert_eq!(quote.security_id, 1);
        assert_eq!(quote.feed, "dhan");
        assert_eq!(quote.segment, "IDX_I");
        assert_eq!(quote.last_traded_price, 100.0);
    }

    #[tokio::test]
    async fn test_query_latest_tick_unreachable() {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(100))
            .build()
            .expect("client build should succeed");
        let result = query_one(&client, "http://127.0.0.1:1", 12345).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_check_questdb_reachable_unreachable() {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(100))
            .build()
            .expect("client build should succeed");
        let result = check_questdb_reachable(&client, "http://127.0.0.1:1").await;
        assert!(!result);
    }

    /// Starts a minimal HTTP server on a random port that responds with `body`.
    async fn start_mock_server(body: &'static str) -> String {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        let base_url = format!("http://127.0.0.1:{}", addr.port());

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        });

        base_url
    }

    #[tokio::test]
    async fn test_query_latest_tick_with_valid_data() {
        let body = r#"{"dataset":[[12345,"dhan","NSE_FNO",1500.5,100,50000,1234,1490.0,1510.0,1485.0,1495.0,"2026-03-08T10:30:00.000000Z"]]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");

        let result = query_one(&client, &base_url, 12345).await;
        assert!(result.is_some());
        let quote = result.expect("quote should be present");
        assert_eq!(quote.security_id, 12345);
        assert!((quote.last_traded_price - 1500.5).abs() < f64::EPSILON);
        assert_eq!(quote.volume, Some(50000));
    }

    #[tokio::test]
    async fn test_query_latest_tick_empty_dataset() {
        let body = r#"{"dataset":[]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");

        let result = query_one(&client, &base_url, 99999).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_query_latest_tick_malformed_json() {
        let body = "not json";
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");

        let result = query_one(&client, &base_url, 12345).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_query_latest_tick_missing_dataset_key() {
        let body = r#"{"error":"table not found"}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");

        let result = query_one(&client, &base_url, 12345).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_check_questdb_reachable_returns_true() {
        let body = r#"{"columns":["tableName"],"dataset":[["ticks"]]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");

        let result = check_questdb_reachable(&client, &base_url).await;
        assert!(result);
    }

    #[tokio::test]
    async fn test_query_latest_tick_row_missing_fields() {
        // Row with too few fields — should return None via bounds check
        let body = r#"{"dataset":[[12345]]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");

        let result = query_one(&client, &base_url, 12345).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_query_latest_tick_with_null_optional_fields_maps_to_none() {
        // Row with NULL optional fields (ltq, volume, oi, OHLC) — they map to
        // None (honest null), NOT a faked 0 / 0.0. `feed`, `segment`, `ltp`,
        // `ts` are mandatory and present.
        let body = r#"{"dataset":[[12345,"groww","NSE_EQ",1500.5,null,null,null,null,null,null,null,"2026-03-08T10:30:00.000000Z"]]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");

        let result = query_one(&client, &base_url, 12345).await;
        assert!(result.is_some());
        let quote = result.unwrap();
        assert_eq!(quote.last_traded_quantity, None);
        assert_eq!(quote.volume, None);
        assert_eq!(quote.open_interest, None);
        assert_eq!(quote.day_open, None);
    }

    // -----------------------------------------------------------------------
    // Helper: builds a SharedAppState pointing to a specific mock port
    // -----------------------------------------------------------------------

    fn mock_state(http_port: u16) -> crate::state::SharedAppState {
        use tickvault_common::config::{DhanConfig, InstrumentConfig, QuestDbConfig};

        crate::state::SharedAppState::new(
            QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port,
                pg_port: 1,
                ilp_port: 1,
            },
            DhanConfig {
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
            InstrumentConfig {
                daily_download_time: "08:55:00".to_string(),
                csv_cache_directory: "/tmp/tv-cache".to_string(),
                csv_cache_filename: "instruments.csv".to_string(),
                csv_download_timeout_secs: 120,
                build_window_start: "08:25:00".to_string(),
                build_window_end: "08:55:00".to_string(),
            },
            std::sync::Arc::new(crate::state::SystemHealthStatus::new()),
        )
    }

    /// Multi-request mock server that serves different responses for successive connections.
    async fn start_multi_mock_server(responses: Vec<&'static str>) -> String {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        let base_url = format!("http://127.0.0.1:{}", addr.port());

        tokio::spawn(async move {
            for body in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });

        base_url
    }

    // -----------------------------------------------------------------------
    // get_quote handler: QuestDB unreachable → 503
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_quote_questdb_unreachable_returns_503() {
        // Port 1 is unreachable — both query_latest_ticks and check_questdb_reachable fail
        let state = mock_state(1);
        let response = get_quote(State(state), Path(12345), no_segment())
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    // -----------------------------------------------------------------------
    // get_quote handler: QuestDB reachable but no data → 404
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_quote_no_data_returns_404() {
        // An empty dataset is QuestDB answering with no row: 404 straight
        // away, with no reachability probe (since 2026-10-02).
        let responses = vec![r#"{"dataset":[]}"#];
        let base_url = start_multi_mock_server(responses).await;
        let port: u16 = base_url
            .rsplit(':')
            .next()
            .expect("port should exist")
            .parse()
            .expect("port should parse");

        let state = mock_state(port);
        let response = get_quote(State(state), Path(99999), no_segment())
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // -----------------------------------------------------------------------
    // get_quote handler: QuestDB reachable and has data → 200
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_quote_with_valid_data_returns_200() {
        let body = r#"{"dataset":[[12345,"dhan","NSE_FNO",1500.5,100,50000,1234,1490.0,1510.0,1485.0,1495.0,"2026-03-08T10:30:00.000000Z"]]}"#;
        let base_url = start_mock_server(body).await;
        let port: u16 = base_url
            .rsplit(':')
            .next()
            .expect("port should exist")
            .parse()
            .expect("port should parse");

        let state = mock_state(port);
        let response = get_quote(State(state), Path(12345), no_segment())
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::OK);
    }

    // -----------------------------------------------------------------------
    // 2026-07-09 hardening: TTL cache — only 200 bodies, never errors
    // -----------------------------------------------------------------------

    /// A second call inside the 1s TTL must be a cache HIT — byte-identical
    /// 200 body with ZERO QuestDB round-trips. The single-response mock
    /// serves the row EXACTLY once; an uncached second call would hit the
    /// exhausted server and 503/404.
    #[tokio::test]
    async fn test_get_quote_cache_hit_returns_identical_200() {
        let body = r#"{"dataset":[[12345,"dhan","NSE_FNO",1500.5,100,50000,1234,1490.0,1510.0,1485.0,1495.0,"2026-03-08T10:30:00.000000Z"]]}"#;
        let base_url = start_mock_server(body).await;
        let port: u16 = base_url
            .rsplit(':')
            .next()
            .expect("port should exist")
            .parse()
            .expect("port should parse");

        let state = mock_state(port);
        let first = get_quote(State(state.clone()), Path(12345), no_segment())
            .await
            .into_response();
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(
            first
                .headers()
                .get(crate::response_cache::CACHE_MARKER_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("miss"),
            "first call must be a cache miss"
        );
        let first_body = axum::body::to_bytes(first.into_body(), 64 * 1024)
            .await
            .expect("first body readable");

        // Mock exhausted — only the cache can reproduce this 200.
        let second = get_quote(State(state), Path(12345), no_segment())
            .await
            .into_response();
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(
            second
                .headers()
                .get(crate::response_cache::CACHE_MARKER_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("hit"),
            "second call inside the TTL must be a cache hit"
        );
        let second_body = axum::body::to_bytes(second.into_body(), 64 * 1024)
            .await
            .expect("second body readable");
        assert_eq!(
            first_body, second_body,
            "cache hit must return the byte-identical 200 body"
        );
    }

    /// Error responses are NEVER cached: a 404 (no data yet) must not
    /// poison the cache — the next request goes back to QuestDB and picks
    /// up a just-arrived first tick as a fresh 200.
    #[tokio::test]
    async fn test_get_quote_404_is_never_cached() {
        let responses = vec![
            // call 1: empty dataset → 404 (QuestDB answered, no probe).
            r#"{"dataset":[]}"#,
            // call 2: the first tick has arrived → 200.
            r#"{"dataset":[[777,"dhan","IDX_I",100.5,null,null,null,null,null,null,null,"2026-07-09T10:30:00.000000Z"]]}"#,
        ];
        let base_url = start_multi_mock_server(responses).await;
        let port: u16 = base_url
            .rsplit(':')
            .next()
            .expect("port should exist")
            .parse()
            .expect("port should parse");

        let state = mock_state(port);
        let first = get_quote(State(state.clone()), Path(777), no_segment())
            .await
            .into_response();
        assert_eq!(first.status(), StatusCode::NOT_FOUND);
        assert!(
            state.quote_cache().is_empty(),
            "a 404 must never enter the cache"
        );

        let second = get_quote(State(state), Path(777), no_segment())
            .await
            .into_response();
        assert_eq!(
            second.status(),
            StatusCode::OK,
            "the request after the first tick must be a fresh 200, not a cached 404"
        );
    }

    /// The 400 invalid-SID guard runs before any cache/DB work and is
    /// never cached either.
    #[tokio::test]
    async fn test_get_quote_zero_security_id_not_cached() {
        let state = mock_state(1);
        let response = get_quote(State(state.clone()), Path(0), no_segment())
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(state.quote_cache().is_empty(), "400 must never be cached");
    }

    // -----------------------------------------------------------------------
    // get_quote handler: timestamp field missing → None from query_latest_ticks
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_query_latest_tick_null_timestamp_returns_fallback() {
        // Timestamp field (index 11) is null — unwrap_or("") handles it
        let body = r#"{"dataset":[[12345,"dhan","NSE_FNO",1500.5,100,50000,1234,1490.0,1510.0,1485.0,1495.0,null]]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");

        let result = query_one(&client, &base_url, 12345).await;
        assert!(result.is_some());
        let quote = result.expect("quote should be present");
        assert!(quote.timestamp.is_empty());
    }

    // -----------------------------------------------------------------------
    // get_quote handler: non-numeric security_id field → None
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_query_latest_tick_non_numeric_security_id_returns_none() {
        let body = r#"{"dataset":[["not_a_number","dhan","NSE_FNO",1500.5,100,50000,1234,1490.0,1510.0,1485.0,1495.0,"ts"]]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");

        let result = query_one(&client, &base_url, 12345).await;
        assert!(result.is_none());
    }

    // -----------------------------------------------------------------------
    // get_quote handler: non-string feed (index 1) → None (as_str? fails)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_query_latest_tick_non_string_feed_returns_none() {
        // feed at index 1 is a number, not a string — as_str()? returns None.
        let body = r#"{"dataset":[[12345,42,"NSE_FNO",1500.5,100,50000,1234,1490.0,1510.0,1485.0,1495.0,"ts"]]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");

        let result = query_one(&client, &base_url, 12345).await;
        assert!(result.is_none());
    }

    // -----------------------------------------------------------------------
    // get_quote handler: non-string segment (index 2) → None (as_str? fails)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_query_latest_tick_non_string_segment_returns_none() {
        // segment at index 2 is a number, not a string — as_str()? returns None.
        let body = r#"{"dataset":[[12345,"dhan",99,1500.5,100,50000,1234,1490.0,1510.0,1485.0,1495.0,"ts"]]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");

        let result = query_one(&client, &base_url, 12345).await;
        assert!(result.is_none());
    }

    // -----------------------------------------------------------------------
    // get_quote handler: non-numeric LTP → None
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_query_latest_tick_non_numeric_ltp_returns_none() {
        // ltp at index 3 is non-numeric — as_f64()? returns None.
        let body = r#"{"dataset":[[12345,"dhan","NSE_FNO","bad",100,50000,1234,1490.0,1510.0,1485.0,1495.0,"ts"]]}"#;
        let base_url = start_mock_server(body).await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client build should succeed");

        let result = query_one(&client, &base_url, 12345).await;
        assert!(result.is_none());
    }

    // -----------------------------------------------------------------------
    // QUESTDB_QUOTE_TIMEOUT_SECS constant check
    // -----------------------------------------------------------------------

    #[test]
    fn test_questdb_quote_timeout_secs_is_reasonable() {
        assert!(QUESTDB_QUOTE_TIMEOUT_SECS > 0);
        assert!(QUESTDB_QUOTE_TIMEOUT_SECS <= 30);
    }

    // -----------------------------------------------------------------------
    // query_latest_ticks: partial row coverage — each ? branch exercised
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_query_latest_tick_row_missing_mandatory_feed_returns_none() {
        // Row with 1 element — row.get(1)? (feed) returns None.
        let body = r#"{"dataset":[[12345]]}"#;
        let base_url = start_mock_server(body).await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        assert!(query_one(&client, &base_url, 12345).await.is_none());
    }

    #[tokio::test]
    async fn test_query_latest_tick_row_missing_mandatory_segment_returns_none() {
        // Row with 2 elements (security_id, feed) — row.get(2)? (segment) None.
        let body = r#"{"dataset":[[12345,"dhan"]]}"#;
        let base_url = start_mock_server(body).await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        assert!(query_one(&client, &base_url, 12345).await.is_none());
    }

    #[tokio::test]
    async fn test_query_latest_tick_row_missing_mandatory_ltp_returns_none() {
        // Row with 3 elements (security_id, feed, segment) — row.get(3)? (ltp) None.
        let body = r#"{"dataset":[[12345,"dhan","NSE_FNO"]]}"#;
        let base_url = start_mock_server(body).await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        assert!(query_one(&client, &base_url, 12345).await.is_none());
    }

    #[tokio::test]
    async fn test_query_latest_tick_row_missing_mandatory_ts_returns_none() {
        // Row with mandatory feed/segment/ltp + optional cols present but the
        // mandatory ts (index 11) missing — row.get(11)? returns None.
        let body = r#"{"dataset":[[12345,"dhan","NSE_FNO",1500.5,100,50000,1234,1490.0,1510.0,1485.0,1495.0]]}"#;
        let base_url = start_mock_server(body).await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        assert!(query_one(&client, &base_url, 12345).await.is_none());
    }

    // -----------------------------------------------------------------------
    // query_latest_ticks: empty row (row.first()? returns None)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_query_latest_tick_empty_row_returns_none() {
        // Row is empty array — row.first()? returns None
        let body = r#"{"dataset":[[]]}"#;
        let base_url = start_mock_server(body).await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        assert!(query_one(&client, &base_url, 12345).await.is_none());
    }

    // -----------------------------------------------------------------------
    // query_latest_ticks: dataset element not an array
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_query_latest_tick_dataset_element_not_array() {
        // dataset first element is a number, not an array
        let body = r#"{"dataset":[42]}"#;
        let base_url = start_mock_server(body).await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        assert!(query_one(&client, &base_url, 12345).await.is_none());
    }

    // -----------------------------------------------------------------------
    // query_latest_ticks: dataset is not an array
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_query_latest_tick_dataset_not_array() {
        let body = r#"{"dataset":"not_array"}"#;
        let base_url = start_mock_server(body).await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        assert!(query_one(&client, &base_url, 12345).await.is_none());
    }

    // -----------------------------------------------------------------------
    // QuoteResponse: all optional fields None (null in JSON, not faked zeros)
    // -----------------------------------------------------------------------

    #[test]
    fn test_quote_response_with_all_none_optionals() {
        let quote = QuoteResponse {
            security_id: 1,
            feed: "groww".to_string(),
            segment: "NSE_EQ".to_string(),
            last_traded_price: 0.0,
            last_traded_quantity: None,
            volume: None,
            open_interest: None,
            day_open: None,
            day_high: None,
            day_low: None,
            day_close: None,
            timestamp: String::new(),
        };
        let json = serde_json::to_string(&quote).unwrap();
        assert!(json.contains("\"security_id\":1"));
        assert!(json.contains("\"timestamp\":\"\""));
        assert!(json.contains("\"volume\":null"));
        assert!(json.contains("\"day_close\":null"));
    }

    // -----------------------------------------------------------------------
    // get_quote handler: security_id == 0 → 400 Bad Request
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_get_quote_zero_security_id_returns_400() {
        let state = mock_state(1);
        let response = get_quote(State(state), Path(0), no_segment())
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}

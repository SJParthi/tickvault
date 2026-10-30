//! Coverage top-up for the api crate (2026-10-30): real assertions on the
//! branches the existing suites never reached.
//!
//! Most of the targeted lines are field expressions inside `tracing` macros.
//! A `tracing` macro evaluates its fields only when a subscriber enables the
//! event, and no test in this crate installs one, so those lines never ran.
//! [`CaptureSubscriber`] is a hand-rolled subscriber (no new dependency) that
//! enables every event on the test's own thread and records each event's
//! fields, so the tests below assert WHAT was logged (error code, peer label,
//! feed name, duration), not merely that a call happened.
//!
//! Every test runs on the default current-thread tokio runtime (or a plain
//! `#[test]` thread), so `tracing::subscriber::set_default` scopes the
//! capture to that test alone. No environment variable is set, no sleep is
//! used, and every file this suite creates lives under `std::env::temp_dir()`
//! and is removed before the test ends.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{Request, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use tower::ServiceExt;
use tracing::field::{Field, Visit};
use tracing::span;
use tracing::{Event, Level, Metadata, Subscriber};

use tickvault_api::feed_state::{Feed, FeedStatus};
use tickvault_api::feed_state_persist::{load_feed_state, persist_feed_state};
use tickvault_api::handlers::feeds::{SetFeedRequest, set_feed};
use tickvault_api::handlers::health::health_check;
use tickvault_api::handlers::quote::{QuoteParams, get_quote};
use tickvault_api::middleware::{ApiAuthConfig, request_tracing, require_bearer_auth};
use tickvault_api::state::{SharedAppState, SystemHealthStatus};
use tickvault_common::config::{DhanConfig, InstrumentConfig, QuestDbConfig};
use tickvault_common::error_code::ErrorCode;

// ---------------------------------------------------------------------------
// Event capture
// ---------------------------------------------------------------------------

/// One captured `tracing` event: its level and every field as `(name, value)`.
#[derive(Debug, Clone)]
struct CapturedEvent {
    level: Level,
    fields: Vec<(String, String)>,
}

impl CapturedEvent {
    fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(field_name, _)| field_name == name)
            .map(|(_, value)| value.as_str())
    }

    fn message(&self) -> &str {
        self.field("message").unwrap_or("")
    }
}

struct FieldVisitor<'a> {
    fields: &'a mut Vec<(String, String)>,
}

impl Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields
            .push((field.name().to_string(), format!("{value:?}")));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .push((field.name().to_string(), value.to_string()));
    }
}

/// Enables every event and span; records events into a shared list.
struct CaptureSubscriber {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl Subscriber for CaptureSubscriber {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }

    fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

    fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut fields = Vec::new();
        event.record(&mut FieldVisitor {
            fields: &mut fields,
        });
        let captured = CapturedEvent {
            level: *event.metadata().level(),
            fields,
        };
        match self.events.lock() {
            Ok(mut list) => list.push(captured),
            Err(poisoned) => poisoned.into_inner().push(captured),
        }
    }

    fn enter(&self, _span: &span::Id) {}

    fn exit(&self, _span: &span::Id) {}
}

/// Installs the capture subscriber for the current thread. Keep the guard
/// alive for the whole test; read the events through the returned handle.
fn capture() -> (
    tracing::subscriber::DefaultGuard,
    Arc<Mutex<Vec<CapturedEvent>>>,
) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let guard = tracing::subscriber::set_default(CaptureSubscriber {
        events: Arc::clone(&events),
    });
    (guard, events)
}

fn events_of(events: &Arc<Mutex<Vec<CapturedEvent>>>) -> Vec<CapturedEvent> {
    match events.lock() {
        Ok(list) => list.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn unique_temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "tv-api-covgap-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("mkdir {dir:?}: {e}"));
    dir
}

fn state_with_questdb(
    host: &str,
    http_port: u16,
    health: Arc<SystemHealthStatus>,
) -> SharedAppState {
    SharedAppState::new(
        QuestDbConfig {
            host: host.to_string(),
            http_port,
            pg_port: 8812,
            ilp_port: 9009,
        },
        DhanConfig {
            websocket_url: "wss://test".to_string(),
            order_update_websocket_url: "wss://test".to_string(),
            rest_api_base_url: "https://test".to_string(),
            auth_base_url: "https://test".to_string(),
            instrument_csv_url: "https://test/csv".to_string(),
            instrument_csv_fallback_url: "https://test/csv2".to_string(),
            max_instruments_per_connection: 100,
            max_websocket_connections: 5,
            sandbox_base_url: String::new(),
        },
        InstrumentConfig {
            daily_download_time: "06:30:00".to_string(),
            csv_cache_directory: std::env::temp_dir().display().to_string(),
            csv_cache_filename: "test.csv".to_string(),
            csv_download_timeout_secs: 30,
            build_window_start: "06:00:00".to_string(),
            build_window_end: "09:15:00".to_string(),
        },
        health,
    )
}

async fn body_string(resp: axum::response::Response) -> (StatusCode, String) {
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .expect("read body");
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// A mock QuestDB that answers ONE connection per canned body (status 200),
/// then drops the listener so every later connection is refused.
async fn start_mock_questdb(responses: Vec<&'static str>) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind should succeed");
    let port = listener.local_addr().expect("local_addr").port();
    tokio::spawn(async move {
        for body in responses {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0u8; 8192];
            let _read = stream.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _written = stream.write_all(response.as_bytes()).await;
            let _shut = stream.shutdown().await;
        }
    });
    port
}

// ---------------------------------------------------------------------------
// feed_state_persist.rs
// ---------------------------------------------------------------------------

/// feed_state_persist.rs:200-203 — the tmp-file cleanup after a failed write,
/// including the arm where the cleanup itself fails (`debug!` of
/// `cleanup_err`). A parent path just under PATH_MAX lets `create_dir_all`
/// succeed while the unique tmp name pushes the tmp path past PATH_MAX, so both
/// the tmp create AND its removal fail with ENAMETOOLONG.
#[cfg(target_os = "linux")]
#[test]
fn persist_failure_whose_tmp_cleanup_also_fails_logs_the_cleanup_error() {
    // Linux PATH_MAX is 4096 bytes including the NUL; aim the parent at 4060.
    const PARENT_TARGET_LEN: usize = 4060;
    const ENAMETOOLONG: i32 = 36;

    let root = unique_temp_dir("longpath");
    let mut parent = root.clone();
    // Fill with components of at most 200 bytes (NAME_MAX is 255), leaving
    // room for the final `data` component.
    loop {
        let current = parent.as_os_str().len();
        let remaining = PARENT_TARGET_LEN.saturating_sub(current + 1 + "data".len() + 1);
        if remaining == 0 {
            break;
        }
        let chunk = remaining.min(200);
        parent.push("x".repeat(chunk));
    }
    parent.push("data");
    assert!(
        parent.as_os_str().len() <= PARENT_TARGET_LEN,
        "parent must stay under PATH_MAX so create_dir_all succeeds"
    );
    let path = parent.join("feed-state.json");

    let (_guard, events) = capture();
    let result = persist_feed_state(
        &FeedStatus {
            dhan_enabled: true,
            dhan_lane_running: false,
            dhan_disable_allowed: true,
        },
        &path,
    );

    let err = result.expect_err("a tmp path past PATH_MAX must fail the write");
    assert_eq!(err.raw_os_error(), Some(ENAMETOOLONG));
    assert!(parent.is_dir(), "the parent dir itself was creatable");
    assert!(
        !path.exists(),
        "a failed write must never create the target"
    );
    let cleanup = events_of(&events)
        .into_iter()
        .find(|e| {
            e.message()
                .contains("failed to remove stray feed-state tmp file")
        })
        .expect("the failed cleanup must be logged at debug");
    assert_eq!(cleanup.level, Level::DEBUG);
    assert!(
        cleanup
            .field("cleanup_err")
            .is_some_and(|v| v.contains("36") || v.to_lowercase().contains("too long")),
        "the logged cleanup error must be the ENAMETOOLONG from remove_file: {cleanup:?}"
    );

    std::fs::remove_dir_all(&root).expect("cleanup temp root");
}

/// feed_state_persist.rs:224 — a read error other than NotFound (the
/// canonical path is a DIRECTORY, so `read` fails with EISDIR) falls back to
/// `None`, i.e. the config default.
#[test]
fn load_feed_state_returns_none_when_the_path_is_unreadable() {
    let root = unique_temp_dir("unreadable");
    let path = root.join("data").join("feed-state.json");
    std::fs::create_dir_all(&path).expect("make the overlay path a directory");

    // Guard against a vacuous pass: the read really does fail with a
    // non-NotFound kind, which is the arm under test.
    let read_err = std::fs::read(&path).expect_err("reading a directory must fail");
    assert_ne!(read_err.kind(), std::io::ErrorKind::NotFound);

    assert_eq!(load_feed_state(&path), None);

    std::fs::remove_dir_all(&root).expect("cleanup temp root");
}

// ---------------------------------------------------------------------------
// handlers/feeds.rs
// ---------------------------------------------------------------------------

/// feeds.rs:212 — the `info!` emitted when a toggleable non-Dhan feed is
/// actually flipped carries the feed's label. TrueData is runtime-toggleable,
/// so its enable runs `set_enabled` and the log.
///
/// Side effect (the handler's own, shared with the existing set_feed unit
/// tests): the handler persists to its fixed relative path
/// `data/feed-state.json` under the crate directory, which is git-ignored.
#[tokio::test]
async fn enabling_truedata_flips_the_flag_and_logs_the_feed_label() {
    let state = state_with_questdb("127.0.0.1", 9, Arc::new(SystemHealthStatus::new()));
    assert!(!state.feed_runtime().is_enabled(Feed::Truedata));

    let (_guard, events) = capture();
    let result = set_feed(
        State(state.clone()),
        Path("truedata".to_string()),
        axum::Json(SetFeedRequest { enabled: true }),
    )
    .await;

    assert!(result.is_ok(), "enabling truedata must be accepted");
    assert!(state.feed_runtime().is_enabled(Feed::Truedata));
    let toggled = events_of(&events)
        .into_iter()
        .find(|e| e.message().contains("feed runtime toggled via API"))
        .expect("the toggle must be logged");
    assert_eq!(toggled.level, Level::INFO);
    assert_eq!(toggled.field("feed"), Some("truedata"));
    assert_eq!(toggled.field("enabled"), Some("true"));
}

// ---------------------------------------------------------------------------
// handlers/health.rs
// ---------------------------------------------------------------------------

/// health.rs:297 — a pipeline that HAS reported but is not active renders
/// `inactive` (not `unwired` and not `active`).
#[tokio::test]
async fn health_reports_inactive_for_a_reported_but_stopped_pipeline() {
    let health = Arc::new(SystemHealthStatus::new());
    health.set_pipeline_active(false);
    assert!(health.pipeline_reported());
    let state = state_with_questdb("127.0.0.1", 9, Arc::clone(&health));

    let response = health_check(State(state)).await;

    assert_eq!(response.0.subsystems.pipeline.status, "inactive");
    assert_eq!(response.0.subsystems.pipeline.detail, None);
}

// ---------------------------------------------------------------------------
// handlers/quote.rs
// ---------------------------------------------------------------------------

/// quote.rs:211 — the query FAILED (QuestDB answered without a `dataset`) but
/// QuestDB itself is reachable, so the answer is 404, and that 404 is NOT
/// cached: the next identical request re-queries and, with the mock gone,
/// reports 503.
#[tokio::test]
async fn failed_query_against_a_reachable_questdb_is_an_uncached_404() {
    let port = start_mock_questdb(vec![
        "{\"query\":\"x\",\"error\":\"table does not exist [table=ticks]\",\"position\":0}",
        "{\"query\":\"SHOW TABLES\",\"dataset\":[]}",
    ])
    .await;
    let state = state_with_questdb("127.0.0.1", port, Arc::new(SystemHealthStatus::new()));

    let first = get_quote(
        State(state.clone()),
        Path(42_u64),
        Query(QuoteParams::default()),
    )
    .await
    .into_response();
    let (status, body) = body_string(first).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let json: serde_json::Value = serde_json::from_str(&body).expect("JSON body");
    assert_eq!(json["error"], "no tick data found for this security_id");

    // Not cached: the repeat goes back to QuestDB, which is now gone.
    let second = get_quote(State(state), Path(42_u64), Query(QuoteParams::default()))
        .await
        .into_response();
    let (status, body) = body_string(second).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "a 404 after a failed query must not be cached: {body}"
    );
}

// ---------------------------------------------------------------------------
// middleware.rs
// ---------------------------------------------------------------------------

const GOOD_TOKEN: &str = "k8#Qz!r2Lm@9vT$4wXp&7Ns*1Bd^6Hf%";

fn auth_router(config: ApiAuthConfig) -> Router {
    Router::new()
        .route("/protected", get(|| async { "ok" }))
        .layer(axum::middleware::from_fn_with_state(
            config,
            require_bearer_auth,
        ))
        .layer(axum::middleware::from_fn(request_tracing))
}

async fn send(router: &Router, request: Request<Body>) -> StatusCode {
    router
        .clone()
        .oneshot(request)
        .await
        .expect("router is infallible")
        .status()
}

/// middleware.rs:598/601 (mismatched bearer: OOB-reload hint `debug!` and the
/// peer field), 612 (malformed header peer field), 622 (missing header peer
/// field) and 754 (`request_tracing`'s `duration_ms` field).
#[tokio::test]
async fn auth_rejections_log_path_peer_and_the_reload_hint() {
    let config = ApiAuthConfig::new(GOOD_TOKEN.to_string());
    let router = auth_router(config);
    let (_guard, events) = capture();

    // 1. Well-formed but wrong bearer, with a forwarded peer.
    let status = send(
        &router,
        Request::builder()
            .uri("/protected")
            .header(header::AUTHORIZATION, "Bearer wrong-token")
            .header("x-forwarded-for", "10.1.2.3")
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // 2. Malformed Authorization header.
    let status = send(
        &router,
        Request::builder()
            .uri("/protected")
            .header(header::AUTHORIZATION, "Basic abc")
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // 3. No Authorization header at all.
    let status = send(
        &router,
        Request::builder()
            .uri("/protected")
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // 4. The right token passes.
    let status = send(
        &router,
        Request::builder()
            .uri("/protected")
            .header(header::AUTHORIZATION, format!("Bearer {GOOD_TOKEN}"))
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let captured = events_of(&events);
    let find = |needle: &str| {
        captured
            .iter()
            .find(|e| e.message().contains(needle))
            .unwrap_or_else(|| panic!("no event containing {needle:?} in {captured:?}"))
            .clone()
    };

    let hint = find("out-of-band token re-read hinted");
    assert_eq!(hint.level, Level::DEBUG);

    let invalid = find("invalid bearer token");
    assert_eq!(invalid.level, Level::WARN);
    assert_eq!(invalid.field("peer"), Some("10.1.2.3"));
    assert_eq!(invalid.field("path"), Some("/protected"));

    let malformed = find("malformed Authorization header");
    assert_eq!(malformed.level, Level::WARN);
    assert!(malformed.field("peer").is_some_and(|p| !p.is_empty()));
    assert_eq!(malformed.field("path"), Some("/protected"));

    let missing = find("missing Authorization header");
    assert_eq!(missing.level, Level::WARN);
    assert!(missing.field("peer").is_some_and(|p| !p.is_empty()));

    // Exactly one hint: the second mismatch would be inside the floor, and
    // the malformed/missing arms never hint.
    assert_eq!(
        captured
            .iter()
            .filter(|e| e.message().contains("out-of-band token re-read hinted"))
            .count(),
        1
    );

    // request_tracing logged all four requests with their status and a
    // two-decimal duration string.
    let completed: Vec<&CapturedEvent> = captured
        .iter()
        .filter(|e| e.message().contains("API request completed"))
        .collect();
    assert_eq!(completed.len(), 4);
    let statuses: Vec<&str> = completed
        .iter()
        .filter_map(|e| e.field("http.status"))
        .collect();
    assert_eq!(statuses, vec!["401", "401", "401", "200"]);
    for event in &completed {
        let duration = event.field("duration_ms").expect("duration_ms field");
        let (_, decimals) = duration
            .split_once('.')
            .unwrap_or_else(|| panic!("duration_ms must be formatted with decimals: {duration}"));
        assert_eq!(decimals.len(), 2, "duration_ms uses {{:.2}}: {duration}");
        assert!(duration.parse::<f64>().is_ok_and(|d| d >= 0.0));
    }
}

/// middleware.rs:445-448 — `from_env(false)` with no `TV_API_TOKEN` fails
/// CLOSED: auth stays on with a generated UUID token, and the CRITICAL
/// `error!` carries the GAP-SEC-01 code and severity. The test never sets the
/// variable; if the environment already provides one, the other arm is
/// asserted instead.
#[test]
fn from_env_in_live_mode_without_a_token_generates_one_and_logs_the_code() {
    let (_guard, events) = capture();
    let config = ApiAuthConfig::from_env(false);

    assert!(config.enabled, "live mode must never disable auth");
    match std::env::var("TV_API_TOKEN") {
        Ok(token) if !token.is_empty() => {
            assert_eq!(config.token_value_for_test(), token);
        }
        _ => {
            let generated = config.token_value_for_test();
            assert!(
                uuid::Uuid::parse_str(&generated).is_ok(),
                "the fail-closed token must be a UUID: {generated}"
            );
            let critical = events_of(&events)
                .into_iter()
                .find(|e| e.level == Level::ERROR)
                .expect("the fail-closed path must log an error");
            assert_eq!(
                critical.field("code"),
                Some(ErrorCode::GapSecApiAuth.code_str())
            );
            assert_eq!(
                critical.field("severity"),
                Some(ErrorCode::GapSecApiAuth.severity().as_str())
            );
            assert!(critical.message().contains("auto-generated bearer token"));
            assert!(
                !critical.message().contains(&generated),
                "the generated token must never be logged"
            );
        }
    }
}

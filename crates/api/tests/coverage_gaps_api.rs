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
//! Each async test uses a current-thread runtime, so the tracing subscriber
//! remains local to that test. Handler persistence and environment-based auth
//! run in bounded Rust child tests with an owned working directory and explicit
//! token inputs. The parent never changes its environment or working directory.
//! Files belong to exclusively created temporary directories; HTTP fixtures use
//! loopback and report completed requests and I/O errors through joined tasks.

use std::io;
use std::path::{Path as FsPath, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

#[derive(Debug)]
struct OwnedDir(PathBuf);

impl OwnedDir {
    fn try_create(path: PathBuf) -> io::Result<Self> {
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }

    fn path(&self) -> &FsPath {
        &self.0
    }
}

impl Drop for OwnedDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn unique_temp_dir(tag: &str) -> OwnedDir {
    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);
    for _ in 0..32 {
        let sequence = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "tv-api-covgap-{tag}-{}-{sequence}",
            std::process::id()
        ));
        match OwnedDir::try_create(path) {
            Ok(owner) => return owner,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => panic!("create owned API test directory: {error}"),
        }
    }
    panic!("API test directory names collided on every bounded attempt");
}

const CHILD_TEST_ENV: &str = "TV_COVERAGE_API_CHILD_TEST";
const CHILD_CASE_ENV: &str = "TV_COVERAGE_API_CHILD_CASE";
const FEED_CHILD_TEST: &str = "enabling_truedata_flips_the_flag_and_logs_the_feed_label";
const AUTH_CHILD_TEST: &str =
    "from_env_in_live_mode_without_a_token_generates_one_and_logs_the_code";

fn child_case(test_name: &str) -> Option<String> {
    if std::env::var(CHILD_TEST_ENV).ok().as_deref() == Some(test_name) {
        Some(std::env::var(CHILD_CASE_ENV).expect("child case is explicit"))
    } else {
        None
    }
}

async fn run_isolated_test(test_name: &str, case: &str, root: &FsPath, token: Option<&str>) {
    let mut command =
        tokio::process::Command::new(std::env::current_exe().expect("test executable"));
    command
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .current_dir(root)
        .env(CHILD_TEST_ENV, test_name)
        .env(CHILD_CASE_ENV, case)
        .env_remove("TV_API_TOKEN")
        .kill_on_drop(true);
    if let Some(token) = token {
        command.env("TV_API_TOKEN", token);
    }
    let output = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .expect("isolated API test timed out")
        .expect("isolated API test must start");
    assert!(
        output.status.success(),
        "isolated test failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("running 1 test"),
        "child selected no test: {stdout}"
    );
    assert!(
        stdout.contains("1 passed; 0 failed"),
        "child did not pass: {stdout}"
    );
}

fn assert_token_not_logged(events: &[CapturedEvent], token: &str) {
    assert!(
        !events
            .iter()
            .flat_map(|event| &event.fields)
            .any(|(_, value)| value.contains(token)),
        "captured event leaked token material"
    );
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
struct MockQuestDb {
    port: u16,
    task: tokio::task::JoinHandle<io::Result<Vec<String>>>,
}

impl MockQuestDb {
    async fn finish(&mut self) -> io::Result<Vec<String>> {
        tokio::time::timeout(Duration::from_secs(6), &mut self.task)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "mock completion timed out"))?
            .map_err(io::Error::other)?
    }
}

impl Drop for MockQuestDb {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start_mock_questdb(responses: Vec<&'static str>) -> MockQuestDb {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind should succeed");
    let port = listener.local_addr().expect("local_addr").port();
    let task = tokio::spawn(async move {
        let mut requests = Vec::with_capacity(responses.len());
        for body in responses {
            let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "mock accept timed out"))??;
            let mut request = Vec::with_capacity(1024);
            let mut buf = [0_u8; 1024];
            loop {
                let count = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
                    .await
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::TimedOut, "mock read timed out")
                    })??;
                if count == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "incomplete HTTP request",
                    ));
                }
                request.extend_from_slice(&buf[..count]);
                if request.len() > 8192 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "HTTP headers exceed fixture bound",
                    ));
                }
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = std::str::from_utf8(&request)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            let request_line = request.lines().next().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "missing request line")
            })?;
            requests.push(request_line.to_owned());
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            tokio::time::timeout(
                Duration::from_secs(5),
                stream.write_all(response.as_bytes()),
            )
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "mock write timed out"))??;
            tokio::time::timeout(Duration::from_secs(5), stream.shutdown())
                .await
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::TimedOut, "mock shutdown timed out")
                })??;
        }
        Ok(requests)
    });
    MockQuestDb { port, task }
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
    let mut parent = root.path().to_path_buf();
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

    let root_path = root.path().to_path_buf();
    drop(root);
    assert!(!root_path.exists(), "owned temp root must be removed");
}

/// feed_state_persist.rs:224 — a read error other than NotFound (the
/// canonical path is a DIRECTORY, so `read` fails with EISDIR) falls back to
/// `None`, i.e. the config default.
#[test]
fn load_feed_state_returns_none_when_the_path_is_unreadable() {
    let root = unique_temp_dir("unreadable");
    let path = root.path().join("data").join("feed-state.json");
    std::fs::create_dir_all(&path).expect("make the overlay path a directory");

    // Guard against a vacuous pass: the read really does fail with a
    // non-NotFound kind, which is the arm under test.
    let read_err = std::fs::read(&path).expect_err("reading a directory must fail");
    assert_ne!(read_err.kind(), std::io::ErrorKind::NotFound);

    assert_eq!(load_feed_state(&path), None);

    let root_path = root.path().to_path_buf();
    drop(root);
    assert!(!root_path.exists(), "owned temp root must be removed");
}

// ---------------------------------------------------------------------------
// handlers/feeds.rs
// ---------------------------------------------------------------------------

/// feeds.rs:212 — the `info!` emitted when a toggleable non-Dhan feed is
/// actually flipped carries the feed's label. TrueData is runtime-toggleable,
/// so its enable runs `set_enabled` and the log.
///
/// The handler's fixed relative persistence path is exercised inside a child
/// process whose working directory belongs to this test, never the checkout.
#[tokio::test]
async fn enabling_truedata_flips_the_flag_and_logs_the_feed_label() {
    if child_case(FEED_CHILD_TEST).is_none() {
        let root = unique_temp_dir("feed-child");
        let cwd_before = std::env::current_dir().expect("parent cwd");
        let sentinel = root.path().join("untouched");
        std::fs::write(&sentinel, b"keep").expect("owned sentinel");
        let overlay = root.path().join("data/feed-state.json");
        assert!(!overlay.exists());
        run_isolated_test(FEED_CHILD_TEST, "feed", root.path(), None).await;
        assert_eq!(std::env::current_dir().expect("parent cwd"), cwd_before);
        assert_eq!(std::fs::read(sentinel).expect("sentinel survives"), b"keep");
        assert!(
            load_feed_state(&overlay).is_some(),
            "child must persist into its owned cwd"
        );
        return;
    }
    assert_eq!(child_case(FEED_CHILD_TEST).as_deref(), Some("feed"));
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
    let persisted = load_feed_state(
        &std::env::current_dir()
            .expect("child cwd")
            .join("data/feed-state.json"),
    )
    .expect("the real handler must persist in the isolated cwd");
    // The current on-disk schema stores only Dhan's enabled flag plus its
    // write timestamp, not the runtime-only lane fields or TrueData flag.
    assert_eq!(
        persisted.dhan_enabled,
        state.feed_runtime().snapshot().dhan_enabled
    );
    assert!(
        chrono::NaiveDateTime::parse_from_str(&persisted.updated_at_ist, "%Y-%m-%d %H:%M:%S")
            .is_ok(),
        "the persisted snapshot carries its documented IST timestamp"
    );
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
    let mut mock = start_mock_questdb(vec![
        "{\"query\":\"x\",\"error\":\"table does not exist [table=ticks]\",\"position\":0}",
        "{\"query\":\"SHOW TABLES\",\"dataset\":[]}",
    ])
    .await;
    let state = state_with_questdb("127.0.0.1", mock.port, Arc::new(SystemHealthStatus::new()));

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

    let requests = mock
        .finish()
        .await
        .expect("both mock requests must complete without I/O failure");
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with("GET /exec?query="));
    assert!(requests[0].contains("security_id"));
    assert!(requests[0].contains("42"));
    assert_eq!(requests[1], "GET /exec?query=SHOW+TABLES HTTP/1.1");

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
/// variable in the parent. Separate child cases remove it or supply a known
/// synthetic value, so neither branch depends on the operator's environment.
#[tokio::test]
async fn from_env_in_live_mode_without_a_token_generates_one_and_logs_the_code() {
    let Some(case) = child_case(AUTH_CHILD_TEST) else {
        let root = unique_temp_dir("auth-child");
        let cwd_before = std::env::current_dir().expect("parent cwd");
        run_isolated_test(AUTH_CHILD_TEST, "missing-token", root.path(), None).await;
        run_isolated_test(
            AUTH_CHILD_TEST,
            "fixed-token",
            root.path(),
            Some(GOOD_TOKEN),
        )
        .await;
        assert_eq!(std::env::current_dir().expect("parent cwd"), cwd_before);
        return;
    };
    match case.as_str() {
        "missing-token" => assert!(std::env::var_os("TV_API_TOKEN").is_none()),
        "fixed-token" => assert_eq!(std::env::var("TV_API_TOKEN").as_deref(), Ok(GOOD_TOKEN)),
        _ => panic!("unexpected isolated auth case"),
    }
    let (_guard, events) = capture();
    let config = ApiAuthConfig::from_env(false);

    assert!(config.enabled, "live mode must never disable auth");
    match case.as_str() {
        "fixed-token" => {
            assert_eq!(config.token_value_for_test(), GOOD_TOKEN);
            assert_token_not_logged(&events_of(&events), GOOD_TOKEN);
        }
        "missing-token" => {
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
            assert_token_not_logged(&events_of(&events), &generated);
        }
        _ => panic!("unexpected isolated auth case"),
    }
}

#[test]
fn test_owned_directory_collision_preserves_existing_contents() {
    let root = unique_temp_dir("collision-parent");
    let existing = root.path().join("existing");
    std::fs::create_dir(&existing).expect("existing owner directory");
    let sentinel = existing.join("sentinel");
    std::fs::write(&sentinel, b"keep").expect("sentinel");
    let error = OwnedDir::try_create(existing.clone()).expect_err("existing directory is not ours");
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(
        std::fs::read(&sentinel).expect("sentinel survives refusal"),
        b"keep"
    );
    let root_path = root.path().to_path_buf();
    drop(root);
    assert!(
        !root_path.exists(),
        "only the successful owner cleans its tree"
    );
}

#[tokio::test]
async fn test_mock_completion_refuses_an_incomplete_request() {
    use tokio::io::AsyncWriteExt;
    let mut mock = start_mock_questdb(vec!["{}"]).await;
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", mock.port))
        .await
        .expect("connect fixture");
    stream
        .write_all(b"GET /incomplete")
        .await
        .expect("partial request");
    stream.shutdown().await.expect("finish partial request");
    let error = mock
        .finish()
        .await
        .expect_err("incomplete requests cannot count as completed cases");
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
}

#[test]
#[should_panic(expected = "captured event leaked token material")]
fn test_redaction_assertion_checks_later_structured_fields() {
    let (_guard, events) = capture();
    tracing::info!("first ordinary event");
    tracing::warn!(
        token = GOOD_TOKEN,
        "second event without token in its message"
    );
    let captured = events_of(&events);
    assert_eq!(captured.len(), 2);
    assert!(!captured[1].message().contains(GOOD_TOKEN));
    assert_token_not_logged(&captured, GOOD_TOKEN);
}

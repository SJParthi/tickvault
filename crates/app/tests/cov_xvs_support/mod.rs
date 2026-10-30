//! Shared helpers for the `cov_xvs_*` coverage tests: a tiny QuestDB-shaped
//! HTTP server on 127.0.0.1 (std threads, so it does not depend on the test's
//! tokio clock), a tracing subscriber that evaluates every log line and keeps
//! the `tickvault*` events for assertions, a paused-clock driver that never
//! lets the clock jump while an HTTP call is in flight, a binary-wide
//! working-directory lock with a restore-on-drop guard, and a once-only
//! `NO_PROXY` setup.

#![allow(dead_code)]

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// Decodes `application/x-www-form-urlencoded` text.
pub fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// Proxy
// ---------------------------------------------------------------------------

/// Appends `127.0.0.1,localhost` to `NO_PROXY` and `no_proxy`, once per test
/// binary. Call it FIRST in every test, before any HTTP client is built.
///
/// Why: the QuestDB ILP writer (questdb-rs 6.1.0) uses ureq 3.1.4, whose
/// default agent config reads `Proxy::try_from_env()` — the FIRST of
/// `ALL_PROXY`, `HTTPS_PROXY`, `HTTP_PROXY` (and lowercase) for EVERY request,
/// whatever its scheme — and bypasses it only for hosts in `NO_PROXY` /
/// `no_proxy` (`NoProxy::try_from_env`, checked in ureq's `run.rs`). So a CI
/// runner with `HTTPS_PROXY` set and no `NO_PROXY` would send the ILP write
/// for `http://127.0.0.1:<port>/write` to the proxy. reqwest's system proxy
/// honours `NO_PROXY` too (the probe client also sets `.no_proxy()`).
pub fn init_no_proxy() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        for key in ["NO_PROXY", "no_proxy"] {
            let value = match std::env::var(key) {
                Ok(v) if !v.trim().is_empty() => format!("{v},127.0.0.1,localhost"),
                _ => "127.0.0.1,localhost".to_owned(),
            };
            // SAFETY: runs once, as the first statement of every test in the
            // binary and before any HTTP client or other env reader in it;
            // the OnceLock makes concurrent tests wait for this write.
            unsafe { std::env::set_var(key, value) };
        }
    });
}

// ---------------------------------------------------------------------------
// Fake QuestDB
// ---------------------------------------------------------------------------

/// What the fake server saw.
#[derive(Default)]
pub struct Seen {
    /// Decoded `query` of every `GET /exec`.
    pub exec_queries: Vec<String>,
    /// Body of every `POST /write` (ILP over HTTP).
    pub writes: Vec<String>,
    /// Every request line.
    pub request_lines: Vec<String>,
}

/// Answers an `/exec` query with `(status, body)`.
pub type ExecResponder = Arc<dyn Fn(&str) -> (u16, String) + Send + Sync>;

/// Connections accepted and connections fully answered (or dropped).
#[derive(Default)]
pub struct Activity {
    started: AtomicUsize,
    finished: AtomicUsize,
}

/// A QuestDB-shaped HTTP server bound to 127.0.0.1:0.
pub struct FakeQuestDb {
    pub port: u16,
    pub seen: Arc<Mutex<Seen>>,
    pub activity: Arc<Activity>,
}

impl FakeQuestDb {
    /// Starts the server. `/exec` is answered by `exec`; `/write` with 204;
    /// anything else (e.g. `/settings`) with 404.
    pub fn start(exec: ExecResponder) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
        let port = listener.local_addr().expect("local addr").port();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let activity = Arc::new(Activity::default());
        let seen_srv = Arc::clone(&seen);
        let activity_srv = Arc::clone(&activity);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                activity_srv.started.fetch_add(1, Ordering::SeqCst);
                let seen = Arc::clone(&seen_srv);
                let exec = Arc::clone(&exec);
                let activity = Arc::clone(&activity_srv);
                std::thread::spawn(move || {
                    serve_one(stream, &seen, &exec);
                    activity.finished.fetch_add(1, Ordering::SeqCst);
                });
            }
        });
        Self {
            port,
            seen,
            activity,
        }
    }

    /// Default responder: DDL OK, counts 0, empty datasets.
    pub fn start_default() -> Self {
        Self::start(Arc::new(|_q: &str| {
            (
                200,
                r#"{"query":"q","columns":[],"dataset":[],"count":0,"ddl":"OK"}"#.to_owned(),
            )
        }))
    }

    pub fn writes(&self) -> Vec<String> {
        self.seen
            .lock()
            .map(|s| s.writes.clone())
            .unwrap_or_default()
    }

    /// Every ILP line written to `table` (lines start with the table name
    /// followed by `,` or a space).
    pub fn ilp_lines(&self, table: &str) -> Vec<String> {
        self.writes()
            .iter()
            .flat_map(|w| w.lines().map(str::to_owned).collect::<Vec<_>>())
            .filter(|l| {
                l.strip_prefix(table)
                    .is_some_and(|rest| rest.starts_with(',') || rest.starts_with(' '))
            })
            .collect()
    }

    pub fn exec_queries(&self) -> Vec<String> {
        self.seen
            .lock()
            .map(|s| s.exec_queries.clone())
            .unwrap_or_default()
    }

    pub fn request_lines(&self) -> Vec<String> {
        self.seen
            .lock()
            .map(|s| s.request_lines.clone())
            .unwrap_or_default()
    }

    /// `(connections accepted, connections finished)`.
    pub fn activity(&self) -> (usize, usize) {
        (
            self.activity.started.load(Ordering::SeqCst),
            self.activity.finished.load(Ordering::SeqCst),
        )
    }

    pub fn questdb_config(&self) -> tickvault_common::config::QuestDbConfig {
        tickvault_common::config::QuestDbConfig {
            host: "127.0.0.1".to_owned(),
            http_port: self.port,
            pg_port: 1,
            ilp_port: 1,
        }
    }

    pub fn exec_url(&self) -> String {
        format!("http://127.0.0.1:{}/exec", self.port)
    }
}

fn serve_one(stream: TcpStream, seen: &Arc<Mutex<Seen>>, exec: &ExecResponder) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let Ok(write_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let mut writer = write_half;
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let mut content_length = 0_usize;
    let mut chunked = false;
    let mut expect_continue = false;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        let lower = trimmed.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        } else if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
            chunked = true;
        } else if lower.starts_with("expect:") && lower.contains("100-continue") {
            expect_continue = true;
        }
    }
    if expect_continue {
        let _ = writer.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
    }
    let mut body = Vec::new();
    if chunked {
        loop {
            let mut size_line = String::new();
            if reader.read_line(&mut size_line).unwrap_or(0) == 0 {
                break;
            }
            let size = usize::from_str_radix(size_line.trim().split(';').next().unwrap_or("0"), 16)
                .unwrap_or(0);
            if size == 0 {
                let mut end = String::new();
                let _ = reader.read_line(&mut end);
                break;
            }
            let mut chunk = vec![0_u8; size];
            if reader.read_exact(&mut chunk).is_err() {
                break;
            }
            body.extend_from_slice(&chunk);
            let mut crlf = [0_u8; 2];
            let _ = reader.read_exact(&mut crlf);
        }
    } else if content_length > 0 {
        body = vec![0_u8; content_length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let target = parts.next().unwrap_or("").to_owned();
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    let (status, reply) = if method == "GET" && path == "/exec" {
        let q = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("query="))
            .map(url_decode)
            .unwrap_or_default();
        if let Ok(mut s) = seen.lock() {
            s.exec_queries.push(q.clone());
        }
        exec(&q)
    } else if method == "POST" && path == "/write" {
        if let Ok(mut s) = seen.lock() {
            s.writes.push(String::from_utf8_lossy(&body).into_owned());
        }
        (204, String::new())
    } else {
        (404, r#"{"error":"not found"}"#.to_owned())
    };
    if let Ok(mut s) = seen.lock() {
        s.request_lines.push(request_line.trim_end().to_owned());
    }
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        404 => "Not Found",
        _ => "Status",
    };
    let head = if status == 204 {
        format!("HTTP/1.1 204 {reason}\r\nConnection: close\r\n\r\n")
    } else {
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            reply.len()
        )
    };
    let _ = writer.write_all(head.as_bytes());
    let _ = writer.write_all(reply.as_bytes());
    let _ = writer.flush();
}

// ---------------------------------------------------------------------------
// Paused-clock driver
// ---------------------------------------------------------------------------

/// Real-time budget of one driven test. Generous because llvm-cov builds on a
/// loaded CI runner are slow; the driver returns as soon as the effect shows.
pub const REAL_BUDGET: Duration = Duration::from_secs(60);

/// One slice of real time spent with the paused clock held still.
const SETTLE_SLICE: Duration = Duration::from_millis(2);

/// The server must stay idle this long (real time) before the clock moves.
const SETTLE_QUIET: Duration = Duration::from_millis(25);

/// Smallest and largest virtual step between two settles.
const MIN_STEP: Duration = Duration::from_secs(5);
const MAX_STEP: Duration = Duration::from_secs(1_800);

/// Lets REAL time pass while the paused tokio clock stands still, until the
/// fake server has been idle (no connection open, none new) for
/// [`SETTLE_QUIET`]. Returns whether any connection was seen meanwhile.
///
/// The clock stands still because a `spawn_blocking` task is in flight the
/// whole time: tokio's current-thread runtime does not auto-advance a paused
/// clock while a blocking task runs (`BlockingSchedule::new` calls
/// `inhibit_auto_advance`). Without this, a runtime parked on a socket read
/// jumps the clock to the next timer, so an HTTP call's 30 s timeout fires
/// before the local server answers (measured: 90 of 100 local GETs under
/// `start_paused` timed out this way).
pub async fn settle(srv: &FakeQuestDb, real_deadline: std::time::Instant) -> bool {
    let first = srv.activity();
    let mut last = first;
    let mut quiet_since = std::time::Instant::now();
    loop {
        let _ = tokio::task::spawn_blocking(|| std::thread::sleep(SETTLE_SLICE)).await;
        let now = srv.activity();
        if now != last || now.0 != now.1 {
            last = now;
            quiet_since = std::time::Instant::now();
        } else if quiet_since.elapsed() >= SETTLE_QUIET {
            break;
        }
        if std::time::Instant::now() >= real_deadline {
            break;
        }
    }
    last != first
}

/// Drives a paused tokio clock forward until `done()` holds, `horizon` of
/// virtual time has passed, or [`REAL_BUDGET`] of real time is spent. Between
/// steps it [`settle`]s, so the clock never moves while the fake server is
/// handling (or about to receive) a request; the step doubles while the
/// server stays idle and drops back to five seconds as soon as it is not.
/// Returns `done()` at the end. Must run on a `start_paused` current-thread
/// runtime.
pub async fn drive_paused(
    srv: &FakeQuestDb,
    horizon: Duration,
    mut done: impl FnMut() -> bool,
) -> bool {
    let real_deadline = std::time::Instant::now() + REAL_BUDGET;
    let virtual_start = tokio::time::Instant::now();
    let mut step = MIN_STEP;
    loop {
        let busy = settle(srv, real_deadline).await;
        if done() {
            return true;
        }
        if std::time::Instant::now() >= real_deadline
            || tokio::time::Instant::now() - virtual_start >= horizon
        {
            return done();
        }
        step = if busy {
            MIN_STEP
        } else {
            (step * 2).min(MAX_STEP)
        };
        tokio::time::advance(step).await;
    }
}

// ---------------------------------------------------------------------------
// Tracing
// ---------------------------------------------------------------------------

/// One captured `tickvault*` event.
#[derive(Clone, Debug)]
pub struct CapturedEvent {
    pub level: tracing::Level,
    pub target: String,
    pub fields: Vec<(String, String)>,
}

impl CapturedEvent {
    /// The value of `name`, as recorded (strings without quotes).
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The event's message.
    pub fn message(&self) -> &str {
        self.field("message").unwrap_or("")
    }
}

static CAPTURED: Mutex<Vec<CapturedEvent>> = Mutex::new(Vec::new());

struct FieldVisitor<'a>(&'a mut Vec<(String, String)>);

impl tracing::field::Visit for FieldVisitor<'_> {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.push((field.name().to_owned(), value.to_owned()));
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push((field.name().to_owned(), format!("{value:?}")));
    }
}

struct CaptureLayer;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let meta = event.metadata();
        if !meta.target().starts_with("tickvault") {
            return;
        }
        let mut fields = Vec::new();
        event.record(&mut FieldVisitor(&mut fields));
        if let Ok(mut all) = CAPTURED.lock() {
            all.push(CapturedEvent {
                level: *meta.level(),
                target: meta.target().to_owned(),
                fields,
            });
        }
    }
}

/// Installs, once per test binary, a TRACE-level subscriber that formats every
/// line to a sink (so the fields of every log line on a driven path are
/// evaluated) and keeps every `tickvault*` event for [`captured_events`].
pub fn init_tracing() {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let _ = tracing_subscriber::registry()
            .with(tracing_subscriber::filter::LevelFilter::TRACE)
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::sink))
            .with(CaptureLayer)
            .try_init();
    });
}

/// A copy of every `tickvault*` event captured so far in this test binary.
pub fn captured_events() -> Vec<CapturedEvent> {
    CAPTURED.lock().map(|v| v.clone()).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Working directory
// ---------------------------------------------------------------------------

/// One lock per test binary, taken by every test in it that changes the
/// working directory or reads a path relative to it. An async mutex, so the
/// guard can be held across `.await` (and a panicking test releases it as
/// its guard drops during unwind).
static CWD_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Takes the binary-wide working-directory lock.
pub async fn lock_cwd() -> tokio::sync::MutexGuard<'static, ()> {
    CWD_LOCK.lock().await
}

/// A unique folder under the OS temp dir, removed when dropped (also on
/// panic).
pub struct TempRoot {
    pub path: PathBuf,
}

impl TempRoot {
    pub fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path =
            std::env::temp_dir().join(format!("tv-cov-xvs-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Moves the process into a fresh temp folder; on drop (also on panic)
/// moves back to the original working directory, then removes the folder.
/// Hold the [`lock_cwd`] guard for at least as long as this.
pub struct CwdGuard {
    original: PathBuf,
    root: TempRoot,
}

impl CwdGuard {
    pub fn enter(tag: &str) -> Self {
        let original = std::env::current_dir().expect("cwd");
        let root = TempRoot::new(tag);
        std::env::set_current_dir(&root.path).expect("chdir temp");
        Self { original, root }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.root.path
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original);
        // `root` drops after this body, removing the folder.
    }
}

// ---------------------------------------------------------------------------
// Clock and calendar
// ---------------------------------------------------------------------------

/// Today's IST date and seconds of day, from the wall clock.
pub fn ist_now() -> (chrono::NaiveDate, u32) {
    use chrono::Timelike as _;
    let now = chrono::Utc::now() + chrono::TimeDelta::seconds(19_800);
    let naive = now.naive_utc();
    (naive.date(), naive.num_seconds_from_midnight())
}

/// Whether `secs` is within `margin` seconds of `edge` (either side): the
/// wall clock the task reads may differ from the test's reading by that much,
/// so an expectation decided on a boundary is not asserted.
pub fn near(secs: u32, edge: u32, margin: u32) -> bool {
    secs.abs_diff(edge) <= margin
}

/// A calendar with no holidays: every Monday to Friday is a trading day.
pub fn weekday_calendar() -> Arc<tickvault_common::trading_calendar::TradingCalendar> {
    let cfg = tickvault_common::config::TradingConfig {
        market_open_time: "09:15:00".to_owned(),
        market_close_time: "15:30:00".to_owned(),
        order_cutoff_time: "15:29:00".to_owned(),
        data_collection_start: "09:00:00".to_owned(),
        data_collection_end: "16:00:00".to_owned(),
        timezone: "Asia/Kolkata".to_owned(),
        max_orders_per_second: 10,
        nse_holidays: Vec::new(),
        muhurat_trading_dates: Vec::new(),
        nse_mock_trading_dates: Vec::new(),
    };
    Arc::new(
        tickvault_common::trading_calendar::TradingCalendar::from_config(&cfg)
            .expect("calendar builds"),
    )
}

/// Whether `d` is Monday to Friday.
pub fn is_weekday(d: chrono::NaiveDate) -> bool {
    use chrono::Datelike as _;
    !matches!(d.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun)
}

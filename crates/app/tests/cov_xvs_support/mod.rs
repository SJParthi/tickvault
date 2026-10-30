//! Shared helpers for the `cov_xvs_*` coverage tests: a tiny QuestDB-shaped
//! HTTP server on 127.0.0.1 (std threads, so it does not depend on the test's
//! tokio clock), a sink tracing subscriber (so the fields of every log line
//! on the driven paths are evaluated), and a temp working directory.

#![allow(dead_code)]

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

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

/// A QuestDB-shaped HTTP server bound to 127.0.0.1:0.
pub struct FakeQuestDb {
    pub port: u16,
    pub seen: Arc<Mutex<Seen>>,
}

impl FakeQuestDb {
    /// Starts the server. `/exec` is answered by `exec`; `/write` with 204;
    /// anything else (e.g. `/settings`) with 404.
    pub fn start(exec: ExecResponder) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
        let port = listener.local_addr().expect("local addr").port();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let seen_srv = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let seen = Arc::clone(&seen_srv);
                let exec = Arc::clone(&exec);
                std::thread::spawn(move || serve_one(stream, &seen, &exec));
            }
        });
        Self { port, seen }
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

    pub fn exec_queries(&self) -> Vec<String> {
        self.seen
            .lock()
            .map(|s| s.exec_queries.clone())
            .unwrap_or_default()
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
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
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

/// Installs a TRACE-level subscriber that writes to a sink, once per test
/// binary, so the fields of every log line on a driven path are evaluated.
pub fn init_tracing() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let _ = tracing_subscriber::fmt()
            .with_writer(std::io::sink)
            .with_max_level(tracing::Level::TRACE)
            .try_init();
    });
}

/// A unique folder under the OS temp dir; removed by [`TempRoot::cleanup`].
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

    pub fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Today's IST date and seconds of day, from the wall clock.
pub fn ist_now() -> (chrono::NaiveDate, u32) {
    use chrono::Timelike as _;
    let now = chrono::Utc::now() + chrono::TimeDelta::seconds(19_800);
    let naive = now.naive_utc();
    (naive.date(), naive.num_seconds_from_midnight())
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

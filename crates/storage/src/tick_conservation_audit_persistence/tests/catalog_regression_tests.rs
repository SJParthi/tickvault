//! Catalog and journal regressions promoted from the November 25 local probes.
//! The HTTP acknowledgements here are fixtures, not database durability proof.

use super::*;
use crate::audit_spill::{
    drain_audit_spill_dir,
    test_support::{OK_204, TestDir},
};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const FIXTURE_IO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
struct Request {
    method: String,
    target: String,
    body: Vec<u8>,
    received: Instant,
}

impl Request {
    fn sql(&self) -> String {
        assert_eq!(self.method, "GET");
        let url = reqwest::Url::parse(&format!("http://fixture{}", self.target))
            .expect("fixture request URL");
        assert_eq!(url.path(), "/exec");
        url.query_pairs()
            .find_map(|(name, value)| (name == "query").then(|| value.into_owned()))
            .expect("SQL query")
    }
}

enum Expected {
    Catalog,
    Ddl(String),
    Write(Vec<u8>),
}

impl Expected {
    fn check(&self, request: &Request) {
        match self {
            Self::Catalog => assert_eq!(request.sql(), CATALOG_SQL.as_str()),
            Self::Ddl(sql) => assert_eq!(&request.sql(), sql),
            Self::Write(bytes) => {
                assert_eq!(request.method, "POST");
                assert!(request.target.starts_with("/write?"));
                assert_eq!(
                    &request.body, bytes,
                    "receipt identity and values must survive"
                );
            }
        }
    }
}

/// A bounded loopback fixture that reads the complete request before replying.
/// Stop and join on every exit, including a failed test assertion.
struct ScriptedServer {
    url: String,
    stop: Arc<AtomicBool>,
    task: Option<std::thread::JoinHandle<Vec<Request>>>,
    expected: usize,
}

impl ScriptedServer {
    fn new(expected: usize, mut reply: impl FnMut(&Request) -> Vec<u8> + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback fixture");
        listener.set_nonblocking(true).expect("nonblocking accept");
        let url = format!("http://{}/write", listener.local_addr().expect("address"));
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::clone(&stop);
        let task = std::thread::spawn(move || {
            let mut requests = Vec::new();
            while !done.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        socket.set_nonblocking(false).expect("blocking fixture I/O");
                        socket
                            .set_write_timeout(Some(FIXTURE_IO_TIMEOUT))
                            .expect("write deadline");
                        let request = read_request(&mut socket);
                        let response = reply(&request);
                        requests.push(request);
                        // An empty response deliberately closes the connection before headers.
                        if !response.is_empty() {
                            socket.write_all(&response).expect("fixture response");
                            socket.flush().expect("flush response");
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("fixture accept failed: {error}"),
                }
            }
            requests
        });
        Self {
            url,
            stop,
            task: Some(task),
            expected,
        }
    }

    fn sequence(exchanges: Vec<(Expected, Vec<u8>)>) -> Self {
        let count = exchanges.len();
        let mut exchanges = exchanges.into_iter();
        Self::new(count, move |request| {
            let (expected, response) = exchanges.next().expect("unexpected extra request");
            expected.check(request);
            response
        })
    }

    fn finish(mut self) -> Vec<Request> {
        self.stop.store(true, Ordering::Release);
        let requests = self
            .task
            .take()
            .expect("server task")
            .join()
            .expect("server assertions");
        assert_eq!(
            requests.len(),
            self.expected,
            "every scripted exchange must run"
        );
        requests
    }
}

impl Drop for ScriptedServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

fn read_request(socket: &mut TcpStream) -> Request {
    const MAX_REQUEST_BYTES: usize = 65_536;
    let deadline = Instant::now() + FIXTURE_IO_TIMEOUT;
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 2048];
    let header_end = loop {
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break index + 4;
        }
        let count = read_before_deadline(socket, &mut chunk, deadline);
        assert!(count > 0, "incomplete request headers");
        bytes.extend_from_slice(&chunk[..count]);
        assert!(bytes.len() <= MAX_REQUEST_BYTES, "oversize fixture request");
    };
    let headers = std::str::from_utf8(&bytes[..header_end]).expect("HTTP headers");
    let mut request_line = headers
        .lines()
        .next()
        .expect("request line")
        .split_whitespace();
    let method = request_line.next().expect("method").to_owned();
    let target = request_line.next().expect("target").to_owned();
    let length = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find_map(|(name, value)| {
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().expect("content length"))
        })
        .unwrap_or(0);
    assert!(
        length <= MAX_REQUEST_BYTES - header_end,
        "oversize request body"
    );
    while bytes.len() < header_end + length {
        let count = read_before_deadline(socket, &mut chunk, deadline);
        assert!(count > 0, "incomplete request body");
        bytes.extend_from_slice(&chunk[..count]);
        assert!(bytes.len() <= MAX_REQUEST_BYTES, "oversize fixture request");
    }
    assert_eq!(
        bytes.len(),
        header_end + length,
        "unexpected trailing request bytes"
    );
    Request {
        method,
        target,
        body: bytes[header_end..].to_vec(),
        received: Instant::now(),
    }
}

fn read_before_deadline(socket: &mut TcpStream, chunk: &mut [u8], deadline: Instant) -> usize {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .expect("fixture request exceeded its I/O deadline");
    socket
        .set_read_timeout(Some(remaining))
        .expect("read deadline");
    socket.read(chunk).expect("complete fixture request")
}

fn response(status: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn ready() -> Vec<u8> {
    response("200 OK", CATALOG_BODY)
}

fn applying() -> Vec<u8> {
    let mut body: serde_json::Value = serde_json::from_str(CATALOG_BODY).expect("catalog fixture");
    for row in body["dataset"].as_array_mut().expect("catalog rows") {
        row[10] = serde_json::json!(row[9].as_i64().expect("writer transaction") + 1);
    }
    response("200 OK", &body.to_string())
}

fn missing() -> Vec<u8> {
    MISSING_TABLE.as_bytes().to_vec()
}

fn receipt_bytes() -> Vec<u8> {
    let mut writer = TickConservationAuditWriter::for_test();
    writer.append_row(&sample()).expect("encode receipt");
    writer.buffer.as_bytes().to_vec()
}

async fn deliver(server: &ScriptedServer, spool: &AuditSpill) -> AuditReceiptWrite {
    let config = fixture_config(&server.url);
    let mut writer = TickConservationAuditWriter::new(&config);
    writer.append_row(&sample()).expect("append receipt");
    tokio::time::timeout(
        Duration::from_secs(10),
        writer.deliver_receipt(&config, spool),
    )
    .await
    .expect("bounded receipt delivery")
    .expect("receipt outcome")
}

fn preserved_journal(dir: &std::path::Path, expected: &[u8]) -> std::path::PathBuf {
    let files = crate::tick_spill_replay::list_spill_files(dir);
    assert_eq!(files.len(), 1, "one complete receipt must remain");
    assert_eq!(std::fs::read(&files[0]).expect("journal bytes"), expected);
    files[0].clone()
}

async fn replay_once_then_empty(dir: &std::path::Path, original: Vec<u8>) {
    let server = ScriptedServer::sequence(vec![
        (Expected::Catalog, ready()),
        (Expected::Write(original), OK_204.as_bytes().to_vec()),
    ]);
    let client = crate::http_client::build_probe_client(3).expect("replay client");
    let url = format!("{}?precision=n", server.url);
    let result =
        drain_audit_spill_dir(dir, AuditSpillTable::TickConservationAudit, &url, &client).await;
    assert!(!result.failed);
    assert_eq!(result.files_replayed, 1);
    assert_eq!(result.rows_replayed, 1);
    assert!(crate::tick_spill_replay::list_spill_files(dir).is_empty());
    let second =
        drain_audit_spill_dir(dir, AuditSpillTable::TickConservationAudit, &url, &client).await;
    assert!(!second.failed);
    assert_eq!(second.files_replayed, 0);
    assert_eq!(second.rows_replayed, 0);
    server.finish(); // An empty drain must not send another catalog query or POST.
}

fn assert_poll_delays(requests: &[Request]) {
    let catalog: Vec<_> = requests
        .iter()
        .filter(|request| request.method == "GET")
        .collect();
    let mut minimum = CATALOG_INITIAL_WAIT_MS;
    for pair in catalog.windows(2) {
        assert!(
            pair[1].received.duration_since(pair[0].received) >= Duration::from_millis(minimum),
            "catalog was polled before its {minimum}ms backoff"
        );
        minimum = (minimum * 2).min(CATALOG_MAX_WAIT_MS);
    }
}

#[tokio::test]
async fn catalog_apply_backoff_waits_until_ready_before_posting() {
    let original = receipt_bytes();
    let mut exchanges = (0..3)
        .map(|_| (Expected::Catalog, applying()))
        .collect::<Vec<_>>();
    exchanges.extend([
        (Expected::Catalog, ready()),
        (Expected::Write(original), OK_204.as_bytes().to_vec()),
    ]);
    let server = ScriptedServer::sequence(exchanges);
    let dir = TestDir::new();
    let spool = AuditSpill::in_dir(AuditSpillTable::TickConservationAudit, dir.path().into());
    assert_eq!(
        deliver(&server, &spool).await,
        AuditReceiptWrite::Acknowledged
    );
    assert!(crate::tick_spill_replay::list_spill_files(dir.path()).is_empty());
    let requests = server.finish();
    assert_poll_delays(&requests);
}

#[tokio::test]
async fn catalog_apply_exhaustion_preserves_exact_receipt_for_later_replay() {
    let original = receipt_bytes();
    let server = ScriptedServer::sequence(
        (0..=CATALOG_APPLY_POLLS)
            .map(|_| (Expected::Catalog, applying()))
            .collect(),
    );
    let dir = TestDir::new();
    let spool = AuditSpill::in_dir(AuditSpillTable::TickConservationAudit, dir.path().into());
    assert_eq!(
        deliver(&server, &spool).await,
        AuditReceiptWrite::QueuedForReplay
    );
    preserved_journal(dir.path(), &original);
    let requests = server.finish();
    assert_poll_delays(&requests);
    replay_once_then_empty(dir.path(), original).await;
}

#[tokio::test]
async fn repaired_schema_is_read_back_before_the_receipt_is_posted() {
    let original = receipt_bytes();
    let mut exchanges = vec![(Expected::Catalog, missing())];
    let statements = tick_conservation_audit_ddl_statements();
    exchanges.extend(
        statements
            .into_iter()
            .map(|sql| (Expected::Ddl(sql), DDL_ACK.as_bytes().to_vec())),
    );
    exchanges.extend([
        (Expected::Catalog, ready()),
        (Expected::Write(original), OK_204.as_bytes().to_vec()),
    ]);
    let server = ScriptedServer::sequence(exchanges);
    let dir = TestDir::new();
    let spool = AuditSpill::in_dir(AuditSpillTable::TickConservationAudit, dir.path().into());
    assert_eq!(
        deliver(&server, &spool).await,
        AuditReceiptWrite::Acknowledged
    );
    assert!(crate::tick_spill_replay::list_spill_files(dir.path()).is_empty());
    server.finish();
}

async fn refused_ddl_then_replay(reply: Vec<u8>) -> AuditReceiptWrite {
    let original = receipt_bytes();
    let first = tick_conservation_audit_ddl_statements().remove(0);
    let server = ScriptedServer::sequence(vec![
        (Expected::Catalog, missing()),
        (Expected::Ddl(first), reply),
    ]);
    let dir = TestDir::new();
    let spool = AuditSpill::in_dir(AuditSpillTable::TickConservationAudit, dir.path().into());
    let outcome = deliver(&server, &spool).await;
    preserved_journal(dir.path(), &original);
    server.finish();
    replay_once_then_empty(dir.path(), original).await;
    outcome
}

#[tokio::test]
async fn ddl_http_refusal_keeps_the_journal_until_a_verified_replay() {
    assert_eq!(
        refused_ddl_then_replay(response("503 Unavailable", r#"{"error":"maintenance"}"#)).await,
        AuditReceiptWrite::QueuedForReplay
    );
}

#[tokio::test]
async fn ddl_transport_disconnect_keeps_the_journal_until_a_verified_replay() {
    assert_eq!(
        refused_ddl_then_replay(Vec::new()).await,
        AuditReceiptWrite::QueuedForReplay
    );
}

#[tokio::test]
async fn acknowledged_receipt_remains_replayable_when_journal_unlink_fails() {
    let dir = TestDir::new();
    let journal_dir = dir.path().join("journal");
    let held_dir = dir.path().join("held");
    let original = receipt_bytes();
    let expected = original.clone();
    let blocked = journal_dir.clone();
    let held = held_dir.clone();
    let mut exchanges = vec![Expected::Catalog, Expected::Write(expected)].into_iter();
    let server = ScriptedServer::new(2, move |request| {
        exchanges.next().expect("unexpected request").check(request);
        if request.method == "GET" {
            return ready();
        }
        preserved_journal(&blocked, &request.body);
        // Force a real unlink error without depending on uid/permission bypasses.
        // Preserve the whole journal directory, then restore it before replay.
        std::fs::rename(&blocked, &held).expect("preserve journal directory");
        std::fs::write(&blocked, b"owned path obstruction").expect("obstruct old directory path");
        OK_204.as_bytes().to_vec()
    });
    let spool = AuditSpill::in_dir(AuditSpillTable::TickConservationAudit, journal_dir.clone());
    assert_eq!(
        deliver(&server, &spool).await,
        AuditReceiptWrite::Acknowledged
    );
    server.finish();
    assert!(
        journal_dir.is_file(),
        "unlink must have encountered the obstruction"
    );
    let retained = preserved_journal(&held_dir, &original);
    assert!(retained.is_file());
    std::fs::remove_file(&journal_dir).expect("remove owned obstruction");
    std::fs::rename(&held_dir, &journal_dir).expect("restore original journal directory");
    preserved_journal(&journal_dir, &original);
    replay_once_then_empty(&journal_dir, original).await;
}

#[tokio::test]
async fn replay_schema_refuses_malformed_or_nonhierarchical_urls() {
    let client = crate::http_client::build_probe_client(3).expect("client");
    for url in [
        "not a URL",
        "mailto:fixture@example.invalid",
        "http://[invalid",
    ] {
        assert!(!ensure_schema_before_replay(&client, url).await, "{url}");
    }
}

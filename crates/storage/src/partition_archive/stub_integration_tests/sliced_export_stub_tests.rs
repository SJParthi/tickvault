//! The ordered export in `ts` slices (`sliced_export`, 2026-11-15) against a
//! stub QuestDB that answers each slice with its own rows: the joined gzip,
//! its SHA-256, the row count and the CSV byte count equal the single
//! ordered query's for the same rows.

use super::*;
use crate::partition_archive::sliced_export::{ExportBusyBackoff, ordered_export_slices};

/// The hour every test exports.
const HOUR: &str = "2026-04-01T09";

/// Header and rows (in DEDUP-key order) of the hour, each without its line
/// end. Rows sit ON slice boundaries (09:10:00, 09:50:00), quoted fields
/// carry commas and newlines (one right after a boundary), and the 09:20
/// and 09:30 slices hold no row (header only).
const HEADER: &str = "\"ts\",\"security_id\",\"note\"";
const ROWS: [&str; 7] = [
    "2026-04-01T09:00:00.000000Z,13,plain",
    "2026-04-01T09:09:59.999999Z,25,\"comma, and\nnewline\"",
    "2026-04-01T09:10:00.000000Z,13,\"\nstarts with a newline\"",
    "2026-04-01T09:10:00.000000Z,25,\"a,b,c\"",
    "2026-04-01T09:41:00.000000Z,51,\"x\"\"quoted\"\"\"",
    "2026-04-01T09:50:00.000000Z,13,last-slice",
    "2026-04-01T09:59:59.999999Z,25,\"end,\nof hour\"",
];

/// The `ts` literal between `marker` and the next `'` in `target`.
fn bound<'a>(target: &'a str, marker: &str) -> &'a str {
    let rest = &target[target.find(marker).expect("bound") + marker.len()..];
    &rest[..rest.find('\'').expect("quote")]
}

/// A body of the rows in `[lo, hi)`, the header first, each record ended by
/// `line_end`; with `final_end` false the last record has none.
fn body(lo: &str, hi: &str, line_end: &str, final_end: bool) -> Vec<u8> {
    let mut records = vec![HEADER];
    records.extend(ROWS.iter().copied().filter(|r| {
        let ts = &r[..27];
        lo <= ts && ts < hi
    }));
    let mut out = records.join(line_end);
    if final_end {
        out.push_str(line_end);
    }
    out.into_bytes()
}

/// A QuestDB stub whose `/exp` answers the rows inside the request's own
/// `ts` bounds, so each slice gets only its own rows.
fn per_slice_qdb(line_end: &'static str, final_end: bool) -> Responder {
    Arc::new(move |req: &SeenRequest| {
        if req.target.starts_with("/exp") {
            let lo = bound(&req.target, "ts >= '");
            let hi = bound(&req.target, "ts < '");
            return (200, Vec::new(), body(lo, hi, line_end, final_end));
        }
        (200, Vec::new(), b"{\"ddl\":\"OK\"}".to_vec())
    })
}

async fn archiver_for(qdb: Responder) -> (PartitionArchiver, Arc<Mutex<Vec<SeenRequest>>>) {
    let (qdb_url, qlog) = spawn_stub(qdb).await;
    let (ilp_url, _) = spawn_stub(ilp_responder(204)).await;
    let (s3_url, _) = spawn_stub(s3_responder(Arc::new(Mutex::new(HashMap::new())))).await;
    (
        build_archiver(&qdb_url, &ilp_url, &s3_url, test_retention_cfg(200)).await,
        qlog,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_sliced_ordered_export_is_byte_identical_to_the_single_query() {
    let key = crate::tick_persistence::DEDUP_KEY_TICKS;
    let (start, end) = partition_range_bounds(HOUR).expect("bounds");
    for (line_end, final_end) in [("\r\n", true), ("\n", true), ("\r\n", false)] {
        let (archiver, qlog) = archiver_for(per_slice_qdb(line_end, final_end)).await;
        let single_path = archiver.temp_dir.join("single.csv.gz");
        let single = archiver
            .export_query_csv(
                &build_export_sql_ordered("ticks", &start, &end, Some(key)),
                single_path.clone(),
            )
            .await
            .expect("single query");
        let sliced_path = archiver.temp_dir.join("sliced.csv.gz");
        let sliced = archiver
            .export_ordered_csv("ticks", HOUR, key, sliced_path.clone())
            .await
            .expect("sliced");
        let ctx = format!("line_end={line_end:?} final_end={final_end}");
        assert_eq!(single.rows, ROWS.len() as u64, "{ctx}");
        assert_eq!(sliced.rows, single.rows, "{ctx}");
        assert_eq!(sliced.csv_bytes, single.csv_bytes, "{ctx}");
        assert_eq!(sliced.gzip_bytes, single.gzip_bytes, "{ctx}");
        assert_eq!(sliced.gzip_sha256_hex, single.gzip_sha256_hex, "{ctx}");
        assert_eq!(sliced.gzip_sha256_b64, single.gzip_sha256_b64, "{ctx}");
        let a = std::fs::read(&single_path).expect("single file");
        let b = std::fs::read(&sliced_path).expect("sliced file");
        assert_eq!(a, b, "{ctx}");
        // One request for the single query, one per slice for the sliced.
        let exports = qlog
            .lock()
            .expect("q")
            .iter()
            .filter(|r| r.target.starts_with("/exp"))
            .count();
        assert_eq!(
            exports,
            1 + ordered_export_slices(HOUR).expect("slices").len(),
            "{ctx}"
        );
    }
}

/// A slice whose header differs from the first slice's (a column added
/// between two slices) fails the export; nothing is left on disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_sliced_export_refuses_a_slice_with_another_header() {
    let responder: Responder = Arc::new(|req: &SeenRequest| {
        if req.target.starts_with("/exp") {
            let lo = bound(&req.target, "ts >= '");
            let header = if lo.ends_with("09:00:00.000000Z") {
                "ts,v"
            } else {
                "ts,v,new_column"
            };
            return (200, Vec::new(), format!("{header}\n").into_bytes());
        }
        (200, Vec::new(), b"{\"ddl\":\"OK\"}".to_vec())
    });
    let (archiver, _) = archiver_for(responder).await;
    let path = archiver.temp_dir.join("refused.csv.gz");
    let err = archiver
        .export_ordered_csv(
            "ticks",
            HOUR,
            crate::tick_persistence::DEDUP_KEY_TICKS,
            path.clone(),
        )
        .await
        .expect_err("a header mismatch must fail the export");
    assert!(format!("{err:#}").contains("header differs"), "{err:#}");
    assert!(!path.exists(), "the temp file is removed");
}

// ---------------------------------------------------------------------------
// 2026-11-17: a `/exp` refused with 429 (QuestDB's export connection limit)
// is waited out and sent again, bounded; any other status fails as before.
// ---------------------------------------------------------------------------

/// The live 429 body of 2026-11-17 17:36 IST.
const QDB_BUSY_BODY: &[u8] = b"exceeded connection limit [name=http_export_connections, \
numOfConnections=3, connectionLimit=2]";

/// A short schedule so a test does not wait the production 120 s.
fn fast_backoff(first_ms: u64, cap_ms: u64, total_ms: u64) -> ExportBusyBackoff {
    ExportBusyBackoff {
        first: Duration::from_millis(first_ms),
        step_cap: Duration::from_millis(cap_ms),
        total: Duration::from_millis(total_ms),
        hold_after_give_up: Duration::from_secs(60),
        resend_margin: Duration::from_millis(5),
    }
}

/// Wraps `inner`: the `/exp` requests whose 0-based number is in `busy`
/// (or every one, with `forever`) get a 429; the rest go to `inner`.
fn busy_on(busy: &'static [u64], forever: bool, inner: Responder) -> Responder {
    let seen = Arc::new(AtomicU64::new(0));
    Arc::new(move |req: &SeenRequest| {
        if req.target.starts_with("/exp") {
            let n = seen.fetch_add(1, Ordering::SeqCst);
            if forever || busy.contains(&n) {
                return (429, Vec::new(), QDB_BUSY_BODY.to_vec());
            }
        }
        inner(req)
    })
}

fn exp_requests(qlog: &Arc<Mutex<Vec<SeenRequest>>>) -> usize {
    qlog.lock()
        .expect("q")
        .iter()
        .filter(|r| r.target.starts_with("/exp"))
        .count()
}

/// 429s on the first slice (twice) and on a middle slice: the export waits,
/// sends each refused request again, and gives byte for byte what a run
/// with no 429 gives. The slices before a refused one are kept, not resent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_export_429_then_200_gives_identical_bytes() {
    let key = crate::tick_persistence::DEDUP_KEY_TICKS;
    let (clean, _) = archiver_for(per_slice_qdb("\n", true)).await;
    let clean_path = clean.temp_dir.join("clean.csv.gz");
    let want = clean
        .export_ordered_csv("ticks", HOUR, key, clean_path.clone())
        .await
        .expect("clean export");

    // Requests 0 and 1 are slice 0's first two tries; request 4 is slice 2's
    // first try (slice 0 succeeds on request 2, slice 1 on request 3).
    let (busy, qlog) = archiver_for(busy_on(&[0, 1, 4], false, per_slice_qdb("\n", true))).await;
    let busy = busy.with_export_busy_backoff(fast_backoff(10, 40, 2_000));
    let busy_path = busy.temp_dir.join("busy.csv.gz");
    let got = busy
        .export_ordered_csv("ticks", HOUR, key, busy_path.clone())
        .await
        .expect("the 429s are waited out");
    assert_eq!(got.rows, want.rows);
    assert_eq!(got.csv_bytes, want.csv_bytes);
    assert_eq!(got.gzip_bytes, want.gzip_bytes);
    assert_eq!(got.gzip_sha256_hex, want.gzip_sha256_hex);
    assert_eq!(
        std::fs::read(&busy_path).expect("busy file"),
        std::fs::read(&clean_path).expect("clean file")
    );
    let slices = ordered_export_slices(HOUR).expect("slices").len();
    assert_eq!(
        exp_requests(&qlog),
        slices + 3,
        "one resend per 429, no more"
    );
}

/// QuestDB stays full: the export gives up once the wait budget is spent,
/// with the same error a 429 always gave (plus the retry count), inside a
/// bounded time, sending a bounded number of requests, and leaves no file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_export_429_forever_gives_up_at_the_bound() {
    let (archiver, qlog) = archiver_for(busy_on(&[], true, per_slice_qdb("\n", true))).await;
    // Waits 20, 40, 50, 50, then 40 (cut to the 200 ms budget): 5 retries.
    let archiver = archiver.with_export_busy_backoff(fast_backoff(20, 50, 200));
    let (start, end) = partition_range_bounds(HOUR).expect("bounds");
    let path = archiver.temp_dir.join("busy-forever.csv.gz");
    let began = std::time::Instant::now();
    let err = archiver
        .export_query_csv(
            &build_export_sql_ordered("ticks", &start, &end, None),
            path.clone(),
        )
        .await
        .expect_err("a QuestDB that stays full must fail the export");
    let took = began.elapsed();
    let msg = format!("{err:#}");
    assert!(msg.contains("/exp export returned 429"), "{msg}");
    assert!(msg.contains("exceeded connection limit"), "{msg}");
    assert!(msg.contains("after 5 busy retries"), "{msg}");
    assert_eq!(exp_requests(&qlog), 6, "the first try plus 5 retries");
    assert!(
        took >= Duration::from_millis(200),
        "waited the budget: {took:?}"
    );
    assert!(took < Duration::from_secs(5), "bounded: {took:?}");
    assert!(!path.exists(), "the temp file is removed");
}

/// A 429 while a deadline is near: the export never waits into it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_export_429_never_waits_past_the_export_deadline() {
    let (archiver, qlog) = archiver_for(busy_on(&[], true, per_slice_qdb("\n", true))).await;
    // A 10 s budget, but the deadline is 150 ms away.
    let archiver = archiver.with_export_busy_backoff(fast_backoff(50, 50, 10_000));
    let (start, end) = partition_range_bounds(HOUR).expect("bounds");
    let path = archiver.temp_dir.join("busy-deadline.csv.gz");
    let began = std::time::Instant::now();
    let err = archiver
        .export_requests_csv(
            &[build_export_sql_ordered("ticks", &start, &end, None)],
            Some(began + Duration::from_millis(150)),
            path.clone(),
        )
        .await
        .expect_err("the deadline ends the waiting");
    let took = began.elapsed();
    assert!(
        format!("{err:#}").contains("/exp export returned 429"),
        "{err:#}"
    );
    assert!(
        took < Duration::from_secs(2),
        "bounded by the deadline: {took:?}"
    );
    assert!(
        exp_requests(&qlog) <= 4,
        "at most 3 waits of 50 ms fit in 150 ms"
    );
    assert!(!path.exists(), "the temp file is removed");
}

/// Any status other than 429 is not retried: one request, today's error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_export_non_429_error_is_not_retried() {
    for status in [400u16, 500, 503] {
        let responder: Responder = Arc::new(move |req: &SeenRequest| {
            if req.target.starts_with("/exp") {
                return (status, Vec::new(), b"boom".to_vec());
            }
            (200, Vec::new(), b"{\"ddl\":\"OK\"}".to_vec())
        });
        let (archiver, qlog) = archiver_for(responder).await;
        let archiver = archiver.with_export_busy_backoff(fast_backoff(10, 10, 1_000));
        let (start, end) = partition_range_bounds(HOUR).expect("bounds");
        let path = archiver.temp_dir.join("refused.csv.gz");
        let err = archiver
            .export_query_csv(
                &build_export_sql_ordered("ticks", &start, &end, None),
                path.clone(),
            )
            .await
            .expect_err("a non-429 error fails the export");
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&format!("/exp export returned {status}")),
            "{msg}"
        );
        assert!(!msg.contains("busy retries"), "{msg}");
        assert_eq!(exp_requests(&qlog), 1, "status {status} is not retried");
        assert!(!path.exists());
    }
}

/// After one export spent its whole wait budget, the same archiver's next
/// 429 fails at once (no second budget), so a pass over many refused
/// partitions is not slowed by one budget each. Only a 2xx answer lifts the
/// hold: a 500 in between leaves it in place.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_export_429_after_a_spent_budget_fails_at_once_until_an_answer() {
    // 0: refuse with 429; 1: answer (CSV); 2: fail with 500.
    const REFUSE: u8 = 0;
    const ANSWER: u8 = 1;
    const FAIL_500: u8 = 2;
    let mode = Arc::new(std::sync::atomic::AtomicU8::new(REFUSE));
    let flag = Arc::clone(&mode);
    let inner = per_slice_qdb("\n", true);
    let responder: Responder = Arc::new(move |req: &SeenRequest| {
        if req.target.starts_with("/exp") {
            match flag.load(Ordering::SeqCst) {
                REFUSE => return (429, Vec::new(), QDB_BUSY_BODY.to_vec()),
                FAIL_500 => return (500, Vec::new(), b"boom".to_vec()),
                _ => {}
            }
        }
        inner(req)
    });
    let (archiver, qlog) = archiver_for(responder).await;
    let archiver = archiver.with_export_busy_backoff(fast_backoff(20, 50, 200));
    let (start, end) = partition_range_bounds(HOUR).expect("bounds");
    let sql = build_export_sql_ordered("ticks", &start, &end, None);
    let path = archiver.temp_dir.join("held.csv.gz");

    archiver
        .export_query_csv(&sql, path.clone())
        .await
        .expect_err("first export spends its budget");
    assert_eq!(exp_requests(&qlog), 6);

    // The request count alone proves there was no wait: every wait is
    // followed by a resend, so a waited 429 would add a request. (No
    // wall-clock bound: it would flake on a loaded, instrumented runner.)
    let err = archiver
        .export_query_csv(&sql, path.clone())
        .await
        .expect_err("second export fails at once");
    assert!(
        format!("{err:#}").contains("/exp export returned 429"),
        "{err:#}"
    );
    assert!(!format!("{err:#}").contains("busy retries"), "{err:#}");
    assert_eq!(exp_requests(&qlog), 7, "one request, no second budget");

    // A non-2xx answer (500) does not lift the hold.
    mode.store(FAIL_500, Ordering::SeqCst);
    let err = archiver
        .export_query_csv(&sql, path.clone())
        .await
        .expect_err("a 500 fails the export");
    assert!(
        format!("{err:#}").contains("/exp export returned 500"),
        "{err:#}"
    );
    assert_eq!(exp_requests(&qlog), 8);
    mode.store(REFUSE, Ordering::SeqCst);
    archiver
        .export_query_csv(&sql, path.clone())
        .await
        .expect_err("still held after a 500");
    assert_eq!(exp_requests(&qlog), 9, "the hold survived the 500: no wait");

    // QuestDB answers again: the export succeeds and the hold is lifted.
    mode.store(ANSWER, Ordering::SeqCst);
    archiver
        .export_query_csv(&sql, path.clone())
        .await
        .expect("an answered request succeeds");
    assert_eq!(exp_requests(&qlog), 10);
    mode.store(REFUSE, Ordering::SeqCst);
    archiver
        .export_query_csv(&sql, path.clone())
        .await
        .expect_err("refused again");
    assert_eq!(exp_requests(&qlog), 16, "after a 2xx, a full budget again");
}

/// 2026-11-17: the export client keeps no idle connection, so QuestDB's
/// export slot is released the moment an export ends. A keep-alive server
/// that counts accepted connections must see one connection per `/exp`
/// request; with reqwest's default pool the second request would reuse the
/// first connection (one accept), and that idle connection would hold one of
/// QuestDB's two export slots for 90 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_export_client_keeps_no_idle_connection_between_exports() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let accepts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&accepts);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 8192];
                let mut seen = Vec::new();
                loop {
                    let Ok(n) = sock.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    seen.extend_from_slice(&buf[..n]);
                    while let Some(end) = seen.windows(4).position(|w| w == b"\r\n\r\n") {
                        seen.drain(..end + 4);
                        let body = format!("{HEADER}\r\n{}\r\n", ROWS[0]);
                        let reply = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/csv\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
                            body.len()
                        );
                        if sock.write_all(reply.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    let qdb_url = format!("http://{addr}");
    let (ilp_url, _) = spawn_stub(ilp_responder(204)).await;
    let (s3_url, _) = spawn_stub(s3_responder(Arc::new(Mutex::new(HashMap::new())))).await;
    let archiver = build_archiver(&qdb_url, &ilp_url, &s3_url, test_retention_cfg(200)).await;
    let (start, end) = partition_range_bounds(HOUR).expect("bounds");
    let sql = build_export_sql_ordered("ticks", &start, &end, None);
    for round in 0..2 {
        let path = archiver.temp_dir.join(format!("idle-{round}.csv.gz"));
        let exported = archiver.export_query_csv(&sql, path).await.expect("export");
        assert_eq!(exported.rows, 1, "round {round}");
    }
    assert_eq!(EXPORT_CLIENT_MAX_IDLE_PER_HOST, 0);
    assert_eq!(
        accepts.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "each /exp request must use its own connection (no idle keep-alive)"
    );
}

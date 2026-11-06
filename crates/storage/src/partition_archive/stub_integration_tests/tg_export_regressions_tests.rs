//! PR TG-3 (2026-11-06 live incident): the partition archiver against a
//! QuestDB whose `/exp` export is refused (HTTP 429, export slot busy) or
//! slow (stalled, or trickling its body).
//!
//! On 2026-11-06 the box booted at 95% disk and the disk-pressure archiver
//! exported 41 partitions through `/exp` while QuestDB had a single HTTP
//! worker. Two properties must hold in that state and these tests pin them:
//!
//! 1. **Keep the partition.** A refused or unfinished export is a failure:
//!    nothing is uploaded and no `DROP PARTITION` is sent. A 429 whose body
//!    happens to look like a valid CSV must not be archived — only the status
//!    check stands between that body and a drop.
//! 2. **The pass stops within a bound.** Each partition is exported at most
//!    once per pass (no retry storm against a busy QuestDB), at most
//!    `max_partitions_per_run` partitions are attempted, and a stalled export
//!    is cut off by the export client's timeout, so the whole pass ends in
//!    about `partitions × timeout`.
//!
//! Reuses the parent module's stub harness (QuestDB `/exec`, ILP and S3 on
//! 127.0.0.1). The export endpoint for the slow cases is a separate local
//! listener so the `/exec` side stays fast, as it was live.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::{
    CSV, HashMap, Responder, SeenRequest, build_archiver, ilp_responder, qdb_responder,
    s3_responder, spawn_stub, test_retention_cfg,
};

/// Export timeout the slow-export tests install in place of the 600 s
/// production one, so a stalled export is cut off in test time.
const TEST_EXPORT_TIMEOUT: Duration = Duration::from_millis(400);

/// Upper bound on a whole pass in the slow-export tests: every attempted
/// partition may spend one export timeout, plus generous slack for the fast
/// `/exec` probes and a loaded CI host.
fn slow_pass_bound(partitions: u32) -> Duration {
    TEST_EXPORT_TIMEOUT * partitions + Duration::from_secs(5)
}

/// Wraps the plain QuestDB stub so every `/exp` answers `429 Too Many
/// Requests` with a body that is a VALID 3-row CSV matching the recount.
/// If the archiver ever ignored the status it would upload this body, the
/// recount would agree, and the partition would be dropped.
fn qdb_exp_429(partitions: Vec<&'static str>) -> Responder {
    let inner = qdb_responder(partitions, 3, CSV, vec![], 200);
    Arc::new(move |req: &SeenRequest| {
        if req.target.starts_with("/exp") {
            return (429, Vec::new(), CSV.as_bytes().to_vec());
        }
        inner(req)
    })
}

fn exp_count(log: &Mutex<Vec<SeenRequest>>) -> usize {
    log.lock()
        .expect("qdb log")
        .iter()
        .filter(|r| r.target.starts_with("/exp"))
        .count()
}

fn drop_count(log: &Mutex<Vec<SeenRequest>>) -> usize {
    log.lock()
        .expect("qdb log")
        .iter()
        .filter(|r| r.target.contains("DROP PARTITION"))
        .count()
}

fn s3_put_count(log: &Mutex<Vec<SeenRequest>>) -> usize {
    log.lock()
        .expect("s3 log")
        .iter()
        .filter(|r| r.method == "PUT")
        .count()
}

/// How the local slow `/exp` endpoint misbehaves.
#[derive(Clone, Copy)]
enum SlowExport {
    /// Reads the request and never answers (QuestDB's only HTTP worker busy).
    Hang,
    /// Answers 200 with a large declared body and sends one byte every
    /// 50 ms (an export that never finishes inside the timeout).
    Trickle,
}

/// Spawns the slow `/exp` endpoint. Returns its base URL and the number of
/// export requests it received.
async fn spawn_slow_exp(mode: SlowExport) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let seen = Arc::new(AtomicUsize::new(0));
    let accept_seen = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let seen = Arc::clone(&accept_seen);
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut tmp).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    }
                }
                if buf.starts_with(b"GET /exp") {
                    seen.fetch_add(1, AtomicOrdering::SeqCst);
                }
                match mode {
                    SlowExport::Hang => {
                        tokio::time::sleep(Duration::from_secs(120)).await;
                    }
                    SlowExport::Trickle => {
                        let head =
                            b"HTTP/1.1 200 OK\r\ncontent-length: 1000000\r\n\r\nts,security_id\n";
                        if sock.write_all(head).await.is_err() {
                            return;
                        }
                        for _ in 0..2_400 {
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            if sock.write_all(b"1").await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });
    (format!("http://{addr}"), seen)
}

/// A 429 from `/exp` is an export failure: the partition is kept, nothing is
/// uploaded, and each partition is asked for once per pass, never more than
/// the per-run cap.
// Multi-thread runtime: the questdb-rs ILP flush is SYNC-blocking (see the
// parent module's tests).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_regression_tg3_export_429_keeps_every_partition_and_uploads_nothing() {
    let (qdb_url, qdb_log) = spawn_stub(qdb_exp_429(vec![
        "2026-04-01T09",
        "2026-04-01T10",
        "2026-04-01T11",
    ]))
    .await;
    let (ilp_url, _ilp_log) = spawn_stub(ilp_responder(204)).await;
    let objects = Arc::new(Mutex::new(HashMap::new()));
    let (s3_url, s3_log) = spawn_stub(s3_responder(Arc::clone(&objects))).await;

    // Cap 2 of 3 eligible partitions.
    let mut archiver = build_archiver(&qdb_url, &ilp_url, &s3_url, test_retention_cfg(2)).await;
    let summary = archiver.run_archive_pass().await;

    assert_eq!(summary.partitions_considered, 2, "the per-run cap binds");
    assert_eq!(summary.failed, 2, "a 429 export is a failure, not 0 rows");
    assert_eq!(summary.verified, 0);
    assert_eq!(summary.dropped, 0, "a refused export never leads to a drop");
    assert_eq!(drop_count(&qdb_log), 0, "no DROP PARTITION was sent");
    assert_eq!(s3_put_count(&s3_log), 0, "the 429 body was not uploaded");
    assert!(objects.lock().expect("objects").is_empty());
    assert_eq!(
        exp_count(&qdb_log),
        2,
        "one export request per attempted partition: no retry storm against a \
         QuestDB that is already refusing"
    );
}

/// The partition a 429 kept is archived by a later pass once QuestDB
/// answers: keeping it defers the archive, it does not lose it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_regression_tg3_a_partition_kept_by_429_is_archived_once_exp_answers() {
    let refusing = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let inner = qdb_responder(vec!["2026-04-01T09"], 3, CSV, vec![], 200);
    let flag = Arc::clone(&refusing);
    let responder: Responder = Arc::new(move |req: &SeenRequest| {
        if req.target.starts_with("/exp") && flag.load(AtomicOrdering::SeqCst) {
            return (429, Vec::new(), b"{\"error\":\"busy\"}".to_vec());
        }
        inner(req)
    });
    let (qdb_url, qdb_log) = spawn_stub(responder).await;
    let (ilp_url, _ilp_log) = spawn_stub(ilp_responder(204)).await;
    let objects = Arc::new(Mutex::new(HashMap::new()));
    let (s3_url, _s3_log) = spawn_stub(s3_responder(Arc::clone(&objects))).await;

    let mut archiver = build_archiver(&qdb_url, &ilp_url, &s3_url, test_retention_cfg(200)).await;
    let first = archiver.run_archive_pass().await;
    assert_eq!((first.failed, first.dropped), (1, 0));
    assert_eq!(drop_count(&qdb_log), 0);

    refusing.store(false, AtomicOrdering::SeqCst);
    let second = archiver.run_archive_pass().await;
    assert_eq!(second.failed, 0, "{second:?}");
    assert_eq!(second.verified, 1);
    assert_eq!(second.dropped, 1, "archived and dropped once /exp answers");
    assert!(
        objects
            .lock()
            .expect("objects")
            .contains_key("tv-test-cold/questdb-partitions/ticks/2026-04-01T09.csv.gz"),
        "the verified copy is in S3 before the drop"
    );
}

/// Runs one pass with `/exp` served by the slow endpoint and the short export
/// timeout. Returns (summary, elapsed, export requests, qdb log, s3 log).
async fn run_slow_export_pass(
    mode: SlowExport,
    partitions: Vec<&'static str>,
    cap: u32,
) -> (
    super::ArchiveRunSummary,
    Duration,
    usize,
    Arc<Mutex<Vec<SeenRequest>>>,
    Arc<Mutex<Vec<SeenRequest>>>,
) {
    let (qdb_url, qdb_log) = spawn_stub(qdb_responder(partitions, 3, CSV, vec![], 200)).await;
    let (slow_url, slow_seen) = spawn_slow_exp(mode).await;
    let (ilp_url, _ilp_log) = spawn_stub(ilp_responder(204)).await;
    let objects = Arc::new(Mutex::new(HashMap::new()));
    let (s3_url, s3_log) = spawn_stub(s3_responder(objects)).await;

    let mut archiver = build_archiver(&qdb_url, &ilp_url, &s3_url, test_retention_cfg(cap)).await;
    archiver.exp_url = format!("{slow_url}/exp");
    archiver.export_client = reqwest::Client::builder()
        .timeout(TEST_EXPORT_TIMEOUT)
        .no_proxy()
        .build()
        .expect("short-timeout export client");

    let started = Instant::now();
    // Outer guard so a regression that removes the export timeout FAILS
    // instead of hanging the test binary.
    let summary = tokio::time::timeout(Duration::from_secs(30), archiver.run_archive_pass())
        .await
        .expect("the pass must end: a stalled export is cut off by the export timeout");
    let elapsed = started.elapsed();
    (
        summary,
        elapsed,
        slow_seen.load(AtomicOrdering::SeqCst),
        qdb_log,
        s3_log,
    )
}

/// An export QuestDB never answers is cut off by the export timeout: the
/// partition is kept, and the pass ends in about `partitions × timeout`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_regression_tg3_stalled_export_keeps_partition_and_pass_ends_within_bound() {
    let (summary, elapsed, exports, qdb_log, s3_log) = run_slow_export_pass(
        SlowExport::Hang,
        vec!["2026-04-01T09", "2026-04-01T10", "2026-04-01T11"],
        2,
    )
    .await;
    assert_eq!(summary.partitions_considered, 2);
    assert_eq!(summary.failed, 2, "{summary:?}");
    assert_eq!(summary.dropped, 0);
    assert_eq!(
        drop_count(&qdb_log),
        0,
        "a stalled export never leads to a drop"
    );
    assert_eq!(s3_put_count(&s3_log), 0);
    assert_eq!(exports, 2, "each attempted partition is asked for once");
    assert!(
        elapsed < slow_pass_bound(2),
        "the pass took {elapsed:?}, over the bound {:?}",
        slow_pass_bound(2)
    );
}

/// An export that starts but trickles its body is cut off the same way, and
/// the half-read body is never uploaded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_regression_tg3_trickling_export_keeps_partition_and_pass_ends_within_bound() {
    let (summary, elapsed, exports, qdb_log, s3_log) =
        run_slow_export_pass(SlowExport::Trickle, vec!["2026-04-01T09"], 200).await;
    assert_eq!(summary.partitions_considered, 1);
    assert_eq!(summary.failed, 1, "{summary:?}");
    assert_eq!(summary.verified, 0);
    assert_eq!(summary.dropped, 0);
    assert_eq!(
        drop_count(&qdb_log),
        0,
        "a half-read export never leads to a drop"
    );
    assert_eq!(
        s3_put_count(&s3_log),
        0,
        "the partial body was not uploaded"
    );
    assert_eq!(exports, 1);
    assert!(
        elapsed < slow_pass_bound(1),
        "the pass took {elapsed:?}, over the bound {:?}",
        slow_pass_bound(1)
    );
}

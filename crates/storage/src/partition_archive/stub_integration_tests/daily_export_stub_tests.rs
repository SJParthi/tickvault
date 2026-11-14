//! The daily Brutex export (owner Quote 60) against the archive's stub
//! QuestDB and a create-only stub S3 that also answers ListObjectsV2.

use super::*;
use crate::partition_archive::daily_export::{
    DailyExporter, EXPORT_PARTITION_WORST_SECS, ExportOptions, ExportOutcome, ExportReport,
    StopReason, counts_key, done_key, manifest_key,
};

/// The partition every test exports.
const PART: &str = "2026-04-01T09";

/// Rows in DEDUP-key order (what `ORDER BY` returns).
const SORTED: &str =
    "ts,security_id,segment,feed\n1,13,IDX_I,dhan\n1,25,IDX_I,dhan\n2,13,IDX_I,dhan\n";
/// The same rows in another physical order (what the unordered query returns).
const UNSORTED: &str =
    "ts,security_id,segment,feed\n2,13,IDX_I,dhan\n1,25,IDX_I,dhan\n1,13,IDX_I,dhan\n";

fn day() -> chrono::NaiveDate {
    chrono::NaiveDate::from_ymd_opt(2026, 4, 1).expect("date")
}

/// Stub key for an S3 key (path-style, bucket first).
fn stub_key(key: &str) -> String {
    format!("tv-test-cold/{key}")
}

/// Create-only S3: PUT with `If-None-Match` on an existing key is a 412;
/// HEAD answers length + checksum; GET `?list-type=2` lists a prefix.
fn create_only_s3(objects: StubObjects) -> Responder {
    Arc::new(move |req: &SeenRequest| {
        let mut parts = req.target.splitn(2, '?');
        let path = parts
            .next()
            .unwrap_or_default()
            .trim_start_matches('/')
            .to_string();
        let query = parts.next().unwrap_or_default().to_string();
        match req.method.as_str() {
            "PUT" => {
                let mut objs = objects.lock().expect("s3 lock");
                if req.header("if-none-match").is_some() && objs.contains_key(&path) {
                    return (
                        412,
                        Vec::new(),
                        b"<Error><Code>PreconditionFailed</Code></Error>".to_vec(),
                    );
                }
                let len = req
                    .header("content-length")
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(0);
                let sum = req.header("x-amz-checksum-sha256").map(ToString::to_string);
                objs.insert(path, (len, sum));
                (200, vec![("etag".into(), "\"stub\"".into())], Vec::new())
            }
            "HEAD" => match objects.lock().expect("s3 lock").get(&path) {
                Some((len, sum)) => {
                    let mut h = vec![
                        ("content-length".into(), len.to_string()),
                        ("etag".into(), "\"stub\"".into()),
                    ];
                    if let Some(s) = sum {
                        h.push(("x-amz-checksum-sha256".into(), s.clone()));
                    }
                    (200, h, Vec::new())
                }
                None => (404, vec![("content-length".into(), "0".into())], Vec::new()),
            },
            "GET" if query.contains("list-type=2") => {
                let prefix = query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("prefix="))
                    .unwrap_or_default()
                    .to_string();
                let bucket_prefix = format!("tv-test-cold/{prefix}");
                let contents: String = objects
                    .lock()
                    .expect("s3 lock")
                    .iter()
                    .filter(|(k, _)| k.starts_with(&bucket_prefix))
                    .map(|(k, (len, _))| {
                        format!(
                            "<Contents><Key>{}</Key><Size>{len}</Size></Contents>",
                            k.trim_start_matches("tv-test-cold/")
                        )
                    })
                    .collect();
                let body = format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult \
                     xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Name>tv-test-cold</Name>\
                     <Prefix>{prefix}</Prefix><MaxKeys>1000</MaxKeys><IsTruncated>false</IsTruncated>\
                     {contents}</ListBucketResult>"
                );
                (
                    200,
                    vec![("content-type".into(), "application/xml".into())],
                    body.into_bytes(),
                )
            }
            _ => (500, Vec::new(), b"unexpected s3 method".to_vec()),
        }
    })
}

/// QuestDB stub: `ticks` lists [`PART`]; an export WITH `ORDER BY` returns
/// [`SORTED`], without it [`UNSORTED`]; the audit query answers `audit`.
fn qdb(audit: &'static str, tally: TallyStub) -> Responder {
    let inner = qdb_responder_tables(
        vec![("ticks", vec![PART])],
        tally,
        UNSORTED,
        Arc::new(|_| (200, wal_tables_body(&[], &[]))),
    );
    Arc::new(move |req: &SeenRequest| {
        if req.target.starts_with("/exp") && req.target.contains("ORDER BY") {
            return (200, Vec::new(), SORTED.as_bytes().to_vec());
        }
        if req.target.contains("FROM partition_archive_audit") {
            return (200, Vec::new(), audit.as_bytes().to_vec());
        }
        inner(req)
    })
}

const NO_AUDIT: &str = "{\"columns\":[],\"dataset\":[]}";

fn three_rows() -> TallyStub {
    Arc::new(|_| (3, Some(STUB_MAX_TS.to_string())))
}

async fn setup(
    audit: &'static str,
    tally: TallyStub,
    objects: StubObjects,
    brutex: bool,
) -> (PartitionArchiver, Arc<Mutex<Vec<SeenRequest>>>) {
    let (qdb_url, _qlog) = spawn_stub(qdb(audit, tally)).await;
    let (ilp_url, _ilog) = spawn_stub(ilp_responder(204)).await;
    let (s3_url, s3_log) = spawn_stub(create_only_s3(objects)).await;
    let mut cfg = test_retention_cfg(200);
    cfg.brutex_export_enabled = brutex;
    (
        build_archiver(&qdb_url, &ilp_url, &s3_url, cfg).await,
        s3_log,
    )
}

fn puts(log: &Arc<Mutex<Vec<SeenRequest>>>) -> Vec<String> {
    log.lock()
        .expect("log")
        .iter()
        .filter(|r| r.method == "PUT")
        .map(|r| r.target.split('?').next().unwrap_or_default().to_string())
        .collect()
}

/// Runs one attempt that is never stopped and is not the last chance, and
/// expects a seal.
async fn seal(ex: &mut DailyExporter, reasons: Vec<String>) -> ExportReport {
    sealed_report(run(ex, reasons, false).await.expect("export")).expect("sealed, not stopped")
}

/// The report of a sealed outcome; `None` for a stop.
fn sealed_report(outcome: ExportOutcome) -> Option<ExportReport> {
    match outcome {
        ExportOutcome::Sealed(report) => Some(report),
        ExportOutcome::Stopped(_) => None,
    }
}

/// No floor in the stubs (the test box's own disk is whatever it is).
fn no_floor(_total: u64) -> u64 {
    0
}

/// No 08:30 bound.
fn unbounded() -> Option<std::time::Duration> {
    None
}

async fn run(
    ex: &mut DailyExporter,
    reasons: Vec<String>,
    last_chance: bool,
) -> anyhow::Result<ExportOutcome> {
    let go = || true;
    ex.export_day(
        day(),
        reasons,
        ExportOptions {
            keep_going: &go,
            time_left: &unbounded,
            floor_for_total: &no_floor,
            open_from: None,
            last_chance,
        },
    )
    .await
}

/// One attempt with the given clock and floor.
async fn run_with(
    ex: &mut DailyExporter,
    time_left: &(dyn Fn() -> Option<std::time::Duration> + Sync),
    floor_for_total: &(dyn Fn(u64) -> u64 + Sync),
) -> anyhow::Result<ExportOutcome> {
    let go = || true;
    ex.export_day(
        day(),
        vec![],
        ExportOptions {
            keep_going: &go,
            time_left,
            floor_for_total,
            open_from: None,
            last_chance: false,
        },
    )
    .await
}

fn has_sidecar(objects: &StubObjects) -> bool {
    objects
        .lock()
        .expect("o")
        .keys()
        .any(|k| k.contains(".sha-"))
}

fn amend_count(objects: &StubObjects) -> usize {
    objects
        .lock()
        .expect("o")
        .keys()
        .filter(|k| k.contains("_daily/2026-04-01/amend-ticks-2026-04-01T09-"))
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_daily_export_writes_done_last() {
    let objects: StubObjects = Arc::new(Mutex::new(HashMap::new()));
    let (archiver, log) = setup(NO_AUDIT, three_rows(), Arc::clone(&objects), true).await;
    let mut ex = DailyExporter::from_archiver(archiver);
    let report = seal(&mut ex, vec![]).await;
    assert_eq!(report.attempt, 1);
    assert_eq!(report.rows, 3);
    assert!(report.complete && report.sealed_by_this_run);
    let puts = puts(&log);
    assert_eq!(
        puts.last().map(String::as_str),
        Some(format!("/{}", stub_key(&done_key(day()))).as_str()),
        "DONE is the last write: {puts:?}"
    );
    let manifest_pos = puts.iter().position(|p| p.ends_with("manifest-1.json"));
    let counts_pos = puts.iter().position(|p| p.contains("instrument-counts-1"));
    let part_pos = puts
        .iter()
        .position(|p| p.ends_with("ticks/2026-04-01T09.csv.gz"));
    assert!(
        part_pos < counts_pos && counts_pos < manifest_pos,
        "{puts:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_daily_export_uploads_then_archiver_reuses_without_sidecar() {
    let objects: StubObjects = Arc::new(Mutex::new(HashMap::new()));
    let (archiver, _log) = setup(NO_AUDIT, three_rows(), Arc::clone(&objects), true).await;
    seal(&mut DailyExporter::from_archiver(archiver), vec![]).await;
    let (mut archiver, log) = setup(NO_AUDIT, three_rows(), Arc::clone(&objects), true).await;
    let summary = archiver.run_archive_pass().await;
    assert_eq!(summary.verified, 1);
    assert_eq!(
        summary.gzip_bytes_uploaded, 0,
        "the archive reused the export's object"
    );
    // Only the (small) amend record is written: no second copy.
    assert!(
        puts(&log)
            .iter()
            .all(|p| p.contains("/_daily/2026-04-01/amend-")),
        "no second copy: {:?}",
        puts(&log)
    );
    assert_eq!(amend_count(&objects), 1);
    assert!(!has_sidecar(&objects));
}

/// Regression (2026-11-14 review, MEDIUM): turning the export off must not
/// make the archive re-copy what the export already placed. The archiver
/// orders whenever an object sits at the canonical key, switch or not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_archive_with_the_switch_off_still_reuses_the_export_object() {
    let objects: StubObjects = Arc::new(Mutex::new(HashMap::new()));
    let (archiver, _log) = setup(NO_AUDIT, three_rows(), Arc::clone(&objects), true).await;
    seal(&mut DailyExporter::from_archiver(archiver), vec![]).await;
    let (mut archiver, log) = setup(NO_AUDIT, three_rows(), Arc::clone(&objects), false).await;
    let summary = archiver.run_archive_pass().await;
    assert_eq!(summary.verified, 1);
    assert_eq!(
        summary.gzip_bytes_uploaded,
        0,
        "no second copy: {:?}",
        puts(&log)
    );
    assert!(!has_sidecar(&objects));
}

/// Regression (2026-11-14 third review, MEDIUM): with the export switched
/// OFF (the documented rollback) the archiver still writes the amend record
/// for a copy of a day that already has a DONE, so a sealed day never hides
/// a later copy from Brutex; a day with no DONE gets none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_switch_off_still_amends_a_sealed_day() {
    for sealed in [true, false] {
        let objects: StubObjects = Arc::new(Mutex::new(HashMap::new()));
        let (archiver, _) = setup(NO_AUDIT, three_rows(), Arc::clone(&objects), true).await;
        seal(&mut DailyExporter::from_archiver(archiver), vec![]).await;
        if !sealed {
            objects
                .lock()
                .expect("o")
                .remove(&stub_key(&done_key(day())));
        }
        let (mut archiver, log) = setup(NO_AUDIT, three_rows(), Arc::clone(&objects), false).await;
        let summary = archiver.run_archive_pass().await;
        assert_eq!(summary.verified, 1);
        assert!(
            puts(&log)
                .iter()
                .all(|p| p.contains("/_daily/2026-04-01/amend-")),
            "only an amend record: {:?}",
            puts(&log)
        );
        assert_eq!(
            amend_count(&objects),
            usize::from(sealed),
            "sealed={sealed}"
        );
    }
}

/// Regression (2026-11-14 review, MEDIUM x2): inside the trading-day window
/// the archiver never asks QuestDB to sort; the cost is a content-addressed
/// copy when the bytes differ (this is also the bite check for ordering).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_archiver_never_sorts_inside_the_trading_window() {
    let objects: StubObjects = Arc::new(Mutex::new(HashMap::new()));
    let (archiver, _log) = setup(NO_AUDIT, three_rows(), Arc::clone(&objects), true).await;
    seal(&mut DailyExporter::from_archiver(archiver), vec![]).await;
    let (qdb_url, qlog) = spawn_stub(qdb(NO_AUDIT, three_rows())).await;
    let (ilp_url, _) = spawn_stub(ilp_responder(204)).await;
    let (s3_url, _) = spawn_stub(create_only_s3(Arc::clone(&objects))).await;
    let mut archiver = build_archiver(&qdb_url, &ilp_url, &s3_url, test_retention_cfg(200)).await;
    archiver.trading_window_now = Arc::new(|| true);
    let _ = archiver.run_archive_pass().await;
    assert!(
        !qlog
            .lock()
            .expect("q")
            .iter()
            .any(|r| r.target.starts_with("/exp") && r.target.contains("ORDER BY")),
        "no sort in session"
    );
    assert!(has_sidecar(&objects), "unordered bytes differ: second copy");
}

/// With nothing at the canonical key the archiver streams unordered (no
/// object to match, so no sort is paid for).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_archiver_streams_unordered_when_no_object_exists() {
    let objects: StubObjects = Arc::new(Mutex::new(HashMap::new()));
    let (qdb_url, qlog) = spawn_stub(qdb(NO_AUDIT, three_rows())).await;
    let (ilp_url, _) = spawn_stub(ilp_responder(204)).await;
    let (s3_url, _) = spawn_stub(create_only_s3(Arc::clone(&objects))).await;
    let mut cfg = test_retention_cfg(200);
    cfg.brutex_export_enabled = true;
    let mut archiver = build_archiver(&qdb_url, &ilp_url, &s3_url, cfg).await;
    let summary = archiver.run_archive_pass().await;
    assert_eq!(summary.verified, 1);
    let exports: Vec<bool> = qlog
        .lock()
        .expect("q")
        .iter()
        .filter(|r| r.target.starts_with("/exp"))
        .map(|r| r.target.contains("ORDER BY"))
        .collect();
    assert_eq!(exports, vec![false]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_daily_export_never_overwrites_an_existing_key() {
    let objects: StubObjects = Arc::new(Mutex::new(HashMap::new()));
    let canonical = stub_key(&canonical_archive_key("ticks", PART));
    objects
        .lock()
        .expect("seed")
        .insert(canonical.clone(), (7, Some("b3RoZXI=".to_string())));
    let (archiver, log) = setup(NO_AUDIT, three_rows(), Arc::clone(&objects), true).await;
    let report = seal(&mut DailyExporter::from_archiver(archiver), vec![]).await;
    assert_eq!(report.objects, 1);
    assert!(
        !puts(&log).iter().any(|p| p.ends_with(&canonical)),
        "canonical never written over"
    );
    assert_eq!(
        objects.lock().expect("o").get(&canonical).map(|v| v.0),
        Some(7)
    );
    assert!(
        objects
            .lock()
            .expect("o")
            .keys()
            .any(|k| k.contains("2026-04-01T09.sha-"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_done_present_means_nothing_to_do() {
    let objects: StubObjects = Arc::new(Mutex::new(HashMap::new()));
    let (archiver, _) = setup(NO_AUDIT, three_rows(), Arc::clone(&objects), true).await;
    seal(&mut DailyExporter::from_archiver(archiver), vec![]).await;
    let (archiver, log) = setup(NO_AUDIT, three_rows(), Arc::clone(&objects), true).await;
    let mut ex = DailyExporter::from_archiver(archiver);
    assert!(ex.is_sealed(day()).await.expect("head"));
    let again = seal(&mut ex, vec![]).await;
    assert!(!again.sealed_by_this_run);
    assert!(puts(&log).is_empty());
}

/// A crash between the manifest and DONE: the next run writes attempt 2.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_rerun_without_done_writes_the_next_attempt() {
    let objects: StubObjects = Arc::new(Mutex::new(HashMap::new()));
    objects
        .lock()
        .expect("seed")
        .insert(stub_key(&manifest_key(day(), 1)), (10, None));
    let (archiver, _) = setup(NO_AUDIT, three_rows(), Arc::clone(&objects), true).await;
    let report = seal(
        &mut DailyExporter::from_archiver(archiver),
        vec!["spill_pending:ticks".into()],
    )
    .await;
    assert_eq!(report.attempt, 2);
    assert!(!report.complete);
    assert_eq!(
        report.incomplete_reasons,
        vec!["spill_pending:ticks".to_string()]
    );
    let o = objects.lock().expect("o");
    assert_eq!(
        o.get(&stub_key(&manifest_key(day(), 1))).map(|v| v.0),
        Some(10)
    );
    assert!(o.contains_key(&stub_key(&manifest_key(day(), 2))));
}

/// Regression (2026-11-14 review, HIGH): a counts file whose manifest write
/// failed used to wedge the day (every retry picked the same `n` and got a
/// 412 on the counts PUT). The next attempt now skips its number.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_orphan_counts_file_does_not_wedge_the_day() {
    let objects: StubObjects = Arc::new(Mutex::new(HashMap::new()));
    objects
        .lock()
        .expect("seed")
        .insert(stub_key(&counts_key(day(), 1)), (10, None));
    let (archiver, _) = setup(NO_AUDIT, three_rows(), Arc::clone(&objects), true).await;
    let report = seal(&mut DailyExporter::from_archiver(archiver), vec![]).await;
    assert_eq!(report.attempt, 2);
    assert!(report.sealed_by_this_run);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_recount_mismatch_fails_the_attempt() {
    let objects: StubObjects = Arc::new(Mutex::new(HashMap::new()));
    let tally: TallyStub = Arc::new(|_| (4, Some(STUB_MAX_TS.to_string())));
    let (archiver, log) = setup(NO_AUDIT, tally, Arc::clone(&objects), true).await;
    let err = run(&mut DailyExporter::from_archiver(archiver), vec![], false)
        .await
        .expect_err("rows moved");
    assert!(format!("{err:#}").contains("recount mismatch"), "{err:#}");
    assert!(puts(&log).is_empty(), "nothing uploaded, no DONE");
}

fn audit_row(table: &str, partition: &str, rows: u64, digest: &[u8; 32]) -> String {
    format!(
        "[\"{table}\",\"{partition}\",\"questdb-partitions/{table}/{partition}.csv.gz\",{rows},9,\"{}\"]",
        crate::s3_cold::hex_encode(digest)
    )
}

fn seeded_copy(objects: &StubObjects, table: &str, partition: &str, digest: &[u8; 32]) {
    objects.lock().expect("seed").insert(
        stub_key(&format!("questdb-partitions/{table}/{partition}.csv.gz")),
        (9, Some(base64_encode(digest))),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_daily_export_lists_dropped_partition_from_audit() {
    use sha2::Digest as _;
    let digest: [u8; 32] = sha2::Sha256::digest(b"old").into();
    let audit: &'static str = Box::leak(
        format!(
            "{{\"columns\":[],\"dataset\":[{}]}}",
            audit_row("market_depth", "2026-04-01T10", 42, &digest)
        )
        .into_boxed_str(),
    );
    let objects: StubObjects = Arc::new(Mutex::new(HashMap::new()));
    seeded_copy(&objects, "market_depth", "2026-04-01T10", &digest);
    let (archiver, _) = setup(audit, three_rows(), Arc::clone(&objects), true).await;
    let report = seal(&mut DailyExporter::from_archiver(archiver), vec![]).await;
    assert_eq!(report.objects, 2, "the live hour plus the dropped one");
    assert_eq!(report.rows, 3 + 42);
    assert!(report.complete);
}

/// QuestDB stub whose size listing shows [`PART`] only on its first call:
/// the archiver dropped the hour between the export's listing and its
/// re-list under the quiesce lock.
fn qdb_dropping_after_first_listing(audit: &'static str) -> Responder {
    let inner = qdb(audit, three_rows());
    let listings = Arc::new(AtomicU64::new(0));
    Arc::new(move |req: &SeenRequest| {
        if req.target.contains("diskSize")
            && req.target.contains("table_partitions('ticks')")
            && listings.fetch_add(1, Ordering::SeqCst) > 0
        {
            return (
                200,
                Vec::new(),
                b"{\"columns\":[{\"name\":\"name\"},{\"name\":\"diskSize\"}],\"dataset\":[]}"
                    .to_vec(),
            );
        }
        inner(req)
    })
}

/// Regression (2026-11-14 review, CRITICAL): an hour the archiver dropped
/// while the export ran is never exported as an empty hour and never left
/// out. With its verified audit row it is listed from the archive copy; with
/// none yet the day is refused (no DONE).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_hour_dropped_mid_export_is_listed_or_refused_never_sealed_empty() {
    use sha2::Digest as _;
    let digest: [u8; 32] = sha2::Sha256::digest(b"bulk").into();
    for with_audit in [false, true] {
        let audit: &'static str = if with_audit {
            Box::leak(
                format!(
                    "{{\"columns\":[],\"dataset\":[{}]}}",
                    audit_row("ticks", PART, 2_000, &digest)
                )
                .into_boxed_str(),
            )
        } else {
            NO_AUDIT
        };
        let objects: StubObjects = Arc::new(Mutex::new(HashMap::new()));
        seeded_copy(&objects, "ticks", PART, &digest);
        let (qdb_url, qlog) = spawn_stub(qdb_dropping_after_first_listing(audit)).await;
        let (ilp_url, _) = spawn_stub(ilp_responder(204)).await;
        let (s3_url, s3_log) = spawn_stub(create_only_s3(Arc::clone(&objects))).await;
        let mut cfg = test_retention_cfg(200);
        cfg.brutex_export_enabled = true;
        let archiver = build_archiver(&qdb_url, &ilp_url, &s3_url, cfg).await;
        let mut ex = DailyExporter::from_archiver(archiver);
        let out = run(&mut ex, vec![], false).await;
        assert!(
            !qlog
                .lock()
                .expect("q")
                .iter()
                .any(|r| r.target.starts_with("/exp")),
            "the vanished hour is never exported"
        );
        if with_audit {
            let report = sealed_report(out.expect("sealed")).expect("sealed");
            assert_eq!(report.rows, 2_000);
            assert!(report.complete);
        } else {
            let err = out.expect_err("refused");
            assert!(
                format!("{err:#}").contains("dropped during the export"),
                "{err:#}"
            );
            assert!(
                !puts(&s3_log).iter().any(|p| p.ends_with("/DONE")),
                "no DONE"
            );
        }
    }
}

/// Regression (2026-11-14 review, CRITICAL): the export waits for the shared
/// side of the archive quiesce lock; while the archiver holds the exclusive
/// side the partition is neither exported nor placed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_export_waits_for_the_archive_quiesce_lock() {
    let objects: StubObjects = Arc::new(Mutex::new(HashMap::new()));
    let (archiver, log) = setup(NO_AUDIT, three_rows(), Arc::clone(&objects), true).await;
    let lock: &'static crate::archive_quiesce::ArchiveQuiesce =
        Box::leak(Box::new(crate::archive_quiesce::ArchiveQuiesce::new()));
    let held = lock
        .hold_writers(std::time::Duration::from_secs(1))
        .await
        .expect("archiver side");
    let mut ex = DailyExporter::with_quiesce(archiver, lock, std::time::Duration::from_millis(200));
    let err = run(&mut ex, vec![], false).await.expect_err("lock held");
    assert!(format!("{err:#}").contains("quiesce lock"), "{err:#}");
    assert!(puts(&log).is_empty());
    drop(held);
    assert!(matches!(
        run(&mut ex, vec![], false).await.expect("free"),
        ExportOutcome::Sealed(_)
    ));
}

/// Regression (2026-11-14 review, HIGH/MEDIUM): `keep_going` turning false
/// (the trading-day window closing) stops the attempt before the next
/// table: nothing sealed, nothing failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_stop_before_the_window_closes_writes_no_done() {
    let objects: StubObjects = Arc::new(Mutex::new(HashMap::new()));
    let (archiver, log) = setup(NO_AUDIT, three_rows(), Arc::clone(&objects), true).await;
    let mut ex = DailyExporter::from_archiver(archiver);
    let calls = AtomicU64::new(0);
    let go = || calls.fetch_add(1, Ordering::SeqCst) < 1;
    let out = ex
        .export_day(
            day(),
            vec![],
            ExportOptions {
                keep_going: &go,
                time_left: &unbounded,
                floor_for_total: &no_floor,
                open_from: None,
                last_chance: false,
            },
        )
        .await
        .expect("stopped is not an error");
    assert_eq!(out, ExportOutcome::Stopped(StopReason::Window));
    assert!(!puts(&log).iter().any(|p| p.ends_with("/DONE")));
    assert!(!ex.is_sealed(day()).await.expect("head"));
}

/// Regression (2026-11-14 review, MEDIUM, Rule 11): a session day with no
/// `ticks` at all is never sealed complete. It is refused (a replay may
/// still bring the rows) and sealed INCOMPLETE only on the last chance.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_a_day_with_no_ticks_is_never_sealed_complete() {
    let responder = qdb_responder_tables(
        vec![],
        three_rows(),
        UNSORTED,
        Arc::new(|_| (200, wal_tables_body(&[], &[]))),
    );
    let wrapped: Responder = Arc::new(move |req: &SeenRequest| {
        if req.target.contains("FROM partition_archive_audit") {
            return (200, Vec::new(), NO_AUDIT.as_bytes().to_vec());
        }
        responder(req)
    });
    let (qdb_url, _) = spawn_stub(wrapped).await;
    let (ilp_url, _) = spawn_stub(ilp_responder(204)).await;
    let objects: StubObjects = Arc::new(Mutex::new(HashMap::new()));
    let (s3_url, _) = spawn_stub(create_only_s3(Arc::clone(&objects))).await;
    let archiver = build_archiver(&qdb_url, &ilp_url, &s3_url, test_retention_cfg(200)).await;
    let mut ex = DailyExporter::from_archiver(archiver);
    let err = run(&mut ex, vec![], false).await.expect_err("refused");
    assert!(format!("{err:#}").contains("nothing captured"), "{err:#}");
    assert!(!ex.is_sealed(day()).await.expect("head"));
    let report =
        sealed_report(run(&mut ex, vec![], true).await.expect("last chance")).expect("sealed");
    assert!(!report.complete);
    assert_eq!(
        report.incomplete_reasons,
        vec!["no_ticks_captured".to_string()]
    );
}

/// QuestDB stub whose ordered export always fails.
fn qdb_failing_ordered_export() -> Responder {
    let inner = qdb(NO_AUDIT, three_rows());
    Arc::new(move |req: &SeenRequest| {
        if req.target.starts_with("/exp") && req.target.contains("ORDER BY") {
            return (
                500,
                Vec::new(),
                b"{\"error\":\"sort out of memory\"}".to_vec(),
            );
        }
        inner(req)
    })
}

/// Under disk pressure the ordered export failing once falls back to the
/// old query, so freeing the disk never waits on the sort.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_archiver_falls_back_once_to_unordered_export() {
    let inner = qdb(NO_AUDIT, three_rows());
    let responder: Responder = Arc::new(move |req: &SeenRequest| {
        if req.target.starts_with("/exp") && req.target.contains("ORDER BY") {
            return (
                500,
                Vec::new(),
                b"{\"error\":\"sort out of memory\"}".to_vec(),
            );
        }
        inner(req)
    });
    let (qdb_url, qlog) = spawn_stub(responder).await;
    let (ilp_url, _) = spawn_stub(ilp_responder(204)).await;
    let objects: StubObjects = Arc::new(Mutex::new(HashMap::new()));
    // An object already at the canonical key is what makes the archiver sort.
    objects.lock().expect("seed").insert(
        stub_key(&canonical_archive_key("ticks", PART)),
        (7, Some("b3RoZXI=".to_string())),
    );
    let (s3_url, _) = spawn_stub(create_only_s3(Arc::clone(&objects))).await;
    let mut cfg = test_retention_cfg(200);
    cfg.brutex_export_enabled = true;
    cfg.under_disk_pressure = true;
    let mut archiver = build_archiver(&qdb_url, &ilp_url, &s3_url, cfg).await;
    let summary = archiver.run_archive_pass().await;
    assert_eq!(
        summary.verified, 1,
        "the unordered retry archived the partition"
    );
    let exports: Vec<bool> = qlog
        .lock()
        .expect("q")
        .iter()
        .filter(|r| r.target.starts_with("/exp"))
        .map(|r| r.target.contains("ORDER BY"))
        .collect();
    assert_eq!(
        exports,
        vec![true, false],
        "one ordered try, then exactly one unordered"
    );
}

/// Regression (2026-11-14 review, MEDIUM): OUTSIDE disk pressure a failed
/// ordered export never falls back to the unordered query (whose bytes can
/// never match the canonical object, so one transient error would leave a
/// permanent full duplicate in S3): the partition is kept and retried.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_failed_ordered_export_keeps_the_partition_outside_pressure() {
    let (qdb_url, qlog) = spawn_stub(qdb_failing_ordered_export()).await;
    let (ilp_url, _) = spawn_stub(ilp_responder(204)).await;
    let objects: StubObjects = Arc::new(Mutex::new(HashMap::new()));
    objects.lock().expect("seed").insert(
        stub_key(&canonical_archive_key("ticks", PART)),
        (7, Some("b3RoZXI=".to_string())),
    );
    let (s3_url, s3_log) = spawn_stub(create_only_s3(Arc::clone(&objects))).await;
    let mut cfg = test_retention_cfg(200);
    cfg.brutex_export_enabled = true;
    let mut archiver = build_archiver(&qdb_url, &ilp_url, &s3_url, cfg).await;
    let summary = archiver.run_archive_pass().await;
    assert_eq!(summary.verified, 0);
    assert_eq!(summary.dropped, 0);
    let exports: Vec<bool> = qlog
        .lock()
        .expect("q")
        .iter()
        .filter(|r| r.target.starts_with("/exp"))
        .map(|r| r.target.contains("ORDER BY"))
        .collect();
    assert_eq!(exports, vec![true], "no unordered retry outside pressure");
    assert!(
        puts(&s3_log).is_empty(),
        "no second copy: {:?}",
        puts(&s3_log)
    );
    assert!(!has_sidecar(&objects));
}

/// Regression (2026-11-14 reviews, MEDIUM): every verified archiver copy
/// gets one amend record, whatever the day's DONE state: a first canonical
/// upload, a content-addressed copy and a reuse of the canonical object
/// (an hour an earlier export attempt placed, which a later attempt may not
/// list); with the switch off none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_archiver_copy_writes_one_amend_record_without_done() {
    for (brutex, seed_other, want) in [(true, false, 1), (true, true, 1), (false, true, 0)] {
        let objects: StubObjects = Arc::new(Mutex::new(HashMap::new()));
        if seed_other {
            objects.lock().expect("seed").insert(
                stub_key(&canonical_archive_key("ticks", PART)),
                (7, Some("b3RoZXI=".to_string())),
            );
        }
        let (mut archiver, _) = setup(NO_AUDIT, three_rows(), Arc::clone(&objects), brutex).await;
        let summary = archiver.run_archive_pass().await;
        assert_eq!(summary.verified, 1);
        assert_eq!(
            amend_count(&objects),
            want,
            "brutex={brutex} seed={seed_other}"
        );
        assert!(
            !objects
                .lock()
                .expect("o")
                .contains_key(&stub_key(&done_key(day())))
        );
    }
    // The export's own object reused by the archive (no DONE yet: attempt 1
    // failed after placing it): the reuse is recorded too.
    let objects: StubObjects = Arc::new(Mutex::new(HashMap::new()));
    let (archiver, _) = setup(NO_AUDIT, three_rows(), Arc::clone(&objects), true).await;
    seal(&mut DailyExporter::from_archiver(archiver), vec![]).await;
    objects
        .lock()
        .expect("o")
        .remove(&stub_key(&done_key(day())));
    let (mut archiver, _) = setup(NO_AUDIT, three_rows(), Arc::clone(&objects), true).await;
    let summary = archiver.run_archive_pass().await;
    assert_eq!(summary.verified, 1);
    assert_eq!(summary.gzip_bytes_uploaded, 0, "a reuse, not an upload");
    assert_eq!(amend_count(&objects), 1, "the reuse has its amend record");
}

/// Regression (2026-11-14 review, MEDIUM): an amend record that cannot be
/// written withholds the drop (the partition stays for the next pass).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_amend_failure_keeps_the_partition() {
    let objects: StubObjects = Arc::new(Mutex::new(HashMap::new()));
    let inner = create_only_s3(Arc::clone(&objects));
    let s3: Responder = Arc::new(move |req: &SeenRequest| {
        if req.method == "PUT" && req.target.contains("/_daily/") {
            return (
                503,
                Vec::new(),
                b"<Error><Code>SlowDown</Code></Error>".to_vec(),
            );
        }
        inner(req)
    });
    let (qdb_url, qlog) = spawn_stub(qdb(NO_AUDIT, three_rows())).await;
    let (ilp_url, _) = spawn_stub(ilp_responder(204)).await;
    let (s3_url, _) = spawn_stub(s3).await;
    let mut cfg = test_retention_cfg(200);
    cfg.brutex_export_enabled = true;
    let mut archiver = build_archiver(&qdb_url, &ilp_url, &s3_url, cfg).await;
    let summary = archiver.run_archive_pass().await;
    assert_eq!(summary.dropped, 0, "drop withheld");
    assert!(
        !qlog
            .lock()
            .expect("q")
            .iter()
            .any(|r| r.target.contains("DROP PARTITION")),
        "no DROP sent"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_pending_reasons_name_a_lagging_table() {
    let responder = qdb_responder_tables(
        vec![("ticks", vec![PART])],
        three_rows(),
        UNSORTED,
        Arc::new(|_| (200, wal_tables_body(&[], &["ticks"]))),
    );
    let (qdb_url, _) = spawn_stub(responder).await;
    let (ilp_url, _) = spawn_stub(ilp_responder(204)).await;
    let (s3_url, _) = spawn_stub(create_only_s3(Arc::new(Mutex::new(HashMap::new())))).await;
    let archiver = build_archiver(&qdb_url, &ilp_url, &s3_url, test_retention_cfg(200)).await;
    let reasons = DailyExporter::from_archiver(archiver)
        .pending_reasons()
        .await;
    assert!(
        reasons.contains(&"wal_not_applied:ticks".to_string()),
        "{reasons:?}"
    );
}

/// Regression (2026-11-14 review, MEDIUM): `partition_archive_audit` not
/// applied is a reason, because its verified rows are how the export lists
/// an hour the archiver already dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_pending_reasons_name_a_lagging_archive_audit_table() {
    let body = "{\"columns\":[{\"name\":\"name\"},{\"name\":\"suspended\"},\
                {\"name\":\"writerTxn\"},{\"name\":\"sequencerTxn\"}],\
                \"dataset\":[[\"partition_archive_audit\",false,5,7],[\"ticks\",false,7,7]]}";
    let responder = qdb_responder_tables(
        vec![("ticks", vec![PART])],
        three_rows(),
        UNSORTED,
        Arc::new(move |_| (200, body.to_string())),
    );
    let (qdb_url, _) = spawn_stub(responder).await;
    let (ilp_url, _) = spawn_stub(ilp_responder(204)).await;
    let (s3_url, _) = spawn_stub(create_only_s3(Arc::new(Mutex::new(HashMap::new())))).await;
    let archiver = build_archiver(&qdb_url, &ilp_url, &s3_url, test_retention_cfg(200)).await;
    let reasons = DailyExporter::from_archiver(archiver)
        .pending_reasons()
        .await;
    assert!(
        reasons.contains(&"wal_not_applied:partition_archive_audit".to_string()),
        "{reasons:?}"
    );
    assert!(!reasons.contains(&"wal_not_applied:ticks".to_string()));
}

/// Regression (2026-11-14 review, HIGH): with less than the after-close floor
/// plus the partition's size free the export stops (not a failure) before
/// it takes the lock or exports anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_export_stops_when_the_disk_is_below_the_floor() {
    let objects: StubObjects = Arc::new(Mutex::new(HashMap::new()));
    let (qdb_url, qlog) = spawn_stub(qdb(NO_AUDIT, three_rows())).await;
    let (ilp_url, _) = spawn_stub(ilp_responder(204)).await;
    let (s3_url, s3_log) = spawn_stub(create_only_s3(Arc::clone(&objects))).await;
    let archiver = build_archiver(&qdb_url, &ilp_url, &s3_url, test_retention_cfg(200)).await;
    let mut ex = DailyExporter::from_archiver(archiver);
    let all = |total: u64| total;
    let out = run_with(&mut ex, &unbounded, &all)
        .await
        .expect("not an error");
    assert_eq!(out, ExportOutcome::Stopped(StopReason::Disk));
    assert!(
        !qlog
            .lock()
            .expect("q")
            .iter()
            .any(|r| r.target.starts_with("/exp")),
        "nothing exported"
    );
    assert!(puts(&s3_log).is_empty());
}

/// Regression (2026-11-14 evening audit, MEDIUM): free space is checked again
/// right after the quiesce lock is taken (the wait can last minutes, and a
/// writer beside the export can fill the disk meanwhile). Fine before the
/// lock and short after it: the lock is released, the export stops for disk
/// (not a failure), and nothing is exported or uploaded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_export_partition_quiesced_rechecks_the_disk_after_taking_the_lock() {
    let objects: StubObjects = Arc::new(Mutex::new(HashMap::new()));
    let (qdb_url, qlog) = spawn_stub(qdb(NO_AUDIT, three_rows())).await;
    let (ilp_url, _) = spawn_stub(ilp_responder(204)).await;
    let (s3_url, s3_log) = spawn_stub(create_only_s3(Arc::clone(&objects))).await;
    let archiver = build_archiver(&qdb_url, &ilp_url, &s3_url, test_retention_cfg(200)).await;
    let lock: &'static crate::archive_quiesce::ArchiveQuiesce =
        Box::leak(Box::new(crate::archive_quiesce::ArchiveQuiesce::new()));
    let mut ex = DailyExporter::with_quiesce(archiver, lock, std::time::Duration::from_secs(5));
    // The first check (before the lock) sees no floor; every later one (after
    // the lock) sees the whole volume as floor: the disk filled meanwhile.
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let fills = |total: u64| {
        if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            0
        } else {
            total
        }
    };
    let out = run_with(&mut ex, &unbounded, &fills)
        .await
        .expect("not an error");
    assert_eq!(out, ExportOutcome::Stopped(StopReason::Disk));
    assert!(
        calls.load(std::sync::atomic::Ordering::SeqCst) >= 2,
        "checked before and after the lock"
    );
    assert!(
        !qlog
            .lock()
            .expect("q")
            .iter()
            .any(|r| r.target.starts_with("/exp")),
        "nothing exported"
    );
    assert!(puts(&s3_log).is_empty(), "nothing uploaded");
    assert!(
        lock.hold_writers(std::time::Duration::from_millis(200))
            .await
            .is_some(),
        "the shared side was released"
    );
}

/// Regression (2026-11-14 review, MEDIUM): a partition never starts with
/// less than its worst case left before 08:30, and a lock wait cut by that
/// clock is a stop, never a failure and never a sort in the session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_export_never_starts_a_partition_it_cannot_finish_before_0830() {
    let objects: StubObjects = Arc::new(Mutex::new(HashMap::new()));
    let (qdb_url, qlog) = spawn_stub(qdb(NO_AUDIT, three_rows())).await;
    let (ilp_url, _) = spawn_stub(ilp_responder(204)).await;
    let (s3_url, s3_log) = spawn_stub(create_only_s3(Arc::clone(&objects))).await;
    let archiver = build_archiver(&qdb_url, &ilp_url, &s3_url, test_retention_cfg(200)).await;
    let mut ex = DailyExporter::from_archiver(archiver);
    let short = || {
        Some(std::time::Duration::from_secs(
            EXPORT_PARTITION_WORST_SECS - 1,
        ))
    };
    let out = run_with(&mut ex, &short, &no_floor)
        .await
        .expect("not an error");
    assert_eq!(out, ExportOutcome::Stopped(StopReason::Window));
    assert!(
        !qlog
            .lock()
            .expect("q")
            .iter()
            .any(|r| r.target.starts_with("/exp")),
        "nothing exported"
    );
    assert!(puts(&s3_log).is_empty());

    // Enough for the partition but not for the full lock wait: the archiver
    // holds the lock, the wait is cut to the 200 ms left over, and the
    // attempt stops instead of failing.
    let lock: &'static crate::archive_quiesce::ArchiveQuiesce =
        Box::leak(Box::new(crate::archive_quiesce::ArchiveQuiesce::new()));
    let held = lock
        .hold_writers(std::time::Duration::from_secs(1))
        .await
        .expect("archiver side");
    let (archiver, _) = setup(NO_AUDIT, three_rows(), Arc::clone(&objects), true).await;
    let mut ex = DailyExporter::with_quiesce(archiver, lock, std::time::Duration::from_secs(900));
    let tight = || {
        Some(
            std::time::Duration::from_secs(EXPORT_PARTITION_WORST_SECS)
                + std::time::Duration::from_millis(200),
        )
    };
    let out = run_with(&mut ex, &tight, &no_floor)
        .await
        .expect("a stop, not an error");
    assert_eq!(out, ExportOutcome::Stopped(StopReason::Window));
    drop(held);
}

/// QuestDB stub for the exact-gate archiver dropping [`PART`] while the
/// export reads it (`process_one` takes no quiesce lock): the size listing
/// shows it for the first two calls (the table listing and the re-list
/// under the lock), then not; every recount reads 0 rows.
fn qdb_dropped_by_the_exact_gate_mid_export(audit: &'static str) -> Responder {
    let inner = qdb(audit, Arc::new(|_| (0, None)));
    let listings = Arc::new(AtomicU64::new(0));
    Arc::new(move |req: &SeenRequest| {
        if req.target.contains("diskSize")
            && req.target.contains("table_partitions('ticks')")
            && listings.fetch_add(1, Ordering::SeqCst) >= 2
        {
            return (
                200,
                Vec::new(),
                b"{\"columns\":[{\"name\":\"name\"},{\"name\":\"diskSize\"}],\"dataset\":[]}"
                    .to_vec(),
            );
        }
        inner(req)
    })
}

/// Regression (2026-11-14 third review, MEDIUM): a partition the exact-gate
/// archiver drops (without the quiesce lock) while the export reads it is
/// recorded as dropped mid-export and listed from its verified copy; it was
/// a failed attempt (recount mismatch, then 0 rows), and three such races
/// gave up the day.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_partition_dropped_without_the_lock_mid_export_is_not_a_failure() {
    use sha2::Digest as _;
    let digest: [u8; 32] = sha2::Sha256::digest(b"bulk").into();
    let audit: &'static str = Box::leak(
        format!(
            "{{\"columns\":[],\"dataset\":[{}]}}",
            audit_row("ticks", PART, 2_000, &digest)
        )
        .into_boxed_str(),
    );
    let objects: StubObjects = Arc::new(Mutex::new(HashMap::new()));
    seeded_copy(&objects, "ticks", PART, &digest);
    let (qdb_url, qlog) = spawn_stub(qdb_dropped_by_the_exact_gate_mid_export(audit)).await;
    let (ilp_url, _) = spawn_stub(ilp_responder(204)).await;
    let (s3_url, s3_log) = spawn_stub(create_only_s3(Arc::clone(&objects))).await;
    let mut cfg = test_retention_cfg(200);
    cfg.brutex_export_enabled = true;
    let archiver = build_archiver(&qdb_url, &ilp_url, &s3_url, cfg).await;
    let mut ex = DailyExporter::from_archiver(archiver);
    let report =
        sealed_report(run(&mut ex, vec![], false).await.expect("not a failure")).expect("sealed");
    assert_eq!(report.rows, 2_000, "listed from the archive copy");
    assert!(report.complete);
    assert!(
        qlog.lock()
            .expect("q")
            .iter()
            .any(|r| r.target.starts_with("/exp")),
        "the export did run before the drop"
    );
    assert!(
        !puts(&s3_log)
            .iter()
            .any(|p| p.ends_with("ticks/2026-04-01T09.csv.gz")),
        "no partial object placed"
    );
}

/// Regression (2026-11-14 third review, MEDIUM): a FAILED HeadObject on the
/// canonical key is "may exist", so the archiver sorts and reuses the
/// export's object; read as absent it exported unordered and wrote a second
/// full copy kept forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_archiver_head_failure_still_orders_and_reuses() {
    let objects: StubObjects = Arc::new(Mutex::new(HashMap::new()));
    let (archiver, _) = setup(NO_AUDIT, three_rows(), Arc::clone(&objects), true).await;
    seal(&mut DailyExporter::from_archiver(archiver), vec![]).await;
    let inner = create_only_s3(Arc::clone(&objects));
    let failed_once = Arc::new(AtomicU64::new(0));
    let canonical = format!("/{}", stub_key(&canonical_archive_key("ticks", PART)));
    let s3: Responder = Arc::new(move |req: &SeenRequest| {
        if req.method == "HEAD"
            && req.target.split('?').next() == Some(canonical.as_str())
            && failed_once.fetch_add(1, Ordering::SeqCst) == 0
        {
            return (403, vec![("content-length".into(), "0".into())], Vec::new());
        }
        inner(req)
    });
    let (qdb_url, qlog) = spawn_stub(qdb(NO_AUDIT, three_rows())).await;
    let (ilp_url, _) = spawn_stub(ilp_responder(204)).await;
    let (s3_url, _) = spawn_stub(s3).await;
    let mut cfg = test_retention_cfg(200);
    cfg.brutex_export_enabled = true;
    let mut archiver = build_archiver(&qdb_url, &ilp_url, &s3_url, cfg).await;
    let summary = archiver.run_archive_pass().await;
    assert_eq!(summary.verified, 1);
    let exports: Vec<bool> = qlog
        .lock()
        .expect("q")
        .iter()
        .filter(|r| r.target.starts_with("/exp"))
        .map(|r| r.target.contains("ORDER BY"))
        .collect();
    assert_eq!(exports, vec![true], "the failed HEAD still orders");
    assert_eq!(summary.gzip_bytes_uploaded, 0, "the export's object reused");
    assert!(!has_sidecar(&objects), "no second copy");
}

/// Regression (2026-11-14 third review, MEDIUM x2): on the session day the
/// arrival-time tables' hours that may still take rows are left out (never
/// exported, never placed at their canonical key), and the manifest names
/// them; the hours before the cut are exported as usual.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_open_arrival_hours_are_left_out_of_the_session_day() {
    let late = "2026-04-01T16";
    let inner = qdb_responder_tables(
        vec![("ticks", vec![PART]), ("tick_quotes", vec![PART, late])],
        three_rows(),
        UNSORTED,
        Arc::new(|_| (200, wal_tables_body(&[], &[]))),
    );
    let responder: Responder = Arc::new(move |req: &SeenRequest| {
        if req.target.starts_with("/exp") && req.target.contains("ORDER BY") {
            return (200, Vec::new(), SORTED.as_bytes().to_vec());
        }
        if req.target.contains("FROM partition_archive_audit") {
            return (200, Vec::new(), NO_AUDIT.as_bytes().to_vec());
        }
        inner(req)
    });
    let (qdb_url, qlog) = spawn_stub(responder).await;
    let (ilp_url, _) = spawn_stub(ilp_responder(204)).await;
    let objects: StubObjects = Arc::new(Mutex::new(HashMap::new()));
    let (s3_url, s3_log) = spawn_stub(create_only_s3(Arc::clone(&objects))).await;
    let archiver = build_archiver(&qdb_url, &ilp_url, &s3_url, test_retention_cfg(200)).await;
    let mut ex = DailyExporter::from_archiver(archiver);
    let go = || true;
    let cut = day().and_hms_opt(16, 0, 0);
    let out = ex
        .export_day(
            day(),
            vec![],
            ExportOptions {
                keep_going: &go,
                time_left: &unbounded,
                floor_for_total: &no_floor,
                open_from: cut,
                last_chance: false,
            },
        )
        .await
        .expect("sealed");
    let report = sealed_report(out).expect("sealed");
    assert_eq!(report.objects, 2, "ticks T09 and tick_quotes T09 only");
    assert!(report.complete);
    assert!(
        !qlog
            .lock()
            .expect("q")
            .iter()
            .any(|r| r.target.starts_with("/exp")
                && r.target.contains("tick_quotes")
                && r.target.contains("2026-04-01T16:00")),
        "the open hour is never exported"
    );
    assert!(
        !puts(&s3_log)
            .iter()
            .any(|p| p.ends_with(&format!("tick_quotes/{late}.csv.gz"))),
        "nothing placed at the open hour's key"
    );
}

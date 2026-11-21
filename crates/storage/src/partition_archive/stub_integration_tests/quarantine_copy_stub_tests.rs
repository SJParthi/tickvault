//! The 20 Nov quarantine copy and drop (daily-universe Quote 63) against the
//! archiver's stub QuestDB, ILP and S3: copy first, verify, record, then the
//! drop through the last-moment recount; never an S3 delete, never a
//! canonical key, never a `partition_archive_audit` row.

use super::*;
use crate::day_repair_audit_persistence::DayRepairAuditWriter;
use crate::partition_archive::quarantine_copy::{
    QUARANTINE_PREFIX, QuarantineIo, QuarantineMode, QuarantineOutcome,
};
use crate::partition_archive::snapshot_gate::ArchiveIo as _;

const DAY_HOUR: &str = "2026-11-20T09";

fn stub_key(key: &str) -> String {
    format!("tv-test-cold/{key}")
}

fn primary_key() -> String {
    stub_key(&format!(
        "{QUARANTINE_PREFIX}/partitions/ticks/{DAY_HOUR}.csv.gz"
    ))
}

fn audit_writer(ilp_url: &str) -> DayRepairAuditWriter {
    let addr = ilp_url.trim_start_matches("http://");
    DayRepairAuditWriter::with_conf(&format!(
        "http::addr={addr};protocol_version=1;retry_timeout=0;request_timeout=5000;"
    ))
}

fn applied_wal() -> WalStub {
    Arc::new(|_probe| (200, wal_tables_body(&[], &[])))
}

fn steady_tally() -> TallyStub {
    Arc::new(|_call| (3, Some(STUB_MAX_TS.to_string())))
}

fn methods(log: &Arc<Mutex<Vec<SeenRequest>>>, method: &str) -> Vec<String> {
    log.lock()
        .expect("log")
        .iter()
        .filter(|r| r.method == method)
        .map(|r| r.target.clone())
        .collect()
}

fn targets(log: &Arc<Mutex<Vec<SeenRequest>>>) -> Vec<String> {
    log.lock()
        .expect("log")
        .iter()
        .map(|r| r.target.clone())
        .collect()
}

struct Rig {
    archiver: PartitionArchiver,
    audit: DayRepairAuditWriter,
    objects: StubObjects,
    qdb_log: Arc<Mutex<Vec<SeenRequest>>>,
    ilp_log: Arc<Mutex<Vec<SeenRequest>>>,
    s3_log: Arc<Mutex<Vec<SeenRequest>>>,
}

async fn rig(
    partitions: Vec<&'static str>,
    tally: TallyStub,
    wal: WalStub,
    ilp_status: u16,
) -> Rig {
    let objects: StubObjects = Arc::new(Mutex::new(HashMap::new()));
    let (qdb_url, qdb_log) =
        spawn_stub(qdb_responder_listing("ticks", partitions, tally, CSV, wal)).await;
    let (ilp_url, ilp_log) = spawn_stub(ilp_responder(ilp_status)).await;
    let (s3_url, s3_log) = spawn_stub(s3_responder(Arc::clone(&objects))).await;
    let archiver = build_archiver(&qdb_url, &ilp_url, &s3_url, test_retention_cfg(10)).await;
    Rig {
        archiver,
        audit: audit_writer(&ilp_url),
        objects,
        qdb_log,
        ilp_log,
        s3_log,
    }
}

/// One copy-only run of the test hour.
async fn copy_only(r: &mut Rig) -> QuarantineOutcome {
    r.archiver
        .quarantine_partition(
            "ticks",
            DAY_HOUR,
            QuarantineMode::CopyOnly,
            &mut r.audit,
            "abc",
        )
        .await
}

/// Nothing this repair does may touch the archive's own record or keys.
fn assert_no_archive_side_effects(r: &Rig) {
    assert!(
        methods(&r.s3_log, "DELETE").is_empty(),
        "the repair never deletes an S3 object"
    );
    assert!(
        targets(&r.s3_log)
            .iter()
            .all(|t| !t.contains(&format!("/{ARCHIVE_S3_PREFIX}/"))),
        "the repair never touches a canonical key: {:?}",
        targets(&r.s3_log)
    );
    assert_eq!(
        r.archiver.audit.pending(),
        0,
        "the repair never writes a partition_archive_audit row"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_quarantine_copy_and_drop_drops_after_a_verified_copy_and_never_deletes() {
    let mut r = rig(vec![DAY_HOUR], steady_tally(), applied_wal(), 204).await;
    let out = r
        .archiver
        .quarantine_partition(
            "ticks",
            DAY_HOUR,
            QuarantineMode::CopyAndDrop,
            &mut r.audit,
            "abc1234",
        )
        .await;
    let QuarantineOutcome::Dropped(copy) = out else {
        panic!("expected a drop, got {out:?}");
    };
    assert_eq!(copy.rows, Some(3));
    assert!(!copy.reused);
    assert_eq!(stub_key(&copy.s3_key), primary_key());
    assert!(r.objects.lock().expect("s3").contains_key(&primary_key()));
    let qdb = targets(&r.qdb_log);
    let drop_at = qdb
        .iter()
        .position(|t| t.contains("DROP PARTITION") && t.contains(DAY_HOUR))
        .expect("the partition was dropped");
    let put_at = methods(&r.s3_log, "PUT").len();
    assert_eq!(put_at, 1, "exactly one create-only upload");
    assert!(
        qdb[..drop_at].iter().any(|t| t.starts_with("/exp")),
        "the export ran before the drop"
    );
    assert!(
        qdb[..drop_at]
            .iter()
            .filter(|t| t.contains("count()"))
            .count()
            >= 2,
        "the verify recount and the last-moment recount both ran before the drop"
    );
    assert_eq!(
        methods(&r.ilp_log, "POST").len(),
        2,
        "one ACKED day_repair_audit row for the copy, one for the drop"
    );
    assert_no_archive_side_effects(&r);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_quarantine_copy_and_drop_keeps_when_the_recount_moves() {
    let tally: TallyStub = Arc::new(|call| {
        if call == 0 {
            (3, Some(STUB_MAX_TS.to_string()))
        } else {
            (4, Some("2026-11-20T09:59:59.500000Z".to_string()))
        }
    });
    let mut r = rig(vec![DAY_HOUR], tally, applied_wal(), 204).await;
    let out = r
        .archiver
        .quarantine_partition(
            "ticks",
            DAY_HOUR,
            QuarantineMode::CopyAndDrop,
            &mut r.audit,
            "abc1234",
        )
        .await;
    assert!(
        matches!(
            out,
            QuarantineOutcome::Kept {
                stage: "count_mismatch",
                ..
            }
        ),
        "{out:?}"
    );
    assert!(
        !targets(&r.qdb_log)
            .iter()
            .any(|t| t.contains("DROP PARTITION")),
        "a moved recount never drops"
    );
    assert!(
        r.objects.lock().expect("s3").contains_key(&primary_key()),
        "the verified copy stays in S3"
    );
    assert_no_archive_side_effects(&r);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_quarantine_copy_only_reuses_on_rerun_and_sidecars_different_bytes() {
    let mut r = rig(vec![DAY_HOUR], steady_tally(), applied_wal(), 204).await;
    let first = copy_only(&mut r).await;
    assert!(
        matches!(&first, QuarantineOutcome::Copied(c) if !c.reused),
        "{first:?}"
    );
    let second = copy_only(&mut r).await;
    assert!(
        matches!(&second, QuarantineOutcome::Copied(c) if c.reused),
        "an identical object is reused: {second:?}"
    );
    assert_eq!(methods(&r.s3_log, "PUT").len(), 1, "no second upload");

    // Different bytes at the primary key: a content-addressed copy beside it.
    r.objects
        .lock()
        .expect("s3")
        .insert(primary_key(), (1, Some("AAAA".to_string())));
    let third = copy_only(&mut r).await;
    let QuarantineOutcome::Copied(side) = third else {
        panic!("expected a sidecar copy, got {third:?}");
    };
    assert!(side.s3_key.contains(".sha-"), "{}", side.s3_key);
    assert!(side.s3_key.starts_with(QUARANTINE_PREFIX));
    assert_eq!(
        r.objects.lock().expect("s3").get(&primary_key()),
        Some(&(1, Some("AAAA".to_string()))),
        "the different object is never overwritten"
    );
    let fourth = copy_only(&mut r).await;
    assert!(
        matches!(&fourth, QuarantineOutcome::Copied(c) if c.reused && c.s3_key == side.s3_key),
        "{fourth:?}"
    );
    assert_eq!(methods(&r.s3_log, "PUT").len(), 2);
    assert!(
        !targets(&r.qdb_log)
            .iter()
            .any(|t| t.contains("DROP PARTITION")),
        "copy-only never drops"
    );
    assert_no_archive_side_effects(&r);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_quarantine_refuses_depth_other_days_and_audit_tables_without_any_request() {
    let mut r = rig(vec![DAY_HOUR], steady_tally(), applied_wal(), 204).await;
    for (table, partition) in [
        ("market_depth_book", DAY_HOUR),
        ("ticks", "2026-11-21T00"),
        ("ticks", "2026-11-19T15"),
        ("partition_archive_audit", "2026-11-20"),
        ("dhan_live_crossverify_daily", "2026-11-20"),
    ] {
        let out = r
            .archiver
            .quarantine_partition(
                table,
                partition,
                QuarantineMode::CopyAndDrop,
                &mut r.audit,
                "abc",
            )
            .await;
        assert_eq!(
            out,
            QuarantineOutcome::Refused {
                reason: "not_allowed"
            },
            "{table} {partition}"
        );
    }
    assert!(targets(&r.qdb_log).is_empty(), "no QuestDB request");
    assert!(targets(&r.s3_log).is_empty(), "no S3 request");
    assert_no_archive_side_effects(&r);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_quarantine_delete_own_upload_never_calls_s3() {
    let mut r = rig(vec![DAY_HOUR], steady_tally(), applied_wal(), 204).await;
    let mut io = QuarantineIo::new(&r.archiver, &mut r.audit, "abc");
    let deleted = io.delete_own_upload(&primary_key()).await;
    assert!(deleted.is_err(), "a delete is refused");
    assert!(targets(&r.s3_log).is_empty(), "and S3 is never asked");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_quarantine_keeps_when_the_wal_is_lagging() {
    let lagging: WalStub = Arc::new(|_probe| (200, wal_tables_body(&[], &["ticks"])));
    let mut r = rig(vec![DAY_HOUR], steady_tally(), lagging, 204).await;
    let out = r
        .archiver
        .quarantine_partition(
            "ticks",
            DAY_HOUR,
            QuarantineMode::CopyAndDrop,
            &mut r.audit,
            "abc",
        )
        .await;
    assert!(
        matches!(
            out,
            QuarantineOutcome::Kept {
                stage: "wal_not_applied",
                ..
            }
        ),
        "{out:?}"
    );
    assert!(
        !targets(&r.qdb_log).iter().any(|t| t.starts_with("/exp")),
        "nothing exported while the WAL is behind"
    );
    assert!(methods(&r.s3_log, "PUT").is_empty());
    assert_no_archive_side_effects(&r);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_quarantine_withholds_the_drop_when_the_copy_row_is_not_acked() {
    let mut r = rig(vec![DAY_HOUR], steady_tally(), applied_wal(), 500).await;
    let out = r
        .archiver
        .quarantine_partition(
            "ticks",
            DAY_HOUR,
            QuarantineMode::CopyAndDrop,
            &mut r.audit,
            "abc",
        )
        .await;
    assert!(
        matches!(
            out,
            QuarantineOutcome::Kept {
                stage: "audit_failed",
                ..
            }
        ),
        "{out:?}"
    );
    assert!(
        !targets(&r.qdb_log)
            .iter()
            .any(|t| t.contains("DROP PARTITION")),
        "no ACKED copy row, no drop"
    );
    assert_no_archive_side_effects(&r);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_quarantine_local_file_copies_once_then_reuses() {
    let mut r = rig(vec![DAY_HOUR], steady_tally(), applied_wal(), 204).await;
    let dir = unique_temp_dir();
    std::fs::create_dir_all(&dir).expect("dir");
    let path = dir.join("seals_v5-20261120.bin");
    std::fs::write(&path, b"sealed candle bytes of 20 Nov").expect("write");
    let name = "seal-spill/seals_v5-20261120.bin";

    let first = r
        .archiver
        .quarantine_local_file(&path, name, &mut r.audit, "abc")
        .await;
    let QuarantineOutcome::Copied(copy) = first else {
        panic!("expected a copy, got {first:?}");
    };
    assert!(!copy.reused);
    assert_eq!(copy.bytes, 29);
    assert_eq!(copy.s3_key, format!("{QUARANTINE_PREFIX}/files/{name}"));
    let second = r
        .archiver
        .quarantine_local_file(&path, name, &mut r.audit, "abc")
        .await;
    assert!(
        matches!(&second, QuarantineOutcome::Copied(c) if c.reused),
        "{second:?}"
    );
    assert_eq!(methods(&r.s3_log, "PUT").len(), 1);
    assert!(
        path.exists(),
        "the local file is never moved or deleted here"
    );

    let refused = r
        .archiver
        .quarantine_local_file(&path, "../escape.bin", &mut r.audit, "abc")
        .await;
    assert_eq!(
        refused,
        QuarantineOutcome::Refused {
            reason: "bad_file_name"
        }
    );
    let missing = r
        .archiver
        .quarantine_local_file(&dir.join("absent.bin"), "absent.bin", &mut r.audit, "abc")
        .await;
    assert!(
        matches!(
            missing,
            QuarantineOutcome::Kept {
                stage: "read_failed",
                ..
            }
        ),
        "{missing:?}"
    );
    assert_no_archive_side_effects(&r);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_repair_day_partitions_lists_only_the_repair_day() {
    let r = rig(
        vec![
            "2026-11-19T15",
            "2026-11-20T09",
            "2026-11-20T10",
            "2026-11-21T00",
        ],
        steady_tally(),
        applied_wal(),
        204,
    )
    .await;
    let listed = r
        .archiver
        .repair_day_partitions("ticks")
        .await
        .expect("listing");
    assert_eq!(
        listed,
        vec![
            ("2026-11-20T09".to_string(), STUB_PARTITION_BYTES),
            ("2026-11-20T10".to_string(), STUB_PARTITION_BYTES),
        ]
    );
    let depth = r
        .archiver
        .repair_day_partitions("market_depth_book")
        .await
        .expect("listing");
    assert!(depth.is_empty(), "depth is never a repair partition");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_canonical_object_reads_without_writing() {
    let r = rig(vec![DAY_HOUR], steady_tally(), applied_wal(), 204).await;
    assert_eq!(
        r.archiver
            .canonical_object("ticks", DAY_HOUR)
            .await
            .expect("head"),
        None
    );
    r.objects.lock().expect("s3").insert(
        stub_key(&canonical_archive_key("ticks", DAY_HOUR)),
        (42, Some("BBBB".to_string())),
    );
    let meta = r
        .archiver
        .canonical_object("ticks", DAY_HOUR)
        .await
        .expect("head")
        .expect("present");
    assert_eq!(meta.len, 42);
    assert!(methods(&r.s3_log, "PUT").is_empty(), "read only");
}

//! The ordered export in `ts` slices (`sliced_export`, 2026-11-15) against a
//! stub QuestDB that answers each slice with its own rows: the joined gzip,
//! its SHA-256, the row count and the CSV byte count equal the single
//! ordered query's for the same rows.

use super::*;
use crate::partition_archive::sliced_export::ordered_export_slices;

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
    let Err(err) = archiver
        .export_ordered_csv(
            "ticks",
            HOUR,
            crate::tick_persistence::DEDUP_KEY_TICKS,
            path.clone(),
        )
        .await
    else {
        panic!("a header mismatch must fail the export");
    };
    assert!(format!("{err:#}").contains("header differs"), "{err:#}");
    assert!(!path.exists(), "the temp file is removed");
}

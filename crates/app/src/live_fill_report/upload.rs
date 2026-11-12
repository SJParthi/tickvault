//! Writes a day's report files and copies them to
//! `s3://tv-<env>-cold/live-fill-logs/<D>/` (plan item LF-7).
//!
//! The S3 half reuses the raw-frame uploader's copy step
//! (`raw_frame_upload::copy_file_verified`: gzip, conditional create,
//! HeadObject verify, content-addressed sidecar). The local record of each
//! copy is written HERE, never through the raw-frame uploader's marker path:
//! that path feeds the paged `cold-backup-failing` alarm, logs a market-data
//! STORAGE-GAP-04 line and holds the file in the prunes' in-memory set (LF-7
//! review findings 4 and 6). A failed record here is reported only as
//! LIVE-FILL-01 (`stage="marker"`). A key already holding other bytes gets a
//! sidecar key, never an overwrite; nothing here deletes, locally or in the
//! bucket. The records live in a per-day `uploaded/` folder, so two days'
//! files can never share one. Before the day's first copy starts, a
//! `copy-started` record is written there: once it exists the day's files are
//! never rebuilt (finding 5), so the bucket never holds a mix of two builds.
//! Cold, O(bytes).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tickvault_storage::raw_frame_upload::{ColdObjectStore, SegmentOutcome, copy_file_verified};

use super::render::report_file_names;

/// Local root of the report folders.
pub const LIVE_FILL_LOGS_DIR: &str = "data/live-fill-logs";

/// Bucket prefix of the report copies.
pub const LIVE_FILL_LOGS_PREFIX: &str = "live-fill-logs";

/// Copy tries per run.
pub const UPLOAD_TRIES: u32 = 3;

/// Spacing between copy tries.
pub const UPLOAD_SPACING_SECS: u64 = 60;

/// The record written in `uploaded/` before the day's first copy starts.
pub const COPY_STARTED_RECORD: &str = "copy-started";

/// `live-fill-logs/<D>/<name>.gz`, or `None` when `day` is not a
/// `YYYY-MM-DD` date or `name` is empty or holds `/`, `\` or `..`. Pure.
#[must_use]
pub fn live_fill_report_key(day: &str, name: &str) -> Option<String> {
    chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").ok()?;
    if day.len() != 10
        || name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
    {
        return None;
    }
    Some(format!("{LIVE_FILL_LOGS_PREFIX}/{day}/{name}.gz"))
}

/// The day's local folder under `base`. Pure.
#[must_use]
pub fn day_dir(base: &Path, day: &str) -> PathBuf {
    base.join(day)
}

/// The day's upload markers folder (per day, so markers never collide). Pure.
#[must_use]
pub fn markers_dir(day_dir: &Path) -> PathBuf {
    day_dir.join("uploaded")
}

/// Whether all five files of `day` are in `dir`. O(5) stats.
#[must_use]
pub fn folder_complete(dir: &Path, day: &str) -> bool {
    report_file_names(day).iter().all(|n| dir.join(n).is_file())
}

/// Whether a copy of the day's folder has ever started: the `uploaded/`
/// folder holds any entry (the `copy-started` record, or a file's copy
/// record). Once it does, the day's files are never rebuilt (LF-7 review
/// finding 5), so a retry or a same-day restart cannot leave the bucket
/// holding some files from one build and some from another. An unreadable
/// folder reads as started (keep the files). The read stops at the first
/// entry, O(1) entries.
#[must_use]
pub fn copy_started(day_dir: &Path) -> bool {
    match std::fs::read_dir(markers_dir(day_dir)) {
        Ok(mut entries) => entries.next().is_some(),
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    }
}

/// Whether every one of the day's five files has its local copy record in
/// `uploaded/` (written only after a verified copy). Unlike the day marker,
/// which `data/state/daily` sweeps after 7 days, these records are never
/// swept, so they say "copied" for a day of any age. O(5) stats.
#[must_use]
pub fn folder_copied(dir: &Path, day: &str) -> bool {
    let markers = markers_dir(dir);
    report_file_names(day)
        .iter()
        .all(|n| markers.join(n).is_file())
}

/// A day folder under the local root that is not copied and that the
/// boot's lookback no longer reaches (LF-7 review round 3, finding 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreportedDay {
    /// The folder's day.
    pub day: chrono::NaiveDate,
    /// All five files are present (a copy-only catch-up can finish it).
    pub complete: bool,
}

/// The day folders under `root` dated before `today`, not reached by the
/// lookback (`in_lookback` false) and not copied ([`folder_copied`] false),
/// oldest first. A folder whose name is not a `YYYY-MM-DD` date is ignored; a
/// missing root holds none. Once per boot, cold, O(folders) entries plus
/// O(5) stats per day folder.
///
/// # Errors
/// The root exists but cannot be listed.
pub fn unreported_day_folders(
    root: &Path,
    today: chrono::NaiveDate,
    in_lookback: impl Fn(chrono::NaiveDate) -> bool,
) -> std::io::Result<Vec<UnreportedDay>> {
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Ok(day) = chrono::NaiveDate::parse_from_str(name, "%Y-%m-%d") else {
            continue;
        };
        if name.len() != 10 || day >= today || in_lookback(day) {
            continue;
        }
        let dir = entry.path();
        if folder_copied(&dir, name) {
            continue;
        }
        out.push(UnreportedDay {
            day,
            complete: folder_complete(&dir, name),
        });
    }
    out.sort_by_key(|u| u.day);
    Ok(out)
}

/// Writes `body` to `dir/name` durably: temp file, `sync_all`, rename, then
/// one sync of the folder. O(bytes).
fn write_record(dir: &Path, name: &str, body: &[u8]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{name}.tmp"));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(body)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, dir.join(name))?;
    std::fs::File::open(dir)?.sync_all()
}

/// Writes each file atomically: temp file, `sync_all`, rename, then one sync
/// of the folder. Never deletes a file. O(bytes).
///
/// # Errors
/// The first failed step.
pub fn write_report_files(dir: &Path, files: &[(String, String)]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    for (name, body) in files {
        let tmp = dir.join(format!("{name}.tmp"));
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(body.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, dir.join(name))?;
    }
    std::fs::File::open(dir)?.sync_all()
}

/// What one copy pass did, per file in upload order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UploadSummary {
    /// `(file name, outcome)`: `uploaded`, `uploaded_sidecar`, `reused`,
    /// `failed:<why>`, `timed_out:<why>`, `marker_failed:<why>`, `bad_key`
    /// or `missing`.
    pub files: Vec<(String, String)>,
}

impl UploadSummary {
    /// Whether every one of the five files is verified in the bucket: the
    /// only state that writes the day's marker. O(5).
    #[must_use]
    pub fn all_verified(&self) -> bool {
        self.files.len() == 5
            && self
                .files
                .iter()
                .all(|(_, o)| o == "uploaded" || o == "uploaded_sidecar" || o == "reused")
    }
}

fn outcome_text(o: &SegmentOutcome) -> String {
    match o {
        SegmentOutcome::Uploaded { .. } => "uploaded".to_owned(),
        SegmentOutcome::UploadedSidecar { .. } => "uploaded_sidecar".to_owned(),
        SegmentOutcome::Reused { .. } => "reused".to_owned(),
        SegmentOutcome::Failed { reason } => format!("failed:{reason}"),
        SegmentOutcome::TimedOut { reason } => format!("timed_out:{reason}"),
    }
}

/// One file's copy, then its local record (`uploaded/<name>`, body = the key
/// that holds the bytes). A verified copy whose record cannot be written
/// reads `marker_failed:<why>` (not verified, so no day marker); nothing is
/// counted or held outside this module. O(file bytes).
async fn copy_one<S: ColdObjectStore>(store: &S, markers: &Path, path: &Path, key: &str) -> String {
    let outcome = copy_file_verified(store, path, key).await;
    let stored_key = match &outcome {
        SegmentOutcome::Uploaded { key }
        | SegmentOutcome::UploadedSidecar { key }
        | SegmentOutcome::Reused { key } => key.clone(),
        SegmentOutcome::Failed { .. } | SegmentOutcome::TimedOut { .. } => {
            return outcome_text(&outcome);
        }
    };
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return "marker_failed:no file name".to_owned();
    };
    match write_record(markers, name, stored_key.as_bytes()) {
        Ok(()) => outcome_text(&outcome),
        Err(e) => format!("marker_failed:{e}"),
    }
}

/// One copy pass over the day's five files. A file already in the bucket
/// with the same bytes is reused (one HeadObject). The `copy-started` record
/// is written before the first copy; when it cannot be written nothing is
/// copied this pass (`failed:copy_start_record:<why>`), so a later rebuild
/// can never mix with a copy it does not know about. O(bytes).
pub async fn upload_day<S: ColdObjectStore>(store: &S, dir: &Path, day: &str) -> UploadSummary {
    let markers = markers_dir(dir);
    let started = if copy_started(dir) {
        Ok(())
    } else {
        write_record(&markers, COPY_STARTED_RECORD, day.as_bytes())
    };
    let mut files = Vec::with_capacity(5);
    for name in report_file_names(day) {
        let path = dir.join(&name);
        let outcome = if !path.is_file() {
            "missing".to_owned()
        } else if let Err(e) = &started {
            format!("failed:copy_start_record:{e}")
        } else if let Some(key) = live_fill_report_key(day, &name) {
            copy_one(store, &markers, &path, &key).await
        } else {
            "bad_key".to_owned()
        };
        files.push((name, outcome));
    }
    UploadSummary { files }
}

/// [`upload_day`] up to `tries` times, `spacing` apart, stopping at the first
/// pass that verifies all five files. The local files are kept whatever the
/// outcome. Returns the last pass and the passes run. O(tries × bytes).
pub async fn upload_day_with_retries<S: ColdObjectStore>(
    store: &S,
    dir: &Path,
    day: &str,
    tries: u32,
    spacing: Duration,
) -> (UploadSummary, u32) {
    let mut last = UploadSummary::default();
    let mut ran = 0;
    for t in 0..tries.max(1) {
        if t > 0 {
            tokio::time::sleep(spacing).await;
        }
        ran += 1;
        last = upload_day(store, dir, day).await;
        if last.all_verified() {
            break;
        }
    }
    (last, ran)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tickvault_storage::s3_cold::S3ObjectMeta;

    const DAY: &str = "2026-11-12";

    /// In-memory bucket: key -> (length, base64 SHA-256). Never deletes.
    #[derive(Default)]
    struct FakeBucket {
        objects: Mutex<HashMap<String, (u64, String)>>,
        puts: AtomicUsize,
        fail_puts: bool,
    }

    impl ColdObjectStore for FakeBucket {
        fn bucket_name(&self) -> &str {
            "tv-test-cold"
        }

        fn call_budget(&self, _gzip_len: u64) -> Duration {
            Duration::from_secs(60)
        }

        async fn head_object(&self, key: &str) -> anyhow::Result<Option<S3ObjectMeta>> {
            let objects = self
                .objects
                .lock()
                .map_err(|_| anyhow::anyhow!("poisoned"))?;
            Ok(objects.get(key).map(|(len, sha)| S3ObjectMeta {
                len: *len,
                checksum_sha256_b64: Some(sha.clone()),
            }))
        }

        async fn put_object_if_absent(
            &self,
            key: &str,
            body: Vec<u8>,
            sha256_b64: &str,
            _metadata: &[(&'static str, String)],
        ) -> anyhow::Result<()> {
            self.puts.fetch_add(1, Ordering::SeqCst);
            if self.fail_puts {
                anyhow::bail!("injected PutObject failure");
            }
            let mut objects = self
                .objects
                .lock()
                .map_err(|_| anyhow::anyhow!("poisoned"))?;
            if objects.contains_key(key) {
                anyhow::bail!("412 precondition failed");
            }
            objects.insert(key.to_string(), (body.len() as u64, sha256_b64.to_string()));
            Ok(())
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "tv_live_fill_report_{tag}_{}_{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn five_files(day: &str) -> Vec<(String, String)> {
        report_file_names(day)
            .into_iter()
            .map(|n| {
                let body = format!("body of {n}\n");
                (n, body)
            })
            .collect()
    }

    #[test]
    fn test_live_fill_report_key_under_prefix_refuses_slash_and_dotdot() {
        assert_eq!(
            live_fill_report_key(DAY, "orders-2026-11-12.csv").as_deref(),
            Some("live-fill-logs/2026-11-12/orders-2026-11-12.csv.gz")
        );
        for bad in ["", "a/b", "..", "x..y", "a\\b"] {
            assert!(live_fill_report_key(DAY, bad).is_none(), "{bad}");
        }
        for bad_day in ["2026-13-01", "../2026", "2026-11-12/x", "2026-1-2"] {
            assert!(
                live_fill_report_key(bad_day, "a.csv").is_none(),
                "{bad_day}"
            );
        }
    }

    #[test]
    fn test_markers_dir_and_day_dir_are_per_day() {
        let base = Path::new("data/live-fill-logs");
        let a = markers_dir(&day_dir(base, "2026-11-12"));
        let b = markers_dir(&day_dir(base, "2026-11-13"));
        assert_ne!(a, b);
        assert_eq!(a, Path::new("data/live-fill-logs/2026-11-12/uploaded"));
    }

    #[test]
    fn test_write_report_files_and_folder_complete() {
        let base = scratch("write");
        let dir = day_dir(&base, DAY);
        assert!(!folder_complete(&dir, DAY));
        write_report_files(&dir, &five_files(DAY)).expect("write");
        assert!(folder_complete(&dir, DAY));
        let body = std::fs::read_to_string(dir.join(format!("report-{DAY}.json"))).unwrap();
        assert_eq!(body, format!("body of report-{DAY}.json\n"));
        assert!(!dir.join(format!("report-{DAY}.json.tmp")).exists());
    }

    #[tokio::test]
    async fn test_upload_day_marker_only_after_all_verified_all_five_files() {
        let base = scratch("verified");
        let dir = day_dir(&base, DAY);
        write_report_files(&dir, &five_files(DAY)).expect("write");
        let bucket = FakeBucket::default();
        let summary = upload_day(&bucket, &dir, DAY).await;
        assert!(summary.all_verified(), "{summary:?}");
        assert_eq!(bucket.puts.load(Ordering::SeqCst), 5);
        let keys: Vec<String> = bucket.objects.lock().unwrap().keys().cloned().collect();
        assert!(
            keys.iter()
                .all(|k| k.starts_with("live-fill-logs/2026-11-12/"))
        );
        // One file missing: not all verified, so no marker.
        std::fs::rename(
            dir.join(format!("rejects-{DAY}.csv")),
            base.join("moved.csv"),
        )
        .unwrap();
        let summary = upload_day(&bucket, &dir, DAY).await;
        assert!(!summary.all_verified());
        assert!(summary.files.iter().any(|(_, o)| o == "missing"));
        assert!(!UploadSummary::default().all_verified());
    }

    #[tokio::test]
    async fn test_upload_day_with_retries_three_failures_keep_files_no_marker() {
        let base = scratch("fail");
        let dir = day_dir(&base, DAY);
        write_report_files(&dir, &five_files(DAY)).expect("write");
        let bucket = FakeBucket {
            fail_puts: true,
            ..FakeBucket::default()
        };
        let (summary, ran) =
            upload_day_with_retries(&bucket, &dir, DAY, UPLOAD_TRIES, Duration::ZERO).await;
        assert_eq!(ran, 3);
        assert!(!summary.all_verified());
        assert!(summary.files.iter().all(|(_, o)| o.starts_with("failed:")));
        // Every local file is kept.
        assert!(folder_complete(&dir, DAY));
        assert_eq!(bucket.puts.load(Ordering::SeqCst), 15);
    }

    #[tokio::test]
    async fn test_rerun_reuses_identical_objects() {
        let base = scratch("rerun");
        let dir = day_dir(&base, DAY);
        write_report_files(&dir, &five_files(DAY)).expect("write");
        let bucket = FakeBucket::default();
        let (first, ran) =
            upload_day_with_retries(&bucket, &dir, DAY, UPLOAD_TRIES, Duration::ZERO).await;
        assert!(first.all_verified());
        assert_eq!(ran, 1);
        let again = upload_day(&bucket, &dir, DAY).await;
        assert!(again.all_verified());
        assert_eq!(bucket.puts.load(Ordering::SeqCst), 5, "no second put");
        // Different bytes for the same day never overwrite: a sidecar key.
        write_report_files(
            &dir,
            &[(format!("orders-{DAY}.csv"), "changed\n".to_owned())],
        )
        .expect("rewrite");
        std::fs::remove_dir_all(markers_dir(&dir)).unwrap();
        let changed = upload_day(&bucket, &dir, DAY).await;
        assert!(changed.all_verified(), "{changed:?}");
        assert!(changed.files.iter().any(|(_, o)| o == "uploaded_sidecar"));
    }

    #[tokio::test]
    async fn test_leftover_day_uploaded_next_boot() {
        // A day whose copy failed keeps its folder; a later run with the
        // bucket back copies it without rebuilding.
        let base = scratch("leftover");
        let dir = day_dir(&base, "2026-11-11");
        write_report_files(&dir, &five_files("2026-11-11")).expect("write");
        let down = FakeBucket {
            fail_puts: true,
            ..FakeBucket::default()
        };
        let (failed, _) =
            upload_day_with_retries(&down, &dir, "2026-11-11", 1, Duration::ZERO).await;
        assert!(!failed.all_verified());
        assert!(folder_complete(&dir, "2026-11-11"));
        let up = FakeBucket::default();
        let (ok, _) =
            upload_day_with_retries(&up, &dir, "2026-11-11", UPLOAD_TRIES, Duration::ZERO).await;
        assert!(ok.all_verified());
    }

    /// LF-7 review round 3, finding 2: a folder four trading days old whose
    /// copy kept failing is found by the boot scan (the lookback no longer
    /// reaches it), copied by a later pass, and then no longer listed. An
    /// incomplete folder is listed as incomplete; a copied one, a non-date
    /// folder, today and a day the lookback reaches are not listed.
    #[tokio::test]
    async fn test_unreported_day_folders_finds_an_old_unmarked_day_and_folder_copied_clears_it() {
        let base = scratch("old_unmarked");
        let today = chrono::NaiveDate::from_ymd_opt(2026, 11, 16).unwrap(); // Monday
        let old = "2026-11-10"; // four trading days back
        let old_dir = day_dir(&base, old);
        write_report_files(&old_dir, &five_files(old)).expect("write");
        let down = FakeBucket {
            fail_puts: true,
            ..FakeBucket::default()
        };
        let (failed, _) = upload_day_with_retries(&down, &old_dir, old, 1, Duration::ZERO).await;
        assert!(!failed.all_verified());
        assert!(!folder_copied(&old_dir, old));
        // An incomplete older folder, a copied one, a lookback day, today
        // and a stray folder.
        let partial = "2026-11-09";
        write_report_files(
            &day_dir(&base, partial),
            &[(format!("orders-{partial}.csv"), "x\n".to_owned())],
        )
        .expect("write");
        let copied = "2026-11-06";
        let copied_dir = day_dir(&base, copied);
        write_report_files(&copied_dir, &five_files(copied)).expect("write");
        let ok = FakeBucket::default();
        assert!(upload_day(&ok, &copied_dir, copied).await.all_verified());
        assert!(folder_copied(&copied_dir, copied));
        for d in ["2026-11-13", "2026-11-16"] {
            write_report_files(&day_dir(&base, d), &five_files(d)).expect("write");
        }
        std::fs::create_dir_all(base.join("not-a-day")).unwrap();
        let lookback =
            |d: chrono::NaiveDate| d >= chrono::NaiveDate::from_ymd_opt(2026, 11, 11).unwrap();
        let found = unreported_day_folders(&base, today, lookback).expect("scan");
        assert_eq!(
            found,
            vec![
                UnreportedDay {
                    day: chrono::NaiveDate::from_ymd_opt(2026, 11, 9).unwrap(),
                    complete: false,
                },
                UnreportedDay {
                    day: chrono::NaiveDate::from_ymd_opt(2026, 11, 10).unwrap(),
                    complete: true,
                },
            ]
        );
        // The bucket is back: a copy-only pass finishes the old day.
        let (done, _) =
            upload_day_with_retries(&ok, &old_dir, old, UPLOAD_TRIES, Duration::ZERO).await;
        assert!(done.all_verified());
        assert!(folder_copied(&old_dir, old));
        let after = unreported_day_folders(&base, today, lookback).expect("scan");
        assert_eq!(after.len(), 1);
        assert!(!after[0].complete);
        // A missing root holds none.
        assert!(
            unreported_day_folders(&base.join("absent"), today, lookback)
                .expect("scan")
                .is_empty()
        );
    }

    /// LF-7 review findings 4 and 6: a verified copy whose local record cannot
    /// be written reads `marker_failed`, and touches nothing shared: not the
    /// raw-frame uploader's `tv_raw_upload_marker_write_failed_total` (an
    /// input of the paged `cold-backup-failing` alarm) and not the prunes'
    /// in-memory verified-unmarked set. A non-empty folder where the record
    /// goes makes its rename fail, as a full disk would.
    #[test]
    fn test_upload_day_marker_failure_touches_no_shared_counter_or_prune_set() {
        let base = scratch("marker_fail");
        let dir = day_dir(&base, DAY);
        write_report_files(&dir, &five_files(DAY)).expect("write");
        let report = format!("report-{DAY}.json");
        let blocker = markers_dir(&dir).join(&report);
        std::fs::create_dir_all(blocker.join("x")).unwrap();
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let summary = metrics::with_local_recorder(&recorder, || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let bucket = FakeBucket::default();
                    upload_day(&bucket, &dir, DAY).await
                })
        });
        let rendered = handle.render();
        assert!(
            !rendered.contains(tickvault_storage::raw_frame_upload::MARKER_WRITE_FAILED_COUNTER),
            "the report must not feed the raw-frame marker counter:\n{rendered}"
        );
        let len = std::fs::metadata(dir.join(&report)).unwrap().len();
        assert!(
            !tickvault_storage::raw_frame_upload::marker_matches(
                &markers_dir(&dir),
                &dir.join(&report),
                len
            ),
            "nothing may enter the prunes' verified-unmarked set"
        );
        let (_, outcome) = summary
            .files
            .iter()
            .find(|(n, _)| *n == report)
            .expect("report file in the pass");
        assert!(outcome.starts_with("marker_failed:"), "{outcome}");
        assert!(!summary.all_verified(), "no day marker on a failed record");
    }

    /// LF-7 review finding 5: the `copy-started` record exists from the first
    /// pass on, so `copy_started` reads true even when the copy then failed;
    /// an unreadable records folder reads as started and verifies nothing.
    #[tokio::test]
    async fn test_copy_started_after_first_pass_and_unreadable_records_folder() {
        let base = scratch("started");
        let dir = day_dir(&base, DAY);
        write_report_files(&dir, &five_files(DAY)).expect("write");
        assert!(!copy_started(&dir));
        let down = FakeBucket {
            fail_puts: true,
            ..FakeBucket::default()
        };
        let failed = upload_day(&down, &dir, DAY).await;
        assert!(!failed.all_verified());
        assert!(copy_started(&dir), "a failed copy still counts as started");
        assert!(markers_dir(&dir).join(COPY_STARTED_RECORD).is_file());
        // An unreadable records folder reads as started (never rebuilt) and
        // no file can be recorded, so nothing is verified.
        let other = day_dir(&base, "2026-11-13");
        write_report_files(&other, &five_files("2026-11-13")).expect("write");
        std::fs::write(markers_dir(&other), b"blocker").unwrap();
        assert!(copy_started(&other));
        let bucket = FakeBucket::default();
        let none = upload_day(&bucket, &other, "2026-11-13").await;
        assert!(!none.all_verified());
        assert!(
            none.files
                .iter()
                .all(|(_, o)| o.starts_with("marker_failed:")),
            "{none:?}"
        );
    }
}

//! Read-only fetch of one IST day of raw WAL segments from
//! `s3://<cold bucket>/raw-frames/<YYYY-MM-DD>/` for the offline live-fill
//! replay (plan item LF-8). ListObjectsV2 and GetObject ONLY: this module
//! never puts, copies or deletes in the bucket (a source scan,
//! `crates/app/tests/live_fill_replay_isolation_guard.rs`, fails the build if
//! it ever does).
//!
//! The uploader (`raw_frame_upload`) files a segment under the IST date of its
//! name, so a day's first frames can sit in the PREVIOUS date's last segment:
//! the listing also takes that one segment, and the replay keeps only frames
//! whose receipt falls on the day. A key that holds different bytes than its
//! canonical name is uploaded beside it as `<name>.sha-<hex>.gz`; such a
//! sidecar means two different copies of one segment exist, so it is listed
//! apart and the replay refuses the day rather than pick one.
//!
//! # Complexity
//!
//! Cold, offline, never on the live path: the listing is O(keys) plus one
//! sort of the canonical keys (O(k log k)); each segment download is
//! O(bytes) to disk, and [`gunzip_to`] is O(bytes) with a fixed 64 KiB copy
//! buffer, refusing more than [`MAX_UNZIPPED_SEGMENT_BYTES`]. Every local
//! file it writes is created new ([`reserve_new_file`], [`gunzip_to`]), so it
//! never overwrites or removes a file it did not make.

// O(1) EXEMPT: begin: cold offline fetch for the replay CLI, never per tick.
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::raw_frame_upload::RAW_FRAME_S3_PREFIX;
use crate::s3_cold::S3Cold;

/// A gunzipped segment larger than this is refused (segments rotate at
/// 128 MiB, so this is eight times the largest honest one).
pub const MAX_UNZIPPED_SEGMENT_BYTES: u64 = 1024 * 1024 * 1024;

/// What one day's listing found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DayListing {
    /// Canonical segment keys and sizes, oldest name first: the previous
    /// date's newest segment (when there is one), then every one of the day.
    pub keys: Vec<(String, u64)>,
    /// Bytes the listed keys hold (what a download costs).
    pub total_bytes: u64,
    /// How many of the keys are segments of the day itself (not the
    /// previous date's): 0 means the day has nothing to replay, and the
    /// replay refuses it before any download.
    pub day_segments: usize,
    /// `.sha-<hex>.gz` sidecar keys of the day, and of the previous date's
    /// segment that is read: a second, different copy of a segment the
    /// replay reads. Non-empty means the day is refused.
    pub sidecars: Vec<String>,
}

/// The segment name inside a key (`ws-frames-<nanos>.wal`), or `None` for a
/// key that is not a canonical segment copy. Pure.
#[must_use]
pub fn segment_name_of_key(key: &str) -> Option<&str> {
    let name = key.rsplit('/').next()?.strip_suffix(".gz")?;
    crate::ws_frame_spill::segment_name_nanos(name).map(|_| name)
}

/// Whether `key` is a content-addressed sidecar (`<name>.sha-<hex>.gz`). Pure.
#[must_use]
pub fn is_sidecar_key(key: &str) -> bool {
    key.rsplit('/')
        .next()
        .and_then(|n| n.strip_suffix(".gz"))
        .is_some_and(|n| n.contains(".wal.sha-"))
}

/// The keys to fetch for a day from the day's listing and the previous
/// date's listing. Pure: O(keys) plus one sort.
#[must_use]
pub fn plan_day_listing(day_keys: &[(String, u64)], prev_day_keys: &[(String, u64)]) -> DayListing {
    let mut out = DayListing::default();
    let nanos =
        |k: &str| segment_name_of_key(k).and_then(crate::ws_frame_spill::segment_name_nanos);
    let prev = prev_day_keys
        .iter()
        .filter_map(|(k, s)| nanos(k).map(|n| (n, k, *s)))
        .max_by_key(|(n, _, _)| *n);
    let mut day: Vec<(u128, &String, u64)> = day_keys
        .iter()
        .filter_map(|(k, s)| {
            if is_sidecar_key(k) {
                None
            } else {
                nanos(k).map(|n| (n, k, *s))
            }
        })
        .collect();
    day.sort_by_key(|(n, _, _)| *n);
    out.day_segments = day.len();
    for (_, k, s) in prev.into_iter().chain(day) {
        out.total_bytes = out.total_bytes.saturating_add(s);
        out.keys.push((k.clone(), s));
    }
    out.sidecars = day_keys
        .iter()
        .filter(|(k, _)| is_sidecar_key(k))
        .map(|(k, _)| k.clone())
        .collect();
    // A second copy of the previous date's segment that is read refuses the
    // day too; sidecars of the previous date's other segments do not matter.
    if let Some(prev_name) = prev.and_then(|(_, k, _)| segment_name_of_key(k)) {
        out.sidecars.extend(
            prev_day_keys
                .iter()
                .filter(|(k, _)| sidecar_segment_name(k) == Some(prev_name))
                .map(|(k, _)| k.clone()),
        );
    }
    out.sidecars.sort();
    out
}

/// The segment a sidecar key is a second copy of (`ws-frames-<nanos>.wal`
/// from `<name>.sha-<hex>.gz`), or `None` for a key that is not a sidecar.
/// Pure.
#[must_use]
fn sidecar_segment_name(key: &str) -> Option<&str> {
    let name = key.rsplit('/').next()?.strip_suffix(".gz")?;
    let (seg, _hex) = name.split_once(".sha-")?;
    seg.ends_with(".wal").then_some(seg)
}

/// The `raw-frames/<date>/` prefix of an IST date, after checking the date
/// is a real `YYYY-MM-DD`. Pure.
///
/// # Errors
/// The text is not a calendar date.
pub fn day_prefix(date_ist: &str) -> Result<String> {
    let d = chrono::NaiveDate::parse_from_str(date_ist, "%Y-%m-%d")
        .with_context(|| format!("bad date {date_ist:?}, want YYYY-MM-DD"))?;
    Ok(format!("{RAW_FRAME_S3_PREFIX}/{}/", d.format("%Y-%m-%d")))
}

/// Lists one IST day of raw segments (and the previous date's newest one).
/// Two ListObjectsV2 walks, nothing else. O(keys).
///
/// # Errors
/// A bad date, or a listing failed.
pub async fn list_raw_frame_day(s3: &S3Cold, date_ist: &str) -> Result<DayListing> {
    let prefix = day_prefix(date_ist)?;
    let d = chrono::NaiveDate::parse_from_str(date_ist, "%Y-%m-%d")?;
    let prev = d
        .pred_opt()
        .context("no previous date")?
        .format("%Y-%m-%d")
        .to_string();
    let day_keys = s3.list_keys(&prefix).await?;
    let prev_keys = s3.list_keys(&day_prefix(&prev)?).await?;
    Ok(plan_day_listing(&day_keys, &prev_keys))
}

/// Downloads one segment copy into `staging` and unzips it there, removing
/// the `.gz` afterwards (a local file this call created, never one that was
/// already there). Returns the `.wal` path. One GetObject. O(bytes).
///
/// The `.gz` is first created with `create_new`, so a file of the same name
/// already in `staging` is refused, never overwritten or removed; the unzip
/// refuses an existing `.wal` the same way ([`gunzip_to`]).
///
/// # Errors
/// The key is not a segment copy, either file already exists, the download
/// failed, or the unzip did.
pub async fn fetch_raw_frame_segment(s3: &S3Cold, key: &str, staging: &Path) -> Result<PathBuf> {
    let Some(name) = segment_name_of_key(key) else {
        bail!("{key:?} is not a raw segment copy");
    };
    let gz = staging.join(format!("{name}.gz"));
    let wal = staging.join(name);
    if std::fs::symlink_metadata(&wal).is_ok() {
        bail!("{} already exists: refusing to overwrite it", wal.display());
    }
    reserve_new_file(&gz)?;
    if let Err(e) = s3.get_to_path(key, &gz).await {
        let _gone = std::fs::remove_file(&gz);
        return Err(e);
    }
    let unzipped = gunzip_to(&gz, &wal);
    std::fs::remove_file(&gz).with_context(|| format!("remove {}", gz.display()))?;
    unzipped?;
    Ok(wal)
}

/// Creates `path` as a new, empty file, refusing one that exists (so a later
/// write and remove only ever touch a file this process made). O(1).
///
/// # Errors
/// `path` exists, or it could not be created.
pub fn reserve_new_file(path: &Path) -> Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create {} (refused if it already exists)", path.display()))?;
    Ok(())
}

/// Unzips `src` into a NEW file `dst` (refused when it exists). O(bytes);
/// refuses more than [`MAX_UNZIPPED_SEGMENT_BYTES`]. Returns the bytes
/// written.
///
/// # Errors
/// An I/O error, a corrupt gzip stream, `dst` exists, or the output is too
/// large (the partial file is removed).
pub fn gunzip_to(src: &Path, dst: &Path) -> Result<u64> {
    let input = std::fs::File::open(src).with_context(|| format!("open {}", src.display()))?;
    let mut dec = flate2::read::MultiGzDecoder::new(std::io::BufReader::new(input));
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)
        .with_context(|| format!("create {}", dst.display()))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut written: u64 = 0;
    let result = loop {
        let n = match dec.read(&mut buf) {
            Ok(0) => break Ok(written),
            Ok(n) => n,
            Err(e) => break Err(anyhow::Error::new(e).context("gunzip")),
        };
        written = written.saturating_add(n as u64);
        if written > MAX_UNZIPPED_SEGMENT_BYTES {
            break Err(anyhow::anyhow!(
                "{} unzips past {MAX_UNZIPPED_SEGMENT_BYTES} bytes",
                src.display()
            ));
        }
        if let Err(e) = out.write_all(&buf[..n]) {
            break Err(anyhow::Error::new(e).context("write"));
        }
    };
    if result.is_err() {
        drop(out);
        let _gone = std::fs::remove_file(dst);
    }
    result
}

/// SHA-256 of a file as lower-case hex (the replay's output summary). O(bytes).
///
/// # Errors
/// The file could not be read.
pub fn file_sha256_hex(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}
// O(1) EXEMPT: end

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "tv-raw-fetch-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&d).expect("tmp");
        d
    }

    #[test]
    fn test_gunzip_to_round_trip() {
        let dir = tmp("gz");
        let body: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let gz = dir.join("a.wal.gz");
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(&body).expect("gz");
        std::fs::write(&gz, enc.finish().expect("gz")).expect("write");
        let out = dir.join("a.wal");
        assert_eq!(gunzip_to(&gz, &out).expect("unzip"), body.len() as u64);
        assert_eq!(std::fs::read(&out).expect("read"), body);
        assert!(gunzip_to(&gz, &out).is_err(), "never overwrites");
        std::fs::write(dir.join("bad.gz"), b"not gzip").expect("write");
        assert!(gunzip_to(&dir.join("bad.gz"), &dir.join("bad.wal")).is_err());
        assert!(
            !dir.join("bad.wal").exists(),
            "a failed unzip leaves nothing"
        );
        assert_eq!(
            file_sha256_hex(&out).expect("sha").len(),
            64,
            "file_sha256_hex is hex"
        );
        let _gone = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_reserve_new_file_never_takes_an_existing_file() {
        let dir = tmp("reserve");
        let p = dir.join("ws-frames-1.wal.gz");
        std::fs::write(&p, b"the operator's own copy").expect("write");
        assert!(reserve_new_file(&p).is_err(), "an existing file is refused");
        assert_eq!(
            std::fs::read(&p).expect("read"),
            b"the operator's own copy",
            "and left untouched"
        );
        let q = dir.join("new.gz");
        reserve_new_file(&q).expect("a new file is made");
        assert_eq!(std::fs::metadata(&q).expect("meta").len(), 0);
        let _gone = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_file_sha256_hex_known_vector() {
        let dir = tmp("sha");
        let p = dir.join("abc");
        std::fs::write(&p, b"abc").expect("write");
        assert_eq!(
            file_sha256_hex(&p).expect("sha"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _gone = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_list_flags_sidecars_and_totals_plan_day_listing() {
        let k = |d: &str, n: u64| format!("raw-frames/{d}/ws-frames-{n}.wal.gz");
        let day = vec![
            (k("2026-11-18", 30), 300),
            (k("2026-11-18", 10), 100),
            (
                "raw-frames/2026-11-18/ws-frames-10.wal.sha-abcd.gz".to_owned(),
                101,
            ),
            ("raw-frames/2026-11-18/notes.txt".to_owned(), 5),
        ];
        let prev = vec![(k("2026-11-17", 5), 50), (k("2026-11-17", 7), 70)];
        let l = plan_day_listing(&day, &prev);
        assert_eq!(
            l.keys,
            vec![
                (k("2026-11-17", 7), 70),
                (k("2026-11-18", 10), 100),
                (k("2026-11-18", 30), 300)
            ],
            "previous date's newest, then the day oldest first"
        );
        assert_eq!(l.total_bytes, 470);
        assert_eq!(
            l.day_segments, 2,
            "the previous date's segment is not the day's"
        );
        let only_prev = plan_day_listing(&[], &prev);
        assert_eq!(
            (only_prev.keys.len(), only_prev.day_segments),
            (1, 0),
            "a day not uploaded yet lists only the previous date's segment"
        );
        assert_eq!(
            l.sidecars,
            vec!["raw-frames/2026-11-18/ws-frames-10.wal.sha-abcd.gz".to_owned()]
        );
        assert!(plan_day_listing(&[], &[]).keys.is_empty());
    }

    /// A second copy of the previous date's segment that is read refuses the
    /// day, as the local folder refuses it; a sidecar of another previous
    /// segment (never read) does not.
    #[test]
    fn test_previous_day_sidecar_of_the_read_segment_refuses_plan_day_listing() {
        let k = |d: &str, n: u64| format!("raw-frames/{d}/ws-frames-{n}.wal.gz");
        let day = vec![(k("2026-11-18", 10), 100)];
        let prev = vec![
            (k("2026-11-17", 5), 50),
            (
                "raw-frames/2026-11-17/ws-frames-5.wal.sha-11.gz".to_owned(),
                51,
            ),
            (k("2026-11-17", 7), 70),
            (
                "raw-frames/2026-11-17/ws-frames-7.wal.sha-ab12.gz".to_owned(),
                71,
            ),
        ];
        let l = plan_day_listing(&day, &prev);
        assert_eq!(l.keys[0], (k("2026-11-17", 7), 70), "the newest is read");
        assert_eq!(
            l.sidecars,
            vec!["raw-frames/2026-11-17/ws-frames-7.wal.sha-ab12.gz".to_owned()],
            "only the read segment's second copy refuses the day"
        );
        assert_eq!(
            sidecar_segment_name("raw-frames/d/ws-frames-7.wal.sha-ab.gz"),
            Some("ws-frames-7.wal")
        );
        assert_eq!(
            sidecar_segment_name("raw-frames/d/ws-frames-7.wal.gz"),
            None
        );
    }

    #[test]
    fn test_day_prefix_segment_name_of_key_and_is_sidecar_key() {
        assert_eq!(
            day_prefix("2026-11-18").expect("date"),
            "raw-frames/2026-11-18/"
        );
        assert!(day_prefix("2026-13-01").is_err());
        assert!(day_prefix("x/../y").is_err());
        assert_eq!(
            segment_name_of_key("raw-frames/2026-11-18/ws-frames-12.wal.gz"),
            Some("ws-frames-12.wal")
        );
        assert_eq!(segment_name_of_key("raw-frames/2026-11-18/x.gz"), None);
        assert!(is_sidecar_key("raw-frames/d/ws-frames-1.wal.sha-ff.gz"));
        assert!(!is_sidecar_key("raw-frames/d/ws-frames-1.wal.gz"));
    }

    /// `list_raw_frame_day` and `fetch_raw_frame_segment` need a bucket; their
    /// pure halves are tested above. Without credentials the listing fails
    /// loudly instead of returning an empty day.
    #[test]
    fn test_list_raw_frame_day_and_fetch_raw_frame_segment_refuse_bad_input() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        rt.block_on(async {
            let conf = aws_sdk_s3::Config::builder()
                .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
                .region(aws_sdk_s3::config::Region::new("ap-south-1"))
                .build();
            let s3 = S3Cold::from_client(aws_sdk_s3::Client::from_conf(conf), "b".to_owned());
            assert!(list_raw_frame_day(&s3, "not-a-date").await.is_err());
            let dir = tmp("fetch");
            assert!(
                fetch_raw_frame_segment(&s3, "raw-frames/d/notes.txt", &dir)
                    .await
                    .is_err()
            );
            // A file already in staging under the segment's name is refused
            // before any download, and is neither overwritten nor removed.
            for existing in ["ws-frames-12.wal.gz", "ws-frames-13.wal"] {
                let p = dir.join(existing);
                std::fs::write(&p, b"keep me").expect("write");
            }
            for key in [
                "raw-frames/2026-11-18/ws-frames-12.wal.gz",
                "raw-frames/2026-11-18/ws-frames-13.wal.gz",
            ] {
                assert!(fetch_raw_frame_segment(&s3, key, &dir).await.is_err());
            }
            for existing in ["ws-frames-12.wal.gz", "ws-frames-13.wal"] {
                assert_eq!(
                    std::fs::read(dir.join(existing)).expect("still there"),
                    b"keep me",
                    "{existing} untouched"
                );
            }
            let _gone = std::fs::remove_dir_all(&dir);
        });
    }
}

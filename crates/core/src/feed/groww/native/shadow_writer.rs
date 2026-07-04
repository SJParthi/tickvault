//! Shadow NDJSON writer — the native client's capture file, in the EXACT
//! `GrowwTickLine` schema the bridge parses (`crates/app/src/groww_bridge.rs`)
//! and the Python sidecar writes (`scripts/groww-sidecar/groww_sidecar.py::
//! _write_record`), so the PR-R2 parity comparer is a trivial keyed diff:
//!
//! `{"security_id": i64, "segment": str, "ts_ist_nanos": i64,
//!   "exchange_ts_millis": i64, "ltp": f64, "cum_volume": 0,
//!   "capture_ns": i64}` — one line per tick.
//!
//! Rotation mirrors the sidecar (writer-owns-rotation, 2026-07-02 PR-3
//! semantics): at the IST day boundary the current file renames to
//! `rust-live-ticks-YYYYMMDD.ndjson` (the COMPLETED day) and a fresh file
//! opens; a rotation failure NEVER stops capture (retry next midnight).
//!
//! The writer runs as a DEDICATED task behind a bounded mpsc so the read loop
//! never does file I/O; the serialization buffer is reused across lines
//! (zero steady-state allocation).

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use chrono::{Duration, TimeZone, Utc};
use tickvault_common::error_code::ErrorCode;
use tokio::sync::mpsc;
use tracing::{error, info};

/// IST offset in seconds (UTC+5:30) — same constant convention as the
/// sidecar's `ms_to_ist_nanos`.
const IST_OFFSET_SECS: i64 = 19_800;

/// One shadow tick — `Copy`, assembled on the read loop, serialized here.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShadowTick {
    /// The `security_id` from the watch-file subject map.
    pub security_id: i64,
    /// Canonical segment string (static — no per-tick allocation).
    pub segment: &'static str,
    /// Groww `tsInMillis` (UTC epoch ms — the millisecond advantage).
    pub exchange_ts_millis: i64,
    /// Last traded price / index value.
    pub ltp: f64,
    /// Capture-at-receipt instant (UTC epoch nanos), stamped in the read loop
    /// the moment the MSG decoded — the native analogue of the sidecar's
    /// per-callback `capture_ns`.
    pub capture_ns: i64,
}

/// Groww `tsInMillis` (UTC epoch ms) → IST epoch nanoseconds — EXACTLY the
/// sidecar's `ms_to_ist_nanos` (UTC + 5:30 as wall-clock nanos, ms precision
/// preserved). Pure + saturating (a garbage future ms can never panic).
#[must_use]
pub const fn ms_to_ist_nanos(ts_millis: i64) -> i64 {
    (ts_millis.saturating_add(IST_OFFSET_SECS * 1_000)).saturating_mul(1_000_000)
}

/// The IST day ordinal (days since epoch, IST wall clock) for a UTC epoch-ms
/// timestamp — the writer's one-integer-compare rotation check.
#[must_use]
pub const fn ist_day_ordinal_from_millis(utc_millis: i64) -> i64 {
    (utc_millis / 1_000 + IST_OFFSET_SECS).div_euclid(86_400)
}

/// Archive path for a completed IST day: `rust-live-ticks.ndjson` →
/// `rust-live-ticks-YYYYMMDD.ndjson` (sibling of the live file — mirrors the
/// sidecar's `live-ticks-YYYYMMDD.ndjson` naming). Pure.
#[must_use]
pub fn archive_path_for_day(live_path: &Path, ist_day_ordinal: i64) -> PathBuf {
    // Midday of the ordinal day (UTC secs) avoids any boundary ambiguity.
    let utc_secs = ist_day_ordinal * 86_400 - IST_OFFSET_SECS + 43_200;
    let yyyymmdd = match Utc.timestamp_opt(utc_secs, 0) {
        chrono::LocalResult::Single(dt) => (dt + Duration::seconds(IST_OFFSET_SECS))
            .format("%Y%m%d")
            .to_string(),
        // Unreachable for any real ordinal; degrade to the ordinal itself so
        // the archive name stays unique rather than panicking.
        _ => format!("day{ist_day_ordinal}"),
    };
    let stem = live_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("rust-live-ticks");
    let mut name = String::with_capacity(stem.len() + 1 + 8 + 7);
    name.push_str(stem);
    name.push('-');
    name.push_str(&yyyymmdd);
    name.push_str(".ndjson");
    live_path.with_file_name(name)
}

/// Serialize one tick as a `GrowwTickLine` NDJSON line into `buf`
/// (buffer reused across calls — zero steady-state allocation). Field ORDER
/// matches the sidecar's writer for byte-diff friendliness; the comparer keys
/// on values, not order.
pub fn format_ndjson_line(buf: &mut String, tick: &ShadowTick) {
    buf.clear();
    // `segment` is a fixed &'static str from our own table (no quoting needed)
    // and every other field is numeric — hand-formatting is JSON-safe here and
    // keeps the per-tick path allocation-free.
    if writeln!(
        buf,
        "{{\"security_id\": {}, \"segment\": \"{}\", \"ts_ist_nanos\": {}, \
         \"exchange_ts_millis\": {}, \"ltp\": {}, \"cum_volume\": 0, \"capture_ns\": {}}}",
        tick.security_id,
        tick.segment,
        ms_to_ist_nanos(tick.exchange_ts_millis),
        tick.exchange_ts_millis,
        JsonF64(tick.ltp),
        tick.capture_ns,
    )
    .is_err()
    {
        // Writing into a String is infallible; clear defensively so a partial
        // line can never reach the file.
        buf.clear();
    }
}

/// f64 formatter that always emits VALID JSON numbers: NaN/±Inf (impossible
/// from the decoder, defensive) degrade to 0, and integral values keep a
/// trailing `.0` so the field parses as f64 everywhere.
struct JsonF64(f64);

impl core::fmt::Display for JsonF64 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let v = if self.0.is_finite() { self.0 } else { 0.0 };
        if v == v.trunc() && v.abs() < 1e15 {
            write!(f, "{v:.1}")
        } else {
            write!(f, "{v}")
        }
    }
}

/// Run the shadow NDJSON writer task: drain the bounded channel, append lines,
/// rotate at the IST day boundary. Returns when the channel closes (client
/// shut down). Write failures are LOUD (`GROWW-NATIVE-04`) and retried on the
/// next line — capture never stops for an I/O error.
/// Every pure primitive this composes (format_ndjson_line / ms_to_ist_nanos /
/// ist_day_ordinal_from_millis / archive_path_for_day) is unit-tested below;
/// the failure arms are typed + counted rather than branch-tested against a
/// real failing filesystem.
// TEST-EXEMPT: file-I/O drain loop over unit-tested pure primitives.
pub async fn run_shadow_writer(mut rx: mpsc::Receiver<ShadowTick>, live_path: PathBuf) {
    let mut line_buf = String::with_capacity(256);
    let mut file: Option<std::fs::File> = None;
    let mut current_day: Option<i64> = None;
    let mut write_error_logged = false;

    while let Some(tick) = rx.recv().await {
        // Rotation check: one integer compare per tick (capture-clock day —
        // the file boundary is the WRITER's wall clock, like the sidecar).
        let day = ist_day_ordinal_from_millis(tick.capture_ns / 1_000_000);
        if current_day.is_some_and(|d| day > d) {
            // Flush + close + archive the COMPLETED day, then reopen fresh.
            file = None;
            if let Some(completed) = current_day {
                let archive = archive_path_for_day(&live_path, completed);
                match std::fs::rename(&live_path, &archive) {
                    Ok(()) => info!(
                        archive = %archive.display(),
                        "groww native shadow capture rotated (completed IST day)"
                    ),
                    Err(e) => error!(
                        code = ErrorCode::GrowwNative04WriterFailed.code_str(),
                        error = %e,
                        "GROWW-NATIVE-04: shadow capture rotation failed — \
                         continuing capture on the current file; retry next midnight"
                    ),
                }
            }
        }
        if current_day != Some(day) {
            current_day = Some(day);
        }

        if file.is_none() {
            file = match open_append(&live_path) {
                Ok(f) => {
                    write_error_logged = false;
                    Some(f)
                }
                Err(e) => {
                    if !write_error_logged {
                        error!(
                            code = ErrorCode::GrowwNative04WriterFailed.code_str(),
                            error = %e,
                            path = %live_path.display(),
                            "GROWW-NATIVE-04: shadow capture file open failed — \
                             dropping shadow lines until the disk recovers"
                        );
                        write_error_logged = true;
                    }
                    metrics::counter!("tv_groww_native_writer_errors_total", "stage" => "open")
                        .increment(1);
                    continue;
                }
            };
        }

        format_ndjson_line(&mut line_buf, &tick);
        if let Some(f) = file.as_mut()
            && let Err(e) = f.write_all(line_buf.as_bytes())
        {
            if !write_error_logged {
                error!(
                    code = ErrorCode::GrowwNative04WriterFailed.code_str(),
                    error = %e,
                    "GROWW-NATIVE-04: shadow capture write failed — \
                     reopening on the next line; capture continues"
                );
                write_error_logged = true;
            }
            metrics::counter!("tv_groww_native_writer_errors_total", "stage" => "write")
                .increment(1);
            file = None; // reopen on the next tick
            continue;
        }
        write_error_logged = false;
        metrics::counter!("tv_groww_native_ticks_captured_total").increment(1);
    }
    info!("groww native shadow writer: channel closed, exiting");
}

/// Open the live capture file in append mode (creating parents).
fn open_append(path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// UTC ms → IST nanos, mirroring the sidecar's `ms_to_ist_nanos`.
    /// 2026-07-03 09:15:00.123 UTC = 14:45:00.123 IST wall clock.
    #[test]
    fn test_ms_to_ist_nanos() {
        // 1970-01-01 00:00:00.000 UTC → 05:30 IST.
        assert_eq!(ms_to_ist_nanos(0), 19_800_000_000_000);
        // ms precision preserved.
        assert_eq!(ms_to_ist_nanos(1), 19_800_001_000_000);
        // A real market ms: 2026-07-03T04:22:33.456Z (09:52:33.456 IST).
        let ms = 1_783_052_553_456i64;
        assert_eq!(ms_to_ist_nanos(ms), (ms + 19_800_000) * 1_000_000);
    }

    /// The rotation day advances exactly at IST midnight (18:30 UTC).
    #[test]
    fn test_ist_day_ordinal_from_millis_boundary() {
        // 2026-07-03 18:29:59.999 UTC = 23:59:59.999 IST (day D)
        let before = 1_783_103_399_999i64;
        // 2026-07-03 18:30:00.000 UTC = 2026-07-04 00:00:00.000 IST (day D+1)
        let after = 1_783_103_400_000i64;
        assert_eq!(
            ist_day_ordinal_from_millis(after) - ist_day_ordinal_from_millis(before),
            1
        );
    }

    /// Archive naming mirrors the sidecar: `<stem>-YYYYMMDD.ndjson`.
    #[test]
    fn test_archive_path_for_day_naming() {
        // Ordinal of the IST day 2026-07-03.
        let ms = 1_783_078_400_000i64; // 2026-07-03T11:33:20Z = 17:03:20 IST
        let ordinal = ist_day_ordinal_from_millis(ms);
        let archive = archive_path_for_day(Path::new("data/groww/rust-live-ticks.ndjson"), ordinal);
        assert_eq!(
            archive,
            PathBuf::from("data/groww/rust-live-ticks-20260703.ndjson")
        );
    }

    /// The NDJSON line matches the Python sidecar's `GrowwTickLine` schema —
    /// field-for-field against a checked-in literal sample of the sidecar's
    /// output format (same keys, same types, `cum_volume` pinned 0).
    #[test]
    fn test_format_ndjson_line_matches_python_sidecar_schema() {
        // Literal line shape produced by scripts/groww-sidecar/groww_sidecar.py
        // `_write_record` (json.dumps of the rec dict).
        let python_sample = r#"{"security_id": 2885, "segment": "NSE_EQ", "ts_ist_nanos": 1782813153456000000, "exchange_ts_millis": 1782793353456, "ltp": 2954.5, "cum_volume": 0, "capture_ns": 1782793353500123456}"#;
        let python: serde_json::Value = serde_json::from_str(python_sample).expect("sample parses");

        let tick = ShadowTick {
            security_id: 2885,
            segment: "NSE_EQ",
            exchange_ts_millis: 1_782_793_353_456,
            ltp: 2954.5,
            capture_ns: 1_782_793_353_500_123_456,
        };
        let mut buf = String::new();
        format_ndjson_line(&mut buf, &tick);
        assert!(buf.ends_with('\n'), "one newline-terminated line per tick");
        let ours: serde_json::Value =
            serde_json::from_str(buf.trim_end()).expect("our line is valid JSON");

        // Same key set.
        let py_keys: std::collections::BTreeSet<_> =
            python.as_object().expect("obj").keys().collect();
        let our_keys: std::collections::BTreeSet<_> =
            ours.as_object().expect("obj").keys().collect();
        assert_eq!(py_keys, our_keys, "key sets must match the bridge schema");
        // Same values for this tick.
        assert_eq!(ours, python);
    }

    /// Integral LTPs keep a `.0` (valid f64 JSON) and non-finite degrades to 0.
    #[test]
    fn test_format_ndjson_line_ltp_boundaries() {
        let mut buf = String::new();
        let mut tick = ShadowTick {
            security_id: 13,
            segment: "IDX_I",
            exchange_ts_millis: 1_782_793_353_000,
            ltp: 25000.0,
            capture_ns: 1,
        };
        format_ndjson_line(&mut buf, &tick);
        assert!(buf.contains("\"ltp\": 25000.0,"), "line: {buf}");

        tick.ltp = f64::NAN;
        format_ndjson_line(&mut buf, &tick);
        let v: serde_json::Value = serde_json::from_str(buf.trim_end()).expect("valid json");
        assert_eq!(v["ltp"], 0.0);

        tick.ltp = 2954.55;
        format_ndjson_line(&mut buf, &tick);
        let v: serde_json::Value = serde_json::from_str(buf.trim_end()).expect("valid json");
        assert!((v["ltp"].as_f64().expect("f64") - 2954.55).abs() < 1e-9);
    }
}

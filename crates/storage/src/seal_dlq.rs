//! Wave 6 Sub-PR #1 item 1.2d — sealed-candle NDJSON dead-letter queue.
//!
//! Third absorption tier of the locked Wave 6 design L-C1:
//! `SealRing` (in-memory FIFO) → `SealSpillWriter` (binary 128-byte
//! disk records) → `SealDlqWriter` (this module — NDJSON last-resort).
//!
//! ## Why NDJSON for the third tier
//!
//! - **Recoverable text**. The binary spill file is exact + compact
//!   but operator-opaque. Once we are routing seals to the DLQ the
//!   normal pipeline is already broken; the operator needs to be
//!   able to `cat data/dlq/seals-YYYY-MM-DD.ndjson | jq` to inspect
//!   what was lost. NDJSON is the canonical text-streaming format
//!   matching the existing `data/logs/errors.jsonl.*` rotation.
//! - **Append-only single-line records.** A partial trailing line on
//!   crash is silently dropped during replay (the `serde_json::from_str`
//!   on the corrupt line returns `Err` and the reader skips it).
//! - **Forward-compatible.** Adding a new field to `SealDlqRecord`
//!   produces a JSON object with one extra key; older replay code
//!   that doesn't know about it ignores via `#[serde(default)]`.
//!
//! ## What this module ships
//!
//! - [`SealDlqRecord`] — serde-serialisable mirror of
//!   `SerializedSeal` (the binary wire-format used by the spill tier).
//!   Lossless `From<&SerializedSeal>` + `From<&SealDlqRecord> for
//!   SerializedSeal` so the writer task can convert from any tier
//!   without re-deriving the trading-side `BufferedSeal`.
//! - [`SealDlqWriter`] — append-only NDJSON file writer with the
//!   exact `seal_spill.rs` API surface:
//!   - IST-date file rotation (`seals-2026-05-10.ndjson`).
//!   - `append_record()` (one line per call).
//!   - `read_all()` recovery scan that silently drops corrupt
//!     lines with `warn!` so a single bad line does NOT stall replay.
//!   - `clear_dlq_for_date()` idempotent removal.
//!   - `with_dlq_dir_for_test()` for parallel test isolation.
//!
//! ## When this fires
//!
//! Per locked decision L-C1, the writer task escalates to this DLQ
//! ONLY when the in-memory ring AND the binary spill BOTH fail. If
//! the DLQ append also fails the writer task logs
//! `error!(code = ErrorCode::AggregatorDrop01.code_str())` per the
//! AGGREGATOR-DROP-01 runbook in
//! `.claude/rules/project/wave-6-error-codes.md`. Triple-tier failure
//! = operator-action-required; the host is OOM AND out of disk AND
//! the `data/dlq/` directory is unwritable.

use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;
use tickvault_common::feed::Feed;

use crate::seal_spill::SerializedSeal;

/// Production DLQ directory — sibling of `data/spill/` so operators
/// looking at `data/` see all three absorption tiers next to each
/// other (`logs/`, `spill/`, `dlq/`).
const SEAL_DLQ_DIR: &str = "data/dlq";

/// JSON-serialisable mirror of [`SerializedSeal`]. Field names are
/// stable wire format for `jq` operability — every new field MUST
/// be added with `#[serde(default)]` so older replay code that
/// doesn't know about it skips it without error.
///
/// Field-by-field correspondence with `SerializedSeal`:
/// - `security_id`, `exchange_segment_code` — composite key (I-P1-11).
/// - `tf_ordinal` — `TfIndex::as_ordinal()` (0..=20).
/// - `bucket_start_ist_secs`, `tick_count`, `volume`,
///   `bucket_start_cumulative`, `oi`, `open`, `high`, `low`, `close`
///   — `LiveCandleState` payload.
/// - `close_pct_from_prev_day`, `oi_pct_from_prev_day`,
///   `volume_pct_from_prev_day` — Wave-5 stamped-at-seal pct fields
///   (per locked decision L-H6).
// `Copy` dropped 2026-06-23: the `feed: String` field (round-trips broker
// provenance through the DLQ NDJSON) makes the record non-`Copy`. The DLQ path
// is the cold last-resort tier (NDJSON serialize/deserialize), never the hot
// path, so a `Clone`-only record is fine — every conversion uses `&self`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SealDlqRecord {
    // `u64` (2026-06-29 widening) — the seal DLQ carries BOTH Dhan (≤u32) and
    // Groww (bit-62 index ids > u32) seals; NDJSON round-trips u64 natively.
    #[serde(default)]
    pub security_id: u64,
    #[serde(default)]
    pub exchange_segment_code: u8,
    #[serde(default)]
    pub tf_ordinal: u8,
    #[serde(default)]
    pub bucket_start_ist_secs: u32,
    #[serde(default)]
    pub tick_count: u32,
    #[serde(default)]
    pub volume: u64,
    #[serde(default)]
    pub bucket_start_cumulative: u64,
    #[serde(default)]
    pub oi: i64,
    #[serde(default)]
    pub open: f64,
    #[serde(default)]
    pub high: f64,
    #[serde(default)]
    pub low: f64,
    #[serde(default)]
    pub close: f64,
    #[serde(default)]
    pub close_pct_from_prev_day: f64,
    #[serde(default)]
    pub oi_pct_from_prev_day: f64,
    #[serde(default)]
    pub volume_pct_from_prev_day: f64,
    /// §31 Option 2 (2026-06-01): % vs the official 09:15 session open.
    #[serde(default)]
    pub open_pct: f64,
    /// Operator request 2026-06-02: headline day change % (close vs
    /// yesterday's close).
    #[serde(default)]
    pub change_pct: f64,
    /// Operator request 2026-06-02: opening gap % (today's 09:15 open vs
    /// yesterday's close).
    #[serde(default)]
    pub open_gap_pct: f64,
    /// Broker-source feed label (`"dhan"` / `"groww"`). `#[serde(default)]` ⇒
    /// pre-feed DLQ NDJSON records (no `feed` key) deserialise to `""`, which
    /// the `→ SerializedSeal` conversion maps to `Feed::Dhan` — backward-compatible.
    /// Round-trips the feed through the DLQ so a Groww seal recovered from the
    /// last-resort NDJSON still writes `feed='groww'`.
    #[serde(default)]
    pub feed: String,
}

impl From<&SerializedSeal> for SealDlqRecord {
    /// Lossless conversion from the binary spill record to the
    /// JSON-serialisable DLQ record. `O(1)`, zero allocation
    /// (`Copy` of fixed-size primitive fields).
    #[inline]
    fn from(s: &SerializedSeal) -> Self {
        Self {
            security_id: s.security_id,
            exchange_segment_code: s.exchange_segment_code,
            // Round-trip feed provenance through the DLQ NDJSON.
            feed: s.feed.as_str().to_string(),
            tf_ordinal: s.tf_ordinal,
            bucket_start_ist_secs: s.bucket_start_ist_secs,
            tick_count: s.tick_count,
            volume: s.volume,
            bucket_start_cumulative: s.bucket_start_cumulative,
            oi: s.oi,
            open: s.open,
            high: s.high,
            low: s.low,
            close: s.close,
            close_pct_from_prev_day: s.close_pct_from_prev_day,
            oi_pct_from_prev_day: s.oi_pct_from_prev_day,
            volume_pct_from_prev_day: s.volume_pct_from_prev_day,
            open_pct: s.open_pct,
            change_pct: s.change_pct,
            open_gap_pct: s.open_gap_pct,
        }
    }
}

impl From<&SealDlqRecord> for SerializedSeal {
    /// Reverse direction — used by the writer task on REPLAY to
    /// re-attempt the ILP send via the existing binary path. Lossless
    /// (every field copied 1:1).
    #[inline]
    fn from(r: &SealDlqRecord) -> Self {
        Self {
            security_id: r.security_id,
            exchange_segment_code: r.exchange_segment_code,
            // Pre-feed DLQ records have feed="" → Feed::Dhan (backward-compatible);
            // an unknown label also degrades to Dhan rather than panicking.
            feed: Feed::parse(&r.feed).unwrap_or(Feed::Dhan),
            tf_ordinal: r.tf_ordinal,
            bucket_start_ist_secs: r.bucket_start_ist_secs,
            tick_count: r.tick_count,
            volume: r.volume,
            bucket_start_cumulative: r.bucket_start_cumulative,
            oi: r.oi,
            open: r.open,
            high: r.high,
            low: r.low,
            close: r.close,
            close_pct_from_prev_day: r.close_pct_from_prev_day,
            oi_pct_from_prev_day: r.oi_pct_from_prev_day,
            volume_pct_from_prev_day: r.volume_pct_from_prev_day,
            open_pct: r.open_pct,
            change_pct: r.change_pct,
            open_gap_pct: r.open_gap_pct,
        }
    }
}

/// Returns today's IST date in `seals-YYYY-MM-DD.ndjson` form for the
/// DLQ filename. Pure function for testability (clock injected by
/// caller in tests). Mirrors `seal_spill::ist_date_filename` but with
/// the `.ndjson` suffix.
fn ist_date_filename(now_unix_secs: i64) -> String {
    // `IST_UTC_OFFSET_SECONDS` per data-integrity.md — 19_800.
    let ist_secs = now_unix_secs.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
    let dt = Utc
        .timestamp_opt(ist_secs, 0)
        .single()
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).single().unwrap_or_default());
    dt.format("seals-%Y-%m-%d.ndjson").to_string()
}

/// Append-only NDJSON DLQ writer. One instance lives in the writer
/// task; `append_record` is the single producer entry point.
pub struct SealDlqWriter {
    /// DLQ directory — production uses `SEAL_DLQ_DIR`; tests
    /// override via `with_dlq_dir_for_test`.
    dlq_dir: PathBuf,
}

impl SealDlqWriter {
    /// Production constructor. Uses `data/dlq/`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            dlq_dir: PathBuf::from(SEAL_DLQ_DIR),
        }
    }

    /// Test constructor. Tests pass an isolated `tempdir` to allow
    /// parallel execution.
    #[must_use]
    // TEST-EXEMPT: test-only helper used as construction source by every test in this module (test_append_record_then_read_all_roundtrip, test_seal_dlq_writer_clear_*, test_seal_dlq_writer_skips_corrupt_lines_gracefully, etc.). Separate name-matched test would be redundant.
    pub fn with_dlq_dir_for_test(dir: PathBuf) -> Self {
        Self { dlq_dir: dir }
    }

    /// Returns the path of the DLQ file for the given UTC unix
    /// timestamp (used to derive IST date). Pure helper.
    #[must_use]
    pub fn dlq_path(&self, now_unix_secs: i64) -> PathBuf {
        self.dlq_dir.join(ist_date_filename(now_unix_secs))
    }

    /// Append one record to the daily DLQ file as a single NDJSON
    /// line (`{...}\n`). Creates the DLQ directory + file if needed.
    /// O(1) per call; uses `BufWriter` to coalesce small writes.
    ///
    /// Per locked decision L-C1, this is the THIRD tier of the
    /// ring → spill → DLQ chain. If THIS append also fails, the
    /// writer task escalates to
    /// `error!(code = ErrorCode::AggregatorDrop01.code_str())` per
    /// the AGGREGATOR-DROP-01 runbook.
    pub fn append_record(&self, record: &SealDlqRecord, now_unix_secs: i64) -> Result<()> {
        std::fs::create_dir_all(&self.dlq_dir)
            .with_context(|| format!("failed to create dlq dir {:?}", self.dlq_dir))?;
        let path = self.dlq_path(now_unix_secs);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open dlq file {path:?}"))?;
        let mut writer = BufWriter::new(file);
        let line = serde_json::to_string(record)
            .with_context(|| "failed to serialise SealDlqRecord to JSON")?;
        writer
            .write_all(line.as_bytes())
            .with_context(|| format!("failed to write dlq line to {path:?}"))?;
        writer
            .write_all(b"\n")
            .with_context(|| format!("failed to write dlq newline to {path:?}"))?;
        writer
            .flush()
            .with_context(|| format!("failed to flush dlq line to {path:?}"))?;
        Ok(())
    }

    /// Drains the daily DLQ file by reading every line into the
    /// returned `Vec`. Lines that fail to parse (corrupt / partial /
    /// truncated tail) are silently dropped with a `warn!` so a
    /// single bad line does NOT stall replay.
    ///
    /// After successful read the caller (writer task) deletes the
    /// DLQ file via [`Self::clear_dlq_for_date`].
    ///
    /// Returns an empty `Vec` if the DLQ file does not exist
    /// (the happy path on a fresh boot).
    pub fn read_all(&self, now_unix_secs: i64) -> Result<Vec<SealDlqRecord>> {
        let path = self.dlq_path(now_unix_secs);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = std::fs::File::open(&path)
            .with_context(|| format!("failed to open dlq file {path:?}"))?;
        let reader = BufReader::new(file);
        let mut all = Vec::new();
        for (line_no, line_result) in reader.lines().enumerate() {
            let line = match line_result {
                Ok(l) => l,
                Err(err) => {
                    warn!(
                        ?path,
                        line_no,
                        ?err,
                        "dlq line read failed — dropping and continuing"
                    );
                    continue;
                }
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                // Blank line — likely a trailing newline after the
                // last record. Skip silently (no warn).
                continue;
            }
            match serde_json::from_str::<SealDlqRecord>(trimmed) {
                Ok(rec) => all.push(rec),
                Err(err) => {
                    warn!(
                        ?path,
                        line_no,
                        ?err,
                        "dlq line failed to parse — dropping and continuing"
                    );
                }
            }
        }
        info!(?path, count = all.len(), "drained dlq file");
        Ok(all)
    }

    /// Removes the DLQ file for the given date. Called by the
    /// writer task after `read_all` is fully replayed via the
    /// binary path. Idempotent: missing file returns Ok.
    pub fn clear_dlq_for_date(&self, now_unix_secs: i64) -> Result<()> {
        let path = self.dlq_path(now_unix_secs);
        if !path.exists() {
            return Ok(());
        }
        std::fs::remove_file(&path)
            .with_context(|| format!("failed to remove dlq file {path:?}"))?;
        info!(?path, "dlq file cleared after successful drain");
        Ok(())
    }
}

impl Default for SealDlqWriter {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn mk_serialized_seal(sid: u32, seg: u8, tf: u8, bucket: u32, close: f64) -> SerializedSeal {
        SerializedSeal {
            security_id: sid,
            exchange_segment_code: seg,
            feed: Feed::Dhan,
            tf_ordinal: tf,
            bucket_start_ist_secs: bucket,
            tick_count: 5,
            volume: 1234,
            bucket_start_cumulative: 1000,
            oi: 50_000,
            open: 100.0,
            high: 105.0,
            low: 99.0,
            close,
            close_pct_from_prev_day: 1.5,
            oi_pct_from_prev_day: -0.2,
            volume_pct_from_prev_day: 12.3,
            open_pct: 7.7,
            change_pct: 1.5,
            open_gap_pct: 0.8,
        }
    }

    fn temp_dlq_dir(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "tickvault-seal-dlq-test-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("test temp dir");
        dir
    }

    #[test]
    fn test_seal_dlq_record_default_is_zeroed() {
        let r = SealDlqRecord::default();
        assert_eq!(r.security_id, 0);
        assert_eq!(r.exchange_segment_code, 0);
        assert_eq!(r.tf_ordinal, 0);
        assert_eq!(r.bucket_start_ist_secs, 0);
        assert_eq!(r.tick_count, 0);
        assert_eq!(r.volume, 0);
        assert_eq!(r.bucket_start_cumulative, 0);
        assert_eq!(r.oi, 0);
        assert_eq!(r.open, 0.0);
        assert_eq!(r.high, 0.0);
        assert_eq!(r.low, 0.0);
        assert_eq!(r.close, 0.0);
        assert_eq!(r.close_pct_from_prev_day, 0.0);
        assert_eq!(r.oi_pct_from_prev_day, 0.0);
        assert_eq!(r.volume_pct_from_prev_day, 0.0);
    }

    #[test]
    fn test_seal_dlq_record_serde_roundtrip_preserves_every_field() {
        let original = SealDlqRecord::from(&mk_serialized_seal(13, 0, 0, 1_716_000_900, 102.5));
        let json = serde_json::to_string(&original).expect("serialise");
        let decoded: SealDlqRecord = serde_json::from_str(&json).expect("parse");
        assert_eq!(decoded, original);
    }

    #[test]
    fn test_seal_dlq_record_json_field_names_are_stable_for_jq() {
        // Operator demand: `cat data/dlq/seals-*.ndjson | jq` must
        // work. Pin the exact JSON keys so a future serde rename
        // does not silently break operator tooling.
        let r = SealDlqRecord::from(&mk_serialized_seal(13, 0, 0, 1_716_000_900, 102.5));
        let json = serde_json::to_string(&r).expect("serialise");
        for field in [
            "security_id",
            "exchange_segment_code",
            "tf_ordinal",
            "bucket_start_ist_secs",
            "tick_count",
            "volume",
            "bucket_start_cumulative",
            "oi",
            "open",
            "high",
            "low",
            "close",
            "close_pct_from_prev_day",
            "oi_pct_from_prev_day",
            "volume_pct_from_prev_day",
            "open_pct",
            "change_pct",
            "open_gap_pct",
        ] {
            assert!(
                json.contains(&format!("\"{field}\"")),
                "JSON missing expected key {field}: {json}"
            );
        }
    }

    #[test]
    fn test_from_serialized_seal_to_dlq_record_lossless() {
        let s = mk_serialized_seal(13, 0, 0, 1_716_000_900, 102.5);
        let r = SealDlqRecord::from(&s);
        assert_eq!(r.security_id, s.security_id);
        assert_eq!(r.exchange_segment_code, s.exchange_segment_code);
        assert_eq!(r.tf_ordinal, s.tf_ordinal);
        assert_eq!(r.bucket_start_ist_secs, s.bucket_start_ist_secs);
        assert_eq!(r.tick_count, s.tick_count);
        assert_eq!(r.volume, s.volume);
        assert_eq!(r.bucket_start_cumulative, s.bucket_start_cumulative);
        assert_eq!(r.oi, s.oi);
        assert_eq!(r.open, s.open);
        assert_eq!(r.high, s.high);
        assert_eq!(r.low, s.low);
        assert_eq!(r.close, s.close);
        assert_eq!(r.close_pct_from_prev_day, s.close_pct_from_prev_day);
        assert_eq!(r.oi_pct_from_prev_day, s.oi_pct_from_prev_day);
        assert_eq!(r.volume_pct_from_prev_day, s.volume_pct_from_prev_day);
    }

    #[test]
    fn test_from_dlq_record_to_serialized_seal_lossless() {
        let original = mk_serialized_seal(25, 1, 4, 1_716_001_500, 200.75);
        let r = SealDlqRecord::from(&original);
        let recovered = SerializedSeal::from(&r);
        assert_eq!(recovered, original);
    }

    #[test]
    fn test_dlq_record_handles_negative_oi_and_pct() {
        // i64 OI can be negative for short positions; pct fields can
        // be negative on red days — JSON round-trip MUST preserve.
        let s = SerializedSeal {
            security_id: 25,
            exchange_segment_code: 1,
            feed: Feed::Dhan,
            tf_ordinal: 4,
            bucket_start_ist_secs: 1_716_001_500,
            tick_count: 0,
            volume: 0,
            bucket_start_cumulative: 0,
            oi: -42_000,
            open: 0.0,
            high: 0.0,
            low: 0.0,
            close: 0.0,
            close_pct_from_prev_day: -3.5,
            oi_pct_from_prev_day: -10.0,
            volume_pct_from_prev_day: -100.0,
            open_pct: -50.0,
            change_pct: -3.5,
            open_gap_pct: -1.2,
        };
        let r = SealDlqRecord::from(&s);
        let json = serde_json::to_string(&r).expect("serialise");
        let decoded: SealDlqRecord = serde_json::from_str(&json).expect("parse");
        let recovered = SerializedSeal::from(&decoded);
        assert_eq!(recovered, s);
    }

    #[test]
    fn test_ist_date_filename_uses_ndjson_suffix() {
        let utc_noon = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp();
        let name = ist_date_filename(utc_noon);
        assert_eq!(name, "seals-2026-01-01.ndjson");
        assert!(name.ends_with(".ndjson"));
    }

    #[test]
    fn test_ist_date_filename_crosses_to_next_day_at_ist_midnight() {
        // 2026-05-09 18:30:00 UTC = 2026-05-10 00:00:00 IST.
        let utc = chrono::Utc
            .with_ymd_and_hms(2026, 5, 9, 18, 30, 0)
            .single()
            .expect("valid")
            .timestamp();
        let name = ist_date_filename(utc);
        assert_eq!(name, "seals-2026-05-10.ndjson");
    }

    #[test]
    fn test_seal_dlq_writer_new_uses_production_dir() {
        let writer = SealDlqWriter::new();
        assert_eq!(writer.dlq_dir, PathBuf::from(SEAL_DLQ_DIR));
    }

    #[test]
    fn test_seal_dlq_writer_default_matches_new() {
        let a = SealDlqWriter::default();
        let b = SealDlqWriter::new();
        assert_eq!(a.dlq_dir, b.dlq_dir);
    }

    #[test]
    fn test_seal_dlq_writer_dlq_path_uses_ist_date_in_filename() {
        let dir = temp_dlq_dir("path-ist-date");
        let writer = SealDlqWriter::with_dlq_dir_for_test(dir.clone());
        let utc_noon = chrono::Utc
            .with_ymd_and_hms(2026, 5, 10, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp();
        let p = writer.dlq_path(utc_noon);
        assert!(p.to_string_lossy().ends_with("seals-2026-05-10.ndjson"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_append_record_then_read_all_roundtrip() {
        let dir = temp_dlq_dir("append-then-read");
        let writer = SealDlqWriter::with_dlq_dir_for_test(dir.clone());
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp();
        let r1 = SealDlqRecord::from(&mk_serialized_seal(13, 0, 0, 1_716_000_900, 100.0));
        let r2 = SealDlqRecord::from(&mk_serialized_seal(25, 0, 4, 1_716_001_500, 200.0));
        writer.append_record(&r1, now).expect("append r1");
        writer.append_record(&r2, now).expect("append r2");
        let drained = writer.read_all(now).expect("read");
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0], r1);
        assert_eq!(drained[1], r2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_seal_dlq_writer_read_all_on_missing_file_returns_empty() {
        let dir = temp_dlq_dir("missing-file");
        let writer = SealDlqWriter::with_dlq_dir_for_test(dir.clone());
        let now = chrono::Utc::now().timestamp();
        let drained = writer.read_all(now).expect("ok");
        assert!(drained.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_clear_dlq_for_date_removes_file() {
        let dir = temp_dlq_dir("clear-removes");
        let writer = SealDlqWriter::with_dlq_dir_for_test(dir.clone());
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp();
        let r1 = SealDlqRecord::from(&mk_serialized_seal(13, 0, 0, 1_716_000_900, 100.0));
        writer.append_record(&r1, now).expect("append");
        let path = writer.dlq_path(now);
        assert!(path.exists());
        writer.clear_dlq_for_date(now).expect("clear");
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_clear_dlq_for_date_on_missing_file_is_noop() {
        let dir = temp_dlq_dir("clear-missing");
        let writer = SealDlqWriter::with_dlq_dir_for_test(dir.clone());
        let now = chrono::Utc::now().timestamp();
        // No file written. Clear must succeed.
        writer.clear_dlq_for_date(now).expect("idempotent");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_seal_dlq_writer_skips_corrupt_lines_gracefully() {
        // Manually write 3 lines: valid, garbage, valid. Reader must
        // return both valid records and silently drop the corrupt
        // line with a warn — no panic, no early return.
        let dir = temp_dlq_dir("corrupt-line");
        let writer = SealDlqWriter::with_dlq_dir_for_test(dir.clone());
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp();
        let r1 = SealDlqRecord::from(&mk_serialized_seal(13, 0, 0, 1_716_000_900, 100.0));
        let r2 = SealDlqRecord::from(&mk_serialized_seal(25, 0, 4, 1_716_001_500, 200.0));
        writer.append_record(&r1, now).expect("append r1");
        // Manually inject a corrupt line.
        let path = writer.dlq_path(now);
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open append");
            f.write_all(b"this-is-not-json\n").expect("write garbage");
            f.flush().expect("flush");
        }
        writer.append_record(&r2, now).expect("append r2");
        let drained = writer.read_all(now).expect("read");
        assert_eq!(drained.len(), 2, "corrupt line must NOT stall replay");
        assert_eq!(drained[0], r1);
        assert_eq!(drained[1], r2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_seal_dlq_writer_skips_blank_lines_silently() {
        // A trailing newline produces a blank line on read — must be
        // skipped silently (no warn flood on the happy path).
        let dir = temp_dlq_dir("blank-line");
        let writer = SealDlqWriter::with_dlq_dir_for_test(dir.clone());
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp();
        let r1 = SealDlqRecord::from(&mk_serialized_seal(13, 0, 0, 1_716_000_900, 100.0));
        writer.append_record(&r1, now).expect("append");
        // Append blank lines manually.
        let path = writer.dlq_path(now);
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open append");
            f.write_all(b"\n\n   \n").expect("write blanks");
            f.flush().expect("flush");
        }
        let drained = writer.read_all(now).expect("read");
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0], r1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_seal_dlq_writer_handles_50_records_in_fifo_order() {
        let dir = temp_dlq_dir("fifo-50");
        let writer = SealDlqWriter::with_dlq_dir_for_test(dir.clone());
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp();
        let n: u32 = 50;
        for i in 0..n {
            let r = SealDlqRecord::from(&mk_serialized_seal(
                13,
                0,
                (i % 9) as u8,
                1_716_000_000 + i,
                100.0 + i as f64,
            ));
            writer.append_record(&r, now).expect("append");
        }
        let drained = writer.read_all(now).expect("read");
        assert_eq!(drained.len(), n as usize);
        for (i, r) in drained.iter().enumerate() {
            assert_eq!(r.bucket_start_ist_secs, 1_716_000_000 + i as u32);
            assert_eq!(r.close, 100.0 + i as f64);
            assert_eq!(r.tf_ordinal, (i % 9) as u8);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_seal_dlq_writer_distinguishes_segments_for_i_p1_11() {
        // I-P1-11: composite key (security_id, exchange_segment_code).
        // Same security_id with different exchange_segment_code MUST
        // round-trip as two distinct records — no collapse.
        let dir = temp_dlq_dir("i-p1-11");
        let writer = SealDlqWriter::with_dlq_dir_for_test(dir.clone());
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp();
        let seg0 = SealDlqRecord::from(&mk_serialized_seal(13, 0, 0, 1_716_000_900, 100.0));
        let seg1 = SealDlqRecord::from(&mk_serialized_seal(13, 1, 0, 1_716_000_900, 200.0));
        writer.append_record(&seg0, now).expect("append seg0");
        writer.append_record(&seg1, now).expect("append seg1");
        let drained = writer.read_all(now).expect("read");
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].exchange_segment_code, 0);
        assert_eq!(drained[1].exchange_segment_code, 1);
        assert_eq!(drained[0].close, 100.0);
        assert_eq!(drained[1].close, 200.0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn test_seal_dlq_writer_appends_one_line_per_record() {
        // Wire-format invariant: NDJSON = one JSON object per line.
        // Operator's `cat | jq` and `wc -l` MUST produce the record
        // count.
        let dir = temp_dlq_dir("one-line-per-record");
        let writer = SealDlqWriter::with_dlq_dir_for_test(dir.clone());
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("valid")
            .timestamp();
        let r1 = SealDlqRecord::from(&mk_serialized_seal(13, 0, 0, 1_716_000_900, 100.0));
        let r2 = SealDlqRecord::from(&mk_serialized_seal(25, 0, 4, 1_716_001_500, 200.0));
        writer.append_record(&r1, now).expect("append r1");
        writer.append_record(&r2, now).expect("append r2");
        let path = writer.dlq_path(now);
        let raw = std::fs::read_to_string(&path).expect("read raw");
        // Two records => exactly two newlines (one per record).
        assert_eq!(raw.matches('\n').count(), 2);
        // No JSON object spans a line.
        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let _: SealDlqRecord = serde_json::from_str(line).expect("each line parses");
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}

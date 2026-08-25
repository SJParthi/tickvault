//! QuestDB partition **archive → verify → drop** — retention that actually
//! frees disk (2026-07-13 prod disk-pressure remediation).
//!
//! The legacy retention path (`partition_manager::detach_old_partitions`)
//! only DETACHes: the partition dir is renamed INSIDE the same EBS volume,
//! so zero bytes are freed — and at `retention_days = 90` on a ~7-week-old
//! dataset it has detached 0 partitions on every run while the 30 GB root
//! volume climbed to 82% (~1.2–2 GB/day of QuestDB growth). This module is
//! the documented missing piece ("S3-upload + local cleanup … NOT yet
//! implemented", partition_manager.rs honest boundary).
//!
//! # Flow (per eligible `(table, partition)`, strictly ordered)
//!
//! 1. **Eligibility** — partitions strictly older than the table's
//!    retention-class hot window (market-data: `ticks` + the 21 `candles_*`
//!    tables → `market_data_hot_days`, default 15 (2026-08-19 operator
//!    approval, was 35; ticks + sub-minute candles left this class entirely
//!    in the same day's Intraday split) —
//!    incl. the per-minute chain tables since the same date; everything else →
//!    `retention_days`, 90), clamped to a hard [`MIN_HOT_DAYS`] = 2 floor so
//!    today's and yesterday's partitions are untouchable regardless of
//!    config. Listed via the SAME `table_partitions()` mechanism + active-
//!    partition exclusion + fail-closed name allowlist the detach path uses.
//! 0. **WAL-suspension gate (review round 1, F3)** — the run probes
//!    `wal_tables()` FIRST. A probe failure skips the ENTIRE run (cannot
//!    prove any table's rows are fully applied); a SUSPENDED table is
//!    skipped table-wide with one `warn!` — a suspended table's export and
//!    recount see only APPLIED rows, so ACKed-but-unapplied rows would be
//!    destroyed on `RESUME WAL` if we dropped its partitions.
//! 2. **Export** — QuestDB HTTP `/exp` CSV of `SELECT * FROM <t> WHERE
//!    ts >= '<start>' AND ts < '<end>'` (explicit ISO range predicates —
//!    never integer literals against TIMESTAMP columns, never
//!    `IN (SELECT …)`), streamed chunk-by-chunk through a gzip encoder into
//!    a temp file under `data/tmp/partition-archive/`, counting CSV RECORDS
//!    in the RAW (pre-compression) stream with a quote-aware state machine
//!    (review round 1, F5 — a raw `\n` count over-counts on STRING fields
//!    carrying embedded newlines, e.g. this module's own audit `detail`).
//! 3. **Upload (never-overwrite, review rounds 1+2)** — `HeadObject` FIRST
//!    (with `ChecksumMode::Enabled`): an existing object is REUSED only on
//!    CONTENT IDENTITY — equal length (fast pre-filter) AND a stored SHA-256
//!    checksum equal to the fresh export's digest (round 2: a stale object
//!    plus a mutated partition with a coincidental compressed length must
//!    never reach a drop; ETag is deliberately NOT used — under SSE-KMS it
//!    is not the MD5). Length/checksum mismatch OR a checksum-less
//!    foreign/legacy object is a loud `S3Conflict` (keep the partition —
//!    never overwrite what may be the only copy of already-dropped data);
//!    absent → conditional `PutObject` with `If-None-Match: "*"` +
//!    `checksum_sha256` (S3 validates the body server-side) to
//!    `s3://<bucket>/questdb-partitions/<table>/<partition>.csv.gz`. Bucket
//!    = explicit config, else `tv-<env>-cold` ONLY when the environment env
//!    var is EXPLICITLY set — no env var, no archival (fail-closed; a dev
//!    box can never write the prod bucket).
//! 4. **Verify (fail-closed, ALL of)** — (a) `SELECT count()` over the same
//!    range taken AFTER the export equals the exported record count (the
//!    ≥2-day floor makes the count stable); (b) `HeadObject` exists and
//!    ContentLength == the local gzip file length; (c) the raw-stream
//!    record count is the byte-equivalent of decompressing the uploaded
//!    file and counting records — the counted bytes are exactly the bytes
//!    fed to the encoder, so no second decompression pass is needed. Only a
//!    passing verify constructs the [`VerifiedArchive`] type-state proof.
//! 5. **Audit-GATED drop (review round 1, F2)** — the `verified` forensic
//!    row is appended to `partition_archive_audit` and flushed over
//!    ILP-over-HTTP (per-flush server ACK — the 2026-07-05 fire-and-forget
//!    lesson); ONLY a confirmed flush releases
//!    `ALTER TABLE <t> DROP PARTITION LIST '<p>'` — this FREES disk. A
//!    failed audit flush KEEPS the partition (`AuditFailed`).
//!
//! On ANY failure the partition is KEPT, one `error!` fires with
//! `code = STORAGE-GAP-04`, and the loop moves on — bounded (the next daily
//! run retries), never a retry storm. The whole leg is gated on
//! `[partition_retention] archive_enabled` (serde default FALSE): a config
//! rollback restores the legacy detach-only behaviour instantly.
//!
//! # SEBI retention
//! The verified S3 copy IS the long-term record (5-year class via the cold
//! bucket; house doctrine: hot window on EBS, older → S3 — aws-budget.md).
//! A partition is only ever dropped AFTER that copy is proven present and
//! size/row-count-consistent.
//!
//! # Hot path
//! ZERO involvement — this is the once-daily post-market cold task.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use flate2::Compression;
use flate2::write::GzEncoder;
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use reqwest::Client;
use tracing::{debug, error, info, warn};

use tickvault_common::config::{PartitionRetentionConfig, QuestDbConfig};
use tickvault_common::constants::IST_UTC_OFFSET_NANOS;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::sanitize::capture_rest_error_body;

use crate::partition_manager::{
    DAY_PARTITIONED_TABLES, HOUR_PARTITIONED_TABLES, RETENTION_EXEMPT_TABLES,
    build_detach_list_sql, is_valid_partition_name, parse_partition_rows,
    select_partitions_to_detach,
};
use crate::wal_suspension_watcher::parse_wal_tables_response;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Hard floor on every retention window: today's and yesterday's partitions
/// are NEVER eligible, regardless of config. ≥2 days also guarantees the
/// partition's row count is stable (no in-flight writes) so the
/// export-then-recount verify is meaningful.
pub const MIN_HOT_DAYS: u32 = 2;

// Compile-time pin: the floor can never be weakened below 2 without editing
// this line (and the plan/rule trail that cites it).
const _: () = assert!(MIN_HOT_DAYS >= 2, "MIN_HOT_DAYS floor must stay >= 2");

/// S3 key prefix for archived partitions inside the cold bucket.
pub const ARCHIVE_S3_PREFIX: &str = "questdb-partitions";

/// Local temp dir for the in-flight gzip export (one partition at a time —
/// worst-case transient disk use is a single compressed partition). Cleaned
/// at the start of every run.
pub const ARCHIVE_TEMP_DIR: &str = "data/tmp/partition-archive";

/// HTTP timeout for DDL / count / drop statements (mirrors the detach path).
const ARCHIVE_DDL_TIMEOUT_SECS: u64 = 30;

/// HTTP timeout for a single partition `/exp` CSV export. Generous — an
/// hourly `ticks` partition can be hundreds of MB of CSV; the stream is
/// consumed incrementally so memory stays bounded either way.
const ARCHIVE_EXPORT_TIMEOUT_SECS: u64 = 600;

/// Cap on any `/exec` response body read into memory (SEC-LOW2, review
/// round 1). The largest legitimate body is a `table_partitions()` listing
/// (a few hundred KB on the first catch-up sweep); 4 MiB bounds a hostile /
/// misrouted response without ever truncating a real one.
const ARCHIVE_MAX_EXEC_BODY_BYTES: u64 = 4 * 1024 * 1024;

/// The WAL-suspension probe (F3) — same query the WAL-SUSPEND-01 watcher
/// issues; parsed by the SAME `parse_wal_tables_response`.
const WAL_TABLES_PROBE_SQL: &str = "select * from wal_tables()";

/// Forensic audit table — one row per archive attempt outcome.
pub const PARTITION_ARCHIVE_AUDIT_TABLE: &str = "partition_archive_audit";

/// DEDUP UPSERT key. Designated timestamp first (2026-04-28 regression rule);
/// `feed` per the 2026-06-28 operator override (feed-in-key everywhere). No
/// `security_id` → I-P1-11 N/A (per-partition table, no instrument key).
pub const DEDUP_KEY_PARTITION_ARCHIVE_AUDIT: &str = "ts, table_name, partition_name, feed";

/// `feed` label stamped on every archive-audit row. An archived partition
/// contains EVERY feed's rows (the shared tables are feed-tagged per row),
/// so the honest scope label is `all` — this is an archive-scope marker, not
/// a broker source. In the DEDUP key per the 2026-06-28 feed-in-key override.
pub const ARCHIVE_FEED_ALL: &str = "all";

// ---------------------------------------------------------------------------
// Retention classes (pure)
// ---------------------------------------------------------------------------

/// Retention class of a swept table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetentionClass {
    /// High-volume market data (`ticks` + the 21 `candles_*` tables + the
    /// per-minute chain tables below) — `market_data_hot_days` hot window
    /// (default 35 since 2026-07-16; was 14).
    MarketData,
    /// Everything else (audit / daily-data tables) — the existing
    /// `retention_days` window (default 90).
    Standard,
    /// `ticks` + the five second-level candle tables — their own class since
    /// 2026-08-19, on the operator directive separating current-day intraday
    /// data from the minute-level history indicators and strategies read.
    ///
    /// At the 25,000-instrument target `ticks` runs ~14 GB/day and the
    /// second-level candles are the heaviest tables after depth; 35 days of
    /// either commits several hundred GB on a 200 GB root, and a full disk
    /// stops EVERY table. Same reasoning that carved out `Depth` in
    /// 2026-08-15, applied one level down.
    Intraday,
    /// `market_depth` — its own class because it is an order of magnitude
    /// heavier than anything else stored (2026-08-15). ~21 GB/day measured
    /// over the 24,000-second session at 72 B/row, ~104 GB/day at five
    /// snapshots per second. Under the MarketData window it would commit
    /// 735 GB minimum on a 100 GB root, so the sweep could never fire before
    /// the disk filled — and a full disk stops EVERY table, not just this one.
    Depth,
}

/// Tables in the [`RetentionClass::Depth`] window.
///
/// Checked BEFORE the market-data test in [`retention_class`], because
/// `market_depth` is also HOUR-partitioned and would otherwise be swept on
/// the 35-day market-data window by virtue of that membership alone.
const DEPTH_TABLES: [&str; 1] = [crate::depth_persistence::MARKET_DEPTH_TABLE];

/// True when `table` belongs to the [`RetentionClass::Intraday`] window —
/// `ticks` plus every SUB-MINUTE candle table.
///
/// DERIVED, not listed. The first version of this was a literal array, and it
/// was wrong on the first run: the timeframe enum defines **16** sub-minute
/// timeframes (1s..15s and 30s), not the five the list named, so
/// `candles_2s` through `candles_14s` would have silently kept the 35-day
/// window — the exact failure the split exists to prevent, hidden inside the
/// fix for it. `intraday_class_covers_every_sub_minute_timeframe` caught it.
///
/// The predicate is now the semantic one: a timeframe whose bucket is shorter
/// than a minute is intraday. A new sub-minute timeframe joins automatically;
/// a minute-or-longer one cannot join by accident. That is worth the O(24)
/// scan, which runs on the cold retention sweep and never on a tick.
fn is_intraday_table(table: &str) -> bool {
    if table == TICKS_TABLE {
        return true;
    }
    (0..tickvault_trading::candles::TF_COUNT).any(|ordinal| {
        tickvault_trading::candles::TfIndex::from_ordinal(ordinal).is_some_and(|tf| {
            tf.table_name() == table && tf.seconds_per_bucket() < SECONDS_PER_MINUTE
        })
    })
}

/// The `ticks` table name, named rather than inlined so a rename is a compile
/// error here. It was a bare literal until 2026-08-19: a rename would have
/// silently reclassified ticks from current-day to the 15-day history window
/// — ~87 GB at target — with nothing failing to say so. `DEPTH_TABLES` uses
/// `MARKET_DEPTH_TABLE` for exactly this reason; this matches it.
const TICKS_TABLE: &str = "ticks";

/// One minute, in seconds — the boundary between "intraday, current-day only"
/// and "history the indicator and strategy paths read".
const SECONDS_PER_MINUTE: u32 = 60;
/// The two per-minute option-chain capture tables (operator directive
/// 2026-07-16: *"for only spots we will have minimum one month data …
/// but option only for the current day"* — the chain tables are the
/// system's HEAVIEST daily volume (~70 MB/day for `option_chain_1m`), so
/// they move from the 90-day Standard window into the `market_data_hot_days`
/// class. 35 days is the smallest clean change (one existing knob covers
/// ticks + candles + chain); tightening chain further toward current-day is
/// a follow-up knob if disk pressure demands it.
///
/// **First-sweep burst (dated honest note, 2026-07-16 round-2 MEDIUM):**
/// the 90 → 35 day move makes every chain partition aged 36..90 days
/// drop-eligible AT ONCE — up to ~55 DAY partitions × 2 tables ≈ 110
/// partitions (~4 GB) in the FIRST post-merge sweep. Bounded by design,
/// no code change: (a) `max_partitions_per_run` (default 200) caps the
/// worklist, so the burst fits one run but can never exceed the cap;
/// (b) each partition is processed STRICTLY serially through
/// export → upload → verify → audit → drop, so temp-disk holds ONE gzip
/// export at a time under `data/tmp/partition-archive/` and the
/// wall-clock cost is ~110 sequential export+upload round-trips (a
/// minutes-class one-time run, off-hours); (c) the leg stays FAIL-CLOSED
/// — a degraded S3 leg (upload/verify failure) keeps every affected
/// partition on disk (no verified copy ⇒ no drop), so the worst case of
/// a bad first sweep is "nothing freed yet", never data loss. Subsequent
/// sweeps return to the ~1-partition/day steady state.
const CHAIN_MARKET_DATA_TABLES: [&str; 2] = [
    crate::option_chain_1m_persistence::OPTION_CHAIN_1M_TABLE,
    crate::option_contract_1m_rest_persistence::OPTION_CONTRACT_1M_REST_TABLE,
];

/// Maps a table to its retention class. Market data = the HOUR-partitioned
/// high-frequency list (`ticks`) + the 21 live candle tables from the single
/// source of truth (`candle_table_names()`) + the two per-minute chain
/// tables (2026-07-16); everything else is Standard.
pub(crate) fn retention_class(table: &str) -> RetentionClass {
    // Depth FIRST. `market_depth` is also HOUR-partitioned, so testing the
    // market-data list before this would classify it on the 35-day window by
    // virtue of that membership alone — 735 GB on a 100 GB root, and the
    // sweep would never fire before the disk filled.
    if DEPTH_TABLES.contains(&table) {
        RetentionClass::Depth
    } else if is_intraday_table(table) {
        // BEFORE the market-data test, for exactly the reason Depth is before
        // both: `ticks` is HOUR_PARTITIONED and the second-level candles are
        // in `candle_table_names()`, so either would otherwise be classified
        // on the 35-day window by virtue of that membership alone.
        RetentionClass::Intraday
    } else if HOUR_PARTITIONED_TABLES.contains(&table)
        || crate::shadow_persistence::candle_table_names().contains(&table)
        || CHAIN_MARKET_DATA_TABLES.contains(&table)
    {
        RetentionClass::MarketData
    } else {
        RetentionClass::Standard
    }
}

/// Clamps a configured hot window to the [`MIN_HOT_DAYS`] floor — today's
/// and yesterday's partitions are untouchable even under `hot_days = 0`.
/// Takes the FLOOR as a parameter since 2026-08-19, when the floor became
/// per-class. Keeping a zero-argument version would have left production
/// calling `.max()` inline while the test suite exercised a function nothing
/// used — a test that passes forever regardless of what the sweep does.
pub(crate) fn effective_hot_days(configured_days: u32, floor: u32) -> u32 {
    configured_days.max(floor)
}

/// Hard floor for the MARKET-DATA classes ([`RetentionClass::Depth`] and
/// [`RetentionClass::Intraday`]): **1 day**, not 2.
///
/// Lowered from the general `MIN_HOT_DAYS` on 2026-08-19, on an operator
/// correction — "why two days bro ... nowhere for the previous day it will
/// never ever have any depths right dude". He is right, and the eligibility
/// SQL is what proves it.
///
/// `build_detach_list_sql` selects on
/// `minTimestamp < dateadd('d', -N, now())` — a ROLLING 24-hour window, not a
/// calendar comparison. At `N = 1`:
///
/// - Yesterday's partitions have `minTimestamp` at least a full day back, so
///   they ARE eligible and the previous day genuinely keeps no depth.
/// - Today's partition has `minTimestamp >= today 00:00`, which is ALWAYS
///   later than `now() - 24h`. It can never be selected — safe by
///   construction, not by scheduling luck.
///
/// So the general floor of 2 was not protecting anything here; it was simply
/// retaining a second day of the two heaviest tables on the box.
///
/// `0` remains forbidden and is exactly where the danger lives: the cutoff
/// becomes `now()`, and the partition being actively written matches. The
/// clamp below makes that unreachable from config.
///
/// The audit/Standard classes keep the 2-day floor: they are not tied to
/// market hours, can receive writes at any hour, and are small enough that a
/// second day costs nothing.
pub(crate) const MIN_HOT_DAYS_MARKET_DATA: u32 = 1;

/// The effective hot window (days) for a table under the given config —
/// class window clamped to the [`MIN_HOT_DAYS`] floor.
pub(crate) fn hot_window_days(table: &str, cfg: &PartitionRetentionConfig) -> u32 {
    let class = retention_class(table);
    let class_days = match class {
        RetentionClass::MarketData => cfg.market_data_hot_days,
        RetentionClass::Standard => cfg.retention_days,
        RetentionClass::Depth => cfg.depth_hot_days,
        RetentionClass::Intraday => cfg.intraday_hot_days,
    };
    // Floor is per-class since 2026-08-19: the two current-day classes clamp
    // at 1 so "current day only" is actually expressible, everything else
    // keeps the 2-day floor. Never 0 — see MIN_HOT_DAYS_MARKET_DATA.
    let floor = match class {
        RetentionClass::Depth | RetentionClass::Intraday => MIN_HOT_DAYS_MARKET_DATA,
        RetentionClass::MarketData | RetentionClass::Standard => MIN_HOT_DAYS,
    };
    effective_hot_days(class_days, floor)
}

// ---------------------------------------------------------------------------
// Partition-range SQL builders (pure)
// ---------------------------------------------------------------------------

/// ISO timestamp literal format QuestDB parses in `WHERE ts >= '<literal>'`
/// comparisons. String literals are used DELIBERATELY: integer literals
/// against a TIMESTAMP column are epoch MICROSECONDS (proven live) — an easy
/// nanos/micros landmine — and `IN (SELECT …)` on TIMESTAMP is rejected.
const TS_LITERAL_FMT: &str = "%Y-%m-%dT%H:%M:%S%.6fZ";

/// Derives the `[start, end)` designated-timestamp bounds of a QuestDB
/// partition from its name: DAY `YYYY-MM-DD` → [midnight, next midnight),
/// HOUR `YYYY-MM-DDTHH` → [hour, next hour). Returns `None` for anything
/// malformed (fail-closed — the partition is skipped, never guessed at).
pub(crate) fn partition_range_bounds(name: &str) -> Option<(String, String)> {
    use chrono::{NaiveDate, NaiveTime, TimeDelta};

    if !is_valid_partition_name(name) {
        return None;
    }
    let (start, end) = match name.len() {
        10 => {
            let date = NaiveDate::parse_from_str(name, "%Y-%m-%d").ok()?;
            let start = date.and_time(NaiveTime::MIN);
            (start, start.checked_add_signed(TimeDelta::days(1))?)
        }
        13 => {
            let (day_part, hour_part) = name.split_at(10);
            let date = NaiveDate::parse_from_str(day_part, "%Y-%m-%d").ok()?;
            let hour: u32 = hour_part.strip_prefix('T')?.parse().ok()?;
            let start = date.and_time(NaiveTime::from_hms_opt(hour, 0, 0)?);
            (start, start.checked_add_signed(TimeDelta::hours(1))?)
        }
        _ => return None,
    };
    Some((
        start.format(TS_LITERAL_FMT).to_string(),
        end.format(TS_LITERAL_FMT).to_string(),
    ))
}

/// `/exp` export SQL — the partition's full rows via explicit range
/// predicates on the designated timestamp `ts` (every swept table's
/// designated column — pinned by `timestamp(ts)` in every storage DDL).
/// `table` is always a trusted constant; the bounds derive from an
/// allowlist-validated partition name.
pub(crate) fn build_export_sql(table: &str, start: &str, end: &str) -> String {
    format!("SELECT * FROM {table} WHERE ts >= '{start}' AND ts < '{end}'")
}

/// Post-export recount SQL over the identical range (verify leg (a)).
pub(crate) fn build_count_sql(table: &str, start: &str, end: &str) -> String {
    format!("SELECT count() FROM {table} WHERE ts >= '{start}' AND ts < '{end}'")
}

/// The DESTRUCTIVE drop DDL. Only callable from
/// [`PartitionArchiver::drop_partition`], which requires the
/// [`VerifiedArchive`] type-state proof — the compiler enforces
/// archive-verify-before-drop.
fn build_drop_sql(table: &str, partition: &str) -> String {
    format!("ALTER TABLE {table} DROP PARTITION LIST '{partition}'")
}

/// Parses QuestDB's `/exec` JSON for a single-cell `count()` result.
/// Fail-closed: anything unexpected → `None` (→ verify fails → keep).
pub(crate) fn parse_count_response(json: &str) -> Option<u64> {
    let parsed: serde_json::Value = serde_json::from_str(json).ok()?;
    let cell = parsed
        .get("dataset")?
        .as_array()?
        .first()?
        .as_array()?
        .first()?;
    let n = cell.as_i64()?;
    u64::try_from(n).ok()
}

// ---------------------------------------------------------------------------
// Quote-aware CSV record counting (pure — review round 1, F5)
// ---------------------------------------------------------------------------

/// Streaming, quote-aware CSV RECORD counter (RFC-4180 quoting rules as
/// QuestDB `/exp` emits them: fields containing `,`/`"`/newlines are quoted;
/// embedded quotes double as `""`).
///
/// A raw `\n` count over-counts records when a STRING field carries an
/// embedded newline — e.g. this module's own `partition_archive_audit.detail`
/// column, or any error text persisted into an audit table — which would
/// make the export-vs-recount verify fail FOREVER for that partition
/// (livelock: kept + retried every run). Counting newlines only OUTSIDE
/// quoted fields counts true records.
///
/// The `""` escape needs no special casing for record counting: the two
/// adjacent quote bytes toggle the state out and back in with no newline
/// possible between them.
#[derive(Debug, Default)]
pub(crate) struct CsvRecordCounter {
    in_quotes: bool,
    terminated_records: u64,
    saw_any_byte: bool,
    last_byte_was_record_end: bool,
}

impl CsvRecordCounter {
    /// Feeds one raw (pre-compression) chunk through the counter.
    pub(crate) fn update(&mut self, chunk: &[u8]) {
        for &b in chunk {
            self.saw_any_byte = true;
            match b {
                b'"' => {
                    self.in_quotes = !self.in_quotes;
                    self.last_byte_was_record_end = false;
                }
                b'\n' if !self.in_quotes => {
                    self.terminated_records = self.terminated_records.saturating_add(1);
                    self.last_byte_was_record_end = true;
                }
                _ => self.last_byte_was_record_end = false,
            }
        }
    }

    /// Total CSV records seen (header INCLUDED). A final record without a
    /// trailing newline still counts; an empty body is 0 records. A dangling
    /// unterminated quote at EOF counts its partial record (the recount
    /// verify then decides — fail-closed either way).
    pub(crate) fn records(&self) -> u64 {
        if !self.saw_any_byte {
            0
        } else if self.last_byte_was_record_end {
            self.terminated_records
        } else {
            self.terminated_records.saturating_add(1)
        }
    }
}

// ---------------------------------------------------------------------------
// Verify decision (pure) + the type-state proof
// ---------------------------------------------------------------------------

/// Private zero-sized proof token. NOT constructible outside this module —
/// the ONLY constructor of [`VerifiedArchive`] is [`verify_archive`], so a
/// partition drop without a passed verification is a COMPILE error, not a
/// runtime hope.
#[derive(Debug)]
struct VerifyProof;

/// Type-state proof that a partition's S3 archive passed EVERY verification
/// check. Required by value at the drop site.
#[derive(Debug)]
pub struct VerifiedArchive {
    table: String,
    partition: String,
    /// Exported (and recount-confirmed) row count.
    pub rows: u64,
    /// Uncompressed CSV bytes streamed through the encoder.
    pub csv_bytes: u64,
    /// Compressed bytes on disk == S3 ContentLength (verified equal).
    pub gzip_bytes: u64,
    /// SHA-256 of the compressed archive (hex) — computed while writing the
    /// gzip stream; the same digest (base64) travels on the S3 PutObject
    /// `checksum_sha256` and gates the reuse arm (review round 2).
    pub gzip_sha256_hex: String,
    _proof: VerifyProof,
}

impl VerifiedArchive {
    /// The archived table name.
    #[must_use]
    pub fn table(&self) -> &str {
        &self.table
    }

    /// The archived partition name.
    #[must_use]
    pub fn partition(&self) -> &str {
        &self.partition
    }
}

/// Why a verification failed (the partition is KEPT in every case).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyFailure {
    /// The post-export recount could not be obtained (SQL error / parse).
    RecountUnavailable,
    /// Post-export `count()` disagreed with the exported row count.
    RowCountMismatch { exported: u64, recount: u64 },
    /// `HeadObject` found no object / no ContentLength.
    S3ObjectMissing,
    /// S3 ContentLength disagreed with the local gzip file length.
    S3SizeMismatch { s3: u64, local: u64 },
}

impl VerifyFailure {
    /// Stable audit-outcome label for this failure.
    #[must_use]
    pub const fn outcome(self) -> ArchiveOutcome {
        match self {
            Self::RecountUnavailable | Self::RowCountMismatch { .. } => {
                ArchiveOutcome::CountMismatch
            }
            Self::S3ObjectMissing | Self::S3SizeMismatch { .. } => ArchiveOutcome::SizeMismatch,
        }
    }
}

/// The fail-closed verify decision — ALL checks must pass to construct the
/// [`VerifiedArchive`] proof:
/// (a) post-export recount present AND equal to the exported row count;
/// (b) S3 ContentLength present AND equal to the local gzip length.
/// (Check (c), the line-count-vs-file equivalence, is structural: the
/// newline count was taken over the exact bytes fed to the encoder.)
#[allow(clippy::too_many_arguments)] // APPROVED: pure decision fn; every input is a distinct verified fact
pub(crate) fn verify_archive(
    table: &str,
    partition: &str,
    exported_rows: u64,
    csv_bytes: u64,
    local_gzip_len: u64,
    gzip_sha256_hex: &str,
    recount: Option<u64>,
    s3_content_length: Option<u64>,
) -> Result<VerifiedArchive, VerifyFailure> {
    let recount = recount.ok_or(VerifyFailure::RecountUnavailable)?;
    if recount != exported_rows {
        return Err(VerifyFailure::RowCountMismatch {
            exported: exported_rows,
            recount,
        });
    }
    let s3_len = s3_content_length.ok_or(VerifyFailure::S3ObjectMissing)?;
    if s3_len != local_gzip_len {
        return Err(VerifyFailure::S3SizeMismatch {
            s3: s3_len,
            local: local_gzip_len,
        });
    }
    Ok(VerifiedArchive {
        table: table.to_string(),
        partition: partition.to_string(),
        rows: exported_rows,
        csv_bytes,
        gzip_bytes: local_gzip_len,
        gzip_sha256_hex: gzip_sha256_hex.to_string(),
        _proof: VerifyProof,
    })
}

// ---------------------------------------------------------------------------
// Content identity (pure — review round 2)
// ---------------------------------------------------------------------------

/// Standard base64 (RFC 4648, with padding) — exactly what S3's
/// `x-amz-checksum-sha256` header carries. Hand-rolled for a fixed 32-byte
/// digest rather than adding a new supply-chain root; pinned by a
/// known-vector unit test below.
pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(triple >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[triple as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

/// Lowercase hex of a digest (audit-row provenance column).
pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// `Write` adapter folding every byte into a SHA-256 digest on its way to
/// the inner writer — the gzip stream is hashed AS IT IS WRITTEN, so the
/// content-identity digest covers exactly the bytes on disk (and therefore
/// exactly the bytes S3 receives) with no second file read.
struct Sha256Writer<W: Write> {
    inner: W,
    hasher: sha2::Sha256,
}

impl<W: Write> Sha256Writer<W> {
    fn new(inner: W) -> Self {
        use sha2::Digest as _;
        Self {
            inner,
            hasher: sha2::Sha256::new(),
        }
    }

    /// Consumes the adapter: `(inner writer, 32-byte digest)`.
    fn finish(self) -> (W, [u8; 32]) {
        use sha2::Digest as _;
        (self.inner, self.hasher.finalize().into())
    }
}

impl<W: Write> Write for Sha256Writer<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        use sha2::Digest as _;
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

// ---------------------------------------------------------------------------
// Bucket resolution (pure + thin env shim)
// ---------------------------------------------------------------------------

/// Resolves the archive bucket (review round 1, F1b — FAIL-CLOSED): an
/// explicit non-empty config value ALWAYS wins; an empty config derives the
/// house cold bucket `tv-<env>-cold` (terraform `tv-${environment}-cold`)
/// ONLY when the environment was EXPLICITLY provided by an env var. No
/// explicit environment → `None` → the caller SKIPS archival entirely for
/// the run. There is deliberately NO `"prod"` default here: a dev box /
/// worktree / CI shell without env vars must never write (or overwrite)
/// objects in the prod cold bucket.
pub(crate) fn resolve_archive_bucket(
    configured: &str,
    environment: Option<&str>,
) -> Option<String> {
    let trimmed = configured.trim();
    if !trimmed.is_empty() {
        return Some(trimmed.to_string());
    }
    environment.map(|env| format!("tv-{env}-cold"))
}

/// Environment-name resolution from raw env-var reads (pure — testable
/// without process-global env mutation). Precedence: `TV_ENVIRONMENT` →
/// `ENVIRONMENT`. `None` when neither is set (F1b fail-closed — the
/// SSM-path `"prod"` fallback in `secret_manager::resolve_environment` is
/// DELIBERATELY not mirrored here). A value with characters outside
/// `[a-zA-Z0-9-]` also resolves `None` (bucket-name safety, fail-closed).
pub(crate) fn resolve_environment_from(
    tv_environment: Option<&str>,
    environment: Option<&str>,
) -> Option<String> {
    let candidate = tv_environment
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| environment.map(str::trim).filter(|s| !s.is_empty()))?;
    if candidate
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        Some(candidate.to_string())
    } else {
        None
    }
}

/// Reads the explicitly-set runtime environment name (`TV_ENVIRONMENT` →
/// `ENVIRONMENT`); `None` when neither env var is set — archival is then
/// skipped for the run (F1b fail-closed).
fn runtime_environment() -> Option<String> {
    resolve_environment_from(
        std::env::var("TV_ENVIRONMENT").ok().as_deref(),
        std::env::var("ENVIRONMENT").ok().as_deref(),
    )
}

// ---------------------------------------------------------------------------
// Archive-outcome labels + audit persistence
// ---------------------------------------------------------------------------

/// Attempt outcome for the `partition_archive_audit.outcome` SYMBOL column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveOutcome {
    /// Every verification check passed — written BEFORE the drop
    /// (audit-first: durable proof the S3 copy was verified).
    Verified,
    /// The partition was dropped after verification — disk freed.
    Dropped,
    /// `/exp` export failed (non-2xx, stream error, or no header row).
    ExportFailed,
    /// Post-export recount unavailable or ≠ exported rows — kept.
    CountMismatch,
    /// S3 object missing or ContentLength ≠ local gzip length — kept.
    SizeMismatch,
    /// S3 PutObject failed — kept.
    UploadFailed,
    /// A DIFFERENT-length object already exists at the archive key (F1) —
    /// NEVER overwritten (it may be the only copy of already-dropped data);
    /// the partition is kept and the operator inspects the key.
    S3Conflict,
    /// The `verified` audit row could not be durably flushed (F2) — the
    /// drop is WITHHELD; the partition is kept and retried next run.
    AuditFailed,
    /// The DROP DDL failed AFTER a passed verify — the S3 copy exists;
    /// retried next run (same-length object is reused, never re-uploaded).
    DropFailed,
}

impl ArchiveOutcome {
    /// Stable wire-format label for the `outcome` SYMBOL column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Dropped => "dropped",
            Self::ExportFailed => "export_failed",
            Self::CountMismatch => "count_mismatch",
            Self::SizeMismatch => "size_mismatch",
            Self::UploadFailed => "upload_failed",
            Self::S3Conflict => "s3_conflict",
            Self::AuditFailed => "audit_failed",
            Self::DropFailed => "drop_failed",
        }
    }
}

/// The canonical S3 key a partition's archive lives at.
///
/// One key per `(table, partition)`. Stable across runs, and the object at
/// this key is **never overwritten and never deleted** by the archiver —
/// it may be the only surviving copy of data that has since been dropped
/// from the database.
#[must_use]
pub fn canonical_archive_key(table: &str, partition: &str) -> String {
    format!("{ARCHIVE_S3_PREFIX}/{table}/{partition}.csv.gz")
}

/// The content-addressed key used when a partition no longer matches the
/// archive already sitting at its canonical key.
///
/// # Why this exists
///
/// A partition is not immutable. `ticks` is `PARTITION BY HOUR` on the
/// exchange last-trade time, and 10.0% of live ticks arrive with a timestamp
/// more than an hour behind their receive time (measured 2026-08-24:
/// 6,433,267 of 64,349,753). Those rows land in an hourly partition that has
/// already been archived, so a later export can never reproduce the archived
/// bytes — and under a pure never-overwrite policy the partition is pinned to
/// disk forever. That is not hypothetical: on 2026-08-25 the live box held
/// 1,000 `s3_conflict` audit rows, zero successful archives, and tick data
/// going back to 2026-06-02 against a ONE-DAY hot window.
///
/// Embedding the FULL SHA-256 hex of the gzip makes the key:
/// - **collision-free** — different content is a different key, so writing
///   here can never destroy another export;
/// - **idempotent** — re-exporting unchanged content yields the same key, so
///   a retry reuses the object instead of uploading a duplicate;
/// - **self-describing** — the key states exactly which bytes it holds.
///
/// The canonical object is left untouched, so BOTH copies survive.
#[must_use]
pub fn sidecar_archive_key(table: &str, partition: &str, gzip_sha256_hex: &str) -> String {
    format!("{ARCHIVE_S3_PREFIX}/{table}/{partition}.sha-{gzip_sha256_hex}.csv.gz")
}

/// The idempotent `CREATE TABLE` DDL for the archive-audit chain. Pure
/// (unit-testable without QuestDB).
#[must_use]
pub fn partition_archive_audit_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {PARTITION_ARCHIVE_AUDIT_TABLE} (\
            ts                 TIMESTAMP, \
            table_name         SYMBOL, \
            partition_name     SYMBOL, \
            outcome            SYMBOL, \
            feed               SYMBOL, \
            rows_archived      LONG, \
            csv_bytes          LONG, \
            gzip_bytes         LONG, \
            gzip_sha256        STRING, \
            s3_key             STRING, \
            detail             STRING\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_PARTITION_ARCHIVE_AUDIT});"
    )
}

/// One archive-attempt audit row.
#[derive(Clone, Debug)]
pub(crate) struct PartitionArchiveAuditRow {
    pub ts_ist_nanos: i64,
    pub table_name: String,
    pub partition_name: String,
    pub outcome: ArchiveOutcome,
    pub rows_archived: i64,
    pub csv_bytes: i64,
    pub gzip_bytes: i64,
    /// SHA-256 (hex) of the compressed archive — content-identity
    /// provenance (review round 2); empty on failure rows with no export.
    pub gzip_sha256: String,
    pub s3_key: String,
    pub detail: String,
}

/// ILP-over-HTTP conf for the archive-audit writer (review round 1, F2) —
/// per-flush server ACK (the 2026-07-05 fire-and-forget lesson: ILP TCP
/// never surfaces server-side rejects, so a "durable verified row before
/// drop" claim over TCP was FALSE). Shadow-candle-writer knobs:
/// `retry_timeout=0` (the questdb-rs internal sleep-and-resend loop is
/// disabled — the next daily run owns retry) + `request_timeout=5000` ms
/// (bounds a hung flush).
pub(crate) fn partition_archive_audit_ilp_http_conf(config: &QuestDbConfig) -> String {
    format!(
        "http::addr={}:{};protocol_version=1;retry_timeout=0;request_timeout=5000;",
        config.host, config.http_port
    )
}

/// Lazy-connect ILP-over-HTTP writer for `partition_archive_audit`. The
/// `verified` row's flush ACK GATES the drop (F2); every other row is
/// best-effort (AUDIT-WS-01 class) — the S3 object + the partition's
/// presence/absence remain the ground truth.
pub(crate) struct PartitionArchiveAuditWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl PartitionArchiveAuditWriter {
    /// Production constructor — ILP-over-HTTP, lazy on failure.
    // TEST-EXEMPT: production ILP-connect constructor (thin shim over
    // with_conf, which the stub-server integration tests exercise).
    pub(crate) fn new(config: &QuestDbConfig) -> Self {
        Self::with_conf(&partition_archive_audit_ilp_http_conf(config))
    }

    /// Builds a writer from an explicit questdb-rs conf string (the stub
    /// integration tests point this at a local ILP-HTTP stub).
    pub(crate) fn with_conf(conf: &str) -> Self {
        match Sender::from_conf(conf) {
            Ok(s) => {
                let b = s.new_buffer();
                Self {
                    sender: Some(s),
                    buffer: b,
                    pending: 0,
                }
            }
            Err(err) => {
                warn!(
                    ?err,
                    "partition_archive_audit writer: QuestDB unreachable — buffering locally"
                );
                Self {
                    sender: None,
                    buffer: Buffer::new(ProtocolVersion::V1),
                    pending: 0,
                }
            }
        }
    }

    /// Test constructor — disconnected writer, empty buffer.
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self {
            sender: None,
            buffer: Buffer::new(ProtocolVersion::V1),
            pending: 0,
        }
    }

    /// Appends one audit row (cold path, once per partition attempt).
    pub(crate) fn append_row(&mut self, r: &PartitionArchiveAuditRow) -> Result<()> {
        self.buffer
            .table(PARTITION_ARCHIVE_AUDIT_TABLE)
            .context("table")?
            .symbol("table_name", &r.table_name)
            .context("table_name")?
            .symbol("partition_name", &r.partition_name)
            .context("partition_name")?
            .symbol("outcome", r.outcome.as_str())
            .context("outcome")?
            // `feed` is a DEDUP-key SYMBOL — ALWAYS written (2026-06-28
            // feed-in-key override). `all` = the archive covers every feed's
            // rows. Symbols precede fields (ILP tags-before-fields rule).
            .symbol("feed", ARCHIVE_FEED_ALL)
            .context("feed")?
            .column_i64("rows_archived", r.rows_archived)
            .context("rows_archived")?
            .column_i64("csv_bytes", r.csv_bytes)
            .context("csv_bytes")?
            .column_i64("gzip_bytes", r.gzip_bytes)
            .context("gzip_bytes")?
            .column_str("gzip_sha256", &r.gzip_sha256)
            .context("gzip_sha256")?
            .column_str("s3_key", &r.s3_key)
            .context("s3_key")?
            .column_str("detail", &r.detail)
            .context("detail")?
            .at(TimestampNanos::new(r.ts_ist_nanos))
            .context("designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Flushes buffered rows to QuestDB — ILP-over-HTTP, so `Ok` means the
    /// server ACKED the batch (F2: this ACK gates the drop for `verified`
    /// rows).
    pub(crate) fn flush(&mut self) -> Result<()> {
        if self.pending == 0 {
            return Ok(());
        }
        let Some(sender) = self.sender.as_mut() else {
            anyhow::bail!("partition_archive_audit: no ILP sender (QuestDB unreachable)");
        };
        sender
            .flush(&mut self.buffer)
            .context("partition_archive_audit ILP flush")?;
        self.pending = 0;
        Ok(())
    }

    /// Discards buffered-but-unflushed rows after a failed flush (the
    /// shadow-writer `discard_pending` poisoned-buffer defense: one
    /// server-rejected row must never wedge every later flush of the run).
    pub(crate) fn discard_pending(&mut self) {
        if self.pending == 0 {
            return;
        }
        self.buffer.clear();
        self.pending = 0;
    }

    /// Rows appended but not yet flushed.
    #[cfg(test)]
    pub(crate) fn pending(&self) -> usize {
        self.pending
    }
}

// ---------------------------------------------------------------------------
// The archiver
// ---------------------------------------------------------------------------

/// Per-run summary for the coalesced `info!` line (and the caller's log).
#[derive(Clone, Copy, Debug, Default)]
pub struct ArchiveRunSummary {
    /// Distinct tables scanned for eligible partitions.
    pub tables_scanned: u32,
    /// Eligible partitions considered this run (post per-run cap).
    pub partitions_considered: u32,
    /// Partitions whose S3 archive VERIFIED (proof constructed).
    pub verified: u32,
    /// Partitions dropped (disk actually freed).
    pub dropped: u32,
    /// Failed attempts (kept partitions — retried next run).
    pub failed: u32,
    /// Tables whose eligible-partition LIST query failed this run.
    ///
    /// A list failure means the table contributed NOTHING to the worklist —
    /// it is skipped whole, exactly as if it had no eligible partitions. That
    /// is indistinguishable from healthy in every other field here, and the
    /// per-table log line for it is `debug!`, which production never emits.
    /// Surfacing the count on the existing cycle-summary line makes a silently
    /// unswept table visible without adding a metric (the EMF selector has no
    /// byte budget left) or a per-run warning for tables that legitimately do
    /// not exist yet.
    pub tables_list_failed: u32,
    /// Rows durably archived to S3 across dropped partitions.
    pub rows_archived: u64,
    /// Compressed bytes uploaded to S3.
    pub gzip_bytes_uploaded: u64,
    /// Uncompressed CSV bytes exported — the disk-bytes-freed ESTIMATE
    /// basis (columnar on-disk size differs; CSV is the honest upper proxy).
    pub csv_bytes_exported: u64,
}

/// Outcome of streaming one partition's `/exp` CSV through gzip to disk.
struct ExportedCsv {
    rows: u64,
    csv_bytes: u64,
    gzip_bytes: u64,
    /// SHA-256 of the compressed file, base64 (the S3 checksum wire format).
    gzip_sha256_b64: String,
    /// SHA-256 of the compressed file, lowercase hex (audit provenance).
    gzip_sha256_hex: String,
    path: PathBuf,
}

/// S3 `HeadObject` metadata relevant to the reuse/conflict decision.
struct S3ObjectMeta {
    /// ContentLength.
    len: u64,
    /// `x-amz-checksum-sha256` attribute (base64), when the object carries
    /// one. Our uploads ALWAYS set it; a foreign/legacy object may not.
    checksum_sha256_b64: Option<String>,
}

/// QuestDB partition archiver — see the module docs for the full contract.
pub struct PartitionArchiver {
    exec_url: String,
    exp_url: String,
    ddl_client: Client,
    export_client: Client,
    s3: aws_sdk_s3::Client,
    bucket: String,
    temp_dir: PathBuf,
    cfg: PartitionRetentionConfig,
    audit: PartitionArchiveAuditWriter,
}

impl PartitionArchiver {
    /// Builds the archiver: HTTP clients, the S3 client from the default AWS
    /// credential chain (instance role on prod), and the resolved bucket.
    ///
    /// Returns `Ok(None)` when NO bucket is resolvable (empty
    /// `archive_bucket` config AND no explicit `TV_ENVIRONMENT`/
    /// `ENVIRONMENT` env var) — review round 1 F1b fail-closed: archival is
    /// SKIPPED for the run rather than defaulting to the prod cold bucket.
    ///
    /// # Errors
    /// HTTP-client build failure (fail-closed — the caller skips the archive
    /// cycle for the run and logs `STORAGE-GAP-04`).
    // TEST-EXEMPT: constructs live AWS/HTTP clients (aws_config::load needs
    // a runtime env); the orchestration is covered by the stub-server
    // integration tests via from_parts(), the resolution by unit tests.
    pub async fn new(
        questdb: &QuestDbConfig,
        cfg: &PartitionRetentionConfig,
    ) -> Result<Option<Self>> {
        let Some(bucket) =
            resolve_archive_bucket(&cfg.archive_bucket, runtime_environment().as_deref())
        else {
            warn!(
                "partition archive SKIPPED: no [partition_retention] archive_bucket configured \
                 and no explicit TV_ENVIRONMENT/ENVIRONMENT env var — refusing to guess a \
                 bucket (fail-closed; set one of them to enable archival)"
            );
            return Ok(None);
        };
        let ddl_client = Client::builder()
            .timeout(Duration::from_secs(ARCHIVE_DDL_TIMEOUT_SECS))
            .build()
            .context("failed to create DDL HTTP client for partition archiver")?;
        let export_client = Client::builder()
            .timeout(Duration::from_secs(ARCHIVE_EXPORT_TIMEOUT_SECS))
            .build()
            .context("failed to create export HTTP client for partition archiver")?;
        let aws_conf = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .load()
            .await;
        let s3 = aws_sdk_s3::Client::new(&aws_conf);
        Ok(Some(Self {
            exec_url: format!("http://{}:{}/exec", questdb.host, questdb.http_port),
            exp_url: format!("http://{}:{}/exp", questdb.host, questdb.http_port),
            ddl_client,
            export_client,
            s3,
            bucket,
            temp_dir: PathBuf::from(ARCHIVE_TEMP_DIR),
            cfg: cfg.clone(),
            audit: PartitionArchiveAuditWriter::new(questdb),
        }))
    }

    /// Test-injection constructor: explicit endpoints, S3 client, bucket,
    /// temp dir, and audit writer — the stub-server integration tests drive
    /// the FULL archive→verify→drop orchestration against local stubs.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_parts(
        exec_url: String,
        exp_url: String,
        s3: aws_sdk_s3::Client,
        bucket: String,
        temp_dir: PathBuf,
        cfg: PartitionRetentionConfig,
        audit: PartitionArchiveAuditWriter,
    ) -> Result<Self> {
        Ok(Self {
            exec_url,
            exp_url,
            // .no_proxy(): the sandbox/CI proxy env vars must never route
            // localhost stub traffic (test-only clients).
            ddl_client: Client::builder()
                .timeout(Duration::from_secs(ARCHIVE_DDL_TIMEOUT_SECS))
                .no_proxy()
                .build()
                .context("test DDL client")?,
            export_client: Client::builder()
                .timeout(Duration::from_secs(ARCHIVE_EXPORT_TIMEOUT_SECS))
                .no_proxy()
                .build()
                .context("test export client")?,
            s3,
            bucket,
            temp_dir,
            cfg,
            audit,
        })
    }

    /// Runs one archive→verify→drop cycle over every retention-swept table.
    /// Every failure is per-partition fail-closed (keep + log + skip); the
    /// run itself never errors — the summary carries the counts.
    // TEST-EXEMPT: live QuestDB + S3 orchestration; every decision inside is
    // a unit-tested pure function and the first prod run is the live probe
    // (a failed probe is a loud no-op, never a drop).
    pub async fn archive_and_drop_old_partitions(&mut self) -> ArchiveRunSummary {
        let mut summary = ArchiveRunSummary::default();

        self.ensure_audit_table().await;
        self.clean_temp_dir();

        // F3 (review round 1): WAL-suspension gate FIRST. A suspended
        // table's export/recount see only APPLIED rows — its ACKed-but-
        // unapplied WAL backlog would be destroyed on `RESUME WAL` if we
        // dropped its partitions. Probe failure = cannot prove ANY table is
        // safe → skip the ENTIRE run (fail-closed; next daily run retries).
        let suspended: Vec<String> = match self.fetch_wal_suspended_tables().await {
            Ok(names) => names,
            Err(err) => {
                error!(
                    ?err,
                    code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                    "partition archive: wal_tables() probe failed — SKIPPING the whole \
                     run (cannot prove tables are not WAL-suspended; fail-closed)"
                );
                return summary;
            }
        };
        for table in &suspended {
            warn!(
                table,
                "partition archive: table is WAL-SUSPENDED — skipped this run \
                 (see WAL-SUSPEND-01 runbook; resume WAL, then the next run archives it)"
            );
        }

        // Build the worklist: (table, partition) pairs older than each
        // table's class hot window, active partition excluded, hostile names
        // rejected — the SAME selection primitives the detach path uses.
        let mut worklist: Vec<(&'static str, String)> = Vec::new();
        for table in swept_tables() {
            if RETENTION_EXEMPT_TABLES.contains(&table) {
                continue;
            }
            if suspended.iter().any(|s| s == table) {
                continue; // WAL-suspended — warned above, skipped table-wide
            }
            summary.tables_scanned = summary.tables_scanned.saturating_add(1);
            let hot_days = hot_window_days(table, &self.cfg);
            match self.list_eligible_partitions(table, hot_days).await {
                Ok(names) => {
                    for name in names {
                        worklist.push((table, name));
                    }
                }
                Err(err) => {
                    // Counted so the cycle summary can report it: a list
                    // failure silently removes the table from this run, and
                    // this log line is invisible at production level.
                    summary.tables_list_failed = summary.tables_list_failed.saturating_add(1);
                    debug!(
                        ?err,
                        table, "partition list failed (table may not exist yet)"
                    );
                }
            }
        }

        // Oldest first WITHIN each table (so an interrupted run makes
        // monotonic progress per table), then FAIR-SHARED across tables so a
        // table with an older backlog cannot starve a newer one out of the
        // per-run budget entirely. See `fair_share_worklist` for the measured
        // incident this replaces.
        let worklist = fair_share_worklist(worklist, self.cfg.max_partitions_per_run as usize);
        summary.partitions_considered = worklist.len() as u32;

        for (table, partition) in worklist {
            self.process_one(table, &partition, &mut summary).await;
        }

        // Best-effort final audit flush (AUDIT-WS-01 class — never gates).
        if let Err(err) = self.audit.flush() {
            error!(
                ?err,
                code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                "partition_archive_audit flush failed — forensic rows for this run \
                 are missing until QuestDB recovers (archive/drop outcomes unaffected)"
            );
        }

        info!(
            tables_scanned = summary.tables_scanned,
            tables_list_failed = summary.tables_list_failed,
            partitions_considered = summary.partitions_considered,
            verified = summary.verified,
            dropped = summary.dropped,
            failed = summary.failed,
            rows_archived = summary.rows_archived,
            gzip_bytes_uploaded = summary.gzip_bytes_uploaded,
            csv_bytes_exported = summary.csv_bytes_exported,
            bucket = %self.bucket,
            "partition archive cycle complete (archive→verify→drop)"
        );

        summary
    }

    /// One partition: export → upload → verify → audit-first → drop.
    async fn process_one(
        &mut self,
        table: &'static str,
        partition: &str,
        summary: &mut ArchiveRunSummary,
    ) {
        let Some((start, end)) = partition_range_bounds(partition) else {
            // Unreachable in practice (names pass the allowlist upstream);
            // fail-closed anyway.
            self.record_failure(
                table,
                partition,
                ArchiveOutcome::ExportFailed,
                "unparseable partition name",
                summary,
            );
            return;
        };
        let mut s3_key = canonical_archive_key(table, partition);
        // Did THIS run put the object there, or did it reuse an existing one?
        // The 4b mismatch arm below needs to tell those apart: deleting an
        // object we did not write could destroy the only copy of already-
        // dropped data, which is exactly what the never-overwrite policy
        // exists to prevent.
        let mut uploaded_this_run = false;

        // 1. Export (stream + gzip + count) — one partition on disk at a time.
        let exported = match self
            .export_partition_csv(table, partition, &start, &end)
            .await
        {
            Ok(e) => e,
            Err(err) => {
                self.record_failure(
                    table,
                    partition,
                    ArchiveOutcome::ExportFailed,
                    &format!("{err:#}"),
                    summary,
                );
                return;
            }
        };
        summary.csv_bytes_exported = summary
            .csv_bytes_exported
            .saturating_add(exported.csv_bytes);

        // 2. Never-overwrite upload (F1): HeadObject FIRST. Reuse requires
        //    CONTENT IDENTITY (review round 2): equal length (fast
        //    pre-filter) AND a stored SHA-256 checksum equal to the freshly
        //    exported gzip's digest — a stale object from a failed prior run
        //    plus a mutated partition (backfill into a >hot-window
        //    partition) with a coincidental compressed length can no longer
        //    slip through to a drop. Length or checksum mismatch, OR an
        //    object with NO checksum attribute (foreign/legacy) → LOUD
        //    conflict, keep the partition (the object may be the only copy
        //    of already-dropped data). Absent → conditional PutObject with
        //    `If-None-Match: "*"` + `checksum_sha256`.
        match self.head_object_meta(&s3_key).await {
            Some(meta)
                if meta.len == exported.gzip_bytes
                    && meta.checksum_sha256_b64.as_deref()
                        == Some(exported.gzip_sha256_b64.as_str()) =>
            {
                info!(
                    table,
                    partition,
                    s3_key = %s3_key,
                    len = meta.len,
                    sha256 = %exported.gzip_sha256_hex,
                    "identical S3 archive already present (length + SHA-256 match) — \
                     reusing it (no re-upload)"
                );
            }
            Some(meta) => {
                // The object at the canonical key does NOT match what this
                // partition holds now. Before 2026-08-25 that ended the
                // attempt: keep the partition, page nobody, retry forever.
                //
                // That policy deadlocked retention completely. Measured on
                // the live box 2026-08-25: `partition_archive_audit` held
                // 1,000 rows, EVERY one `s3_conflict`, ZERO successes ever
                // recorded; the oldest surviving tick partition was
                // 2026-06-02 against a ONE-DAY hot window, and the volume
                // was 86% full and projected to fill within three sessions.
                //
                // The cause is not a bug in the never-overwrite rule — that
                // rule is right. It is that a partition can legitimately
                // CHANGE after it was archived: `ticks` is PARTITION BY HOUR
                // on the exchange last-trade time, and 10.0% of live ticks
                // arrive with a timestamp more than an hour behind their
                // receive time (6,433,267 of 64,349,753 on 2026-08-24). Those
                // land in an already-archived hourly partition, so the next
                // export can never reproduce the archived bytes again, and
                // the partition is pinned to disk for the life of the box.
                //
                // Resolution: keep the never-overwrite guarantee EXACTLY as
                // it is — the existing object is never touched, never
                // deleted, never PUT over — and write the current content to
                // a CONTENT-ADDRESSED sidecar key instead. The key embeds the
                // full SHA-256 of the gzip, so it is:
                //   * collision-free  — a different digest is a different key;
                //   * idempotent      — a re-run of an unchanged partition
                //                       computes the same key and reuses it
                //                       rather than uploading twice;
                //   * complete        — it holds the partition as it stands
                //                       NOW, which is what the drop removes.
                // Both copies survive. The drop below then verifies against
                // whichever key this run actually used.
                let sidecar_key = sidecar_archive_key(table, partition, &exported.gzip_sha256_hex);
                let reason = if meta.len != exported.gzip_bytes {
                    format!(
                        "existing S3 object length {} != local gzip {}",
                        meta.len, exported.gzip_bytes
                    )
                } else if meta.checksum_sha256_b64.is_none() {
                    "existing S3 object has NO sha256 checksum attribute \
                     (foreign/legacy object)"
                        .to_string()
                } else {
                    format!(
                        "existing S3 object sha256 {} != local gzip sha256 {}",
                        meta.checksum_sha256_b64.as_deref().unwrap_or(""),
                        exported.gzip_sha256_b64
                    )
                };
                match self.head_object_meta(&sidecar_key).await {
                    Some(side)
                        if side.len == exported.gzip_bytes
                            && side.checksum_sha256_b64.as_deref()
                                == Some(exported.gzip_sha256_b64.as_str()) =>
                    {
                        // A previous run already archived exactly this
                        // content under the sidecar key. Reuse it — this is
                        // the idempotent path, not a second upload.
                        metrics::counter!("tv_partition_archive_sidecar_reused_total").increment(1);
                        info!(
                            table,
                            partition,
                            s3_key = %sidecar_key,
                            reason = %reason,
                            "canonical archive key differs from the current partition \
                             (late-arriving rows changed it); identical sidecar archive \
                             already present — reusing it, canonical key untouched"
                        );
                        s3_key = sidecar_key;
                    }
                    Some(_) => {
                        // The sidecar key exists with DIFFERENT content. Two
                        // distinct gzips cannot share a full SHA-256, so this
                        // is a foreign object squatting the key rather than
                        // one of ours. Refuse exactly as before.
                        self.record_failure(
                            table,
                            partition,
                            ArchiveOutcome::S3Conflict,
                            &format!(
                                "{reason}; and the content-addressed sidecar key is \
                                 occupied by an object with a DIFFERENT digest — \
                                 neither key overwritten; operator must inspect both"
                            ),
                            summary,
                        );
                        remove_temp_file(&exported.path);
                        return;
                    }
                    None => {
                        uploaded_this_run = true;
                        if let Err(err) = self
                            .upload_to_s3(&exported.path, &sidecar_key, &exported.gzip_sha256_b64)
                            .await
                        {
                            self.record_failure(
                                table,
                                partition,
                                ArchiveOutcome::UploadFailed,
                                &format!("{err:#}"),
                                summary,
                            );
                            remove_temp_file(&exported.path);
                            return;
                        }
                        summary.gzip_bytes_uploaded = summary
                            .gzip_bytes_uploaded
                            .saturating_add(exported.gzip_bytes);
                        metrics::counter!("tv_partition_archived_total").increment(1);
                        // Counted separately from a clean first archive: a
                        // rising sidecar rate means partitions are being
                        // re-dirtied after archival, which is the signal that
                        // the hot window is shorter than the feed's real
                        // out-of-order lateness.
                        metrics::counter!("tv_partition_archive_sidecar_written_total")
                            .increment(1);
                        warn!(
                            code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                            table,
                            partition,
                            s3_key = %sidecar_key,
                            reason = %reason,
                            "canonical archive key differs from the current partition \
                             (late-arriving rows changed it) — archived to a \
                             content-addressed sidecar key; canonical object NEVER \
                             overwritten, both copies retained"
                        );
                        s3_key = sidecar_key;
                    }
                }
            }
            None => {
                uploaded_this_run = true;
                if let Err(err) = self
                    .upload_to_s3(&exported.path, &s3_key, &exported.gzip_sha256_b64)
                    .await
                {
                    self.record_failure(
                        table,
                        partition,
                        ArchiveOutcome::UploadFailed,
                        &format!("{err:#}"),
                        summary,
                    );
                    remove_temp_file(&exported.path);
                    return;
                }
                summary.gzip_bytes_uploaded = summary
                    .gzip_bytes_uploaded
                    .saturating_add(exported.gzip_bytes);
                metrics::counter!("tv_partition_archived_total").increment(1);
            }
        }

        // 3. Verify — recount AFTER the export + S3 head. All fail-closed.
        let recount = self.recount_rows(table, &start, &end).await;
        let s3_len = self.head_object_meta(&s3_key).await.map(|m| m.len);
        let proof = match verify_archive(
            table,
            partition,
            exported.rows,
            exported.csv_bytes,
            exported.gzip_bytes,
            &exported.gzip_sha256_hex,
            recount,
            s3_len,
        ) {
            Ok(proof) => proof,
            Err(failure) => {
                self.record_failure(
                    table,
                    partition,
                    failure.outcome(),
                    &format!("{failure:?}"),
                    summary,
                );
                remove_temp_file(&exported.path);
                return;
            }
        };
        remove_temp_file(&exported.path);
        summary.verified = summary.verified.saturating_add(1);
        metrics::counter!("tv_partition_archive_verified_total").increment(1);

        // 4. Audit-GATED (F2): the `verified` row must be ACK-flushed
        //    (ILP-over-HTTP) BEFORE the drop — a failed flush WITHHOLDS the
        //    drop (the partition is kept; retried next run).
        if let Err(err) = self.append_verified_audit_gated(table, partition, &proof, &s3_key) {
            self.record_failure(
                table,
                partition,
                ArchiveOutcome::AuditFailed,
                &format!("verified-audit flush not ACKed — drop withheld: {err:#}"),
                summary,
            );
            return;
        }

        // 4b. LAST-MOMENT RECOUNT (2026-08-19) — close the write-during-drop
        //     window.
        //
        //     Steps 3 and 5 are not adjacent: the audit flush between them is
        //     an ILP-over-HTTP ACK, a real network round-trip. A row landing
        //     in the partition during that gap is in the DROP but not in the
        //     archive — silent, unrecoverable loss, with nothing anywhere to
        //     say it happened.
        //
        //     This was a theoretical concern while the floor was 2 days: a
        //     48-hour-old partition has no writer. It became REACHABLE the
        //     moment the market-data floor dropped to 1 day, because the
        //     15:31 post-session sweep, WAL-spill replay and the seal-DLQ
        //     drain all write into YESTERDAY's partition — which is exactly
        //     what a 1-day window makes eligible.
        //
        //     Fail-closed: any change at all means KEEP the partition. The
        //     export is stale by definition, so re-exporting next run is the
        //     only correct answer. An UNAVAILABLE recount also refuses — the
        //     same posture `verify_archive` already takes for a missing
        //     count, because "I could not check" must never read as "safe".
        //
        //     This narrows the window to the single round-trip between this
        //     check and the DROP statement; it cannot close it entirely
        //     without a table lock QuestDB does not offer. What it converts
        //     is the FAILURE MODE: silent loss becomes a counted, coded
        //     refusal that retries.
        match self.recount_rows(table, &start, &end).await {
            Some(rows) if rows == proof.rows => {}
            other => {
                let detail = match other {
                    Some(rows) => format!(
                        "row count CHANGED between verify and drop: verified \
                         {} rows, now {rows} — a writer touched this partition \
                         (15:31 sweep / WAL replay / DLQ drain). The S3 copy is \
                         stale, so the partition is KEPT, the stale object is \
                         removed, and it is re-exported next run",
                        proof.rows
                    ),
                    None => format!(
                        "pre-drop recount UNAVAILABLE (verified {} rows) — \
                         cannot prove the partition is unchanged, so the drop is \
                         withheld and retried next run",
                        proof.rows
                    ),
                };
                metrics::counter!("tv_partition_drop_withheld_total").increment(1);
                // CLEAR THE STALE OBJECT so the next run is not deadlocked
                // (2026-08-19, found by the adversarial audit — the comment
                // above said "re-exported next run", and that was FALSE).
                //
                // The partition changed, so the object we just uploaded no
                // longer matches it. On the next run the export produces
                // DIFFERENT bytes, the never-overwrite policy compares
                // SHA-256, sees a mismatch, and raises S3Conflict — "keep the
                // partition, never overwrite". That repeats forever: the
                // partition is never dropped and the disk grows without
                // bound. A safety check that silently converts into a
                // permanent leak is worse than the race it closed.
                //
                // Deleting is safe ONLY for an object this run uploaded. If
                // the object was REUSED, some earlier run wrote it and its
                // partition may already have been dropped — deleting it could
                // destroy the only surviving copy of that data, which is the
                // precise scenario the never-overwrite policy protects. So a
                // reused object is left alone and the operator is told, which
                // is the honest end of the fail-closed path rather than a
                // silent one.
                if uploaded_this_run && let Err(err) = self.delete_s3_object(&s3_key).await {
                    warn!(
                            table,
                            partition,
                            s3_key = %s3_key,
                            error = %format!("{err:#}"),
                            "could not remove the stale archive object after a \
                             pre-drop mismatch — the next run will raise \
                             S3Conflict on this key until it is removed"
                    );
                }
                self.record_failure(
                    table,
                    partition,
                    ArchiveOutcome::CountMismatch,
                    &detail,
                    summary,
                );
                return;
            }
        }

        // 5. Drop — the ONLY destructive step, gated on the type-state proof.
        match self.drop_partition(&proof).await {
            Ok(()) => {
                summary.dropped = summary.dropped.saturating_add(1);
                summary.rows_archived = summary.rows_archived.saturating_add(proof.rows);
                metrics::counter!("tv_partition_dropped_total").increment(1);
                self.append_audit(
                    table,
                    partition,
                    ArchiveOutcome::Dropped,
                    &proof,
                    &s3_key,
                    "",
                );
                info!(
                    table,
                    partition,
                    rows = proof.rows,
                    gzip_bytes = proof.gzip_bytes,
                    s3_key = %s3_key,
                    "partition archived to S3 (verified) and dropped — disk freed"
                );
            }
            Err(err) => {
                // S3 copy already verified — safe; retried next run.
                self.append_audit(
                    table,
                    partition,
                    ArchiveOutcome::DropFailed,
                    &proof,
                    &s3_key,
                    &format!("{err:#}"),
                );
                summary.failed = summary.failed.saturating_add(1);
                metrics::counter!("tv_partition_archive_failed_total", "stage" => "drop")
                    .increment(1);
                error!(
                    ?err,
                    code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                    table,
                    partition,
                    "partition DROP failed AFTER verified S3 archive — partition kept, \
                     retried next run (S3 copy is durable)"
                );
            }
        }
    }

    /// F3 (review round 1): probes `wal_tables()` and returns the
    /// currently-SUSPENDED table names via the SAME by-column-name parser
    /// the WAL-SUSPEND-01 watcher uses. Errors when the probe cannot
    /// produce a usable answer — the caller then skips the ENTIRE run
    /// (fail-closed: no drop without proof the table's WAL is applied).
    async fn fetch_wal_suspended_tables(&self) -> Result<Vec<String>> {
        let response = self
            .ddl_client
            .get(&self.exec_url)
            .query(&[("query", WAL_TABLES_PROBE_SQL)])
            .send()
            .await
            .context("wal_tables() probe request failed")?;
        if !response.status().is_success() {
            anyhow::bail!("wal_tables() probe returned {}", response.status());
        }
        let body = read_body_capped(response).await?;
        let value: serde_json::Value =
            serde_json::from_str(&body).context("wal_tables() probe body is not JSON")?;
        let rows = parse_wal_tables_response(&value)
            .map_err(|f| anyhow::anyhow!("wal_tables() probe parse failed: {}", f.as_str()))?;
        Ok(rows
            .into_iter()
            .filter(|r| r.suspended)
            .map(|r| r.name)
            .collect()) // O(1) EXEMPT: cold-path once-per-run probe
    }

    /// Lists a table's eligible (aged-out, inactive, well-formed-name)
    /// partitions via the SAME primitives the detach path uses.
    async fn list_eligible_partitions(&self, table: &str, hot_days: u32) -> Result<Vec<String>> {
        let list_sql = build_detach_list_sql(table, hot_days);
        let response = self
            .ddl_client
            .get(&self.exec_url)
            .query(&[("query", &list_sql)])
            .send()
            .await
            .context("partition list query failed")?;
        if !response.status().is_success() {
            // Table may not exist yet — the detach path treats this as
            // empty; mirror it (the caller logs at debug).
            anyhow::bail!("partition list returned {}", response.status());
        }
        let body = read_body_capped(response)
            .await
            .context("failed to read partition list response")?;
        let rows = parse_partition_rows(&body);
        Ok(select_partitions_to_detach(&rows))
    }

    /// Streams the partition's `/exp` CSV through gzip into the temp dir,
    /// counting rows from the RAW byte stream. Cold path — the small
    /// blocking encoder writes are bounded per chunk and the task runs
    /// post-market on a multi-thread runtime.
    async fn export_partition_csv(
        &self,
        table: &str,
        partition: &str,
        start: &str,
        end: &str,
    ) -> Result<ExportedCsv> {
        std::fs::create_dir_all(&self.temp_dir).context("create archive temp dir")?;
        let path = self.temp_dir.join(format!("{table}-{partition}.csv.gz"));
        let sql = build_export_sql(table, start, end);

        let mut response = self
            .export_client
            .get(&self.exp_url)
            .query(&[("query", sql.as_str())])
            .send()
            .await
            .context("/exp export request failed")?;
        if !response.status().is_success() {
            let status = response.status();
            let body =
                capture_rest_error_body(&read_body_capped(response).await.unwrap_or_default());
            anyhow::bail!("/exp export returned {status}: {body}");
        }

        let file = std::fs::File::create(&path).context("create archive temp file")?;
        // Review round 2: the compressed stream is SHA-256-hashed as it is
        // written — the digest is the content identity for the S3
        // reuse/conflict decision and travels on the conditional PutObject.
        let mut encoder = GzEncoder::new(
            Sha256Writer::new(std::io::BufWriter::new(file)),
            Compression::default(),
        );
        // F5 (review round 1): quote-aware RECORD counting — a raw `\n`
        // count over-counts on STRING fields carrying embedded newlines.
        let mut counter = CsvRecordCounter::default();
        let mut csv_bytes: u64 = 0;
        while let Some(chunk) = response.chunk().await.context("/exp stream read failed")? {
            counter.update(&chunk);
            csv_bytes = csv_bytes.saturating_add(chunk.len() as u64);
            encoder.write_all(&chunk).context("gzip write failed")?;
        }
        let hashing_writer = encoder.finish().context("gzip finish failed")?;
        let (mut writer, digest) = hashing_writer.finish();
        writer.flush().context("temp file flush failed")?;
        drop(writer);
        let gzip_sha256_b64 = base64_encode(&digest);
        let gzip_sha256_hex = hex_encode(&digest);

        let records = counter.records();
        if records == 0 {
            // Not even the CSV header arrived — an empty body is an export
            // failure, never "0 rows" (audit Rule 11: no false-OK).
            remove_temp_file(&path);
            anyhow::bail!("/exp export returned an empty body (no CSV header)");
        }
        let gzip_bytes = std::fs::metadata(&path)
            .context("stat archive temp file")?
            .len();
        Ok(ExportedCsv {
            // records − 1 header record = data rows (QuestDB /exp always
            // emits the header record first).
            rows: records.saturating_sub(1),
            csv_bytes,
            gzip_bytes,
            gzip_sha256_b64,
            gzip_sha256_hex,
            path,
        })
    }

    /// Uploads the gzip'd export to the cold bucket — CONDITIONAL create
    /// (`If-None-Match: "*"`, review round 1 F1): only called after the
    /// HeadObject pre-check saw no object, and the condition makes even a
    /// lost race (concurrent writer landing between head and put) a loud
    /// 412 failure instead of a silent overwrite. The precomputed SHA-256
    /// (review round 2) rides as `x-amz-checksum-sha256`: S3 validates the
    /// received body against it server-side (a corrupted upload is REJECTED,
    /// never stored), and the stored attribute is the content identity a
    /// later run's reuse decision requires.
    async fn upload_to_s3(&self, path: &Path, key: &str, sha256_b64: &str) -> Result<()> {
        let body = aws_sdk_s3::primitives::ByteStream::from_path(path)
            .await
            .context("open temp file for S3 upload")?;
        self.s3
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .if_none_match("*")
            .checksum_sha256(sha256_b64)
            .body(body)
            .send()
            .await
            .context("S3 conditional PutObject (If-None-Match + checksum_sha256) failed")?;
        Ok(())
    }

    /// Removes an archive object this run uploaded, after a pre-drop mismatch.
    ///
    /// Called ONLY for an object this run wrote (`uploaded_this_run`). Never
    /// for a reused one: an object an earlier run uploaded may be the only
    /// surviving copy of an already-dropped partition, and deleting it would
    /// destroy exactly what the never-overwrite policy exists to protect.
    ///
    /// Best-effort by design. A failure here leaves the stale object in place,
    /// which the next run reports as `S3Conflict` — visible and diagnosable,
    /// never silent.
    async fn delete_s3_object(&self, key: &str) -> Result<()> {
        self.s3
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .context("S3 DeleteObject for a stale post-mismatch archive object")?;
        Ok(())
    }

    /// Post-export recount over the identical range (verify leg (a)).
    /// `None` = unavailable → verify fails → partition kept.
    async fn recount_rows(&self, table: &str, start: &str, end: &str) -> Option<u64> {
        let sql = build_count_sql(table, start, end);
        let response = self
            .ddl_client
            .get(&self.exec_url)
            .query(&[("query", sql.as_str())])
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body = read_body_capped(response).await.ok()?;
        parse_count_response(&body)
    }

    /// S3 `HeadObject` metadata: ContentLength (verify leg (b) + the reuse
    /// pre-filter) and the stored SHA-256 checksum attribute (base64 — the
    /// review-round-2 content identity; requested via
    /// `ChecksumMode::Enabled`, `None` when the object was written without
    /// one, e.g. a foreign/legacy object). `None` overall = object missing.
    async fn head_object_meta(&self, key: &str) -> Option<S3ObjectMeta> {
        let head = self
            .s3
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .checksum_mode(aws_sdk_s3::types::ChecksumMode::Enabled)
            .send()
            .await
            .ok()?;
        let len = head.content_length().and_then(|n| u64::try_from(n).ok())?;
        Some(S3ObjectMeta {
            len,
            checksum_sha256_b64: head
                .checksum_sha256()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string),
        })
    }

    /// The ONLY destructive step — requires the [`VerifiedArchive`] proof,
    /// which is constructible ONLY by [`verify_archive`] (compile-time
    /// archive-verify-before-drop guarantee).
    async fn drop_partition(&self, proof: &VerifiedArchive) -> Result<()> {
        let drop_sql = build_drop_sql(proof.table(), proof.partition());
        let response = self
            .ddl_client
            .get(&self.exec_url)
            .query(&[("query", drop_sql.as_str())])
            .send()
            .await
            .context("DROP PARTITION request failed")?;
        if !response.status().is_success() {
            let status = response.status();
            let body =
                capture_rest_error_body(&read_body_capped(response).await.unwrap_or_default());
            anyhow::bail!("DROP PARTITION returned {status}: {body}");
        }
        Ok(())
    }

    /// Records one failed attempt: counter + audit row + STORAGE-GAP-04.
    fn record_failure(
        &mut self,
        table: &str,
        partition: &str,
        outcome: ArchiveOutcome,
        detail: &str,
        summary: &mut ArchiveRunSummary,
    ) {
        summary.failed = summary.failed.saturating_add(1);
        let stage: &'static str = match outcome {
            ArchiveOutcome::ExportFailed => "export",
            ArchiveOutcome::CountMismatch => "count",
            ArchiveOutcome::SizeMismatch => "size",
            ArchiveOutcome::UploadFailed => "upload",
            ArchiveOutcome::S3Conflict => "s3_conflict",
            ArchiveOutcome::AuditFailed => "audit",
            ArchiveOutcome::DropFailed => "drop",
            ArchiveOutcome::Verified | ArchiveOutcome::Dropped => "unexpected",
        };
        // SEC-LOW1 (review round 1): the detail may embed an HTTP error body
        // — sanitize/redact/truncate before it reaches logs or QuestDB.
        let detail = capture_rest_error_body(detail);
        metrics::counter!("tv_partition_archive_failed_total", "stage" => stage).increment(1);
        error!(
            code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
            table,
            partition,
            outcome = outcome.as_str(),
            detail = %detail,
            "partition archive attempt failed — partition KEPT (fail-closed), \
             retried next run"
        );
        let row = PartitionArchiveAuditRow {
            ts_ist_nanos: now_ist_nanos(),
            table_name: table.to_string(),
            partition_name: partition.to_string(),
            outcome,
            rows_archived: 0,
            csv_bytes: 0,
            gzip_bytes: 0,
            gzip_sha256: String::new(),
            s3_key: String::new(),
            detail,
        };
        let flushed = self
            .audit
            .append_row(&row)
            .and_then(|()| self.audit.flush());
        if let Err(err) = flushed {
            // Best-effort; discard so one rejected row cannot wedge every
            // later flush of the run (shadow-writer poisoned-buffer lesson).
            self.audit.discard_pending();
            warn!(
                ?err,
                table, partition, "archive-audit failure-row write failed (best-effort)"
            );
        }
    }

    /// F2 (review round 1): appends + ACK-flushes the `verified` audit row.
    /// The ILP-over-HTTP flush gives a per-request server ACK, so `Ok(())`
    /// means the row is durably accepted — ONLY then may the caller drop.
    /// On `Err` the pending buffer is discarded (poisoned-row defense) and
    /// the caller withholds the drop (`AuditFailed`, partition kept).
    fn append_verified_audit_gated(
        &mut self,
        table: &str,
        partition: &str,
        proof: &VerifiedArchive,
        s3_key: &str,
    ) -> Result<()> {
        let row = audit_row_from_proof(
            table,
            partition,
            ArchiveOutcome::Verified,
            proof,
            s3_key,
            "",
        );
        let flushed = self
            .audit
            .append_row(&row)
            .and_then(|()| self.audit.flush());
        if flushed.is_err() {
            self.audit.discard_pending();
        }
        flushed
    }

    /// Appends a post-drop (`dropped` / `drop_failed`) audit row + flushes.
    /// Best-effort — the drop already happened (or already failed); the S3
    /// object + partition state remain the ground truth.
    fn append_audit(
        &mut self,
        table: &str,
        partition: &str,
        outcome: ArchiveOutcome,
        proof: &VerifiedArchive,
        s3_key: &str,
        detail: &str,
    ) {
        let row = audit_row_from_proof(table, partition, outcome, proof, s3_key, detail);
        let flushed = self
            .audit
            .append_row(&row)
            .and_then(|()| self.audit.flush());
        if let Err(err) = flushed {
            self.audit.discard_pending();
            error!(
                ?err,
                code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                table,
                partition,
                outcome = outcome.as_str(),
                "partition_archive_audit write failed (best-effort — flow continues; \
                 the S3 object + partition state remain the ground truth)"
            );
        }
    }

    /// Ensures the audit table exists (idempotent, once per run).
    async fn ensure_audit_table(&self) {
        let ddl = partition_archive_audit_create_ddl();
        match self
            .ddl_client
            .get(&self.exec_url)
            .query(&[("query", ddl.as_str())])
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => {
                let status = resp.status();
                error!(
                    %status,
                    code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                    "partition_archive_audit: CREATE TABLE returned non-2xx (audit rows \
                     for this run may be lost; archive flow continues fail-closed)"
                );
            }
            Err(err) => {
                error!(
                    ?err,
                    code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                    "partition_archive_audit: CREATE TABLE request failed"
                );
            }
        }
    }

    /// Clears stale temp files from a previous interrupted run (one file per
    /// in-flight partition; a crash mid-export leaves at most one).
    fn clean_temp_dir(&self) {
        let Ok(entries) = std::fs::read_dir(&self.temp_dir) else {
            return; // dir absent — created lazily at first export
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                remove_temp_file(&path);
            }
        }
    }
}

/// The full swept-table set: HOUR + DAY constants + the 21 live candle
/// tables from the single source of truth.
fn swept_tables() -> Vec<&'static str> {
    let candles = crate::shadow_persistence::candle_table_names();
    let mut tables: Vec<&'static str> = Vec::with_capacity(
        HOUR_PARTITIONED_TABLES.len() + DAY_PARTITIONED_TABLES.len() + candles.len(),
    );
    tables.extend_from_slice(HOUR_PARTITIONED_TABLES);
    tables.extend_from_slice(DAY_PARTITIONED_TABLES);
    tables.extend_from_slice(&candles);
    tables
}

/// Fair-shares the per-run partition budget across tables, oldest-first
/// within each table.
///
/// # The incident this replaces (MEASURED on the prod box, 2026-08-25)
///
/// The previous form sorted the whole worklist by partition name — i.e.
/// globally oldest-first across every table — and then truncated to
/// `max_partitions_per_run`. Partition names sort chronologically, so that is
/// a global date order, and a table whose backlog is OLDER takes every slot
/// before a table whose backlog is NEWER gets one.
///
/// That is not a theoretical ordering nit. On the box that day:
///
/// | table | eligible partitions | oldest |
/// |---|---|---|
/// | `ticks` | 185 | `2026-07-30T09` |
/// | `market_depth` | 4 | `2026-08-24T08` |
///
/// plus a handful of `2026-06-02` candle partitions that fail with
/// `s3_conflict` every run and are re-listed the next run, permanently
/// occupying the very front of the queue. With `max_partitions_per_run = 200`,
/// the June conflicts and the 185 `ticks` entries filled the budget before a
/// single `market_depth` partition was reached — so `market_depth` received
/// **zero** archive attempts, deterministically, every run, forever.
///
/// `market_depth` was 174 GB of the 196 GB QuestDB volume at the time (~17 GB
/// per HOUR-partition), so the one table that could actually relieve disk
/// pressure was the one table structurally guaranteed never to be archived.
/// The disk filled, QuestDB WAL-suspended 14 tables, and a suspended table
/// keeps ACKing ILP writes while discarding them.
///
/// # Why round-robin rather than "biggest first"
///
/// Selecting by bytes-reclaimed would target disk pressure more directly, but
/// it needs a size probe per partition (another QuestDB round trip per
/// candidate) and it can starve small tables indefinitely — the same failure
/// in the other direction. Round-robin needs no extra I/O, cannot starve
/// anything, and is deterministic, which is what makes it testable.
///
/// Within a table the order stays oldest-first, so the monotonic-progress
/// property the previous comment claimed is genuinely preserved: an
/// interrupted run still advances every table from its oldest end.
///
/// `cap == 0` means "no cap" (the config's documented disable value) and
/// returns every item, still grouped and fairly interleaved.
fn fair_share_worklist(
    worklist: Vec<(&'static str, String)>,
    cap: usize,
) -> Vec<(&'static str, String)> {
    // Oldest-first overall first, so each table's queue is oldest-first when
    // the entries are distributed below.
    let mut sorted = worklist;
    sorted.sort_by(|a, b| a.1.cmp(&b.1));

    // Group by table, preserving FIRST-SEEN table order so the output is
    // deterministic (a HashMap iteration order would not be).
    let mut queues: Vec<(&'static str, std::collections::VecDeque<String>)> = Vec::new();
    for (table, partition) in sorted {
        match queues.iter_mut().find(|(t, _)| *t == table) {
            Some((_, q)) => q.push_back(partition),
            None => {
                let mut q = std::collections::VecDeque::new();
                q.push_back(partition);
                queues.push((table, q));
            }
        }
    }

    let total: usize = queues.iter().map(|(_, q)| q.len()).sum();
    let limit = if cap == 0 { total } else { cap.min(total) };

    let mut out: Vec<(&'static str, String)> = Vec::with_capacity(limit);
    // One pass per round, one partition per table per round.
    while out.len() < limit {
        let mut progressed = false;
        for (table, queue) in &mut queues {
            if out.len() >= limit {
                break;
            }
            if let Some(partition) = queue.pop_front() {
                out.push((*table, partition));
                progressed = true;
            }
        }
        if !progressed {
            // Every queue is empty — cannot happen while out.len() < limit,
            // but a bounded loop beats a provably-unreachable infinite one.
            break;
        }
    }
    out
}

/// Builds a proof-backed audit row (verified/dropped/drop_failed classes).
/// The `detail` is sanitized (SEC-LOW1) before it reaches logs/QuestDB.
fn audit_row_from_proof(
    table: &str,
    partition: &str,
    outcome: ArchiveOutcome,
    proof: &VerifiedArchive,
    s3_key: &str,
    detail: &str,
) -> PartitionArchiveAuditRow {
    PartitionArchiveAuditRow {
        ts_ist_nanos: now_ist_nanos(),
        table_name: table.to_string(),
        partition_name: partition.to_string(),
        outcome,
        rows_archived: i64::try_from(proof.rows).unwrap_or(i64::MAX),
        csv_bytes: i64::try_from(proof.csv_bytes).unwrap_or(i64::MAX),
        gzip_bytes: i64::try_from(proof.gzip_bytes).unwrap_or(i64::MAX),
        gzip_sha256: proof.gzip_sha256_hex.clone(),
        s3_key: s3_key.to_string(),
        detail: capture_rest_error_body(detail),
    }
}

/// Reads a response body with a hard byte cap (SEC-LOW2, review round 1) —
/// a hostile / misrouted `/exec` response can never balloon memory. Bytes
/// past [`ARCHIVE_MAX_EXEC_BODY_BYTES`] are dropped: truncation only
/// affects diagnostics, or fails JSON parsing CLOSED (keep, never drop).
async fn read_body_capped(mut response: reqwest::Response) -> Result<String> {
    let mut out: Vec<u8> = Vec::new(); // O(1) EXEMPT: cold-path bounded body read
    while let Some(chunk) = response.chunk().await.context("body read failed")? {
        let remaining = usize::try_from(ARCHIVE_MAX_EXEC_BODY_BYTES)
            .unwrap_or(usize::MAX)
            .saturating_sub(out.len());
        if remaining == 0 {
            break;
        }
        out.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// Best-effort temp-file removal (a leftover is cleaned at next run start).
fn remove_temp_file(path: &Path) {
    if let Err(err) = std::fs::remove_file(path) {
        warn!(?err, path = %path.display(), "archive temp file removal failed");
    }
}

/// Wall-clock now as IST nanos — `Utc::now()` needs the +5:30 offset for
/// QuestDB display (the greeks-pipeline timestamp rule: non-WS timestamps
/// from the system clock ALWAYS add IST_UTC_OFFSET_NANOS).
fn now_ist_nanos() -> i64 {
    chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(0)
        .saturating_add(IST_UTC_OFFSET_NANOS)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod fair_share_tests {
    use super::fair_share_worklist;

    /// Reproduces the prod worklist of 2026-08-25 exactly.
    ///
    /// `ticks` had 185 eligible partitions starting `2026-07-30`;
    /// `market_depth` had 4 starting `2026-08-24`; the cap was 200. Under the
    /// previous global oldest-first sort + truncate, every `ticks` entry sorts
    /// ahead of every `market_depth` entry, so the 200-slot budget was spent
    /// before `market_depth` was reached — zero attempts, every run, forever.
    ///
    /// BITE: replace the call below with
    /// `w.sort_by(|a, b| a.1.cmp(&b.1)); w.truncate(200);` and this fails with
    /// `market_depth scheduled: 0`.
    #[test]
    fn the_heaviest_table_is_not_starved_by_an_older_backlog() {
        let mut worklist: Vec<(&'static str, String)> = Vec::new();
        // The stuck 2026-06-02-class candle partitions: OLDEST of all, they
        // fail with s3_conflict every run and are re-listed the next one, so
        // they permanently occupy the head of a globally date-sorted queue.
        for tf in ["candles_1m", "candles_3m", "candles_5m", "candles_15m"] {
            for day in 1..=5 {
                worklist.push((tf, format!("2026-06-{day:02}")));
            }
        }
        // 185 ticks partitions, all OLDER than any market_depth one.
        for day in 1..=37 {
            for hour in 9..14 {
                worklist.push(("ticks", format!("2026-07-{day:02}T{hour:02}")));
            }
        }
        // 4 market_depth partitions, all NEWER — and the only ones that free
        // meaningful disk (~17 GB per hour-partition vs ticks' far smaller).
        for hour in 8..12 {
            worklist.push(("market_depth", format!("2026-08-24T{hour:02}")));
        }
        assert!(
            worklist.len() > 200,
            "the reproduction must EXCEED the cap, or the truncate is a no-op \
             and this test proves nothing — it did exactly that when first \
             written, and the bite-proof is what caught it"
        );

        let scheduled = fair_share_worklist(worklist, 200);

        let depth = scheduled
            .iter()
            .filter(|(t, _)| *t == "market_depth")
            .count();
        assert_eq!(
            depth, 4,
            "market_depth scheduled: {depth} — an older backlog on another \
             table must never consume the whole per-run budget"
        );
        assert!(scheduled.len() <= 200, "cap must still bound the run");
    }

    #[test]
    fn each_table_is_still_processed_oldest_first() {
        let worklist = vec![
            ("ticks", "2026-08-03T09".to_string()),
            ("ticks", "2026-08-01T09".to_string()),
            ("ticks", "2026-08-02T09".to_string()),
            ("market_depth", "2026-08-24T09".to_string()),
        ];
        let scheduled = fair_share_worklist(worklist, 0);
        let ticks: Vec<&str> = scheduled
            .iter()
            .filter(|(t, _)| *t == "ticks")
            .map(|(_, p)| p.as_str())
            .collect();
        assert_eq!(
            ticks,
            vec!["2026-08-01T09", "2026-08-02T09", "2026-08-03T09"],
            "monotonic per-table progress must survive the fair share"
        );
    }

    #[test]
    fn a_zero_cap_means_no_cap_and_keeps_every_item() {
        let worklist = vec![
            ("ticks", "2026-08-01T09".to_string()),
            ("market_depth", "2026-08-24T09".to_string()),
        ];
        assert_eq!(fair_share_worklist(worklist, 0).len(), 2);
    }

    #[test]
    fn a_cap_smaller_than_the_table_count_still_reaches_more_than_one_table() {
        let worklist = vec![
            ("ticks", "2026-07-01T09".to_string()),
            ("ticks", "2026-07-02T09".to_string()),
            ("market_depth", "2026-08-24T09".to_string()),
        ];
        let scheduled = fair_share_worklist(worklist, 2);
        assert_eq!(scheduled.len(), 2);
        assert!(
            scheduled.iter().any(|(t, _)| *t == "market_depth"),
            "round-robin must hand the second slot to the second table, not \
             to the older table's next partition"
        );
    }

    #[test]
    fn the_output_is_deterministic_across_runs() {
        let build = || {
            vec![
                ("ticks", "2026-07-30T09".to_string()),
                ("market_depth", "2026-08-24T08".to_string()),
                ("candles_1m", "2026-06-02".to_string()),
                ("ticks", "2026-07-30T10".to_string()),
            ]
        };
        assert_eq!(
            fair_share_worklist(build(), 3),
            fair_share_worklist(build(), 3),
            "selection must not depend on hash iteration order"
        );
    }

    #[test]
    fn an_empty_worklist_is_empty_and_does_not_hang() {
        assert!(fair_share_worklist(Vec::new(), 200).is_empty());
        assert!(fair_share_worklist(Vec::new(), 0).is_empty());
    }
}

#[cfg(test)]
mod tests {

    // ---- Content-addressed sidecar archive keys (2026-08-25) -------------
    //
    // These pin the property that unblocked retention on the live box without
    // weakening the never-overwrite guarantee: a changed partition is written
    // to a key DERIVED FROM ITS CONTENT, so it can never land on top of an
    // existing archive.

    const SHA_A: &str = "3b1f2c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f809";
    const SHA_B: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

    #[test]
    fn canonical_archive_key_is_one_stable_key_per_partition() {
        let a = canonical_archive_key("ticks", "2026-07-24T13");
        assert_eq!(a, canonical_archive_key("ticks", "2026-07-24T13"));
        assert!(
            a.ends_with("/ticks/2026-07-24T13.csv.gz"),
            "shape changed: {a}"
        );
        assert_ne!(a, canonical_archive_key("ticks", "2026-07-24T14"));
        assert_ne!(a, canonical_archive_key("market_depth", "2026-07-24T13"));
    }

    #[test]
    fn a_sidecar_key_can_never_collide_with_the_canonical_key() {
        // The whole safety argument rests on this: if these could ever be
        // equal, the sidecar upload would overwrite the object the
        // never-overwrite policy exists to protect.
        let canonical = canonical_archive_key("ticks", "2026-07-24T13");
        for sha in [SHA_A, SHA_B] {
            assert_ne!(
                canonical,
                sidecar_archive_key("ticks", "2026-07-24T13", sha),
                "sidecar key collided with the canonical key"
            );
        }
    }

    #[test]
    fn different_content_yields_different_sidecar_keys() {
        // Two distinct exports of the same partition must never share a key,
        // or the second would destroy the first.
        assert_ne!(
            sidecar_archive_key("ticks", "2026-07-24T13", SHA_A),
            sidecar_archive_key("ticks", "2026-07-24T13", SHA_B)
        );
    }

    #[test]
    fn identical_content_yields_the_same_sidecar_key_so_retries_reuse() {
        // Idempotence is what stops a retry loop uploading a duplicate object
        // on every run for a partition that is no longer changing.
        assert_eq!(
            sidecar_archive_key("ticks", "2026-07-24T13", SHA_A),
            sidecar_archive_key("ticks", "2026-07-24T13", SHA_A)
        );
    }

    #[test]
    fn the_sidecar_key_carries_the_full_digest_not_a_prefix() {
        // A truncated digest reintroduces collision risk on the one path
        // whose entire safety argument is "a different digest is a different
        // key". Pin the full 64 hex characters.
        let key = sidecar_archive_key("ticks", "2026-07-24T13", SHA_A);
        assert!(key.contains(SHA_A), "full digest missing from key: {key}");
        assert!(
            key.ends_with(&format!(".sha-{SHA_A}.csv.gz")),
            "unexpected sidecar shape: {key}"
        );
    }

    #[test]
    fn the_conflict_arm_never_uploads_to_the_canonical_key() {
        // Source-scan ratchet. The mismatch arm must resolve to the sidecar
        // key and nothing else — if a future edit points its upload back at
        // `s3_key` while the canonical object is still present, the
        // never-overwrite guarantee is silently gone and no unit test that
        // does not read the source would notice.
        let src = include_str!("partition_archive.rs");
        let arm = src
            .split("Some(meta) => {")
            .nth(1)
            .expect("the S3 mismatch arm must exist");
        let arm = arm
            .split("\n            None => {")
            .next()
            .expect("the mismatch arm must be followed by the absent arm");
        assert!(
            arm.contains("sidecar_archive_key("),
            "the mismatch arm must derive a content-addressed key"
        );
        assert!(
            arm.contains("&sidecar_key,"),
            "the mismatch arm must upload to the sidecar key"
        );
        assert!(
            !arm.contains("&s3_key,"),
            "the mismatch arm must NEVER upload to the canonical key"
        );
    }

    // -----------------------------------------------------------------
    // INTRADAY class (2026-08-19) — ticks + second-level candles are
    // current-day; minute-level and above are the history that indicators
    // and strategies read and keep the 35-day window.
    // -----------------------------------------------------------------

    #[test]
    fn intraday_class_covers_ticks_and_every_second_level_candle() {
        // These six are the heaviest tables on the box after depth. Before
        // the split they inherited `market_data_hot_days`, which at the
        // 25,000-instrument target commits several hundred GB on a 200 GB
        // root — and a full disk stops EVERY table, not just these.
        for table in [
            "ticks",
            "candles_1s",
            "candles_2s",
            "candles_14s",
            "candles_30s",
        ] {
            assert_eq!(
                retention_class(table),
                RetentionClass::Intraday,
                "{table} must be current-day, not on the 35-day market-data window"
            );
        }
    }

    #[test]
    fn minute_and_above_candles_stay_on_the_market_data_window() {
        // The other half, and the one that would silently destroy the
        // operator's stated requirement if it regressed: minute-level history
        // is what indicators and strategies read. Sweeping it at 2 days would
        // leave them with nothing to warm up from.
        for table in ["candles_1m", "candles_5m", "candles_15m", "candles_1d"] {
            assert_eq!(
                retention_class(table),
                RetentionClass::MarketData,
                "{table} is HISTORY — it must keep the market-data window"
            );
        }
    }

    #[test]
    fn intraday_class_covers_every_sub_minute_timeframe() {
        // Pins the literal list against the timeframe enum in the ONE
        // direction that matters: a new second-level timeframe added to
        // `TfIndex` without being added to `INTRADAY_TABLES` would silently
        // inherit the 35-day window. The list stays literal on purpose (the
        // class boundary is a retention decision, not an enum property), so
        // this test is what keeps the two honest.
        let second_level: Vec<&str> = crate::shadow_persistence::candle_table_names()
            .into_iter()
            .filter(|t| t.ends_with('s'))
            .collect();
        assert!(
            !second_level.is_empty(),
            "no second-level candle tables found — the filter has drifted and \
             this guard is vacuous"
        );
        for table in second_level {
            assert_eq!(
                retention_class(table),
                RetentionClass::Intraday,
                "{table} is sub-minute but did not classify as Intraday"
            );
        }
    }

    #[test]
    fn depth_still_outranks_intraday_and_market_data() {
        // Ordering matters: market_depth is hour-partitioned like ticks, so a
        // reordering that tested Intraday or MarketData first would move the
        // single heaviest table onto a longer window.
        assert_eq!(
            retention_class(crate::depth_persistence::MARKET_DEPTH_TABLE),
            RetentionClass::Depth
        );
    }

    #[test]
    fn current_day_classes_floor_at_one_day_not_two() {
        // The operator correction, 2026-08-19: "why two days bro ... nowhere
        // for the previous day it will never ever have any depths". At a
        // 2-day floor yesterday's depth was retained; at 1 it is dropped.
        let mut c = cfg(90, 35);
        c.depth_hot_days = 1;
        c.intraday_hot_days = 1;
        assert_eq!(
            hot_window_days(crate::depth_persistence::MARKET_DEPTH_TABLE, &c),
            1,
            "depth must be able to express current-day-only"
        );
        assert_eq!(hot_window_days("ticks", &c), 1);
        assert_eq!(hot_window_days("candles_1s", &c), 1);
    }

    #[test]
    fn zero_is_still_forbidden_on_every_class() {
        // The ONE genuinely dangerous value, and the reason a floor exists at
        // all: at 0 the cutoff becomes `now()`, which matches the partition
        // being actively written. No class may reach it from config.
        let mut c = cfg(0, 0);
        c.depth_hot_days = 0;
        c.intraday_hot_days = 0;
        for table in [
            crate::depth_persistence::MARKET_DEPTH_TABLE,
            "ticks",
            "candles_1s",
            "candles_1m",
            "ws_event_audit",
        ] {
            let days = hot_window_days(table, &c);
            assert!(
                days >= MIN_HOT_DAYS_MARKET_DATA,
                "{table} reached {days} — a 0-day window would select the \
                 partition currently being written"
            );
            assert_ne!(days, 0, "{table} must never resolve to a zero window");
        }
    }

    #[test]
    fn audit_classes_keep_the_two_day_floor() {
        // The other half of the split. Audit tables are NOT tied to market
        // hours and can receive writes at any hour, so their partitions are
        // not reliably stable one day back. They keep the 2-day floor, and
        // lowering the market-data floor must not have moved them.
        let mut c = cfg(1, 1);
        c.depth_hot_days = 1;
        assert_eq!(hot_window_days("ws_event_audit", &c), MIN_HOT_DAYS);
        assert_eq!(hot_window_days("partition_archive_audit", &c), MIN_HOT_DAYS);
        // ...and minute-level history is on the audit-side floor too.
        assert_eq!(hot_window_days("candles_1m", &c), MIN_HOT_DAYS);
    }

    #[test]
    fn the_two_floors_are_ordered_and_neither_is_zero() {
        // Guards the invariant rather than the literals: if someone raises
        // the market-data floor above the general one, or drops either to 0,
        // that is a design change and must fail here first.
        assert!(
            MIN_HOT_DAYS_MARKET_DATA >= 1,
            "a zero floor makes the in-flight partition eligible"
        );
        assert!(
            MIN_HOT_DAYS_MARKET_DATA <= MIN_HOT_DAYS,
            "the current-day classes must not be forced to retain MORE than \
             the audit classes"
        );
        assert_eq!(MIN_HOT_DAYS, 2, "audit floor unchanged");
        assert_eq!(MIN_HOT_DAYS_MARKET_DATA, 1, "current-day floor");
    }

    #[test]
    fn a_one_day_window_can_never_select_todays_partition() {
        // The safety argument stated as a test on the real SQL builder rather
        // than left in prose. `minTimestamp < dateadd('d', -1, now())` is a
        // ROLLING 24h cutoff: today's partition starts at today 00:00, which
        // is always later than now()-24h, so it cannot match. At 0 the cutoff
        // is now() itself and it CAN match — which is what the floor stops.
        let one = crate::partition_manager::build_detach_list_sql("market_depth", 1);
        assert!(
            one.contains("dateadd('d', -1, now())"),
            "1-day window must build a rolling 24h cutoff, got: {one}"
        );
        let zero = crate::partition_manager::build_detach_list_sql("market_depth", 0);
        assert!(
            zero.contains("dateadd('d', -0, now())"),
            "sanity: the 0 form is what the floor exists to prevent reaching"
        );
    }

    #[test]
    fn intraday_hot_days_clamps_to_the_min_hot_days_floor() {
        // "Current day only" means the floor, because a day-partitioned
        // table's current partition cannot be dropped while rows still land
        // in it. Configuring 0 must not be able to express something unsafe.
        let mut cfg = PartitionRetentionConfig::default();
        cfg.intraday_hot_days = 0;
        assert_eq!(
            hot_window_days("ticks", &cfg),
            MIN_HOT_DAYS_MARKET_DATA,
            "a zero intraday window must clamp to the current-day floor, never to zero"
        );
    }
    use super::*;

    fn cfg(retention_days: u32, market_data_hot_days: u32) -> PartitionRetentionConfig {
        PartitionRetentionConfig {
            retention_days,
            market_data_hot_days,
            depth_hot_days: 3,
            archive_enabled: true,
            archive_bucket: String::new(),
            max_partitions_per_run: 200,
            ..PartitionRetentionConfig::default()
        }
    }

    // ---- depth retention class (2026-08-15) --------------------------------

    #[test]
    fn market_depth_is_its_own_retention_class_not_market_data() {
        // `market_depth` is HOUR-partitioned, so a naive class check that
        // tests the market-data list first would classify it on the 35-day
        // window — 735 GB minimum on a 100 GB root at the measured ~21 GB/day.
        // The depth test must come FIRST, and this proves it does.
        assert_eq!(
            retention_class(crate::depth_persistence::MARKET_DEPTH_TABLE),
            RetentionClass::Depth,
            "market_depth must NOT inherit the 35-day market-data window"
        );
        // Re-blessed 2026-08-19: `ticks` moved to the Intraday class, so the
        // market-data comparator here is now a MINUTE candle — the data that
        // actually still lives on the 35-day window.
        assert_eq!(retention_class("candles_1m"), RetentionClass::MarketData);
    }

    #[test]
    fn depth_hot_window_is_far_shorter_than_the_market_data_window() {
        let c = cfg(90, 35);
        let depth = hot_window_days(crate::depth_persistence::MARKET_DEPTH_TABLE, &c);
        // Re-blessed 2026-08-19: compares against a MINUTE candle. `ticks` is
        // no longer market-data class, and comparing against it would now be
        // comparing two current-day windows — which proves nothing.
        let market = hot_window_days("candles_1m", &c);
        assert!(
            depth < market,
            "depth ({depth}d) must retain a SHORTER hot window than minute \
             candles ({market}d) — it is ~1000x the bytes per instrument-day"
        );
        assert_eq!(depth, 3);
    }

    #[test]
    fn depth_hot_window_respects_the_min_hot_days_floor() {
        // Re-blessed 2026-08-19: depth now floors at MIN_HOT_DAYS_MARKET_DATA
        // (1 day), not the general 2. What this test actually protects is
        // unchanged and is the part that matters — a 0 from config must never
        // reach the sweep, because at 0 the cutoff is `now()` and the
        // partition being written this second becomes eligible. TODAY stays
        // untouchable; YESTERDAY is now correctly dropped, which is the whole
        // point of the operator's current-day-only design.
        let mut c = cfg(90, 35);
        c.depth_hot_days = 0;
        assert_eq!(
            hot_window_days(crate::depth_persistence::MARKET_DEPTH_TABLE, &c),
            MIN_HOT_DAYS_MARKET_DATA
        );
    }

    #[test]
    fn market_depth_is_actually_swept_not_merely_classified() {
        // A retention CLASS on a table nobody sweeps is decoration. The
        // archiver's worklist is built from the partition-manager lists, so
        // membership there is what makes the class bite.
        assert!(
            HOUR_PARTITIONED_TABLES.contains(&crate::depth_persistence::MARKET_DEPTH_TABLE),
            "market_depth must be in a swept-table list or its retention class \
             never runs and the table grows until the disk dies"
        );
    }

    #[test]
    fn every_retention_class_maps_to_a_configured_window() {
        // Exhaustiveness by construction: adding a class without a config
        // knob would not compile in `hot_window_days`, but a class that maps
        // to an EXISTING knob by accident would. This pins that the three
        // classes read three DIFFERENT knobs.
        let c = cfg(90, 35);
        let standard = hot_window_days("ws_event_audit", &c);
        // Re-blessed 2026-08-19 for the fourth class. `ticks` reads the
        // INTRADAY window now; `candles_1m` is the market-data representative.
        let market = hot_window_days("candles_1m", &c);
        let depth = hot_window_days(crate::depth_persistence::MARKET_DEPTH_TABLE, &c);
        let intraday = hot_window_days("ticks", &c);
        assert_eq!(
            (standard, market, depth, intraday),
            (90, 35, 3, MIN_HOT_DAYS)
        );
    }

    // ---- retention-class mapping -------------------------------------------

    #[test]
    fn test_retention_class_ticks_is_intraday() {
        // Renamed and re-blessed 2026-08-19. `ticks` runs ~14 GB/day at the
        // 25,000-instrument target; 35 days of it is ~480 GB on a 200 GB root.
        assert_eq!(retention_class("ticks"), RetentionClass::Intraday);
    }

    #[test]
    fn test_retention_class_every_candle_table_is_intraday_or_market_data() {
        // Re-blessed 2026-08-19: the candle tables now split by bucket length
        // rather than all landing in one class. Every table must still land in
        // exactly one of the two — a candle table falling through to Standard
        // (90 days) would be far worse than either.
        for table in crate::shadow_persistence::candle_table_names() {
            let class = retention_class(table);
            assert!(
                matches!(class, RetentionClass::Intraday | RetentionClass::MarketData),
                "candle table {table} classified as {class:?} — must be \
                 Intraday (sub-minute) or MarketData (minute and above)"
            );
        }
    }

    #[test]
    fn test_retention_class_chain_tables_are_market_data() {
        // 2026-07-16 operator directive: the per-minute chain captures are
        // the heaviest daily volume — they ride the market-data hot window
        // (35d) instead of the 90d Standard class.
        assert_eq!(
            retention_class("rest_option_chain_1m"),
            RetentionClass::MarketData
        );
        assert_eq!(
            retention_class("rest_option_contract_1m"),
            RetentionClass::MarketData
        );
    }

    #[test]
    fn test_retention_class_audit_tables_are_standard() {
        for table in [
            "ws_event_audit",
            "instrument_fetch_audit",
            "prev_day_ohlcv",
            "feed_scoreboard_daily",
            "rest_spot_1m",
            "partition_archive_audit",
        ] {
            assert_eq!(
                retention_class(table),
                RetentionClass::Standard,
                "audit/daily table must be standard class: {table}"
            );
        }
    }

    #[test]
    fn test_hot_window_days_maps_classes_to_config() {
        let c = cfg(90, 14);
        // Re-blessed 2026-08-19: `ticks` reads `intraday_hot_days` now, which
        // the cfg() helper leaves at its default (the MIN_HOT_DAYS floor), NOT
        // the market-data window this line used to assert.
        assert_eq!(hot_window_days("ticks", &c), MIN_HOT_DAYS);
        assert_eq!(hot_window_days("candles_1s", &c), MIN_HOT_DAYS);
        assert_eq!(hot_window_days("candles_1m", &c), 14);
        assert_eq!(hot_window_days("ws_event_audit", &c), 90);
        assert_eq!(hot_window_days("partition_archive_audit", &c), 90);
    }

    // ---- MIN_HOT_DAYS floor ------------------------------------------------

    #[test]
    fn test_min_hot_days_floor_const_is_two() {
        assert_eq!(MIN_HOT_DAYS, 2, "today + yesterday must be untouchable");
    }

    #[test]
    fn test_effective_hot_days_floor_overrides_zero_and_one() {
        // Audit-class floor (2): 0 and 1 both clamp up.
        assert_eq!(effective_hot_days(0, MIN_HOT_DAYS), MIN_HOT_DAYS);
        assert_eq!(effective_hot_days(1, MIN_HOT_DAYS), MIN_HOT_DAYS);
        assert_eq!(effective_hot_days(2, MIN_HOT_DAYS), 2);
        assert_eq!(effective_hot_days(14, MIN_HOT_DAYS), 14);
        assert_eq!(effective_hot_days(90, MIN_HOT_DAYS), 90);
        // Current-day floor (1): 0 clamps up, 1 is honoured as-is — the whole
        // point of the split. Neither floor can ever yield 0.
        assert_eq!(effective_hot_days(0, MIN_HOT_DAYS_MARKET_DATA), 1);
        assert_eq!(effective_hot_days(1, MIN_HOT_DAYS_MARKET_DATA), 1);
        assert_eq!(effective_hot_days(35, MIN_HOT_DAYS_MARKET_DATA), 35);
    }

    #[test]
    fn test_hot_window_days_applies_floor_per_class() {
        // Even a hostile market_data_hot_days = 0 can never select today's
        // or yesterday's partitions.
        let c = cfg(0, 0);
        assert_eq!(hot_window_days("ticks", &c), MIN_HOT_DAYS);
        assert_eq!(hot_window_days("ws_event_audit", &c), MIN_HOT_DAYS);
    }

    #[test]
    fn test_eligibility_boundary_hot_days_minus_one_stays_hot() {
        // The cutoff SQL is `minTimestamp < dateadd('d', -N, now())` — a
        // partition exactly N−1 days old fails the strict `<` and stays hot.
        // Pin the builder emits the effective (clamped) N verbatim so the
        // boundary lives server-side, unchanged from the detach path.
        let n = effective_hot_days(14, MIN_HOT_DAYS);
        let sql = build_detach_list_sql("ticks", n);
        assert!(
            sql.contains("dateadd('d', -14, now())"),
            "cutoff must carry the effective hot window: {sql}"
        );
    }

    // ---- partition_range_bounds --------------------------------------------

    #[test]
    fn test_partition_range_bounds_day() {
        let (start, end) = partition_range_bounds("2026-04-01").expect("day partition");
        assert_eq!(start, "2026-04-01T00:00:00.000000Z");
        assert_eq!(end, "2026-04-02T00:00:00.000000Z");
    }

    #[test]
    fn test_partition_range_bounds_hour() {
        let (start, end) = partition_range_bounds("2026-04-01T09").expect("hour partition");
        assert_eq!(start, "2026-04-01T09:00:00.000000Z");
        assert_eq!(end, "2026-04-01T10:00:00.000000Z");
    }

    #[test]
    fn test_partition_range_bounds_month_rollover() {
        let (start, end) = partition_range_bounds("2026-06-30").expect("month rollover");
        assert_eq!(start, "2026-06-30T00:00:00.000000Z");
        assert_eq!(end, "2026-07-01T00:00:00.000000Z");
    }

    #[test]
    fn test_partition_range_bounds_year_rollover_and_hour_23() {
        let (_, end) = partition_range_bounds("2026-12-31").expect("year rollover");
        assert_eq!(end, "2027-01-01T00:00:00.000000Z");
        let (start, end) = partition_range_bounds("2026-12-31T23").expect("hour 23 rollover");
        assert_eq!(start, "2026-12-31T23:00:00.000000Z");
        assert_eq!(end, "2027-01-01T00:00:00.000000Z");
    }

    #[test]
    fn test_partition_range_bounds_leap_day() {
        let (_, end) = partition_range_bounds("2028-02-29").expect("leap day");
        assert_eq!(end, "2028-03-01T00:00:00.000000Z");
    }

    #[test]
    fn test_partition_range_bounds_rejects_garbage() {
        for bad in [
            "",
            "2026-13-01",          // invalid month
            "2026-04-01T24",       // invalid hour
            "2026-04-01T9",        // wrong width
            "2026-04-01'; DROP--", // injection (allowlist)
            "2026-04-01 09",       // space
            "not-a-date!!",
        ] {
            assert!(partition_range_bounds(bad).is_none(), "must reject {bad:?}");
        }
    }

    // ---- SQL builders (microsecond/int-literal landmine avoided) ----------

    #[test]
    fn test_build_export_sql_uses_explicit_string_range() {
        let (start, end) = partition_range_bounds("2026-04-01").expect("bounds");
        let sql = build_export_sql("ticks", &start, &end);
        assert_eq!(
            sql,
            "SELECT * FROM ticks WHERE ts >= '2026-04-01T00:00:00.000000Z' \
             AND ts < '2026-04-02T00:00:00.000000Z'"
        );
        // Never the rejected/landmine forms.
        assert!(
            !sql.contains("IN (SELECT"),
            "IN (SELECT…) on TIMESTAMP is rejected by QuestDB"
        );
        assert!(
            sql.contains('\''),
            "bounds must be string literals, not integer micros"
        );
    }

    #[test]
    fn test_build_count_sql_matches_export_range_exactly() {
        let (start, end) = partition_range_bounds("2026-04-01T09").expect("bounds");
        let count = build_count_sql("ticks", &start, &end);
        let export = build_export_sql("ticks", &start, &end);
        assert_eq!(
            count.replace("count()", "*"),
            export,
            "recount must cover the IDENTICAL range as the export"
        );
    }

    #[test]
    fn test_build_drop_sql_shape() {
        assert_eq!(
            build_drop_sql("ticks", "2026-04-01T09"),
            "ALTER TABLE ticks DROP PARTITION LIST '2026-04-01T09'"
        );
    }

    #[test]
    fn test_parse_count_response_valid_and_garbage() {
        let json = r#"{"columns":[{"name":"count","type":"LONG"}],"dataset":[[12345]],"count":1}"#;
        assert_eq!(parse_count_response(json), Some(12345));
        assert_eq!(parse_count_response(r#"{"dataset":[[0]]}"#), Some(0));
        assert_eq!(parse_count_response(r#"{"dataset":[[-1]]}"#), None);
        assert_eq!(parse_count_response("not json"), None);
        assert_eq!(parse_count_response(r#"{"dataset":[]}"#), None);
        assert_eq!(parse_count_response(r#"{"dataset":[["x"]]}"#), None);
    }

    // ---- verify decision (every failure combination → keep) ----------------

    #[test]
    fn test_verify_archive_all_checks_pass_constructs_proof() {
        let proof = verify_archive(
            "ticks",
            "2026-04-01T09",
            100,
            5000,
            1200,
            "aabbcc",
            Some(100),
            Some(1200),
        )
        .expect("all checks pass");
        assert_eq!(proof.table(), "ticks");
        assert_eq!(proof.partition(), "2026-04-01T09");
        assert_eq!(proof.rows, 100);
        assert_eq!(proof.csv_bytes, 5000);
        assert_eq!(proof.gzip_bytes, 1200);
        assert_eq!(proof.gzip_sha256_hex, "aabbcc");
    }

    #[test]
    fn test_verify_archive_zero_row_partition_passes() {
        // Header-only export (0 data rows) with recount 0 is a legitimate
        // empty partition — verified and droppable.
        let proof = verify_archive(
            "ticks",
            "2026-04-01T09",
            0,
            20,
            40,
            "d1e2",
            Some(0),
            Some(40),
        )
        .expect("empty ok");
        assert_eq!(proof.rows, 0);
    }

    #[test]
    fn test_verify_archive_recount_unavailable_fails() {
        let err = verify_archive("t", "2026-04-01", 100, 5000, 1200, "d1e2", None, Some(1200))
            .expect_err("recount unavailable must fail");
        assert_eq!(err, VerifyFailure::RecountUnavailable);
        assert_eq!(err.outcome(), ArchiveOutcome::CountMismatch);
    }

    #[test]
    fn test_verify_archive_row_count_mismatch_fails() {
        let err = verify_archive(
            "t",
            "2026-04-01",
            100,
            5000,
            1200,
            "d1e2",
            Some(101),
            Some(1200),
        )
        .expect_err("recount != exported must fail");
        assert_eq!(
            err,
            VerifyFailure::RowCountMismatch {
                exported: 100,
                recount: 101
            }
        );
        assert_eq!(err.outcome(), ArchiveOutcome::CountMismatch);
    }

    #[test]
    fn test_verify_archive_s3_missing_fails() {
        let err = verify_archive("t", "2026-04-01", 100, 5000, 1200, "d1e2", Some(100), None)
            .expect_err("missing S3 object must fail");
        assert_eq!(err, VerifyFailure::S3ObjectMissing);
        assert_eq!(err.outcome(), ArchiveOutcome::SizeMismatch);
    }

    #[test]
    fn test_verify_archive_s3_size_mismatch_fails() {
        let err = verify_archive(
            "t",
            "2026-04-01",
            100,
            5000,
            1200,
            "d1e2",
            Some(100),
            Some(1199),
        )
        .expect_err("size mismatch must fail");
        assert_eq!(
            err,
            VerifyFailure::S3SizeMismatch {
                s3: 1199,
                local: 1200
            }
        );
        assert_eq!(err.outcome(), ArchiveOutcome::SizeMismatch);
    }

    #[test]
    fn test_verify_archive_multiple_failures_fail_on_first_check() {
        // Both legs broken → still Err (order: recount first). Every
        // combination of a failing check yields Err — never a proof.
        let err = verify_archive("t", "2026-04-01", 100, 5000, 1200, "d1e2", Some(99), None)
            .expect_err("multiple failures must fail");
        assert!(matches!(err, VerifyFailure::RowCountMismatch { .. }));
    }

    // ---- gzip line-count equivalence (verify leg (c)) -----------------------

    #[test]
    fn test_gzip_roundtrip_preserves_streamed_line_count() {
        use std::io::Read;
        // The bytes fed to the encoder ARE the file's decompressed content —
        // counting newlines on the stream is byte-equivalent to decompressing
        // and counting. Prove it on a synthetic 3-row CSV.
        let csv = b"ts,security_id\n1,13\n2,25\n3,51\n";
        let streamed_newlines = csv.iter().filter(|&&b| b == b'\n').count();
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(csv).expect("gzip write");
        let compressed = enc.finish().expect("gzip finish");
        let mut dec = flate2::read::GzDecoder::new(compressed.as_slice());
        let mut out = Vec::new();
        dec.read_to_end(&mut out).expect("gzip read");
        assert_eq!(out, csv);
        assert_eq!(
            out.iter().filter(|&&b| b == b'\n').count(),
            streamed_newlines
        );
        // rows = newlines − 1 header
        assert_eq!(streamed_newlines - 1, 3);
    }

    // ---- content identity: base64 / hex / Sha256Writer (review round 2) ----

    #[test]
    fn test_base64_encode_known_vectors() {
        // RFC 4648 vectors + the well-known sha256("abc") digest in the
        // exact wire format S3's x-amz-checksum-sha256 carries.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        use sha2::Digest as _;
        let digest: [u8; 32] = sha2::Sha256::digest(b"abc").into();
        assert_eq!(
            base64_encode(&digest),
            "ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=",
            "sha256(\"abc\") base64 known vector"
        );
        assert_eq!(
            hex_encode(&digest),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "sha256(\"abc\") hex known vector"
        );
    }

    #[test]
    fn test_sha256_writer_digest_matches_oneshot_and_passes_bytes_through() {
        use sha2::Digest as _;
        let payload = b"the compressed archive bytes, split across writes";
        let mut w = Sha256Writer::new(Vec::new());
        w.write_all(&payload[..7]).expect("write 1");
        w.write_all(&payload[7..]).expect("write 2");
        w.flush().expect("flush");
        let (inner, digest) = w.finish();
        assert_eq!(
            inner, payload,
            "every byte must pass through to the inner writer"
        );
        let oneshot: [u8; 32] = sha2::Sha256::digest(payload).into();
        assert_eq!(digest, oneshot, "streamed digest == one-shot digest");
    }

    // ---- bucket / environment resolution ------------------------------------

    #[test]
    fn test_resolve_archive_bucket_explicit_wins() {
        assert_eq!(
            resolve_archive_bucket("my-bucket", Some("prod")).as_deref(),
            Some("my-bucket")
        );
        assert_eq!(
            resolve_archive_bucket("  my-bucket  ", None).as_deref(),
            Some("my-bucket"),
            "explicit config needs no env var"
        );
    }

    #[test]
    fn test_resolve_archive_bucket_derives_cold_bucket_only_with_explicit_env() {
        assert_eq!(
            resolve_archive_bucket("", Some("prod")).as_deref(),
            Some("tv-prod-cold")
        );
        assert_eq!(
            resolve_archive_bucket("   ", Some("staging")).as_deref(),
            Some("tv-staging-cold")
        );
        // F1b fail-closed: no explicit environment → NO bucket → archival
        // skipped. A dev box can never default onto the prod cold bucket.
        assert_eq!(resolve_archive_bucket("", None), None);
        assert_eq!(resolve_archive_bucket("   ", None), None);
    }

    #[test]
    fn test_resolve_environment_from_precedence_and_fail_closed() {
        assert_eq!(
            resolve_environment_from(Some("prod"), None).as_deref(),
            Some("prod")
        );
        assert_eq!(
            resolve_environment_from(Some("staging"), Some("prod")).as_deref(),
            Some("staging")
        );
        assert_eq!(
            resolve_environment_from(None, Some("dev-1")).as_deref(),
            Some("dev-1")
        );
        // F1b: NO default — unset env vars resolve to None (skip archival).
        assert_eq!(resolve_environment_from(None, None), None);
        assert_eq!(resolve_environment_from(Some(""), Some("  ")), None);
        // Bucket-hostile values also resolve None (fail-closed).
        assert_eq!(resolve_environment_from(Some("../etc"), None), None);
        assert_eq!(resolve_environment_from(Some("a b"), None), None);
    }

    // ---- audit table contract -----------------------------------------------

    #[test]
    fn test_archive_audit_ddl_contains_expected_columns() {
        let ddl = partition_archive_audit_create_ddl();
        for col in [
            "ts ",
            "table_name",
            "partition_name",
            "outcome",
            "feed",
            "rows_archived",
            "csv_bytes",
            "gzip_bytes",
            "s3_key",
            "detail",
        ] {
            assert!(ddl.contains(col), "DDL missing column {col:?}: {ddl}");
        }
        assert!(ddl.contains("CREATE TABLE IF NOT EXISTS"));
        assert!(ddl.contains("PARTITION BY DAY"));
        assert!(ddl.contains(&format!(
            "DEDUP UPSERT KEYS({DEDUP_KEY_PARTITION_ARCHIVE_AUDIT})"
        )));
    }

    #[test]
    fn test_archive_audit_dedup_key_has_ts_and_feed() {
        // 2026-04-28 regression rule (`ts` in every audit DEDUP key) +
        // 2026-06-28 feed-in-key override.
        let has_token = |t: &str| {
            DEDUP_KEY_PARTITION_ARCHIVE_AUDIT
                .split([',', ' '])
                .map(str::trim)
                .any(|tok| tok == t)
        };
        assert!(has_token("ts"));
        assert!(has_token("feed"));
        assert!(has_token("table_name"));
        assert!(has_token("partition_name"));
    }

    #[test]
    fn test_archive_outcome_labels_stable() {
        assert_eq!(ArchiveOutcome::Verified.as_str(), "verified");
        assert_eq!(ArchiveOutcome::Dropped.as_str(), "dropped");
        assert_eq!(ArchiveOutcome::ExportFailed.as_str(), "export_failed");
        assert_eq!(ArchiveOutcome::CountMismatch.as_str(), "count_mismatch");
        assert_eq!(ArchiveOutcome::SizeMismatch.as_str(), "size_mismatch");
        assert_eq!(ArchiveOutcome::UploadFailed.as_str(), "upload_failed");
        assert_eq!(ArchiveOutcome::S3Conflict.as_str(), "s3_conflict");
        assert_eq!(ArchiveOutcome::AuditFailed.as_str(), "audit_failed");
        assert_eq!(ArchiveOutcome::DropFailed.as_str(), "drop_failed");
    }

    // ---- F5: quote-aware CSV record counting --------------------------------

    #[test]
    fn test_csv_record_counter_plain_rows_trailing_newline() {
        let mut c = CsvRecordCounter::default();
        c.update(b"ts,detail\n1,a\n2,b\n");
        assert_eq!(c.records(), 3, "header + 2 data records");
    }

    #[test]
    fn test_csv_record_counter_no_trailing_newline_counts_final_record() {
        let mut c = CsvRecordCounter::default();
        c.update(b"ts,detail\n1,a\n2,b");
        assert_eq!(c.records(), 3, "final unterminated record still counts");
    }

    #[test]
    fn test_csv_record_counter_embedded_newline_in_quoted_field() {
        // The F5 bug: a raw \n count would say 4 records here — the quoted
        // detail field carries an embedded newline. True records: 3.
        let body = b"ts,detail\n1,\"line one\nline two\"\n2,plain\n";
        assert_eq!(body.iter().filter(|&&b| b == b'\n').count(), 4);
        let mut c = CsvRecordCounter::default();
        c.update(body);
        assert_eq!(c.records(), 3, "embedded newline must not count");
    }

    #[test]
    fn test_csv_record_counter_escaped_quotes_inside_quoted_field() {
        // RFC-4180 `""` escape: toggles out+in with no newline between —
        // record counting stays correct.
        let body = b"ts,detail\n1,\"say \"\"hi\"\"\nnext\"\n";
        let mut c = CsvRecordCounter::default();
        c.update(body);
        assert_eq!(c.records(), 2, "header + 1 quoted record");
    }

    #[test]
    fn test_csv_record_counter_empty_body_is_zero_and_chunk_split_safe() {
        let c = CsvRecordCounter::default();
        assert_eq!(c.records(), 0, "empty body = 0 records (export failure)");
        // The same bytes split across arbitrary chunk boundaries (mid-quote,
        // mid-record) must count identically to one contiguous feed.
        let body: &[u8] = b"ts,detail\n1,\"a\nb\"\n2,c\n";
        let mut whole = CsvRecordCounter::default();
        whole.update(body);
        for split in 1..body.len() {
            let mut parts = CsvRecordCounter::default();
            parts.update(&body[..split]);
            parts.update(&body[split..]);
            assert_eq!(parts.records(), whole.records(), "split at {split}");
        }
    }

    // ---- F2: ILP-over-HTTP conf + discard_pending ---------------------------

    #[test]
    fn test_audit_ilp_conf_targets_http_port_with_ack_knobs() {
        let cfg = QuestDbConfig {
            host: "tv-questdb".to_string(),
            http_port: 9000,
            ilp_port: 9009,
            pg_port: 8812,
        };
        let conf = partition_archive_audit_ilp_http_conf(&cfg);
        assert_eq!(
            conf,
            "http::addr=tv-questdb:9000;protocol_version=1;retry_timeout=0;request_timeout=5000;"
        );
        assert!(
            !conf.contains("tcp::addr"),
            "ILP TCP is fire-and-forget — the verified-row ACK gate needs HTTP"
        );
    }

    #[test]
    fn test_audit_discard_pending_clears_buffer_for_next_flush() {
        let mut w = PartitionArchiveAuditWriter::for_test();
        let row = PartitionArchiveAuditRow {
            ts_ist_nanos: 1,
            table_name: "ticks".to_string(),
            partition_name: "2026-04-01".to_string(),
            outcome: ArchiveOutcome::AuditFailed,
            rows_archived: 0,
            csv_bytes: 0,
            gzip_bytes: 0,
            gzip_sha256: String::new(),
            s3_key: String::new(),
            detail: String::new(),
        };
        w.append_row(&row).expect("append");
        assert_eq!(w.pending(), 1);
        w.discard_pending();
        assert_eq!(w.pending(), 0);
        assert!(w.flush().is_ok(), "post-discard flush is a clean no-op");
    }

    #[test]
    fn test_archive_audit_row_writes_feed_all_symbol() {
        let mut w = PartitionArchiveAuditWriter::for_test();
        let row = PartitionArchiveAuditRow {
            ts_ist_nanos: 1_770_000_000_000_000_000,
            table_name: "ticks".to_string(),
            partition_name: "2026-04-01T09".to_string(),
            outcome: ArchiveOutcome::Verified,
            rows_archived: 100,
            csv_bytes: 5000,
            gzip_bytes: 1200,
            gzip_sha256: "ab12".to_string(),
            s3_key: "questdb-partitions/ticks/2026-04-01T09.csv.gz".to_string(),
            detail: String::new(),
        };
        w.append_row(&row).expect("append must succeed");
        assert_eq!(w.pending(), 1);
    }

    #[test]
    fn test_archive_audit_flush_disconnected_errors_rows_stay_pending() {
        let mut w = PartitionArchiveAuditWriter::for_test();
        let row = PartitionArchiveAuditRow {
            ts_ist_nanos: 1,
            table_name: "ticks".to_string(),
            partition_name: "2026-04-01".to_string(),
            outcome: ArchiveOutcome::UploadFailed,
            rows_archived: 0,
            csv_bytes: 0,
            gzip_bytes: 0,
            gzip_sha256: String::new(),
            s3_key: String::new(),
            detail: "S3 PutObject failed".to_string(),
        };
        w.append_row(&row).expect("append must succeed");
        let err = w.flush().expect_err("disconnected flush must error");
        assert!(err.to_string().contains("no ILP sender"));
        assert_eq!(w.pending(), 1);
        // Empty flush is a no-op Ok.
        let mut empty = PartitionArchiveAuditWriter::for_test();
        assert!(empty.flush().is_ok());
    }

    // ---- swept-table set + S3 key shape -------------------------------------

    #[test]
    fn test_swept_tables_cover_ticks_candles_and_audits_never_exempt() {
        let tables = swept_tables();
        assert!(tables.contains(&"ticks"));
        assert!(tables.contains(&"candles_1m"));
        assert!(tables.contains(&"ws_event_audit"));
        assert!(tables.contains(&"partition_archive_audit"));
        // Exempt masters are filtered at run time; they must not be in the
        // sweep constants at all (pinned in partition_manager too).
        for exempt in RETENTION_EXEMPT_TABLES {
            assert!(
                !tables.contains(exempt),
                "exempt table must never be swept: {exempt}"
            );
        }
    }

    #[test]
    fn test_archive_constants_shape() {
        assert_eq!(ARCHIVE_S3_PREFIX, "questdb-partitions");
        assert_eq!(ARCHIVE_TEMP_DIR, "data/tmp/partition-archive");
        assert_eq!(PARTITION_ARCHIVE_AUDIT_TABLE, "partition_archive_audit");
        assert_eq!(ARCHIVE_FEED_ALL, "all");
        let key = format!("{ARCHIVE_S3_PREFIX}/ticks/2026-04-01T09.csv.gz");
        assert_eq!(key, "questdb-partitions/ticks/2026-04-01T09.csv.gz");
    }

    #[test]
    fn test_now_ist_nanos_is_ahead_of_utc() {
        // Non-WS system-clock timestamps carry +5:30 for QuestDB display
        // (data-integrity greeks rule).
        let utc = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let ist = now_ist_nanos();
        let delta = ist - utc;
        assert!(
            (IST_UTC_OFFSET_NANOS - 1_000_000_000..=IST_UTC_OFFSET_NANOS + 1_000_000_000)
                .contains(&delta),
            "ist-utc delta must be ~+5:30: {delta}"
        );
    }
}

// ---------------------------------------------------------------------------
// Stub-server integration tests (review round 1) — the FULL
// archive→verify→drop orchestration against local QuestDB (/exec + /exp),
// ILP-over-HTTP, and S3 stubs. No live services: every network edge is a
// 127.0.0.1 TcpListener owned by the test, so the F1 never-overwrite, F2
// audit-gated drop, and F3 WAL gate are exercised END-TO-END.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod stub_integration_tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    /// One parsed stub request (method + decoded target + lowercased headers).
    #[derive(Clone, Debug)]
    struct SeenRequest {
        method: String,
        /// Percent/plus-decoded request target (path + query).
        target: String,
        headers: Vec<(String, String)>,
    }

    impl SeenRequest {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        }
    }

    /// (status, extra headers, body). Extra headers carrying `content-length`
    /// suppress the auto length (HEAD responses advertise the object size
    /// with an empty body).
    type StubResponse = (u16, Vec<(String, String)>, Vec<u8>);
    type Responder = Arc<dyn Fn(&SeenRequest) -> StubResponse + Send + Sync>;

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    /// Minimal `+`/percent decoding so responders can match on plain SQL.
    fn url_decode(s: &str) -> String {
        let bytes = s.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'+' => {
                    out.push(b' ');
                    i += 1;
                }
                b'%' if i + 2 < bytes.len() => {
                    let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                        .ok()
                        .and_then(|h| u8::from_str_radix(h, 16).ok());
                    if let Some(b) = hex {
                        out.push(b);
                        i += 3;
                    } else {
                        out.push(b'%');
                        i += 1;
                    }
                }
                b => {
                    out.push(b);
                    i += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Spawns a minimal HTTP/1.1 keep-alive stub on 127.0.0.1; returns
    /// `(base_url, request_log)`.
    async fn spawn_stub(responder: Responder) -> (String, Arc<Mutex<Vec<SeenRequest>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("stub bind");
        let addr = listener.local_addr().expect("stub addr");
        let log: Arc<Mutex<Vec<SeenRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let accept_log = Arc::clone(&log);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let responder = Arc::clone(&responder);
                let log = Arc::clone(&accept_log);
                tokio::spawn(async move {
                    let mut buf: Vec<u8> = Vec::new();
                    let mut tmp = [0u8; 8192];
                    loop {
                        // Read one full header block.
                        let header_end = loop {
                            if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                                break pos + 4;
                            }
                            match sock.read(&mut tmp).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                            }
                        };
                        let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
                        let mut lines = head.split("\r\n");
                        let mut req_line = lines.next().unwrap_or_default().split(' ');
                        let method = req_line.next().unwrap_or_default().to_string();
                        let raw_target = req_line.next().unwrap_or_default().to_string();
                        let mut headers = Vec::new();
                        let mut content_length = 0usize;
                        for line in lines {
                            if let Some((k, v)) = line.split_once(':') {
                                let k = k.trim().to_ascii_lowercase();
                                let v = v.trim().to_string();
                                if k == "content-length" {
                                    content_length = v.parse().unwrap_or(0);
                                }
                                headers.push((k, v));
                            }
                        }
                        // Consume the body (bytes may already be buffered);
                        // anything past it belongs to the next request.
                        let mut consumed = buf.split_off(header_end);
                        buf.clear();
                        while consumed.len() < content_length {
                            match sock.read(&mut tmp).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => consumed.extend_from_slice(&tmp[..n]),
                            }
                        }
                        buf = consumed.split_off(content_length.min(consumed.len()));
                        let req = SeenRequest {
                            method,
                            target: url_decode(&raw_target),
                            headers,
                        };
                        log.lock().expect("stub log lock").push(req.clone());
                        let (status, extra, body) = responder(&req);
                        let reason = match status {
                            200 => "OK",
                            204 => "No Content",
                            404 => "Not Found",
                            412 => "Precondition Failed",
                            500 => "Internal Server Error",
                            _ => "Stub",
                        };
                        let mut resp = format!("HTTP/1.1 {status} {reason}\r\n");
                        if !extra.iter().any(|(k, _)| k == "content-length") {
                            resp.push_str(&format!("content-length: {}\r\n", body.len()));
                        }
                        for (k, v) in &extra {
                            resp.push_str(&format!("{k}: {v}\r\n"));
                        }
                        resp.push_str("\r\n");
                        if sock.write_all(resp.as_bytes()).await.is_err() {
                            return;
                        }
                        if sock.write_all(&body).await.is_err() {
                            return;
                        }
                        let _ = sock.flush().await;
                    }
                });
            }
        });
        (format!("http://{addr}"), log)
    }

    /// Stateful mini-S3 (path-style `/bucket/key…`): PUT stores the body
    /// length, HEAD answers 200 + content-length or 404.
    /// Stored stub object: (content length, optional sha256 checksum b64).
    type StubObjects = Arc<Mutex<HashMap<String, (usize, Option<String>)>>>;

    fn s3_responder(objects: StubObjects) -> Responder {
        Arc::new(move |req: &SeenRequest| {
            let key = req
                .target
                .split('?')
                .next()
                .unwrap_or_default()
                .trim_start_matches('/')
                .to_string();
            match req.method.as_str() {
                "PUT" => {
                    let len = req
                        .header("content-length")
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(0);
                    let checksum = req.header("x-amz-checksum-sha256").map(ToString::to_string);
                    objects
                        .lock()
                        .expect("s3 lock")
                        .insert(key, (len, checksum));
                    (200, vec![("etag".into(), "\"stub\"".into())], Vec::new())
                }
                "HEAD" => match objects.lock().expect("s3 lock").get(&key) {
                    Some((len, checksum)) => {
                        let mut headers = vec![
                            ("content-length".into(), len.to_string()),
                            ("etag".into(), "\"stub\"".into()),
                        ];
                        if let Some(sum) = checksum {
                            headers.push(("x-amz-checksum-sha256".into(), sum.clone()));
                        }
                        (200, headers, Vec::new())
                    }
                    None => (404, vec![("content-length".into(), "0".into())], Vec::new()),
                },
                _ => (500, Vec::new(), b"unexpected s3 method".to_vec()),
            }
        })
    }

    /// QuestDB `/exec` + `/exp` stub: `ticks` carries the given partitions;
    /// every other table is empty; `count()` returns `count`; `wal_tables()`
    /// reports `wal_suspended` (or fails with `wal_status`).
    fn qdb_responder(
        partitions: Vec<&'static str>,
        count: u64,
        csv_body: &'static str,
        wal_suspended: Vec<&'static str>,
        wal_status: u16,
    ) -> Responder {
        Arc::new(move |req: &SeenRequest| {
            let t = &req.target;
            if t.starts_with("/exp") {
                return (200, Vec::new(), csv_body.as_bytes().to_vec());
            }
            if t.contains("wal_tables()") {
                if wal_status != 200 {
                    return (wal_status, Vec::new(), b"{}".to_vec());
                }
                let rows: Vec<String> = wal_suspended
                    .iter()
                    .map(|n| format!("[\"{n}\",true]"))
                    .collect();
                let body = format!(
                    "{{\"columns\":[{{\"name\":\"name\"}},{{\"name\":\"suspended\"}}],\
                     \"dataset\":[{}]}}",
                    rows.join(",")
                );
                return (200, Vec::new(), body.into_bytes());
            }
            if t.contains("CREATE TABLE") || t.contains("DROP PARTITION") {
                return (200, Vec::new(), b"{\"ddl\":\"OK\"}".to_vec());
            }
            if t.contains("count()") {
                let body =
                    format!("{{\"columns\":[{{\"name\":\"count\"}}],\"dataset\":[[{count}]]}}");
                return (200, Vec::new(), body.into_bytes());
            }
            if t.contains("table_partitions('ticks')") {
                let rows: Vec<String> = partitions
                    .iter()
                    .map(|p| format!("[\"{p}\",false]"))
                    .collect();
                let body = format!(
                    "{{\"columns\":[{{\"name\":\"name\"}},{{\"name\":\"active\"}}],\
                     \"dataset\":[{}]}}",
                    rows.join(",")
                );
                return (200, Vec::new(), body.into_bytes());
            }
            if t.contains("table_partitions(") {
                return (
                    200,
                    Vec::new(),
                    b"{\"columns\":[{\"name\":\"name\"},{\"name\":\"active\"}],\"dataset\":[]}"
                        .to_vec(),
                );
            }
            (500, Vec::new(), b"unexpected qdb query".to_vec())
        })
    }

    /// ILP-over-HTTP stub: fixed status for every flush POST.
    fn ilp_responder(status: u16) -> Responder {
        Arc::new(move |_req: &SeenRequest| (status, Vec::new(), Vec::new()))
    }

    fn test_retention_cfg(max_per_run: u32) -> PartitionRetentionConfig {
        PartitionRetentionConfig {
            retention_days: 90,
            market_data_hot_days: 14,
            depth_hot_days: 3,
            archive_enabled: true,
            archive_bucket: "tv-test-cold".to_string(),
            max_partitions_per_run: max_per_run,
            ..PartitionRetentionConfig::default()
        }
    }

    static TEMP_DIR_SEQ: AtomicU64 = AtomicU64::new(0);

    fn unique_temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "tv-partition-archive-test-{}-{}",
            std::process::id(),
            TEMP_DIR_SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// Deterministic gzip (length, sha256 b64) of a body under the export's
    /// settings (flate2 default level, default header) — pre-seeds the F1
    /// reuse/conflict cases with exactly what the export will produce.
    fn gzip_meta(body: &[u8]) -> (usize, String) {
        use sha2::Digest as _;
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(body).expect("gzip");
        let compressed = enc.finish().expect("finish");
        let digest: [u8; 32] = sha2::Sha256::digest(&compressed).into();
        (compressed.len(), base64_encode(&digest))
    }

    async fn build_archiver(
        qdb_url: &str,
        ilp_url: &str,
        s3_url: &str,
        cfg: PartitionRetentionConfig,
    ) -> PartitionArchiver {
        let creds = aws_sdk_s3::config::Credentials::new("test", "test", None, None, "stub");
        let s3_conf = aws_sdk_s3::config::Builder::new()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new("ap-south-1"))
            .endpoint_url(s3_url)
            .credentials_provider(creds)
            .force_path_style(true)
            // Plain body + Content-Length (no aws-chunked checksum trailers)
            // so the stub's stored length equals the file length.
            .request_checksum_calculation(
                aws_sdk_s3::config::RequestChecksumCalculation::WhenRequired,
            )
            .build();
        let s3 = aws_sdk_s3::Client::from_conf(s3_conf);
        let ilp_addr = ilp_url.trim_start_matches("http://");
        let audit = PartitionArchiveAuditWriter::with_conf(&format!(
            "http::addr={ilp_addr};protocol_version=1;retry_timeout=0;request_timeout=5000;"
        ));
        PartitionArchiver::from_parts(
            format!("{qdb_url}/exec"),
            format!("{qdb_url}/exp"),
            s3,
            "tv-test-cold".to_string(),
            unique_temp_dir(),
            cfg,
            audit,
        )
        .expect("test archiver")
    }

    const CSV: &str = "ts,security_id\n1,13\n2,25\n3,51\n"; // header + 3 rows
    const TICKS_KEY: &str = "tv-test-cold/questdb-partitions/ticks/2026-04-01T09.csv.gz";

    // Multi-thread runtime: the questdb-rs ILP flush is SYNC-blocking;
    // a current_thread runtime would starve the stub server tasks while
    // the flush blocks (deadlock until the request timeout).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_stub_full_flow_verifies_audits_then_drops() {
        let (qdb_url, qdb_log) =
            spawn_stub(qdb_responder(vec!["2026-04-01T09"], 3, CSV, vec![], 200)).await;
        let (ilp_url, ilp_log) = spawn_stub(ilp_responder(204)).await;
        let objects = Arc::new(Mutex::new(HashMap::new()));
        let (s3_url, s3_log) = spawn_stub(s3_responder(Arc::clone(&objects))).await;

        let mut archiver =
            build_archiver(&qdb_url, &ilp_url, &s3_url, test_retention_cfg(200)).await;
        let summary = archiver.archive_and_drop_old_partitions().await;

        assert_eq!(summary.partitions_considered, 1);
        assert_eq!(summary.verified, 1);
        assert_eq!(summary.dropped, 1);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.rows_archived, 3);
        assert!(summary.gzip_bytes_uploaded > 0);

        // F1: HeadObject preceded a CONDITIONAL PutObject.
        let s3_reqs = s3_log.lock().expect("s3 log").clone();
        let head_pos = s3_reqs
            .iter()
            .position(|r| r.method == "HEAD")
            .expect("HEAD before upload");
        let put_pos = s3_reqs
            .iter()
            .position(|r| r.method == "PUT")
            .expect("one PUT");
        assert!(head_pos < put_pos, "HeadObject must precede PutObject");
        assert_eq!(
            s3_reqs[put_pos].header("if-none-match"),
            Some("*"),
            "PutObject must be conditional (If-None-Match: *)"
        );
        let (_, expected_sha) = gzip_meta(CSV.as_bytes());
        assert_eq!(
            s3_reqs[put_pos].header("x-amz-checksum-sha256"),
            Some(expected_sha.as_str()),
            "PutObject must carry the precomputed sha256 (review round 2 — \
             S3 validates the body server-side and stores the content identity)"
        );
        assert!(objects.lock().expect("objects").contains_key(TICKS_KEY));

        // F2: the audit stub ACKed flushes (verified + dropped rows).
        assert!(
            ilp_log
                .lock()
                .expect("ilp log")
                .iter()
                .any(|r| r.method == "POST"),
            "verified/dropped audit rows must flush over ILP-HTTP"
        );

        // The drop fired exactly once, and the WAL gate ran.
        let qdb_reqs = qdb_log.lock().expect("qdb log").clone();
        assert_eq!(
            qdb_reqs
                .iter()
                .filter(|r| r.target.contains("DROP PARTITION"))
                .count(),
            1
        );
        assert!(qdb_reqs.iter().any(|r| r.target.contains("wal_tables()")));
    }

    // Multi-thread runtime: the questdb-rs ILP flush is SYNC-blocking;
    // a current_thread runtime would starve the stub server tasks while
    // the flush blocks (deadlock until the request timeout).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_stub_recount_mismatch_keeps_partition_no_drop() {
        // Recount says 4, export streamed 3 rows → verify fails → keep.
        let (qdb_url, qdb_log) =
            spawn_stub(qdb_responder(vec!["2026-04-01T09"], 4, CSV, vec![], 200)).await;
        let (ilp_url, _ilp_log) = spawn_stub(ilp_responder(204)).await;
        let objects = Arc::new(Mutex::new(HashMap::new()));
        let (s3_url, _s3_log) = spawn_stub(s3_responder(objects)).await;

        let mut archiver =
            build_archiver(&qdb_url, &ilp_url, &s3_url, test_retention_cfg(200)).await;
        let summary = archiver.archive_and_drop_old_partitions().await;

        assert_eq!(summary.dropped, 0);
        assert_eq!(summary.failed, 1);
        assert!(
            !qdb_log
                .lock()
                .expect("qdb log")
                .iter()
                .any(|r| r.target.contains("DROP PARTITION")),
            "a failed verify must never reach the drop"
        );
    }

    // Multi-thread runtime: the questdb-rs ILP flush is SYNC-blocking;
    // a current_thread runtime would starve the stub server tasks while
    // the flush blocks (deadlock until the request timeout).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_stub_f1_conflict_different_length_never_overwrites() {
        let (qdb_url, qdb_log) =
            spawn_stub(qdb_responder(vec!["2026-04-01T09"], 3, CSV, vec![], 200)).await;
        let (ilp_url, _ilp_log) = spawn_stub(ilp_responder(204)).await;
        let objects = Arc::new(Mutex::new(HashMap::new()));
        objects
            .lock()
            .expect("seed")
            .insert(TICKS_KEY.to_string(), (999_999, None));
        let (s3_url, s3_log) = spawn_stub(s3_responder(Arc::clone(&objects))).await;

        let mut archiver =
            build_archiver(&qdb_url, &ilp_url, &s3_url, test_retention_cfg(200)).await;
        let summary = archiver.archive_and_drop_old_partitions().await;

        // CONTRACT CHANGED 2026-08-25. This previously asserted
        // "conflict => keep the partition forever". That policy deadlocked
        // retention on the live box: 1,000 `s3_conflict` audit rows, ZERO
        // successful archives ever recorded, tick data surviving back to
        // 2026-06-02 against a ONE-DAY hot window, and the volume 86% full
        // and projected to fill within three sessions.
        //
        // The cause is legitimate rather than a bug: `ticks` is
        // PARTITION BY HOUR on the exchange last-trade time, and 10.0% of
        // live ticks arrive with a timestamp over an hour behind their
        // receive time, so an already-archived hourly partition keeps
        // gaining rows and can never reproduce the archived bytes again.
        //
        // The guarantee that actually mattered is UNCHANGED and is still
        // asserted below: the pre-existing object is never overwritten.
        // What changed is that the partition's CURRENT content now goes to
        // a content-addressed sidecar key, so both copies survive and the
        // partition can finally be released.
        assert_eq!(
            summary.dropped, 1,
            "a resolvable conflict must release the partition"
        );
        assert_eq!(
            summary.failed, 0,
            "the sidecar path is a success, not a failure"
        );
        let puts: Vec<String> = s3_log
            .lock()
            .expect("s3 log")
            .iter()
            .filter(|r| r.method == "PUT")
            .map(|r| r.target.clone())
            .collect();
        assert_eq!(puts.len(), 1, "exactly one upload, to the sidecar key");
        assert!(
            puts[0].contains(".sha-"),
            "upload must use a content-addressed key, got {}",
            puts[0]
        );
        assert!(
            !puts[0].ends_with("/2026-04-01T09.csv.gz"),
            "upload must NEVER target the canonical key, got {}",
            puts[0]
        );
        assert!(
            qdb_log
                .lock()
                .expect("qdb log")
                .iter()
                .any(|r| r.target.contains("DROP PARTITION")),
            "the partition must now be dropped"
        );
    }

    // Multi-thread runtime: the questdb-rs ILP flush is SYNC-blocking;
    // a current_thread runtime would starve the stub server tasks while
    // the flush blocks (deadlock until the request timeout).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_stub_f1_identical_length_and_checksum_reuses_then_drops() {
        let (qdb_url, qdb_log) =
            spawn_stub(qdb_responder(vec!["2026-04-01T09"], 3, CSV, vec![], 200)).await;
        let (ilp_url, _ilp_log) = spawn_stub(ilp_responder(204)).await;
        let objects = Arc::new(Mutex::new(HashMap::new()));
        let (len, sha_b64) = gzip_meta(CSV.as_bytes());
        objects
            .lock()
            .expect("seed")
            .insert(TICKS_KEY.to_string(), (len, Some(sha_b64)));
        let (s3_url, s3_log) = spawn_stub(s3_responder(Arc::clone(&objects))).await;

        let mut archiver =
            build_archiver(&qdb_url, &ilp_url, &s3_url, test_retention_cfg(200)).await;
        let summary = archiver.archive_and_drop_old_partitions().await;

        assert_eq!(summary.verified, 1);
        assert_eq!(summary.dropped, 1, "reused archive still verifies + drops");
        assert_eq!(
            summary.gzip_bytes_uploaded, 0,
            "nothing re-uploaded on reuse"
        );
        assert!(
            !s3_log
                .lock()
                .expect("s3 log")
                .iter()
                .any(|r| r.method == "PUT"),
            "an identical (length + sha256) existing object is reused, never re-uploaded"
        );
        assert!(
            qdb_log
                .lock()
                .expect("qdb log")
                .iter()
                .any(|r| r.target.contains("DROP PARTITION"))
        );
    }

    // Multi-thread runtime: the questdb-rs ILP flush is SYNC-blocking;
    // a current_thread runtime would starve the stub server tasks while
    // the flush blocks (deadlock until the request timeout).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_stub_r2_same_length_different_checksum_is_conflict() {
        // Review round 2: length equality is only a pre-filter — a stale
        // object with a coincidental compressed length but DIFFERENT content
        // must be a conflict-keep, never a reuse-then-drop.
        let (qdb_url, qdb_log) =
            spawn_stub(qdb_responder(vec!["2026-04-01T09"], 3, CSV, vec![], 200)).await;
        let (ilp_url, _ilp_log) = spawn_stub(ilp_responder(204)).await;
        let objects = Arc::new(Mutex::new(HashMap::new()));
        let (len, _) = gzip_meta(CSV.as_bytes());
        objects.lock().expect("seed").insert(
            TICKS_KEY.to_string(),
            (
                len,
                Some("c3RhbGUgY29udGVudCBkaWdlc3QgISEhISEhISE=".to_string()),
            ),
        );
        let (s3_url, s3_log) = spawn_stub(s3_responder(Arc::clone(&objects))).await;

        let mut archiver =
            build_archiver(&qdb_url, &ilp_url, &s3_url, test_retention_cfg(200)).await;
        let summary = archiver.archive_and_drop_old_partitions().await;

        // CONTRACT CHANGED 2026-08-25. This previously asserted
        // "conflict => keep the partition forever". That policy deadlocked
        // retention on the live box: 1,000 `s3_conflict` audit rows, ZERO
        // successful archives ever recorded, tick data surviving back to
        // 2026-06-02 against a ONE-DAY hot window, and the volume 86% full
        // and projected to fill within three sessions.
        //
        // The cause is legitimate rather than a bug: `ticks` is
        // PARTITION BY HOUR on the exchange last-trade time, and 10.0% of
        // live ticks arrive with a timestamp over an hour behind their
        // receive time, so an already-archived hourly partition keeps
        // gaining rows and can never reproduce the archived bytes again.
        //
        // The guarantee that actually mattered is UNCHANGED and is still
        // asserted below: the pre-existing object is never overwritten.
        // What changed is that the partition's CURRENT content now goes to
        // a content-addressed sidecar key, so both copies survive and the
        // partition can finally be released.
        assert_eq!(
            summary.dropped, 1,
            "a resolvable conflict must release the partition"
        );
        assert_eq!(
            summary.failed, 0,
            "the sidecar path is a success, not a failure"
        );
        let puts: Vec<String> = s3_log
            .lock()
            .expect("s3 log")
            .iter()
            .filter(|r| r.method == "PUT")
            .map(|r| r.target.clone())
            .collect();
        assert_eq!(puts.len(), 1, "exactly one upload, to the sidecar key");
        assert!(
            puts[0].contains(".sha-"),
            "upload must use a content-addressed key, got {}",
            puts[0]
        );
        assert!(
            !puts[0].ends_with("/2026-04-01T09.csv.gz"),
            "upload must NEVER target the canonical key, got {}",
            puts[0]
        );
        assert!(
            qdb_log
                .lock()
                .expect("qdb log")
                .iter()
                .any(|r| r.target.contains("DROP PARTITION")),
            "the partition must now be dropped"
        );
    }

    // Multi-thread runtime: see the note on the first stub test.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_stub_r2_same_length_missing_checksum_is_conflict() {
        // Review round 2: a foreign/legacy object without a sha256 checksum
        // attribute has UNPROVABLE content — conflict-keep, never reuse.
        let (qdb_url, qdb_log) =
            spawn_stub(qdb_responder(vec!["2026-04-01T09"], 3, CSV, vec![], 200)).await;
        let (ilp_url, _ilp_log) = spawn_stub(ilp_responder(204)).await;
        let objects = Arc::new(Mutex::new(HashMap::new()));
        let (len, _) = gzip_meta(CSV.as_bytes());
        objects
            .lock()
            .expect("seed")
            .insert(TICKS_KEY.to_string(), (len, None));
        let (s3_url, s3_log) = spawn_stub(s3_responder(Arc::clone(&objects))).await;

        let mut archiver =
            build_archiver(&qdb_url, &ilp_url, &s3_url, test_retention_cfg(200)).await;
        let summary = archiver.archive_and_drop_old_partitions().await;

        // CONTRACT CHANGED 2026-08-25. This previously asserted
        // "conflict => keep the partition forever". That policy deadlocked
        // retention on the live box: 1,000 `s3_conflict` audit rows, ZERO
        // successful archives ever recorded, tick data surviving back to
        // 2026-06-02 against a ONE-DAY hot window, and the volume 86% full
        // and projected to fill within three sessions.
        //
        // The cause is legitimate rather than a bug: `ticks` is
        // PARTITION BY HOUR on the exchange last-trade time, and 10.0% of
        // live ticks arrive with a timestamp over an hour behind their
        // receive time, so an already-archived hourly partition keeps
        // gaining rows and can never reproduce the archived bytes again.
        //
        // The guarantee that actually mattered is UNCHANGED and is still
        // asserted below: the pre-existing object is never overwritten.
        // What changed is that the partition's CURRENT content now goes to
        // a content-addressed sidecar key, so both copies survive and the
        // partition can finally be released.
        assert_eq!(
            summary.dropped, 1,
            "a resolvable conflict must release the partition"
        );
        assert_eq!(
            summary.failed, 0,
            "the sidecar path is a success, not a failure"
        );
        let puts: Vec<String> = s3_log
            .lock()
            .expect("s3 log")
            .iter()
            .filter(|r| r.method == "PUT")
            .map(|r| r.target.clone())
            .collect();
        assert_eq!(puts.len(), 1, "exactly one upload, to the sidecar key");
        assert!(
            puts[0].contains(".sha-"),
            "upload must use a content-addressed key, got {}",
            puts[0]
        );
        assert!(
            !puts[0].ends_with("/2026-04-01T09.csv.gz"),
            "upload must NEVER target the canonical key, got {}",
            puts[0]
        );
        assert!(
            qdb_log
                .lock()
                .expect("qdb log")
                .iter()
                .any(|r| r.target.contains("DROP PARTITION")),
            "the partition must now be dropped"
        );
    }

    // Multi-thread runtime: the questdb-rs ILP flush is SYNC-blocking;
    // a current_thread runtime would starve the stub server tasks while
    // the flush blocks (deadlock until the request timeout).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_stub_f2_audit_flush_failure_withholds_drop() {
        let (qdb_url, qdb_log) =
            spawn_stub(qdb_responder(vec!["2026-04-01T09"], 3, CSV, vec![], 200)).await;
        let (ilp_url, ilp_log) = spawn_stub(ilp_responder(500)).await; // reject flushes
        let objects = Arc::new(Mutex::new(HashMap::new()));
        let (s3_url, _s3_log) = spawn_stub(s3_responder(objects)).await;

        let mut archiver =
            build_archiver(&qdb_url, &ilp_url, &s3_url, test_retention_cfg(200)).await;
        let summary = archiver.archive_and_drop_old_partitions().await;

        assert_eq!(summary.verified, 1, "the archive itself verified");
        assert_eq!(summary.dropped, 0, "drop must be WITHHELD without the ACK");
        assert_eq!(summary.failed, 1, "AuditFailed counts as a failure");
        assert!(
            ilp_log
                .lock()
                .expect("ilp log")
                .iter()
                .any(|r| r.method == "POST"),
            "the flush was attempted (and rejected)"
        );
        assert!(
            !qdb_log
                .lock()
                .expect("qdb log")
                .iter()
                .any(|r| r.target.contains("DROP PARTITION")),
            "no verified-audit ACK → no drop"
        );
    }

    // Multi-thread runtime: the questdb-rs ILP flush is SYNC-blocking;
    // a current_thread runtime would starve the stub server tasks while
    // the flush blocks (deadlock until the request timeout).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_stub_f3_suspended_table_skipped_table_wide() {
        let (qdb_url, qdb_log) = spawn_stub(qdb_responder(
            vec!["2026-04-01T09"],
            3,
            CSV,
            vec!["ticks"],
            200,
        ))
        .await;
        let (ilp_url, _ilp_log) = spawn_stub(ilp_responder(204)).await;
        let objects = Arc::new(Mutex::new(HashMap::new()));
        let (s3_url, _s3_log) = spawn_stub(s3_responder(objects)).await;

        let mut archiver =
            build_archiver(&qdb_url, &ilp_url, &s3_url, test_retention_cfg(200)).await;
        let summary = archiver.archive_and_drop_old_partitions().await;

        assert_eq!(
            summary.partitions_considered, 0,
            "the suspended table's partitions must never be considered"
        );
        assert_eq!(summary.dropped, 0);
        let qdb_reqs = qdb_log.lock().expect("qdb log").clone();
        assert!(
            !qdb_reqs.iter().any(|r| r.target.starts_with("/exp")),
            "no export may run against a WAL-suspended table"
        );
        assert!(
            !qdb_reqs
                .iter()
                .any(|r| r.target.contains("table_partitions('ticks')")),
            "the suspended table is skipped before listing"
        );
    }

    // Multi-thread runtime: the questdb-rs ILP flush is SYNC-blocking;
    // a current_thread runtime would starve the stub server tasks while
    // the flush blocks (deadlock until the request timeout).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_stub_f3_wal_probe_failure_skips_entire_run() {
        let (qdb_url, qdb_log) =
            spawn_stub(qdb_responder(vec!["2026-04-01T09"], 3, CSV, vec![], 500)).await;
        let (ilp_url, _ilp_log) = spawn_stub(ilp_responder(204)).await;
        let objects = Arc::new(Mutex::new(HashMap::new()));
        let (s3_url, s3_log) = spawn_stub(s3_responder(objects)).await;

        let mut archiver =
            build_archiver(&qdb_url, &ilp_url, &s3_url, test_retention_cfg(200)).await;
        let summary = archiver.archive_and_drop_old_partitions().await;

        assert_eq!(summary.tables_scanned, 0);
        assert_eq!(summary.partitions_considered, 0);
        assert_eq!(summary.dropped, 0);
        assert!(
            !qdb_log
                .lock()
                .expect("qdb log")
                .iter()
                .any(|r| r.target.contains("table_partitions(")),
            "an unprovable WAL state must skip the whole run"
        );
        assert!(s3_log.lock().expect("s3 log").is_empty());
    }

    // Multi-thread runtime: the questdb-rs ILP flush is SYNC-blocking;
    // a current_thread runtime would starve the stub server tasks while
    // the flush blocks (deadlock until the request timeout).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_stub_empty_export_body_is_export_failure() {
        let (qdb_url, qdb_log) =
            spawn_stub(qdb_responder(vec!["2026-04-01T09"], 3, "", vec![], 200)).await;
        let (ilp_url, _ilp_log) = spawn_stub(ilp_responder(204)).await;
        let objects = Arc::new(Mutex::new(HashMap::new()));
        let (s3_url, s3_log) = spawn_stub(s3_responder(objects)).await;

        let mut archiver =
            build_archiver(&qdb_url, &ilp_url, &s3_url, test_retention_cfg(200)).await;
        let summary = archiver.archive_and_drop_old_partitions().await;

        assert_eq!(summary.failed, 1, "empty body = export failure, not 0 rows");
        assert_eq!(summary.dropped, 0);
        assert!(
            s3_log.lock().expect("s3 log").is_empty(),
            "nothing uploaded"
        );
        assert!(
            !qdb_log
                .lock()
                .expect("qdb log")
                .iter()
                .any(|r| r.target.contains("DROP PARTITION"))
        );
    }

    // Multi-thread runtime: the questdb-rs ILP flush is SYNC-blocking;
    // a current_thread runtime would starve the stub server tasks while
    // the flush blocks (deadlock until the request timeout).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_stub_max_partitions_per_run_caps_oldest_first() {
        let (qdb_url, qdb_log) = spawn_stub(qdb_responder(
            vec!["2026-04-01T11", "2026-04-01T09", "2026-04-01T10"],
            3,
            CSV,
            vec![],
            200,
        ))
        .await;
        let (ilp_url, _ilp_log) = spawn_stub(ilp_responder(204)).await;
        let objects = Arc::new(Mutex::new(HashMap::new()));
        let (s3_url, _s3_log) = spawn_stub(s3_responder(Arc::clone(&objects))).await;

        let mut archiver = build_archiver(&qdb_url, &ilp_url, &s3_url, test_retention_cfg(1)).await;
        let summary = archiver.archive_and_drop_old_partitions().await;

        assert_eq!(summary.partitions_considered, 1, "bounded per run");
        assert_eq!(summary.dropped, 1);
        let qdb_reqs = qdb_log.lock().expect("qdb log").clone();
        assert_eq!(
            qdb_reqs
                .iter()
                .filter(|r| r.target.starts_with("/exp"))
                .count(),
            1,
            "exactly one partition exported under the cap"
        );
        assert!(
            objects
                .lock()
                .expect("objects")
                .contains_key("tv-test-cold/questdb-partitions/ticks/2026-04-01T09.csv.gz"),
            "the OLDEST partition goes first (monotonic progress)"
        );
    }
}

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
//!    tables → `market_data_hot_days`, default 14; everything else →
//!    `retention_days`, 90), clamped to a hard [`MIN_HOT_DAYS`] = 2 floor so
//!    today's and yesterday's partitions are untouchable regardless of
//!    config. Listed via the SAME `table_partitions()` mechanism + active-
//!    partition exclusion + fail-closed name allowlist the detach path uses.
//! 2. **Export** — QuestDB HTTP `/exp` CSV of `SELECT * FROM <t> WHERE
//!    ts >= '<start>' AND ts < '<end>'` (explicit ISO range predicates —
//!    never integer literals against TIMESTAMP columns, never
//!    `IN (SELECT …)`), streamed chunk-by-chunk through a gzip encoder into
//!    a temp file under `data/tmp/partition-archive/`, counting newlines in
//!    the RAW (pre-compression) stream.
//! 3. **Upload** — S3 `PutObject` to
//!    `s3://<bucket>/questdb-partitions/<table>/<partition>.csv.gz` (bucket
//!    = config override or derived `tv-<env>-cold`; the prod instance role
//!    already carries Put/Get/List on it — no IAM change).
//! 4. **Verify (fail-closed, ALL of)** — (a) `SELECT count()` over the same
//!    range taken AFTER the export equals the exported row count (the ≥2-day
//!    floor makes the count stable); (b) `HeadObject` exists and
//!    ContentLength == the local gzip file length; (c) the raw-stream
//!    newline count is the byte-equivalent of decompressing the uploaded
//!    file and counting lines — the counted bytes are exactly the bytes fed
//!    to the encoder, so no second decompression pass is needed. Only a
//!    passing verify constructs the [`VerifiedArchive`] type-state proof.
//! 5. **Audit-first + drop** — a `verified` forensic row is appended to
//!    `partition_archive_audit` BEFORE the destructive step, then
//!    `ALTER TABLE <t> DROP PARTITION LIST '<p>'` — this FREES disk.
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

use crate::partition_manager::{
    DAY_PARTITIONED_TABLES, HOUR_PARTITIONED_TABLES, RETENTION_EXEMPT_TABLES,
    build_detach_list_sql, is_valid_partition_name, parse_partition_rows,
    select_partitions_to_detach,
};

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
    /// High-volume market data (`ticks` + the 21 `candles_*` tables) —
    /// `market_data_hot_days` hot window (default 14).
    MarketData,
    /// Everything else (audit / daily-data tables) — the existing
    /// `retention_days` window (default 90).
    Standard,
}

/// Maps a table to its retention class. Market data = the HOUR-partitioned
/// high-frequency list (`ticks`) + the 21 live candle tables from the single
/// source of truth (`candle_table_names()`); everything else is Standard.
pub(crate) fn retention_class(table: &str) -> RetentionClass {
    if HOUR_PARTITIONED_TABLES.contains(&table)
        || crate::shadow_persistence::candle_table_names().contains(&table)
    {
        RetentionClass::MarketData
    } else {
        RetentionClass::Standard
    }
}

/// Clamps a configured hot window to the [`MIN_HOT_DAYS`] floor — today's
/// and yesterday's partitions are untouchable even under `hot_days = 0`.
pub(crate) fn effective_hot_days(configured_days: u32) -> u32 {
    configured_days.max(MIN_HOT_DAYS)
}

/// The effective hot window (days) for a table under the given config —
/// class window clamped to the [`MIN_HOT_DAYS`] floor.
pub(crate) fn hot_window_days(table: &str, cfg: &PartitionRetentionConfig) -> u32 {
    let class_days = match retention_class(table) {
        RetentionClass::MarketData => cfg.market_data_hot_days,
        RetentionClass::Standard => cfg.retention_days,
    };
    effective_hot_days(class_days)
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
pub(crate) fn verify_archive(
    table: &str,
    partition: &str,
    exported_rows: u64,
    csv_bytes: u64,
    local_gzip_len: u64,
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
        _proof: VerifyProof,
    })
}

// ---------------------------------------------------------------------------
// Bucket resolution (pure + thin env shim)
// ---------------------------------------------------------------------------

/// Resolves the archive bucket: an explicit config value wins; empty derives
/// the house cold bucket `tv-<env>-cold` (terraform `tv-${environment}-cold`
/// — the bucket the prod instance role already reads/writes).
pub(crate) fn resolve_archive_bucket(configured: &str, environment: &str) -> String {
    let trimmed = configured.trim();
    if trimmed.is_empty() {
        format!("tv-{environment}-cold")
    } else {
        trimmed.to_string()
    }
}

/// Environment-name resolution from raw env-var reads (pure — testable
/// without process-global env mutation). Precedence mirrors
/// `secret_manager::resolve_environment`: `TV_ENVIRONMENT` → `ENVIRONMENT`
/// → `"prod"`; anything with characters outside `[a-zA-Z0-9-]` fails back
/// to `prod` (path/bucket-name safety, fail-closed).
pub(crate) fn resolve_environment_from(
    tv_environment: Option<&str>,
    environment: Option<&str>,
) -> String {
    let candidate = tv_environment
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| environment.map(str::trim).filter(|s| !s.is_empty()))
        .unwrap_or("prod");
    if candidate
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        candidate.to_string()
    } else {
        "prod".to_string()
    }
}

/// Reads the runtime environment name (`TV_ENVIRONMENT` → `ENVIRONMENT` →
/// `"prod"`), matching the SSM/config env selection the rest of the app uses.
fn runtime_environment() -> String {
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
    /// The DROP DDL failed AFTER a passed verify — the S3 copy exists;
    /// retried next run (idempotent overwrite).
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
            Self::DropFailed => "drop_failed",
        }
    }
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
    pub s3_key: String,
    pub detail: String,
}

/// Lazy-connect ILP writer for `partition_archive_audit` — the
/// `TickConservationAuditWriter` template. Best-effort (AUDIT-WS-01 class):
/// a write failure logs loudly but never gates the archive flow; the S3
/// object + the partition's presence/absence remain the ground truth.
pub(crate) struct PartitionArchiveAuditWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl PartitionArchiveAuditWriter {
    /// Production constructor — ILP TCP, lazy on failure.
    // TEST-EXEMPT: production ILP-connect constructor (needs live QuestDB);
    // append/flush paths covered via for_test().
    pub(crate) fn new(config: &QuestDbConfig) -> Self {
        let conf = format!("tcp::addr={}:{};", config.host, config.ilp_port);
        match Sender::from_conf(&conf) {
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
            .column_str("s3_key", &r.s3_key)
            .context("s3_key")?
            .column_str("detail", &r.detail)
            .context("detail")?
            .at(TimestampNanos::new(r.ts_ist_nanos))
            .context("designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Flushes buffered rows to QuestDB.
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
    path: PathBuf,
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
    /// # Errors
    /// HTTP-client build failure (fail-closed — the caller skips the archive
    /// cycle for the run and logs `STORAGE-GAP-04`).
    // TEST-EXEMPT: constructs live AWS/HTTP clients (aws_config::load needs
    // a runtime env); the pure pieces (bucket/env resolution, SQL builders,
    // verify decision) are unit-tested below.
    pub async fn new(questdb: &QuestDbConfig, cfg: &PartitionRetentionConfig) -> Result<Self> {
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
        let bucket = resolve_archive_bucket(&cfg.archive_bucket, &runtime_environment());
        Ok(Self {
            exec_url: format!("http://{}:{}/exec", questdb.host, questdb.http_port),
            exp_url: format!("http://{}:{}/exp", questdb.host, questdb.http_port),
            ddl_client,
            export_client,
            s3,
            bucket,
            temp_dir: PathBuf::from(ARCHIVE_TEMP_DIR),
            cfg: cfg.clone(),
            audit: PartitionArchiveAuditWriter::new(questdb),
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

        // Build the worklist: (table, partition) pairs older than each
        // table's class hot window, active partition excluded, hostile names
        // rejected — the SAME selection primitives the detach path uses.
        let mut worklist: Vec<(&'static str, String)> = Vec::new();
        for table in swept_tables() {
            if RETENTION_EXEMPT_TABLES.contains(&table) {
                continue;
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
                    debug!(
                        ?err,
                        table, "partition list failed (table may not exist yet)"
                    );
                }
            }
        }

        // Oldest first (partition names sort chronologically) so an
        // interrupted run makes monotonic progress; bounded per run so the
        // first catch-up sweep converges over a few evenings.
        worklist.sort_by(|a, b| a.1.cmp(&b.1));
        if self.cfg.max_partitions_per_run > 0 {
            worklist.truncate(self.cfg.max_partitions_per_run as usize);
        }
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
        let s3_key = format!("{ARCHIVE_S3_PREFIX}/{table}/{partition}.csv.gz");

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

        // 2. Upload.
        if let Err(err) = self.upload_to_s3(&exported.path, &s3_key).await {
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

        // 3. Verify — recount AFTER the export + S3 head. All fail-closed.
        let recount = self.recount_rows(table, &start, &end).await;
        let s3_len = self.head_object_len(&s3_key).await;
        let proof = match verify_archive(
            table,
            partition,
            exported.rows,
            exported.csv_bytes,
            exported.gzip_bytes,
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

        // 4. Audit-first: the durable `verified` row precedes the drop.
        self.append_audit(
            table,
            partition,
            ArchiveOutcome::Verified,
            &proof,
            &s3_key,
            "",
        );

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
        let body = response
            .text()
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
            let body = response
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect::<String>(); // O(1) EXEMPT: error logging
            anyhow::bail!("/exp export returned {status}: {body}");
        }

        let file = std::fs::File::create(&path).context("create archive temp file")?;
        let mut encoder = GzEncoder::new(std::io::BufWriter::new(file), Compression::default());
        let mut newline_count: u64 = 0;
        let mut csv_bytes: u64 = 0;
        while let Some(chunk) = response.chunk().await.context("/exp stream read failed")? {
            newline_count =
                newline_count.saturating_add(chunk.iter().filter(|&&b| b == b'\n').count() as u64);
            csv_bytes = csv_bytes.saturating_add(chunk.len() as u64);
            encoder.write_all(&chunk).context("gzip write failed")?;
        }
        let mut writer = encoder.finish().context("gzip finish failed")?;
        writer.flush().context("temp file flush failed")?;
        drop(writer);

        if newline_count == 0 {
            // Not even the CSV header arrived — an empty body is an export
            // failure, never "0 rows" (audit Rule 11: no false-OK).
            remove_temp_file(&path);
            anyhow::bail!("/exp export returned an empty body (no CSV header)");
        }
        let gzip_bytes = std::fs::metadata(&path)
            .context("stat archive temp file")?
            .len();
        Ok(ExportedCsv {
            // newlines − 1 header line = data rows (QuestDB /exp always
            // emits the header row and terminates lines with newlines).
            rows: newline_count.saturating_sub(1),
            csv_bytes,
            gzip_bytes,
            path,
        })
    }

    /// Uploads the gzip'd export to the cold bucket.
    async fn upload_to_s3(&self, path: &Path, key: &str) -> Result<()> {
        let body = aws_sdk_s3::primitives::ByteStream::from_path(path)
            .await
            .context("open temp file for S3 upload")?;
        self.s3
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(body)
            .send()
            .await
            .context("S3 PutObject failed")?;
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
        let body = response.text().await.ok()?;
        parse_count_response(&body)
    }

    /// S3 `HeadObject` ContentLength (verify leg (b)). `None` = missing.
    async fn head_object_len(&self, key: &str) -> Option<u64> {
        let head = self
            .s3
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .ok()?;
        head.content_length().and_then(|n| u64::try_from(n).ok())
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
            let body = response
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect::<String>(); // O(1) EXEMPT: error logging
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
            ArchiveOutcome::DropFailed => "drop",
            ArchiveOutcome::Verified | ArchiveOutcome::Dropped => "unexpected",
        };
        metrics::counter!("tv_partition_archive_failed_total", "stage" => stage).increment(1);
        error!(
            code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
            table,
            partition,
            outcome = outcome.as_str(),
            detail,
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
            s3_key: String::new(),
            detail: detail.chars().take(300).collect(),
        };
        if let Err(err) = self.audit.append_row(&row) {
            warn!(
                ?err,
                table, partition, "archive-audit append failed (best-effort)"
            );
        }
    }

    /// Appends a success-class (`verified` / `dropped` / `drop_failed`)
    /// audit row + flushes so the `verified` row is DURABLE before the drop
    /// (audit-first, §24 discipline). Best-effort — never gates.
    fn append_audit(
        &mut self,
        table: &str,
        partition: &str,
        outcome: ArchiveOutcome,
        proof: &VerifiedArchive,
        s3_key: &str,
        detail: &str,
    ) {
        let row = PartitionArchiveAuditRow {
            ts_ist_nanos: now_ist_nanos(),
            table_name: table.to_string(),
            partition_name: partition.to_string(),
            outcome,
            rows_archived: i64::try_from(proof.rows).unwrap_or(i64::MAX),
            csv_bytes: i64::try_from(proof.csv_bytes).unwrap_or(i64::MAX),
            gzip_bytes: i64::try_from(proof.gzip_bytes).unwrap_or(i64::MAX),
            s3_key: s3_key.to_string(),
            detail: detail.chars().take(300).collect(),
        };
        let appended = self.audit.append_row(&row);
        let flushed = appended.and_then(|()| self.audit.flush());
        if let Err(err) = flushed {
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
mod tests {
    use super::*;

    fn cfg(retention_days: u32, market_data_hot_days: u32) -> PartitionRetentionConfig {
        PartitionRetentionConfig {
            retention_days,
            market_data_hot_days,
            archive_enabled: true,
            archive_bucket: String::new(),
            max_partitions_per_run: 200,
        }
    }

    // ---- retention-class mapping -------------------------------------------

    #[test]
    fn test_retention_class_ticks_is_market_data() {
        assert_eq!(retention_class("ticks"), RetentionClass::MarketData);
    }

    #[test]
    fn test_retention_class_every_candle_table_is_market_data() {
        for table in crate::shadow_persistence::candle_table_names() {
            assert_eq!(
                retention_class(table),
                RetentionClass::MarketData,
                "candle table must be market-data class: {table}"
            );
        }
    }

    #[test]
    fn test_retention_class_audit_tables_are_standard() {
        for table in [
            "ws_event_audit",
            "instrument_fetch_audit",
            "prev_day_ohlcv",
            "feed_scoreboard_daily",
            "spot_1m_rest",
            "option_chain_1m",
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
        assert_eq!(hot_window_days("ticks", &c), 14);
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
        assert_eq!(effective_hot_days(0), MIN_HOT_DAYS);
        assert_eq!(effective_hot_days(1), MIN_HOT_DAYS);
        assert_eq!(effective_hot_days(2), 2);
        assert_eq!(effective_hot_days(14), 14);
        assert_eq!(effective_hot_days(90), 90);
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
        let n = effective_hot_days(14);
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
            Some(100),
            Some(1200),
        )
        .expect("all checks pass");
        assert_eq!(proof.table(), "ticks");
        assert_eq!(proof.partition(), "2026-04-01T09");
        assert_eq!(proof.rows, 100);
        assert_eq!(proof.csv_bytes, 5000);
        assert_eq!(proof.gzip_bytes, 1200);
    }

    #[test]
    fn test_verify_archive_zero_row_partition_passes() {
        // Header-only export (0 data rows) with recount 0 is a legitimate
        // empty partition — verified and droppable.
        let proof = verify_archive("ticks", "2026-04-01T09", 0, 20, 40, Some(0), Some(40))
            .expect("empty ok");
        assert_eq!(proof.rows, 0);
    }

    #[test]
    fn test_verify_archive_recount_unavailable_fails() {
        let err = verify_archive("t", "2026-04-01", 100, 5000, 1200, None, Some(1200))
            .expect_err("recount unavailable must fail");
        assert_eq!(err, VerifyFailure::RecountUnavailable);
        assert_eq!(err.outcome(), ArchiveOutcome::CountMismatch);
    }

    #[test]
    fn test_verify_archive_row_count_mismatch_fails() {
        let err = verify_archive("t", "2026-04-01", 100, 5000, 1200, Some(101), Some(1200))
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
        let err = verify_archive("t", "2026-04-01", 100, 5000, 1200, Some(100), None)
            .expect_err("missing S3 object must fail");
        assert_eq!(err, VerifyFailure::S3ObjectMissing);
        assert_eq!(err.outcome(), ArchiveOutcome::SizeMismatch);
    }

    #[test]
    fn test_verify_archive_s3_size_mismatch_fails() {
        let err = verify_archive("t", "2026-04-01", 100, 5000, 1200, Some(100), Some(1199))
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
        let err = verify_archive("t", "2026-04-01", 100, 5000, 1200, Some(99), None)
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

    // ---- bucket / environment resolution ------------------------------------

    #[test]
    fn test_resolve_archive_bucket_explicit_wins() {
        assert_eq!(resolve_archive_bucket("my-bucket", "prod"), "my-bucket");
        assert_eq!(resolve_archive_bucket("  my-bucket  ", "prod"), "my-bucket");
    }

    #[test]
    fn test_resolve_archive_bucket_derives_cold_bucket() {
        assert_eq!(resolve_archive_bucket("", "prod"), "tv-prod-cold");
        assert_eq!(resolve_archive_bucket("   ", "staging"), "tv-staging-cold");
    }

    #[test]
    fn test_resolve_environment_from_precedence_and_validation() {
        assert_eq!(resolve_environment_from(Some("prod"), None), "prod");
        assert_eq!(
            resolve_environment_from(Some("staging"), Some("prod")),
            "staging"
        );
        assert_eq!(resolve_environment_from(None, Some("dev-1")), "dev-1");
        assert_eq!(resolve_environment_from(None, None), "prod");
        assert_eq!(resolve_environment_from(Some(""), Some("  ")), "prod");
        // Path/bucket-hostile values fail back to prod (fail-closed).
        assert_eq!(resolve_environment_from(Some("../etc"), None), "prod");
        assert_eq!(resolve_environment_from(Some("a b"), None), "prod");
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
        assert_eq!(ArchiveOutcome::DropFailed.as_str(), "drop_failed");
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

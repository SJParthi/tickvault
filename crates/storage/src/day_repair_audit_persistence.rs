//! `day_repair_audit` — one row per step of the one-off repair of a held day
//! (daily-universe Quote 63, 2026-11-20: hold, copy, delete and rebuild the
//! 20 Nov trade-time shift; plan ITEM UTC-REPAIR).
//!
//! ## What a row is
//!
//! One step on one object: a partition copied to the quarantine prefix, a
//! partition dropped after its verified copy, a local seal/spill/DLQ file
//! copied or moved aside, the rebuild, the verify, the `.done` marker. A row
//! is written for every outcome, the refusals and kept objects included,
//! because Quote 63 requires the repair to leave its own audit trail and the
//! repair tool writes NO `partition_archive_audit` row (that table belongs to
//! the archiver and feeds the Brutex export's audit read).
//!
//! ## Table
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS day_repair_audit (
//!     ts          TIMESTAMP,  -- when the step finished (IST nanos)
//!     repair_day  SYMBOL,     -- the held day being repaired, YYYY-MM-DD
//!     step        SYMBOL,     -- preflight / enumerate / copy / drop / move_files / rebuild / verify / done
//!     object      SYMBOL,     -- table/partition, files/<name>, or the step's subject
//!     feed        SYMBOL,     -- `all`: a partition holds every feed's rows
//!     outcome     SYMBOL,     -- ok / copied / reused / dropped / kept / refused / failed
//!     run_id      SYMBOL,     -- the deployed commit that ran the step
//!     row_count   LONG,       -- rows copied / dropped / rebuilt; NULL when not counted
//!     byte_count  LONG,       -- bytes stored in S3 (gzip for a partition); NULL when none
//!     sha256      STRING,     -- SHA-256 (hex) of the stored object; empty when none
//!     s3_key      STRING,     -- where the copy lives; empty when none
//!     detail      STRING      -- the reason, for a kept / refused / failed step
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, repair_day, step, object, feed);
//! ```
//!
//! DEDUP carries `ts` and `feed` (operator override 2026-06-28). Rows are per
//! object, not per instrument, so I-P1-11's `(security_id, exchange_segment)`
//! pair is N/A. The table is retention-exempt (kept, never dropped): it is
//! the record of a one-off destructive repair.
//!
//! ## Honest limits
//!
//! * A row is written with [`DayRepairAuditWriter::append_and_flush`], one
//!   ILP-over-HTTP request per row, so a flush `Ok` means QuestDB ACKED it.
//!   A failed flush DISCARDS the buffered row (a server-rejected row must not
//!   wedge every later row): the caller treats that as "the step is not
//!   recorded" and, where the row gates a destructive step, does not take it.
//! * Cold path: a few hundred rows once, in an offline job with the app
//!   stopped. Each append is O(row bytes); nothing here is on a hot path.

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::error;

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::IST_UTC_OFFSET_NANOS;
use tickvault_common::error_code::ErrorCode;

/// QuestDB table name.
pub const DAY_REPAIR_AUDIT_TABLE: &str = "day_repair_audit";

/// DEDUP UPSERT key — `ts` first, then the day, the step, the object and the
/// feed. A re-run of a step at a new instant is a new row (the history of
/// attempts is the point); a replayed flush of the same row upserts.
pub const DEDUP_KEY_DAY_REPAIR_AUDIT: &str = "ts, repair_day, step, object, feed";

/// `feed` value of every row: a QuestDB partition holds every feed's rows, so
/// the honest scope label is `all` (the `partition_archive_audit` convention).
pub const DAY_REPAIR_FEED_ALL: &str = "all";

/// Timeout of one DDL statement of the ensure.
pub const DAY_REPAIR_AUDIT_DDL_TIMEOUT_SECS: u64 = 10;

/// ILP-HTTP request timeout of the writer's sender, in seconds. Built with
/// `retry_timeout=0` and `protocol_version=1`, so one flush is one request.
pub const DAY_REPAIR_AUDIT_ILP_REQUEST_TIMEOUT_SECS: u64 = 5;

/// Longest `object` symbol stored, in bytes.
pub const DAY_REPAIR_OBJECT_MAX_BYTES: usize = 512;

/// Longest `s3_key` stored, in bytes (an S3 key is at most 1,024).
pub const DAY_REPAIR_S3_KEY_MAX_BYTES: usize = 1_024;

/// Longest `detail` stored, in bytes.
pub const DAY_REPAIR_DETAIL_MAX_BYTES: usize = 2_000;

/// `(column, type)` for every column, in DDL order. Single source for the
/// `CREATE` and the per-column `ADD COLUMN IF NOT EXISTS` self-heal.
pub const DAY_REPAIR_AUDIT_COLUMNS: &[(&str, &str)] = &[
    ("ts", "TIMESTAMP"),
    ("repair_day", "SYMBOL"),
    ("step", "SYMBOL"),
    ("object", "SYMBOL"),
    ("feed", "SYMBOL"),
    ("outcome", "SYMBOL"),
    ("run_id", "SYMBOL"),
    ("row_count", "LONG"),
    ("byte_count", "LONG"),
    ("sha256", "STRING"),
    ("s3_key", "STRING"),
    ("detail", "STRING"),
];

/// The step a row records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayRepairStep {
    /// Refusals before any change (window, app running, WAL lock, expiry).
    Preflight,
    /// The listing of the day's partitions and files.
    Enumerate,
    /// A verified copy to the quarantine prefix.
    Copy,
    /// A partition DROP after its verified copy.
    Drop,
    /// A local seal/spill/DLQ file moved aside (rename, never a delete).
    MoveFiles,
    /// The rebuild of the day from its raw frames.
    Rebuild,
    /// The verify gate.
    Verify,
    /// The `.done` marker.
    Done,
}

impl DayRepairStep {
    /// Stable wire label for the `step` SYMBOL column. Pure, O(1).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Preflight => "preflight",
            Self::Enumerate => "enumerate",
            Self::Copy => "copy",
            Self::Drop => "drop",
            Self::MoveFiles => "move_files",
            Self::Rebuild => "rebuild",
            Self::Verify => "verify",
            Self::Done => "done",
        }
    }
}

/// How a step ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayRepairOutcome {
    /// Finished (a step with no more specific outcome).
    Ok,
    /// Copied: this run put the object in S3.
    Copied,
    /// An identical object was already in S3 (a re-run); reused, not uploaded.
    Reused,
    /// The partition was dropped after its verified copy.
    Dropped,
    /// Kept: something did not prove out, nothing destructive happened.
    Kept,
    /// Refused before any work (outside the allowlist, a guard said no).
    Refused,
    /// Failed after work started; see `detail`.
    Failed,
}

impl DayRepairOutcome {
    /// Stable wire label for the `outcome` SYMBOL column. Pure, O(1).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Copied => "copied",
            Self::Reused => "reused",
            Self::Dropped => "dropped",
            Self::Kept => "kept",
            Self::Refused => "refused",
            Self::Failed => "failed",
        }
    }
}

/// One step, ready for ILP write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DayRepairAuditRow {
    /// When the step finished (IST epoch nanos). The designated timestamp.
    pub ts_ist_nanos: i64,
    /// The held day being repaired, `YYYY-MM-DD`.
    pub repair_day: String,
    /// The step.
    pub step: DayRepairStep,
    /// `table/partition`, `files/<name>`, or the step's subject.
    pub object: String,
    /// How it ended.
    pub outcome: DayRepairOutcome,
    /// The deployed commit that ran the step.
    pub run_id: String,
    /// Rows copied / dropped / rebuilt; `None` when not counted.
    pub row_count: Option<u64>,
    /// Bytes stored in S3; `None` when nothing was stored.
    pub byte_count: Option<u64>,
    /// SHA-256 (hex) of the stored object; empty when none.
    pub sha256: String,
    /// Where the copy lives; empty when none.
    pub s3_key: String,
    /// The reason, for a kept / refused / failed step.
    pub detail: String,
}

impl DayRepairAuditRow {
    /// A row stamped now, with every optional field empty. O(1) plus the
    /// string copies.
    #[must_use]
    pub fn now(
        repair_day: &str,
        step: DayRepairStep,
        object: &str,
        outcome: DayRepairOutcome,
        run_id: &str,
    ) -> Self {
        Self {
            ts_ist_nanos: day_repair_now_ist_nanos(),
            repair_day: repair_day.to_string(),
            step,
            object: object.to_string(),
            outcome,
            run_id: run_id.to_string(),
            row_count: None,
            byte_count: None,
            sha256: String::new(),
            s3_key: String::new(),
            detail: String::new(),
        }
    }
}

/// Wall-clock now as IST epoch nanos (the house rule: a non-feed timestamp
/// from the system clock adds `IST_UTC_OFFSET_NANOS` so QuestDB shows IST).
/// A clock outside the nanosecond range reads 0, which the ILP writer then
/// accepts as 1970 rather than refusing the row. Pure apart from the clock.
#[must_use]
pub fn day_repair_now_ist_nanos() -> i64 {
    chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(0)
        .saturating_add(IST_UTC_OFFSET_NANOS)
}

/// `s` cut to at most `max` bytes on a char boundary. Pure, O(max).
#[must_use]
pub fn bounded_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    &s[..cut]
}

/// A symbol value: an empty one is written as `-` (an ILP symbol needs a
/// value), anything else cut to `max` bytes. Pure.
fn symbol_value(s: &str, max: usize) -> &str {
    if s.is_empty() {
        "-"
    } else {
        bounded_str(s, max)
    }
}

/// The writer's ILP conf string for `host:port`. Pure.
#[must_use]
pub fn day_repair_audit_ilp_conf(host: &str, http_port: u16) -> String {
    format!(
        "http::addr={host}:{http_port};protocol_version=1;retry_timeout=0;request_timeout={};",
        DAY_REPAIR_AUDIT_ILP_REQUEST_TIMEOUT_SECS * 1000
    )
}

/// `CREATE` → per-column `ALTER ADD COLUMN IF NOT EXISTS` → `DEDUP ENABLE`.
/// Never a drop. Pure.
#[must_use]
pub fn day_repair_audit_ddl_statements() -> Vec<String> {
    // O(1) EXEMPT: begin — one-off DDL build over a fixed 12-column list
    let cols = DAY_REPAIR_AUDIT_COLUMNS
        .iter()
        .map(|(name, ty)| format!("{name} {ty}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut statements = vec![format!(
        "CREATE TABLE IF NOT EXISTS {DAY_REPAIR_AUDIT_TABLE} ({cols}) \
         timestamp(ts) PARTITION BY DAY \
         DEDUP UPSERT KEYS({DEDUP_KEY_DAY_REPAIR_AUDIT});"
    )];
    for (col, ty) in DAY_REPAIR_AUDIT_COLUMNS {
        if *col == "ts" {
            continue;
        }
        statements.push(format!(
            "ALTER TABLE {DAY_REPAIR_AUDIT_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty};"
        ));
    }
    // O(1) EXEMPT: end
    statements.push(format!(
        "ALTER TABLE {DAY_REPAIR_AUDIT_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_DAY_REPAIR_AUDIT});"
    ));
    statements
}

/// Ensures the table (idempotent, self-healing). Every failure is logged
/// with a code (STORAGE-GAP-03). Returns `true` only when every statement
/// was accepted; the repair refuses to start without it, so no row is ever
/// written into a table ILP auto-created with no DEDUP key.
// TEST-EXEMPT: live-QuestDB DDL runner (DDL strings unit-tested below); the unreachable path is tested.
pub async fn ensure_day_repair_audit_table(questdb_config: &QuestDbConfig) -> bool {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(
            DAY_REPAIR_AUDIT_DDL_TIMEOUT_SECS,
        ))
        .no_proxy()
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            error!(
                code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                stage = "ensure_client_build",
                ?err,
                "day_repair_audit: HTTP client build failed — table not ensured"
            );
            return false;
        }
    };
    let mut all_ok = true;
    for ddl in &day_repair_audit_ddl_statements() {
        match client
            .get(&base_url)
            .query(&[("query", ddl.as_str())])
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => {
                all_ok = false;
                let status = resp.status();
                error!(
                    code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                    stage = "ensure_ddl",
                    %status,
                    ddl = ddl.as_str(),
                    "day_repair_audit: DDL returned non-2xx"
                );
            }
            Err(err) => {
                all_ok = false;
                error!(
                    code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                    stage = "ensure_ddl",
                    ?err,
                    ddl = ddl.as_str(),
                    "day_repair_audit: DDL request failed"
                );
            }
        }
    }
    all_ok
}

/// Lazy ILP-over-HTTP writer. An unreachable QuestDB still builds
/// (`sender = None`); `flush` returns `Err` until it is reachable.
pub struct DayRepairAuditWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl DayRepairAuditWriter {
    /// Production constructor.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (needs live QuestDB); with_conf and for_test are covered below.
    pub fn new(config: &QuestDbConfig) -> Self {
        Self::with_conf(&day_repair_audit_ilp_conf(&config.host, config.http_port))
    }

    /// Builds a writer from an explicit questdb-rs conf string (the stub
    /// tests point it at a local ILP-HTTP stub).
    #[must_use]
    pub fn with_conf(conf: &str) -> Self {
        match Sender::from_conf(conf) {
            Ok(s) => {
                let buffer = s.new_buffer();
                Self {
                    sender: Some(s),
                    buffer,
                    pending: 0,
                }
            }
            Err(err) => {
                // An address config error, not an unreachable QuestDB (the HTTP
                // sender connects lazily). Every flush then returns `Err`, so
                // no gated step can proceed.
                error!(
                    code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                    stage = "sender_build",
                    ?err,
                    "day_repair_audit: no ILP sender (address config refused)"
                );
                Self::for_test()
            }
        }
    }

    /// Disconnected writer with an empty buffer.
    #[must_use]
    // TEST-EXEMPT: test helper and the address-refused fallback of `with_conf`; exercised by the append tests below.
    pub fn for_test() -> Self {
        Self {
            sender: None,
            buffer: Buffer::new(ProtocolVersion::V1),
            pending: 0,
        }
    }

    /// Rows appended but not yet flushed.
    #[must_use]
    // TEST-EXEMPT: accessor, exercised by the append tests below.
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// Appends one row. O(row bytes).
    ///
    /// # Errors
    /// Propagates ILP buffer errors (a negative timestamp among them).
    pub fn append_row(&mut self, r: &DayRepairAuditRow) -> Result<()> {
        let mut row = self
            .buffer
            .table(DAY_REPAIR_AUDIT_TABLE)
            .context("table")?
            .symbol("repair_day", symbol_value(&r.repair_day, 10))
            .context("repair_day")?
            .symbol("step", r.step.as_str())
            .context("step")?
            .symbol(
                "object",
                symbol_value(&r.object, DAY_REPAIR_OBJECT_MAX_BYTES),
            )
            .context("object")?
            .symbol("feed", DAY_REPAIR_FEED_ALL)
            .context("feed")?
            .symbol("outcome", r.outcome.as_str())
            .context("outcome")?
            .symbol("run_id", symbol_value(&r.run_id, 64))
            .context("run_id")?;
        if let Some(n) = r.row_count {
            row = row
                .column_i64("row_count", i64::try_from(n).unwrap_or(i64::MAX))
                .context("row_count")?;
        }
        if let Some(n) = r.byte_count {
            row = row
                .column_i64("byte_count", i64::try_from(n).unwrap_or(i64::MAX))
                .context("byte_count")?;
        }
        row = row
            .column_str("sha256", bounded_str(&r.sha256, 64))
            .context("sha256")?
            .column_str(
                "s3_key",
                bounded_str(&r.s3_key, DAY_REPAIR_S3_KEY_MAX_BYTES),
            )
            .context("s3_key")?
            .column_str(
                "detail",
                bounded_str(&r.detail, DAY_REPAIR_DETAIL_MAX_BYTES),
            )
            .context("detail")?;
        row.at(TimestampNanos::new(r.ts_ist_nanos))
            .context("designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Flushes buffered rows over ILP-HTTP; `Ok` means QuestDB ACKED them.
    /// The round trip runs off the shared tokio worker. On failure the
    /// buffered rows are DISCARDED (see the module limits).
    ///
    /// # Errors
    /// `Err` when disconnected or the flush fails.
    pub fn flush(&mut self) -> Result<()> {
        if self.pending == 0 {
            return Ok(());
        }
        let Some(sender) = self.sender.as_mut() else {
            self.discard_pending();
            anyhow::bail!("day_repair_audit: no ILP sender (QuestDB unreachable)");
        };
        let buffer = &mut self.buffer;
        let flushed = crate::off_worker::off_worker(|| sender.flush(buffer));
        if let Err(err) = flushed {
            self.discard_pending();
            return Err(anyhow::Error::new(err).context("day_repair_audit ILP flush"));
        }
        self.pending = 0;
        Ok(())
    }

    /// Discards buffered-but-unflushed rows.
    pub fn discard_pending(&mut self) {
        if self.pending == 0 {
            return;
        }
        self.buffer.clear();
        self.pending = 0;
    }

    /// Appends `r` and flushes it: `Ok` means QuestDB ACKED the row. The
    /// gate for a destructive step (the drop follows only an ACKED copy row).
    ///
    /// # Errors
    /// The append or the flush failed; nothing is left buffered.
    pub fn append_and_flush(&mut self, r: &DayRepairAuditRow) -> Result<()> {
        if let Err(err) = self.append_row(r) {
            self.discard_pending();
            return Err(err);
        }
        self.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TS: i64 = 1_795_305_600_000_000_000; // 2026-11-22 00:00 IST-epoch

    fn sample() -> DayRepairAuditRow {
        DayRepairAuditRow {
            ts_ist_nanos: TS,
            repair_day: "2026-11-20".to_string(),
            step: DayRepairStep::Copy,
            object: "ticks/2026-11-20T09".to_string(),
            outcome: DayRepairOutcome::Copied,
            run_id: "abc1234".to_string(),
            row_count: Some(3),
            byte_count: Some(120),
            sha256: "ab".repeat(32),
            s3_key: "quarantine/2026-11-20-ltt-utc-shift/partitions/ticks/2026-11-20T09.csv.gz"
                .to_string(),
            detail: String::new(),
        }
    }

    #[test]
    fn test_dedup_key_day_repair_audit_includes_ts_object_and_feed() {
        assert_eq!(
            DEDUP_KEY_DAY_REPAIR_AUDIT,
            "ts, repair_day, step, object, feed"
        );
        assert!(DEDUP_KEY_DAY_REPAIR_AUDIT.starts_with("ts"));
        assert!(DEDUP_KEY_DAY_REPAIR_AUDIT.contains("feed"));
        for key in DEDUP_KEY_DAY_REPAIR_AUDIT.split(", ") {
            assert!(
                DAY_REPAIR_AUDIT_COLUMNS
                    .iter()
                    .any(|(c, ty)| *c == key && (*ty == "SYMBOL" || *ty == "TIMESTAMP")),
                "DEDUP key column {key} must be a SYMBOL or the timestamp"
            );
        }
    }

    #[test]
    fn test_day_repair_audit_ddl_statements_create_self_heal_and_dedup() {
        let stmts = day_repair_audit_ddl_statements();
        assert_eq!(stmts.len(), DAY_REPAIR_AUDIT_COLUMNS.len() + 1);
        assert!(stmts[0].starts_with("CREATE TABLE IF NOT EXISTS day_repair_audit ("));
        assert!(stmts[0].contains("timestamp(ts) PARTITION BY DAY"));
        assert!(stmts[0].contains("DEDUP UPSERT KEYS(ts, repair_day, step, object, feed)"));
        for (col, ty) in DAY_REPAIR_AUDIT_COLUMNS {
            assert!(stmts[0].contains(&format!("{col} {ty}")), "{col}");
            if *col != "ts" {
                assert!(
                    stmts.contains(&format!(
                        "ALTER TABLE day_repair_audit ADD COLUMN IF NOT EXISTS {col} {ty};"
                    )),
                    "{col} self-heals"
                );
            }
        }
        assert!(
            stmts
                .iter()
                .all(|s| !s.to_ascii_uppercase().contains("DROP"))
        );
        assert_eq!(
            stmts.last().map(String::as_str),
            Some(
                "ALTER TABLE day_repair_audit DEDUP ENABLE UPSERT KEYS(ts, repair_day, step, object, feed);"
            )
        );
    }

    #[test]
    fn test_day_repair_audit_ilp_conf_is_one_bounded_request() {
        let conf = day_repair_audit_ilp_conf("tv-questdb", 9000);
        assert_eq!(
            conf,
            "http::addr=tv-questdb:9000;protocol_version=1;retry_timeout=0;request_timeout=5000;"
        );
        assert!(Sender::from_conf(conf).is_ok());
    }

    #[test]
    fn test_step_and_outcome_labels_are_stable_and_distinct() {
        let steps = [
            DayRepairStep::Preflight,
            DayRepairStep::Enumerate,
            DayRepairStep::Copy,
            DayRepairStep::Drop,
            DayRepairStep::MoveFiles,
            DayRepairStep::Rebuild,
            DayRepairStep::Verify,
            DayRepairStep::Done,
        ];
        let labels: Vec<&str> = steps.iter().map(|s| s.as_str()).collect();
        assert_eq!(
            labels,
            [
                "preflight",
                "enumerate",
                "copy",
                "drop",
                "move_files",
                "rebuild",
                "verify",
                "done"
            ]
        );
        let outcomes = [
            DayRepairOutcome::Ok,
            DayRepairOutcome::Copied,
            DayRepairOutcome::Reused,
            DayRepairOutcome::Dropped,
            DayRepairOutcome::Kept,
            DayRepairOutcome::Refused,
            DayRepairOutcome::Failed,
        ];
        let mut labels: Vec<&str> = outcomes.iter().map(|o| o.as_str()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), outcomes.len(), "outcome labels are distinct");
    }

    #[test]
    fn test_append_row_writes_every_column() {
        let mut w = DayRepairAuditWriter::for_test();
        w.append_row(&sample()).expect("append");
        assert_eq!(w.pending(), 1);
        let line = String::from_utf8(w.buffer.as_bytes().to_vec()).unwrap_or_default();
        for needle in [
            "day_repair_audit,repair_day=2026-11-20,step=copy,object=ticks/2026-11-20T09,\
             feed=all,outcome=copied,run_id=abc1234 ",
            "row_count=3i",
            "byte_count=120i",
            "s3_key=\"quarantine/2026-11-20-ltt-utc-shift/partitions/ticks/2026-11-20T09.csv.gz\"",
            "detail=\"\"",
        ] {
            assert!(line.contains(needle), "{needle} missing in {line}");
        }
    }

    #[test]
    fn test_append_row_omits_null_counts_and_fills_empty_symbols() {
        let mut w = DayRepairAuditWriter::for_test();
        let mut r = DayRepairAuditRow::now(
            "2026-11-20",
            DayRepairStep::Preflight,
            "",
            DayRepairOutcome::Refused,
            "",
        );
        r.detail = "app is running".to_string();
        w.append_row(&r).expect("append");
        let line = String::from_utf8(w.buffer.as_bytes().to_vec()).unwrap_or_default();
        assert!(line.contains("object=-"), "{line}");
        assert!(line.contains("run_id=-"), "{line}");
        assert!(line.contains("outcome=refused"), "{line}");
        assert!(!line.contains("row_count"), "NULL omitted: {line}");
        assert!(!line.contains("byte_count"), "NULL omitted: {line}");
        assert!(line.contains("detail=\"app is running\""), "{line}");
    }

    #[test]
    fn test_append_row_refuses_a_negative_timestamp() {
        let mut w = DayRepairAuditWriter::for_test();
        let mut r = sample();
        r.ts_ist_nanos = -1;
        assert!(w.append_row(&r).is_err());
    }

    #[test]
    fn test_bounded_str_cuts_on_a_char_boundary() {
        assert_eq!(bounded_str("abc", 10), "abc");
        let s = "é".repeat(10);
        let cut = bounded_str(&s, 5);
        assert!(cut.len() <= 5);
        assert!(s.starts_with(cut));
        assert_eq!(bounded_str("", 0), "");
        let mut r = sample();
        r.detail = "x".repeat(DAY_REPAIR_DETAIL_MAX_BYTES * 2);
        let mut w = DayRepairAuditWriter::for_test();
        w.append_row(&r).expect("append");
        let line = String::from_utf8(w.buffer.as_bytes().to_vec()).unwrap_or_default();
        assert!(line.contains(&"x".repeat(DAY_REPAIR_DETAIL_MAX_BYTES)));
        assert!(!line.contains(&"x".repeat(DAY_REPAIR_DETAIL_MAX_BYTES + 1)));
    }

    #[test]
    fn test_flush_without_sender_discards_and_errs() {
        let mut w = DayRepairAuditWriter::for_test();
        assert!(w.flush().is_ok(), "nothing pending is a no-op");
        w.append_row(&sample()).expect("append");
        assert!(w.flush().is_err());
        assert_eq!(w.pending(), 0, "a failed flush leaves nothing buffered");
        assert!(w.append_and_flush(&sample()).is_err());
        assert_eq!(w.pending(), 0);
    }

    #[test]
    fn test_append_and_flush_refused_append_leaves_nothing_buffered() {
        let mut w = DayRepairAuditWriter::for_test();
        let mut r = sample();
        r.ts_ist_nanos = -5;
        assert!(w.append_and_flush(&r).is_err());
        assert_eq!(w.pending(), 0);
    }

    #[test]
    fn test_day_repair_now_ist_nanos_and_now_row_are_stamped_in_ist() {
        let before = chrono::Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or(0)
            .saturating_add(IST_UTC_OFFSET_NANOS);
        let r = DayRepairAuditRow::now(
            "2026-11-20",
            DayRepairStep::Done,
            "2026-11-20",
            DayRepairOutcome::Ok,
            "abc",
        );
        assert!(r.ts_ist_nanos >= before);
        assert!(r.row_count.is_none() && r.byte_count.is_none());
        assert!(r.sha256.is_empty() && r.s3_key.is_empty() && r.detail.is_empty());
    }

    #[tokio::test]
    async fn test_ensure_day_repair_audit_table_returns_false_when_questdb_is_unreachable() {
        let cfg = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        let done = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            ensure_day_repair_audit_table(&cfg),
        )
        .await;
        assert_eq!(done.ok(), Some(false), "returns, and says not ensured");
    }
}

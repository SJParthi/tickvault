//! Per-table OBSERVED disk usage (`table_storage_daily`).
//!
//! ## The gap this closes
//!
//! `dhan-rest-only-noise-lock-2026-07-14.md` §2.3o-i records it in one line:
//! *"No per-table byte metric exists anywhere … every attribution above is
//! derived rather than observed. Nobody can say from telemetry where the
//! 138 GB went."*
//!
//! That is exactly right, and it is why the disk-burn table in that section
//! carries row widths and modelled row counts instead of measurements. A
//! number derived from `sizeof(row) × assumed rows/day` is an argument, not
//! evidence — and this repository has been bitten repeatedly by treating the
//! two as interchangeable.
//!
//! QuestDB already knows the answer. `table_partitions()` reports a
//! `diskSize` per partition, and the partition manager has queried that
//! function in production since the retention sweep shipped. This module sums
//! it per table, once a day, into a queryable row.
//!
//! ## Deliberately NOT a CloudWatch metric
//!
//! Roughly thirty tables would mean roughly thirty new metric names at
//! ~$0.30/mo each, against a budget the noise lock already records as ~$8
//! above the automatic `STOP_EC2_INSTANCES` line in a maximal month. The
//! operator asked for this saved to the DB and made analysable; the DB is
//! where it goes, at zero recurring cost. Charting it later is a decision
//! with a price tag attached, and that decision is not taken here.
//!
//! ## Complexity
//!
//! One query per table, once per day, on the post-close cold path: O(tables ×
//! partitions) over a bounded set, nothing on the tick hot path. One row per
//! table per day, reached by its DEDUP key — so "how big was `market_depth`
//! on the 14th?" is a single keyed read.
//!
//! ## Fail LOUD, never fail to zero
//!
//! `diskSize` is resolved BY NAME from the response's own column metadata, so
//! a projection reorder cannot silently shift the reading, and a QuestDB
//! version that renames or drops the column yields `None` — reported with a
//! coded error — rather than a confident 0. A fabricated zero here would be
//! worse than no table at all: it would say a table takes no space, which is
//! precisely the false-OK this codebase keeps having to retire.
//!
//! ## Table
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS table_storage_daily (
//!     ts               TIMESTAMP,  -- deterministic IST-midnight daily ts
//!     trading_date_ist TIMESTAMP,  -- the trading day (IST midnight)
//!     table_name       SYMBOL,     -- the measured table
//!     disk_bytes       LONG,       -- summed diskSize across its partitions
//!     partition_count  LONG,       -- how many partitions that sum covered
//!     row_count        LONG,       -- summed numRows, -1 when unavailable
//!     measured         BOOLEAN     -- false = the probe could not read a size
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, trading_date_ist, table_name);
//! ```

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::config::QuestDbConfig;

/// QuestDB table name. One row per measured table per day.
pub const TABLE_STORAGE_DAILY_TABLE: &str = "table_storage_daily";

/// DEDUP UPSERT key — designated `ts` first (2026-04-28 regression rule),
/// then the day and the table being measured.
pub const DEDUP_KEY_TABLE_STORAGE_DAILY: &str = "ts, trading_date_ist, table_name";

/// `-1` sentinel for "not measured", never a fabricated 0. Mirrors
/// `SCOREBOARD_UNAVAILABLE_SENTINEL` for the same reason: a zero byte count
/// and an unreadable byte count are different facts.
pub const STORAGE_UNAVAILABLE_SENTINEL: i64 = -1;

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Every column, in DDL order. Single source of truth for the `CREATE` and
/// the per-column self-heal `ALTER`s, so the two cannot drift.
pub const TABLE_STORAGE_DAILY_COLUMNS: &[(&str, &str)] = &[
    ("ts", "TIMESTAMP"),
    ("trading_date_ist", "TIMESTAMP"),
    ("table_name", "SYMBOL"),
    ("disk_bytes", "LONG"),
    ("partition_count", "LONG"),
    ("row_count", "LONG"),
    ("measured", "BOOLEAN"),
];

/// One table's measured footprint on a given day.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TableStorageRow {
    /// Deterministic daily timestamp — IST midnight nanoseconds.
    pub ts_ist_nanos: i64,
    /// The trading day — IST midnight nanoseconds.
    pub trading_date_ist_nanos: i64,
    /// The measured table.
    pub table_name: String,
    /// Summed `diskSize` across the table's partitions, or the sentinel.
    pub disk_bytes: i64,
    /// How many partitions that sum covered, or the sentinel.
    pub partition_count: i64,
    /// Summed `numRows`, or the sentinel when the column is unavailable.
    pub row_count: i64,
    /// `false` = the probe could not read a size. The counts above are then
    /// sentinels, and the row exists to say the measurement was ATTEMPTED and
    /// failed — which is a different fact from the table being absent.
    pub measured: bool,
}

/// The parsed result of one `table_partitions()` probe.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TableStorage {
    /// Summed `diskSize` across partitions.
    pub disk_bytes: i64,
    /// Number of partition rows summed.
    pub partition_count: i64,
    /// Summed `numRows`, or [`STORAGE_UNAVAILABLE_SENTINEL`] when that column
    /// is absent. Rows are a nice-to-have; bytes are the point.
    pub row_count: i64,
}

/// The probe SQL for one table.
///
/// `SELECT *` rather than a named projection, deliberately: the column set of
/// `table_partitions()` differs across QuestDB versions, and naming columns
/// that a future version drops turns a degraded reading into a hard query
/// error. Taking everything and resolving BY NAME degrades gracefully instead.
///
/// `table` is always a trusted compile-time or derived constant (the same
/// class the partition manager passes), never external input.
#[must_use]
pub fn build_table_storage_sql(table: &str) -> String {
    format!("SELECT * FROM table_partitions('{table}')")
}

/// Resolve a column index by name from a QuestDB `/exec` response's own
/// metadata. Case-sensitive: QuestDB reports `diskSize`, and matching
/// loosely would risk pairing with a differently-meaning column.
fn column_index(columns: &[serde_json::Value], want: &str) -> Option<usize> {
    columns
        .iter()
        .position(|c| c.get("name").and_then(|n| n.as_str()) == Some(want))
}

/// Parse a `table_partitions()` response into a summed footprint.
///
/// Returns `None` when the response is unparsable OR carries no `diskSize`
/// column — the two cases a caller must report rather than record as zero.
/// An EMPTY dataset with a valid `diskSize` column is `Some(zero)`: a table
/// with no partitions genuinely occupies nothing, and that is a measurement.
#[must_use]
pub fn parse_table_storage(json: &str) -> Option<TableStorage> {
    let parsed: serde_json::Value = serde_json::from_str(json).ok()?;
    let columns = parsed.get("columns")?.as_array()?;
    let size_idx = column_index(columns, "diskSize")?;
    // Rows are optional detail — their absence must not discard the bytes.
    let rows_idx = column_index(columns, "numRows");
    let dataset = parsed.get("dataset")?.as_array()?;

    let mut disk_bytes = 0_i64;
    let mut partition_count = 0_i64;
    let mut row_count = 0_i64;
    let mut any_rows_read = false;

    for row in dataset {
        let Some(cols) = row.as_array() else {
            continue;
        };
        // A partition whose size cell is null or non-numeric is COUNTED but
        // contributes no bytes — the alternative is discarding a whole
        // table's measurement over one odd partition.
        let bytes = cols.get(size_idx).and_then(serde_json::Value::as_i64);
        partition_count = partition_count.saturating_add(1);
        if let Some(b) = bytes {
            disk_bytes = disk_bytes.saturating_add(b.max(0));
        }
        if let Some(ri) = rows_idx
            && let Some(n) = cols.get(ri).and_then(serde_json::Value::as_i64)
        {
            row_count = row_count.saturating_add(n.max(0));
            any_rows_read = true;
        }
    }

    Some(TableStorage {
        disk_bytes,
        partition_count,
        row_count: if any_rows_read {
            row_count
        } else {
            STORAGE_UNAVAILABLE_SENTINEL
        },
    })
}

/// Build the persisted row for one table from a probe result.
///
/// `None` (the probe could not read a size) becomes a row with sentinels and
/// `measured = false` — never a zero-byte row. The row is still WRITTEN,
/// because "we tried and could not read it" is information the next reader
/// needs, and an absent row would be indistinguishable from a table that does
/// not exist.
#[must_use]
pub fn storage_row(
    table_name: &str,
    probe: Option<TableStorage>,
    day_ist_midnight_nanos: i64,
) -> TableStorageRow {
    match probe {
        Some(s) => TableStorageRow {
            ts_ist_nanos: day_ist_midnight_nanos,
            trading_date_ist_nanos: day_ist_midnight_nanos,
            table_name: table_name.to_string(),
            disk_bytes: s.disk_bytes,
            partition_count: s.partition_count,
            row_count: s.row_count,
            measured: true,
        },
        None => TableStorageRow {
            ts_ist_nanos: day_ist_midnight_nanos,
            trading_date_ist_nanos: day_ist_midnight_nanos,
            table_name: table_name.to_string(),
            disk_bytes: STORAGE_UNAVAILABLE_SENTINEL,
            partition_count: STORAGE_UNAVAILABLE_SENTINEL,
            row_count: STORAGE_UNAVAILABLE_SENTINEL,
            measured: false,
        },
    }
}

/// Total measured bytes across rows, SKIPPING unmeasured ones.
///
/// Summing the `-1` sentinels would silently subtract a byte per failed
/// probe from the headline figure — small, wrong, and invisible. Returns
/// `(total_bytes, measured_count, unmeasured_count)` so a caller can never
/// present a total without knowing how complete it is.
#[must_use]
pub fn measured_total(rows: &[TableStorageRow]) -> (i64, usize, usize) {
    let mut total = 0_i64;
    let mut measured = 0_usize;
    let mut unmeasured = 0_usize;
    for r in rows {
        if r.measured && r.disk_bytes >= 0 {
            total = total.saturating_add(r.disk_bytes);
            measured += 1;
        } else {
            unmeasured += 1;
        }
    }
    (total, measured, unmeasured)
}

/// The idempotent `CREATE TABLE` DDL, generated from
/// [`TABLE_STORAGE_DAILY_COLUMNS`]. Pure (testable without QuestDB).
#[must_use]
pub fn table_storage_daily_create_ddl() -> String {
    let cols = TABLE_STORAGE_DAILY_COLUMNS
        .iter()
        .map(|(name, ty)| format!("{name} {ty}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "CREATE TABLE IF NOT EXISTS {TABLE_STORAGE_DAILY_TABLE} ({cols}) \
         timestamp(ts) PARTITION BY DAY \
         DEDUP UPSERT KEYS({DEDUP_KEY_TABLE_STORAGE_DAILY});"
    )
}

/// `CREATE` → per-column `ALTER ADD COLUMN IF NOT EXISTS` → `DEDUP ENABLE`.
/// Never a drop. Pure.
#[must_use]
pub fn table_storage_daily_ddl_statements() -> Vec<String> {
    let mut statements = vec![table_storage_daily_create_ddl()];
    for (col, ty) in TABLE_STORAGE_DAILY_COLUMNS {
        if *col == "ts" {
            continue;
        }
        statements.push(format!(
            "ALTER TABLE {TABLE_STORAGE_DAILY_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty};"
        ));
    }
    statements.push(format!(
        "ALTER TABLE {TABLE_STORAGE_DAILY_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_TABLE_STORAGE_DAILY});"
    ));
    statements
}

/// Create the table if absent (idempotent, schema-self-heal pattern).
/// Failures log at `error!` but never block the caller.
// TEST-EXEMPT: live-QuestDB DDL runner (DDL string unit-tested via table_storage_daily_ddl_statements tests)
pub async fn ensure_table_storage_daily_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            error!(
                code = "SCOREBOARD-01",
                stage = "ensure_client_build",
                ?err,
                "SCOREBOARD-01: HTTP client build failed — table_storage_daily not \
                 ensured (first ILP write may auto-create it WITHOUT dedup)"
            );
            return;
        }
    };
    for ddl in &table_storage_daily_ddl_statements() {
        match client
            .get(&base_url)
            .query(&[("query", ddl.as_str())])
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                error!(
                    code = "SCOREBOARD-01",
                    stage = "ensure_ddl",
                    %status,
                    ddl = ddl.as_str(),
                    body = %body.chars().take(200).collect::<String>(),
                    "SCOREBOARD-01: table_storage_daily DDL returned non-2xx"
                );
            }
            Err(err) => error!(
                code = "SCOREBOARD-01",
                stage = "ensure_ddl",
                ?err,
                ddl = ddl.as_str(),
                "SCOREBOARD-01: table_storage_daily DDL request failed"
            ),
        }
    }
}

/// Lazy ILP-over-HTTP writer for `table_storage_daily`.
pub struct TableStorageDailyWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl TableStorageDailyWriter {
    /// Production constructor — ILP-over-HTTP, lazy on failure.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (needs live QuestDB); append/flush covered via for_test()
    pub fn new(config: &QuestDbConfig) -> Self {
        let conf = format!("http::addr={}:{};", config.host, config.http_port);
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
                    "table_storage_daily writer: QuestDB unreachable — buffering locally"
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
    #[must_use]
    // TEST-EXEMPT: test-only helper used by the append/flush unit tests below.
    pub fn for_test() -> Self {
        Self {
            sender: None,
            buffer: Buffer::new(ProtocolVersion::V1),
            pending: 0,
        }
    }

    /// Rows appended but not yet flushed.
    #[must_use]
    // TEST-EXEMPT: observability accessor, exercised by the append tests below.
    pub fn pending(&self) -> usize {
        self.pending
    }

    #[cfg(test)]
    fn buffer_utf8(&self) -> String {
        String::from_utf8(self.buffer.as_bytes().to_vec()).unwrap_or_default()
    }

    /// Append one measured table-day row.
    ///
    /// # Errors
    /// Propagates ILP buffer errors.
    pub fn append_row(&mut self, r: &TableStorageRow) -> Result<()> {
        self.buffer
            .table(TABLE_STORAGE_DAILY_TABLE)
            .context("table")?
            .symbol("table_name", r.table_name.as_str())
            .context("table_name")?
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(r.trading_date_ist_nanos),
            )
            .context("trading_date_ist")?
            .column_i64("disk_bytes", r.disk_bytes)
            .context("disk_bytes")?
            .column_i64("partition_count", r.partition_count)
            .context("partition_count")?
            .column_i64("row_count", r.row_count)
            .context("row_count")?
            .column_bool("measured", r.measured)
            .context("measured")?
            .at(TimestampNanos::new(r.ts_ist_nanos))
            .context("designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Flush buffered rows over ILP-HTTP (per-flush server ACK).
    ///
    /// # Errors
    /// `Err` when disconnected or the flush fails (rows stay buffered).
    pub fn flush(&mut self) -> Result<()> {
        if self.pending == 0 {
            return Ok(());
        }
        let Some(sender) = self.sender.as_mut() else {
            anyhow::bail!("table_storage_daily: no ILP sender (QuestDB unreachable)");
        };
        if let Err(err) = sender.flush(&mut self.buffer) {
            let dropped = crate::ilp_overflow::discard_if_overflowing(
                &mut self.buffer,
                &mut self.pending,
                "table_storage_daily",
            );
            return Err(anyhow::Error::new(err).context(
                crate::ilp_overflow::flush_failure_context(
                    "table_storage_daily ILP flush",
                    dropped,
                ),
            ));
        }
        self.pending = 0;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 1_769_990_400_000_000_000;

    fn body(columns: &str, dataset: &str) -> String {
        format!("{{\"columns\":[{columns}],\"dataset\":[{dataset}]}}")
    }

    #[test]
    fn test_build_table_storage_sql_quotes_the_table_and_takes_every_column() {
        let sql = build_table_storage_sql("market_depth");
        assert_eq!(sql, "SELECT * FROM table_partitions('market_depth')");
        assert!(
            sql.contains("SELECT *"),
            "a named projection would hard-error on a QuestDB version that \
             dropped one of the columns; SELECT * degrades to a missing name"
        );
    }

    #[test]
    fn test_parse_table_storage_sums_disk_size_across_partitions() {
        let json = body(
            r#"{"name":"name","type":"VARCHAR"},{"name":"diskSize","type":"LONG"},{"name":"numRows","type":"LONG"}"#,
            r#"["2026-08-28",1000,10],["2026-08-29",2500,25]"#,
        );
        let s = parse_table_storage(&json).expect("parsed");
        assert_eq!(s.disk_bytes, 3500);
        assert_eq!(s.partition_count, 2);
        assert_eq!(s.row_count, 35);
    }

    #[test]
    fn test_parse_table_storage_finds_disk_size_by_name_not_position() {
        // A projection reorder must not shift the reading onto another column.
        let json = body(
            r#"{"name":"diskSize","type":"LONG"},{"name":"numRows","type":"LONG"},{"name":"name","type":"VARCHAR"}"#,
            r#"[4096,7,"2026-08-29"]"#,
        );
        let s = parse_table_storage(&json).expect("parsed");
        assert_eq!(s.disk_bytes, 4096);
        assert_eq!(s.row_count, 7);
    }

    #[test]
    fn test_a_missing_disk_size_column_is_none_not_zero() {
        // THE POINT of this module. A QuestDB version that renames or drops
        // the column must yield "unreadable", never "this table takes no
        // space" — the false-OK class this codebase keeps having to retire.
        let json = body(
            r#"{"name":"name","type":"VARCHAR"},{"name":"numRows","type":"LONG"}"#,
            r#"["2026-08-29",25]"#,
        );
        assert_eq!(parse_table_storage(&json), None);
    }

    #[test]
    fn test_unparsable_or_shapeless_bodies_are_none() {
        assert_eq!(parse_table_storage("not json"), None);
        assert_eq!(parse_table_storage(r#"{"dataset":[]}"#), None, "no columns");
        assert_eq!(
            parse_table_storage(r#"{"columns":[{"name":"diskSize","type":"LONG"}]}"#),
            None,
            "no dataset"
        );
    }

    #[test]
    fn test_an_empty_partition_list_is_a_real_zero_measurement() {
        // Distinct from the case above: the column EXISTS and the table has no
        // partitions. That genuinely is zero bytes, and recording it as
        // measured is correct.
        let json = body(r#"{"name":"diskSize","type":"LONG"}"#, "");
        let s = parse_table_storage(&json).expect("parsed");
        assert_eq!(s.disk_bytes, 0);
        assert_eq!(s.partition_count, 0);
        assert_eq!(
            s.row_count, STORAGE_UNAVAILABLE_SENTINEL,
            "no numRows column read, so rows are unavailable rather than 0"
        );
    }

    #[test]
    fn test_a_null_size_cell_counts_the_partition_but_adds_no_bytes() {
        // Discarding a whole table's measurement over one odd partition would
        // lose more than it protects.
        let json = body(
            r#"{"name":"diskSize","type":"LONG"}"#,
            r#"[1000],[null],[500]"#,
        );
        let s = parse_table_storage(&json).expect("parsed");
        assert_eq!(s.disk_bytes, 1500);
        assert_eq!(s.partition_count, 3, "the odd partition is still counted");
    }

    #[test]
    fn test_a_negative_size_never_reduces_the_total() {
        let json = body(r#"{"name":"diskSize","type":"LONG"}"#, r#"[1000],[-9999]"#);
        let s = parse_table_storage(&json).expect("parsed");
        assert_eq!(s.disk_bytes, 1000);
    }

    #[test]
    fn test_missing_num_rows_leaves_the_sentinel_and_keeps_the_bytes() {
        let json = body(r#"{"name":"diskSize","type":"LONG"}"#, r#"[8192]"#);
        let s = parse_table_storage(&json).expect("parsed");
        assert_eq!(s.disk_bytes, 8192, "bytes survive a missing rows column");
        assert_eq!(s.row_count, STORAGE_UNAVAILABLE_SENTINEL);
    }

    #[test]
    fn test_storage_row_marks_an_unreadable_probe_unmeasured_with_sentinels() {
        let r = storage_row("ticks", None, DAY);
        assert!(!r.measured);
        assert_eq!(r.disk_bytes, STORAGE_UNAVAILABLE_SENTINEL);
        assert_eq!(r.partition_count, STORAGE_UNAVAILABLE_SENTINEL);
        assert_eq!(r.row_count, STORAGE_UNAVAILABLE_SENTINEL);
        assert_eq!(r.ts_ist_nanos, DAY, "re-runs must UPSERT, not duplicate");
        assert_eq!(r.trading_date_ist_nanos, DAY);
    }

    #[test]
    fn test_storage_row_carries_a_real_measurement_through() {
        let r = storage_row(
            "market_depth",
            Some(TableStorage {
                disk_bytes: 110_000_000_000,
                partition_count: 9,
                row_count: 1_530_651_649,
            }),
            DAY,
        );
        assert!(r.measured);
        assert_eq!(r.disk_bytes, 110_000_000_000);
        assert_eq!(r.partition_count, 9);
        assert_eq!(r.row_count, 1_530_651_649);
    }

    #[test]
    fn test_measured_total_skips_sentinels_instead_of_subtracting_them() {
        // Summing the -1s would quietly shave a byte per failed probe off the
        // headline figure: small, wrong, and invisible.
        let rows = vec![
            storage_row(
                "a",
                Some(TableStorage {
                    disk_bytes: 100,
                    partition_count: 1,
                    row_count: 1,
                }),
                DAY,
            ),
            storage_row("b", None, DAY),
            storage_row(
                "c",
                Some(TableStorage {
                    disk_bytes: 250,
                    partition_count: 2,
                    row_count: 2,
                }),
                DAY,
            ),
        ];
        let (total, measured, unmeasured) = measured_total(&rows);
        assert_eq!(total, 350);
        assert_eq!(measured, 2);
        assert_eq!(
            unmeasured, 1,
            "a caller can never present a total without its completeness"
        );
    }

    #[test]
    fn test_measured_total_of_nothing_is_zero_measured_zero() {
        let (total, measured, unmeasured) = measured_total(&[]);
        assert_eq!((total, measured, unmeasured), (0, 0, 0));
    }

    #[test]
    fn test_table_storage_daily_ddl_statements_create_then_alter_then_enable_dedup() {
        let s = table_storage_daily_ddl_statements();
        assert!(
            s.first()
                .is_some_and(|x| x.contains("CREATE TABLE IF NOT EXISTS")),
            "CREATE must come first"
        );
        assert!(
            s.last()
                .is_some_and(|x| x.contains("DEDUP ENABLE UPSERT KEYS")),
            "DEDUP ENABLE must come last"
        );
        let joined = s.join("\n");
        for (col, ty) in TABLE_STORAGE_DAILY_COLUMNS {
            if *col == "ts" {
                continue;
            }
            assert!(
                joined.contains(&format!("ADD COLUMN IF NOT EXISTS {col} {ty}")),
                "column `{col}` has no self-heal ALTER"
            );
        }
        assert!(
            !joined.to_ascii_uppercase().contains("DROP "),
            "no DDL statement may DROP anything"
        );
    }

    #[test]
    fn test_dedup_key_columns_all_exist_in_the_column_list() {
        for part in DEDUP_KEY_TABLE_STORAGE_DAILY.split(',') {
            let name = part.trim();
            assert!(
                TABLE_STORAGE_DAILY_COLUMNS.iter().any(|(c, _)| *c == name),
                "DEDUP key names `{name}`, which is not a declared column"
            );
        }
        assert!(
            DEDUP_KEY_TABLE_STORAGE_DAILY
                .trim_start()
                .starts_with("ts,"),
            "designated timestamp must lead the DEDUP key"
        );
    }

    #[test]
    fn test_append_row_writes_the_identity_and_the_measurement() {
        let mut w = TableStorageDailyWriter::for_test();
        w.append_row(&storage_row(
            "market_depth",
            Some(TableStorage {
                disk_bytes: 110_000_000_000,
                partition_count: 9,
                row_count: 42,
            }),
            DAY,
        ))
        .expect("append");
        assert_eq!(w.pending(), 1);
        let line = w.buffer_utf8();
        assert!(line.contains(TABLE_STORAGE_DAILY_TABLE));
        assert!(line.contains("table_name=market_depth"));
        assert!(line.contains("disk_bytes=110000000000"));
        assert!(line.contains("partition_count=9"));
        assert!(line.contains("measured=t"));
    }

    #[test]
    fn test_append_row_writes_an_unmeasured_row_rather_than_skipping_it() {
        // "We tried and could not read it" must reach the table. An absent row
        // is indistinguishable from a table that does not exist.
        let mut w = TableStorageDailyWriter::for_test();
        w.append_row(&storage_row("ticks", None, DAY))
            .expect("append");
        let line = w.buffer_utf8();
        assert!(line.contains("measured=f"));
        assert!(line.contains("disk_bytes=-1"));
    }

    #[test]
    fn test_flush_without_sender_errors_and_retains_rows() {
        let mut w = TableStorageDailyWriter::for_test();
        w.append_row(&storage_row("ticks", None, DAY))
            .expect("append");
        let err = w
            .flush()
            .expect_err("disconnected writer must not report success");
        assert!(err.to_string().contains("no ILP sender"));
        assert_eq!(w.pending(), 1, "rows retained, never silently dropped");
    }

    #[test]
    fn test_flush_with_nothing_pending_is_a_noop_success() {
        let mut w = TableStorageDailyWriter::for_test();
        assert!(w.flush().is_ok());
    }

    #[test]
    fn test_table_storage_daily_create_ddl_declares_every_column_and_the_key() {
        let ddl = table_storage_daily_create_ddl();
        for (col, ty) in TABLE_STORAGE_DAILY_COLUMNS {
            assert!(
                ddl.contains(&format!("{col} {ty}")),
                "CREATE is missing `{col} {ty}`"
            );
        }
        assert!(ddl.contains("CREATE TABLE IF NOT EXISTS"));
        assert!(ddl.contains("timestamp(ts) PARTITION BY DAY"));
        assert!(ddl.contains(&format!(
            "DEDUP UPSERT KEYS({DEDUP_KEY_TABLE_STORAGE_DAILY})"
        )));
        assert!(
            !ddl.to_ascii_uppercase().contains("DROP "),
            "the CREATE must never drop anything"
        );
    }
}

//! Puts the `contract` name back on market-data rows a boot rewrote without
//! one.
//!
//! ## Why this exists (9 Oct 2026)
//!
//! The candle, tick and aux writers fill `contract` from the process-wide name
//! table ([`crate::candle_contract_labels`]). A row whose id is not in that
//! table is written with a NULL name, and because the tables DEDUP on a key
//! that does not include `contract`, a rewrite of an existing row replaces the
//! whole row, name included. The two evening redeploys on 9 Oct replayed about
//! two million frames while the table held only the 862 spot names, so the
//! option rows they rewrote lost their names, and a query by name showed gaps
//! (ITC-27Oct2026-255-CE read rows at 09:15, 09:21–09:23 and from 09:38 only).
//! The boot fix makes a replay name its rows; this module repairs the rows
//! that were already written blank.
//!
//! ## How
//!
//! QuestDB 9.3.5 refuses `UPDATE … FROM` on WAL tables ("UPDATE statements
//! with join are not supported yet for WAL tables", checked against a local
//! 9.3.5 on 2026-10-10). So, per day and per table:
//!
//! 1. the day's names go into [`CONTRACT_NAME_REPAIR_MAP_TABLE`], one row per
//!    `(security_id, segment)` (multi-row `INSERT … VALUES`, batched);
//! 2. the repair waits until that table's WAL has applied those inserts;
//! 3. one `INSERT INTO t (every column) SELECT …, m.contract, … FROM t JOIN
//!    map …  WHERE contract IS NULL` re-inserts each blank row with its name.
//!    Every other column is copied by the database itself, so no value passes
//!    through this process, and the table's DEDUP key replaces the blank row
//!    in place. A table without DEDUP is never touched: the same statement
//!    would duplicate its rows.
//!
//! One statement per table per day keeps the out-of-order merge to one per
//! partition. Only rows of feed `dhan` with a NULL name and an id the day's
//! own files name are touched; an id no file names stays NULL, as the writer
//! itself would leave it. `market_depth` is NOT repaired: its day partition
//! holds about 1.5 billion rows, and rewriting the blank share of it is not
//! worth the disk churn on a volume that was at 98% on 9 Oct.
//!
//! Cold path: runs from the app's once-a-day after-close task, never on the
//! frame drain.

use std::time::Duration;

use reqwest::Client;
use tracing::{info, warn};

/// The per-day name table the repair joins against. Small: one row per named
/// instrument per repaired day (about 26,000).
pub const CONTRACT_NAME_REPAIR_MAP_TABLE: &str = "contract_name_repair_map";

/// DEDUP key of [`CONTRACT_NAME_REPAIR_MAP_TABLE`]: a re-run of the same day
/// replaces each name in place instead of adding a second row, which would
/// make the join emit every blank row twice.
pub const DEDUP_KEY_CONTRACT_NAME_REPAIR_MAP: &str = "ts, security_id, segment, feed";

/// The only feed whose rows are repaired; every live writer stamps it.
pub const CONTRACT_NAME_REPAIR_FEED: &str = "dhan";

/// Name rows per `INSERT … VALUES`. A row is about 70 characters, so 200 keep
/// the URL-encoded statement near 20 KB, well under QuestDB's default 64 KB
/// request header buffer (`http.request.header.buffer.size`; `/exec` takes
/// GET only, so the statement travels in the URL).
pub const CONTRACT_NAME_REPAIR_MAP_BATCH_ROWS: usize = 200;

/// Ceiling on one repair statement. A day of `ticks` is tens of millions of
/// rows and the select runs inside the request.
pub const CONTRACT_NAME_REPAIR_REQUEST_TIMEOUT_SECS: u64 = 600;

/// How long the repair waits for the name table's WAL to apply its inserts
/// before giving up on the day.
pub const CONTRACT_NAME_REPAIR_APPLY_WAIT_SECS: u64 = 600;

/// Poll interval while waiting for the name table to apply.
const APPLY_POLL_SECS: u64 = 5;

/// One name to put back: the row identity and the name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NameRow<'a> {
    /// The row's `security_id`.
    pub security_id: i64,
    /// The row's `segment` string (`NSE_FNO`, `IDX_I`, ...).
    pub segment: &'a str,
    /// The name to write into `contract`.
    pub contract: &'a str,
}

/// What one day's repair did, for the caller's log line and metrics.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RepairDayTally {
    /// Rows re-inserted with their name (counted before the insert).
    pub rows_restored: u64,
    /// Tables repaired (at least one row).
    pub tables_repaired: u32,
    /// Tables with no blank row the day's names cover.
    pub tables_clean: u32,
    /// Tables skipped: absent, no DEDUP, not WAL, or missing a needed column.
    pub tables_skipped: u32,
    /// Statements QuestDB refused or that did not answer.
    pub failures: u32,
}

/// `true` when `day` is exactly `YYYY-MM-DD` in ASCII digits. Every SQL string
/// below interpolates the day, so anything else is refused before it is used.
#[must_use]
pub fn is_valid_day(day: &str) -> bool {
    let b = day.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

/// `true` for a bare table or column name this module will interpolate.
#[must_use]
pub fn is_plain_identifier(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

/// A SQL string literal: single quotes doubled.
fn sql_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push('\'');
        }
        out.push(ch);
    }
    out.push('\'');
    out
}

/// CREATE statement for [`CONTRACT_NAME_REPAIR_MAP_TABLE`].
#[must_use]
pub fn map_table_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {CONTRACT_NAME_REPAIR_MAP_TABLE} (\
         ts TIMESTAMP, feed SYMBOL, segment SYMBOL, security_id LONG, contract SYMBOL\
         ) timestamp(ts) PARTITION BY DAY WAL \
         DEDUP UPSERT KEYS({DEDUP_KEY_CONTRACT_NAME_REPAIR_MAP})"
    )
}

/// One multi-row insert of `rows` for `day` (caller has validated the day).
/// `None` for an empty batch.
#[must_use]
pub fn map_insert_sql(day: &str, rows: &[NameRow<'_>]) -> Option<String> {
    if rows.is_empty() {
        return None;
    }
    let mut sql = format!("INSERT INTO {CONTRACT_NAME_REPAIR_MAP_TABLE} VALUES ");
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            sql.push_str(", ");
        }
        sql.push_str(&format!(
            "('{day}T00:00:00.000000Z', '{CONTRACT_NAME_REPAIR_FEED}', {}, {}, {})",
            sql_string(row.segment),
            row.security_id,
            sql_string(row.contract)
        ));
    }
    Some(sql)
}

/// The WAL progress of the name table: applied txn, committed txn, suspended.
#[must_use]
pub fn map_wal_progress_sql() -> String {
    format!(
        "SELECT writerTxn, sequencerTxn, suspended FROM wal_tables() \
         WHERE name = '{CONTRACT_NAME_REPAIR_MAP_TABLE}'"
    )
}

/// Whether `table` exists with DEDUP and WAL on.
#[must_use]
pub fn table_flags_sql(table: &str) -> String {
    format!("SELECT dedup, walEnabled FROM tables() WHERE table_name = '{table}'")
}

/// The column names of `table`, in table order.
#[must_use]
pub fn table_columns_sql(table: &str) -> String {
    format!("SELECT \"column\" FROM table_columns('{table}')")
}

/// The `FROM … JOIN … WHERE …` both the count and the repair share: every
/// `dhan` row of `day` with a NULL name whose id the day's names cover.
fn blank_named_rows_source(table: &str, day: &str) -> String {
    format!(
        "FROM {table} c JOIN (SELECT security_id, segment, contract \
         FROM {CONTRACT_NAME_REPAIR_MAP_TABLE} \
         WHERE ts IN '{day}' AND feed = '{CONTRACT_NAME_REPAIR_FEED}') m \
         ON c.security_id = m.security_id AND c.segment = m.segment \
         WHERE c.ts IN '{day}' AND c.contract IS NULL AND c.feed = '{CONTRACT_NAME_REPAIR_FEED}'"
    )
}

/// How many rows the repair of `table` for `day` would name.
#[must_use]
pub fn blank_named_count_sql(table: &str, day: &str) -> String {
    format!("SELECT count() {}", blank_named_rows_source(table, day))
}

/// The repair statement: re-insert every blank row with its name, copying
/// every other column as stored. `None` when a column the join needs is
/// missing or a column name is not a plain identifier.
#[must_use]
pub fn repair_insert_sql(table: &str, day: &str, columns: &[String]) -> Option<String> {
    for needed in ["ts", "feed", "segment", "security_id", "contract"] {
        if !columns.iter().any(|c| c == needed) {
            return None;
        }
    }
    if !columns.iter().all(|c| is_plain_identifier(c)) {
        return None;
    }
    let target: Vec<String> = columns.iter().map(|c| format!("\"{c}\"")).collect();
    let source: Vec<String> = columns
        .iter()
        .map(|c| {
            if c == "contract" {
                "m.contract".to_string()
            } else {
                format!("c.\"{c}\"")
            }
        })
        .collect();
    Some(format!(
        "INSERT INTO {table} ({}) SELECT {} {}",
        target.join(", "),
        source.join(", "),
        blank_named_rows_source(table, day)
    ))
}

/// The `dataset` rows of an `/exec` answer.
fn dataset(body: &str) -> Option<Vec<serde_json::Value>> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value.get("dataset")?.as_array().cloned()
}

/// The single LONG of a one-row, one-column answer (`count()`).
#[must_use]
pub fn parse_single_long(body: &str) -> Option<i64> {
    dataset(body)?.first()?.as_array()?.first()?.as_i64()
}

/// `(dedup, walEnabled)` from [`table_flags_sql`]; `None` when the table does
/// not exist or the answer does not parse.
#[must_use]
pub fn parse_table_flags(body: &str) -> Option<(bool, bool)> {
    let rows = dataset(body)?;
    let row = rows.first()?.as_array()?;
    Some((row.first()?.as_bool()?, row.get(1)?.as_bool()?))
}

/// `(writerTxn, sequencerTxn, suspended)` from [`map_wal_progress_sql`].
#[must_use]
pub fn parse_wal_progress(body: &str) -> Option<(i64, i64, bool)> {
    let rows = dataset(body)?;
    let row = rows.first()?.as_array()?;
    Some((
        row.first()?.as_i64()?,
        row.get(1)?.as_i64()?,
        row.get(2)?.as_bool()?,
    ))
}

/// Column names from [`table_columns_sql`]; `None` when empty or unparseable.
#[must_use]
pub fn parse_column_names(body: &str) -> Option<Vec<String>> {
    let names: Option<Vec<String>> = dataset(body)?
        .iter()
        .map(|row| Some(row.as_array()?.first()?.as_str()?.to_string()))
        .collect();
    names.filter(|n| !n.is_empty())
}

/// `true` when QuestDB answered with an `error` field.
fn answer_is_error(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .is_some_and(|v| v.get("error").is_some())
}

/// Run one statement. `Some(body)` on a 2xx answer without an `error` field.
async fn exec(client: &Client, exec_url: &str, sql: &str) -> Option<String> {
    match client.get(exec_url).query(&[("query", sql)]).send().await {
        Ok(resp) if resp.status().is_success() => {
            let body = resp.text().await.ok()?;
            if answer_is_error(&body) {
                warn!(answer = %body, "contract name repair: statement refused");
                None
            } else {
                Some(body)
            }
        }
        Ok(resp) => {
            warn!(status = %resp.status(), "contract name repair: statement refused");
            None
        }
        Err(err) => {
            warn!(?err, "contract name repair: statement got no answer");
            None
        }
    }
}

/// Load `names` into the name table for `day` and wait until QuestDB has
/// applied them. `false` when any step fails; the day is then left for the
/// next run.
async fn load_names(client: &Client, exec_url: &str, day: &str, names: &[NameRow<'_>]) -> bool {
    if exec(client, exec_url, &map_table_ddl()).await.is_none() {
        return false;
    }
    for batch in names.chunks(CONTRACT_NAME_REPAIR_MAP_BATCH_ROWS) {
        let Some(sql) = map_insert_sql(day, batch) else {
            continue;
        };
        if exec(client, exec_url, &sql).await.is_none() {
            return false;
        }
    }
    let deadline =
        tokio::time::Instant::now() + Duration::from_secs(CONTRACT_NAME_REPAIR_APPLY_WAIT_SECS);
    let mut target: Option<i64> = None;
    loop {
        let progress = exec(client, exec_url, &map_wal_progress_sql())
            .await
            .and_then(|b| parse_wal_progress(&b));
        if let Some((applied, committed, suspended)) = progress {
            if suspended {
                warn!(
                    table = CONTRACT_NAME_REPAIR_MAP_TABLE,
                    "contract name repair: name table WAL is suspended, day left for the next run"
                );
                return false;
            }
            // The committed txn read after the inserts includes them.
            let goal = *target.get_or_insert(committed);
            if applied >= goal {
                return true;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            warn!(
                day,
                wait_secs = CONTRACT_NAME_REPAIR_APPLY_WAIT_SECS,
                "contract name repair: name table not applied in time, day left for the next run"
            );
            return false;
        }
        tokio::time::sleep(Duration::from_secs(APPLY_POLL_SECS)).await;
    }
}

/// Repair one table for one day. Adds to `tally`.
async fn repair_table(
    client: &Client,
    exec_url: &str,
    table: &str,
    day: &str,
    tally: &mut RepairDayTally,
) {
    let flags = exec(client, exec_url, &table_flags_sql(table))
        .await
        .and_then(|b| parse_table_flags(&b));
    match flags {
        Some((true, true)) => {}
        Some((dedup, wal)) => {
            warn!(
                table,
                dedup,
                wal,
                "contract name repair: table skipped, a re-insert without DEDUP would duplicate rows"
            );
            tally.tables_skipped += 1;
            return;
        }
        None => {
            tally.tables_skipped += 1;
            return;
        }
    }
    let Some(columns) = exec(client, exec_url, &table_columns_sql(table))
        .await
        .and_then(|b| parse_column_names(&b))
    else {
        tally.failures += 1;
        return;
    };
    let Some(repair_sql) = repair_insert_sql(table, day, &columns) else {
        tally.tables_skipped += 1;
        return;
    };
    let Some(blank) = exec(client, exec_url, &blank_named_count_sql(table, day))
        .await
        .and_then(|b| parse_single_long(&b))
    else {
        tally.failures += 1;
        return;
    };
    if blank <= 0 {
        tally.tables_clean += 1;
        return;
    }
    if exec(client, exec_url, &repair_sql).await.is_none() {
        tally.failures += 1;
        return;
    }
    let rows = u64::try_from(blank).unwrap_or(0);
    tally.tables_repaired += 1;
    tally.rows_restored = tally.rows_restored.saturating_add(rows);
    metrics::counter!("tv_contract_name_repair_rows_total").increment(rows);
    info!(
        table,
        day, rows, "contract name repair: blank names put back"
    );
}

/// Repair every table in `tables` for `day` with the day's `names`.
///
/// `exec_url` is `http://host:port/exec`; `client` should carry a timeout of
/// at least [`CONTRACT_NAME_REPAIR_REQUEST_TIMEOUT_SECS`]. Only `dhan` rows
/// with a NULL `contract` whose `(security_id, segment)` is in `names` change.
///
/// O(names / batch) inserts plus a few statements per table; cold path.
pub async fn repair_contract_names_for_day(
    client: &Client,
    exec_url: &str,
    day: &str,
    names: &[NameRow<'_>],
    tables: &[&str],
) -> RepairDayTally {
    let mut tally = RepairDayTally::default();
    if !is_valid_day(day) || names.is_empty() {
        return tally;
    }
    if !load_names(client, exec_url, day, names).await {
        tally.failures += 1;
        return tally;
    }
    for table in tables {
        if !is_plain_identifier(table) {
            tally.tables_skipped += 1;
            continue;
        }
        repair_table(client, exec_url, table, day, &mut tally).await;
    }
    tally
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn test_is_valid_day_accepts_only_iso_dates() {
        assert!(is_valid_day("2026-10-09"));
        assert!(!is_valid_day("2026-10-9"));
        assert!(!is_valid_day("2026/10/09"));
        assert!(!is_valid_day("2026-10-09' OR 1=1"));
        assert!(!is_valid_day(""));
    }

    #[test]
    fn test_is_plain_identifier_refuses_quotes_and_spaces() {
        assert!(is_plain_identifier("candles_1m"));
        assert!(!is_plain_identifier("ticks; DROP"));
        assert!(!is_plain_identifier("a\"b"));
        assert!(!is_plain_identifier(""));
    }

    #[test]
    fn test_map_insert_sql_escapes_quotes_and_stamps_the_day() {
        let rows = [
            NameRow {
                security_id: 85650,
                segment: "NSE_FNO",
                contract: "ITC-27Oct2026-255-CE",
            },
            NameRow {
                security_id: 7,
                segment: "NSE_EQ",
                contract: "O'NEIL",
            },
        ];
        let sql = map_insert_sql("2026-10-09", &rows).expect("non-empty");
        assert!(sql.starts_with("INSERT INTO contract_name_repair_map VALUES "));
        assert!(sql.contains(
            "('2026-10-09T00:00:00.000000Z', 'dhan', 'NSE_FNO', 85650, 'ITC-27Oct2026-255-CE')"
        ));
        assert!(sql.contains("'O''NEIL'"));
        assert!(map_insert_sql("2026-10-09", &[]).is_none());
    }

    #[test]
    fn test_map_insert_sql_batch_stays_under_the_request_buffer() {
        let rows: Vec<NameRow<'_>> = (0..CONTRACT_NAME_REPAIR_MAP_BATCH_ROWS)
            .map(|i| NameRow {
                security_id: 1_000_000 + i as i64,
                segment: "NSE_FNO",
                contract: "BANKNIFTY-28Oct2026-123456-CE",
            })
            .collect();
        let sql = map_insert_sql("2026-10-09", &rows).expect("non-empty");
        // Worst-case URL encoding triples a byte; the buffer is 64 KiB.
        assert!(
            sql.len() * 3 < 64 * 1024,
            "batch too large: {} bytes",
            sql.len()
        );
    }

    #[test]
    fn test_map_table_ddl_has_dedup_with_feed() {
        let ddl = map_table_ddl();
        assert!(ddl.contains("WAL"));
        assert!(ddl.contains("DEDUP UPSERT KEYS(ts, security_id, segment, feed)"));
        assert!(DEDUP_KEY_CONTRACT_NAME_REPAIR_MAP.contains("segment"));
        assert!(DEDUP_KEY_CONTRACT_NAME_REPAIR_MAP.contains("feed"));
    }

    #[test]
    fn test_repair_insert_sql_copies_every_column_and_names_only_blank_dhan_rows() {
        let columns = cols(&[
            "ts",
            "feed",
            "segment",
            "security_id",
            "contract",
            "close",
            "open_latency",
        ]);
        let sql = repair_insert_sql("candles_1m", "2026-10-09", &columns).expect("builds");
        assert!(sql.starts_with(
            "INSERT INTO candles_1m (\"ts\", \"feed\", \"segment\", \"security_id\", \"contract\", \"close\", \"open_latency\") \
             SELECT c.\"ts\", c.\"feed\", c.\"segment\", c.\"security_id\", m.contract, c.\"close\", c.\"open_latency\" "
        ));
        assert!(sql.contains("c.contract IS NULL"));
        assert!(sql.contains("c.feed = 'dhan'"));
        assert!(sql.contains("c.ts IN '2026-10-09'"));
        assert!(sql.contains("WHERE ts IN '2026-10-09' AND feed = 'dhan'"));
        assert!(sql.contains("ON c.security_id = m.security_id AND c.segment = m.segment"));
    }

    #[test]
    fn test_repair_insert_sql_refuses_a_table_missing_a_join_column() {
        for missing in ["ts", "feed", "segment", "security_id", "contract"] {
            let columns: Vec<String> =
                ["ts", "feed", "segment", "security_id", "contract", "close"]
                    .iter()
                    .filter(|c| **c != missing)
                    .map(|c| (*c).to_string())
                    .collect();
            assert!(
                repair_insert_sql("t", "2026-10-09", &columns).is_none(),
                "{missing}"
            );
        }
        let odd = cols(&[
            "ts",
            "feed",
            "segment",
            "security_id",
            "contract",
            "bad\"col",
        ]);
        assert!(repair_insert_sql("t", "2026-10-09", &odd).is_none());
    }

    #[test]
    fn test_count_and_repair_share_one_row_source() {
        let count = blank_named_count_sql("ticks", "2026-10-09");
        let repair = repair_insert_sql(
            "ticks",
            "2026-10-09",
            &cols(&["ts", "feed", "segment", "security_id", "contract"]),
        )
        .expect("builds");
        let source = blank_named_rows_source("ticks", "2026-10-09");
        assert!(count.ends_with(&source));
        assert!(repair.ends_with(&source));
    }
    #[test]
    fn test_parse_single_long_reads_a_count() {
        let count = r#"{"query":"x","columns":[{"name":"count()","type":"LONG"}],"dataset":[[42]],"count":1}"#;
        assert_eq!(parse_single_long(count), Some(42));
        assert_eq!(parse_single_long("not json"), None);
        assert_eq!(parse_single_long(r#"{"dataset":[],"count":0}"#), None);
    }

    #[test]
    fn test_parse_table_flags_reads_dedup_and_wal() {
        let flags = r#"{"columns":[],"dataset":[[true,false]],"count":1}"#;
        assert_eq!(parse_table_flags(flags), Some((true, false)));
        assert_eq!(parse_table_flags(r#"{"dataset":[],"count":0}"#), None);
    }

    #[test]
    fn test_parse_wal_progress_reads_applied_committed_and_suspended() {
        let wal = r#"{"dataset":[[5,7,true]],"count":1}"#;
        assert_eq!(parse_wal_progress(wal), Some((5, 7, true)));
        assert_eq!(parse_wal_progress(r#"{"dataset":[[5]],"count":1}"#), None);
    }

    #[test]
    fn test_parse_column_names_reads_names_in_order() {
        let columns = r#"{"dataset":[["ts"],["feed"],["contract"]],"count":3}"#;
        assert_eq!(
            parse_column_names(columns),
            Some(cols(&["ts", "feed", "contract"]))
        );
        assert_eq!(parse_column_names(r#"{"dataset":[],"count":0}"#), None);
    }

    #[test]
    fn test_answer_is_error_spots_a_refusal() {
        assert!(answer_is_error(
            r#"{"query":"x","error":"no","position":0}"#
        ));
        assert!(!answer_is_error(r#"{"dml":"OK"}"#));
    }

    #[test]
    fn test_table_flags_sql_names_the_table() {
        assert_eq!(
            table_flags_sql("ticks"),
            "SELECT dedup, walEnabled FROM tables() WHERE table_name = 'ticks'"
        );
    }

    #[test]
    fn test_table_columns_sql_names_the_table() {
        assert_eq!(
            table_columns_sql("ticks"),
            "SELECT \"column\" FROM table_columns('ticks')"
        );
    }

    #[test]
    fn test_map_wal_progress_sql_reads_the_name_table() {
        assert!(map_wal_progress_sql().contains("name = 'contract_name_repair_map'"));
    }

    #[test]
    fn test_blank_named_count_sql_counts_only_blank_dhan_rows_of_the_day() {
        let sql = blank_named_count_sql("candles_1m", "2026-10-09");
        assert!(sql.starts_with("SELECT count() FROM candles_1m c JOIN"));
        assert!(sql.contains("c.contract IS NULL"));
        assert!(sql.contains("c.feed = 'dhan'"));
    }

    #[tokio::test]
    async fn test_repair_contract_names_for_day_sends_nothing_for_a_bad_day_or_no_names() {
        // Port 9 (discard) on a loopback: any request would fail and count a
        // failure, so a clean default tally proves nothing was sent.
        let client = Client::new();
        let url = "http://127.0.0.1:9/exec";
        let names = [NameRow {
            security_id: 1,
            segment: "NSE_FNO",
            contract: "X",
        }];
        let tally =
            repair_contract_names_for_day(&client, url, "2026-10-09' --", &names, &["t"]).await;
        assert_eq!(tally, RepairDayTally::default());
        let tally = repair_contract_names_for_day(&client, url, "2026-10-09", &[], &["t"]).await;
        assert_eq!(tally, RepairDayTally::default());
    }
}

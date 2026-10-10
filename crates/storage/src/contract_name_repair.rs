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
//! 9.3.5 on 2026-10-10). So, per day:
//!
//! 1. every table is checked first (DEDUP and WAL on, `contract` present and
//!    not a DEDUP key column, at least one blank `dhan` row that day). A day
//!    with no blank row anywhere loads nothing;
//! 2. the day's names go into [`CONTRACT_NAME_REPAIR_MAP_TABLE`], a small
//!    table without WAL, dropped and created fresh, so the rows are readable as soon as
//!    the insert answers;
//! 3. per table, the repair waits until QuestDB has applied every write to it
//!    and no new write has arrived for [`TABLE_QUIET_POLL_SECS`]. A row still
//!    waiting in the WAL is invisible to a SELECT, and re-inserting the stored
//!    copy would put older values back over it;
//! 4. one `INSERT INTO t (every column) SELECT …, m.contract, … FROM t JOIN
//!    map …  WHERE contract IS NULL` re-inserts each blank row with its name.
//!    Every other column is copied by the database itself, so no value passes
//!    through this process, and the table's DEDUP key replaces the blank row
//!    in place.
//!
//! One statement per table per day keeps the out-of-order merge to one per
//! partition. Only rows of feed `dhan` with a NULL name and an id the day's
//! own files name are touched. The files name more contracts than the live
//! name table holds (it stops at 25,000), so a row the live writer left blank
//! past that cap is named too, with the same name. `market_depth` is NOT
//! repaired: its day partition holds about 1.5 billion rows, and rewriting the
//! blank share of it is not worth the disk churn on a volume that was at 98%
//! on 9 Oct.
//!
//! ## Honest limits
//!
//! - A writer that starts while a repair statement runs (a boot replay, a
//!   spill drain) can have its newer copy of a row replaced by the stored copy
//!   the statement read. The quiet check makes this a window of one statement,
//!   not a certainty; it is not closed.
//! - The repair rewrites each touched partition (copy-on-write). If the
//!   disk-pressure archive drops a partition while a statement runs, the
//!   statement can leave only the repaired rows in it.
//!
//! Cold path: runs from the app's once-a-day after-close task, never on the
//! frame drain.

use std::time::Duration;

use reqwest::Client;
use tracing::{info, warn};

/// The name table the repair joins against: one row per named instrument of
/// the day being repaired (about 122,000), created fresh for each day and dropped after it.
/// No WAL and no partitions, so an insert is readable as soon as it answers.
/// Not market data and not an audit record.
pub const CONTRACT_NAME_REPAIR_MAP_TABLE: &str = "contract_name_repair_map";

/// The only feed whose rows are repaired; every live writer stamps it.
pub const CONTRACT_NAME_REPAIR_FEED: &str = "dhan";

/// Name rows per `INSERT … VALUES`. A row is about 50 characters, so 200 keep
/// the URL-encoded statement near 20 KB, well under QuestDB's default 64 KB
/// request header buffer (`http.request.header.buffer.size`; `/exec` takes
/// GET only, so the statement travels in the URL).
pub const CONTRACT_NAME_REPAIR_MAP_BATCH_ROWS: usize = 200;

/// Ceiling on one repair statement. A day of `ticks` is tens of millions of
/// rows and the select runs inside the request.
pub const CONTRACT_NAME_REPAIR_REQUEST_TIMEOUT_SECS: u64 = 600;

/// How long the repair waits for a table to be applied and quiet before it
/// gives up on that table for this run.
pub const CONTRACT_NAME_REPAIR_QUIET_WAIT_SECS: u64 = 600;

/// Spacing of the WAL polls while waiting. A table is quiet when two polls
/// this far apart read the same committed transaction and it is applied.
pub const TABLE_QUIET_POLL_SECS: u64 = 30;

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
    /// Tables skipped: absent, no DEDUP, not WAL, missing a needed column, or
    /// `contract` is a DEDUP key column.
    pub tables_skipped: u32,
    /// Statements QuestDB refused or that did not answer, or a table that was
    /// never applied and quiet in time. A day with a failure is tried again.
    pub failures: u32,
    /// Names left out because the live writers would store them differently
    /// (a character [`tickvault_common::sanitize::sanitize_ilp_symbol`]
    /// strips, or too long).
    pub names_refused: u32,
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

/// `true` when every live writer stores `name` exactly as given. The tick and
/// aux writers pass it through `sanitize_ilp_symbol`, the candle writer does
/// not; a name that function would change is refused so the repair never
/// writes a different spelling from the one the live path writes.
#[must_use]
pub fn name_is_stored_verbatim(name: &str) -> bool {
    !name.is_empty()
        && matches!(
            tickvault_common::sanitize::sanitize_ilp_symbol(name),
            std::borrow::Cow::Borrowed(_)
        )
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

/// CREATE statement for [`CONTRACT_NAME_REPAIR_MAP_TABLE`]. With no designated
/// timestamp QuestDB makes it unpartitioned and without WAL (and refuses
/// `BYPASS WAL` on it; both checked on 9.3.5, 2026-10-10). Run right after [`map_drop_sql`], so
/// the table is built fresh for each day and a column change here can never
/// meet an older copy of the table (no self-heal needed).
#[must_use]
pub fn map_table_ddl() -> String {
    format!(
        "CREATE TABLE {CONTRACT_NAME_REPAIR_MAP_TABLE} (\
         security_id LONG, segment SYMBOL, contract VARCHAR\
         )"
    )
}

/// Drops [`CONTRACT_NAME_REPAIR_MAP_TABLE`], the scratch table and nothing
/// else; it holds only names copied from the day's contract files.
#[must_use]
pub fn map_drop_sql() -> String {
    format!("DROP TABLE IF EXISTS {CONTRACT_NAME_REPAIR_MAP_TABLE}")
}

/// One multi-row insert of `rows`. `None` for an empty batch.
#[must_use]
pub fn map_insert_sql(rows: &[NameRow<'_>]) -> Option<String> {
    if rows.is_empty() {
        return None;
    }
    let mut sql = format!("INSERT INTO {CONTRACT_NAME_REPAIR_MAP_TABLE} VALUES ");
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            sql.push_str(", ");
        }
        sql.push_str(&format!(
            "({}, {}, {})",
            row.security_id,
            sql_string(row.segment),
            sql_string(row.contract)
        ));
    }
    Some(sql)
}

/// The WAL progress of `table`: applied txn, committed txn, suspended.
#[must_use]
pub fn wal_progress_sql(table: &str) -> String {
    format!("SELECT writerTxn, sequencerTxn, suspended FROM wal_tables() WHERE name = '{table}'")
}

/// Whether `table` exists with DEDUP and WAL on.
#[must_use]
pub fn table_flags_sql(table: &str) -> String {
    format!("SELECT dedup, walEnabled FROM tables() WHERE table_name = '{table}'")
}

/// The column names of `table`, in table order, and whether each is a DEDUP
/// key column.
#[must_use]
pub fn table_columns_sql(table: &str) -> String {
    format!("SELECT \"column\", upsertKey FROM table_columns('{table}')")
}

/// How many `dhan` rows of `day` in `table` carry no name, with no join: the
/// cheap check that decides whether the day needs the name table at all.
#[must_use]
pub fn blank_count_sql(table: &str, day: &str) -> String {
    format!(
        "SELECT count() FROM {table} WHERE ts IN '{day}' AND contract IS NULL \
         AND feed = '{CONTRACT_NAME_REPAIR_FEED}'"
    )
}

/// The `FROM … JOIN … WHERE …` both the count and the repair share: every
/// `dhan` row of `day` with a NULL name whose id the name table covers.
fn blank_named_rows_source(table: &str, day: &str) -> String {
    format!(
        "FROM {table} c JOIN {CONTRACT_NAME_REPAIR_MAP_TABLE} m \
         ON c.security_id = m.security_id AND c.segment = m.segment \
         WHERE c.ts IN '{day}' AND c.contract IS NULL AND c.feed = '{CONTRACT_NAME_REPAIR_FEED}'"
    )
}

/// How many rows the repair of `table` for `day` would name.
#[must_use]
pub fn blank_named_count_sql(table: &str, day: &str) -> String {
    format!("SELECT count() {}", blank_named_rows_source(table, day))
}

/// One column of a target table, from [`table_columns_sql`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableColumn {
    /// The column name.
    pub name: String,
    /// Whether the column is part of the table's DEDUP key.
    pub upsert_key: bool,
}

/// The repair statement: re-insert every blank row with its name, copying
/// every other column as stored. `None` when a column the join needs is
/// missing, a column name is not a plain identifier, or `contract` is a DEDUP
/// key column (re-inserting with a new name would then add a row, not
/// replace one).
#[must_use]
pub fn repair_insert_sql(table: &str, day: &str, columns: &[TableColumn]) -> Option<String> {
    for needed in ["ts", "feed", "segment", "security_id", "contract"] {
        if !columns.iter().any(|c| c.name == needed) {
            return None;
        }
    }
    if columns.iter().any(|c| c.name == "contract" && c.upsert_key) {
        return None;
    }
    if !columns.iter().all(|c| is_plain_identifier(&c.name)) {
        return None;
    }
    let target: Vec<String> = columns.iter().map(|c| format!("\"{}\"", c.name)).collect();
    let source: Vec<String> = columns
        .iter()
        .map(|c| {
            if c.name == "contract" {
                "m.contract".to_string()
            } else {
                format!("c.\"{}\"", c.name)
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

/// What [`table_flags_sql`] said about a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableFlags {
    /// The answer listed no such table.
    Absent,
    /// The table exists; `(dedup, wal)`.
    Present(bool, bool),
}

/// Reads a [`table_flags_sql`] answer. `None` when it does not parse, which
/// the caller counts as a failure, never as an absent table.
#[must_use]
pub fn parse_table_flags(body: &str) -> Option<TableFlags> {
    let rows = dataset(body)?;
    let Some(first) = rows.first() else {
        return Some(TableFlags::Absent);
    };
    let row = first.as_array()?;
    Some(TableFlags::Present(
        row.first()?.as_bool()?,
        row.get(1)?.as_bool()?,
    ))
}

/// `(writerTxn, sequencerTxn, suspended)` from [`wal_progress_sql`].
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

/// Columns from [`table_columns_sql`]; `None` when empty or unparseable.
#[must_use]
pub fn parse_columns(body: &str) -> Option<Vec<TableColumn>> {
    let columns: Option<Vec<TableColumn>> = dataset(body)?
        .iter()
        .map(|row| {
            let row = row.as_array()?;
            Some(TableColumn {
                name: row.first()?.as_str()?.to_string(),
                upsert_key: row.get(1)?.as_bool()?,
            })
        })
        .collect();
    columns.filter(|c| !c.is_empty())
}

/// `true` once a table is applied and quiet: everything committed is applied
/// and the committed transaction has not moved since the previous poll.
#[must_use]
pub fn table_is_quiet(applied: i64, committed: i64, previous_committed: Option<i64>) -> bool {
    applied >= committed && previous_committed == Some(committed)
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

/// Empty the name table and load `names` into it. `false` when any step
/// fails; the day is then left for the next run.
async fn load_names(client: &Client, exec_url: &str, names: &[NameRow<'_>]) -> bool {
    if exec(client, exec_url, &map_drop_sql()).await.is_none() {
        return false;
    }
    if exec(client, exec_url, &map_table_ddl()).await.is_none() {
        return false;
    }
    for batch in names.chunks(CONTRACT_NAME_REPAIR_MAP_BATCH_ROWS) {
        let Some(sql) = map_insert_sql(batch) else {
            continue;
        };
        if exec(client, exec_url, &sql).await.is_none() {
            return false;
        }
    }
    true
}

/// Wait until `table` is applied and quiet (see [`table_is_quiet`]). `false`
/// when it is suspended or not quiet within
/// [`CONTRACT_NAME_REPAIR_QUIET_WAIT_SECS`].
async fn wait_until_quiet(client: &Client, exec_url: &str, table: &str) -> bool {
    let deadline =
        tokio::time::Instant::now() + Duration::from_secs(CONTRACT_NAME_REPAIR_QUIET_WAIT_SECS);
    let mut previous: Option<i64> = None;
    loop {
        let progress = exec(client, exec_url, &wal_progress_sql(table))
            .await
            .and_then(|b| parse_wal_progress(&b));
        match progress {
            Some((_, _, true)) => {
                warn!(
                    table,
                    "contract name repair: table WAL is suspended, table left for the next run"
                );
                return false;
            }
            Some((applied, committed, false)) => {
                if table_is_quiet(applied, committed, previous) {
                    return true;
                }
                previous = Some(committed);
            }
            None => previous = None,
        }
        if tokio::time::Instant::now() >= deadline {
            warn!(
                table,
                wait_secs = CONTRACT_NAME_REPAIR_QUIET_WAIT_SECS,
                "contract name repair: table still being written, left for the next run"
            );
            return false;
        }
        tokio::time::sleep(Duration::from_secs(TABLE_QUIET_POLL_SECS)).await;
    }
}

/// Check one table for `day`. `Some(repair statement)` when it has blank rows
/// to look at; `None` when it is clean, skipped or failed (already tallied).
async fn survey_table(
    client: &Client,
    exec_url: &str,
    table: &str,
    day: &str,
    tally: &mut RepairDayTally,
) -> Option<String> {
    let Some(flags) = exec(client, exec_url, &table_flags_sql(table))
        .await
        .and_then(|b| parse_table_flags(&b))
    else {
        tally.failures += 1;
        return None;
    };
    match flags {
        TableFlags::Present(true, true) => {}
        TableFlags::Present(dedup, wal) => {
            warn!(
                table,
                dedup,
                wal,
                "contract name repair: table skipped, a re-insert without DEDUP would duplicate rows"
            );
            tally.tables_skipped += 1;
            return None;
        }
        TableFlags::Absent => {
            tally.tables_skipped += 1;
            return None;
        }
    }
    let Some(columns) = exec(client, exec_url, &table_columns_sql(table))
        .await
        .and_then(|b| parse_columns(&b))
    else {
        tally.failures += 1;
        return None;
    };
    let Some(repair_sql) = repair_insert_sql(table, day, &columns) else {
        warn!(
            table,
            "contract name repair: table skipped, a needed column is missing or `contract` is a DEDUP key"
        );
        tally.tables_skipped += 1;
        return None;
    };
    let Some(blank) = exec(client, exec_url, &blank_count_sql(table, day))
        .await
        .and_then(|b| parse_single_long(&b))
    else {
        tally.failures += 1;
        return None;
    };
    if blank <= 0 {
        tally.tables_clean += 1;
        return None;
    }
    Some(repair_sql)
}

/// Repair one surveyed table for `day` against the loaded name table.
async fn repair_table(
    client: &Client,
    exec_url: &str,
    table: &str,
    day: &str,
    repair_sql: &str,
    tally: &mut RepairDayTally,
) {
    // Count first: a table whose blank rows the names do not cover needs no
    // quiet wait at all.
    let count_sql = blank_named_count_sql(table, day);
    let Some(named) = exec(client, exec_url, &count_sql)
        .await
        .and_then(|b| parse_single_long(&b))
    else {
        tally.failures += 1;
        return;
    };
    if named <= 0 {
        tally.tables_clean += 1;
        return;
    }
    if !wait_until_quiet(client, exec_url, table).await {
        tally.failures += 1;
        return;
    }
    // Count again on the quiet table, so the tally matches what is written.
    let Some(named) = exec(client, exec_url, &count_sql)
        .await
        .and_then(|b| parse_single_long(&b))
    else {
        tally.failures += 1;
        return;
    };
    if named <= 0 {
        tally.tables_clean += 1;
        return;
    }
    if exec(client, exec_url, repair_sql).await.is_none() {
        tally.failures += 1;
        return;
    }
    let rows = u64::try_from(named).unwrap_or(0);
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
/// O(tables) checks, then, only when a table has a blank row, O(names / batch)
/// inserts plus a few statements per such table; cold path.
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
    let mut todo: Vec<(&str, String)> = Vec::new();
    for table in tables {
        if !is_plain_identifier(table) {
            tally.tables_skipped += 1;
            continue;
        }
        if let Some(sql) = survey_table(client, exec_url, table, day, &mut tally).await {
            todo.push((table, sql));
        }
    }
    if todo.is_empty() {
        return tally;
    }
    let kept: Vec<NameRow<'_>> = names
        .iter()
        .filter(|n| name_is_stored_verbatim(n.contract))
        .copied()
        .collect();
    tally.names_refused = u32::try_from(names.len() - kept.len()).unwrap_or(u32::MAX);
    if !load_names(client, exec_url, &kept).await {
        tally.failures += 1;
        return tally;
    }
    for (table, sql) in &todo {
        repair_table(client, exec_url, table, day, sql, &mut tally).await;
    }
    // Best effort: the next day's load drops it first anyway.
    let _ = exec(client, exec_url, &map_drop_sql()).await;
    tally
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols(names: &[&str]) -> Vec<TableColumn> {
        names
            .iter()
            .map(|s| TableColumn {
                name: (*s).to_string(),
                upsert_key: matches!(*s, "ts" | "feed" | "segment" | "security_id"),
            })
            .collect()
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
    fn test_name_is_stored_verbatim_refuses_what_the_tick_writer_would_change() {
        assert!(name_is_stored_verbatim("ITC-27Oct2026-255-CE"));
        assert!(!name_is_stored_verbatim("A,B"));
        assert!(!name_is_stored_verbatim("A=B"));
        assert!(!name_is_stored_verbatim(""));
    }

    #[test]
    fn test_map_insert_sql_escapes_quotes() {
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
        let sql = map_insert_sql(&rows).expect("non-empty");
        assert!(sql.starts_with("INSERT INTO contract_name_repair_map VALUES "));
        assert!(sql.contains("(85650, 'NSE_FNO', 'ITC-27Oct2026-255-CE')"));
        assert!(sql.contains("'O''NEIL'"));
        assert!(map_insert_sql(&[]).is_none());
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
        let sql = map_insert_sql(&rows).expect("non-empty");
        // Worst-case URL encoding triples a byte; the buffer is 64 KiB.
        assert!(
            sql.len() * 3 < 64 * 1024,
            "batch too large: {} bytes",
            sql.len()
        );
    }

    #[test]
    fn test_map_table_has_no_wal_and_a_varchar_name() {
        let ddl = map_table_ddl();
        assert!(
            !ddl.contains("timestamp("),
            "a designated timestamp would make it WAL"
        );
        assert!(ddl.contains("contract VARCHAR"));
        assert!(!ddl.contains("PARTITION BY"));
        assert!(ddl.starts_with("CREATE TABLE contract_name_repair_map ("));
        // The drop names the scratch table and nothing else.
        assert_eq!(
            map_drop_sql(),
            "DROP TABLE IF EXISTS contract_name_repair_map"
        );
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
        assert!(sql.contains("ON c.security_id = m.security_id AND c.segment = m.segment"));
    }

    #[test]
    fn test_repair_insert_sql_refuses_a_table_missing_a_join_column() {
        for missing in ["ts", "feed", "segment", "security_id", "contract"] {
            let names: Vec<&str> = ["ts", "feed", "segment", "security_id", "contract", "close"]
                .into_iter()
                .filter(|c| *c != missing)
                .collect();
            assert!(
                repair_insert_sql("t", "2026-10-09", &cols(&names)).is_none(),
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
    fn test_repair_insert_sql_refuses_a_table_keyed_on_contract() {
        let mut columns = cols(&["ts", "feed", "segment", "security_id", "contract"]);
        for c in &mut columns {
            if c.name == "contract" {
                c.upsert_key = true;
            }
        }
        assert!(repair_insert_sql("t", "2026-10-09", &columns).is_none());
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
    fn test_blank_count_sql_needs_no_join() {
        let sql = blank_count_sql("candles_1m", "2026-10-09");
        assert_eq!(
            sql,
            "SELECT count() FROM candles_1m WHERE ts IN '2026-10-09' AND contract IS NULL \
             AND feed = 'dhan'"
        );
    }

    #[test]
    fn test_table_is_quiet_needs_applied_and_two_equal_polls() {
        assert!(!table_is_quiet(10, 10, None));
        assert!(table_is_quiet(10, 10, Some(10)));
        assert!(!table_is_quiet(9, 10, Some(10)));
        assert!(!table_is_quiet(11, 11, Some(10)));
    }

    #[test]
    fn test_parse_single_long_reads_a_count() {
        let count = r#"{"query":"x","columns":[{"name":"count()","type":"LONG"}],"dataset":[[42]],"count":1}"#;
        assert_eq!(parse_single_long(count), Some(42));
        assert_eq!(parse_single_long("not json"), None);
        assert_eq!(parse_single_long(r#"{"dataset":[],"count":0}"#), None);
    }

    #[test]
    fn test_parse_table_flags_tells_absent_from_unreadable() {
        let flags = r#"{"columns":[],"dataset":[[true,false]],"count":1}"#;
        assert_eq!(
            parse_table_flags(flags),
            Some(TableFlags::Present(true, false))
        );
        assert_eq!(
            parse_table_flags(r#"{"dataset":[],"count":0}"#),
            Some(TableFlags::Absent)
        );
        assert_eq!(parse_table_flags("not json"), None);
    }

    #[test]
    fn test_parse_wal_progress_reads_applied_committed_and_suspended() {
        let wal = r#"{"dataset":[[5,7,true]],"count":1}"#;
        assert_eq!(parse_wal_progress(wal), Some((5, 7, true)));
        assert_eq!(parse_wal_progress(r#"{"dataset":[[5]],"count":1}"#), None);
    }

    #[test]
    fn test_parse_columns_reads_names_and_keys_in_order() {
        let columns = r#"{"dataset":[["ts",true],["feed",true],["contract",false]],"count":3}"#;
        let parsed = parse_columns(columns).expect("parses");
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].name, "ts");
        assert!(parsed[0].upsert_key);
        assert_eq!(parsed[2].name, "contract");
        assert!(!parsed[2].upsert_key);
        assert_eq!(parse_columns(r#"{"dataset":[],"count":0}"#), None);
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
    fn test_table_columns_sql_reads_names_and_keys() {
        assert_eq!(
            table_columns_sql("ticks"),
            "SELECT \"column\", upsertKey FROM table_columns('ticks')"
        );
    }

    #[test]
    fn test_wal_progress_sql_names_the_table() {
        assert!(wal_progress_sql("candles_1m").contains("name = 'candles_1m'"));
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

    #[tokio::test]
    async fn test_an_unreachable_database_is_a_failure_not_a_skip() {
        // Regression (review 2026-10-10): an unanswered tables() check was
        // counted as skipped, so an outage ended the run "ok".
        let client = Client::new();
        let names = [NameRow {
            security_id: 1,
            segment: "NSE_FNO",
            contract: "X",
        }];
        let tally = repair_contract_names_for_day(
            &client,
            "http://127.0.0.1:9/exec",
            "2026-10-09",
            &names,
            &["candles_1m"],
        )
        .await;
        assert_eq!(tally.failures, 1);
        assert_eq!(tally.tables_skipped, 0);
    }
}

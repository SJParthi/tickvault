//! The daily per-table storage measurement (`table_storage_daily`).
//!
//! ## Why
//!
//! `dhan-rest-only-noise-lock-2026-07-14.md` §2.3o-i, on the day a session
//! burned 138 GB: *"No per-table byte metric exists anywhere … every
//! attribution above is derived rather than observed. Nobody can say from
//! telemetry where the 138 GB went."*
//!
//! Every disk figure this repository quotes — the 46.3 GB of inline depth,
//! the 110.2 GB depth total, the 24x row ratio — is `row_width × assumed
//! rows`. Useful for arguing about a design, useless for settling a question
//! after the fact, and this codebase has been bitten more than once by
//! treating a derivation as a measurement. QuestDB has known the answer the
//! whole time.
//!
//! ## Complexity, and the honest part of it
//!
//! One query per managed table, once a day, on the post-close cold path.
//! O(tables × partitions) over a bounded set; nothing on the tick hot path.
//! The O(1) property is the READ: one keyed row per table per day.
//!
//! ## Cost
//!
//! Zero recurring. This writes to the database and nowhere else — no
//! CloudWatch metric, no alarm. Roughly thirty tables would be roughly thirty
//! metric names at ~$0.30/mo against a budget the noise lock already records
//! as ~$8 above the automatic `STOP_EC2_INSTANCES` line in a maximal month.
//! Charting it is a decision with a price attached and is not taken here.

use tickvault_common::config::QuestDbConfig;
use tickvault_storage::partition_manager::all_managed_table_names;
use tickvault_storage::table_storage_probe::{
    TableStorageDailyWriter, TableStorageRow, build_table_storage_sql,
    ensure_table_storage_daily_table, measured_total, parse_table_storage, storage_row,
};
use tracing::{error, info};

/// HTTP timeout for one `table_partitions()` probe.
const PROBE_HTTP_TIMEOUT_SECS: u64 = 20;

/// Measure every managed table's disk footprint and persist one row each.
///
/// Best-effort and additive: it writes its own table and can never fail,
/// degrade or delay whatever called it.
///
/// A table whose probe cannot be read still gets a ROW, marked unmeasured
/// with `-1` sentinels. That is deliberate and is the whole discipline of
/// this module: "we tried and could not read it" is a different fact from
/// "this table takes no space", and an ABSENT row is indistinguishable from
/// a table that does not exist. Writing a confident zero instead would be
/// the false-OK class this repository keeps having to retire — most recently
/// on 2026-08-28, when a counter that published no series at all read exactly
/// like a healthy zero.
///
/// Returns the rows written.
// TEST-EXEMPT: orchestration over the unit-tested pure parts (build_table_storage_sql / parse_table_storage / storage_row / measured_total); a direct test needs live QuestDB.
pub async fn run_table_storage_rollup(
    questdb: &QuestDbConfig,
    day_ist_midnight_nanos: i64,
) -> Vec<TableStorageRow> {
    ensure_table_storage_daily_table(questdb).await;

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(PROBE_HTTP_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            error!(
                code = "SCOREBOARD-01",
                stage = "table_storage_client",
                ?err,
                "SCOREBOARD-01: per-table storage measurement skipped — HTTP \
                 client build failed. Disk attribution stays DERIVED for this \
                 day rather than observed."
            );
            return Vec::new();
        }
    };
    let url = format!("http://{}:{}/exec", questdb.host, questdb.http_port);

    let tables = all_managed_table_names();
    let mut rows: Vec<TableStorageRow> = Vec::with_capacity(tables.len());
    let mut column_missing = 0_usize;

    for table in tables {
        let sql = build_table_storage_sql(table);
        let body = match client
            .get(&url)
            .query(&[("query", sql.as_str())])
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => resp.text().await.ok(),
            // A table that does not exist yet answers non-2xx. That is not an
            // error worth paging over — it is measured as unmeasured and the
            // row says so.
            Ok(_) | Err(_) => None,
        };
        let probe = body.as_deref().and_then(parse_table_storage);
        if body.is_some() && probe.is_none() {
            column_missing += 1;
        }
        rows.push(storage_row(table, probe, day_ist_midnight_nanos));
    }

    // A response that PARSED but carried no `diskSize` column means QuestDB
    // renamed or dropped it — every future measurement is blind until someone
    // looks. Loud, once, with the count.
    if column_missing > 0 {
        error!(
            code = "SCOREBOARD-01",
            stage = "table_storage_no_disk_size_column",
            tables_affected = column_missing,
            "SCOREBOARD-01: {column_missing} table(s) answered table_partitions() \
             WITHOUT a `diskSize` column — the per-table byte measurement is \
             blind for them and their rows are marked unmeasured, never zero. \
             Check whether the QuestDB version renamed that column."
        );
    }

    let mut writer = TableStorageDailyWriter::new(questdb);
    let mut appended = 0_usize;
    for r in &rows {
        if let Err(err) = writer.append_row(r) {
            error!(
                code = "SCOREBOARD-01",
                stage = "table_storage_append",
                table = r.table_name.as_str(),
                ?err,
                "SCOREBOARD-01: table_storage_daily append failed for one table"
            );
            continue;
        }
        appended += 1;
    }
    if let Err(err) = writer.flush() {
        error!(
            code = "SCOREBOARD-01",
            stage = "table_storage_flush",
            rows = appended,
            ?err,
            "SCOREBOARD-01: table_storage_daily flush failed — disk attribution \
             stays DERIVED for this day"
        );
        return rows;
    }

    let (total_bytes, measured, unmeasured) = measured_total(&rows);
    info!(
        rows = appended,
        total_bytes,
        total_gb = total_bytes / 1_000_000_000,
        tables_measured = measured,
        tables_unmeasured = unmeasured,
        "table_storage_daily: per-table disk footprint OBSERVED — \
         'where did the disk go?' is now a query, not a derivation"
    );
    rows
}

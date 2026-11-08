//! Probe: does QuestDB re-create a dropped WAL hour partition when a late row
//! for that hour is inserted afterwards? (plan item PR AA, step 3; owner
//! Quote 51, 2026-11-07.)
//!
//! # Why this exists
//!
//! The archiver drops an hour partition only after its S3 copy is verified.
//! If a spill replay later posts a row for that hour, QuestDB either
//! re-creates the partition holding only the late row (which a later pass
//! copies again, to a sidecar key) or refuses the row. Which one happens is
//! Unknown until measured, and it decides whether a table other than
//! `market_depth` may ever be dropped while one of its spill files is still
//! waiting (owner decision 2 of PR AA). Until then the hour path keeps those
//! tables off it while their own spill folder is pending.
//!
//! The test checks what must hold either way: the dropped rows never come
//! back, and the hour holds at most the late row. It prints which of the two
//! behaviours it saw.
//!
//! # How it runs
//!
//! `#[ignore]`d: it needs a scratch QuestDB and is run by hand.
//!
//! ```text
//! docker run -d -p 9000:9000 questdb/questdb:9.3.5
//! TV_QDB_REAL_HTTP=127.0.0.1:9000 cargo test -p tickvault-storage \
//!     --test questdb_dropped_partition_probe -- --ignored --nocapture
//! ```
//!
//! With `TV_QDB_REAL_HTTP` unset it prints why and returns. It creates one
//! table named for its process id and drops it at the end.

use std::time::{Duration, Instant};

use serde_json::Value;

/// How long the probe waits for QuestDB to apply its WAL after a write.
const APPLY_DEADLINE: Duration = Duration::from_secs(90);

/// The hour the probe drops (an old, closed hour; it is never the active
/// partition because two later hours are written).
const DROPPED_HOUR: &str = "2026-01-01T09";

/// Whether the probe's outcome is one the archiver may live with (PR AA
/// review). Safe: the late insert was REFUSED and the hour stays empty, or it
/// was ACCEPTED and the hour holds exactly the late row in a re-created
/// partition. Everything else fails: the dropped row came back, or QuestDB
/// accepted the late row and it is not there (accepted and lost, the case a
/// spill replay into a dropped hour would hit). Pure.
fn probe_outcome_is_safe(late_accepted: bool, rows_after: &[i64], recreated: bool) -> bool {
    let rows_ok = if late_accepted {
        rows_after == [9]
    } else {
        rows_after.is_empty()
    };
    rows_ok && recreated != rows_after.is_empty()
}

/// The judge of the probe: the accepted-and-lost outcome must fail it (the
/// first version of this probe passed on it). Runs without QuestDB.
#[test]
fn test_probe_outcome_judge_fails_accepted_and_lost() {
    assert!(probe_outcome_is_safe(false, &[], false), "refused, empty");
    assert!(
        probe_outcome_is_safe(true, &[9], true),
        "re-created with the late row"
    );
    assert!(
        !probe_outcome_is_safe(true, &[], false),
        "accepted and lost must fail"
    );
    assert!(
        !probe_outcome_is_safe(true, &[1, 9], true),
        "the dropped row came back"
    );
    assert!(
        !probe_outcome_is_safe(false, &[1], true),
        "the dropped row came back"
    );
    assert!(
        !probe_outcome_is_safe(true, &[9], false),
        "listing and rows disagree"
    );
}

/// The QuestDB HTTP base URL, or `None` when no QuestDB is configured.
fn base_url() -> Option<String> {
    let Some(addr) = std::env::var("TV_QDB_REAL_HTTP")
        .ok()
        .filter(|v| !v.is_empty())
    else {
        eprintln!("questdb_dropped_partition_probe: skipped, set TV_QDB_REAL_HTTP=host:port");
        return None;
    };
    Some(format!("http://{addr}"))
}

/// Runs one SQL statement through `/exec`; `Err` carries QuestDB's error.
async fn exec(base: &str, sql: &str) -> Result<Value, String> {
    let resp = reqwest::Client::new()
        .get(format!("{base}/exec"))
        .query(&[("query", sql)])
        .send()
        .await
        .map_err(|e| format!("QuestDB unreachable for `{sql}`: {e}"))?;
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("QuestDB answer to `{sql}` is not JSON: {e}"))?;
    match body.get("error") {
        Some(error) => Err(format!("QuestDB refused `{sql}`: {error}")),
        None => Ok(body),
    }
}

/// Runs a statement that must succeed.
async fn must(base: &str, sql: &str) -> Value {
    exec(base, sql).await.unwrap_or_else(|e| panic!("{e}"))
}

/// The dataset rows of a query.
async fn rows(base: &str, sql: &str) -> Vec<Value> {
    must(base, sql)
        .await
        .get("dataset")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Waits until QuestDB has applied every WAL transaction of `table`.
async fn wait_applied(base: &str, table: &str) {
    let sql = format!("SELECT writerTxn, sequencerTxn FROM wal_tables() WHERE name = '{table}'");
    let start = Instant::now();
    loop {
        let r = rows(base, &sql).await;
        if let Some(row) = r.first() {
            let writer = row.get(0).and_then(Value::as_i64);
            let sequencer = row.get(1).and_then(Value::as_i64);
            if writer.is_some() && writer == sequencer {
                return;
            }
        }
        assert!(
            start.elapsed() < APPLY_DEADLINE,
            "{table}: WAL not applied within {APPLY_DEADLINE:?} (last: {r:?})"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The partition names of `table`.
async fn partitions(base: &str, table: &str) -> Vec<String> {
    rows(
        base,
        &format!("SELECT name FROM table_partitions('{table}')"),
    )
    .await
    .iter()
    .filter_map(|r| r.get(0).and_then(Value::as_str).map(str::to_string))
    .collect()
}

/// The `v` values in the dropped hour.
async fn values_in_dropped_hour(base: &str, table: &str) -> Vec<i64> {
    rows(
        base,
        &format!(
            "SELECT v FROM {table} WHERE ts >= '2026-01-01T09:00:00.000000Z' \
             AND ts < '2026-01-01T10:00:00.000000Z' ORDER BY v"
        ),
    )
    .await
    .iter()
    .filter_map(|r| r.get(0).and_then(Value::as_i64))
    .collect()
}

#[tokio::test]
#[ignore = "needs a scratch QuestDB (TV_QDB_REAL_HTTP); gates PR AA owner decision 2"]
async fn dropped_wal_partition_recreated_on_insert() {
    let Some(base) = base_url() else {
        return;
    };
    let table = format!("pr_aa_drop_probe_{}", std::process::id());
    let _ = exec(&base, &format!("DROP TABLE IF EXISTS {table}")).await;
    must(
        &base,
        &format!("CREATE TABLE {table} (ts TIMESTAMP, v LONG) TIMESTAMP(ts) PARTITION BY HOUR WAL"),
    )
    .await;
    must(
        &base,
        &format!(
            "INSERT INTO {table} VALUES \
             ('2026-01-01T09:30:00.000000Z', 1), \
             ('2026-01-01T10:30:00.000000Z', 2), \
             ('2026-01-01T11:30:00.000000Z', 3)"
        ),
    )
    .await;
    wait_applied(&base, &table).await;
    assert_eq!(values_in_dropped_hour(&base, &table).await, vec![1]);

    must(
        &base,
        &format!("ALTER TABLE {table} DROP PARTITION LIST '{DROPPED_HOUR}'"),
    )
    .await;
    wait_applied(&base, &table).await;
    assert!(
        !partitions(&base, &table)
            .await
            .iter()
            .any(|p| p == DROPPED_HOUR),
        "the hour was not dropped"
    );
    assert!(values_in_dropped_hour(&base, &table).await.is_empty());

    // The late row a spill replay would post into the dropped hour.
    let late = exec(
        &base,
        &format!("INSERT INTO {table} VALUES ('2026-01-01T09:45:00.000000Z', 9)"),
    )
    .await;
    if late.is_ok() {
        wait_applied(&base, &table).await;
    }
    let after = values_in_dropped_hour(&base, &table).await;
    let recreated = partitions(&base, &table)
        .await
        .iter()
        .any(|p| p == DROPPED_HOUR);
    eprintln!(
        "questdb_dropped_partition_probe: late insert {}; hour {DROPPED_HOUR} {}; rows in it: {after:?}",
        if late.is_ok() { "accepted" } else { "refused" },
        if recreated { "RE-CREATED" } else { "absent" },
    );
    // Either way: the dropped row never comes back, and an ACCEPTED late row
    // is visible (accepted and then lost is the outcome this probe exists to
    // catch). See `probe_outcome_is_safe`.
    assert!(
        probe_outcome_is_safe(late.is_ok(), &after, recreated),
        "unsafe outcome: late insert {}, rows in the dropped hour {after:?}, re-created \
         {recreated}",
        if late.is_ok() { "accepted" } else { "refused" },
    );

    let _ = exec(&base, &format!("DROP TABLE IF EXISTS {table}")).await;
}

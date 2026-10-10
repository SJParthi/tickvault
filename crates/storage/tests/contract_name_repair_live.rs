//! End-to-end check of the contract name repair against a real QuestDB.
//!
//! `#[ignore]`d: it needs a QuestDB. Run with
//! `TV_QUESTDB_EXEC_URL=http://127.0.0.1:9000/exec cargo test -p tickvault-storage --test contract_name_repair_live -- --ignored`.
//! It creates and drops its own table, `contract_name_repair_probe`.

use std::time::Duration;

use tickvault_storage::contract_name_repair::{NameRow, repair_contract_names_for_day};

const TABLE: &str = "contract_name_repair_probe";
const DAY: &str = "2026-10-09";

async fn exec(client: &reqwest::Client, url: &str, sql: &str) -> serde_json::Value {
    let body = client
        .get(url)
        .query(&[("query", sql)])
        .send()
        .await
        .expect("questdb answers")
        .text()
        .await
        .expect("body");
    serde_json::from_str(&body).expect("json")
}

async fn wait_applied(client: &reqwest::Client, url: &str) {
    for _ in 0..120 {
        let v = exec(
            client,
            url,
            &format!("SELECT writerTxn, sequencerTxn FROM wal_tables() WHERE name = '{TABLE}'"),
        )
        .await;
        let row = &v["dataset"][0];
        if row[0].as_i64().is_some() && row[0] == row[1] {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("{TABLE} never applied");
}

#[tokio::test]
#[ignore = "needs a QuestDB at TV_QUESTDB_EXEC_URL"]
async fn repair_names_blank_rows_in_place_and_leaves_everything_else() {
    let url = std::env::var("TV_QUESTDB_EXEC_URL").expect("TV_QUESTDB_EXEC_URL");
    let client = reqwest::Client::new();
    exec(&client, &url, &format!("DROP TABLE IF EXISTS {TABLE}")).await;
    exec(
        &client,
        &url,
        &format!(
            "CREATE TABLE {TABLE} (ts TIMESTAMP, feed SYMBOL, segment SYMBOL, \
             security_id LONG, contract SYMBOL, close DOUBLE, volume LONG) \
             timestamp(ts) PARTITION BY DAY WAL \
             DEDUP UPSERT KEYS(ts, security_id, segment, feed)"
        ),
    )
    .await;
    // Blank option row (the 9 Oct shape), an already-named row, an id no file
    // names, another feed, and a blank row on the next day.
    exec(
        &client,
        &url,
        &format!(
            "INSERT INTO {TABLE} VALUES \
             ('{DAY}T09:16:00.000000Z', 'dhan', 'NSE_FNO', 85650, NULL, 1.85, 4800), \
             ('{DAY}T09:15:00.000000Z', 'dhan', 'NSE_FNO', 85650, 'ITC-27Oct2026-255-CE', 1.9, 3200), \
             ('{DAY}T09:16:00.000000Z', 'dhan', 'NSE_FNO', 99999, NULL, 5.0, 10), \
             ('{DAY}T09:16:00.000000Z', 'groww', 'NSE_FNO', 85650, NULL, 1.85, 4800), \
             ('2026-10-10T09:16:00.000000Z', 'dhan', 'NSE_FNO', 85650, NULL, 2.0, 1)"
        ),
    )
    .await;
    wait_applied(&client, &url).await;

    let names = [NameRow {
        security_id: 85650,
        segment: "NSE_FNO",
        contract: "ITC-27Oct2026-255-CE",
    }];
    let tally = repair_contract_names_for_day(&client, &url, DAY, &names, &[TABLE]).await;
    assert_eq!(tally.rows_restored, 1, "{tally:?}");
    assert_eq!(tally.failures, 0, "{tally:?}");
    wait_applied(&client, &url).await;

    let rows = exec(
        &client,
        &url,
        &format!("SELECT ts, feed, security_id, contract, close, volume FROM {TABLE} ORDER BY ts, feed, security_id"),
    )
    .await;
    let got = rows["dataset"].as_array().expect("rows").clone();
    assert_eq!(got.len(), 5, "no row added or lost: {got:?}");
    let find = |ts: &str, feed: &str, id: i64| {
        got.iter()
            .find(|r| {
                r[0].as_str().is_some_and(|t| t.starts_with(ts)) && r[1] == feed && r[2] == id
            })
            .cloned()
            .expect("row present")
    };
    let fixed = find("2026-10-09T09:16", "dhan", 85650);
    assert_eq!(fixed[3], "ITC-27Oct2026-255-CE");
    assert_eq!(fixed[4], 1.85);
    assert_eq!(fixed[5], 4800);
    assert!(find("2026-10-09T09:16", "dhan", 99999)[3].is_null());
    assert!(find("2026-10-09T09:16", "groww", 85650)[3].is_null());
    assert!(find("2026-10-10T09:16", "dhan", 85650)[3].is_null());

    // A second run finds nothing left to do.
    let again = repair_contract_names_for_day(&client, &url, DAY, &names, &[TABLE]).await;
    assert_eq!(again.rows_restored, 0, "{again:?}");
    assert_eq!(again.tables_clean, 1, "{again:?}");

    exec(&client, &url, &format!("DROP TABLE IF EXISTS {TABLE}")).await;
}

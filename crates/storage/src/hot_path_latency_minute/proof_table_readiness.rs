//! Cold-path readback of the three proof tables' actual identities.

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;

use super::{DEDUP_KEY_HOT_PATH_LATENCY_MINUTE, HOT_PATH_LATENCY_MINUTE_TABLE};
use crate::host_capacity_minute::{
    CAPACITY_VERDICT_DAILY_TABLE, DEDUP_KEY_CAPACITY_VERDICT_DAILY, DEDUP_KEY_HOST_CAPACITY_MINUTE,
    HOST_CAPACITY_MINUTE_TABLE,
};

pub(super) const TABLE_KEYS: [(&str, &str); 3] = [
    (
        HOT_PATH_LATENCY_MINUTE_TABLE,
        DEDUP_KEY_HOT_PATH_LATENCY_MINUTE,
    ),
    (HOST_CAPACITY_MINUTE_TABLE, DEDUP_KEY_HOST_CAPACITY_MINUTE),
    (
        CAPACITY_VERDICT_DAILY_TABLE,
        DEDUP_KEY_CAPACITY_VERDICT_DAILY,
    ),
];

/// Every response is a small DDL acknowledgement or a fixed proof-table
/// metadata projection. Bound bytes before JSON parsing, including when the
/// peer omits Content-Length or supplies a misleading length with chunked IO.
pub(super) const QUERY_RESPONSE_MAX_BYTES: usize = 64 * 1024;

/// A 2xx status is insufficient: SQL refusals and unreadable bodies fail closed.
pub(super) async fn exec(client: &reqwest::Client, url: &str, sql: &str) -> Result<Value> {
    let mut response = client
        .get(url)
        .query(&[("query", sql)])
        .send()
        .await
        .context("proof-table query request")?
        .error_for_status()
        .context("proof-table query status")?;
    let max_bytes = u64::try_from(QUERY_RESPONSE_MAX_BYTES).context("response cap conversion")?;
    // Check the declared header itself as well as the decoded body's hint.
    // Transfer-Encoding may make the hint unknown despite a length header.
    for declared in response
        .headers()
        .get_all(reqwest::header::CONTENT_LENGTH)
        .iter()
    {
        let declared = declared
            .to_str()
            .context("invalid proof-table Content-Length")?
            .trim()
            .parse::<u64>()
            .context("invalid proof-table Content-Length")?;
        ensure!(
            declared <= max_bytes,
            "proof-table declared response exceeds {QUERY_RESPONSE_MAX_BYTES}-byte limit"
        );
    }
    ensure!(
        response
            .content_length()
            .is_none_or(|length| length <= max_bytes),
        "proof-table declared response exceeds {QUERY_RESPONSE_MAX_BYTES}-byte limit"
    );
    // One fixed allocation; neither an untrusted length nor repeated chunks
    // can grow this buffer beyond the cap. Check each chunk before copying.
    let mut body = Vec::with_capacity(QUERY_RESPONSE_MAX_BYTES);
    while let Some(chunk) = response
        .chunk()
        .await
        .context("proof-table response body")?
    {
        ensure!(
            chunk.len() <= QUERY_RESPONSE_MAX_BYTES.saturating_sub(body.len()),
            "proof-table streamed response exceeds {QUERY_RESPONSE_MAX_BYTES}-byte limit"
        );
        body.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&body).context("proof-table query JSON")?;
    ensure!(
        value.is_object(),
        "proof-table query returned no JSON object"
    );
    if let Some(error) = value.get("error") {
        bail!("proof-table query was refused: {error}");
    }
    Ok(value)
}

fn dataset(value: &Value) -> Result<&[Value]> {
    ensure!(
        value.get("error").is_none(),
        "metadata contains an SQL error"
    );
    value
        .get("dataset")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .context("metadata has no dataset")
}

/// Require the exact key set, a WAL table with DEDUP on, and designated `ts`.
/// Metadata order is immaterial; duplicate/malformed rows are refused.
pub(super) fn validate(flags: &Value, columns: &Value, keys: &str) -> Result<()> {
    let flags = dataset(flags)?;
    ensure!(flags.len() == 1, "proof table absent or ambiguous");
    let flags = flags[0].as_array().context("malformed table flags")?;
    ensure!(
        flags.len() == 2 && flags[0].as_bool() == Some(true) && flags[1].as_bool() == Some(true),
        "proof table must have DEDUP and WAL enabled"
    );
    let expected: std::collections::BTreeSet<&str> = keys.split(',').map(str::trim).collect();
    let mut seen = std::collections::BTreeSet::new();
    let mut actual = std::collections::BTreeSet::new();
    let mut designated = None;
    for row in dataset(columns)? {
        let row = row.as_array().context("malformed column metadata")?;
        ensure!(row.len() == 4, "incomplete column metadata");
        let name = row[0].as_str().context("column name is not a string")?;
        let ty = row[1].as_str().context("column type is not a string")?;
        let is_designated = row[2].as_bool().context("invalid designated flag")?;
        let is_key = row[3].as_bool().context("invalid upsertKey flag")?;
        ensure!(seen.insert(name), "duplicate column metadata: {name}");
        if is_designated {
            ensure!(designated.is_none(), "multiple designated timestamps");
            ensure!(ty == "TIMESTAMP", "designated ts has the wrong type");
            designated = Some(name);
        }
        if is_key {
            let expected_type = match name {
                "ts" | "trading_date_ist" => "TIMESTAMP",
                "stage" | "feed" => "SYMBOL",
                _ => bail!("unexpected proof-table key: {name}"),
            };
            ensure!(ty == expected_type, "key {name} has the wrong type");
            actual.insert(name);
        }
    }
    ensure!(designated == Some("ts"), "proof table must designate ts");
    ensure!(
        actual == expected,
        "proof table has the wrong DEDUP key set"
    );
    Ok(())
}

pub(super) async fn verify_all(client: &reqwest::Client, url: &str) -> Result<()> {
    for (table, keys) in TABLE_KEYS {
        // The flags projection is shared with the existing contract-repair
        // verifier; the column projection adds types and the timestamp flag.
        let flags = exec(
            client,
            url,
            &crate::contract_name_repair::table_flags_sql(table),
        )
        .await
        .with_context(|| format!("proof table {table} flags"))?;
        let sql =
            format!("SELECT \"column\", type, designated, upsertKey FROM table_columns('{table}')");
        let columns = exec(client, url, &sql)
            .await
            .with_context(|| format!("proof table {table} columns"))?;
        validate(&flags, &columns, keys)
            .with_context(|| format!("proof table {table} is not ready"))?;
    }
    Ok(())
}

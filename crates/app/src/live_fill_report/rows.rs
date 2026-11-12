//! Reads one IST day of `live_order_events` (plan item LF-7). Cold, once per
//! report: one query, O(rows) parse, at most [`REPORT_ROW_CAP`] rows.
//!
//! SPACE at the cap (Assumed, estimated, not measured): about 500 bytes of
//! response text per row (about 25 MB), a JSON tree of it about three times
//! that (about 75 MB), then the typed rows (about 30 MB). The body is freed
//! once parsed and the tree is consumed row by row as the typed rows are
//! built, so the peak is about the tree plus the body, roughly 0.1 GB, for one
//! run a day. The parse runs off the runtime worker (`off_worker`).

use std::time::Duration;

use chrono::NaiveDate;

/// Most rows one report reads. More than this makes the report truncated
/// (Amber); the query asks for one more so truncation is visible. An order
/// writes about seven rows (decision, built, sent, ack, fill, cancel or
/// reject), so 50,000 covers some 7,000 orders, 14 times the 500 the report
/// re-prices. Lowered from 200,000 (LF-7 review finding 14): the read holds
/// the body and a JSON tree of it, so the cap is what bounds the peak.
pub const REPORT_ROW_CAP: usize = 50_000;

/// The day read's bound. The shared probe client's own timeout is 2 s, so the
/// request sets this one (a per-request timeout overrides the client's).
pub const REPORT_QUERY_TIMEOUT_SECS: u64 = 60;

/// The columns the report reads, in query order.
pub const EVENT_COLUMNS: &[&str] = &[
    "ts",
    "event",
    "signal_id",
    "seq",
    "recipe",
    "recipe_version",
    "security_id",
    "segment",
    "symbol",
    "side",
    "qty",
    "candle_end",
    "decision_ts",
    "best_bid_paise",
    "best_bid_qty",
    "best_ask_paise",
    "best_ask_qty",
    "ltp_paise",
    "limit_paise",
    "queue_wait_us",
    "feed_lag_us",
    "api_rtt_us",
    "fill_qty",
    "avg_price_paise",
    "remainder_qty",
    "model_price_paise",
    "worse_than_model",
    "reason",
    "tick_paise",
    "lot_size",
    "participation_bps",
    "max_fill_seconds",
];

/// One stored event row (an owned subset of `live_order_events`).
/// Timestamps are IST epoch nanoseconds (IST written as Z, at microseconds).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventRow {
    pub ts_ns: i64,
    pub event: String,
    pub signal_id: i64,
    pub seq: i64,
    pub recipe: String,
    pub recipe_version: i64,
    pub security_id: i64,
    pub segment: String,
    pub symbol: String,
    pub side: String,
    pub qty: i64,
    pub candle_end_ns: Option<i64>,
    pub decision_ns: Option<i64>,
    pub best_bid_paise: Option<i64>,
    pub best_bid_qty: Option<i64>,
    pub best_ask_paise: Option<i64>,
    pub best_ask_qty: Option<i64>,
    pub ltp_paise: Option<i64>,
    pub limit_paise: Option<i64>,
    pub queue_wait_us: Option<i64>,
    pub feed_lag_us: Option<i64>,
    pub api_rtt_us: Option<i64>,
    pub fill_qty: Option<i64>,
    pub avg_price_paise: Option<i64>,
    pub remainder_qty: Option<i64>,
    pub model_price_paise: Option<i64>,
    pub worse_than_model: Option<String>,
    pub reason: Option<String>,
    pub tick_paise: Option<i64>,
    pub lot_size: Option<i64>,
    pub participation_bps: Option<i64>,
    pub max_fill_seconds: Option<i64>,
}

/// The day's query. `day` is a validated date, so the SQL holds no caller
/// text. Pure. The `LIMIT` bounds the REPLY, not the server: `ORDER BY` runs
/// before it, so QuestDB sorts the whole day's `live_order_events` first
/// (O(day rows log day rows) on the server; LF-7 review finding 8).
#[must_use]
pub fn day_sql(day: NaiveDate) -> String {
    format!(
        "SELECT {} FROM live_order_events WHERE ts IN '{}' AND feed = 'dhan' \
         ORDER BY signal_id, seq, ts LIMIT {}",
        EVENT_COLUMNS.join(", "),
        day.format("%Y-%m-%d"),
        REPORT_ROW_CAP + 1
    )
}

/// QuestDB timestamp text (`2026-11-12T10:00:05.123456Z`) to epoch nanos.
#[must_use]
pub fn parse_ts_nanos(v: &serde_json::Value) -> Option<i64> {
    let s = v.as_str()?;
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .and_then(|d| d.timestamp_nanos_opt())
}

/// An integer cell: a JSON number or a numeric string; `None` for null.
#[must_use]
pub fn json_i64(v: &serde_json::Value) -> Option<i64> {
    match v {
        serde_json::Value::Number(n) => n.as_i64(),
        serde_json::Value::String(s) => s.trim().parse::<i64>().ok(),
        serde_json::Value::Bool(b) => Some(i64::from(*b)),
        _ => None,
    }
}

fn json_text(v: &serde_json::Value) -> Option<String> {
    v.as_str().map(str::to_owned).filter(|s| !s.is_empty())
}

/// Column positions by name from a QuestDB `/exec` body's `columns` array.
fn column_index(value: &serde_json::Value) -> Result<Vec<Option<usize>>, String> {
    let columns = value
        .get("columns")
        .and_then(|c| c.as_array())
        .ok_or_else(|| "response has no `columns` array".to_owned())?;
    let names: Vec<&str> = columns
        .iter()
        .map(|c| c.get("name").and_then(|n| n.as_str()).unwrap_or_default())
        .collect();
    Ok(EVENT_COLUMNS
        .iter()
        .map(|want| names.iter().position(|n| n == want))
        .collect())
}

/// Parses a QuestDB `/exec` body for [`day_sql`] into rows, by column name (a
/// missing column reads as NULL). Returns the rows (at most
/// [`REPORT_ROW_CAP`]) and whether the day held more. A row without its
/// designated `ts`, `event` or `signal_id` is skipped. O(rows).
///
/// # Errors
/// The body is not JSON or has no `columns` / `dataset` array.
pub fn parse_exec_body(body: &str) -> Result<(Vec<EventRow>, bool), String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| "response is not valid JSON".to_owned())?;
    rows_from_value(value)
}

/// [`parse_exec_body`] on an owned body, freeing the text as soon as it is
/// parsed (the day read's peak, LF-7 review finding 14). O(rows).
///
/// # Errors
/// As [`parse_exec_body`].
pub fn parse_exec_body_owned(body: String) -> Result<(Vec<EventRow>, bool), String> {
    let parsed = serde_json::from_str::<serde_json::Value>(&body);
    drop(body);
    rows_from_value(parsed.map_err(|_| "response is not valid JSON".to_owned())?)
}

/// The rows of a parsed body. The dataset is taken out of the tree and
/// consumed row by row, so the tree shrinks as the typed rows grow. O(rows).
fn rows_from_value(mut value: serde_json::Value) -> Result<(Vec<EventRow>, bool), String> {
    let idx = column_index(&value)?;
    let dataset = match value.get_mut("dataset").map(serde_json::Value::take) {
        Some(serde_json::Value::Array(a)) => a,
        _ => return Err("response has no `dataset` array".to_owned()),
    };
    drop(value);
    let truncated = dataset.len() > REPORT_ROW_CAP;
    let mut rows = Vec::with_capacity(dataset.len().min(REPORT_ROW_CAP));
    let null = serde_json::Value::Null;
    for raw in dataset.into_iter().take(REPORT_ROW_CAP) {
        let Some(cells) = raw.as_array() else {
            continue;
        };
        let cell = |i: usize| -> &serde_json::Value {
            idx.get(i)
                .copied()
                .flatten()
                .and_then(|p| cells.get(p))
                .unwrap_or(&null)
        };
        let (Some(ts_ns), Some(event), Some(signal_id)) = (
            parse_ts_nanos(cell(0)),
            json_text(cell(1)),
            json_i64(cell(2)),
        ) else {
            continue;
        };
        rows.push(EventRow {
            ts_ns,
            event,
            signal_id,
            seq: json_i64(cell(3)).unwrap_or(0),
            recipe: json_text(cell(4)).unwrap_or_default(),
            recipe_version: json_i64(cell(5)).unwrap_or(0),
            security_id: json_i64(cell(6)).unwrap_or(-1),
            segment: json_text(cell(7)).unwrap_or_default(),
            symbol: json_text(cell(8)).unwrap_or_default(),
            side: json_text(cell(9)).unwrap_or_default(),
            qty: json_i64(cell(10)).unwrap_or(0),
            candle_end_ns: parse_ts_nanos(cell(11)),
            decision_ns: parse_ts_nanos(cell(12)),
            best_bid_paise: json_i64(cell(13)),
            best_bid_qty: json_i64(cell(14)),
            best_ask_paise: json_i64(cell(15)),
            best_ask_qty: json_i64(cell(16)),
            ltp_paise: json_i64(cell(17)),
            limit_paise: json_i64(cell(18)),
            queue_wait_us: json_i64(cell(19)),
            feed_lag_us: json_i64(cell(20)),
            api_rtt_us: json_i64(cell(21)),
            fill_qty: json_i64(cell(22)),
            avg_price_paise: json_i64(cell(23)),
            remainder_qty: json_i64(cell(24)),
            model_price_paise: json_i64(cell(25)),
            worse_than_model: json_text(cell(26)),
            reason: json_text(cell(27)),
            tick_paise: json_i64(cell(28)),
            lot_size: json_i64(cell(29)),
            participation_bps: json_i64(cell(30)),
            max_fill_seconds: json_i64(cell(31)),
        });
    }
    Ok((rows, truncated))
}

/// Reads one IST day of `live_order_events` (feed `dhan`), under
/// [`REPORT_QUERY_TIMEOUT_SECS`].
///
/// # Errors
/// A text reason: client, timeout, request, status, body or parse.
// TEST-EXEMPT: one bounded QuestDB GET; the SQL (`day_sql`) and the parse (`parse_exec_body`) are unit-tested.
pub async fn read_day(exec_url: &str, day: NaiveDate) -> Result<(Vec<EventRow>, bool), String> {
    let client = tickvault_storage::http_client::shared_probe_client()
        .map_err(|e| format!("client build: {e}"))?;
    let sql = day_sql(day);
    let limit = Duration::from_secs(REPORT_QUERY_TIMEOUT_SECS);
    let resp = tokio::time::timeout(
        limit,
        client
            .get(exec_url)
            .timeout(limit)
            .query(&[("query", sql.as_str())])
            .send(),
    )
    .await
    .map_err(|_| "timeout".to_owned())?
    .map_err(|e| format!("request: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("status {status}"));
    }
    let body = tokio::time::timeout(limit, resp.text())
        .await
        .map_err(|_| "body timeout".to_owned())?
        .map_err(|e| format!("body: {e}"))?;
    // The parse holds the body and a JSON tree of it: off the runtime worker.
    tickvault_storage::off_worker::off_worker(move || parse_exec_body_owned(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(rows: &[serde_json::Value]) -> String {
        let cols: Vec<serde_json::Value> = EVENT_COLUMNS
            .iter()
            .map(|c| serde_json::json!({"name": c, "type": "STRING"}))
            .collect();
        serde_json::json!({"columns": cols, "dataset": rows}).to_string()
    }

    fn row(signal: i64, seq: i64) -> serde_json::Value {
        let mut cells = vec![serde_json::Value::Null; EVENT_COLUMNS.len()];
        cells[0] = serde_json::json!("2026-11-12T10:00:05.123456Z");
        cells[1] = serde_json::json!("decision");
        cells[2] = serde_json::json!(signal);
        cells[3] = serde_json::json!(seq);
        cells[6] = serde_json::json!("2885");
        cells[7] = serde_json::json!("NSE_FNO");
        cells[10] = serde_json::json!(50);
        cells[28] = serde_json::json!(5);
        serde_json::Value::Array(cells)
    }

    #[test]
    fn test_day_sql_names_every_column_the_day_and_the_cap() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 12).unwrap();
        let sql = day_sql(day);
        assert!(sql.contains("FROM live_order_events WHERE ts IN '2026-11-12'"));
        assert!(sql.contains("feed = 'dhan'"));
        assert!(sql.contains("ORDER BY signal_id, seq, ts LIMIT 50001"));
        for c in EVENT_COLUMNS {
            assert!(sql.contains(c), "{c}");
        }
    }

    #[test]
    fn test_parse_exec_body_nulls_bad_json_and_cap() {
        // Bad JSON and missing arrays are errors.
        assert!(parse_exec_body("not json").is_err());
        assert!(parse_exec_body("{}").is_err());
        assert!(parse_exec_body(r#"{"columns": []}"#).is_err());
        // NULL cells read as None; a numeric string reads as a number.
        let (rows, truncated) = parse_exec_body(&body(&[row(7, 0)])).expect("parse");
        assert!(!truncated);
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.signal_id, 7);
        assert_eq!(r.security_id, 2885);
        assert_eq!(r.segment, "NSE_FNO");
        assert_eq!(r.tick_paise, Some(5));
        assert_eq!(r.lot_size, None);
        assert_eq!(r.reason, None);
        assert_eq!(r.ts_ns, 1_794_477_605_123_456_000);
        // A row with no designated ts or signal is skipped.
        let mut bad = row(8, 0);
        bad[0] = serde_json::Value::Null;
        let (rows, _) = parse_exec_body(&body(&[bad])).expect("parse");
        assert!(rows.is_empty());
        // A missing column reads as NULL, never an error.
        let partial = r#"{"columns":[{"name":"ts"},{"name":"event"},{"name":"signal_id"}],
            "dataset":[["2026-11-12T10:00:05Z","fill",3]]}"#;
        let (rows, _) = parse_exec_body(partial).expect("parse");
        assert_eq!(rows[0].event, "fill");
        assert_eq!(rows[0].fill_qty, None);
        // More than the cap: truncated, and only the cap is kept.
        let many: Vec<serde_json::Value> = (0..=REPORT_ROW_CAP as i64).map(|i| row(i, 0)).collect();
        let (rows, truncated) = parse_exec_body(&body(&many)).expect("parse");
        assert!(truncated);
        assert_eq!(rows.len(), REPORT_ROW_CAP);
    }

    #[test]
    fn test_parse_ts_nanos_and_json_i64() {
        assert_eq!(
            parse_ts_nanos(&serde_json::json!("1970-01-01T00:00:01Z")),
            Some(1_000_000_000)
        );
        assert_eq!(parse_ts_nanos(&serde_json::json!("nope")), None);
        assert_eq!(parse_ts_nanos(&serde_json::Value::Null), None);
        assert_eq!(json_i64(&serde_json::json!(-4)), Some(-4));
        assert_eq!(json_i64(&serde_json::json!(" 12 ")), Some(12));
        assert_eq!(json_i64(&serde_json::json!(true)), Some(1));
        assert_eq!(json_i64(&serde_json::Value::Null), None);
    }

    /// Review finding 14 (LF-7): the owned parse gives the same rows as the
    /// borrowed one (it only frees the text early), and the cap is the
    /// lowered 50,000, still far above the 500 re-priced orders (about seven
    /// rows each).
    #[test]
    fn test_parse_exec_body_owned_equals_parse_exec_body_and_cap_is_lowered() {
        let text = body(&[row(7, 0), row(8, 1)]);
        assert_eq!(parse_exec_body_owned(text.clone()), parse_exec_body(&text));
        assert!(parse_exec_body_owned("x".to_owned()).is_err());
        assert!(parse_exec_body_owned(r#"{"columns": []}"#.to_owned()).is_err());
        assert_eq!(REPORT_ROW_CAP, 50_000);
        assert!(REPORT_ROW_CAP >= super::super::recheck::RECHECK_MAX_ORDERS * 7 * 10);
    }

    #[tokio::test]
    async fn test_read_day_unreachable_is_an_error() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 12).unwrap();
        assert!(read_day("http://127.0.0.1:1/exec", day).await.is_err());
    }
}

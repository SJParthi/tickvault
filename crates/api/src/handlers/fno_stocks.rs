//! F&O stocks price movers — `GET /api/fno-stocks`.
//!
//! Owner, 30 Oct 2026: "yes add the fno stocks page to dashboard dude". The
//! same view as Dhan's Markets > Stocks > Price Movers filtered to "F&O
//! Stocks": every F&O stock with its last price, change and change % against
//! the previous session's close, the day's volume and traded value. Built
//! from OUR captured `ticks`, never from a broker REST call.
//!
//! # Where each number comes from
//!
//! | Field | Source |
//! |---|---|
//! | which stocks | today's F&O underlying list, `data/instrument-cache/dhan-fno-underlyings-<IST date>.json`, rows tagged `FNO_UNDERLYING` on `NSE_EQ`, written by the morning instrument build |
//! | LTP | `ticks.ltp` of the stock's latest `NSE_EQ` row |
//! | previous close | `ticks.close` of that row: on a Quote/Full packet Dhan carries the PREVIOUS session's close there (Ticket #5525125, `docs/dhan-ref/03-live-market-feed-websocket.md` §7.3). NULL on a row without it, and then change is `null` |
//! | change, change % | `ltp − prev_close`, `change / prev_close × 100`, only when both are finite and positive |
//! | volume | `ticks.volume`, the day's cumulative volume |
//! | value (Cr.) | an ESTIMATE, `volume × ticks.avg_price ÷ 1 crore`. Dhan's reference does not define its "Value" column, so the page labels this a turnover estimate |
//!
//! The live spot universe is NOT only F&O stocks (config subscribes the Nifty
//! Total Market list), so the F&O list is the filter, not the `NSE_EQ`
//! segment alone. A stock on the list that is not subscribed simply has no
//! row; the response reports how many of the list carry a price.
//!
//! # Honesty rules
//!
//! - Outside the trading session (`[09:00, 15:40)` IST on a trading day) the
//!   endpoint runs no query and answers `status = "closed"` with no rows.
//! - Inside it, only rows from the last [`FNO_STOCKS_WINDOW_SECS`] are read,
//!   and the answer is `live` only when the newest row is at most
//!   [`FNO_STOCKS_LIVE_MAX_AGE_SECS`] old; otherwise `stale`, which the page
//!   shows as a warning, never as live.
//! - A failed query is `unreachable` (no answer) or `query_failed` (QuestDB
//!   answered with an error or a malformed body), never an empty "no movers".
//!
//! # Identity (I-P1-11)
//!
//! Every stock is keyed on `(security_id, exchange segment code)`. A row of a
//! different segment with the same id (for example an `IDX_I` index) is
//! dropped even if QuestDB returns it.
//!
//! # Complexity
//!
//! Cold path, never per tick. Per refresh: one bounded QuestDB query
//! (`LATEST ON ts PARTITION BY security_id, segment` over at most
//! [`FNO_STOCKS_WINDOW_SECS`] of `ticks`, ids in an `IN` list), then
//! O(F&O stocks) to parse and O(k log k) to sort the k priced rows (k ≤ about
//! 220). The F&O list file is read and parsed once per IST date (O(list
//! rows)) and kept; the 2 s response cache and the shared public rate limiter
//! bound how often the query runs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tickvault_common::constants::{EXCHANGE_SEGMENT_NSE_EQ, IST_UTC_OFFSET_NANOS};
use tickvault_common::segment::{segment_code_to_str, segment_str_to_code};

use crate::response_cache::cached_json_response;
use crate::state::SharedAppState;

/// Folder the morning instrument build writes the day's artifacts into,
/// relative to the process working directory (the app and this API run in
/// one process). Pinned against the writer by a test in the app crate.
pub const FNO_UNDERLYING_ARTIFACT_DIR: &str = "data/instrument-cache";

/// File name prefix of the day's F&O underlying list; the full name is
/// `<prefix><YYYY-MM-DD>.json`. Pinned against the writer by a test in the
/// app crate.
pub const FNO_UNDERLYING_ARTIFACT_PREFIX: &str = "dhan-fno-underlyings-";

/// The `index_name` tag the writer puts on F&O stock rows (the same file also
/// carries NSE index rows, which are not F&O stocks).
pub const FNO_UNDERLYING_TAG: &str = "FNO_UNDERLYING";

/// Per-request timeout for the movers query (cold path).
const FNO_STOCKS_QUERY_TIMEOUT_SECS: u64 = 3;

/// How far back the query reads `ticks`. F&O stocks trade every few seconds
/// in session, so 15 minutes keeps the scan small and still finds each one.
pub const FNO_STOCKS_WINDOW_SECS: i64 = 900;

/// The newest row may be at most this old for the answer to read `live`.
pub const FNO_STOCKS_LIVE_MAX_AGE_SECS: i64 = 60;

/// Upper bound on rows the query may return. The F&O list is about 220
/// stocks; the bound only stops a malformed list from asking for more.
const FNO_STOCKS_MAX_ROWS: usize = 1_000;

/// One crore, for the value column.
const RUPEES_PER_CRORE: f64 = 10_000_000.0;

/// What the page shows under the value column.
const VALUE_BASIS: &str = "Value (Cr.) is an estimate: day volume x average traded price / 1 crore";

/// The `feed` whose rows the page reads (the one live market-data feed).
const FNO_STOCKS_FEED: &str = "dhan";

/// One F&O stock on the page.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FnoStockRow {
    pub security_id: u64,
    /// Always `"NSE_EQ"`.
    pub segment: &'static str,
    /// Trading symbol from the F&O list (for example `RELIANCE`).
    pub name: String,
    pub ltp: f64,
    /// Previous session's close; `None` when the row carried none.
    pub prev_close: Option<f64>,
    /// `ltp − prev_close`; `None` without a usable previous close.
    pub change: Option<f64>,
    /// `change / prev_close × 100`; `None` without a usable previous close.
    pub change_pct: Option<f64>,
    /// Day's cumulative volume; `None` when NULL.
    pub volume: Option<u64>,
    /// Turnover estimate in crore; `None` without volume or average price.
    pub value_cr: Option<f64>,
    /// Exchange time of the row, `HH:MM:SS` IST.
    pub last_trade_ist: String,
}

/// The full `GET /api/fno-stocks` answer.
#[derive(Debug, Serialize)]
pub struct FnoStocksResponse {
    /// `live`, `stale`, `closed`, `no_fno_list`, `no_data`, `unreachable`
    /// or `query_failed`. Only `live` may be shown as live.
    pub status: &'static str,
    /// Trading session open right now (weekend and holiday aware).
    pub market_open: bool,
    /// Time of the newest row, `HH:MM:SS` IST; `None` without rows.
    pub as_of_ist: Option<String>,
    /// Seconds between the newest row and now; `None` without rows.
    pub data_age_secs: Option<i64>,
    /// F&O stocks on today's list (0 when the list is missing).
    pub fno_stocks_total: usize,
    /// F&O stocks with a price in the window.
    pub stocks_priced: usize,
    /// How far back the query reads, in seconds.
    pub window_secs: i64,
    /// How the value column is computed.
    pub value_basis: &'static str,
    /// Priced stocks, change % descending (no change last).
    pub stocks: Vec<FnoStockRow>,
}

impl FnoStocksResponse {
    fn empty(status: &'static str, market_open: bool, fno_stocks_total: usize) -> Self {
        Self {
            status,
            market_open,
            as_of_ist: None,
            data_age_secs: None,
            fno_stocks_total,
            stocks_priced: 0,
            window_secs: FNO_STOCKS_WINDOW_SECS,
            value_basis: VALUE_BASIS,
            stocks: Vec::new(),
        }
    }
}

/// Today's F&O stocks, keyed on the composite `(security_id, segment code)`.
#[derive(Debug, Default)]
pub struct FnoUnderlyings {
    names: HashMap<(u64, u8), String>,
}

impl FnoUnderlyings {
    /// Number of F&O stocks on the list.
    #[must_use]
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// True when the list holds no stock.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// The symbol of the stock with this composite key, if it is on the list.
    #[must_use]
    pub fn name_of(&self, security_id: u64, segment_code: u8) -> Option<&str> {
        self.names
            .get(&(security_id, segment_code))
            .map(String::as_str)
    }

    /// Security ids on the list, ascending (all `NSE_EQ`).
    fn sorted_ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self.names.keys().map(|(id, _)| *id).collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }
}

#[derive(Deserialize)]
struct ArtifactEnvelope {
    mappings: Vec<ArtifactEntry>,
}

#[derive(Deserialize)]
struct ArtifactEntry {
    index_name: String,
    symbol: String,
    security_id: u64,
    exchange_segment: u8,
}

/// Path of the day's F&O underlying list inside `dir`.
#[must_use]
pub fn fno_underlying_artifact_path(dir: &Path, date_ist: &str) -> PathBuf {
    dir.join(format!("{FNO_UNDERLYING_ARTIFACT_PREFIX}{date_ist}.json"))
}

/// Parses the F&O underlying list: rows tagged [`FNO_UNDERLYING_TAG`] on
/// `NSE_EQ` only. `None` when the bytes are not the writer's envelope.
#[must_use]
pub fn parse_fno_underlyings(bytes: &[u8]) -> Option<FnoUnderlyings> {
    let envelope: ArtifactEnvelope = serde_json::from_slice(bytes).ok()?;
    let mut names = HashMap::with_capacity(envelope.mappings.len());
    for entry in envelope.mappings {
        if entry.index_name == FNO_UNDERLYING_TAG
            && entry.exchange_segment == EXCHANGE_SEGMENT_NSE_EQ
            && entry.security_id != 0
        {
            names
                .entry((entry.security_id, entry.exchange_segment))
                .or_insert(entry.symbol);
        }
    }
    Some(FnoUnderlyings { names })
}

/// The last list read, keyed by its path (the path carries the IST date).
static FNO_LIST_CACHE: Mutex<Option<(PathBuf, Arc<FnoUnderlyings>)>> = Mutex::new(None);

/// Loads the day's list, reading the file only when the path changed (a new
/// IST date) or the last read failed. `None` when the file is missing,
/// unreadable, malformed or empty.
fn load_fno_underlyings(path: &Path) -> Option<Arc<FnoUnderlyings>> {
    let mut slot = FNO_LIST_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((cached_path, list)) = slot.as_ref()
        && cached_path == path
    {
        return Some(Arc::clone(list));
    }
    let bytes = std::fs::read(path).ok()?;
    let list = parse_fno_underlyings(&bytes)?;
    if list.is_empty() {
        return None;
    }
    let list = Arc::new(list);
    *slot = Some((path.to_path_buf(), Arc::clone(&list)));
    Some(list)
}

/// IST wall-clock now, as a naive date-time (the same convention `ticks.ts`
/// is stored in).
fn now_ist_naive() -> chrono::NaiveDateTime {
    (chrono::Utc::now() + chrono::Duration::nanoseconds(IST_UTC_OFFSET_NANOS)).naive_utc()
}

/// Start of the query window: `now − window`, never before IST midnight.
#[must_use]
fn window_start(now_ist: chrono::NaiveDateTime) -> chrono::NaiveDateTime {
    let midnight = now_ist.date().and_time(chrono::NaiveTime::MIN);
    let start = now_ist - chrono::Duration::seconds(FNO_STOCKS_WINDOW_SECS);
    start.max(midnight)
}

/// The movers SQL. Every interpolated value is a number or a constant; no
/// caller text reaches it.
#[must_use]
fn build_fno_stocks_sql(ids: &[u64], window_start_ist: chrono::NaiveDateTime) -> String {
    let id_list = ids.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
    format!(
        "SELECT security_id, segment, ltp, close, volume, avg_price, ts FROM ticks \
         WHERE segment = '{}' AND feed = '{FNO_STOCKS_FEED}' AND ts >= '{}' \
         AND security_id IN ({id_list}) \
         LATEST ON ts PARTITION BY security_id, segment LIMIT {FNO_STOCKS_MAX_ROWS}",
        segment_code_to_str(EXCHANGE_SEGMENT_NSE_EQ),
        window_start_ist.format("%Y-%m-%dT%H:%M:%S%.6fZ"),
    )
}

/// One parsed `ticks` row.
#[derive(Debug, Clone, PartialEq)]
struct TickRow {
    security_id: u64,
    segment_code: u8,
    ltp: f64,
    close: Option<f64>,
    volume: Option<u64>,
    avg_price: Option<f64>,
    ts: chrono::NaiveDateTime,
}

/// Parses one row in [`build_fno_stocks_sql`] column order. `None` for a row
/// missing a mandatory field.
fn parse_tick_row(row: &[serde_json::Value]) -> Option<TickRow> {
    let ts_text = row.get(6)?.as_str()?;
    let ts = chrono::NaiveDateTime::parse_from_str(ts_text.get(..19)?, "%Y-%m-%dT%H:%M:%S").ok()?;
    Some(TickRow {
        security_id: row.first()?.as_u64()?,
        segment_code: segment_str_to_code(row.get(1)?.as_str()?)?,
        ltp: row.get(2)?.as_f64()?,
        close: row.get(3).and_then(serde_json::Value::as_f64),
        volume: row.get(4).and_then(serde_json::Value::as_u64),
        avg_price: row.get(5).and_then(serde_json::Value::as_f64),
        ts,
    })
}

/// Why the query gave no rows.
#[derive(Debug, PartialEq, Eq)]
enum QueryFailure {
    /// No answer (refused, timed out).
    Unreachable,
    /// An answer that was an error or not a dataset.
    Refused,
}

/// Runs the movers query. Malformed rows are skipped (counted by the caller
/// as unpriced), a malformed body is a failure.
async fn query_fno_rows(
    client: &reqwest::Client,
    base_url: &str,
    sql: &str,
    timeout: Duration,
) -> Result<Vec<TickRow>, QueryFailure> {
    let resp = client
        .get(format!("{base_url}/exec"))
        .timeout(timeout)
        .query(&[("query", sql)])
        .send()
        .await
        .map_err(|_| QueryFailure::Unreachable)?;
    if !resp.status().is_success() {
        return Err(QueryFailure::Refused);
    }
    let body: serde_json::Value = resp.json().await.map_err(|_| QueryFailure::Refused)?;
    let dataset = body
        .get("dataset")
        .and_then(serde_json::Value::as_array)
        .ok_or(QueryFailure::Refused)?;
    Ok(dataset
        .iter()
        .filter_map(|row| parse_tick_row(row.as_array()?))
        .collect())
}

/// Change and change % against the previous close, only when both prices are
/// finite and positive.
#[must_use]
fn change_against(ltp: f64, prev_close: Option<f64>) -> (Option<f64>, Option<f64>) {
    match prev_close {
        Some(prev) if prev.is_finite() && prev > 0.0 && ltp.is_finite() && ltp > 0.0 => {
            let change = ltp - prev;
            (Some(change), Some(change / prev * 100.0))
        }
        _ => (None, None),
    }
}

/// Turnover estimate in crore.
#[must_use]
fn value_crore(volume: Option<u64>, avg_price: Option<f64>) -> Option<f64> {
    let volume = volume?;
    let avg = avg_price.filter(|p| p.is_finite() && *p > 0.0)?;
    // A day volume fits in f64 exactly up to 2^53 shares; the column is an
    // estimate in any case.
    let value = volume as f64 * avg / RUPEES_PER_CRORE;
    Some(value)
}

/// Builds the page rows from the query rows: drops rows not on the list
/// (another segment with the same id included), sorts by change % descending.
fn build_rows(list: &FnoUnderlyings, rows: Vec<TickRow>) -> Vec<FnoStockRow> {
    let mut out: Vec<FnoStockRow> = rows
        .into_iter()
        .filter_map(|r| {
            let name = list.name_of(r.security_id, r.segment_code)?;
            if !r.ltp.is_finite() || r.ltp <= 0.0 {
                return None;
            }
            let (change, change_pct) = change_against(r.ltp, r.close);
            Some(FnoStockRow {
                security_id: r.security_id,
                segment: segment_code_to_str(r.segment_code),
                name: name.to_string(),
                ltp: r.ltp,
                prev_close: r.close.filter(|p| p.is_finite() && *p > 0.0),
                change,
                change_pct,
                volume: r.volume,
                value_cr: value_crore(r.volume, r.avg_price),
                last_trade_ist: r.ts.format("%H:%M:%S").to_string(),
            })
        })
        .collect();
    // O(k log k), k ≤ the list size.
    out.sort_by(|a, b| match (a.change_pct, b.change_pct) {
        (Some(x), Some(y)) => y.total_cmp(&x).then_with(|| a.name.cmp(&b.name)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.name.cmp(&b.name),
    });
    out
}

/// Everything the computation needs from outside, so tests can drive it.
struct FnoStocksInputs<'a> {
    client: &'a reqwest::Client,
    base_url: &'a str,
    artifact_dir: &'a Path,
    market_open: bool,
    now_ist: chrono::NaiveDateTime,
    timeout: Duration,
}

/// Computes the answer. Never panics; every failure is a status.
async fn compute_fno_stocks(inputs: FnoStocksInputs<'_>) -> FnoStocksResponse {
    let date = inputs.now_ist.format("%Y-%m-%d").to_string();
    let list = load_fno_underlyings(&fno_underlying_artifact_path(inputs.artifact_dir, &date));
    let total = list.as_ref().map_or(0, |l| l.len());
    if !inputs.market_open {
        return FnoStocksResponse::empty("closed", false, total);
    }
    let Some(list) = list else {
        return FnoStocksResponse::empty("no_fno_list", true, 0);
    };
    let sql = build_fno_stocks_sql(&list.sorted_ids(), window_start(inputs.now_ist));
    let rows = match query_fno_rows(inputs.client, inputs.base_url, &sql, inputs.timeout).await {
        Ok(rows) => rows,
        Err(QueryFailure::Unreachable) => {
            return FnoStocksResponse::empty("unreachable", true, total);
        }
        Err(QueryFailure::Refused) => return FnoStocksResponse::empty("query_failed", true, total),
    };
    let newest = rows
        .iter()
        .filter(|r| list.name_of(r.security_id, r.segment_code).is_some())
        .map(|r| r.ts)
        .max();
    let stocks = build_rows(&list, rows);
    let Some(newest) = newest.filter(|_| !stocks.is_empty()) else {
        return FnoStocksResponse::empty("no_data", true, total);
    };
    let age = (inputs.now_ist - newest).num_seconds();
    FnoStocksResponse {
        status: if age <= FNO_STOCKS_LIVE_MAX_AGE_SECS {
            "live"
        } else {
            "stale"
        },
        market_open: true,
        as_of_ist: Some(newest.format("%H:%M:%S").to_string()),
        data_age_secs: Some(age),
        fno_stocks_total: total,
        stocks_priced: stocks.len(),
        window_secs: FNO_STOCKS_WINDOW_SECS,
        value_basis: VALUE_BASIS,
        stocks,
    }
}

/// `GET /api/fno-stocks` — the F&O stocks price movers. Bearer-gated and on
/// the shared public rate limiter (both in `build_router_with_auth`); the
/// body is cached for 2 s (`SharedAppState::fno_stocks_cache`), so a cache
/// hit runs no query and reads no file. Every answer is 200 with a status;
/// see the module docs.
// TEST-EXEMPT: covered by #[tokio::test] test_get_fno_stocks_cache_hit_skips_the_query (the pub-fn-test-guard greps #[test] files only, not #[tokio::test]).
pub async fn get_fno_stocks(State(state): State<SharedAppState>) -> Response {
    if let Some(body) = state.fno_stocks_cache().get() {
        metrics::counter!("tv_api_cache_hits_total", "endpoint" => "fno_stocks").increment(1);
        return cached_json_response(body, "hit");
    }
    let cfg = state.questdb_config();
    let base_url = format!("http://{}:{}", cfg.host, cfg.http_port);
    let snapshot = compute_fno_stocks(FnoStocksInputs {
        client: state.questdb_http_client(),
        base_url: &base_url,
        artifact_dir: Path::new(FNO_UNDERLYING_ARTIFACT_DIR),
        market_open: tickvault_common::market_hours::is_trading_session_now(),
        now_ist: now_ist_naive(),
        timeout: Duration::from_secs(FNO_STOCKS_QUERY_TIMEOUT_SECS),
    })
    .await;
    match serde_json::to_string(&snapshot) {
        Ok(body) => {
            state.fno_stocks_cache().put(body.clone());
            cached_json_response(body, "miss")
        }
        Err(_) => axum::Json(snapshot).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> chrono::NaiveDateTime {
        chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").expect("valid time")
    }

    const NOW: &str = "2026-10-30 11:15:30";

    /// A temp folder holding today's list: RELIANCE (2885), INFY (1594), and
    /// id 27 on NSE_EQ (whose id an IDX_I index also uses), plus an index row
    /// that must not count.
    fn list_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tv-fno-stocks-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let body = r#"{"count":5,"mappings":[
            {"index_name":"FNO_UNDERLYING","symbol":"RELIANCE","isin":"INE002A01018","security_id":2885,"exchange_segment":1},
            {"index_name":"FNO_UNDERLYING","symbol":"INFY","isin":"INE009A01021","security_id":1594,"exchange_segment":1},
            {"index_name":"FNO_UNDERLYING","symbol":"TWENTYSEVEN","isin":"X","security_id":27,"exchange_segment":1},
            {"index_name":"NSE_INDEX","symbol":"NIFTY 50","isin":"","security_id":13,"exchange_segment":0},
            {"index_name":"FNO_UNDERLYING","symbol":"BADSEG","isin":"","security_id":99,"exchange_segment":0}
        ]}"#;
        std::fs::write(fno_underlying_artifact_path(&dir, "2026-10-30"), body).expect("write");
        dir
    }

    async fn mock(status_line: &'static str, body: String) -> String {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut buf = vec![0u8; 16 * 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
            let resp = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes()).await;
            let _ = stream.shutdown().await;
        });
        format!("http://127.0.0.1:{}", addr.port())
    }

    /// A listener that accepts and never answers.
    async fn black_hole() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        format!("http://127.0.0.1:{}", addr.port())
    }

    async fn run(base_url: &str, dir: &Path, market_open: bool) -> FnoStocksResponse {
        let client = reqwest::Client::new();
        compute_fno_stocks(FnoStocksInputs {
            client: &client,
            base_url,
            artifact_dir: dir,
            market_open,
            now_ist: at(NOW),
            timeout: Duration::from_millis(300),
        })
        .await
    }

    fn dataset(rows: &[&str]) -> String {
        format!(r#"{{"dataset":[{}]}}"#, rows.join(","))
    }

    const RELIANCE: &str =
        r#"[2885,"NSE_EQ",1450.5,1400.0,1200000,1430.25,"2026-10-30T11:15:20.123456Z"]"#;
    const INFY: &str =
        r#"[1594,"NSE_EQ",1500.0,1550.0,800000,1510.0,"2026-10-30T11:15:25.000000Z"]"#;

    // ---- SQL ----

    #[test]
    fn test_build_fno_stocks_sql_is_bounded_keyed_and_constant() {
        let sql = build_fno_stocks_sql(&[1594, 2885], window_start(at(NOW)));
        assert!(
            sql.contains("FROM ticks WHERE segment = 'NSE_EQ' AND feed = 'dhan'"),
            "{sql}"
        );
        assert!(
            sql.contains("AND ts >= '2026-10-30T11:00:30.000000Z'"),
            "{sql}"
        );
        assert!(sql.contains("AND security_id IN (1594,2885)"), "{sql}");
        assert!(
            sql.contains("LATEST ON ts PARTITION BY security_id, segment"),
            "{sql}"
        );
        assert!(
            sql.ends_with(&format!("LIMIT {FNO_STOCKS_MAX_ROWS}")),
            "{sql}"
        );
    }

    #[test]
    fn test_window_start_never_before_ist_midnight() {
        assert_eq!(
            window_start(at("2026-10-30 00:05:00")),
            at("2026-10-30 00:00:00")
        );
        assert_eq!(
            window_start(at("2026-10-30 09:30:00")),
            at("2026-10-30 09:15:00")
        );
    }

    // ---- arithmetic ----

    #[test]
    fn test_change_against_needs_a_finite_positive_prev_close() {
        let (c, p) = change_against(110.0, Some(100.0));
        assert!((c.expect("change") - 10.0).abs() < 1e-9);
        assert!((p.expect("pct") - 10.0).abs() < 1e-9);
        for bad in [
            None,
            Some(0.0),
            Some(-5.0),
            Some(f64::NAN),
            Some(f64::INFINITY),
        ] {
            assert_eq!(change_against(110.0, bad), (None, None), "{bad:?}");
        }
        assert_eq!(change_against(f64::NAN, Some(100.0)), (None, None));
    }

    #[test]
    fn test_value_crore_is_volume_times_average_price() {
        let v = value_crore(Some(1_000_000), Some(1500.0)).expect("value");
        assert!((v - 150.0).abs() < 1e-9);
        assert_eq!(value_crore(None, Some(1500.0)), None);
        assert_eq!(value_crore(Some(10), None), None);
        assert_eq!(value_crore(Some(10), Some(f64::NAN)), None);
        assert_eq!(value_crore(Some(10), Some(0.0)), None);
    }

    // ---- the list ----

    #[test]
    fn test_parse_fno_underlyings_keeps_only_tagged_nse_eq_rows() {
        let dir = list_dir("parse");
        let bytes = std::fs::read(fno_underlying_artifact_path(&dir, "2026-10-30")).expect("read");
        let list = parse_fno_underlyings(&bytes).expect("parses");
        assert_eq!(list.len(), 3);
        assert_eq!(
            list.name_of(2885, EXCHANGE_SEGMENT_NSE_EQ),
            Some("RELIANCE")
        );
        assert_eq!(list.name_of(13, 0), None, "index rows are not F&O stocks");
        assert_eq!(list.name_of(99, 0), None, "a non-NSE_EQ row is refused");
        assert_eq!(
            list.name_of(27, 0),
            None,
            "composite key: id 27 on IDX_I is not on the list"
        );
        assert!(parse_fno_underlyings(b"not json").is_none());
        assert!(parse_fno_underlyings(br#"{"mappings":7}"#).is_none());
    }

    // ---- the handler computation ----

    #[tokio::test]
    async fn test_closed_market_runs_no_query_and_shows_no_rows() {
        let dir = list_dir("closed");
        // Port 1 refuses: a query would turn the status into `unreachable`.
        let r = run("http://127.0.0.1:1", &dir, false).await;
        assert_eq!(r.status, "closed");
        assert!(r.stocks.is_empty());
        assert!(!r.market_open);
        assert_eq!(r.fno_stocks_total, 3);
    }

    #[tokio::test]
    async fn test_missing_list_is_no_fno_list() {
        let dir = std::env::temp_dir().join("tv-fno-stocks-missing-list-dir");
        let r = run("http://127.0.0.1:1", &dir, true).await;
        assert_eq!(r.status, "no_fno_list");
        assert_eq!(r.fno_stocks_total, 0);
    }

    #[tokio::test]
    async fn test_empty_result_is_no_data_not_live() {
        let dir = list_dir("empty");
        let url = mock("200 OK", dataset(&[])).await;
        let r = run(&url, &dir, true).await;
        assert_eq!(r.status, "no_data");
        assert!(r.stocks.is_empty());
        assert_eq!(r.as_of_ist, None);
    }

    #[tokio::test]
    async fn test_one_row_is_live_with_change_and_value() {
        let dir = list_dir("one");
        let url = mock("200 OK", dataset(&[RELIANCE])).await;
        let r = run(&url, &dir, true).await;
        assert_eq!(r.status, "live");
        assert_eq!(r.stocks_priced, 1);
        assert_eq!(r.as_of_ist.as_deref(), Some("11:15:20"));
        assert_eq!(r.data_age_secs, Some(10));
        let s = &r.stocks[0];
        assert_eq!((s.name.as_str(), s.segment), ("RELIANCE", "NSE_EQ"));
        assert!((s.change.expect("change") - 50.5).abs() < 1e-9);
        assert!((s.change_pct.expect("pct") - 3.607_142_857).abs() < 1e-6);
        assert!((s.value_cr.expect("value") - 171.63).abs() < 1e-9);
        assert_eq!(s.volume, Some(1_200_000));
    }

    #[tokio::test]
    async fn test_rows_sort_by_change_pct_with_no_change_last() {
        let dir = list_dir("sort");
        let no_prev = r#"[27,"NSE_EQ",10.0,null,5,10.0,"2026-10-30T11:15:00.000000Z"]"#;
        let url = mock("200 OK", dataset(&[INFY, no_prev, RELIANCE])).await;
        let r = run(&url, &dir, true).await;
        let names: Vec<&str> = r.stocks.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["RELIANCE", "INFY", "TWENTYSEVEN"]);
        assert_eq!(
            r.stocks[2].change_pct, None,
            "missing prev close gives no change"
        );
        assert_eq!(r.stocks[2].prev_close, None);
        assert_eq!(
            r.as_of_ist.as_deref(),
            Some("11:15:25"),
            "newest row is the time"
        );
    }

    #[tokio::test]
    async fn test_zero_and_nan_prev_close_give_no_change() {
        let dir = list_dir("zero");
        let zero = r#"[2885,"NSE_EQ",1450.5,0.0,1,1450.0,"2026-10-30T11:15:20.000000Z"]"#;
        let url = mock("200 OK", dataset(&[zero])).await;
        let r = run(&url, &dir, true).await;
        assert_eq!(r.stocks[0].change, None);
        assert_eq!(r.stocks[0].prev_close, None);
        // JSON has no NaN: QuestDB writes a NaN double as null.
        let nan = r#"[2885,"NSE_EQ",1450.5,null,1,1450.0,"2026-10-30T11:15:20.000000Z"]"#;
        let url = mock("200 OK", dataset(&[nan])).await;
        let r = run(&url, &dir, true).await;
        assert_eq!(r.stocks[0].change_pct, None);
    }

    #[tokio::test]
    async fn test_same_security_id_in_two_segments_keeps_only_nse_eq() {
        let dir = list_dir("collide");
        let index = r#"[27,"IDX_I",21500.0,21400.0,null,null,"2026-10-30T11:15:29.000000Z"]"#;
        let stock = r#"[27,"NSE_EQ",412.25,400.0,9000,411.0,"2026-10-30T11:15:10.000000Z"]"#;
        let url = mock("200 OK", dataset(&[index, stock])).await;
        let r = run(&url, &dir, true).await;
        assert_eq!(r.stocks.len(), 1);
        assert_eq!(r.stocks[0].segment, "NSE_EQ");
        assert!((r.stocks[0].ltp - 412.25).abs() < 1e-9);
        assert_eq!(
            r.as_of_ist.as_deref(),
            Some("11:15:10"),
            "the index row sets no time"
        );
    }

    #[tokio::test]
    async fn test_old_newest_row_is_stale_not_live() {
        let dir = list_dir("stale");
        let old = r#"[2885,"NSE_EQ",1450.5,1400.0,1,1450.0,"2026-10-30T11:10:00.000000Z"]"#;
        let url = mock("200 OK", dataset(&[old])).await;
        let r = run(&url, &dir, true).await;
        assert_eq!(r.status, "stale");
        assert_eq!(r.data_age_secs, Some(330));
    }

    #[tokio::test]
    async fn test_questdb_timeout_is_unreachable() {
        let dir = list_dir("timeout");
        let url = black_hole().await;
        let r = run(&url, &dir, true).await;
        assert_eq!(r.status, "unreachable");
        assert!(r.stocks.is_empty());
    }

    #[tokio::test]
    async fn test_questdb_error_answer_is_query_failed() {
        let dir = list_dir("refused");
        let url = mock(
            "400 Bad Request",
            r#"{"error":"table does not exist"}"#.to_string(),
        )
        .await;
        assert_eq!(run(&url, &dir, true).await.status, "query_failed");
        let url = mock("200 OK", r#"{"nope":1}"#.to_string()).await;
        assert_eq!(run(&url, &dir, true).await.status, "query_failed");
    }

    // ---- route ----

    fn mock_state(http_port: u16) -> SharedAppState {
        use tickvault_common::config::{DhanConfig, InstrumentConfig, QuestDbConfig};
        SharedAppState::new(
            QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port,
                pg_port: 1,
                ilp_port: 1,
            },
            DhanConfig {
                websocket_url: "wss://test".to_string(),
                order_update_websocket_url: "wss://test".to_string(),
                rest_api_base_url: "https://test".to_string(),
                auth_base_url: "https://test".to_string(),
                instrument_csv_url: "https://test".to_string(),
                instrument_csv_fallback_url: "https://test".to_string(),
                max_instruments_per_connection: 5000,
                max_websocket_connections: 5,
                sandbox_base_url: String::new(),
            },
            InstrumentConfig {
                daily_download_time: "08:55:00".to_string(),
                csv_cache_directory: "/tmp/tv-cache".to_string(),
                csv_cache_filename: "instruments.csv".to_string(),
                csv_download_timeout_secs: 120,
                build_window_start: "08:25:00".to_string(),
                build_window_end: "08:55:00".to_string(),
            },
            Arc::new(crate::state::SystemHealthStatus::new()),
        )
    }

    #[tokio::test]
    async fn test_get_fno_stocks_cache_hit_skips_the_query() {
        let state = mock_state(1);
        state
            .fno_stocks_cache()
            .put(r#"{"status":"live","sentinel":true}"#.to_string());
        let resp = get_fno_stocks(State(state)).await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .expect("body");
        assert!(String::from_utf8_lossy(&body).contains("sentinel"));
    }

    #[tokio::test]
    async fn test_fno_stocks_route_requires_the_bearer_token() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use secrecy::SecretString;
        use tower::ServiceExt;

        let auth = crate::middleware::ApiAuthConfig::from_token(SecretString::from(
            "secret-tok".to_string(),
        ));
        let router = crate::build_router_with_auth(mock_state(1), &[], auth.clone(), false);
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/api/fno-stocks")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("response");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let router = crate::build_router_with_auth(mock_state(1), &[], auth, false);
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/api/fno-stocks")
                    .header("Authorization", "Bearer secret-tok")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("response");
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "a valid token reaches the handler"
        );
    }

    #[test]
    fn test_handler_uses_the_shared_client_and_a_timeout() {
        let source = include_str!("fno_stocks.rs");
        let production = source
            .split_once("#[cfg(test)]\nmod tests")
            .map_or(source, |(before, _)| before);
        assert!(production.contains("state.questdb_http_client()"));
        assert!(production.contains(".timeout(timeout)"));
        assert!(!production.contains(concat!("Client::", "builder")));
        assert!(!production.contains(concat!("Client::", "new")));
    }
}

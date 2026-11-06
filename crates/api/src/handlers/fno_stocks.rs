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
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tickvault_common::constants::{
    EXCHANGE_SEGMENT_NSE_EQ, IST_UTC_OFFSET_NANOS, MARKET_CLOSE_IST_NANOS, MARKET_OPEN_IST_NANOS,
};
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

/// Upper bound on a reported window (one day), the wire's own limit.
pub(super) const FNO_STOCKS_MAX_WINDOW_SECS: i64 = 86_400;

/// The newest row may be at most this old for the answer to read `live`.
pub const FNO_STOCKS_LIVE_MAX_AGE_SECS: i64 = 60;

/// How far an exchange trade time may run ahead of this box's clock and still
/// count. The query keeps only each stock's newest row, so a tighter bound
/// would drop a stock whose newest trade is stamped a second ahead instead of
/// falling back to an older row. Same value as the spot price store's
/// `FUTURE_TRADE_TIME_SKEW_SECS`; a row further ahead is a wrong clock and is
/// still refused.
pub(super) const FNO_STOCKS_FUTURE_SKEW_SECS: i64 = 5;

/// Upper bound on rows the query may return. The F&O list is about 220
/// stocks; the bound only stops a malformed list from asking for more.
pub(super) const FNO_STOCKS_MAX_ROWS: usize = 1_000;
const FNO_STOCKS_QUERY_ROWS: usize = FNO_STOCKS_MAX_ROWS + 1;
const FNO_ARTIFACT_MAX_BYTES: usize = 256 * 1024;

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
    /// UTC epoch seconds of the original observation, before the query.
    /// Retained in cached bodies so callers can age `data_age_secs` correctly.
    pub observed_at_epoch_secs: i64,
    /// Time of the newest row, `HH:MM:SS` IST; `None` without rows.
    pub as_of_ist: Option<String>,
    /// Seconds between the newest row and the original observation; `None` without rows.
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
    pub(super) fn empty(
        status: &'static str,
        market_open: bool,
        fno_stocks_total: usize,
        observed_at_epoch_secs: i64,
    ) -> Self {
        Self {
            status,
            market_open,
            observed_at_epoch_secs,
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
    pub(super) names: HashMap<(u64, u8), String>,
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
    pub(super) fn sorted_ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self.names.keys().map(|(id, _)| *id).collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }
}

#[derive(Deserialize)]
struct ArtifactEnvelope {
    count: usize,
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
    if bytes.len() > FNO_ARTIFACT_MAX_BYTES {
        return None;
    }
    let envelope: ArtifactEnvelope = serde_json::from_slice(bytes).ok()?;
    if envelope.count != envelope.mappings.len() {
        return None;
    }
    let mut names = HashMap::with_capacity(envelope.mappings.len());
    for entry in envelope.mappings {
        if entry.index_name == FNO_UNDERLYING_TAG
            && entry.exchange_segment == EXCHANGE_SEGMENT_NSE_EQ
            && entry.security_id != 0
        {
            match names.entry((entry.security_id, entry.exchange_segment)) {
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(entry.symbol);
                }
                std::collections::hash_map::Entry::Occupied(slot)
                    if slot.get() != &entry.symbol =>
                {
                    return None;
                }
                std::collections::hash_map::Entry::Occupied(_) => (),
            }
        }
    }
    Some(FnoUnderlyings { names })
}

/// The last list read, keyed by its path (the path carries the IST date).
type FnoListCache = Option<(PathBuf, u64, SystemTime, Arc<FnoUnderlyings>)>;
static FNO_LIST_CACHE: Mutex<FnoListCache> = Mutex::new(None);

/// Loads the day's list, re-reading after path, length or modification time
/// changes. Missing or invalid replacement files never reuse a cached list. `None` when the file is missing,
/// unreadable, malformed or empty.
pub(super) fn load_fno_underlyings(path: &Path) -> Option<Arc<FnoUnderlyings>> {
    let mut file = std::fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > FNO_ARTIFACT_MAX_BYTES as u64 {
        return None;
    }
    let modified = metadata.modified().ok()?;
    let mut slot = FNO_LIST_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((cached_path, len, mtime, list)) = slot.as_ref()
        && cached_path == path
        && *len == metadata.len()
        && *mtime == modified
    {
        return Some(Arc::clone(list));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    (&mut file)
        .take((FNO_ARTIFACT_MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    let list = parse_fno_underlyings(&bytes)?;
    if list.is_empty() {
        return None;
    }
    let list = Arc::new(list);
    *slot = Some((
        path.to_path_buf(),
        metadata.len(),
        modified,
        Arc::clone(&list),
    ));
    Some(list)
}

/// IST wall-clock now, as a naive date-time (the same convention `ticks.ts`
/// is stored in).
pub(super) fn now_ist_naive() -> chrono::NaiveDateTime {
    (chrono::Utc::now() + chrono::Duration::nanoseconds(IST_UTC_OFFSET_NANOS)).naive_utc()
}

/// Timeout for the after-close read, which covers the whole session that just
/// ended instead of the last 15 minutes. Measured 2026-11-03 on the live box:
/// 743 NSE_EQ stocks over 09:15–15:40 answered inside the portal's 4 s round
/// trip. Stays under the helper's 12 s capture bound.
pub(super) const FNO_STOCKS_CLOSED_QUERY_TIMEOUT_SECS: u64 = 8;

/// The session whose final stored prices the movers show while the market is
/// closed: `[date 09:15, min(now, date 15:40)]` IST. Owner, 2026-11-03: the
/// page must always show the data the server holds, never a blank table after
/// the close. `None` before the date's 09:15 (nothing traded yet that day).
/// A weekend or holiday yields a window with no rows, which reads `closed`.
#[must_use]
pub(super) fn closed_session_window(
    now_ist: chrono::NaiveDateTime,
) -> Option<(chrono::NaiveDateTime, chrono::NaiveDateTime)> {
    let midnight = now_ist.date().and_time(chrono::NaiveTime::MIN);
    let open = midnight + chrono::Duration::nanoseconds(MARKET_OPEN_IST_NANOS);
    let close = midnight + chrono::Duration::nanoseconds(MARKET_CLOSE_IST_NANOS);
    (now_ist > open).then_some((open, now_ist.min(close)))
}

/// Start of the query window: `now − window`, never before IST midnight.
#[must_use]
pub(super) fn window_start(now_ist: chrono::NaiveDateTime) -> chrono::NaiveDateTime {
    let midnight = now_ist.date().and_time(chrono::NaiveTime::MIN);
    let start = now_ist - chrono::Duration::seconds(FNO_STOCKS_WINDOW_SECS);
    start.max(midnight)
}

/// The movers SQL. Every interpolated value is a number or a constant; no
/// caller text reaches it.
#[must_use]
pub(super) fn build_fno_stocks_sql(
    ids: &[u64],
    window_start_ist: chrono::NaiveDateTime,
    window_end_ist: Option<chrono::NaiveDateTime>,
    row_limit: usize,
) -> String {
    let id_list = ids.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
    let end = window_end_ist.map_or_else(String::new, |end| {
        format!(" AND ts <= '{}'", end.format("%Y-%m-%dT%H:%M:%S%.6fZ"))
    });
    format!(
        "SELECT security_id, segment, ltp, close, volume, avg_price, ts FROM ticks \
         WHERE segment = '{}' AND feed = '{FNO_STOCKS_FEED}' AND ts >= '{}'{end} \
         AND security_id IN ({id_list}) \
         LATEST ON ts PARTITION BY security_id, segment LIMIT {row_limit}",
        segment_code_to_str(EXCHANGE_SEGMENT_NSE_EQ),
        window_start_ist.format("%Y-%m-%dT%H:%M:%S%.6fZ"),
    )
}

/// One parsed `ticks` row.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct TickRow {
    pub(super) security_id: u64,
    pub(super) segment_code: u8,
    pub(super) ltp: f64,
    pub(super) close: Option<f64>,
    pub(super) volume: Option<u64>,
    pub(super) avg_price: Option<f64>,
    pub(super) ts: chrono::NaiveDateTime,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QueryFailure {
    /// No answer (refused, timed out).
    Unreachable,
    /// An answer that was an error or not a dataset.
    Refused,
}

/// Runs the movers query. Malformed rows are skipped (counted by the caller
/// as unpriced), a malformed body is a failure.
pub(super) async fn query_fno_rows(
    client: &reqwest::Client,
    base_url: &str,
    sql: &str,
    timeout: Duration,
    max_rows: usize,
    body_limit: usize,
) -> Result<Vec<TickRow>, QueryFailure> {
    let mut resp = client
        .get(format!("{base_url}/exec"))
        .timeout(timeout)
        .query(&[("query", sql)])
        .send()
        .await
        .map_err(|_| QueryFailure::Unreachable)?;
    if !resp.status().is_success() {
        return Err(QueryFailure::Refused);
    }
    if resp.content_length().is_some_and(|n| n > body_limit as u64) {
        return Err(QueryFailure::Refused);
    }
    let mut bytes = Vec::with_capacity(body_limit);
    while let Some(chunk) = resp.chunk().await.map_err(|_| QueryFailure::Refused)? {
        if chunk.len() > body_limit.saturating_sub(bytes.len()) {
            return Err(QueryFailure::Refused);
        }
        bytes.extend_from_slice(&chunk);
    }
    let body: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| QueryFailure::Refused)?;
    let dataset = body
        .get("dataset")
        .and_then(serde_json::Value::as_array)
        .ok_or(QueryFailure::Refused)?;
    if dataset.len() > max_rows {
        return Err(QueryFailure::Refused);
    }
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
pub(super) fn build_rows(list: &FnoUnderlyings, rows: Vec<TickRow>) -> Vec<FnoStockRow> {
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
pub(super) struct FnoStocksInputs<'a> {
    pub(super) client: &'a reqwest::Client,
    pub(super) base_url: &'a str,
    pub(super) artifact_dir: &'a Path,
    pub(super) market_open: bool,
    pub(super) now_ist: chrono::NaiveDateTime,
    /// A chosen past IST minute to read instead of now (owner 2026-11-03:
    /// pick any stored date and time). `None` reads now. When set, the
    /// answer is a stored-session read: `closed`, `market_open` false, the
    /// window `[that day 09:15, min(at, that day 15:40)]`.
    pub(super) read_at_ist: Option<chrono::NaiveDateTime>,
    pub(super) timeout: Duration,
}

/// Computes the answer. Never panics; every failure is a status.
async fn compute_fno_stocks(inputs: FnoStocksInputs<'_>) -> FnoStocksResponse {
    let date = inputs.now_ist.format("%Y-%m-%d").to_string();
    let list = load_fno_underlyings(&fno_underlying_artifact_path(inputs.artifact_dir, &date));
    compute_for_list(inputs, list).await
}

/// Shared price engine: callers supply a canonical eligible-name set.
/// Market hours, query, prices, turnover and sorting retain one implementation.
pub(super) async fn compute_for_list(
    inputs: FnoStocksInputs<'_>,
    list: Option<Arc<FnoUnderlyings>>,
) -> FnoStocksResponse {
    let observed_at_epoch_secs = inputs.now_ist.and_utc().timestamp()
        - chrono::Duration::nanoseconds(IST_UTC_OFFSET_NANOS).num_seconds();
    let total = list.as_ref().map_or(0, |l| l.len());
    // A chosen minute is always a stored-session read, never live.
    let read_at = inputs.read_at_ist.map(|at| at.min(inputs.now_ist));
    let open = inputs.market_open && read_at.is_none();
    let at = read_at.unwrap_or(inputs.now_ist);
    // Closed: read the session that just ended (owner 2026-11-03). Before the
    // day's 09:15 there is nothing to show, so the answer stays empty `closed`.
    let closed_window = if open {
        None
    } else {
        match closed_session_window(at) {
            Some(window) => Some(window),
            None => {
                return FnoStocksResponse::empty("closed", false, total, observed_at_epoch_secs);
            }
        }
    };
    let Some(list) = list else {
        let status = if open { "no_fno_list" } else { "closed" };
        return FnoStocksResponse::empty(status, open, 0, observed_at_epoch_secs);
    };
    let (start, end) = closed_window.unwrap_or((
        window_start(inputs.now_ist),
        inputs.now_ist + chrono::Duration::seconds(FNO_STOCKS_FUTURE_SKEW_SECS),
    ));
    let sql = build_fno_stocks_sql(
        &list.sorted_ids(),
        start,
        closed_window.map(|w| w.1),
        FNO_STOCKS_QUERY_ROWS,
    );
    let timeout = if open {
        inputs.timeout
    } else {
        inputs
            .timeout
            .max(Duration::from_secs(FNO_STOCKS_CLOSED_QUERY_TIMEOUT_SECS))
    };
    let rows = match query_fno_rows(
        inputs.client,
        inputs.base_url,
        &sql,
        timeout,
        FNO_STOCKS_MAX_ROWS,
        tickvault_common::operator_movers_wire::MAX_SOURCE_BYTES,
    )
    .await
    {
        Ok(rows) => rows,
        Err(QueryFailure::Unreachable) => {
            return FnoStocksResponse::empty("unreachable", open, total, observed_at_epoch_secs);
        }
        Err(QueryFailure::Refused) => {
            return FnoStocksResponse::empty("query_failed", open, total, observed_at_epoch_secs);
        }
    };
    // A malformed price or out-of-window timestamp cannot make older accepted
    // prices look live. Use the same accepted rows for both age and rendering.
    let rows: Vec<_> = rows
        .into_iter()
        .filter(|r| r.ltp.is_finite() && r.ltp > 0.0 && r.ts >= start && r.ts <= end)
        .collect();
    let newest = rows
        .iter()
        .filter(|r| list.name_of(r.security_id, r.segment_code).is_some())
        .map(|r| r.ts)
        .max();
    let stocks = build_rows(&list, rows);
    let Some(newest) = newest.filter(|_| !stocks.is_empty()) else {
        let status = if open { "no_data" } else { "closed" };
        return FnoStocksResponse::empty(status, open, total, observed_at_epoch_secs);
    };
    // A newest row inside the forward skew reads as age 0, never negative.
    let age = (at - newest).num_seconds().max(0);
    let window_secs = closed_window.map_or(FNO_STOCKS_WINDOW_SECS, |(s, e)| {
        (e - s).num_seconds().clamp(1, FNO_STOCKS_MAX_WINDOW_SECS)
    });
    FnoStocksResponse {
        status: if !open {
            "closed"
        } else if age <= FNO_STOCKS_LIVE_MAX_AGE_SECS {
            "live"
        } else {
            "stale"
        },
        market_open: open,
        observed_at_epoch_secs,
        as_of_ist: Some(newest.format("%H:%M:%S").to_string()),
        data_age_secs: Some(age),
        fno_stocks_total: total,
        stocks_priced: stocks.len(),
        window_secs,
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
        read_at_ist: None,
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
pub(super) mod tests {
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
            read_at_ist: None,
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
        let sql = build_fno_stocks_sql(
            &[1594, 2885],
            window_start(at(NOW)),
            None,
            FNO_STOCKS_QUERY_ROWS,
        );
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
            sql.ends_with(&format!("LIMIT {FNO_STOCKS_QUERY_ROWS}")),
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

    #[test]
    fn test_fno_underlying_artifact_path_is_dir_prefix_date_json() {
        assert_eq!(
            fno_underlying_artifact_path(Path::new(FNO_UNDERLYING_ARTIFACT_DIR), "2026-10-30"),
            PathBuf::from("data/instrument-cache/dhan-fno-underlyings-2026-10-30.json")
        );
    }

    #[test]
    fn test_name_of_needs_the_full_composite_key() {
        let list = parse_fno_underlyings(
            br#"{"count":1,"mappings":[{"index_name":"FNO_UNDERLYING","symbol":"ABC","isin":"","security_id":5,"exchange_segment":1}]}"#,
        )
        .expect("parses");
        assert_eq!(list.name_of(5, EXCHANGE_SEGMENT_NSE_EQ), Some("ABC"));
        assert_eq!(list.name_of(5, 2), None, "same id, other segment");
        assert_eq!(list.name_of(6, EXCHANGE_SEGMENT_NSE_EQ), None);
        assert!(!list.is_empty());
    }

    // ---- the handler computation ----

    #[tokio::test]
    async fn test_closed_market_shows_the_ended_sessions_stored_prices() {
        // Owner 2026-11-03: after the close the page shows the stored prices.
        let dir = list_dir("closed");
        let url = mock("200 OK", dataset(&[RELIANCE, INFY])).await;
        let r = run(&url, &dir, false).await;
        assert_eq!(r.status, "closed");
        assert!(!r.market_open);
        assert_eq!(r.stocks.len(), 2);
        assert_eq!(r.stocks_priced, 2);
        assert_eq!(r.fno_stocks_total, 3);
        assert_eq!(r.as_of_ist.as_deref(), Some("11:15:25"));
        // 09:15 to NOW (11:15:30): the whole session so far, not 15 minutes.
        assert_eq!(r.window_secs, 2 * 3600 + 30);
    }

    #[tokio::test]
    async fn test_closed_market_with_no_rows_reads_closed_not_no_data() {
        let dir = list_dir("closed-empty");
        let url = mock("200 OK", dataset(&[])).await;
        let r = run(&url, &dir, false).await;
        assert_eq!(r.status, "closed");
        assert!(r.stocks.is_empty());
        assert!(!r.market_open);
    }

    #[tokio::test]
    async fn test_closed_market_failure_stays_a_failure_and_never_reads_open() {
        let dir = list_dir("closed-fail");
        let r = run("http://127.0.0.1:1", &dir, false).await;
        assert_eq!(r.status, "unreachable");
        assert!(!r.market_open);
        assert!(r.stocks.is_empty());
    }

    #[test]
    fn test_closed_session_window_bounds() {
        assert_eq!(closed_session_window(at("2026-10-30 09:15:00")), None);
        assert_eq!(closed_session_window(at("2026-10-30 08:00:00")), None);
        assert_eq!(
            closed_session_window(at("2026-10-30 11:15:30")),
            Some((at("2026-10-30 09:15:00"), at("2026-10-30 11:15:30")))
        );
        assert_eq!(
            closed_session_window(at("2026-10-30 18:40:00")),
            Some((at("2026-10-30 09:15:00"), at("2026-10-30 15:40:00")))
        );
    }

    #[test]
    fn test_closed_sql_carries_the_upper_bound_and_live_sql_does_not() {
        let end = at("2026-10-30 15:40:00");
        let closed = build_fno_stocks_sql(
            &[1594],
            at("2026-10-30 09:15:00"),
            Some(end),
            FNO_STOCKS_QUERY_ROWS,
        );
        assert!(
            closed.contains(
                "ts >= '2026-10-30T09:15:00.000000Z' AND ts <= '2026-10-30T15:40:00.000000Z'"
            ),
            "{closed}"
        );
        let live = build_fno_stocks_sql(
            &[1594],
            at("2026-10-30 09:15:00"),
            None,
            FNO_STOCKS_QUERY_ROWS,
        );
        assert!(!live.contains("ts <="), "{live}");
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
        assert_eq!(
            r.observed_at_epoch_secs,
            chrono::DateTime::parse_from_rfc3339("2026-10-30T05:45:30Z")
                .unwrap()
                .timestamp(),
            "age must retain its original pre-query UTC clock, with the IST offset removed"
        );
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
    async fn test_regression_newest_row_slightly_ahead_of_the_clock_is_kept() {
        // NOW is 11:15:30. The newest trade is stamped 3 s ahead: the stock
        // stays on the page with age 0. One 10 s ahead is a wrong clock and is
        // refused, so only the other stock remains.
        let dir = list_dir("ahead");
        let ahead = r#"[2885,"NSE_EQ",1450.5,1400.0,1,1450.0,"2026-10-30T11:15:33.000000Z"]"#;
        let url = mock("200 OK", dataset(&[ahead])).await;
        let r = run(&url, &dir, true).await;
        assert_eq!(r.status, "live");
        assert_eq!(r.stocks_priced, 1);
        assert_eq!(r.data_age_secs, Some(0));
        assert_eq!(r.as_of_ist.as_deref(), Some("11:15:33"));

        let far = r#"[2885,"NSE_EQ",1450.5,1400.0,1,1450.0,"2026-10-30T11:15:40.000000Z"]"#;
        let url = mock("200 OK", dataset(&[far, INFY])).await;
        let r = run(&url, &dir, true).await;
        let names: Vec<&str> = r.stocks.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["INFY"]);
        assert_eq!(r.data_age_secs, Some(5));
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

    pub(in crate::handlers) fn mock_state(http_port: u16) -> SharedAppState {
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
        state.fno_stocks_cache().put(
            r#"{"status":"live","observed_at_epoch_secs":1800000000,"sentinel":true}"#.to_string(),
        );
        let resp = get_fno_stocks(State(state)).await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .expect("body");
        assert!(String::from_utf8_lossy(&body).contains("sentinel"));
        let cached: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            cached["observed_at_epoch_secs"], 1_800_000_000_i64,
            "a cache hit must not refresh the original observation clock"
        );
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
        let (production, _) = source
            .split_once("\n#[cfg(test)]\npub(super) mod tests {")
            .expect("the shared F&O test-module boundary must remain explicit");
        assert!(production.contains("state.questdb_http_client()"));
        assert!(production.contains(".timeout(timeout)"));
        assert!(!production.contains(concat!("Client::", "builder")));
        assert!(!production.contains(concat!("Client::", "new")));
    }
    #[test]
    fn fno_artifact_rejects_missing_or_mismatched_declared_count() {
        for bytes in [
            br#"{"mappings":[]}"#.as_slice(),
            br#"{"count":1,"mappings":[]}"#.as_slice(),
        ] {
            assert!(parse_fno_underlyings(bytes).is_none());
        }
        assert!(parse_fno_underlyings(&vec![b' '; FNO_ARTIFACT_MAX_BYTES + 1]).is_none());
    }
    #[test]
    fn fno_duplicate_identity_cannot_choose_a_conflicting_symbol() {
        let body = |name: &str| {
            serde_json::json!({"count":2,"mappings":[{"index_name":"FNO_UNDERLYING","symbol":"ABC","security_id":5,"exchange_segment":1},{"index_name":"FNO_UNDERLYING","symbol":name,"security_id":5,"exchange_segment":1}]}).to_string()
        };
        assert_eq!(
            parse_fno_underlyings(body("ABC").as_bytes())
                .unwrap()
                .name_of(5, 1),
            Some("ABC")
        );
        assert!(parse_fno_underlyings(body("WRONG").as_bytes()).is_none());
    }
}

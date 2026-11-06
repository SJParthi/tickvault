//! Canonical index/F&O enrichment and full-universe filtering before pagination.
//! Reads existing daily artifacts and captured ticks only. No subscription,
//! broker request, strategy activation or database write.
//!
//! Plan PR MV (2026-11-06): the route builds ONE movers universe (one QuestDB
//! query over the union of today's F&O list and index-membership list, plus
//! both lists) and answers every filter from it with the shared pure
//! [`select`]; the universe is cached for 2 s, so a sort, page or filter
//! change inside that window reads no file and runs no query. The app
//! publishes the same universe for the operator Lambda
//! ([`build_live_universe`]). The per-filter path this replaced is kept as the
//! test oracle (`oracle` module) and a property test proves the two equal.
//!
//! Complexity: cold path. Per build O(list rows) to read the lists and one
//! bounded query; per request O(n log n) in [`select`], n ≤ 2,000 rows.
use super::fno_stocks::{self, FnoStocksInputs, FnoUnderlyings, QueryFailure, TickRow};
use crate::response_cache::cached_json_response;
use crate::state::SharedAppState;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tickvault_common::constants::{
    EXCHANGE_SEGMENT_NSE_EQ, INDEX_CONSTITUENCY_SLUGS, IST_UTC_OFFSET_SECONDS_I64,
};
use tickvault_common::operator_movers_universe::{
    ListedStock, MAX_UNIVERSE_PRICE_ROWS, MoversUniverse, PriceRow, StockIndices,
    UNIVERSE_DEFAULT_WINDOW_SECS, UNIVERSE_FORMAT_VERSION, UniverseMembership, UniversePrices,
    select, snapshot_deadline,
};
use tickvault_common::operator_movers_wire::{BridgeError, MAX_SOURCE_BYTES, MAX_SOURCE_ROWS};
use tickvault_common::operator_stock_movers_wire::{
    Filter, MAX_INDEX_NAME_BYTES, MAX_INDEX_NAMES, Reply,
};

/// Existing writer path, pinned against app::dhan_universe by an integration test.
pub const INDEX_MAPPING_PREFIX: &str = "dhan-nse-mapping-";
const MAX_MAPPING_BYTES: usize = 8 * 1024 * 1024;
const MAX_MAPPING_ROWS: usize = 32_768;
const CACHE_TTL_SECONDS: u64 = 2;
const QUERY_TIMEOUT_SECONDS: u64 = 3;
const CACHE_TTL: Duration = Duration::from_secs(CACHE_TTL_SECONDS);
const QUERY_TIMEOUT: Duration = Duration::from_secs(QUERY_TIMEOUT_SECONDS);
/// Body bound of the union query: two lists' worth of rows.
const UNIVERSE_SOURCE_BYTES: usize = 2 * MAX_SOURCE_BYTES;

/// One cached universe: built at, the `as_of` it was built for (`None` =
/// live), the universe, its reply deadline and the session state it saw.
type CachedUniverse = (Instant, Option<String>, Arc<MoversUniverse>, i64, bool);

#[derive(Default)]
pub(crate) struct MoversCache {
    slot: tokio::sync::Mutex<Option<CachedUniverse>>,
}
#[derive(Deserialize)]
struct MappingArtifact {
    trading_date_ist: String,
    resolved: usize,
    unresolved: usize,
    mappings: Vec<MappingEntry>,
}
#[derive(Deserialize)]
struct MappingEntry {
    index_name: String,
    symbol: String,
    security_id: u64,
    exchange_segment: u8,
}
#[derive(Debug)]
struct Memberships {
    names: HashMap<(u64, u8), String>,
    indices: Vec<String>,
    by_stock: HashMap<(u64, u8), Vec<usize>>,
    partial: bool,
}
fn parse_memberships(bytes: &[u8], date: &str) -> Option<Memberships> {
    if bytes.len() > MAX_MAPPING_BYTES {
        return None;
    }
    let artifact: MappingArtifact = serde_json::from_slice(bytes).ok()?;
    if artifact.trading_date_ist != date || artifact.mappings.len() > MAX_MAPPING_ROWS {
        return None;
    }
    let equity_rows: Vec<_> = artifact
        .mappings
        .into_iter()
        .filter(|r| r.exchange_segment == EXCHANGE_SEGMENT_NSE_EQ)
        .collect();
    if equity_rows.len() != artifact.resolved || equity_rows.is_empty() {
        return None;
    }
    let mut names = HashMap::with_capacity(MAX_SOURCE_ROWS);
    let mut labels = BTreeSet::new();
    let mut pairs = HashSet::with_capacity(equity_rows.len());
    for row in &equity_rows {
        if row.security_id == 0
            || row.symbol.is_empty()
            || row.symbol.len() > 128
            || row.symbol.chars().any(char::is_control)
            || row.index_name.is_empty()
            || row.index_name.len() > MAX_INDEX_NAME_BYTES
            || row.index_name.chars().any(char::is_control)
            || !pairs.insert((row.security_id, row.index_name.as_str()))
        {
            return None;
        }
        let key = (row.security_id, row.exchange_segment);
        if names
            .insert(key, row.symbol.clone())
            .is_some_and(|held| held != row.symbol)
        {
            return None;
        }
        labels.insert(row.index_name.clone());
    }
    if names.len() > MAX_SOURCE_ROWS || labels.len() > MAX_INDEX_NAMES {
        return None;
    }
    let indices: Vec<String> = labels.into_iter().collect();
    let mut by_stock: HashMap<_, Vec<usize>> = HashMap::with_capacity(names.len());
    for row in equity_rows {
        let index = indices.binary_search(&row.index_name).ok()?;
        by_stock
            .entry((row.security_id, row.exchange_segment))
            .or_default()
            .push(index);
    }
    for row in by_stock.values_mut() {
        row.sort_unstable();
    }
    let partial = artifact.unresolved > 0
        || INDEX_CONSTITUENCY_SLUGS
            .iter()
            .any(|(name, _)| indices.binary_search_by(|s| s.as_str().cmp(name)).is_err());
    Some(Memberships {
        names,
        indices,
        by_stock,
        partial,
    })
}

/// The two most recent membership files by path and observed metadata
/// (today's and one chosen other date), never a stale version of either.
static MEMBERSHIP_CACHE: fno_stocks::FileCache<Memberships> = fno_stocks::FileCache::new();
fn load_memberships(dir: &Path, date: &str) -> Option<Arc<Memberships>> {
    let path = dir.join(format!("{INDEX_MAPPING_PREFIX}{date}.json"));
    let mut file = std::fs::File::open(&path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_MAPPING_BYTES as u64 {
        return None;
    }
    let modified = metadata.modified().ok()?;
    if let Some(data) = MEMBERSHIP_CACHE.get(&path, metadata.len(), modified) {
        return Some(data);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    (&mut file)
        .take((MAX_MAPPING_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    let data = Arc::new(parse_memberships(&bytes, date)?);
    MEMBERSHIP_CACHE.put(&path, metadata.len(), modified, Arc::clone(&data));
    Some(data)
}
fn cache_current(deadline: i64, captured_open: bool, now: i64, current_open: bool) -> bool {
    now < deadline && captured_open == current_open
}

/// When and what one universe build reads (the clock half of the old
/// per-filter `compute` + `compute_for_list`).
struct ReadPlan {
    /// UTC epoch seconds of the observation.
    observed: i64,
    /// Session open AND a live read.
    open: bool,
    /// The read instant: now, or the chosen minute (never after now).
    at: chrono::NaiveDateTime,
    /// `(start, end, closed)` of the query window; `None` before the day's
    /// 09:15 on a closed read (nothing is queried).
    window: Option<(chrono::NaiveDateTime, chrono::NaiveDateTime, bool)>,
}

fn plan_read(
    now_ist: chrono::NaiveDateTime,
    market_open: bool,
    read_at_ist: Option<chrono::NaiveDateTime>,
) -> ReadPlan {
    let observed = now_ist.and_utc().timestamp() - IST_UTC_OFFSET_SECONDS_I64;
    let read_at = read_at_ist.map(|at| at.min(now_ist));
    let open = market_open && read_at.is_none();
    let at = read_at.unwrap_or(now_ist);
    let window = if open {
        Some((
            fno_stocks::window_start(now_ist),
            now_ist + chrono::Duration::seconds(fno_stocks::FNO_STOCKS_FUTURE_SKEW_SECS),
            false,
        ))
    } else {
        fno_stocks::closed_session_window(at).map(|(start, end)| (start, end, true))
    };
    ReadPlan {
        observed,
        open,
        at,
        window,
    }
}

/// Security ids of the union of both lists, ascending (all `NSE_EQ`). O(n log n).
fn union_ids(fno: Option<&FnoUnderlyings>, membership: Option<&Memberships>) -> Vec<u64> {
    let mut ids: Vec<u64> = fno
        .map(|f| f.sorted_ids())
        .unwrap_or_default()
        .into_iter()
        .chain(
            membership
                .into_iter()
                .flat_map(|m| m.names.keys().map(|(id, _)| *id)),
        )
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// What the union query gave.
enum Fetched {
    /// Closed and before 09:15: nothing is read.
    BeforeSession,
    /// Neither list exists: no query (no filter can reach the prices).
    NoQuery,
    Rows(Vec<TickRow>),
    Failed(QueryFailure),
}

fn sorted_listed(names: &HashMap<(u64, u8), String>) -> Vec<ListedStock> {
    let mut list: Vec<ListedStock> = names
        .iter()
        .map(|((security_id, segment_code), name)| ListedStock {
            security_id: *security_id,
            segment_code: *segment_code,
            name: name.clone(),
        })
        .collect();
    list.sort_unstable_by_key(|s| (s.security_id, s.segment_code));
    list
}

/// Pure: the universe from a read plan, the two lists and what the query
/// gave. Keeps only rows the per-filter path accepted (finite positive last
/// price, inside the window), in database order. O(rows + list rows log).
fn assemble_universe(
    plan: &ReadPlan,
    fno: Option<&FnoUnderlyings>,
    membership: Option<&Memberships>,
    fetched: Fetched,
) -> MoversUniverse {
    let prices = match (fetched, plan.window) {
        (Fetched::BeforeSession, _) | (Fetched::Rows(_), None) => UniversePrices::BeforeSession,
        (Fetched::NoQuery, _) => UniversePrices::Rows { rows: Vec::new() },
        (Fetched::Failed(QueryFailure::Unreachable), _) => UniversePrices::Unreachable,
        (Fetched::Failed(QueryFailure::Refused), _) => UniversePrices::QueryFailed,
        (Fetched::Rows(rows), Some((start, end, _))) => UniversePrices::Rows {
            rows: rows
                .into_iter()
                .filter(|r| r.ltp.is_finite() && r.ltp > 0.0 && r.ts >= start && r.ts <= end)
                .map(|r| PriceRow {
                    security_id: r.security_id,
                    segment_code: r.segment_code,
                    ltp: r.ltp,
                    prev_close: r.close.filter(|p| p.is_finite() && *p > 0.0),
                    volume: r.volume,
                    ts_ist_secs: r.ts.and_utc().timestamp(),
                })
                .collect(),
        },
    };
    let window_secs = match plan.window {
        Some((start, end, true)) => (end - start)
            .num_seconds()
            .clamp(1, fno_stocks::FNO_STOCKS_MAX_WINDOW_SECS),
        _ => UNIVERSE_DEFAULT_WINDOW_SECS,
    };
    MoversUniverse {
        format_version: UNIVERSE_FORMAT_VERSION,
        trading_date_ist: plan.at.format("%Y-%m-%d").to_string(),
        observed_at_epoch_secs: plan.observed,
        read_at_ist_secs: plan.at.and_utc().timestamp(),
        market_open: plan.open,
        window_secs,
        prices,
        fno: fno.map(|f| sorted_listed(&f.names)),
        membership: membership.map(|m| {
            let mut by_stock: Vec<StockIndices> = m
                .by_stock
                .iter()
                .map(|((security_id, segment_code), indices)| StockIndices {
                    security_id: *security_id,
                    segment_code: *segment_code,
                    indices: indices.clone(),
                })
                .collect();
            by_stock.sort_unstable_by_key(|s| (s.security_id, s.segment_code));
            UniverseMembership {
                names: sorted_listed(&m.names),
                indices: m.indices.clone(),
                by_stock,
                partial: m.partial,
            }
        }),
    }
}

/// Reads the day's lists and runs ONE query over their union. Never panics;
/// every failure is carried as a price state. A union over
/// [`MAX_UNIVERSE_PRICE_ROWS`] ids reads `query_failed` (Limitation: the
/// per-filter path could still serve the smaller list then).
async fn build_universe(inputs: FnoStocksInputs<'_>) -> MoversUniverse {
    let plan = plan_read(inputs.now_ist, inputs.market_open, inputs.read_at_ist);
    let date = plan.at.format("%Y-%m-%d").to_string();
    // File reads and JSON parsing are blocking work: on the blocking pool,
    // never a runtime worker (plan PR MV review). A failed hand-off reads as
    // both lists unavailable, which the reply reports as such.
    let dir = inputs.artifact_dir.to_path_buf();
    let (fno, membership) = tokio::task::spawn_blocking(move || {
        (
            fno_stocks::load_fno_underlyings(&fno_stocks::fno_underlying_artifact_path(
                &dir, &date,
            )),
            load_memberships(&dir, &date),
        )
    })
    .await
    .unwrap_or((None, None));
    let ids = union_ids(fno.as_deref(), membership.as_deref());
    let fetched = match plan.window {
        None => Fetched::BeforeSession,
        Some(_) if ids.is_empty() => Fetched::NoQuery,
        Some(_) if ids.len() > MAX_UNIVERSE_PRICE_ROWS => Fetched::Failed(QueryFailure::Refused),
        Some((start, window_end, closed)) => {
            let end = closed.then_some(window_end);
            let bound = ids.len().max(fno_stocks::FNO_STOCKS_MAX_ROWS);
            let sql = fno_stocks::build_fno_stocks_sql(&ids, start, end, bound + 1);
            let timeout = if closed {
                inputs.timeout.max(Duration::from_secs(
                    fno_stocks::FNO_STOCKS_CLOSED_QUERY_TIMEOUT_SECS,
                ))
            } else {
                inputs.timeout
            };
            match fno_stocks::query_fno_rows(
                inputs.client,
                inputs.base_url,
                &sql,
                timeout,
                bound,
                UNIVERSE_SOURCE_BYTES,
            )
            .await
            {
                Ok(rows) => Fetched::Rows(rows),
                Err(failure) => Fetched::Failed(failure),
            }
        }
    };
    assemble_universe(&plan, fno.as_deref(), membership.as_deref(), fetched)
}

/// One clock reading: the session state and the IST time of the SAME instant
/// (plan PR MV review: reading them separately could straddle 09:00 or
/// 15:40 and build an "open" universe for a closed minute). O(1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiveClock {
    pub market_open: bool,
    pub now_ist: chrono::NaiveDateTime,
    /// UTC epoch seconds of the same instant.
    pub now_epoch: i64,
}

impl LiveClock {
    /// The clock at one UTC instant.
    #[must_use]
    pub fn at(now: chrono::DateTime<chrono::Utc>) -> Self {
        let now_epoch = now.timestamp();
        Self {
            market_open: tickvault_common::market_hours::is_trading_session_at(now_epoch),
            now_ist: (now + chrono::Duration::seconds(IST_UTC_OFFSET_SECONDS_I64)).naive_utc(),
            now_epoch,
        }
    }

    /// The clock now (one `Utc::now()` read).
    #[must_use]
    pub fn now() -> Self {
        Self::at(chrono::Utc::now())
    }
}

/// What makes a stored-session (closed) live read the same read as an
/// earlier one: its IST date and its window end. Two reads with the same key
/// query the same rows, so the publisher may reuse the first one's price rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClosedReadKey {
    pub date_ist: String,
    pub window_end_ist_secs: i64,
}

/// The reuse key of a live read at this clock: `Some` only for a closed read
/// that queries (after the day's 09:15), `None` for an open read (its window
/// moves every second) or a read that queries nothing. O(1).
#[must_use]
pub fn closed_read_key(clock: LiveClock) -> Option<ClosedReadKey> {
    let plan = plan_read(clock.now_ist, clock.market_open, None);
    match plan.window {
        Some((_, end, true)) if !plan.open => Some(ClosedReadKey {
            date_ist: plan.at.format("%Y-%m-%d").to_string(),
            window_end_ist_secs: end.and_utc().timestamp(),
        }),
        _ => None,
    }
}

/// An earlier closed universe re-stamped as observed at this clock: the same
/// price rows and lists, the observation and read instants of `clock`
/// (exactly what a fresh build at `clock` with the same rows would carry).
/// `None` unless `clock` is a closed read with the universe's key. O(n) copy.
#[must_use]
pub fn restamped(
    universe: &MoversUniverse,
    key: &ClosedReadKey,
    clock: LiveClock,
) -> Option<MoversUniverse> {
    if closed_read_key(clock).as_ref() != Some(key)
        || universe.market_open
        || universe.trading_date_ist != key.date_ist
    {
        return None;
    }
    let plan = plan_read(clock.now_ist, clock.market_open, None);
    let mut out = universe.clone();
    out.observed_at_epoch_secs = plan.observed;
    out.read_at_ist_secs = plan.at.and_utc().timestamp();
    Some(out)
}

/// The live universe (today's lists, at `clock`) for the app's publisher,
/// read with this API state's QuestDB client. Cold path: two small file reads
/// (cached by metadata, on the blocking pool) and one bounded query.
// TEST-EXEMPT: thin wrapper over build_universe (driven by every route test below); needs a live QuestDB
pub async fn build_live_universe(state: &SharedAppState, clock: LiveClock) -> MoversUniverse {
    let cfg = state.questdb_config();
    let base_url = format!("http://{}:{}", cfg.host, cfg.http_port);
    build_universe(FnoStocksInputs {
        client: state.questdb_http_client(),
        base_url: &base_url,
        artifact_dir: Path::new(fno_stocks::FNO_UNDERLYING_ARTIFACT_DIR),
        market_open: clock.market_open,
        now_ist: clock.now_ist,
        read_at_ist: None,
        timeout: QUERY_TIMEOUT,
    })
    .await
}

/// Same bearer gate and shared limiter as /api/fno-stocks. One bounded slot
/// holds at most one universe (keyed on `as_of`); every filter is selected
/// from it, so sort, page and filter changes inside 2 s hit the cache.
/// Concurrent builds return typed Busy.
// TEST-EXEMPT: exercised by the async authenticated route and cache tests below.
pub async fn get_stock_movers(
    State(state): State<SharedAppState>,
    Query(filter): Query<Filter>,
) -> Response {
    if !filter.valid() {
        return (axum::http::StatusCode::BAD_REQUEST, "Invalid movers filter").into_response();
    }
    // A chosen minute must be a real calendar minute (the wire checks only
    // its shape).
    let read_at_ist = match filter.as_of.as_deref() {
        None => None,
        Some(text) => match chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M") {
            Ok(at) => Some(at),
            Err(_) => {
                return (axum::http::StatusCode::BAD_REQUEST, "Invalid movers filter")
                    .into_response();
            }
        },
    };
    let clock = LiveClock::now();
    let market_open = clock.market_open;
    let now = clock.now_epoch;
    let Ok(mut slot) = state.stock_movers_cache().slot.try_lock() else {
        return axum::Json(Reply::error(
            BridgeError::Busy,
            chrono::Utc::now().timestamp(),
        ))
        .into_response();
    };
    let cached = slot
        .as_ref()
        .filter(|(stored, as_of, _, deadline, captured_open)| {
            *as_of == filter.as_of
                && stored.elapsed() < CACHE_TTL
                && cache_current(*deadline, *captured_open, now, market_open)
        })
        .map(|(_, _, universe, _, _)| Arc::clone(universe));
    let (universe, cache_label) = if let Some(universe) = cached {
        (universe, "hit")
    } else {
        let cfg = state.questdb_config();
        let base_url = format!("http://{}:{}", cfg.host, cfg.http_port);
        let universe = Arc::new(
            build_universe(FnoStocksInputs {
                client: state.questdb_http_client(),
                base_url: &base_url,
                artifact_dir: Path::new(fno_stocks::FNO_UNDERLYING_ARTIFACT_DIR),
                market_open,
                now_ist: clock.now_ist,
                read_at_ist,
                timeout: QUERY_TIMEOUT,
            })
            .await,
        );
        if let Some(deadline) = snapshot_deadline(universe.observed_at_epoch_secs) {
            *slot = Some((
                Instant::now(),
                filter.as_of.clone(),
                Arc::clone(&universe),
                deadline,
                market_open,
            ));
        }
        (universe, "miss")
    };
    drop(slot);
    let reply = select(&universe, &filter);
    let reply = if market_open != tickvault_common::market_hours::is_trading_session_now()
        || !reply.valid_at(chrono::Utc::now().timestamp(), &filter)
    {
        Reply::error(BridgeError::Busy, chrono::Utc::now().timestamp())
    } else {
        reply
    };
    match serde_json::to_string(&reply) {
        Ok(body) => cached_json_response(body, cache_label),
        Err(_) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "Movers unavailable",
        )
            .into_response(),
    }
}

/// Test-only: one filter's reply through the production path (build the
/// universe, then select). Every route-level test below drives this.
#[cfg(test)]
async fn compute(inputs: FnoStocksInputs<'_>, filter: Filter) -> Reply {
    select(&build_universe(inputs).await, &filter)
}

/// The per-filter path this module ran until 2026-11-06 (plan PR MV), kept
/// verbatim in logic as the oracle `select` is proven equal to. The only
/// change: the price query is an injected closure, so it runs synchronously
/// in a property test.
#[cfg(test)]
mod oracle {
    use super::fno_stocks::FnoStocksResponse;
    use super::*;
    use tickvault_common::operator_movers_universe::ACTUAL_VALUE_UNAVAILABLE;
    use tickvault_common::operator_movers_wire::{
        MAX_WIRE_BYTES, SELECTION_LIMIT, SourceStatus, Stock,
    };
    use tickvault_common::operator_stock_movers_wire::{
        Direction, FILTERED_PROTOCOL_VERSION, MemberStock, MembershipStatus, Outcome, Snapshot,
    };

    pub(super) fn stock(row: fno_stocks::FnoStockRow) -> Stock {
        Stock {
            security_id: row.security_id,
            segment: row.segment.into(),
            name: row.name,
            ltp: row.ltp,
            prev_close: row.prev_close,
            change: row.change,
            change_pct: row.change_pct,
            volume: row.volume.filter(|volume| *volume > 0),
            value_cr: None,
            last_trade_ist: row.last_trade_ist,
        }
    }

    pub(super) fn finish(
        prices: FnoStocksResponse,
        filter: Filter,
        membership: Option<&Memberships>,
        fno: Option<&FnoUnderlyings>,
    ) -> Reply {
        let status =
            serde_json::from_value::<SourceStatus>(serde_json::Value::String(prices.status.into()));
        let Ok(status) = status else {
            return Reply::error(BridgeError::InvalidResponse, prices.observed_at_epoch_secs);
        };
        let index_names = membership.map_or_else(Vec::new, |m| m.indices.clone());
        let membership_status = match membership {
            None => MembershipStatus::Unavailable,
            Some(m) if m.partial => MembershipStatus::Partial,
            Some(_) => MembershipStatus::Available,
        };
        let mut ids = HashSet::with_capacity(prices.stocks.len());
        let mut selected = Vec::with_capacity(prices.stocks.len());
        for price in prices.stocks.into_iter().map(stock) {
            if !price.valid() || !ids.insert(price.security_id) {
                return Reply::error(BridgeError::InvalidResponse, prices.observed_at_epoch_secs);
            }
            if !filter.matches_price(&price) {
                continue;
            }
            let key = (price.security_id, EXCHANGE_SEGMENT_NSE_EQ);
            let indices = membership
                .and_then(|m| m.by_stock.get(&key))
                .cloned()
                .unwrap_or_default();
            let eligible = fno.map(|f| f.name_of(key.0, key.1).is_some());
            let volume_text = price.volume.map(|volume| volume.to_string());
            selected.push(MemberStock {
                price,
                company_name: None,
                volume_text,
                indices,
                fno: eligible,
            });
        }
        if filter.sort_by.is_some() {
            selected.sort_unstable_by(|left, right| filter.compare_rows(left, right));
        } else if filter.direction == Direction::Losers {
            selected.reverse();
        }
        let matching_total = selected.len();
        let rows = selected
            .into_iter()
            .skip(filter.offset)
            .take(SELECTION_LIMIT)
            .collect();
        let snapshot = Snapshot {
            filter: filter.clone(),
            status,
            market_open: prices.market_open,
            as_of_ist: prices.as_of_ist,
            data_age_secs: prices.data_age_secs,
            universe_available: (if filter.fno_only {
                fno.is_some()
            } else {
                membership.is_some()
            }) && filter.index.as_ref().is_none_or(|name| {
                membership.is_some_and(|m| m.indices.binary_search(name).is_ok())
            }),
            universe_total: prices.fno_stocks_total,
            stocks_priced: prices.stocks_priced,
            matching_total,
            selection_limit: SELECTION_LIMIT,
            window_secs: prices.window_secs,
            value_basis: ACTUAL_VALUE_UNAVAILABLE.into(),
            index_names,
            membership_status,
            fno_list_available: fno.is_some(),
            rows,
        };
        let Some(valid_until_epoch_secs) = snapshot_deadline(prices.observed_at_epoch_secs) else {
            return Reply::error(BridgeError::InvalidResponse, prices.observed_at_epoch_secs);
        };
        let reply = Reply {
            valid_until_epoch_secs,
            version: FILTERED_PROTOCOL_VERSION,
            captured_at_epoch_secs: prices.observed_at_epoch_secs,
            outcome: Outcome::Snapshot {
                snapshot: Box::new(snapshot),
            },
        };
        let Outcome::Snapshot { snapshot } = &reply.outcome else {
            return Reply::error(BridgeError::InvalidResponse, prices.observed_at_epoch_secs);
        };
        if !snapshot.valid() {
            return Reply::error(BridgeError::InvalidResponse, prices.observed_at_epoch_secs);
        }
        if serde_json::to_vec(&reply).map_or(true, |bytes| bytes.len() > MAX_WIRE_BYTES) {
            return Reply::error(BridgeError::ResponseTooLarge, prices.observed_at_epoch_secs);
        }
        reply
    }

    /// The old `fno_stocks::compute_for_list`, with the query injected.
    fn compute_for_list(
        now_ist: chrono::NaiveDateTime,
        market_open: bool,
        read_at_ist: Option<chrono::NaiveDateTime>,
        list: &FnoUnderlyings,
        query: &dyn Fn(&[u64]) -> Result<Vec<TickRow>, QueryFailure>,
    ) -> FnoStocksResponse {
        let observed_at_epoch_secs = now_ist.and_utc().timestamp() - IST_UTC_OFFSET_SECONDS_I64;
        let total = list.len();
        let read_at = read_at_ist.map(|at| at.min(now_ist));
        let open = market_open && read_at.is_none();
        let at = read_at.unwrap_or(now_ist);
        let closed_window = if open {
            None
        } else {
            match fno_stocks::closed_session_window(at) {
                Some(window) => Some(window),
                None => {
                    return FnoStocksResponse::empty(
                        "closed",
                        false,
                        total,
                        observed_at_epoch_secs,
                    );
                }
            }
        };
        let (start, end) = closed_window.unwrap_or((
            fno_stocks::window_start(now_ist),
            now_ist + chrono::Duration::seconds(fno_stocks::FNO_STOCKS_FUTURE_SKEW_SECS),
        ));
        let rows = match query(&list.sorted_ids()) {
            Ok(rows) => rows,
            Err(QueryFailure::Unreachable) => {
                return FnoStocksResponse::empty(
                    "unreachable",
                    open,
                    total,
                    observed_at_epoch_secs,
                );
            }
            Err(QueryFailure::Refused) => {
                return FnoStocksResponse::empty(
                    "query_failed",
                    open,
                    total,
                    observed_at_epoch_secs,
                );
            }
        };
        let rows: Vec<_> = rows
            .into_iter()
            .filter(|r| r.ltp.is_finite() && r.ltp > 0.0 && r.ts >= start && r.ts <= end)
            .collect();
        let newest = rows
            .iter()
            .filter(|r| list.name_of(r.security_id, r.segment_code).is_some())
            .map(|r| r.ts)
            .max();
        let stocks = fno_stocks::build_rows(list, rows);
        let Some(newest) = newest.filter(|_| !stocks.is_empty()) else {
            let status = if open { "no_data" } else { "closed" };
            return FnoStocksResponse::empty(status, open, total, observed_at_epoch_secs);
        };
        let age = (at - newest).num_seconds().max(0);
        let window_secs = closed_window.map_or(fno_stocks::FNO_STOCKS_WINDOW_SECS, |(s, e)| {
            (e - s)
                .num_seconds()
                .clamp(1, fno_stocks::FNO_STOCKS_MAX_WINDOW_SECS)
        });
        let mut response = FnoStocksResponse::empty("closed", open, total, observed_at_epoch_secs);
        response.status = if !open {
            "closed"
        } else if age <= fno_stocks::FNO_STOCKS_LIVE_MAX_AGE_SECS {
            "live"
        } else {
            "stale"
        };
        response.as_of_ist = Some(newest.format("%H:%M:%S").to_string());
        response.data_age_secs = Some(age);
        response.stocks_priced = stocks.len();
        response.window_secs = window_secs;
        response.stocks = stocks;
        response
    }

    /// The old per-filter `compute`, with the lists and the query injected.
    pub(super) fn reply(
        now_ist: chrono::NaiveDateTime,
        market_open: bool,
        read_at_ist: Option<chrono::NaiveDateTime>,
        fno: Option<&FnoUnderlyings>,
        membership: Option<&Memberships>,
        filter: Filter,
        query: &dyn Fn(&[u64]) -> Result<Vec<TickRow>, QueryFailure>,
    ) -> Reply {
        let now = now_ist.and_utc().timestamp() - IST_UTC_OFFSET_SECONDS_I64;
        let open = market_open && read_at_ist.is_none();
        let finish_empty = |status| {
            finish(
                FnoStocksResponse::empty(status, open, 0, now),
                filter.clone(),
                membership,
                fno,
            )
        };
        if filter.fno_only && fno.is_none() {
            return finish_empty(if open { "no_fno_list" } else { "closed" });
        }
        if (!filter.fno_only || filter.index.is_some()) && membership.is_none() {
            return finish_empty(if open { "no_index_list" } else { "closed" });
        }
        let names = if filter.fno_only {
            fno.map(|f| &f.names)
        } else {
            membership.map(|m| &m.names)
        };
        let Some(names) = names else {
            return finish_empty(if open { "no_index_list" } else { "closed" });
        };
        let selected_index = filter
            .index
            .as_ref()
            .and_then(|name| membership?.indices.binary_search(name).ok());
        if filter.index.is_some() && selected_index.is_none() {
            return finish_empty(if open { "no_index_list" } else { "closed" });
        }
        let names: HashMap<_, _> = names
            .iter()
            .filter(|(key, _)| {
                selected_index.is_none_or(|index| {
                    membership
                        .and_then(|m| m.by_stock.get(key))
                        .is_some_and(|indices| indices.binary_search(&index).is_ok())
                })
            })
            .map(|(key, name)| (*key, name.clone()))
            .collect();
        if names.len() > MAX_SOURCE_ROWS {
            return Reply::error(BridgeError::ResponseTooLarge, now);
        }
        if names.is_empty() {
            return finish_empty(if open { "no_data" } else { "closed" });
        }
        let list = FnoUnderlyings { names };
        let prices = compute_for_list(now_ist, market_open, read_at_ist, &list, query);
        finish(prices, filter, membership, fno)
    }
}

#[cfg(test)]
mod tests {
    use super::oracle::finish;
    use super::*;
    use fno_stocks::FnoStocksResponse;
    use serde_json::json;
    use std::path::PathBuf;
    use std::time::SystemTime;
    use tickvault_common::operator_movers_universe::ACTUAL_VALUE_UNAVAILABLE;
    use tickvault_common::operator_movers_wire::{MAX_WIRE_BYTES, SourceStatus};
    use tickvault_common::operator_stock_movers_wire::{
        Direction, MembershipStatus, Outcome, Snapshot,
    };
    use tickvault_common::operator_stock_movers_wire::{SortColumn, SortOrder};
    const DATE: &str = "2026-10-30";
    fn at(text: &str) -> chrono::NaiveDateTime {
        chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").unwrap()
    }
    fn mapping(count: usize) -> serde_json::Value {
        let mut mappings = Vec::new();
        for id in 1..=count {
            mappings.push(json!({"index_name":"Nifty 50","symbol":format!("STOCK{id:03}"),"security_id":id,"exchange_segment":1}));
        }
        for id in [2, 4, 6] {
            if id <= count {
                mappings.push(json!({"index_name":"Nifty Bank","symbol":format!("STOCK{id:03}"),"security_id":id,"exchange_segment":1}));
            }
        }
        json!({"trading_date_ist":DATE,"resolved":mappings.len(),"unresolved":0,"mappings":mappings})
    }
    struct Fixture(PathBuf);
    impl Fixture {
        fn new(count: usize) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "tv-index-movers-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(
                dir.join(format!("{INDEX_MAPPING_PREFIX}{DATE}.json")),
                serde_json::to_vec(&mapping(count)).unwrap(),
            )
            .unwrap();
            let rows:Vec<_>=(1..=count).filter(|id|id%2==0).map(|id|json!({"index_name":"FNO_UNDERLYING","symbol":format!("STOCK{id:03}"),"security_id":id,"exchange_segment":1})).collect();
            std::fs::write(
                fno_stocks::fno_underlying_artifact_path(&dir, DATE),
                serde_json::to_vec(&json!({"count":rows.len(),"mappings":rows})).unwrap(),
            )
            .unwrap();
            Self(dir)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    async fn http(status: &str, body: String) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let status = status.to_owned();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 16384];
            let read = stream.read(&mut request).await.unwrap();
            assert!(read > 0);
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        format!("http://{address}")
    }
    fn prices(count: usize, time: &str) -> String {
        json!({"dataset":(1..=count).map(|id|json!([id,"NSE_EQ",100.0+id as f64,100.0,1000,101.0,time])).collect::<Vec<_>>()}).to_string()
    }
    async fn run(dir: &Path, url: &str, open: bool, filter: Filter) -> Reply {
        compute(
            FnoStocksInputs {
                client: &reqwest::Client::builder().no_proxy().build().unwrap(),
                base_url: url,
                artifact_dir: dir,
                market_open: open,
                now_ist: at("2026-10-30 11:15:30"),
                read_at_ist: filter
                    .as_of
                    .as_deref()
                    .map(|t| chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M").unwrap()),
                timeout: Duration::from_millis(300),
            },
            filter,
        )
        .await
    }
    fn snapshot(reply: Reply) -> Snapshot {
        match reply.outcome {
            Outcome::Snapshot { snapshot } => *snapshot,
            other => panic!("expected snapshot, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stocks_columns_preserve_captured_values_without_relabeling_turnover_estimate() {
        let fixture = Fixture::new(6);
        let data = json!({"dataset":[
            [2,"NSE_EQ",110.0,100.0,123456,108.0,"2026-10-30T11:15:10Z"],
            [4,"NSE_EQ",90.0,null,null,null,"2026-10-30T11:15:10Z"],
            [6,"NSE_EQ",100.0,100.0,0,100.0,"2026-10-30T11:15:10Z"]
        ]})
        .to_string();
        let url = http("200 OK", data).await;
        let result = snapshot(run(&fixture.0, &url, true, Filter::default()).await);
        assert_eq!(result.value_basis, ACTUAL_VALUE_UNAVAILABLE);
        assert_eq!(result.rows.len(), 3);
        let known = &result.rows[0];
        assert_eq!(known.price.name, "STOCK002");
        assert_eq!(known.price.ltp, 110.0);
        assert_eq!(known.price.prev_close, Some(100.0));
        assert_eq!(known.price.change, Some(10.0));
        assert_eq!(known.price.change_pct, Some(10.0));
        assert_eq!(known.price.volume, Some(123456));
        assert_eq!(known.volume_text.as_deref(), Some("123456"));
        assert!(known.price.value_cr.is_none());
        assert!(known.company_name.is_none());
        let missing_row = result
            .rows
            .iter()
            .find(|row| row.price.security_id == 4)
            .unwrap();
        let missing = &missing_row.price;
        assert!(
            missing.prev_close.is_none()
                && missing.change.is_none()
                && missing.change_pct.is_none()
        );
        assert!(missing.volume.is_none() && missing.value_cr.is_none());
        assert!(missing_row.volume_text.is_none());
        let zero = result
            .rows
            .iter()
            .find(|row| row.price.security_id == 6)
            .unwrap();
        assert!(zero.price.volume.is_none() && zero.volume_text.is_none());
        assert_eq!(
            zero.price.change,
            Some(0.0),
            "a real zero change remains known"
        );
    }

    #[tokio::test]
    async fn explicit_column_sort_runs_before_paging_the_full_eligible_universe() {
        let fixture = Fixture::new(80);
        for (column, order, offset, first, last) in [
            (SortColumn::Name, SortOrder::Asc, 0, 1, 20),
            (SortColumn::Ltp, SortOrder::Asc, 20, 21, 40),
            (SortColumn::Change, SortOrder::Desc, 0, 80, 61),
            (SortColumn::ChangePct, SortOrder::Asc, 0, 1, 20),
            (SortColumn::Volume, SortOrder::Desc, 0, 1, 20),
            // Actual value is unavailable for every row, so stable identity
            // ordering applies rather than sorting an unlabelled estimate.
            (SortColumn::ValueCr, SortOrder::Desc, 20, 21, 40),
        ] {
            let data = json!({"dataset":(1..=80).map(|id|json!([
                id,"NSE_EQ",100.0+id as f64,100.0,10000-id,100.0,
                "2026-10-30T11:15:10Z"
            ])).collect::<Vec<_>>()})
            .to_string();
            let url = http("200 OK", data).await;
            let filter = Filter {
                fno_only: false,
                sort_by: Some(column),
                sort_order: Some(order),
                offset,
                ..Filter::default()
            };
            let result = snapshot(run(&fixture.0, &url, true, filter.clone()).await);
            assert_eq!(result.filter, filter);
            assert_eq!(
                (
                    result.universe_total,
                    result.stocks_priced,
                    result.matching_total,
                    result.rows.len()
                ),
                (80, 80, 80, 20)
            );
            assert_eq!(
                result.rows.first().unwrap().price.security_id,
                first,
                "{column:?}"
            );
            assert_eq!(
                result.rows.last().unwrap().price.security_id,
                last,
                "{column:?}"
            );
            assert!(result.valid());
        }
    }

    #[tokio::test]
    async fn sorted_missing_fields_stay_last_and_equal_values_keep_page_identity() {
        let fixture = Fixture::new(80);
        for order in [SortOrder::Asc, SortOrder::Desc] {
            let data = json!({"dataset":(1..=80).rev().map(|id|json!([
                id,"NSE_EQ",100.0+id as f64,100.0,
                if id > 40 {None} else {Some(7)},100.0,"2026-10-30T11:15:10Z"
            ])).collect::<Vec<_>>()})
            .to_string();
            let url = http("200 OK", data).await;
            let result = snapshot(
                run(
                    &fixture.0,
                    &url,
                    true,
                    Filter {
                        fno_only: false,
                        sort_by: Some(SortColumn::Volume),
                        sort_order: Some(order),
                        offset: 20,
                        ..Filter::default()
                    },
                )
                .await,
            );
            assert_eq!(
                result
                    .rows
                    .iter()
                    .map(|r| r.price.security_id)
                    .collect::<Vec<_>>(),
                (21..=40).collect::<Vec<_>>()
            );
            assert!(result.rows.iter().all(|r| r.price.volume == Some(7)));
        }
    }

    #[tokio::test]
    async fn sort_filter_errors_are_rejected_by_router_before_capture() {
        use axum::{
            body::Body,
            http::{Request, StatusCode},
        };
        use secrecy::SecretString;
        use tower::ServiceExt;
        let state = fno_stocks::tests::mock_state(1);
        // Holding the capture slot would yield Busy if validation reached the
        // query path. Each bad sort must instead fail with HTTP 400 first.
        let _held = state.stock_movers_cache().slot.lock().await;
        let router = crate::build_router_with_auth(
            state.clone(),
            &[],
            crate::middleware::ApiAuthConfig::from_token(SecretString::from(
                "mover-secret".to_owned(),
            )),
            false,
        );
        for query in [
            "sort_by=volume",
            "sort_order=asc",
            "sort_by=turnover&sort_order=desc",
            "sort_by=volume&sort_order=sideways",
            "sort_by=volume&sort_order=asc&offset=1000",
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/api/stock-movers?{query}"))
                        .header("authorization", "Bearer mover-secret")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
        }
    }

    #[tokio::test]
    async fn api_cache_never_serves_a_differently_sorted_snapshot() {
        // Plan PR MV: the cache holds the universe, not a body, so each sort
        // is selected afresh from it and echoes its own filter.
        let state = fno_stocks::tests::mock_state(1);
        let observed = chrono::Utc::now().timestamp();
        let universe = cached_universe(observed);
        seed_cache(&state, None, Arc::clone(&universe)).await;
        let filter = Filter {
            sort_by: Some(SortColumn::Volume),
            sort_order: Some(SortOrder::Desc),
            ..Filter::default()
        };
        let changed = Filter {
            sort_order: Some(SortOrder::Asc),
            ..filter.clone()
        };
        for f in [filter, changed] {
            let hit = get_stock_movers(State(state.clone()), Query(f.clone())).await;
            assert_eq!(hit.headers()["x-tv-cache"], "hit");
            let reply = reply_of(hit).await;
            assert_eq!(reply, select(&universe, &f));
            assert_eq!(snapshot(reply).filter, f);
        }
    }

    #[test]
    fn membership_parser_checks_date_counts_identity_and_bounds() {
        let value = mapping(8);
        let good = parse_memberships(&serde_json::to_vec(&value).unwrap(), DATE).unwrap();
        assert_eq!(good.names.len(), 8);
        assert_eq!(good.by_stock[&(2, 1)].len(), 2);
        assert!(good.partial);
        for (pointer, bad) in [
            ("/trading_date_ist", json!("2026-10-29")),
            ("/resolved", json!(999)),
            ("/mappings/0/security_id", json!(0)),
            ("/mappings/0/symbol", json!("\n")),
            (
                "/mappings/0/index_name",
                json!("x".repeat(MAX_INDEX_NAME_BYTES + 1)),
            ),
            ("/mappings/0/exchange_segment", json!(0)),
        ] {
            let mut changed = value.clone();
            *changed.pointer_mut(pointer).unwrap() = bad;
            assert!(
                parse_memberships(&serde_json::to_vec(&changed).unwrap(), DATE).is_none(),
                "{pointer}"
            );
        }
        let mut duplicate = value.clone();
        let row = duplicate["mappings"][0].clone();
        duplicate["mappings"].as_array_mut().unwrap().push(row);
        duplicate["resolved"] = json!(12);
        assert!(parse_memberships(&serde_json::to_vec(&duplicate).unwrap(), DATE).is_none());
        let mut conflict = value;
        conflict["mappings"][8]["symbol"] = json!("WRONG");
        assert!(parse_memberships(&serde_json::to_vec(&conflict).unwrap(), DATE).is_none());
        assert!(parse_memberships(&vec![b' '; MAX_MAPPING_BYTES + 1], DATE).is_none());
    }
    #[tokio::test]
    async fn index_and_fno_filter_the_universe_before_top_page() {
        let f = Fixture::new(80);
        let url = http("200 OK", prices(80, "2026-10-30T11:15:10.000000Z")).await;
        let s = snapshot(
            run(
                &f.0,
                &url,
                true,
                Filter {
                    index: Some("Nifty Bank".into()),
                    ..Filter::default()
                },
            )
            .await,
        );
        assert_eq!(
            (s.universe_total, s.stocks_priced, s.matching_total),
            (3, 3, 3)
        );
        assert_eq!(
            s.rows
                .iter()
                .map(|s| s.price.security_id)
                .collect::<Vec<_>>(),
            [6, 4, 2]
        );
        assert!(
            s.rows
                .iter()
                .all(|r| r.fno == Some(true) && r.indices.len() == 2)
        );
    }
    #[tokio::test]
    async fn search_finds_a_stock_outside_the_unfiltered_top_twenty() {
        let f = Fixture::new(80);
        let url = http("200 OK", prices(80, "2026-10-30T11:15:10.000000Z")).await;
        let s = snapshot(
            run(
                &f.0,
                &url,
                true,
                Filter {
                    fno_only: false,
                    search: "stock002".into(),
                    ..Filter::default()
                },
            )
            .await,
        );
        assert_eq!(
            (s.universe_total, s.stocks_priced, s.matching_total),
            (80, 80, 1)
        );
        assert_eq!(s.rows[0].price.security_id, 2);
    }
    #[tokio::test]
    async fn all_equities_include_non_fno_and_pagination_preserves_totals() {
        let f = Fixture::new(80);
        let url = http("200 OK", prices(80, "2026-10-30T11:15:10.000000Z")).await;
        let s = snapshot(
            run(
                &f.0,
                &url,
                true,
                Filter {
                    fno_only: false,
                    offset: 20,
                    ..Filter::default()
                },
            )
            .await,
        );
        assert_eq!(
            (
                s.universe_total,
                s.stocks_priced,
                s.matching_total,
                s.rows.len()
            ),
            (80, 80, 80, 20)
        );
        assert_eq!(s.rows[0].price.security_id, 60);
        assert!(s.rows.iter().any(|r| r.fno == Some(false)));
    }
    #[tokio::test]
    async fn missing_fno_is_unknown_for_all_but_refused_for_fno_only() {
        let f = Fixture::new(8);
        std::fs::remove_file(fno_stocks::fno_underlying_artifact_path(&f.0, DATE)).unwrap();
        let s = snapshot(run(&f.0, "http://127.0.0.1:1", true, Filter::default()).await);
        assert_eq!(s.status, SourceStatus::NoFnoList);
        assert!(s.rows.is_empty());
        let url = http("200 OK", prices(8, "2026-10-30T11:15:10.000000Z")).await;
        let s = snapshot(
            run(
                &f.0,
                &url,
                true,
                Filter {
                    fno_only: false,
                    ..Filter::default()
                },
            )
            .await,
        );
        assert!(!s.fno_list_available);
        assert!(s.rows.iter().all(|r| r.fno.is_none()));
    }
    #[tokio::test]
    async fn missing_membership_never_guesses_an_index_or_uses_yesterday() {
        let f = Fixture::new(8);
        let path = f.0.join(format!("{INDEX_MAPPING_PREFIX}{DATE}.json"));
        std::fs::rename(
            &path,
            f.0.join(format!("{INDEX_MAPPING_PREFIX}2026-10-29.json")),
        )
        .unwrap();
        for filter in [
            Filter {
                fno_only: false,
                ..Filter::default()
            },
            Filter {
                index: Some("Nifty Bank".into()),
                ..Filter::default()
            },
        ] {
            let s = snapshot(run(&f.0, "http://127.0.0.1:1", true, filter).await);
            assert_eq!(s.status, SourceStatus::NoIndexList);
            assert_eq!(s.membership_status, MembershipStatus::Unavailable);
            assert!(s.rows.is_empty());
        }
        let url = http("200 OK", prices(8, "2026-10-30T11:15:10.000000Z")).await;
        let s = snapshot(run(&f.0, &url, true, Filter::default()).await);
        assert_eq!(s.rows.len(), 4);
        assert!(s.rows.iter().all(|r| r.indices.is_empty()));
        assert_eq!(s.membership_status, MembershipStatus::Unavailable);
    }
    #[tokio::test]
    async fn closed_stale_partial_empty_and_query_failure_remain_distinct() {
        let f = Fixture::new(8);
        // After the close the stored session prices show (owner 2026-11-03).
        let url = http("200 OK", prices(8, "2026-10-30T11:15:10.000000Z")).await;
        let closed = snapshot(run(&f.0, &url, false, Filter::default()).await);
        assert_eq!(closed.status, SourceStatus::Closed);
        assert!(!closed.market_open);
        assert!(!closed.rows.is_empty());
        assert!(closed.universe_available);
        assert_eq!(closed.universe_total, 4);
        let failed_closed =
            snapshot(run(&f.0, "http://127.0.0.1:1", false, Filter::default()).await);
        assert_eq!(failed_closed.status, SourceStatus::Unreachable);
        assert!(failed_closed.rows.is_empty());
        let url = http("200 OK", prices(2, "2026-10-30T11:10:00.000000Z")).await;
        let stale = snapshot(run(&f.0, &url, true, Filter::default()).await);
        assert_eq!(stale.status, SourceStatus::Stale);
        assert_eq!((stale.universe_total, stale.stocks_priced), (4, 1));
        assert_eq!(stale.membership_status, MembershipStatus::Partial);
        let url = http("200 OK", json!({"dataset":[]}).to_string()).await;
        assert_eq!(
            snapshot(run(&f.0, &url, true, Filter::default()).await).status,
            SourceStatus::NoData
        );
        let url = http(
            "400 Bad Request",
            json!({"error":"private detail"}).to_string(),
        )
        .await;
        let failed = run(&f.0, &url, true, Filter::default()).await;
        assert!(
            !serde_json::to_string(&failed)
                .unwrap()
                .contains("private detail")
        );
        assert_eq!(snapshot(failed).status, SourceStatus::QueryFailed);
        assert_eq!(
            snapshot(run(&f.0, "http://127.0.0.1:1", true, Filter::default()).await).status,
            SourceStatus::Unreachable
        );
    }
    #[tokio::test]
    async fn chosen_minute_reads_the_stored_session_even_while_the_market_is_open() {
        // Owner 2026-11-03: pick any stored date and time on the movers page.
        let f = Fixture::new(8);
        let url = http("200 OK", prices(8, "2026-10-30T09:59:10.000000Z")).await;
        let filter = Filter {
            as_of: Some("2026-10-30T10:00".into()),
            ..Filter::default()
        };
        let s = snapshot(run(&f.0, &url, true, filter.clone()).await);
        assert_eq!(s.status, SourceStatus::Closed);
        assert!(!s.market_open);
        assert!(!s.rows.is_empty());
        assert_eq!(s.filter, filter);
        assert_eq!(s.as_of_ist.as_deref(), Some("09:59:10"));
        assert_eq!(s.data_age_secs, Some(50));
        // Before that day's 09:15 nothing traded: an empty `closed`, no query.
        let early = Filter {
            as_of: Some("2026-10-30T09:00".into()),
            ..Filter::default()
        };
        let s = snapshot(run(&f.0, "http://127.0.0.1:1", true, early).await);
        assert_eq!(s.status, SourceStatus::Closed);
        assert!(s.rows.is_empty());
    }
    #[tokio::test]
    async fn direction_and_missing_close_are_filtered_before_pagination() {
        let f = Fixture::new(8);
        let data=json!({"dataset":[[2,"NSE_EQ",90.0,100.0,1,90.0,"2026-10-30T11:15:10Z"],[4,"NSE_EQ",80.0,100.0,1,80.0,"2026-10-30T11:15:10Z"],[6,"NSE_EQ",100.0,null,1,100.0,"2026-10-30T11:15:10Z"],[8,"NSE_EQ",100.0,100.0,1,100.0,"2026-10-30T11:15:10Z"]]}).to_string();
        let url = http("200 OK", data.clone()).await;
        let s = snapshot(
            run(
                &f.0,
                &url,
                true,
                Filter {
                    direction: Direction::Losers,
                    ..Filter::default()
                },
            )
            .await,
        );
        assert_eq!(
            s.rows
                .iter()
                .map(|r| r.price.security_id)
                .collect::<Vec<_>>(),
            [4, 2]
        );
        let url = http("200 OK", data).await;
        let s = snapshot(run(&f.0, &url, true, Filter::default()).await);
        assert_eq!(s.matching_total, 4);
        assert_eq!(s.rows.last().unwrap().price.change_pct, None);
        assert!(s.rows.iter().any(|r| r.price.change_pct == Some(0.0)));
    }
    #[tokio::test]
    async fn another_segment_cannot_supply_a_price_or_membership() {
        let f = Fixture::new(8);
        let data=json!({"dataset":[[2,"IDX_I",50000.0,40000.0,1,1.0,"2026-10-30T11:15:10Z"],[4,"NSE_EQ",110.0,100.0,1,1.0,"2026-10-30T11:15:10Z"]]}).to_string();
        let url = http("200 OK", data).await;
        let s = snapshot(run(&f.0, &url, true, Filter::default()).await);
        assert_eq!(s.rows.len(), 1);
        assert_eq!(s.rows[0].price.security_id, 4);
        assert_eq!(s.rows[0].price.segment, "NSE_EQ");
    }
    #[tokio::test]
    async fn new_route_preserves_bearer_auth_and_validates_filter() {
        use axum::{
            body::Body,
            http::{Request, StatusCode},
        };
        use secrecy::SecretString;
        use tower::ServiceExt;
        let state = fno_stocks::tests::mock_state(1);
        let auth = crate::middleware::ApiAuthConfig::from_token(SecretString::from(
            "mover-secret".to_owned(),
        ));
        let router = crate::build_router_with_auth(state, &[], auth, false);
        let request = |path, token: bool| {
            let mut r = Request::builder().uri(path);
            if token {
                r = r.header("authorization", "Bearer mover-secret");
            }
            r.body(Body::empty()).unwrap()
        };
        assert_eq!(
            router
                .clone()
                .oneshot(request("/api/stock-movers", false))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            router
                .clone()
                .oneshot(request("/api/stock-movers?offset=1", true))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            router
                .oneshot(request("/api/stock-movers?command=evil", true))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    #[tokio::test]
    async fn api_cache_answers_every_live_filter_from_one_universe_and_retains_original_clock() {
        let state = fno_stocks::tests::mock_state(1);
        let observed = chrono::Utc::now().timestamp() - 10;
        seed_cache(&state, None, cached_universe(observed)).await;
        for f in [
            Filter::default(),
            Filter {
                fno_only: false,
                ..Filter::default()
            },
        ] {
            let response = get_stock_movers(State(state.clone()), Query(f.clone())).await;
            assert_eq!(response.headers()["x-tv-cache"], "hit", "{f:?}");
            let reply = reply_of(response).await;
            assert_eq!(
                reply.captured_at_epoch_secs, observed,
                "original clock kept"
            );
            assert_eq!(snapshot(reply).filter, f);
        }
        // A chosen minute is a different universe: never served from the live one.
        let response = get_stock_movers(
            State(state),
            Query(Filter {
                as_of: Some("2026-11-03T15:29".into()),
                ..Filter::default()
            }),
        )
        .await;
        assert_eq!(response.headers()["x-tv-cache"], "miss");
    }
    #[tokio::test]
    async fn test_build_live_universe_never_panics_and_builds_a_valid_universe() {
        // No lists in the test working directory and no QuestDB: the build
        // still returns a well-formed universe (no query, no lists).
        let state = fno_stocks::tests::mock_state(1);
        let clock = LiveClock::now();
        let universe = build_live_universe(&state, clock).await;
        assert!(universe.valid(), "{universe:?}");
        assert_eq!(universe.market_open, clock.market_open);
        assert_eq!(universe.observed_at_epoch_secs, clock.now_epoch);
        assert_eq!(
            universe.read_at_ist_secs - universe.observed_at_epoch_secs,
            19_800
        );
        assert!(universe.fno.is_none() && universe.membership.is_none());
    }
    fn cached_universe(observed: i64) -> Arc<MoversUniverse> {
        use tickvault_common::operator_movers_universe::{
            UNIVERSE_IST_OFFSET_SECS, ist_date_of_epoch,
        };
        let listed = ListedStock {
            security_id: 1,
            segment_code: EXCHANGE_SEGMENT_NSE_EQ,
            name: "STOCK001".into(),
        };
        Arc::new(MoversUniverse {
            format_version: UNIVERSE_FORMAT_VERSION,
            trading_date_ist: ist_date_of_epoch(observed).unwrap(),
            observed_at_epoch_secs: observed,
            read_at_ist_secs: observed + UNIVERSE_IST_OFFSET_SECS,
            market_open: tickvault_common::market_hours::is_trading_session_now(),
            window_secs: UNIVERSE_DEFAULT_WINDOW_SECS,
            prices: UniversePrices::Rows { rows: Vec::new() },
            fno: Some(vec![listed.clone()]),
            membership: Some(UniverseMembership {
                names: vec![listed],
                indices: vec!["Nifty 50".into()],
                by_stock: vec![StockIndices {
                    security_id: 1,
                    segment_code: EXCHANGE_SEGMENT_NSE_EQ,
                    indices: vec![0],
                }],
                partial: false,
            }),
        })
    }
    async fn seed_cache(
        state: &SharedAppState,
        as_of: Option<String>,
        universe: Arc<MoversUniverse>,
    ) {
        *state.stock_movers_cache().slot.lock().await = Some((
            Instant::now(),
            as_of,
            universe,
            chrono::Utc::now().timestamp() + 60,
            tickvault_common::market_hours::is_trading_session_now(),
        ));
    }
    async fn reply_of(response: Response) -> Reply {
        let bytes = axum::body::to_bytes(response.into_body(), MAX_WIRE_BYTES)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }
    #[tokio::test]
    async fn invalid_or_future_newest_rows_cannot_refresh_old_valid_prices() {
        let f = Fixture::new(8);
        let data=json!({"dataset":[[2,"NSE_EQ",110.0,100.0,1,1.0,"2026-10-30T11:10:00Z"],[4,"NSE_EQ",0.0,100.0,1,1.0,"2026-10-30T11:15:30Z"],[6,"NSE_EQ",110.0,100.0,1,1.0,"2026-10-30T11:16:00Z"]]}).to_string();
        let url = http("200 OK", data).await;
        let s = snapshot(run(&f.0, &url, true, Filter::default()).await);
        assert_eq!(s.status, SourceStatus::Stale);
        assert_eq!(s.data_age_secs, Some(330));
        assert_eq!(s.rows.len(), 1);
        assert_eq!(s.rows[0].price.security_id, 2);
    }
    #[tokio::test]
    async fn unknown_index_does_not_fall_back_to_unfiltered_prices() {
        let f = Fixture::new(8);
        let s = snapshot(
            run(
                &f.0,
                "http://127.0.0.1:1",
                true,
                Filter {
                    index: Some("Not a real index".into()),
                    ..Filter::default()
                },
            )
            .await,
        );
        assert_eq!(s.status, SourceStatus::NoIndexList);
        assert!(!s.universe_available);
        assert!(s.rows.is_empty());
    }
    #[tokio::test]
    async fn duplicate_price_identity_and_over_cap_database_body_are_refused() {
        let f = Fixture::new(8);
        let row = json!([2, "NSE_EQ", 110.0, 100.0, 1, 1.0, "2026-10-30T11:15:10Z"]);
        let url = http("200 OK", json!({"dataset":[row.clone(),row]}).to_string()).await;
        assert!(matches!(
            run(&f.0, &url, true, Filter::default()).await.outcome,
            Outcome::Error {
                code: BridgeError::InvalidResponse
            }
        ));
        let url = http(
            "200 OK",
            " ".repeat(tickvault_common::operator_movers_wire::MAX_SOURCE_BYTES + 1),
        )
        .await;
        assert_eq!(
            snapshot(run(&f.0, &url, true, Filter::default()).await).status,
            SourceStatus::QueryFailed
        );
    }
    #[test]
    fn worst_case_escaped_membership_never_raises_the_ssm_byte_cap() {
        let mut prices = FnoStocksResponse::empty("live", true, 20, 1_800_000_000);
        prices.as_of_ist = Some("10:15:00".into());
        prices.data_age_secs = Some(2);
        prices.stocks_priced = 20;
        prices.stocks = (1..=20)
            .map(|id| fno_stocks::FnoStockRow {
                security_id: id,
                segment: "NSE_EQ",
                name: "\\".repeat(128),
                ltp: 100.0,
                prev_close: Some(100.0),
                change: Some(1.0),
                change_pct: Some(1.0),
                volume: Some(1),
                value_cr: Some(0.1),
                last_trade_ist: "10:15:00".into(),
            })
            .collect();
        let indices: Vec<_> = (0..MAX_INDEX_NAMES)
            .map(|n| format!("{n:02}{}", "\\".repeat(MAX_INDEX_NAME_BYTES - 2)))
            .collect();
        let membership = Memberships {
            names: HashMap::new(),
            indices,
            by_stock: (1..=20)
                .map(|id| ((id, 1), (0..MAX_INDEX_NAMES).collect()))
                .collect(),
            partial: false,
        };
        let reply = finish(
            prices,
            Filter {
                fno_only: false,
                ..Filter::default()
            },
            Some(&membership),
            None,
        );
        assert_eq!(
            reply.outcome,
            Outcome::Error {
                code: BridgeError::ResponseTooLarge
            }
        );
        assert!(serde_json::to_vec(&reply).unwrap().len() < MAX_WIRE_BYTES);
    }
    #[tokio::test]
    async fn concurrent_api_request_returns_busy_without_an_unbounded_wait_queue() {
        let state = fno_stocks::tests::mock_state(1);
        let held = state.stock_movers_cache().slot.lock().await;
        let response = tokio::time::timeout(
            Duration::from_millis(50),
            get_stock_movers(State(state.clone()), Query(Filter::default())),
        )
        .await
        .unwrap();
        let body = axum::body::to_bytes(response.into_body(), 1000)
            .await
            .unwrap();
        let reply: Reply = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            reply.outcome,
            Outcome::Error {
                code: BridgeError::Busy
            }
        );
        drop(held);
    }
    #[test]
    fn cache_deadline_stops_at_open_close_and_ist_day_boundaries() {
        let epoch = |text| at(text).and_utc().timestamp() - IST_UTC_OFFSET_SECONDS_I64;
        for (observed, expiry) in [
            ("2026-10-30 08:59:59", "2026-10-30 09:00:00"),
            ("2026-10-30 15:39:59", "2026-10-30 15:40:00"),
            ("2026-10-30 23:59:59", "2026-10-31 00:00:00"),
            ("2026-10-30 10:00:00", "2026-10-30 10:01:00"),
        ] {
            let deadline = snapshot_deadline(epoch(observed)).unwrap();
            assert_eq!(deadline, epoch(expiry));
            assert!(cache_current(deadline, true, deadline - 1, true));
            assert!(!cache_current(deadline, true, deadline, true));
            assert!(!cache_current(deadline, true, deadline - 1, false));
            assert!(!cache_current(deadline, false, deadline - 1, true));
        }
        assert!(snapshot_deadline(i64::MAX).is_none());
    }

    #[tokio::test]
    async fn query_accepts_full_thousand_and_refuses_an_overflow_probe_row() {
        let f = Fixture::new(1000);
        let url = http("200 OK", prices(1000, "2026-10-30T11:15:10Z")).await;
        let s = snapshot(
            run(
                &f.0,
                &url,
                true,
                Filter {
                    fno_only: false,
                    search: "STOCK001".into(),
                    ..Filter::default()
                },
            )
            .await,
        );
        assert_eq!(
            (s.universe_total, s.stocks_priced, s.matching_total),
            (1000, 1000, 1)
        );
        assert_eq!(s.rows[0].price.security_id, 1);
        let url = http("200 OK", prices(1001, "2026-10-30T11:15:10Z")).await;
        let s = snapshot(
            run(
                &f.0,
                &url,
                true,
                Filter {
                    fno_only: false,
                    ..Filter::default()
                },
            )
            .await,
        );
        assert_eq!(s.status, SourceStatus::QueryFailed);
        assert!(s.rows.is_empty());
    }
    #[test]
    fn fno_metadata_cache_rereads_replacement_and_refuses_deleted_or_torn_list() {
        let f = Fixture::new(8);
        let path = fno_stocks::fno_underlying_artifact_path(&f.0, DATE);
        assert_eq!(fno_stocks::load_fno_underlyings(&path).unwrap().len(), 4);
        std::fs::write(&path,br#"{"count":1,"mappings":[{"index_name":"FNO_UNDERLYING","symbol":"NEW","security_id":100,"exchange_segment":1}]}"#).unwrap();
        assert_eq!(
            fno_stocks::load_fno_underlyings(&path)
                .unwrap()
                .name_of(100, 1),
            Some("NEW")
        );
        std::fs::write(&path, br#"{"count":5,"mappings":[]}"#).unwrap();
        assert!(fno_stocks::load_fno_underlyings(&path).is_none());
        std::fs::remove_file(path).unwrap();
        assert!(
            fno_stocks::load_fno_underlyings(&fno_stocks::fno_underlying_artifact_path(&f.0, DATE))
                .is_none()
        );
    }
    #[tokio::test]
    async fn test_require_configured_bearer_auth_refuses_disabled_or_empty_auth_even_during_paper_boot()
     {
        use axum::{
            body::Body,
            http::{Request, StatusCode},
        };
        use secrecy::SecretString;
        use tower::ServiceExt;
        let mut forced = crate::middleware::ApiAuthConfig::disabled();
        forced.enabled = true;
        for auth in [
            crate::middleware::ApiAuthConfig::disabled(),
            crate::middleware::ApiAuthConfig::from_token(SecretString::from(String::new())),
            forced,
        ] {
            let router =
                crate::build_router_with_auth(fno_stocks::tests::mock_state(1), &[], auth, true);
            for path in ["/api/fno-stocks", "/api/stock-movers"] {
                for token in [None, Some("Bearer "), Some("Bearer arbitrary")] {
                    let mut request = Request::builder().uri(path);
                    if let Some(token) = token {
                        request = request.header("authorization", token);
                    }
                    assert_eq!(
                        router
                            .clone()
                            .oneshot(request.body(Body::empty()).unwrap())
                            .await
                            .unwrap()
                            .status(),
                        StatusCode::SERVICE_UNAVAILABLE,
                        "{path} {token:?}"
                    );
                }
            }
        }
    }
    #[tokio::test]
    async fn test_require_configured_bearer_auth_distinguishes_missing_wrong_and_correct_tokens() {
        use axum::{
            body::Body,
            http::{Request, StatusCode},
        };
        use secrecy::SecretString;
        use tower::ServiceExt;
        let state = fno_stocks::tests::mock_state(1);
        state.fno_stocks_cache().put("{}".into());
        seed_cache(
            &state,
            None,
            cached_universe(chrono::Utc::now().timestamp()),
        )
        .await;
        let auth = crate::middleware::ApiAuthConfig::from_token(SecretString::from(
            "configured-test-token".to_owned(),
        ));
        let router = crate::build_router_with_auth(state, &[], auth, true);
        for path in ["/api/fno-stocks", "/api/stock-movers"] {
            for (token, status) in [
                (None, StatusCode::UNAUTHORIZED),
                (Some("Bearer wrong"), StatusCode::UNAUTHORIZED),
                (Some("Bearer configured-test-token"), StatusCode::OK),
            ] {
                let mut request = Request::builder().uri(path);
                if let Some(token) = token {
                    request = request.header("authorization", token);
                }
                assert_eq!(
                    router
                        .clone()
                        .oneshot(request.body(Body::empty()).unwrap())
                        .await
                        .unwrap()
                        .status(),
                    status,
                    "{path}"
                );
            }
        }
    }

    // ------------------------------------------- plan PR MV review fixes
    /// UTC instant of an IST wall time.
    fn utc_of(ist_text: &str) -> chrono::DateTime<chrono::Utc> {
        (at(ist_text) - chrono::Duration::seconds(IST_UTC_OFFSET_SECONDS_I64)).and_utc()
    }

    #[test]
    fn test_universe_constants_equal_the_fno_stocks_constants() {
        use tickvault_common::operator_movers_universe::UNIVERSE_LIVE_MAX_AGE_SECS;
        assert_eq!(
            UNIVERSE_LIVE_MAX_AGE_SECS,
            fno_stocks::FNO_STOCKS_LIVE_MAX_AGE_SECS
        );
        assert_eq!(
            UNIVERSE_DEFAULT_WINDOW_SECS,
            fno_stocks::FNO_STOCKS_WINDOW_SECS
        );
    }

    #[test]
    fn test_live_clock_takes_session_state_and_time_from_one_instant() {
        // Fri 2026-10-30 15:39:59 and 15:40:00 IST.
        let before = LiveClock::at(utc_of("2026-10-30 15:39:59"));
        let after = LiveClock::at(utc_of("2026-10-30 15:40:00"));
        assert_eq!(before.now_ist, at("2026-10-30 15:39:59"));
        assert_eq!(after.now_ist, at("2026-10-30 15:40:00"));
        assert_eq!(after.now_epoch, utc_of("2026-10-30 15:40:00").timestamp());
        assert!(!after.market_open, "15:40 is closed");
        assert_eq!(
            before.market_open,
            tickvault_common::market_hours::is_trading_session_at(before.now_epoch)
        );
        // Saturday: never open.
        assert!(!LiveClock::at(utc_of("2026-10-31 11:00:00")).market_open);
        let now = LiveClock::now();
        assert!((now.now_epoch - chrono::Utc::now().timestamp()).abs() <= 1);
    }

    fn clock(ist_text: &str, market_open: bool) -> LiveClock {
        let at_utc = utc_of(ist_text);
        LiveClock {
            market_open,
            now_ist: at(ist_text),
            now_epoch: at_utc.timestamp(),
        }
    }

    #[test]
    fn test_closed_read_key_names_the_date_and_window_end_of_a_closed_read_only() {
        assert_eq!(closed_read_key(clock("2026-10-30 11:00:00", true)), None);
        assert_eq!(
            closed_read_key(clock("2026-10-30 09:10:00", false)),
            None,
            "before 09:15 nothing is queried"
        );
        let evening = closed_read_key(clock("2026-10-30 16:05:00", false)).unwrap();
        assert_eq!(evening.date_ist, DATE);
        assert_eq!(
            evening.window_end_ist_secs,
            at("2026-10-30 15:40:00").and_utc().timestamp()
        );
        assert_eq!(
            closed_read_key(clock("2026-10-30 22:30:00", false)),
            Some(evening.clone()),
            "the same read all evening"
        );
        assert_ne!(
            closed_read_key(clock("2026-10-31 16:05:00", false)),
            Some(evening),
            "a new date is a new read"
        );
        // A closed day inside the session hours: the window end moves.
        assert_ne!(
            closed_read_key(clock("2026-10-31 11:00:00", false)),
            closed_read_key(clock("2026-10-31 11:00:05", false))
        );
    }

    #[test]
    fn test_restamped_moves_only_the_observation_and_only_for_the_same_closed_read() {
        let first = clock("2026-10-30 16:05:00", false);
        let key = closed_read_key(first).unwrap();
        let plan = plan_read(first.now_ist, false, None);
        let fetched = Fetched::Rows(vec![TickRow {
            security_id: 2,
            segment_code: EXCHANGE_SEGMENT_NSE_EQ,
            ltp: 110.0,
            close: Some(100.0),
            volume: Some(5),
            avg_price: None,
            ts: at("2026-10-30 15:29:00"),
        }]);
        let built = assemble_universe(&plan, None, None, fetched);
        let later = clock("2026-10-30 18:00:00", false);
        let again = restamped(&built, &key, later).unwrap();
        let fresh_plan = plan_read(later.now_ist, false, None);
        assert_eq!(again.observed_at_epoch_secs, fresh_plan.observed);
        assert_eq!(again.observed_at_epoch_secs, later.now_epoch);
        assert_eq!(
            again.read_at_ist_secs,
            at("2026-10-30 18:00:00").and_utc().timestamp()
        );
        let mut expected = built.clone();
        expected.observed_at_epoch_secs = again.observed_at_epoch_secs;
        expected.read_at_ist_secs = again.read_at_ist_secs;
        assert_eq!(again, expected, "rows, lists and window unchanged");
        assert!(again.valid());
        assert!(restamped(&built, &key, clock("2026-10-31 16:05:00", false)).is_none());
        assert!(restamped(&built, &key, clock("2026-10-30 16:05:00", true)).is_none());
        let mut open = built.clone();
        open.market_open = true;
        assert!(restamped(&open, &key, later).is_none());
    }

    #[test]
    fn test_file_cache_keeps_today_and_one_other_date() {
        let cache: fno_stocks::FileCache<u32> = fno_stocks::FileCache::new();
        let t = SystemTime::UNIX_EPOCH;
        let today = Path::new("/x/today.json");
        let other = Path::new("/x/other.json");
        let third = Path::new("/x/third.json");
        assert!(cache.get(today, 1, t).is_none());
        cache.put(today, 1, t, Arc::new(1));
        cache.put(other, 1, t, Arc::new(2));
        assert_eq!(
            cache.get(today, 1, t).as_deref(),
            Some(&1),
            "today survives"
        );
        assert_eq!(cache.get(other, 1, t).as_deref(), Some(&2));
        assert!(cache.get(today, 2, t).is_none(), "a changed file is a miss");
        cache.put(today, 2, t, Arc::new(3));
        assert_eq!(cache.get(today, 2, t).as_deref(), Some(&3));
        assert_eq!(cache.get(other, 1, t).as_deref(), Some(&2));
        // A third path evicts the least recently used one (today, here).
        cache.put(third, 1, t, Arc::new(4));
        assert!(cache.get(today, 2, t).is_none());
        assert_eq!(cache.get(other, 1, t).as_deref(), Some(&2));
        assert_eq!(cache.get(third, 1, t).as_deref(), Some(&4));
    }
}

/// Plan PR MV identity proof: for random lists, memberships, database rows,
/// clocks, query outcomes and filters, `select` over the universe equals the
/// reply the per-filter path (`oracle`) produced from the same inputs, and a
/// JSON round trip of the universe changes nothing.
#[cfg(test)]
mod select_equivalence {
    use super::*;
    use proptest::prelude::*;
    use tickvault_common::operator_stock_movers_wire::{Direction, Outcome, SortColumn, SortOrder};

    const INDEX_LABELS: [&str; 3] = ["Nifty 50", "Nifty Bank", "Nifty IT"];

    fn clock(hour: u32, minute: u32, second: u32, millis: u32) -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 10, 30)
            .unwrap()
            .and_hms_milli_opt(hour, minute, second, millis)
            .unwrap()
    }
    fn name_for(id: u64, kind: u8) -> String {
        match (kind, id % 6) {
            (1, 0 | 3) => "DUP".into(),
            (2, 0) => String::new(),
            (2, 5) => "L".repeat(130),
            _ => format!("STOCK{id:02}"),
        }
    }
    fn fno_list(mask: u32, kind: u8) -> FnoUnderlyings {
        FnoUnderlyings {
            names: (1..=24u64)
                .filter(|id| mask & (1 << id) != 0)
                .map(|id| ((id, EXCHANGE_SEGMENT_NSE_EQ), name_for(id, kind)))
                .collect(),
        }
    }
    fn memberships(
        mask: u32,
        labels: u8,
        per_stock: &[u8],
        partial: bool,
        kind: u8,
    ) -> Memberships {
        let indices: Vec<String> = INDEX_LABELS
            .iter()
            .enumerate()
            .filter(|(i, _)| labels & (1 << i) != 0)
            .map(|(_, s)| (*s).to_string())
            .collect();
        let ids: Vec<u64> = (1..=24u64).filter(|id| mask & (1 << id) != 0).collect();
        let names = ids
            .iter()
            .map(|id| ((*id, EXCHANGE_SEGMENT_NSE_EQ), name_for(*id, kind)))
            .collect();
        let by_stock = ids
            .iter()
            .map(|id| {
                let bits = per_stock[(*id as usize) % per_stock.len()];
                let positions: Vec<usize> = (0..indices.len())
                    .filter(|i| bits & (1 << i) != 0)
                    .collect();
                ((*id, EXCHANGE_SEGMENT_NSE_EQ), positions)
            })
            .collect();
        Memberships {
            names,
            indices,
            by_stock,
            partial,
        }
    }
    fn tick_row(
        id: u64,
        seg: u8,
        ltp: u8,
        close: u8,
        volume: u8,
        offset: i64,
        base: chrono::NaiveDateTime,
    ) -> TickRow {
        let ltp = match ltp {
            0 => 0.0,
            1 => -5.0,
            2 => f64::NAN,
            3 => f64::INFINITY,
            // Few distinct prices, so equal change % (tie-breaks) is common.
            n => 97.5 + f64::from(n % 4) * 2.5,
        };
        let close = match close {
            0 => None,
            1 => Some(0.0),
            2 => Some(f64::NAN),
            3 => Some(-1.0),
            _ => Some(100.0),
        };
        let volume = match volume {
            0 => None,
            1 => Some(0),
            2 => Some(u64::MAX),
            _ => Some(1_000 + id),
        };
        let base = base.with_nanosecond(0).unwrap();
        TickRow {
            security_id: id,
            segment_code: if seg == 0 { 0 } else { EXCHANGE_SEGMENT_NSE_EQ },
            ltp,
            close,
            volume,
            avg_price: Some(101.0),
            ts: base + chrono::Duration::seconds(offset),
        }
    }
    use chrono::Timelike as _;

    /// Both paths for one input; returns (new, old).
    #[allow(clippy::too_many_arguments)] // APPROVED: test oracle harness, one call site per input shape
    fn both(
        now: chrono::NaiveDateTime,
        open: bool,
        read_at: Option<chrono::NaiveDateTime>,
        fno: Option<&FnoUnderlyings>,
        membership: Option<&Memberships>,
        db: &[TickRow],
        failure: Option<QueryFailure>,
        filter: &Filter,
    ) -> (Reply, Reply, MoversUniverse) {
        let query = |ids: &[u64]| -> Result<Vec<TickRow>, QueryFailure> {
            match failure {
                Some(f) => Err(f),
                None => Ok(db
                    .iter()
                    .filter(|r| ids.binary_search(&r.security_id).is_ok())
                    .cloned()
                    .collect()),
            }
        };
        let old = oracle::reply(now, open, read_at, fno, membership, filter.clone(), &query);
        let plan = plan_read(now, open, read_at);
        let ids = union_ids(fno, membership);
        let fetched = match plan.window {
            None => Fetched::BeforeSession,
            Some(_) if ids.is_empty() => Fetched::NoQuery,
            Some(_) => match query(&ids) {
                Ok(rows) => Fetched::Rows(rows),
                Err(f) => Fetched::Failed(f),
            },
        };
        let universe = assemble_universe(&plan, fno, membership, fetched);
        (select(&universe, filter), old, universe)
    }

    fn filter_from(
        fno_only: bool,
        index: u8,
        search: u8,
        direction: u8,
        offset: u8,
        sort: Option<(u8, bool)>,
    ) -> Filter {
        Filter {
            fno_only,
            index: match index {
                0 => None,
                1 => Some("Zzz unknown".into()),
                n => Some(INDEX_LABELS[usize::from(n - 2)].into()),
            },
            search: ["", "STOCK1", "stock0", "DUP", "q"][usize::from(search)].into(),
            direction: [Direction::All, Direction::Gainers, Direction::Losers]
                [usize::from(direction)],
            offset: usize::from(offset) * 20,
            sort_by: sort.map(|(c, _)| {
                [
                    SortColumn::Name,
                    SortColumn::Ltp,
                    SortColumn::Change,
                    SortColumn::ChangePct,
                    SortColumn::ValueCr,
                    SortColumn::Volume,
                ][usize::from(c)]
            }),
            sort_order: sort.map(|(_, asc)| if asc { SortOrder::Asc } else { SortOrder::Desc }),
            as_of: None,
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]
        #[test]
        fn proptest_select_equals_the_per_filter_path(
            clock_kind in 0u8..10,
            open in any::<bool>(),
            read_kind in 0u8..8,
            fno_mask in proptest::option::weighted(0.9, any::<u32>()),
            mem_mask in proptest::option::weighted(0.9, any::<u32>()),
            labels in 0u8..8,
            per_stock in proptest::collection::vec(any::<u8>(), 1..6),
            partial in any::<bool>(),
            name_kind in 0u8..3,
            rows in proptest::collection::vec(
                (1u64..=26, 0u8..4, 0u8..16, 0u8..8, 0u8..4, -950i64..=10),
                0..60,
            ),
            failure_kind in 0u8..12,
            allow_duplicates in 0u8..8,
            fno_only in any::<bool>(),
            index in 0u8..10,
            search in 0u8..10,
            direction in 0u8..6,
            offset in 0u8..4,
            sort in proptest::option::weighted(0.4, (0u8..6, any::<bool>())),
        ) {
            let now = [
                clock(8, 0, 0, 0),
                clock(9, 10, 0, 0),
                clock(9, 20, 0, 400),
                clock(11, 15, 30, 700),
                clock(15, 35, 0, 0),
                clock(16, 0, 0, 0),
            ][usize::from(match clock_kind {
                0..=2 => clock_kind,
                3..=6 => 3,
                7 | 8 => 4,
                _ => 5,
            })];
            let read_at = [
                None,
                Some(clock(9, 10, 0, 0)),
                Some(clock(11, 0, 0, 0)),
                Some(clock(15, 29, 0, 0)),
                Some(clock(23, 59, 0, 0)),
            ][usize::from(read_kind.saturating_sub(3))];
            let fno = fno_mask.map(|m| fno_list(m, name_kind));
            let membership = mem_mask.map(|m| memberships(m, labels, &per_stock, partial, name_kind));
            let base = read_at.map_or(now, |r| r.min(now));
            // One row per (id, segment), as `LATEST ON` returns, except in one
            // case in eight, which keeps duplicates (the refusal path).
            let mut seen = std::collections::HashSet::new();
            let db: Vec<TickRow> = rows
                .iter()
                .filter(|(id, seg, ..)| allow_duplicates == 0 || seen.insert((*id, *seg == 0)))
                .map(|(id, seg, ltp, close, vol, off)| tick_row(*id, *seg, *ltp, *close, *vol, *off, base))
                .collect();
            let failure = match failure_kind {
                10 => Some(QueryFailure::Unreachable),
                11 => Some(QueryFailure::Refused),
                _ => None,
            };
            // Weighted toward the informative cases: no index and no search
            // six times in ten, so most cases select priced rows.
            let code = |kind: u8| kind.saturating_sub(5);
            let direction = direction.saturating_sub(3);
            let offset = u8::from(offset == 3);
            let filter = filter_from(fno_only, code(index), code(search), direction, offset, sort);
            prop_assume!(filter.valid());
            let (new, old, universe) = both(now, open, read_at, fno.as_ref(), membership.as_ref(), &db, failure, &filter);
            prop_assert_eq!(&new, &old);
            prop_assert!(universe.valid());
            let text = serde_json::to_string(&universe).unwrap();
            let back: MoversUniverse = serde_json::from_str(&text).unwrap();
            prop_assert_eq!(&select(&back, &filter), &new);
        }
    }

    #[test]
    fn test_select_equals_the_per_filter_path_on_named_edge_cases() {
        let now = clock(11, 15, 30, 700);
        let fno = fno_list(0b1111_0000, 0);
        let membership = memberships(0b1111_1110, 0b011, &[1, 2, 3], false, 0);
        let db: Vec<TickRow> = (1..=8)
            .map(|id| tick_row(id, 1, 6, 4, 3, -5, now))
            .chain([tick_row(3, 1, 7, 4, 3, -5, now)]) // duplicate identity
            .collect();
        let cases: Vec<(bool, Option<&FnoUnderlyings>, Option<&Memberships>, Filter)> = vec![
            // fno_only with no F&O list
            (true, None, Some(&membership), Filter::default()),
            // index filter naming an index absent today
            (
                true,
                Some(&fno),
                Some(&membership),
                filter_from(false, 4, 0, 0, 0, None),
            ),
            // empty membership list
            (true, Some(&fno), None, filter_from(false, 0, 0, 0, 0, None)),
            // offset past the end
            (
                true,
                Some(&fno),
                Some(&membership),
                filter_from(true, 0, 0, 0, 1, Some((1, true))),
            ),
            // duplicate security id among all equities
            (
                true,
                Some(&fno),
                Some(&membership),
                filter_from(false, 0, 0, 0, 0, None),
            ),
            // market closed (reads the ended session)
            (
                false,
                Some(&fno),
                Some(&membership),
                filter_from(true, 2, 0, 2, 0, None),
            ),
        ];
        for (open, fno, membership, filter) in cases {
            let (new, old, _) = both(now, open, None, fno, membership, &db, None, &filter);
            assert_eq!(new, old, "{filter:?}");
        }
        // Equal change % everywhere: the name breaks the tie, in both paths.
        let tied: Vec<TickRow> = (1..=8)
            .map(|id| tick_row(id, 1, 6, 4, 3, -5, now))
            .collect();
        let all = filter_from(false, 0, 0, 0, 0, None);
        let (new, old, _) = both(
            now,
            true,
            None,
            Some(&fno),
            Some(&membership),
            &tied,
            None,
            &all,
        );
        assert_eq!(new, old);
        let Outcome::Snapshot { snapshot } = &new.outcome else {
            panic!("expected a snapshot, got {new:?}");
        };
        let names: Vec<&str> = snapshot
            .rows
            .iter()
            .map(|r| r.price.name.as_str())
            .collect();
        assert_eq!(names.first(), Some(&"STOCK01"), "{names:?}");
        // Before 09:15 on a closed read: no query, closed for every filter.
        let early = clock(9, 10, 0, 0);
        let (new, old, universe) = both(
            early,
            false,
            None,
            Some(&fno),
            Some(&membership),
            &db,
            None,
            &Filter::default(),
        );
        assert_eq!(new, old);
        assert_eq!(universe.prices, UniversePrices::BeforeSession);
    }
}

//! Canonical index/F&O enrichment and full-universe filtering before pagination.
//! Reads existing daily artifacts and captured ticks only. No subscription,
//! broker request, strategy activation or database write. Cold O(n + k log k).
use super::fno_stocks::{self, FnoStocksInputs, FnoStocksResponse, FnoUnderlyings};
use crate::response_cache::cached_json_response;
use crate::state::SharedAppState;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tickvault_common::constants::{
    EXCHANGE_SEGMENT_NSE_EQ, INDEX_CONSTITUENCY_SLUGS, IST_UTC_OFFSET_NANOS,
    IST_UTC_OFFSET_SECONDS_I64, TICK_PERSIST_END_SECS_OF_DAY_IST,
    TICK_PERSIST_START_SECS_OF_DAY_IST,
};
use tickvault_common::operator_movers_wire::{
    BridgeError, MAX_SOURCE_ROWS, MAX_WIRE_BYTES, SELECTION_LIMIT, SourceStatus, Stock,
};
use tickvault_common::operator_stock_movers_wire::{
    Direction, FILTERED_PROTOCOL_VERSION, Filter, MAX_INDEX_NAME_BYTES, MAX_INDEX_NAMES,
    MemberStock, MembershipStatus, Outcome, Reply, Snapshot,
};

/// Existing writer path, pinned against app::dhan_universe by an integration test.
pub const INDEX_MAPPING_PREFIX: &str = "dhan-nse-mapping-";
const MAX_MAPPING_BYTES: usize = 8 * 1024 * 1024;
const MAX_MAPPING_ROWS: usize = 32_768;
const CACHE_TTL_SECONDS: u64 = 2;
const QUERY_TIMEOUT_SECONDS: u64 = 3;
const CACHE_TTL: Duration = Duration::from_secs(CACHE_TTL_SECONDS);
const QUERY_TIMEOUT: Duration = Duration::from_secs(QUERY_TIMEOUT_SECONDS);
const ACTUAL_VALUE_UNAVAILABLE: &str =
    "Actual traded value is not captured; Value (Cr.) is unavailable.";

#[derive(Default)]
pub(crate) struct MoversCache {
    slot: tokio::sync::Mutex<Option<(Instant, Filter, String, i64, bool)>>,
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

/// One current file cached by path and observed metadata, never yesterday's list.
type MembershipCache = Option<(PathBuf, u64, SystemTime, Arc<Memberships>)>;
static MEMBERSHIP_CACHE: Mutex<MembershipCache> = Mutex::new(None);
fn load_memberships(dir: &Path, date: &str) -> Option<Arc<Memberships>> {
    let path = dir.join(format!("{INDEX_MAPPING_PREFIX}{date}.json"));
    let mut file = std::fs::File::open(&path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_MAPPING_BYTES as u64 {
        return None;
    }
    let modified = metadata.modified().ok()?;
    let mut cache = MEMBERSHIP_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((held, len, mtime, data)) = cache.as_ref()
        && held == &path
        && *len == metadata.len()
        && *mtime == modified
    {
        return Some(Arc::clone(data));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    (&mut file)
        .take((MAX_MAPPING_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    let data = Arc::new(parse_memberships(&bytes, date)?);
    *cache = Some((path, metadata.len(), modified, Arc::clone(&data)));
    Some(data)
}
fn stock(row: fno_stocks::FnoStockRow) -> Stock {
    Stock {
        security_id: row.security_id,
        segment: row.segment.into(),
        name: row.name,
        ltp: row.ltp,
        prev_close: row.prev_close,
        change: row.change,
        change_pct: row.change_pct,
        // Ticker packets persist their default zero without a packet-kind
        // column. It cannot prove a reported zero, so preserve only positive
        // cumulative values and leave the ambiguous zero/NULL unavailable.
        volume: row.volume.filter(|volume| *volume > 0),
        // The canonical F&O value is explicitly an estimate (volume x average
        // price). Stocks' actual-Value column must not relabel that estimate.
        value_cr: None,
        last_trade_ist: row.last_trade_ist,
    }
}
/// Canonical time boundaries are an expiry fence, not a second holiday decision.
fn snapshot_deadline(observed: i64) -> Option<i64> {
    const SECONDS_PER_DAY: i64 = 86_400;
    let local = observed.checked_add(IST_UTC_OFFSET_SECONDS_I64)?;
    let midnight = local
        .div_euclid(SECONDS_PER_DAY)
        .checked_mul(SECONDS_PER_DAY)?
        .checked_sub(IST_UTC_OFFSET_SECONDS_I64)?;
    let mut deadline = observed.checked_add(60)?;
    for seconds in [
        i64::from(TICK_PERSIST_START_SECS_OF_DAY_IST),
        i64::from(TICK_PERSIST_END_SECS_OF_DAY_IST),
        SECONDS_PER_DAY,
    ] {
        let boundary = midnight.checked_add(seconds)?;
        if boundary > observed {
            deadline = deadline.min(boundary);
        }
    }
    Some(deadline)
}
fn cache_current(deadline: i64, captured_open: bool, now: i64, current_open: bool) -> bool {
    now < deadline && captured_open == current_open
}

fn finish(
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
            // The canonical mapping persists the symbol, not the company name.
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
        }) && filter
            .index
            .as_ref()
            .is_none_or(|name| membership.is_some_and(|m| m.indices.binary_search(name).is_ok())),
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

async fn compute(inputs: FnoStocksInputs<'_>, filter: Filter) -> Reply {
    let now = inputs.now_ist.and_utc().timestamp()
        - chrono::Duration::nanoseconds(IST_UTC_OFFSET_NANOS).num_seconds();
    let date = inputs.now_ist.format("%Y-%m-%d").to_string();
    let fno = fno_stocks::load_fno_underlyings(&fno_stocks::fno_underlying_artifact_path(
        inputs.artifact_dir,
        &date,
    ));
    let membership = load_memberships(inputs.artifact_dir, &date);
    let finish_empty = |status| {
        finish(
            FnoStocksResponse::empty(status, inputs.market_open, 0, now),
            filter.clone(),
            membership.as_deref(),
            fno.as_deref(),
        )
    };
    if filter.fno_only && fno.is_none() {
        return finish_empty(if inputs.market_open {
            "no_fno_list"
        } else {
            "closed"
        });
    }
    if (!filter.fno_only || filter.index.is_some()) && membership.is_none() {
        return finish_empty(if inputs.market_open {
            "no_index_list"
        } else {
            "closed"
        });
    }
    let names = if filter.fno_only {
        fno.as_ref().map(|f| &f.names)
    } else {
        membership.as_ref().map(|m| &m.names)
    };
    let Some(names) = names else {
        return finish_empty(if inputs.market_open {
            "no_index_list"
        } else {
            "closed"
        });
    };
    let selected_index = filter
        .index
        .as_ref()
        .and_then(|name| membership.as_ref()?.indices.binary_search(name).ok());
    if filter.index.is_some() && selected_index.is_none() {
        return finish_empty(if inputs.market_open {
            "no_index_list"
        } else {
            "closed"
        });
    }
    let names: HashMap<_, _> = names
        .iter()
        .filter(|(key, _)| {
            selected_index.is_none_or(|index| {
                membership
                    .as_ref()
                    .and_then(|m| m.by_stock.get(key))
                    .is_some_and(|indices| indices.binary_search(&index).is_ok())
            })
        })
        .map(|(key, name)| (*key, name.clone()))
        .collect();
    if names.len() > MAX_SOURCE_ROWS {
        return Reply::error(BridgeError::ResponseTooLarge, now);
    }
    if !inputs.market_open {
        return finish(
            FnoStocksResponse::empty("closed", false, names.len(), now),
            filter,
            membership.as_deref(),
            fno.as_deref(),
        );
    }
    if names.is_empty() {
        return finish_empty("no_data");
    }
    let prices =
        fno_stocks::compute_for_list(inputs, Some(Arc::new(FnoUnderlyings { names }))).await;
    finish(prices, filter, membership.as_deref(), fno.as_deref())
}

/// Same bearer gate and shared limiter as /api/fno-stocks. One bounded keyed
/// slot holds at most one response; concurrent computes return typed Busy.
// TEST-EXEMPT: exercised by the async authenticated route and cache tests below.
pub async fn get_stock_movers(
    State(state): State<SharedAppState>,
    Query(filter): Query<Filter>,
) -> Response {
    if !filter.valid() {
        return (axum::http::StatusCode::BAD_REQUEST, "Invalid movers filter").into_response();
    }
    let market_open = tickvault_common::market_hours::is_trading_session_now();
    let now = chrono::Utc::now().timestamp();
    let Ok(mut slot) = state.stock_movers_cache().slot.try_lock() else {
        return axum::Json(Reply::error(
            BridgeError::Busy,
            chrono::Utc::now().timestamp(),
        ))
        .into_response();
    };
    if let Some((stored, held, body, deadline, captured_open)) = slot.as_ref()
        && *held == filter
        && stored.elapsed() < CACHE_TTL
        && cache_current(*deadline, *captured_open, now, market_open)
    {
        return cached_json_response(body.clone(), "hit");
    }
    let cfg = state.questdb_config();
    let base_url = format!("http://{}:{}", cfg.host, cfg.http_port);
    let reply = compute(
        FnoStocksInputs {
            client: state.questdb_http_client(),
            base_url: &base_url,
            artifact_dir: Path::new(fno_stocks::FNO_UNDERLYING_ARTIFACT_DIR),
            market_open,
            now_ist: fno_stocks::now_ist_naive(),
            timeout: QUERY_TIMEOUT,
        },
        filter.clone(),
    )
    .await;
    let reply = if market_open != tickvault_common::market_hours::is_trading_session_now()
        || !reply.valid_at(chrono::Utc::now().timestamp(), &filter)
    {
        Reply::error(BridgeError::Busy, chrono::Utc::now().timestamp())
    } else {
        reply
    };
    match serde_json::to_string(&reply) {
        Ok(body) => {
            *slot = Some((
                Instant::now(),
                filter,
                body.clone(),
                reply.valid_until_epoch_secs,
                market_open,
            ));
            cached_json_response(body, "miss")
        }
        Err(_) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "Movers unavailable",
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
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
    async fn api_cache_does_not_reuse_a_differently_sorted_snapshot() {
        let state = fno_stocks::tests::mock_state(1);
        let filter = Filter {
            sort_by: Some(SortColumn::Volume),
            sort_order: Some(SortOrder::Desc),
            ..Filter::default()
        };
        let body = r#"{"sort_fixture":"volume_desc"}"#.to_owned();
        *state.stock_movers_cache().slot.lock().await = Some((
            Instant::now(),
            filter.clone(),
            body.clone(),
            chrono::Utc::now().timestamp() + 60,
            tickvault_common::market_hours::is_trading_session_now(),
        ));
        let hit = get_stock_movers(State(state.clone()), Query(filter.clone())).await;
        assert_eq!(hit.headers()["x-tv-cache"], "hit");
        assert_eq!(
            axum::body::to_bytes(hit.into_body(), 1000)
                .await
                .unwrap()
                .as_ref(),
            body.as_bytes()
        );
        let changed = Filter {
            sort_order: Some(SortOrder::Asc),
            ..filter
        };
        let miss = get_stock_movers(State(state), Query(changed)).await;
        assert_eq!(miss.headers()["x-tv-cache"], "miss");
        assert_ne!(
            axum::body::to_bytes(miss.into_body(), MAX_WIRE_BYTES)
                .await
                .unwrap()
                .as_ref(),
            body.as_bytes()
        );
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
        let closed = snapshot(run(&f.0, "http://127.0.0.1:1", false, Filter::default()).await);
        assert_eq!(closed.status, SourceStatus::Closed);
        assert!(closed.rows.is_empty());
        assert!(closed.universe_available);
        assert_eq!(closed.universe_total, 4);
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
    async fn api_cache_separates_filters_and_retains_original_clock() {
        let state = fno_stocks::tests::mock_state(1);
        let f = Filter::default();
        let body = r#"{"original_capture":1800000000}"#.to_owned();
        *state.stock_movers_cache().slot.lock().await = Some((
            Instant::now(),
            f.clone(),
            body.clone(),
            chrono::Utc::now().timestamp() + 60,
            tickvault_common::market_hours::is_trading_session_now(),
        ));
        let response = get_stock_movers(State(state.clone()), Query(f)).await;
        assert_eq!(response.headers()["x-tv-cache"], "hit");
        let bytes = axum::body::to_bytes(response.into_body(), 1000)
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), body.as_bytes());
        let response = get_stock_movers(
            State(state),
            Query(Filter {
                fno_only: false,
                ..Filter::default()
            }),
        )
        .await;
        assert_eq!(response.headers()["x-tv-cache"], "miss");
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
    async fn movers_routes_refuse_disabled_or_empty_auth_even_during_paper_boot() {
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
    async fn enabled_movers_auth_distinguishes_missing_wrong_and_correct_tokens() {
        use axum::{
            body::Body,
            http::{Request, StatusCode},
        };
        use secrecy::SecretString;
        use tower::ServiceExt;
        let state = fno_stocks::tests::mock_state(1);
        state.fno_stocks_cache().put("{}".into());
        *state.stock_movers_cache().slot.lock().await = Some((
            Instant::now(),
            Filter::default(),
            "{}".into(),
            chrono::Utc::now().timestamp() + 60,
            tickvault_common::market_hours::is_trading_session_now(),
        ));
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
}

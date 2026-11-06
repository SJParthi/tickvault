//! The Stocks movers UNIVERSE and the pure per-filter selection over it
//! (plan PR MV, 2026-11-06 live incident).
//!
//! Measured live on 2026-11-06: a portal Stocks movers click took 2.6 to 8.0 s
//! because every filter, sort or page change went portal → Lambda → SSM →
//! box helper → `/api/stock-movers` → QuestDB, although the database part
//! was 0.1 to 0.4 s. A universe is ONE observation of every price row the
//! view can show (the union of the day's F&O list and the index-membership
//! list), plus both lists. [`select`] turns it into exactly the reply the
//! per-filter path returned for that filter from the same rows and times, so
//! the app answers every filter from one cached universe and the operator
//! Lambda answers from a published copy without a round trip to the box.
//!
//! Shared transport, dependency-light: the operator Lambda includes this
//! source file directly (`#[path]`), as it does `operator_movers_wire.rs`, so
//! nothing here may name `crate::constants`. The constants below are pinned
//! against the common constants by `crates/common/tests/operator_movers_universe_constants.rs`.
//!
//! # Complexity
//!
//! Cold path, never per tick. [`select`] is O(n log n) in the rows it keeps
//! (n ≤ [`MAX_UNIVERSE_PRICE_ROWS`]): one pass to build the lookup maps from
//! the stored lists (O(list rows)), one pass over the price rows, one sort of
//! the kept rows, one optional sort by the requested column. [`MoversUniverse::valid`]
//! is O(rows + list rows).
//!
//! # Honest limits
//!
//! - A universe read from JSON carries the same floats the box held only up
//!   to `serde_json`'s default float parsing, which can differ from the
//!   printed value by one unit in the last place for long mantissas. Prices
//!   in paise round-trip exactly.
//! - The universe reads the union in ONE query. A database answer with more
//!   rows than the bound the API sets for it is refused for every filter,
//!   where the per-filter path refused it only for the filters whose own
//!   answer was too long. With one row per stock (the query's `LATEST ON`)
//!   this cannot happen on a real answer.

use crate::operator_movers_wire::{
    BridgeError, MAX_SOURCE_ROWS, MAX_WIRE_BYTES, SELECTION_LIMIT, SourceStatus, Stock,
};
use crate::operator_stock_movers_wire::{
    Direction, FILTERED_PROTOCOL_VERSION, Filter, MAX_INDEX_NAME_BYTES, MAX_INDEX_NAMES,
    MemberStock, MembershipStatus, Outcome, Reply, Snapshot,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Version of the universe format. A reader refuses any other value.
pub const UNIVERSE_FORMAT_VERSION: u32 = 1;
/// Largest universe object a reader accepts, in bytes.
pub const MAX_UNIVERSE_BYTES: usize = 4 * 1024 * 1024;
/// Largest number of price rows a universe may carry (the union of two lists
/// of at most [`MAX_SOURCE_ROWS`] stocks each).
pub const MAX_UNIVERSE_PRICE_ROWS: usize = 2 * MAX_SOURCE_ROWS;
/// Largest number of rows in one stored list (the mapping artifact's own cap).
pub const MAX_UNIVERSE_LIST_ROWS: usize = 32_768;
/// Dhan's `NSE_EQ` segment code. Pinned to `constants::EXCHANGE_SEGMENT_NSE_EQ`.
pub const UNIVERSE_NSE_EQ_SEGMENT_CODE: u8 = 1;
/// The text of the `NSE_EQ` segment on the wire.
pub const UNIVERSE_NSE_EQ_SEGMENT: &str = "NSE_EQ";
/// IST minus UTC, seconds. Pinned to `constants::IST_UTC_OFFSET_SECONDS_I64`.
pub const UNIVERSE_IST_OFFSET_SECS: i64 = 19_800;
/// 09:00 IST, seconds of day. Pinned to `constants::TICK_PERSIST_START_SECS_OF_DAY_IST`.
pub const UNIVERSE_SESSION_START_SECS_OF_DAY_IST: i64 = 32_400;
/// 15:40 IST, seconds of day. Pinned to `constants::TICK_PERSIST_END_SECS_OF_DAY_IST`.
pub const UNIVERSE_SESSION_END_SECS_OF_DAY_IST: i64 = 56_400;
/// Newest row at most this old reads `live` (the API's `FNO_STOCKS_LIVE_MAX_AGE_SECS`).
pub const UNIVERSE_LIVE_MAX_AGE_SECS: i64 = 60;
/// Window reported by an answer with no rows (the API's `FNO_STOCKS_WINDOW_SECS`).
pub const UNIVERSE_DEFAULT_WINDOW_SECS: i64 = 900;
/// A published universe older than this, while the session was open when it
/// was built, is not used (the reader falls back to asking the box).
pub const UNIVERSE_MAX_AGE_OPEN_SECS: i64 = 20;
/// The same bound when the session was closed when it was built. The reply's
/// own 60 s capture bound still applies after [`select`].
pub const UNIVERSE_MAX_AGE_CLOSED_SECS: i64 = 300;
/// A universe stamped this far ahead of the reader's clock is refused.
pub const UNIVERSE_MAX_FUTURE_SKEW_SECS: i64 = 5;
/// Object key of the published universe inside the live-view bucket.
pub const MOVERS_UNIVERSE_OBJECT_KEY: &str = "movers/universe.json";
/// The Stocks view never shows the turnover estimate as actual value.
pub const ACTUAL_VALUE_UNAVAILABLE: &str =
    "Actual traded value is not captured; Value (Cr.) is unavailable.";

const SECONDS_PER_DAY: i64 = 86_400;
/// Bound on one stored list name; a name the view cannot show (over 128
/// bytes) is still carried so `select` refuses it exactly as the API did.
const MAX_LIST_NAME_BYTES: usize = 1_024;

/// The live-view bucket of one environment, `tv-<env>-live-view`.
#[must_use]
pub fn live_view_bucket_for(environment: &str) -> String {
    format!("tv-{environment}-live-view")
}

/// One stock on a stored list, keyed on the composite `(security_id, segment)`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ListedStock {
    pub security_id: u64,
    pub segment_code: u8,
    pub name: String,
}

/// The index memberships of one stock (positions in [`UniverseMembership::indices`]).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StockIndices {
    pub security_id: u64,
    pub segment_code: u8,
    pub indices: Vec<usize>,
}

/// The day's index-membership mapping, as the API read it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UniverseMembership {
    pub names: Vec<ListedStock>,
    /// Index labels, strictly ascending.
    pub indices: Vec<String>,
    pub by_stock: Vec<StockIndices>,
    /// An index was unresolved or a known index is missing.
    pub partial: bool,
}

/// One accepted price row: finite positive last price, inside the read window.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PriceRow {
    pub security_id: u64,
    pub segment_code: u8,
    pub ltp: f64,
    /// Previous session's close, only when finite and positive.
    pub prev_close: Option<f64>,
    /// Day's cumulative volume as stored (zero kept; the view drops it).
    pub volume: Option<u64>,
    /// Exchange time of the row, IST seconds since the epoch (naive IST).
    pub ts_ist_secs: i64,
}

/// What the price query gave.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum UniversePrices {
    /// The accepted rows, in the order the database returned them.
    Rows { rows: Vec<PriceRow> },
    /// Closed and before the day's 09:15: nothing to read, no query ran.
    BeforeSession,
    /// The database did not answer.
    Unreachable,
    /// The database answered with an error or a malformed body.
    QueryFailed,
}

/// One observation of the Stocks movers view.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MoversUniverse {
    pub format_version: u32,
    /// IST date of the read instant, `YYYY-MM-DD`; the lists are that day's.
    pub trading_date_ist: String,
    /// UTC epoch seconds of the observation (the reply's capture time).
    pub observed_at_epoch_secs: i64,
    /// The read instant (now, or a chosen past minute), naive IST seconds.
    pub read_at_ist_secs: i64,
    /// Session open AND a live read (not a chosen minute).
    pub market_open: bool,
    /// Window the query read, seconds.
    pub window_secs: i64,
    pub prices: UniversePrices,
    /// Today's F&O list; `None` when missing or unreadable.
    pub fno: Option<Vec<ListedStock>>,
    /// Today's index memberships; `None` when missing or unreadable.
    pub membership: Option<UniverseMembership>,
}

/// Why a published universe is not used by a reader.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UniverseRefusal {
    /// Wrong format version or a field outside its bounds.
    Invalid,
    /// Built for a different IST date than the reader's.
    OtherDay,
    /// Older than [`UNIVERSE_MAX_AGE_OPEN_SECS`] / [`UNIVERSE_MAX_AGE_CLOSED_SECS`].
    Stale,
    /// Stamped more than [`UNIVERSE_MAX_FUTURE_SKEW_SECS`] ahead of the reader.
    Future,
}

impl UniverseRefusal {
    /// Stable label for logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::OtherDay => "other_day",
            Self::Stale => "stale",
            Self::Future => "future",
        }
    }
}

/// IST date of a UTC epoch second, `YYYY-MM-DD`.
#[must_use]
pub fn ist_date_of_epoch(epoch_secs: i64) -> Option<String> {
    let local = epoch_secs.checked_add(UNIVERSE_IST_OFFSET_SECS)?;
    chrono::DateTime::from_timestamp(local, 0).map(|t| t.format("%Y-%m-%d").to_string())
}

fn valid_date_shape(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes.iter().enumerate().all(|(i, b)| match i {
            4 | 7 => *b == b'-',
            _ => b.is_ascii_digit(),
        })
}

fn valid_list(list: &[ListedStock]) -> bool {
    let mut keys = HashSet::with_capacity(list.len());
    list.len() <= MAX_UNIVERSE_LIST_ROWS
        && list.iter().all(|s| {
            s.security_id != 0
                && s.segment_code == UNIVERSE_NSE_EQ_SEGMENT_CODE
                && s.name.len() <= MAX_LIST_NAME_BYTES
                && keys.insert((s.security_id, s.segment_code))
        })
}

impl MoversUniverse {
    /// Shape and bounds check a reader runs before [`select`]. O(rows + list rows).
    #[must_use]
    pub fn valid(&self) -> bool {
        let prices_valid = match &self.prices {
            UniversePrices::Rows { rows } => {
                rows.len() <= MAX_UNIVERSE_PRICE_ROWS
                    && rows.iter().all(|r| {
                        r.ltp.is_finite()
                            && r.ltp > 0.0
                            && r.prev_close.is_none_or(|p| p.is_finite() && p > 0.0)
                    })
            }
            _ => true,
        };
        let membership_valid = self.membership.as_ref().is_none_or(|m| {
            let mut keys = HashSet::with_capacity(m.by_stock.len());
            valid_list(&m.names)
                && m.indices.len() <= MAX_INDEX_NAMES
                && m.indices.iter().all(|s| {
                    !s.is_empty()
                        && s.len() <= MAX_INDEX_NAME_BYTES
                        && !s.chars().any(char::is_control)
                })
                && m.indices.windows(2).all(|w| w[0] < w[1])
                && m.by_stock.len() <= MAX_UNIVERSE_LIST_ROWS
                && m.by_stock.iter().all(|s| {
                    s.segment_code == UNIVERSE_NSE_EQ_SEGMENT_CODE
                        && keys.insert((s.security_id, s.segment_code))
                        && s.indices.len() <= m.indices.len()
                        && s.indices.iter().all(|i| *i < m.indices.len())
                        && s.indices.windows(2).all(|w| w[0] < w[1])
                })
        });
        self.format_version == UNIVERSE_FORMAT_VERSION
            && valid_date_shape(&self.trading_date_ist)
            && self.window_secs > 0
            && self.window_secs <= SECONDS_PER_DAY
            && prices_valid
            && self.fno.as_deref().is_none_or(valid_list)
            && membership_valid
    }

    /// Whether a reader at `now_epoch` (UTC seconds) may answer from this
    /// universe. O(rows) for the shape check, O(1) otherwise.
    ///
    /// # Errors
    ///
    /// The [`UniverseRefusal`] naming why it may not.
    pub fn usable_at(&self, now_epoch: i64) -> Result<(), UniverseRefusal> {
        if !self.valid() {
            return Err(UniverseRefusal::Invalid);
        }
        if ist_date_of_epoch(now_epoch).as_deref() != Some(self.trading_date_ist.as_str()) {
            return Err(UniverseRefusal::OtherDay);
        }
        let age = now_epoch.saturating_sub(self.observed_at_epoch_secs);
        if age < -UNIVERSE_MAX_FUTURE_SKEW_SECS {
            return Err(UniverseRefusal::Future);
        }
        let limit = if self.market_open {
            UNIVERSE_MAX_AGE_OPEN_SECS
        } else {
            UNIVERSE_MAX_AGE_CLOSED_SECS
        };
        if age > limit {
            return Err(UniverseRefusal::Stale);
        }
        Ok(())
    }
}

/// Reply expiry: one minute after the observation, or the next 09:00, 15:40
/// or IST midnight, whichever comes first. `None` on overflow.
#[must_use]
pub fn snapshot_deadline(observed: i64) -> Option<i64> {
    let local = observed.checked_add(UNIVERSE_IST_OFFSET_SECS)?;
    let midnight = local
        .div_euclid(SECONDS_PER_DAY)
        .checked_mul(SECONDS_PER_DAY)?
        .checked_sub(UNIVERSE_IST_OFFSET_SECS)?;
    let mut deadline = observed.checked_add(60)?;
    for seconds in [
        UNIVERSE_SESSION_START_SECS_OF_DAY_IST,
        UNIVERSE_SESSION_END_SECS_OF_DAY_IST,
        SECONDS_PER_DAY,
    ] {
        let boundary = midnight.checked_add(seconds)?;
        if boundary > observed {
            deadline = deadline.min(boundary);
        }
    }
    Some(deadline)
}

fn hh_mm_ss(ist_secs: i64) -> String {
    let of_day = ist_secs.rem_euclid(SECONDS_PER_DAY);
    format!(
        "{:02}:{:02}:{:02}",
        of_day / 3_600,
        (of_day / 60) % 60,
        of_day % 60
    )
}

fn change_against(ltp: f64, prev_close: Option<f64>) -> (Option<f64>, Option<f64>) {
    match prev_close {
        Some(prev) if prev.is_finite() && prev > 0.0 && ltp.is_finite() && ltp > 0.0 => {
            let change = ltp - prev;
            (Some(change), Some(change / prev * 100.0))
        }
        _ => (None, None),
    }
}

type Key = (u64, u8);

fn name_map(list: &[ListedStock]) -> HashMap<Key, &str> {
    list.iter()
        .map(|s| ((s.security_id, s.segment_code), s.name.as_str()))
        .collect()
}

struct MembershipView<'a> {
    names: HashMap<Key, &'a str>,
    indices: &'a [String],
    by_stock: HashMap<Key, &'a [usize]>,
    partial: bool,
}

impl<'a> MembershipView<'a> {
    fn new(m: &'a UniverseMembership) -> Self {
        Self {
            names: name_map(&m.names),
            indices: &m.indices,
            by_stock: m
                .by_stock
                .iter()
                .map(|s| ((s.security_id, s.segment_code), s.indices.as_slice()))
                .collect(),
            partial: m.partial,
        }
    }
}

struct PricedStock {
    security_id: u64,
    name: String,
    ltp: f64,
    prev_close: Option<f64>,
    change: Option<f64>,
    change_pct: Option<f64>,
    volume: Option<u64>,
    last_trade_ist: String,
}

struct PriceView {
    status: SourceStatus,
    market_open: bool,
    observed: i64,
    as_of_ist: Option<String>,
    data_age_secs: Option<i64>,
    total: usize,
    stocks_priced: usize,
    window_secs: i64,
    stocks: Vec<PricedStock>,
}

impl PriceView {
    fn empty(status: SourceStatus, market_open: bool, total: usize, observed: i64) -> Self {
        Self {
            status,
            market_open,
            observed,
            as_of_ist: None,
            data_age_secs: None,
            total,
            stocks_priced: 0,
            window_secs: UNIVERSE_DEFAULT_WINDOW_SECS,
            stocks: Vec::new(),
        }
    }
}

/// The price engine over the selected names (the API's `compute_for_list`).
fn price_view(universe: &MoversUniverse, names: &HashMap<Key, &str>) -> PriceView {
    let total = names.len();
    let open = universe.market_open;
    let now = universe.observed_at_epoch_secs;
    let rows = match &universe.prices {
        UniversePrices::BeforeSession => {
            return PriceView::empty(SourceStatus::Closed, false, total, now);
        }
        UniversePrices::Unreachable => {
            return PriceView::empty(SourceStatus::Unreachable, open, total, now);
        }
        UniversePrices::QueryFailed => {
            return PriceView::empty(SourceStatus::QueryFailed, open, total, now);
        }
        UniversePrices::Rows { rows } => rows,
    };
    let mut newest: Option<i64> = None;
    let mut stocks = Vec::with_capacity(rows.len().min(total));
    for row in rows {
        let Some(name) = names.get(&(row.security_id, row.segment_code)) else {
            continue;
        };
        if !row.ltp.is_finite() || row.ltp <= 0.0 {
            continue;
        }
        newest = Some(newest.map_or(row.ts_ist_secs, |n| n.max(row.ts_ist_secs)));
        let prev_close = row.prev_close.filter(|p| p.is_finite() && *p > 0.0);
        let (change, change_pct) = change_against(row.ltp, prev_close);
        stocks.push(PricedStock {
            security_id: row.security_id,
            name: (*name).to_string(),
            ltp: row.ltp,
            prev_close,
            change,
            change_pct,
            volume: row.volume,
            last_trade_ist: hh_mm_ss(row.ts_ist_secs),
        });
    }
    // O(k log k): change % descending, no change last, name breaks ties;
    // stable, so equal keys keep the database order.
    stocks.sort_by(|a, b| match (a.change_pct, b.change_pct) {
        (Some(x), Some(y)) => y.total_cmp(&x).then_with(|| a.name.cmp(&b.name)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.name.cmp(&b.name),
    });
    let Some(newest) = newest.filter(|_| !stocks.is_empty()) else {
        let status = if open {
            SourceStatus::NoData
        } else {
            SourceStatus::Closed
        };
        return PriceView::empty(status, open, total, now);
    };
    let age = universe.read_at_ist_secs.saturating_sub(newest).max(0);
    PriceView {
        status: if !open {
            SourceStatus::Closed
        } else if age <= UNIVERSE_LIVE_MAX_AGE_SECS {
            SourceStatus::Live
        } else {
            SourceStatus::Stale
        },
        market_open: open,
        observed: now,
        as_of_ist: Some(hh_mm_ss(newest)),
        data_age_secs: Some(age),
        total,
        stocks_priced: stocks.len(),
        window_secs: universe.window_secs,
        stocks,
    }
}

fn finish(
    prices: PriceView,
    filter: &Filter,
    membership: Option<&MembershipView<'_>>,
    fno: Option<&HashMap<Key, &str>>,
) -> Reply {
    let observed = prices.observed;
    let index_names = membership.map_or_else(Vec::new, |m| m.indices.to_vec());
    let membership_status = match membership {
        None => MembershipStatus::Unavailable,
        Some(m) if m.partial => MembershipStatus::Partial,
        Some(_) => MembershipStatus::Available,
    };
    let mut ids = HashSet::with_capacity(prices.stocks.len());
    let mut selected = Vec::with_capacity(prices.stocks.len());
    for row in prices.stocks {
        let price = Stock {
            security_id: row.security_id,
            segment: UNIVERSE_NSE_EQ_SEGMENT.into(),
            name: row.name,
            ltp: row.ltp,
            prev_close: row.prev_close,
            change: row.change,
            change_pct: row.change_pct,
            // A zero cumulative cannot prove a reported zero; unavailable.
            volume: row.volume.filter(|volume| *volume > 0),
            // The captured value is an estimate; never shown as actual value.
            value_cr: None,
            last_trade_ist: row.last_trade_ist,
        };
        if !price.valid() || !ids.insert(price.security_id) {
            return Reply::error(BridgeError::InvalidResponse, observed);
        }
        if !filter.matches_price(&price) {
            continue;
        }
        let key = (price.security_id, UNIVERSE_NSE_EQ_SEGMENT_CODE);
        let indices = membership
            .and_then(|m| m.by_stock.get(&key))
            .map(|i| i.to_vec())
            .unwrap_or_default();
        let eligible = fno.map(|f| f.contains_key(&key));
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
        status: prices.status,
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
        universe_total: prices.total,
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
    let Some(valid_until_epoch_secs) = snapshot_deadline(observed) else {
        return Reply::error(BridgeError::InvalidResponse, observed);
    };
    if !snapshot.valid() {
        return Reply::error(BridgeError::InvalidResponse, observed);
    }
    let reply = Reply {
        valid_until_epoch_secs,
        version: FILTERED_PROTOCOL_VERSION,
        captured_at_epoch_secs: observed,
        outcome: Outcome::Snapshot {
            snapshot: Box::new(snapshot),
        },
    };
    if serde_json::to_vec(&reply).map_or(true, |bytes| bytes.len() > MAX_WIRE_BYTES) {
        return Reply::error(BridgeError::ResponseTooLarge, observed);
    }
    reply
}

/// The filtered, paged movers reply for `filter` from one universe: exactly
/// what the per-filter API path returned for the same rows, lists and times
/// (pinned by the API's property test against that path). Pure; O(n log n),
/// n ≤ [`MAX_UNIVERSE_PRICE_ROWS`]. Never panics; every failure is a typed
/// reply.
#[must_use]
pub fn select(universe: &MoversUniverse, filter: &Filter) -> Reply {
    let now = universe.observed_at_epoch_secs;
    let open = universe.market_open;
    let fno = universe.fno.as_deref().map(name_map);
    let membership = universe.membership.as_ref().map(MembershipView::new);
    let closed_or = |status| {
        if open { status } else { SourceStatus::Closed }
    };
    let finish_empty = |status| {
        finish(
            PriceView::empty(status, open, 0, now),
            filter,
            membership.as_ref(),
            fno.as_ref(),
        )
    };
    if filter.fno_only && fno.is_none() {
        return finish_empty(closed_or(SourceStatus::NoFnoList));
    }
    if (!filter.fno_only || filter.index.is_some()) && membership.is_none() {
        return finish_empty(closed_or(SourceStatus::NoIndexList));
    }
    let names = if filter.fno_only {
        fno.as_ref()
    } else {
        membership.as_ref().map(|m| &m.names)
    };
    let Some(names) = names else {
        return finish_empty(closed_or(SourceStatus::NoIndexList));
    };
    let selected_index = filter
        .index
        .as_ref()
        .and_then(|name| membership.as_ref()?.indices.binary_search(name).ok());
    if filter.index.is_some() && selected_index.is_none() {
        return finish_empty(closed_or(SourceStatus::NoIndexList));
    }
    let names: HashMap<Key, &str> = names
        .iter()
        .filter(|(key, _)| {
            selected_index.is_none_or(|index| {
                membership
                    .as_ref()
                    .and_then(|m| m.by_stock.get(*key))
                    .is_some_and(|indices| indices.binary_search(&index).is_ok())
            })
        })
        .map(|(key, name)| (*key, *name))
        .collect();
    if names.len() > MAX_SOURCE_ROWS {
        return Reply::error(BridgeError::ResponseTooLarge, now);
    }
    if names.is_empty() {
        return finish_empty(closed_or(SourceStatus::NoData));
    }
    finish(
        price_view(universe, &names),
        filter,
        membership.as_ref(),
        fno.as_ref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operator_stock_movers_wire::{SortColumn, SortOrder};

    const OBSERVED: i64 = 1_793_252_730; // 2026-10-29 11:15:30 IST
    fn ist(observed: i64) -> i64 {
        observed + UNIVERSE_IST_OFFSET_SECS
    }
    fn listed(id: u64) -> ListedStock {
        ListedStock {
            security_id: id,
            segment_code: UNIVERSE_NSE_EQ_SEGMENT_CODE,
            name: format!("STOCK{id:03}"),
        }
    }
    fn universe(rows: Vec<PriceRow>) -> MoversUniverse {
        MoversUniverse {
            format_version: UNIVERSE_FORMAT_VERSION,
            trading_date_ist: ist_date_of_epoch(OBSERVED).unwrap(),
            observed_at_epoch_secs: OBSERVED,
            read_at_ist_secs: ist(OBSERVED),
            market_open: true,
            window_secs: UNIVERSE_DEFAULT_WINDOW_SECS,
            prices: UniversePrices::Rows { rows },
            fno: Some((1..=4).map(|id| listed(id * 2)).collect()),
            membership: Some(UniverseMembership {
                names: (1..=8).map(listed).collect(),
                indices: vec!["Nifty 50".into(), "Nifty Bank".into()],
                by_stock: (1..=8)
                    .map(|id| StockIndices {
                        security_id: id,
                        segment_code: UNIVERSE_NSE_EQ_SEGMENT_CODE,
                        indices: if id % 2 == 0 { vec![0, 1] } else { vec![0] },
                    })
                    .collect(),
                partial: false,
            }),
        }
    }
    fn row(id: u64, ltp: f64, age: i64) -> PriceRow {
        PriceRow {
            security_id: id,
            segment_code: UNIVERSE_NSE_EQ_SEGMENT_CODE,
            ltp,
            prev_close: Some(100.0),
            volume: Some(1_000 + id),
            ts_ist_secs: ist(OBSERVED) - age,
        }
    }
    fn snapshot(reply: Reply) -> Snapshot {
        match reply.outcome {
            Outcome::Snapshot { snapshot } => *snapshot,
            Outcome::Error { code } => panic!("expected snapshot, got {code:?}"),
        }
    }

    #[test]
    fn test_select_filters_sorts_and_pages_one_universe_for_every_filter() {
        let u = universe((1..=8).map(|id| row(id, 100.0 + id as f64, 5)).collect());
        assert!(u.valid());
        let fno = snapshot(select(&u, &Filter::default()));
        assert_eq!(fno.status, SourceStatus::Live);
        assert_eq!((fno.universe_total, fno.stocks_priced), (4, 4));
        assert_eq!(fno.rows[0].price.security_id, 8);
        assert!(fno.rows.iter().all(|r| r.fno == Some(true)));
        let all = snapshot(select(
            &u,
            &Filter {
                fno_only: false,
                sort_by: Some(SortColumn::Name),
                sort_order: Some(SortOrder::Asc),
                ..Filter::default()
            },
        ));
        assert_eq!(all.matching_total, 8);
        assert_eq!(all.rows[0].price.name, "STOCK001");
        assert_eq!(all.rows[0].fno, Some(false));
        let bank = snapshot(select(
            &u,
            &Filter {
                fno_only: false,
                index: Some("Nifty Bank".into()),
                ..Filter::default()
            },
        ));
        assert_eq!(bank.universe_total, 4);
        let losers = snapshot(select(
            &u,
            &Filter {
                direction: Direction::Losers,
                ..Filter::default()
            },
        ));
        assert!(losers.rows.is_empty(), "every change is positive");
        let reply = select(&u, &Filter::default());
        assert!(reply.valid_at(OBSERVED, &Filter::default()));
    }

    #[test]
    fn test_select_reports_missing_lists_unknown_index_and_failed_query_distinctly() {
        let mut u = universe(vec![row(2, 101.0, 5)]);
        u.fno = None;
        assert_eq!(
            snapshot(select(&u, &Filter::default())).status,
            SourceStatus::NoFnoList
        );
        let mut u = universe(vec![row(2, 101.0, 5)]);
        u.membership = None;
        let no_index = Filter {
            fno_only: false,
            ..Filter::default()
        };
        assert_eq!(
            snapshot(select(&u, &no_index)).status,
            SourceStatus::NoIndexList
        );
        let u = universe(vec![row(2, 101.0, 5)]);
        let unknown = Filter {
            index: Some("Not an index".into()),
            ..Filter::default()
        };
        let s = snapshot(select(&u, &unknown));
        assert_eq!(s.status, SourceStatus::NoIndexList);
        assert!(!s.universe_available);
        for (prices, status) in [
            (UniversePrices::Unreachable, SourceStatus::Unreachable),
            (UniversePrices::QueryFailed, SourceStatus::QueryFailed),
        ] {
            let mut u = universe(vec![]);
            u.prices = prices;
            assert_eq!(snapshot(select(&u, &Filter::default())).status, status);
        }
        let mut u = universe(vec![]);
        u.prices = UniversePrices::BeforeSession;
        u.market_open = false;
        let s = snapshot(select(&u, &Filter::default()));
        assert_eq!((s.status, s.market_open), (SourceStatus::Closed, false));
        assert_eq!(
            snapshot(select(&universe(vec![]), &Filter::default())).status,
            SourceStatus::NoData
        );
    }

    #[test]
    fn test_select_ages_stale_rows_and_refuses_duplicate_identity() {
        let u = universe(vec![row(2, 101.0, 330)]);
        let s = snapshot(select(&u, &Filter::default()));
        assert_eq!(
            (s.status, s.data_age_secs),
            (SourceStatus::Stale, Some(330))
        );
        let u = universe(vec![row(2, 101.0, -3)]);
        assert_eq!(
            snapshot(select(&u, &Filter::default())).data_age_secs,
            Some(0)
        );
        let u = universe(vec![row(2, 101.0, 5), row(2, 102.0, 5)]);
        assert_eq!(
            select(&u, &Filter::default()).outcome,
            Outcome::Error {
                code: BridgeError::InvalidResponse
            }
        );
    }

    #[test]
    fn test_select_drops_zero_volume_and_never_shows_estimated_value() {
        let mut zero = row(2, 101.0, 5);
        zero.volume = Some(0);
        let mut no_close = row(4, 99.0, 5);
        no_close.prev_close = None;
        let s = snapshot(select(&universe(vec![zero, no_close]), &Filter::default()));
        let by_id = |id| s.rows.iter().find(|r| r.price.security_id == id).unwrap();
        assert!(by_id(2).price.volume.is_none() && by_id(2).volume_text.is_none());
        assert!(by_id(4).price.change.is_none() && by_id(4).price.change_pct.is_none());
        assert!(s.rows.iter().all(|r| r.price.value_cr.is_none()));
        assert_eq!(s.value_basis, ACTUAL_VALUE_UNAVAILABLE);
        assert_eq!(by_id(2).price.last_trade_ist, "11:15:25");
    }

    #[test]
    fn test_universe_round_trips_through_json_and_selects_identically() {
        let u = universe((1..=8).map(|id| row(id, 100.25 + id as f64, 5)).collect());
        let text = serde_json::to_string(&u).unwrap();
        let back: MoversUniverse = serde_json::from_str(&text).unwrap();
        assert_eq!(back, u);
        assert_eq!(
            select(&back, &Filter::default()),
            select(&u, &Filter::default())
        );
        assert!(
            serde_json::from_str::<MoversUniverse>(&text.replace("\"format_version\"", "\"x\""))
                .is_err()
        );
    }

    #[test]
    fn test_valid_refuses_out_of_bound_fields() {
        let good = universe(vec![row(2, 101.0, 5)]);
        assert!(good.valid());
        let mutations: [fn(&mut MoversUniverse); 9] = [
            |u| u.format_version = 2,
            |u| u.trading_date_ist = "2026-1-29".into(),
            |u| u.window_secs = 0,
            |u| {
                u.prices = UniversePrices::Rows {
                    rows: vec![row(2, f64::NAN, 5)],
                }
            },
            |u| {
                u.prices = UniversePrices::Rows {
                    rows: vec![row(2, 0.0, 5)],
                }
            },
            |u| u.fno.as_mut().unwrap().push(listed(2)),
            |u| u.fno.as_mut().unwrap()[0].segment_code = 2,
            |u| u.membership.as_mut().unwrap().indices.reverse(),
            |u| u.membership.as_mut().unwrap().by_stock[0].indices = vec![5],
        ];
        for change in mutations {
            let mut bad = good.clone();
            change(&mut bad);
            assert!(!bad.valid(), "{bad:?}");
            assert_eq!(bad.usable_at(OBSERVED), Err(UniverseRefusal::Invalid));
        }
    }

    #[test]
    fn test_usable_at_refuses_other_day_stale_and_future_universes() {
        let mut u = universe(vec![]);
        assert_eq!(u.usable_at(OBSERVED), Ok(()));
        assert_eq!(u.usable_at(OBSERVED + UNIVERSE_MAX_AGE_OPEN_SECS), Ok(()));
        assert_eq!(
            u.usable_at(OBSERVED + UNIVERSE_MAX_AGE_OPEN_SECS + 1),
            Err(UniverseRefusal::Stale)
        );
        assert_eq!(
            u.usable_at(OBSERVED - UNIVERSE_MAX_FUTURE_SKEW_SECS - 1),
            Err(UniverseRefusal::Future)
        );
        u.market_open = false;
        assert_eq!(u.usable_at(OBSERVED + UNIVERSE_MAX_AGE_CLOSED_SECS), Ok(()));
        assert_eq!(
            u.usable_at(OBSERVED + UNIVERSE_MAX_AGE_CLOSED_SECS + 1),
            Err(UniverseRefusal::Stale)
        );
        assert_eq!(
            u.usable_at(OBSERVED + SECONDS_PER_DAY),
            Err(UniverseRefusal::OtherDay)
        );
        for refusal in [
            UniverseRefusal::Invalid,
            UniverseRefusal::OtherDay,
            UniverseRefusal::Stale,
            UniverseRefusal::Future,
        ] {
            assert!(!refusal.as_str().is_empty());
        }
    }

    #[test]
    fn test_ist_date_of_epoch_uses_the_ist_calendar_day() {
        assert_eq!(ist_date_of_epoch(OBSERVED).as_deref(), Some("2026-10-29"));
        // 23:59:59 IST and the next IST midnight fall on different dates.
        let midnight = OBSERVED + (86_400 - 40_530);
        assert_eq!(
            ist_date_of_epoch(midnight - 1).as_deref(),
            Some("2026-10-29")
        );
        assert_eq!(ist_date_of_epoch(midnight).as_deref(), Some("2026-10-30"));
        assert_eq!(ist_date_of_epoch(i64::MAX), None);
        assert_eq!(hh_mm_ss(-1), "23:59:59");
    }

    #[test]
    fn test_live_view_bucket_for_names_the_environment_bucket() {
        assert_eq!(live_view_bucket_for("prod"), "tv-prod-live-view");
    }

    #[test]
    fn test_snapshot_deadline_stops_at_session_and_day_boundaries() {
        assert_eq!(snapshot_deadline(OBSERVED), Some(OBSERVED + 60));
        let midnight = OBSERVED + (86_400 - 40_530);
        assert_eq!(snapshot_deadline(midnight - 10), Some(midnight));
        let session_end = midnight - 86_400 + UNIVERSE_SESSION_END_SECS_OF_DAY_IST;
        assert_eq!(snapshot_deadline(session_end - 1), Some(session_end));
        assert!(snapshot_deadline(i64::MAX).is_none());
    }
}

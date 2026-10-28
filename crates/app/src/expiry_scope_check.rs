//! The expiry-scope guarantee (owner request 2026-10-28: "go ahead and add the
//! daily expiry check"): only the CURRENT expiry of each underlying is ever
//! subscribed, and every trading day proves it from the stored data.
//!
//! ## Two layers
//!
//! | Layer | When | What | On a violation |
//! |---|---|---|---|
//! | subscribe time | every contract attach attempt, in RAM, before the selection reaches the wire | every selected option's expiry must be on or after today AND equal to the nearest real expiry of its underlying, recomputed here from the day's contract file, independently of the selector | the contract is REFUSED (dropped from the selection), counted, one coded `error!` (`EXPIRY-SCOPE-01`) |
//! | daily | 15:50 IST on trading days, its own task | today's distinct `(security_id, segment, contract)` of `NSE_FNO` / `BSE_FNO` from `candles_1m`, each contract's expiry (stored name, or the contract file), judged against the same nearest-expiry table | one `expiry_scope_audit` row (`clean` / `violation` / `partial`), local metrics, one coded `error!` |
//!
//! ### Why refuse at subscribe time instead of only counting
//!
//! A contract on the wrong expiry is a wrong instrument that looks healthy:
//! its ticks arrive, its candles are written, and a strategy reading the
//! chain sees two expiries mixed. Dropping it costs one leg of one ladder; the
//! selector cannot produce one by construction, so a refusal here means the
//! selector, the contract file or the vendor changed, and the coded line says
//! which contract. Fail-closed is the safer error.
//!
//! ### "Nearest real expiry"
//!
//! The same rule the selector uses (`dhan_contract_universe::nearest_expiry`),
//! computed by a separate pass: per `(segment, underlying)`, over the option
//! rows of the day's contract file with expiry >= today, a usable strike, a
//! `CE`/`PE` leg and an underlying, the SMALLEST expiry carrying at least
//! `MIN_LEGS_FOR_A_REAL_EXPIRY` (4) legs. A thinner, earlier date is a stub
//! and is skipped. The expiry day itself counts as current (expiry >= today).
//!
//! ## Verdicts (daily)
//!
//! | verdict | reason | when |
//! |---|---|---|
//! | `partial` | `artifact_missing` | today's contract file could not be read |
//! | `partial` | `read_failed` | the `candles_1m` read failed after every retry |
//! | `violation` | `expired` / `off_expiry` / `expired_and_off_expiry` | at least one stored contract is expired or not on its underlying's current expiry (reported even when other rows could not be judged) |
//! | `partial` | `row_cap_hit` | the read returned more than [`EXPIRY_SCOPE_MAX_ROWS`] rows |
//! | `partial` | `no_rows` | nothing to check (a zero proves nothing) |
//! | `partial` | `unverifiable_rows` | some contract could not be judged (unknown underlying, or no usable name and no file row) |
//! | `partial` | `label_mismatch` | a stored name disagrees with the contract file |
//! | `partial` | `malformed_labels` | a stored name does not parse (the contract file still identified it) |
//! | `clean` | `all_current_expiry` | every stored contract was judged and every one is current |
//!
//! ## Complexity (honest)
//!
//! Subscribe time: **O(rows + subscribed)** per attach attempt — one pass over
//! the day's contract rows (~120,000) to count legs, then one O(1) probe per
//! selected contract. Cold, never on the frame drain. Daily: **O(rows +
//! contracts)** once a day on a blocking thread. Neither is O(1): "is every
//! subscribed contract current?" is a question about all of them, so it is
//! flagged here, never relabelled.
//!
//! ## Honest limits
//!
//! - The daily check reads `candles_1m`, so a contract that was subscribed
//!   and never traded leaves no candle and is not judged there; the
//!   subscribe-time layer covers every subscribed contract.
//! - It judges against TODAY's contract file. A contract the vendor delisted
//!   overnight but still delivered today reads `unverifiable_rows`, never
//!   `clean`.
//! - Delivery is log only: no CloudWatch filter, no EMF name, no alarm, no
//!   Telegram (`dhan-rest-only-noise-lock-2026-07-14.md` §3).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use chrono::NaiveDate;
use tracing::{debug, error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::{IST_UTC_OFFSET_NANOS, TICK_PERSIST_END_SECS_OF_DAY_IST};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_common::types::ExchangeSegment;
use tickvault_core::websocket::pool_supervisor::SubscribeInstrument;
use tickvault_storage::expiry_scope_audit_persistence::ExpiryScopeRow;
use tickvault_trading::candles::tf_index::TfIndex;

use crate::contract_underlying_map::{ContractKey, contract_label};
use crate::dhan_contract_universe::{ContractRow, ContractSelection, MIN_LEGS_FOR_A_REAL_EXPIRY};

const NANOS_PER_SEC: i64 = 1_000_000_000;
const NANOS_PER_DAY: i64 = 86_400 * NANOS_PER_SEC;

/// IST seconds-of-day of the daily check: 15:50:00, ten minutes after the
/// persist window closes, so the session's last 1-minute candles are sealed
/// and written (the cross-verification's own first read is 15:44:05).
pub const EXPIRY_SCOPE_CHECK_SECS_OF_DAY_IST: u32 = TICK_PERSIST_END_SECS_OF_DAY_IST + 600;

// After the close, before the 16:25 scheduled-stop window.
const _: () = assert!(EXPIRY_SCOPE_CHECK_SECS_OF_DAY_IST > TICK_PERSIST_END_SECS_OF_DAY_IST);
const _: () = assert!(EXPIRY_SCOPE_CHECK_SECS_OF_DAY_IST < 16 * 3600 + 25 * 60);

/// Most distinct rows one daily read takes. The main feed carries at most
/// 25,000 instruments, so a normal day is far below; more rows than this is
/// reported as `row_cap_hit`, never judged as complete.
pub const EXPIRY_SCOPE_MAX_ROWS: usize = 100_000;

/// Read attempts per day (a failed read is retried; a successful one is not).
pub const EXPIRY_SCOPE_MAX_READS: u32 = 3;

/// Seconds between read attempts.
pub const EXPIRY_SCOPE_READ_SPACING_SECS: u64 = 120;

/// Timeout on one read.
pub const EXPIRY_SCOPE_QUERY_TIMEOUT_SECS: u64 = 60;

/// Counter: contracts the subscribe-time check refused (per attempt).
pub const SUBSCRIBE_REFUSED_COUNTER: &str = "tv_expiry_scope_subscribe_refused_total";
/// Gauge: contracts the latest subscribe-time check judged.
pub const SUBSCRIBE_CHECKED_GAUGE: &str = "tv_expiry_scope_subscribe_checked";
/// Counter: daily runs by verdict.
pub const DAILY_RUNS_COUNTER: &str = "tv_expiry_scope_daily_runs_total";
/// Gauge: the latest daily check's off-expiry contracts.
pub const DAILY_OFF_EXPIRY_GAUGE: &str = "tv_expiry_scope_off_expiry_contracts";
/// Gauge: the latest daily check's expired contracts.
pub const DAILY_EXPIRED_GAUGE: &str = "tv_expiry_scope_expired_contracts";
/// Gauge: the latest daily check's contracts with a missing stored name.
pub const DAILY_UNLABELLED_GAUGE: &str = "tv_expiry_scope_unlabelled_contracts";

// ---------------------------------------------------------------------------
// The contract name parser
// ---------------------------------------------------------------------------

/// One stored contract name, split. Borrowed from the name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParsedContractLabel<'a> {
    /// Underlying symbol (may itself contain `-`, e.g. `BAJAJ-AUTO`).
    pub underlying: &'a str,
    /// Expiry as `YYYYMMDD`.
    pub expiry_ymd: u32,
    /// Strike in paise.
    pub strike_paise: i64,
    /// `CE` or `PE`.
    pub leg: &'a str,
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Splits a name rendered by `contract_underlying_map::contract_label`
/// (`SBIN-23Nov2026-1040-PE`), reading from the RIGHT: leg, strike, expiry,
/// and everything left of them is the underlying, so a hyphenated underlying
/// (`BAJAJ-AUTO-23Nov2026-...`) survives. `None` for anything else: a missing
/// leg, a truncated (capped) name, a strike that is not `rupees[.paise]`, an
/// expiry that is not a real `DDMonYYYY` date, or an empty underlying. Pure,
/// O(name length), no allocation.
#[must_use]
pub fn parse_contract_label(label: &str) -> Option<ParsedContractLabel<'_>> {
    let mut parts = label.rsplitn(4, '-');
    let leg = parts.next()?;
    let strike = parts.next()?;
    let expiry = parts.next()?;
    let underlying = parts.next()?;
    if leg != "CE" && leg != "PE" {
        return None;
    }
    if underlying.trim().is_empty() {
        return None;
    }
    Some(ParsedContractLabel {
        underlying,
        expiry_ymd: parse_label_expiry(expiry)?,
        strike_paise: parse_label_strike(strike)?,
        leg,
    })
}

/// `DDMonYYYY` → `YYYYMMDD`, only for a real calendar date.
fn parse_label_expiry(s: &str) -> Option<u32> {
    if s.len() != 9 || !s.is_ascii() {
        return None;
    }
    let day: u32 = s.get(0..2)?.parse().ok()?;
    let month_name = s.get(2..5)?;
    let year: i32 = s.get(5..9)?.parse().ok()?;
    let month = u32::try_from(MONTHS.iter().position(|m| *m == month_name)?).ok()? + 1;
    NaiveDate::from_ymd_opt(year, month, day)?;
    u32::try_from(year)
        .ok()?
        .checked_mul(10_000)?
        .checked_add(month * 100 + day)
}

/// `rupees` or `rupees.p` / `rupees.pp` → paise. Digits only, no sign.
fn parse_label_strike(s: &str) -> Option<i64> {
    let (rupees, paise) = match s.split_once('.') {
        Some((r, p)) => (r, Some(p)),
        None => (s, None),
    };
    if rupees.is_empty() || !rupees.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let whole: i64 = rupees.parse().ok()?;
    let fraction: i64 = match paise {
        None => 0,
        Some(p) if (1..=2).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit()) => {
            let v: i64 = p.parse().ok()?;
            if p.len() == 1 { v * 10 } else { v }
        }
        Some(_) => return None,
    };
    whole.checked_mul(100)?.checked_add(fraction)
}

// ---------------------------------------------------------------------------
// The nearest-expiry table, from the day's contract file
// ---------------------------------------------------------------------------

/// The derivative segment an option row of the contract file belongs to:
/// `NSE` → `NSE_FNO`, `BSE` → `BSE_FNO`. Deliberately its own mapping, not
/// the selector's (which refuses BSE), so a BSE contract that ever reached a
/// candle can still be judged. Pure.
#[must_use]
pub fn artifact_option_segment(exch_id: &str) -> Option<ExchangeSegment> {
    match exch_id {
        "NSE" => Some(ExchangeSegment::NseFno),
        "BSE" => Some(ExchangeSegment::BseFno),
        _ => None,
    }
}

fn is_option_row(row: &ContractRow) -> bool {
    matches!(row.c.as_str(), "OPTIDX" | "OPTSTK")
}

/// Per `(segment, underlying)` the current expiry, plus a composite-key
/// index of the option rows. Borrowed from the contract rows.
#[derive(Debug, Default)]
pub struct ExpiryIndex<'a> {
    /// `(segment, trimmed underlying)` → nearest real expiry `YYYYMMDD`.
    pub nearest: HashMap<(ExchangeSegment, &'a str), u32>,
    /// `(security_id, segment)` → its option row (every expiry, expired too).
    pub by_key: HashMap<ContractKey, &'a ContractRow>,
    /// Earlier expiries skipped for carrying fewer than the minimum legs.
    pub thin_expiries_skipped: usize,
}

/// Builds the [`ExpiryIndex`]. `only`, when given, limits `by_key` to those
/// composite keys (the subscribe-time check needs only the selection); the
/// nearest-expiry table always covers the whole file. A duplicate composite
/// key keeps its first row. O(rows), cold.
#[must_use]
pub fn build_expiry_index<'a>(
    contracts: &'a [ContractRow],
    today_ymd: u32,
    only: Option<&HashSet<ContractKey>>,
) -> ExpiryIndex<'a> {
    let mut legs: HashMap<(ExchangeSegment, &'a str, u32), usize> = HashMap::new();
    let mut index = ExpiryIndex::default();
    for row in contracts {
        if !is_option_row(row) || row.i == 0 {
            continue;
        }
        let Some(segment) = artifact_option_segment(&row.x) else {
            continue;
        };
        let key = (row.i, segment);
        if only.is_none_or(|keys| keys.contains(&key)) {
            index.by_key.entry(key).or_insert(row);
        }
        let underlying = row.u.trim();
        let usable = row.e != 0
            && row.e >= today_ymd
            && row.s != 0
            && matches!(row.l.as_str(), "CE" | "PE")
            && !underlying.is_empty();
        if usable {
            *legs.entry((segment, underlying, row.e)).or_insert(0) += 1;
        }
    }
    for (&(segment, underlying, expiry), &n) in &legs {
        if n >= MIN_LEGS_FOR_A_REAL_EXPIRY {
            index
                .nearest
                .entry((segment, underlying))
                .and_modify(|held| *held = (*held).min(expiry))
                .or_insert(expiry);
        }
    }
    index.thin_expiries_skipped = legs
        .iter()
        .filter(|&(&(segment, underlying, expiry), &n)| {
            n < MIN_LEGS_FOR_A_REAL_EXPIRY
                && index
                    .nearest
                    .get(&(segment, underlying))
                    .is_some_and(|&nearest| expiry < nearest)
        })
        .count();
    index
}

// ---------------------------------------------------------------------------
// Layer 1: subscribe time
// ---------------------------------------------------------------------------

/// What the subscribe-time check found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SubscribeScopeReport {
    /// Selected contracts judged.
    pub checked: usize,
    /// Selected contracts on a live expiry that is not the current one.
    pub off_expiry: usize,
    /// Selected contracts whose expiry is before today.
    pub expired: usize,
    /// Selected contracts the contract file does not list as an option, or
    /// whose underlying has no real expiry.
    pub unknown: usize,
    /// Every contract to refuse (`off_expiry + expired + unknown` of them).
    pub refused: Vec<SubscribeInstrument>,
    /// The first refused contract's name, for the log line.
    pub sample: Option<String>,
}

/// Judges every selected contract against the day's contract file. Pure,
/// O(rows + selected). See the module header.
#[must_use]
pub fn check_selected_expiries(
    selected: &[SubscribeInstrument],
    contracts: &[ContractRow],
    today_ymd: u32,
) -> SubscribeScopeReport {
    let keys: HashSet<ContractKey> = selected
        .iter()
        .map(|i| (i.security_id, i.segment))
        .collect();
    let index = build_expiry_index(contracts, today_ymd, Some(&keys));
    let mut report = SubscribeScopeReport::default();
    for instrument in selected {
        let key = (instrument.security_id, instrument.segment);
        let row = index.by_key.get(&key).copied();
        let refuse = match row {
            None => {
                report.unknown += 1;
                true
            }
            Some(r) if r.e < today_ymd => {
                report.checked += 1;
                report.expired += 1;
                true
            }
            Some(r) => match index.nearest.get(&(instrument.segment, r.u.trim())) {
                None => {
                    report.unknown += 1;
                    true
                }
                Some(&nearest) => {
                    report.checked += 1;
                    let off = nearest != r.e;
                    if off {
                        report.off_expiry += 1;
                    }
                    off
                }
            },
        };
        if !refuse {
            continue;
        }
        if report.sample.is_none() {
            report.sample = Some(match row {
                Some(r) => contract_label(&r.u, r.e, r.s, &r.l),
                None => format!(
                    "security_id {} segment {}",
                    instrument.security_id,
                    tickvault_common::segment::segment_code_to_str(
                        instrument.segment.binary_code()
                    )
                ),
            });
        }
        report.refused.push(*instrument);
    }
    report
}

/// Runs [`check_selected_expiries`] and REMOVES every refused contract from
/// the selection (instruments and backup rank), lowering the per-class counts
/// and recording the total in `refused_off_expiry`. Pure apart from the
/// mutation of `selection`.
#[must_use]
pub fn enforce_subscribe_expiry_scope(
    selection: &mut ContractSelection,
    contracts: &[ContractRow],
    today_ymd: u32,
) -> SubscribeScopeReport {
    let report = check_selected_expiries(&selection.instruments, contracts, today_ymd);
    if report.refused.is_empty() {
        return report;
    }
    let refused: HashSet<ContractKey> = report
        .refused
        .iter()
        .map(|i| (i.security_id, i.segment))
        .collect();
    let classes: HashMap<ContractKey, bool> = contracts
        .iter()
        .filter_map(|r| {
            let key = (r.i, artifact_option_segment(&r.x)?);
            refused.contains(&key).then_some((key, r.c == "OPTIDX"))
        })
        .collect();
    for key in &refused {
        match classes.get(key) {
            Some(true) => selection.index_options = selection.index_options.saturating_sub(1),
            Some(false) => selection.stock_options = selection.stock_options.saturating_sub(1),
            None => {}
        }
    }
    selection
        .instruments
        .retain(|i| !refused.contains(&(i.security_id, i.segment)));
    selection
        .backup_rank
        .retain(|i| !refused.contains(&(i.security_id, i.segment)));
    selection.refused_off_expiry = selection
        .refused_off_expiry
        .saturating_add(report.refused.len());
    report
}

/// The IST day (`YYYYMMDD`) of the latest subscribe-time check, 0 before any.
static SUBSCRIBE_SCOPE_DAY: AtomicU32 = AtomicU32::new(0);
/// The largest refusal count of any subscribe-time check on that day.
static SUBSCRIBE_SCOPE_REFUSED: AtomicU64 = AtomicU64::new(0);

/// Logs, counts and remembers one subscribe-time check. A refusal is one
/// coded `error!` (`EXPIRY-SCOPE-01`, log only); a clean check is `debug!`
/// (the attach retries every few seconds through the morning).
// TEST-EXEMPT: logging + metrics side effects over the unit-tested report; the day memory it writes is tested through `subscribe_refused_for_day`.
pub fn record_subscribe_expiry_scope(report: &SubscribeScopeReport, today_ymd: u32) {
    let refused = report.refused.len() as u64;
    if SUBSCRIBE_SCOPE_DAY.swap(today_ymd, Ordering::AcqRel) != today_ymd {
        SUBSCRIBE_SCOPE_REFUSED.store(refused, Ordering::Release);
    } else {
        SUBSCRIBE_SCOPE_REFUSED.fetch_max(refused, Ordering::AcqRel);
    }
    metrics::gauge!(SUBSCRIBE_CHECKED_GAUGE).set(report.checked as f64);
    metrics::counter!(SUBSCRIBE_REFUSED_COUNTER).increment(refused);
    if refused > 0 {
        error!(
            code = ErrorCode::ExpiryScope01OffExpiry.code_str(),
            stage = "subscribe",
            refused,
            off_expiry = report.off_expiry,
            expired = report.expired,
            unknown = report.unknown,
            checked = report.checked,
            sample = report.sample.as_deref().unwrap_or(""),
            "EXPIRY-SCOPE-01: the contract selection held contracts outside their underlying's \
             current expiry; they were REFUSED and will not be subscribed"
        );
    } else {
        debug!(
            checked = report.checked,
            "expiry scope: every selected contract is on its underlying's current expiry"
        );
    }
}

/// The refusal count the subscribe-time check recorded for `today_ymd`, or
/// `None` when this process has not checked that day.
#[must_use]
pub fn subscribe_refused_for_day(today_ymd: u32) -> Option<u64> {
    (SUBSCRIBE_SCOPE_DAY.load(Ordering::Acquire) == today_ymd)
        .then(|| SUBSCRIBE_SCOPE_REFUSED.load(Ordering::Acquire))
}

// ---------------------------------------------------------------------------
// Layer 2: the daily check over today's stored candles
// ---------------------------------------------------------------------------

/// One distinct `(security_id, segment, contract)` row of `candles_1m`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CandleContractRow {
    /// `security_id` as stored.
    pub security_id: i64,
    /// Segment name (`NSE_FNO`, `BSE_FNO`).
    pub segment: String,
    /// Stored contract name; `None` when the column is NULL or empty.
    pub contract: Option<String>,
}

/// The daily read: today's distinct option contracts of feed `dhan` in
/// `candles_1m`, at most [`EXPIRY_SCOPE_MAX_ROWS`] + 1 rows (one more than the
/// cap, so a capped read is detectable). Pure.
#[must_use]
pub fn expiry_scope_sql(day: NaiveDate) -> String {
    format!(
        "select distinct security_id, segment, contract from {} where ts in '{}' \
         and feed = '{}' and segment in ('NSE_FNO', 'BSE_FNO') limit {}",
        TfIndex::M1.table_name(),
        day.format("%Y-%m-%d"),
        Feed::Dhan.as_str(),
        EXPIRY_SCOPE_MAX_ROWS + 1
    )
}

/// Parses QuestDB's `/exec` answer to [`expiry_scope_sql`]. A row whose id is
/// not an integer is KEPT with id `-1` (and a missing segment as an empty
/// string), so the classifier counts it as `unresolved` instead of it
/// silently vanishing. `Err` when the body has no `dataset` array. Pure,
/// O(rows).
///
/// # Errors
/// The body is not JSON or carries no `dataset` array.
pub fn parse_candle_contract_rows(body: &str) -> Result<Vec<CandleContractRow>, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| "response is not valid JSON".to_owned())?;
    let dataset = value
        .get("dataset")
        .and_then(|d| d.as_array())
        .ok_or_else(|| "response has no `dataset` array".to_owned())?;
    let mut rows = Vec::with_capacity(dataset.len());
    for row in dataset {
        let Some(cols) = row.as_array() else {
            continue;
        };
        let security_id = match cols.first() {
            Some(serde_json::Value::String(s)) => s.trim().parse::<i64>().unwrap_or(-1),
            Some(other) => other.as_i64().unwrap_or(-1),
            None => -1,
        };
        let segment = cols
            .get(1)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned();
        let contract = cols
            .get(2)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        rows.push(CandleContractRow {
            security_id,
            segment,
            contract,
        });
    }
    Ok(rows)
}

/// What the daily check counted. Every count is of DISTINCT contracts
/// (composite key), so a contract stored under two names counts once.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExpiryScopeCounts {
    /// Distinct rows the read returned.
    pub rows_read: usize,
    /// Contracts whose expiry was judged.
    pub contracts_checked: usize,
    /// Judged contracts on a live expiry that is not the current one.
    pub off_expiry: usize,
    /// Judged contracts whose expiry is before the day.
    pub expired: usize,
    /// Contracts with at least one candle carrying no name.
    pub unlabelled: usize,
    /// Contracts whose underlying has no real expiry in the contract file.
    pub unknown_underlying: usize,
    /// Contracts with a stored name that does not parse (judged from the
    /// contract file when it lists them).
    pub malformed_labels: usize,
    /// Contracts whose stored name(s) disagree with the contract file, or
    /// with each other.
    pub label_mismatch: usize,
    /// Contracts with no usable name (none, or one that does not parse) that
    /// the contract file does not list, and rows with an unusable id or
    /// segment.
    pub unresolved: usize,
}

impl ExpiryScopeCounts {
    /// `off_expiry + expired`.
    #[must_use]
    pub const fn violations(&self) -> usize {
        self.off_expiry.saturating_add(self.expired)
    }

    /// Contracts that could not be judged.
    #[must_use]
    pub const fn unverifiable(&self) -> usize {
        self.unknown_underlying.saturating_add(self.unresolved)
    }
}

#[derive(Default)]
struct KeyState<'a> {
    any_unlabelled: bool,
    malformed: bool,
    parsed: Option<(&'a str, u32)>,
    conflicting: bool,
    first_name: Option<&'a str>,
}

/// Judges every stored contract. Returns the counts and the name of the
/// lowest-keyed offender (off-expiry or expired), for the audit row. Pure,
/// O(rows + contracts in the index lookups).
#[must_use]
pub fn classify_candle_contracts(
    rows: &[CandleContractRow],
    index: &ExpiryIndex<'_>,
    today_ymd: u32,
) -> (ExpiryScopeCounts, Option<String>) {
    let mut counts = ExpiryScopeCounts {
        rows_read: rows.len(),
        ..ExpiryScopeCounts::default()
    };
    let mut keys: HashMap<ContractKey, KeyState<'_>> = HashMap::with_capacity(rows.len());
    for row in rows {
        let segment = tickvault_common::segment::segment_str_to_code(row.segment.trim())
            .and_then(ExchangeSegment::from_byte);
        let (Ok(id), Some(segment)) = (u64::try_from(row.security_id), segment) else {
            counts.unresolved += 1;
            continue;
        };
        let state = keys.entry((id, segment)).or_default();
        let Some(name) = row.contract.as_deref() else {
            state.any_unlabelled = true;
            continue;
        };
        state.first_name.get_or_insert(name);
        match parse_contract_label(name) {
            None => state.malformed = true,
            Some(p) => {
                let seen = (p.underlying.trim(), p.expiry_ymd);
                match state.parsed {
                    None => state.parsed = Some(seen),
                    Some(held) if held != seen => state.conflicting = true,
                    Some(_) => {}
                }
            }
        }
    }
    let mut worst: Option<(ContractKey, String)> = None;
    for (key, state) in &keys {
        if state.any_unlabelled {
            counts.unlabelled += 1;
        }
        let artifact = index.by_key.get(key);
        let resolved = match (artifact, state.parsed) {
            (Some(row), parsed) => {
                let truth = (row.u.trim(), row.e);
                if state.conflicting || parsed.is_some_and(|p| p != truth) {
                    counts.label_mismatch += 1;
                }
                Some(truth)
            }
            (None, Some(parsed)) => {
                if state.conflicting {
                    counts.label_mismatch += 1;
                }
                Some(parsed)
            }
            (None, None) => None,
        };
        if state.malformed && state.parsed.is_none() {
            counts.malformed_labels += 1;
        }
        let Some((underlying, expiry)) = resolved else {
            counts.unresolved += 1;
            continue;
        };
        let offends = if expiry < today_ymd {
            counts.contracts_checked += 1;
            counts.expired += 1;
            true
        } else {
            match index.nearest.get(&(key.1, underlying)) {
                None => {
                    counts.unknown_underlying += 1;
                    false
                }
                Some(&nearest) => {
                    counts.contracts_checked += 1;
                    let off = nearest != expiry;
                    if off {
                        counts.off_expiry += 1;
                    }
                    off
                }
            }
        };
        if offends
            && worst
                .as_ref()
                .is_none_or(|(held, _)| sort_key(*key) < sort_key(*held))
        {
            let name = match (state.first_name, artifact) {
                (Some(n), _) => n.to_owned(),
                (None, Some(row)) => contract_label(&row.u, row.e, row.s, &row.l),
                (None, None) => format!("security_id {}", key.0),
            };
            worst = Some((*key, name));
        }
    }
    (counts, worst.map(|(_, name)| name))
}

/// A total order on a composite key (`ExchangeSegment` is not `Ord`): the
/// segment's wire code, then the id.
fn sort_key(key: ContractKey) -> (u8, u64) {
    (key.1.binary_code(), key.0)
}

/// The outcome of one daily check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExpiryScopeVerdict {
    /// Every stored contract was judged and is on its current expiry.
    Clean,
    /// At least one stored contract is expired or off its current expiry.
    Violation,
    /// The check could not vouch for the whole day.
    Partial,
}

impl ExpiryScopeVerdict {
    /// The `verdict` SYMBOL value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Violation => "violation",
            Self::Partial => "partial",
        }
    }
}

/// What one daily check classifies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExpiryScopeInputs {
    /// Today's contract file was read.
    pub artifact_read: bool,
    /// The `candles_1m` read succeeded.
    pub rows_read_ok: bool,
    /// The read returned more than [`EXPIRY_SCOPE_MAX_ROWS`] rows.
    pub truncated: bool,
    /// The counts (all zero when either read failed).
    pub counts: ExpiryScopeCounts,
}

/// Classifies one daily check. Pure, O(1). See the module table. A real
/// violation is reported even when other rows could not be judged; a read
/// failure or a missing file is never `clean`.
#[must_use]
pub const fn classify_expiry_scope(i: ExpiryScopeInputs) -> (ExpiryScopeVerdict, &'static str) {
    if !i.artifact_read {
        return (ExpiryScopeVerdict::Partial, "artifact_missing");
    }
    if !i.rows_read_ok {
        return (ExpiryScopeVerdict::Partial, "read_failed");
    }
    let c = i.counts;
    match (c.expired > 0, c.off_expiry > 0) {
        (true, true) => return (ExpiryScopeVerdict::Violation, "expired_and_off_expiry"),
        (true, false) => return (ExpiryScopeVerdict::Violation, "expired"),
        (false, true) => return (ExpiryScopeVerdict::Violation, "off_expiry"),
        (false, false) => {}
    }
    if i.truncated {
        return (ExpiryScopeVerdict::Partial, "row_cap_hit");
    }
    if c.rows_read == 0 || c.contracts_checked == 0 {
        return (ExpiryScopeVerdict::Partial, "no_rows");
    }
    if c.unverifiable() > 0 {
        return (ExpiryScopeVerdict::Partial, "unverifiable_rows");
    }
    if c.label_mismatch > 0 {
        return (ExpiryScopeVerdict::Partial, "label_mismatch");
    }
    if c.malformed_labels > 0 {
        return (ExpiryScopeVerdict::Partial, "malformed_labels");
    }
    (ExpiryScopeVerdict::Clean, "all_current_expiry")
}

/// Seconds to sleep before today's check, or `None` when the check time has
/// passed (the day is skipped). Pure.
#[must_use]
pub const fn secs_until_expiry_scope_check(now_secs_of_day_ist: u32) -> Option<u64> {
    if now_secs_of_day_ist < EXPIRY_SCOPE_CHECK_SECS_OF_DAY_IST {
        Some((EXPIRY_SCOPE_CHECK_SECS_OF_DAY_IST - now_secs_of_day_ist) as u64)
    } else {
        None
    }
}

/// `YYYYMMDD` of an IST date. Pure.
#[must_use]
pub fn ymd_of(day: NaiveDate) -> u32 {
    day.format("%Y%m%d").to_string().parse().unwrap_or(0)
}

/// Registers every series at 0 so the first sample is a real baseline.
pub fn pre_register_expiry_scope_metrics() {
    metrics::counter!(SUBSCRIBE_REFUSED_COUNTER).increment(0);
    metrics::gauge!(SUBSCRIBE_CHECKED_GAUGE).set(0.0);
    for verdict in [
        ExpiryScopeVerdict::Clean,
        ExpiryScopeVerdict::Violation,
        ExpiryScopeVerdict::Partial,
    ] {
        metrics::counter!(DAILY_RUNS_COUNTER, "verdict" => verdict.as_str()).increment(0);
    }
    for gauge in [
        DAILY_OFF_EXPIRY_GAUGE,
        DAILY_EXPIRED_GAUGE,
        DAILY_UNLABELLED_GAUGE,
    ] {
        metrics::gauge!(gauge).set(0.0);
    }
}

fn now_utc_nanos() -> i64 {
    chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
}

fn ist_date_of(ist_nanos: i64) -> Option<NaiveDate> {
    chrono::DateTime::from_timestamp(ist_nanos.div_euclid(NANOS_PER_SEC), 0)
        .map(|dt| dt.date_naive())
}

fn secs_of_day(ist_nanos: i64) -> u32 {
    u32::try_from(ist_nanos.rem_euclid(NANOS_PER_DAY) / NANOS_PER_SEC).unwrap_or(0)
}

/// Spawns the daily check on its own task (never the frame drain). Returns
/// the task handle.
// TEST-EXEMPT: tokio::spawn wrapper over the unit-tested pure parts (parse_contract_label, build_expiry_index, classify_candle_contracts, classify_expiry_scope, expiry_scope_sql, parse_candle_contract_rows, secs_until_expiry_scope_check); spawn site pinned by test_main_wires_spawn_expiry_scope_check.
pub fn spawn_expiry_scope_check(
    questdb: QuestDbConfig,
    calendar: Arc<TradingCalendar>,
) -> tokio::task::JoinHandle<()> {
    pre_register_expiry_scope_metrics();
    tokio::spawn(async move {
        tickvault_storage::expiry_scope_audit_persistence::ensure_expiry_scope_audit_table(
            &questdb,
        )
        .await;
        loop {
            let now_ist = now_utc_nanos().saturating_add(IST_UTC_OFFSET_NANOS);
            let today = ist_date_of(now_ist);
            let trading = today.is_some_and(|d| calendar.is_trading_day(d));
            if let (true, Some(day), Some(wait)) = (
                trading,
                today,
                secs_until_expiry_scope_check(secs_of_day(now_ist)),
            ) {
                tokio::time::sleep(Duration::from_secs(wait)).await;
                run_one_check(&questdb, day, now_ist).await;
            }
            // Sleep to just past the next IST midnight (a multi-day process).
            let now_ist = now_utc_nanos().saturating_add(IST_UTC_OFFSET_NANOS);
            let to_midnight = NANOS_PER_DAY - now_ist.rem_euclid(NANOS_PER_DAY);
            let secs = u64::try_from(to_midnight / NANOS_PER_SEC).unwrap_or(0) + 5;
            tokio::time::sleep(Duration::from_secs(secs)).await;
        }
    })
}

async fn read_candle_contracts(
    questdb: &QuestDbConfig,
    day: NaiveDate,
) -> Result<Vec<CandleContractRow>, String> {
    let client =
        tickvault_storage::http_client::build_probe_client(EXPIRY_SCOPE_QUERY_TIMEOUT_SECS)
            .map_err(|e| format!("client build: {e}"))?;
    let url = format!("http://{}:{}/exec", questdb.host, questdb.http_port);
    let sql = expiry_scope_sql(day);
    let resp = tokio::time::timeout(
        Duration::from_secs(EXPIRY_SCOPE_QUERY_TIMEOUT_SECS),
        client.get(&url).query(&[("query", sql.as_str())]).send(),
    )
    .await
    .map_err(|_| "timeout".to_owned())?
    .map_err(|e| format!("request: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("status {status}"));
    }
    let body = resp.text().await.map_err(|e| format!("body: {e}"))?;
    parse_candle_contract_rows(&body)
}

async fn run_one_check(questdb: &QuestDbConfig, day: NaiveDate, day_ist_nanos: i64) {
    let today_ymd = ymd_of(day);
    let date = day.format("%Y-%m-%d").to_string();
    let artifact = tokio::task::spawn_blocking(move || {
        crate::dhan_contract_universe::read_contract_artifact(&date).map_err(|e| e.to_string())
    })
    .await
    .unwrap_or_else(|e| Err(format!("artifact read task: {e}")));
    let mut reads = 0_u32;
    let rows = loop {
        reads += 1;
        match read_candle_contracts(questdb, day).await {
            Ok(rows) => break Ok(rows),
            Err(err) if reads >= EXPIRY_SCOPE_MAX_READS => break Err(err),
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(EXPIRY_SCOPE_READ_SPACING_SECS)).await;
            }
        }
    };
    let read_error = rows.as_ref().err().cloned();
    let artifact_error = artifact.as_ref().err().cloned();
    let (inputs, sample) = match (artifact, rows) {
        (Ok(contracts), Ok(rows)) => tokio::task::spawn_blocking(move || {
            let truncated = rows.len() > EXPIRY_SCOPE_MAX_ROWS;
            let index = build_expiry_index(&contracts, today_ymd, None);
            let (counts, sample) = classify_candle_contracts(&rows, &index, today_ymd);
            (
                ExpiryScopeInputs {
                    artifact_read: true,
                    rows_read_ok: true,
                    truncated,
                    counts,
                },
                sample,
            )
        })
        .await
        .unwrap_or((
            ExpiryScopeInputs {
                artifact_read: true,
                rows_read_ok: false,
                truncated: false,
                counts: ExpiryScopeCounts::default(),
            },
            None,
        )),
        (artifact, rows) => (
            ExpiryScopeInputs {
                artifact_read: artifact.is_ok(),
                rows_read_ok: rows.is_ok(),
                truncated: false,
                counts: ExpiryScopeCounts::default(),
            },
            None,
        ),
    };
    let (verdict, reason) = classify_expiry_scope(inputs);
    let c = inputs.counts;
    let to_i64 = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
    let row = ExpiryScopeRow {
        checked_at_ist_nanos: now_utc_nanos().saturating_add(IST_UTC_OFFSET_NANOS),
        day_ist_nanos,
        feed: Feed::Dhan,
        verdict: verdict.as_str(),
        reason,
        rows_read: if inputs.rows_read_ok {
            to_i64(c.rows_read)
        } else {
            -1
        },
        contracts_checked: to_i64(c.contracts_checked),
        off_expiry: to_i64(c.off_expiry),
        expired: to_i64(c.expired),
        unlabelled: to_i64(c.unlabelled),
        unknown_underlying: to_i64(c.unknown_underlying),
        malformed_labels: to_i64(c.malformed_labels),
        label_mismatch: to_i64(c.label_mismatch),
        unresolved: to_i64(c.unresolved),
        subscribe_refused: subscribe_refused_for_day(today_ymd)
            .map_or(-1, |n| i64::try_from(n).unwrap_or(i64::MAX)),
        sample_offender: sample.unwrap_or_default(),
    };
    report_daily(
        &row,
        verdict,
        read_error.as_deref(),
        artifact_error.as_deref(),
    );
    persist(questdb, &row).await;
}

fn report_daily(
    row: &ExpiryScopeRow,
    verdict: ExpiryScopeVerdict,
    read_error: Option<&str>,
    artifact_error: Option<&str>,
) {
    metrics::counter!(DAILY_RUNS_COUNTER, "verdict" => verdict.as_str()).increment(1);
    metrics::gauge!(DAILY_OFF_EXPIRY_GAUGE).set(row.off_expiry as f64);
    metrics::gauge!(DAILY_EXPIRED_GAUGE).set(row.expired as f64);
    metrics::gauge!(DAILY_UNLABELLED_GAUGE).set(row.unlabelled as f64);
    match verdict {
        ExpiryScopeVerdict::Violation => error!(
            code = ErrorCode::ExpiryScope01OffExpiry.code_str(),
            stage = "daily",
            reason = row.reason,
            off_expiry = row.off_expiry,
            expired = row.expired,
            contracts_checked = row.contracts_checked,
            sample = row.sample_offender.as_str(),
            "EXPIRY-SCOPE-01: today's stored option candles include contracts outside their \
             underlying's current expiry"
        ),
        ExpiryScopeVerdict::Partial => warn!(
            reason = row.reason,
            rows_read = row.rows_read,
            contracts_checked = row.contracts_checked,
            unknown_underlying = row.unknown_underlying,
            unresolved = row.unresolved,
            malformed_labels = row.malformed_labels,
            label_mismatch = row.label_mismatch,
            read_error = read_error.unwrap_or(""),
            artifact_error = artifact_error.unwrap_or(""),
            "expiry scope: today's check could not vouch for every contract (partial)"
        ),
        ExpiryScopeVerdict::Clean => info!(
            contracts_checked = row.contracts_checked,
            unlabelled = row.unlabelled,
            subscribe_refused = row.subscribe_refused,
            "expiry scope: every stored option contract today is on its underlying's current \
             expiry"
        ),
    }
}

async fn persist(questdb: &QuestDbConfig, row: &ExpiryScopeRow) {
    use tickvault_storage::expiry_scope_audit_persistence::ExpiryScopeAuditWriter;
    let mut writer = ExpiryScopeAuditWriter::new(questdb);
    let written = writer
        .append_row(row)
        .and_then(|()| crate::order_observability::blocking_flush(|| writer.flush()));
    if let Err(err) = written {
        error!(
            code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
            stage = "expiry_scope_audit_write",
            verdict = row.verdict,
            ?err,
            "expiry_scope_audit: today's row was not written (the verdict is in this log)"
        );
    }
}

#[cfg(test)]
#[path = "expiry_scope_check_tests.rs"]
mod tests;

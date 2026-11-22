//! Re-prices one paper order from the stored 1 s candles with the chain's own
//! pricing (`live_fill::fill_window::price_held_window`), plan item LF-7.
//!
//! The chain priced each order from its own 1 s bars; this re-pricing reads
//! the stored `candles_1s` of the same seconds. A late tick the chain never
//! saw can widen a stored bar, so a difference is a finding to read, not
//! proof of a defect.
//!
//! Cold, once a day, at most [`RECHECK_MAX_ORDERS`] orders and
//! [`RECHECK_WALL_BUDGET_SECS`] in all (flagged: not O(1)). Each order is one
//! query whose REPLY is at most `max_fill_seconds + 3` rows, but whose COST
//! on QuestDB is larger (LF-7 review finding 8): in `candles_1s`
//! `security_id` is an unindexed LONG and `segment` / `feed` are
//! low-cardinality symbols, so QuestDB finds the time slice
//! `[decision_sec, horizon_end]` and then filters EVERY instrument's 1 s rows
//! in it, O(instruments that traded in the window × window seconds) per
//! order (Assumed, not measured: about 1e5 to 3e5 rows for a 62 s window,
//! about 3e6 at the 600 s ceiling). The 10 s client timeout drops the
//! request but does not cancel the query on the server, so a slow read still
//! costs QuestDB its whole scan; only the wall budget bounds the total. The
//! pricing itself is O(seconds) per order.

use std::time::Duration;

use tickvault_common::config::LIVE_FILL_MAX_FILL_SECONDS_CEILING;
use tickvault_trading::brutex_host::engine::Side;
use tickvault_trading::brutex_host::worst_fill::{
    BPS_DENOMINATOR, FillSchedule, SecondBar, rupees_to_paise,
};

use super::rows::{EventRow, json_i64, parse_ts_nanos};
use crate::live_fill::fill_window::{
    HeldPricing, ShareOrder, allot_second, beyond_limit, price_held_window,
};

/// Most orders re-priced per report; later orders read `recheck_skipped`.
pub const RECHECK_MAX_ORDERS: usize = 500;

/// Wall budget for all re-pricing of one report.
pub const RECHECK_WALL_BUDGET_SECS: u64 = 120;

/// One order's second read bound.
pub const RECHECK_QUERY_TIMEOUT_SECS: u64 = 10;

/// Fill-second offsets the histogram keeps (0..=60); later seconds fold into
/// the last bucket.
pub const FILL_OFFSET_BUCKETS: usize = 61;

const NANOS_PER_SEC: i64 = 1_000_000_000;

/// What the chain priced an order from, read off its Decision row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderInputs {
    pub signal_id: i64,
    pub security_id: i64,
    pub segment: String,
    pub side: Side,
    /// Shares.
    pub qty: u64,
    pub tick_paise: i64,
    pub lot_size: u64,
    pub participation_bps: u32,
    pub max_fill_seconds: u32,
    pub limit_paise: i64,
    /// The candle's last second (IST epoch seconds).
    pub decision_sec: i64,
    /// The candle's close, paise (the decision second's price when nothing
    /// traded in it).
    pub ltp_paise: i64,
    /// Best price and quantity on our side at the decision.
    pub book: Option<(i64, u64)>,
}

impl OrderInputs {
    /// The inputs of a Decision row, or `None` when any pricing input is
    /// missing (a row from before LF-7, or an order refused before pricing).
    /// O(1).
    #[must_use]
    pub fn from_decision(row: &EventRow) -> Option<Self> {
        let side = match row.side.as_str() {
            "BUY" => Side::Buy,
            "SELL" => Side::Sell,
            _ => return None,
        };
        let qty = u64::try_from(row.qty).ok().filter(|q| *q > 0)?;
        let tick_paise = row.tick_paise.filter(|t| *t > 0)?;
        let lot_size = row
            .lot_size
            .and_then(|l| u64::try_from(l).ok())
            .filter(|l| *l > 0)?;
        let participation_bps = row
            .participation_bps
            .and_then(|p| u32::try_from(p).ok())
            .filter(|p| *p > 0 && *p <= BPS_DENOMINATOR)?;
        let max_fill_seconds = row
            .max_fill_seconds
            .and_then(|m| u32::try_from(m).ok())
            // The config ceiling: the value sizes the fill schedule and the
            // read LIMIT, so a stored value past it is refused, never trusted.
            .filter(|m| (1..=LIVE_FILL_MAX_FILL_SECONDS_CEILING).contains(m))?;
        let limit_paise = row.limit_paise.filter(|l| *l > 0)?;
        let decision_sec = row
            .candle_end_ns?
            .div_euclid(NANOS_PER_SEC)
            .checked_sub(1)?;
        let ltp_paise = row.ltp_paise.filter(|p| *p > 0)?;
        let (price, size) = match side {
            Side::Buy => (row.best_ask_paise, row.best_ask_qty),
            Side::Sell => (row.best_bid_paise, row.best_bid_qty),
        };
        let book = match (price, size) {
            (Some(p), Some(q)) if p > 0 && q > 0 => Some((p, u64::try_from(q).ok()?)),
            _ => None,
        };
        Some(Self {
            signal_id: row.signal_id,
            security_id: row.security_id,
            segment: row.segment.clone(),
            side,
            qty,
            tick_paise,
            lot_size,
            participation_bps,
            max_fill_seconds,
            limit_paise,
            decision_sec,
            ltp_paise,
            book,
        })
    }

    /// The last fill second the chain could use. O(1).
    #[must_use]
    pub fn horizon_end(&self) -> i64 {
        self.decision_sec
            .saturating_add(1)
            .saturating_add(i64::from(self.max_fill_seconds))
    }
}

/// The re-priced fill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecheckKind {
    /// Priced. `filled_qty` is floored to whole lots like the chain's; the
    /// average is the full model's, as the chain books it.
    Priced {
        filled_qty: u64,
        avg_paise: Option<i64>,
        model_paise: Option<i64>,
        beyond_limit_qty: u64,
        /// Shares per fill-second offset (0 = the first second after the
        /// decision), summing to `filled_qty`.
        fill_offsets: Vec<(u32, u64)>,
    },
    /// The model refused (for example no traded second at all).
    Refused(String),
    /// The order could not be re-priced (inputs or stored seconds missing).
    Unavailable(&'static str),
    /// Not attempted (a cap was reached).
    Skipped(&'static str),
}

/// One order's re-pricing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recheck {
    /// Stored fill seconds found inside the order's window.
    pub seconds_seen: usize,
    pub kind: RecheckKind,
}

impl Recheck {
    /// A re-pricing that was not attempted or could not run.
    #[must_use]
    pub const fn not_run(kind: RecheckKind) -> Self {
        Self {
            seconds_seen: 0,
            kind,
        }
    }
}

/// Re-prices one order exactly as the chain does, alone on its instrument
/// ([`recheck_order_shared`] with nothing taken by an earlier order). O(seconds
/// log seconds).
pub fn recheck_order(
    inp: &OrderInputs,
    decision: Option<SecondBar>,
    seconds: &mut Vec<SecondBar>,
    out: &mut FillSchedule,
) -> Recheck {
    recheck_order_shared(
        inp,
        decision,
        seconds,
        &mut std::collections::BTreeMap::new(),
        out,
    )
}

/// Re-prices one order exactly as the chain does: the decision bar is the
/// stored second at `decision_sec` (else the candle close with no volume, as
/// `chain.rs` builds it); the fill seconds are those inside the window, each
/// with the volume the earlier held orders of the same instrument left
/// (`taken`, volume per second, updated with this order's own share); the
/// order takes its share of each second with the chain's own
/// [`allot_second`] (nothing beyond its LIMIT, LF-9 findings 1 and 3) and the
/// seconds are cut at the one after the second its shares first cover the
/// quantity (the chain finishes the order there); then [`price_held_window`]
/// and the whole-lot floor. `seconds` may hold any stored seconds of the
/// instrument; they are sorted and filtered here. `out` is reused (capacity
/// at least `max_fill_seconds + 1`). O(seconds log seconds).
pub fn recheck_order_shared(
    inp: &OrderInputs,
    decision: Option<SecondBar>,
    seconds: &mut Vec<SecondBar>,
    taken: &mut std::collections::BTreeMap<i64, u64>,
    out: &mut FillSchedule,
) -> Recheck {
    let first = inp.decision_sec.saturating_add(1);
    let horizon_end = inp.horizon_end();
    seconds.retain(|s| s.sec >= first && s.sec <= horizon_end);
    seconds.sort_unstable_by_key(|s| s.sec);
    seconds.dedup_by_key(|s| s.sec);
    let seconds_seen = seconds.len();
    let decision_bar = decision
        .filter(|d| d.sec == inp.decision_sec)
        .unwrap_or(SecondBar {
            sec: inp.decision_sec,
            high_paise: inp.ltp_paise,
            low_paise: inp.ltp_paise,
            volume: 0,
        });
    let pricing = HeldPricing {
        side: inp.side,
        qty: inp.qty,
        tick_paise: inp.tick_paise,
        participation_bps: inp.participation_bps,
        max_fill_seconds: inp.max_fill_seconds,
        limit_paise: inp.limit_paise,
        decision_bar,
        book: inp.book,
    };
    // The volume the earlier held orders of this instrument left.
    for s in seconds.iter_mut() {
        s.volume = s
            .volume
            .saturating_sub(taken.get(&s.sec).copied().unwrap_or(0));
    }
    // This order's share of each second, as the chain settles it.
    let share = ShareOrder::of_pricing(&pricing);
    let mut alloc_sum: u64 = 0;
    let mut trigger: Option<i64> = None;
    for j in 0..seconds.len() {
        let s = seconds[j];
        let next = seconds.get(j + 1).copied();
        let (shares, used) = allot_second(&share, alloc_sum, &s, next.as_ref());
        if used > 0 {
            let t = taken.entry(s.sec).or_insert(0);
            *t = t.saturating_add(used);
        }
        alloc_sum = alloc_sum.saturating_add(shares);
        if shares > 0 && alloc_sum >= inp.qty {
            trigger = Some(s.sec);
            break;
        }
    }
    if let Some(t) = trigger {
        let last = t.saturating_add(1);
        seconds.retain(|s| s.sec <= last);
    }
    let kind = match price_held_window(&pricing, seconds, out) {
        Err(refusal) => RecheckKind::Refused(refusal.to_string()),
        Ok((summary, beyond_limit_qty)) => {
            let lot = inp.lot_size.max(1);
            let filled_qty = (summary.filled_qty / lot) * lot;
            let mut left = filled_qty;
            let mut fill_offsets: Vec<(u32, u64)> = Vec::new();
            for slice in out.slices() {
                if left == 0 {
                    break;
                }
                if beyond_limit(inp.side, slice.price_paise, inp.limit_paise) {
                    continue;
                }
                let take = slice.qty.min(left);
                left -= take;
                let off = u32::try_from(slice.sec.saturating_sub(first).max(0))
                    .unwrap_or(u32::MAX)
                    .min(u32::try_from(FILL_OFFSET_BUCKETS - 1).unwrap_or(60));
                match fill_offsets.last_mut() {
                    Some((o, q)) if *o == off => *q = q.saturating_add(take),
                    _ => fill_offsets.push((off, take)),
                }
            }
            RecheckKind::Priced {
                filled_qty,
                avg_paise: if filled_qty > 0 {
                    summary.vwap_paise
                } else {
                    None
                },
                model_paise: summary.vwap_paise,
                beyond_limit_qty,
                fill_offsets,
            }
        }
    };
    Recheck { seconds_seen, kind }
}

/// The stored seconds handed to [`recheck_walk`] for one order (LF-9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderSeconds {
    /// The instrument's stored seconds from the decision second through the
    /// horizon end (the decision second included when it traded).
    Seconds(Vec<SecondBar>),
    /// Not attempted or not possible for a compared order (a cap, the wall
    /// budget, a catch-up rebuild).
    NotRun(Recheck),
    /// The read failed.
    ReadFailed,
}

/// One order in the re-pricing walk (LF-9 finding 3): every order the report
/// compares and every order the chain held, by instrument then signal id
/// (the chain's insert order, in which held orders take their share of a
/// shared second).
#[derive(Debug, Clone)]
pub struct WalkItem<'a> {
    pub row: &'a EventRow,
    pub inputs: Option<OrderInputs>,
    /// The report compares this order.
    pub is_target: bool,
    /// The chain worked this order's fill out, so its share of the seconds
    /// is known (`build::held_share_known`).
    pub known_share: bool,
    /// A later held order of the instrument starts inside this one's
    /// window, so its share must be worked out (and its seconds read) even
    /// when it is not compared.
    pub needed_later: bool,
    /// The fill price the chain's own model gave this order (the
    /// `model_price_paise` on its rows; `None` when it filled nothing or the
    /// model refused). When the re-pricing of a known-share order gives a
    /// different one, the stored seconds no longer say what the chain saw
    /// (a late print amended one after the order finished), so the share it
    /// took cannot be worked out here and the later overlapping orders read
    /// `shared_volume_unknown` (LF-9 review, finding 4).
    pub chain_model_paise: Option<i64>,
    /// The chain counted a late 1 s print inside this order's fill window
    /// (a `late_fill_ticks=` reason on its rows). A late print can add VOLUME
    /// to a second the chain had already settled (the chain's candle book
    /// drops the volume of a tick that lands after its 1 s bar was closed by
    /// the clock; the stored second keeps it), which moves this order's
    /// re-priced share and trigger with no change to its price. So a
    /// known-share order carrying it leaves the later overlapping orders
    /// `shared_volume_unknown` (LF-9 review, third pass).
    pub chain_late_fill: bool,
}

impl WalkItem<'_> {
    /// Whether the walk needs this order's seconds: compared, or not compared
    /// but its known share is needed by a later order. O(1).
    #[must_use]
    pub fn wants_seconds(&self) -> bool {
        self.inputs.is_some() && (self.is_target || (self.known_share && self.needed_later))
    }
}

/// The walk order (see [`WalkItem`]): the Decision rows of the compared
/// orders (`targets`) and of the held ones (`held`, from
/// `build::held_share_known`), by instrument then signal id. Each order looks
/// at most [`crate::live_fill::fill_window::MAX_PENDING_ORDERS`] later orders
/// of its instrument for an overlap (the chain holds no more at once).
/// O(rows log rows + orders × 64).
///
/// LF-9 review (second pass): a held order whose Decision row is missing
/// (the event channel or the outbox dropped it; its other rows landed) is
/// still in the walk, on its first other row and with no pricing inputs, so
/// its share counts as unknown and every later order of its instrument reads
/// `shared_volume_unknown`. Before, it was left out, and a later overlapping
/// order was re-priced as if it took nothing (a false model mismatch).
#[must_use]
pub fn walk_items<'a>(
    rows: &'a [EventRow],
    targets: &std::collections::BTreeSet<i64>,
    held: &std::collections::BTreeMap<i64, bool>,
) -> Vec<WalkItem<'a>> {
    let mut ordered: Vec<&EventRow> = rows
        .iter()
        .filter(|r| {
            r.event == "decision"
                && (targets.contains(&r.signal_id) || held.contains_key(&r.signal_id))
        })
        .collect();
    let mut seen: std::collections::BTreeSet<i64> = ordered.iter().map(|r| r.signal_id).collect();
    for r in rows {
        if held.contains_key(&r.signal_id) && seen.insert(r.signal_id) {
            ordered.push(r);
        }
    }
    ordered.sort_by(|a, b| {
        (a.security_id, a.segment.as_str(), a.signal_id).cmp(&(
            b.security_id,
            b.segment.as_str(),
            b.signal_id,
        ))
    });
    // The chain's model price per order (finding 4) and whether the chain
    // counted a late print in its window (third pass): one pass over the rows.
    let mut models: std::collections::BTreeMap<i64, i64> = std::collections::BTreeMap::new();
    let mut late: std::collections::BTreeSet<i64> = std::collections::BTreeSet::new();
    for r in rows {
        if r.reason.as_deref().is_some_and(carries_late_fill)
            && (targets.contains(&r.signal_id) || held.contains_key(&r.signal_id))
        {
            late.insert(r.signal_id);
        }
        if let Some(m) = r.model_price_paise
            && (targets.contains(&r.signal_id) || held.contains_key(&r.signal_id))
        {
            models.entry(r.signal_id).or_insert(m);
        }
    }
    let inputs: Vec<Option<OrderInputs>> = ordered
        .iter()
        .map(|r| {
            if r.event == "decision" {
                OrderInputs::from_decision(r)
            } else {
                None
            }
        })
        .collect();
    let mut out = Vec::with_capacity(ordered.len());
    for (i, row) in ordered.iter().enumerate() {
        let same_key = |o: &EventRow| o.security_id == row.security_id && o.segment == row.segment;
        let needed_later = inputs[i].as_ref().is_some_and(|x| {
            let end = x.horizon_end();
            ordered[i + 1..]
                .iter()
                .zip(&inputs[i + 1..])
                .take_while(|(o, _)| same_key(o))
                .take(crate::live_fill::fill_window::MAX_PENDING_ORDERS)
                .any(|(_, later)| {
                    later
                        .as_ref()
                        .is_none_or(|l| l.decision_sec.saturating_add(1) <= end)
                })
        });
        out.push(WalkItem {
            row,
            inputs: inputs[i].clone(),
            is_target: targets.contains(&row.signal_id),
            known_share: held.get(&row.signal_id).copied().unwrap_or(false),
            needed_later,
            chain_model_paise: models.get(&row.signal_id).copied(),
            chain_late_fill: late.contains(&row.signal_id),
        });
    }
    out
}

/// Re-prices the compared orders of `items` (from [`walk_items`]) with the
/// seconds in `got` (by signal id; an order with none there was not read),
/// sharing each instrument's seconds out over its held orders in order with
/// one map of the volume taken per second, as the chain's held-order book
/// does (LF-9 finding 3). An order whose window overlaps one whose share is
/// unknown (the chain finished it unpriced, or it was not read or not
/// worked out here) reads `recheck_unavailable:shared_volume_unknown`, and so
/// does every later overlapping one. Pure: the daily report and the offline
/// replay both call it. O(orders × seconds log seconds).
pub fn recheck_walk(
    items: &[WalkItem<'_>],
    got: &mut std::collections::BTreeMap<i64, OrderSeconds>,
) -> Vec<(i64, Recheck)> {
    let mut out = Vec::new();
    let mut schedule = FillSchedule::with_capacity(FillSchedule::required_capacity(61, 0));
    let mut taken: std::collections::BTreeMap<i64, u64> = std::collections::BTreeMap::new();
    let mut unknown_until = i64::MIN;
    for (i, item) in items.iter().enumerate() {
        let row = item.row;
        if i == 0 || {
            let prev = items[i - 1].row;
            prev.security_id != row.security_id || prev.segment != row.segment
        } {
            taken.clear();
            unknown_until = i64::MIN;
        }
        let window = item
            .inputs
            .as_ref()
            .map(|x| (x.decision_sec.saturating_add(1), x.horizon_end()));
        let share_unknown = window.is_none_or(|(first, _)| first <= unknown_until);
        let seconds = got.remove(&row.signal_id);
        let mut computed = false;
        let mut diverged = false;
        let result = match (item.is_target, seconds, item.inputs.as_ref()) {
            (true, Some(OrderSeconds::NotRun(r)), _) => Some(r),
            (true, _, None) => Some(Recheck::not_run(RecheckKind::Unavailable(
                "no_pricing_inputs",
            ))),
            (true, _, Some(_)) if share_unknown => Some(Recheck::not_run(
                RecheckKind::Unavailable("shared_volume_unknown"),
            )),
            (true, Some(OrderSeconds::ReadFailed) | None, Some(_)) => Some(Recheck::not_run(
                RecheckKind::Unavailable("seconds_read_failed"),
            )),
            (is_target, Some(OrderSeconds::Seconds(all)), Some(inp))
                if is_target || (item.known_share && !share_unknown) =>
            {
                let decision = all.iter().copied().find(|s| s.sec == inp.decision_sec);
                let mut secs: Vec<SecondBar> = all
                    .into_iter()
                    .filter(|s| s.sec != inp.decision_sec)
                    .collect();
                let need = FillSchedule::required_capacity(inp.max_fill_seconds, 0);
                if schedule.capacity() < need {
                    schedule = FillSchedule::with_capacity(need);
                }
                computed = true;
                let r = recheck_order_shared(inp, decision, &mut secs, &mut taken, &mut schedule);
                diverged = item.known_share
                    && (item.chain_late_fill || recheck_model_paise(&r) != item.chain_model_paise);
                is_target.then_some(r)
            }
            _ => None,
        };
        // An order whose share was not worked out here (or that the chain
        // finished unpriced) leaves the overlapping later ones unknown. So
        // does one whose re-price differs from the chain's own model price
        // (LF-9 review, finding 4): the stored seconds no longer say what
        // the chain shared out, so the share it left the later orders is not
        // known (a late print amended a second after the order finished).
        // And so does one whose rows show the chain counted a late print in
        // its window (third pass): a late print that only adds VOLUME to a
        // settled second moves the share and trigger with the price unchanged.
        // Honest limit: a late print that lands after the order finished, or
        // on an order the runtime then refused (its Reject reason replaces the
        // count), is not seen here.
        if !computed || !item.known_share || diverged {
            unknown_until = match window {
                Some((_, end)) => unknown_until.max(end),
                None => i64::MAX,
            };
        }
        if let Some(r) = result {
            out.push((row.signal_id, r));
        }
    }
    out
}

/// Whether a row's reason carries the chain's late-print count
/// (`late_fill_ticks=N`, alone or before `;beyond_limit_qty=M`) with N above
/// 0. Pure, O(len).
fn carries_late_fill(reason: &str) -> bool {
    reason.split(';').any(|part| {
        part.split_once('=').is_some_and(|(k, v)| {
            k == crate::live_fill::LATE_FILL_TICKS_KEY && v.parse::<u32>().is_ok_and(|n| n > 0)
        })
    })
}

/// The model price a re-pricing gave (the chain's `model_price_paise` for
/// the same order): `None` when it was refused or not priced. Pure, O(1).
fn recheck_model_paise(r: &Recheck) -> Option<i64> {
    match &r.kind {
        RecheckKind::Priced { model_paise, .. } => *model_paise,
        _ => None,
    }
}

/// IST epoch seconds as QuestDB timestamp text (IST written as Z). Pure.
#[must_use]
pub fn ts_text(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0).map_or_else(
        || "1970-01-01T00:00:00.000000Z".to_owned(),
        |d| d.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string(),
    )
}

/// The second-read SQL for one order: its instrument's stored 1 s candles
/// from the decision second through the horizon end, a reply of at most
/// `max_s + 3` rows. Server cost: every instrument's rows in that time slice
/// are filtered (`security_id` is not indexed); see the module docs. `None` when the segment is not a plain `[A-Z_]` name (it comes from
/// the database and goes into SQL). Pure.
#[must_use]
pub fn seconds_sql(inp: &OrderInputs) -> Option<String> {
    if inp.segment.is_empty()
        || !inp
            .segment
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b == b'_')
    {
        return None;
    }
    Some(format!(
        "SELECT ts, high, low, volume FROM candles_1s WHERE security_id = {} AND segment = '{}' \
         AND feed = 'dhan' AND ts >= '{}' AND ts <= '{}' ORDER BY ts LIMIT {}",
        inp.security_id,
        inp.segment,
        ts_text(inp.decision_sec),
        ts_text(inp.horizon_end()),
        u64::from(inp.max_fill_seconds).saturating_add(3)
    ))
}

/// Parses the [`seconds_sql`] body: `(ts, high, low, volume)` rows to
/// seconds (rupees to paise as the chain converts them; the signed candle
/// volume is taken as its magnitude). A row with a bad price is skipped.
/// O(rows).
///
/// # Errors
/// The body is not JSON or has no `dataset` array.
pub fn parse_seconds_body(body: &str) -> Result<Vec<SecondBar>, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| "response is not valid JSON".to_owned())?;
    let dataset = value
        .get("dataset")
        .and_then(|d| d.as_array())
        .ok_or_else(|| "response has no `dataset` array".to_owned())?;
    let mut out = Vec::with_capacity(dataset.len());
    for raw in dataset {
        let Some(c) = raw.as_array() else {
            continue;
        };
        let (Some(ts), Some(high), Some(low), Some(vol)) = (
            c.first().and_then(parse_ts_nanos),
            c.get(1).and_then(serde_json::Value::as_f64),
            c.get(2).and_then(serde_json::Value::as_f64),
            c.get(3).and_then(json_i64),
        ) else {
            continue;
        };
        let (Ok(high_paise), Ok(low_paise)) = (rupees_to_paise(high), rupees_to_paise(low)) else {
            continue;
        };
        out.push(SecondBar {
            sec: ts.div_euclid(NANOS_PER_SEC),
            high_paise,
            low_paise,
            volume: vol.unsigned_abs(),
        });
    }
    Ok(out)
}

/// Reads one order's stored seconds through the shared probe client, under
/// [`RECHECK_QUERY_TIMEOUT_SECS`].
///
/// # Errors
/// A text reason (bad segment, client, timeout, request, status, body).
// TEST-EXEMPT: one bounded QuestDB GET; the SQL (`seconds_sql`) and the parse (`parse_seconds_body`) are unit-tested.
pub async fn read_seconds(exec_url: &str, inp: &OrderInputs) -> Result<Vec<SecondBar>, String> {
    let sql = seconds_sql(inp).ok_or_else(|| "segment not a plain name".to_owned())?;
    let client = tickvault_storage::http_client::shared_probe_client()
        .map_err(|e| format!("client build: {e}"))?;
    let limit = Duration::from_secs(RECHECK_QUERY_TIMEOUT_SECS);
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
    parse_seconds_body(&body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live_fill::fill_window::{Finished, HeldOrder, PendingBook};
    use tickvault_trading::brutex_host::InstrumentKey;

    const DEC: i64 = 1_794_441_600 + 36_000;

    fn inputs(side: Side, qty: u64, lot: u64, limit: i64) -> OrderInputs {
        OrderInputs {
            signal_id: 1,
            security_id: 2885,
            segment: "NSE_FNO".to_owned(),
            side,
            qty,
            tick_paise: 5,
            lot_size: lot,
            participation_bps: 2_500,
            max_fill_seconds: 60,
            limit_paise: limit,
            decision_sec: DEC,
            ltp_paise: 10_000,
            book: Some((10_005, 40)),
        }
    }

    fn bar(sec: i64, low: i64, high: i64, vol: u64) -> SecondBar {
        SecondBar {
            sec,
            high_paise: high,
            low_paise: low,
            volume: vol,
        }
    }

    /// Runs the chain's own `PendingBook` as the chain does: each order is
    /// held when its candle is released (decision second + 2 s), each second
    /// is handed over when it is final (second + 2 s), and every pass
    /// finishes the orders `collect_ready` names. Returns each order's fill
    /// (floored to whole lots) and model price, in `orders` order. All orders
    /// are on one instrument and share its seconds.
    fn chain_fills(
        orders: &[(OrderInputs, SecondBar)],
        secs: &[SecondBar],
    ) -> Vec<(u64, Option<i64>)> {
        use crate::live_fill::timing::BookClock;
        const NS: i64 = 1_000_000_000;
        let window = orders
            .iter()
            .map(|o| o.0.max_fill_seconds)
            .max()
            .unwrap_or(60);
        let mut book = PendingBook::<usize>::new(window, 1);
        let clock = BookClock::new(&[1, 60], 1_000, 2_500);
        let mut out = vec![(0, None); orders.len()];
        let mut inserted = vec![false; orders.len()];
        let mut done = vec![false; orders.len()];
        let mut next = 0usize;
        let mut ready = Vec::with_capacity(crate::live_fill::fill_window::MAX_PENDING_ORDERS);
        let lo = orders.iter().map(|o| o.0.decision_sec).min().unwrap_or(DEC);
        let hi = orders
            .iter()
            .map(|o| o.0.horizon_end())
            .max()
            .unwrap_or(DEC)
            + 6;
        for x in lo..=hi {
            for (i, (inp, decision)) in orders.iter().enumerate() {
                if !inserted[i] && inp.decision_sec + 2 <= x {
                    inserted[i] = true;
                    book.insert(HeldOrder {
                        key_index: 0,
                        key: InstrumentKey {
                            security_id: 2885,
                            exchange_segment: 2,
                        },
                        frame_index: 1,
                        candle_open_secs: inp.decision_sec - 59,
                        candle_end_secs: inp.decision_sec + 1,
                        side: inp.side,
                        qty: inp.qty,
                        tick_paise: inp.tick_paise,
                        participation_bps: inp.participation_bps,
                        decision_bar: *decision,
                        book: inp.book,
                        limit_paise: inp.limit_paise,
                        reduce_only: false,
                        meta: i,
                    })
                    .expect("room");
                }
            }
            while next < secs.len() && secs[next].sec + 2 <= x {
                book.on_second(0, secs[next]);
                next += 1;
            }
            book.collect_ready(x * NS, &clock, &mut ready);
            for slot in ready.clone() {
                match book.finish(slot) {
                    Some(Finished::Filled { order, summary, .. }) => {
                        let lot = orders[order.meta].0.lot_size.max(1);
                        out[order.meta] = ((summary.filled_qty / lot) * lot, summary.vwap_paise);
                        done[order.meta] = true;
                    }
                    Some(Finished::Refused { order, .. }) => done[order.meta] = true,
                    other => panic!("unexpected {other:?}"),
                }
            }
        }
        assert!(done.iter().all(|d| *d), "every order finished");
        out
    }

    /// The same orders through `recheck_order_shared`, in order, sharing one
    /// instrument's seconds: each order's (fill, model price).
    fn recheck_fills(
        orders: &[(OrderInputs, SecondBar)],
        secs: &[SecondBar],
    ) -> Vec<(u64, Option<i64>)> {
        let mut taken = std::collections::BTreeMap::new();
        let mut out = Vec::new();
        for (inp, decision) in orders {
            let mut s = secs.to_vec();
            let mut sched = FillSchedule::with_capacity(FillSchedule::required_capacity(
                inp.max_fill_seconds,
                0,
            ));
            let r = recheck_order_shared(inp, Some(*decision), &mut s, &mut taken, &mut sched);
            out.push(match r.kind {
                RecheckKind::Priced {
                    filled_qty,
                    model_paise,
                    ..
                } => (filled_qty, model_paise),
                RecheckKind::Refused(_) => (0, None),
                other => panic!("unexpected {other:?}"),
            });
        }
        out
    }

    fn chain_fill(
        inp: &OrderInputs,
        decision: SecondBar,
        secs: &[SecondBar],
    ) -> (u64, Option<i64>) {
        chain_fills(&[(inp.clone(), decision)], secs)[0]
    }

    /// LF-9 finding 1: an order whose first fill seconds are beyond its LIMIT
    /// is not finished early on their volume. BUY 100 at 25%: second 1 trades
    /// 400 (a raw share of 100, enough for the whole order) but it and second
    /// 2 print above the limit; seconds 3 to 60 trade inside it. The order
    /// fills from seconds 3 on, where it used to finish after second 2 with
    /// nothing filled (`no_fill_in_window`). The re-pricing agrees.
    #[test]
    fn test_regression_early_finish_counts_only_volume_inside_the_limit() {
        let mut inp = inputs(Side::Buy, 100, 1, 10_100);
        inp.book = None;
        let decision = bar(DEC, 9_995, 10_005, 30);
        let mut secs = vec![
            bar(DEC + 1, 10_150, 10_200, 400),
            bar(DEC + 2, 10_150, 10_200, 400),
        ];
        for i in 3..=60 {
            secs.push(bar(DEC + i, 10_000, 10_020, 40));
        }
        let chain = chain_fill(&inp, decision, &secs);
        assert_eq!(
            chain.0, 100,
            "the in-limit seconds fill the order: {chain:?}"
        );
        assert!(
            chain.1.is_some_and(|p| p <= 10_100),
            "never beyond the limit"
        );
        assert_eq!(recheck_fills(&[(inp, decision)], &secs), vec![chain]);
    }

    /// LF-9 finding 3: two orders held on one instrument share each second's
    /// 25% cap, the earlier first. Second 1 trades 400 (a cap of 100 shares)
    /// and nothing trades after: the first order takes all 100, the second
    /// gets nothing, where each used to fill 100 (twice the cap). With more
    /// seconds, the second order fills from what the first left. The
    /// re-pricing gives every order the same fill.
    #[test]
    fn test_regression_held_orders_on_one_instrument_share_the_participation_cap() {
        let mut a = inputs(Side::Buy, 100, 1, 10_100);
        a.book = None;
        let mut b = a.clone();
        b.signal_id = 2;
        let decision = bar(DEC, 9_995, 10_005, 30);
        let one = vec![bar(DEC + 1, 10_000, 10_020, 400)];
        let orders = [(a.clone(), decision), (b.clone(), decision)];
        let chain = chain_fills(&orders, &one);
        assert_eq!(chain[0].0, 100);
        assert_eq!(
            chain[1].0, 0,
            "the cap was used by the first order: {chain:?}"
        );
        assert_eq!(recheck_fills(&orders, &one), chain);
        // Odd participation: the volume a share uses is rounded up, so the
        // two orders never take more than the second's cap between them.
        let mut a3 = a.clone();
        a3.participation_bps = 3_333;
        a3.qty = 7;
        let mut b3 = b.clone();
        b3.participation_bps = 3_333;
        b3.qty = 50;
        let few = vec![bar(DEC + 1, 10_000, 10_020, 31)];
        let orders3 = [(a3, decision), (b3, decision)];
        let chain3 = chain_fills(&orders3, &few);
        // 31 × 33.33% = 10 shares in all. The first takes 7, using the
        // ⌈7 / 33.33%⌉ = 22 units of volume they need; the 9 left give the
        // second 2. Rounding the volume up costs the later order a share,
        // never lets the two take more than the cap.
        assert!(chain3[0].0 + chain3[1].0 <= 10, "{chain3:?}");
        assert_eq!(chain3[0].0, 7);
        assert_eq!(chain3[1].0, 2);
        assert_eq!(recheck_fills(&orders3, &few), chain3);
        // Overlapping windows, staggered decisions, plenty of volume later.
        let mut b2 = b.clone();
        b2.decision_sec = DEC + 10;
        let decision2 = bar(DEC + 10, 9_995, 10_005, 30);
        let many: Vec<SecondBar> = (1..=80)
            .map(|i| bar(DEC + i, 10_000, 10_020, 120))
            .collect();
        let orders2 = [(a, decision), (b2, decision2)];
        let chain2 = chain_fills(&orders2, &many);
        assert_eq!(chain2[0].0, 100);
        assert_eq!(chain2[1].0, 100);
        assert_eq!(recheck_fills(&orders2, &many), chain2);
    }

    proptest::proptest! {
        /// LF-9: on random seconds and one to three orders on one instrument
        /// (random sides, sizes, limits, books, participation and staggered
        /// decisions), the chain's held-order book and the daily re-pricing
        /// give every order the same fill and model price, and the orders
        /// never fill more between them than the instrument's participation
        /// cap over the seconds.
        #[test]
        fn proptest_chain_and_recheck_order_shared_share_and_price_identically(
            specs in proptest::collection::vec(
                (proptest::bool::ANY, 1u64..600, -300i64..300, 0i64..20, 1u32..=10_000,
                 proptest::option::of((9_800i64..10_200, 1u64..200))),
                1..4,
            ),
            bars in proptest::collection::vec((1i64..3, 9_700i64..10_300, 0i64..200, 0u64..900), 1..90),
        ) {
            let mut secs = Vec::with_capacity(bars.len());
            let mut t = DEC;
            for (gap, low, span, vol) in &bars {
                t += gap;
                secs.push(bar(t, *low, low + span, *vol));
            }
            let part = specs[0].4;
            let mut orders = Vec::new();
            for (i, (buy, qty, limit_off, stagger, _, book)) in specs.iter().enumerate() {
                let mut inp = inputs(if *buy { Side::Buy } else { Side::Sell }, *qty, 1, 10_000 + limit_off);
                inp.signal_id = i as i64 + 1;
                inp.participation_bps = part;
                inp.book = *book;
                inp.decision_sec = DEC + stagger + i as i64;
                let d = bar(inp.decision_sec, 9_990, 10_010, 10);
                orders.push((inp, d));
            }
            orders.sort_by_key(|o| o.0.decision_sec);
            let chain = chain_fills(&orders, &secs);
            let re = recheck_fills(&orders, &secs);
            proptest::prop_assert_eq!(&chain, &re);
            let cap: u64 = secs
                .iter()
                .map(|s| s.volume * u64::from(part) / 10_000)
                .sum();
            let filled: u64 = chain.iter().map(|c| c.0).sum();
            proptest::prop_assert!(filled <= cap, "{} > {}", filled, cap);
        }
    }

    /// LF-9 finding 3: the report's walk (`walk_items`, `wants_seconds`,
    /// `recheck_walk`) shares an instrument's seconds out over its held
    /// orders in signal order, exactly as the chain does: two overlapping
    /// orders re-price to the chain's fills. A third held order that is not
    /// compared (a runtime rejection) but overlaps a later compared one is
    /// read for its share; one on another instrument is independent.
    #[test]
    fn test_walk_items_wants_seconds_recheck_walk_share_in_signal_order() {
        let decision_row = |sid: i64, security_id: i64, dec: i64| EventRow {
            event: "decision".to_owned(),
            signal_id: sid,
            security_id,
            segment: "NSE_FNO".to_owned(),
            side: "BUY".to_owned(),
            qty: 100,
            candle_end_ns: Some((dec + 1) * NANOS_PER_SEC),
            ltp_paise: Some(10_000),
            limit_paise: Some(10_100),
            tick_paise: Some(5),
            lot_size: Some(1),
            participation_bps: Some(2_500),
            max_fill_seconds: Some(60),
            ..EventRow::default()
        };
        // Order 1's chain fill: 100 at 10,025 (the 10,020 high plus a tick);
        // the re-pricing gives the same, so its share is known (finding 4).
        let sent_one = EventRow {
            event: "sent".to_owned(),
            signal_id: 1,
            security_id: 2885,
            segment: "NSE_FNO".to_owned(),
            model_price_paise: Some(10_025),
            ..EventRow::default()
        };
        let rows = vec![
            decision_row(1, 2885, DEC),
            decision_row(2, 2885, DEC),
            decision_row(3, 7, DEC),
            sent_one,
        ];
        let targets: std::collections::BTreeSet<i64> = [2, 3].into_iter().collect();
        // Order 1 was held and priced but rejected by the runtime (known
        // share, not compared).
        let held: std::collections::BTreeMap<i64, bool> =
            [(1, true), (2, true), (3, true)].into_iter().collect();
        let items = walk_items(&rows, &targets, &held);
        assert_eq!(
            items.iter().map(|i| i.row.signal_id).collect::<Vec<_>>(),
            vec![3, 1, 2],
            "by instrument, then signal"
        );
        assert!(items[1].wants_seconds() && items[1].needed_later && !items[1].is_target);
        assert!(items[2].wants_seconds() && items[2].is_target);
        let decision = bar(DEC, 9_995, 10_005, 30);
        let one = vec![decision, bar(DEC + 1, 10_000, 10_020, 400)];
        let mut got: std::collections::BTreeMap<i64, OrderSeconds> = items
            .iter()
            .filter(|i| i.wants_seconds())
            .map(|i| (i.row.signal_id, OrderSeconds::Seconds(one.clone())))
            .collect();
        let out = recheck_walk(&items, &mut got);
        let filled = |sid: i64| match &out
            .iter()
            .find(|(s, _)| *s == sid)
            .expect("rechecked")
            .1
            .kind
        {
            RecheckKind::Priced { filled_qty, .. } => *filled_qty,
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(out.len(), 2, "only the compared orders: {out:?}");
        assert_eq!(filled(2), 0, "order 1 took the second's whole 25% cap");
        assert_eq!(filled(3), 100, "another instrument is independent");
        let mut inp = inputs(Side::Buy, 100, 1, 10_100);
        inp.book = None;
        let chain = chain_fills(&[(inp.clone(), decision), (inp, decision)], &one[1..]);
        assert_eq!(chain[1].0, filled(2), "the chain agrees");
        // Order 1 finished unpriced: order 2's share is unknown.
        let held: std::collections::BTreeMap<i64, bool> =
            [(1, false), (2, true), (3, true)].into_iter().collect();
        let items = walk_items(&rows, &targets, &held);
        assert!(!items[1].wants_seconds());
        let mut got: std::collections::BTreeMap<i64, OrderSeconds> = items
            .iter()
            .filter(|i| i.wants_seconds())
            .map(|i| (i.row.signal_id, OrderSeconds::Seconds(one.clone())))
            .collect();
        let out = recheck_walk(&items, &mut got);
        assert!(out.contains(&(
            2,
            Recheck::not_run(RecheckKind::Unavailable("shared_volume_unknown"))
        )));
    }

    /// LF-9 review, finding 4: a late print amended a stored second after
    /// order A finished, so A's re-price (here no fill: the amended high is
    /// past its LIMIT) differs from the chain's own model price for A (it
    /// filled at 10,045 on the seconds it saw). A's real share of that second
    /// is not known from the stored seconds, so the later overlapping order B
    /// reads `shared_volume_unknown` rather than a false mismatch priced on
    /// volume the chain had already given A. With A's re-price agreeing with
    /// the chain, B is priced as before.
    #[test]
    fn test_regression_a_diverged_earlier_reprice_leaves_later_orders_unknown() {
        let decision_row = |sid: i64, side: &str, limit: i64| EventRow {
            event: "decision".to_owned(),
            signal_id: sid,
            security_id: 2885,
            segment: "NSE_FNO".to_owned(),
            side: side.to_owned(),
            qty: 100,
            candle_end_ns: Some((DEC + 1) * NANOS_PER_SEC),
            ltp_paise: Some(10_000),
            limit_paise: Some(limit),
            tick_paise: Some(5),
            lot_size: Some(1),
            participation_bps: Some(2_500),
            max_fill_seconds: Some(60),
            ..EventRow::default()
        };
        let sent_a = |model: Option<i64>| EventRow {
            event: "sent".to_owned(),
            signal_id: 1,
            security_id: 2885,
            segment: "NSE_FNO".to_owned(),
            model_price_paise: model,
            ..EventRow::default()
        };
        // The stored (late-amended) second: high 10,060, past A's 10,050.
        let stored = vec![
            bar(DEC, 9_995, 10_005, 30),
            bar(DEC + 1, 10_000, 10_060, 400),
        ];
        let targets: std::collections::BTreeSet<i64> = [2].into_iter().collect();
        let held: std::collections::BTreeMap<i64, bool> =
            [(1, true), (2, true)].into_iter().collect();
        let run = |chain_model_a: Option<i64>| {
            let rows = vec![
                decision_row(1, "BUY", 10_050),
                decision_row(2, "SELL", 9_900),
                sent_a(chain_model_a),
            ];
            let items = walk_items(&rows, &targets, &held);
            assert_eq!(items[0].chain_model_paise, chain_model_a);
            let mut got: std::collections::BTreeMap<i64, OrderSeconds> = items
                .iter()
                .filter(|i| i.wants_seconds())
                .map(|i| (i.row.signal_id, OrderSeconds::Seconds(stored.clone())))
                .collect();
            recheck_walk(&items, &mut got)
        };
        // The chain filled A at 10,045 on the seconds it saw; the stored
        // seconds give A nothing: B's share is unknown.
        let out = run(Some(10_045));
        assert_eq!(
            out,
            vec![(
                2,
                Recheck::not_run(RecheckKind::Unavailable("shared_volume_unknown"))
            )]
        );
        // A's re-price agrees with the chain (no fill): B is priced.
        let out = run(None);
        assert!(
            matches!(
                out.as_slice(),
                [(
                    2,
                    Recheck {
                        kind: RecheckKind::Priced {
                            filled_qty: 100,
                            ..
                        },
                        ..
                    }
                )]
            ),
            "{out:?}"
        );
    }

    /// LF-9 review, third pass: a late print that only adds VOLUME to a
    /// second the chain had already settled for order A moves A's re-priced
    /// share and trigger with its price unchanged (a flat market prices every
    /// slice the same). The chain saw 400 at DEC + 5 and took 100 there and
    /// 100 at DEC + 10 (all of DEC + 10's cap); the stored DEC + 5 holds 800,
    /// so the re-price gives A all 200 at DEC + 5. A's rows carry the chain's
    /// `late_fill_ticks=1`, so the later overlapping order B reads
    /// `shared_volume_unknown` instead of being re-priced with the whole of
    /// DEC + 10 (a false verdict on an order that had no late print). With no
    /// late print on A's rows, B is priced as before.
    #[test]
    fn test_regression_a_late_volume_amendment_with_equal_price_leaves_later_orders_unknown() {
        let decision_row = |sid: i64, dec: i64, qty: i64| EventRow {
            event: "decision".to_owned(),
            signal_id: sid,
            security_id: 2885,
            segment: "NSE_FNO".to_owned(),
            side: "BUY".to_owned(),
            qty,
            candle_end_ns: Some((dec + 1) * NANOS_PER_SEC),
            ltp_paise: Some(10_000),
            limit_paise: Some(10_100),
            tick_paise: Some(5),
            lot_size: Some(1),
            participation_bps: Some(2_500),
            max_fill_seconds: Some(60),
            ..EventRow::default()
        };
        // Flat market: every fill second prices at 10,020 + one tick.
        let mut stored = vec![
            bar(DEC, 9_995, 10_005, 30),
            bar(DEC + 5, 10_000, 10_020, 800),
            bar(DEC + 9, 9_995, 10_005, 30),
        ];
        stored.extend((10..=20).map(|i| bar(DEC + i, 10_000, 10_020, 400)));
        let targets: std::collections::BTreeSet<i64> = [2].into_iter().collect();
        let held: std::collections::BTreeMap<i64, bool> =
            [(1, true), (2, true)].into_iter().collect();
        let run = |reason_a: Option<&str>| {
            let sent_a = EventRow {
                event: "sent".to_owned(),
                signal_id: 1,
                security_id: 2885,
                segment: "NSE_FNO".to_owned(),
                model_price_paise: Some(10_025),
                reason: reason_a.map(str::to_owned),
                ..EventRow::default()
            };
            let rows = vec![
                decision_row(1, DEC, 200),
                decision_row(2, DEC + 9, 100),
                sent_a,
            ];
            let items = walk_items(&rows, &targets, &held);
            assert_eq!(items[0].chain_late_fill, reason_a.is_some());
            let mut got: std::collections::BTreeMap<i64, OrderSeconds> = items
                .iter()
                .filter(|i| i.wants_seconds())
                .map(|i| (i.row.signal_id, OrderSeconds::Seconds(stored.clone())))
                .collect();
            recheck_walk(&items, &mut got)
        };
        // The re-price of A agrees with the chain's price (10,025), yet the
        // late print moved its share: B's share is unknown.
        for reason in ["late_fill_ticks=1", "late_fill_ticks=2;beyond_limit_qty=4"] {
            assert_eq!(
                run(Some(reason)),
                vec![(
                    2,
                    Recheck::not_run(RecheckKind::Unavailable("shared_volume_unknown"))
                )],
                "{reason}"
            );
        }
        // No late print on A: B is priced.
        let out = run(None);
        assert!(
            matches!(
                out.as_slice(),
                [(
                    2,
                    Recheck {
                        kind: RecheckKind::Priced { .. },
                        ..
                    }
                )]
            ),
            "{out:?}"
        );
        // Only a count above 0 under the chain's own key is a late print.
        assert!(carries_late_fill("late_fill_ticks=3"));
        assert!(carries_late_fill("late_fill_ticks=3;beyond_limit_qty=1"));
        assert!(!carries_late_fill("late_fill_ticks=0"));
        assert!(!carries_late_fill("beyond_limit_qty=5"));
        assert!(!carries_late_fill("risk:halt"));
        assert_eq!(crate::live_fill::LATE_FILL_TICKS_KEY, "late_fill_ticks");
    }

    /// LF-9 review (second pass): a held order whose Decision row (or its
    /// Built row too) was dropped by the event channel is still in the walk,
    /// with its share unknown, so the later overlapping compared order reads
    /// `shared_volume_unknown` instead of being re-priced as if the earlier
    /// order took nothing (a definite, false verdict).
    #[test]
    fn test_regression_a_held_order_missing_its_decision_row_leaves_later_orders_unknown() {
        let row = |event: &str, sid: i64, seq: i64| EventRow {
            event: event.to_owned(),
            signal_id: sid,
            seq,
            security_id: 2885,
            segment: "NSE_FNO".to_owned(),
            ..EventRow::default()
        };
        let decision_two = EventRow {
            event: "decision".to_owned(),
            signal_id: 2,
            security_id: 2885,
            segment: "NSE_FNO".to_owned(),
            side: "BUY".to_owned(),
            qty: 100,
            candle_end_ns: Some((DEC + 1) * NANOS_PER_SEC),
            ltp_paise: Some(10_000),
            limit_paise: Some(10_100),
            tick_paise: Some(5),
            lot_size: Some(1),
            participation_bps: Some(2_500),
            max_fill_seconds: Some(60),
            ..EventRow::default()
        };
        let stored = vec![
            bar(DEC, 9_995, 10_005, 30),
            bar(DEC + 1, 10_000, 10_020, 400),
        ];
        let targets: std::collections::BTreeSet<i64> = [2].into_iter().collect();
        let earlier_rows = [
            // Its Built row and a finishing Reject (tainted after it shared).
            vec![row("built", 1, 1), row("reject", 1, 2)],
            // No Built row either: only a finishing Reject.
            vec![row("reject", 1, 2)],
            // No Built row either: only the runtime's rows of its Execute.
            vec![row("sent", 1, 2), row("fill", 1, 3)],
        ];
        for earlier in earlier_rows {
            let mut rows = vec![decision_two.clone(), row("built", 2, 1)];
            rows.extend(earlier.iter().cloned());
            let held = crate::live_fill_report::build::held_share_known(&rows);
            assert!(
                held.contains_key(&1),
                "the earlier order was held: {earlier:?}"
            );
            let items = walk_items(&rows, &targets, &held);
            assert_eq!(
                items.iter().map(|i| i.row.signal_id).collect::<Vec<_>>(),
                vec![1, 2],
                "{earlier:?}"
            );
            let mut got: std::collections::BTreeMap<i64, OrderSeconds> = items
                .iter()
                .filter(|i| i.wants_seconds())
                .map(|i| (i.row.signal_id, OrderSeconds::Seconds(stored.clone())))
                .collect();
            let out = recheck_walk(&items, &mut got);
            assert_eq!(
                out,
                vec![(
                    2,
                    Recheck::not_run(RecheckKind::Unavailable("shared_volume_unknown"))
                )],
                "{earlier:?}"
            );
        }
        // A refusal before the hold (Reject seq 1, no Built) took no share.
        let rows = vec![row("reject", 1, 1), decision_two, row("built", 2, 1)];
        let held = crate::live_fill_report::build::held_share_known(&rows);
        assert!(!held.contains_key(&1));
    }

    /// LF-7: re-pricing the chain's own seconds gives the chain's fill and
    /// model price, including when the LIMIT binds and with an odd-share lot
    /// floor, for both sides.
    #[test]
    fn test_recheck_matches_chain_on_same_seconds() {
        let secs: Vec<SecondBar> = (1..=70)
            .map(|i| {
                bar(
                    DEC + i,
                    9_990 + (i % 7) * 5,
                    10_010 + (i % 5) * 10,
                    40 + (i as u64 % 3) * 7,
                )
            })
            .collect();
        let decision = bar(DEC, 9_995, 10_005, 30);
        for side in [Side::Buy, Side::Sell] {
            for (qty, lot, limit) in [
                (50, 25, 10_100),  // limit loose
                (300, 75, 10_030), // limit binds for a buy
                (55, 25, 9_985),   // limit binds for a sell, odd shares
                (2_000, 1, 10_050),
            ] {
                let inp = inputs(side, qty, lot, limit);
                let (chain_qty, chain_model) = chain_fill(&inp, decision, &secs);
                let mut s = secs.clone();
                let mut out = FillSchedule::with_capacity(FillSchedule::required_capacity(60, 0));
                let r = recheck_order(&inp, Some(decision), &mut s, &mut out);
                match r.kind {
                    RecheckKind::Priced {
                        filled_qty,
                        model_paise,
                        fill_offsets,
                        ..
                    } => {
                        assert_eq!(filled_qty, chain_qty, "{side:?} {qty} {lot} {limit}");
                        assert_eq!(model_paise, chain_model, "{side:?} {qty} {lot} {limit}");
                        assert_eq!(fill_offsets.iter().map(|(_, q)| q).sum::<u64>(), filled_qty);
                    }
                    RecheckKind::Refused(_) => assert_eq!(chain_qty, 0),
                    other => panic!("unexpected {other:?}"),
                }
            }
        }
    }

    #[test]
    fn test_recheck_order_with_no_stored_seconds_is_refused_and_counts_none() {
        let inp = inputs(Side::Buy, 50, 25, 10_100);
        let mut out = FillSchedule::with_capacity(FillSchedule::required_capacity(60, 0));
        let r = recheck_order(&inp, None, &mut Vec::new(), &mut out);
        assert_eq!(r.seconds_seen, 0);
        assert!(matches!(r.kind, RecheckKind::Refused(_)), "{r:?}");
        assert_eq!(
            Recheck::not_run(RecheckKind::Skipped("cap")).seconds_seen,
            0
        );
    }

    #[test]
    fn test_order_inputs_from_decision_and_horizon_end_need_every_pricing_input() {
        let row = EventRow {
            event: "decision".into(),
            signal_id: 1,
            side: "BUY".into(),
            qty: 50,
            segment: "NSE_FNO".into(),
            security_id: 2885,
            candle_end_ns: Some((DEC + 1) * NANOS_PER_SEC),
            ltp_paise: Some(10_000),
            limit_paise: Some(10_100),
            best_ask_paise: Some(10_005),
            best_ask_qty: Some(40),
            tick_paise: Some(5),
            lot_size: Some(25),
            participation_bps: Some(2_500),
            max_fill_seconds: Some(60),
            ..EventRow::default()
        };
        let inp = OrderInputs::from_decision(&row).expect("inputs");
        assert_eq!(inp, inputs(Side::Buy, 50, 25, 10_100));
        assert_eq!(inp.horizon_end(), DEC + 61);
        for strip in [
            |r: &mut EventRow| r.tick_paise = None,
            |r: &mut EventRow| r.lot_size = Some(0),
            |r: &mut EventRow| r.participation_bps = Some(10_001),
            |r: &mut EventRow| r.max_fill_seconds = None,
            |r: &mut EventRow| r.limit_paise = None,
            |r: &mut EventRow| r.candle_end_ns = None,
            |r: &mut EventRow| r.side = "HOLD".into(),
            |r: &mut EventRow| r.qty = 0,
        ] {
            let mut r = row.clone();
            strip(&mut r);
            assert!(OrderInputs::from_decision(&r).is_none());
        }
        let mut sell = row;
        sell.side = "SELL".into();
        assert_eq!(
            OrderInputs::from_decision(&sell).map(|i| i.book),
            Some(None)
        );
    }

    #[test]
    fn test_seconds_sql_refuses_an_unsafe_segment_and_ts_text_is_iso() {
        let inp = inputs(Side::Buy, 50, 25, 10_100);
        let sql = seconds_sql(&inp).expect("sql");
        assert!(sql.contains("FROM candles_1s WHERE security_id = 2885 AND segment = 'NSE_FNO'"));
        assert!(sql.contains("ts >= '2026-11-12T10:00:00.000000Z'"));
        assert!(sql.contains("ts <= '2026-11-12T10:01:01.000000Z'"));
        assert!(sql.ends_with("LIMIT 63"));
        let mut bad = inp;
        bad.segment = "NSE'--".into();
        assert!(seconds_sql(&bad).is_none());
        bad.segment = String::new();
        assert!(seconds_sql(&bad).is_none());
        assert_eq!(ts_text(0), "1970-01-01T00:00:00.000000Z");
    }

    #[test]
    fn test_parse_seconds_body_takes_magnitude_and_skips_bad_prices() {
        let body = r#"{"dataset":[
            ["2026-11-12T10:00:01.000000Z", 100.05, 99.9, -120],
            ["2026-11-12T10:00:02.000000Z", "x", 99.9, 5],
            ["2026-11-12T10:00:03.000000Z", 0.0, 99.9, 5]
        ]}"#;
        let secs = parse_seconds_body(body).expect("parse");
        assert_eq!(secs, vec![bar(DEC + 1, 9_990, 10_005, 120)]);
        assert!(parse_seconds_body("nope").is_err());
        assert!(parse_seconds_body("{}").is_err());
    }

    #[tokio::test]
    async fn test_read_seconds_unreachable_or_bad_segment_is_an_error() {
        let inp = inputs(Side::Buy, 50, 25, 10_100);
        assert!(read_seconds("http://127.0.0.1:1/exec", &inp).await.is_err());
        let mut bad = inp;
        bad.segment = "x".into();
        assert!(read_seconds("http://127.0.0.1:1/exec", &bad).await.is_err());
    }

    /// Review finding 13 (LF-7): a stored `max_fill_seconds` past the config
    /// ceiling sized a `Vec` (about 51 GB at `i32::MAX`) and the read LIMIT.
    /// It now reads as no pricing inputs; the ceiling itself still prices.
    #[test]
    fn test_order_inputs_from_decision_refuses_max_fill_seconds_past_the_ceiling() {
        let row = EventRow {
            event: "decision".into(),
            signal_id: 1,
            side: "BUY".into(),
            qty: 50,
            segment: "NSE_FNO".into(),
            security_id: 2885,
            candle_end_ns: Some((DEC + 1) * NANOS_PER_SEC),
            ltp_paise: Some(10_000),
            limit_paise: Some(10_100),
            tick_paise: Some(5),
            lot_size: Some(25),
            participation_bps: Some(2_500),
            max_fill_seconds: Some(i64::from(i32::MAX)),
            ..EventRow::default()
        };
        assert!(OrderInputs::from_decision(&row).is_none());
        let past = EventRow {
            max_fill_seconds: Some(i64::from(LIVE_FILL_MAX_FILL_SECONDS_CEILING) + 1),
            ..row.clone()
        };
        assert!(OrderInputs::from_decision(&past).is_none());
        let at = EventRow {
            max_fill_seconds: Some(i64::from(LIVE_FILL_MAX_FILL_SECONDS_CEILING)),
            ..row
        };
        let inp = OrderInputs::from_decision(&at).expect("the ceiling itself prices");
        assert_eq!(inp.max_fill_seconds, LIVE_FILL_MAX_FILL_SECONDS_CEILING);
    }

    /// Review finding 11: `Recheck::not_run` carries no seen seconds and the
    /// kind it was given.
    #[test]
    fn test_not_run_has_zero_seconds_seen_and_keeps_its_kind() {
        let r = Recheck::not_run(RecheckKind::Skipped("order_cap"));
        assert_eq!(r.seconds_seen, 0);
        assert_eq!(r.kind, RecheckKind::Skipped("order_cap"));
        let u = Recheck::not_run(RecheckKind::Unavailable("catch_up_rebuild"));
        assert_eq!(u.seconds_seen, 0);
        assert_eq!(u.kind, RecheckKind::Unavailable("catch_up_rebuild"));
    }
}

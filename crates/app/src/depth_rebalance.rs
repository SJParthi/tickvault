//! What the per-minute depth rebalance READS — the source layer between the
//! database and the two swap engines.
//!
//! # Why this module exists (2026-08-26)
//!
//! Three pieces of the minute-by-minute depth rebalance already exist and are
//! tested: the at-the-money tracker and its planner ([`crate::depth200_atm`]),
//! the top-mover socket, and the gainer/loser ranking ([`crate::movers`]).
//! Every one of them is a PURE function over data it is handed.
//!
//! Nothing handed them anything. They were, in the honest phrase, inert — not
//! broken, just never fed. This module is the feeding.
//!
//! # The two sources, and why only one needs new SQL
//!
//! | Engine needs | Comes from | New query? |
//! |---|---|---|
//! | Index chain snapshot (spot + CE/PE per strike) | the `DepthCandidate` slice the attach already builds every retry | **no** |
//! | Today's stock moves, ranked | `candles_1m.close_pct_from_prev_day` joined to the lifecycle master for the symbol | **yes** |
//!
//! Reusing `DepthCandidate` for the chain half is not a shortcut. It is the
//! same slice `select_depth_universe` consumes, so the strikes the rebalance
//! reasons about are *by construction* the strikes the attach would have
//! chosen. A second query for the same facts could drift from the first, and a
//! drift here moves a socket onto a contract the selector never considered.
//!
//! # The percentage this ranks on
//!
//! `close_pct_from_prev_day` — the move against YESTERDAY'S CLOSE. That is the
//! column the operator confirmed on 2026-08-26 when he corrected the labelling,
//! and [`crate::movers`] records at length why the other column produces a
//! plausible, completely different list. This module's query names that column
//! and no other.
//!
//! # This module is pure
//!
//! It builds query strings and parses response bodies. It opens no socket and
//! holds no state, so every edge below is a unit test rather than a live
//! surprise.

use std::collections::HashMap;

use tickvault_common::types::ExchangeSegment;
use tickvault_core::websocket::pool_supervisor::{LiveSubscriptionCommand, SubscribeInstrument};

use crate::depth200_atm::{
    ChainMinute, Depth200AtmConfig, Depth200AtmTracker, NoTopMoverSwitch, PlannedSwap, StrikePair,
    TOP_MOVER_CONFIRM_OBSERVATIONS, TopMoverPick, TopMoverSocket, plan_swaps,
};
use crate::dhan_depth_universe::{DepthCandidate, contract_segment_for_underlying};
use crate::dhan_feed_stack::ist_second_of_day_now;
use crate::movers::StockMove;

/// The segment a STOCK option trades in.
///
/// Not derived per row, and that is deliberate. [`crate::dhan_depth_universe::contract_segment_for_underlying`]
/// is fail-closed and INDEX-only by design — it refuses `RELIANCE` rather than
/// guessing, and widening it would blunt the refusal that makes it useful.
///
/// For stocks the answer is a property of this system's scope rather than of
/// the row: the spot universe is NSE-only by construction (the 2026-08-22
/// narrowing to NSE indices + Nifty Total Market, with BSE excluded and two
/// tests pinning it), so every F&O stock reachable here trades its options in
/// `NSE_FNO`. It is also the only stock-option segment that CAN carry a depth
/// book — `segment_supports_depth` is true for `NSE_FNO` and false for
/// `BSE_FNO`, because Dhan serves depth on NSE only.
pub const STOCK_OPTION_SEGMENT: ExchangeSegment = ExchangeSegment::NseFno;

/// The segment the movers query reads.
///
/// Pinned in the SQL *and* asserted here rather than parsed back out of the
/// response. One place to drift is better than two, and a mis-parsed segment
/// would produce a well-formed ranking of the wrong instruments — id 13 is
/// NIFTY in `IDX_I` and an unrelated cash stock near ₹7,600 in `NSE_EQ`.
pub const MOVER_UNDERLYING_SEGMENT: ExchangeSegment = ExchangeSegment::NseEquity;

/// One stock's move, with the symbol kept alongside it.
///
/// [`StockMove`] carries only the numeric identity, which is all the ranking
/// needs. The symbol is what joins a winner back to its option ladder — the
/// contract rows key on the underlying's SYMBOL, not its id — so it is carried
/// here and dropped at the ranking boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct MoverRow {
    /// Dhan `SecurityId` of the underlying stock.
    pub security_id: u64,
    /// Always [`MOVER_UNDERLYING_SEGMENT`]; see that constant.
    pub segment: ExchangeSegment,
    /// The stock's trading symbol, as the lifecycle master spells it.
    pub symbol: String,
    /// `close_pct_from_prev_day` — the move against yesterday's close.
    pub pct_change: f64,
}

impl MoverRow {
    /// The identity-only view the ranking consumes.
    #[must_use]
    pub const fn to_move(&self) -> StockMove {
        StockMove {
            security_id: self.security_id,
            segment: self.segment,
            pct_change: self.pct_change,
        }
    }
}

/// One underlying's chain view, owning its strikes.
///
/// [`ChainMinute`] borrows its pairs so the tracker can read a slice without
/// copying. Something has to own that slice for the duration of a minute, and
/// this is it.
#[derive(Debug, Clone, PartialEq)]
pub struct OwnedChainMinute {
    /// Canonical underlying symbol.
    pub underlying: String,
    /// Underlying spot at this snapshot.
    pub spot: f64,
    /// Every strike carrying BOTH legs, ascending.
    pub pairs: Vec<StrikePair>,
}

impl OwnedChainMinute {
    /// The borrowed view the tracker takes.
    #[must_use]
    pub fn as_minute(&self) -> ChainMinute<'_> {
        ChainMinute {
            underlying: &self.underlying,
            spot: self.spot,
            pairs: &self.pairs,
        }
    }
}

/// Strike in paise from a rupee `f64`, or `None` if it cannot be one.
///
/// Paise because the tracker compares strikes for EQUALITY, and two chain rows
/// for one strike must never fail to group through float drift. Rounding
/// rather than truncating: a strike arriving as `24499.999999` is 24500, and
/// truncation would file it one paise low and split the pair.
fn strike_paise(strike: f64) -> Option<i64> {
    if !strike.is_finite() || strike <= 0.0 {
        return None;
    }
    let paise = (strike * 100.0).round();
    // Far outside any real strike, but a NaN-free finite check is not enough
    // on its own: an absurd value would cast to a saturated i64 and compare
    // equal to another absurd value, silently pairing two different strikes.
    if paise > 9_000_000_000_000.0 {
        return None;
    }
    #[expect(
        clippy::cast_possible_truncation,
        reason = "bounded above by the check on the line above and below by the > 0.0 guard"
    )]
    Some(paise as i64)
}

/// The nearest expiry an underlying has in the slice, or `None` when it has
/// none.
///
/// Ties are impossible — an expiry is a date — so `min` is total here.
#[must_use]
fn nearest_expiry_for(candidates: &[DepthCandidate], underlying: &str) -> Option<i64> {
    candidates
        .iter()
        .filter(|c| c.underlying == underlying && c.expiry_micros > 0)
        .map(|c| c.expiry_micros)
        .min()
}

/// Whether a candidate belongs to its underlying's nearest expiry.
///
/// # Why every depth grouping must ask this
///
/// Operator, 2026-08-26: *"always current expiry alone only ... especially for
/// depth 20 and depth 200"*. The reason is not preference. The candidate slice
/// carries EVERY expiry from today forward — this week's, next month's, the
/// quarterly — because `depth_candidates_from_master` drops only what has
/// already expired.
///
/// Group that slice by `(underlying, strike)` and the same strike from two
/// different expiries lands in one bucket, so whichever row arrives last wins.
/// The socket then subscribes a far-month contract that barely quotes, at a
/// strike chosen from this month's spot — a well-formed subscription to an
/// almost-empty book, and nothing downstream can tell it from a real one.
///
/// So the expiry filter is not a refinement of the at-the-money search. It is
/// what makes the search meaningful at all.
///
/// # NEAREST, not "monthly" — and this distinction must not be tidied away
///
/// Operator, same day: *"for nifty always weekly current expiry and remaining
/// entirely only monthly, so current month expiry"*. Both are true, and ONE
/// rule delivers both:
///
/// | Underlying | Expiries NSE lists | Nearest is |
/// |---|---|---|
/// | NIFTY | weekly + monthly | this week's |
/// | BANKNIFTY, stocks | monthly only | this month's |
///
/// Taking the nearest gives the weekly for NIFTY and the monthly for
/// everything else without the code ever having to know which is which. A
/// future reader who "corrects" this into a month-matching filter would
/// silently push NIFTY onto a contract up to four weeks out — the exact
/// far-expiry subscription this function exists to prevent, arrived at by
/// making the rule look more explicit.
#[must_use]
fn is_nearest_expiry(candidate: &DepthCandidate, nearest: Option<i64>) -> bool {
    match nearest {
        Some(expiry) => candidate.expiry_micros == expiry,
        // An underlying whose rows all carry a missing expiry. Nothing to
        // compare against, so nothing is refused on that basis — the id and
        // strike guards still apply.
        None => true,
    }
}

/// Counts a chain whose rows disagree about one underlying's spot price.
///
/// Not an error on its own — see [`consensus_spot`] for why the disagreement
/// is a normal consequence of the query — but a chain where it climbs is one
/// where the at-the-money window is being centred on a price most of the
/// chain no longer quotes.
pub const SPOT_DISAGREEMENT: &str = "tv_depth_spot_disagreement_total";

/// The spot price the underlying's rows AGREE on: the most common usable
/// value, ties going to the lower one.
///
/// # Why the rows can disagree at all
///
/// They can, routinely, and the reason is in the query. `LATEST ON ts
/// PARTITION BY underlying_security_id, expiry, strike, leg` takes each
/// strike's own newest row, so a strike the vendor stopped returning at 09:47
/// keeps 09:47's `underlying_spot` while every actively-quoted strike carries
/// this minute's. One underlying, one slice, two prices — and by 15:00 they
/// can be hundreds of points apart.
///
/// # Why not first-seen or last-seen
///
/// Both were tried here, and both make the answer depend on the ORDER the
/// rows arrive in. The query carries no `ORDER BY`, and the grouping map is a
/// `HashMap` whose iteration order is not even stable between runs of the
/// same process — so the at-the-money strike could differ minute to minute on
/// identical data, recentring the window and swapping sockets for the rest of
/// the day while every counter reported healthy activity.
///
/// The most common value is order-independent by construction, and it is the
/// price the chain as a whole is quoting: stale strikes are the ones that
/// dropped out of the vendor's response, so they are the minority. Ties go to
/// the lower value — arbitrary between two equally-attested prices, but
/// deterministic, which is the property that matters.
///
/// Refusing the underlying outright on any disagreement was considered and
/// rejected: under this query a single stale strike is the NORMAL state, so
/// refusal would drop depth for an underlying the operator asked to be
/// covered, most days.
///
/// Grouping is on the bit pattern, so rows carrying literally the same column
/// value group exactly and no epsilon has to be invented.
///
/// # Complexity
///
/// O(candidates), one small map. Cold path, once a minute.
#[must_use]
pub fn consensus_spot(
    candidates: &[DepthCandidate],
    underlying: &str,
    nearest: Option<i64>,
) -> Option<f64> {
    let mut tally: HashMap<u64, (usize, f64)> = HashMap::new();
    for c in candidates {
        if c.underlying != underlying || !is_nearest_expiry(c, nearest) {
            continue;
        }
        if !c.spot.is_finite() || c.spot <= 0.0 {
            continue;
        }
        let slot = tally.entry(c.spot.to_bits()).or_insert((0, c.spot));
        slot.0 = slot.0.saturating_add(1);
    }
    if tally.len() > 1 {
        metrics::counter!(SPOT_DISAGREEMENT).increment(1);
    }
    let mut best: Option<(usize, f64)> = None;
    for (count, value) in tally.into_values() {
        let take = match best {
            None => true,
            Some((best_count, best_value)) => {
                count > best_count || (count == best_count && value < best_value)
            }
        };
        if take {
            best = Some((count, value));
        }
    }
    best.map(|(_, value)| value)
}

/// Groups a candidate slice into per-underlying chain views.
///
/// # What is dropped, and why each is dropped rather than guessed
///
/// - **A strike with only one leg.** The tracker subscribes a CALL socket and
///   a PUT socket per underlying; a strike that can fill only one of them
///   would move one socket and strand the other on a different strike, so the
///   two sockets would no longer be reading the same book.
/// - **A non-positive or non-finite strike or spot.** There is no nearest
///   strike to a spot that is not a number.
/// - **A contract id of zero or below.** Subscribing instrument 0 is a
///   well-formed request that returns nothing forever and looks healthy.
///
/// Everything dropped is dropped SILENTLY at this layer and counted by the
/// caller against the totals it already has: this function's contract is
/// "usable pairs only", and a caller that gets fewer underlyings than it
/// expects knows immediately.
///
/// # Complexity
///
/// O(candidates) to bucket, then O(k log k) per underlying to order strikes —
///
/// # Current expiry only
///
/// Rows outside each underlying's NEAREST expiry are dropped before grouping.
/// See [`is_nearest_expiry`] — without it, one strike from two expiries shares
/// a bucket and the later row wins.
#[must_use]
pub fn chain_minutes_from_candidates(candidates: &[DepthCandidate]) -> Vec<OwnedChainMinute> {
    // (underlying, strike_paise) -> (ce, pe)
    let mut by_strike: HashMap<(&str, i64), (Option<i64>, Option<i64>)> = HashMap::new();
    let mut order: Vec<&str> = Vec::new();
    // CURRENT EXPIRY ONLY, per underlying. Resolved once rather than per row:
    // the slice carries every expiry from today forward, and grouping by
    // (underlying, strike) without this puts two expiries' contracts in one
    // bucket. See `is_nearest_expiry`.
    let mut nearest: HashMap<&str, Option<i64>> = HashMap::new();
    for c in candidates {
        if !c.is_index_option {
            continue;
        }
        let expiry = *nearest
            .entry(&c.underlying)
            .or_insert_with(|| nearest_expiry_for(candidates, &c.underlying));
        if !is_nearest_expiry(c, expiry) {
            continue;
        }
        let Some(paise) = strike_paise(c.strike) else {
            continue;
        };
        if !c.spot.is_finite() || c.spot <= 0.0 || c.contract_security_id <= 0 {
            continue;
        }
        if !order.contains(&c.underlying.as_str()) {
            order.push(&c.underlying);
        }
        let slot = by_strike
            .entry((&c.underlying, paise))
            .or_insert((None, None));
        // LOWEST id wins a contested leg, rather than the last row seen.
        //
        // A well-formed chain lists one contract per (underlying, expiry,
        // strike, leg), so this only bites on vendor data that is already
        // wrong. But "already wrong" is not "cannot happen", and the two
        // available behaviours differ in a way that matters:
        //
        //   last-write-wins  the answer depends on the ORDER the rows
        //                    arrive in. The query carries no ORDER BY, so
        //                    two minutes can legitimately return the same
        //                    rows in a different order and pick different
        //                    contracts — the socket then swaps back and
        //                    forth between two ids for the rest of the day,
        //                    reporting healthy swap counts the whole time.
        //   lowest id wins   the same input always gives the same answer.
        //
        // Neither can tell WHICH id is right; only one of them is stable.
        // Refusing the strike outright was considered and rejected: it
        // spends a real depth slot to punish a duplicate that is usually
        // benign, and the strike is one the operator asked to be covered.
        //
        // Found by a property test — the hand-written suite had no
        // malformed-chain case at all.
        match c.leg.as_str() {
            "CE" => {
                slot.0 = Some(slot.0.map_or(c.contract_security_id, |had| {
                    had.min(c.contract_security_id)
                }));
            }
            "PE" => {
                slot.1 = Some(slot.1.map_or(c.contract_security_id, |had| {
                    had.min(c.contract_security_id)
                }));
            }
            // An unrecognised leg. Not a future — those carry no strike and
            // were filtered upstream — so this is a row we cannot place, and
            // placing it on a guess would subscribe the wrong side.
            _ => {}
        }
    }

    let mut out = Vec::with_capacity(order.len());
    for underlying in order {
        let mut pairs: Vec<StrikePair> = Vec::new();
        // The spot the underlying's rows AGREE on, not the last one this
        // loop happened to touch. `by_strike` is a `HashMap`, so its
        // iteration order is not stable even between runs of the same
        // process on identical input — last-wins made this function's own
        // output non-reproducible whenever the rows disagreed, which under
        // the per-strike `LATEST ON ts` query is routine. See
        // [`consensus_spot`].
        let spot = consensus_spot(
            candidates,
            underlying,
            nearest.get(underlying).copied().flatten(),
        )
        .unwrap_or(f64::NAN);
        for ((u, paise), (ce, pe)) in &by_strike {
            if *u != underlying {
                continue;
            }
            let (Some(ce), Some(pe)) = (*ce, *pe) else {
                continue;
            };
            // One contract cannot be both legs of its own strike. A chain
            // that says so is malformed, and honouring it would spend TWO of
            // the 250 authorized depth slots on a single instrument while
            // the strike's real other leg goes unsubscribed. Dropping the
            // strike is the same treatment a half-listed strike already
            // gets, and for the same reason: a CALL socket and a PUT socket
            // that are not reading two different books are not doing the job
            // they were dialed for.
            if ce == pe {
                continue;
            }
            pairs.push(StrikePair {
                strike_paise: *paise,
                ce_security_id: ce,
                pe_security_id: pe,
            });
        }
        if pairs.is_empty() || !spot.is_finite() {
            continue;
        }
        pairs.sort_unstable_by_key(|p| p.strike_paise);
        out.push(OwnedChainMinute {
            underlying: underlying.to_owned(),
            spot,
            pairs,
        });
    }
    out
}

/// The at-the-money pair for one underlying, from the same candidate slice.
///
/// Nearest strike by absolute distance from spot, ties going to the LOWER
/// strike — a deterministic rule, so an exact midpoint does not flip the socket
/// between two strikes on alternating minutes.
///
/// # Complexity
///
/// O(candidates). Called once a minute for one stock.
#[must_use]
pub fn atm_pair_for(candidates: &[DepthCandidate], underlying: &str) -> Option<StrikePair> {
    let mut pairs: HashMap<i64, (Option<i64>, Option<i64>)> = HashMap::new();
    // CURRENT EXPIRY ONLY. Without this the same strike from two expiries
    // shares one map slot and the later row wins — so the socket can land on
    // a far-month contract at a strike chosen from this month's spot. See
    // `is_nearest_expiry`.
    let nearest = nearest_expiry_for(candidates, underlying);
    // The spot the rows AGREE on. Neither the first nor the last usable
    // value: both make the at-the-money strike depend on the order the rows
    // arrived in, and the query carries no `ORDER BY`. See
    // [`consensus_spot`] — including why disagreement is routine rather than
    // a data error.
    let spot = consensus_spot(candidates, underlying, nearest).unwrap_or(f64::NAN);
    for c in candidates {
        if c.underlying != underlying {
            continue;
        }
        if !is_nearest_expiry(c, nearest) {
            continue;
        }
        let Some(paise) = strike_paise(c.strike) else {
            continue;
        };
        if c.contract_security_id <= 0 {
            continue;
        }
        let slot = pairs.entry(paise).or_insert((None, None));
        match c.leg.as_str() {
            "CE" => slot.0 = Some(c.contract_security_id),
            "PE" => slot.1 = Some(c.contract_security_id),
            _ => {}
        }
    }
    if !spot.is_finite() {
        return None;
    }
    let spot_paise = strike_paise(spot)?;
    let mut best: Option<StrikePair> = None;
    let mut best_distance = i64::MAX;
    // Deterministic order, because a `HashMap` iteration is not. Without this
    // an exact tie would resolve differently run to run, and the socket would
    // move on a distinction that is not in the data.
    let mut strikes: Vec<i64> = pairs.keys().copied().collect();
    strikes.sort_unstable();
    for paise in strikes {
        let (Some(ce), Some(pe)) = pairs[&paise] else {
            continue;
        };
        // Same refusal as the chain view — see there for why.
        if ce == pe {
            continue;
        }
        let distance = (paise - spot_paise).abs();
        if distance < best_distance {
            best_distance = distance;
            best = Some(StrikePair {
                strike_paise: paise,
                ce_security_id: ce,
                pe_security_id: pe,
            });
        }
    }
    best
}

/// The day's leading mover, resolved to the contracts its at-the-money strike
/// would subscribe.
///
/// # What "leading" means here
///
/// The largest move by ABSOLUTE size, in either direction. A stock down 9%
/// leads a stock up 8%: both have a deep book, and the fifth socket's job is
/// to watch the one the market is most interested in, not the one that happens
/// to be rising.
///
/// # Every refusal returns `None`, and none of them is a failure
///
/// A non-finite move, an exactly-flat move, a stock whose ladder is not in the
/// candidate slice, or a ladder with no complete at-the-money pair — each
/// returns `None`, and [`crate::depth200_atm::TopMoverSocket::observe`] treats
/// that as "keep what you have". A stale deep book beats an empty one, and
/// unsubscribing spends a wire call to end up with less.
///
/// # Complexity
///
/// O(rows) to find the leader, then O(candidates) to resolve its pair.
#[must_use]
pub fn top_mover_pick(rows: &[MoverRow], candidates: &[DepthCandidate]) -> Option<TopMoverPick> {
    let mut leader: Option<&MoverRow> = None;
    for row in rows {
        if !row.pct_change.is_finite() || row.pct_change == 0.0 {
            continue;
        }
        let better = match leader {
            None => true,
            Some(best) => row.pct_change.abs() > best.pct_change.abs(),
        };
        if better {
            leader = Some(row);
        }
    }
    let leader = leader?;
    let pair = atm_pair_for(candidates, &leader.symbol)?;
    Some(TopMoverPick {
        underlying_security_id: leader.security_id,
        contract_segment: STOCK_OPTION_SEGMENT,
        pct_change: leader.pct_change,
        atm_ce_security_id: pair.ce_security_id,
        atm_pe_security_id: pair.pe_security_id,
    })
}

/// The per-minute movers query.
///
/// # Why `LATEST ON` and not `max(ts)`
///
/// The rebalance runs a few seconds after a minute seals, and not every stock
/// seals every minute — a thin F&O stock can go minutes without a trade, so
/// its newest candle is older than the newest candle in the table. Anchoring
/// on a single minute would silently drop exactly the illiquid names whose
/// depth is most worth reading. `LATEST ON ts PARTITION BY` gives each stock
/// its own newest row.
///
/// # Why the day bound is not optional
///
/// Without `ts >= today` the same `LATEST ON` returns the newest row per stock
/// from ANY day, so a pre-open caller gets YESTERDAY'S ranking — plausible,
/// completely stale, and indistinguishable from a real one. This is the same
/// bound, for the same reason, that
/// [`crate::dhan_depth_universe::build_depth_candidate_query`] carries.
///
/// # Why the join
///
/// The ranking needs only the id, but resolving a winner to its option ladder
/// needs the SYMBOL: contract rows key on the underlying's symbol, not its id.
/// A LEFT join would return rows with a null symbol that could rank but never
/// resolve, so this is an inner join — a stock the master cannot name is a
/// stock this socket cannot subscribe, and it is better absent from the
/// ranking than present and unusable.
#[must_use]
pub fn build_movers_query(today_ist_micros: i64) -> String {
    let segment = MOVER_UNDERLYING_SEGMENT.as_str();
    format!(
        "SELECT c.security_id, il.symbol_name, c.close_pct_from_prev_day \
         FROM (SELECT security_id, close_pct_from_prev_day FROM candles_1m \
         WHERE feed = 'dhan' AND segment = '{segment}' AND ts >= {today_ist_micros} \
         LATEST ON ts PARTITION BY security_id) c \
         JOIN (SELECT security_id, symbol_name FROM instrument_lifecycle \
         WHERE feed = 'dhan' AND exchange_segment = '{segment}') il \
         ON c.security_id = il.security_id;"
    )
}

/// The pre-open ranking, read from ticks instead of candles.
///
/// # Why this exists — the 09:13 gap, measured
///
/// The operator's requirement is that depth is connected and subscribed by
/// **09:13**. Five of the ten depth sockets could not meet it, and the reason
/// is a boundary two subsystems disagree about:
///
/// | Reads | From | Available at 09:13 |
/// |---|---|---|
/// | `fetch_spot_prices` | `ticks` | YES — capture starts 09:00 |
/// | `build_movers_query` | `candles_1m` | **NO** |
///
/// Ticks are PERSISTED from 09:00, but the aggregator refuses to FOLD
/// anything before 09:15:00 (`MARKET_OPEN_SECS_OF_DAY_IST`), so `candles_1m`
/// is necessarily empty until the 09:15 candle seals around 09:16. Anything
/// keyed on a ranking was therefore dark for the first three minutes the
/// operator asked to be covered:
///
/// - depth-20's three movers sockets (150 of its 250 instruments)
/// - depth-200's fifth socket, the day's biggest mover
///
/// The at-the-money sockets were unaffected — they read spot, which is there.
///
/// **The ranking itself is not missing at 09:13, only its usual source is.**
/// A tick carries the previous day's close (`day_close`, populated on Quote
/// and Full packets), and the pre-open equilibrium price is in `ltp` by
/// 09:12. Percent change is exactly the same arithmetic the candle column
/// performs — so this reads it one layer lower rather than inventing a
/// substitute.
///
/// `close > 0` is required, not assumed: a Ticker-mode packet carries
/// `0.0` there by documented design, and dividing by it would rank every
/// such instrument at infinity — putting the WORST-populated instruments at
/// the top of the list.
#[must_use]
pub fn build_preopen_movers_query(today_ist_micros: i64) -> String {
    let segment = MOVER_UNDERLYING_SEGMENT.as_str();
    format!(
        "SELECT t.security_id, il.symbol_name,          ((t.ltp - t.close) / t.close) * 100.0 AS close_pct_from_prev_day          FROM (SELECT security_id, ltp, close FROM ticks          WHERE feed = 'dhan' AND segment = '{segment}' AND ts >= {today_ist_micros}          AND ltp > 0 AND close > 0          LATEST ON ts PARTITION BY security_id) t          JOIN (SELECT security_id, symbol_name FROM instrument_lifecycle          WHERE feed = 'dhan' AND exchange_segment = '{segment}') il          ON t.security_id = il.security_id;"
    )
}

/// Which source a minute's ranking came from.
pub const MOVERS_SOURCE: &str = "tv_depth_movers_source_total";

/// Parse the `/exec` dataset into mover rows.
///
/// Fail-LOUD on a malformed body or a missing `dataset` key, fail-soft per row
/// — the house pattern. The distinction matters: an empty `Vec` returned for
/// garbage is indistinguishable from a genuinely flat market, and the caller's
/// response to those two is different. A flat market keeps the sockets where
/// they are; a broken query needs saying out loud.
///
/// # Errors
///
/// Returns `Err` when the body is not JSON or carries no `dataset` array.
pub fn parse_movers_dataset(body: &str) -> Result<Vec<MoverRow>, String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Err("malformed /exec response: not valid JSON".to_owned());
    };
    let Some(rows) = v.get("dataset").and_then(|d| d.as_array()) else {
        return Err("malformed /exec response: missing dataset array".to_owned());
    };
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(cols) = row.as_array() else { continue };
        if cols.len() < 3 {
            continue;
        }
        let Some(security_id) = cols[0].as_u64() else {
            continue;
        };
        if security_id == 0 {
            continue;
        }
        let symbol = cols[1].as_str().unwrap_or_default();
        if symbol.is_empty() {
            continue;
        }
        let Some(pct_change) = cols[2].as_f64() else {
            continue;
        };
        if !pct_change.is_finite() {
            continue;
        }
        out.push(MoverRow {
            security_id,
            segment: MOVER_UNDERLYING_SEGMENT,
            symbol: symbol.to_owned(),
            pct_change,
        });
    }
    Ok(out)
}

/// What one minute's rebalance decided, for all five depth-200 sockets.
///
/// Returned rather than sent, so the decision is testable without a socket and
/// the caller owns every wire call. On an ordinary minute every field is empty
/// or `None` and the caller does nothing at all — which is the common case and
/// must stay free.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RebalanceDecision {
    /// Swaps for the four index at-the-money sockets, in dial order.
    pub atm_swaps: Vec<PlannedSwap>,
    /// The fifth socket's swap, when it already carried something.
    pub top_mover_swap: Option<PlannedSwap>,
    /// The fifth socket's FIRST subscription. Not a swap: there is nothing to
    /// unsubscribe, and inventing an `old` to make it look like one would ask
    /// the guard to drop an instrument the connection never held.
    pub top_mover_first: Option<SubscribeInstrument>,
    /// Why the fifth socket stayed put, when it did. Carried rather than
    /// swallowed: "already right" and "we could not tell" are different states
    /// and must not share a silence.
    pub top_mover_idle: Option<NoTopMoverSwitch>,
}

impl RebalanceDecision {
    /// Whether this minute costs any wire calls at all.
    #[must_use]
    pub fn is_quiet(&self) -> bool {
        self.atm_swaps.is_empty() && self.top_mover_swap.is_none() && self.top_mover_first.is_none()
    }
}

/// One minute of rebalance, decided but not sent.
///
/// # Why both engines run on one call
///
/// They share a minute and they share a candidate slice. Running them from two
/// timers would let the index sockets act on one snapshot while the fifth acts
/// on the next — reading two different moments as though they were one, which
/// is the sort of skew that shows up as an inexplicable swap in a log weeks
/// later.
///
/// # Why a failed half does not stop the other
///
/// An empty `movers` slice leaves the fifth socket where it is and the four
/// index sockets carry on. The two answers are independent facts about the
/// market and one being unavailable is not evidence about the other.
///
/// # Complexity
///
/// O(candidates) to group, O(pairs) per tracked underlying, O(movers) to rank.
/// Cold path, once a minute.
#[must_use]
pub fn plan_minute(
    tracker: &mut Depth200AtmTracker,
    top_mover: &mut TopMoverSocket,
    held: &[SubscribeInstrument],
    candidates: &[DepthCandidate],
    movers: &[MoverRow],
) -> RebalanceDecision {
    let owned = chain_minutes_from_candidates(candidates);
    let minutes: Vec<ChainMinute<'_>> = owned.iter().map(OwnedChainMinute::as_minute).collect();
    let atm_swaps = plan_swaps(tracker, &minutes, held, |underlying| {
        contract_segment_for_underlying(underlying)
    });

    let pick = top_mover_pick(movers, candidates);
    let mut decision = RebalanceDecision {
        atm_swaps,
        ..RebalanceDecision::default()
    };
    match top_mover.observe(pick.as_ref()) {
        Ok(switch) => {
            if let Some(swap) = TopMoverSocket::plan(&switch) {
                decision.top_mover_swap = Some(swap);
            } else {
                // A first adoption. `plan` returns `None` precisely because
                // there is no old instrument, and the caller needs to know the
                // difference: a swap unsubscribes first, a first subscription
                // must not.
                decision.top_mover_first = Some(switch.to);
            }
        }
        Err(idle) => decision.top_mover_idle = Some(idle),
    }
    decision
}

/// One depth-200 connection the rebalance can steer.
///
/// The channel and what the connection currently holds, kept together because
/// the two must never disagree: the guard replaces IN PLACE and refuses
/// fail-closed if the old instrument is not on the connection, so a `held`
/// that has drifted from the wire produces a swap that is refused every time
/// and a socket that never moves again.
pub struct RebalanceSocket {
    /// Where a swap travels.
    pub tx: tokio::sync::mpsc::Sender<LiveSubscriptionCommand>,
    /// What this connection holds. A depth-200 connection holds exactly one
    /// instrument; `None` only before its first subscription.
    pub held: Option<SubscribeInstrument>,
}

/// How often the rebalance looks — once a minute, offset past the boundary.
///
/// The offset is not politeness. A candle seals AT the boundary and the writer
/// needs a moment to flush it, so a query fired at :00 reads the minute before
/// last and the whole loop runs one minute behind the market for the entire
/// session — a bug that would look exactly like normal operation.
pub const REBALANCE_OFFSET_SECS: u64 = 8;

/// How often the heartbeat ticker republishes the age.
///
/// Half the alarm's own period, so every window carries a fresh reading
/// rather than one that happened to land badly.
pub const REBALANCE_HEARTBEAT_SECS: u64 = 30;

/// The age to publish, given the last stamp and the time now.
///
/// A pure function because the interesting cases are the boundaries — a loop
/// that has never evaluated, and a clock that went backwards — and neither is
/// assertable through a `tokio::time` sleep.
#[must_use]
pub fn rebalance_age_value(last_stamp_secs: i64, now_secs: i64) -> f64 {
    if last_stamp_secs <= 0 {
        // Never evaluated. Reported as the age since the process started
        // would be a guess; reporting a large constant says "not running"
        // without inventing a duration.
        return f64::from(u16::MAX);
    }
    // Saturating, and never negative: a backwards clock step must not make a
    // stalled loop look freshly alive, which is the one direction that
    // matters.
    now_secs.saturating_sub(last_stamp_secs).max(0) as f64
}

/// Seconds since the rebalance loop last completed a minute's evaluation.
///
/// # Why a gauge, when five counters already exist
///
/// The counters answer "how much moved". None of them answers the question an
/// operator actually has at 14:00, which is **"is depth steering still
/// running at all?"** — and that question has several causes with one
/// symptom: the task panicked, the loop is wedged on a query, every channel
/// closed, or the whole stack never spawned it. A counter that stops
/// incrementing is indistinguishable from a quiet market; this is not.
///
/// A GAUGE specifically, for a reason this repository has already paid to
/// learn: the CloudWatch agent's prometheus pipeline is ambiguous about
/// whether a `_total` arrives as a delta or a running cumulative, and an
/// alarm written for the wrong one is silently blind. A gauge is published
/// verbatim either way, so the alarm cannot be disarmed by a pipeline detail
/// nobody here can check.
///
/// Published on EVERY iteration, including quiet ones — a series that only
/// appears when something moves is missing at exactly the moment it is
/// needed.
pub const REBALANCE_AGE_SECS: &str = "tv_depth_rebalance_age_secs";

/// The counter for swaps that actually reached a channel.
pub const REBALANCE_SWAPS_SENT: &str = "tv_depth_rebalance_swaps_sent_total";

/// The counter for swaps that could not be sent, by reason.
pub const REBALANCE_SWAPS_REFUSED: &str = "tv_depth_rebalance_swaps_refused_total";

/// Reasons a decided swap never reached the wire. Every one is COUNTED — a
/// swap that is decided and then quietly dropped is the worst outcome
/// available, because the socket stays on a stale contract while every log
/// line says the rebalance is working.
pub const REBALANCE_REFUSAL_REASONS: [&str; 3] = ["no_socket", "channel_full", "channel_closed"];

/// Pre-register the counters so a session that never refuses anything still
/// publishes a zero, rather than a gap an alarm cannot distinguish from a dead
/// process.
pub fn pre_register_rebalance_counters() {
    metrics::counter!(REBALANCE_SWAPS_SENT).increment(0);
    for reason in REBALANCE_REFUSAL_REASONS {
        metrics::counter!(REBALANCE_SWAPS_REFUSED, "reason" => reason).increment(0);
    }
}

/// Sends one decided swap, updating what the socket is believed to hold.
///
/// # Why `try_send` and never `send().await`
///
/// This runs on its own task, but the channel's other end is the connection's
/// read loop — the same task that drains frames. An `await` here on a full
/// channel would apply backpressure to a socket whose job is to never fall
/// behind, and Dhan skips a slow consumer forward to the latest available
/// state with no sequence number for us to detect the loss. A full channel is
/// a refusal, counted, and retried next minute; a stalled drain is tick loss.
fn send_swap(socket: &mut RebalanceSocket, swap: &PlannedSwap) -> bool {
    let command = LiveSubscriptionCommand::Swap {
        old: swap.old,
        new: swap.new,
    };
    match socket.tx.try_send(command) {
        Ok(()) => {
            // Believed-held moves only on a SUCCESSFUL send. Moving it
            // optimistically would leave the next minute's swap naming an old
            // instrument the connection never received, and the guard would
            // refuse it — one dropped message costing every future swap.
            socket.held = Some(swap.new);
            metrics::counter!(REBALANCE_SWAPS_SENT).increment(1);
            true
        }
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            metrics::counter!(REBALANCE_SWAPS_REFUSED, "reason" => "channel_full").increment(1);
            tracing::warn!(
                code =
                    tickvault_common::error_code::ErrorCode::WsGapSubscriptionBatching.code_str(),
                socket_index = swap.socket_index,
                "depth rebalance: the command channel is full, so this minute's swap is \
                 dropped rather than awaited. Awaiting would apply backpressure to the \
                 frame drain, and a stalled drain is tick loss. Retried next minute."
            );
            false
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            metrics::counter!(REBALANCE_SWAPS_REFUSED, "reason" => "channel_closed").increment(1);
            tracing::error!(
                code =
                    tickvault_common::error_code::ErrorCode::WsGapSubscriptionBatching.code_str(),
                socket_index = swap.socket_index,
                "depth rebalance: the command channel is CLOSED — this connection is gone \
                 and its socket will never move again this session."
            );
            false
        }
    }
}

/// Applies one minute's decision to the sockets.
///
/// Returns how many swaps reached a channel. Split from [`plan_minute`] so the
/// decision stays testable without a socket and the sending stays testable
/// without a market.
pub fn apply_decision(sockets: &mut [RebalanceSocket], decision: &RebalanceDecision) -> usize {
    let mut sent = 0usize;
    for swap in &decision.atm_swaps {
        let Some(socket) = sockets.get_mut(swap.socket_index) else {
            metrics::counter!(REBALANCE_SWAPS_REFUSED, "reason" => "no_socket").increment(1);
            tracing::error!(
                code =
                    tickvault_common::error_code::ErrorCode::WsGapSubscriptionBatching.code_str(),
                socket_index = swap.socket_index,
                sockets = sockets.len(),
                "depth rebalance decided a swap for a socket that does not exist"
            );
            continue;
        };
        if send_swap(socket, swap) {
            sent = sent.saturating_add(1);
        }
    }
    if let Some(swap) = &decision.top_mover_swap {
        match sockets.get_mut(swap.socket_index) {
            Some(socket) => {
                if send_swap(socket, swap) {
                    sent = sent.saturating_add(1);
                }
            }
            None => {
                metrics::counter!(REBALANCE_SWAPS_REFUSED, "reason" => "no_socket").increment(1);
            }
        }
    }
    sent
}

/// How long a movers query may take before it is abandoned for the minute.
///
/// Deliberately shorter than the minute it runs in. A query that outlived its
/// own minute would return a ranking for a minute that has already passed,
/// and the socket would chase the market one step behind all session — which
/// looks exactly like working correctly.
const MOVERS_QUERY_TIMEOUT_SECS: u64 = 10;

/// Runs the movers query, returning an empty ranking on every failure.
///
/// Empty rather than an error, because the caller's response is identical
/// either way: leave the fifth socket where it is. Each failure is logged with
/// its reason so "the market is flat" and "the query died" are still
/// distinguishable to a reader, just not to the control flow.
// TEST-EXEMPT: HTTP composition of build_movers_query + parse_movers_dataset, both tested.
pub async fn fetch_movers(
    questdb: &tickvault_common::config::QuestDbConfig,
    today_ist_micros: i64,
) -> Vec<MoverRow> {
    let url = format!("http://{}:{}/exec", questdb.host, questdb.http_port);
    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(MOVERS_QUERY_TIMEOUT_SECS))
        .build()
    else {
        tracing::error!(
            code = tickvault_common::error_code::ErrorCode::WsGapSubscriptionBatching.code_str(),
            "depth rebalance: HTTP client build failed, so the fifth socket has no ranking \
             to follow this minute. It keeps whatever it holds."
        );
        return Vec::new();
    };
    // Sealed candles first. They are the better source once they exist: a
    // candle's percent change is the same arithmetic but computed over a
    // whole minute rather than one instant.
    let rows = run_movers_query(&client, &url, &build_movers_query(today_ist_micros)).await;
    if !rows.is_empty() {
        metrics::counter!(MOVERS_SOURCE, "source" => "candles").increment(1);
        return rows;
    }

    // Empty is the NORMAL state before ~09:16, not a fault — the aggregator
    // refuses to fold anything before 09:15:00, so no candle can have sealed
    // yet. Falling back to ticks is what lets the movers sockets meet the
    // 09:13 deadline instead of standing dark for the first three minutes.
    //
    // Also reached, deliberately, if the candle query fails mid-session: a
    // ranking from ticks is worse than one from candles and far better than
    // none, and "none" leaves five sockets frozen wherever they were.
    let preopen =
        run_movers_query(&client, &url, &build_preopen_movers_query(today_ist_micros)).await;
    metrics::counter!(
        MOVERS_SOURCE,
        "source" => if preopen.is_empty() { "none" } else { "ticks" }
    )
    .increment(1);
    preopen
}

/// Runs one ranking query and parses it. Every failure is a warn plus an
/// empty result — the caller decides what an empty ranking means, because at
/// 09:13 it means "try ticks" and at 14:00 it means "keep what you hold".
async fn run_movers_query(client: &reqwest::Client, url: &str, sql: &str) -> Vec<MoverRow> {
    let body = match client.get(url).query(&[("query", sql)]).send().await {
        Ok(resp) if resp.status().is_success() => match resp.text().await {
            Ok(b) => b,
            Err(err) => {
                tracing::warn!(?err, "depth rebalance: movers response unreadable");
                return Vec::new();
            }
        },
        Ok(resp) => {
            tracing::warn!(status = %resp.status(), "depth rebalance: movers query non-2xx");
            return Vec::new();
        }
        Err(err) => {
            tracing::warn!(?err, "depth rebalance: movers query failed");
            return Vec::new();
        }
    };
    match parse_movers_dataset(&body) {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(err, "depth rebalance: movers response unparseable");
            Vec::new()
        }
    }
}

/// Seconds to sleep to land [`REBALANCE_OFFSET_SECS`] past the next minute.
///
/// Pure so the boundary behaviour is testable without a clock — the whole
/// question is what happens at :00, at exactly the offset, and one second
/// after it, and an inline arithmetic expression inside a `sleep` cannot be
/// asserted at all.
///
/// Never returns zero. A zero sleep at the exact offset second would spin the
/// loop through the same minute repeatedly, firing a query every iteration
/// until the second rolled over — the sort of busy loop that shows up as
/// inexplicable database load.
#[must_use]
pub const fn secs_until_next_rebalance(second_of_minute: u64) -> u64 {
    let target = REBALANCE_OFFSET_SECS;
    if second_of_minute < target {
        target - second_of_minute
    } else {
        // Past this minute's slot: wait for the next one.
        60 - second_of_minute + target
    }
}

/// The per-minute rebalance: the thing that makes every engine above live.
///
/// # Why the sockets are owned, not borrowed
///
/// It runs for the session and outlives the attach that dialed the
/// connections. Holding the channels is also what keeps them OPEN: a
/// `Sender` dropped at the end of the attach closes the channel, which is
/// exactly why the machinery was inert rather than merely idle.
///
/// # Why it refuses to start with no sockets
///
/// A rebalance over zero connections runs two queries a minute for a whole
/// session to decide swaps that can never be sent. Refusing loudly at the
/// start is the difference between a visible gap and a silent one.
// TEST-EXEMPT: async loop over secs_until_next_rebalance + load_depth_candidates + fetch_movers + plan_minute + apply_decision, each separately tested.
pub async fn run_depth_rebalance(
    questdb: tickvault_common::config::QuestDbConfig,
    date_ist: String,
    today_ymd: u32,
    today_ist_micros: i64,
    mut sockets: Vec<RebalanceSocket>,
    mut depth20: Vec<crate::depth20_track::Depth20LiveSocket>,
) {
    if sockets.is_empty() && depth20.is_empty() {
        tracing::error!(
            code = tickvault_common::error_code::ErrorCode::WsGapSubscriptionBatching.code_str(),
            "depth rebalance refused to start: NEITHER depth pool handed it a steerable \
             connection, so every at-the-money strike and every index window stays wherever \
             it was dialed for the whole session"
        );
        return;
    }
    pre_register_rebalance_counters();
    crate::depth20_track::pre_register_depth20_counters();
    // The heartbeat, published by a task this loop cannot wedge.
    //
    // Registered BEFORE the first iteration: a loop that dies on its very
    // first pass would otherwise never create the series at all, and a
    // never-created series reads as missing data rather than as stale.
    let heartbeat = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
    spawn_rebalance_heartbeat(std::sync::Arc::clone(&heartbeat));
    let mut tracker = Depth200AtmTracker::new(Depth200AtmConfig::default());
    // Seed the fifth socket's tracker from what the dial actually put on it.
    //
    // A fresh tracker holds nothing, so its first minute would report a first
    // adoption and subscribe a contract the connection is already holding.
    // Dhan answers a duplicate subscription with an 804, which is Fatal and
    // drops the connection — losing the socket on its first minute from a
    // message that was never needed. Read from the socket rather than passed
    // in, so the seed cannot disagree with the wire.
    let mut top_mover = match sockets
        .get(crate::depth200_atm::DEPTH_200_TOP_MOVER_SOCKET)
        .and_then(|s| s.held)
    {
        Some(held) => TopMoverSocket::seeded(held, TOP_MOVER_CONFIRM_OBSERVATIONS),
        None => TopMoverSocket::default(),
    };
    tracing::info!(
        sockets = sockets.len(),
        offset_secs = REBALANCE_OFFSET_SECS,
        "depth rebalance started: the at-the-money strikes now follow spot through the session"
    );

    loop {
        let second = u64::from(ist_second_of_day_now() % 60);
        tokio::time::sleep(std::time::Duration::from_secs(secs_until_next_rebalance(
            second,
        )))
        .await;

        let candidates =
            crate::dhan_depth_universe::load_depth_candidates(&questdb, &date_ist, today_ymd).await;
        let movers = fetch_movers(&questdb, today_ist_micros).await;

        // What the four index sockets are believed to hold, in dial order.
        // Read from the sockets rather than tracked separately: two records of
        // the same fact drift, and a drift here produces swaps the guard
        // refuses forever.
        let held: Vec<SubscribeInstrument> = sockets.iter().filter_map(|s| s.held).collect();

        // ---- depth-20: the windows and the movers, every minute ----
        //
        // Runs BEFORE the depth-200 quiet check, because that check
        // `continue`s the loop. Placing depth-20 after it would silently tie
        // depth-20 tracking to depth-200 having something to do — and the
        // overwhelmingly common minute is one where depth-200 is quiet.
        if !depth20.is_empty() {
            let layout = crate::depth20_layout::build_depth20_layout(&candidates, &movers);
            let held_20: Vec<Vec<SubscribeInstrument>> =
                depth20.iter().map(|s| s.held.clone()).collect();
            let plan_20 = crate::depth20_track::plan_depth20_minute(&held_20, &layout);
            if !plan_20.is_quiet() {
                let sent_20 = crate::depth20_track::apply_depth20_plan(&mut depth20, &plan_20);
                tracing::info!(
                    sent = sent_20,
                    planned = plan_20.swap_count(),
                    sockets_moved = plan_20.sockets.len(),
                    sockets_left_alone = plan_20.sockets_left_alone,
                    "depth-20 tracking moved windows"
                );
            }
        }

        // The heartbeat stamp. Written on EVERY iteration — including the
        // overwhelmingly common quiet one, which is exactly when a signal
        // that only appears on activity would be missing.
        //
        // A STAMP, not the gauge itself: the ticker below publishes the age.
        // Publishing here would freeze the gauge at its last value if this
        // loop ever wedged, and a frozen zero reads as perfectly healthy —
        // the false-OK the gauge exists to prevent.
        heartbeat.store(now_epoch_secs(), std::sync::atomic::Ordering::Relaxed);

        let decision = plan_minute(&mut tracker, &mut top_mover, &held, &candidates, &movers);
        if decision.is_quiet() {
            // The overwhelmingly common minute. No log line: ~375 of these a
            // session would bury the ones that matter.
            continue;
        }
        let sent = apply_decision(&mut sockets, &decision);
        tracing::info!(
            sent,
            atm_swaps = decision.atm_swaps.len(),
            top_mover_swap = decision.top_mover_swap.is_some(),
            top_mover_first = decision.top_mover_first.is_some(),
            candidates = candidates.len(),
            movers = movers.len(),
            "depth rebalance moved sockets"
        );
    }
}

/// Seconds since the Unix epoch, or 0 if the clock is unreadable.
#[must_use]
fn now_epoch_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Publishes the rebalance age forever, from a task the loop cannot wedge.
///
/// Separate from the loop deliberately. If the loop stalls on a query, this
/// keeps publishing a GROWING age and the alarm fires. If the whole task tree
/// dies, nothing publishes at all and CloudWatch sees missing data — which,
/// treated as breaching, fires too. Those are the two ways depth steering can
/// die, and both are visible.
// TEST-EXEMPT: spawn wrapper over rebalance_age_value, which is tested directly.
fn spawn_rebalance_heartbeat(stamp: std::sync::Arc<std::sync::atomic::AtomicI64>) {
    tokio::spawn(async move {
        loop {
            let last = stamp.load(std::sync::atomic::Ordering::Relaxed);
            metrics::gauge!(REBALANCE_AGE_SECS).set(rebalance_age_value(last, now_epoch_secs()));
            tokio::time::sleep(std::time::Duration::from_secs(REBALANCE_HEARTBEAT_SECS)).await;
        }
    });
}

/// Everything the attach needs to shape depth, loaded once.
///
/// Both halves — the depth-20 layout and the fifth depth-200 socket — need the
/// same candidate slice and the same ranking. Loading them once is not only
/// cheaper: two loads a few seconds apart can disagree, and a disagreement
/// here means the fifth socket is chosen from one moment's ranking while the
/// movers sockets are filled from another's.
#[derive(Debug, Clone, Default)]
pub struct AttachInputs {
    /// The contract slice the depth selector consumes.
    pub candidates: Vec<DepthCandidate>,
    /// Today's stock moves, unranked.
    pub movers: Vec<MoverRow>,
}

/// Loads both halves in one pass.
// TEST-EXEMPT: async composition of load_depth_candidates + fetch_movers, both tested.
pub async fn load_attach_inputs(
    questdb: &tickvault_common::config::QuestDbConfig,
    date_ist: &str,
    today_ymd: u32,
    today_ist_micros: i64,
) -> AttachInputs {
    AttachInputs {
        candidates: crate::dhan_depth_universe::load_depth_candidates(questdb, date_ist, today_ymd)
            .await,
        movers: fetch_movers(questdb, today_ist_micros).await,
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(underlying: &str, strike: f64, leg: &str, id: i64, spot: f64) -> DepthCandidate {
        DepthCandidate {
            underlying: underlying.to_owned(),
            contract_security_id: id,
            expiry_micros: 1_900_000_000_000_000,
            strike,
            spot,
            leg: leg.to_owned(),
            is_index_option: underlying == "NIFTY" || underlying == "BANKNIFTY",
        }
    }

    fn mover(id: u64, symbol: &str, pct: f64) -> MoverRow {
        MoverRow {
            security_id: id,
            segment: MOVER_UNDERLYING_SEGMENT,
            symbol: symbol.to_owned(),
            pct_change: pct,
        }
    }

    // ---- strike_paise ----

    #[test]
    fn a_strike_becomes_exact_paise() {
        assert_eq!(strike_paise(24_500.0), Some(2_450_000));
        assert_eq!(strike_paise(24_500.5), Some(2_450_050));
    }

    #[test]
    fn a_strike_a_hair_under_rounds_up_rather_than_splitting_the_pair() {
        // The whole reason paise exist here. Truncation would file this one
        // paise low, and the CE at 24499.999999 would never meet the PE at
        // 24500.0 — two legs of one strike, silently unpaired.
        assert_eq!(strike_paise(24_499.999_999), strike_paise(24_500.0));
    }

    #[test]
    fn a_strike_that_is_not_a_number_is_refused() {
        assert_eq!(strike_paise(f64::NAN), None);
        assert_eq!(strike_paise(f64::INFINITY), None);
        assert_eq!(strike_paise(0.0), None);
        assert_eq!(strike_paise(-100.0), None);
    }

    #[test]
    fn an_absurd_strike_is_refused_rather_than_saturated() {
        // Two absurd values both saturate to i64::MAX and would compare EQUAL,
        // pairing a call at 1e30 with a put at 2e30 as one strike.
        assert_eq!(strike_paise(1e30), None);
        assert_eq!(strike_paise(2e30), None);
    }

    // ---- consensus_spot ----

    /// The ordinary case: every row carries the same price, so there is
    /// nothing to decide.
    #[test]
    fn one_agreed_spot_is_returned_unchanged() {
        let rows = [
            candidate("NIFTY", 24_500.0, "CE", 101, 24_480.0),
            candidate("NIFTY", 24_550.0, "PE", 102, 24_480.0),
        ];
        let got = consensus_spot(&rows, "NIFTY", nearest_expiry_for(&rows, "NIFTY"));
        assert!((got.expect("a spot") - 24_480.0).abs() < f64::EPSILON);
    }

    /// The stale-strike shape the per-strike `LATEST ON ts` query produces:
    /// one strike stopped being quoted at 09:47 and kept 09:47's price while
    /// the rest of the chain moved on. The majority price wins — NOT the
    /// first row, NOT the last.
    #[test]
    fn the_majority_price_wins_over_a_stale_strike() {
        let rows = [
            // The stale one, FIRST in the slice — first-wins would take it.
            candidate("NIFTY", 24_400.0, "CE", 101, 24_100.0),
            candidate("NIFTY", 24_500.0, "CE", 102, 24_480.0),
            candidate("NIFTY", 24_550.0, "CE", 103, 24_480.0),
        ];
        let got = consensus_spot(&rows, "NIFTY", nearest_expiry_for(&rows, "NIFTY"));
        assert!((got.expect("a spot") - 24_480.0).abs() < f64::EPSILON);
    }

    /// Reversing the rows cannot change the answer — the property the whole
    /// rule exists for, pinned here as a case a reader can see rather than
    /// only as a generated one.
    #[test]
    fn reversing_the_rows_gives_the_same_spot() {
        let rows = [
            candidate("NIFTY", 24_400.0, "CE", 101, 24_100.0),
            candidate("NIFTY", 24_500.0, "CE", 102, 24_480.0),
            candidate("NIFTY", 24_550.0, "CE", 103, 24_480.0),
        ];
        let mut back = rows.clone();
        back.reverse();
        let nearest = nearest_expiry_for(&rows, "NIFTY");
        assert_eq!(
            consensus_spot(&rows, "NIFTY", nearest).map(f64::to_bits),
            consensus_spot(&back, "NIFTY", nearest).map(f64::to_bits),
        );
    }

    /// A tie between two equally-attested prices goes to the LOWER one.
    /// Arbitrary between them, deterministic against the input order — which
    /// is the property that stops the socket alternating all day.
    #[test]
    fn an_exact_tie_goes_to_the_lower_price() {
        let rows = [
            candidate("NIFTY", 24_500.0, "CE", 101, 24_600.0),
            candidate("NIFTY", 24_550.0, "CE", 102, 24_400.0),
        ];
        let got = consensus_spot(&rows, "NIFTY", nearest_expiry_for(&rows, "NIFTY"));
        assert!((got.expect("a spot") - 24_400.0).abs() < f64::EPSILON);
    }

    /// Values that are not prices are not candidates for the consensus at
    /// all, however many rows carry them.
    #[test]
    fn non_prices_never_win_the_vote() {
        let rows = [
            candidate("NIFTY", 24_400.0, "CE", 101, 0.0),
            candidate("NIFTY", 24_450.0, "CE", 102, -1.0),
            candidate("NIFTY", 24_500.0, "CE", 103, f64::NAN),
            candidate("NIFTY", 24_550.0, "CE", 104, 24_480.0),
        ];
        let got = consensus_spot(&rows, "NIFTY", nearest_expiry_for(&rows, "NIFTY"));
        assert!((got.expect("a spot") - 24_480.0).abs() < f64::EPSILON);
    }

    /// No usable price at all is `None`, never a guess.
    #[test]
    fn no_usable_price_is_none() {
        let rows = [candidate("NIFTY", 24_400.0, "CE", 101, 0.0)];
        assert!(consensus_spot(&rows, "NIFTY", nearest_expiry_for(&rows, "NIFTY")).is_none());
    }

    // ---- chain_minutes_from_candidates ----

    #[test]
    fn both_legs_of_a_strike_become_one_pair() {
        let got = chain_minutes_from_candidates(&[
            candidate("NIFTY", 24_500.0, "CE", 101, 24_480.0),
            candidate("NIFTY", 24_500.0, "PE", 102, 24_480.0),
        ]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].underlying, "NIFTY");
        assert!((got[0].spot - 24_480.0).abs() < f64::EPSILON);
        assert_eq!(got[0].pairs.len(), 1);
        assert_eq!(got[0].pairs[0].ce_security_id, 101);
        assert_eq!(got[0].pairs[0].pe_security_id, 102);
    }

    #[test]
    fn a_strike_with_one_leg_is_dropped_whole() {
        // Not "subscribe the call and leave the put where it was" — the two
        // sockets would then be reading different strikes of the same
        // underlying, which is worse than not moving at all.
        let got = chain_minutes_from_candidates(&[
            candidate("NIFTY", 24_500.0, "CE", 101, 24_480.0),
            candidate("NIFTY", 24_600.0, "CE", 103, 24_480.0),
            candidate("NIFTY", 24_600.0, "PE", 104, 24_480.0),
        ]);
        assert_eq!(got[0].pairs.len(), 1);
        assert_eq!(got[0].pairs[0].strike_paise, 2_460_000);
    }

    #[test]
    fn pairs_come_back_in_ascending_strike_order() {
        let got = chain_minutes_from_candidates(&[
            candidate("NIFTY", 24_600.0, "CE", 103, 24_480.0),
            candidate("NIFTY", 24_600.0, "PE", 104, 24_480.0),
            candidate("NIFTY", 24_400.0, "CE", 105, 24_480.0),
            candidate("NIFTY", 24_400.0, "PE", 106, 24_480.0),
            candidate("NIFTY", 24_500.0, "CE", 101, 24_480.0),
            candidate("NIFTY", 24_500.0, "PE", 102, 24_480.0),
        ]);
        let strikes: Vec<i64> = got[0].pairs.iter().map(|p| p.strike_paise).collect();
        assert_eq!(strikes, vec![2_440_000, 2_450_000, 2_460_000]);
    }

    #[test]
    fn two_underlyings_stay_separate() {
        let got = chain_minutes_from_candidates(&[
            candidate("NIFTY", 24_500.0, "CE", 101, 24_480.0),
            candidate("NIFTY", 24_500.0, "PE", 102, 24_480.0),
            candidate("BANKNIFTY", 54_000.0, "CE", 201, 53_950.0),
            candidate("BANKNIFTY", 54_000.0, "PE", 202, 53_950.0),
        ]);
        assert_eq!(got.len(), 2);
        let nifty = got.iter().find(|m| m.underlying == "NIFTY").expect("nifty");
        let bank = got
            .iter()
            .find(|m| m.underlying == "BANKNIFTY")
            .expect("banknifty");
        assert_eq!(nifty.pairs[0].ce_security_id, 101);
        assert_eq!(bank.pairs[0].ce_security_id, 201);
        assert!((bank.spot - 53_950.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_stock_option_never_reaches_the_index_chain() {
        // The index tracker's four sockets are NIFTY and BANKNIFTY by
        // operator lock. A stock leaking in would take one.
        let mut stock = candidate("RELIANCE", 3_000.0, "CE", 301, 2_990.0);
        stock.is_index_option = false;
        let mut stock_pe = candidate("RELIANCE", 3_000.0, "PE", 302, 2_990.0);
        stock_pe.is_index_option = false;
        let got = chain_minutes_from_candidates(&[stock, stock_pe]);
        assert!(got.is_empty());
    }

    #[test]
    fn a_zero_contract_id_is_refused_rather_than_subscribed() {
        // Instrument 0 is a perfectly well-formed subscription that returns
        // nothing forever and looks completely healthy.
        let got = chain_minutes_from_candidates(&[
            candidate("NIFTY", 24_500.0, "CE", 0, 24_480.0),
            candidate("NIFTY", 24_500.0, "PE", 102, 24_480.0),
        ]);
        assert!(got.is_empty());
    }

    #[test]
    fn a_spot_that_is_not_a_number_drops_the_underlying() {
        let got = chain_minutes_from_candidates(&[
            candidate("NIFTY", 24_500.0, "CE", 101, f64::NAN),
            candidate("NIFTY", 24_500.0, "PE", 102, f64::NAN),
        ]);
        assert!(got.is_empty());
    }

    #[test]
    fn an_unrecognised_leg_does_not_fill_either_side() {
        let got = chain_minutes_from_candidates(&[
            candidate("NIFTY", 24_500.0, "XX", 101, 24_480.0),
            candidate("NIFTY", 24_500.0, "PE", 102, 24_480.0),
        ]);
        assert!(got.is_empty());
    }

    #[test]
    fn an_empty_slice_is_an_empty_result_not_a_panic() {
        assert!(chain_minutes_from_candidates(&[]).is_empty());
    }

    // ---- atm_pair_for ----

    #[test]
    fn the_nearest_strike_to_spot_wins() {
        let rows = vec![
            candidate("RELIANCE", 2_900.0, "CE", 1, 2_988.0),
            candidate("RELIANCE", 2_900.0, "PE", 2, 2_988.0),
            candidate("RELIANCE", 3_000.0, "CE", 3, 2_988.0),
            candidate("RELIANCE", 3_000.0, "PE", 4, 2_988.0),
        ];
        let got = atm_pair_for(&rows, "RELIANCE").expect("a pair");
        assert_eq!(got.strike_paise, 300_000);
        assert_eq!(got.ce_security_id, 3);
        assert_eq!(got.pe_security_id, 4);
    }

    #[test]
    fn an_exact_midpoint_resolves_the_same_way_every_time() {
        // Spot exactly between two strikes. Without the deterministic order
        // the `HashMap` iteration decides, and the socket flips between two
        // strikes on alternating minutes over a distinction not in the data.
        let rows = vec![
            candidate("RELIANCE", 2_900.0, "CE", 1, 2_950.0),
            candidate("RELIANCE", 2_900.0, "PE", 2, 2_950.0),
            candidate("RELIANCE", 3_000.0, "CE", 3, 2_950.0),
            candidate("RELIANCE", 3_000.0, "PE", 4, 2_950.0),
        ];
        let first = atm_pair_for(&rows, "RELIANCE").expect("a pair");
        for _ in 0..50 {
            assert_eq!(atm_pair_for(&rows, "RELIANCE"), Some(first));
        }
        // And it is the LOWER strike, as documented.
        assert_eq!(first.strike_paise, 290_000);
    }

    #[test]
    fn a_ladder_with_no_complete_pair_yields_nothing() {
        let rows = vec![
            candidate("RELIANCE", 2_900.0, "CE", 1, 2_988.0),
            candidate("RELIANCE", 3_000.0, "CE", 3, 2_988.0),
        ];
        assert_eq!(atm_pair_for(&rows, "RELIANCE"), None);
    }

    #[test]
    fn an_underlying_absent_from_the_slice_yields_nothing() {
        let rows = vec![candidate("NIFTY", 24_500.0, "CE", 101, 24_480.0)];
        assert_eq!(atm_pair_for(&rows, "RELIANCE"), None);
    }

    #[test]
    fn a_ladder_with_no_usable_spot_yields_nothing() {
        let rows = vec![
            candidate("RELIANCE", 2_900.0, "CE", 1, 0.0),
            candidate("RELIANCE", 2_900.0, "PE", 2, 0.0),
        ];
        assert_eq!(atm_pair_for(&rows, "RELIANCE"), None);
    }

    // ---- top_mover_pick ----

    #[test]
    fn the_biggest_absolute_move_leads_in_either_direction() {
        let rows = vec![mover(10, "ALPHA", 8.0), mover(20, "BETA", -9.0)];
        let candidates = vec![
            candidate("BETA", 100.0, "CE", 501, 99.0),
            candidate("BETA", 100.0, "PE", 502, 99.0),
        ];
        let got = top_mover_pick(&rows, &candidates).expect("a pick");
        assert_eq!(got.underlying_security_id, 20);
        assert!((got.pct_change + 9.0).abs() < f64::EPSILON);
        // A faller carries the PUT — the side with the order flow.
        assert_eq!(got.leg_security_id(), 502);
    }

    #[test]
    fn a_riser_carries_the_call() {
        let rows = vec![mover(10, "ALPHA", 8.21)];
        let candidates = vec![
            candidate("ALPHA", 100.0, "CE", 601, 99.0),
            candidate("ALPHA", 100.0, "PE", 602, 99.0),
        ];
        let got = top_mover_pick(&rows, &candidates).expect("a pick");
        assert_eq!(got.leg_security_id(), 601);
        assert_eq!(got.contract_segment, ExchangeSegment::NseFno);
    }

    #[test]
    fn a_leader_with_no_ladder_leaves_the_socket_alone() {
        // Refusing is safe; switching on a stock whose contracts we cannot
        // name is not.
        let rows = vec![mover(10, "ALPHA", 8.0)];
        let candidates = vec![
            candidate("BETA", 100.0, "CE", 501, 99.0),
            candidate("BETA", 100.0, "PE", 502, 99.0),
        ];
        assert_eq!(top_mover_pick(&rows, &candidates), None);
    }

    #[test]
    fn a_flat_or_unmeasurable_move_never_leads() {
        let candidates = vec![
            candidate("ALPHA", 100.0, "CE", 601, 99.0),
            candidate("ALPHA", 100.0, "PE", 602, 99.0),
        ];
        let rows = vec![
            mover(10, "ALPHA", 0.0),
            mover(11, "ALPHA", f64::NAN),
            mover(12, "ALPHA", f64::INFINITY),
        ];
        assert_eq!(top_mover_pick(&rows, &candidates), None);
    }

    #[test]
    fn a_flat_leader_does_not_block_a_real_one_behind_it() {
        let rows = vec![mover(10, "ALPHA", 0.0), mover(20, "BETA", -3.0)];
        let candidates = vec![
            candidate("BETA", 100.0, "CE", 501, 99.0),
            candidate("BETA", 100.0, "PE", 502, 99.0),
        ];
        let got = top_mover_pick(&rows, &candidates).expect("a pick");
        assert_eq!(got.underlying_security_id, 20);
    }

    #[test]
    fn an_empty_ranking_yields_nothing() {
        assert_eq!(top_mover_pick(&[], &[]), None);
    }

    #[test]
    fn a_tie_on_absolute_size_keeps_the_first_seen() {
        // Strictly-greater, not greater-or-equal: an exact tie must not shuffle
        // the socket every minute between two names that never separate.
        let rows = vec![mover(10, "ALPHA", 5.0), mover(20, "BETA", -5.0)];
        let candidates = vec![
            candidate("ALPHA", 100.0, "CE", 601, 99.0),
            candidate("ALPHA", 100.0, "PE", 602, 99.0),
        ];
        let got = top_mover_pick(&rows, &candidates).expect("a pick");
        assert_eq!(got.underlying_security_id, 10);
    }

    // ---- build_movers_query ----

    #[test]
    fn the_movers_query_ranks_on_the_previous_day_column() {
        let sql = build_movers_query(1_900_000_000_000_000);
        assert!(
            sql.contains("close_pct_from_prev_day"),
            "the operator's percentage change is against yesterday's close"
        );
        assert!(
            !sql.contains("open_pct"),
            "open_pct is the PRE-OPEN percentage — a plausible, completely \
             different ranking: {sql}"
        );
    }

    #[test]
    fn the_movers_query_is_bounded_to_today() {
        // Without this the same LATEST ON returns yesterday's ranking, which
        // is indistinguishable from a real one.
        let sql = build_movers_query(1_900_000_000_000_000);
        assert!(sql.contains("ts >= 1900000000000000"), "{sql}");
        assert!(
            sql.contains("LATEST ON ts PARTITION BY security_id"),
            "{sql}"
        );
    }

    #[test]
    fn the_movers_query_reads_cash_equities_not_indices() {
        let sql = build_movers_query(1);
        assert!(sql.contains("'NSE_EQ'"), "{sql}");
        assert!(!sql.contains("IDX_I"), "{sql}");
    }

    #[test]
    fn the_movers_query_inner_joins_so_an_unnamed_stock_never_ranks() {
        let sql = build_movers_query(1);
        assert!(
            !sql.to_ascii_uppercase().contains("LEFT JOIN"),
            "a LEFT join returns rows that can rank but never resolve: {sql}"
        );
        assert!(sql.contains("instrument_lifecycle"), "{sql}");
    }

    // ---- parse_movers_dataset ----

    #[test]
    fn a_well_formed_dataset_parses() {
        let body = r#"{"dataset":[[2885,"RELIANCE",8.21],[1594,"INFY",-5.93]]}"#;
        let got = parse_movers_dataset(body).expect("valid dataset");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].security_id, 2885);
        assert_eq!(got[0].symbol, "RELIANCE");
        assert!((got[0].pct_change - 8.21).abs() < 1e-9);
        assert_eq!(got[0].segment, ExchangeSegment::NseEquity);
        assert!((got[1].pct_change + 5.93).abs() < 1e-9);
    }

    #[test]
    fn garbage_is_an_error_not_an_empty_ranking() {
        // The distinction that matters: an empty Vec for garbage is
        // indistinguishable from a genuinely flat market, and the response to
        // those two differs.
        assert!(parse_movers_dataset("not json").is_err());
        assert!(parse_movers_dataset(r#"{"columns":[]}"#).is_err());
    }

    #[test]
    fn an_empty_dataset_is_a_flat_market_not_an_error() {
        let got = parse_movers_dataset(r#"{"dataset":[]}"#).expect("valid, empty");
        assert!(got.is_empty());
    }

    #[test]
    fn a_bad_row_is_skipped_and_the_good_ones_survive() {
        let body = r#"{"dataset":[
            [0,"ZEROID",1.0],
            [2885,"",2.0],
            [1594,"INFY"],
            "not a row",
            [7,"SHORT"],
            [2885,"RELIANCE",8.21]
        ]}"#;
        let got = parse_movers_dataset(body).expect("valid dataset");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].symbol, "RELIANCE");
    }

    #[test]
    fn a_null_percentage_is_skipped_rather_than_read_as_flat() {
        // A stock whose candle carried no baseline. Zero would rank it as
        // flat, which is a claim; absent is the truth.
        let body = r#"{"dataset":[[2885,"RELIANCE",null],[1594,"INFY",1.5]]}"#;
        let got = parse_movers_dataset(body).expect("valid dataset");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].symbol, "INFY");
    }

    #[test]
    fn the_row_converts_to_the_identity_the_ranking_takes() {
        let row = mover(2885, "RELIANCE", 8.21);
        let m = row.to_move();
        assert_eq!(m.security_id, 2885);
        assert_eq!(m.segment, ExchangeSegment::NseEquity);
        assert!((m.pct_change - 8.21).abs() < f64::EPSILON);
        assert_eq!(m.key(), (2885, ExchangeSegment::NseEquity));
    }

    // ---- the borrowed view ----

    #[test]
    fn the_owned_minute_lends_the_tracker_its_pairs() {
        let owned = OwnedChainMinute {
            underlying: "NIFTY".to_owned(),
            spot: 24_480.0,
            pairs: vec![StrikePair {
                strike_paise: 2_450_000,
                ce_security_id: 101,
                pe_security_id: 102,
            }],
        };
        let borrowed = owned.as_minute();
        assert_eq!(borrowed.underlying, "NIFTY");
        assert!((borrowed.spot - 24_480.0).abs() < f64::EPSILON);
        assert_eq!(borrowed.pairs.len(), 1);
        assert_eq!(borrowed.pairs[0].ce_security_id, 101);
    }

    #[test]
    fn the_stock_option_segment_can_actually_carry_a_book() {
        // A segment Dhan does not serve depth on would produce a socket that
        // dies on connect (200-level) or sits live and silent (20-level).
        assert!(crate::dhan_depth_universe::segment_supports_depth(
            STOCK_OPTION_SEGMENT
        ));
    }
}

#[cfg(test)]
mod plan_minute_tests {
    use super::*;
    use crate::depth200_atm::{DEPTH_200_TOP_MOVER_SOCKET, Depth200AtmConfig};

    fn candidate(underlying: &str, strike: f64, leg: &str, id: i64, spot: f64) -> DepthCandidate {
        DepthCandidate {
            underlying: underlying.to_owned(),
            contract_security_id: id,
            expiry_micros: 1_900_000_000_000_000,
            strike,
            spot,
            leg: leg.to_owned(),
            is_index_option: underlying == "NIFTY" || underlying == "BANKNIFTY",
        }
    }

    fn mover(id: u64, symbol: &str, pct: f64) -> MoverRow {
        MoverRow {
            security_id: id,
            segment: MOVER_UNDERLYING_SEGMENT,
            symbol: symbol.to_owned(),
            pct_change: pct,
        }
    }

    fn instrument(id: u64, segment: ExchangeSegment) -> SubscribeInstrument {
        SubscribeInstrument {
            security_id: id,
            segment,
        }
    }

    /// A NIFTY chain at 50-point spacing around `spot`, plus BANKNIFTY at 100.
    fn index_chain(nifty_spot: f64, bank_spot: f64) -> Vec<DepthCandidate> {
        let mut out = Vec::new();
        for k in 0..5 {
            let strike = 24_300.0 + f64::from(k) * 50.0;
            let ce = 1_000 + i64::from(k);
            let pe = 2_000 + i64::from(k);
            out.push(candidate("NIFTY", strike, "CE", ce, nifty_spot));
            out.push(candidate("NIFTY", strike, "PE", pe, nifty_spot));
        }
        for k in 0..5 {
            let strike = 53_800.0 + f64::from(k) * 100.0;
            let ce = 3_000 + i64::from(k);
            let pe = 4_000 + i64::from(k);
            out.push(candidate("BANKNIFTY", strike, "CE", ce, bank_spot));
            out.push(candidate("BANKNIFTY", strike, "PE", pe, bank_spot));
        }
        out
    }

    /// The four index sockets in dial order: NIFTY call, NIFTY put,
    /// BANKNIFTY call, BANKNIFTY put.
    fn held_four(nifty_k: i64, bank_k: i64) -> Vec<SubscribeInstrument> {
        vec![
            instrument(
                u64::try_from(1_000 + nifty_k).expect("positive"),
                ExchangeSegment::NseFno,
            ),
            instrument(
                u64::try_from(2_000 + nifty_k).expect("positive"),
                ExchangeSegment::NseFno,
            ),
            instrument(
                u64::try_from(3_000 + bank_k).expect("positive"),
                ExchangeSegment::NseFno,
            ),
            instrument(
                u64::try_from(4_000 + bank_k).expect("positive"),
                ExchangeSegment::NseFno,
            ),
        ]
    }

    fn trackers() -> (Depth200AtmTracker, TopMoverSocket) {
        (
            Depth200AtmTracker::new(Depth200AtmConfig::default()),
            TopMoverSocket::default(),
        )
    }

    #[test]
    fn a_settled_minute_costs_nothing_at_all() {
        let (mut tracker, mut top) = trackers();
        let candidates = index_chain(24_400.0, 54_000.0);
        // Seed both engines so nothing is a first adoption.
        let held = held_four(2, 2);
        let movers = vec![mover(10, "ALPHA", 5.0)];
        let alpha = vec![
            candidate("ALPHA", 100.0, "CE", 601, 99.0),
            candidate("ALPHA", 100.0, "PE", 602, 99.0),
        ];
        let mut all = candidates.clone();
        all.extend(alpha.iter().cloned());
        // Run enough minutes for the top mover to adopt and settle.
        for _ in 0..6 {
            let _ = plan_minute(&mut tracker, &mut top, &held, &all, &movers);
        }
        let quiet = plan_minute(&mut tracker, &mut top, &held, &all, &movers);
        assert!(
            quiet.is_quiet(),
            "an unchanged minute must cost zero wire calls: {quiet:?}"
        );
        assert_eq!(
            quiet.top_mover_idle,
            Some(NoTopMoverSwitch::AlreadySubscribed)
        );
    }

    #[test]
    fn the_fifth_socket_first_adoption_is_a_subscribe_not_a_swap() {
        // Nothing is subscribed yet, so there is no `old`. Reporting it as a
        // swap would ask the guard to drop an instrument the connection never
        // held, and the guard refuses fail-closed.
        let (mut tracker, mut top) = trackers();
        let candidates = vec![
            candidate("ALPHA", 100.0, "CE", 601, 99.0),
            candidate("ALPHA", 100.0, "PE", 602, 99.0),
        ];
        let movers = vec![mover(10, "ALPHA", 5.0)];
        let got = plan_minute(&mut tracker, &mut top, &[], &candidates, &movers);
        assert!(got.top_mover_swap.is_none());
        assert_eq!(
            got.top_mover_first,
            Some(instrument(601, ExchangeSegment::NseFno))
        );
        assert!(!got.is_quiet());
    }

    #[test]
    fn a_market_with_no_movers_leaves_the_fifth_socket_alone() {
        // A stale deep book beats an empty one. Unsubscribing here spends a
        // wire call to end up with less.
        let (mut tracker, mut top) = trackers();
        let candidates = index_chain(24_400.0, 54_000.0);
        let got = plan_minute(&mut tracker, &mut top, &held_four(2, 2), &candidates, &[]);
        assert_eq!(got.top_mover_idle, Some(NoTopMoverSwitch::NoMover));
        assert!(got.top_mover_swap.is_none());
        assert!(got.top_mover_first.is_none());
    }

    #[test]
    fn a_missing_movers_query_does_not_stop_the_index_sockets() {
        // The two halves are independent facts about the market. One being
        // unavailable is not evidence about the other.
        let (mut tracker, mut top) = trackers();
        // Seed the tracker at the 24,400 strike.
        let seed = index_chain(24_400.0, 54_000.0);
        let held = held_four(2, 2);
        let _ = plan_minute(&mut tracker, &mut top, &held, &seed, &[]);
        // Spot moves a full strike, with NO movers available at all. The
        // tracker has its OWN confirmation gate, so this takes more than one
        // minute — the point of the test is that it happens at all while the
        // movers half is dark, not that it happens instantly.
        let moved = index_chain(24_500.0, 54_000.0);
        let mut swapped = false;
        for _ in 0..4 {
            let got = plan_minute(&mut tracker, &mut top, &held, &moved, &[]);
            assert_eq!(got.top_mover_idle, Some(NoTopMoverSwitch::NoMover));
            if !got.atm_swaps.is_empty() {
                swapped = true;
                break;
            }
        }
        assert!(
            swapped,
            "the index sockets must act on their own evidence even with the \
             movers query dark"
        );
    }

    #[test]
    fn an_index_socket_swap_names_the_socket_it_is_for() {
        let (mut tracker, mut top) = trackers();
        let held = held_four(2, 2);
        let _ = plan_minute(
            &mut tracker,
            &mut top,
            &held,
            &index_chain(24_400.0, 54_000.0),
            &[],
        );
        let got = plan_minute(
            &mut tracker,
            &mut top,
            &held,
            &index_chain(24_500.0, 54_000.0),
            &[],
        );
        for swap in &got.atm_swaps {
            assert!(
                swap.socket_index < DEPTH_200_TOP_MOVER_SOCKET,
                "an index swap must never target the fifth socket: {swap:?}"
            );
            assert_ne!(
                swap.old, swap.new,
                "a swap to the same instrument is a wasted wire call"
            );
        }
    }

    #[test]
    fn a_stock_in_the_candidate_slice_never_takes_an_index_socket() {
        // The four index sockets are NIFTY and BANKNIFTY by operator lock.
        let (mut tracker, mut top) = trackers();
        let mut candidates = index_chain(24_400.0, 54_000.0);
        candidates.push(candidate("ALPHA", 100.0, "CE", 601, 99.0));
        candidates.push(candidate("ALPHA", 100.0, "PE", 602, 99.0));
        let held = held_four(2, 2);
        let _ = plan_minute(&mut tracker, &mut top, &held, &candidates, &[]);
        let got = plan_minute(&mut tracker, &mut top, &held, &candidates, &[]);
        assert!(got.atm_swaps.is_empty(), "{got:?}");
    }

    #[test]
    fn an_empty_minute_decides_nothing_and_does_not_panic() {
        let (mut tracker, mut top) = trackers();
        let got = plan_minute(&mut tracker, &mut top, &[], &[], &[]);
        assert!(got.is_quiet());
        assert_eq!(got.top_mover_idle, Some(NoTopMoverSwitch::NoMover));
    }

    #[test]
    fn a_challenger_waits_out_the_confirmation_gate_before_the_socket_moves() {
        let (mut tracker, mut top) = trackers();
        let candidates = vec![
            candidate("ALPHA", 100.0, "CE", 601, 99.0),
            candidate("ALPHA", 100.0, "PE", 602, 99.0),
            candidate("BETA", 200.0, "CE", 701, 199.0),
            candidate("BETA", 200.0, "PE", 702, 199.0),
        ];
        // ALPHA adopts immediately.
        let first = plan_minute(
            &mut tracker,
            &mut top,
            &[],
            &candidates,
            &[mover(10, "ALPHA", 5.0)],
        );
        assert_eq!(
            first.top_mover_first,
            Some(instrument(601, ExchangeSegment::NseFno))
        );

        // BETA leads. It must hold the lead, not merely appear at the top.
        let beta = vec![mover(20, "BETA", 9.0), mover(10, "ALPHA", 5.0)];
        let mut moved_at = None;
        for minute in 1..=6 {
            let got = plan_minute(&mut tracker, &mut top, &[], &candidates, &beta);
            if got.top_mover_swap.is_some() {
                moved_at = Some(minute);
                break;
            }
            assert_eq!(
                got.top_mover_idle,
                Some(NoTopMoverSwitch::AwaitingConfirmation)
            );
        }
        assert_eq!(
            moved_at,
            Some(
                i32::try_from(crate::depth200_atm::TOP_MOVER_CONFIRM_OBSERVATIONS).expect("small")
            ),
            "the socket must move exactly when the gate is satisfied, no sooner"
        );
    }

    #[test]
    fn the_fifth_socket_swap_carries_the_old_instrument_it_actually_held() {
        // The guard replaces in place and refuses fail-closed if the old
        // instrument is not on the connection. A swap that invents an old
        // would be refused every time and the socket would never move.
        let (mut tracker, mut top) = trackers();
        let candidates = vec![
            candidate("ALPHA", 100.0, "CE", 601, 99.0),
            candidate("ALPHA", 100.0, "PE", 602, 99.0),
        ];
        // Adopt the CALL on a riser.
        let first = plan_minute(
            &mut tracker,
            &mut top,
            &[],
            &candidates,
            &[mover(10, "ALPHA", 5.0)],
        );
        let adopted = first.top_mover_first.expect("first adoption");
        assert_eq!(adopted, instrument(601, ExchangeSegment::NseFno));

        // The same stock flips sign. The busy leg is now the put.
        let flipped = [mover(10, "ALPHA", -5.0)];
        let mut swap = None;
        for _ in 0..6 {
            let got = plan_minute(&mut tracker, &mut top, &[], &candidates, &flipped);
            if let Some(s) = got.top_mover_swap {
                swap = Some(s);
                break;
            }
        }
        let swap = swap.expect("a direction flip must eventually move the socket");
        assert_eq!(swap.old, adopted, "the old must be what was actually held");
        assert_eq!(swap.new, instrument(602, ExchangeSegment::NseFno));
        assert_eq!(swap.socket_index, DEPTH_200_TOP_MOVER_SOCKET);
    }

    #[test]
    fn a_leader_whose_ladder_is_missing_is_an_unusable_minute_not_a_switch() {
        let (mut tracker, mut top) = trackers();
        // ALPHA leads but no ALPHA contracts exist in the slice.
        let candidates = index_chain(24_400.0, 54_000.0);
        let got = plan_minute(
            &mut tracker,
            &mut top,
            &held_four(2, 2),
            &candidates,
            &[mover(10, "ALPHA", 8.0)],
        );
        assert_eq!(got.top_mover_idle, Some(NoTopMoverSwitch::NoMover));
        assert!(got.is_quiet());
    }
}

#[cfg(test)]
mod apply_tests {
    use super::*;
    use crate::depth200_atm::SwitchReason;

    fn instrument(id: u64, segment: ExchangeSegment) -> SubscribeInstrument {
        SubscribeInstrument {
            security_id: id,
            segment,
        }
    }

    fn swap(socket_index: usize, old: u64, new: u64) -> PlannedSwap {
        PlannedSwap {
            socket_index,
            old: instrument(old, ExchangeSegment::NseFno),
            new: instrument(new, ExchangeSegment::NseFno),
            reason: SwitchReason::SpotMoved,
        }
    }

    fn socket(
        capacity: usize,
        held: u64,
    ) -> (
        RebalanceSocket,
        tokio::sync::mpsc::Receiver<LiveSubscriptionCommand>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::channel(capacity);
        (
            RebalanceSocket {
                tx,
                held: Some(instrument(held, ExchangeSegment::NseFno)),
            },
            rx,
        )
    }

    #[test]
    fn a_swap_reaches_its_own_socket_and_no_other() {
        let (s0, mut r0) = socket(4, 1_000);
        let (s1, mut r1) = socket(4, 2_000);
        let mut sockets = vec![s0, s1];
        let decision = RebalanceDecision {
            atm_swaps: vec![swap(1, 2_000, 2_001)],
            ..RebalanceDecision::default()
        };
        assert_eq!(apply_decision(&mut sockets, &decision), 1);
        assert!(r0.try_recv().is_err(), "socket 0 must be untouched");
        match r1.try_recv().expect("socket 1 receives") {
            LiveSubscriptionCommand::Swap { old, new } => {
                assert_eq!(old.security_id, 2_000);
                assert_eq!(new.security_id, 2_001);
            }
            other => panic!("expected a swap, got {other:?}"),
        }
    }

    #[test]
    fn a_successful_send_advances_what_the_socket_is_believed_to_hold() {
        let (s0, _r0) = socket(4, 1_000);
        let mut sockets = vec![s0];
        let decision = RebalanceDecision {
            atm_swaps: vec![swap(0, 1_000, 1_001)],
            ..RebalanceDecision::default()
        };
        apply_decision(&mut sockets, &decision);
        assert_eq!(
            sockets[0].held,
            Some(instrument(1_001, ExchangeSegment::NseFno))
        );
    }

    #[test]
    fn a_dropped_send_does_not_advance_the_believed_hold() {
        // The failure this prevents is permanent, not transient: if `held`
        // moved on a dropped message, every future swap would name an old
        // instrument the connection never received, the guard would refuse
        // each one fail-closed, and the socket would never move again.
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(rx);
        let mut sockets = vec![RebalanceSocket {
            tx,
            held: Some(instrument(1_000, ExchangeSegment::NseFno)),
        }];
        let decision = RebalanceDecision {
            atm_swaps: vec![swap(0, 1_000, 1_001)],
            ..RebalanceDecision::default()
        };
        assert_eq!(apply_decision(&mut sockets, &decision), 0);
        assert_eq!(
            sockets[0].held,
            Some(instrument(1_000, ExchangeSegment::NseFno)),
            "a swap that never reached the wire must not move the believed hold"
        );
    }

    #[test]
    fn a_full_channel_drops_this_minute_rather_than_blocking() {
        // Awaiting here would apply backpressure to the frame drain, and a
        // stalled drain is tick loss at Dhan's side with no sequence number to
        // detect it. Dropping costs one stale minute.
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        tx.try_send(LiveSubscriptionCommand::Extend(vec![]))
            .expect("fills the channel");
        let mut sockets = vec![RebalanceSocket {
            tx,
            held: Some(instrument(1_000, ExchangeSegment::NseFno)),
        }];
        let decision = RebalanceDecision {
            atm_swaps: vec![swap(0, 1_000, 1_001)],
            ..RebalanceDecision::default()
        };
        assert_eq!(apply_decision(&mut sockets, &decision), 0);
        assert_eq!(
            sockets[0].held,
            Some(instrument(1_000, ExchangeSegment::NseFno))
        );
    }

    #[test]
    fn a_swap_for_a_socket_that_does_not_exist_is_counted_not_panicked() {
        // The fifth socket is not dialed at attach — nothing exists to put on
        // it before a leader emerges — so a top-mover swap can legitimately
        // arrive with no socket to carry it.
        let (s0, _r0) = socket(4, 1_000);
        let mut sockets = vec![s0];
        let decision = RebalanceDecision {
            top_mover_swap: Some(swap(4, 9_000, 9_001)),
            ..RebalanceDecision::default()
        };
        assert_eq!(apply_decision(&mut sockets, &decision), 0);
    }

    #[test]
    fn a_quiet_minute_sends_nothing() {
        let (s0, mut r0) = socket(4, 1_000);
        let mut sockets = vec![s0];
        let decision = RebalanceDecision::default();
        assert!(decision.is_quiet());
        assert_eq!(apply_decision(&mut sockets, &decision), 0);
        assert!(r0.try_recv().is_err());
    }

    #[test]
    fn every_socket_in_a_multi_swap_minute_gets_exactly_its_own() {
        let (s0, mut r0) = socket(4, 1_000);
        let (s1, mut r1) = socket(4, 2_000);
        let (s2, mut r2) = socket(4, 3_000);
        let (s3, mut r3) = socket(4, 4_000);
        let mut sockets = vec![s0, s1, s2, s3];
        let decision = RebalanceDecision {
            atm_swaps: vec![
                swap(0, 1_000, 1_001),
                swap(1, 2_000, 2_001),
                swap(2, 3_000, 3_001),
                swap(3, 4_000, 4_001),
            ],
            ..RebalanceDecision::default()
        };
        assert_eq!(apply_decision(&mut sockets, &decision), 4);
        for (rx, expected) in [
            (&mut r0, 1_001_u64),
            (&mut r1, 2_001),
            (&mut r2, 3_001),
            (&mut r3, 4_001),
        ] {
            match rx.try_recv().expect("one command") {
                LiveSubscriptionCommand::Swap { new, .. } => {
                    assert_eq!(new.security_id, expected);
                }
                other => panic!("expected a swap, got {other:?}"),
            }
            assert!(rx.try_recv().is_err(), "exactly one command per socket");
        }
    }

    #[test]
    fn the_offset_clears_the_boundary_the_writer_needs() {
        // A query fired at :00 reads the minute before last, because the
        // candle seals AT the boundary and the writer has not flushed yet.
        // The whole loop would then run a minute behind the market all
        // session, looking exactly like normal operation.
        assert!(REBALANCE_OFFSET_SECS > 0);
        assert!(REBALANCE_OFFSET_SECS < 60);
    }

    #[test]
    fn every_refusal_reason_is_a_registered_label() {
        // A reason string that is not in the list is a counter nothing
        // pre-registers, so its series appears only after the first failure —
        // and an absent series is indistinguishable from a dead process.
        assert!(REBALANCE_REFUSAL_REASONS.contains(&"no_socket"));
        assert!(REBALANCE_REFUSAL_REASONS.contains(&"channel_full"));
        assert!(REBALANCE_REFUSAL_REASONS.contains(&"channel_closed"));
        pre_register_rebalance_counters();
    }
}

#[cfg(test)]
mod schedule_tests {
    use super::*;

    #[test]
    fn before_the_slot_it_waits_only_the_remaining_seconds() {
        assert_eq!(secs_until_next_rebalance(0), REBALANCE_OFFSET_SECS);
        assert_eq!(secs_until_next_rebalance(1), REBALANCE_OFFSET_SECS - 1);
        assert_eq!(secs_until_next_rebalance(REBALANCE_OFFSET_SECS - 1), 1);
    }

    #[test]
    fn at_the_slot_it_waits_a_whole_minute_rather_than_spinning() {
        // A zero sleep at the exact offset second would fire a query every
        // loop iteration until the second rolled over — a busy loop that
        // surfaces as inexplicable database load, not as an error.
        assert_eq!(secs_until_next_rebalance(REBALANCE_OFFSET_SECS), 60);
    }

    #[test]
    fn it_never_sleeps_for_zero_at_any_second_of_the_minute() {
        for second in 0..60 {
            let wait = secs_until_next_rebalance(second);
            assert!(wait > 0, "second {second} would spin");
            assert!(wait <= 60, "second {second} would skip a minute: {wait}");
        }
    }

    #[test]
    fn every_second_lands_on_the_offset() {
        // The property that actually matters: whatever second we start at,
        // waking up puts us at the offset past a minute boundary — never at
        // :00, where the candle has sealed but the writer has not flushed.
        for second in 0..60 {
            let landed = (second + secs_until_next_rebalance(second)) % 60;
            assert_eq!(landed, REBALANCE_OFFSET_SECS, "from second {second}");
        }
    }

    #[test]
    fn late_in_the_minute_it_waits_into_the_next_one() {
        assert_eq!(secs_until_next_rebalance(59), 1 + REBALANCE_OFFSET_SECS);
        assert_eq!(secs_until_next_rebalance(30), 30 + REBALANCE_OFFSET_SECS);
    }

    #[test]
    fn the_movers_timeout_cannot_outlive_its_own_minute() {
        // A query that outlived its minute would return a ranking for a minute
        // that has already passed, and the socket would chase the market one
        // step behind all session — which looks exactly like working.
        assert!(MOVERS_QUERY_TIMEOUT_SECS < 60);
        assert!(MOVERS_QUERY_TIMEOUT_SECS > 0);
    }
}

#[cfg(test)]
mod fifth_socket_tests {
    use super::*;
    use crate::depth200_atm::DEPTH_200_TOP_MOVER_SOCKET;

    #[test]
    fn the_fifth_socket_index_is_the_last_of_the_authorized_connections() {
        // The index the planner names and the connection the dial creates must
        // be the same socket. `plan_pool` assigns instruments to connections
        // in order, so five depth-200 instruments become indices 0..4 — and 4
        // is where the top mover belongs. Off by one here and the top mover
        // would land on BANKNIFTY's put socket, which is a valid subscription
        // to entirely the wrong contract.
        assert_eq!(
            DEPTH_200_TOP_MOVER_SOCKET,
            crate::dhan_depth_universe::DEPTH_200_TOTAL_SOCKETS - 1
        );
    }

    #[test]
    fn the_pair_budget_stays_even_so_no_lone_leg_can_strand() {
        // The reason the two budgets are separate constants. An odd PAIR
        // budget lets the selector reach for a third underlying and stop
        // half-way, filling the fifth socket with a lone leg — the exact shape
        // the 2026-08-26 retirement removed.
        assert_eq!(crate::dhan_depth_universe::DEPTH_200_MAX_SOCKETS % 2, 0);
        assert!(
            crate::dhan_depth_universe::DEPTH_200_MAX_SOCKETS
                < crate::dhan_depth_universe::DEPTH_200_TOTAL_SOCKETS
        );
    }

    #[test]
    fn the_four_pair_sockets_and_the_fifth_together_are_the_whole_budget() {
        assert_eq!(
            crate::dhan_depth_universe::DEPTH_200_MAX_SOCKETS + 1,
            crate::dhan_depth_universe::DEPTH_200_TOTAL_SOCKETS
        );
    }

    #[test]
    fn the_attach_dials_the_fifth_socket_from_the_same_rule_that_steers_it() {
        // If the dial picked by a different rule, the socket's first contract
        // would be one the rebalance would never have chosen, and its first
        // real minute would be a swap away from it — a wasted wire call on
        // every session start.
        let source = include_str!("dhan_feed_stack.rs");
        let production = source
            .split_once("\n#[cfg(test)]")
            .map_or(source, |(before, _)| before);
        assert!(
            production.contains("crate::depth_rebalance::top_mover_pick("),
            "the fifth socket must be dialed by top_mover_pick — the SAME function \
             plan_minute calls, so the dialed contract is one the rebalance would \
             have chosen"
        );
    }

    #[test]
    fn the_loop_seeds_its_tracker_from_the_socket_rather_than_a_parameter() {
        // A seed passed in can disagree with the wire; a seed read from the
        // socket cannot. And an unseeded tracker re-subscribes what the dial
        // already placed, which Dhan answers with a Fatal 804.
        let source = include_str!("depth_rebalance.rs");
        let production = source
            .split_once("\n#[cfg(test)]")
            .map_or(source, |(before, _)| before);
        assert!(
            production.contains("TopMoverSocket::seeded(held,"),
            "the rebalance loop must seed the top-mover tracker from what the socket holds"
        );
    }
}

#[cfg(test)]
mod current_expiry_tests {
    use super::*;

    const THIS_WEEK: i64 = 1_900_000_000_000_000;
    const NEXT_WEEK: i64 = 1_900_604_800_000_000;
    const NEXT_MONTH: i64 = 1_902_592_000_000_000;

    fn at(
        underlying: &str,
        expiry: i64,
        strike: f64,
        leg: &str,
        id: i64,
        index: bool,
    ) -> DepthCandidate {
        DepthCandidate {
            underlying: underlying.to_owned(),
            contract_security_id: id,
            expiry_micros: expiry,
            strike,
            spot: 24_500.0,
            leg: leg.to_owned(),
            is_index_option: index,
        }
    }

    /// The collision the operator caught, reproduced exactly.
    ///
    /// Both expiries list a 24500 strike. Keyed on (underlying, strike) alone
    /// they share one map slot, and the row that happens to arrive last wins.
    #[test]
    fn a_far_expiry_never_wins_a_strike_from_the_near_one() {
        let c = vec![
            at("NIFTY", THIS_WEEK, 24_500.0, "CE", 101, true),
            at("NIFTY", THIS_WEEK, 24_500.0, "PE", 102, true),
            // Same strike, next week. Listed AFTER, so without the filter it
            // overwrites — and nothing downstream could tell.
            at("NIFTY", NEXT_WEEK, 24_500.0, "CE", 901, true),
            at("NIFTY", NEXT_WEEK, 24_500.0, "PE", 902, true),
        ];
        let got = chain_minutes_from_candidates(&c);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].pairs.len(), 1);
        assert_eq!(
            got[0].pairs[0].ce_security_id, 101,
            "the near expiry's contract must win"
        );
        assert_eq!(got[0].pairs[0].pe_security_id, 102);
    }

    #[test]
    fn the_at_the_money_pair_comes_from_the_near_expiry_too() {
        let c = vec![
            at("RELIANCE", THIS_WEEK, 24_500.0, "CE", 101, false),
            at("RELIANCE", THIS_WEEK, 24_500.0, "PE", 102, false),
            at("RELIANCE", NEXT_MONTH, 24_500.0, "CE", 901, false),
            at("RELIANCE", NEXT_MONTH, 24_500.0, "PE", 902, false),
        ];
        let got = atm_pair_for(&c, "RELIANCE").expect("a pair");
        assert_eq!(got.ce_security_id, 101);
        assert_eq!(got.pe_security_id, 102);
    }

    #[test]
    fn a_far_expiry_strike_nearer_to_spot_still_loses() {
        // The nastiest shape: the far expiry lists a strike CLOSER to spot.
        // A filter applied after the at-the-money search would pick it.
        let c = vec![
            at("NIFTY", THIS_WEEK, 24_400.0, "CE", 101, true),
            at("NIFTY", THIS_WEEK, 24_400.0, "PE", 102, true),
            at("NIFTY", NEXT_WEEK, 24_500.0, "CE", 901, true),
            at("NIFTY", NEXT_WEEK, 24_500.0, "PE", 902, true),
        ];
        let got = atm_pair_for(&c, "NIFTY").expect("a pair");
        assert_eq!(
            got.ce_security_id, 101,
            "spot is 24500 and the far expiry has the exact strike — it must \
             STILL lose, because expiry is filtered before the search, not after"
        );
    }

    #[test]
    fn nifty_takes_its_weekly_while_banknifty_takes_its_monthly() {
        // ONE rule, both answers. NIFTY lists weeklies so its nearest is this
        // week's; BANKNIFTY lists monthlies only so its nearest is this
        // month's. Neither needs the code to know which is which.
        let c = vec![
            at("NIFTY", THIS_WEEK, 24_500.0, "CE", 101, true),
            at("NIFTY", THIS_WEEK, 24_500.0, "PE", 102, true),
            at("NIFTY", NEXT_WEEK, 24_500.0, "CE", 111, true),
            at("NIFTY", NEXT_WEEK, 24_500.0, "PE", 112, true),
            at("NIFTY", NEXT_MONTH, 24_500.0, "CE", 121, true),
            at("NIFTY", NEXT_MONTH, 24_500.0, "PE", 122, true),
            at("BANKNIFTY", THIS_WEEK, 24_500.0, "CE", 201, true),
            at("BANKNIFTY", THIS_WEEK, 24_500.0, "PE", 202, true),
            at("BANKNIFTY", NEXT_MONTH, 24_500.0, "CE", 211, true),
            at("BANKNIFTY", NEXT_MONTH, 24_500.0, "PE", 212, true),
        ];
        let got = chain_minutes_from_candidates(&c);
        let nifty = got.iter().find(|m| m.underlying == "NIFTY").expect("nifty");
        let bank = got
            .iter()
            .find(|m| m.underlying == "BANKNIFTY")
            .expect("banknifty");
        assert_eq!(nifty.pairs[0].ce_security_id, 101);
        assert_eq!(bank.pairs[0].ce_security_id, 201);
    }

    #[test]
    fn each_underlying_gets_its_own_nearest_not_the_slices() {
        // BANKNIFTY's nearest is later than NIFTY's. A single slice-wide
        // minimum would drop BANKNIFTY entirely.
        let c = vec![
            at("NIFTY", THIS_WEEK, 24_500.0, "CE", 101, true),
            at("NIFTY", THIS_WEEK, 24_500.0, "PE", 102, true),
            at("BANKNIFTY", NEXT_MONTH, 54_000.0, "CE", 201, true),
            at("BANKNIFTY", NEXT_MONTH, 54_000.0, "PE", 202, true),
        ];
        let got = chain_minutes_from_candidates(&c);
        assert_eq!(got.len(), 2, "both underlyings must survive: {got:?}");
    }

    #[test]
    fn the_whole_near_expiry_ladder_survives_not_just_one_strike() {
        let mut c = Vec::new();
        for k in 0..5_i64 {
            #[expect(clippy::cast_precision_loss, reason = "k is tiny")]
            let strike = 24_400.0 + (k as f64) * 50.0;
            c.push(at("NIFTY", THIS_WEEK, strike, "CE", 100 + k * 2, true));
            c.push(at("NIFTY", THIS_WEEK, strike, "PE", 101 + k * 2, true));
            c.push(at("NIFTY", NEXT_WEEK, strike, "CE", 900 + k * 2, true));
            c.push(at("NIFTY", NEXT_WEEK, strike, "PE", 901 + k * 2, true));
        }
        let got = chain_minutes_from_candidates(&c);
        assert_eq!(got[0].pairs.len(), 5, "every near-expiry strike survives");
        for pair in &got[0].pairs {
            assert!(
                pair.ce_security_id < 900,
                "a far-expiry id leaked into the ladder: {pair:?}"
            );
        }
    }

    #[test]
    fn rows_with_no_expiry_at_all_are_not_refused_on_that_basis() {
        // Nothing to compare against. The id and strike guards still apply;
        // refusing here would drop a whole underlying over a missing field.
        let c = vec![
            at("NIFTY", 0, 24_500.0, "CE", 101, true),
            at("NIFTY", 0, 24_500.0, "PE", 102, true),
        ];
        let got = chain_minutes_from_candidates(&c);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].pairs[0].ce_security_id, 101);
    }

    #[test]
    fn the_nearest_expiry_lookup_ignores_other_underlyings() {
        let c = vec![
            at("NIFTY", NEXT_MONTH, 24_500.0, "CE", 101, true),
            at("BANKNIFTY", THIS_WEEK, 54_000.0, "CE", 201, true),
        ];
        assert_eq!(nearest_expiry_for(&c, "NIFTY"), Some(NEXT_MONTH));
        assert_eq!(nearest_expiry_for(&c, "BANKNIFTY"), Some(THIS_WEEK));
        assert_eq!(nearest_expiry_for(&c, "MISSING"), None);
    }
}

#[cfg(test)]
mod preopen_readiness_tests {
    use super::*;

    const TODAY: i64 = 1_700_000_000_000_000;

    #[test]
    fn the_preopen_ranking_reads_ticks_not_candles() {
        // The whole point: candles_1m cannot have a row before ~09:16.
        let sql = build_preopen_movers_query(TODAY);
        assert!(sql.contains("FROM ticks"), "{sql}");
        assert!(
            !sql.contains("candles_1m"),
            "the fallback reads the very table it exists to avoid: {sql}"
        );
    }

    #[test]
    fn a_zero_previous_close_can_never_reach_the_ranking() {
        // A Ticker-mode packet carries close = 0.0 by documented design.
        // Dividing by it ranks the WORST-populated instruments at the top.
        let sql = build_preopen_movers_query(TODAY);
        assert!(sql.contains("close > 0"), "{sql}");
        assert!(sql.contains("ltp > 0"), "{sql}");
    }

    #[test]
    fn the_preopen_ranking_computes_the_same_quantity_as_the_candle_column() {
        // Not a substitute metric — the same arithmetic, one layer lower.
        let sql = build_preopen_movers_query(TODAY);
        assert!(
            sql.contains("((t.ltp - t.close) / t.close) * 100.0"),
            "{sql}"
        );
        assert!(
            sql.contains("AS close_pct_from_prev_day"),
            "the column must be named what the parser reads: {sql}"
        );
    }

    #[test]
    fn both_rankings_are_bounded_to_today_and_keyed_per_instrument() {
        // An unbounded LATEST ON walks every partition ever written.
        for sql in [build_movers_query(TODAY), build_preopen_movers_query(TODAY)] {
            assert!(sql.contains("ts >= 1700000000000000"), "{sql}");
            assert!(
                sql.contains("LATEST ON ts PARTITION BY security_id"),
                "{sql}"
            );
        }
    }

    #[test]
    fn both_rankings_agree_on_the_segment_and_the_feed() {
        // A ranking that changed segment between its two sources would rank
        // different instruments depending on the time of day.
        let a = build_movers_query(TODAY);
        let b = build_preopen_movers_query(TODAY);
        let seg = MOVER_UNDERLYING_SEGMENT.as_str();
        for sql in [&a, &b] {
            assert!(sql.contains(&format!("segment = '{seg}'")), "{sql}");
            assert!(sql.contains("feed = 'dhan'"), "{sql}");
            assert!(
                sql.contains(&format!("exchange_segment = '{seg}'")),
                "the join side must match too: {sql}"
            );
        }
    }

    #[test]
    fn the_preopen_ranking_parses_with_the_same_parser() {
        // Shipping a second parser is how two readings of one thing drift.
        let body = r#"{"dataset":[[1234,"RELIANCE",2.5],[5678,"TCS",-1.25]]}"#;
        let rows = parse_movers_dataset(body).expect("parses");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].security_id, 1234);
        assert!((rows[0].pct_change - 2.5).abs() < f64::EPSILON);
    }
}

#[cfg(test)]
mod heartbeat_tests {
    use super::*;

    #[test]
    fn a_loop_that_never_evaluated_does_not_report_as_fresh() {
        // Zero would read as "evaluated this instant" — the exact false-OK
        // this gauge exists to prevent.
        let v = rebalance_age_value(0, 1_700_000_000);
        assert!(v > 60.0, "a never-run loop reported an age of {v}");
    }

    #[test]
    fn a_running_loop_reports_the_real_gap() {
        assert!((rebalance_age_value(1_700_000_000, 1_700_000_045) - 45.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_fresh_evaluation_reports_zero() {
        assert!(rebalance_age_value(1_700_000_000, 1_700_000_000).abs() < f64::EPSILON);
    }

    #[test]
    fn a_backwards_clock_never_makes_a_stalled_loop_look_alive() {
        // NTP steps backwards. Reporting a negative age would let a wedged
        // loop read as healthy, which is the one direction that matters.
        assert!(rebalance_age_value(1_700_000_100, 1_700_000_000) >= 0.0);
    }

    #[test]
    fn the_heartbeat_is_published_more_often_than_an_alarm_period() {
        assert!(
            REBALANCE_HEARTBEAT_SECS <= 30,
            "a heartbeat slower than the alarm period can land badly and \
             report a value nearly a full period old"
        );
    }
}

#[cfg(test)]
mod expiry_permutation_tests {
    use super::*;

    const NEAR: i64 = 1_900_000_000_000_000;
    const FAR: i64 = 1_902_592_000_000_000;

    fn c(underlying: &str, expiry: i64, strike: f64, leg: &str, id: i64) -> DepthCandidate {
        DepthCandidate {
            underlying: underlying.to_owned(),
            contract_security_id: id,
            expiry_micros: expiry,
            strike,
            spot: 24_500.0,
            leg: leg.to_owned(),
            is_index_option: true,
        }
    }

    /// The mixed case: some rows carry an expiry, some carry the missing
    /// sentinel. The rows that KNOW their expiry decide, and the ones that do
    /// not are refused rather than allowed to ride along.
    ///
    /// That is the right way round. A row with no expiry could belong to any
    /// month, so admitting it beside dated rows would put an unknown-month
    /// contract on a socket reserved for the current one — exactly what the
    /// filter exists to prevent. Refusing it costs one strike; admitting it
    /// costs the guarantee.
    #[test]
    fn a_row_with_no_expiry_loses_to_siblings_that_have_one() {
        let rows = vec![
            c("NIFTY", NEAR, 24_500.0, "CE", 101),
            c("NIFTY", NEAR, 24_500.0, "PE", 102),
            c("NIFTY", 0, 24_550.0, "CE", 901),
            c("NIFTY", 0, 24_550.0, "PE", 902),
        ];
        let got = chain_minutes_from_candidates(&rows);
        assert_eq!(
            got[0].pairs.len(),
            1,
            "the undated strike rode along: {got:?}"
        );
        assert_eq!(got[0].pairs[0].ce_security_id, 101);
    }

    /// A NEGATIVE expiry is the missing sentinel by another name — a parse
    /// that produced garbage, not a contract that expired before the epoch.
    #[test]
    fn a_negative_expiry_is_treated_as_missing_not_as_the_nearest() {
        let rows = vec![
            c("NIFTY", -5, 24_450.0, "CE", 901),
            c("NIFTY", -5, 24_450.0, "PE", 902),
            c("NIFTY", NEAR, 24_500.0, "CE", 101),
            c("NIFTY", NEAR, 24_500.0, "PE", 102),
        ];
        let got = chain_minutes_from_candidates(&rows);
        assert_eq!(got[0].pairs.len(), 1);
        assert_eq!(
            got[0].pairs[0].ce_security_id, 101,
            "a negative expiry ranked as 'nearest' and won: {got:?}"
        );
    }

    /// Three expiries, not two. The filter takes the nearest, not merely
    /// "not the farthest".
    #[test]
    fn with_three_expiries_only_the_nearest_survives() {
        let mid = (NEAR + FAR) / 2;
        let rows = vec![
            c("NIFTY", FAR, 24_500.0, "CE", 901),
            c("NIFTY", FAR, 24_500.0, "PE", 902),
            c("NIFTY", mid, 24_500.0, "CE", 501),
            c("NIFTY", mid, 24_500.0, "PE", 502),
            c("NIFTY", NEAR, 24_500.0, "CE", 101),
            c("NIFTY", NEAR, 24_500.0, "PE", 102),
        ];
        let got = chain_minutes_from_candidates(&rows);
        assert_eq!(got[0].pairs.len(), 1);
        assert_eq!(got[0].pairs[0].ce_security_id, 101);
    }

    /// Input order must not decide the outcome. The far expiry listed FIRST
    /// is the arrangement that would fool a first-wins implementation.
    #[test]
    fn the_order_rows_arrive_in_does_not_change_which_expiry_wins() {
        let far_first = vec![
            c("NIFTY", FAR, 24_500.0, "CE", 901),
            c("NIFTY", FAR, 24_500.0, "PE", 902),
            c("NIFTY", NEAR, 24_500.0, "CE", 101),
            c("NIFTY", NEAR, 24_500.0, "PE", 102),
        ];
        let near_first: Vec<DepthCandidate> = far_first.iter().rev().cloned().collect();
        let a = chain_minutes_from_candidates(&far_first);
        let b = chain_minutes_from_candidates(&near_first);
        assert_eq!(a[0].pairs[0].ce_security_id, 101);
        assert_eq!(b[0].pairs[0].ce_security_id, 101);
    }

    /// A strike listed in the near expiry with only ONE leg, and both legs in
    /// the far one. The single-leg rule and the expiry rule must not combine
    /// into "fall back to the far month" — that would subscribe exactly what
    /// the operator ruled out, and only for the strikes where it is least
    /// noticeable.
    #[test]
    fn a_single_legged_near_strike_does_not_fall_back_to_the_far_month() {
        let rows = vec![
            c("NIFTY", NEAR, 24_500.0, "CE", 101),
            c("NIFTY", FAR, 24_500.0, "CE", 901),
            c("NIFTY", FAR, 24_500.0, "PE", 902),
        ];
        let got = chain_minutes_from_candidates(&rows);
        assert!(
            got.is_empty() || got[0].pairs.is_empty(),
            "the far month filled a gap the near month left: {got:?}"
        );
    }

    /// Every underlying resolves its own expiry, and one underlying having
    /// only far-dated rows must not drag another onto its month.
    #[test]
    fn one_underlyings_far_expiry_does_not_reach_another() {
        let rows = vec![
            c("NIFTY", NEAR, 24_500.0, "CE", 101),
            c("NIFTY", NEAR, 24_500.0, "PE", 102),
            c("BANKNIFTY", FAR, 54_000.0, "CE", 201),
            c("BANKNIFTY", FAR, 54_000.0, "PE", 202),
        ];
        let got = chain_minutes_from_candidates(&rows);
        assert_eq!(got.len(), 2, "an underlying was dropped: {got:?}");
        for m in &got {
            assert_eq!(m.pairs.len(), 1, "{m:?}");
        }
    }

    /// The at-the-money search and the chain view must agree about which
    /// expiry is current. Two answers to one question is how a socket ends up
    /// holding a month the rest of the system is not looking at.
    #[test]
    fn the_atm_search_and_the_chain_view_pick_the_same_expiry() {
        let mut rows = Vec::new();
        for (exp, base) in [(NEAR, 100_i64), (FAR, 900)] {
            for k in 0..5_i64 {
                #[expect(clippy::cast_precision_loss, reason = "k is tiny")]
                let strike = (k as f64).mul_add(50.0, 24_400.0);
                rows.push(c("NIFTY", exp, strike, "CE", base + k * 2));
                rows.push(c("NIFTY", exp, strike, "PE", base + k * 2 + 1));
            }
        }
        let chain = chain_minutes_from_candidates(&rows);
        let pair = atm_pair_for(&rows, "NIFTY").expect("a pair");
        assert!(
            chain[0]
                .pairs
                .iter()
                .any(|p| p.ce_security_id == pair.ce_security_id),
            "the at-the-money pick is not in the chain view's own expiry: \
             atm={pair:?} chain={:?}",
            chain[0].pairs
        );
        assert!(
            pair.ce_security_id < 900,
            "the at-the-money pick is far-dated"
        );
    }
}

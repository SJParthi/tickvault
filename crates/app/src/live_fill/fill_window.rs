//! Would-be orders held while their worst-case fill is worked out (plan item
//! LF-6b, owner rules of 2026-11-12).
//!
//! The chain does not send an order the moment the strategy asks. It holds
//! the would-be order here, appends each traded second of its instrument as
//! that second becomes final, and works the fill out once:
//!
//! * the seconds so far could fill it (a running sum of
//!   `⌊volume × participation⌋` reaches the quantity at second `s`) and second
//!   `s + 1` is final too (its price is a candidate for `s`); or
//! * the fill window ends (owner rule: 60 s from the first fill second) and
//!   the second after it is final.
//!
//! `worst_fill` then runs ONCE over the stored seconds (owner rule: the worst
//! case is priced per second). Only then is anything sent to the paper OMS:
//! the order, its fill and a cancel of any remainder (owner rule: a partial
//! fill's remainder is cancelled after the window). A candle whose final
//! price arrives after the decision cancels the held order before any paper
//! order exists (owner rule: skip and log why).
//!
//! The fill respects the order's LIMIT: a second whose worst price (one tick
//! against us included) is beyond the limit contributes no fill, and any share
//! the model still prices beyond it is left unfilled (`beyond_limit_qty`), so
//! the paper fill never breaches a price no exchange would give. A stretch the
//! chain stops trusting while the order is held (a drop, a feed gap, a clock
//! step overlapping its candle or its fill seconds, see [`PendingBook::on_taint`])
//! refuses the order before any paper order exists.
//!
//! # Complexity
//!
//! At most [`MAX_PENDING_ORDERS`] orders, each with its own pre-sized second
//! buffer (`max_fill_seconds + 1` entries). [`PendingBook::on_second`] and
//! [`PendingBook::on_late_tick`] cost one array read while the instrument has
//! no held order, else one scan of the 64 slots; [`PendingBook::collect_ready`]
//! scans the 64 slots once per pass. `worst_fill` is O(seconds used) once per
//! order. No allocation after construction.

use tickvault_trading::brutex_host::InstrumentKey;
use tickvault_trading::brutex_host::engine::Side;
use tickvault_trading::brutex_host::worst_fill::{
    BPS_DENOMINATOR, BookAtDecision, FillParams, FillRefusal, FillSchedule, FillSlice, FillSummary,
    SecondBar, worst_fill,
};

use super::taint::TaintReason;
use super::timing::BookClock;

/// Most would-be orders held at once (the send queue's own ceiling).
pub const MAX_PENDING_ORDERS: usize = 64;

/// Everything about a held order except its seconds.
#[derive(Debug, Clone)]
pub struct HeldOrder<M> {
    pub key_index: u32,
    pub key: InstrumentKey,
    /// The candle book frame of the decision candle.
    pub frame_index: u8,
    pub candle_open_secs: i64,
    pub candle_end_secs: i64,
    pub side: Side,
    /// Shares.
    pub qty: u64,
    pub tick_paise: i64,
    pub participation_bps: u32,
    /// The decision second (the candle's last second) with its high/low.
    pub decision_bar: SecondBar,
    /// Best price and quantity on our side of the book at the decision.
    pub book: Option<(i64, u64)>,
    /// The order's LIMIT price, paise: no paper share fills beyond it.
    pub limit_paise: i64,
    /// The caller's payload (event context).
    pub meta: M,
}

/// How a held order ended.
#[derive(Debug)]
pub enum Finished<M> {
    /// A late price arrived for the decision candle: no order is sent.
    LatePrice { order: HeldOrder<M> },
    /// A stretch the chain cannot trust (a drop, a feed gap, a clock step)
    /// touched the decision candle or the fill seconds after the decision:
    /// no order is sent.
    Tainted {
        order: HeldOrder<M>,
        reason: TaintReason,
    },
    /// The fill was worked out (`summary.filled_qty` may be 0).
    Filled {
        order: HeldOrder<M>,
        summary: FillSummary,
        late_fill_ticks: u32,
        /// Shares the worst case priced beyond the LIMIT, left unfilled.
        beyond_limit_qty: u64,
    },
    /// The model refused the inputs (for example no traded second at all).
    Refused {
        order: HeldOrder<M>,
        refusal: FillRefusal,
    },
}

#[derive(Debug)]
struct Slot<M> {
    order: Option<HeldOrder<M>>,
    seconds: Vec<SecondBar>,
    cap_sum: u64,
    trigger_sec: Option<i64>,
    late_price: bool,
    late_fill_ticks: u32,
    /// Late 1 s prices for fill seconds not yet appended (a second closed by
    /// the next second's tick and still waiting for its release): `(sec,
    /// low, high)` paise, folded into the second when it is appended.
    late_widen: [(i64, i64, i64); LATE_WIDEN_SLOTS],
    late_widen_len: u8,
    tainted: Option<TaintReason>,
}

/// Fill seconds per held order that can carry a late price while they wait
/// for their release (the longest release wait is a few seconds). A late
/// price past these is still counted on the order.
const LATE_WIDEN_SLOTS: usize = 4;

/// The held orders (see the module docs).
#[derive(Debug)]
pub struct PendingBook<M> {
    slots: Vec<Slot<M>>,
    /// Held orders per instrument index.
    per_key: Vec<u8>,
    held: usize,
    max_fill_seconds: u32,
    schedule: FillSchedule,
}

impl<M> PendingBook<M> {
    /// Cold; the only allocation.
    #[must_use]
    pub fn new(max_fill_seconds: u32, key_count: usize) -> Self {
        let per_order = max_fill_seconds as usize + 1;
        let slots = (0..MAX_PENDING_ORDERS)
            .map(|_| Slot {
                order: None,
                seconds: Vec::with_capacity(per_order),
                cap_sum: 0,
                trigger_sec: None,
                late_price: false,
                late_fill_ticks: 0,
                late_widen: [(0, 0, 0); LATE_WIDEN_SLOTS],
                late_widen_len: 0,
                tainted: None,
            })
            .collect();
        Self {
            slots,
            per_key: vec![0; key_count],
            held: 0,
            max_fill_seconds: max_fill_seconds.max(1),
            schedule: FillSchedule::with_capacity(FillSchedule::required_capacity(
                max_fill_seconds.max(1),
                0,
            )),
        }
    }

    /// Orders held now.
    #[must_use]
    pub fn held(&self) -> usize {
        self.held
    }

    /// Whether instrument `ki` has a held order. O(1).
    #[must_use]
    pub fn has_held(&self, ki: u32) -> bool {
        self.per_key.get(ki as usize).is_some_and(|c| *c > 0)
    }

    /// Holds an order. Gives it back when all [`MAX_PENDING_ORDERS`] slots are
    /// taken. O(64).
    ///
    /// # Errors
    /// The order, when full or when its key index is out of range.
    pub fn insert(&mut self, order: HeldOrder<M>) -> Result<(), HeldOrder<M>> {
        let ki = order.key_index as usize;
        if ki >= self.per_key.len() {
            return Err(order);
        }
        let Some(slot) = self.slots.iter_mut().find(|s| s.order.is_none()) else {
            return Err(order);
        };
        slot.seconds.clear();
        slot.cap_sum = 0;
        slot.trigger_sec = None;
        slot.late_price = false;
        slot.late_fill_ticks = 0;
        slot.late_widen_len = 0;
        slot.tainted = None;
        slot.order = Some(order);
        self.per_key[ki] = self.per_key[ki].saturating_add(1);
        self.held += 1;
        Ok(())
    }

    /// A final traded second of instrument `ki`. Appended to every held order
    /// of that instrument whose window it falls in. See the module docs for
    /// the cost.
    pub fn on_second(&mut self, ki: u32, bar: SecondBar) {
        if !self.has_held(ki) {
            return;
        }
        let window = i64::from(self.max_fill_seconds);
        for slot in &mut self.slots {
            let Some(o) = slot.order.as_ref() else {
                continue;
            };
            if o.key_index != ki {
                continue;
            }
            let first = o.decision_bar.sec.saturating_add(1);
            let horizon_end = first.saturating_add(window);
            if bar.sec < first || bar.sec > horizon_end {
                continue;
            }
            if slot.seconds.last().is_some_and(|l| l.sec >= bar.sec) {
                continue;
            }
            if slot.seconds.len() >= slot.seconds.capacity() {
                continue;
            }
            let mut bar = bar;
            take_late_widen(&mut slot.late_widen, &mut slot.late_widen_len, &mut bar);
            slot.seconds.push(bar);
            if bar.sec < horizon_end && slot.trigger_sec.is_none() {
                let cap = u128::from(bar.volume) * u128::from(o.participation_bps)
                    / u128::from(BPS_DENOMINATOR);
                slot.cap_sum = slot
                    .cap_sum
                    .saturating_add(u64::try_from(cap).unwrap_or(u64::MAX));
                if slot.cap_sum >= o.qty {
                    slot.trigger_sec = Some(bar.sec);
                }
            }
        }
    }

    /// A tick of instrument `ki` the candle book counted late on book frame
    /// `frame_index`, at exchange second `tick_secs`, price `price_paise`. A
    /// late price inside a held order's decision candle marks it to be
    /// skipped. One on the 1 s frame (frame 0) inside the order's fill window
    /// is counted on the order whether or not that second was appended yet
    /// (a second closed by the next second's tick waits for its release; LF-6b
    /// review round 3, finding 2), and widens that second's high/low, now if
    /// it is appended, else when it is. See the module docs for the cost.
    pub fn on_late_tick(
        &mut self,
        ki: u32,
        frame_index: u8,
        tick_secs: i64,
        price_paise: Option<i64>,
    ) {
        if !self.has_held(ki) {
            return;
        }
        let window = i64::from(self.max_fill_seconds);
        for slot in &mut self.slots {
            let Some(o) = slot.order.as_ref() else {
                continue;
            };
            if o.key_index != ki {
                continue;
            }
            if o.frame_index == frame_index
                && tick_secs >= o.candle_open_secs
                && tick_secs < o.candle_end_secs
            {
                slot.late_price = true;
            }
            let first = o.decision_bar.sec.saturating_add(1);
            let horizon_end = first.saturating_add(window);
            if frame_index == 0 && tick_secs >= first && tick_secs <= horizon_end {
                slot.late_fill_ticks = slot.late_fill_ticks.saturating_add(1);
                if let Some(p) = price_paise.filter(|p| *p > 0) {
                    widen_late(slot, tick_secs, p);
                }
            }
        }
    }

    /// A stretch `lo..=hi` (IST seconds) the chain no longer trusts: every held
    /// order whose decision candle or fill window (`[candle open, horizon
    /// end]`) overlaps it is marked, and finishes as
    /// [`Finished::Tainted`] instead of being filled. The first reason is
    /// kept. O(64), no allocation.
    pub fn on_taint(&mut self, lo: i64, hi: i64, reason: TaintReason) {
        if self.held == 0 {
            return;
        }
        let window = i64::from(self.max_fill_seconds);
        for slot in &mut self.slots {
            let Some(o) = slot.order.as_ref() else {
                continue;
            };
            let horizon_end = o.decision_bar.sec.saturating_add(1).saturating_add(window);
            if lo <= horizon_end && hi >= o.candle_open_secs && slot.tainted.is_none() {
                slot.tainted = Some(reason);
            }
        }
    }

    /// Indices of the orders that can be finished at `now_ist_ns`, written to
    /// `out` (cleared first; capacity [`MAX_PENDING_ORDERS`]). O(64).
    pub fn collect_ready(&self, now_ist_ns: i64, clock: &BookClock, out: &mut Vec<usize>) {
        out.clear();
        let window = i64::from(self.max_fill_seconds);
        for (i, slot) in self.slots.iter().enumerate() {
            let Some(o) = slot.order.as_ref() else {
                continue;
            };
            let ready = if slot.late_price || slot.tainted.is_some() {
                true
            } else if let Some(s) = slot.trigger_sec {
                // Second s+1's price is a candidate for s.
                clock.second_final(s.saturating_add(1), now_ist_ns)
            } else {
                let horizon_end = o.decision_bar.sec.saturating_add(1).saturating_add(window);
                clock.second_final(horizon_end, now_ist_ns)
            };
            if ready && out.len() < out.capacity() {
                out.push(i);
            }
        }
    }

    /// Finishes the order in slot `i` (from [`Self::collect_ready`]) and frees
    /// the slot. `None` when the slot is empty. O(seconds used), no
    /// allocation.
    pub fn finish(&mut self, i: usize) -> Option<Finished<M>> {
        let slot = self.slots.get_mut(i)?;
        let order = slot.order.take()?;
        let ki = order.key_index as usize;
        if let Some(c) = self.per_key.get_mut(ki) {
            *c = c.saturating_sub(1);
        }
        self.held = self.held.saturating_sub(1);
        if slot.late_price {
            return Some(Finished::LatePrice { order });
        }
        if let Some(reason) = slot.tainted {
            return Some(Finished::Tainted { order, reason });
        }
        let result = price_held_window(
            &HeldPricing {
                side: order.side,
                qty: order.qty,
                tick_paise: order.tick_paise,
                participation_bps: order.participation_bps,
                max_fill_seconds: self.max_fill_seconds,
                limit_paise: order.limit_paise,
                decision_bar: order.decision_bar,
                book: order.book,
            },
            &mut slot.seconds,
            &mut self.schedule,
        );
        let late_fill_ticks = slot.late_fill_ticks;
        slot.seconds.clear();
        Some(match result {
            Ok((summary, beyond_limit_qty)) => Finished::Filled {
                order,
                summary,
                late_fill_ticks,
                beyond_limit_qty,
            },
            Err(refusal) => Finished::Refused { order, refusal },
        })
    }

    /// Drops every held order (a new trading day), returning how many. O(64).
    pub fn clear(&mut self) -> usize {
        let n = self.held;
        for slot in &mut self.slots {
            slot.order = None;
            slot.seconds.clear();
        }
        self.per_key.iter_mut().for_each(|c| *c = 0);
        self.held = 0;
        n
    }
}

/// The inputs that price one held order (everything but its seconds). The
/// chain builds it from its held order; the daily calibration report (plan
/// item LF-7) builds it from the stored Decision row, so both price the same
/// order the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeldPricing {
    pub side: Side,
    /// Shares.
    pub qty: u64,
    pub tick_paise: i64,
    pub participation_bps: u32,
    /// The fill window, seconds (at least 1).
    pub max_fill_seconds: u32,
    /// The order's LIMIT, paise; 0 or below means no limit.
    pub limit_paise: i64,
    /// The decision second (the candle's last second) with its high/low.
    pub decision_bar: SecondBar,
    /// Best price and quantity on our side of the book at the decision.
    pub book: Option<(i64, u64)>,
}

/// Prices one held order over its fill seconds, exactly as
/// [`PendingBook::finish`] does (plan item LF-7 moved the body here so the
/// daily report re-prices with the same code):
///
/// 1. a second whose worst price (one tick against us, and the next
///    second's price as a candidate, included) is beyond the LIMIT has its
///    volume set to 0 in `seconds` (it cannot fill a LIMIT order);
/// 2. `worst_fill` over the best quote only (no depth levels);
/// 3. [`within_limit`]: any share still priced beyond the LIMIT is left
///    unfilled.
///
/// Returns the summary and the shares left beyond the LIMIT. `out` holds the
/// slices before the LIMIT cut. O(seconds used), no allocation.
///
/// # Errors
/// The model's refusal (for example no traded second at all).
pub fn price_held_window(
    p: &HeldPricing,
    seconds: &mut [SecondBar],
    out: &mut FillSchedule,
) -> Result<(FillSummary, u64), FillRefusal> {
    // A second whose worst price (one tick against us included) is beyond
    // the LIMIT cannot fill a LIMIT order: its volume is not ours.
    if p.limit_paise > 0 {
        let mut base = extreme(p.side, &p.decision_bar);
        if let Some((best, _)) = p.book {
            base = worse(p.side, base, best);
        }
        for j in 0..seconds.len() {
            let s = seconds[j];
            let mut price = worse(p.side, base, extreme(p.side, &s));
            if let Some(n) = seconds.get(j + 1)
                && n.sec == s.sec.saturating_add(1)
            {
                price = worse(p.side, price, extreme(p.side, n));
            }
            if beyond_limit(p.side, against(p.side, price, p.tick_paise), p.limit_paise) {
                seconds[j].volume = 0;
            }
        }
    }
    let max_seconds = p.max_fill_seconds.max(1);
    let params = FillParams {
        side: p.side,
        qty: p.qty,
        tick_paise: p.tick_paise,
        participation_bps: p.participation_bps,
        decision_sec: p.decision_bar.sec,
        first_fill_sec: p.decision_bar.sec.saturating_add(1),
        max_seconds,
    };
    let book = p.book.map(|(best_paise, best_qty)| BookAtDecision {
        best_paise,
        best_qty,
        levels: &[],
    });
    let summary = worst_fill(&params, &p.decision_bar, seconds, book.as_ref(), out)?;
    Ok(limit_cut(
        p.side,
        p.qty,
        p.limit_paise,
        summary,
        out.slices(),
    ))
}

/// Folds a late price into fill second `sec` of `slot`: in place when the
/// second is appended (a binary search over at most `max_fill_seconds + 1`
/// entries), else kept until it is. Past [`LATE_WIDEN_SLOTS`] waiting seconds
/// the price is not kept (the order still counts it). O(log seconds).
fn widen_late<M>(slot: &mut Slot<M>, sec: i64, price: i64) {
    if let Ok(i) = slot.seconds.binary_search_by_key(&sec, |b| b.sec) {
        let b = &mut slot.seconds[i];
        b.high_paise = b.high_paise.max(price);
        b.low_paise = b.low_paise.min(price);
        return;
    }
    if slot.seconds.last().is_some_and(|l| l.sec > sec) {
        // A second with no trade before the newest appended one: it holds
        // no volume to fill against, so there is nothing to price.
        return;
    }
    let n = usize::from(slot.late_widen_len);
    for w in &mut slot.late_widen[..n] {
        if w.0 == sec {
            w.1 = w.1.min(price);
            w.2 = w.2.max(price);
            return;
        }
    }
    if n < LATE_WIDEN_SLOTS {
        slot.late_widen[n] = (sec, price, price);
        slot.late_widen_len = slot.late_widen_len.saturating_add(1);
    }
}

/// Applies (and drops) a late price kept for `bar`'s second. O(slots).
fn take_late_widen(
    widen: &mut [(i64, i64, i64); LATE_WIDEN_SLOTS],
    len: &mut u8,
    bar: &mut SecondBar,
) {
    let n = usize::from(*len).min(LATE_WIDEN_SLOTS);
    for i in 0..n {
        let (sec, lo, hi) = widen[i];
        if sec == bar.sec {
            bar.low_paise = bar.low_paise.min(lo);
            bar.high_paise = bar.high_paise.max(hi);
            widen[i] = widen[n - 1];
            *len = len.saturating_sub(1);
            return;
        }
    }
}

/// The side's worst price of a second: its high for a buy, low for a sell. O(1).
const fn extreme(side: Side, bar: &SecondBar) -> i64 {
    match side {
        Side::Buy => bar.high_paise,
        Side::Sell => bar.low_paise,
    }
}

/// The worse of two prices for `side`. O(1).
const fn worse(side: Side, a: i64, b: i64) -> i64 {
    match side {
        Side::Buy => {
            if a > b {
                a
            } else {
                b
            }
        }
        Side::Sell => {
            if a < b {
                a
            } else {
                b
            }
        }
    }
}

/// `price` moved `tick` against `side`. O(1).
const fn against(side: Side, price: i64, tick: i64) -> i64 {
    match side {
        Side::Buy => price.saturating_add(tick),
        Side::Sell => price.saturating_sub(tick),
    }
}

/// Whether a fill at `price` breaches the LIMIT `limit` for `side`. O(1).
#[must_use]
pub const fn beyond_limit(side: Side, price: i64, limit: i64) -> bool {
    match side {
        Side::Buy => price > limit,
        Side::Sell => price < limit,
    }
}

/// Keeps only the schedule's slices at or inside the LIMIT: the summary of
/// those, and the shares the model priced beyond it (left unfilled). The
/// average is rounded against us, as `worst_fill` rounds it. O(slices), no
/// allocation.
#[must_use]
pub fn within_limit<M>(
    order: &HeldOrder<M>,
    summary: FillSummary,
    slices: &[FillSlice],
) -> (FillSummary, u64) {
    limit_cut(order.side, order.qty, order.limit_paise, summary, slices)
}

/// [`within_limit`] on the bare inputs. O(slices), no allocation.
fn limit_cut(
    side: Side,
    qty: u64,
    limit_paise: i64,
    summary: FillSummary,
    slices: &[FillSlice],
) -> (FillSummary, u64) {
    if limit_paise <= 0 {
        return (summary, 0);
    }
    let mut filled: u64 = 0;
    let mut cost: i128 = 0;
    let mut beyond: u64 = 0;
    for s in slices {
        if beyond_limit(side, s.price_paise, limit_paise) {
            beyond = beyond.saturating_add(s.qty);
        } else {
            filled = filled.saturating_add(s.qty);
            cost += i128::from(s.price_paise) * i128::from(s.qty);
        }
    }
    if beyond == 0 {
        return (summary, 0);
    }
    let vwap_paise = if filled == 0 {
        None
    } else {
        let f = i128::from(filled);
        let avg = match side {
            Side::Buy => (cost + f - 1) / f,
            Side::Sell => cost / f,
        };
        i64::try_from(avg).ok()
    };
    (
        FillSummary {
            filled_qty: filled,
            vwap_paise,
            unfilled_qty: qty.saturating_sub(filled),
            seconds_used: summary.seconds_used,
            depth_exhausted: summary.depth_exhausted,
        },
        beyond,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 1_779_321_600;
    const NS: i64 = 1_000_000_000;
    const DEC: i64 = DAY + 40_000;

    fn order(ki: u32, qty: u64) -> HeldOrder<u32> {
        HeldOrder {
            key_index: ki,
            key: InstrumentKey {
                security_id: u64::from(ki) + 1,
                exchange_segment: 2,
            },
            frame_index: 1,
            candle_open_secs: DEC - 59,
            candle_end_secs: DEC + 1,
            side: Side::Buy,
            qty,
            tick_paise: 5,
            participation_bps: 2_500,
            decision_bar: SecondBar {
                sec: DEC,
                high_paise: 10_010,
                low_paise: 9_990,
                volume: 100,
            },
            book: None,
            limit_paise: 1_000_000,
            meta: 7,
        }
    }

    fn sec(s: i64, vol: u64) -> SecondBar {
        SecondBar {
            sec: s,
            high_paise: 10_020,
            low_paise: 10_000,
            volume: vol,
        }
    }

    fn clock() -> BookClock {
        BookClock::new(&[1, 60], 1_000, 2_500)
    }

    #[test]
    fn test_pending_book_insert_full_and_has_held() {
        let mut p = PendingBook::<u32>::new(60, 4);
        assert_eq!(p.held(), 0);
        for _ in 0..MAX_PENDING_ORDERS {
            assert!(p.insert(order(0, 10)).is_ok());
        }
        assert!(p.has_held(0));
        assert!(!p.has_held(1));
        assert!(p.insert(order(1, 10)).is_err(), "full");
        assert!(
            PendingBook::<u32>::new(60, 1).insert(order(5, 1)).is_err(),
            "key out of range"
        );
        assert_eq!(p.clear(), MAX_PENDING_ORDERS);
        assert!(!p.has_held(0));
    }

    #[test]
    fn test_on_second_and_collect_ready_trigger_once_the_sum_covers_the_quantity() {
        let mut p = PendingBook::<u32>::new(60, 2);
        p.insert(order(0, 50)).expect("room");
        let c = clock();
        let mut ready = Vec::with_capacity(MAX_PENDING_ORDERS);
        // 25% of 100 = 25 per second: covered after the second second.
        p.on_second(0, sec(DEC + 1, 100));
        p.on_second(1, sec(DEC + 1, 1_000_000)); // another instrument
        p.collect_ready((DEC + 100) * NS, &c, &mut ready);
        // Not covered yet, but the window... 100 s later the window is over.
        assert_eq!(ready, vec![0]);
        let mut p = PendingBook::<u32>::new(60, 2);
        p.insert(order(0, 50)).expect("room");
        p.on_second(0, sec(DEC + 1, 100));
        p.on_second(0, sec(DEC + 2, 100));
        // Triggered at DEC+2; ready once DEC+3 is final (DEC+4 + 1 s).
        p.collect_ready((DEC + 5) * NS - 1, &c, &mut ready);
        assert!(ready.is_empty());
        p.collect_ready((DEC + 5) * NS, &c, &mut ready);
        assert_eq!(ready, vec![0]);
        p.on_second(0, sec(DEC + 3, 100));
        match p.finish(0) {
            Some(Finished::Filled { summary, .. }) => {
                assert_eq!(summary.filled_qty, 50);
                assert_eq!(summary.unfilled_qty, 0);
                // Worst of 10,020 (this and next second) plus one tick.
                assert_eq!(summary.vwap_paise, Some(10_025));
            }
            other => panic!("expected a fill, got {other:?}"),
        }
        assert_eq!(p.held(), 0);
        assert!(p.finish(0).is_none());
    }

    #[test]
    fn test_window_end_gives_a_partial_fill_and_the_window_is_60_seconds() {
        let mut p = PendingBook::<u32>::new(60, 1);
        p.insert(order(0, 1_000)).expect("room");
        for s in 1..=70 {
            p.on_second(0, sec(DEC + s, 4));
        }
        let c = clock();
        let mut ready = Vec::with_capacity(MAX_PENDING_ORDERS);
        // horizon_end = DEC + 61; final at DEC + 63.
        p.collect_ready((DEC + 63) * NS - 1, &c, &mut ready);
        assert!(ready.is_empty());
        p.collect_ready((DEC + 63) * NS, &c, &mut ready);
        assert_eq!(ready, vec![0]);
        match p.finish(0) {
            Some(Finished::Filled { summary, .. }) => {
                assert_eq!(summary.filled_qty, 60, "1 share a second for 60 seconds");
                assert_eq!(summary.unfilled_qty, 940);
                assert_eq!(summary.seconds_used, 60);
            }
            other => panic!("expected a partial fill, got {other:?}"),
        }
    }

    #[test]
    fn test_on_late_tick_in_the_decision_candle_skips_the_order() {
        let mut p = PendingBook::<u32>::new(60, 1);
        p.insert(order(0, 10)).expect("room");
        // Late on another frame, or outside the candle: nothing.
        p.on_late_tick(0, 2, DEC, None);
        p.on_late_tick(0, 1, DEC + 1, None);
        let c = clock();
        let mut ready = Vec::with_capacity(MAX_PENDING_ORDERS);
        p.collect_ready(DEC * NS, &c, &mut ready);
        assert!(ready.is_empty());
        p.on_late_tick(0, 1, DEC - 3, None);
        p.collect_ready(DEC * NS, &c, &mut ready);
        assert_eq!(ready, vec![0]);
        assert!(matches!(p.finish(0), Some(Finished::LatePrice { .. })));
    }

    /// LF-6b review round 3, finding 2: a late price for a fill second that
    /// is closed (by the next second's tick) but not yet released, or for the
    /// first fill second before anything was appended, is counted on the
    /// order and widens that second when it is appended, so the worst-case
    /// price reflects it.
    #[test]
    fn test_regression_late_price_on_a_second_still_waiting_for_release_is_counted_and_priced() {
        let c = clock();
        let mut ready = Vec::with_capacity(MAX_PENDING_ORDERS);
        // The first fill second (nothing appended yet) takes a late print at
        // 105.00 before its release; its bar tops out at 100.20.
        let mut p = PendingBook::<u32>::new(60, 1);
        p.insert(order(0, 10)).expect("room");
        p.on_late_tick(0, 0, DEC + 1, Some(10_500));
        p.on_second(0, sec(DEC + 1, 1_000));
        p.on_second(0, sec(DEC + 2, 1_000));
        p.collect_ready((DEC + 4) * NS, &c, &mut ready);
        assert_eq!(ready, vec![0]);
        match p.finish(0) {
            Some(Finished::Filled {
                summary,
                late_fill_ticks,
                ..
            }) => {
                assert_eq!(late_fill_ticks, 1, "the late print is counted");
                assert_eq!(summary.filled_qty, 10);
                assert!(
                    summary.vwap_paise.is_some_and(|v| v >= 10_500),
                    "the fill is priced no better than the late print: {summary:?}"
                );
            }
            other => panic!("expected a fill, got {other:?}"),
        }
        // A later second (DEC + 2) waits for release while DEC + 1 is
        // appended: the late print for DEC + 2 is counted and folded in.
        let mut p = PendingBook::<u32>::new(60, 1);
        p.insert(order(0, 400)).expect("room");
        p.on_second(0, sec(DEC + 1, 1_000));
        p.on_late_tick(0, 0, DEC + 2, Some(10_600));
        p.on_second(0, sec(DEC + 2, 1_000));
        p.on_second(0, sec(DEC + 3, 1_000));
        ready.clear();
        p.collect_ready((DEC + 5) * NS, &c, &mut ready);
        assert_eq!(ready, vec![0]);
        match p.finish(0) {
            Some(Finished::Filled {
                summary,
                late_fill_ticks,
                ..
            }) => {
                assert_eq!(late_fill_ticks, 1);
                assert!(
                    summary.vwap_paise.is_some_and(|v| v > 10_025),
                    "the waiting second carries the late print: {summary:?}"
                );
            }
            other => panic!("expected a fill, got {other:?}"),
        }
    }

    #[test]
    fn test_late_fill_ticks_are_counted_and_no_trade_is_refused() {
        let mut p = PendingBook::<u32>::new(5, 1);
        p.insert(order(0, 10)).expect("room");
        p.on_second(0, sec(DEC + 2, 0));
        p.on_late_tick(0, 0, DEC + 1, None);
        p.on_late_tick(0, 0, DEC + 9, None);
        p.on_second(0, sec(DEC + 2, 1_000)); // a repeat second is ignored
        let c = clock();
        let mut ready = Vec::with_capacity(MAX_PENDING_ORDERS);
        p.collect_ready((DEC + 9) * NS, &c, &mut ready);
        assert_eq!(ready, vec![0]);
        match p.finish(0) {
            Some(Finished::Filled {
                summary,
                late_fill_ticks,
                ..
            }) => {
                assert_eq!(summary.filled_qty, 0);
                assert_eq!(late_fill_ticks, 1);
            }
            other => panic!("expected a zero fill, got {other:?}"),
        }
        let mut p = PendingBook::<u32>::new(5, 1);
        p.insert(order(0, 10)).expect("room");
        assert!(matches!(
            p.finish(0),
            Some(Finished::Refused {
                refusal: FillRefusal::EmptySeconds,
                ..
            })
        ));
    }

    /// Finding 4 (LF-6b review): a stretch tainted while an order is held,
    /// overlapping its candle or its fill seconds, refuses it; one outside
    /// both leaves it alone.
    #[test]
    fn test_on_taint_refuses_a_held_order_whose_candle_or_fill_window_it_touches() {
        let c = clock();
        let mut ready = Vec::with_capacity(MAX_PENDING_ORDERS);
        // Outside: before the candle and after the horizon (DEC + 61).
        let mut p = PendingBook::<u32>::new(60, 1);
        p.insert(order(0, 10)).expect("room");
        p.on_taint(DEC - 200, DEC - 60, TaintReason::FeedGap);
        p.on_taint(DEC + 62, DEC + 90, TaintReason::FeedGap);
        p.collect_ready(DEC * NS, &c, &mut ready);
        assert!(ready.is_empty());
        // Inside the fill window: ready at once, refused with the reason.
        p.on_taint(DEC + 30, DEC + 31, TaintReason::TickDropped);
        p.on_taint(DEC + 30, DEC + 31, TaintReason::ClockStep);
        p.collect_ready(DEC * NS, &c, &mut ready);
        assert_eq!(ready, vec![0]);
        assert!(matches!(
            p.finish(0),
            Some(Finished::Tainted {
                reason: TaintReason::TickDropped,
                ..
            })
        ));
        // Inside the decision candle.
        let mut p = PendingBook::<u32>::new(60, 1);
        p.insert(order(0, 10)).expect("room");
        p.on_taint(DEC - 59, DEC - 59, TaintReason::ChainStart);
        p.collect_ready(DEC * NS, &c, &mut ready);
        assert_eq!(ready, vec![0]);
        // An empty book ignores taints.
        let mut empty = PendingBook::<u32>::new(60, 1);
        empty.on_taint(0, i64::MAX, TaintReason::FeedGap);
        assert_eq!(empty.held(), 0);
    }

    /// Finding 6 (LF-6b review): the paper fill never breaches the LIMIT. A
    /// 15% move inside the window: seconds beyond the limit fill nothing.
    #[test]
    fn test_fill_never_breaches_the_limit_and_beyond_limit_and_within_limit() {
        // Limit 10,100: decision high 10,010 + tick 5 is inside.
        let mut o = order(0, 100);
        o.limit_paise = 10_100;
        let mut p = PendingBook::<u32>::new(60, 1);
        p.insert(o).expect("room");
        // Second 1 at 10,020 (inside), then the price jumps 15% to 11,500.
        p.on_second(0, sec(DEC + 1, 40));
        for s in 2..=10 {
            p.on_second(
                0,
                SecondBar {
                    sec: DEC + s,
                    high_paise: 11_500,
                    low_paise: 11_400,
                    volume: 1_000,
                },
            );
        }
        let c = clock();
        let mut ready = Vec::with_capacity(MAX_PENDING_ORDERS);
        p.collect_ready((DEC + 63) * NS, &c, &mut ready);
        assert_eq!(ready, vec![0]);
        match p.finish(0) {
            Some(Finished::Filled {
                summary,
                beyond_limit_qty,
                ..
            }) => {
                // Second 1's worst price includes second 2's high (11,500):
                // beyond the limit, so not even it fills.
                assert_eq!(summary.filled_qty, 0);
                assert_eq!(summary.vwap_paise, None);
                assert_eq!(summary.unfilled_qty, 100);
                assert_eq!(beyond_limit_qty, 0, "zeroed seconds were never priced");
            }
            other => panic!("expected an empty fill, got {other:?}"),
        }
        assert!(beyond_limit(Side::Buy, 101, 100));
        assert!(!beyond_limit(Side::Buy, 100, 100));
        assert!(beyond_limit(Side::Sell, 99, 100));
        assert!(!beyond_limit(Side::Sell, 100, 100));
        // within_limit drops slices beyond the limit and re-averages.
        let mut o = order(0, 30);
        o.limit_paise = 10_030;
        let summary = FillSummary {
            filled_qty: 30,
            vwap_paise: Some(10_030),
            unfilled_qty: 0,
            seconds_used: 2,
            depth_exhausted: false,
        };
        let slices = [
            FillSlice {
                sec: DEC + 1,
                qty: 10,
                price_paise: 10_025,
            },
            FillSlice {
                sec: DEC + 1,
                qty: 10,
                price_paise: 10_030,
            },
            FillSlice {
                sec: DEC + 2,
                qty: 10,
                price_paise: 10_035,
            },
        ];
        let (s, beyond) = within_limit(&o, summary, &slices);
        assert_eq!(beyond, 10);
        assert_eq!(s.filled_qty, 20);
        assert_eq!(s.unfilled_qty, 10);
        assert_eq!(s.vwap_paise, Some(10_028), "rounded up for a buy");
        o.limit_paise = 0;
        assert_eq!(
            within_limit(&o, summary, &slices).1,
            0,
            "no limit, unchanged"
        );
    }

    proptest::proptest! {
        /// The fill is worked out exactly once per order, never before the
        /// trigger second's successor is final nor after the window plus its
        /// successor, and never fills more than the order or the volume cap.
        #[test]
        fn proptest_fill_window_once_and_bounded(
            qty in 1u64..400,
            vols in proptest::collection::vec(0u64..200, 1..80),
        ) {
            let mut p = PendingBook::<u32>::new(60, 1);
            p.insert(order(0, qty)).expect("room");
            let c = clock();
            let mut ready = Vec::with_capacity(MAX_PENDING_ORDERS);
            let mut finished = 0;
            let mut cap_total = 0u64;
            for (i, v) in vols.iter().enumerate() {
                let s = DEC + 1 + i as i64;
                p.on_second(0, sec(s, *v));
                if s < DEC + 61 {
                    cap_total += v * 2_500 / 10_000;
                }
                // The pass after second s is final.
                p.collect_ready((s + 2) * NS, &c, &mut ready);
                for &slot in &ready.clone() {
                    match p.finish(slot) {
                        Some(Finished::Filled { summary, .. }) => {
                            proptest::prop_assert!(summary.filled_qty <= qty);
                            proptest::prop_assert!(summary.filled_qty <= cap_total);
                            proptest::prop_assert_eq!(summary.filled_qty + summary.unfilled_qty, qty);
                            finished += 1;
                        }
                        Some(Finished::Refused { .. }) => finished += 1,
                        other => proptest::prop_assert!(false, "unexpected {:?}", other),
                    }
                }
            }
            p.collect_ready((DEC + 200) * NS, &c, &mut ready);
            for &slot in &ready.clone() {
                if p.finish(slot).is_some() {
                    finished += 1;
                }
            }
            proptest::prop_assert_eq!(finished, 1);
            proptest::prop_assert_eq!(p.held(), 0);
        }
    }

    /// The pre-LF-7 body of `PendingBook::finish` after its early returns,
    /// kept verbatim as the oracle for the moved [`price_held_window`].
    fn oracle_finish_pricing(
        order: &HeldOrder<u32>,
        max_fill_seconds: u32,
        seconds: &mut [SecondBar],
        schedule: &mut FillSchedule,
    ) -> Result<(FillSummary, u64), FillRefusal> {
        if order.limit_paise > 0 {
            let mut base = extreme(order.side, &order.decision_bar);
            if let Some((best, _)) = order.book {
                base = worse(order.side, base, best);
            }
            for j in 0..seconds.len() {
                let s = seconds[j];
                let mut p = worse(order.side, base, extreme(order.side, &s));
                if let Some(n) = seconds.get(j + 1)
                    && n.sec == s.sec.saturating_add(1)
                {
                    p = worse(order.side, p, extreme(order.side, n));
                }
                if beyond_limit(
                    order.side,
                    against(order.side, p, order.tick_paise),
                    order.limit_paise,
                ) {
                    seconds[j].volume = 0;
                }
            }
        }
        let params = FillParams {
            side: order.side,
            qty: order.qty,
            tick_paise: order.tick_paise,
            participation_bps: order.participation_bps,
            decision_sec: order.decision_bar.sec,
            first_fill_sec: order.decision_bar.sec.saturating_add(1),
            max_seconds: max_fill_seconds.max(1),
        };
        let book = order.book.map(|(best_paise, best_qty)| BookAtDecision {
            best_paise,
            best_qty,
            levels: &[],
        });
        let summary = worst_fill(
            &params,
            &order.decision_bar,
            seconds,
            book.as_ref(),
            schedule,
        )?;
        Ok(within_limit(order, summary, schedule.slices()))
    }

    proptest::proptest! {
        /// LF-7: the moved pricing gives byte-for-byte the same result as the
        /// old `finish` body, on random sides, quantities, limits, books,
        /// windows and seconds (gaps included).
        #[test]
        fn test_price_held_window_equals_finish_on_random_orders(
            buy in proptest::bool::ANY,
            qty in 0u64..2_000,
            tick in 1i64..20,
            part in 1u32..=10_000,
            window in 1u32..70,
            limit_off in -400i64..400,
            no_limit in proptest::bool::ANY,
            book in proptest::option::of((9_800i64..10_200, 0u64..500)),
            bars in proptest::collection::vec((1i64..3, 9_700i64..10_300, 0i64..200, 0u64..600), 0..75),
        ) {
            let mut o = order(0, qty);
            o.side = if buy { Side::Buy } else { Side::Sell };
            o.tick_paise = tick;
            o.participation_bps = part;
            o.book = book;
            o.limit_paise = if no_limit { 0 } else { 10_000 + limit_off };
            let mut secs = Vec::with_capacity(bars.len());
            let mut s = DEC;
            for (gap, low, span, vol) in &bars {
                s += gap;
                secs.push(SecondBar { sec: s, high_paise: low + span, low_paise: *low, volume: *vol });
            }
            let mut a = secs.clone();
            let mut b = secs;
            let cap = FillSchedule::required_capacity(window, 0);
            let mut sa = FillSchedule::with_capacity(cap);
            let mut sb = FillSchedule::with_capacity(cap);
            let old = oracle_finish_pricing(&o, window, &mut a, &mut sa);
            let new = price_held_window(
                &HeldPricing {
                    side: o.side,
                    qty: o.qty,
                    tick_paise: o.tick_paise,
                    participation_bps: o.participation_bps,
                    max_fill_seconds: window,
                    limit_paise: o.limit_paise,
                    decision_bar: o.decision_bar,
                    book: o.book,
                },
                &mut b,
                &mut sb,
            );
            proptest::prop_assert_eq!(old, new);
            proptest::prop_assert_eq!(a, b);
            proptest::prop_assert_eq!(sa.slices(), sb.slices());
        }
    }
}

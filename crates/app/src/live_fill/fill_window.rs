//! Would-be orders held while their worst-case fill is worked out (plan item
//! LF-6b, owner rules of 2026-11-12).
//!
//! The chain does not send an order the moment the strategy asks. It holds
//! the would-be order here, appends each traded second of its instrument as
//! that second becomes final, and works the fill out once:
//!
//! * the seconds so far could fill it and second `s + 1` is final too (its
//!   price is a candidate for `s`). Since LF-9 the running sum counts only
//!   this order's SHARE of each settled second: the held orders of one
//!   instrument settle a second in insert order, each taking
//!   `⌊left × participation⌋` of the volume the earlier ones left
//!   ([`allot_second`]), and a second whose worst price is beyond the
//!   order's LIMIT gives it nothing; or
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
//! no held order. Otherwise `on_late_tick` is one scan of the 64 slots (plus
//! a binary search per matching order), and `on_second` is one scan plus one
//! `settle_key` (another scan of the 64 slots and an in-place sort of at most
//! 64 indices). [`PendingBook::collect_ready`] is one scan of the 64 slots,
//! plus O(64) and a sort of at most 64 indices per instrument it settles,
//! plus an O(64) `earlier_held` scan per ready exit: O(64²) slot visits and
//! up to 64 small sorts per pass in the worst case (64 held orders on 64
//! instruments all settling in the same pass), all on the stack. `worst_fill`
//! is O(seconds used) once per order. No allocation after construction.

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
    /// An exit (reduce-only): it finishes only after every earlier held
    /// order of its instrument (LF-9 review, findings 2 and 7), so the
    /// runtime books those first and clamps it against what they left.
    pub reduce_only: bool,
    /// The caller's payload (event context).
    pub meta: M,
}

/// How a held order ended.
#[derive(Debug)]
pub enum Finished<M> {
    /// A late price arrived for the decision candle: no order is sent.
    LatePrice { order: HeldOrder<M> },
    /// A late 1 s price reached a fill second whose share was already
    /// counted and moved it (or the second before it, through its candidate)
    /// past the LIMIT (LF-9 review): the share the order counted no longer
    /// holds, so no order is sent rather than one priced on a share the
    /// daily re-pricing would never give it.
    LateFillPrice { order: HeldOrder<M> },
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
    /// The order's fill seconds. A SETTLED second (index below `settled`)
    /// holds the volume left for this order after every earlier held order
    /// of the same instrument took its share; the last second may still hold
    /// the raw volume until its successor is known (LF-9 findings 1 and 3).
    seconds: Vec<SecondBar>,
    /// Per entry of `seconds`: whether that second, once settled, gave this
    /// order any shares (LF-9 review, third pass). A settled second that gave
    /// none (its share floored to 0, earlier orders left it nothing, or it
    /// is at the horizon) cannot change this order's fill whatever a late
    /// print does to its price. Pre-sized like `seconds`; no allocation after
    /// construction.
    gave: Vec<bool>,
    /// Seconds whose share is decided.
    settled: usize,
    /// Insert order: earlier held orders of an instrument take their share of
    /// a second first.
    seq: u64,
    /// Shares the settled seconds can fill, counting only seconds inside the
    /// LIMIT and only this order's share of each (LF-9 findings 1 and 3).
    alloc_sum: u64,
    trigger_sec: Option<i64>,
    late_price: bool,
    late_fill_ticks: u32,
    /// Late 1 s prices for fill seconds not yet appended (a second closed by
    /// the next second's tick and still waiting for its release): `(sec,
    /// low, high)` paise, folded into the second when it is appended.
    late_widen: [(i64, i64, i64); LATE_WIDEN_SLOTS],
    late_widen_len: u8,
    /// A late price moved a settled second past the LIMIT (LF-9 review).
    late_fill_price: bool,
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
    /// The next insert's `seq`.
    next_seq: u64,
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
                gave: Vec::with_capacity(per_order),
                settled: 0,
                seq: 0,
                alloc_sum: 0,
                trigger_sec: None,
                late_price: false,
                late_fill_ticks: 0,
                late_widen: [(0, 0, 0); LATE_WIDEN_SLOTS],
                late_widen_len: 0,
                late_fill_price: false,
                tainted: None,
            })
            .collect();
        Self {
            slots,
            per_key: vec![0; key_count],
            held: 0,
            next_seq: 0,
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

    /// Shares of instrument `ki` that held ENTRY orders (not reduce-only) on
    /// `side` ask for (LF-9 review): an exit decided while an entry is still
    /// held may close what that entry will open, as the backtest's position
    /// does. If the entry fills short, the runtime clamps the exit against
    /// what it booked and reports the position back. O(64), no allocation.
    #[must_use]
    pub fn held_entry_qty(&self, ki: u32, side: Side) -> u64 {
        if !self.has_held(ki) {
            return 0;
        }
        self.slots
            .iter()
            .filter_map(|s| s.order.as_ref())
            .filter(|o| o.key_index == ki && !o.reduce_only && o.side == side)
            .fold(0u64, |acc, o| acc.saturating_add(o.qty))
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
        slot.gave.clear();
        slot.settled = 0;
        slot.seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        slot.alloc_sum = 0;
        slot.trigger_sec = None;
        slot.late_price = false;
        slot.late_fill_ticks = 0;
        slot.late_widen_len = 0;
        slot.late_fill_price = false;
        slot.tainted = None;
        slot.order = Some(order);
        self.per_key[ki] = self.per_key[ki].saturating_add(1);
        self.held += 1;
        Ok(())
    }

    /// A final traded second of instrument `ki`. First settles the share of
    /// the second each held order of that instrument holds last (its
    /// successor is now known), then appends `bar` to every held order whose
    /// window it falls in. See the module docs for the cost.
    pub fn on_second(&mut self, ki: u32, bar: SecondBar) {
        if !self.has_held(ki) {
            return;
        }
        self.settle_key(ki, Some(bar));
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
            slot.gave.push(false);
        }
    }

    /// Settles the last, still unsettled second of every held order of
    /// instrument `ki`, earlier orders first: each takes its share of what the
    /// earlier ones left ([`allot_second`]), so the orders of one instrument
    /// share the participation cap of a second (LF-9 finding 3), and a second
    /// beyond an order's LIMIT gives it nothing (finding 1). `next` is the
    /// instrument's next traded second (its price is a candidate), or `None`
    /// when the second after is final with no trade. O(64 log 64) on the
    /// stack, no allocation.
    fn settle_key(&mut self, ki: u32, next: Option<SecondBar>) {
        let mut idx = [0u8; MAX_PENDING_ORDERS];
        let mut n = 0usize;
        for (i, slot) in self.slots.iter().enumerate() {
            if slot.order.as_ref().is_some_and(|o| o.key_index == ki)
                && slot.settled < slot.seconds.len()
                && n < MAX_PENDING_ORDERS
            {
                idx[n] = u8::try_from(i).unwrap_or(u8::MAX);
                n += 1;
            }
        }
        if n == 0 {
            return;
        }
        let slots = &self.slots;
        idx[..n].sort_unstable_by_key(|&i| slots[usize::from(i)].seq);
        // Volume left per second, shared by the orders settling it.
        let mut left: [(i64, u64); MAX_PENDING_ORDERS] = [(i64::MIN, 0); MAX_PENDING_ORDERS];
        let mut left_n = 0usize;
        let window = i64::from(self.max_fill_seconds);
        for &i in &idx[..n] {
            let slot = &mut self.slots[usize::from(i)];
            let Some(o) = slot.order.as_ref() else {
                continue;
            };
            // An order already known to send nothing (a taint, a late price
            // on its candle or on a settled fill second) takes no share
            // (LF-9 review): the later orders of the instrument see the
            // whole second. A share it took BEFORE that was known is not
            // given back; the daily re-pricing reads the later orders as
            // unavailable (`held_share_known`).
            if slot.tainted.is_some() || slot.late_price || slot.late_fill_price {
                slot.settled = slot.seconds.len();
                continue;
            }
            let at = slot.seconds.len() - 1;
            let raw = slot.seconds[at];
            let k = match left[..left_n].iter().position(|(sec, _)| *sec == raw.sec) {
                Some(k) => k,
                None => {
                    left[left_n] = (raw.sec, raw.volume);
                    left_n += 1;
                    left_n - 1
                }
            };
            let mut s = raw;
            s.volume = left[k].1;
            // The candidate as this order will store it: with any late price
            // kept for that second folded in, so the share and the pricing
            // read the same price.
            //
            // LF-9 hardening: when the next clock second is final with no
            // released bar (`next` is `None`, or a later second), a late
            // price kept for it is that second's only price. It is still a
            // candidate for `raw` (the stored `candles_1s` holds that second,
            // so the daily re-pricing reads it), and it is appended below as
            // a second with no volume, so the pricing reads it too.
            let after = raw.sec.saturating_add(1);
            let (candidate, from_late) = match next {
                Some(mut n) if n.sec == after => {
                    peek_late_widen(&slot.late_widen, slot.late_widen_len, &mut n);
                    (Some(n), false)
                }
                Some(n) if n.sec < after => (None, false),
                _ => {
                    let c = late_widen_bar(&slot.late_widen, slot.late_widen_len, after);
                    let from_late = c.is_some();
                    (c, from_late)
                }
            };
            let share = ShareOrder::of(o, window);
            let (alloc, used) = if slot.trigger_sec.is_none() {
                allot_second(&share, slot.alloc_sum, &s, candidate.as_ref())
            } else {
                (0, 0)
            };
            left[k].1 = left[k].1.saturating_sub(used);
            slot.seconds[at].volume = s.volume;
            if let Some(g) = slot.gave.get_mut(at) {
                *g = alloc > 0;
            }
            if from_late
                && after <= share.horizon_end
                && slot.seconds.len() < slot.seconds.capacity()
                && slot.gave.len() < slot.gave.capacity()
                && let Some(mut b) = candidate
            {
                // Final with no bar: no volume, settled, gives nothing.
                take_late_widen(&mut slot.late_widen, &mut slot.late_widen_len, &mut b);
                slot.seconds.push(b);
                slot.gave.push(false);
            }
            slot.settled = slot.seconds.len();
            slot.alloc_sum = slot.alloc_sum.saturating_add(alloc);
            if slot.trigger_sec.is_none() && alloc > 0 && slot.alloc_sum >= o.qty {
                slot.trigger_sec = Some(raw.sec);
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
                let share = ShareOrder::of(o, window);
                slot.late_fill_ticks = slot.late_fill_ticks.saturating_add(1);
                if let Some(p) = price_paise.filter(|p| *p > 0)
                    && widen_late(slot, &share, tick_secs, p)
                {
                    slot.late_fill_price = true;
                }
            }
        }
    }

    /// A stretch `lo..=hi` (IST seconds) the chain no longer trusts: every held
    /// order whose decision candle or fill window (`[candle open, horizon
    /// end]`) overlaps it is marked, and finishes as
    /// [`Finished::Tainted`] instead of being filled. The first reason is
    /// kept. O(64), no allocation.
    ///
    /// LF-9 review (second pass): an order whose trigger is set prices only
    /// from its seconds up to the trigger's candidate (trigger + 1), so a
    /// stretch starting after that second no longer touches it. Before this,
    /// an exit that had triggered but was waiting for an earlier held order
    /// of its instrument (`collect_ready`) was refused by a taint in seconds
    /// it never used, and the position it would have closed stayed open.
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
            let used_end = slot
                .trigger_sec
                .map_or(horizon_end, |s| s.saturating_add(1).min(horizon_end));
            if lo <= used_end && hi >= o.candle_open_secs && slot.tainted.is_none() {
                slot.tainted = Some(reason);
            }
        }
    }

    /// Indices of the orders that can be finished at `now_ist_ns`, written to
    /// `out` (cleared first; capacity [`MAX_PENDING_ORDERS`]) in insert order
    /// (`seq`), so the orders of one instrument reach the runtime in the
    /// order they were decided. First settles every held order's last second
    /// whose successor is final with no trade (`settle_key` with no
    /// candidate), so a trigger counts only volume the pricing allows.
    ///
    /// An exit (`reduce_only`) is never ready while an earlier held order of
    /// its instrument is still held (LF-9 review, findings 2 and 7): an exit
    /// is sized against the entries still held and the exits before it, so
    /// its Execute must reach the runtime after theirs, where the runtime
    /// clamps it against what they actually booked. It waits at most until
    /// the earlier order's own window ends; an earlier order never waits on a
    /// later one, so nothing waits in a circle.
    ///
    /// O(64) while nothing waits to settle, else O(64) per instrument
    /// settled; plus O(64) per ready exit and an in-place sort of at most 64
    /// indices (no allocation).
    pub fn collect_ready(&mut self, now_ist_ns: i64, clock: &BookClock, out: &mut Vec<usize>) {
        out.clear();
        if self.held > 0 {
            for i in 0..self.slots.len() {
                let slot = &self.slots[i];
                let Some(o) = slot.order.as_ref() else {
                    continue;
                };
                if slot.settled < slot.seconds.len() {
                    let last = slot.seconds[slot.seconds.len() - 1].sec;
                    if clock.second_final(last.saturating_add(1), now_ist_ns) {
                        let ki = o.key_index;
                        self.settle_key(ki, None);
                    }
                }
            }
        }
        let window = i64::from(self.max_fill_seconds);
        for (i, slot) in self.slots.iter().enumerate() {
            let Some(o) = slot.order.as_ref() else {
                continue;
            };
            let ready = if slot.late_price || slot.late_fill_price || slot.tainted.is_some() {
                true
            } else if let Some(s) = slot.trigger_sec {
                // Second s+1's price is a candidate for s.
                clock.second_final(s.saturating_add(1), now_ist_ns)
            } else {
                let horizon_end = o.decision_bar.sec.saturating_add(1).saturating_add(window);
                clock.second_final(horizon_end, now_ist_ns)
            };
            if ready && o.reduce_only && self.earlier_held(o.key_index, slot.seq) {
                // Waits for the orders it was sized against (see above).
                continue;
            }
            if ready && out.len() < out.capacity() {
                out.push(i);
            }
        }
        let slots = &self.slots;
        out.sort_unstable_by_key(|&i| slots[i].seq);
    }

    /// Whether instrument `ki` holds an order inserted before `seq`. O(64).
    fn earlier_held(&self, ki: u32, seq: u64) -> bool {
        self.slots
            .iter()
            .any(|s| s.seq < seq && s.order.as_ref().is_some_and(|o| o.key_index == ki))
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
        if slot.late_fill_price {
            slot.seconds.clear();
            slot.gave.clear();
            return Some(Finished::LateFillPrice { order });
        }
        if let Some(reason) = slot.tainted {
            return Some(Finished::Tainted { order, reason });
        }
        // Every second this order prices from is settled (its successor was
        // final when the order became ready); the last one, if not, is past
        // the trigger's candidate or at the horizon, where nothing fills.
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
        slot.gave.clear();
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
            slot.gave.clear();
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

/// One held order as the volume of a second is shared out (LF-9 findings 1
/// and 3): the inputs [`allot_second`] reads. The chain builds it from its
/// held order; the daily re-pricing from the stored Decision row, so both
/// share the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShareOrder {
    pub side: Side,
    /// Shares.
    pub qty: u64,
    pub tick_paise: i64,
    pub participation_bps: u32,
    /// The LIMIT, paise; 0 or below means no limit.
    pub limit_paise: i64,
    /// The worst of the decision second's extreme and the best quote: the
    /// floor of every fill second's worst price.
    pub base_paise: i64,
    /// The first second past the fill window (no share there).
    pub horizon_end: i64,
}

impl ShareOrder {
    /// The share inputs of a held order whose window is `window` seconds.
    /// O(1).
    #[must_use]
    pub fn of<M>(o: &HeldOrder<M>, window: i64) -> Self {
        Self::of_pricing(&HeldPricing {
            side: o.side,
            qty: o.qty,
            tick_paise: o.tick_paise,
            participation_bps: o.participation_bps,
            max_fill_seconds: u32::try_from(window).unwrap_or(u32::MAX),
            limit_paise: o.limit_paise,
            decision_bar: o.decision_bar,
            book: o.book,
        })
    }

    /// The share inputs of pricing inputs `p`. O(1).
    #[must_use]
    pub fn of_pricing(p: &HeldPricing) -> Self {
        Self {
            side: p.side,
            qty: p.qty,
            tick_paise: p.tick_paise,
            participation_bps: p.participation_bps,
            limit_paise: p.limit_paise,
            base_paise: pricing_base(p),
            horizon_end: p
                .decision_bar
                .sec
                .saturating_add(1)
                .saturating_add(i64::from(p.max_fill_seconds.max(1))),
        }
    }
}

/// The share of second `s` order `o` takes, given the shares `taken` it
/// already holds from earlier seconds: `(shares, volume used)`. `s.volume` is
/// the volume the earlier held orders of the instrument left. Nothing when the
/// order is covered, the second is at or past the horizon, or its worst price
/// (one tick against us, `next`'s price a candidate when it is the next clock
/// second) is beyond the LIMIT, exactly the seconds [`price_held_window`]
/// gives no volume. Otherwise `⌊volume × participation⌋` capped at what the
/// order still needs, and the volume it uses is the least whose share is that
/// many shares (`⌈shares / participation⌉`), so the next order sees the rest.
/// O(1), no allocation.
#[must_use]
pub fn allot_second(
    o: &ShareOrder,
    taken: u64,
    s: &SecondBar,
    next: Option<&SecondBar>,
) -> (u64, u64) {
    if taken >= o.qty
        || s.sec >= o.horizon_end
        || o.participation_bps == 0
        || second_beyond_limit(o.side, o.base_paise, o.tick_paise, o.limit_paise, s, next)
    {
        return (0, 0);
    }
    let bps = u128::from(o.participation_bps);
    let den = u128::from(BPS_DENOMINATOR);
    let cap = u128::from(s.volume) * bps / den;
    let shares = cap.min(u128::from(o.qty - taken));
    let used = (shares * den).div_ceil(bps).min(u128::from(s.volume));
    (
        u64::try_from(shares).unwrap_or(u64::MAX),
        u64::try_from(used).unwrap_or(u64::MAX),
    )
}

/// The floor of every fill second's worst price: the decision second's
/// extreme, worsened by the best quote. O(1).
fn pricing_base(p: &HeldPricing) -> i64 {
    let mut base = extreme(p.side, &p.decision_bar);
    if let Some((best, _)) = p.book {
        base = worse(p.side, base, best);
    }
    base
}

/// Whether second `s` cannot fill a LIMIT order: its worst price (the base,
/// its own extreme, `next`'s when that is the next clock second, then one
/// tick against us) is beyond `limit` (`limit <= 0`: no limit). O(1).
fn second_beyond_limit(
    side: Side,
    base: i64,
    tick: i64,
    limit: i64,
    s: &SecondBar,
    next: Option<&SecondBar>,
) -> bool {
    if limit <= 0 {
        return false;
    }
    let mut price = worse(side, base, extreme(side, s));
    if let Some(n) = next
        && n.sec == s.sec.saturating_add(1)
    {
        price = worse(side, price, extreme(side, n));
    }
    beyond_limit(side, against(side, price, tick), limit)
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
    // the LIMIT cannot fill a LIMIT order: its volume is not ours. The same
    // test decides the share a second gives (`allot_second`).
    if p.limit_paise > 0 {
        let base = pricing_base(p);
        for j in 0..seconds.len() {
            let s = seconds[j];
            let next = seconds.get(j + 1).copied();
            if second_beyond_limit(p.side, base, p.tick_paise, p.limit_paise, &s, next.as_ref()) {
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
/// entries); inserted as a second with no volume when it is final and no bar
/// for it can come; else kept until it is appended or found final. Past
/// [`LATE_WIDEN_SLOTS`] waiting seconds the price is not kept (the order
/// still counts it).
///
/// Returns `true` when the price moved a SETTLED second past the order's
/// LIMIT that was inside it when its share was counted: the second itself,
/// or the settled second before it whose candidate it is (LF-9 review). The
/// share that second gave can no longer be priced, so the caller refuses the
/// order (fail-closed) rather than price it on a share the daily re-pricing,
/// reading the stored late-amended seconds, never gives. A second past the
/// order's trigger candidate is ignored: it gave no share. So is a settled
/// second that gave this order no shares (LF-9 review, third pass: its share
/// floored to 0, earlier orders left it nothing, or it is at the horizon):
/// the re-pricing gives it none either, so the late print cannot change the
/// fill.
///
/// LF-9 hardening: a late print on a second the chain never had a bar for
/// (its book counted the trade late, so no bar is ever released) is final
/// once a later second is appended, or once the second before it is settled
/// (a second settles only when its successor is final). Its price is still
/// a candidate for the second before it: the fold opens that bucket, so the
/// stored `candles_1s`, the daily re-pricing and the replay all hold it. It
/// is inserted as a second with no volume, so the pricing reads it, and the
/// second before it is tested against the LIMIT with it as its candidate.
/// Before, such a price was kept for an append that never came and the
/// settled second kept a share priced without it.
///
/// O(log seconds), plus an in-place shift of at most `max_fill_seconds + 1`
/// entries when a second is inserted (no allocation: the buffers are
/// pre-sized to the window).
fn widen_late<M>(slot: &mut Slot<M>, share: &ShareOrder, sec: i64, price: i64) -> bool {
    let i = match slot.seconds.binary_search_by_key(&sec, |b| b.sec) {
        Ok(i) => {
            // Settled seconds whose verdict this price can move: `i` itself
            // and, through its candidate, `i - 1`.
            let prev = i.checked_sub(1);
            let inside = |slot: &Slot<M>, j: usize| {
                gave_share(slot, j) && !settled_beyond_limit(share, &slot.seconds, j)
            };
            let was_inside = [prev.is_some_and(|j| inside(slot, j)), inside(slot, i)];
            let b = &mut slot.seconds[i];
            b.high_paise = b.high_paise.max(price);
            b.low_paise = b.low_paise.min(price);
            let now_beyond =
                |j: usize| gave_share(slot, j) && settled_beyond_limit(share, &slot.seconds, j);
            return (was_inside[0] && prev.is_some_and(now_beyond))
                || (was_inside[1] && now_beyond(i));
        }
        Err(i) => i,
    };
    let after_prev = i
        .checked_sub(1)
        .is_some_and(|j| slot.seconds[j].sec == sec.saturating_sub(1));
    // Final with no bar to come: a later second is appended, or the second
    // before it is appended and settled (its successor, `sec`, was final).
    let final_no_bar = i < slot.seconds.len() || (after_prev && i <= slot.settled);
    if final_no_bar {
        return insert_late_second(slot, share, i, after_prev, sec, price);
    }
    let n = usize::from(slot.late_widen_len);
    for w in &mut slot.late_widen[..n] {
        if w.0 == sec {
            w.1 = w.1.min(price);
            w.2 = w.2.max(price);
            return false;
        }
    }
    if n < LATE_WIDEN_SLOTS {
        slot.late_widen[n] = (sec, price, price);
        slot.late_widen_len = slot.late_widen_len.saturating_add(1);
    }
    false
}

/// Whether settled second `j` of `slot` gave the order a share: settled,
/// at or before the trigger, and more than 0 shares. O(1).
fn gave_share<M>(slot: &Slot<M>, j: usize) -> bool {
    j < slot.settled
        && slot.trigger_sec.is_none_or(|t| slot.seconds[j].sec <= t)
        && slot.gave.get(j).copied().unwrap_or(false)
}

/// Inserts late second `sec` (final, no bar to come) at index `i` of `slot`
/// as a settled second with no volume, and returns whether its price moved
/// the settled second before it (`after_prev`: that second is `sec - 1`, so
/// `sec` is its candidate) past the LIMIT while that second gave a share.
/// A second already final when it is not the successor of an appended one
/// (`after_prev` false) cannot move a verdict; it is inserted all the same,
/// so a later late print on the second before it finds it. O(seconds), no
/// allocation.
fn insert_late_second<M>(
    slot: &mut Slot<M>,
    share: &ShareOrder,
    i: usize,
    after_prev: bool,
    sec: i64,
    price: i64,
) -> bool {
    let bar = SecondBar {
        sec,
        high_paise: price,
        low_paise: price,
        volume: 0,
    };
    let crossed = after_prev && {
        let j = i - 1;
        gave_share(slot, j)
            && !settled_beyond_limit(share, &slot.seconds, j)
            && second_beyond_limit(
                share.side,
                share.base_paise,
                share.tick_paise,
                share.limit_paise,
                &slot.seconds[j],
                Some(&bar),
            )
    };
    // Every second before `i` is settled here (a later append settled them,
    // or `i - 1` is settled), so the new second joins the settled prefix.
    if i <= slot.settled
        && slot.seconds.len() < slot.seconds.capacity()
        && slot.gave.len() < slot.gave.capacity()
    {
        slot.seconds.insert(i, bar);
        slot.gave.insert(i, false);
        slot.settled += 1;
    }
    crossed
}

/// Whether settled second `j` of `seconds` is beyond the order's LIMIT, read
/// the way its share was counted: its own price and, when the next appended
/// second is the next clock second, that one's as a candidate. O(1).
fn settled_beyond_limit(share: &ShareOrder, seconds: &[SecondBar], j: usize) -> bool {
    let Some(s) = seconds.get(j) else {
        return false;
    };
    second_beyond_limit(
        share.side,
        share.base_paise,
        share.tick_paise,
        share.limit_paise,
        s,
        seconds.get(j + 1),
    )
}

/// Folds a late price kept for `bar`'s second into `bar` without dropping it
/// (the append still takes it). O(slots).
fn peek_late_widen(widen: &[(i64, i64, i64); LATE_WIDEN_SLOTS], len: u8, bar: &mut SecondBar) {
    let n = usize::from(len).min(LATE_WIDEN_SLOTS);
    for &(sec, lo, hi) in &widen[..n] {
        if sec == bar.sec {
            bar.low_paise = bar.low_paise.min(lo);
            bar.high_paise = bar.high_paise.max(hi);
            return;
        }
    }
}

/// Applies (and drops) a late price kept for `bar`'s second, and drops any
/// kept for an earlier second (LF-9 hardening): once `bar` is appended, no
/// earlier second can be appended, so such a price can never be applied and
/// would only use up one of the [`LATE_WIDEN_SLOTS`]. O(slots).
fn take_late_widen(
    widen: &mut [(i64, i64, i64); LATE_WIDEN_SLOTS],
    len: &mut u8,
    bar: &mut SecondBar,
) {
    let mut n = usize::from(*len).min(LATE_WIDEN_SLOTS);
    let mut i = 0;
    while i < n {
        let (sec, lo, hi) = widen[i];
        if sec <= bar.sec {
            if sec == bar.sec {
                bar.low_paise = bar.low_paise.min(lo);
                bar.high_paise = bar.high_paise.max(hi);
            }
            n -= 1;
            widen[i] = widen[n];
        } else {
            i += 1;
        }
    }
    *len = u8::try_from(n).unwrap_or(0);
}

/// The late price kept for second `sec`, as a second with no volume. O(slots).
fn late_widen_bar(
    widen: &[(i64, i64, i64); LATE_WIDEN_SLOTS],
    len: u8,
    sec: i64,
) -> Option<SecondBar> {
    let n = usize::from(len).min(LATE_WIDEN_SLOTS);
    widen[..n]
        .iter()
        .find(|w| w.0 == sec)
        .map(|&(sec, lo, hi)| SecondBar {
            sec,
            high_paise: hi,
            low_paise: lo,
            volume: 0,
        })
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
            reduce_only: false,
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

    /// LF-9 review: a late 1 s print that moves a SETTLED fill second past
    /// the order's LIMIT (its share already counted) refuses the order,
    /// instead of pricing it short on the seconds that are left. A late print
    /// that keeps the second inside the LIMIT, or lands on a second past the
    /// trigger's candidate, changes no share and refuses nothing.
    #[test]
    fn test_regression_late_price_past_the_limit_on_a_settled_second_refuses_the_order() {
        let c = clock();
        let mut ready = Vec::with_capacity(MAX_PENDING_ORDERS);
        let limited = || {
            let mut o = order(0, 50);
            o.limit_paise = 10_100;
            o
        };
        // DEC + 1 settles with a share of 30 (120 x 25%); a late print at
        // 101.00 then moves it past the 101.00 limit (one tick against us).
        let mut p = PendingBook::<u32>::new(60, 1);
        p.insert(limited()).expect("room");
        p.on_second(0, sec(DEC + 1, 120));
        p.on_second(0, sec(DEC + 2, 80));
        p.on_late_tick(0, 0, DEC + 1, Some(10_100));
        p.on_second(0, sec(DEC + 3, 80));
        p.collect_ready((DEC + 3) * NS, &c, &mut ready);
        assert_eq!(ready, vec![0], "refused at once, not after the window");
        assert!(
            matches!(p.finish(0), Some(Finished::LateFillPrice { .. })),
            "a settled share that no longer holds refuses the order"
        );
        // The same through its candidate: a late print on DEC + 2 moves the
        // settled DEC + 1 (whose candidate it is) past the limit.
        let mut p = PendingBook::<u32>::new(60, 1);
        p.insert(limited()).expect("room");
        p.on_second(0, sec(DEC + 1, 120));
        p.on_second(0, sec(DEC + 2, 80));
        p.on_late_tick(0, 0, DEC + 2, Some(10_100));
        ready.clear();
        p.collect_ready((DEC + 2) * NS, &c, &mut ready);
        assert!(matches!(p.finish(0), Some(Finished::LateFillPrice { .. })));
        // A late print that stays inside the limit: priced as before.
        let mut p = PendingBook::<u32>::new(60, 1);
        p.insert(limited()).expect("room");
        p.on_second(0, sec(DEC + 1, 120));
        p.on_second(0, sec(DEC + 2, 80));
        p.on_late_tick(0, 0, DEC + 1, Some(10_050));
        p.on_second(0, sec(DEC + 3, 80));
        ready.clear();
        p.collect_ready((DEC + 5) * NS, &c, &mut ready);
        assert_eq!(ready, vec![0]);
        match p.finish(0) {
            Some(Finished::Filled { summary, .. }) => assert_eq!(summary.filled_qty, 50),
            other => panic!("expected a fill, got {other:?}"),
        }
        // A late print past the limit on a second past the trigger's
        // candidate (it gave no share): priced as before.
        let mut p = PendingBook::<u32>::new(60, 1);
        p.insert(limited()).expect("room");
        p.on_second(0, sec(DEC + 1, 400));
        p.on_second(0, sec(DEC + 2, 80));
        p.on_second(0, sec(DEC + 3, 80));
        p.on_second(0, sec(DEC + 4, 80));
        p.on_late_tick(0, 0, DEC + 3, Some(10_200));
        ready.clear();
        p.collect_ready((DEC + 4) * NS, &c, &mut ready);
        assert_eq!(ready, vec![0]);
        match p.finish(0) {
            Some(Finished::Filled { summary, .. }) => assert_eq!(summary.filled_qty, 50),
            other => panic!("expected a fill, got {other:?}"),
        }
    }

    /// LF-9 review, third pass: a late print past the LIMIT on a settled
    /// second that gave the order NO shares (2 shares traded, 25% floors to
    /// 0) cannot change its fill, so the order is priced, not refused. Before,
    /// `widen_late` treated every settled second at or before the trigger as
    /// one that gave a share and refused a valid paper order.
    #[test]
    fn test_regression_late_price_on_a_zero_share_settled_second_refuses_nothing() {
        let c = clock();
        let mut ready = Vec::with_capacity(MAX_PENDING_ORDERS);
        let mut o = order(0, 50);
        o.limit_paise = 10_100;
        let mut p = PendingBook::<u32>::new(60, 1);
        p.insert(o).expect("room");
        // DEC + 1 gives 30; DEC + 3 (2 shares traded) gives 0; DEC + 4 gives
        // the last 20 and triggers. DEC + 2 did not trade, so DEC + 3 is no
        // second's candidate.
        p.on_second(0, sec(DEC + 1, 120));
        p.on_second(0, sec(DEC + 3, 2));
        p.on_second(0, sec(DEC + 4, 120));
        // DEC + 3 is settled now; a late print past the limit lands on it.
        p.on_late_tick(0, 0, DEC + 3, Some(10_100));
        p.on_second(0, sec(DEC + 5, 80));
        p.collect_ready((DEC + 8) * NS, &c, &mut ready);
        assert_eq!(ready, vec![0]);
        match p.finish(0) {
            Some(Finished::Filled {
                summary,
                late_fill_ticks,
                beyond_limit_qty,
                ..
            }) => {
                assert_eq!(summary.filled_qty, 50, "{summary:?}");
                assert_eq!(beyond_limit_qty, 0);
                assert_eq!(late_fill_ticks, 1, "the late print is still counted");
            }
            other => panic!("a zero-share second must not refuse the order: {other:?}"),
        }
    }

    fn sec_at(s: i64, high: i64, vol: u64) -> SecondBar {
        SecondBar {
            sec: s,
            high_paise: high,
            low_paise: 10_000,
            volume: vol,
        }
    }

    /// LF-9 hardening (finding 1): a late 1 s print on a second the chain
    /// never had a bar for (its book counted the trade late), right after a
    /// settled second that gave the order shares, is that second's candidate:
    /// the fold opens the late bucket, so `candles_1s`, the daily re-pricing
    /// and the replay all hold it. Past the LIMIT it refuses the order; inside
    /// the LIMIT it prices the settled second. Before, the price was kept for
    /// an append that never came, and the order filled 50 priced as if the
    /// print never happened.
    #[test]
    fn test_regression_late_print_on_an_untraded_second_after_a_settled_share() {
        let c = clock();
        let mut ready = Vec::with_capacity(MAX_PENDING_ORDERS);
        let limited = || {
            let mut o = order(0, 50);
            o.limit_paise = 10_100;
            o
        };
        // Past the limit: DEC + 1 gives 30 (120 x 25%), DEC + 2 is final with
        // no trade, then a print stamped DEC + 2 at 101.00 arrives late.
        let mut p = PendingBook::<u32>::new(60, 1);
        p.insert(limited()).expect("room");
        p.on_second(0, sec(DEC + 1, 120));
        p.collect_ready((DEC + 5) * NS, &c, &mut ready);
        assert!(ready.is_empty(), "DEC + 1 settled, nothing triggered yet");
        p.on_late_tick(0, 0, DEC + 2, Some(10_100));
        p.on_second(0, sec(DEC + 3, 120));
        ready.clear();
        p.collect_ready((DEC + 8) * NS, &c, &mut ready);
        assert_eq!(ready, vec![0]);
        assert!(
            matches!(p.finish(0), Some(Finished::LateFillPrice { .. })),
            "a settled share past the limit through a late candidate refuses"
        );
        // Inside the limit: the late print at 100.50 is DEC + 1's candidate,
        // so its 30 shares price at 100.55, the 20 from DEC + 3 at 100.25.
        let mut p = PendingBook::<u32>::new(60, 1);
        p.insert(limited()).expect("room");
        p.on_second(0, sec(DEC + 1, 120));
        ready.clear();
        p.collect_ready((DEC + 5) * NS, &c, &mut ready);
        p.on_late_tick(0, 0, DEC + 2, Some(10_050));
        p.on_second(0, sec(DEC + 3, 120));
        ready.clear();
        p.collect_ready((DEC + 8) * NS, &c, &mut ready);
        assert_eq!(ready, vec![0]);
        match p.finish(0) {
            Some(Finished::Filled { summary, .. }) => {
                assert_eq!(summary.filled_qty, 50);
                assert_eq!(
                    summary.vwap_paise,
                    Some(10_043),
                    "(30 x 10,055 + 20 x 10,025) / 50"
                );
            }
            other => panic!("expected a fill, got {other:?}"),
        }
    }

    /// LF-9 hardening (finding 4): a late print kept for a second that then
    /// turns out final with no trade is the candidate of the second before
    /// it when that one settles. BUY 100 at 25%, limit 101.00: DEC + 1
    /// trades 400 but the late DEC + 2 print at 101.00 puts it past the
    /// limit, so it gives nothing and the order fills from DEC + 3 at 100.45.
    /// Before, DEC + 1 settled with no candidate, gave all 100 and the order
    /// finished at 100.25, while the re-pricing (reading the stored DEC + 2)
    /// cut DEC + 1 as beyond the limit.
    #[test]
    fn test_regression_kept_late_price_is_the_candidate_when_its_second_never_trades() {
        let c = clock();
        let mut ready = Vec::with_capacity(MAX_PENDING_ORDERS);
        let mut o = order(0, 100);
        o.limit_paise = 10_100;
        let mut p = PendingBook::<u32>::new(60, 1);
        p.insert(o).expect("room");
        p.on_second(0, sec(DEC + 1, 400));
        p.on_late_tick(0, 0, DEC + 2, Some(10_100));
        p.collect_ready((DEC + 5) * NS, &c, &mut ready);
        assert!(ready.is_empty(), "DEC + 1 gave nothing, nothing triggered");
        p.on_second(0, sec_at(DEC + 3, 10_040, 400));
        p.on_second(0, sec_at(DEC + 4, 10_040, 400));
        ready.clear();
        p.collect_ready((DEC + 8) * NS, &c, &mut ready);
        assert_eq!(ready, vec![0]);
        match p.finish(0) {
            Some(Finished::Filled { summary, .. }) => {
                assert_eq!(summary.filled_qty, 100);
                assert_eq!(summary.vwap_paise, Some(10_045));
            }
            other => panic!("expected a fill, got {other:?}"),
        }
    }

    /// LF-9 hardening (finding 1, second effect): late prices kept for
    /// seconds that can no longer be appended (an appended later second
    /// passed them) are dropped, so they do not use up the kept-price slots
    /// and push out a real waiting price. Four stale prints, then a print
    /// past the limit on DEC + 12 while it waits for its release: it must
    /// still be DEC + 11's candidate, so the order fills from DEC + 13 at
    /// 100.45, not from DEC + 11 at 100.25.
    #[test]
    fn test_regression_stale_kept_late_prices_do_not_crowd_out_a_waiting_one() {
        let c = clock();
        let mut ready = Vec::with_capacity(MAX_PENDING_ORDERS);
        let mut o = order(0, 100);
        o.limit_paise = 10_100;
        let mut p = PendingBook::<u32>::new(60, 1);
        p.insert(o).expect("room");
        p.on_second(0, sec(DEC + 1, 2));
        for s in 3..=6 {
            p.on_late_tick(0, 0, DEC + s, Some(10_010));
        }
        p.on_second(0, sec(DEC + 10, 2));
        p.on_second(0, sec(DEC + 11, 400));
        p.on_late_tick(0, 0, DEC + 12, Some(10_100));
        p.on_second(0, sec(DEC + 12, 400));
        p.on_second(0, sec_at(DEC + 13, 10_040, 400));
        p.on_second(0, sec_at(DEC + 14, 10_040, 400));
        p.collect_ready((DEC + 20) * NS, &c, &mut ready);
        assert_eq!(ready, vec![0]);
        match p.finish(0) {
            Some(Finished::Filled { summary, .. }) => {
                assert_eq!(summary.filled_qty, 100);
                assert_eq!(summary.vwap_paise, Some(10_045));
            }
            other => panic!("expected a fill, got {other:?}"),
        }
    }

    /// LF-9 review: an order already known to send nothing (a late price on
    /// its candle) takes no share of a later settled second, so the next
    /// held order of the instrument sees the whole second.
    #[test]
    fn test_regression_an_order_that_sends_nothing_takes_no_share() {
        let c = clock();
        let mut ready = Vec::with_capacity(MAX_PENDING_ORDERS);
        let mut p = PendingBook::<u32>::new(60, 1);
        p.insert(order(0, 100)).expect("room");
        let mut b = order(0, 100);
        b.frame_index = 2;
        p.insert(b).expect("room");
        // A late price on the first order's candle (frame 1 only).
        p.on_late_tick(0, 1, DEC - 3, None);
        p.on_second(0, sec(DEC + 1, 400));
        p.on_second(0, sec(DEC + 2, 400));
        // The second order took all of DEC + 1's cap: triggered there, ready
        // once DEC + 2 is final.
        p.collect_ready((DEC + 4) * NS, &c, &mut ready);
        assert_eq!(ready, vec![0, 1], "the second order triggered at DEC + 1");
        assert!(matches!(p.finish(0), Some(Finished::LatePrice { .. })));
        match p.finish(1) {
            Some(Finished::Filled { summary, .. }) => {
                assert_eq!(summary.filled_qty, 100);
                assert_eq!(summary.seconds_used, 1, "{summary:?}");
            }
            other => panic!("expected a fill, got {other:?}"),
        }
    }

    #[test]
    fn test_held_entry_qty_counts_only_held_entries_of_the_side() {
        let mut p = PendingBook::<u32>::new(60, 2);
        assert_eq!(p.held_entry_qty(0, Side::Buy), 0);
        p.insert(order(0, 30)).expect("room");
        let mut exit = order(0, 20);
        exit.reduce_only = true;
        p.insert(exit).expect("room");
        let mut sell = order(0, 7);
        sell.side = Side::Sell;
        p.insert(sell).expect("room");
        p.insert(order(1, 99)).expect("room");
        assert_eq!(
            p.held_entry_qty(0, Side::Buy),
            30,
            "the exit is not counted"
        );
        assert_eq!(p.held_entry_qty(0, Side::Sell), 7);
        assert_eq!(p.held_entry_qty(1, Side::Buy), 99);
    }

    /// LF-9 review finding 1, judged against the code and NOT changed: with
    /// a book, the shares past the best quantity are priced one more tick
    /// against us and cut at the LIMIT, but `worst_fill` still takes them out
    /// of the order's remainder, so no later in-limit second can fill them.
    /// Finishing the order at the second its shares first cover the quantity
    /// therefore gives exactly the fill the whole 60 s window gives; the early
    /// finish cuts off nothing. (Letting a later second fill those shares
    /// would change the fill rule, not this trigger.)
    #[test]
    fn test_trigger_finish_equals_the_whole_window_with_a_book_past_best_qty() {
        let pricing = HeldPricing {
            side: Side::Buy,
            qty: 100,
            tick_paise: 5,
            participation_bps: 2_500,
            max_fill_seconds: 60,
            limit_paise: 10_050,
            decision_bar: SecondBar {
                sec: DEC,
                high_paise: 10_000,
                low_paise: 10_000,
                volume: 0,
            },
            book: Some((10_000, 10)),
        };
        let bar = |s: i64, high: i64, vol: u64| SecondBar {
            sec: s,
            high_paise: high,
            low_paise: 10_000,
            volume: vol,
        };
        let mut whole: Vec<SecondBar> = vec![bar(DEC + 1, 10_045, 400), bar(DEC + 2, 10_045, 400)];
        whole.extend((3..=60).map(|s| bar(DEC + s, 10_000, 10_000)));
        let share = ShareOrder::of_pricing(&pricing);
        let (shares, _) = allot_second(&share, 0, &whole[0], Some(&whole[1]));
        assert_eq!(
            shares, 100,
            "the first second covers the order: the trigger"
        );
        let mut cut: Vec<SecondBar> = whole[..2].to_vec();
        let mut out = FillSchedule::with_capacity(FillSchedule::required_capacity(60, 0));
        let at_trigger = price_held_window(&pricing, &mut cut, &mut out).expect("priced");
        let full = price_held_window(&pricing, &mut whole, &mut out).expect("priced");
        assert_eq!(at_trigger, full, "the later seconds add nothing");
        assert_eq!(full.0.filled_qty, 10, "the best quantity only");
        assert_eq!(full.1, 90, "the rest priced past the LIMIT");
    }

    /// LF-9 review, findings 2 and 7: an exit in a LOWER slot than the entry
    /// it was sized against is never finished before that entry, whether both
    /// are covered on the same second or the exit is covered first (the
    /// entry's seconds beyond its own LIMIT); the ready list is in insert
    /// order. Without this the exit's Execute reached the runtime first, was
    /// refused `exit_without_position`, and the entry's long stayed open.
    #[test]
    fn test_regression_an_exit_in_a_lower_slot_waits_for_its_entry_collect_ready() {
        for entry_limit in [1_000_000, 1] {
            let mut p = PendingBook::<u32>::new(60, 6);
            for k in 1..=5 {
                p.insert(order(k, 10)).expect("room"); // slots 0..=4
            }
            let mut entry = order(0, 5);
            entry.limit_paise = entry_limit;
            p.insert(entry).expect("room"); // slot 5
            assert!(p.finish(2).is_some(), "slot 2 freed");
            let mut exit = order(0, 5);
            exit.side = Side::Sell;
            exit.limit_paise = 1;
            exit.reduce_only = true;
            p.insert(exit).expect("room"); // slot 2, a lower index
            p.on_second(0, sec(DEC + 1, 400));
            p.on_second(0, sec(DEC + 2, 400));
            let c = clock();
            let mut ready = Vec::with_capacity(MAX_PENDING_ORDERS);
            p.collect_ready((DEC + 5) * NS, &c, &mut ready);
            if entry_limit == 1 {
                assert!(ready.is_empty(), "the exit waits for its entry: {ready:?}");
                p.collect_ready((DEC + 63) * NS, &c, &mut ready);
                assert!(ready.contains(&5), "the entry's window ended: {ready:?}");
                assert!(!ready.contains(&2), "the exit still waits: {ready:?}");
            } else {
                assert_eq!(ready, vec![5], "the entry first; the exit waits");
            }
            assert!(p.finish(5).is_some());
            p.collect_ready((DEC + 63) * NS, &c, &mut ready);
            assert!(ready.contains(&2), "the exit follows its entry: {ready:?}");
            let pos = |i| ready.iter().position(|r| *r == i);
            for other in [0, 1, 3, 4] {
                if let (Some(a), Some(b)) = (pos(other), pos(2)) {
                    assert!(a < b, "insert order: {ready:?}");
                }
            }
        }
    }

    /// LF-9 review (second pass): an exit that has triggered but waits for an
    /// earlier thin entry of its instrument is not refused by a taint that
    /// starts after its own trigger + 1 (inside the entry's window). The
    /// entry, whose window the taint touches, is still refused.
    #[test]
    fn test_regression_a_triggered_waiting_exit_ignores_a_later_taint_on_taint() {
        let mut p = PendingBook::<u32>::new(60, 2);
        let mut entry = order(0, 5);
        entry.limit_paise = 1; // a Buy at 1 paisa never fills: held to its horizon
        p.insert(entry).expect("room"); // slot 0
        let mut exit = order(0, 5);
        exit.side = Side::Sell;
        exit.limit_paise = 1;
        exit.reduce_only = true;
        p.insert(exit).expect("room"); // slot 1
        p.on_second(0, sec(DEC + 1, 400));
        p.on_second(0, sec(DEC + 2, 400));
        let c = clock();
        let mut ready = Vec::with_capacity(MAX_PENDING_ORDERS);
        p.collect_ready((DEC + 5) * NS, &c, &mut ready);
        assert!(ready.is_empty(), "the exit waits for its entry: {ready:?}");
        // A tick loss well after the exit's trigger, inside the entry's window.
        p.on_taint(DEC + 30, i64::MAX, TaintReason::TickDropped);
        p.collect_ready((DEC + 31) * NS, &c, &mut ready);
        assert_eq!(ready, vec![0], "the tainted entry finishes first");
        assert!(matches!(p.finish(0), Some(Finished::Tainted { .. })));
        p.collect_ready((DEC + 31) * NS, &c, &mut ready);
        assert_eq!(ready, vec![1]);
        match p.finish(1) {
            Some(Finished::Filled { summary, .. }) => {
                assert_eq!(summary.filled_qty, 5, "the exit fills: {summary:?}");
            }
            other => panic!("the exit must fill, got {other:?}"),
        }
        // A taint reaching back into the exit's used seconds still refuses it.
        let mut p = PendingBook::<u32>::new(60, 1);
        let mut exit = order(0, 5);
        exit.side = Side::Sell;
        exit.limit_paise = 1;
        p.insert(exit).expect("room");
        p.on_second(0, sec(DEC + 1, 400));
        p.on_second(0, sec(DEC + 2, 400));
        p.on_taint(DEC + 2, DEC + 2, TaintReason::FeedGap);
        p.collect_ready((DEC + 5) * NS, &c, &mut ready);
        assert!(matches!(p.finish(0), Some(Finished::Tainted { .. })));
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

    /// LF-9 findings 1 and 3: `allot_second` takes `⌊volume × part⌋` capped
    /// at what is left of the order, uses the least volume that gives it, and
    /// gives nothing past the horizon, once covered, or for a second whose
    /// worst price (the next clock second's included, one tick against us) is
    /// beyond the LIMIT (`ShareOrder::of_pricing`).
    #[test]
    fn test_share_order_of_pricing_and_allot_second_limit_cap_and_horizon() {
        let mut o = order(0, 30);
        o.limit_paise = 10_030;
        let share = ShareOrder::of(&o, 60);
        assert_eq!(share.base_paise, 10_010, "the decision high (a buy)");
        assert_eq!(share.horizon_end, DEC + 61);
        assert_eq!(
            ShareOrder::of_pricing(&HeldPricing {
                side: o.side,
                qty: o.qty,
                tick_paise: o.tick_paise,
                participation_bps: o.participation_bps,
                max_fill_seconds: 60,
                limit_paise: o.limit_paise,
                decision_bar: o.decision_bar,
                book: o.book,
            }),
            share
        );
        // 25% of 100 = 25 shares, using all 100 of the volume.
        let s = sec(DEC + 1, 100);
        assert_eq!(allot_second(&share, 0, &s, None), (25, 100));
        // Only 5 more are needed: 20 of the volume is enough for them.
        assert_eq!(allot_second(&share, 25, &s, None), (5, 20));
        assert_eq!(allot_second(&share, 30, &s, None), (0, 0), "covered");
        // The next second prints at 10,030: with the tick, beyond the limit.
        let hot = SecondBar {
            sec: DEC + 2,
            high_paise: 10_030,
            low_paise: 10_000,
            volume: 9,
        };
        assert_eq!(
            allot_second(&share, 0, &s, Some(&hot)),
            (0, 0),
            "beyond the limit"
        );
        // A gap: the next traded second is not a candidate.
        let later = SecondBar {
            sec: DEC + 5,
            ..hot
        };
        assert_eq!(allot_second(&share, 0, &s, Some(&later)), (25, 100));
        assert_eq!(
            allot_second(&share, 0, &sec(DEC + 61, 100), None),
            (0, 0),
            "horizon"
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

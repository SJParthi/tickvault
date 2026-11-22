//! Stretches of time the paper fill chain cannot trust (plan item LF-6b).
//!
//! A candle overlapping one of these still goes to the strategy (its state
//! must advance), but any order it asks for is refused with the reason. A
//! range added while an order is already held is checked against that order
//! too (`LiveFillChain` hands every fresh range to the held-order book): its
//! decision candle and its fill seconds must both be trusted.
//!
//! * `tick_dropped`: ticks went missing from the live stream (`drop_seq`
//!   moved between two received ticks: the tap's own ring, a frame shed before
//!   the drain, or an abandoned frame tail). The missing ticks' exchange
//!   seconds are bounded below by the newest receipt seen before the drop
//!   minus [`MAX_DROP_LAG_SECS`] (delivery lag differs per instrument, so the
//!   newest exchange second seen is NOT a bound), and above by the wall clock
//!   of the next pass plus [`FUTURE_TICK_SKEW_SECS`] (a frame shed before the
//!   drain can be newer than the tick that reports it). Every candle
//!   overlapping that stretch, in every frame and for every instrument, reads
//!   tainted.
//! * `feed_gap`: no tick at all for [`FEED_GAP_NS`] of receipt time inside the
//!   session (the silence is measured from 09:15:00 at the earliest, so a
//!   quiet pre-open is not a gap). Detected when the gap ends (the next tick)
//!   AND while it lasts (each pass compares the wall clock with the last
//!   receipt), so a candle inside a gap is tainted before it is decided.
//! * `clock_step`: the wall clock moved more than [`CLOCK_STEP_NS`] against
//!   the monotonic clock between two passes; candles closed across the step
//!   were closed at the wrong instant.
//! * `chain_start`: the chain started after 09:15:00 (a restart): every
//!   instrument's first bar on every frame holds only the ticks after the
//!   start, and the volume baseline swallows the first tick's volume.
//!
//! # Complexity
//!
//! [`TaintRanges::add`]: O(1) (merges into the newest range when they touch).
//! [`TaintRanges::taint_of`]: O([`TAINT_RANGES_CAPACITY`]) = O(256) compares,
//! run only for a candle that produced an order (flagged, not O(1)). When the
//! ring is full the oldest range is folded into a floor: every candle opening
//! at or before the floor reads tainted, so an overflow only ever refuses more
//! orders, never fewer. No allocation after construction.

use tickvault_trading::brutex_host::live_candles::FUTURE_TICK_SKEW_SECS;

use super::timing::{FEED_WATCH_CLOSE_SOD, SESSION_CLOSE_SOD, SESSION_OPEN_SOD};

/// Receipt-time gap between two ticks, inside the session, that taints.
pub const FEED_GAP_NS: i64 = 2_000_000_000;
/// Wall-vs-monotonic disagreement between two passes that counts as a step.
pub const CLOCK_STEP_NS: i64 = 250_000_000;
/// Ranges kept before the oldest is folded into the floor.
pub const TAINT_RANGES_CAPACITY: usize = 256;
/// The largest delivery lag (receipt minus exchange second) a dropped tick is
/// assumed to have. The production maximum measured is 199 s (scope lock);
/// 300 s keeps a margin. A dropped tick lagging more than this is not
/// covered (honest limit).
pub const MAX_DROP_LAG_SECS: i64 = 300;
const NANOS_PER_SEC: i64 = 1_000_000_000;
const SECS_PER_DAY: i64 = 86_400;

/// Why a candle is not trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaintReason {
    TickDropped,
    FeedGap,
    ClockStep,
    ChainStart,
}

impl TaintReason {
    /// The reason text written on the refusal row. O(1).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TickDropped => "tick_dropped",
            Self::FeedGap => "feed_gap",
            Self::ClockStep => "clock_step",
            Self::ChainStart => "chain_start",
        }
    }

    /// How many reasons there are.
    pub const COUNT: usize = 4;

    /// Every reason, in counter order. The metric label is [`Self::as_str`].
    pub const ALL: [Self; Self::COUNT] = [
        Self::TickDropped,
        Self::FeedGap,
        Self::ClockStep,
        Self::ChainStart,
    ];
}

#[derive(Debug, Clone, Copy)]
struct Range {
    lo: i64,
    hi: i64,
    reason: TaintReason,
}

/// A bounded set of tainted second ranges (IST epoch seconds, inclusive).
#[derive(Debug)]
pub struct TaintRanges {
    ring: Vec<Range>,
    /// Index of the oldest range when the ring is full.
    head: usize,
    /// Candles opening at or before this second are tainted (overflow).
    floor: Option<(i64, TaintReason)>,
    added: [u64; TaintReason::COUNT],
    /// The hull of every range added or extended since the last
    /// [`Self::take_fresh`], with the first reason, for the held orders.
    fresh: Option<(i64, i64, TaintReason)>,
}

impl Default for TaintRanges {
    fn default() -> Self {
        Self::new()
    }
}

impl TaintRanges {
    /// Cold; the only allocation.
    #[must_use]
    pub fn new() -> Self {
        Self {
            ring: Vec::with_capacity(TAINT_RANGES_CAPACITY),
            head: 0,
            floor: None,
            added: [0; TaintReason::COUNT],
            fresh: None,
        }
    }

    /// Ranges added per reason, in [`TaintReason::ALL`] order.
    #[must_use]
    pub fn added(&self) -> [u64; TaintReason::COUNT] {
        self.added
    }

    /// Clears everything (a new trading day). O(1).
    pub fn clear(&mut self) {
        self.ring.clear();
        self.head = 0;
        self.floor = None;
        self.fresh = None;
    }

    /// Marks seconds `lo..=hi` (either order) as untrusted and counts one
    /// range for `reason`. O(1).
    pub fn add(&mut self, a: i64, b: i64, reason: TaintReason) {
        let idx = reason as usize;
        self.added[idx] = self.added[idx].saturating_add(1);
        self.insert(a, b, reason);
    }

    /// [`Self::add`] without counting: widens a stretch already counted (an
    /// ongoing feed gap, a drop's upper bound). O(1).
    pub fn extend(&mut self, a: i64, b: i64, reason: TaintReason) {
        self.insert(a, b, reason);
    }

    /// The hull of the ranges added since the last call, or `None`. O(1).
    pub fn take_fresh(&mut self) -> Option<(i64, i64, TaintReason)> {
        self.fresh.take()
    }

    fn insert(&mut self, a: i64, b: i64, reason: TaintReason) {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        self.fresh = Some(match self.fresh {
            Some((flo, fhi, fr)) => (flo.min(lo), fhi.max(hi), fr),
            None => (lo, hi, reason),
        });
        // Merge into the newest range when they touch and agree.
        let newest = if self.ring.len() < TAINT_RANGES_CAPACITY {
            self.ring.len().checked_sub(1)
        } else {
            Some((self.head + TAINT_RANGES_CAPACITY - 1) % TAINT_RANGES_CAPACITY)
        };
        if let Some(n) = newest {
            let r = &mut self.ring[n];
            if r.reason == reason && lo <= r.hi.saturating_add(1) && hi >= r.lo.saturating_sub(1) {
                r.lo = r.lo.min(lo);
                r.hi = r.hi.max(hi);
                return;
            }
        }
        let range = Range { lo, hi, reason };
        if self.ring.len() < TAINT_RANGES_CAPACITY {
            self.ring.push(range);
            return;
        }
        // Full: fold the oldest into the floor, then reuse its slot.
        let old = self.ring[self.head];
        self.floor = Some(match self.floor {
            Some((f, r)) if f >= old.hi => (f, r),
            _ => (old.hi, old.reason),
        });
        self.ring[self.head] = range;
        self.head = (self.head + 1) % TAINT_RANGES_CAPACITY;
    }

    /// The reason a candle covering `[open_secs, end_secs)` is untrusted, or
    /// `None`. See the module docs for the cost.
    #[must_use]
    pub fn taint_of(&self, open_secs: i64, end_secs: i64) -> Option<TaintReason> {
        let last = end_secs.saturating_sub(1).max(open_secs);
        if let Some((f, r)) = self.floor
            && open_secs <= f
        {
            return Some(r);
        }
        self.ring
            .iter()
            .find(|r| r.lo <= last && r.hi >= open_secs)
            .map(|r| r.reason)
    }
}

/// 09:15:00 IST of the day `ist_ns` falls in, IST epoch nanoseconds. O(1).
#[must_use]
pub fn session_open_ns(ist_ns: i64) -> i64 {
    ist_ns
        .div_euclid(NANOS_PER_SEC)
        .div_euclid(SECS_PER_DAY)
        .saturating_mul(SECS_PER_DAY)
        .saturating_add(SESSION_OPEN_SOD)
        .saturating_mul(NANOS_PER_SEC)
}

/// Whether `ist_ns` lies in 09:15:00-15:30:00 of its day. O(1).
#[must_use]
pub fn in_session_ns(ist_ns: i64) -> bool {
    let sod = ist_ns.div_euclid(NANOS_PER_SEC).rem_euclid(SECS_PER_DAY);
    (SESSION_OPEN_SOD..SESSION_CLOSE_SOD).contains(&sod)
}

/// Whether `ist_ns` lies in 09:15:00-15:40:00 of its day: the stretch the
/// feed-gap watch is armed, up to the end of F&O trading, so a held order
/// filling on seconds after the 15:30 strategy close still sees a stall
/// (LF-6b review round 2, finding 4). O(1).
#[must_use]
pub fn in_feed_watch_ns(ist_ns: i64) -> bool {
    let sod = ist_ns.div_euclid(NANOS_PER_SEC).rem_euclid(SECS_PER_DAY);
    (SESSION_OPEN_SOD..FEED_WATCH_CLOSE_SOD).contains(&sod)
}

/// The stretch a chain started at `now_ist_ns` cannot trust: from 09:15:00 of
/// the day to the end of the longest bucket open at the start, when the start
/// lies inside the session (strictly after 09:15:00); else `None`. O(1).
#[must_use]
pub fn chain_start_taint(now_ist_ns: i64, longest_frame_secs: u32) -> Option<(i64, i64)> {
    let open_ns = session_open_ns(now_ist_ns);
    if now_ist_ns <= open_ns || !in_session_ns(now_ist_ns) {
        return None;
    }
    let now_secs = now_ist_ns.div_euclid(NANOS_PER_SEC);
    Some((
        open_ns.div_euclid(NANOS_PER_SEC),
        now_secs.saturating_add(i64::from(longest_frame_secs.max(1))),
    ))
}

/// Whether loss count `seq` is ahead of `last` (wrapping, 32 bits). O(1).
#[inline]
fn seq_ahead(seq: u32, last: u32) -> bool {
    // APPROVED: wrapping distance read as signed; a count never moves 2^31.
    #[allow(clippy::cast_possible_wrap)]
    let d = seq.wrapping_sub(last) as i32;
    d > 0
}

/// Watches the tap's stream for drops and feed gaps, and the clocks for steps.
/// O(1) per tick and per pass, no allocation.
#[derive(Debug)]
pub struct StreamWatch {
    /// The tap's loss count carried by the last received tick; starts at 0,
    /// the count of a fresh tap.
    last_drop_seq: u32,
    /// Newest exchange second seen.
    max_exchange_secs: i64,
    last_receipt_ns: Option<i64>,
    last_pass: Option<(i64, i64)>,
    /// A drop's lower bound waiting for the next pass's wall clock to bound
    /// it above.
    drop_pending_lo: Option<i64>,
    /// The feed gap the passes are reporting while it lasts: its lower bound.
    gap_open_lo: Option<i64>,
}

impl Default for StreamWatch {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamWatch {
    /// Cold.
    #[must_use]
    pub fn new() -> Self {
        Self {
            last_drop_seq: 0,
            max_exchange_secs: 0,
            last_receipt_ns: None,
            last_pass: None,
            drop_pending_lo: None,
            gap_open_lo: None,
        }
    }

    /// The lower bound of a silence ending at or after `prev_ns`: the newest
    /// exchange second seen (lagging trades may still be missing) or the last
    /// receipt second, whichever is earlier, never before 09:15:00. O(1).
    fn gap_lo(&self, prev_eff_ns: i64) -> i64 {
        let prev_secs = prev_eff_ns.div_euclid(NANOS_PER_SEC);
        if self.max_exchange_secs > 0 {
            prev_secs.min(self.max_exchange_secs)
        } else {
            prev_secs
        }
    }

    /// One received tick: taints the stretch a drop or a feed gap could have
    /// touched. `in_session` gates the feed-gap check (09:15-15:40, see
    /// [`in_feed_watch_ns`]). O(1).
    pub fn on_tick(
        &mut self,
        drop_seq: u32,
        exchange_secs: i64,
        receipt_ist_ns: i64,
        in_session: bool,
        taints: &mut TaintRanges,
    ) {
        let receipt_secs = receipt_ist_ns.div_euclid(NANOS_PER_SEC);
        if seq_ahead(drop_seq, self.last_drop_seq) {
            self.note_drop(exchange_secs.max(receipt_secs), taints);
            self.last_drop_seq = drop_seq;
        }
        if let Some(prev) = self.last_receipt_ns
            && in_session
        {
            let prev_eff = prev.max(session_open_ns(receipt_ist_ns));
            if receipt_ist_ns.saturating_sub(prev_eff) >= FEED_GAP_NS {
                // Exchange seconds the gap may have hidden trades in: from the
                // newest second seen before it to this tick's second.
                let lo = self.gap_lo(prev_eff);
                let hi = receipt_secs.max(exchange_secs);
                if self.gap_open_lo.is_some() {
                    // The passes already counted this gap while it lasted.
                    taints.extend(lo, hi, TaintReason::FeedGap);
                } else {
                    taints.add(lo, hi, TaintReason::FeedGap);
                }
            }
        }
        self.gap_open_lo = None;
        if self.last_receipt_ns.is_none_or(|p| receipt_ist_ns > p) {
            self.last_receipt_ns = Some(receipt_ist_ns);
        }
        if exchange_secs > self.max_exchange_secs {
            self.max_exchange_secs = exchange_secs;
        }
    }

    /// A loss the tap counted: taints from the newest receipt seen minus
    /// [`MAX_DROP_LAG_SECS`] (or from 0 with nothing received yet) to
    /// `newest_secs` plus [`FUTURE_TICK_SKEW_SECS`], and leaves the lower
    /// bound for the next pass to extend to its wall clock. O(1).
    fn note_drop(&mut self, newest_secs: i64, taints: &mut TaintRanges) {
        let lo = match self.last_receipt_ns {
            Some(prev) => {
                let by_lag = prev
                    .div_euclid(NANOS_PER_SEC)
                    .saturating_sub(MAX_DROP_LAG_SECS);
                if self.max_exchange_secs > 0 {
                    by_lag.min(self.max_exchange_secs)
                } else {
                    by_lag
                }
            }
            None => 0,
        };
        let hi = newest_secs.saturating_add(FUTURE_TICK_SKEW_SECS).max(lo);
        taints.add(lo, hi, TaintReason::TickDropped);
        // A frame shed before the drain may be newer than the tick that
        // reports it: the next pass bounds the stretch by its wall clock.
        self.drop_pending_lo = Some(self.drop_pending_lo.map_or(lo, |p| p.min(lo)));
    }

    /// The tap's loss total read OUT OF BAND (its low 32 bits, the same count
    /// the next queued tick's `drop_seq` will carry), before ticks still
    /// queued behind the loss are read: a loss ahead of the last one seen
    /// taints now, so no release can run on the seconds it touched. The
    /// queued ticks that later carry the same count, or an older one, taint
    /// nothing more. O(1).
    pub fn on_loss_total(&mut self, seq: u32, taints: &mut TaintRanges) {
        if seq_ahead(seq, self.last_drop_seq) {
            let newest = self
                .last_receipt_ns
                .map_or(0, |r| r.div_euclid(NANOS_PER_SEC))
                .max(self.max_exchange_secs);
            self.note_drop(newest, taints);
            self.last_drop_seq = seq;
        }
    }

    /// The newest receipt (IST ns) of a tick received today, if any. O(1).
    #[must_use]
    pub fn newest_receipt_ns(&self) -> Option<i64> {
        self.last_receipt_ns
    }

    /// One pass: bounds a pending drop by the wall clock, taints an ongoing
    /// feed gap, and compares the wall step with the monotonic step since the
    /// last pass, tainting the wall-clock stretch when they disagree by more
    /// than [`CLOCK_STEP_NS`]. Returns whether a step was seen. O(1).
    pub fn on_pass(&mut self, wall_ist_ns: i64, mono_ns: i64, taints: &mut TaintRanges) -> bool {
        let wall_secs = wall_ist_ns.div_euclid(NANOS_PER_SEC);
        if let Some(lo) = self.drop_pending_lo.take() {
            taints.extend(
                lo,
                wall_secs.saturating_add(FUTURE_TICK_SKEW_SECS).max(lo),
                TaintReason::TickDropped,
            );
        }
        if let Some(prev) = self.last_receipt_ns
            && in_feed_watch_ns(wall_ist_ns)
        {
            let prev_eff = prev.max(session_open_ns(wall_ist_ns));
            if wall_ist_ns.saturating_sub(prev_eff) >= FEED_GAP_NS {
                let lo = self.gap_lo(prev_eff);
                if self.gap_open_lo.is_some() {
                    taints.extend(lo, wall_secs.max(lo), TaintReason::FeedGap);
                } else {
                    taints.add(lo, wall_secs.max(lo), TaintReason::FeedGap);
                    self.gap_open_lo = Some(lo);
                }
            }
        }
        let mut stepped = false;
        if let Some((pw, pm)) = self.last_pass {
            let dw = wall_ist_ns.saturating_sub(pw);
            let dm = mono_ns.saturating_sub(pm);
            if dw.saturating_sub(dm).abs() > CLOCK_STEP_NS {
                let a = pw.div_euclid(NANOS_PER_SEC);
                let b = wall_secs;
                // One second either side: a bar closed at the wrong instant
                // may end up to a second before the step.
                taints.add(
                    a.min(b).saturating_sub(1),
                    a.max(b).saturating_add(1),
                    TaintReason::ClockStep,
                );
                stepped = true;
            }
        }
        self.last_pass = Some((wall_ist_ns, mono_ns));
        stepped
    }

    /// The newest loss count seen (the tap's running total, wrapping). O(1).
    #[must_use]
    pub fn drop_seq(&self) -> u32 {
        self.last_drop_seq
    }

    /// Forgets the previous day's seconds and passes. The drop count is kept:
    /// it is the tap's running total, not a per-day value. O(1).
    pub fn reset(&mut self) {
        let keep = self.last_drop_seq;
        *self = Self::new();
        self.last_drop_seq = keep;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// IST midnight of a trading day (a multiple of 86,400).
    const DAY: i64 = 1_779_321_600;
    const NS: i64 = 1_000_000_000;
    /// 10:00:00 IST, in session.
    const T10: i64 = DAY + 36_000;
    const SKEW: i64 = FUTURE_TICK_SKEW_SECS;

    #[test]
    fn test_taint_ranges_add_merge_and_taint_of() {
        let mut t = TaintRanges::new();
        assert_eq!(t.taint_of(100, 105), None);
        t.add(110, 104, TaintReason::TickDropped);
        assert_eq!(t.taint_of(100, 105), Some(TaintReason::TickDropped));
        assert_eq!(t.taint_of(100, 104), None, "end is exclusive");
        assert_eq!(t.taint_of(110, 115), Some(TaintReason::TickDropped));
        assert_eq!(t.taint_of(111, 120), None);
        // Touching ranges of one reason merge.
        t.add(111, 112, TaintReason::TickDropped);
        assert_eq!(t.ring.len(), 1);
        assert_eq!(t.taint_of(112, 113), Some(TaintReason::TickDropped));
        // A different reason does not merge.
        t.add(113, 113, TaintReason::FeedGap);
        assert_eq!(t.ring.len(), 2);
        assert_eq!(t.taint_of(113, 114), Some(TaintReason::FeedGap));
        assert_eq!(t.added(), [2, 1, 0, 0]);
        // `extend` widens without counting.
        t.extend(113, 130, TaintReason::FeedGap);
        assert_eq!(t.added(), [2, 1, 0, 0]);
        assert_eq!(t.taint_of(129, 130), Some(TaintReason::FeedGap));
        t.clear();
        assert_eq!(t.taint_of(100, 200), None);
        assert_eq!(t.take_fresh(), None, "clear forgets the fresh hull");
    }

    #[test]
    fn test_take_fresh_returns_the_hull_since_the_last_call() {
        let mut t = TaintRanges::new();
        assert_eq!(t.take_fresh(), None);
        t.add(50, 60, TaintReason::FeedGap);
        t.add(10, 20, TaintReason::TickDropped);
        assert_eq!(t.take_fresh(), Some((10, 60, TaintReason::FeedGap)));
        assert_eq!(t.take_fresh(), None);
        t.extend(70, 70, TaintReason::ClockStep);
        assert_eq!(t.take_fresh(), Some((70, 70, TaintReason::ClockStep)));
    }

    #[test]
    fn test_taint_ranges_overflow_folds_into_a_floor_that_only_refuses_more() {
        let mut t = TaintRanges::new();
        for i in 0..(TAINT_RANGES_CAPACITY as i64 + 3) {
            t.add(i * 10, i * 10, TaintReason::ClockStep);
        }
        // The first three ranges were folded into the floor at second 20.
        assert_eq!(
            t.taint_of(5, 6),
            Some(TaintReason::ClockStep),
            "below the floor"
        );
        assert_eq!(t.taint_of(20, 21), Some(TaintReason::ClockStep));
        assert_eq!(
            t.taint_of(21, 22),
            None,
            "above the floor and between ranges"
        );
        assert_eq!(t.taint_of(30, 31), Some(TaintReason::ClockStep));
        let top = (TAINT_RANGES_CAPACITY as i64 + 2) * 10;
        assert_eq!(t.taint_of(top, top + 1), Some(TaintReason::ClockStep));
    }

    /// LF-6b review round 2, finding 4: the feed-gap watch is armed from
    /// 09:15:00 to 15:40:00 (the end of F&O trading), past the 15:30 strategy
    /// close, and a stall at 15:30:20 is tainted by a pass.
    #[test]
    fn test_in_feed_watch_ns_runs_to_the_end_of_fo_trading() {
        assert!(!in_feed_watch_ns((DAY + 33_299) * NS));
        assert!(in_feed_watch_ns((DAY + 33_300) * NS));
        assert!(in_feed_watch_ns((DAY + 55_820) * NS));
        assert!(in_feed_watch_ns((DAY + 56_399) * NS));
        assert!(!in_feed_watch_ns((DAY + 56_400) * NS));
        let mut w = StreamWatch::new();
        let mut t = TaintRanges::new();
        w.on_tick(0, DAY + 55_810, (DAY + 55_810) * NS, true, &mut t);
        let wall = (DAY + 55_820) * NS;
        w.on_pass(wall, wall, &mut t);
        assert_eq!(
            t.taint_of(DAY + 55_815, DAY + 55_816),
            Some(TaintReason::FeedGap)
        );
    }

    #[test]
    fn test_session_open_ns_in_session_ns_and_chain_start_taint() {
        assert_eq!(session_open_ns(T10 * NS), (DAY + 33_300) * NS);
        assert!(in_session_ns(T10 * NS));
        assert!(!in_session_ns((DAY + 33_299) * NS));
        assert!(!in_session_ns((DAY + 55_800) * NS));
        // Before or at the open, or after the close: nothing to taint.
        assert_eq!(chain_start_taint((DAY + 30_000) * NS, 60), None);
        assert_eq!(chain_start_taint((DAY + 33_300) * NS, 60), None);
        assert_eq!(chain_start_taint((DAY + 56_000) * NS, 60), None);
        // A restart at 10:00:00.5: 09:15:00 to the end of the longest bucket.
        assert_eq!(
            chain_start_taint(T10 * NS + 500_000_000, 300),
            Some((DAY + 33_300, T10 + 300))
        );
    }

    /// LF-6b review finding 3: a loss read out of band taints once, from the
    /// newest receipt seen minus the lag bound, before the queued ticks that
    /// carry it (or an older count) arrive; those taint nothing more.
    #[test]
    fn test_stream_watch_on_loss_total_taints_once_and_newest_receipt_ns() {
        let mut w = StreamWatch::new();
        let mut t = TaintRanges::new();
        assert_eq!(w.newest_receipt_ns(), None);
        w.on_tick(0, T10, T10 * NS, false, &mut t);
        assert_eq!(w.newest_receipt_ns(), Some(T10 * NS));
        w.on_loss_total(0, &mut t);
        assert_eq!(
            t.added()[TaintReason::TickDropped as usize],
            0,
            "no new loss"
        );
        w.on_loss_total(2, &mut t);
        assert_eq!(t.added()[TaintReason::TickDropped as usize], 1);
        assert_eq!(w.drop_seq(), 2, "the newest loss count");
        assert_eq!(
            t.taint_of(T10 - MAX_DROP_LAG_SECS, T10 - MAX_DROP_LAG_SECS + 1),
            Some(TaintReason::TickDropped)
        );
        assert_eq!(t.taint_of(T10 + 5, T10 + 6), Some(TaintReason::TickDropped));
        // Queued ticks carrying an older or the same count change nothing.
        w.on_tick(1, T10 + 1, (T10 + 1) * NS, false, &mut t);
        w.on_tick(2, T10 + 1, (T10 + 1) * NS, false, &mut t);
        assert_eq!(t.added()[TaintReason::TickDropped as usize], 1);
        // The pass extends the stretch to its wall clock.
        w.on_pass((T10 + 200) * NS, 0, &mut t);
        assert_eq!(
            t.taint_of(T10 + 200, T10 + 201),
            Some(TaintReason::TickDropped)
        );
        // A later count still taints.
        w.on_tick(3, T10 + 2, (T10 + 2) * NS, false, &mut t);
        assert_eq!(t.added()[TaintReason::TickDropped as usize], 2);
    }

    #[test]
    fn test_stream_watch_drop_taint_reaches_back_by_the_lag_bound_and_on_to_the_wall() {
        let mut w = StreamWatch::new();
        let mut t = TaintRanges::new();
        w.on_tick(0, T10, T10 * NS, false, &mut t);
        w.on_tick(0, T10 + 5, (T10 + 5) * NS, false, &mut t);
        assert_eq!(t.taint_of(T10 - 400, T10 + 400), None);
        // Ticks lost between receipt T10+5 and the next received tick.
        w.on_tick(3, T10 + 7, (T10 + 5) * NS + 500_000_000, false, &mut t);
        let lo = T10 + 5 - MAX_DROP_LAG_SECS;
        assert_eq!(t.taint_of(lo, lo + 1), Some(TaintReason::TickDropped));
        assert_eq!(t.taint_of(lo - 1, lo), None);
        assert_eq!(
            t.taint_of(T10 + 7 + SKEW, T10 + 8 + SKEW),
            Some(TaintReason::TickDropped)
        );
        assert_eq!(t.taint_of(T10 + 8 + SKEW, T10 + 9 + SKEW), None);
        // The next pass bounds it by its wall clock (a shed frame can be newer
        // than the tick that reported it), without counting a second range.
        w.on_pass((T10 + 100) * NS, 0, &mut t);
        assert_eq!(
            t.taint_of(T10 + 100 + SKEW, T10 + 101 + SKEW),
            Some(TaintReason::TickDropped)
        );
        assert_eq!(t.added()[0], 1);
        // With nothing received yet, a drop reaches back to second 0.
        let mut w2 = StreamWatch::new();
        let mut t2 = TaintRanges::new();
        w2.on_tick(1, T10, T10 * NS, false, &mut t2);
        assert_eq!(t2.taint_of(0, 1), Some(TaintReason::TickDropped));
    }

    #[test]
    fn test_stream_watch_feed_gap_only_in_session_and_never_from_a_quiet_pre_open() {
        let mut w = StreamWatch::new();
        let mut t = TaintRanges::new();
        w.on_tick(0, T10, T10 * NS, true, &mut t);
        w.on_tick(0, T10 + 3, (T10 + 3) * NS, true, &mut t);
        assert_eq!(t.taint_of(T10 + 1, T10 + 2), Some(TaintReason::FeedGap));
        let mut w2 = StreamWatch::new();
        let mut t2 = TaintRanges::new();
        w2.on_tick(0, T10, T10 * NS, false, &mut t2);
        w2.on_tick(0, T10 + 3, (T10 + 3) * NS, false, &mut t2);
        assert_eq!(t2.taint_of(T10 - 10, T10 + 10), None);
        // A 1.9 s gap does not taint.
        let mut w3 = StreamWatch::new();
        let mut t3 = TaintRanges::new();
        w3.on_tick(0, T10, T10 * NS, true, &mut t3);
        w3.on_tick(0, T10 + 1, T10 * NS + 1_900_000_000, true, &mut t3);
        assert_eq!(t3.taint_of(T10 - 10, T10 + 10), None);
        // Pre-open ticks at 09:08, then the first session tick at 09:15:00.3:
        // the silence counts from 09:15:00, so the opening second is clean.
        let open = DAY + 33_300;
        let mut w4 = StreamWatch::new();
        let mut t4 = TaintRanges::new();
        w4.on_tick(0, DAY + 32_880, (DAY + 32_880) * NS, false, &mut t4);
        w4.on_tick(0, open, open * NS + 300_000_000, true, &mut t4);
        assert_eq!(t4.taint_of(open, open + 60), None);
    }

    #[test]
    fn test_stream_watch_on_pass_taints_a_gap_while_it_lasts_and_counts_it_once() {
        let mut w = StreamWatch::new();
        let mut t = TaintRanges::new();
        w.on_tick(0, T10, T10 * NS, true, &mut t);
        assert!(!w.on_pass(T10 * NS + 500_000_000, 0, &mut t));
        assert_eq!(t.taint_of(T10, T10 + 1), None);
        // 2.5 s with no tick: the pass taints up to the wall clock.
        w.on_pass(T10 * NS + 2_500_000_000, 2_000_000_000, &mut t);
        assert_eq!(t.taint_of(T10 + 2, T10 + 3), Some(TaintReason::FeedGap));
        w.on_pass((T10 + 4) * NS, 3_500_000_000, &mut t);
        assert_eq!(t.taint_of(T10 + 4, T10 + 5), Some(TaintReason::FeedGap));
        // The tick that ends it widens the same counted range.
        w.on_tick(0, T10 + 5, (T10 + 5) * NS, true, &mut t);
        assert_eq!(t.added()[1], 1, "one gap, counted once");
        assert_eq!(t.taint_of(T10 + 5, T10 + 6), Some(TaintReason::FeedGap));
        // Outside the session the passes never report a gap.
        let mut w2 = StreamWatch::new();
        let mut t2 = TaintRanges::new();
        let eve = DAY + 60_000;
        w2.on_tick(0, eve, eve * NS, false, &mut t2);
        w2.on_pass((eve + 10) * NS, 10 * NS, &mut t2);
        assert_eq!(t2.added(), [0, 0, 0, 0]);
    }

    #[test]
    fn test_stream_watch_on_pass_detects_steps_both_ways() {
        let mut w = StreamWatch::new();
        let mut t = TaintRanges::new();
        assert!(!w.on_pass(1_000_000_000_000, 0, &mut t));
        assert!(!w.on_pass(1_001_000_000_000, 1_000_000_000, &mut t));
        assert!(
            !w.on_pass(1_002_200_000_000, 2_000_000_000, &mut t),
            "200 ms drift"
        );
        // Wall jumps 5 s ahead of the monotonic clock.
        assert!(w.on_pass(1_008_200_000_000, 3_000_000_000, &mut t));
        assert_eq!(t.taint_of(1_004, 1_005), Some(TaintReason::ClockStep));
        // Wall steps back 3 s.
        assert!(w.on_pass(1_006_200_000_000, 4_000_000_000, &mut t));
        w.on_loss_total(3, &mut t);
        w.reset();
        assert_eq!(w.drop_seq(), 3, "the loss count is kept across a day");
        assert!(!w.on_pass(5, 9, &mut t));
    }

    proptest::proptest! {
        /// Against an oracle that knows every dropped tick: no candle that a
        /// dropped tick belonged to reads clean. Receipts rise; each tick's
        /// exchange second lags its receipt by its own 0..200 s, so exchange
        /// seconds are NOT monotone across instruments (finding 5).
        #[test]
        fn proptest_drop_taint_covers_every_dropped_ticks_candle(
            ticks in proptest::collection::vec(
                (0i64..3, 0i64..200, proptest::bool::ANY), 1..300),
            frame in proptest::sample::select(vec![1i64, 5, 60, 300]),
        ) {
            let mut w = StreamWatch::new();
            let mut t = TaintRanges::new();
            let mut receipt = T10;
            let mut dropped = 0u32;
            // (exchange second, was it dropped)
            let mut stream = Vec::new();
            for (step, lag, drop) in ticks {
                receipt += step;
                let secs = receipt - lag;
                stream.push((secs, drop));
                if drop {
                    dropped = dropped.wrapping_add(1);
                    continue;
                }
                w.on_tick(dropped, secs, receipt * NS, false, &mut t);
            }
            // A dropped tick after the last received tick has no successor yet,
            // so nothing has named it: excluded from the check.
            let last_received = stream.iter().rposition(|(_, d)| !d);
            for (i, (s, drop)) in stream.iter().enumerate() {
                if !*drop || last_received.is_none_or(|lr| i > lr) {
                    continue;
                }
                let open = s - s.rem_euclid(frame);
                proptest::prop_assert!(
                    t.taint_of(open, open + frame).is_some(),
                    "dropped tick at {} left its {}s candle clean", s, frame
                );
            }
        }
    }
}

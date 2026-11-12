//! When the paper fill chain may trust a candle (plan item LF-6b).
//!
//! The measured basis: a second's final price arrives by the end of the next
//! second, and the very first second of the session (09:15:00) arrives later.
//! So the chain waits `wait` (1 s) after a candle's end before deciding on
//! it, and `open_wait` for the first candle of each size that opens at
//! 09:15:00. The owner set `open_wait` = 1 s (= `wait`) on 2026-11-12 (Quote
//! 57 addendum: only 0-4 of ~165 stocks per opening second came later; those
//! are skipped as late prices), so by default the clamp below does nothing; it
//! stays for a configured `open_wait` above `wait` (the tests use 2.5 s).
//!
//! The candle book has ONE clock for all its frames, and closes every bar at
//! the clock reading. If the chain simply ran that clock `open_wait` behind
//! the wall, every ordinary bar would wait 2.5 s too. Instead:
//!
//! * [`BookClock::reading`] is `now − wait`, except that for each frame whose
//!   first bar opens at 09:15:00 with end `E_f`, while `now − wait ≥ E_f` and
//!   `now < E_f + open_wait`, the reading is clamped to `E_f − 1 ns`. The
//!   reading never moves backwards.
//! * The book can also close a bar early, when the instrument's next tick
//!   arrives; [`ReleaseQueue`] holds every closed bar until its release
//!   instant [`BookClock::release_ns`]: `end + W(bar)` (`W = open_wait` for a
//!   bar opening at 09:15:00, else `wait`), raised to the release of any
//!   opening bar ending at or before it, and releases due bars in
//!   `(end, frame, instrument)` order. The raise keeps every instrument's
//!   bars of one frame in bucket order across releases, makes the 1 s bar of
//!   an opening candle's last second leave with that candle, and matches the
//!   instant the clamped clock closes the bar, so a held order's fill second
//!   is final exactly when its 1 s bar is released.
//!
//! A tick for a bucket arriving within `W` after the bucket's end therefore
//! still folds into its bar (unless the instrument's own later tick already
//! closed it; that case is a late price, handled by the chain).
//!
//! # Complexity
//!
//! * [`BookClock::reading`], [`BookClock::release_ns`]: O(distinct opening
//!   ends) ≤ O(frames), no allocation.
//! * [`ReleaseQueue::push`]: O(1). [`ReleaseQueue::release_due`]: O(1) while
//!   the earliest release is in the future; otherwise one O(pending) sweep plus
//!   an in-place O(k log k) sort of the k bars released (flagged, not O(1):
//!   at a frame boundary every instrument's bar comes due together). No
//!   allocation: both buffers are sized once at construction.

use tickvault_trading::brutex_host::live_candles::ClosedBar;

/// 09:15:00 IST, seconds of day.
pub const SESSION_OPEN_SOD: i64 = 33_300;
/// 15:30:00 IST, seconds of day: the STRATEGY session close. Only bars ending
/// at or before it reach the engine ([`bar_in_session`]).
pub const SESSION_CLOSE_SOD: i64 = 55_800;
/// 15:40:00 IST, seconds of day: the end of F&O continuous trading (NSE,
/// 2026-08-03), from the one source `MARKET_CLOSE_IST_NANOS`. The fold folds
/// and taps ticks until then, and a held order decided on a bar ending by
/// 15:30 fills on live seconds up to `15:30 + max_fill_seconds`, so the
/// feed-gap watch stays armed until this close, not the strategy one (LF-6b
/// review round 2, finding 4).
pub const FEED_WATCH_CLOSE_SOD: i64 =
    tickvault_common::constants::MARKET_CLOSE_IST_NANOS / NANOS_PER_SEC;
const SECS_PER_DAY: i64 = 86_400;
const NANOS_PER_SEC: i64 = 1_000_000_000;

const _: () = assert!(FEED_WATCH_CLOSE_SOD > SESSION_CLOSE_SOD);

/// The end (seconds of day) of a frame's bar that opens at 09:15:00, or `None`
/// when the frame's grid has no boundary at 09:15:00 (for example 7 s). The
/// end is cut at midnight, as the candle book cuts it. O(1).
#[must_use]
pub fn opening_bar_end_sod(frame_secs: u32) -> Option<i64> {
    if frame_secs == 0 {
        return None;
    }
    let size = i64::from(frame_secs);
    if SESSION_OPEN_SOD % size != 0 {
        return None;
    }
    Some((SESSION_OPEN_SOD + size).min(SECS_PER_DAY))
}

/// Whether a bar opening at `bucket_open_secs` (IST epoch seconds) is a
/// first-of-session bar that gets the opening wait. O(1).
#[must_use]
pub fn is_opening_bar(bucket_open_secs: i64) -> bool {
    bucket_open_secs.rem_euclid(SECS_PER_DAY) == SESSION_OPEN_SOD
}

/// Whether a bar lies inside the trading session: it opens at or after 09:15
/// and ends at or before 15:30. Only such bars reach the strategy. O(1).
#[must_use]
pub fn bar_in_session(bucket_open_secs: i64, bucket_end_secs: i64) -> bool {
    let day = bucket_open_secs.div_euclid(SECS_PER_DAY);
    let open_sod = bucket_open_secs - day * SECS_PER_DAY;
    let end_rel = bucket_end_secs - day * SECS_PER_DAY;
    open_sod >= SESSION_OPEN_SOD && end_rel <= SESSION_CLOSE_SOD && end_rel > open_sod
}

/// The candle book's clock (see the module docs).
#[derive(Debug, Clone)]
pub struct BookClock {
    wait_ns: i64,
    open_wait_ns: i64,
    /// Distinct ends (seconds of day) of the frames' 09:15:00 bars.
    opening_ends_sod: Vec<i64>,
    last: i64,
}

impl BookClock {
    /// Cold; allocates the opening-end list once.
    #[must_use]
    pub fn new(frames_secs: &[u32], wait_ms: u64, open_wait_ms: u64) -> Self {
        let mut opening_ends_sod: Vec<i64> = Vec::with_capacity(frames_secs.len());
        for &f in frames_secs {
            if let Some(e) = opening_bar_end_sod(f)
                && !opening_ends_sod.contains(&e)
            {
                opening_ends_sod.push(e);
            }
        }
        let wait_ns = i64::try_from(wait_ms)
            .unwrap_or(i64::MAX / 2)
            .saturating_mul(1_000_000);
        let open_wait_ns = i64::try_from(open_wait_ms)
            .unwrap_or(i64::MAX / 2)
            .saturating_mul(1_000_000)
            .max(wait_ns);
        Self {
            wait_ns,
            open_wait_ns,
            opening_ends_sod,
            last: i64::MIN,
        }
    }

    /// The wait for one bar: `open_wait` for a bar opening at 09:15:00, else
    /// `wait`. O(1).
    #[must_use]
    pub fn release_wait_ns(&self, bucket_open_secs: i64) -> i64 {
        if is_opening_bar(bucket_open_secs) {
            self.open_wait_ns
        } else {
            self.wait_ns
        }
    }

    /// The instant (IST ns) a bar ending at `bucket_end_secs` and opening at
    /// `bucket_open_secs` may be decided on: `end + W(bar)`, but never before
    /// the release of any opening bar (of any frame) that ends at or before
    /// this bar. So the opening clamp never lets a later bar leave before an
    /// earlier one of the same instrument and frame, the 1 s bar of a
    /// candle's last second always leaves with the candle (the decision
    /// second), and an order inserted at an opening candle's release still
    /// sees its first fill second. It also equals the instant the clamped book
    /// clock closes the bar (see [`Self::reading`]). O(opening ends).
    #[must_use]
    pub fn release_ns(&self, bucket_open_secs: i64, bucket_end_secs: i64) -> i64 {
        let mut release = bucket_end_secs
            .saturating_mul(NANOS_PER_SEC)
            .saturating_add(self.release_wait_ns(bucket_open_secs));
        let day = bucket_end_secs.div_euclid(SECS_PER_DAY);
        let end_sod = bucket_end_secs - day * SECS_PER_DAY;
        for &e_sod in &self.opening_ends_sod {
            if e_sod <= end_sod {
                let opening_release = day
                    .saturating_mul(SECS_PER_DAY)
                    .saturating_add(e_sod)
                    .saturating_mul(NANOS_PER_SEC)
                    .saturating_add(self.open_wait_ns);
                if opening_release > release {
                    release = opening_release;
                }
            }
        }
        release
    }

    /// Whether every bar of the 1 s frame covering `sec` is final at `now`:
    /// the bar of second `sec` would have been released (the same instant the
    /// release queue uses, opening clamp included). O(opening ends).
    #[must_use]
    pub fn second_final(&self, sec: i64, now_ist_ns: i64) -> bool {
        now_ist_ns >= self.release_ns(sec, sec.saturating_add(1))
    }

    /// The book clock reading for wall time `now_ist_ns`. Never moves
    /// backwards. O(opening ends), no allocation.
    pub fn reading(&mut self, now_ist_ns: i64) -> i64 {
        let mut c = now_ist_ns.saturating_sub(self.wait_ns);
        let day = now_ist_ns
            .div_euclid(NANOS_PER_SEC)
            .div_euclid(SECS_PER_DAY);
        for &end_sod in &self.opening_ends_sod {
            let e_ns = day
                .saturating_mul(SECS_PER_DAY)
                .saturating_add(end_sod)
                .saturating_mul(NANOS_PER_SEC);
            if c >= e_ns && now_ist_ns < e_ns.saturating_add(self.open_wait_ns) {
                c = c.min(e_ns.saturating_sub(1));
            }
        }
        if c < self.last {
            c = self.last;
        }
        self.last = c;
        c
    }
}

/// Closed bars waiting for their release instant (see the module docs).
#[derive(Debug)]
pub struct ReleaseQueue {
    pending: Vec<(i64, ClosedBar)>,
    cap: usize,
    /// The earliest release instant pending (a lower bound), `i64::MAX` when
    /// empty.
    next_due: i64,
    overflowed: u64,
}

impl ReleaseQueue {
    /// Cold; the only allocation the queue makes.
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            pending: Vec::with_capacity(cap),
            cap,
            next_due: i64::MAX,
            overflowed: 0,
        }
    }

    /// Queues a bar to release at `release_ns`. `false` when full (counted;
    /// the bar is lost to the strategy and the caller taints its window).
    /// O(1), no allocation.
    pub fn push(&mut self, bar: ClosedBar, release_ns: i64) -> bool {
        if self.pending.len() >= self.cap {
            self.overflowed = self.overflowed.saturating_add(1);
            return false;
        }
        self.pending.push((release_ns, bar));
        if release_ns < self.next_due {
            self.next_due = release_ns;
        }
        true
    }

    /// Bars still waiting.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Whether nothing waits.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Bars refused because the queue was full.
    #[must_use]
    pub fn overflowed(&self) -> u64 {
        self.overflowed
    }

    /// Moves every bar due at `now_ist_ns` into `out` (cleared first, never
    /// grown past its capacity), sorted by `(end, frame, instrument)` so one
    /// instrument's bars of one frame always leave in bucket order. See the
    /// module docs for the cost.
    pub fn release_due(&mut self, now_ist_ns: i64, out: &mut Vec<ClosedBar>) {
        out.clear();
        if now_ist_ns < self.next_due {
            return;
        }
        let mut next_due = i64::MAX;
        let mut i = 0;
        while i < self.pending.len() {
            let (due, bar) = self.pending[i];
            if due <= now_ist_ns && out.len() < out.capacity() {
                out.push(bar);
                self.pending.swap_remove(i);
            } else {
                if due < next_due {
                    next_due = due;
                }
                i += 1;
            }
        }
        self.next_due = next_due;
        out.sort_unstable_by_key(|b| (b.bucket_end_secs, b.frame_index, b.key));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_trading::brutex_host::InstrumentKey;
    use tickvault_trading::brutex_host::live_candles::CloseBy;

    /// IST midnight of a day (a multiple of 86,400).
    const DAY: i64 = 1_779_321_600;
    const NS: i64 = 1_000_000_000;

    fn bar(id: u64, fi: u8, open: i64, end: i64) -> ClosedBar {
        ClosedBar {
            key: InstrumentKey {
                security_id: id,
                exchange_segment: 2,
            },
            frame_index: fi,
            frame_secs: u32::try_from(end - open).unwrap_or(1),
            bucket_open_secs: open,
            bucket_end_secs: end,
            open: 10.0,
            high: 10.0,
            low: 10.0,
            close: 10.0,
            volume: 1,
            tick_count: 1,
            last_tick_exchange_secs: open,
            last_tick_receipt_ns: open * NS,
            close_by: CloseBy::Clock,
            trigger_ns: end * NS,
            close_delay_ns: 0,
        }
    }

    #[test]
    fn test_opening_bar_end_sod_only_for_grids_with_a_0915_boundary() {
        assert_eq!(opening_bar_end_sod(1), Some(33_301));
        assert_eq!(opening_bar_end_sod(60), Some(33_360));
        assert_eq!(opening_bar_end_sod(300), Some(33_600));
        assert_eq!(opening_bar_end_sod(900), Some(34_200));
        assert_eq!(opening_bar_end_sod(7), None);
        assert_eq!(opening_bar_end_sod(0), None);
        // 33,300 = 2^2 · 3^2 · 5^2 · 37; a whole-day frame does not divide it.
        assert_eq!(opening_bar_end_sod(86_400), None);
        assert_eq!(opening_bar_end_sod(33_300), Some(66_600));
    }

    #[test]
    fn test_is_opening_bar_and_bar_in_session() {
        assert!(is_opening_bar(DAY + 33_300));
        assert!(!is_opening_bar(DAY + 33_301));
        assert!(bar_in_session(DAY + 33_300, DAY + 33_360));
        assert!(bar_in_session(DAY + 55_740, DAY + 55_800));
        assert!(
            !bar_in_session(DAY + 33_299, DAY + 33_306),
            "opens before 09:15"
        );
        assert!(
            !bar_in_session(DAY + 55_740, DAY + 55_801),
            "ends after 15:30"
        );
        assert!(!bar_in_session(DAY + 40_000, DAY + 40_000), "empty");
    }

    #[test]
    fn test_book_clock_reading_lags_by_wait_and_clamps_the_opening_bar() {
        let mut c = BookClock::new(&[1, 60], 1_000, 2_500);
        let e1 = (DAY + 33_301) * NS; // the 1 s opening bar's end
        // Before the opening end: plain lag.
        assert_eq!(c.reading(e1), e1 - NS);
        // now = e1 + 1 s: c = e1, which would close the 09:15:00 bar; clamped.
        assert_eq!(c.reading(e1 + NS), e1 - 1);
        assert_eq!(c.reading(e1 + 2 * NS), e1 - 1);
        assert_eq!(c.reading(e1 + 2_499_999_999), e1 - 1);
        // At end + open_wait the clamp lifts.
        assert_eq!(c.reading(e1 + 2_500_000_000), e1 + 1_500_000_000);
        // An ordinary second later: plain lag again.
        let s = (DAY + 33_320) * NS;
        assert_eq!(c.reading(s), s - NS);
        // The 1-min opening bar (end 09:16:00) clamps in its own window.
        let e60 = (DAY + 33_360) * NS;
        assert_eq!(c.reading(e60 + NS), e60 - 1);
        assert_eq!(c.reading(e60 + 2_500_000_000), e60 + 1_500_000_000);
        // Never backwards: an earlier wall reading returns the last value.
        assert_eq!(c.reading(s), e60 + 1_500_000_000);
    }

    #[test]
    fn test_book_clock_release_wait_ns_release_ns_and_second_final() {
        let c = BookClock::new(&[1], 1_000, 2_500);
        assert_eq!(c.release_wait_ns(DAY + 33_300), 2_500_000_000);
        assert_eq!(c.release_wait_ns(DAY + 33_301), NS);
        assert_eq!(
            c.release_ns(DAY + 33_300, DAY + 33_301),
            (DAY + 33_301) * NS + 2_500_000_000
        );
        let s = DAY + 40_000;
        assert!(!c.second_final(s, (s + 2) * NS - 1));
        assert!(c.second_final(s, (s + 2) * NS));
        // open_wait below wait is raised to wait.
        let low = BookClock::new(&[1], 1_000, 10);
        assert_eq!(low.release_wait_ns(DAY + 33_300), NS);
    }

    #[test]
    fn test_release_due_holds_until_due_and_orders_by_end_frame_key() {
        let mut q = ReleaseQueue::with_capacity(8);
        let mut out = Vec::with_capacity(8);
        assert!(q.is_empty());
        q.push(bar(2, 1, 100, 160), 161 * NS);
        q.push(bar(1, 0, 159, 160), 161 * NS);
        q.push(bar(1, 0, 160, 161), 162 * NS);
        q.push(bar(1, 1, 100, 160), 161 * NS);
        assert_eq!(q.len(), 4);
        q.release_due(161 * NS - 1, &mut out);
        assert!(out.is_empty());
        q.release_due(161 * NS, &mut out);
        let order: Vec<(i64, u8, u64)> = out
            .iter()
            .map(|b| (b.bucket_end_secs, b.frame_index, b.key.security_id))
            .collect();
        assert_eq!(order, vec![(160, 0, 1), (160, 1, 1), (160, 1, 2)]);
        assert_eq!(q.len(), 1);
        q.release_due(162 * NS, &mut out);
        assert_eq!(out.len(), 1);
        assert!(q.is_empty());
        // Full queue refuses and counts.
        let mut small = ReleaseQueue::with_capacity(1);
        assert!(small.push(bar(1, 0, 1, 2), 3));
        assert!(!small.push(bar(1, 0, 2, 3), 4));
        assert_eq!(small.overflowed(), 1);
    }

    #[test]
    fn test_release_queue_never_grows_the_output_past_its_capacity() {
        let mut q = ReleaseQueue::with_capacity(4);
        for i in 0..4 {
            q.push(bar(i, 0, 10, 11), 1);
        }
        let mut out = Vec::with_capacity(2);
        q.release_due(5, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out.capacity(), 2);
        q.release_due(5, &mut out);
        assert_eq!(out.len(), 2);
        assert!(q.is_empty());
    }

    /// Findings 3 and 7 (LF-6b review): no later 1 s bar leaves before the
    /// 09:15:00 bar; an opening candle's last second leaves WITH the candle;
    /// its first fill second leaves no earlier; and `second_final` agrees with
    /// the clamped clock.
    #[test]
    fn test_release_ns_never_releases_a_later_bar_before_an_opening_one() {
        let c = BookClock::new(&[1, 60], 1_000, 2_500);
        let open_1s = c.release_ns(DAY + 33_300, DAY + 33_301);
        assert_eq!(open_1s, (DAY + 33_301) * NS + 2_500_000_000);
        // 09:15:01 bar: end 33,302 + 1 s = 33,303.0 would beat 33,303.5.
        assert_eq!(c.release_ns(DAY + 33_301, DAY + 33_302), open_1s);
        assert_eq!(
            c.release_ns(DAY + 33_302, DAY + 33_303),
            (DAY + 33_304) * NS,
            "past the window: plain wait"
        );
        // The 1-min opening candle (end 09:16:00) and its last second.
        let candle = c.release_ns(DAY + 33_300, DAY + 33_360);
        assert_eq!(candle, (DAY + 33_360) * NS + 2_500_000_000);
        assert_eq!(c.release_ns(DAY + 33_359, DAY + 33_360), candle);
        // Its first fill second (09:16:00) leaves no earlier than the candle.
        assert_eq!(c.release_ns(DAY + 33_360, DAY + 33_361), candle);
        // second_final matches the clamped clock: the 09:15:59 second is final
        // only when the clock may close it.
        let mut clock = BookClock::new(&[1, 60], 1_000, 2_500);
        assert!(!c.second_final(DAY + 33_359, candle - 1));
        assert!(clock.reading(candle - 1) < (DAY + 33_360) * NS);
        assert!(c.second_final(DAY + 33_359, candle));
        assert!(clock.reading(candle) >= (DAY + 33_360) * NS);
        // An ordinary second is untouched.
        assert!(c.second_final(DAY + 40_000, (DAY + 40_002) * NS));
    }

    proptest::proptest! {
        /// Against a brute-force reference: every bar is released exactly
        /// once, never before `end + W(bar)`, in (end, frame, key) order per
        /// release call, and 09:15:00 bars wait `open_wait`.
        #[test]
        fn proptest_release_each_bar_once_never_early(
            bars in proptest::collection::vec((0u64..6, 0u8..3, 0i64..120, 1i64..4), 1..60),
            steps in proptest::collection::vec(1i64..900_000_000, 1..200),
        ) {
            let clock = BookClock::new(&[1, 60], 1_000, 2_500);
            let mut q = ReleaseQueue::with_capacity(bars.len());
            let mut out = Vec::with_capacity(bars.len());
            let mut want = Vec::new();
            for (id, fi, off, len) in &bars {
                let open = DAY + 33_300 + off;
                let end = open + len;
                let b = bar(*id, *fi, open, end);
                let rel = clock.release_ns(open, end);
                // Reference: end + W, raised to every opening release (1 s
                // frame ends 09:15:01, 60 s frame ends 09:16:00) at or before.
                let w = if is_opening_bar(open) { 2_500_000_000 } else { NS };
                let mut want_rel = end * NS + w;
                for e in [DAY + 33_301, DAY + 33_360] {
                    if e <= end {
                        want_rel = want_rel.max(e * NS + 2_500_000_000);
                    }
                }
                proptest::prop_assert_eq!(rel, want_rel);
                proptest::prop_assert!(q.push(b, rel));
                want.push((rel, b));
            }
            let mut now = (DAY + 33_300) * NS;
            let mut got = 0usize;
            for s in steps {
                now += s;
                q.release_due(now, &mut out);
                for w in out.windows(2) {
                    proptest::prop_assert!(
                        (w[0].bucket_end_secs, w[0].frame_index, w[0].key)
                            <= (w[1].bucket_end_secs, w[1].frame_index, w[1].key)
                    );
                }
                for b in &out {
                    let rel = clock.release_ns(b.bucket_open_secs, b.bucket_end_secs);
                    proptest::prop_assert!(rel <= now, "released early");
                }
                got += out.len();
            }
            q.release_due(i64::MAX, &mut out);
            got += out.len();
            proptest::prop_assert_eq!(got, want.len(), "each bar exactly once");
            proptest::prop_assert!(q.is_empty());
        }

        /// The clock never goes backwards and never closes an opening bar
        /// before `end + open_wait`, nor any other bar before `end + wait`.
        #[test]
        fn proptest_book_clock_monotone_and_never_early(
            steps in proptest::collection::vec(1i64..700_000_000, 1..400),
            start_off in 0i64..5,
        ) {
            let mut clock = BookClock::new(&[1, 5, 60], 1_000, 2_500);
            let mut now = (DAY + 33_300 + start_off) * NS;
            let mut last = i64::MIN;
            for s in steps {
                now += s;
                let c = clock.reading(now);
                proptest::prop_assert!(c >= last);
                last = c;
                // A bar of end E is closed iff c >= E. Opening bars (1 s, 5 s,
                // 60 s at 09:15:00) must not close before E + 2.5 s.
                for e in [33_301i64, 33_305, 33_360] {
                    let e_ns = (DAY + e) * NS;
                    if c >= e_ns {
                        proptest::prop_assert!(now >= e_ns + 2_500_000_000);
                    }
                }
                proptest::prop_assert!(c <= now - NS, "never less than the plain wait");
            }
        }

        /// Findings 3 and 7: on the real grids (1 s, 5 s, 60 s) around the
        /// open, the clamped clock closes a bar exactly at its release instant,
        /// and across release calls each (instrument, frame) leaves in bucket
        /// order.
        #[test]
        fn proptest_release_order_per_key_frame_and_clock_agree(
            steps in proptest::collection::vec(1i64..700_000_000, 1..300),
            keys in 1u64..4,
        ) {
            let frames = [1u32, 5, 60];
            let mut clock = BookClock::new(&frames, 1_000, 2_500);
            let mut q = ReleaseQueue::with_capacity(4_096);
            let mut out = Vec::with_capacity(4_096);
            for (fi, &f) in frames.iter().enumerate() {
                let f = i64::from(f);
                let mut open = DAY + 33_300;
                while open < DAY + 33_300 + 130 {
                    for k in 0..keys {
                        let b = bar(k, u8::try_from(fi).unwrap_or(0), open, open + f);
                        proptest::prop_assert!(q.push(b, clock.release_ns(open, open + f)));
                    }
                    open += f;
                }
            }
            let mut last_end: std::collections::HashMap<(u64, u8), i64> =
                std::collections::HashMap::new();
            let mut now = (DAY + 33_300) * NS;
            for s in steps {
                now += s;
                let c = clock.reading(now);
                for &f in &frames {
                    let f = i64::from(f);
                    let mut open = DAY + 33_300;
                    while open < DAY + 33_300 + 130 {
                        let closed = c >= (open + f) * NS;
                        let released = now >= clock.release_ns(open, open + f);
                        proptest::prop_assert_eq!(closed, released, "bar {}+{}", open, f);
                        open += f;
                    }
                }
                q.release_due(now, &mut out);
                for b in &out {
                    let k = (b.key.security_id, b.frame_index);
                    if let Some(prev) = last_end.get(&k) {
                        proptest::prop_assert!(*prev < b.bucket_end_secs, "out of order");
                    }
                    last_end.insert(k, b.bucket_end_secs);
                }
            }
        }
    }
}

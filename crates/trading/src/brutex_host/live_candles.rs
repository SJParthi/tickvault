//! In-memory strategy candles of any whole-second size, built from live ticks.
//!
//! Owner rule: "Never let a tick stamped at/after the boundary into the closing
//! candle". Exchange times are whole IST seconds since the Unix epoch (the same
//! field as [`TickInput::exchange_time_ist_secs`]), so IST midnight is an exact
//! multiple of 86,400. Every frame is aligned to IST midnight, whatever its
//! size: with `day = floor(secs / 86_400)` and `sod = secs mod 86_400`, a
//! candle of size `s` seconds has index `idx = floor(sod / s)` within its day,
//! opens at `day * 86_400 + idx * s` and ends at
//! `min(open + s, (day + 1) * 86_400)`, so the last candle of a day whose size
//! does not divide 86,400 is cut short at midnight and the next day starts a
//! fresh grid. A 5 s candle opened at 09:15:00 holds ticks stamped 09:15:00 to
//! 09:15:04 and closes at the boundary 09:15:05.000; a 7 s candle on any day
//! holds 09:14:59 to 09:15:05 (sod 33,299 = 4,757 × 7). A size of 86,400 s or
//! more is one candle per day. Buckets are ordered by the key
//! `day * ceil(86_400 / s) + idx`, which rises with time across midnight.
//!
//! # Who closes a candle
//!
//! Whichever comes first, and the closing event is recorded on the bar:
//! * a tick of a LATER bucket (`close_by = Tick`, `trigger_ns` = that tick's
//!   receipt), which closes the open bar BEFORE it opens the next one; or
//! * the clock ([`LiveCandleBook::on_clock`], `close_by = Clock`,
//!   `trigger_ns` = the clock reading) once the bar's end boundary is at or
//!   before it.
//!
//! A tick whose bucket the clock has already passed, or that is older than
//! the bucket already open (or already closed) for its instrument and frame,
//! is LATE: it changes no bar and is counted per frame. Empty buckets emit
//! nothing; a jump over them is counted (`skipped_empty`).
//!
//! Inside the open bucket, a tick that is OLDER than the bar's last tick (its
//! exchange second is below the bar's last one, or its cumulative volume is
//! below the instrument's high mark) can only widen the high and low and add
//! to the tick count and volume; it never moves the close, the last exchange
//! second or the last receipt (an older packet must not overwrite the close).
//!
//! # Future-stamped ticks
//!
//! A tick whose exchange second is more than [`FUTURE_TICK_SKEW_SECS`] past
//! the reference second, `max(last clock reading's second, its receipt's
//! second)`, is refused ([`TickVerdict::RefusedFuture`], counted
//! `refused_future`): it opens, closes and changes nothing, and does not move
//! the volume baseline. Without this one bad stamp would close the real bar
//! early, open a far-future bucket and make every later real tick of that
//! instrument late until exchange time caught up.
//!
//! # Time scales
//!
//! `receipt_ns` and the clock reading are both IST epoch nanoseconds (UTC
//! epoch ns plus [`IST_OFFSET_NS`]); a caller with a UTC receipt adds the
//! offset. `close_delay_ns = trigger_ns - end boundary` is recorded as is: it
//! is negative only when the clock that stamped the trigger runs behind the
//! exchange.
//!
//! # Volume
//!
//! Per instrument, the bar volume is the rise of the vendor's cumulative day
//! volume since the previous accepted tick. The first tick of an instrument
//! contributes 0 (counted `baseline`). The baseline only moves UP: a
//! cumulative that falls (an out-of-order packet) adds 0 and is counted
//! `volume_down`, so a later in-order tick is not counted twice. Build a fresh
//! book each trading day: a vendor counter that restarts reads as
//! `volume_down` until it passes the old high.
//!
//! # Complexity (honest)
//!
//! * [`LiveCandleBook::configure`]: cold, allocates once, O(keys × frames).
//! * [`LiveCandleBook::on_tick`]: one hash probe on the composite key, then
//!   O(frames) fixed work; no allocation (DHAT
//!   `tests/dhat_live_candles.rs`).
//! * [`LiveCandleBook::on_clock`]: O(frames) while no frame's due bound has
//!   passed. The bound is a per-frame lower bound on the earliest open end; a
//!   tick that closes a bar never RAISES it (only a clock scan does), so once
//!   the clock passes a frame's bound the call does one **O(open bars of that
//!   frame)** scan for that frame, up to the number of instruments, once per
//!   boundary crossed, even when ticks already closed every bar due at it.
//!   The scan resets the bound to the earliest end still open. No allocation
//!   either way (the open lists are pre-sized).

use std::collections::HashMap;

use super::InstrumentKey;
use super::engine::{BarInput, TickInput};
use crate::candles::multi_tf_aggregator::FOLD_FUTURE_TRADE_TIME_SKEW_SECS;

/// IST is UTC + 5:30, in nanoseconds.
pub const IST_OFFSET_NS: i64 = 19_800 * NANOS_PER_SEC;

const NANOS_PER_SEC: i64 = 1_000_000_000;
/// Most frames one book holds: `BarInput::timeframe` is a `u8` index.
pub const MAX_FRAMES: usize = 256;
/// Sentinel: no bucket opened yet for a slot.
const NO_BUCKET: i64 = i64::MIN;
/// Seconds in a day; IST midnight is a multiple of this in IST epoch seconds.
const SECS_PER_DAY: i64 = 86_400;
/// Most seconds a tick's exchange stamp may run ahead of the reference second
/// (`max(last clock second, receipt second)`) before it is refused. It is the
/// live candle fold's own margin, [`FOLD_FUTURE_TRADE_TIME_SKEW_SECS`] (60 s):
/// the fold treats a box clock up to 10 s behind the exchange as honest, so a
/// 5 s margin (the spot store's) would refuse every real tick while the clock
/// lags (review finding, 2026-11-12). Unlike the spot store, which keeps the
/// tick and caps its stamp, this book refuses it and counts it.
pub const FUTURE_TICK_SKEW_SECS: i64 = FOLD_FUTURE_TRADE_TIME_SKEW_SECS;

/// Why [`LiveCandleBook::configure`] refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("no instruments given")]
    EmptyKeys,
    #[error("no candle sizes given")]
    EmptyFrames,
    #[error("a candle size of 0 seconds")]
    ZeroFrame,
    #[error("instrument {0:?} listed twice")]
    DuplicateKey(InstrumentKey),
    #[error("candle size {0} s listed twice")]
    DuplicateFrame(u32),
    #[error("{0} candle sizes, at most {MAX_FRAMES}")]
    TooManyFrames(usize),
    #[error("{0} instruments, more than a u32 index holds")]
    TooManyKeys(usize),
}

/// Which event closed a bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseBy {
    Clock,
    Tick,
}

/// One closed candle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClosedBar {
    pub key: InstrumentKey,
    /// Index into the `frames_secs` given to `configure`.
    pub frame_index: u8,
    pub frame_secs: u32,
    /// First IST second the bar covers.
    pub bucket_open_secs: i64,
    /// The boundary: first IST second the bar does NOT cover.
    pub bucket_end_secs: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: u64,
    pub tick_count: u32,
    pub last_tick_exchange_secs: i64,
    pub last_tick_receipt_ns: i64,
    pub close_by: CloseBy,
    /// Receipt of the closing tick, or the clock reading (IST epoch ns).
    pub trigger_ns: i64,
    /// `trigger_ns - bucket_end_secs * 1e9`; not clamped.
    pub close_delay_ns: i64,
}

impl ClosedBar {
    /// The strategy engine's input; `timeframe` is the frame index, so pass
    /// `frames_secs` to `configure` in the recipe's `timeframes` order.
    #[must_use]
    pub fn to_bar_input(&self) -> BarInput {
        BarInput {
            key: self.key,
            timeframe: self.frame_index,
            open_ist_secs: self.bucket_open_secs,
            open: self.open,
            high: self.high,
            low: self.low,
            close: self.close,
            volume: self.volume,
        }
    }
}

/// A caller-owned, fixed-capacity buffer of closed bars: no allocation after
/// construction. A push past capacity is refused and counted; the first bars
/// are kept.
#[derive(Debug)]
pub struct ClosedBars {
    buf: Vec<ClosedBar>,
    cap: usize,
    overflowed: u64,
}

impl ClosedBars {
    /// Allocates once (cold).
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
            cap,
            overflowed: 0,
        }
    }
    /// O(1), no allocation. False when full (counted).
    pub fn push(&mut self, bar: ClosedBar) -> bool {
        if self.buf.len() >= self.cap {
            self.overflowed = self.overflowed.saturating_add(1);
            return false;
        }
        self.buf.push(bar);
        true
    }
    #[must_use]
    pub fn bars(&self) -> &[ClosedBar] {
        &self.buf
    }
    #[must_use]
    pub fn overflowed(&self) -> u64 {
        self.overflowed
    }
    /// Keeps capacity; resets the overflow count.
    pub fn clear(&mut self) {
        self.buf.clear();
        self.overflowed = 0;
    }
}

/// What happened to one tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickVerdict {
    /// Passed the checks; `late_frames` frames counted it late.
    Accepted {
        late_frames: u16,
    },
    RefusedUnknownKey,
    RefusedPrice,
    /// Exchange second more than [`FUTURE_TICK_SKEW_SECS`] past the reference
    /// second; changed nothing.
    RefusedFuture,
}

/// Counters the caller can read at any time.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Counters {
    pub accepted: u64,
    /// Late ticks, per frame index.
    pub late: Vec<u64>,
    pub refused_unknown: u64,
    pub refused_price: u64,
    /// Ticks stamped too far in the future (see [`FUTURE_TICK_SKEW_SECS`]).
    pub refused_future: u64,
    pub baseline: u64,
    pub volume_down: u64,
    pub closed_by_clock: u64,
    pub closed_by_tick: u64,
    /// Bars the caller's buffer had no room for.
    pub overflowed: u64,
    /// Empty buckets jumped over by a tick.
    pub skipped_empty: u64,
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    /// Last bucket opened (open or closed since); `NO_BUCKET` before any.
    bucket: i64,
    is_open: bool,
    /// Position in the frame's open list while open.
    open_pos: u32,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: u64,
    tick_count: u32,
    last_exchange_secs: i64,
    last_receipt_ns: i64,
}

impl Slot {
    const EMPTY: Self = Self {
        bucket: NO_BUCKET,
        is_open: false,
        open_pos: 0,
        open: 0.0,
        high: 0.0,
        low: 0.0,
        close: 0.0,
        volume: 0,
        tick_count: 0,
        last_exchange_secs: 0,
        last_receipt_ns: 0,
    };
}

#[derive(Debug)]
struct Frame {
    secs: u32,
    /// Size in seconds.
    size: i64,
    /// Buckets per IST day: `ceil(86_400 / size)`.
    per_day: i64,
    /// Buckets below this the clock has passed (the bucket key of the clock's
    /// second).
    min_live_bucket: i64,
    /// A lower bound on the earliest end (ns) of any open bar.
    next_due_ns: i64,
    /// Key indices of the open bars, pre-sized to the key count.
    open_list: Vec<u32>,
}

impl Frame {
    /// The bucket key of an IST second: `day * per_day + idx`, rising with
    /// time across midnight. O(1).
    #[inline]
    fn bucket_of(&self, secs: i64) -> i64 {
        let day = secs.div_euclid(SECS_PER_DAY);
        let idx = secs.rem_euclid(SECS_PER_DAY) / self.size;
        day.saturating_mul(self.per_day).saturating_add(idx)
    }

    /// `(open, end)` IST seconds of a bucket key; the end is cut at the next
    /// IST midnight. O(1).
    #[inline]
    fn bounds(&self, bucket: i64) -> (i64, i64) {
        let day = bucket.div_euclid(self.per_day);
        let idx = bucket.rem_euclid(self.per_day);
        let midnight = day.saturating_mul(SECS_PER_DAY);
        let open = midnight.saturating_add(idx.saturating_mul(self.size));
        let end = open
            .saturating_add(self.size)
            .min(midnight.saturating_add(SECS_PER_DAY));
        (open, end)
    }

    /// The end boundary of a bucket, in ns.
    #[inline]
    fn end_ns(&self, bucket: i64) -> i64 {
        self.bounds(bucket).1.saturating_mul(NANOS_PER_SEC)
    }
}

/// The candle book: every (instrument, frame) pair has one slot.
#[derive(Debug)]
pub struct LiveCandleBook {
    keys: Vec<InstrumentKey>,
    index: HashMap<InstrumentKey, u32, ahash::RandomState>,
    frames: Vec<Frame>,
    /// Dense, `key_index * frames.len() + frame_index`.
    slots: Vec<Slot>,
    /// Per key: highest cumulative volume seen; `None` before the first tick.
    last_cum: Vec<Option<u64>>,
    last_clock_ns: i64,
    counters: Counters,
}

impl LiveCandleBook {
    // O(1) EXEMPT: begin — cold path: `configure` runs once before the session; it is the only place the book allocates.
    /// Sizes everything. Cold; the only allocation the book ever makes.
    ///
    /// # Errors
    /// Empty inputs, a zero size, a duplicate key or size, too many of either.
    pub fn configure(keys: &[InstrumentKey], frames_secs: &[u32]) -> Result<Self, ConfigError> {
        if keys.is_empty() {
            return Err(ConfigError::EmptyKeys);
        }
        if frames_secs.is_empty() {
            return Err(ConfigError::EmptyFrames);
        }
        if frames_secs.len() > MAX_FRAMES {
            return Err(ConfigError::TooManyFrames(frames_secs.len()));
        }
        let key_count =
            u32::try_from(keys.len()).map_err(|_| ConfigError::TooManyKeys(keys.len()))?;
        let mut frames = Vec::with_capacity(frames_secs.len());
        for (i, &secs) in frames_secs.iter().enumerate() {
            if secs == 0 {
                return Err(ConfigError::ZeroFrame);
            }
            if frames_secs[..i].contains(&secs) {
                return Err(ConfigError::DuplicateFrame(secs));
            }
            let size = i64::from(secs);
            frames.push(Frame {
                secs,
                size,
                per_day: i64::from(86_400u32.div_ceil(secs)),
                min_live_bucket: NO_BUCKET,
                next_due_ns: i64::MAX,
                open_list: Vec::with_capacity(keys.len()),
            });
        }
        let mut index = HashMap::with_capacity_and_hasher(keys.len(), ahash::RandomState::new());
        for (i, &key) in keys.iter().enumerate() {
            // `i < key_count`, which fits a u32.
            let i = u32::try_from(i).unwrap_or(key_count);
            if index.insert(key, i).is_some() {
                return Err(ConfigError::DuplicateKey(key));
            }
        }
        let late = core::iter::repeat_n(0, frames.len()).collect();
        Ok(Self {
            keys: keys.to_vec(),
            index,
            slots: core::iter::repeat_n(Slot::EMPTY, keys.len() * frames.len()).collect(),
            frames,
            last_cum: core::iter::repeat_n(None, keys.len()).collect(),
            last_clock_ns: i64::MIN,
            counters: Counters {
                late,
                ..Counters::default()
            },
        })
    }
    // O(1) EXEMPT: end

    #[must_use]
    pub fn counters(&self) -> &Counters {
        &self.counters
    }

    #[must_use]
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// Bars currently open across all frames.
    #[must_use]
    pub fn open_bars(&self) -> usize {
        self.frames.iter().map(|f| f.open_list.len()).sum()
    }

    /// [`Self::on_tick`] for a [`TickInput`].
    pub fn on_tick_input(&mut self, tick: &TickInput, out: &mut ClosedBars) -> TickVerdict {
        self.on_tick(
            tick.key,
            tick.exchange_time_ist_secs,
            tick.receipt_ns,
            tick.ltp,
            tick.cumulative_volume,
            out,
        )
    }

    /// One live tick. O(1) hash probe plus O(frames); no allocation.
    pub fn on_tick(
        &mut self,
        key: InstrumentKey,
        exchange_secs: u32,
        receipt_ns: i64,
        ltp: f64,
        cum_volume: u64,
        out: &mut ClosedBars,
    ) -> TickVerdict {
        let Some(&ki) = self.index.get(&key) else {
            self.counters.refused_unknown = self.counters.refused_unknown.saturating_add(1);
            return TickVerdict::RefusedUnknownKey;
        };
        if !ltp.is_finite() || ltp <= 0.0 {
            self.counters.refused_price = self.counters.refused_price.saturating_add(1);
            return TickVerdict::RefusedPrice;
        }
        let secs = i64::from(exchange_secs);
        let reference_secs = self
            .last_clock_ns
            .div_euclid(NANOS_PER_SEC)
            .max(receipt_ns.div_euclid(NANOS_PER_SEC));
        // With no clock reading yet and no receipt (0 or less: a replayed frame
        // with no stamp), there is no reference to judge by: accept, as the
        // candle fold and the spot store do.
        let has_reference = self.last_clock_ns != i64::MIN || receipt_ns > 0;
        if has_reference && secs > reference_secs.saturating_add(FUTURE_TICK_SKEW_SECS) {
            // Before any bar or the volume baseline is touched.
            self.counters.refused_future = self.counters.refused_future.saturating_add(1);
            return TickVerdict::RefusedFuture;
        }
        self.counters.accepted = self.counters.accepted.saturating_add(1);
        let ki = ki as usize;
        let mut volume_down = false;
        let delta = match self.last_cum[ki] {
            None => {
                self.counters.baseline = self.counters.baseline.saturating_add(1);
                self.last_cum[ki] = Some(cum_volume);
                0
            }
            Some(prev) if cum_volume < prev => {
                self.counters.volume_down = self.counters.volume_down.saturating_add(1);
                volume_down = true;
                0
            }
            Some(prev) => {
                self.last_cum[ki] = Some(cum_volume);
                cum_volume - prev
            }
        };
        let nf = self.frames.len();
        let mut late_frames: u16 = 0;
        for fi in 0..nf {
            let frame = &self.frames[fi];
            let bucket = frame.bucket_of(secs);
            let si = ki * nf + fi;
            let slot = self.slots[si];
            if bucket < frame.min_live_bucket
                || bucket < slot.bucket
                || (bucket == slot.bucket && !slot.is_open)
            {
                self.counters.late[fi] = self.counters.late[fi].saturating_add(1);
                late_frames = late_frames.saturating_add(1);
                continue;
            }
            if bucket == slot.bucket {
                let s = &mut self.slots[si];
                if ltp > s.high {
                    s.high = ltp;
                }
                if ltp < s.low {
                    s.low = ltp;
                }
                s.volume = s.volume.saturating_add(delta);
                s.tick_count = s.tick_count.saturating_add(1);
                // An older packet (older exchange second, or a cumulative
                // below the high mark) never overwrites the close.
                if !volume_down && secs >= s.last_exchange_secs {
                    s.close = ltp;
                    s.last_exchange_secs = secs;
                    s.last_receipt_ns = receipt_ns;
                }
                continue;
            }
            // A later bucket: close the open bar FIRST, then open this one.
            if slot.bucket != NO_BUCKET {
                let gap = bucket - slot.bucket - 1;
                self.counters.skipped_empty = self
                    .counters
                    .skipped_empty
                    .saturating_add(u64::try_from(gap).unwrap_or(0));
            }
            if slot.is_open {
                self.close_slot(ki, fi, CloseBy::Tick, receipt_ns, out);
                self.unlink_open(fi, si);
            }
            let frame = &mut self.frames[fi];
            let end_ns = frame.end_ns(bucket);
            let pos = u32::try_from(frame.open_list.len()).unwrap_or(u32::MAX);
            frame.open_list.push(ki as u32);
            if end_ns < frame.next_due_ns {
                frame.next_due_ns = end_ns;
            }
            self.slots[si] = Slot {
                bucket,
                is_open: true,
                open_pos: pos,
                open: ltp,
                high: ltp,
                low: ltp,
                close: ltp,
                volume: delta,
                tick_count: 1,
                last_exchange_secs: secs,
                last_receipt_ns: receipt_ns,
            };
        }
        TickVerdict::Accepted { late_frames }
    }

    /// The synced wall clock, IST epoch ns. Closes every open bar whose end
    /// boundary is at or before `now_ns`. A reading at or before an earlier
    /// one does nothing. O(frames) while no frame's due bound has passed;
    /// once a frame's bound passes, one O(open bars of that frame) scan for
    /// it (the bound is never raised by a tick close, so the scan can run
    /// and find nothing due). No allocation.
    pub fn on_clock(&mut self, now_ns: i64, out: &mut ClosedBars) {
        if now_ns <= self.last_clock_ns {
            return;
        }
        self.last_clock_ns = now_ns;
        let now_secs = now_ns.div_euclid(NANOS_PER_SEC);
        let nf = self.frames.len();
        for fi in 0..nf {
            let live = self.frames[fi].bucket_of(now_secs);
            self.frames[fi].min_live_bucket = live;
            if self.frames[fi].next_due_ns > now_ns {
                continue;
            }
            let mut next_due = i64::MAX;
            let mut i = 0;
            while i < self.frames[fi].open_list.len() {
                let ki = self.frames[fi].open_list[i] as usize;
                let si = ki * nf + fi;
                let end_ns = self.frames[fi].end_ns(self.slots[si].bucket);
                if end_ns <= now_ns {
                    self.close_slot(ki, fi, CloseBy::Clock, now_ns, out);
                    self.unlink_open(fi, si);
                    // The last entry moved into `i`; look at it next.
                } else {
                    if end_ns < next_due {
                        next_due = end_ns;
                    }
                    i += 1;
                }
            }
            self.frames[fi].next_due_ns = next_due;
        }
    }

    /// Pushes the open bar of `(ki, fi)` to `out` and marks it closed.
    fn close_slot(
        &mut self,
        ki: usize,
        fi: usize,
        close_by: CloseBy,
        trigger_ns: i64,
        out: &mut ClosedBars,
    ) {
        let frame = &self.frames[fi];
        let si = ki * self.frames.len() + fi;
        let s = &mut self.slots[si];
        s.is_open = false;
        let (open_secs, end_secs) = frame.bounds(s.bucket);
        let bar = ClosedBar {
            key: self.keys[ki],
            frame_index: u8::try_from(fi).unwrap_or(u8::MAX),
            frame_secs: frame.secs,
            bucket_open_secs: open_secs,
            bucket_end_secs: end_secs,
            open: s.open,
            high: s.high,
            low: s.low,
            close: s.close,
            volume: s.volume,
            tick_count: s.tick_count,
            last_tick_exchange_secs: s.last_exchange_secs,
            last_tick_receipt_ns: s.last_receipt_ns,
            close_by,
            trigger_ns,
            close_delay_ns: trigger_ns.saturating_sub(end_secs.saturating_mul(NANOS_PER_SEC)),
        };
        match close_by {
            CloseBy::Clock => {
                self.counters.closed_by_clock = self.counters.closed_by_clock.saturating_add(1);
            }
            CloseBy::Tick => {
                self.counters.closed_by_tick = self.counters.closed_by_tick.saturating_add(1);
            }
        }
        if !out.push(bar) {
            self.counters.overflowed = self.counters.overflowed.saturating_add(1);
        }
    }

    /// Removes slot `si` from frame `fi`'s open list (swap-remove, O(1)).
    fn unlink_open(&mut self, fi: usize, si: usize) {
        let pos = self.slots[si].open_pos as usize;
        let list = &mut self.frames[fi].open_list;
        if pos >= list.len() {
            return;
        }
        list.swap_remove(pos);
        if let Some(&moved) = list.get(pos) {
            let nf = self.frames.len();
            self.slots[moved as usize * nf + fi].open_pos = pos as u32;
        }
    }
}

#[cfg(test)]
#[path = "live_candles_tests.rs"]
mod tests;

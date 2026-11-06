//! Cache warm-up for one instrument's candle state (plan item PF-7,
//! 2026-11-06).
//!
//! # Why
//!
//! At 25,000 instruments the fold's slot table is about 72 MB, twice the L3 of
//! the r8g host, so the slot of an instrument that last traded a few seconds
//! ago is usually in memory, not in cache. The fold then walks the timeframes
//! one by one, and each open bucket (160 bytes, three cache lines) is a fresh
//! miss that the core only meets when it reaches that timeframe.
//!
//! Right after the index lookup, `AggregatorCell::warm_for_tick` reads one
//! field on each cache line of every open bucket, and of the sealed bucket and
//! the carry of each timeframe the tick will roll, and folds the bits into one
//! word that goes to [`std::hint::black_box`]. Those loads do not depend on
//! each other, so the core issues them together and the misses overlap; the
//! fold that follows finds the lines already in cache or on their way.
//!
//! # Why loads and not a prefetch instruction
//!
//! A prefetch instruction would do the same with less work, but this crate
//! forbids `unsafe` (`deny(unsafe_code)`), the x86-64 `_mm_prefetch` needs an
//! `unsafe` block to call, and the aarch64 prefetch intrinsic (the production
//! host) is still unstable in Rust 1.95. Plain loads are safe, run on both
//! targets, and measured within the noise of a whole-slot `prefetcht0` on the
//! x86 dev container (`benches/candle_fold_cold.rs`).
//!
//! # Correctness
//!
//! The warm-up takes `&self` (no interior mutability anywhere in the cell) and
//! its result is discarded, so it changes no value the fold computes. The
//! proptest `proptest_warm_up_never_changes_a_bar` (in
//! `multi_tf_aggregator_pf7_tests.rs`) folds random streams with and without
//! it and compares every emitted bar, every per-tick outcome and the whole
//! slot state.

use std::mem::offset_of;

use crate::candles::LiveCandleState;

/// Bytes in one cache line on both targets we build for.
const CACHE_LINE_BYTES: usize = 64;

/// Start offset of every field of [`LiveCandleState`]. `touch_state` names
/// every field (no `..`), so adding a field fails to compile there; add its
/// offset here in the same change.
const ALL_FIELD_OFFSETS: [usize; 23] = [
    offset_of!(LiveCandleState, bucket_start_ist_secs),
    offset_of!(LiveCandleState, open),
    offset_of!(LiveCandleState, high),
    offset_of!(LiveCandleState, low),
    offset_of!(LiveCandleState, close),
    offset_of!(LiveCandleState, volume),
    offset_of!(LiveCandleState, net_volume_signed),
    offset_of!(LiveCandleState, bucket_start_cumulative),
    offset_of!(LiveCandleState, oi),
    offset_of!(LiveCandleState, tick_count),
    offset_of!(LiveCandleState, close_ts_ist_secs),
    offset_of!(LiveCandleState, open_ts_ist_secs),
    offset_of!(LiveCandleState, prev_day_close),
    offset_of!(LiveCandleState, close_pct_from_prev_day),
    offset_of!(LiveCandleState, bucket_open_prev_close),
    offset_of!(LiveCandleState, total_buy_qty),
    offset_of!(LiveCandleState, total_sell_qty),
    offset_of!(LiveCandleState, session_open),
    offset_of!(LiveCandleState, open_pct),
    offset_of!(LiveCandleState, open_gap_pct),
    offset_of!(LiveCandleState, net_volume_classified),
    offset_of!(LiveCandleState, first_receipt_ist_nanos),
    offset_of!(LiveCandleState, last_receipt_ist_nanos),
];

/// Start offsets of the fields `touch_state` reads, in ascending order.
const TOUCHED_OFFSETS: [usize; 4] = [
    offset_of!(LiveCandleState, open),
    offset_of!(LiveCandleState, prev_day_close),
    offset_of!(LiveCandleState, bucket_start_ist_secs),
    offset_of!(LiveCandleState, net_volume_classified),
];

/// `true` when reading the fields at `touched` (ascending) touches every cache
/// line that holds a field byte, wherever the state sits in memory: the first
/// read is at offset 0 (it covers any line that starts before the state), no
/// two consecutive reads are more than one line apart (so no line can start
/// and end between them), and the last read is at or after the last field's
/// start (a line starting after it holds only padding).
const fn covers_every_line(touched: &[usize], all: &[usize]) -> bool {
    if touched.is_empty() || touched[0] != 0 {
        return false;
    }
    let mut i = 1;
    while i < touched.len() {
        if touched[i] <= touched[i - 1] || touched[i] - touched[i - 1] > CACHE_LINE_BYTES {
            return false;
        }
        i += 1;
    }
    let last_touched = touched[touched.len() - 1];
    let mut j = 0;
    while j < all.len() {
        if all[j] > last_touched {
            return false;
        }
        j += 1;
    }
    true
}

// The compiler chooses the field order of `LiveCandleState`; if a toolchain or
// a new field moves them so the four reads no longer reach every line, the
// build stops here instead of the warm-up silently missing a line.
const _: () = assert!(
    covers_every_line(&TOUCHED_OFFSETS, &ALL_FIELD_OFFSETS),
    "the PF-7 warm-up no longer reads one field on every cache line of \
     LiveCandleState; pick fields that do (see covers_every_line)"
);

/// Reads one field on each cache line of `s` and folds the bits into one
/// word.
///
/// The destructuring names every field (no `..`) so a field added to
/// [`LiveCandleState`] fails to compile here until its offset is added to
/// `ALL_FIELD_OFFSETS`, where the compile-time line check sees it.
///
/// # Performance
/// O(1): four loads and three xors, no branch, no allocation.
#[inline(always)]
pub(crate) fn touch_state(s: &LiveCandleState) -> u64 {
    let LiveCandleState {
        open,
        prev_day_close,
        bucket_start_ist_secs,
        net_volume_classified,
        high: _,
        low: _,
        close: _,
        volume: _,
        net_volume_signed: _,
        bucket_start_cumulative: _,
        oi: _,
        tick_count: _,
        close_ts_ist_secs: _,
        open_ts_ist_secs: _,
        close_pct_from_prev_day: _,
        bucket_open_prev_close: _,
        total_buy_qty: _,
        total_sell_qty: _,
        session_open: _,
        open_pct: _,
        open_gap_pct: _,
        first_receipt_ist_nanos: _,
        last_receipt_ist_nanos: _,
    } = s;
    open.to_bits()
        ^ prev_day_close.to_bits()
        ^ u64::from(*bucket_start_ist_secs)
        ^ u64::from(*net_volume_classified)
}

/// `true` when the fold runs the warm-up. Always `true` outside tests.
#[cfg(not(test))]
#[inline(always)]
pub(crate) const fn warm_up_enabled() -> bool {
    true
}

#[cfg(test)]
thread_local! {
    /// Test-only: `true` while [`with_warm_up_off`] runs on this thread.
    static WARM_UP_OFF: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test-only form: `false` inside [`with_warm_up_off`] on this thread, so the
/// equivalence proptest can fold the same stream without the warm-up.
#[cfg(test)]
pub(crate) fn warm_up_enabled() -> bool {
    !WARM_UP_OFF.with(std::cell::Cell::get)
}

/// Test-only: runs `f` with the warm-up off on this thread.
#[cfg(test)]
pub(crate) fn with_warm_up_off<R>(f: impl FnOnce() -> R) -> R {
    WARM_UP_OFF.with(|off| off.set(true));
    let out = f();
    WARM_UP_OFF.with(|off| off.set(false));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_touch_state_reads_one_field_per_line() {
        let base = LiveCandleState::empty();
        let base_word = touch_state(&base);
        let variants: [fn(&mut LiveCandleState); 4] = [
            |s| s.open = 1.5,
            |s| s.prev_day_close = 2.5,
            |s| s.bucket_start_ist_secs = 7,
            |s| s.net_volume_classified = !s.net_volume_classified,
        ];
        for (i, set) in variants.iter().enumerate() {
            let mut s = base;
            set(&mut s);
            assert_ne!(touch_state(&s), base_word, "touched field {i} is not read");
        }
    }

    #[test]
    fn test_field_offsets_list_every_field_once_inside_the_state() {
        let mut sorted = ALL_FIELD_OFFSETS;
        sorted.sort_unstable();
        for pair in sorted.windows(2) {
            assert!(
                pair[0] < pair[1],
                "two fields listed at one offset: {sorted:?}"
            );
        }
        let last = sorted[sorted.len() - 1];
        assert!(last < size_of::<LiveCandleState>());
    }

    #[test]
    fn test_covers_every_line_rejects_gaps_and_late_starts() {
        assert!(covers_every_line(&[0, 64, 128], &[0, 8, 128]));
        // A gap of 65 lets a whole line sit between two reads.
        assert!(!covers_every_line(&[0, 65, 128], &[0, 128]));
        // Not starting at 0 misses the line holding the first bytes.
        assert!(!covers_every_line(&[8, 64], &[0, 8, 64]));
        // A field after the last read is on a line nobody reads.
        assert!(!covers_every_line(&[0, 64], &[0, 64, 120]));
        // Out of order or empty is refused.
        assert!(!covers_every_line(&[0, 64, 32], &[0]));
        assert!(!covers_every_line(&[], &[0]));
    }

    #[test]
    fn test_warm_up_enabled_is_on_by_default_and_with_warm_up_off_restores_it() {
        assert!(warm_up_enabled());
        let inside = with_warm_up_off(warm_up_enabled);
        assert!(!inside);
        assert!(warm_up_enabled());
    }
}

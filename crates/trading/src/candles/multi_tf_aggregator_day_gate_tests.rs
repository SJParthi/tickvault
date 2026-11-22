//! The day-gate permutation sweep for `multi_tf_aggregator.rs`, moved to its
//! own file unchanged on 2026-11-13 (LF-9) so the aggregator stays under its
//! size pin (`source_size_ratchet_guard`). Original module documentation:
//!
//! The DAY-GATE PERMUTATION SWEEP (2026-09-10).
//!
//! The gate added on 2026-09-10 refuses a tick whose EXCHANGE day is not the
//! RECEIPT day. That is two clocks, two directions and a stand-down sentinel,
//! and it sits in the middle of a chain of five earlier refusals — so the
//! behaviour that matters is not one arm but the GRID: which arm wins, what
//! happens at each boundary, and what happens when a clock is not merely
//! wrong but absurd.
//!
//! Every test here was written because the grid position was UNPINNED, not
//! because it was known broken. Three of them turned out to matter:
//!
//!   * the one-nanosecond receipt is exactly what broke seven drain tests on
//!     the day the gate shipped — fixtures passing `1_000_000` were handing
//!     the gate a 1970 receipt against a 2026 stamp;
//!   * the `i64::MAX` receipt is the only input that could PANIC, because the
//!     release profile sets `overflow-checks = true` and this arm does
//!     arithmetic on a caller-supplied number;
//!   * the ORDER tests pin that a corrupt price or an out-of-band second is
//!     still judged by the arm that owns it, so a day mismatch can never
//!     mask a harder fault or steal its counter.

use super::tests::{DAY, SEG_IDX, tick};
use super::*;

/// IST seconds -> the UTC epoch nanos the drain would stamp for them.
fn receipt_at_ist(ist_secs: i64) -> i64 {
    (ist_secs - crate::candles::tf_index::IST_UTC_OFFSET_SECS) * 1_000_000_000
}

const TODAY_0916: u32 = DAY + 9 * 3_600 + 16 * 60;

// -- the stand-down sentinel, on the STALE side -------------------------

/// The sentinel case is pinned for the FUTURE arm one module up; this is
/// its mirror, and it is the one that actually runs in production. A
/// pre-TVW3 WAL frame carries no receipt, and boot replay of a segment
/// deferred at yesterday's shutdown is PRIOR-DAY by construction — so if
/// the sentinel did not stand down here, every such replay would be
/// refused and the deferred backlog would never fold.
///
/// The watermark gate below it is what judges those frames, exactly as it
/// did before this change.
#[test]
fn a_prior_day_frame_with_no_receipt_is_left_to_the_watermark() {
    let mut agg = MultiTfAggregator::default();
    let yesterday = DAY - 86_400 + 33_400;
    let t = tick(13, SEG_IDX, yesterday, 100.0, 1);
    assert_eq!(t.received_at_nanos, 0, "fixture must exercise the sentinel");

    let stats = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

    assert!(
        !stats.stale_trading_day,
        "with no second clock the receipt gate must stand down — refusing \
         here would strand every pre-TVW3 boot replay, which is prior-day \
         by construction"
    );
}

/// A NEGATIVE receipt is garbage, not a clock, and it is treated as the
/// sentinel rather than as a 1970 instant.
///
/// The distinction is not academic: `receipt_ist_secs / 86_400` truncates
/// TOWARD ZERO in Rust, so a negative receipt would compute day 0 for
/// anything inside the first 86,400 seconds before the epoch and slide the
/// comparison silently. Standing down is the only answer that cannot be
/// subtly wrong, and the `> 0` guard is what delivers it.
#[test]
fn a_negative_receipt_clock_stands_the_gate_down_rather_than_guessing() {
    let mut agg = MultiTfAggregator::default();
    let mut t = tick(13, SEG_IDX, TODAY_0916, 100.0, 1);
    t.received_at_nanos = -1_000_000_000;

    let stats = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

    assert!(
        !stats.stale_trading_day && !stats.future_trading_day,
        "a negative receipt is not a clock; the gate must not judge a day \
         against it in either direction"
    );
}

// -- absurd-but-positive clocks ----------------------------------------

/// THE BUG THAT BROKE SEVEN TESTS on the day this gate shipped.
///
/// Six drain fixtures passed `received_at_nanos = 1_000_000` — one
/// millisecond after the epoch — as a "don't care" value, because before
/// the gate nothing read it. Against a live 2026 exchange stamp that is a
/// receipt fifty-six years in the past, so the stamp is FUTURE-dated by
/// fifty-six years and refused. `folded` went to 0 and the fixtures failed
/// on assertions about frame-walk accounting, which is a symptom miles
/// from the cause.
///
/// Pinned so the next reader who sees `folded: 0` in a drain test has the
/// answer in a test name instead of an afternoon.
#[test]
fn a_one_nanosecond_receipt_reads_a_live_stamp_as_future_dated() {
    let mut agg = MultiTfAggregator::default();
    let mut t = tick(13, SEG_IDX, TODAY_0916, 100.0, 1);
    t.received_at_nanos = 1;

    let stats = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

    assert!(
        stats.future_trading_day,
        "a 1970 receipt against a 2026 stamp IS a future-dated tick, and \
         the gate is right to say so — the fixture was wrong, not the gate"
    );
}

/// The only input that could PANIC: `overflow-checks = true` is set on the
/// release profile, and this arm divides and ADDS to a caller-supplied
/// `i64`. `i64::MAX / 1_000_000_000` is 9,223,372,036, and adding the
/// 19,800-second IST offset stays nine orders of magnitude below the
/// ceiling — so the arithmetic is safe by construction rather than by
/// luck, and this test is what keeps it that way if the offset ever moves
/// or the division is removed.
#[test]
fn a_receipt_clock_at_the_i64_ceiling_refuses_without_overflowing() {
    let mut agg = MultiTfAggregator::default();
    let mut t = tick(13, SEG_IDX, TODAY_0916, 100.0, 1);
    t.received_at_nanos = i64::MAX;

    let stats = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

    assert!(
        stats.stale_trading_day,
        "a receipt at the end of time makes every real stamp stale — the \
         verdict is unhelpful but it must be a VERDICT, never a panic on \
         the per-tick path"
    );
}

/// The measured worst case, from the vendor's own behaviour: Dhan sends
/// LAST TRADE TIME, and a dormant contract's connect snapshot was measured
/// at a maximum of 34 days stale. That is the top of the envelope this
/// gate exists to refuse.
#[test]
fn the_measured_thirty_four_day_maximum_stale_snapshot_is_refused() {
    let mut agg = MultiTfAggregator::default();
    let thirty_four_days_ago = DAY - 34 * 86_400 + 33_400;
    let mut t = tick(66_422, SEG_IDX, thirty_four_days_ago, 142.50, 12_000);
    t.received_at_nanos = receipt_at_ist(i64::from(TODAY_0916));

    let stats = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

    assert!(
        stats.stale_trading_day,
        "the measured 34-day maximum must be refused — this is the top of \
         the envelope, not a hypothetical"
    );
    // Audit PR58: it may take a slot for its proof; it opens no bucket.
    assert!(
        TfIndex::ALL.iter().all(|&tf| agg
            .snapshot(Feed::Dhan, 66_422, SEG_IDX, tf)
            .is_none_or(|s| s.bucket_start_ist_secs == 0)),
        "and it must not open a bucket"
    );
}

/// `receipt_day_mismatch` separates the two ways `stale_trading_day` is
/// raised, and a WAL replay depends on the difference (2026-09-23).
///
/// The morning replay on 2026-09-23 counted 4 ticks as LOST and paged
/// `WS-SPILL-01`. All 4 were connect snapshots whose trade date was the
/// previous session while the receipt was today — the receipt-clock arm,
/// the operator's 2026-09-10 rule, the same verdict the live feed reaches.
/// A tick received on the day it traded and replayed the NEXT morning is
/// different: it is refused by the watermark, it is real captured data, and
/// it must still read as loss. One flag, three arms, all pinned here.
#[test]
fn receipt_day_mismatch_is_set_by_the_receipt_arms_and_never_by_the_watermark() {
    // Receipt-clock STALE: traded yesterday, received today.
    let mut agg = MultiTfAggregator::default();
    let yesterday_1529 = DAY - 86_400 + 15 * 3_600 + 29 * 60;
    let mut stale = tick(66_422, SEG_IDX, yesterday_1529, 142.50, 12_000);
    stale.received_at_nanos = receipt_at_ist(i64::from(TODAY_0916));
    let stats = agg.consume_tick(Feed::Dhan, &stale, None, ignore_seal);
    assert!(stats.stale_trading_day && stats.receipt_day_mismatch);

    // Receipt-clock FUTURE: stamped tomorrow, received today.
    let mut agg = MultiTfAggregator::default();
    let mut future = tick(13, SEG_IDX, TODAY_0916 + 86_400, 100.0, 1);
    future.received_at_nanos = receipt_at_ist(i64::from(TODAY_0916));
    let stats = agg.consume_tick(Feed::Dhan, &future, None, ignore_seal);
    assert!(stats.future_trading_day && stats.receipt_day_mismatch);

    // WATERMARK stale: a fresh tick today moves the watermark; then a tick
    // that traded AND was received yesterday arrives (a replayed frame).
    // Its receipt matches its trade day, so the receipt arm passes it and
    // the watermark refuses it — and this one is real data.
    let mut agg = MultiTfAggregator::default();
    let mut today = tick(13, SEG_IDX, TODAY_0916, 100.0, 1);
    today.received_at_nanos = receipt_at_ist(i64::from(TODAY_0916));
    let first = agg.consume_tick(Feed::Dhan, &today, None, ignore_seal);
    assert!(
        first.folded(),
        "fixture: today's tick must fold and set the watermark"
    );
    let mut replayed = tick(13, SEG_IDX, yesterday_1529, 99.0, 1);
    replayed.received_at_nanos = receipt_at_ist(i64::from(yesterday_1529));
    let stats = agg.consume_tick(Feed::Dhan, &replayed, None, ignore_seal);
    assert!(
        stats.stale_trading_day,
        "fixture: the watermark arm must be what refuses this tick"
    );
    assert!(
        !stats.receipt_day_mismatch,
        "a tick received on the day it traded is NOT a receipt-day mismatch — \
         marking it would let a replay report real lost data as by-design"
    );
}

// -- day boundaries -----------------------------------------------------

/// One SECOND apart across IST midnight is a genuine day mismatch and is
/// refused.
///
/// ## ⚠ Re-stated 2026-09-18 — the old safety argument cited a mechanism
/// ## and a test that no longer exist
///
/// This used to read: *"It is safe because the trusted band already
/// collapses a real straddle onto one clock (pinned by
/// `a_tick_straddling_ist_midnight_inside_the_trusted_band_is_not_stale`)."*
/// Both halves are gone. The ts-bucketing directive deleted the trusted
/// band, and the test it named now asserts the OPPOSITE under the name
/// `a_tick_stamped_before_ist_midnight_and_received_after_it_is_stale` —
/// because with one clock the gate finally compares the EXCHANGE day
/// against the receipt day, which is the rule its own comment always
/// claimed to enforce.
///
/// The sharp edge is therefore real and unmitigated: the comparison is on
/// DAYS, so a single second flips it, and nothing collapses a straddle any
/// more. **What makes it safe is the session window, not the fold clock**:
/// the candle session is `[09:00, 15:40)` IST and the box is stopped by
/// 17:30, so nothing legitimate trades within a second of IST midnight,
/// and anything that did would already be refused `out_of_session` by the
/// seconds-of-day gate. That is a weaker guarantee than the one this
/// comment used to claim, and it is stated plainly rather than implied.
#[test]
fn an_exchange_stamp_one_second_before_midnight_is_stale_against_the_next_day() {
    let mut agg = MultiTfAggregator::default();
    // Receipt a full working day later — the mismatch is unambiguous and
    // does not depend on any band, which no longer exists.
    let mut t = tick(13, SEG_IDX, DAY - 1, 100.0, 1);
    t.received_at_nanos = receipt_at_ist(i64::from(DAY + 33_400));

    let stats = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

    assert!(
        stats.stale_trading_day,
        "23:59:59 of the previous IST day is a different day, and the gate \
         is a DAY comparison — a second is enough"
    );
}

/// Exactly IST midnight is the FIRST second of the new day, not the last
/// second of the old one. Off-by-one here would refuse a whole day's
/// opening tick on the boundary.
#[test]
fn an_exchange_stamp_exactly_at_ist_midnight_belongs_to_the_new_day() {
    let mut agg = MultiTfAggregator::default();
    let mut t = tick(13, SEG_IDX, DAY, 100.0, 1);
    t.received_at_nanos = receipt_at_ist(i64::from(DAY + 33_400));

    let stats = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

    assert!(
        !stats.stale_trading_day,
        "00:00:00 IST is day D, not day D-1 — the floor division must put \
         the boundary second on the new day"
    );
}

// -- ORDER: which arm wins when two faults are true at once -------------

/// A corrupt PRICE outranks a day mismatch, and must: the price arm is a
/// hard refusal whose remedy is "this packet is unusable", while the day
/// arm's remedy is "this is a stale snapshot". Booking a NaN price under
/// `stale_trading_day` would make the stale-snapshot rate — the number the
/// operator reads to size reconnect noise — silently include corruption.
#[test]
fn an_insane_price_is_judged_before_a_day_mismatch() {
    let mut agg = MultiTfAggregator::default();
    let yesterday = DAY - 86_400 + 33_400;
    let mut t = tick(13, SEG_IDX, yesterday, f32::NAN, 1);
    t.received_at_nanos = receipt_at_ist(i64::from(TODAY_0916));

    let stats = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

    assert!(stats.refused_price, "the price arm owns a NaN");
    assert!(
        !stats.stale_trading_day,
        "and the day arm must not also claim it — one tick, one reason, or \
         the stale-snapshot rate stops meaning what it says"
    );
}

/// The vendor's never-traded sentinel (`exchange_timestamp == 0`) is
/// judged by its own arm, which KEEPS THE ROW. Letting it fall to the day
/// gate would turn it into a hard refusal and lose the ability to tell
/// "did not trade today" from "did not capture" — the exact false-OK the
/// 2026-08-26 fix removed.
#[test]
fn an_untraded_sentinel_never_reaches_the_day_gate() {
    let mut agg = MultiTfAggregator::default();
    let mut t = tick(13, SEG_IDX, 0, 100.0, 1);
    t.received_at_nanos = receipt_at_ist(i64::from(TODAY_0916));

    let stats = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

    assert!(
        stats.untraded_timestamp,
        "the sentinel keeps its own arm, which keeps the row"
    );
    assert!(
        !stats.stale_trading_day && !stats.future_trading_day,
        "the day gate must not see it — epoch 0 is a sentinel, not a 1970 \
         trading day, and refusing it hard would lose the row"
    );
}

/// An out-of-band second is also judged before the day gate, and also
/// keeps its row. It is the LARGEST of the candle-only reasons — 2,008,916
/// ticks in one measured session — so a day gate that swallowed it would
/// silently convert 2.4% of a session from kept rows to discarded ones.
#[test]
fn an_out_of_band_stamp_never_reaches_the_day_gate() {
    let mut agg = MultiTfAggregator::default();
    let mut t = tick(13, SEG_IDX, MIN_PLAUSIBLE_EXCHANGE_TS_SECS - 1, 100.0, 1);
    t.received_at_nanos = receipt_at_ist(i64::from(TODAY_0916));

    let stats = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

    assert!(
        stats.out_of_band_timestamp,
        "a below-floor second belongs to the band arm, which keeps the row"
    );
    assert!(
        !stats.stale_trading_day && !stats.future_trading_day,
        "the day gate must not re-judge it as a stale day and discard the row"
    );
}

// -- the grid -----------------------------------------------------------

/// STALE and FUTURE can never both be true, across the whole grid.
///
/// They are exclusive by arithmetic (`>` then `<` on the same pair), but
/// the drain books ONE reason per tick from an `if/else if` chain, so an
/// overlap would silently drop one counter and mis-attribute the other.
/// This walks nine day offsets against three receipt days and asserts the
/// invariant on every cell, rather than trusting the arithmetic to stay
/// the arithmetic.
#[test]
fn stale_and_future_are_mutually_exclusive_across_the_grid() {
    for day_offset in [-34_i64, -2, -1, 0, 1, 2, 34] {
        for receipt_offset in [-1_i64, 0, 1] {
            let mut agg = MultiTfAggregator::default();
            let stamp = i64::from(DAY) + day_offset * 86_400 + 33_400;
            let Ok(stamp_u32) = u32::try_from(stamp) else {
                continue;
            };
            let mut t = tick(13, SEG_IDX, stamp_u32, 100.0, 1);
            t.received_at_nanos = receipt_at_ist(i64::from(DAY) + receipt_offset * 86_400 + 33_400);

            let stats = agg.consume_tick(Feed::Dhan, &t, None, ignore_seal);

            assert!(
                !(stats.stale_trading_day && stats.future_trading_day),
                "day_offset {day_offset}, receipt_offset {receipt_offset}: \
                 both day flags set. The drain books ONE reason per tick, \
                 so an overlap loses a counter and mis-names the other"
            );
            // And exactly one of the three outcomes holds.
            let same_day = day_offset == receipt_offset;
            assert_eq!(
                !stats.stale_trading_day && !stats.future_trading_day,
                same_day,
                "day_offset {day_offset}, receipt_offset {receipt_offset}: \
                 a tick folds if and only if the two days match"
            );
        }
    }
}

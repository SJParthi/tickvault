//! Wave 6 Sub-PR #1 item 1.3 — boundary-timer pure-function primitives.
//!
//! The per-tick boundary detection (does this tick belong to a new
//! bucket? if so seal the old one) ALREADY lives in
//! [`AggregatorCell::consume_tick`] (item 1.1, PR #555). This module
//! ships the COLD-PATH primitives the future boundary-timer tokio
//! task (item 1.3.5+) will use:
//!
//! 1. **[`next_ist_midnight_after`]** — given a UTC unix second,
//!    return the UTC unix second of the next IST midnight. Used by
//!    the timer to compute its sleep duration. **L-C4** (Muhurat
//!    deferred to Wave 7); this slice covers regular-day rollover
//!    only.
//! 2. **[`trading_date_ist`]** — derive the IST trading-date from
//!    a WS-derived `exchange_timestamp` per locked decision **L-H7**
//!    (NEVER `Utc::now()`).
//! 3. **[`missed_boundaries_for_tf`]** — given a TF, the cell's
//!    previous bucket-start, and the new tick's IST second, return
//!    how many TF boundaries were skipped (e.g. system paused 5
//!    minutes → 4 missed minute-boundaries on M1). The
//!    `AggregatorCell::consume_tick` flow then seals the prev bucket
//!    once and opens the new one without emitting empty bars per
//!    **L-H13**; this helper is for OBSERVABILITY only — drives
//!    `BOUNDARY-01` when missed_count > 0 so the operator notices
//!    the gap.
//!
//! ## Why these are pure functions
//!
//! The boundary timer's tokio loop wakes on a wall-clock cadence
//! (sleep until next IST midnight). The TIMER itself is async; the
//! MATH driving it is pure. Splitting them lets us exhaustively test
//! the math without spinning up a tokio runtime, and lets the
//! aggregator producer (a synchronous fold over each tick) call the
//! same `missed_boundaries_for_tf` helper for its own observability
//! without paying for an async context switch.
//!
//! ## Hot-path vs cold-path
//!
//! - [`missed_boundaries_for_tf`] is `O(1)`, branch-free, allocation-free
//!   — safe to call from `consume_tick` if a Sub-PR ever needs to.
//! - [`next_ist_midnight_after`] / [`trading_date_ist`] are also `O(1)`,
//!   but should be COLD path because they take/return calendar
//!   timestamps (the timer task uses them once a day).

use chrono::NaiveDate;

use tickvault_common::constants::IST_UTC_OFFSET_SECONDS_I64;

use crate::candles::TfIndex;

/// One day in seconds. Pinned here so the cross-crate
/// `chrono::Duration::days(1).num_seconds()` is not pulled into the
/// hot path's compile-time constants.
pub const SECONDS_PER_DAY: i64 = 86_400;

/// Returns the UTC unix-second of the next IST midnight strictly
/// AFTER `now_utc_secs`. If `now_utc_secs` is exactly at IST
/// midnight, the NEXT midnight is returned (24 hours later) — the
/// timer wants to wake AFTER the boundary, not exactly at it.
///
/// Math: IST = UTC + 19800s. IST-day-start = UTC time aligned to a
/// (now + 19800) % 86400 == 0 boundary. The next such UTC second
/// strictly after `now_utc_secs` is what we return.
///
/// `O(1)`, branch-free.
#[must_use]
pub const fn next_ist_midnight_after(now_utc_secs: i64) -> i64 {
    let ist_secs = now_utc_secs.saturating_add(IST_UTC_OFFSET_SECONDS_I64);
    let secs_today_ist = ist_secs.rem_euclid(SECONDS_PER_DAY);
    let secs_until_midnight = SECONDS_PER_DAY - secs_today_ist;
    now_utc_secs.saturating_add(secs_until_midnight)
}

/// Returns the IST trading date for a tick whose
/// `exchange_timestamp` is an IST epoch second (per WS LTT
/// semantics). The conversion is the identity mapping at the
/// calendar layer — IST seconds → IST date — so we do NOT add any
/// offset. Mirrors locked decision **L-H7**: the WS LTT carries IST
/// already, NEVER add `+19_800`.
///
/// Returns `None` if the i64-conversion would overflow (caller treats
/// this as "use today's date as fallback"; in practice `u32` IST
/// seconds always fit in `chrono`'s range).
#[must_use]
pub fn trading_date_ist(exchange_ts_ist_secs: u32) -> Option<NaiveDate> {
    use chrono::DateTime;
    let dt = DateTime::from_timestamp(i64::from(exchange_ts_ist_secs), 0)?;
    Some(dt.naive_utc().date())
}

/// Number of timeframe boundaries SKIPPED between the cell's
/// previous bucket-start and the current tick's bucket. Returns 0
/// if no boundaries were missed (i.e. the new bucket is exactly
/// `prev + seconds_per_bucket`, the normal case).
///
/// Examples for `TfIndex::M1` (60-second buckets):
/// - prev=09:00, tick=09:00:30 → 0 (same bucket, in-bucket fold)
/// - prev=09:00, tick=09:01:15 → 0 (1 boundary, normal next-bucket)
/// - prev=09:00, tick=09:05:30 → 4 (4 minute-boundaries skipped)
/// - prev=0 (uninitialised) → 0 (no prev bucket; not a "miss")
/// - tick before prev (late) → 0 (handled by `ConsumeOutcome::DiscardLate`)
///
/// The future boundary timer fires `BOUNDARY-01` (Severity::Medium)
/// when this returns > 0 — see `wave-6-error-codes.md`.
///
/// `O(1)`, branch-free, allocation-free.
#[must_use]
pub const fn missed_boundaries_for_tf(
    tf: TfIndex,
    prev_bucket_start: u32,
    current_tick_ist_secs: u32,
) -> u32 {
    // No previous bucket → no miss.
    if prev_bucket_start == 0 {
        return 0;
    }
    let current_bucket = tf.bucket_start(current_tick_ist_secs);
    // Late or same-bucket tick — handled elsewhere.
    if current_bucket <= prev_bucket_start {
        return 0;
    }
    let secs = tf.seconds_per_bucket();
    let buckets_skipped = (current_bucket - prev_bucket_start) / secs;
    // Adjacent bucket (1) is the normal case, not a miss.
    if buckets_skipped <= 1 {
        return 0;
    }
    buckets_skipped - 1
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ist_midnight_2026_05_10_utc() -> i64 {
        // 2026-05-10 00:00:00 IST = 2026-05-09 18:30:00 UTC
        chrono::Utc
            .with_ymd_and_hms(2026, 5, 9, 18, 30, 0)
            .single()
            .expect("valid")
            .timestamp()
    }

    fn ist_noon_2026_05_10_utc() -> i64 {
        // 2026-05-10 12:00:00 IST = 2026-05-10 06:30:00 UTC
        chrono::Utc
            .with_ymd_and_hms(2026, 5, 10, 6, 30, 0)
            .single()
            .expect("valid")
            .timestamp()
    }

    // ── next_ist_midnight_after ─────────────────────────────────

    #[test]
    fn test_next_ist_midnight_after_noon_returns_next_midnight() {
        let noon = ist_noon_2026_05_10_utc();
        let next = next_ist_midnight_after(noon);
        // Next IST midnight = 2026-05-11 00:00:00 IST = 2026-05-10 18:30:00 UTC.
        let expected = chrono::Utc
            .with_ymd_and_hms(2026, 5, 10, 18, 30, 0)
            .single()
            .expect("valid")
            .timestamp();
        assert_eq!(next, expected);
        assert_eq!(next - noon, 12 * 3600); // 12 hours after noon
    }

    #[test]
    fn test_next_ist_midnight_after_exactly_at_midnight_returns_next_day() {
        // The contract is "strictly after". Caller about to sleep
        // must wake after the boundary, not exactly at it (the cell
        // hasn't crossed the boundary yet at second N exactly).
        let midnight = ist_midnight_2026_05_10_utc();
        let next = next_ist_midnight_after(midnight);
        let expected_next_day = chrono::Utc
            .with_ymd_and_hms(2026, 5, 10, 18, 30, 0)
            .single()
            .expect("valid")
            .timestamp();
        assert_eq!(next, expected_next_day);
        assert_eq!(next - midnight, SECONDS_PER_DAY);
    }

    #[test]
    fn test_next_ist_midnight_after_one_second_before_midnight() {
        // 1 second before next IST midnight → next is 1 second later.
        let one_sec_before = ist_midnight_2026_05_10_utc() - 1;
        let next = next_ist_midnight_after(one_sec_before);
        assert_eq!(next - one_sec_before, 1);
    }

    #[test]
    fn test_next_ist_midnight_after_one_second_after_midnight() {
        // 1 second after IST midnight → next is 86399 seconds later.
        let one_sec_after = ist_midnight_2026_05_10_utc() + 1;
        let next = next_ist_midnight_after(one_sec_after);
        assert_eq!(next - one_sec_after, SECONDS_PER_DAY - 1);
    }

    #[test]
    fn test_next_ist_midnight_after_handles_pre_epoch_safely() {
        // 1970-01-01 00:00:00 UTC = 1970-01-01 05:30:00 IST
        // Next IST midnight = 1970-01-01 18:30:00 UTC = 66600 secs.
        let next = next_ist_midnight_after(0);
        assert_eq!(next, 66_600);
    }

    #[test]
    fn test_next_ist_midnight_after_is_const_evaluable() {
        // The function is `const`; this is a compile-time check that
        // we can use it in const contexts without surprises.
        const RESULT: i64 = next_ist_midnight_after(0);
        assert_eq!(RESULT, 66_600);
    }

    // ── trading_date_ist ────────────────────────────────────────

    #[test]
    fn test_trading_date_ist_for_known_ist_second() {
        // IST 2026-05-10 09:15:00 = unix epoch (IST seconds since 1970)
        let ist_dt = chrono::Utc
            .with_ymd_and_hms(2026, 5, 10, 9, 15, 0)
            .single()
            .expect("valid");
        let secs = u32::try_from(ist_dt.timestamp()).expect("u32");
        let date = trading_date_ist(secs).expect("ok");
        assert_eq!(date, NaiveDate::from_ymd_opt(2026, 5, 10).expect("valid"));
    }

    #[test]
    fn test_trading_date_ist_at_ist_midnight_yields_new_day() {
        // IST 2026-05-10 00:00:00 — calendar date = 2026-05-10.
        let ist_midnight_secs = chrono::Utc
            .with_ymd_and_hms(2026, 5, 10, 0, 0, 0)
            .single()
            .expect("valid")
            .timestamp() as u32;
        let date = trading_date_ist(ist_midnight_secs).expect("ok");
        assert_eq!(date, NaiveDate::from_ymd_opt(2026, 5, 10).expect("valid"));
    }

    #[test]
    fn test_trading_date_ist_one_second_before_midnight_is_prev_day() {
        let ist_secs = chrono::Utc
            .with_ymd_and_hms(2026, 5, 9, 23, 59, 59)
            .single()
            .expect("valid")
            .timestamp() as u32;
        let date = trading_date_ist(ist_secs).expect("ok");
        assert_eq!(date, NaiveDate::from_ymd_opt(2026, 5, 9).expect("valid"));
    }

    #[test]
    fn test_trading_date_ist_handles_max_u32() {
        // u32::MAX seconds = ~2106-02-07 06:28:15. chrono can handle.
        let date = trading_date_ist(u32::MAX);
        assert!(date.is_some());
    }

    // ── missed_boundaries_for_tf ────────────────────────────────

    #[test]
    fn test_missed_boundaries_returns_zero_for_in_bucket_tick_m1() {
        // prev=09:00:00, tick=09:00:30 → same bucket, no miss.
        let prev = 1_716_000_000_u32; // hypothetical IST 09:00:00
        let tick = prev + 30;
        assert_eq!(missed_boundaries_for_tf(TfIndex::M1, prev, tick), 0);
    }

    #[test]
    fn test_missed_boundaries_returns_zero_for_adjacent_next_bucket_m1() {
        // prev=09:00:00, tick=09:01:15 → 1 boundary, normal next-bucket.
        let prev = 1_716_000_000_u32;
        let tick = prev + 75; // 1 min 15 s later
        assert_eq!(missed_boundaries_for_tf(TfIndex::M1, prev, tick), 0);
    }

    #[test]
    fn test_missed_boundaries_returns_four_for_5min_gap_m1() {
        // prev=09:00, tick=09:05:30 → 4 minute-boundaries skipped.
        let prev = 1_716_000_000_u32;
        let tick = prev + (5 * 60) + 30;
        assert_eq!(missed_boundaries_for_tf(TfIndex::M1, prev, tick), 4);
    }

    #[test]
    fn test_missed_boundaries_returns_zero_when_prev_uninitialised() {
        // prev_bucket_start == 0 means the cell never opened.
        // No miss to report.
        assert_eq!(missed_boundaries_for_tf(TfIndex::M1, 0, 1_716_000_900), 0);
    }

    #[test]
    fn test_missed_boundaries_returns_zero_for_late_tick_m1() {
        // tick is BEFORE prev — the late-arrival path
        // (ConsumeOutcome::DiscardLate). No miss.
        let prev = 1_716_000_900_u32;
        let tick = prev - 30;
        assert_eq!(missed_boundaries_for_tf(TfIndex::M1, prev, tick), 0);
    }

    #[test]
    fn test_missed_boundaries_for_each_tf_with_multi_bucket_gap() {
        // For each TF, prev bucket + 5×bucket_size = 4 missed.
        for tf in TfIndex::ALL {
            let prev = tf.bucket_start(1_716_000_000);
            let tick = prev + tf.seconds_per_bucket() * 5 + 30;
            let missed = missed_boundaries_for_tf(tf, prev, tick);
            assert_eq!(
                missed, 4,
                "TF {tf:?}: expected 4 missed across 5×bucket gap"
            );
        }
    }

    #[test]
    fn test_missed_boundaries_returns_zero_for_each_tf_in_bucket() {
        // Tick lands in the same bucket as prev → 0 miss for every TF.
        for tf in TfIndex::ALL {
            let prev = tf.bucket_start(1_716_000_000);
            // Tick 1 second after prev → still in same bucket (since
            // tf.seconds_per_bucket() >= 60 for all our TFs).
            let tick = prev + 1;
            assert_eq!(
                missed_boundaries_for_tf(tf, prev, tick),
                0,
                "TF {tf:?}: in-bucket tick must yield 0 misses"
            );
        }
    }

    #[test]
    fn test_missed_boundaries_const_evaluable() {
        const RESULT: u32 = missed_boundaries_for_tf(TfIndex::M1, 1_716_000_000, 1_716_000_330);
        assert_eq!(RESULT, 4);
    }

    #[test]
    fn test_seconds_per_day_constant_pinned() {
        assert_eq!(SECONDS_PER_DAY, 86_400);
    }
}

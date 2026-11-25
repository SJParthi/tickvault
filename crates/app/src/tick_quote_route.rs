//! Which table a main-feed packet's row goes to: `ticks` or `tick_quotes`
//! (plan item M5; owner Quotes 41 and 43, 2026-10-27;
//! `websocket-connection-scope-lock.md` §"2026-10-27 (SECOND) — `tick_quotes`
//! ROUTES ON THE PACKET ALONE").
//!
//! A packet goes to `tick_quotes` when its exchange trade time (whole IST
//! seconds) is more than [`TICK_QUOTES_STALE_TRADE_SECS`] before the frame's
//! receipt time (the WAL record's receipt stamp, whole IST seconds); otherwise
//! it goes to `ticks`. Index packets follow the same rule. A packet with no
//! usable receipt time stays in `ticks`.
//!
//! The inputs are the packet and its own WAL record only: no per-contract,
//! per-socket or order-dependent state. A WAL replay reads the same trade time
//! and the same receipt stamp, so it routes every packet exactly as live did,
//! whatever segment it starts from, and no packet can reach both tables.
//! O(1), no allocation, no lookup.
//!
//! **Cost of the rule (Quote 43, owner-accepted):** a real trade that reaches
//! us more than 2 s after it printed is written to `tick_quotes` (with its
//! exchange time in `exchange_timestamp`), not to `ticks`. Nothing is lost and
//! the candle fold, which reads every packet, is unaffected.
//!
//! **Why the receipt must be the WAL stamp.** The live drain substitutes a
//! wall-clock reading for a receipt the WAL could not record (stored as the
//! unknown sentinel), which a replay cannot reproduce. The caller therefore
//! passes `receipt_is_wal_stamp = false` for such a frame and the packet stays
//! in `ticks`, live and on replay (where the receipt reads as `0`).

use tickvault_common::constants::IST_UTC_OFFSET_NANOS;

/// A packet whose trade time is MORE than this many whole seconds before its
/// receipt goes to `tick_quotes` (Quote 43). Changing it needs a fresh dated
/// owner quote in the scope lock.
pub const TICK_QUOTES_STALE_TRADE_SECS: i64 = 2;

/// The first receipt instant routed by this rule: 2026-10-28 00:00:00 IST, as
/// IST epoch seconds. A frame received earlier was written to `ticks` by the
/// binary that captured it, so a replay of a pre-deploy WAL segment must send
/// it to `ticks` again; routing it to `tick_quotes` would leave the packet in
/// BOTH tables (the `ticks` DEDUP key cannot collapse a row in another table).
/// It also keeps the Quote 42 wipe's premise true: `tick_quotes` holds no row
/// dated before 2026-10-28. Still a pure function of the packet and its WAL
/// record (the receipt stamp), so live and replay agree.
pub const TICK_QUOTES_FIRST_RECEIPT_IST_SECS: i64 = 1_793_145_600;

/// Nanoseconds in one second.
const NANOS_PER_SEC: i64 = 1_000_000_000;

/// `true` when the packet's row belongs in `tick_quotes`. Pure, O(1).
///
/// * `ltt_ist_secs` — the packet's exchange trade time, IST epoch seconds.
/// * `received_at_utc_nanos` — the frame's receipt time from its WAL record
///   (UTC epoch nanos); `0` means unknown.
/// * `receipt_is_wal_stamp` — `false` when the WAL could not record the
///   receipt and the live drain substituted a wall-clock reading.
#[must_use]
pub fn routes_to_tick_quotes(
    ltt_ist_secs: u32,
    received_at_utc_nanos: i64,
    receipt_is_wal_stamp: bool,
) -> bool {
    if !receipt_is_wal_stamp || received_at_utc_nanos == 0 {
        return false;
    }
    let receipt_ist_secs = received_at_utc_nanos
        .saturating_add(IST_UTC_OFFSET_NANOS)
        .div_euclid(NANOS_PER_SEC);
    receipt_ist_secs >= TICK_QUOTES_FIRST_RECEIPT_IST_SECS
        && receipt_ist_secs.saturating_sub(i64::from(ltt_ist_secs)) > TICK_QUOTES_STALE_TRADE_SECS
}

/// `true` when `received_at_utc_nanos`, as recorded for a frame, is a real
/// WAL receipt stamp rather than the unknown sentinel the WAL stores for an
/// implausible receipt. The live drain and the replay call it once per frame
/// with the same recorded value. Pure, O(1): two compares.
#[must_use]
pub fn receipt_is_wal_stamp(received_at_utc_nanos: i64) -> bool {
    tickvault_storage::ws_frame_spill::plausible_receipt_nanos(received_at_utc_nanos)
        != tickvault_storage::ws_frame_spill::WAL_RECEIPT_UNKNOWN_NANOS
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// 2026-10-28 10:00:00 IST, IST epoch seconds (the first day the rule
    /// routes; see `TICK_QUOTES_FIRST_RECEIPT_IST_SECS`).
    const TEN_AM_IST: u32 = 1_793_181_600;

    /// UTC nanos of an IST receipt `secs` after 10:00:00 IST, plus `sub` nanos.
    fn receipt(secs: u32, sub: i64) -> i64 {
        (i64::from(TEN_AM_IST + secs) * NANOS_PER_SEC) - IST_UTC_OFFSET_NANOS + sub
    }

    /// Quote 43's boundary: 2 s old stays in `ticks`, 3 s old goes to quotes.
    #[test]
    fn test_regression_routes_to_tick_quotes_exact_two_second_boundary() {
        let at = receipt(10, 0);
        assert!(!routes_to_tick_quotes(TEN_AM_IST + 10, at, true), "0 s");
        assert!(
            !routes_to_tick_quotes(TEN_AM_IST + 8, at, true),
            "2 s → ticks"
        );
        assert!(
            routes_to_tick_quotes(TEN_AM_IST + 7, at, true),
            "3 s → tick_quotes"
        );
        assert!(
            routes_to_tick_quotes(TEN_AM_IST - 3_600, at, true),
            "an hour old"
        );
        // Whole seconds: the sub-second part of the receipt never moves it.
        assert!(!routes_to_tick_quotes(
            TEN_AM_IST + 8,
            receipt(10, 999_999_999),
            true
        ));
        assert!(routes_to_tick_quotes(TEN_AM_IST + 7, receipt(10, 1), true));
        // A trade time ahead of the receipt stays in `ticks`.
        assert!(!routes_to_tick_quotes(TEN_AM_IST + 60, at, true));
    }

    /// Review finding (2026-10-27): a frame captured before the deploy was
    /// written to `ticks` live; its replay must not add a `tick_quotes` copy.
    #[test]
    fn test_regression_frames_received_before_2026_10_28_stay_in_ticks() {
        // 2026-10-28 00:00:00 IST is the day boundary.
        assert_eq!(TICK_QUOTES_FIRST_RECEIPT_IST_SECS % 86_400, 0);
        assert_eq!(
            TICK_QUOTES_FIRST_RECEIPT_IST_SECS,
            i64::from(TEN_AM_IST) - 36_000
        );
        let day_start = TEN_AM_IST - 36_000;
        // An hour-old trade received one second before the cutoff: `ticks`.
        let before = receipt(0, 0) - 36_001 * NANOS_PER_SEC;
        assert!(!routes_to_tick_quotes(day_start - 3_601, before, true));
        // The same age received exactly at the cutoff: `tick_quotes`.
        let at = receipt(0, 0) - 36_000 * NANOS_PER_SEC;
        assert!(routes_to_tick_quotes(day_start - 3_600, at, true));
        // 2026-10-27 10:00:10 IST receipt, 10 s old trade: `ticks`.
        let yesterday = receipt(10, 0) - 86_400 * NANOS_PER_SEC;
        assert!(!routes_to_tick_quotes(TEN_AM_IST - 86_400, yesterday, true));
    }

    #[test]
    fn test_routes_to_tick_quotes_unknown_receipt_stays_in_ticks() {
        assert!(!routes_to_tick_quotes(TEN_AM_IST - 3_600, 0, true));
        assert!(!routes_to_tick_quotes(
            TEN_AM_IST - 3_600,
            receipt(10, 0),
            false
        ));
        // The never-traded sentinel trade time with an unknown receipt.
        assert!(!routes_to_tick_quotes(0, 0, true));
    }

    #[test]
    fn test_receipt_is_wal_stamp_refuses_the_unknown_sentinel_and_absurd_values() {
        assert!(receipt_is_wal_stamp(receipt(10, 0)));
        assert!(!receipt_is_wal_stamp(0));
        assert!(!receipt_is_wal_stamp(-1));
        assert!(!receipt_is_wal_stamp(i64::MAX));
    }

    #[test]
    fn test_tick_quotes_stale_trade_secs_is_the_quote_43_value() {
        assert_eq!(TICK_QUOTES_STALE_TRADE_SECS, 2);
    }

    proptest! {
        /// The route is a function of (trade second, receipt second) alone:
        /// any two receipts in the same IST second give the same table, and
        /// the answer is exactly "received on or after 2026-10-28 IST and
        /// more than 2 s older".
        #[test]
        fn proptest_routes_to_tick_quotes_is_a_pure_function_of_the_two_seconds(
            ltt in 1_700_000_000_u32..1_900_000_000,
            receipt_secs in 1_700_000_000_i64..1_900_000_000,
            sub_a in 0_i64..NANOS_PER_SEC,
            sub_b in 0_i64..NANOS_PER_SEC,
        ) {
            let utc = |sub: i64| receipt_secs * NANOS_PER_SEC - IST_UTC_OFFSET_NANOS + sub;
            let a = routes_to_tick_quotes(ltt, utc(sub_a), true);
            prop_assert_eq!(a, routes_to_tick_quotes(ltt, utc(sub_b), true));
            prop_assert_eq!(
                a,
                receipt_secs >= TICK_QUOTES_FIRST_RECEIPT_IST_SECS
                    && receipt_secs - i64::from(ltt) > TICK_QUOTES_STALE_TRADE_SECS
            );
        }
    }
}

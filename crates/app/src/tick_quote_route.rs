//! Which table a main-feed packet's row goes to: `ticks` or `tick_quotes`
//! (plan item M5, owner Quote 41, 2026-10-27;
//! `websocket-connection-scope-lock.md` §"2026-10-27 — REPEAT QUOTES GET THEIR
//! OWN TABLE").
//!
//! A packet has **no new trade** when its last trade time and cumulative
//! volume equal the last ones this process saw for the same
//! `(security_id, segment)`, or when it is the first packet seen for that key
//! and its last trade time is before the session open (09:15 IST) of the day
//! it arrived. Such a packet goes to `tick_quotes`; every other packet goes to
//! `ticks` exactly as before.
//!
//! The per-key state is the tick-gap detector's, which the drain already
//! probes once for every packet ([`TickGapDetector::observe`] keeps the last
//! trade time and cumulative volume per composite key). So the route costs no
//! extra map lookup: one pure function over that verdict, O(1), no allocation.
//!
//! **Live and replay.** The detector sees every packet the drain sees, in the
//! drain's order, live and on a WAL replay (the boot and catch-up replays run
//! before the sockets dial, through the same `ingest_tick_at`), and the
//! first-packet test reads only the packet's own trade time and its WAL
//! receipt time. So a packet replayed after the packets before it gets the
//! same table it got live.
//!
//! **Honest limit (Risk).** A replay that starts part-way through a key's
//! stream (the earlier packets of that key are in segments the replay skips as
//! already applied, or a replay gap skipped them) judges that key's first
//! replayed packet by the first-packet test, without the state the live
//! process had. A repeat quote the live process sent to `tick_quotes` can then
//! be written again to `ticks`: a second row, never a lost one, and only when
//! the live row is also being replayed. The WAL carries no per-packet table
//! decision, so this cannot be closed without one.
//!
//! [`TickGapDetector::observe`]: tickvault_core::pipeline::tick_gap_detector::TickGapDetector::observe

use tickvault_common::constants::{IST_UTC_OFFSET_NANOS, MARKET_OPEN_IST_NANOS};
use tickvault_core::pipeline::tick_gap_detector::{LttVerdict, TickAssessment, VolumeVerdict};

/// Nanoseconds in one day.
const NANOS_PER_DAY: i64 = 86_400 * 1_000_000_000;

/// Nanoseconds in one second.
const NANOS_PER_SEC: i64 = 1_000_000_000;

/// `true` when the packet's row belongs in `tick_quotes`. Pure, O(1).
///
/// * `assessment` — the detector's verdict for this packet, `None` when the
///   segment code is unknown (no state is kept for it).
/// * `ltt_ist_secs` — the packet's last trade time, IST epoch seconds.
/// * `received_at_utc_nanos` — the frame's receipt time (UTC nanos, from the
///   WAL record on replay). `0` means unknown: the row cannot be stamped at
///   arrival, so it stays in `ticks`.
#[must_use]
pub fn routes_to_tick_quotes(
    assessment: Option<TickAssessment>,
    ltt_ist_secs: u32,
    received_at_utc_nanos: i64,
) -> bool {
    if received_at_utc_nanos == 0 {
        return false;
    }
    match assessment {
        Some(a) if !a.refused && a.ltt != LttVerdict::FirstSight => {
            a.ltt == LttVerdict::Repeated && a.volume == VolumeVerdict::Unchanged
        }
        // No earlier packet for this key (or no state could be kept for it).
        _ => trade_time_before_session_open(ltt_ist_secs, received_at_utc_nanos),
    }
}

/// `true` when `ltt_ist_secs` is before 09:15 IST of the IST day the frame
/// arrived on. Pure, O(1).
#[must_use]
pub fn trade_time_before_session_open(ltt_ist_secs: u32, received_at_utc_nanos: i64) -> bool {
    let receipt_ist = received_at_utc_nanos.saturating_add(IST_UTC_OFFSET_NANOS);
    let open = receipt_ist - receipt_ist.rem_euclid(NANOS_PER_DAY) + MARKET_OPEN_IST_NANOS;
    i64::from(ltt_ist_secs) * NANOS_PER_SEC < open
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-27 10:00:00 IST as UTC nanos (04:30 UTC).
    const RECEIPT_UTC: i64 = 1_793_075_400 * NANOS_PER_SEC;
    /// 2026-10-27 10:00:00 IST as IST epoch seconds.
    const TEN_AM_IST: u32 = 1_793_095_200;

    fn seen(ltt: LttVerdict, volume: VolumeVerdict) -> Option<TickAssessment> {
        Some(TickAssessment {
            ltt,
            volume,
            oi: tickvault_core::pipeline::tick_gap_detector::OiVerdict::Unchanged,
            refused: false,
        })
    }

    #[test]
    fn test_regression_repeat_quote_routes_to_tick_quotes() {
        assert!(routes_to_tick_quotes(
            seen(LttVerdict::Repeated, VolumeVerdict::Unchanged),
            TEN_AM_IST - 3_600,
            RECEIPT_UTC
        ));
    }

    #[test]
    fn test_new_trade_or_moved_trade_time_stays_in_ticks() {
        for (ltt, volume) in [
            (
                LttVerdict::Advanced { by_secs: 1 },
                VolumeVerdict::Advanced { delta: 5 },
            ),
            (LttVerdict::Repeated, VolumeVerdict::Advanced { delta: 5 }),
            (
                LttVerdict::Advanced { by_secs: 1 },
                VolumeVerdict::Unchanged,
            ),
            (
                LttVerdict::Backward { by_secs: 2 },
                VolumeVerdict::Unchanged,
            ),
            (
                LttVerdict::Repeated,
                VolumeVerdict::Regressed {
                    previous: 9,
                    current: 1,
                },
            ),
        ] {
            assert!(
                !routes_to_tick_quotes(seen(ltt, volume), TEN_AM_IST, RECEIPT_UTC),
                "{ltt:?} / {volume:?}"
            );
        }
    }

    #[test]
    fn test_first_packet_routes_by_trade_time_against_the_session_open() {
        let first = seen(LttVerdict::FirstSight, VolumeVerdict::FirstSight);
        let open = TEN_AM_IST - 45 * 60; // 09:15:00 IST
        assert!(routes_to_tick_quotes(first, open - 1, RECEIPT_UTC));
        assert!(!routes_to_tick_quotes(first, open, RECEIPT_UTC));
        assert!(!routes_to_tick_quotes(first, TEN_AM_IST, RECEIPT_UTC));
        // Yesterday's trade time is before today's open.
        assert!(routes_to_tick_quotes(
            first,
            TEN_AM_IST - 86_400,
            RECEIPT_UTC
        ));
        // Unknown segment (no detector state) and a refused observation take
        // the same first-packet test.
        assert!(routes_to_tick_quotes(None, open - 1, RECEIPT_UTC));
        assert!(!routes_to_tick_quotes(None, TEN_AM_IST, RECEIPT_UTC));
        let refused = Some(TickAssessment {
            ltt: LttVerdict::FirstSight,
            volume: VolumeVerdict::FirstSight,
            oi: tickvault_core::pipeline::tick_gap_detector::OiVerdict::FirstSight,
            refused: true,
        });
        assert!(routes_to_tick_quotes(refused, open - 1, RECEIPT_UTC));
    }

    #[test]
    fn test_routes_to_tick_quotes_unknown_receipt_stays_in_ticks() {
        assert!(!routes_to_tick_quotes(
            seen(LttVerdict::Repeated, VolumeVerdict::Unchanged),
            TEN_AM_IST,
            0
        ));
        assert!(!routes_to_tick_quotes(None, 0, 0));
    }

    #[test]
    fn test_trade_time_before_session_open_uses_the_ist_day_of_the_receipt() {
        // 2026-10-27 00:10 IST is still 26 Oct in UTC; the open is 27 Oct 09:15 IST.
        let just_after_ist_midnight = RECEIPT_UTC - (9 * 3_600 + 50 * 60) * NANOS_PER_SEC;
        let open = TEN_AM_IST - 45 * 60;
        assert!(trade_time_before_session_open(
            open - 1,
            just_after_ist_midnight
        ));
        assert!(!trade_time_before_session_open(
            open,
            just_after_ist_midnight
        ));
    }
}

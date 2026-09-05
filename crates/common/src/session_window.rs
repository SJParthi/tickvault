//! The persistence session window — one definition, used by every writer.
//!
//! # Why this module exists
//!
//! Operator requirement, 2026-09-05 (verbatim): *"clelary ensure to capture the
//! data starting 9 am till 3.39 pm alone only dude i mean even if we try to do
//! outisde of an yamkret horus chekc or whatver it is our ts and received at
//! shodu lbe always between 9 am and 3.39 pm"*.
//!
//! Two constants already named that window —
//! [`TICK_PERSIST_START_SECS_OF_DAY_IST`] (09:00) and
//! [`TICK_PERSIST_END_SECS_OF_DAY_IST`] (15:40, **exclusive**, so the last
//! accepted instant is 15:39:59.999999999) — and **nothing on any write path
//! read either of them.** `dhan_feed_stack.rs` says so in its own words:
//!
//! > "there is no persistence window GATE on this lane at all —
//! > `tick_persistence.rs` references neither constant (grep: zero hits). A row
//! > outside the window is written because NOTHING STOPS THE WRITER, not
//! > because a wider window permits it."
//!
//! So the window was documentation, not enforcement, and editing the constants
//! would have changed nothing. This module is the enforcement.
//!
//! # Why here and not in the tick writer
//!
//! `ticks` is not the only table with the problem — `market_depth` has no gate
//! either. Putting the decision in ONE pure function keeps the two writers from
//! drifting into different definitions of "the session", which is exactly how
//! the candle window and the persistence window came to be documented as
//! different when they are identical.
//!
//! # Complexity
//!
//! O(1) time, O(1) space: two integer divisions and two comparisons. No
//! allocation, no lookup, no branch on instrument. Safe on the hot path.

use crate::constants::{
    SECONDS_PER_DAY, TICK_PERSIST_END_SECS_OF_DAY_IST, TICK_PERSIST_START_SECS_OF_DAY_IST,
};

/// Nanoseconds in one second. Local rather than imported: `constants.rs` has no
/// such name today, and inventing a workspace-wide one for a single division
/// invites the next reader to reach for it where a `Duration` belongs.
const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// Why a row was refused, or that it was accepted.
///
/// A distinct reason per cause because the operator-facing question is never
/// "how many were refused" but "which kind" — a pre-open refusal is routine,
/// an unknown-receipt refusal means a WAL format we can no longer time-stamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowVerdict {
    /// Both stamps are inside [09:00, 15:40) IST. Write it.
    InWindow,
    /// The designated timestamp falls outside the window.
    TsOutOfWindow,
    /// The designated timestamp is fine but the RECEIPT clock is outside it.
    ///
    /// Reachable on a replay: a frame captured at 15:39 that is re-offered by a
    /// boot at 17:00 keeps its in-window `ts` and carries an out-of-window
    /// receipt only if the receipt was re-stamped. With TVW3+ records the
    /// original receipt is preserved, so this arm is the honest detector for a
    /// path that re-stamps when it should not.
    ReceivedAtOutOfWindow,
}

impl WindowVerdict {
    /// The metric label for this verdict. `&'static str` so the counter is
    /// allocation-free on the hot path.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::InWindow => "in_window",
            Self::TsOutOfWindow => "ts_out_of_window",
            Self::ReceivedAtOutOfWindow => "received_at_out_of_window",
        }
    }

    /// True when the row must NOT be written.
    ///
    /// # Only the DESIGNATED timestamp refuses. The receipt never does.
    ///
    /// CORRECTED 2026-09-05 by an adversarial review, and the first version of
    /// this function was WRONG in the most expensive possible direction.
    ///
    /// It returned `true` for [`Self::ReceivedAtOutOfWindow`] too. The two
    /// clocks differ by DELIVERY LAG, and this repository's own measured Dhan
    /// lag (`websocket-connection-scope-lock.md` §E, 2026-07-06) is p50
    /// **1.38 s**, p95 **14.93 s**, p99 **46.37 s**, max **198.69 s**. So a
    /// trade stamped by the exchange at 15:39:30 -- squarely in window, and
    /// part of the closing-auction stretch -- that reached us at 15:40:05 was
    /// REFUSED, silently and permanently: the caller returns `Ok(())` and does
    /// not mark the frame unapplied, so no replay ever re-offers it.
    ///
    /// At the measured p99 that discards the last ~46 seconds of every
    /// session for the slowest 1% of ticks; at the measured max, the last
    /// ~3.3 minutes. The module doc above warns in as many words against
    /// discarding "the closing-auction prints" -- and the receipt leg
    /// discarded them anyway.
    ///
    /// The distinction that fixes it: `ts` says WHAT THE ROW IS -- an event
    /// that happened at that instant -- while `received_at` says how fast our
    /// network was. A real print must never be dropped because the vendor was
    /// slow, which is the operator's first principle ("not even a single tick
    /// should be missed"). So the receipt is COUNTED and LOGGED, never
    /// refused: see [`Self::is_noteworthy`].
    ///
    /// The operator's rule was "ts and received_at always between 9 am and
    /// 3.39 pm". Its purpose is to stop OUT-OF-SESSION data polluting the
    /// tables -- a restart at 18:00, a replay the next morning -- and `ts`
    /// alone achieves that completely, because `ts` is the designated
    /// timestamp every partition, query and retention sweep keys on.
    #[must_use]
    pub const fn is_refusal(self) -> bool {
        matches!(self, Self::TsOutOfWindow)
    }

    /// True when the verdict is worth counting -- a refusal, OR an accepted
    /// row whose receipt fell outside the window.
    ///
    /// Separate from [`Self::is_refusal`] so the late-arrival case stays
    /// VISIBLE without being destructive. A rising
    /// `received_at_out_of_window` count is the honest signal that the vendor
    /// is delivering the close late; it is not a reason to delete the close.
    #[must_use]
    pub const fn is_noteworthy(self) -> bool {
        !matches!(self, Self::InWindow)
    }
}

/// True when an IST epoch-nanosecond stamp falls inside [09:00, 15:40) IST.
///
/// The end is EXCLUSIVE, which is what makes this "till 3.39 pm": the last
/// accepted instant is 15:39:59.999999999. Do not "fix" 56_400 to 56_340
/// thinking it reads 15:39 — that would discard the entire 15:39 minute,
/// including the closing-auction prints.
#[must_use]
pub fn nanos_in_session_window(ist_nanos: i64) -> bool {
    if ist_nanos < 0 {
        // A negative IST stamp is a pre-1970 clock or a corrupt widening.
        // Refuse rather than let a modulo produce a plausible seconds-of-day.
        return false;
    }
    let secs_of_day = (ist_nanos / NANOS_PER_SECOND) % i64::from(SECONDS_PER_DAY);
    secs_of_day >= i64::from(TICK_PERSIST_START_SECS_OF_DAY_IST)
        && secs_of_day < i64::from(TICK_PERSIST_END_SECS_OF_DAY_IST)
}

/// The window verdict for a row carrying a designated timestamp and an
/// OPTIONAL receipt timestamp, both IST epoch nanoseconds.
///
/// # The `None` receipt decision, stated rather than buried
///
/// `received_at` is `None` for rows replayed from pre-TVW3 WAL records, which
/// carry no receipt clock at all. A NULL is not evidence that the row is out of
/// window — it is evidence that we cannot tell.
///
/// This returns [`WindowVerdict::InWindow`] for that case (given an in-window
/// `ts`), i.e. it does NOT refuse. Refusing would discard real ticks from older
/// segments to enforce a rule about a value that does not exist, which trades
/// certain data loss for a hypothetical. The rows are still distinguishable
/// afterwards — `received_at` is NULL in the table — so the operator can tighten
/// this to a refusal later without losing the ability to find them.
#[must_use]
pub fn verdict(ts_ist_nanos: i64, received_at_ist_nanos: Option<i64>) -> WindowVerdict {
    if !nanos_in_session_window(ts_ist_nanos) {
        return WindowVerdict::TsOutOfWindow;
    }
    match received_at_ist_nanos {
        Some(r) if !nanos_in_session_window(r) => WindowVerdict::ReceivedAtOutOfWindow,
        _ => WindowVerdict::InWindow,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// IST epoch nanos for a given seconds-of-day, on an arbitrary day.
    fn at(secs_of_day: i64) -> i64 {
        let day: i64 = 20_000;
        (day * i64::from(SECONDS_PER_DAY) + secs_of_day) * NANOS_PER_SECOND
    }

    #[test]
    fn nanos_in_session_window_opens_at_0900_exactly() {
        assert!(
            !nanos_in_session_window(at(9 * 3600 - 1)),
            "08:59:59 is out"
        );
        assert!(nanos_in_session_window(at(9 * 3600)), "09:00:00 is in");
    }

    #[test]
    fn nanos_in_session_window_last_accepted_instant_is_1539_59() {
        // This is the assertion that stops someone "fixing" 56_400 to 56_340.
        assert!(
            nanos_in_session_window(at(15 * 3600 + 39 * 60)),
            "15:39:00 must be IN -- the operator asked for data 'till 3.39 pm'"
        );
        assert!(
            nanos_in_session_window(at(15 * 3600 + 39 * 60 + 59)),
            "15:39:59 must be IN -- the end is EXCLUSIVE at 15:40"
        );
        assert!(
            !nanos_in_session_window(at(15 * 3600 + 40 * 60)),
            "15:40:00 must be OUT"
        );
    }

    #[test]
    fn nanos_in_session_window_refuses_a_negative_stamp_rather_than_moduloing_it() {
        // -1 ns would modulo to a seconds-of-day of 0 or 86_399 depending on
        // sign rules; neither is a truth about the row. Refuse.
        assert!(!nanos_in_session_window(-1));
        assert!(!nanos_in_session_window(i64::MIN));
    }

    #[test]
    fn verdict_classifies_each_clock_independently() {
        let good = at(10 * 3600);
        let bad = at(16 * 3600);
        assert_eq!(verdict(good, Some(good)), WindowVerdict::InWindow);
        assert_eq!(verdict(bad, Some(good)), WindowVerdict::TsOutOfWindow);
        assert_eq!(
            verdict(good, Some(bad)),
            WindowVerdict::ReceivedAtOutOfWindow,
            "a late receipt is REPORTED distinctly -- it is counted, not refused"
        );
    }

    #[test]
    fn verdict_treats_a_null_receipt_as_unknown_not_as_a_refusal() {
        // The documented decision: NULL means "cannot tell", not "out".
        assert_eq!(verdict(at(10 * 3600), None), WindowVerdict::InWindow);
        // ...but a bad ts is still refused with no receipt to lean on.
        assert_eq!(verdict(at(2 * 3600), None), WindowVerdict::TsOutOfWindow);
    }

    /// The label strings are an OPERATOR-FACING contract: they are the
    /// `reason` dimension on `tv_ticks_out_of_window_refused_total`, so a
    /// rename silently splits one series into two and the old one goes flat
    /// rather than to zero -- which reads as "the problem stopped".
    #[test]
    fn reason_strings_are_stable_and_distinct() {
        assert_eq!(WindowVerdict::InWindow.reason(), "in_window");
        assert_eq!(WindowVerdict::TsOutOfWindow.reason(), "ts_out_of_window");
        assert_eq!(
            WindowVerdict::ReceivedAtOutOfWindow.reason(),
            "received_at_out_of_window"
        );
        let all = [
            WindowVerdict::InWindow.reason(),
            WindowVerdict::TsOutOfWindow.reason(),
            WindowVerdict::ReceivedAtOutOfWindow.reason(),
        ];
        for (i, a) in all.iter().enumerate() {
            for b in all.iter().skip(i + 1) {
                assert_ne!(a, b, "two verdicts must never share a metric label");
            }
        }
    }

    /// The never-running test that encoded the DEFECT.
    ///
    /// Until 2026-09-05 this function had NO `#[test]` attribute -- a stray
    /// duplicate `#[test]` sat above the previous test instead, so that one
    /// ran twice and this one never ran at all. It asserted
    /// `ReceivedAtOutOfWindow.is_refusal()`, which is exactly the behaviour an
    /// adversarial review then proved was destroying the close of every
    /// session. A test that does not run cannot be wrong out loud; it is just
    /// wrong quietly, and it took an outside reader to notice.
    #[test]
    fn only_the_designated_timestamp_refuses_a_row() {
        assert!(!WindowVerdict::InWindow.is_refusal());
        assert!(WindowVerdict::TsOutOfWindow.is_refusal());
        assert!(
            !WindowVerdict::ReceivedAtOutOfWindow.is_refusal(),
            "a late RECEIPT must never delete a real print -- the two clocks \
             differ by delivery lag, measured p99 46.37s on this very feed"
        );
        // ...but it must still be COUNTED, or the lateness is invisible.
        assert!(!WindowVerdict::InWindow.is_noteworthy());
        assert!(WindowVerdict::TsOutOfWindow.is_noteworthy());
        assert!(WindowVerdict::ReceivedAtOutOfWindow.is_noteworthy());
    }

    /// The closing-auction scenario, in the numbers this repository measured.
    ///
    /// Dhan lag on 2026-07-06: p50 1.38s, p95 14.93s, p99 46.37s, max
    /// 198.69s (`websocket-connection-scope-lock.md` §E). A 15:39:30 print
    /// delivered 35 seconds late lands at 15:40:05 -- past the window's
    /// exclusive end. It MUST still be written.
    #[test]
    fn a_late_delivered_closing_print_is_kept_not_discarded() {
        let event = at(15 * 3600 + 39 * 60 + 30); // 15:39:30, in window
        let arrival = at(15 * 3600 + 40 * 60 + 5); // 15:40:05, out of window
        let v = verdict(event, Some(arrival));
        assert_eq!(v, WindowVerdict::ReceivedAtOutOfWindow);
        assert!(
            !v.is_refusal(),
            "refusing this discards the closing auction whenever the vendor \
             is >30s late, which the measured p99 says happens every session"
        );
        assert!(
            v.is_noteworthy(),
            "and it must be counted so lateness shows"
        );
    }

    /// The pollution case the operator's rule actually exists to stop.
    #[test]
    fn an_out_of_session_event_is_still_refused_however_it_arrived() {
        // A restart at 18:00 replaying an 18:00 event: ts decides, and refuses.
        assert!(verdict(at(18 * 3600), Some(at(18 * 3600))).is_refusal());
        // Even if it somehow arrived inside the window, the EVENT is out.
        assert!(verdict(at(18 * 3600), Some(at(10 * 3600))).is_refusal());
    }

    #[test]
    fn nanos_in_session_window_uses_the_constants_that_name_the_window() {
        // Anti-drift: if someone edits either constant, this test says so
        // rather than the window silently moving under every writer.
        assert_eq!(TICK_PERSIST_START_SECS_OF_DAY_IST, 32_400, "09:00");
        assert_eq!(TICK_PERSIST_END_SECS_OF_DAY_IST, 56_400, "15:40 exclusive");
    }
}

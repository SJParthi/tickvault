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
    #[must_use]
    pub const fn is_refusal(self) -> bool {
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
    fn verdict_requires_both_stamps_in_window() {
        let good = at(10 * 3600);
        let bad = at(16 * 3600);
        assert_eq!(verdict(good, Some(good)), WindowVerdict::InWindow);
        assert_eq!(verdict(bad, Some(good)), WindowVerdict::TsOutOfWindow);
        assert_eq!(
            verdict(good, Some(bad)),
            WindowVerdict::ReceivedAtOutOfWindow,
            "an in-window ts does NOT excuse an out-of-window receipt"
        );
    }

    #[test]
    fn verdict_treats_a_null_receipt_as_unknown_not_as_a_refusal() {
        // The documented decision: NULL means "cannot tell", not "out".
        assert_eq!(verdict(at(10 * 3600), None), WindowVerdict::InWindow);
        // ...but a bad ts is still refused with no receipt to lean on.
        assert_eq!(verdict(at(2 * 3600), None), WindowVerdict::TsOutOfWindow);
    }

    #[test]
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

    fn is_refusal_and_reason_agree_on_every_variant() {
        assert!(!WindowVerdict::InWindow.is_refusal());
        assert!(WindowVerdict::TsOutOfWindow.is_refusal());
        assert!(WindowVerdict::ReceivedAtOutOfWindow.is_refusal());
        assert_eq!(WindowVerdict::InWindow.reason(), "in_window");
        assert_eq!(WindowVerdict::TsOutOfWindow.reason(), "ts_out_of_window");
        assert_eq!(
            WindowVerdict::ReceivedAtOutOfWindow.reason(),
            "received_at_out_of_window"
        );
    }

    #[test]
    fn nanos_in_session_window_uses_the_constants_that_name_the_window() {
        // Anti-drift: if someone edits either constant, this test says so
        // rather than the window silently moving under every writer.
        assert_eq!(TICK_PERSIST_START_SECS_OF_DAY_IST, 32_400, "09:00");
        assert_eq!(TICK_PERSIST_END_SECS_OF_DAY_IST, 56_400, "15:40 exclusive");
    }
}

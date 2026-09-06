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
    ARRIVAL_GRACE_TAIL_SECS, MUHURAT_PERSIST_END_SECS_OF_DAY_IST,
    MUHURAT_PERSIST_START_SECS_OF_DAY_IST, SECONDS_PER_DAY, TICK_PERSIST_END_SECS_OF_DAY_IST,
    TICK_PERSIST_START_SECS_OF_DAY_IST,
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

/// Seconds-of-day for an IST epoch-nanosecond stamp, or `None` if the stamp is
/// negative.
///
/// A negative IST stamp is a pre-1970 clock or a corrupt widening. It is
/// rejected here rather than left to a modulo, which on a negative input would
/// produce a plausible-looking seconds-of-day and let a corrupt row through.
fn secs_of_day(ist_nanos: i64) -> Option<i64> {
    if ist_nanos < 0 {
        return None;
    }
    Some((ist_nanos / NANOS_PER_SECOND) % i64::from(SECONDS_PER_DAY))
}

/// True when an IST epoch-nanosecond stamp falls inside the MUHURAT evening
/// session, [18:00, 19:30) IST.
///
/// NSE trades a ceremonial ~1-hour session on Diwali. The box connects for it —
/// `main.rs` computes `is_muhurat` from the calendar and installs it via
/// [`crate::muhurat::init_muhurat_session`] — so without this the whole session
/// would connect, receive, and persist nothing: a live connection storing zero
/// data, which is the false-OK class the charter forbids.
#[must_use]
pub fn nanos_in_muhurat_window(ist_nanos: i64) -> bool {
    match secs_of_day(ist_nanos) {
        None => false,
        Some(s) => {
            s >= i64::from(MUHURAT_PERSIST_START_SECS_OF_DAY_IST)
                && s < i64::from(MUHURAT_PERSIST_END_SECS_OF_DAY_IST)
        }
    }
}

/// True when the stamp is inside ANY window open on this day — the regular
/// session always, plus the Muhurat evening session when `muhurat_active`.
///
/// PURE by design: the flag is a parameter, never a global read, so every
/// window decision stays unit-testable in isolation and cannot be perturbed by
/// another test in the same process setting the boot `OnceLock`. The global is
/// read exactly once, in [`row_is_in_an_open_window`], which is what the
/// writers call.
#[must_use]
pub fn nanos_in_any_open_window(ist_nanos: i64, muhurat_active: bool) -> bool {
    nanos_in_session_window(ist_nanos) || (muhurat_active && nanos_in_muhurat_window(ist_nanos))
}

/// The window predicate a WRITER should call: the regular session, widened to
/// the Muhurat evening session on a Muhurat day.
///
/// Reads the boot-installed flag ([`crate::muhurat::current`]) so a future
/// fourth writer cannot silently be Muhurat-blind by calling the narrow form.
/// That read is one `OnceLock` load — O(1), no allocation, safe on the hot
/// path; the DHAT gates on the tick seam and the depth append cover it.
///
/// Off a Muhurat day the flag is `false` and this is exactly
/// [`nanos_in_session_window`].
#[must_use]
pub fn row_is_in_an_open_window(ist_nanos: i64) -> bool {
    nanos_in_any_open_window(ist_nanos, crate::muhurat::current())
}

/// True when an ARRIVAL-clocked stamp falls inside an open window, allowing a
/// bounded [`ARRIVAL_GRACE_TAIL_SECS`] tail past the window's end.
///
/// # Why a second predicate rather than widening the first
///
/// The two are asking different questions and must keep different answers.
///
/// [`nanos_in_any_open_window`] asks *"did this EVENT happen inside the
/// session?"* — the operator's 2026-09-05 rule, and it is exactly right for a
/// tick carrying a real exchange timestamp. Widening it would admit trades the
/// exchange itself stamped after the close.
///
/// This one asks *"did this row REACH US near enough to the session that the
/// thing it describes was inside it?"* — the only question available for a row
/// whose single clock is the receipt. `market_depth` is that row: its
/// designated `ts` IS the arrival instant, because the depth protocol carries
/// no exchange timestamp field at all.
///
/// Without this, a depth snapshot of the 15:39 book that the vendor delivered
/// at 15:40:05 was refused — silently and permanently, because the depth writer
/// returns `Ok(())` on a window refusal (deliberately, so a refused pre-open
/// frame is not re-offered forever) and therefore never marks the frame
/// unapplied. At this repository's measured p99 delivery lag of 46.37 s that
/// discards the tail of every session for the slowest 1% of depth frames; at
/// the measured max of 198.69 s, the last ~3.3 minutes.
///
/// # What it still refuses, which is the half that matters
///
/// The START is NOT graced. A frame arriving at 08:58 is genuinely pre-open —
/// delivery lag makes a row LATE, never EARLY, so a grace at the front would
/// only admit pre-open noise.
///
/// An evening restart at 18:00 is 2h20m past the graced end and is refused
/// exactly as before. The const-asserts in `constants.rs` pin that the tail can
/// never reach the Muhurat window's 18:00 start.
///
/// # Complexity
///
/// O(1) time, O(1) space — one division, two comparisons per window, no
/// allocation. Same envelope as [`nanos_in_any_open_window`]; safe on the hot
/// path and covered by the depth append DHAT gate.
#[must_use]
pub fn nanos_in_any_open_window_with_arrival_grace(ist_nanos: i64, muhurat_active: bool) -> bool {
    let Some(s) = secs_of_day(ist_nanos) else {
        return false;
    };
    let grace = i64::from(ARRIVAL_GRACE_TAIL_SECS);
    let in_regular = s >= i64::from(TICK_PERSIST_START_SECS_OF_DAY_IST)
        && s < i64::from(TICK_PERSIST_END_SECS_OF_DAY_IST) + grace;
    let in_muhurat = muhurat_active
        && s >= i64::from(MUHURAT_PERSIST_START_SECS_OF_DAY_IST)
        && s < i64::from(MUHURAT_PERSIST_END_SECS_OF_DAY_IST) + grace;
    in_regular || in_muhurat
}

/// The window predicate an ARRIVAL-CLOCKED writer should call — the graced form
/// of [`row_is_in_an_open_window`], reading the boot-installed Muhurat flag.
///
/// `market_depth` is the caller today. A future writer whose only timestamp is
/// a receipt should call THIS one; a writer with a real exchange stamp must
/// keep calling [`row_is_in_an_open_window`], because grace on an event clock
/// admits genuinely out-of-session events.
#[must_use]
pub fn arrival_row_is_in_an_open_window(ist_nanos: i64) -> bool {
    nanos_in_any_open_window_with_arrival_grace(ist_nanos, crate::muhurat::current())
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
    verdict_in(
        ts_ist_nanos,
        received_at_ist_nanos,
        crate::muhurat::current(),
    )
}

/// The pure form of [`verdict`] — the Muhurat flag as a parameter, no global.
///
/// Every test in this module uses THIS one. The `OnceLock` behind
/// [`crate::muhurat::current`] is set once per process, so a test that read it
/// would be at the mercy of whichever other test in the same binary installed
/// it first — an order-dependent window decision, which is worse than no test.
#[must_use]
pub fn verdict_in(
    ts_ist_nanos: i64,
    received_at_ist_nanos: Option<i64>,
    muhurat_active: bool,
) -> WindowVerdict {
    if !nanos_in_any_open_window(ts_ist_nanos, muhurat_active) {
        return WindowVerdict::TsOutOfWindow;
    }
    match received_at_ist_nanos {
        Some(r) if !nanos_in_any_open_window(r, muhurat_active) => {
            WindowVerdict::ReceivedAtOutOfWindow
        }
        _ => WindowVerdict::InWindow,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every test below asks the window question on a NON-Muhurat day, which
    /// is 364 days of the year and the only shape whose expectations are
    /// stable. Routed through the pure form on purpose: reading the boot
    /// `OnceLock` here would make each assertion depend on whether some other
    /// test in this binary installed the flag first, and an order-dependent
    /// window decision is worse than no test at all.
    fn verdict(ts_ist_nanos: i64, received_at_ist_nanos: Option<i64>) -> WindowVerdict {
        verdict_in(ts_ist_nanos, received_at_ist_nanos, false)
    }

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
    ///
    /// ## ⚠ Scope of "kept", corrected 2026-09-05 by an adversarial sweep
    ///
    /// This keeps the print in `ticks`. It does **not** put it in a candle.
    /// The two paths read DIFFERENT CLOCKS for the same decision:
    ///
    /// | | window | clock |
    /// |---|---|---|
    /// | this gate | `[09:00, 15:40)` | the EXCHANGE stamp, `row.ts_ist_nanos` |
    /// | the fold (`MultiTfAggregator::consume`) | `[09:00, 15:40)` — identical | `tf_index::fold_clock_ist_secs`, which PREFERS the receipt when it is within ±300 s |
    ///
    /// So for exactly this tick the fold clock is the 15:40:05 receipt, which
    /// is `>= MARKET_CLOSE_SECS_OF_DAY_IST`, and the aggregator returns
    /// `out_of_session`: the print lands in `ticks` and is absent from the
    /// 15:39 bar of every timeframe. The converse holds too — a receipt
    /// running AHEAD of a just-out-of-window exchange stamp is folded into a
    /// candle while this gate refuses the tick.
    ///
    /// ⚠ AND IT IS NOT A SILENT LOSS — verified in source 2026-09-06, because
    /// an earlier version of this note omitted the half that matters and so
    /// read like an unhandled gap. `MultiTfAggregator::consume` returning
    /// `out_of_session` is a CANDLE-ONLY refusal in `dhan_feed_stack`:
    /// `hard_refusal` is `refused_price || refused_timestamp` and nothing
    /// else, so the tick falls through to `append_tick_with_seq` and the ROW
    /// IS WRITTEN, then counted as
    /// `tv_aggregator_tick_refused_total{reason="out_of_session"}` and
    /// reported as a delta by the 30-second `AGGREGATOR-DROP-01` line. The
    /// divergence is therefore observable, bounded and deliberate: the tick
    /// is in `ticks`, absent from the bar, and a counter says how often.
    ///
    /// The affected band is bounded on BOTH sides, which is why it is small.
    /// The fold falls back to the exchange stamp once the lag exceeds
    /// `MAX_PLAUSIBLE_RECEIPT_LAG_SECS` (300 s), so only an exchange stamp in
    /// `[15:35:00, 15:40:00)` with a receipt at or past 15:40 diverges at all;
    /// at Dhan's measured p99 lag of 46 s the band actually reached is about
    /// `[15:39:14, 15:40:00)`. A lag beyond 300 s folds correctly.
    ///
    /// Deliberately NOT reconciled here. Making the two agree means choosing
    /// one clock for both, which is plan item W1b/W2 ("candles bucket on
    /// `received_at`"): W1b is REMAINING with its design settled, and W2 is
    /// blocked on W1b. (An earlier version of this note called W1b itself
    /// "blocked", which it is not.) Widening this change to settle it would
    /// alter candle bucketing on a path the operator has separately scoped,
    /// and the 2026-08-28 receipt-clock directive is what puts the receipt in
    /// the fold clock in the first place — so the current behaviour is that
    /// directive's own consequence, not a defect against it.
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

    #[test]
    fn nanos_in_muhurat_window_is_1800_to_1930_exclusive() {
        assert!(
            !nanos_in_muhurat_window(at(18 * 3600 - 1)),
            "17:59:59 is out"
        );
        assert!(nanos_in_muhurat_window(at(18 * 3600)), "18:00:00 is in");
        assert!(
            nanos_in_muhurat_window(at(19 * 3600 + 30 * 60 - 1)),
            "19:29:59 is in"
        );
        assert!(
            !nanos_in_muhurat_window(at(19 * 3600 + 30 * 60)),
            "19:30:00 is OUT — the end is exclusive, exactly like the regular window"
        );
        assert!(
            !nanos_in_muhurat_window(-1),
            "a negative stamp is refused before any modulo can make it look plausible"
        );
    }

    #[test]
    fn nanos_in_any_open_window_never_sees_the_two_windows_overlap() {
        // The regular session ends 15:40 and Muhurat opens 18:00. If these ever
        // touched, a row inside both would be accepted for the wrong reason and
        // an operator reading `reason` would be told the wrong session.
        for s in [
            15 * 3600 + 39 * 60 + 59,
            15 * 3600 + 40 * 60,
            17 * 3600,
            18 * 3600,
        ] {
            let regular = nanos_in_session_window(at(s));
            let muhurat = nanos_in_muhurat_window(at(s));
            assert!(
                !(regular && muhurat),
                "seconds-of-day {s} sits in BOTH windows"
            );
        }
    }

    #[test]
    fn row_is_in_an_open_window_refuses_a_muhurat_evening_row_on_an_ordinary_day() {
        // THE regression this closes. Diwali evening, 18:30 IST: the box
        // connects (main.rs widens `should_connect_ws` for Muhurat), frames
        // arrive, and every one of them would be refused by a window that only
        // knows 09:00-15:39 -- a live connection persisting nothing, which is
        // the false-OK the charter forbids.
        let evening = at(18 * 3600 + 30 * 60);

        assert!(
            !nanos_in_any_open_window(evening, false),
            "on an ordinary day 18:30 is out of window and must stay out"
        );
        assert!(
            nanos_in_any_open_window(evening, true),
            "on a Muhurat day 18:30 must be ACCEPTED"
        );

        assert_eq!(
            verdict_in(evening, Some(evening), false),
            WindowVerdict::TsOutOfWindow
        );
        assert_eq!(
            verdict_in(evening, Some(evening), true),
            WindowVerdict::InWindow
        );
    }

    #[test]
    fn verdict_in_widens_with_the_muhurat_flag_and_never_narrows() {
        // A Muhurat day must not cost the regular session. The flag is a
        // widening only -- `nanos_in_any_open_window` is an OR whose first
        // term is the regular window, so this holds by construction; the test
        // exists so a future rewrite into a match on the flag cannot silently
        // trade one session for the other.
        for s in [
            9 * 3600,
            12 * 3600,
            15 * 3600 + 39 * 60 + 59,
            18 * 3600,
            19 * 3600,
        ] {
            let narrow = nanos_in_any_open_window(at(s), false);
            let wide = nanos_in_any_open_window(at(s), true);
            assert!(
                !narrow || wide,
                "seconds-of-day {s} was accepted on an ordinary day and REFUSED on a Muhurat day"
            );
        }
    }

    // -----------------------------------------------------------------------
    // ARRIVAL GRACE TAIL
    // -----------------------------------------------------------------------

    /// Shorthand: the graced predicate on an ordinary (non-Muhurat) day.
    fn arrival(secs_of_day: i64) -> bool {
        nanos_in_any_open_window_with_arrival_grace(at(secs_of_day), false)
    }

    #[test]
    fn arrival_grace_accepts_the_tail_and_stops_at_its_end() {
        let end = i64::from(TICK_PERSIST_END_SECS_OF_DAY_IST);
        let grace = i64::from(ARRIVAL_GRACE_TAIL_SECS);

        // The instant the ungraced window closes -- the one this whole change
        // exists for. A depth snapshot of the 15:39 book delivered at 15:40:00.
        assert!(
            arrival(end),
            "15:40:00 exactly must be admitted for an arrival-clocked row: it \
             is the first instant the old gate refused, and the vendor's \
             measured p99 lag alone puts real 15:39 book state here"
        );
        // Past the measured worst-case Dhan lag of 198.69 s, still inside.
        assert!(
            arrival(end + 199),
            "the measured 198.69 s worst-case delivery lag must be inside the \
             grace, or the grace does not cover the case it was sized for"
        );
        // The last graced instant.
        assert!(
            arrival(end + grace - 1),
            "the final graced second must be in"
        );
        // And the first instant past it, which must close.
        assert!(
            !arrival(end + grace),
            "the grace END is EXCLUSIVE. An open-ended tail is not a grace, it \
             is the absence of a window"
        );
    }

    #[test]
    fn arrival_grace_never_admits_an_evening_restart() {
        // The junk class the window exists to exclude, and the reason the tail
        // is 240s rather than a comfortable round hour.
        for s in [
            16 * 3600,           // 16:00, an after-close deploy
            17 * 3600 + 30 * 60, // 17:30, the scheduled box stop
            18 * 3600,           // 18:00, a restart -- and the Muhurat start
            23 * 3600,           // 23:00, an overnight batch
        ] {
            assert!(
                !arrival(s),
                "seconds-of-day {s} is hours outside the session and must be \
                 refused even with the arrival grace"
            );
        }
    }

    #[test]
    fn arrival_grace_is_never_applied_to_the_window_start() {
        // Delivery lag makes a row LATE, never EARLY. A grace at the front
        // would admit pre-open noise while fixing nothing.
        let start = i64::from(TICK_PERSIST_START_SECS_OF_DAY_IST);
        assert!(
            !arrival(start - 1),
            "08:59:59 is pre-open and must stay out"
        );
        assert!(
            !arrival(start - i64::from(ARRIVAL_GRACE_TAIL_SECS)),
            "the grace must not be mirrored onto the window start"
        );
        assert!(
            arrival(start),
            "09:00:00 itself is in window, graced or not"
        );
    }

    #[test]
    fn arrival_grace_does_not_widen_the_event_clock_path() {
        // The load-bearing separation. `nanos_in_any_open_window` answers the
        // operator's 2026-09-05 rule about the EVENT clock and must be
        // completely unaffected: a trade the exchange stamped at 15:41 is still
        // out of session, however fast it reached us.
        let end = i64::from(TICK_PERSIST_END_SECS_OF_DAY_IST);
        for offset in [0, 60, 199, i64::from(ARRIVAL_GRACE_TAIL_SECS) - 1] {
            assert!(
                !nanos_in_any_open_window(at(end + offset), false),
                "the ungraced predicate accepted {offset}s past the close -- \
                 the grace has leaked onto the event clock, which admits \
                 genuinely out-of-session trades"
            );
        }
    }

    #[test]
    fn arrival_grace_widens_the_muhurat_window_too_and_only_when_active() {
        let m_end = i64::from(MUHURAT_PERSIST_END_SECS_OF_DAY_IST);
        assert!(
            nanos_in_any_open_window_with_arrival_grace(at(m_end), true),
            "a Muhurat depth frame is arrival-clocked exactly like a regular \
             one, so the ceremonial session's close needs the same grace"
        );
        assert!(
            !nanos_in_any_open_window_with_arrival_grace(at(m_end), false),
            "the Muhurat tail must not open on an ordinary day"
        );
    }

    #[test]
    fn arrival_grace_still_refuses_a_negative_stamp() {
        // A pre-1970 clock or a corrupt widening. `secs_of_day` rejects it
        // before any arithmetic, and the grace must not reopen that door.
        assert!(!nanos_in_any_open_window_with_arrival_grace(-1, false));
        assert!(!nanos_in_any_open_window_with_arrival_grace(i64::MIN, true));
    }

    #[test]
    fn arrival_grace_is_a_strict_superset_of_the_ungraced_window() {
        // Every instant the event-clock predicate accepts, the arrival one must
        // accept too. A future rewrite that made the graced form a different
        // range rather than a widened one would silently start refusing
        // mid-session depth, which no counter would distinguish from a quiet
        // market.
        for s in (0..i64::from(SECONDS_PER_DAY)).step_by(37) {
            for muhurat in [false, true] {
                if nanos_in_any_open_window(at(s), muhurat) {
                    assert!(
                        nanos_in_any_open_window_with_arrival_grace(at(s), muhurat),
                        "seconds-of-day {s} (muhurat={muhurat}) is accepted by \
                         the event-clock window and refused by the arrival one"
                    );
                }
            }
        }
    }

    #[test]
    fn nanos_in_any_open_window_with_arrival_grace_admits_only_the_tail_it_names() {
        // Named for the function so the pub-fn guard can see its test, and
        // written as the one-line contract: the graced window is the ungraced
        // one plus exactly ARRIVAL_GRACE_TAIL_SECS at the END, and nothing else
        // moves.
        let end = i64::from(TICK_PERSIST_END_SECS_OF_DAY_IST);
        let grace = i64::from(ARRIVAL_GRACE_TAIL_SECS);
        for (sod, want) in [
            (i64::from(TICK_PERSIST_START_SECS_OF_DAY_IST) - 1, false),
            (i64::from(TICK_PERSIST_START_SECS_OF_DAY_IST), true),
            (end - 1, true),
            (end, true),
            (end + grace - 1, true),
            (end + grace, false),
        ] {
            assert_eq!(
                nanos_in_any_open_window_with_arrival_grace(at(sod), false),
                want,
                "seconds-of-day {sod}: the graced window is [09:00, 15:40+{grace})"
            );
        }
    }

    #[test]
    fn arrival_row_is_in_an_open_window_agrees_with_the_pure_form() {
        // The wrapper's only job is to supply the boot Muhurat flag. Asserting
        // a fixed expectation here would make this test depend on whichever
        // other test in this binary installed the `OnceLock` first, so instead
        // it asserts the property that actually matters and is order-blind:
        // the wrapper must not narrow, widen or otherwise differ from the pure
        // form evaluated with the SAME flag.
        let flag = crate::muhurat::current();
        for sod in [
            0_i64, 32_399, 32_400, 56_399, 56_400, 56_639, 56_640, 64_800, 86_399,
        ] {
            assert_eq!(
                arrival_row_is_in_an_open_window(at(sod)),
                nanos_in_any_open_window_with_arrival_grace(at(sod), flag),
                "seconds-of-day {sod}: the wrapper disagreed with the pure form \
                 it delegates to, so a writer calling it gets a different window \
                 than the one this module's tests pin"
            );
        }
    }
}

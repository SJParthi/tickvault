//! The trade-safety switch (plan item UTC-SAFETY, daily-universe §28.7, owner
//! card "Yes, after this merges (Recommended)", 2026-11-21).
//!
//! While today's `DHAN-FEED-TIME-01` alarm is on (cause A: index prices carry
//! a time outside the session; cause B: no candle for 10 minutes), the paper
//! path opens no NEW entries. Exits always pass. It followed 2026-11-20, when
//! Dhan sent every trade time as UTC and no candle formed until about 2:52 PM:
//! a strategy fed those candles must not open trades on them.
//!
//! # The switch
//!
//! One process-wide word, [`SWITCH`]: `((day + 1) << 8) | causes`, or `0`
//! before anything engaged it. `day` is the IST day number (days since
//! 1970-01-01 IST) the alarm belongs to. One word, so a reader never pairs a
//! new day with old causes. [`SafetySwitch::engage`] ORs causes into the
//! same day and REPLACES another day (the latest engage wins): a clock step
//! forward that stamps D+1 cannot hide a later real alarm on D, which a
//! "highest day wins" rule would. [`SafetySwitch::blocked_on`] is one Acquire
//! load and one compare. Because it is keyed by day it ends by itself at the
//! IST day roll; nothing clears it at 16:00.
//!
//! # Who sets it
//!
//! - The feed-time page's `fire` (on the frame drain's 30 s arm): one CAS,
//!   before its `error!` and before the hand-off to its writer thread, so a
//!   full queue or a missing thread still blocks in RAM.
//! - The day's latch markers, read STRICTLY ([`seed_from_markers`]): present
//!   engages that cause, unreadable engages [`CAUSE_MARKER_UNREADABLE`]
//!   (fail closed, §28.7 REJECT "failing open when the marker cannot be read
//!   at start"); a missing file or folder is absent (a fresh box is not
//!   blocked). Read at boot (`live_fill::boot`), by the drain's
//!   `restore_latch` (start and every day change), and by [`ensure_seeded`]
//!   on the order runtime's cold paths.
//!
//! # Who reads it
//!
//! Gate A (`LiveFillChain::decide`), gate B (`Finished::Filled`), gate C (the
//! runtime's paper `Execute` arm) and gate D (the morning self-test). Every
//! one tests "is this an entry" first; an exit never reads the switch.
//!
//! # Honest limits
//!
//! - The page lags the fault (about 60 s for cause A, 10 minutes for cause
//!   B); entries in between go through.
//! - The reversion page (`FormatReverted`) neither engages nor writes a
//!   marker (§28.7 covers causes A and B).
//! - A failed marker write leaves the block in RAM only; a restart that day
//!   loses it.
//! - Latest-wins: a backward clock step that engages D-1 after D lets D read
//!   unblocked once the clock returns (needs an alarm or unreadable marker on
//!   D-1, rare).
//! - Latent today: no engine runs in production (`live_fill::select_engine`
//!   returns `None`), so nothing decides or sends an entry.

use std::path::Path;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use tickvault_common::error_code::ErrorCode;
use tracing::warn;

use crate::daily_task_marker::MarkerState;

/// Reject reason of gates A and B (the chain's own refusal).
pub const REASON_FEED_TIME_ALARM: &str = "feed_time_alarm";
/// Reject reason of gate C (the runtime's backstop). The `refused:` prefix
/// makes the LF-7 report count it as a runtime reject.
pub const RUNTIME_REASON_FEED_TIME_ALARM: &str = "refused:feed_time_alarm";
/// The daily latch marker of cause A (written by the feed-time page's writer
/// thread, read here and by the page's `restore_latch`).
pub(crate) const TASK_CAUSE_A: &str = "feed-time-page-a";
/// The daily latch marker of cause B.
pub(crate) const TASK_CAUSE_B: &str = "feed-time-page-b";

/// Cause A: index prices carried a time outside the session.
pub const CAUSE_A: u8 = 1;
/// Cause B: no candle for 10 minutes while prices arrived.
pub const CAUSE_B: u8 = 2;
/// A latch marker could not be read at start (fail closed).
pub const CAUSE_MARKER_UNREADABLE: u8 = 4;

/// Gauge: 1 while today's entries are blocked (local `/metrics` only).
pub const ENTRIES_BLOCKED_GAUGE: &str = "tv_trade_safety_entries_blocked";
/// Counter: entries the chain refused (gates A and B).
pub const CHAIN_BLOCKED_COUNTER: &str = "tv_live_fill_entries_blocked_feed_time_total";
/// Counter: entries the runtime refused (gate C).
pub const RUNTIME_BLOCKED_COUNTER: &str = "tv_paper_entries_blocked_feed_time_total";

/// The largest day the packed word holds (`(day + 1) << 8` must fit).
const MAX_DAY: i64 = (1_i64 << 55) - 2;
/// Nanoseconds per second.
const NANOS_PER_SEC: i64 = 1_000_000_000;
/// Seconds per day.
const SECS_PER_DAY: i64 = 86_400;

/// The process-wide switch. See the module doc.
#[derive(Debug)]
pub struct SafetySwitch {
    /// `((day + 1) << 8) | causes`, 0 = never engaged.
    state: AtomicU64,
    /// The last day [`seed_from_markers`] read the markers for, so
    /// [`ensure_seeded`] reads them once a day.
    seeded_day: AtomicI64,
}

impl Default for SafetySwitch {
    fn default() -> Self {
        Self::new()
    }
}

impl SafetySwitch {
    /// A switch nothing has engaged.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: AtomicU64::new(0),
            seeded_day: AtomicI64::new(i64::MIN),
        }
    }

    /// Blocks entries on `day` for `causes`: ORs them into the same day,
    /// replaces any other day (the latest engage wins). A negative or
    /// too-large day, or no cause, does nothing. One CAS loop, O(1), no
    /// lock, no allocation. Returns true when the word changed.
    pub fn engage(&self, day: i64, causes: u8) -> bool {
        if causes == 0 || !(0..=MAX_DAY).contains(&day) {
            return false;
        }
        // The range check above makes the cast lossless.
        #[allow(clippy::cast_sign_loss)] // APPROVED: 0 <= day <= MAX_DAY checked above
        let day_bits = ((day + 1) as u64) << 8;
        let mut current = self.state.load(Ordering::Acquire);
        loop {
            let next = if current >> 8 == day_bits >> 8 {
                current | u64::from(causes)
            } else {
                day_bits | u64::from(causes)
            };
            if next == current {
                return false;
            }
            match self.state.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(seen) => current = seen,
            }
        }
    }

    /// The causes blocking entries on `day`, or `None`. One Acquire load and
    /// one compare, O(1), no allocation.
    #[inline]
    #[must_use]
    pub fn blocked_on(&self, day: i64) -> Option<u8> {
        let word = self.state.load(Ordering::Acquire);
        if word == 0 || !(0..=MAX_DAY).contains(&day) {
            return None;
        }
        #[allow(clippy::cast_sign_loss)] // APPROVED: 0 <= day <= MAX_DAY checked above
        let want = (day + 1) as u64;
        // The low byte is never 0 once engaged (`engage` refuses no cause).
        #[allow(clippy::cast_possible_truncation)] // APPROVED: masked to the low byte
        let causes = (word & 0xff) as u8;
        (word >> 8 == want && causes != 0).then_some(causes)
    }
}

/// The process-wide switch every production gate reads.
pub static SWITCH: SafetySwitch = SafetySwitch::new();

/// The switch production code hands a gate: [`SWITCH`]. Under the crate's own
/// unit tests each caller gets a fresh private switch, so one test's engage
/// can never block another test's entries.
#[cfg(not(test))]
#[must_use]
pub(crate) fn default_switch() -> &'static SafetySwitch {
    &SWITCH
}

/// Test build: a fresh private switch per call (leaked, a few bytes each).
#[cfg(test)]
#[must_use]
pub(crate) fn default_switch() -> &'static SafetySwitch {
    Box::leak(Box::new(SafetySwitch::new()))
}

/// The IST day number of an IST epoch nanosecond: the formula the live paper
/// chain keys its day on. `const`, O(1).
#[inline]
#[must_use]
pub const fn ist_day_of_ist_nanos(ist_ns: i64) -> i64 {
    ist_ns.div_euclid(NANOS_PER_SEC).div_euclid(SECS_PER_DAY)
}

/// Today's IST day number from the wall clock (the chain's own clock).
#[must_use]
pub fn ist_day_now() -> i64 {
    ist_day_of_ist_nanos(crate::live_fill::wall_ist_ns())
}

/// Reads the day's two latch markers strictly and engages what they show:
/// present engages that cause, unreadable engages
/// [`CAUSE_MARKER_UNREADABLE`]. Records the day as seeded. Returns what it
/// read, cause A first. Cold: two small file reads, off the tokio worker.
pub fn seed_from_markers(switch: &SafetySwitch, day: i64) -> [MarkerState; 2] {
    seed_from_markers_in(
        switch,
        day,
        Path::new(crate::daily_task_marker::DAILY_MARKER_DIR),
    )
}

/// [`seed_from_markers`] against a marker folder (the test seam).
pub(crate) fn seed_from_markers_in(
    switch: &SafetySwitch,
    day: i64,
    base: &Path,
) -> [MarkerState; 2] {
    let states = match tickvault_storage::ltt_correction::ist_day_date(day) {
        Some(date) => [
            marker_state(base, TASK_CAUSE_A, date),
            marker_state(base, TASK_CAUSE_B, date),
        ],
        // A day with no calendar date cannot be read: fail closed.
        None => [MarkerState::Unreadable; 2],
    };
    let mut causes = 0;
    for (state, cause) in states.iter().zip([CAUSE_A, CAUSE_B]) {
        causes |= match state {
            MarkerState::Present => cause,
            MarkerState::Unreadable => CAUSE_MARKER_UNREADABLE,
            MarkerState::Absent => 0,
        };
    }
    switch.engage(day, causes);
    switch.seeded_day.store(day, Ordering::Release);
    states
}

fn marker_state(base: &Path, task: &str, date: chrono::NaiveDate) -> MarkerState {
    crate::daily_task_marker::marker_state_in(base, task, date)
}

/// Reads the day's markers once per day: one Acquire load when `day` is
/// already seeded, otherwise [`seed_from_markers`]. For the order runtime's
/// cold paths (gate C, gate D), so a day roll is seeded even when the drain
/// is gone. Two racing callers may both read the markers once (harmless:
/// engage is idempotent).
pub fn ensure_seeded(switch: &SafetySwitch, day: i64) {
    if switch.seeded_day.load(Ordering::Acquire) != day {
        seed_from_markers(switch, day);
    }
}

/// Registers the switch's gauge and both counters at 0, so an absent series
/// never reads as "never blocked" before the first block. Cold, once at boot.
pub fn register_metrics() {
    metrics::gauge!(ENTRIES_BLOCKED_GAUGE).set(0.0);
    metrics::counter!(CHAIN_BLOCKED_COUNTER).increment(0);
    metrics::counter!(RUNTIME_BLOCKED_COUNTER).increment(0);
}

/// Logs the edge into "entries blocked" and keeps the gauge, off the frame
/// drain: the order runtime calls [`EdgePublisher::tick`] on its 1 s
/// housekeeping tick. One coded `warn!` per edge (`DHAN-FEED-TIME-01`,
/// `source = "trade_safety_entries_blocked"`): WARN, so the ERROR-only alarm
/// filter never pages on it (noise lock §2.24; the alarm's own `error!` was
/// the page). O(1), no allocation except on an edge.
#[derive(Debug, Default)]
pub struct EdgePublisher {
    /// `(day, causes)` last seen blocked, or `None`.
    last: Option<(i64, u8)>,
}

impl EdgePublisher {
    /// A publisher that has seen nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self { last: None }
    }

    /// Compares `switch` on `today` with the last tick. Returns true on an
    /// edge (blocked, unblocked, or new causes).
    pub fn tick(&mut self, switch: &SafetySwitch, today: i64) -> bool {
        let now = switch.blocked_on(today).map(|causes| (today, causes));
        if now == self.last {
            return false;
        }
        self.last = now;
        match now {
            Some((day, causes)) => {
                metrics::gauge!(ENTRIES_BLOCKED_GAUGE).set(1.0);
                warn!(
                    code = ErrorCode::DhanFeedTime01TradeTimesWrong.code_str(),
                    source = "trade_safety_entries_blocked",
                    day,
                    causes,
                    cause_prices_outside_session = causes & CAUSE_A != 0,
                    cause_no_candles = causes & CAUSE_B != 0,
                    cause_marker_unreadable = causes & CAUSE_MARKER_UNREADABLE != 0,
                    "New paper entries are paused for today because Dhan feed times look \
                     wrong; exits still go out. It ends by itself at midnight IST."
                );
            }
            None => metrics::gauge!(ENTRIES_BLOCKED_GAUGE).set(0.0),
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use proptest::prelude::*;

    const DAY: i64 = 20_777;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tv-trade-safety-{tag}-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("t")
                .replace("::", "-")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn date(day: i64) -> NaiveDate {
        tickvault_storage::ltt_correction::ist_day_date(day).unwrap_or_default()
    }

    #[test]
    fn test_blocked_on_same_day_only() {
        let s = SafetySwitch::new();
        assert_eq!(s.blocked_on(DAY), None);
        assert!(s.engage(DAY, CAUSE_A));
        assert_eq!(s.blocked_on(DAY), Some(CAUSE_A));
        assert_eq!(
            s.blocked_on(DAY + 1),
            None,
            "yesterday's alarm never blocks today"
        );
        assert_eq!(s.blocked_on(DAY - 1), None);
    }

    #[test]
    fn test_same_day_causes_or() {
        let s = SafetySwitch::new();
        assert!(s.engage(DAY, CAUSE_A));
        assert!(s.engage(DAY, CAUSE_B));
        assert!(
            !s.engage(DAY, CAUSE_A),
            "an engage that changes nothing reports false"
        );
        assert_eq!(s.blocked_on(DAY), Some(CAUSE_A | CAUSE_B));
    }

    #[test]
    fn test_latest_engage_wins_after_forward_step() {
        // A clock step forward stamps D+1 first; the real alarm on D then
        // still blocks D (a highest-day-wins rule would ignore it).
        let s = SafetySwitch::new();
        assert!(s.engage(DAY + 1, CAUSE_B));
        assert!(s.engage(DAY, CAUSE_A));
        assert_eq!(s.blocked_on(DAY), Some(CAUSE_A));
        assert_eq!(s.blocked_on(DAY + 1), None);
    }

    #[test]
    fn test_backward_step_limit_pinned() {
        // The documented limit: an engage of D-1 after D replaces D.
        let s = SafetySwitch::new();
        s.engage(DAY, CAUSE_A);
        s.engage(DAY - 1, CAUSE_MARKER_UNREADABLE);
        assert_eq!(s.blocked_on(DAY), None);
        assert_eq!(s.blocked_on(DAY - 1), Some(CAUSE_MARKER_UNREADABLE));
    }

    #[test]
    fn test_negative_day_noop() {
        let s = SafetySwitch::new();
        assert!(!s.engage(-1, CAUSE_A));
        assert!(!s.engage(MAX_DAY + 1, CAUSE_A));
        assert!(!s.engage(DAY, 0));
        assert_eq!(s.blocked_on(-1), None);
        assert_eq!(s.blocked_on(DAY), None);
        assert!(s.engage(MAX_DAY, CAUSE_B));
        assert_eq!(s.blocked_on(MAX_DAY), Some(CAUSE_B));
        assert!(s.engage(0, CAUSE_A));
        assert_eq!(s.blocked_on(0), Some(CAUSE_A));
    }

    #[test]
    fn test_concurrent_engages_keep_both_causes() {
        for _ in 0..200 {
            let s = std::sync::Arc::new(SafetySwitch::new());
            let threads: Vec<_> = [CAUSE_A, CAUSE_B, CAUSE_MARKER_UNREADABLE]
                .into_iter()
                .map(|cause| {
                    let s = std::sync::Arc::clone(&s);
                    std::thread::spawn(move || {
                        s.engage(DAY, cause);
                    })
                })
                .collect();
            for t in threads {
                assert!(t.join().is_ok());
            }
            assert_eq!(
                s.blocked_on(DAY),
                Some(CAUSE_A | CAUSE_B | CAUSE_MARKER_UNREADABLE)
            );
        }
    }

    #[test]
    fn test_default_switch_is_private_per_call_under_test() {
        let a = default_switch();
        let b = default_switch();
        a.engage(DAY, CAUSE_A);
        assert_eq!(b.blocked_on(DAY), None);
        assert!(!std::ptr::eq(a, b));
    }

    #[test]
    fn test_ist_day_now_and_ist_day_of_ist_nanos() {
        // The chain's own formula on IST epoch nanoseconds.
        assert_eq!(ist_day_of_ist_nanos(0), 0);
        assert_eq!(ist_day_of_ist_nanos(86_399_999_999_999), 0);
        assert_eq!(ist_day_of_ist_nanos(86_400_000_000_000), 1);
        assert_eq!(ist_day_of_ist_nanos(-1), -1);
        let now = ist_day_now();
        let receipt = tickvault_storage::ltt_correction::receipt_ist_day(
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
        );
        assert!((now - receipt).abs() <= 1);
    }

    proptest! {
        #[test]
        fn proptest_day_key_equals_chain_runtime_page_and_receipt_formulas(
            utc_ns in -1_000_000_000_000_000_000_i64..4_000_000_000_000_000_000_i64,
        ) {
            let ist_ns = utc_ns.saturating_add(tickvault_trading::brutex_host::live_candles::IST_OFFSET_NS);
            // The chain: IST ns → secs → days.
            let chain = ist_ns.div_euclid(1_000_000_000).div_euclid(86_400);
            prop_assert_eq!(ist_day_of_ist_nanos(ist_ns), chain);
            // The feed-time page and the WAL receipt: UTC ns + IST offset.
            let page = tickvault_storage::ltt_correction::receipt_ist_day(utc_ns);
            prop_assert_eq!(ist_day_of_ist_nanos(ist_ns), page);
        }

        #[test]
        fn proptest_switch_against_oracle(
            ops in proptest::collection::vec((0_i64..8, 0_u8..8, any::<bool>()), 1..64),
        ) {
            // Oracle: the latest engage's day wins; causes OR within a day.
            let s = SafetySwitch::new();
            let mut oracle: Option<(i64, u8)> = None;
            for (day, causes, read) in ops {
                let day = DAY + day;
                if read {
                    let want = oracle.and_then(|(d, c)| (d == day).then_some(c));
                    prop_assert_eq!(s.blocked_on(day), want);
                } else {
                    s.engage(day, causes);
                    if causes != 0 {
                        oracle = Some(match oracle {
                            Some((d, c)) if d == day => (d, c | causes),
                            _ => (day, causes),
                        });
                    }
                }
            }
            for day in DAY..DAY + 8 {
                let want = oracle.and_then(|(d, c)| (d == day).then_some(c));
                prop_assert_eq!(s.blocked_on(day), want);
            }
        }
    }

    #[test]
    fn test_seed_present_unreadable_absent() {
        let base = tmp("seed");
        // Absent: the folder does not even exist (a fresh box).
        let s = SafetySwitch::new();
        assert_eq!(
            seed_from_markers_in(&s, DAY, &base),
            [MarkerState::Absent, MarkerState::Absent]
        );
        assert_eq!(s.blocked_on(DAY), None, "a fresh box is not blocked");
        // Present: cause A's marker written by the real writer.
        let written =
            crate::daily_task_marker::test_write_marker_in(&base, TASK_CAUSE_A, date(DAY));
        assert!(written.is_ok());
        let s = SafetySwitch::new();
        assert_eq!(
            seed_from_markers_in(&s, DAY, &base),
            [MarkerState::Present, MarkerState::Absent]
        );
        assert_eq!(s.blocked_on(DAY), Some(CAUSE_A));
        // Unreadable: a torn cause-B file (no day line) blocks too.
        let path = base.join(format!("{TASK_CAUSE_B}-{}.marker", date(DAY)));
        assert!(std::fs::write(&path, b"sha=abc\n").is_ok());
        let s = SafetySwitch::new();
        assert_eq!(
            seed_from_markers_in(&s, DAY, &base),
            [MarkerState::Present, MarkerState::Unreadable]
        );
        assert_eq!(s.blocked_on(DAY), Some(CAUSE_A | CAUSE_MARKER_UNREADABLE));
        // Unreadable: a directory where the file should be.
        assert!(std::fs::remove_file(&path).is_ok());
        assert!(std::fs::create_dir_all(&path).is_ok());
        let s = SafetySwitch::new();
        assert_eq!(
            seed_from_markers_in(&s, DAY, &base)[1],
            MarkerState::Unreadable
        );
        assert_eq!(s.blocked_on(DAY), Some(CAUSE_A | CAUSE_MARKER_UNREADABLE));
        // Another day's marker does not block this day.
        let s = SafetySwitch::new();
        assert_eq!(
            seed_from_markers_in(&s, DAY + 1, &base),
            [MarkerState::Absent, MarkerState::Absent]
        );
        assert_eq!(s.blocked_on(DAY + 1), None);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_seed_from_markers_reads_the_production_folder() {
        // The production entry point reads `data/state/daily`; on a test box
        // with no marker for a far-future day it reads Absent and blocks
        // nothing.
        let s = SafetySwitch::new();
        let far = 80_000;
        assert_eq!(
            seed_from_markers(&s, far),
            [MarkerState::Absent, MarkerState::Absent]
        );
        assert_eq!(s.blocked_on(far), None);
    }

    #[test]
    fn test_seed_fails_closed_on_a_day_with_no_date() {
        let s = SafetySwitch::new();
        let base = tmp("nodate");
        assert_eq!(
            seed_from_markers_in(&s, i64::MAX, &base),
            [MarkerState::Unreadable; 2]
        );
    }

    #[test]
    fn test_ensure_seeded_reads_once_per_day() {
        let base = tmp("ensure");
        let s = SafetySwitch::new();
        // Seed the day through the test seam: nothing there.
        seed_from_markers_in(&s, DAY, &base);
        assert_eq!(s.blocked_on(DAY), None);
        // A marker appearing later is NOT read again the same day: one
        // Acquire load, no file read (fire engages in RAM for that case).
        assert!(
            crate::daily_task_marker::test_write_marker_in(&base, TASK_CAUSE_A, date(DAY)).is_ok()
        );
        ensure_seeded(&s, DAY);
        assert_eq!(s.blocked_on(DAY), None);
        assert_eq!(s.seeded_day.load(Ordering::Acquire), DAY);
        // A new day reads (the production folder: absent for a far day).
        ensure_seeded(&s, 80_001);
        assert_eq!(s.seeded_day.load(Ordering::Acquire), 80_001);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_edge_publisher_warns_once_per_edge() {
        register_metrics();
        let s = SafetySwitch::new();
        let mut p = EdgePublisher::new();
        assert!(!p.tick(&s, DAY), "nothing blocked, no edge");
        s.engage(DAY, CAUSE_A);
        assert!(p.tick(&s, DAY), "edge into blocked");
        assert!(!p.tick(&s, DAY), "still blocked, no second line");
        s.engage(DAY, CAUSE_B);
        assert!(p.tick(&s, DAY), "a new cause is a new edge");
        assert!(p.tick(&s, DAY + 1), "midnight: the edge out");
        assert!(!p.tick(&s, DAY + 1));
    }

    #[test]
    fn test_reasons_and_task_names() {
        assert_eq!(REASON_FEED_TIME_ALARM, "feed_time_alarm");
        assert!(RUNTIME_REASON_FEED_TIME_ALARM.starts_with("refused:"));
        assert_eq!(TASK_CAUSE_A, "feed-time-page-a");
        assert_eq!(TASK_CAUSE_B, "feed-time-page-b");
        assert_eq!(CAUSE_A & CAUSE_B & CAUSE_MARKER_UNREADABLE, 0);
    }
}

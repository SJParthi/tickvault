//! The trade-safety switch (plan item UTC-SAFETY, daily-universe §28.7, owner
//! card "Yes, after this merges (Recommended)", 2026-11-21).
//!
//! While today's `DHAN-FEED-TIME-01` alarm is on (the feed's trade times
//! still look wrong after the correction, noise lock §2.24), the paper path
//! opens NO new entries. An exit always passes, at every gate. The switch is
//! keyed by IST day number, so it ends by itself at the day roll; it is not
//! cleared at 16:00 and nothing turns it off within the day.
//!
//! ```text
//! drain 30 s arm: FeedTimePage::fire ──one store──────────▶ FEED_TIME_BLOCK
//!   (its warn! and gauge run on the tv-feed-time-latch thread)    │
//! chain/page start: today's markers + .hold ──engage───────▶      │
//!                                                                 ▼
//!   Gate A  LiveFillChain::decide         entry → Decision + Reject "feed_time_alarm"
//!   Gate B  LiveFillChain::on_finished    held entry → Reject "feed_time_alarm"
//!   Gate C  order runtime execute         entry → Reject "refused:feed_time_alarm"
//!   Gate D  paper self-test entry         skipped (not a failure)
//! ```
//!
//! # Complexity and allocation
//!
//! One `AtomicI64` packs the day (high 56 bits) and the cause bits (low 8),
//! so a read is one Acquire load and a compare, and a write is one bounded
//! compare-and-swap loop (a few writes a day). No lock, no allocation. The
//! design named two statics (day and causes); one packed word is used
//! instead so two engages racing on a new day can never lose a cause bit.
//! The value never moves to an earlier day, so a clock stepped back a day
//! reads that earlier day as not blocked.

use std::sync::atomic::{AtomicI64, Ordering};

use tickvault_common::constants::{IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY};
use tickvault_common::error_code::ErrorCode;
use tracing::warn;

/// Cause A: index trade times outside the session after the correction.
pub const CAUSE_PRICES_OUTSIDE_SESSION: u8 = 1;
/// Cause B: frames arriving and no candle sealed for 10 minutes.
pub const CAUSE_NO_CANDLES: u8 = 2;

/// The feed-time page's daily-marker task names, one per cause, with the
/// cause bit each one engages (the page writes them; the chain start reads
/// them).
pub const FEED_TIME_PAGE_TASKS: [(&str, u8); 2] = [
    ("feed-time-page-a", CAUSE_PRICES_OUTSIDE_SESSION),
    ("feed-time-page-b", CAUSE_NO_CANDLES),
];

/// Gauge: 1 while entries are blocked today, 0 otherwise (local `/metrics`
/// only, no page, alarm, filter or EMF name).
pub const ENTRIES_BLOCKED_GAUGE: &str = "tv_trade_safety_entries_blocked";
/// Counter: entries refused because the switch was on: the live paper
/// chain's Gates A and B and its on-engage cut, and the order runtime's Gate C
/// backstop (each entry counted once; local `/metrics` only).
pub const ENTRIES_BLOCKED_COUNTER: &str = "tv_live_fill_entries_blocked_feed_time_total";

/// The packed word that means "never engaged" (day `i64::MIN >> 8`, no bit).
const NEVER: i64 = i64::MIN;
const CAUSE_MASK: i64 = 0xFF;

/// Day of a packed word (arithmetic shift, so negative days survive).
const fn day_of(v: i64) -> i64 {
    v >> 8
}

/// Packs a day and its cause bits. A day outside the 56-bit range is clamped
/// to it (real IST day numbers are about 20,000).
const fn pack(day: i64, causes: u8) -> i64 {
    let lo = i64::MIN >> 8;
    let hi = i64::MAX >> 8;
    let d = if day < lo {
        lo
    } else if day > hi {
        hi
    } else {
        day
    };
    (d << 8) | (causes as i64)
}

/// One switch: the blocked IST day and its causes, packed in one atomic.
#[derive(Debug)]
pub struct SafetySwitch {
    word: AtomicI64,
}

impl Default for SafetySwitch {
    fn default() -> Self {
        Self::new()
    }
}

impl SafetySwitch {
    /// A switch that blocks no day.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            word: AtomicI64::new(NEVER),
        }
    }

    /// Blocks entries on `day` for `cause_bit`. A later day replaces the
    /// stored day and its causes; the same day ORs the bit in; an earlier
    /// day changes nothing. Returns `true` when this call changed the switch
    /// (a new day, or a bit not set before). O(1) expected: a CAS loop that
    /// retries only while another engage races it. A zero `cause_bit`
    /// changes nothing.
    pub fn engage(&self, day: i64, cause_bit: u8) -> bool {
        if cause_bit == 0 {
            return false;
        }
        let mut current = self.word.load(Ordering::Acquire);
        loop {
            let stored_day = day_of(current);
            let next = if current == NEVER || stored_day < day {
                pack(day, cause_bit)
            } else if stored_day == day {
                current | i64::from(cause_bit)
            } else {
                return false;
            };
            if next == current {
                return false;
            }
            match self.word.compare_exchange_weak(
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

    /// The cause bits when entries are blocked on `day`, else `None`. One
    /// Acquire load and a compare.
    #[inline]
    #[must_use]
    pub fn entries_blocked_on(&self, day: i64) -> Option<u8> {
        let v = self.word.load(Ordering::Acquire);
        let causes = (v & CAUSE_MASK) as u8;
        (v != NEVER && day_of(v) == day && causes != 0).then_some(causes)
    }
}

/// The process switch every production gate reads.
pub static FEED_TIME_BLOCK: SafetySwitch = SafetySwitch::new();

/// Blocks entries on `day` for `cause_bit` on `switch` (production passes
/// [`FEED_TIME_BLOCK`] from the chain start's and the page start's marker and
/// hold reads, and from the page's writer thread at a day change; tests pass
/// a switch of their own). NEVER on the frame drain: the drain's `fire` calls
/// [`SafetySwitch::engage`] (the store only) and the writer thread runs
/// [`report_engaged`]. On a
/// change it logs one coded `warn!` (never `error!`: the page is already the
/// `DHAN-FEED-TIME-01` error, and a second one would page again, §2.24) and
/// sets the gauge from the switch for today ([`publish_gauge`]). Cold: a few
/// calls a day. Returns whether it changed.
pub fn engage(switch: &SafetySwitch, day: i64, cause_bit: u8) -> bool {
    let changed = switch.engage(day, cause_bit);
    if changed {
        report_engaged(switch, day);
    }
    changed
}

/// The engage edge's report, split from the store so the frame drain pays only
/// the store (daily-universe §28.7: "nothing on the frame drain beyond one
/// store when the alarm fires"; 2026-11-21 review finding 1). Logs one coded
/// `warn!` (never `error!`, §2.24) and sets the gauge from the switch for
/// today. Called by [`engage`] off the drain, and by the feed-time page's
/// writer thread after the drain's [`SafetySwitch::engage`] changed the
/// switch. Cold: a few calls a day; allocates in the log layers and takes the
/// metrics registry lock, so NEVER call it on the frame drain.
pub fn report_engaged(switch: &SafetySwitch, day: i64) {
    let causes = switch.entries_blocked_on(day).unwrap_or(0);
    let _gauge = publish_gauge(switch, ist_day_now());
    warn!(
        code = ErrorCode::OrderReady01GateRefused.code_str(),
        source = "trade_safety_entries_blocked",
        ist_day = day,
        causes,
        "trade safety: the paper path opens no new entries for the rest of today because \
         the feed's trade times look wrong; exits still go out"
    );
}

/// The `reason=` text the feed-time page's writer thread puts in the day's
/// `.hold` file, followed by the cause label (`prices_outside_session`,
/// `no_candles`). The chain start and the page start read it back as a second
/// record of the alarm (2026-11-21 review finding 2).
pub const FEED_TIME_HOLD_REASON_PREFIX: &str = "feed-time page ";

/// The cause bits a day's `.hold` file in `dir` records for the feed-time
/// page: a whole, checked file whose reason is the feed-time page gives that
/// cause's bit (both bits for a label this build does not know); anything
/// else is 0: no file, a file held for another reason, and a file that cannot
/// be read or checked. Only the page's own hold blocks: a day held for any
/// other reason, or by hand (an empty `.hold`, Quote 63), does not block
/// entries (owner decision pending, recorded in the CLAUDE.md row). The page
/// writes its hold atomically (temporary file, rename), so a torn copy of it
/// is not expected; the daily marker beside it is the record read fail closed
/// (2026-11-21 review finding 2: this is the second record, for a page whose
/// marker write failed while the hold landed). One open and one read of at
/// most 4 KiB. Cold: at the chain start, the page start and the writer
/// thread's day change, never per tick.
#[must_use]
pub fn feed_time_hold_bits(dir: &std::path::Path, date: chrono::NaiveDate) -> u8 {
    use std::io::Read;
    use tickvault_storage::held_days::{
        DAY_MARKER_MAX_BYTES, MarkerKind, MarkerRead, day_marker_path, decode_day_marker,
    };
    let Ok(file) = std::fs::File::open(day_marker_path(dir, MarkerKind::Hold, date)) else {
        return 0;
    };
    let mut text = String::new();
    match file
        .take(DAY_MARKER_MAX_BYTES.saturating_add(1))
        .read_to_string(&mut text)
    {
        Ok(n) if (n as u64) <= DAY_MARKER_MAX_BYTES => {}
        Ok(_) | Err(_) => return 0,
    }
    if !matches!(
        decode_day_marker(MarkerKind::Hold, date, &text),
        MarkerRead::Valid { .. }
    ) {
        return 0;
    }
    let Some(label) = text.lines().find_map(|l| {
        l.strip_prefix("reason=")
            .and_then(|r| r.strip_prefix(FEED_TIME_HOLD_REASON_PREFIX))
    }) else {
        return 0;
    };
    match label.trim() {
        "prices_outside_session" => CAUSE_PRICES_OUTSIDE_SESSION,
        "no_candles" => CAUSE_NO_CANDLES,
        _ => CAUSE_PRICES_OUTSIDE_SESSION | CAUSE_NO_CANDLES,
    }
}

/// [`feed_time_hold_bits`] for the process hold folder
/// (`data/state/held-days`), run through `off_worker` so a caller on a tokio
/// worker never blocks it. Cold.
#[must_use]
pub fn process_feed_time_hold_bits(date: chrono::NaiveDate) -> u8 {
    tickvault_storage::off_worker::off_worker(|| {
        feed_time_hold_bits(
            std::path::Path::new(tickvault_storage::held_days::HELD_DAYS_DIR),
            date,
        )
    })
}

/// Sets [`ENTRIES_BLOCKED_GAUGE`] from `switch` for `day` (1 when entries are
/// blocked on that day, else 0) and returns the value. The gauge has ONE
/// source of truth, the switch read for the wall-clock day: the engage edge
/// writes it, and the feed-time page's writer thread writes it again at each
/// IST day change (the drain hands it the day), so it reads 0 after the day
/// roll even when no chain runs. Nothing
/// else writes it (2026-11-21 review: the live fill chain's own write, from
/// the chain's day, could disagree). One load, one gauge store. Cold.
pub fn publish_gauge(switch: &SafetySwitch, day: i64) -> f64 {
    let value = if switch.entries_blocked_on(day).is_some() {
        1.0
    } else {
        0.0
    };
    metrics::gauge!(ENTRIES_BLOCKED_GAUGE).set(value);
    value
}

/// Today's IST day number from the wall clock (IST seconds / 86,400, floor):
/// the same formula as the drain's day and the chain's day. O(1).
#[must_use]
pub fn ist_day_now() -> i64 {
    ist_day_of_utc_secs(chrono::Utc::now().timestamp())
}

/// The IST day number of a UTC epoch second. Pure, O(1).
#[must_use]
pub const fn ist_day_of_utc_secs(utc_secs: i64) -> i64 {
    utc_secs
        .saturating_add(IST_UTC_OFFSET_SECONDS as i64)
        .div_euclid(SECONDS_PER_DAY as i64)
}

/// Registers both series so an absent series never reads as healthy: the
/// counter at 0, the gauge at the process switch's value for today (never a
/// blind 0, which would undo a block the drain engaged before the live fill
/// boot ran). Cold, once at the live fill boot.
// TEST-EXEMPT: covered by test_register_metrics_and_process_engage_set_the_gauge
pub fn register_metrics() {
    let _gauge = publish_gauge(&FEED_TIME_BLOCK, ist_day_now());
    metrics::counter!(ENTRIES_BLOCKED_COUNTER).increment(0);
}

#[cfg(test)]
#[path = "trade_safety_tests.rs"]
mod tests;

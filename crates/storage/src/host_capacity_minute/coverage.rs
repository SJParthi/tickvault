//! Fixed-size coverage state for the cold daily proof. No new persisted columns.

use tickvault_common::trading_calendar::TradingCalendar;

use super::{
    SECONDS_PER_DAY, SESSION_END_SECS_OF_DAY_IST, SESSION_START_SECS_OF_DAY_IST,
    VERDICT_AT_SECS_OF_DAY_IST,
};

pub(super) const SECONDS_PER_MINUTE: u32 = 60;
const VERDICT_SETTLE_SECS: u32 = VERDICT_AT_SECS_OF_DAY_IST - SESSION_END_SECS_OF_DAY_IST;

/// Coverage behind the existing daily results/reasons; not an ILP schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DayCoverage {
    /// Expected distinct minutes, or unknown calendar/session.
    pub expected_minutes: Option<u64>,
    /// A fixed diagnostic reason when the date cannot be classified.
    pub calendar_issue: Option<&'static str>,
    /// Due minute as IST seconds of day, if the session is known.
    pub verdict_at_secs: Option<u32>,
    /// Nonempty valid minute rows for each tick-step stage.
    pub stage_minutes: [u64; 4],
    /// Observed minute populations whose exact strict p99 failed. Positive
    /// strict counts do not certify whole-session intervals or freshness.
    pub strict_failed_minutes: [u64; 4],
    /// Nonempty valid strict count pairs, without an interval/freshness claim.
    pub strict_sampled_minutes: [u64; 4],
    /// Minutes with every supplied aggregate reader observation present.
    /// This does not certify every registered member behind that aggregate.
    pub reader_minutes: u64,
    /// Minutes with every supplied drain-group observation present.
    pub drain_minutes: u64,
    /// Minutes with finite, physically valid whole-host utilization evidence.
    pub host_busy_minutes: u64,
    /// Minutes with both lost-tick and not-landed counters measured.
    pub exclusion_counter_minutes: u64,
    /// Refused main-feed frames, which have no downstream live-path timing.
    pub ring_refused_frames: u64,
    /// Observed rows not landed on the normal live path.
    pub ticks_not_landed: u64,
    /// Observed lost ticks; distinct from not-landed and never added to it.
    pub ticks_lost: u64,
    /// Eligible minutes with mismatched timestamps, stage identity or cadence.
    pub malformed_minutes: u64,
}

impl DayCoverage {
    pub(super) fn all_minutes(&self, seen: u64) -> bool {
        self.expected_minutes.is_some_and(|n| n > 0 && n == seen)
            && self.calendar_issue.is_none()
            && self.malformed_minutes == 0
    }

    pub(super) fn no_exclusions(&self) -> bool {
        self.ring_refused_frames == 0 && self.ticks_not_landed == 0 && self.ticks_lost == 0
    }

    pub(super) fn stage_complete(&self, seen: u64, index: usize) -> bool {
        self.all_minutes(seen)
            && self.stage_minutes[index] == seen
            && self.exclusion_counter_minutes == seen
            && self.no_exclusions()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionPlan {
    Closed,
    Unknown(&'static str),
    Open { start: u32, end: u32, due: u32 },
}

impl SessionPlan {
    /// Cold, once per IST date. The coverage-horizon lookup scans configured
    /// holidays; no frame/tick code calls it and it allocates nothing.
    pub(super) fn for_day(calendar: Option<&TradingCalendar>, day: i64) -> Self {
        if day < 0 {
            return Self::Unknown("calendar_invalid_date");
        }
        let Some(calendar) = calendar else {
            return Self::Unknown("calendar_missing");
        };
        // These are already IST-shifted epoch nanos: do not apply +05:30 again.
        let date = chrono::DateTime::from_timestamp_nanos(day).date_naive();
        if calendar.coverage_end_date().is_none_or(|end| date > end) {
            return Self::Unknown("calendar_stale_or_empty");
        }
        if calendar.is_muhurat_trading_day(date) {
            let Some((start, end)) = calendar.muhurat_session_secs(date) else {
                return Self::Unknown("muhurat_times_unknown");
            };
            if start >= end
                || start % SECONDS_PER_MINUTE != 0
                || end % SECONDS_PER_MINUTE != 0
                || end > SECONDS_PER_DAY.saturating_sub(VERDICT_SETTLE_SECS)
                || !tickvault_common::muhurat::session_is_inside_capture_windows(start, end)
            {
                return Self::Unknown("muhurat_times_invalid");
            }
            return Self::Open {
                start,
                end,
                due: end + VERDICT_SETTLE_SECS,
            };
        }
        if calendar.is_trading_day(date) {
            Self::Open {
                start: SESSION_START_SECS_OF_DAY_IST,
                end: SESSION_END_SECS_OF_DAY_IST,
                due: VERDICT_AT_SECS_OF_DAY_IST,
            }
        } else {
            // Includes ordinary holidays/weekends and mock-only Saturdays.
            Self::Closed
        }
    }

    pub(super) fn contains(self, secs: u32) -> bool {
        matches!(self, Self::Open { start, end, .. } if (start..end).contains(&secs))
    }

    pub(super) fn coverage(self) -> DayCoverage {
        match self {
            Self::Open { start, end, due } => DayCoverage {
                expected_minutes: Some(u64::from((end - start) / SECONDS_PER_MINUTE)),
                verdict_at_secs: Some(due),
                ..DayCoverage::default()
            },
            Self::Unknown(reason) => DayCoverage {
                calendar_issue: Some(reason),
                ..DayCoverage::default()
            },
            Self::Closed => DayCoverage::default(),
        }
    }
}

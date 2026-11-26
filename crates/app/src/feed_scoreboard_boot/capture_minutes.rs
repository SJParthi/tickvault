//! The feed-wide minute score measures whether any persisted tick fell in each
//! scheduled minute. A tick from one instrument cannot establish delivery for
//! every instrument or continuous trading in every segment.

use super::{MICROS_PER_SEC, SESSION_END_SECS_OF_DAY_IST, SESSION_START_SECS_OF_DAY_IST};
use chrono::{Datelike, NaiveDate, Timelike};
use std::collections::HashSet;
use tickvault_common::market_hours::{
    SessionDayInfo, covered_calendar_session_day, installed_muhurat_session_secs,
};

type Bounds = (i64, i64);

/// One session can include a regular window and a separately configured
/// evening session. Bounds are disjoint, whole-minute, half-open IST ranges.
#[derive(Debug)]
pub(super) struct CaptureMinutes {
    date: NaiveDate,
    includes_regular: bool,
    first: Bounds,
    second: Option<Bounds>,
}

impl CaptureMinutes {
    pub(super) fn for_day(day: u64) -> Option<Self> {
        let date = NaiveDate::from_epoch_days(i32::try_from(day).ok()?)?;
        Self::for_date(date)
    }

    pub(super) fn for_date(date: NaiveDate) -> Option<Self> {
        Self::for_schedule(
            date,
            covered_calendar_session_day(date),
            installed_muhurat_session_secs(date),
        )
    }

    fn for_schedule(
        date: NaiveDate,
        calendar: Option<SessionDayInfo>,
        special: Option<(u32, u32)>,
    ) -> Option<Self> {
        let calendar = calendar?;
        let regular_bounds = calendar.trading.then(|| {
            // NSE introduced the closing auction on 2026-08-03.
            let close = if (date.year(), date.month(), date.day()) < (2026, 8, 3) {
                15 * 3600 + 30 * 60
            } else {
                SESSION_END_SECS_OF_DAY_IST
            };
            (SESSION_START_SECS_OF_DAY_IST, close)
        });
        let special_bounds = if calendar.muhurat_end_secs.is_some() {
            let (open, close) = special?;
            if open >= close || close > 86_400 || open % 60 != 0 || close % 60 != 0 {
                return None;
            }
            Some((i64::from(open), i64::from(close)))
        } else {
            None
        };
        let (first, second) = match (regular_bounds, special_bounds) {
            (Some(a), Some(b)) => {
                let (earlier, later) = if a.0 <= b.0 { (a, b) } else { (b, a) };
                if earlier.1 >= later.0 {
                    ((earlier.0, earlier.1.max(later.1)), None)
                } else {
                    (earlier, Some(later))
                }
            }
            (Some(window), None) | (None, Some(window)) => (window, None),
            (None, None) => return None,
        };
        Some(Self {
            date,
            includes_regular: calendar.trading,
            first,
            second,
        })
    }

    /// Retain the configured time for normal days and its post-close delay
    /// for a separately timed auction. An evening close can move a mixed
    /// day's final score later. The daily job operates within one IST date.
    pub(super) fn trigger(&self, regular_trigger: u32) -> Option<u32> {
        let last_close = u32::try_from(self.second.unwrap_or(self.first).1).ok()?;
        let regular_close = super::MARKET_HOURS_END_SECS_OF_DAY_IST;
        let delay = regular_trigger.saturating_sub(regular_close);
        let dated = last_close.saturating_add(delay).min(86_399);
        Some(if self.includes_regular {
            dated.max(regular_trigger)
        } else {
            dated
        })
    }

    pub(super) fn before_open(&self, sec: i64) -> bool {
        sec < self.first.0
    }

    pub(super) fn market_data_up(&self, sec: i64) -> bool {
        // The regular feed's 09:00 pre-open connection is evidence, as
        // before. For a separately configured auction, only published
        // session hours qualify. A gap between sessions is not in-session.
        let in_bounds = |(open, close): Bounds| (open..close).contains(&sec);
        in_bounds(self.first)
            || self.second.is_some_and(in_bounds)
            || (self.includes_regular
                && (i64::from(super::MARKET_HOURS_START_SECS_OF_DAY_IST)
                    ..SESSION_START_SECS_OF_DAY_IST)
                    .contains(&sec))
    }

    pub(super) fn duration(&self) -> i64 {
        (self.first.1 - self.first.0 + self.second.map_or(0, |(start, end)| end - start)) / 60
    }

    pub(super) fn sql(&self, feed: &str) -> String {
        // QuestDB stores IST wall time in the timestamp column.
        let day_start = self
            .date
            .and_time(chrono::NaiveTime::MIN)
            .and_utc()
            .timestamp_micros();
        let predicate = |(start, end): Bounds| {
            let lo = day_start.saturating_add(start * MICROS_PER_SEC);
            let hi = day_start.saturating_add(end * MICROS_PER_SEC);
            format!("ts >= {lo} and ts < {hi}")
        };
        let mut window = predicate(self.first);
        if let Some(second) = self.second {
            window = format!("(({window}) or ({}))", predicate(second));
        }
        format!(
            "select distinct date_trunc('minute', ts) from ticks where feed = '{feed}' and {window}"
        )
    }

    /// A genuine empty result means zero. An invalid, out-of-window, or
    /// wrongly dated row makes the entire measurement unknown.
    pub(super) fn observed(&self, body: &str) -> Option<i64> {
        let response: serde_json::Value = serde_json::from_str(body).ok()?;
        let rows = response.get("dataset")?.as_array()?;
        if response.get("error").is_some()
            || response
                .get("count")
                .is_some_and(|count| count.as_u64() != u64::try_from(rows.len()).ok())
        {
            return None;
        }
        let mut minutes = HashSet::new();
        for row in rows {
            let [value] = row.as_array()?.as_slice() else {
                return None;
            };
            let raw = value.as_str()?;
            let stamp = chrono::DateTime::parse_from_rfc3339(raw).ok()?.to_utc();
            let sec = i64::from(stamp.num_seconds_from_midnight());
            let inside = |(open, close): Bounds| (open..close).contains(&sec);
            if stamp.date_naive() != self.date
                || stamp.second() != 0
                || stamp.nanosecond() != 0
                || !(inside(self.first) || self.second.is_some_and(inside))
            {
                return None;
            }
            minutes.insert(sec);
        }
        i64::try_from(minutes.len()).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed_scoreboard_boot::day_bounds_micros;

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).expect("valid fixture")
    }

    const ORDINARY: Option<SessionDayInfo> = Some(SessionDayInfo {
        trading: true,
        muhurat_end_secs: None,
    });

    #[test]
    fn capture_minutes_change_on_the_auction_effective_date() {
        let before = CaptureMinutes::for_schedule(date(2026, 7, 31), ORDINARY, None).unwrap();
        let after = CaptureMinutes::for_schedule(date(2026, 8, 3), ORDINARY, None).unwrap();
        assert_eq!(before.duration(), 375);
        assert_eq!(after.duration(), 385);
        let day = u64::try_from(after.date.to_epoch_days()).unwrap();
        let start_micros = day_bounds_micros(day).0;
        let sql = after.sql("dhan");
        assert!(sql.contains(&format!(
            "ts >= {} and ts < {}",
            start_micros + 33_300_000_000,
            start_micros + 56_400_000_000
        )));
        assert_eq!(
            before.observed(r#"{"dataset":[["2026-07-31T15:30:00Z"]]}"#),
            None
        );
        assert_eq!(
            after.observed(r#"{"dataset":[["2026-08-03T15:39:00Z"]]}"#),
            Some(1)
        );
    }

    #[test]
    fn score_only_real_disjoint_or_overlapping_calendar_windows() {
        let day = date(2026, 11, 8);
        let special = SessionDayInfo {
            trading: false,
            muhurat_end_secs: Some(19 * 3600),
        };
        assert!(CaptureMinutes::for_schedule(day, None, None).is_none());
        assert!(CaptureMinutes::for_schedule(day, Some(special), None).is_none());
        assert!(
            CaptureMinutes::for_schedule(
                day,
                Some(SessionDayInfo {
                    trading: false,
                    muhurat_end_secs: None
                }),
                None
            )
            .is_none()
        );
        for invalid in [
            (18 * 3600 + 1, 19 * 3600),
            (19 * 3600, 18 * 3600),
            (18 * 3600, 90_000),
        ] {
            assert!(CaptureMinutes::for_schedule(day, Some(special), Some(invalid)).is_none());
        }
        let evening = Some((18 * 3600, 19 * 3600));
        let solo = CaptureMinutes::for_schedule(day, Some(special), evening).unwrap();
        assert_eq!(solo.duration(), 60);
        assert!(solo.before_open(15 * 3600));
        assert!(!solo.market_data_up(9 * 3600));
        assert!(solo.market_data_up(18 * 3600));
        assert_eq!(
            solo.observed(r#"{"dataset":[["2026-11-08T09:15:00Z"]]}"#),
            None
        );
        assert_eq!(
            solo.observed(r#"{"dataset":[["2026-11-08T18:59:00Z"]]}"#),
            Some(1)
        );
        let both = SessionDayInfo {
            trading: true,
            ..special
        };
        let split = CaptureMinutes::for_schedule(day, Some(both), evening).unwrap();
        assert_eq!(split.duration(), 445);
        assert!(split.market_data_up(9 * 3600));
        assert!(!split.market_data_up(17 * 3600));
        assert!(split.market_data_up(18 * 3600));
        assert!(split.sql("dhan").contains(") or ("));
        let overlap =
            CaptureMinutes::for_schedule(day, Some(both), Some((15 * 3600, 16 * 3600))).unwrap();
        assert_eq!(overlap.duration(), 405);
        assert!(overlap.second.is_none());
    }

    #[test]
    fn bad_minute_data_stays_unknown_and_equivalent_timestamps_count_once() {
        let window = CaptureMinutes::for_schedule(date(2026, 11, 26), ORDINARY, None).unwrap();
        assert_eq!(window.observed(r#"{"dataset":[]}"#), Some(0));
        assert_eq!(window.observed(r#"{"dataset":[["2026-11-26T09:15:00Z"], ["2026-11-26T09:15:00.000000Z"], ["2026-11-26T15:39:00Z"]]}"#), Some(2));
        for raw in [
            "2026-11-26T09:14:00Z",
            "2026-11-26T15:40:00Z",
            "2026-11-25T09:15:00Z",
            "2026-11-26T09:15:01Z",
            "2026-11-26T09:15:00.1Z",
            "bad",
        ] {
            let body = serde_json::json!({"dataset":[["2026-11-26T09:15:00Z"], [raw]]});
            assert_eq!(window.observed(&body.to_string()), None);
        }
        for bad in [
            r#"{"dataset":[],"count":1}"#,
            r#"{"dataset":[],"error":"partition unavailable"}"#,
            r#"{"dataset":[["2026-11-26T09:15:00Z",null]]}"#,
            r#"{"dataset":[[null]]}"#,
            r#"{"dataset":[[]]}"#,
            r#"{"dataset":[9]}"#,
            "{}",
        ] {
            assert_eq!(window.observed(bad), None);
        }
    }
}

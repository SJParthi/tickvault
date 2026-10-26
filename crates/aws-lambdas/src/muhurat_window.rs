//! Muhurat box window — when the box runs on an NSE Muhurat trading date
//! (plan item J8, owner 2026-10-26: "yes capture muhurat").
//!
//! The box's normal schedule is Monday to Friday 08:30–16:30 IST
//! (`main.tf` `daily_start` / `daily_stop`). A Muhurat session can fall on a
//! weekend (2026: Sunday 2026-11-08), so on that date nothing starts the box
//! and nothing is captured. This module decides, for one configured Muhurat
//! session, when the box must be up:
//!
//! ```text
//! box start = session start - MUHURAT_BOX_START_LEAD_MINUTES (60 min)
//! box stop  = session end   + MUHURAT_BOX_STOP_TAIL_MINUTES  (60 min)
//! ```
//!
//! **Where the sessions come from.** Terraform's `muhurat_sessions` variable
//! (`start-watchdog-lambda.tf`), rendered into the `MUHURAT_SESSIONS` env var
//! of the start watchdog and the hard-stop guard as
//! `YYYY-MM-DD@HH:MM-HH:MM`, entries separated by `;`. Every date must also
//! be a `[[trading.muhurat_trading_dates]]` entry in `config/base.toml`
//! (pinned by `tests/muhurat_window_wiring_guard.rs`), so the box schedule and
//! the app's calendar cannot name different days.
//!
//! **Times not invented.** NSE announces each year's Muhurat timing in a
//! separate circular weeks before Diwali (2024 was 18:00–19:00 IST, 2025 was
//! 13:45–14:45 IST). An entry whose times are empty (`2026-11-08@-`) is
//! [`MuhuratPhase::TimingUnset`]: the start watchdog logs a coded
//! `LAMBDA-CONFIG-01` error and does NOT start the box (fail closed). The
//! times are filled from NSE's circular by the owner, never guessed here.
//!
//! **Who reads it.** The start watchdog's `muhurat_check` mode (a dated
//! EventBridge rule, every 15 minutes on each Muhurat date) starts the box
//! inside the window and stops a box it started once the window has passed
//! (keep-alive and drain hold honoured). The start watchdog's hourly curfew
//! and the hard-stop guard treat the window as in-hours, so neither stops the
//! box during the session; the hard-stop guard still runs its in-window
//! budget check, so the budget stop applies exactly as on a weekday.
//!
//! Pure functions only: no I/O, total on any input.

use chrono::{DateTime, Duration, FixedOffset, NaiveDate, NaiveTime, Offset, TimeZone, Utc};

/// Env var carrying the rendered sessions (see the module header).
pub const MUHURAT_SESSIONS_ENV: &str = "MUHURAT_SESSIONS";

/// The box starts this long before the session's first NSE window. The
/// weekday schedule gives 30 minutes (08:30 start, 09:00 pre-open); a
/// one-off Sunday boot gets double that, so a slow boot, a capacity retry
/// (the rule fires every 15 minutes) or an after-start deploy still finishes
/// before the session.
pub const MUHURAT_BOX_START_LEAD_MINUTES: i64 = 60;

/// The box stops this long after the session ends: the candle close seal,
/// the writers' flush and the evening drain run inside it, and from 16:25 IST
/// the app's drain hold can keep the box up longer (`drain_hold`).
pub const MUHURAT_BOX_STOP_TAIL_MINUTES: i64 = 60;

/// One Muhurat date with its NSE session times (IST).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MuhuratSession {
    pub date: NaiveDate,
    pub start_ist: NaiveTime,
    pub end_ist: NaiveTime,
}

/// The parsed `MUHURAT_SESSIONS` value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MuhuratSchedule {
    /// Dates with valid session times.
    pub sessions: Vec<MuhuratSession>,
    /// Dates listed with empty times (NSE timing not yet filled in).
    pub unset_dates: Vec<NaiveDate>,
    /// Entries that could not be read at all (kept verbatim for the log).
    pub malformed: Vec<String>,
}

/// What the box should be doing right now with respect to Muhurat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MuhuratPhase {
    /// No Muhurat entry names today's IST date.
    NotToday,
    /// Today is listed but its session times are empty: fail closed.
    TimingUnset { date: NaiveDate },
    /// An entry could not be parsed and none valid names today: fail closed.
    Malformed { entries: Vec<String> },
    /// Today's session, before the box window opens.
    BeforeWindow { box_start: DateTime<Utc> },
    /// Inside `[box start, box stop]`: the box should be running.
    InWindow {
        box_start: DateTime<Utc>,
        box_stop: DateTime<Utc>,
    },
    /// Today's session, after the box window closed.
    AfterWindow {
        box_start: DateTime<Utc>,
        box_stop: DateTime<Utc>,
    },
}

fn ist() -> FixedOffset {
    FixedOffset::east_opt(crate::time::IST_OFFSET_SECS).unwrap_or_else(|| Utc.fix())
}

fn parse_hhmm(raw: &str) -> Option<NaiveTime> {
    let raw = raw.trim();
    // Exactly HH:MM, so "9:5" or "09:05:00" are refused rather than guessed.
    if raw.len() != 5 || raw.as_bytes()[2] != b':' {
        return None;
    }
    NaiveTime::parse_from_str(raw, "%H:%M").ok()
}

/// Parse the rendered `MUHURAT_SESSIONS` value. Total: an empty value is an
/// empty schedule, and every unreadable entry lands in `malformed`.
#[must_use]
pub fn parse_muhurat_sessions(raw: &str) -> MuhuratSchedule {
    let mut schedule = MuhuratSchedule::default();
    for entry in raw.split(';').map(str::trim).filter(|e| !e.is_empty()) {
        let Some((date_raw, times_raw)) = entry.split_once('@') else {
            schedule.malformed.push(entry.to_string());
            continue;
        };
        let Ok(date) = NaiveDate::parse_from_str(date_raw.trim(), "%Y-%m-%d") else {
            schedule.malformed.push(entry.to_string());
            continue;
        };
        let times = times_raw.trim();
        if times.is_empty() || times == "-" {
            schedule.unset_dates.push(date);
            continue;
        }
        let parsed = times
            .split_once('-')
            .and_then(|(s, e)| Some((parse_hhmm(s)?, parse_hhmm(e)?)));
        match parsed {
            Some((start_ist, end_ist)) if start_ist < end_ist => {
                schedule.sessions.push(MuhuratSession {
                    date,
                    start_ist,
                    end_ist,
                });
            }
            _ => schedule.malformed.push(entry.to_string()),
        }
    }
    schedule
}

/// The box window `[start, stop]` in UTC for one session.
#[must_use]
pub fn box_window_utc(session: &MuhuratSession) -> (DateTime<Utc>, DateTime<Utc>) {
    let at = |t: NaiveTime| {
        ist()
            .from_local_datetime(&session.date.and_time(t))
            .single()
            .map(|dt| dt.with_timezone(&Utc))
            // Unreachable: a fixed offset has no gaps or folds.
            .unwrap_or_else(|| Utc.from_utc_datetime(&session.date.and_time(t)))
    };
    (
        at(session.start_ist) - Duration::minutes(MUHURAT_BOX_START_LEAD_MINUTES),
        at(session.end_ist) + Duration::minutes(MUHURAT_BOX_STOP_TAIL_MINUTES),
    )
}

/// Classify `now` against the schedule. A valid session for today wins over
/// an unset or malformed entry, so one bad line cannot block a good one.
#[must_use]
pub fn muhurat_phase(now_utc: DateTime<Utc>, schedule: &MuhuratSchedule) -> MuhuratPhase {
    let today = now_utc.with_timezone(&ist()).date_naive();
    if let Some(session) = schedule.sessions.iter().find(|s| s.date == today) {
        let (box_start, box_stop) = box_window_utc(session);
        return if now_utc < box_start {
            MuhuratPhase::BeforeWindow { box_start }
        } else if now_utc <= box_stop {
            MuhuratPhase::InWindow {
                box_start,
                box_stop,
            }
        } else {
            MuhuratPhase::AfterWindow {
                box_start,
                box_stop,
            }
        };
    }
    if schedule.unset_dates.contains(&today) {
        return MuhuratPhase::TimingUnset { date: today };
    }
    if !schedule.malformed.is_empty() {
        return MuhuratPhase::Malformed {
            entries: schedule.malformed.clone(),
        };
    }
    MuhuratPhase::NotToday
}

/// True inside a configured Muhurat box window. Used by the curfew and the
/// hard-stop guard so neither stops the box during the session.
#[must_use]
pub fn in_muhurat_up_window(now_utc: DateTime<Utc>, schedule: &MuhuratSchedule) -> bool {
    matches!(
        muhurat_phase(now_utc, schedule),
        MuhuratPhase::InWindow { .. }
    )
}

/// `in_muhurat_up_window` over the raw env value (parse included).
#[must_use]
pub fn in_muhurat_up_window_raw(now_utc: DateTime<Utc>, raw: &str) -> bool {
    in_muhurat_up_window(now_utc, &parse_muhurat_sessions(raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn hm(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    #[test]
    fn test_parse_muhurat_sessions_valid_session() {
        let s = parse_muhurat_sessions("2026-11-08@14:05-15:30");
        assert_eq!(
            s.sessions,
            vec![MuhuratSession {
                date: date(2026, 11, 8),
                start_ist: hm(14, 5),
                end_ist: hm(15, 30),
            }]
        );
        assert!(s.unset_dates.is_empty() && s.malformed.is_empty());
    }

    #[test]
    fn test_parse_unset_times_is_timing_unset_not_a_session() {
        for raw in ["2026-11-08@-", "2026-11-08@", " 2026-11-08 @ - "] {
            let s = parse_muhurat_sessions(raw);
            assert!(s.sessions.is_empty(), "{raw}");
            assert_eq!(s.unset_dates, vec![date(2026, 11, 8)], "{raw}");
            assert!(s.malformed.is_empty(), "{raw}");
        }
    }

    #[test]
    fn test_parse_empty_value_is_empty_schedule() {
        assert_eq!(parse_muhurat_sessions(""), MuhuratSchedule::default());
        assert_eq!(parse_muhurat_sessions(" ; ;"), MuhuratSchedule::default());
    }

    #[test]
    fn test_parse_malformed_entries_are_refused_never_guessed() {
        for raw in [
            "2026-11-08",                // no @
            "2026-13-08@14:05-15:30",    // bad month
            "2026-11-08@14:05",          // no end
            "2026-11-08@9:05-15:30",     // not HH:MM
            "2026-11-08@14:05:00-15:30", // seconds
            "2026-11-08@15:30-14:05",    // end before start
            "2026-11-08@14:05-14:05",    // empty session
            "2026-11-08@25:00-26:00",    // not a time
        ] {
            let s = parse_muhurat_sessions(raw);
            assert!(s.sessions.is_empty(), "{raw}");
            assert!(s.unset_dates.is_empty(), "{raw}");
            assert_eq!(s.malformed, vec![raw.to_string()], "{raw}");
        }
    }

    #[test]
    fn test_parse_multiple_entries() {
        let s = parse_muhurat_sessions("2026-11-08@14:05-15:30;2027-10-29@-;junk");
        assert_eq!(s.sessions.len(), 1);
        assert_eq!(s.unset_dates, vec![date(2027, 10, 29)]);
        assert_eq!(s.malformed, vec!["junk".to_string()]);
    }

    #[test]
    fn test_box_window_utc_is_lead_before_start_and_tail_after_end() {
        let session = MuhuratSession {
            date: date(2026, 11, 8),
            start_ist: hm(14, 5),
            end_ist: hm(15, 30),
        };
        let (start, stop) = box_window_utc(&session);
        // 13:05 IST = 07:35 UTC; 16:30 IST = 11:00 UTC.
        assert_eq!(start, utc(2026, 11, 8, 7, 35));
        assert_eq!(stop, utc(2026, 11, 8, 11, 0));
    }

    #[test]
    fn test_box_window_evening_session() {
        let session = MuhuratSession {
            date: date(2026, 11, 8),
            start_ist: hm(18, 0),
            end_ist: hm(19, 15),
        };
        let (start, stop) = box_window_utc(&session);
        assert_eq!(start, utc(2026, 11, 8, 11, 30)); // 17:00 IST
        assert_eq!(stop, utc(2026, 11, 8, 14, 45)); // 20:15 IST
    }

    #[test]
    fn test_muhurat_phase_walks_before_in_after_with_inclusive_edges() {
        let s = parse_muhurat_sessions("2026-11-08@14:05-15:30");
        let start = utc(2026, 11, 8, 7, 35);
        let stop = utc(2026, 11, 8, 11, 0);
        assert_eq!(
            muhurat_phase(start - Duration::minutes(1), &s),
            MuhuratPhase::BeforeWindow { box_start: start }
        );
        for now in [start, stop] {
            assert_eq!(
                muhurat_phase(now, &s),
                MuhuratPhase::InWindow {
                    box_start: start,
                    box_stop: stop
                }
            );
            assert!(in_muhurat_up_window(now, &s));
        }
        assert_eq!(
            muhurat_phase(stop + Duration::minutes(1), &s),
            MuhuratPhase::AfterWindow {
                box_start: start,
                box_stop: stop
            }
        );
        assert!(!in_muhurat_up_window(stop + Duration::minutes(1), &s));
    }

    #[test]
    fn test_phase_uses_the_ist_date_not_the_utc_date() {
        // 2026-11-07 19:00 UTC is 2026-11-08 00:30 IST: today is the Muhurat
        // date in IST, before the window.
        let s = parse_muhurat_sessions("2026-11-08@14:05-15:30");
        assert!(matches!(
            muhurat_phase(utc(2026, 11, 7, 19, 0), &s),
            MuhuratPhase::BeforeWindow { .. }
        ));
        // 2026-11-08 18:45 UTC is 2026-11-09 00:15 IST: not today.
        assert_eq!(
            muhurat_phase(utc(2026, 11, 8, 18, 45), &s),
            MuhuratPhase::NotToday
        );
    }

    #[test]
    fn test_phase_other_day_is_not_today() {
        let s = parse_muhurat_sessions("2026-11-08@14:05-15:30");
        assert_eq!(
            muhurat_phase(utc(2026, 11, 9, 9, 0), &s),
            MuhuratPhase::NotToday
        );
        assert!(!in_muhurat_up_window(utc(2026, 11, 9, 9, 0), &s));
    }

    #[test]
    fn test_phase_unset_and_malformed_fail_closed() {
        let unset = parse_muhurat_sessions("2026-11-08@-");
        let now = utc(2026, 11, 8, 9, 0);
        assert_eq!(
            muhurat_phase(now, &unset),
            MuhuratPhase::TimingUnset {
                date: date(2026, 11, 8)
            }
        );
        assert!(!in_muhurat_up_window(now, &unset));
        let bad = parse_muhurat_sessions("2026-11-08@15:30-14:05");
        assert!(matches!(
            muhurat_phase(now, &bad),
            MuhuratPhase::Malformed { .. }
        ));
        assert!(!in_muhurat_up_window(now, &bad));
    }

    #[test]
    fn test_in_muhurat_up_window_valid_session_wins_over_a_bad_neighbour() {
        let s = parse_muhurat_sessions("junk;2026-11-08@14:05-15:30");
        assert!(in_muhurat_up_window(utc(2026, 11, 8, 9, 0), &s));
    }

    #[test]
    fn test_in_muhurat_up_window_raw_matches_parsed() {
        let now = utc(2026, 11, 8, 9, 0);
        assert!(in_muhurat_up_window_raw(now, "2026-11-08@14:05-15:30"));
        assert!(!in_muhurat_up_window_raw(now, ""));
        assert!(!in_muhurat_up_window_raw(now, "2026-11-08@-"));
    }

    #[test]
    fn test_parse_never_panics_on_garbage() {
        for raw in [
            "@",
            "@@@",
            "-",
            ";;@-;",
            "\u{0}",
            "2026-11-08@-@-",
            "日本@-",
        ] {
            let _ = parse_muhurat_sessions(raw);
            let _ = muhurat_phase(utc(2026, 11, 8, 9, 0), &parse_muhurat_sessions(raw));
        }
    }
}

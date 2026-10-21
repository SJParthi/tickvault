//! Drain hold — one SSM parameter that keeps the box running after the
//! close until the day's data is in the database (operator Quote 31,
//! 2026-10-21).
//!
//! The app writes `/tickvault/<env>/drain-hold` while evening work remains
//! (deferred depth buckets, waiting WAL segments, spill folders, QuestDB
//! apply lag) and refreshes it every few minutes:
//!
//! ```text
//! {"v":1,"state":"holding"|"released","until_unix":<i64>,"written_unix":<i64>,"left":"<text>"}
//! ```
//!
//! **Readers:** the start watchdog's `daily_stop` (17:30 IST), `stop_check`
//! (17:45 IST) and hourly `curfew_check`, and the hard-stop guard's
//! out-of-window stop. **Never** the budget stop: the guard's in-window
//! breach stop and the AWS-Budgets kill-switch do not read this parameter.
//!
//! **Bounds that keep a hold from costing money it should not:**
//! - a hold counts only while `until_unix` is in the future and at most
//!   [`MAX_HOLD_AHEAD_SECS`] ahead, so an app that stops refreshing it (a
//!   crash, a dead SSM write) lets it lapse within the hour;
//! - a hold is ignored from 07:30 IST to 17:25 IST, so the 07:35 curfew
//!   check stops a box still held overnight and the 08:30 start boots it
//!   fresh; the hold can never keep the box past 07:30 IST the next morning.
//!
//! **Readers fail toward STOPPING** (an unreadable or malformed hold is no
//! hold): the cost-safe direction. The only cost of a lost hold is the old
//! behaviour, and the next boot replays what was left.

use serde::Deserialize;
use tracing::{info, warn};

/// Default parameter name (prod). Terraform injects `DRAIN_HOLD_PARAM` per
/// environment. It sits under `/tickvault/<env>/` on purpose: the app box
/// is the writer, and the box's role may write there.
pub const DEFAULT_DRAIN_HOLD_PARAM: &str = "/tickvault/prod/drain-hold";

/// The only record version this reader understands.
pub const DRAIN_HOLD_VERSION: i64 = 1;

/// A hold more than this far ahead of now is invalid (the app refreshes it
/// every few minutes, so a far expiry can only be a bug or a forgery).
pub const MAX_HOLD_AHEAD_SECS: i64 = 3600;

/// A record written within this long ago means the box was held tonight:
/// the stop that follows is the expected end of the drain and pages nobody.
pub const RECENT_HOLD_WINDOW_SECS: i64 = 16 * 3600;

/// Clock skew allowed on `written_unix` (the app's clock against the
/// Lambda's). A record stamped this far in the future still counts as
/// recent; one further ahead does not.
pub const WRITTEN_CLOCK_SKEW_SECS: i64 = 300;

/// IST seconds of day from which a hold is ignored (07:30 IST).
pub const HOLD_IGNORED_FROM_IST_SECS: i64 = 7 * 3600 + 30 * 60;

/// IST seconds of day at which a hold counts again (17:25 IST, five minutes
/// before the 17:30 daily stop).
pub const HOLD_IGNORED_UNTIL_IST_SECS: i64 = 17 * 3600 + 25 * 60;

const SECS_PER_DAY: i64 = 86_400;

/// What a stop path should do about the drain hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainHold {
    /// A fresh, valid hold: do not stop the box.
    Active { until_unix: i64 },
    /// No hold now, but the box was held tonight (released, expired, or
    /// ignored after 07:30 IST). A SUCCESSFUL stop of the box that was held
    /// goes without a page; see [`DrainHold::silences_stop_of`].
    RecentlyHeld { written_unix: i64 },
    /// No usable record: behave exactly as before the drain hold existed.
    None,
}

impl DrainHold {
    /// `true` when a stop of a box launched at `launch_unix` may go without
    /// a page: the hold ended tonight AND the box was already running when
    /// the record was written (`launch < written_unix`). A box started after
    /// the record is a different session, and an unknown launch time proves
    /// nothing, so both keep the existing behaviour. The caller must still
    /// page a stop that FAILED.
    pub fn silences_stop_of(&self, launch_unix: Option<i64>) -> bool {
        match (self, launch_unix) {
            (DrainHold::RecentlyHeld { written_unix }, Some(launch)) => launch < *written_unix,
            _ => false,
        }
    }
}

/// The parameter's JSON. `left` is informational and optional; unknown
/// fields are ignored so the writer can add more without breaking readers.
#[derive(Debug, Deserialize)]
struct DrainHoldRecord {
    v: i64,
    state: String,
    until_unix: i64,
    written_unix: i64,
    #[serde(default)]
    left: String,
}

/// IST (fixed UTC+5:30) seconds of day for a unix instant.
fn ist_seconds_of_day(now_unix: i64) -> i64 {
    (now_unix.rem_euclid(SECS_PER_DAY) + i64::from(crate::time::IST_OFFSET_SECS))
        .rem_euclid(SECS_PER_DAY)
}

/// `true` inside [07:30, 17:25) IST, where no hold is honoured.
fn hold_ignored_at(now_unix: i64) -> bool {
    let tod = ist_seconds_of_day(now_unix);
    (HOLD_IGNORED_FROM_IST_SECS..HOLD_IGNORED_UNTIL_IST_SECS).contains(&tod)
}

/// The verdict for one read of the parameter. `raw` is the parameter value,
/// `None` when it is missing or could not be read. Pure apart from logging.
pub fn drain_hold_verdict(raw: Option<&str>, now_unix: i64) -> DrainHold {
    let Some(raw) = raw.map(str::trim).filter(|r| !r.is_empty()) else {
        info!("drain hold absent — no hold");
        return DrainHold::None;
    };
    let record: DrainHoldRecord = match serde_json::from_str(raw) {
        Ok(record) => record,
        Err(e) => {
            warn!(error = %e, "drain hold unparseable — treating as no hold");
            return DrainHold::None;
        }
    };
    if record.v != DRAIN_HOLD_VERSION {
        warn!(
            version = record.v,
            "drain hold has an unknown version — treating as no hold"
        );
        return DrainHold::None;
    }
    let state_known = matches!(record.state.as_str(), "holding" | "released");
    if !state_known {
        warn!(state = %record.state, "drain hold has an unknown state — treating as no hold");
        return DrainHold::None;
    }

    let ahead = record.until_unix.saturating_sub(now_unix);
    if record.state == "holding" && ahead > 0 {
        if ahead > MAX_HOLD_AHEAD_SECS {
            warn!(
                until_unix = record.until_unix,
                ahead_secs = ahead,
                "drain hold expires more than an hour ahead — invalid, not honoured"
            );
        } else if hold_ignored_at(now_unix) {
            info!(
                until_unix = record.until_unix,
                "drain hold ignored between 07:30 and 17:25 IST"
            );
        } else {
            info!(
                until_unix = record.until_unix,
                left = %record.left,
                "drain hold active"
            );
            return DrainHold::Active {
                until_unix: record.until_unix,
            };
        }
    }

    let age = now_unix.saturating_sub(record.written_unix);
    if (-WRITTEN_CLOCK_SKEW_SECS..=RECENT_HOLD_WINDOW_SECS).contains(&age) {
        info!(
            written_unix = record.written_unix,
            state = %record.state,
            "drain hold ended tonight"
        );
        return DrainHold::RecentlyHeld {
            written_unix: record.written_unix,
        };
    }
    info!(
        written_unix = record.written_unix,
        "drain hold is stale — no hold"
    );
    DrainHold::None
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, TimeZone};

    /// Unix seconds for an IST wall-clock time.
    fn ist_unix(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
        FixedOffset::east_opt(crate::time::IST_OFFSET_SECS)
            .expect("ist offset")
            .with_ymd_and_hms(y, mo, d, h, mi, s)
            .single()
            .expect("valid IST time")
            .timestamp()
    }

    fn record(state: &str, until: i64, written: i64) -> String {
        format!(
            r#"{{"v":1,"state":"{state}","until_unix":{until},"written_unix":{written},"left":"3 buckets"}}"#
        )
    }

    fn is_recent(verdict: DrainHold) -> bool {
        matches!(verdict, DrainHold::RecentlyHeld { .. })
    }

    #[test]
    fn test_drain_hold_verdict_recently_held_carries_the_write_instant() {
        let now = evening();
        let raw = record("released", now - 60, now - 90);
        assert_eq!(
            drain_hold_verdict(Some(&raw), now),
            DrainHold::RecentlyHeld {
                written_unix: now - 90
            }
        );
    }

    #[test]
    fn test_silences_stop_of_only_a_box_running_before_the_write() {
        let held = DrainHold::RecentlyHeld {
            written_unix: 1_000,
        };
        assert!(held.silences_stop_of(Some(999)));
        assert!(!held.silences_stop_of(Some(1_000)), "launched at the write");
        assert!(!held.silences_stop_of(Some(1_001)), "a later session");
        assert!(
            !held.silences_stop_of(None),
            "unknown launch proves nothing"
        );
        assert!(!DrainHold::None.silences_stop_of(Some(0)));
        assert!(!DrainHold::Active { until_unix: 2_000 }.silences_stop_of(Some(0)));
    }

    /// Tuesday 2026-10-20 19:00 IST: inside the honoured evening.
    fn evening() -> i64 {
        ist_unix(2026, 10, 20, 19, 0, 0)
    }

    #[test]
    fn test_drain_hold_verdict_fresh_holding_record_in_the_evening_is_active() {
        let now = evening();
        let raw = record("holding", now + 600, now - 30);
        assert_eq!(
            drain_hold_verdict(Some(&raw), now),
            DrainHold::Active {
                until_unix: now + 600
            }
        );
    }

    #[test]
    fn until_equal_to_now_is_not_active_but_recently_held() {
        let now = evening();
        let raw = record("holding", now, now - 300);
        assert!(is_recent(drain_hold_verdict(Some(&raw), now)));
    }

    #[test]
    fn until_exactly_one_hour_ahead_is_active_one_second_more_is_not() {
        let now = evening();
        let raw = record("holding", now + 3600, now);
        assert_eq!(
            drain_hold_verdict(Some(&raw), now),
            DrainHold::Active {
                until_unix: now + 3600
            }
        );
        let raw = record("holding", now + 3601, now);
        assert!(is_recent(drain_hold_verdict(Some(&raw), now)));
    }

    #[test]
    fn the_ignored_window_boundaries_are_0730_and_1725_ist() {
        let at = |h, m, s| ist_unix(2026, 10, 21, h, m, s);
        for (now, active) in [
            (at(7, 29, 59), true),
            (at(7, 30, 0), false),
            (at(12, 0, 0), false),
            (at(17, 24, 59), false),
            (at(17, 25, 0), true),
            (at(23, 59, 59), true),
            (at(0, 0, 0), true),
        ] {
            let raw = record("holding", now + 600, now - 60);
            let verdict = drain_hold_verdict(Some(&raw), now);
            if active {
                assert_eq!(
                    verdict,
                    DrainHold::Active {
                        until_unix: now + 600
                    },
                    "at {now}"
                );
            } else {
                assert!(is_recent(verdict), "at {now}");
            }
        }
    }

    #[test]
    fn a_released_record_is_recently_held_never_active() {
        let now = evening();
        let raw = record("released", now + 600, now - 60);
        assert!(is_recent(drain_hold_verdict(Some(&raw), now)));
    }

    #[test]
    fn recently_held_ends_sixteen_hours_after_the_write() {
        let now = evening();
        let raw = record("released", now - 100, now - RECENT_HOLD_WINDOW_SECS);
        assert!(is_recent(drain_hold_verdict(Some(&raw), now)));
        let raw = record("released", now - 100, now - RECENT_HOLD_WINDOW_SECS - 1);
        assert_eq!(drain_hold_verdict(Some(&raw), now), DrainHold::None);
    }

    #[test]
    fn a_write_stamped_in_the_future_counts_only_within_the_skew() {
        let now = evening();
        let raw = record("released", now - 100, now + WRITTEN_CLOCK_SKEW_SECS);
        assert!(is_recent(drain_hold_verdict(Some(&raw), now)));
        let raw = record("released", now - 100, now + WRITTEN_CLOCK_SKEW_SECS + 1);
        assert_eq!(drain_hold_verdict(Some(&raw), now), DrainHold::None);
    }

    #[test]
    fn an_unknown_version_is_no_hold() {
        let now = evening();
        let raw = format!(
            r#"{{"v":2,"state":"holding","until_unix":{},"written_unix":{now},"left":""}}"#,
            now + 600
        );
        assert_eq!(drain_hold_verdict(Some(&raw), now), DrainHold::None);
    }

    #[test]
    fn an_unknown_state_is_no_hold() {
        let now = evening();
        let raw = record("paused", now + 600, now);
        assert_eq!(drain_hold_verdict(Some(&raw), now), DrainHold::None);
    }

    #[test]
    fn missing_empty_and_malformed_values_are_no_hold() {
        let now = evening();
        for raw in [
            None,
            Some(""),
            Some("   "),
            Some("not json"),
            Some("{}"),
            Some(r#"{"v":1,"state":"holding"}"#),
            Some(r#"{"v":"1","state":"holding","until_unix":1,"written_unix":1}"#),
            Some("[1,2,3]"),
        ] {
            assert_eq!(drain_hold_verdict(raw, now), DrainHold::None, "{raw:?}");
        }
    }

    #[test]
    fn left_is_optional_and_unknown_fields_are_ignored() {
        let now = evening();
        let raw = format!(
            r#"{{"v":1,"state":"holding","until_unix":{},"written_unix":{now},"extra":true}}"#,
            now + 60
        );
        assert_eq!(
            drain_hold_verdict(Some(&raw), now),
            DrainHold::Active {
                until_unix: now + 60
            }
        );
    }

    #[test]
    fn extreme_instants_never_panic() {
        for now in [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX] {
            for (until, written) in [(i64::MIN, i64::MIN), (i64::MAX, i64::MAX), (0, 0)] {
                let raw = record("holding", until, written);
                let _ = drain_hold_verdict(Some(&raw), now);
            }
        }
    }

    /// Exhaustive sweep over every minute of one IST day, with holds of
    /// several lengths: the verdict is Active exactly when the hold is in
    /// (0, 3600] seconds ahead and the minute is outside [07:30, 17:25), and
    /// an Active verdict is never returned from 07:30 IST onward, so no hold
    /// keeps the box past 07:30 the next morning.
    #[test]
    fn every_minute_of_a_day_obeys_the_bounds() {
        let midnight = ist_unix(2026, 10, 21, 0, 0, 0);
        for minute in 0..(24 * 60) {
            let now = midnight + minute * 60;
            let ignored = (7 * 60 + 30..17 * 60 + 25).contains(&minute);
            for ahead in [-60, 0, 1, 600, 3600, 3601, 7200] {
                let raw = record("holding", now + ahead, now - 120);
                let verdict = drain_hold_verdict(Some(&raw), now);
                let expect_active = !ignored && (1..=MAX_HOLD_AHEAD_SECS).contains(&ahead);
                if expect_active {
                    assert_eq!(
                        verdict,
                        DrainHold::Active {
                            until_unix: now + ahead
                        }
                    );
                } else {
                    assert_eq!(
                        verdict,
                        DrainHold::RecentlyHeld {
                            written_unix: now - 120
                        },
                        "minute {minute} ahead {ahead}"
                    );
                }
            }
        }
    }
}

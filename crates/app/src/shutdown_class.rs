//! Pure shutdown classifier (Telegram cleanliness overhaul,
//! coordinator-relayed directive 2026-07-15).
//!
//! Every restart used to pair a boot bubble with a `[MEDIUM] Shutdown
//! initiated` page — even the daily EventBridge 17:30 IST auto-stop. This
//! classifier turns the already-known signal kind + runtime source + IST
//! clock + trading calendar into a [`ShutdownClass`], so the routine stops
//! render one quiet Low line while anything unexpected STAYS Medium and
//! loud.
//!
//! Fail-safe direction: ANY doubt (unknown signal string, AWS SIGTERM
//! outside the two explicit quiet windows) lands
//! [`ShutdownClass::ExternalStop`] — today's loudness, never quieter.
//!
//! G5 (fix round 2, 2026-07-15): the quiet arms are EXPLICIT IST windows
//! only. The original blanket "!is_trading_day → quiet at ANY hour" arm
//! made every external stop on a calendar non-trading day silent —
//! including a live Muhurat evening session kill (daily-universe lock §22
//! contemplates Muhurat operation) and the operator's documented manual
//! weekend/holiday runs (§7 Quote 5) killed mid-session by a budget
//! killswitch or console stop. Now only the holiday-gate self-stop window
//! (08:25–09:00 IST, non-trading days — the box auto-starts 08:30 and the
//! gate stops it minutes later) and the weekday 17:25–17:45 stop-cron
//! window are quiet; everything else stays Medium.
//!
//! Documented residual (plan Edge Cases): a genuine manual/budget stop
//! DURING one of the two quiet windows renders quiet — bounded (the
//! budget alarm pages independently; both windows sit outside the
//! [09:15, 15:30) trading session).

use std::path::Path;

use tickvault_core::notification::events::ShutdownClass;
use tracing::warn;

/// Start of the scheduled-stop IST window: 17:25:00 (the EventBridge stop
/// cron fires 17:30 IST — `cron(0 12 ? * MON-FRI *)`, weekday-only and
/// deliberately NOT holiday-aware; the wide window absorbs the documented
/// EventBridge scheduler jitter).
///
/// MOVED 2026-08-08 with operator Quote 14 ("make it as 8.30 till 5.30 pm").
/// This window is COUPLED to the stop cron and moving one without the other is
/// a real defect, not a stale comment: with the cron at 17:30 and this window
/// still at 16:25–16:45, EVERY ordinary weekday shutdown would fall outside
/// the quiet arm and classify as `OperatorStop` — a loud Telegram page every
/// single trading evening for a completely normal scheduled stop. Same failure
/// class as the boot-heartbeat/stop-verify coupling recorded in
/// daily-universe-scope-expansion-2026-05-27.md §7.
pub const SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST: u32 = 62_700;

/// End (exclusive) of the scheduled-stop IST window: 17:45:00.
pub const SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST: u32 = 63_900;

/// Start of the holiday-gate self-stop IST window: 08:25:00 (G5, fix
/// round 2). The box auto-starts 08:30 IST on weekdays; on a non-trading
/// weekday the holiday gate stops it within minutes — the window absorbs
/// start-cron jitter on both sides.
pub const HOLIDAY_GATE_STOP_WINDOW_START_SECS_OF_DAY_IST: u32 = 30_300;

/// End (exclusive) of the holiday-gate self-stop IST window: 09:00:00.
pub const HOLIDAY_GATE_STOP_WINDOW_END_SECS_OF_DAY_IST: u32 = 32_400;

/// Classify a graceful shutdown. Inputs are all in-process:
///
/// - `signal`: the signal name `wait_for_shutdown_signal` already produces
///   (`"ctrl_c"` / `"sigterm"`; anything else is treated as unknown → loud).
/// - `is_aws`: `true` when the runtime source is the systemd-managed AWS
///   box (`source_badge::runtime_source()`).
/// - `ist_secs_of_day`: IST seconds-of-day at classification time.
/// - `is_weekday`: Mon–Fri (the stop cron is weekday-only).
/// - `is_trading_day`: NSE trading calendar verdict for today — the
///   holiday-gate self-stop stops the box on non-trading days at any hour.
///
/// Truth table (design §4, 2026-07-15; quiet arms narrowed to explicit
/// windows by G5, fix round 2):
///
/// | signal   | is_aws | condition                                  | class         |
/// |----------|--------|--------------------------------------------|---------------|
/// | ctrl_c   | any    | any                                        | OperatorStop  |
/// | sigterm  | false  | any                                        | OperatorStop  |
/// | sigterm  | true   | weekday ∧ 17:25–17:45 IST (any day)        | ScheduledStop |
/// | sigterm  | true   | !is_trading_day ∧ 08:25–09:00 IST          | ScheduledStop |
/// | sigterm  | true   | otherwise (incl. Muhurat / manual weekend) | ExternalStop  |
/// | anything else     | —                                          | ExternalStop  |
#[must_use]
pub fn classify_shutdown(
    signal: &str,
    is_aws: bool,
    ist_secs_of_day: u32,
    is_weekday: bool,
    is_trading_day: bool,
) -> ShutdownClass {
    match signal {
        // The operator is at the keyboard.
        "ctrl_c" => ShutdownClass::OperatorStop,
        "sigterm" => {
            if !is_aws {
                // Local `make stop` / container stop.
                return ShutdownClass::OperatorStop;
            }
            // The daily EventBridge 17:30 IST stop (weekday-only cron,
            // NOT holiday-aware — it fires on weekday holidays too),
            // inside the jitter-absorbing 17:25–17:45 window. Applies on
            // trading AND non-trading weekdays alike.
            if is_weekday
                && (SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST
                    ..SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST)
                    .contains(&ist_secs_of_day)
            {
                return ShutdownClass::ScheduledStop;
            }
            // Holiday-gate self-stop: the box auto-starts 08:30 IST and
            // the gate stops it minutes later on a non-trading day —
            // quiet ONLY inside that morning window (G5). Any OTHER
            // non-trading-day external stop stays loud: a live Muhurat
            // evening session (daily-universe §22) or the operator's
            // manual weekend/holiday run (§7 Quote 5) killed mid-session
            // must page, not vanish as a "scheduled stop".
            if !is_trading_day
                && (HOLIDAY_GATE_STOP_WINDOW_START_SECS_OF_DAY_IST
                    ..HOLIDAY_GATE_STOP_WINDOW_END_SECS_OF_DAY_IST)
                    .contains(&ist_secs_of_day)
            {
                return ShutdownClass::ScheduledStop;
            }
            // Deploy / budget killswitch / manual stop — stays loud.
            ShutdownClass::ExternalStop
        }
        // Unknown signal string: any ambiguity fails TOWARD loud.
        _ => ShutdownClass::ExternalStop,
    }
}

/// Planned-stop marker the deploy pipeline writes immediately BEFORE it
/// restarts the app or stops a box it started (`deploy-aws.yml`,
/// `dhan-rest-only-noise-lock-2026-07-14.md` §2.3x, 2026-09-23). Relative to
/// the app's working directory (`/opt/tickvault` under systemd), so the same
/// path resolves under `data/` on the box and in a local run.
///
/// Why it exists: every deploy restart sent SIGTERM outside the 17:25–17:45
/// quiet window, so [`classify_shutdown`] — correctly, from what it could
/// see — paged "Unexpected stop" for a change the operator merged himself.
/// Nothing in-process can tell a deploy SIGTERM from a manual one; the deploy
/// has to say so, and this file is how it says so.
pub const PLANNED_DEPLOY_MARKER_PATH: &str = "data/planned-restart.marker";

/// Oldest marker still honoured. A deploy writes it and restarts within
/// seconds; 15 minutes covers a slow SSM step while bounding how long a
/// marker left by an aborted deploy could quiet a genuinely unexpected stop.
pub const PLANNED_DEPLOY_MARKER_MAX_AGE_SECS: i64 = 900;

/// A marker stamped in the "future" by at most this much is still honoured
/// (the writer and the app read the same host clock, so any skew is tiny;
/// anything larger is treated as garbage, i.e. loud).
pub const PLANNED_DEPLOY_MARKER_MAX_FUTURE_SKEW_SECS: i64 = 60;

/// Is the marker body a fresh deploy stamp? The body is the writer's
/// `date +%s` (epoch seconds). Pure: unparseable, stale, or implausibly
/// future ⇒ `false`, i.e. the stop stays loud.
#[must_use]
pub fn planned_deploy_marker_is_fresh(contents: &str, now_epoch_secs: i64) -> bool {
    let Ok(written) = contents.trim().parse::<i64>() else {
        return false;
    };
    let age = now_epoch_secs.saturating_sub(written);
    (-PLANNED_DEPLOY_MARKER_MAX_FUTURE_SKEW_SECS..=PLANNED_DEPLOY_MARKER_MAX_AGE_SECS)
        .contains(&age)
}

/// Read AND delete the planned-stop marker, returning whether it announced
/// THIS stop. Consumed on every call — fresh or stale — so one deploy's
/// announcement can never quiet a later stop.
///
/// Fail-loud direction throughout: an absent marker, an unreadable one, or
/// one that cannot be deleted all return `false`. A marker that survives its
/// own consumption would keep quieting stops until it aged out, so a failed
/// delete is treated as "not announced" and logged.
pub fn take_planned_deploy_marker(path: &Path, now_epoch_secs: i64) -> bool {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return false,
        Err(err) => {
            warn!(
                ?err,
                path = %path.display(),
                "planned-deploy marker unreadable — classifying the stop normally"
            );
            // Best effort: a marker we cannot read must not linger either.
            if let Err(remove_err) = std::fs::remove_file(path) {
                warn!(
                    ?remove_err,
                    path = %path.display(),
                    "unreadable planned-deploy marker could not be removed"
                );
            }
            return false;
        }
    };
    if let Err(err) = std::fs::remove_file(path) {
        warn!(
            ?err,
            path = %path.display(),
            "planned-deploy marker could not be consumed — classifying the stop \
             normally so a surviving marker cannot quiet a later stop"
        );
        return false;
    }
    planned_deploy_marker_is_fresh(&contents, now_epoch_secs)
}

/// [`classify_shutdown`] plus the deploy's announcement. A fresh marker turns
/// an AWS SIGTERM into [`ShutdownClass::PlannedDeployRestart`]; every other
/// input — Ctrl+C, a local stop, an unknown signal, no marker — classifies
/// exactly as [`classify_shutdown`] does, so the marker can only ever quiet a
/// stop the deploy positively announced.
#[must_use]
pub fn classify_shutdown_with_deploy_marker(
    signal: &str,
    is_aws: bool,
    ist_secs_of_day: u32,
    is_weekday: bool,
    is_trading_day: bool,
    planned_deploy_marker: bool,
) -> ShutdownClass {
    if planned_deploy_marker && is_aws && signal == "sigterm" {
        return ShutdownClass::PlannedDeployRestart;
    }
    classify_shutdown(signal, is_aws, ist_secs_of_day, is_weekday, is_trading_day)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch path unique to one test, removed by the test itself.
    fn scratch_marker(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("tv-planned-marker-{name}-{}", std::process::id()))
    }

    #[test]
    fn test_classify_with_deploy_marker_table() {
        const MID_SESSION: u32 = 11 * 3600;
        // (signal, is_aws, secs, marker) -> class
        let cases = [
            // The fix: a deploy SIGTERM mid-day is no longer "Unexpected".
            (
                "sigterm",
                true,
                MID_SESSION,
                true,
                ShutdownClass::PlannedDeployRestart,
            ),
            // Without the marker the same stop stays loud (unchanged).
            (
                "sigterm",
                true,
                MID_SESSION,
                false,
                ShutdownClass::ExternalStop,
            ),
            // Out-of-hours stop of a box the deploy started.
            (
                "sigterm",
                true,
                20 * 3600,
                true,
                ShutdownClass::PlannedDeployRestart,
            ),
            // The marker never quiets a non-AWS or non-SIGTERM stop.
            (
                "sigterm",
                false,
                MID_SESSION,
                true,
                ShutdownClass::OperatorStop,
            ),
            (
                "ctrl_c",
                true,
                MID_SESSION,
                true,
                ShutdownClass::OperatorStop,
            ),
            (
                "sighup",
                true,
                MID_SESSION,
                true,
                ShutdownClass::ExternalStop,
            ),
            // No marker, scheduled window: still the ordinary quiet stop.
            (
                "sigterm",
                true,
                STOP_CRON_SECS,
                false,
                ShutdownClass::ScheduledStop,
            ),
        ];
        for (signal, is_aws, secs, marker, expected) in cases {
            assert_eq!(
                classify_shutdown_with_deploy_marker(signal, is_aws, secs, true, true, marker),
                expected,
                "signal={signal} is_aws={is_aws} secs={secs} marker={marker}"
            );
        }
    }

    #[test]
    fn test_planned_deploy_marker_freshness_boundaries() {
        let now = 1_790_000_000_i64;
        assert!(planned_deploy_marker_is_fresh(&now.to_string(), now));
        assert!(planned_deploy_marker_is_fresh(&format!("{now}\n"), now));
        let oldest = now - PLANNED_DEPLOY_MARKER_MAX_AGE_SECS;
        assert!(planned_deploy_marker_is_fresh(&oldest.to_string(), now));
        assert!(!planned_deploy_marker_is_fresh(
            &(oldest - 1).to_string(),
            now
        ));
        let future = now + PLANNED_DEPLOY_MARKER_MAX_FUTURE_SKEW_SECS;
        assert!(planned_deploy_marker_is_fresh(&future.to_string(), now));
        assert!(!planned_deploy_marker_is_fresh(
            &(future + 1).to_string(),
            now
        ));
        for garbage in [
            "",
            "   ",
            "yesterday",
            "12.5",
            "-",
            "9999999999999999999999",
        ] {
            assert!(
                !planned_deploy_marker_is_fresh(garbage, now),
                "garbage {garbage:?} must stay loud"
            );
        }
    }

    #[test]
    fn test_take_planned_deploy_marker_consumes_fresh_and_stale() {
        let now = 1_790_000_000_i64;

        let fresh = scratch_marker("fresh");
        std::fs::write(&fresh, now.to_string()).expect("write fresh marker");
        assert!(take_planned_deploy_marker(&fresh, now));
        assert!(!fresh.exists(), "a fresh marker must be consumed");
        // Consumed ⇒ the next stop is NOT announced.
        assert!(!take_planned_deploy_marker(&fresh, now));

        let stale = scratch_marker("stale");
        std::fs::write(&stale, (now - 3_600).to_string()).expect("write stale marker");
        assert!(!take_planned_deploy_marker(&stale, now));
        assert!(!stale.exists(), "a stale marker must be consumed too");

        let absent = scratch_marker("absent");
        assert!(!take_planned_deploy_marker(&absent, now));
    }

    #[test]
    fn test_planned_deploy_marker_path_is_under_the_data_dir() {
        assert_eq!(PLANNED_DEPLOY_MARKER_PATH, "data/planned-restart.marker");
    }

    /// 17:30:00 IST — the EventBridge stop cron's nominal fire instant.
    const STOP_CRON_SECS: u32 = 17 * 3600 + 30 * 60;

    #[test]
    fn test_classify_shutdown_window_consts_are_1725_and_1745_ist() {
        assert_eq!(
            SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST,
            17 * 3600 + 25 * 60
        );
        assert_eq!(
            SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST,
            17 * 3600 + 45 * 60
        );
        // The nominal 17:30 cron instant sits inside the window.
        assert!(
            (SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST
                ..SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST)
                .contains(&STOP_CRON_SECS)
        );
    }

    #[test]
    fn test_classify_shutdown_ctrl_c_is_operator_stop_everywhere() {
        for is_aws in [true, false] {
            for is_trading in [true, false] {
                assert_eq!(
                    classify_shutdown("ctrl_c", is_aws, STOP_CRON_SECS, true, is_trading),
                    ShutdownClass::OperatorStop
                );
            }
        }
    }

    #[test]
    fn test_classify_shutdown_local_sigterm_is_operator_stop() {
        // `make stop` / `docker stop` on the Mac — any hour, any day.
        assert_eq!(
            classify_shutdown("sigterm", false, 11 * 3600, true, true),
            ShutdownClass::OperatorStop
        );
        assert_eq!(
            classify_shutdown("sigterm", false, STOP_CRON_SECS, true, true),
            ShutdownClass::OperatorStop
        );
    }

    #[test]
    fn test_classify_shutdown_aws_sigterm_in_weekday_window_is_scheduled() {
        assert_eq!(
            classify_shutdown("sigterm", true, STOP_CRON_SECS, true, true),
            ShutdownClass::ScheduledStop
        );
    }

    #[test]
    fn test_classify_shutdown_window_boundaries() {
        // Inclusive start: 17:25:00 exactly is scheduled.
        assert_eq!(
            classify_shutdown(
                "sigterm",
                true,
                SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST,
                true,
                true
            ),
            ShutdownClass::ScheduledStop
        );
        // One second before the window: loud.
        assert_eq!(
            classify_shutdown(
                "sigterm",
                true,
                SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST - 1,
                true,
                true
            ),
            ShutdownClass::ExternalStop
        );
        // Exclusive end: 17:45:00 exactly is already outside — loud.
        assert_eq!(
            classify_shutdown(
                "sigterm",
                true,
                SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST,
                true,
                true
            ),
            ShutdownClass::ExternalStop
        );
        // Last in-window second: 16:44:59 is scheduled.
        assert_eq!(
            classify_shutdown(
                "sigterm",
                true,
                SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST - 1,
                true,
                true
            ),
            ShutdownClass::ScheduledStop
        );
    }

    #[test]
    fn test_classify_shutdown_holiday_gate_self_stop_window_only() {
        // G5 (fix round 2): the non-trading-day quiet arm is the EXPLICIT
        // 08:25–09:00 IST holiday-gate self-stop window — never any-hour.
        // Inside the window (weekday holiday auto-start morning): quiet.
        for secs in [
            HOLIDAY_GATE_STOP_WINDOW_START_SECS_OF_DAY_IST,
            8 * 3600 + 31 * 60,
            HOLIDAY_GATE_STOP_WINDOW_END_SECS_OF_DAY_IST - 1,
        ] {
            assert_eq!(
                classify_shutdown("sigterm", true, secs, true, false),
                ShutdownClass::ScheduledStop,
                "secs={secs}"
            );
        }
        // Boundaries: one second before 08:25 / at 09:00 exactly → loud.
        assert_eq!(
            classify_shutdown(
                "sigterm",
                true,
                HOLIDAY_GATE_STOP_WINDOW_START_SECS_OF_DAY_IST - 1,
                true,
                false
            ),
            ShutdownClass::ExternalStop
        );
        assert_eq!(
            classify_shutdown(
                "sigterm",
                true,
                HOLIDAY_GATE_STOP_WINDOW_END_SECS_OF_DAY_IST,
                true,
                false
            ),
            ShutdownClass::ExternalStop
        );
        // The window is TRADING-day-gated the other way: a trading-day
        // 08:30 external stop (pre-open!) stays loud.
        assert_eq!(
            classify_shutdown("sigterm", true, 8 * 3600 + 30 * 60, true, true),
            ShutdownClass::ExternalStop
        );
    }

    #[test]
    fn test_classify_shutdown_holiday_gate_window_consts_are_0825_and_0900_ist() {
        assert_eq!(
            HOLIDAY_GATE_STOP_WINDOW_START_SECS_OF_DAY_IST,
            8 * 3600 + 25 * 60
        );
        assert_eq!(HOLIDAY_GATE_STOP_WINDOW_END_SECS_OF_DAY_IST, 9 * 3600);
    }

    #[test]
    fn test_classify_shutdown_nontrading_day_arbitrary_hour_stays_loud() {
        // G5 (fix round 2): a live Muhurat EVENING session kill (a calendar
        // holiday with a live session — daily-universe §22), a manual
        // weekend run killed mid-session (§7 Quote 5), and a budget
        // killswitch at an arbitrary holiday hour all page Medium now —
        // the old blanket "!is_trading_day → quiet at any hour" is gone.
        for (secs, is_weekday) in [
            (18 * 3600 + 30 * 60, true), // Muhurat evening (weekday holiday)
            (11 * 3600, false),          // manual Sunday run, mid-morning kill
            (0, false),                  // midnight weekend kill
            (86_399, true),              // last second of a weekday holiday
        ] {
            assert_eq!(
                classify_shutdown("sigterm", true, secs, is_weekday, false),
                ShutdownClass::ExternalStop,
                "secs={secs} weekday={is_weekday}"
            );
        }
        // Weekend 16:30 kill: no stop cron exists on weekends (MON-FRI
        // cron) — a Saturday 16:30 console/budget kill stays loud.
        assert_eq!(
            classify_shutdown("sigterm", true, STOP_CRON_SECS, false, false),
            ShutdownClass::ExternalStop
        );
    }

    #[test]
    fn test_classify_shutdown_weekday_holiday_stop_cron_window_is_scheduled() {
        // The EventBridge stop cron is weekday-only but NOT holiday-aware:
        // a weekday NSE holiday still gets the 16:30 stop — quiet.
        assert_eq!(
            classify_shutdown("sigterm", true, STOP_CRON_SECS, true, false),
            ShutdownClass::ScheduledStop
        );
    }

    #[test]
    fn test_classify_shutdown_aws_sigterm_outside_window_stays_loud() {
        // Mid-market trading-day stop: deploy / budget killswitch / manual.
        assert_eq!(
            classify_shutdown("sigterm", true, 11 * 3600, true, true),
            ShutdownClass::ExternalStop
        );
        // In-window on a trading day that is NOT a weekday cannot happen on
        // NSE (trading ⇒ weekday), but the classifier must still fail
        // toward loud if the inputs ever disagree.
        assert_eq!(
            classify_shutdown("sigterm", true, STOP_CRON_SECS, false, true),
            ShutdownClass::ExternalStop
        );
    }

    #[test]
    fn test_classify_shutdown_unknown_signal_fails_toward_loud() {
        for signal in ["", "sighup", "SIGTERM", "market_close"] {
            assert_eq!(
                classify_shutdown(signal, true, STOP_CRON_SECS, true, true),
                ShutdownClass::ExternalStop,
                "signal={signal:?}"
            );
        }
    }
}

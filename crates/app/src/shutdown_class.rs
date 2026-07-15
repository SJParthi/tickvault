//! Pure shutdown classifier (Telegram cleanliness overhaul,
//! coordinator-relayed directive 2026-07-15).
//!
//! Every restart used to pair a boot bubble with a `[MEDIUM] Shutdown
//! initiated` page — even the daily EventBridge 16:30 IST auto-stop. This
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
//! gate stops it minutes later) and the weekday 16:25–16:45 stop-cron
//! window are quiet; everything else stays Medium.
//!
//! Documented residual (plan Edge Cases): a genuine manual/budget stop
//! DURING one of the two quiet windows renders quiet — bounded (the
//! budget alarm pages independently; both windows sit outside the
//! [09:15, 15:30) trading session).

use tickvault_core::notification::events::ShutdownClass;

/// Start of the scheduled-stop IST window: 16:25:00 (the EventBridge stop
/// cron fires 16:30 IST — `cron(0 11 ? * MON-FRI *)`, weekday-only and
/// deliberately NOT holiday-aware; the wide window absorbs the documented
/// EventBridge scheduler jitter).
pub const SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST: u32 = 59_100;

/// End (exclusive) of the scheduled-stop IST window: 16:45:00.
pub const SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST: u32 = 60_300;

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
/// | sigterm  | true   | weekday ∧ 16:25–16:45 IST (any day)        | ScheduledStop |
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
            // The daily EventBridge 16:30 IST stop (weekday-only cron,
            // NOT holiday-aware — it fires on weekday holidays too),
            // inside the jitter-absorbing 16:25–16:45 window. Applies on
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 16:30:00 IST — the EventBridge stop cron's nominal fire instant.
    const STOP_CRON_SECS: u32 = 16 * 3600 + 30 * 60;

    #[test]
    fn test_classify_shutdown_window_consts_are_1625_and_1645_ist() {
        assert_eq!(
            SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST,
            16 * 3600 + 25 * 60
        );
        assert_eq!(
            SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST,
            16 * 3600 + 45 * 60
        );
        // The nominal 16:30 cron instant sits inside the window.
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
        // Inclusive start: 16:25:00 exactly is scheduled.
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
        // Exclusive end: 16:45:00 exactly is already outside — loud.
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

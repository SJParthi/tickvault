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
//! outside the scheduled window on a trading day) lands
//! [`ShutdownClass::ExternalStop`] — today's loudness, never quieter.
//!
//! Documented residual (plan Edge Cases): a genuine manual/budget stop
//! DURING the 16:25–16:45 weekday window, or on a holiday, renders quiet —
//! bounded (the budget alarm pages independently; holidays carry no market
//! exposure).

use tickvault_core::notification::events::ShutdownClass;

/// Start of the scheduled-stop IST window: 16:25:00 (the EventBridge stop
/// cron fires 16:30 IST — `cron(0 11 ? * MON-FRI *)`, weekday-only and
/// deliberately NOT holiday-aware; the wide window absorbs the documented
/// EventBridge scheduler jitter).
pub const SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST: u32 = 59_100;

/// End (exclusive) of the scheduled-stop IST window: 16:45:00.
pub const SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST: u32 = 60_300;

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
/// Truth table (design §4, 2026-07-15):
///
/// | signal   | is_aws | condition                     | class         |
/// |----------|--------|-------------------------------|---------------|
/// | ctrl_c   | any    | any                           | OperatorStop  |
/// | sigterm  | false  | any                           | OperatorStop  |
/// | sigterm  | true   | !is_trading_day               | ScheduledStop |
/// | sigterm  | true   | weekday ∧ 16:25–16:45 IST     | ScheduledStop |
/// | sigterm  | true   | otherwise                     | ExternalStop  |
/// | anything else     | —                             | ExternalStop  |
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
            // Holiday-gate self-stop: the box stops itself on non-trading
            // days at any hour — never an incident.
            if !is_trading_day {
                return ShutdownClass::ScheduledStop;
            }
            // The daily EventBridge 16:30 IST stop (weekday-only cron),
            // inside the jitter-absorbing 16:25–16:45 window.
            if is_weekday
                && (SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST
                    ..SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST)
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
    fn test_classify_shutdown_holiday_gate_self_stop_is_scheduled_any_hour() {
        // A non-trading day (weekend / NSE holiday) AWS SIGTERM at ANY hour
        // is the holiday-gate self-stop — never an incident page.
        for secs in [0, 9 * 3600, 11 * 3600 + 15 * 60, STOP_CRON_SECS, 86_399] {
            assert_eq!(
                classify_shutdown("sigterm", true, secs, false, false),
                ShutdownClass::ScheduledStop,
                "secs={secs}"
            );
        }
        // A holiday that falls on a WEEKDAY (auto-start morning) too.
        assert_eq!(
            classify_shutdown("sigterm", true, 9 * 3600, true, false),
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

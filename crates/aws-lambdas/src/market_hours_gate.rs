//! Market-hours liveness window gate — Rust port of the
//! `market-hours-liveness-alarm.tf` inline legacy heredoc
//! (`tv-${env}-market-hours-liveness-gate`, phase 2b-1).
//!
//! mode="open"  (09:20 IST) → enable the gated alarms' actions, but ONLY if
//!                            (a) the holiday-stop marker is NOT today and
//!                            (b) the tv-app box is actually up; then reset
//!                            each alarm to OK.
//! mode="close" (15:35 IST) → disable them again.
//!
//! Environment variables (unchanged): `ALARM_NAMES` (comma-separated),
//! `EC2_INSTANCE_ID`, `HOLIDAY_STOP_PARAM`.
//!
//! FAIL-OPEN parity: an SSM error on the holiday marker = not a holiday; a
//! DescribeInstances error = treat as up — a real trading day must never
//! lose the liveness page.

use chrono::Utc;
use lambda_runtime::Error;
use serde_json::{Value, json};
use tracing::{info, warn};

pub use crate::alarm_gate::GateMode;

/// The exact SetAlarmState reason the heredoc used.
pub const MARKET_OPEN_STATE_REASON: &str = "market-hours window opened (09:20 IST)";

/// Instance states that count as "up". Legacy parity: `'pending'` counts —
/// a late trading-day start must still arm the window (the OK reset +
/// 5-15 min evaluation absorb the boot).
pub fn state_counts_as_up(state: &str) -> bool {
    matches!(state, "running" | "pending")
}

/// Classify the DescribeInstances result SHAPE. `None` = the call
/// SUCCEEDED but the reservations/instances/state chain was missing —
/// FAIL-OPEN (up), exactly like the Err arm (hostile-review r1 F3):
/// the legacy heredoc's doctrine is "a real trading day must never lose
/// the liveness page", and the legacy runtime failed OPEN on a missing shape;
/// only a POSITIVE non-up state may leave the alarms disabled.
pub fn classify_instance_state(state: Option<String>) -> (bool, String) {
    match state {
        Some(s) => (state_counts_as_up(&s), s),
        None => (true, "unknown".to_string()),
    }
}

/// Legacy parity: `[n.strip() for n in raw.split(',') if n.strip()]`.
pub fn parse_alarm_names(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Marker == today is AUTHORITATIVE for "intentionally stopped today".
/// `raw_param = None` is the SSM-error / missing-param arm → fail-open
/// (not a holiday). Stale markers (a previous holiday's date) never match.
pub fn holiday_marker_matches_today(raw_param: Option<&str>, today_ist: &str) -> bool {
    match raw_param {
        Some(raw) => raw.trim() == today_ist,
        None => false,
    }
}

/// The open-path decision, pure over its three inputs — mirrors the
/// heredoc's ordering: marker check FIRST (race-proof, round 3), instance
/// state second (covers the marker-less manual-stop case, round 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenDecision {
    SkipHolidayStop,
    SkipInstanceDown { state: String },
    Enable,
}

pub fn open_decision(holiday_stop_today: bool, instance_up: bool, state: &str) -> OpenDecision {
    if holiday_stop_today {
        return OpenDecision::SkipHolidayStop;
    }
    if !instance_up {
        return OpenDecision::SkipInstanceDown {
            state: state.to_string(),
        };
    }
    OpenDecision::Enable
}

/// Result JSON shapes — legacy parity:
/// holiday skip  → `{'mode','enabled':false,'holiday_stop':true}`
/// instance skip → `{'mode','enabled':false,'instance_state':state}`
/// enabled open  → `{'mode','enabled':true}`
/// close         → `{'mode','enabled':false}`
pub fn open_result(mode: GateMode, decision: &OpenDecision) -> Value {
    match decision {
        OpenDecision::SkipHolidayStop => {
            json!({"mode": mode.as_str(), "enabled": false, "holiday_stop": true})
        }
        OpenDecision::SkipInstanceDown { state } => {
            json!({"mode": mode.as_str(), "enabled": false, "instance_state": state})
        }
        OpenDecision::Enable => json!({"mode": mode.as_str(), "enabled": true}),
    }
}

/// Entry point. UNPROVEN until deploy: the live SSM/EC2/CloudWatch legs
/// run only in a real Lambda invoke.
pub async fn handle(event: Value) -> Result<Value, Error> {
    let alarm_names_raw =
        std::env::var("ALARM_NAMES").map_err(|_| Error::from("ALARM_NAMES env var is missing"))?;
    let instance_id = std::env::var("EC2_INSTANCE_ID")
        .map_err(|_| Error::from("EC2_INSTANCE_ID env var is missing"))?;
    let holiday_param = std::env::var("HOLIDAY_STOP_PARAM")
        .map_err(|_| Error::from("HOLIDAY_STOP_PARAM env var is missing"))?;

    let alarm_names = parse_alarm_names(&alarm_names_raw);
    let mode = GateMode::from_event(&event);

    let config = crate::clients::sdk_config().await;
    let cw = crate::clients::cloudwatch(&config);

    if mode == GateMode::Close {
        cw.disable_alarm_actions()
            .set_alarm_names(Some(alarm_names.clone()))
            .send()
            .await?;
        info!(alarms = ?alarm_names, "disabled actions");
        return Ok(json!({"mode": mode.as_str(), "enabled": false}));
    }

    // --- mode == open ---
    let ssm = crate::clients::ssm(&config);
    let today_ist = crate::time::today_ist_string(Utc::now());
    // FAIL-OPEN: any SSM error / missing param = not a holiday.
    let marker: Option<String> = match ssm.get_parameter().name(&holiday_param).send().await {
        Ok(r) => r.parameter().and_then(|p| p.value()).map(str::to_string),
        Err(e) => {
            warn!(error = %e, "holiday-stop marker unavailable -- fail-open, not a holiday");
            None
        }
    };
    let holiday_stop_today = holiday_marker_matches_today(marker.as_deref(), &today_ist);

    // FAIL-OPEN: any DescribeInstances error enables as before.
    let ec2 = crate::clients::ec2(&config);
    let (instance_up, state) = match ec2
        .describe_instances()
        .instance_ids(&instance_id)
        .send()
        .await
    {
        // Missing shape on a SUCCESSFUL call routes through
        // classify_instance_state's None arm → fail-open (legacy parity).
        Ok(r) => classify_instance_state(
            r.reservations()
                .first()
                .and_then(|res| res.instances().first())
                .and_then(|i| i.state())
                .and_then(|s| s.name())
                .map(|n| n.as_str().to_string()),
        ),
        Err(e) => {
            warn!(error = %e, "describe_instances failed -- fail-open, treating as up");
            (true, "unknown".to_string())
        }
    };

    let decision = open_decision(holiday_stop_today, instance_up, &state);
    match &decision {
        OpenDecision::SkipHolidayStop => {
            info!(
                alarms = ?alarm_names,
                "holiday-stop marker == today (NSE holiday self-stop); leaving actions disabled"
            );
        }
        OpenDecision::SkipInstanceDown { state } => {
            info!(
                instance = %instance_id,
                state = %state,
                alarms = ?alarm_names,
                "intentional stop (NSE holiday self-stop / manual); leaving actions disabled"
            );
        }
        OpenDecision::Enable => {
            // ORDER IS LOAD-BEARING -- corrected 2026-08-28. See
            // `alarm_gate.rs` for the full reasoning; in short: AWS runs an
            // alarm's actions on `SetAlarmState` whenever the new state
            // differs from the old, so enabling BEFORE resetting paged the
            // operator every trading morning at 09:20.
            //
            // Five of the gated alarms are `treat_missing_data = breaching`
            // AND carry `ok_actions` -- dhan_live_lane_down,
            // dhan_no_ticks_flowing, depth_steering_stalled,
            // market_hours_liveness_missing, app_log_ingestion_silent. The box
            // stops at 17:30, they enter ALARM overnight with actions
            // disabled (correct, no page), and the old order then enabled
            // actions and immediately transitioned them ALARM -> OK, firing
            // five "recovered" messages for a condition that was the box being
            // switched off on schedule. Roughly 110 pages a month, all noise,
            // all at the same minute.
            //
            // Resetting while actions are still disabled costs nothing, keeps
            // every alarm's genuine in-session recovery signal, and also shuts
            // the mirror-image window in which a stale ALARM briefly had live
            // actions.
            // THE RESET MUST NOT BE ABLE TO BLOCK THE ENABLE -- 2026-08-28,
            // same day, correcting the change directly above.
            //
            // The first version of this reordering used `?` inside the loop.
            // That is a strictly WORSE failure mode than the bug it fixed: one
            // failed `SetAlarmState` returns early, `enable_alarm_actions` never
            // runs, and EVERY gated alarm stays action-disabled for the entire
            // trading day. Trading five spurious morning pages for a chance of
            // total alerting silence is not a trade worth making.
            //
            // So a failed reset is COUNTED and LOGGED and the loop continues.
            // The worst case is now the OLD behaviour for that one alarm -- a
            // stale ALARM transitioning to OK with actions live, i.e. one
            // spurious recovery page -- which is exactly the noise this change
            // set out to remove, and infinitely better than silence.
            //
            // `enable_alarm_actions` is therefore unconditional and is the ONLY
            // call in this arm that may propagate: if the alarms cannot be
            // armed at all, the invocation must fail loudly so the Lambda's
            // Errors alarm fires.
            // ARM EACH ALARM AS SOON AS IT IS RESET -- 2026-08-29, closing the
            // last hole an adversarial review found in the two corrections
            // above.
            //
            // Those corrections handled a failed `SetAlarmState`. Neither
            // handled the INVOCATION dying: this Lambda has a 30s timeout, and
            // under CloudWatch throttling the SDK retries each call with
            // backoff. A timeout part-way through a reset-all-then-enable-all
            // shape means `enable_alarm_actions` never runs and ALL of the
            // gated alarms stay unarmed for the whole trading day -- the total
            // silence the comment above correctly calls the worse trade, just
            // reached through a door it was not watching.
            //
            // Pairing the two calls per alarm removes the trade rather than
            // choosing a side of it. A death mid-loop now leaves every
            // ALREADY-PROCESSED alarm armed and only the remainder unarmed,
            // instead of losing all of them. The reset still happens while
            // that alarm's actions are disabled, so the spurious-page property
            // the reorder was made for is unchanged.
            let mut reset_failures = 0usize;
            let mut arm_failures = 0usize;
            for name in &alarm_names {
                if let Err(err) = cw
                    .set_alarm_state()
                    .alarm_name(name)
                    .state_value(aws_sdk_cloudwatch::types::StateValue::Ok)
                    .state_reason(MARKET_OPEN_STATE_REASON)
                    .send()
                    .await
                {
                    reset_failures += 1;
                    tracing::error!(
                        code = "LAMBDA-AWS-02",
                        alarm = %name,
                        error = %err,
                        "could not pre-reset this alarm to OK before arming. Arming \
                         continues regardless -- the cost is at most one spurious \
                         recovery page for this alarm, and the alternative is an \
                         unarmed alarm for the whole session."
                    );
                }
                if let Err(err) = cw.enable_alarm_actions().alarm_names(name).send().await {
                    arm_failures += 1;
                    tracing::error!(
                        code = "LAMBDA-AWS-02",
                        alarm = %name,
                        error = %err,
                        "could not arm this alarm -- the others are armed independently, \
                         and if NONE of them arms the invocation fails below"
                    );
                }
            }
            // There is exactly ONE arming call in this file (the guard in
            // `cloudwatch_agent_glob_guard.rs` counts them, so this comment
            // deliberately does not spell the token), and it is the per-alarm
            // one inside the loop above. A second bulk
            // call would read as an unconditional arm to anyone auditing this
            // arm -- the shape the holiday guard exists to forbid -- and it
            // bought nothing the loop does not already do: the loop arms every
            // alarm independently, so a mid-loop failure never costs the rest.
            //
            // What the bulk call DID carry was the loud-failure property, and
            // that is kept here explicitly, at the SAME strength: the bulk `?`
            // failed the invocation on any arming error, so ANY unarmed alarm
            // fails it here too. Deliberately not "all of them failed" -- one
            // unarmed alarm is one signal that pages nobody for the session,
            // and returning Ok on that is the false-OK class this repo forbids.
            //
            // Reset failures are NOT fatal, and the asymmetry is the point: a
            // failed pre-reset costs at most one spurious recovery page, while
            // a failed arm costs the page itself.
            if arm_failures > 0 {
                return Err(Error::from(format!(
                    "could not arm {arm_failures} of {} gated alarms -- each is a \
                     liveness signal that would page nobody for the session",
                    alarm_names.len()
                )));
            }
            info!(
                alarms = ?alarm_names,
                reset_failures,
                arm_failures,
                "enabled actions"
            );
        }
    }
    Ok(open_result(mode, &decision))
}

#[cfg(test)]
mod tests {

    /// A failed pre-reset must never prevent the alarms being ARMED.
    ///
    /// The reordering that removed the daily false-recovery flood was first
    /// written with `?` inside the reset loop. That is a strictly worse failure
    /// than the one it fixed: one failed `SetAlarmState` returns early,
    /// `enable_alarm_actions` never runs, and every gated alarm stays
    /// action-disabled for the whole trading day. Five spurious pages traded
    /// for a chance of total silence is not a trade worth making.
    ///
    /// Source-order and shape assertion, because the failure is entirely about
    /// which call can abort which -- there is no way to observe it from a unit
    /// test without an AWS client.
    #[test]
    fn a_failed_alarm_reset_cannot_block_arming() {
        // BOTH gates, and the scope is the point (2026-08-28).
        //
        // This test read only `market_hours_gate.rs` when it was written, and
        // `alarm_gate.rs` carried the IDENTICAL `.await?` on its reset — so the
        // fix this test pinned was half a fix, and the test was structurally
        // unable to say so. A guard scoped to one of two identical call sites
        // certifies the site it can see and quietly blesses the one it cannot.
        for (label, source) in [
            ("market_hours_gate", include_str!("market_hours_gate.rs")),
            ("alarm_gate", include_str!("alarm_gate.rs")),
        ] {
            let enable = source
                .find("enable_alarm_actions()")
                .unwrap_or_else(|| panic!("{label}: the open path must arm the alarms"));
            let reset = source[..enable]
                .rfind("set_alarm_state()")
                .unwrap_or_else(|| panic!("{label}: the open path must reset to OK first"));
            // Everything between the reset call and the arm call. If a `?` sits
            // in there, one transient SetAlarmState failure returns early and
            // the arm never happens.
            let body = &source[reset..enable];
            assert!(
                !body.contains(".await?"),
                "{label}: the pre-arm reset propagates with `?`. One failed \
                 SetAlarmState then returns early and enable_alarm_actions NEVER RUNS, \
                 leaving the gated alarm(s) silent for the entire session it was being \
                 armed for. A failed reset must be logged and arming must proceed"
            );
            assert!(
                body.contains("tracing::error!") || body.contains("reset_failures"),
                "{label}: a failed reset must be VISIBLE — silently swallowing it hides \
                 that an alarm may fire one spurious recovery page, and hides that the \
                 reset is failing at all"
            );
        }
    }

    /// The gates must RESET to OK before ENABLING actions, never after.
    ///
    /// AWS runs an alarm's actions on `SetAlarmState` whenever the new state
    /// differs from the old one. Enabling first therefore turned the daily
    /// window open into a page: the box stops at 17:30, the five gated alarms
    /// that are `treat_missing_data = breaching` AND carry `ok_actions` enter
    /// ALARM overnight (correctly silent, actions disabled), and the 09:20
    /// reset then fired five "recovered" messages for a condition that was the
    /// box being switched off on schedule -- roughly 110 pages a month, all
    /// noise, all at the same minute.
    ///
    /// Pinned as a source-order assertion because there is no way to observe
    /// AWS's action dispatch from a unit test: the bug is entirely in WHICH
    /// call happens first, and that is exactly what this reads.
    #[test]
    fn the_gates_reset_to_ok_before_enabling_actions() {
        for (label, source) in [
            ("market_hours_gate", include_str!("market_hours_gate.rs")),
            ("alarm_gate", include_str!("alarm_gate.rs")),
        ] {
            let enable = source
                .find("enable_alarm_actions()")
                .unwrap_or_else(|| panic!("{label}: the open path must enable actions"));
            let reset = source
                .find("set_alarm_state()")
                .unwrap_or_else(|| panic!("{label}: the open path must reset to OK"));
            assert!(
                reset < enable,
                "{label}: SetAlarmState(OK) must come BEFORE enable_alarm_actions. \
                 Enabling first makes the daily window-open transition ALARM -> OK \
                 with live actions, which pages the operator every trading morning \
                 for alarms that were never broken -- and leaves a window in which a \
                 stale ALARM has live actions too"
            );
        }
    }
    use super::*;

    #[test]
    fn test_parse_alarm_names_splits_trims_and_drops_empties() {
        assert_eq!(
            parse_alarm_names("a, b ,,c , "),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert!(parse_alarm_names("").is_empty());
        assert!(parse_alarm_names(" , ,").is_empty());
    }

    #[test]
    fn test_parse_alarm_names_current_prod_list_is_4() {
        // The 2026-07-15 trimmed set the tf ALARM_NAMES join produces.
        let raw = "tv-prod-market-hours-liveness-missing,tv-prod-app-log-ingestion-silent,tv-prod-boundary-catchup-storm-dhan,tv-prod-dhan-exchange-lag-p99-high";
        assert_eq!(parse_alarm_names(raw).len(), 4);
    }

    #[test]
    fn test_holiday_marker_matches_only_today() {
        assert!(holiday_marker_matches_today(
            Some("2026-07-18"),
            "2026-07-18"
        ));
        assert!(holiday_marker_matches_today(
            Some("  2026-07-18\n"),
            "2026-07-18"
        ));
        // Stale marker (a previous holiday) never matches.
        assert!(!holiday_marker_matches_today(
            Some("2026-06-17"),
            "2026-07-18"
        ));
        assert!(!holiday_marker_matches_today(Some(""), "2026-07-18"));
    }

    #[test]
    fn test_holiday_marker_fails_open_on_ssm_error() {
        // None models the SSM-error / missing-param arm.
        assert!(!holiday_marker_matches_today(None, "2026-07-18"));
    }

    #[test]
    fn test_state_counts_as_up_running_and_pending_only() {
        assert!(state_counts_as_up("running"));
        assert!(state_counts_as_up("pending"));
        assert!(!state_counts_as_up("stopped"));
        assert!(!state_counts_as_up("stopping"));
        assert!(!state_counts_as_up("terminated"));
        assert!(!state_counts_as_up("shutting-down"));
    }

    #[test]
    fn test_classify_instance_state_missing_shape_fails_open_as_up() {
        // Hostile-review r1 F3: a SUCCESSFUL DescribeInstances whose
        // reservations/instances/state shape is missing must fail OPEN
        // (up=true) exactly like the Err arm — legacy parity: a real
        // trading day must never lose the liveness page.
        assert_eq!(classify_instance_state(None), (true, "unknown".to_string()));
        // A POSITIVE state still classifies normally.
        assert_eq!(
            classify_instance_state(Some("running".to_string())),
            (true, "running".to_string())
        );
        assert_eq!(
            classify_instance_state(Some("stopped".to_string())),
            (false, "stopped".to_string())
        );
    }

    #[test]
    fn test_open_decision_holiday_wins_over_instance_state() {
        // Marker check FIRST — race-proof (round 3): even an up instance
        // (a restart-war up-burst at the 09:20 sample) stays disabled.
        assert_eq!(
            open_decision(true, true, "running"),
            OpenDecision::SkipHolidayStop
        );
    }

    #[test]
    fn test_open_decision_instance_down_skips() {
        assert_eq!(
            open_decision(false, false, "stopped"),
            OpenDecision::SkipInstanceDown {
                state: "stopped".to_string()
            }
        );
    }

    #[test]
    fn test_open_decision_enables_on_trading_day_with_box_up() {
        assert_eq!(open_decision(false, true, "running"), OpenDecision::Enable);
    }

    #[test]
    fn test_open_result_shapes_match_legacy() {
        assert_eq!(
            open_result(GateMode::Open, &OpenDecision::SkipHolidayStop),
            json!({"mode": "open", "enabled": false, "holiday_stop": true})
        );
        assert_eq!(
            open_result(
                GateMode::Open,
                &OpenDecision::SkipInstanceDown {
                    state: "stopped".to_string()
                }
            ),
            json!({"mode": "open", "enabled": false, "instance_state": "stopped"})
        );
        assert_eq!(
            open_result(GateMode::Open, &OpenDecision::Enable),
            json!({"mode": "open", "enabled": true})
        );
    }

    #[test]
    fn test_market_open_state_reason_is_legacy_literal() {
        assert_eq!(
            MARKET_OPEN_STATE_REASON,
            "market-hours window opened (09:20 IST)"
        );
    }
}

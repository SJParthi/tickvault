//! Market-hours liveness window gate — Rust port of the
//! `market-hours-liveness-alarm.tf` inline Python heredoc
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

/// Instance states that count as "up". Python parity: `'pending'` counts —
/// a late trading-day start must still arm the window (the OK reset +
/// 5-15 min evaluation absorb the boot).
pub fn state_counts_as_up(state: &str) -> bool {
    matches!(state, "running" | "pending")
}

/// Classify the DescribeInstances result SHAPE. `None` = the call
/// SUCCEEDED but the reservations/instances/state chain was missing —
/// FAIL-OPEN (up), exactly like the Err arm (hostile-review r1 F3):
/// the Python heredoc's doctrine is "a real trading day must never lose
/// the liveness page", and the python failed OPEN on a missing shape;
/// only a POSITIVE non-up state may leave the alarms disabled.
pub fn classify_instance_state(state: Option<String>) -> (bool, String) {
    match state {
        Some(s) => (state_counts_as_up(&s), s),
        None => (true, "unknown".to_string()),
    }
}

/// Python parity: `[n.strip() for n in raw.split(',') if n.strip()]`.
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

/// Result JSON shapes — Python parity:
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
        // classify_instance_state's None arm → fail-open (python parity).
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
            cw.enable_alarm_actions()
                .set_alarm_names(Some(alarm_names.clone()))
                .send()
                .await?;
            // Reset to OK on open so a stale ALARM from a prior window does
            // not immediately re-fire on the first enabled evaluation.
            for name in &alarm_names {
                cw.set_alarm_state()
                    .alarm_name(name)
                    .state_value(aws_sdk_cloudwatch::types::StateValue::Ok)
                    .state_reason(MARKET_OPEN_STATE_REASON)
                    .send()
                    .await?;
            }
            info!(alarms = ?alarm_names, "enabled actions");
        }
    }
    Ok(open_result(mode, &decision))
}

#[cfg(test)]
mod tests {
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
        // (up=true) exactly like the Err arm — python parity: a real
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
    fn test_open_result_shapes_match_python() {
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
    fn test_market_open_state_reason_is_python_literal() {
        assert_eq!(
            MARKET_OPEN_STATE_REASON,
            "market-hours window opened (09:20 IST)"
        );
    }
}

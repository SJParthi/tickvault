//! Boot-heartbeat window gate — Rust port of the `boot-heartbeat-alarm.tf`
//! inline legacy heredoc (`tv-${env}-boot-heartbeat-gate`, phase 2b-1).
//!
//! mode="open"  (08:50 IST) → enable the alarm's actions for the boot window
//!                            and reset it to OK so a stale ALARM from a
//!                            prior window does not immediately re-fire.
//! mode="close" (09:20 IST) → disable them again so the nightly/weekend stop
//!                            (metric goes missing intentionally) never pages.
//!
//! Environment variables (unchanged): `ALARM_NAME`.

use lambda_runtime::Error;
use serde_json::{Value, json};
use tracing::info;

/// The exact SetAlarmState reason the heredoc used.
pub const BOOT_OPEN_STATE_REASON: &str = "boot-heartbeat window opened (08:50 IST)";

/// The gate mode — legacy parity: `(event or {}).get('mode', 'close')`,
/// then `if mode == 'open'` — ANY other value (missing, null, garbage)
/// behaves as close (the fail-safe direction: actions disabled).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateMode {
    Open,
    Close,
}

impl GateMode {
    pub fn from_event(event: &Value) -> Self {
        match event.get("mode").and_then(Value::as_str) {
            Some("open") => GateMode::Open,
            _ => GateMode::Close,
        }
    }

    /// The literal echoed back in the result JSON. Parity nuance: the
    /// The legacy runtime echoed the RAW mode string (so `{"mode":"garbage"}` came
    /// back verbatim while acting as close). We echo the EFFECTIVE mode —
    /// a deliberate, documented deviation (the result value is only read
    /// by humans in CloudWatch logs; the acted-on behavior is identical).
    pub fn as_str(self) -> &'static str {
        match self {
            GateMode::Open => "open",
            GateMode::Close => "close",
        }
    }
}

/// Result JSON — legacy parity: `{'mode': mode, 'enabled': bool}`.
pub fn gate_result(mode: GateMode, enabled: bool) -> Value {
    json!({"mode": mode.as_str(), "enabled": enabled})
}

/// Entry point. UNPROVEN until deploy: the live CloudWatch legs run only
/// in a real Lambda invoke.
pub async fn handle(event: Value) -> Result<Value, Error> {
    let alarm_name =
        std::env::var("ALARM_NAME").map_err(|_| Error::from("ALARM_NAME env var is missing"))?;
    let mode = GateMode::from_event(&event);

    let config = crate::clients::sdk_config().await;
    let cw = crate::clients::cloudwatch(&config);

    match mode {
        GateMode::Open => {
            // ORDER IS LOAD-BEARING -- corrected 2026-08-28.
            //
            // Reset FIRST, enable SECOND. AWS runs an alarm's actions on
            // `SetAlarmState` whenever the new state differs from the old one,
            // so doing this the other way round paged the operator every single
            // trading morning: the box stops at 17:30, `treat_missing_data =
            // breaching` puts the alarm into ALARM overnight, and the 09:20
            // reset to OK then fired `ok_actions` -- a "recovered" message for
            // a condition that was the box being switched off on schedule.
            //
            // Dropping `ok_actions` would also have silenced the case that
            // matters, where one of these alarms fires mid-session and really
            // does recover. Resetting while actions are still disabled costs
            // nothing and keeps that signal.
            //
            // It also closes the mirror-image hole: enabling first left a
            // window in which a STALE ALARM had live actions, so the old order
            // could fire a spurious alarm page as well as a spurious recovery.
            // THE RESET MUST NOT BE ABLE TO BLOCK THE ARM (2026-08-28).
            //
            // This was `.await?`, and the `?` is the whole defect: a transient
            // SetAlarmState failure propagated out of the handler and
            // `enable_alarm_actions` below never ran — leaving the alarm
            // action-DISABLED for the entire session it was being armed for.
            // A best-effort tidy-up would have silenced the alarm it exists to
            // arm, which is strictly worse than the stale-state page the reset
            // is here to avoid.
            //
            // Recorded plainly because this is the SECOND time: the identical
            // bug was found and fixed in `market_hours_gate.rs` earlier the same
            // day, and left standing here — the guard that pinned the fix read
            // `include_str!("market_hours_gate.rs")` and was structurally unable
            // to see this file. A guard scoped to one of two identical call
            // sites is a guard that certifies half a fix.
            if let Err(err) = cw
                .set_alarm_state()
                .alarm_name(&alarm_name)
                .state_value(aws_sdk_cloudwatch::types::StateValue::Ok)
                .state_reason(BOOT_OPEN_STATE_REASON)
                .send()
                .await
            {
                tracing::error!(
                    code = "LAMBDA-AWS-02",
                    alarm = %alarm_name,
                    error = %err,
                    "could not reset the alarm to OK before arming it — arming anyway. \
                     The cost of proceeding is one possible stale page; the cost of \
                     returning here is an alarm that stays disabled all session."
                );
            }
            cw.enable_alarm_actions()
                .alarm_names(&alarm_name)
                .send()
                .await?;
            info!(alarm = %alarm_name, "enabled actions");
            Ok(gate_result(mode, true))
        }
        GateMode::Close => {
            cw.disable_alarm_actions()
                .alarm_names(&alarm_name)
                .send()
                .await?;
            info!(alarm = %alarm_name, "disabled actions");
            Ok(gate_result(mode, false))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mode_open_parses() {
        assert_eq!(
            GateMode::from_event(&json!({"mode": "open"})),
            GateMode::Open
        );
    }

    #[test]
    fn test_mode_close_parses() {
        assert_eq!(
            GateMode::from_event(&json!({"mode": "close"})),
            GateMode::Close
        );
    }

    #[test]
    fn test_missing_or_garbage_mode_defaults_to_close() {
        // legacy: `(event or {}).get('mode', 'close')` + `== 'open'` gate.
        assert_eq!(GateMode::from_event(&json!({})), GateMode::Close);
        assert_eq!(GateMode::from_event(&json!(null)), GateMode::Close);
        assert_eq!(
            GateMode::from_event(&json!({"mode": "OPEN"})),
            GateMode::Close,
            "mode compare is case-sensitive, like the legacy == 'open'"
        );
        assert_eq!(
            GateMode::from_event(&json!({"mode": 42})),
            GateMode::Close,
            "non-string mode behaves like a missing key"
        );
    }

    #[test]
    fn test_gate_result_shapes_match_legacy() {
        assert_eq!(
            gate_result(GateMode::Open, true),
            json!({"mode": "open", "enabled": true})
        );
        assert_eq!(
            gate_result(GateMode::Close, false),
            json!({"mode": "close", "enabled": false})
        );
    }

    #[test]
    fn test_open_state_reason_is_legacy_literal() {
        assert_eq!(
            BOOT_OPEN_STATE_REASON,
            "boot-heartbeat window opened (08:50 IST)"
        );
    }
}

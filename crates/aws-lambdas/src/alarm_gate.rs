//! Boot-heartbeat window gate — Rust port of the `boot-heartbeat-alarm.tf`
//! inline Python heredoc (`tv-${env}-boot-heartbeat-gate`, phase 2b-1).
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

/// The gate mode — Python parity: `(event or {}).get('mode', 'close')`,
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
    /// Python echoed the RAW mode string (so `{"mode":"garbage"}` came
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

/// Result JSON — Python parity: `{'mode': mode, 'enabled': bool}`.
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
            cw.enable_alarm_actions()
                .alarm_names(&alarm_name)
                .send()
                .await?;
            // Reset to OK on open so a stale ALARM from a prior window does
            // not immediately re-fire on the first enabled evaluation.
            cw.set_alarm_state()
                .alarm_name(&alarm_name)
                .state_value(aws_sdk_cloudwatch::types::StateValue::Ok)
                .state_reason(BOOT_OPEN_STATE_REASON)
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
        // Python: `(event or {}).get('mode', 'close')` + `== 'open'` gate.
        assert_eq!(GateMode::from_event(&json!({})), GateMode::Close);
        assert_eq!(GateMode::from_event(&json!(null)), GateMode::Close);
        assert_eq!(
            GateMode::from_event(&json!({"mode": "OPEN"})),
            GateMode::Close,
            "mode compare is case-sensitive, like the Python == 'open'"
        );
        assert_eq!(
            GateMode::from_event(&json!({"mode": 42})),
            GateMode::Close,
            "non-string mode behaves like a missing key"
        );
    }

    #[test]
    fn test_gate_result_shapes_match_python() {
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
    fn test_open_state_reason_is_python_literal() {
        assert_eq!(
            BOOT_OPEN_STATE_REASON,
            "boot-heartbeat window opened (08:50 IST)"
        );
    }
}

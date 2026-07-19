//! Event types shared by the Lambda bins.
//!
//! Two invoke shapes exist in this fleet:
//! - SNS pushes (budget-killswitch): the standard SNS envelope
//!   `{"Records":[{"Sns":{"Subject":..,"Message":..}}]}`. The killswitch
//!   parses it tolerantly via `serde_json::Value` (mirroring the Python
//!   `dict.get` chain exactly) — the typed structs here document the shape
//!   and serve tests/other consumers.
//! - EventBridge scheduled rules with a constant JSON `input`
//!   (`{"mode":"open"}` / `{"mode":"close"}`) for the two window gates.

use serde::Deserialize;

/// The constant EventBridge `input` the gate crons send.
///
/// Python parity: `(event or {}).get('mode', 'close')` — a missing/odd
/// field means `close` (the fail-safe direction: actions disabled).
#[derive(Debug, Clone, Deserialize)]
pub struct GateEvent {
    #[serde(default)]
    pub mode: Option<String>,
}

/// One record of the standard SNS → Lambda envelope.
#[derive(Debug, Clone, Deserialize)]
pub struct SnsRecord {
    #[serde(rename = "Sns", default)]
    pub sns: Option<SnsBody>,
}

/// The `Sns` body of a record.
#[derive(Debug, Clone, Deserialize)]
pub struct SnsBody {
    #[serde(rename = "Subject", default)]
    pub subject: Option<String>,
    #[serde(rename = "Message", default)]
    pub message: Option<String>,
}

/// The full SNS → Lambda envelope.
#[derive(Debug, Clone, Deserialize)]
pub struct SnsEnvelope {
    #[serde(rename = "Records", default)]
    pub records: Vec<SnsRecord>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_event_parses_mode_open() {
        let e: GateEvent = serde_json::from_str(r#"{"mode":"open"}"#).unwrap();
        assert_eq!(e.mode.as_deref(), Some("open"));
    }

    #[test]
    fn gate_event_tolerates_missing_mode() {
        let e: GateEvent = serde_json::from_str("{}").unwrap();
        assert!(e.mode.is_none());
    }

    #[test]
    fn sns_envelope_parses_typical_budget_push() {
        let e: SnsEnvelope = serde_json::from_str(
            r#"{"Records":[{"Sns":{"Subject":"AWS Budgets: budget threshold ALARM","Message":"Actual: $12.50"}}]}"#,
        )
        .unwrap();
        assert_eq!(e.records.len(), 1);
        let sns = e.records[0].sns.as_ref().unwrap();
        assert_eq!(
            sns.subject.as_deref(),
            Some("AWS Budgets: budget threshold ALARM")
        );
        assert_eq!(sns.message.as_deref(), Some("Actual: $12.50"));
    }

    #[test]
    fn sns_envelope_tolerates_empty_object() {
        let e: SnsEnvelope = serde_json::from_str("{}").unwrap();
        assert!(e.records.is_empty());
    }
}

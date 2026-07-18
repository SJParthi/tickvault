//! Budget kill-switch — Z+ L7 COOLDOWN layer (Rust port of
//! `deploy/aws/lambda/budget-killswitch/handler.py`, phase 2b-1).
//!
//! Subscribed to the dedicated `tv-${env}-budget-kill` SNS topic. AWS
//! Budgets publishes here when monthly spend crosses 100% of the
//! operator-locked limit (aws-budget.md). On invocation:
//!   1. Stops the tv-app EC2 instance (caps further EC2-hour spend)
//!   2. Publishes a Critical message to the operator's `tv_alerts` topic so
//!      the existing Telegram webhook pages the operator immediately
//!
//! Environment variables (set by Terraform — unchanged from the Python):
//!   EC2_INSTANCE_ID  — instance to stop on budget breach
//!   ALERTS_TOPIC_ARN — operator's tv_alerts SNS topic for Telegram
//!   LOG_LEVEL        — INFO (default) / DEBUG / WARNING
//!
//! Parity notes: every pure helper mirrors its `_snake_case` Python
//! original (`_extract_budget_message`, `_format_alert_payload`, the env
//! guards, the SNS 99-char subject truncation, the re-raise-on-error
//! semantics so the Lambda Errors metric + SNS retry policy still fire).

use lambda_runtime::Error;
use serde_json::{Value, json};
use tracing::{error, info};

/// SNS hard subject limit is 100 chars; the Python truncated at 99.
pub const SNS_SUBJECT_MAX: usize = 99;

/// Message-body truncation cap — Python: `message[:1000] + " …(truncated)"`.
pub const MESSAGE_TRUNCATE_CHARS: usize = 1000;

/// One instance state transition from an `ec2:StopInstances` response,
/// decoupled from the SDK types so formatting stays pure/testable.
#[derive(Debug, Clone, Default)]
pub struct StateTransition {
    pub instance_id: Option<String>,
    pub previous_state: Option<String>,
    pub current_state: Option<String>,
}

/// The SNS publish payload for the operator alert.
#[derive(Debug, Clone)]
pub struct AlertPayload {
    pub subject: String,
    pub message: String,
}

/// Pull a human-readable summary out of the SNS budget envelope.
///
/// Python parity (`_extract_budget_message`): tolerant `dict.get` chain —
/// no Records → `<no SNS Records>`; missing Subject/Message →
/// `<no subject>` / `<no message>`; bodies over 1000 chars truncate with
/// ` …(truncated)`. Operates on `Value` to mirror the dict semantics
/// exactly (a Records entry of the wrong type behaves like a missing key).
pub fn extract_budget_message(event: &Value) -> String {
    let Some(first) = event
        .get("Records")
        .and_then(Value::as_array)
        .and_then(|r| r.first())
    else {
        return "<no SNS Records>".to_string();
    };
    let sns = first.get("Sns");
    let subject = sns
        .and_then(|s| s.get("Subject"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("<no subject>");
    let raw_message = sns
        .and_then(|s| s.get("Message"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("<no message>");
    let message = if raw_message.chars().count() > MESSAGE_TRUNCATE_CHARS {
        let head: String = raw_message.chars().take(MESSAGE_TRUNCATE_CHARS).collect();
        format!("{head} …(truncated)")
    } else {
        raw_message.to_string()
    };
    format!("Subject: {subject}\n{message}")
}

/// Build the operator-alert SNS payload.
///
/// Python parity (`_format_alert_payload`): transitions render
/// `"<id>: <prev> -> <curr>"` joined by `", "`, with `?` for any missing
/// key and `<no state change reported>` when the list is empty; the body
/// carries the Trigger context, the numbered Next steps, and the
/// aws-budget.md §6 cost note verbatim.
pub fn format_alert_payload(
    instance_id: &str,
    state_changes: &[StateTransition],
    budget_context: &str,
) -> AlertPayload {
    let transitions = if state_changes.is_empty() {
        "<no state change reported>".to_string()
    } else {
        state_changes
            .iter()
            .map(|sc| {
                format!(
                    "{}: {} -> {}",
                    sc.instance_id.as_deref().unwrap_or("?"),
                    sc.previous_state.as_deref().unwrap_or("?"),
                    sc.current_state.as_deref().unwrap_or("?"),
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let message = format!(
        "BUDGET KILL-SWITCH ACTIVATED\n\
         Instance {instance_id} stop requested.\n\
         Transition: {transitions}\n\
         \n\
         Trigger:\n{budget_context}\n\
         \n\
         Next steps:\n\
         \x20 1. Inspect AWS Cost Explorer for the runaway cost source.\n\
         \x20 2. Fix the underlying issue (rogue stress test, unintended\n\
         \x20    resource, exchange-rate spike, etc).\n\
         \x20 3. Restart via AWS Console or `aws ec2 start-instances`.\n\
         \x20 4. Charter aws-budget.md §6: EIP + EBS continue to accrue at\n\
         \x20    ~Rs 0.51/hour while stopped (vs Rs 1.90/hour running)."
    );
    AlertPayload {
        subject: "BUDGET KILL-SWITCH ACTIVATED".to_string(),
        message,
    }
}

/// Char-boundary-safe SNS subject truncation (Python `[:99]`).
pub fn truncate_subject(subject: &str) -> String {
    subject.chars().take(SNS_SUBJECT_MAX).collect()
}

/// Env-var guards — Python parity: empty `EC2_INSTANCE_ID` /
/// `ALERTS_TOPIC_ARN` short-circuit with `{"ok": false, "reason": ...}`
/// (checked in this order) BEFORE any AWS client is built.
pub fn guard_config(instance_id: &str, topic_arn: &str) -> Result<(), Value> {
    if instance_id.is_empty() {
        error!("EC2_INSTANCE_ID env var is empty — cannot stop instance");
        return Err(json!({"ok": false, "reason": "missing EC2_INSTANCE_ID"}));
    }
    if topic_arn.is_empty() {
        error!("ALERTS_TOPIC_ARN env var is empty — cannot send operator alert");
        return Err(json!({"ok": false, "reason": "missing ALERTS_TOPIC_ARN"}));
    }
    Ok(())
}

/// Render the success return value — Python parity:
/// `{"ok": True, "instance_id": ..., "stop_state_changes": [...]}`.
pub fn success_result(instance_id: &str, state_changes: &[StateTransition]) -> Value {
    let changes: Vec<Value> = state_changes
        .iter()
        .map(|sc| {
            json!({
                "InstanceId": sc.instance_id,
                "PreviousState": {"Name": sc.previous_state},
                "CurrentState": {"Name": sc.current_state},
            })
        })
        .collect();
    json!({
        "ok": true,
        "instance_id": instance_id,
        "stop_state_changes": changes,
    })
}

/// Entry point — invoked by SNS publish from AWS Budgets.
///
/// UNPROVEN until deploy: the live `ec2:StopInstances` + `sns:Publish`
/// legs run only in a real Lambda. Errors are propagated (`?`) so the
/// Lambda Errors metric increments and SNS retries — the Python re-raise
/// semantics.
pub async fn handle(event: Value) -> Result<Value, Error> {
    let instance_id = std::env::var("EC2_INSTANCE_ID").unwrap_or_default();
    let topic_arn = std::env::var("ALERTS_TOPIC_ARN").unwrap_or_default();
    if let Err(not_ok) = guard_config(&instance_id, &topic_arn) {
        return Ok(not_ok);
    }

    let config = crate::clients::sdk_config().await;
    let ec2 = crate::clients::ec2(&config);
    let sns = crate::clients::sns(&config);

    let budget_context = extract_budget_message(&event);
    info!(
        context = %budget_context.chars().take(200).collect::<String>(),
        "budget kill triggered"
    );

    // ec2:StopInstances is idempotent — stopping an already-stopped
    // instance returns the current state, not an error.
    let stop_result = ec2
        .stop_instances()
        .instance_ids(&instance_id)
        .send()
        .await
        .map_err(|e| {
            error!(error = %e, "ec2:StopInstances failed — instance may still be running");
            e
        })?;
    let state_changes: Vec<StateTransition> = stop_result
        .stopping_instances()
        .iter()
        .map(|sc| StateTransition {
            instance_id: sc.instance_id().map(str::to_string),
            previous_state: sc
                .previous_state()
                .and_then(|s| s.name())
                .map(|n| n.as_str().to_string()),
            current_state: sc
                .current_state()
                .and_then(|s| s.name())
                .map(|n| n.as_str().to_string()),
        })
        .collect();
    info!(transitions = state_changes.len(), "ec2 stop requested");

    let payload = format_alert_payload(&instance_id, &state_changes, &budget_context);
    let publish = sns
        .publish()
        .topic_arn(&topic_arn)
        .subject(truncate_subject(&payload.subject))
        .message(&payload.message)
        .send()
        .await
        .map_err(|e| {
            // EC2 is already stopped (good!) but the operator may not know —
            // the Lambda self-error alarm in TF catches this and pages via
            // the same tv_alerts pipe.
            error!(error = %e, "sns:Publish to operator alerts topic failed");
            e
        })?;
    info!(message_id = ?publish.message_id(), "operator alert published");

    Ok(success_result(&instance_id, &state_changes))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- ExtractBudgetMessage (Python: 3 tests) ----

    #[test]
    fn test_empty_event_returns_placeholder() {
        let out = extract_budget_message(&json!({}));
        assert!(out.contains("<no SNS Records>"));
    }

    #[test]
    fn test_typical_sns_envelope_extracts_subject_and_body() {
        let event = json!({
            "Records": [{
                "Sns": {
                    "Subject": "AWS Budgets: budget threshold ALARM",
                    "Message": "Budget Name: tv-prod-monthly-budget\nActual: $12.50",
                }
            }]
        });
        let out = extract_budget_message(&event);
        assert!(out.contains("AWS Budgets"));
        assert!(out.contains("tv-prod-monthly-budget"));
        assert!(out.contains("$12.50"));
    }

    #[test]
    fn test_long_message_truncates_under_telegram_cap() {
        let event = json!({
            "Records": [{"Sns": {"Subject": "x", "Message": "y".repeat(5000)}}]
        });
        let out = extract_budget_message(&event);
        assert!(out.contains("…(truncated)"));
        assert!(out.chars().count() < 1500);
    }

    // ---- FormatAlertPayload (Python: 6 tests) ----

    fn one_transition(id: &str) -> Vec<StateTransition> {
        vec![StateTransition {
            instance_id: Some(id.to_string()),
            previous_state: Some("running".to_string()),
            current_state: Some("stopping".to_string()),
        }]
    }

    #[test]
    fn test_payload_subject_is_under_sns_99_char_limit() {
        let out = format_alert_payload("i-deadbeef", &one_transition("i-deadbeef"), "some context");
        assert!(truncate_subject(&out.subject).chars().count() <= 99);
        assert!(out.subject.chars().count() <= 99);
    }

    #[test]
    fn test_payload_includes_kill_switch_marker() {
        let out = format_alert_payload("i-x", &[], "ctx");
        assert!(out.subject.contains("BUDGET KILL-SWITCH ACTIVATED"));
        assert!(out.message.contains("BUDGET KILL-SWITCH ACTIVATED"));
    }

    #[test]
    fn test_payload_renders_state_transition() {
        let out = format_alert_payload("i-cafe", &one_transition("i-cafe"), "ctx");
        assert!(out.message.contains("i-cafe: running -> stopping"));
    }

    #[test]
    fn test_payload_handles_empty_state_changes() {
        let out = format_alert_payload("i-y", &[], "ctx");
        assert!(out.message.contains("<no state change reported>"));
        assert!(out.message.contains("Next steps:"));
    }

    #[test]
    fn test_payload_includes_cost_note_for_stopped_state() {
        let out = format_alert_payload("i-z", &[], "ctx");
        assert!(out.message.contains("aws-budget.md"));
    }

    #[test]
    fn test_payload_handles_missing_state_keys_gracefully() {
        // AWS APIs sometimes return partial responses on degraded paths.
        let partial = vec![StateTransition {
            instance_id: Some("i-no-state".to_string()),
            previous_state: None,
            current_state: None,
        }];
        let out = format_alert_payload("i-no-state", &partial, "ctx");
        assert!(out.message.contains("i-no-state"));
        assert!(out.message.contains("i-no-state: ? -> ?"));
        assert!(!out.message.contains("Traceback"));
    }

    // ---- LambdaHandlerGuards (Python: 2 tests) ----
    // The Python tests mutated module globals then called lambda_handler;
    // here the guard is a pure fn over the same two values — no process-env
    // mutation (test-parallelism-safe), same observable outcomes.

    #[test]
    fn test_missing_instance_id_returns_not_ok() {
        let out = guard_config("", "arn:aws:sns:ap-south-1:111:tv_alerts").unwrap_err();
        assert_eq!(out["ok"], json!(false));
        assert!(out["reason"].as_str().unwrap().contains("EC2_INSTANCE_ID"));
    }

    #[test]
    fn test_missing_topic_arn_returns_not_ok() {
        let out = guard_config("i-x", "").unwrap_err();
        assert_eq!(out["ok"], json!(false));
        assert!(out["reason"].as_str().unwrap().contains("ALERTS_TOPIC_ARN"));
    }

    // ---- Rust-side additions beyond the Python suite ----

    #[test]
    fn test_guard_passes_with_both_set() {
        assert!(guard_config("i-x", "arn:aws:sns:ap-south-1:111:t").is_ok());
    }

    #[test]
    fn test_missing_subject_and_message_render_placeholders() {
        let event = json!({"Records": [{"Sns": {}}]});
        let out = extract_budget_message(&event);
        assert!(out.contains("<no subject>"));
        assert!(out.contains("<no message>"));
    }

    #[test]
    fn test_truncate_subject_is_char_boundary_safe() {
        let s = "₹".repeat(150); // multibyte chars
        let t = truncate_subject(&s);
        assert_eq!(t.chars().count(), 99);
    }

    #[test]
    fn test_success_result_shape() {
        let out = success_result("i-x", &one_transition("i-x"));
        assert_eq!(out["ok"], json!(true));
        assert_eq!(out["instance_id"], json!("i-x"));
        assert_eq!(
            out["stop_state_changes"][0]["PreviousState"]["Name"],
            json!("running")
        );
    }

    #[test]
    fn test_multiple_transitions_join_with_comma() {
        let mut v = one_transition("i-a");
        v.extend(one_transition("i-b"));
        let out = format_alert_payload("i-a", &v, "ctx");
        assert!(
            out.message
                .contains("i-a: running -> stopping, i-b: running -> stopping")
        );
    }
}

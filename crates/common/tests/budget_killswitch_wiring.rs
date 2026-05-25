//! Ratchet: the budget kill-switch chain stays end-to-end wired.
//!
//! The chain:
//!   AWS Budgets 100% actual notification
//!     -> publishes to tv-${env}-budget-kill SNS topic
//!     -> triggers budget-killswitch Lambda
//!     -> Lambda calls ec2:StopInstances on tv_app
//!     -> Lambda publishes Severity::Critical to tv_alerts
//!     -> Telegram webhook Lambda (PR #781) pages operator
//!
//! Without these guards, removing any single hop silently breaks the
//! L7 COOLDOWN layer — the operator would only discover this at the
//! next budget overrun, when the bill is already accruing.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .expect("workspace root must exist above crates/common") // APPROVED: test
}

fn read(rel: &str) -> String {
    let p = workspace_root().join(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())) // APPROVED: test
}

#[test]
fn test_killswitch_lambda_source_exists() {
    for rel in &[
        "deploy/aws/lambda/budget-killswitch/handler.py",
        "deploy/aws/lambda/budget-killswitch/test_handler.py",
    ] {
        let path = workspace_root().join(rel);
        assert!(
            path.exists(),
            "Z+ L7 COOLDOWN ratchet: {rel} is missing. Budget kill-switch \
             Lambda will not deploy."
        );
    }
}

#[test]
fn test_killswitch_handler_is_python_module() {
    let body = read("deploy/aws/lambda/budget-killswitch/handler.py");
    assert!(
        body.contains("def lambda_handler"),
        "Z+ L7 COOLDOWN ratchet: handler.py missing lambda_handler entry \
         point — AWS Lambda runtime expects handler.lambda_handler."
    );
}

#[test]
fn test_killswitch_test_handler_is_executable_via_unittest() {
    let body = read("deploy/aws/lambda/budget-killswitch/test_handler.py");
    assert!(
        body.contains("unittest.main()") || body.contains("import unittest"),
        "Z+ L7 COOLDOWN ratchet: test_handler.py is not a unittest module — \
         CI cannot run it as `python3 -m unittest test_handler`."
    );
}

#[test]
fn test_killswitch_terraform_wiring_exists() {
    let path = workspace_root().join("deploy/aws/terraform/budget-killswitch-lambda.tf");
    assert!(
        path.exists(),
        "Z+ L7 COOLDOWN ratchet: budget-killswitch-lambda.tf is missing. \
         Without it the Lambda is not provisioned even if handler.py exists."
    );
    let mode = fs::metadata(&path).unwrap().permissions().mode(); // APPROVED: test
    assert!(mode & 0o444 != 0, "tf file must be readable");
}

#[test]
fn test_killswitch_uses_dedicated_sns_topic_not_tv_alerts() {
    // Safety-critical invariant: the Lambda subscribes to a DEDICATED
    // topic, NOT tv_alerts. If it subscribed to tv_alerts, every
    // CloudWatch alarm fire (WS dead, QuestDB slow, etc.) would
    // accidentally stop the instance — catastrophic.
    let tf = read("deploy/aws/terraform/budget-killswitch-lambda.tf");
    assert!(
        tf.contains("aws_sns_topic.tv_budget_kill"),
        "Z+ L4 PREVENT ratchet: kill-switch Lambda must subscribe to the \
         dedicated tv_budget_kill SNS topic, not the shared tv_alerts. \
         Without the dedicated topic, any CloudWatch alarm would stop the \
         instance."
    );
    // Confirm the subscription target IS the dedicated topic (not tv_alerts).
    // Find the aws_sns_topic_subscription block and check its topic_arn.
    let sub_block_start = tf
        .find("aws_sns_topic_subscription\" \"budget_killswitch\"")
        .expect("kill-switch subscription block must exist"); // APPROVED: test
    let sub_block = &tf[sub_block_start..(sub_block_start + 400).min(tf.len())];
    assert!(
        sub_block.contains("aws_sns_topic.tv_budget_kill.arn"),
        "Z+ L4 PREVENT ratchet: kill-switch SNS subscription's topic_arn \
         is not tv_budget_kill.arn. The Lambda may be wired to the wrong \
         topic."
    );
    assert!(
        !sub_block.contains("aws_sns_topic.tv_alerts.arn"),
        "Z+ L4 PREVENT ratchet: kill-switch SNS subscription accidentally \
         points at tv_alerts.arn instead of the dedicated kill topic. This \
         would auto-stop the instance on EVERY CloudWatch alarm."
    );
}

#[test]
fn test_killswitch_iam_scopes_stop_to_tv_app_only() {
    // Safety-critical invariant: the IAM policy must scope
    // ec2:StopInstances to the tv_app instance ARN ONLY. A wildcard
    // (resources = ["*"]) would let a compromised Lambda stop the
    // entire account's EC2 fleet.
    let tf = read("deploy/aws/terraform/budget-killswitch-lambda.tf");
    assert!(
        tf.contains("ec2:StopInstances"),
        "Z+ L4 PREVENT ratchet: IAM policy missing ec2:StopInstances action."
    );
    assert!(
        tf.contains("instance/${aws_instance.tv_app.id}"),
        "Z+ L4 PREVENT ratchet: ec2:StopInstances must be ARN-scoped to \
         the tv_app instance. A wildcard resource would let a compromised \
         Lambda stop other instances in the account."
    );
    // Reject explicit wildcard.
    let stop_block_start = tf.find("ec2:StopInstances").unwrap(); // APPROVED: test
    let stop_block = &tf[stop_block_start..(stop_block_start + 300).min(tf.len())];
    assert!(
        !stop_block.contains("resources = [\"*\"]"),
        "Z+ L4 PREVENT ratchet: ec2:StopInstances has a wildcard resource \
         scope. Tighten to the tv_app instance ARN."
    );
}

#[test]
fn test_budget_100pct_notification_wires_kill_topic() {
    // L3 RECONCILE: the budget.tf 100% notification MUST publish to the
    // tv_budget_kill topic. Without this wiring the Lambda never fires.
    let tf = read("deploy/aws/terraform/budget.tf");
    // Find the second `notification {` block (100% actual).
    let mut iter = tf.match_indices("notification {");
    let _first = iter
        .next()
        .expect("expected at least one notification block"); // APPROVED: test
    let (second_idx, _) = iter
        .next()
        .expect("expected second notification block (100% actual)"); // APPROVED: test
    let second_block = &tf[second_idx..(second_idx + 600).min(tf.len())];
    assert!(
        second_block.contains("aws_sns_topic.tv_budget_kill.arn"),
        "Z+ L3 RECONCILE ratchet: budget.tf 100% actual notification \
         must publish to aws_sns_topic.tv_budget_kill.arn. Without this \
         wiring the kill-switch Lambda is never invoked."
    );
    assert!(
        second_block.contains("100"),
        "Z+ L3 RECONCILE ratchet: kill-switch should attach to the 100% \
         actual notification, not the 80% forecasted (which is intentional \
         early warning, not a kill trigger)."
    );
}

#[test]
fn test_killswitch_has_self_error_alarm() {
    // L2 VERIFY of the L7 layer itself: if the kill-switch Lambda errors,
    // the budget cap is offline. Operator MUST be paged through the
    // regular tv_alerts pipe.
    let tf = read("deploy/aws/terraform/budget-killswitch-lambda.tf");
    assert!(
        tf.contains("budget_killswitch_errors"),
        "Z+ L2 VERIFY ratchet: kill-switch Lambda missing self-error \
         CloudWatch alarm. If the Lambda itself errors, the budget cap is \
         silently offline."
    );
    assert!(
        tf.contains("aws_sns_topic.tv_alerts.arn"),
        "Z+ L2 VERIFY ratchet: kill-switch self-error alarm must route \
         alarm_actions back through tv_alerts so the operator's Telegram \
         pipe (PR #781) fires."
    );
}

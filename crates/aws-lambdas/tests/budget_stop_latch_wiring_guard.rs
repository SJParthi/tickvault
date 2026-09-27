//! Build-failing ratchet for the **budget-stop latch** (audit PR30,
//! 2026-09-27): one test per writer and per reader.
//!
//! A budget stop used to be undone the same day by four things that start
//! the box without looking at the budget — the 08:45 start watchdog's
//! self-start, `scripts/aws-autopilot.sh`'s up-window self-start,
//! `deploy-aws.yml`'s start of a stopped box, and the terraform apply that
//! set the `daily-start` rule back to ENABLED after the hourly guard
//! disabled it. The latch is one SSM parameter,
//! `/tickvault-guard/<env>/budget-stop-month` (outside the box's own
//! writable `/tickvault/<env>/*` prefix), holding the UTC billing month of
//! the stop. Two writers (the hard-stop guard's breach stop, the AWS-Budgets
//! kill-switch), four readers (the watchdog, the autopilot, the deploy
//! workflow, BOTH jobs of the apply workflow) and one release (the guard
//! re-enables the rule once a new billing month starts). Remove any one leg
//! and the war comes back quietly: every leg fails OPEN by design, so a
//! missing grant or a missing env var degrades to the old behaviour with no
//! error at all. That is why each leg is pinned
//! here rather than trusted.
//!
//! Every pin reads CODE lines only (comment lines are dropped first), so a
//! dated comment can never satisfy one.

#![cfg(test)]

use std::path::{Path, PathBuf};

const LATCH_PATH: &str = "/tickvault-guard/${var.environment}/budget-stop-month";
const LATCH_ARN_TAIL: &str = "parameter/tickvault-guard/${var.environment}/budget-stop-month";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/aws-lambdas parent")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let path: &Path = &repo_root().join(rel);
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

/// The file with comment lines removed and every run of spaces collapsed to
/// one, so HCL alignment padding never breaks a pin.
fn code_only(body: &str) -> String {
    body.lines()
        .filter(|l| {
            let t = l.trim_start();
            !(t.starts_with('#') || t.starts_with("//"))
        })
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n")
}

fn position(body: &str, needle: &str, what: &str) -> usize {
    body.find(needle)
        .unwrap_or_else(|| panic!("{what}: `{needle}` not found"))
}

#[test]
fn writer_hard_stop_guard_latches_after_the_stop_and_before_the_disable() {
    let rs = code_only(&read("crates/aws-lambdas/src/hard_stop_guard.rs"));
    let body_start = position(&rs, "pub async fn execute_breach_stop", "breach stop");
    let body = &rs[body_start..];
    let stop = position(body, "ec2.stop_instances(&env.instance_id)", "breach stop");
    let latch = position(
        body,
        "ssm.put_parameter(&env.budget_stop_param, &month)",
        "breach stop latch write",
    );
    let disable = position(
        body,
        "events.disable_rule(&env.start_rule_name)",
        "breach stop",
    );
    assert!(
        stop < latch && latch < disable,
        "the breach stop must stop the box FIRST, then write the budget-stop \
         latch, then disable the start rule"
    );

    let tf = code_only(&read("deploy/aws/terraform/budget-guards.tf"));
    assert!(
        tf.contains(&format!("BUDGET_STOP_PARAM = \"{LATCH_PATH}\"")),
        "budget-guards.tf must pass BUDGET_STOP_PARAM to the hard-stop guard"
    );
    let grant = position(&tf, "Sid = \"WriteBudgetStopLatch\"", "guard IAM");
    let grant_block = &tf[grant..grant + 400];
    assert!(
        grant_block.contains("\"ssm:PutParameter\"") && grant_block.contains(LATCH_ARN_TAIL),
        "the hard-stop guard role must be able to PutParameter the latch, \
         scoped to its one ARN"
    );
}

#[test]
fn writer_budget_killswitch_latches_after_the_stop() {
    let rs = code_only(&read("crates/aws-lambdas/src/budget_killswitch.rs"));
    let stop = position(&rs, ".stop_instances()", "kill-switch stop");
    let latch = position(&rs, ".put_parameter()", "kill-switch latch write");
    let publish = position(&rs, "payload.message.push_str(&note)", "kill-switch page");
    assert!(
        stop < latch && latch < publish,
        "the kill-switch must stop the box, then write the latch, then page \
         (reporting the latch outcome)"
    );

    let tf = code_only(&read("deploy/aws/terraform/budget-killswitch-lambda.tf"));
    assert!(
        tf.contains(&format!("BUDGET_STOP_PARAM = \"{LATCH_PATH}\"")),
        "budget-killswitch-lambda.tf must pass BUDGET_STOP_PARAM"
    );
    let grant = position(&tf, "sid = \"WriteBudgetStopLatch\"", "kill-switch IAM");
    let grant_block = &tf[grant..grant + 400];
    assert!(
        grant_block.contains("\"ssm:PutParameter\"") && grant_block.contains(LATCH_ARN_TAIL),
        "the kill-switch role must be able to PutParameter the latch"
    );
}

#[test]
fn reader_start_watchdog_checks_the_latch_before_the_self_start() {
    let rs = code_only(&read("crates/aws-lambdas/src/start_watchdog.rs"));
    let run_start = position(&rs, "pub async fn run_watchdog", "watchdog");
    let run = &rs[run_start..];
    let latch = position(
        run,
        "budget_stop_is_this_month(ssm, &env.budget_stop_param, now)",
        "watchdog latch read",
    );
    let start = position(
        run,
        "try_self_start(ec2, &env.instance_id)",
        "watchdog self-start",
    );
    assert!(
        latch < start,
        "the 08:45 check must consult the budget-stop latch BEFORE its self-start"
    );

    let tf = code_only(&read("deploy/aws/terraform/start-watchdog-lambda.tf"));
    assert!(
        tf.contains(&format!("BUDGET_STOP_PARAM = \"{LATCH_PATH}\"")),
        "start-watchdog-lambda.tf must pass BUDGET_STOP_PARAM"
    );
    assert!(
        tf.contains(LATCH_ARN_TAIL),
        "the start watchdog role must be able to GetParameter the latch; \
         without it every read fails open and the self-start returns"
    );
}

#[test]
fn reader_autopilot_checks_the_latch_before_its_self_start() {
    let sh = code_only(&read("scripts/aws-autopilot.sh"));
    let latch = position(&sh, "budget-stop-month", "autopilot latch read");
    let branch = position(
        &sh,
        "elif [ -n \"$BUDGET_LATCH\" ] && [ \"$BUDGET_LATCH\" = \"$(date -u +%Y-%m)\" ]; then",
        "autopilot latch branch",
    );
    let start = position(
        &sh,
        "aws ec2 start-instances --region \"$REGION\" --instance-ids \"$INSTANCE_ID\"",
        "autopilot self-start",
    );
    assert!(
        latch < branch && branch < start,
        "autopilot must read and honour the latch BEFORE its up-window start-instances"
    );

    let oidc = code_only(&read("deploy/aws/terraform/oidc.tf"));
    assert!(
        oidc.contains(LATCH_ARN_TAIL),
        "oidc.tf must let the autopilot role GetParameter the latch"
    );
}

#[test]
fn reader_terraform_apply_plans_the_start_rule_disabled_while_latched() {
    let main_tf = code_only(&read("deploy/aws/terraform/main.tf"));
    let rule = position(
        &main_tf,
        "resource \"aws_cloudwatch_event_rule\" \"daily_start\"",
        "daily_start rule",
    );
    let rule_block = &main_tf[rule..rule + 400];
    assert!(
        rule_block.contains("state = var.daily_start_enabled ? \"ENABLED\" : \"DISABLED\""),
        "the daily_start rule state must follow var.daily_start_enabled, or \
         an apply re-enables the rule the guard disabled"
    );

    let vars = code_only(&read("deploy/aws/terraform/variables.tf"));
    let var = position(&vars, "variable \"daily_start_enabled\"", "variables.tf");
    let var_block = &vars[var..var + 400];
    assert!(
        var_block.contains("default = true"),
        "daily_start_enabled must default to true (a local apply without the \
         latch step keeps the morning start)"
    );

    // BOTH jobs: the apply job re-plans on its own runner, and $GITHUB_ENV
    // does not cross jobs, so a latch read only in the plan job leaves the
    // REAL apply re-enabling the rule (the first version of PR30 did exactly
    // that; the hostile review caught it).
    let wf = code_only(&read(".github/workflows/terraform-apply.yml"));
    let plan_job = position(&wf, "\nterraform-plan:", "plan job");
    let apply_job = position(&wf, "\nterraform-apply:", "apply job");
    assert!(plan_job < apply_job, "plan job precedes apply job");
    for (job, job_start, job_end, action) in [
        ("plan", plan_job, apply_job, "- name: terraform plan"),
        ("apply", apply_job, wf.len(), "- name: terraform apply"),
    ] {
        let body = &wf[job_start..job_end];
        let read_step = position(body, "- name: Read the budget-stop latch", job);
        let act = position(body, action, job);
        assert!(
            read_step < act,
            "the {job} job must read the latch BEFORE `{action}`"
        );
        let step = &body[read_step..act];
        assert!(
            step.contains("/tickvault-guard/prod/budget-stop-month")
                && step.contains("date -u +%Y-%m")
                && step.contains("echo \"TF_VAR_daily_start_enabled=false\" >> \"$GITHUB_ENV\""),
            "the {job} job's latch step must compare the latch with the UTC \
             month and set the rule DISABLED when it matches"
        );
        assert!(
            !step.contains("${LATCH}')"),
            "the {job} job must not echo the raw latch value into the log"
        );
    }
}

#[test]
fn reader_deploy_workflow_checks_the_latch_before_starting_a_stopped_box() {
    let wf = code_only(&read(".github/workflows/deploy-aws.yml"));
    let latch = position(
        &wf,
        "--name \"/tickvault-guard/prod/budget-stop-month\"",
        "deploy latch read",
    );
    let skip = position(
        &wf,
        "echo \"DEPLOY_SKIP_REASON=budget_stop\" >> \"$GITHUB_ENV\"",
        "deploy latch skip",
    );
    let start = position(
        &wf,
        "aws ec2 start-instances --region \"$REGION\" --instance-ids \"$IID\"",
        "deploy self-start",
    );
    assert!(
        latch < skip && skip < start,
        "deploy-aws.yml must read and honour the latch BEFORE it starts a \
         stopped box"
    );
}

#[test]
fn release_the_guard_re_enables_the_rule_only_while_stopped_and_can() {
    let rs = code_only(&read("crates/aws-lambdas/src/hard_stop_guard.rs"));
    let run_start = position(&rs, "pub async fn run_guard", "run_guard");
    let run = &rs[run_start..];
    let stopped = position(run, "\"stopped\" | \"stopping\"", "stopped branch");
    let release = position(
        run,
        "release_expired_budget_stop(env, sns, events, ssm, now_utc)",
        "new-month release",
    );
    let window = position(run, "if in_up_window(now_utc)", "window branch");
    assert!(
        stopped < release && release < window,
        "the new-month release must run inside the already-stopped branch"
    );
    let fn_start = position(
        &rs,
        "pub async fn release_expired_budget_stop",
        "release fn",
    );
    let body = &rs[fn_start..];
    let check = position(body, "latch_names_an_earlier_month(", "release check");
    let enable = position(
        body,
        "events.enable_rule(&env.start_rule_name)",
        "release enable",
    );
    let mark = position(body, "format!(\"released-{old_month}\")", "release marker");
    assert!(
        check < enable && enable < mark,
        "the release must check the month, then enable the rule, then mark the \
         latch released (so it happens once)"
    );

    let tf = code_only(&read("deploy/aws/terraform/budget-guards.tf"));
    let grant = position(&tf, "Sid = \"ReleaseDailyStartRuleNextMonth\"", "guard IAM");
    let grant_block = &tf[grant..grant + 300];
    assert!(
        grant_block.contains("\"events:EnableRule\"")
            && grant_block.contains("aws_cloudwatch_event_rule.daily_start.arn"),
        "the guard role must be able to EnableRule on the one daily-start rule"
    );
    let latch = position(&tf, "Sid = \"WriteBudgetStopLatch\"", "guard IAM");
    assert!(
        tf[latch..latch + 400].contains("\"ssm:GetParameter\""),
        "the guard role must be able to read the latch for the release"
    );
}

#[test]
fn no_latch_leg_names_the_instance_writable_prefix() {
    // The box's own role can PutParameter anywhere under /tickvault/<env>/*.
    // A latch there could be forged by anything holding the box's
    // credentials, so every leg must use the /tickvault-guard/ path.
    for rel in [
        "crates/aws-lambdas/src/budget_stop_latch.rs",
        "deploy/aws/terraform/budget-guards.tf",
        "deploy/aws/terraform/budget-killswitch-lambda.tf",
        "deploy/aws/terraform/start-watchdog-lambda.tf",
        "deploy/aws/terraform/oidc.tf",
        ".github/workflows/terraform-apply.yml",
        ".github/workflows/deploy-aws.yml",
        "scripts/aws-autopilot.sh",
    ] {
        let body = read(rel);
        for bad in [
            "/tickvault/prod/budget-stop-month",
            "/tickvault/${var.environment}/budget-stop-month",
            "/tickvault/${ENVIRONMENT}/budget-stop-month",
        ] {
            assert!(!body.contains(bad), "{rel} names the latch as `{bad}`");
        }
    }
}

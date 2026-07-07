//! Never-again ratchet for the 2026-07-06 17:19 IST log-shipper incident:
//! the app moved its machine logs to `data/logs/machine/` (the "one human
//! log file" reorg, observability.rs `MACHINE_LOGS_DIR`) while the on-box
//! CloudWatch agent kept tailing the OLD top-level globs — the app stayed
//! healthy, the shipper went silently dead, and `/tickvault/prod/app`
//! ingested NOTHING with zero operator signal.
//!
//! This guard fails the build if:
//!
//!  1. The reference agent config (`deploy/aws/cloudwatch-agent.json`)
//!     collect_list globs drift from the Rust observability constants
//!     (`MACHINE_LOGS_DIR` / `APP_LOG_PREFIX` / `ERRORS_JSONL_PREFIX`) —
//!     the exact drift class of the incident.
//!  2. The DEPLOYED inline config (`user-data.sh.tftpl`) drifts from the
//!     reference config's app-log globs.
//!  3. Any collect_list glob regresses to the dead top-level
//!     `data/logs/` dir (without `machine/`).
//!  4. The stream names (`{instance_id}/errors-jsonl` + `{instance_id}/app`)
//!     change — dashboards + the deploy smoke check key on them.
//!  5. The `app_log_ingestion_silent` alarm (log-retention.tf) loses its
//!     detection model (`IncomingLogEvents` + `treat_missing_data =
//!     "breaching"` + `actions_enabled = false`) or falls out of the
//!     market-hours gate Lambda's ALARM_NAMES join.
//!  6. The deploy workflow loses the LOG-INGESTION-SMOKE step (or oidc.tf
//!     loses the `logs:FilterLogEvents` grant it needs).
//!  7. The market-hours gate Lambda loses its weekday-NSE-holiday safety
//!     (2026-07-07 round-1 review fix): holiday-gate.sh SELF-STOPS the box
//!     on weekday NSE holidays while the gate's open cron is holiday-blind
//!     MON-FRI — a blind 09:20 enable + OK reset would false-page both
//!     breaching-on-missing gated alarms (~09:25 / ~09:35 IST) every
//!     weekday holiday. The open path must verify the instance is up
//!     (ec2:DescribeInstances, fail-open) before enabling.

#![cfg(test)]

use std::path::PathBuf;

use tickvault_app::observability::{APP_LOG_PREFIX, ERRORS_JSONL_PREFIX, MACHINE_LOGS_DIR};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/app parent")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display())) // APPROVED: test
}

/// The two app-log globs, DERIVED from the Rust observability constants so
/// a rename of `MACHINE_LOGS_DIR` / either prefix breaks this guard instead
/// of silently killing the shipper. The `.2*` suffix pins the hourly
/// `errors.jsonl.YYYY-MM-DD-HH` / `app.YYYY-MM-DD-HH` files while EXCLUDING
/// the `errors.jsonl` compat symlink and the 0-byte `app.log` placeholder.
fn expected_errors_glob() -> String {
    format!("/opt/tickvault/{MACHINE_LOGS_DIR}/{ERRORS_JSONL_PREFIX}.2*")
}

fn expected_app_glob() -> String {
    format!("/opt/tickvault/{MACHINE_LOGS_DIR}/{APP_LOG_PREFIX}.2*")
}

/// Every collect_list entry of an agent config body, as
/// `(file_path, log_stream_name)` pairs, pulled from the parsed JSON in
/// the reference config. For the tftpl (not valid JSON because of the
/// `$${ENVIRONMENT}` escapes) use [`tftpl_file_paths`] instead.
fn reference_collect_list() -> Vec<(String, String)> {
    let body = read("deploy/aws/cloudwatch-agent.json");
    let json: serde_json::Value =
        serde_json::from_str(&body).expect("cloudwatch-agent.json must parse as JSON"); // APPROVED: test
    let list = json["logs"]["logs_collected"]["files"]["collect_list"]
        .as_array()
        .expect("logs.logs_collected.files.collect_list must be an array"); // APPROVED: test
    list.iter()
        .map(|entry| {
            (
                entry["file_path"]
                    .as_str()
                    .expect("collect_list entry file_path") // APPROVED: test
                    .to_string(),
                entry["log_stream_name"]
                    .as_str()
                    .expect("collect_list entry log_stream_name") // APPROVED: test
                    .to_string(),
            )
        })
        .collect()
}

/// Line-scan the `"file_path":` values out of the user-data template's
/// inline agent config (the file `amazon-cloudwatch-agent-ctl` actually
/// loads on the box). Only `"file_path"` lines are considered, so comments
/// / mkdir lines mentioning `data/logs` can never false-fail the guard.
fn tftpl_file_paths() -> Vec<String> {
    let body = read("deploy/aws/terraform/user-data.sh.tftpl");
    body.lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("\"file_path\":")?;
            let start = rest.find('"')? + 1;
            let end = rest[start..].find('"')? + start;
            Some(rest[start..end].to_string())
        })
        .collect()
}

/// Collapse whitespace runs so terraform-body pins survive `terraform fmt`
/// realignment (`treat_missing_data  = "..."` vs `treat_missing_data = "..."`).
fn normalized(body: &str) -> String {
    body.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Extract one `resource "<type>" "<name>" { ... }` block from a terraform
/// body (everything from the resource header to the next `resource ` header
/// or EOF — coarse but sufficient for containment pins).
fn terraform_resource_block<'a>(body: &'a str, header: &str) -> &'a str {
    let start = body
        .find(header)
        .unwrap_or_else(|| panic!("terraform block `{header}` not found")); // APPROVED: test
    let rest = &body[start..];
    match rest[header.len()..].find("\nresource ") {
        Some(end) => &rest[..header.len() + end],
        None => rest,
    }
}

// ---------------------------------------------------------------------------
// 1. Reference config globs == observability constants
// ---------------------------------------------------------------------------

#[test]
fn test_reference_agent_config_globs_match_observability_constants() {
    let entries = reference_collect_list();
    let mut paths: Vec<String> = entries.iter().map(|(p, _)| p.clone()).collect();
    paths.sort();
    let mut expected = vec![expected_errors_glob(), expected_app_glob()];
    expected.sort();
    assert_eq!(
        paths, expected,
        "NEVER-AGAIN ratchet (2026-07-06 shipper incident): the collect_list \
         file_path globs in deploy/aws/cloudwatch-agent.json must be EXACTLY the \
         two constants-derived machine-dir globs. If observability.rs moved the \
         machine log dir or renamed a prefix, update the agent config in the SAME \
         PR — a drift here means the app logs healthy while CloudWatch ingests \
         nothing."
    );
}

// ---------------------------------------------------------------------------
// 2. Deployed inline config (user-data.sh.tftpl) == reference config
// ---------------------------------------------------------------------------

#[test]
fn test_userdata_inline_config_globs_match_reference() {
    // The tftpl also ships /var/log/messages to the system group — that
    // entry is allowlisted; the remaining app-log globs must byte-match the
    // reference config (which test 1 pins to the observability constants).
    let mut app_paths: Vec<String> = tftpl_file_paths()
        .into_iter()
        .filter(|p| p != "/var/log/messages")
        .collect();
    app_paths.sort();
    let mut expected = vec![expected_errors_glob(), expected_app_glob()];
    expected.sort();
    assert_eq!(
        app_paths, expected,
        "Z+ L3 RECONCILE drift-guard: user-data.sh.tftpl is the config the box \
         ACTUALLY loads (`amazon-cloudwatch-agent-ctl -a fetch-config`); its \
         app-log collect_list globs drifted from deploy/aws/cloudwatch-agent.json \
         / the observability constants. The 2026-07-06 incident was exactly this \
         file tailing globs the app no longer writes."
    );
}

// ---------------------------------------------------------------------------
// 3. No glob may regress to the dead top-level data/logs/ dir
// ---------------------------------------------------------------------------

#[test]
fn test_no_collect_list_glob_points_at_top_level_logs_dir() {
    let reference_paths: Vec<String> = reference_collect_list()
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    for (source, paths) in [
        ("deploy/aws/cloudwatch-agent.json", reference_paths),
        (
            "deploy/aws/terraform/user-data.sh.tftpl",
            tftpl_file_paths(),
        ),
    ] {
        for path in paths {
            if !path.contains("data/logs") {
                continue; // /var/log/messages etc.
            }
            assert!(
                path.contains("data/logs/machine/"),
                "NEVER-AGAIN ratchet: {source} collect_list glob `{path}` points at \
                 the TOP-LEVEL data/logs/ dir. That level is launcher-owned (human \
                 log + symlink only, frozen since the 2026-07-05 machine/ reorg) — \
                 the app's machine sinks live under data/logs/machine/ \
                 (observability.rs MACHINE_LOGS_DIR). A top-level glob is the exact \
                 dead-shipper regression of 2026-07-06."
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 4. Stream names are stable (dashboards + LOG-INGESTION-SMOKE key on them)
// ---------------------------------------------------------------------------

#[test]
fn test_stream_names_are_stable() {
    // Reference config: pin the glob → stream-name pairing.
    for (path, stream) in reference_collect_list() {
        let expected_stream = if path == expected_errors_glob() {
            "{instance_id}/errors-jsonl"
        } else if path == expected_app_glob() {
            "{instance_id}/app"
        } else {
            panic!("unexpected collect_list glob `{path}` — test 1 should have caught this") // APPROVED: test
        };
        assert_eq!(
            stream, expected_stream,
            "stream-name drift for glob `{path}` in cloudwatch-agent.json — the \
             CloudWatch dashboards and the deploy LOG-INGESTION-SMOKE poll key on \
             the historical stream names"
        );
    }
    // Deployed config: both stream names must survive verbatim.
    let user_data = read("deploy/aws/terraform/user-data.sh.tftpl");
    for stream in ["{instance_id}/errors-jsonl", "{instance_id}/app"] {
        assert!(
            user_data.contains(&format!("\"log_stream_name\": \"{stream}\"")),
            "user-data.sh.tftpl lost the stable log stream name `{stream}`"
        );
    }
}

// ---------------------------------------------------------------------------
// 5. The ingestion-silence alarm exists, keeps its detection model, and is
//    gated by the market-hours Lambda (log-retention.tf + gate join pins)
// ---------------------------------------------------------------------------

#[test]
fn test_app_log_ingestion_silent_alarm_pinned_in_terraform() {
    let tf = read("deploy/aws/terraform/log-retention.tf");
    let block = terraform_resource_block(
        &tf,
        "resource \"aws_cloudwatch_metric_alarm\" \"app_log_ingestion_silent\"",
    );
    let norm = normalized(block);
    for (pin, why) in [
        (
            "metric_name = \"IncomingLogEvents\"",
            "the alarm must watch INGESTION COUNT (the literal predicate: zero \
             events shipped), not a value-based app metric",
        ),
        (
            "treat_missing_data = \"breaching\"",
            "AWS/Logs publishes NO IncomingLogEvents datapoint for a zero-ingestion \
             period — silence IS missing data; anything but `breaching` blinds the \
             alarm to the exact 2026-07-06 failure",
        ),
        (
            "actions_enabled = false",
            "actions must start OFF — the market-hours gate Lambda flips them ON \
             09:20-15:35 IST so the intentional nightly/weekend box stop can never \
             false-page",
        ),
        (
            "comparison_operator = \"LessThanThreshold\"",
            "silence detection = Sum(IncomingLogEvents) < 1",
        ),
        (
            "LogGroupName = aws_cloudwatch_log_group.tv_app.name",
            "the alarm must be dimensioned on the APP log group, not the \
             account-wide aggregate",
        ),
    ] {
        assert!(
            norm.contains(pin),
            "log-retention.tf app_log_ingestion_silent alarm lost `{pin}` — {why}.\n\
             Block was:\n{block}"
        );
    }
}

#[test]
fn test_alarm_is_gated_by_market_hours_lambda() {
    let tf = read("deploy/aws/terraform/market-hours-liveness-alarm.tf");
    let join_start = tf
        .find("ALARM_NAMES = join(")
        .expect("market-hours-liveness-alarm.tf must carry the ALARM_NAMES join"); // APPROVED: test
    let rest = &tf[join_start..];
    let join_end = rest
        .find("])")
        .expect("ALARM_NAMES join must close with `])`"); // APPROVED: test
    let join_body = &rest[..join_end];
    assert!(
        join_body.contains("aws_cloudwatch_metric_alarm.app_log_ingestion_silent.alarm_name"),
        "gate-membership rot: app_log_ingestion_silent fell out of the market-hours \
         gate Lambda's ALARM_NAMES join. With treat_missing_data=breaching and \
         actions permanently disabled (never re-enabled at 09:20 IST), the alarm \
         can NEVER page — it exists but is dead. Join body was:\n{join_body}"
    );
}

/// Round-1 review fix (2026-07-07): the gate Lambda's OPEN mode must be
/// weekday-NSE-holiday safe. `deploy/aws/holiday-gate.sh` self-stops the box
/// at boot on a definitive holiday verdict, while the gate's open cron is a
/// holiday-blind plain MON-FRI schedule — so a blind enable + OK reset at
/// 09:20 IST would drive both breaching-on-missing gated alarms
/// (`market_hours_liveness_missing` ~09:25, `app_log_ingestion_silent`
/// ~09:35) into a false SNS/Telegram page on EVERY weekday NSE holiday.
/// The open path must first verify the tv-app instance is up, and must
/// fail OPEN on an EC2 API error so a real trading day never loses the page.
#[test]
fn test_gate_lambda_open_is_holiday_safe() {
    let tf = read("deploy/aws/terraform/market-hours-liveness-alarm.tf");
    let norm = normalized(&tf);
    for (pin, why) in [
        (
            "EC2_INSTANCE_ID = aws_instance.tv_app.id",
            "the gate Lambda must know WHICH instance to check — the cycle-free \
             env-var pattern proven by start-watchdog-lambda.tf",
        ),
        (
            "ec2.describe_instances(InstanceIds=[INSTANCE_ID])",
            "the open path must ask EC2 whether the box is up before enabling \
             the breaching-on-missing alarms (holiday self-stop = box OFF)",
        ),
        (
            "fail-open, treating as up",
            "an EC2 API error must NEVER suppress the trading-day liveness page \
             — the check fails open, exactly mirroring holiday-gate.sh",
        ),
        (
            "leaving actions disabled",
            "a not-up box (NSE holiday self-stop / operator manual stop) must \
             skip BOTH the enable and the OK reset",
        ),
        (
            "\"ec2:DescribeInstances\"",
            "the gate role must carry the DescribeInstances grant or the check \
             throws AccessDenied every open (and fail-open would blindly enable, \
             silently restoring the holiday false-page)",
        ),
    ] {
        assert!(
            norm.contains(pin),
            "market-hours-liveness-alarm.tf lost the holiday-safety pin `{pin}` — {why}. \
             Regressing this restores the weekday-NSE-holiday false page \
             (box self-stopped by holiday-gate.sh + holiday-blind MON-FRI open cron \
             + treat_missing_data=breaching)."
        );
    }
    // The enable call must come AFTER the instance-up guard in the Lambda
    // source (source-order scan, house pattern).
    let guard_pos = tf
        .find("if not up:")
        .expect("gate Lambda must carry the `if not up:` holiday guard arm"); // APPROVED: test
    let enable_pos = tf
        .find("cw.enable_alarm_actions(AlarmNames=ALARM_NAMES)")
        .expect("gate Lambda must still enable alarm actions on open"); // APPROVED: test
    assert!(
        guard_pos < enable_pos,
        "the instance-up guard must run BEFORE enable_alarm_actions — enabling \
         first re-opens the holiday false-page window"
    );
}

// ---------------------------------------------------------------------------
// 6. Deploy-time smoke check + its IAM grant survive
// ---------------------------------------------------------------------------

#[test]
fn test_deploy_workflow_carries_log_ingestion_smoke() {
    let workflow = read(".github/workflows/deploy-aws.yml");
    assert!(
        workflow.contains("LOG-INGESTION-SMOKE"),
        "deploy-aws.yml lost the LOG-INGESTION-SMOKE step — the per-deploy \
         independent detector for a dead log shipper (the gated alarm alone can \
         be silenced by gate-Lambda EventBridge drift, the #1404 class)"
    );
    assert!(
        workflow.contains("filter-log-events"),
        "deploy-aws.yml LOG-INGESTION-SMOKE must poll `aws logs filter-log-events` \
         for fresh app events after readiness"
    );
    let oidc = read("deploy/aws/terraform/oidc.tf");
    assert!(
        oidc.contains("logs:FilterLogEvents"),
        "oidc.tf lost the `logs:FilterLogEvents` grant — the deploy role can no \
         longer run the LOG-INGESTION-SMOKE poll (it would warn on every deploy)"
    );
}

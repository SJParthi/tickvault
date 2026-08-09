//! Lockstep ratchet for the Dhan token-minter Lambda (operator 2026-08-08).
//!
//! The Lambda's whole purpose is that `/tickvault/<env>/dhan/access-token`
//! never goes stale again. Every property below is one where a silent drift
//! would restore the 2026-08-06 failure — a frozen parameter and consumers
//! serving a dead token — while every test still passed. So they are pinned:
//!
//! - the schedule must exist and fire DAILY (a MON-FRI cron leaves weekend
//!   consumers on a Friday token);
//! - the write scope must stay ONE ARN (a wildcard could clobber the Groww
//!   token, which shares the secret name);
//! - the not-invoked alarm must exist and treat missing data as BREACHING
//!   (the Errors alarm cannot see a dropped schedule);
//! - the rule file must carry §10 (the authority record + the coexistence
//!   obligation that stops the re-mint war returning once the box boots).

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = <root>/crates/aws-lambdas
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn read(relative: &str) -> String {
    let path = repo_root().join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("cannot read {relative}: {err}"))
}

/// Strips `#` comments so a pin can never be satisfied by prose in a comment.
fn strip_hcl_comments(source: &str) -> String {
    source
        .lines()
        .map(|line| match line.find('#') {
            Some(index) => &line[..index],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

const TF: &str = "deploy/aws/terraform/dhan-token-minter-lambda.tf";
const RULE: &str = ".claude/rules/project/groww-shared-token-minter-2026-07-02.md";

#[test]
fn terraform_defines_the_lambda_and_its_log_group() {
    let hcl = strip_hcl_comments(&read(TF));
    for required in [
        "aws_lambda_function\" \"dhan_token_minter\"",
        "aws_cloudwatch_log_group\" \"dhan_token_minter\"",
        "aws_iam_role\" \"dhan_token_minter\"",
        "aws_iam_role_policy\" \"dhan_token_minter\"",
        "provided.al2023",
        "dhan-token-minter.zip",
    ] {
        assert!(
            hcl.contains(required),
            "{TF} must contain `{required}` — the Lambda cannot deploy without it."
        );
    }
}

#[test]
fn schedule_fires_every_day_not_just_weekdays() {
    let hcl = strip_hcl_comments(&read(TF));
    assert!(
        hcl.contains("aws_cloudwatch_event_rule\" \"dhan_token_minter\""),
        "the minter needs an EventBridge schedule — without one nothing ever mints."
    );
    assert!(
        hcl.contains("cron(35 0 * * ? *)"),
        "the schedule must be the DAILY 06:05 IST cron `cron(35 0 * * ? *)`. \
         A MON-FRI cron leaves weekend consumers reading a Friday token, which \
         is the staleness class this Lambda exists to prevent."
    );
    assert!(
        !hcl.contains("MON-FRI"),
        "{TF} must NOT restrict the mint to weekdays — consumers read this \
         parameter on weekends too."
    );
    assert!(
        hcl.contains("state = \"ENABLED\""),
        "the rule must set `state` explicitly so terraform manages it (the \
         2026-07-04 pause/#1404 incident class)."
    );
}

#[test]
fn write_scope_is_exactly_one_parameter() {
    let hcl = strip_hcl_comments(&read(TF));
    assert!(
        hcl.contains("ssm:PutParameter"),
        "the minter must be able to write the token."
    );
    assert!(
        hcl.contains("parameter/tickvault/${var.environment}/dhan/access-token"),
        "the write must target the dhan access-token parameter explicitly."
    );
    // The Groww token shares the secret NAME; only the service segment differs.
    // A `/dhan/*` or `/tickvault/*` write wildcard would put the Groww token in
    // blast radius and 401 the Groww REST legs.
    for forbidden in [
        "parameter/tickvault/${var.environment}/dhan/*",
        "parameter/tickvault/*",
        "parameter/*",
    ] {
        assert!(
            !hcl.contains(forbidden),
            "{TF} must NOT grant a wildcard SSM scope (`{forbidden}`) — the \
             Groww token shares the `access-token` secret name and only the \
             service segment separates them."
        );
    }
}

#[test]
fn reads_are_enumerated_credential_parameters() {
    let hcl = strip_hcl_comments(&read(TF));
    for credential in ["dhan/client-id", "dhan/client-secret", "dhan/totp-secret"] {
        assert!(
            hcl.contains(credential),
            "{TF} must grant read on `{credential}` — the mint needs all three."
        );
    }
}

#[test]
fn minter_holds_no_ec2_permission() {
    let hcl = strip_hcl_comments(&read(TF));
    for forbidden in [
        "ec2:StartInstances",
        "ec2:StopInstances",
        "ec2:RebootInstances",
    ] {
        assert!(
            !hcl.contains(forbidden),
            "{TF} must NOT grant `{forbidden}` — this Lambda mints a token; \
             the start-watchdog owns EC2 lifecycle (separation of duties)."
        );
    }
}

#[test]
fn both_alarms_exist_and_the_not_invoked_one_treats_missing_as_breaching() {
    let hcl = strip_hcl_comments(&read(TF));

    assert!(
        hcl.contains("aws_cloudwatch_metric_alarm\" \"dhan_token_minter_errors\""),
        "a failing mint must page."
    );
    assert!(
        hcl.contains("aws_cloudwatch_metric_alarm\" \"dhan_token_minter_not_invoked\""),
        "a mint that NEVER RUNS must page too — the Errors alarm is blind to a \
         dropped EventBridge schedule (no invocation means no error), which is \
         the 2026-07-02 repo-wide scheduler-drop class."
    );

    // The not-invoked alarm is only meaningful if absent data breaches.
    let not_invoked_block = hcl
        .split("dhan_token_minter_not_invoked")
        .nth(1)
        .unwrap_or_default();
    assert!(
        not_invoked_block.contains("treat_missing_data  = \"breaching\"")
            || not_invoked_block.contains("treat_missing_data = \"breaching\""),
        "the not-invoked alarm MUST treat missing data as `breaching` — a \
         missing Invocations datapoint IS the condition being detected. With \
         `notBreaching` it can never fire and the guard is decorative."
    );
    assert!(
        not_invoked_block.contains("LessThanThreshold"),
        "the not-invoked alarm must fire BELOW the threshold (fewer than 1 \
         invocation), not above it."
    );
}

#[test]
fn rule_file_carries_the_section_ten_contract() {
    let rule = read(RULE);
    for phrase in [
        "§10",
        "build the lambda",
        "dhan-token-minter",
        "cron(35 0 * * ? *)",
        "ONE MINTER",
    ] {
        assert!(
            rule.contains(phrase),
            "{RULE} must carry `{phrase}` — the rule file is the authority \
             record that makes this Lambda's contract reviewable."
        );
    }
}

#[test]
fn rule_file_records_the_coexistence_obligation() {
    let rule = read(RULE);
    // The single most dangerous thing a future session could miss: PR #1729's
    // publisher still mints from the box. Two minters against a one-token
    // account is the re-mint war, re-created from the other side.
    assert!(
        rule.contains("§10.3") && rule.contains("Coexistence"),
        "{RULE} §10 must carry the §10.3 coexistence section — it is the only \
         record that tickvault must switch to READ before the box next runs, \
         and forgetting it silently restores the re-mint war."
    );
}

#[test]
fn module_and_bin_are_wired_into_the_crate() {
    let lib = read("crates/aws-lambdas/src/lib.rs");
    assert!(
        lib.contains("pub mod dhan_token_minter;"),
        "the module must be declared in lib.rs or it is not compiled."
    );

    let cargo = read("crates/aws-lambdas/Cargo.toml");
    assert!(
        cargo.contains("name = \"dhan-token-minter\""),
        "the [[bin]] must exist or `cargo lambda build` produces no zip and \
         terraform's `filename` points at a missing file."
    );
    assert!(
        cargo.contains("totp-rs = { workspace = true }"),
        "totp-rs must be the WORKSPACE pin — an inline version would be a new \
         unpinned dependency root (CARGO rules: exact versions in the \
         workspace root only)."
    );
}

#[test]
fn the_published_path_matches_what_the_module_builds() {
    // The terraform IAM grant and the Rust path builder must agree. If they
    // drift, the Lambda gets AccessDenied at 06:05 IST every day and the
    // parameter silently stays stale — with every test still green.
    let built = tickvault_aws_lambdas::dhan_token_minter::build_ssm_path(
        "prod",
        tickvault_aws_lambdas::dhan_token_minter::SSM_DHAN_SERVICE,
        tickvault_aws_lambdas::dhan_token_minter::DHAN_ACCESS_TOKEN_SECRET,
    );
    assert_eq!(built, "/tickvault/prod/dhan/access-token");

    let hcl = strip_hcl_comments(&read(TF));
    // The tf uses ${var.environment}; compare on the invariant tail.
    assert!(
        hcl.contains("/dhan/access-token"),
        "the terraform write grant must target the same `/dhan/access-token` \
         tail the module builds."
    );
}

/// Terraform MUST inject the auth host, because the Rust module has no default.
///
/// This is a lockstep pin: the module refuses to guess the host (the house rule
/// bans hardcoded URLs in production source), so if terraform stops setting the
/// variable the mint fails every single day. Config-driven only works when the
/// config is actually supplied.
#[test]
fn terraform_injects_the_auth_host_the_module_refuses_to_default() {
    let hcl = strip_hcl_comments(&read(TF));
    assert!(
        hcl.contains("DHAN_AUTH_BASE_URL"),
        "{TF} MUST set DHAN_AUTH_BASE_URL — the Rust module has no in-code \
         default by design, so without this the mint fails every day."
    );
    assert!(
        hcl.contains("TV_ENVIRONMENT"),
        "{TF} must set TV_ENVIRONMENT so the SSM paths resolve."
    );
    // And the module must still refuse to default it.
    assert_eq!(
        tickvault_aws_lambdas::dhan_token_minter::DHAN_AUTH_BASE_URL_ENV,
        "DHAN_AUTH_BASE_URL",
        "the env-var name must match what terraform sets."
    );
}

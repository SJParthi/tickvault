//! Lockstep ratchet for the DEPTH-account Dhan token minter
//! (`groww-shared-token-minter-2026-07-02.md` §10.9, operator 2026-09-26).
//!
//! One minter per ACCOUNT. The second, depth-only Dhan account gets exactly one
//! minter of its own, built from the same code as the primary one. Every pin
//! below is a property whose silent drift would either let one minter reach the
//! other account (a re-mint war that kills a live token) or page the operator
//! every morning for an account that does not exist yet:
//!
//! - the depth function runs the SAME zip as the primary one, configured only by
//!   `SSM_SERVICE = "dhan-depth"` (no second implementation);
//! - its IAM grant reads the three enumerated `/dhan-depth/` credentials and
//!   writes `/dhan-depth/access-token` only, and never names `/dhan/`;
//! - the primary minter's file never names `/dhan-depth/`;
//! - the schedule and the not-invoked alarm are both gated on
//!   `var.dhan_depth_account_enabled`, which defaults to `false`.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
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

/// Collapses whitespace so alignment changes from `terraform fmt` never break a pin.
fn compact(source: &str) -> String {
    source.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The body of one `resource "<kind>" "<name>" { ... }` block, brace-matched.
fn resource_block(hcl: &str, kind: &str, name: &str) -> String {
    let needle = format!("resource \"{kind}\" \"{name}\"");
    let start = hcl
        .find(&needle)
        .unwrap_or_else(|| panic!("resource `{kind}.{name}` not found"));
    let rest = &hcl[start..];
    let open = rest
        .find('{')
        .unwrap_or_else(|| panic!("`{name}` has no body"));
    let mut depth = 0usize;
    for (index, ch) in rest[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return rest[..open + index + 1].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("`{name}` block is not closed")
}

const DEPTH_TF: &str = "deploy/aws/terraform/dhan-depth-token-minter-lambda.tf";
const PRIMARY_TF: &str = "deploy/aws/terraform/dhan-token-minter-lambda.tf";
const VARIABLES_TF: &str = "deploy/aws/terraform/variables.tf";
const RULE: &str = "docs/claude-rules-full/project/groww-shared-token-minter-2026-07-02.md";
const SWITCH: &str = "var.dhan_depth_account_enabled";

fn depth_hcl() -> String {
    strip_hcl_comments(&read(DEPTH_TF))
}

#[test]
fn the_depth_minter_runs_the_same_binary_configured_for_its_own_account() {
    let hcl = depth_hcl();
    let function = compact(&resource_block(
        &hcl,
        "aws_lambda_function",
        "dhan_depth_token_minter",
    ));
    assert!(
        function.contains("dhan-token-minter.zip"),
        "the depth minter must run the SAME zip as the primary one — §10.9 forbids a \
         second implementation of the mint."
    );
    assert!(
        function.contains("SSM_SERVICE = \"dhan-depth\""),
        "the depth minter must set SSM_SERVICE = \"dhan-depth\"; without it the code \
         defaults to the PRIMARY account and the two minters fight over one token."
    );
    assert!(
        function.contains("DHAN_AUTH_BASE_URL") && function.contains("TV_ENVIRONMENT"),
        "the depth minter needs the same injected environment as the primary one."
    );
    assert!(
        function.contains("timeout = 120"),
        "§10.8: the timeout must cover the 82 s worst case, same as the primary minter."
    );
}

#[test]
fn the_depth_minter_value_is_one_the_code_accepts() {
    // The terraform value and the Rust allowlist must agree, or the depth mint
    // fails every morning with a configuration error.
    assert_eq!(
        tickvault_aws_lambdas::dhan_token_minter::parse_ssm_service(Some("dhan-depth")),
        Ok(tickvault_aws_lambdas::dhan_token_minter::SSM_DHAN_DEPTH_SERVICE)
    );
    let primary = compact(&strip_hcl_comments(&read(PRIMARY_TF)));
    assert!(
        primary.contains("SSM_SERVICE = \"dhan\""),
        "the primary minter must name its account explicitly."
    );
}

#[test]
fn the_depth_grant_reaches_only_the_depth_account() {
    let hcl = depth_hcl();
    let policy = resource_block(&hcl, "aws_iam_role_policy", "dhan_depth_token_minter");
    for credential in [
        "dhan-depth/client-id",
        "dhan-depth/client-secret",
        "dhan-depth/totp-secret",
    ] {
        assert!(
            policy.contains(credential),
            "missing read on `{credential}`"
        );
    }
    assert!(
        policy.contains("parameter/tickvault/${var.environment}/dhan-depth/access-token"),
        "the write must target the depth account's token parameter explicitly."
    );
    assert!(
        !hcl.contains("/dhan/"),
        "{DEPTH_TF} must never name a `/dhan/` path — that is the PRIMARY account."
    );
    for forbidden in [
        "dhan-depth/*",
        "parameter/tickvault/*",
        "parameter/*",
        "ec2:StartInstances",
        "ec2:StopInstances",
        "ec2:RebootInstances",
    ] {
        assert!(
            !hcl.contains(forbidden),
            "{DEPTH_TF} must not grant `{forbidden}`."
        );
    }
}

#[test]
fn the_primary_minter_never_reaches_the_depth_account() {
    let primary = strip_hcl_comments(&read(PRIMARY_TF));
    assert!(
        !primary.contains("dhan-depth"),
        "{PRIMARY_TF} must never name the depth account — one minter per account."
    );
}

#[test]
fn the_schedule_is_daily_and_gated_on_the_switch() {
    let hcl = depth_hcl();
    let rule = compact(&resource_block(
        &hcl,
        "aws_cloudwatch_event_rule",
        "dhan_depth_token_minter",
    ));
    assert!(
        rule.contains("cron(35 0 * * ? *)"),
        "daily 06:05 IST, like the primary."
    );
    assert!(!rule.contains("MON-FRI"), "never weekdays only (§10.4).");
    assert!(
        rule.contains(&format!("state = {SWITCH} ? \"ENABLED\" : \"DISABLED\"")),
        "the schedule must be created DISABLED until the account exists (§10.9)."
    );
}

#[test]
fn both_alarms_exist_and_the_not_invoked_one_is_gated_and_breaching() {
    let hcl = depth_hcl();
    let errors = compact(&resource_block(
        &hcl,
        "aws_cloudwatch_metric_alarm",
        "dhan_depth_token_minter_errors",
    ));
    assert!(errors.contains("metric_name = \"Errors\""));
    assert!(errors.contains("ok_actions = []"));

    let not_invoked = compact(&resource_block(
        &hcl,
        "aws_cloudwatch_metric_alarm",
        "dhan_depth_token_minter_not_invoked",
    ));
    assert!(not_invoked.contains("metric_name = \"Invocations\""));
    assert!(not_invoked.contains("LessThanThreshold"));
    assert!(
        not_invoked.contains("treat_missing_data = \"breaching\""),
        "a missing Invocations datapoint IS the condition; notBreaching makes the \
         alarm decorative."
    );
    assert!(
        not_invoked.contains(&format!("count = {SWITCH} ? 1 : 0")),
        "the not-invoked alarm must be gated on the same switch as the schedule, or a \
         disabled schedule pages every morning (§10.9)."
    );
}

#[test]
fn the_switch_defaults_off() {
    let variables = compact(&strip_hcl_comments(&read(VARIABLES_TF)));
    let at = variables
        .find("variable \"dhan_depth_account_enabled\"")
        .expect("variables.tf must declare dhan_depth_account_enabled");
    let block = &variables[at..];
    // Bounded at the NEXT variable declaration, if any.
    let end = block[1..]
        .find("variable \"")
        .map_or(block.len(), |next| next + 1);
    let block = &block[..end];
    assert!(block.contains("type = bool"));
    assert!(
        block.contains("default = false"),
        "the depth account ships OFF (websocket-connection-scope-lock.md § 2026-09-26)."
    );
}

#[test]
fn the_rule_file_carries_section_ten_nine() {
    let rule = read(RULE);
    for phrase in [
        "§10.9",
        "ONE MINTER PER ACCOUNT",
        "tv-<env>-dhan-depth-token-minter",
        "/tickvault/<env>/dhan-depth/access-token",
    ] {
        assert!(rule.contains(phrase), "{RULE} must carry `{phrase}`.");
    }
}

#[test]
fn guard_self_test_the_block_finder_and_stripper_bite() {
    let src =
        "# resource \"a\" \"fake\" { x = 1 }\nresource \"a\" \"real\" {\n  y = { z = 1 }\n}\n";
    let stripped = strip_hcl_comments(src);
    assert!(!stripped.contains("fake"));
    assert_eq!(
        compact(&resource_block(&stripped, "a", "real")),
        "resource \"a\" \"real\" { y = { z = 1 } }"
    );
}

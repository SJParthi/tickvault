//! Audit M4 (2026-10-04): the operator portal's public exposure stays capped.
//!
//! The portal is a public URL behind one shared secret, and that secret can
//! stop the trading box and run a root shell on it. This guard pins the three
//! infrastructure caps added with the in-handler failed-key guard, so none of
//! them can be removed quietly:
//!
//! 1. no CORS configuration on the Function URL (it used to allow origin
//!    `*`; the page is same-origin, see the comment at the resource);
//! 2. a reserved-concurrency cap on the function;
//! 3. a throttle on the API Gateway stage (the only global rate bound).
//!
//! Honest limit: this reads the terraform text. It proves what the file
//! asks for, not what an apply produced.

use std::path::{Path, PathBuf};

const TF: &str = "deploy/aws/terraform/operator-control-lambda.tf";
/// The largest concurrency cap this guard accepts.
const MAX_RESERVED_CONCURRENCY: u32 = 5;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/aws-lambdas parent")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

/// The terraform with `#` comments removed, so prose cannot satisfy a check.
fn tf_code() -> String {
    read(&repo_root().join(TF))
        .lines()
        .map(|l| l.split_once('#').map_or(l, |(code, _)| code))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The body of `resource "<kind>" "<name>" { ... }` (brace-matched).
fn resource_body(code: &str, kind: &str, name: &str) -> String {
    let head = format!("resource \"{kind}\" \"{name}\"");
    let start = code
        .find(&head)
        .unwrap_or_else(|| panic!("{TF} has no {head}"));
    let open = start + code[start..].find('{').expect("resource opens a block");
    let mut depth = 0_i32;
    for (i, c) in code[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return code[open..=open + i].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("{head} block never closes");
}

fn number_after(body: &str, key: &str) -> u32 {
    let line = body
        .lines()
        .find(|l| l.trim_start().starts_with(key))
        .unwrap_or_else(|| panic!("`{key}` missing"));
    line.split('=')
        .nth(1)
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or_else(|| panic!("`{key}` is not a plain number: {line}"))
}

#[test]
fn function_url_has_no_cors_configuration() {
    let code = tf_code();
    let url = resource_body(&code, "aws_lambda_function_url", "operator_control");
    assert!(
        !url.contains("cors"),
        "the operator Function URL must not carry a cors block: the page is \
         same-origin, and a wildcard origin let any web page drive it"
    );
    assert!(
        !code.contains("allow_origins"),
        "no allow_origins anywhere in {TF}"
    );
    let api = resource_body(&code, "aws_apigatewayv2_api", "operator_control");
    assert!(
        !api.contains("cors_configuration"),
        "the portal API must stay same-origin (no cors_configuration)"
    );
}

#[test]
fn function_has_a_small_reserved_concurrency_cap() {
    let code = tf_code();
    let f = resource_body(&code, "aws_lambda_function", "operator_control");
    let cap = number_after(&f, "reserved_concurrent_executions");
    assert!(
        (1..=MAX_RESERVED_CONCURRENCY).contains(&cap),
        "reserved_concurrent_executions = {cap}; must be 1..={MAX_RESERVED_CONCURRENCY} \
         (0 disables the function, a large cap multiplies the per-container \
         failed-key allowance)"
    );
}

#[test]
fn api_stage_is_throttled() {
    let code = tf_code();
    let stage = resource_body(&code, "aws_apigatewayv2_stage", "operator_control");
    assert!(
        stage.contains("default_route_settings"),
        "stage throttle missing"
    );
    let burst = number_after(&stage, "throttling_burst_limit");
    let rate = number_after(&stage, "throttling_rate_limit");
    assert!((1..=20).contains(&burst), "burst {burst} outside 1..=20");
    assert!((1..=5).contains(&rate), "rate {rate}/s outside 1..=5");
}

#[test]
fn handler_keeps_the_constant_time_compare_and_the_guard() {
    let src = read(&repo_root().join("crates/aws-lambdas/src/operator_control.rs"));
    assert!(src.contains("verify_slices_are_equal(presented.as_bytes(), secret.as_bytes())"));
    // The guard runs before the secret is read, and failures are recorded.
    let route = src
        .split_once("pub async fn route<S: OpsShell>")
        .map(|(_, r)| r)
        .expect("route present");
    let check = route.find("guard.check(").expect("guard checked in route");
    let secret = route.find("shell.control_secret()").expect("secret read");
    assert!(
        check < secret,
        "the failed-key check must run before the secret is read"
    );
    assert!(route.contains("guard.record_failure("));
}

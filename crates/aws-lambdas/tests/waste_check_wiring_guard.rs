//! Wiring guard for the daily AWS waste check (noise lock §2.18, owner card
//! 2026-11-11 "Yes, daily waste check (Recommended)").
//!
//! §2.18 REJECT rows this file makes mechanical:
//! - the waste Lambda's role holds a delete, release, modify, stop (or any
//!   other mutating) action, a bare `*` action, a `NotAction`, a `*`
//!   resource on anything but the reads that have no resource-level
//!   permissions, or anything beyond the enumerated reads and the scoped
//!   writes (its own state object and day claims, its audit prefix, the
//!   alerts topic, its logs);
//! - a second policy (inline or attached) reaches its role from any
//!   terraform file;
//! - it runs more than once a day, or inside 09:00-15:40 IST;
//! - a failed run is retried;
//! - it has an alarm beyond its own did-it-run check, or none.
//!
//! The terraform is read at RUNTIME (no `include_str!`), the same way the
//! alarm phrase guard reads it, so a moved file fails loudly here.

use std::path::PathBuf;

fn terraform_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../deploy/aws/terraform")
}

fn terraform(file: &str) -> String {
    let path = terraform_dir().join(file);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {path:?}: {e}"))
}

fn code_only(src: &str) -> String {
    src.lines()
        .map(|line| match line.find('#') {
            Some(i) => &line[..i],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The text from `open` (an opening bracket) to its matching close.
fn balanced(src: &str, open: usize, left: char, right: char) -> &str {
    let mut depth = 0usize;
    for (i, ch) in src[open..].char_indices() {
        if ch == left {
            depth += 1;
        } else if ch == right {
            depth -= 1;
            if depth == 0 {
                return &src[open..open + i + ch.len_utf8()];
            }
        }
    }
    panic!("unbalanced {left}{right}");
}

fn block<'a>(src: &'a str, kind: &str, name: &str) -> &'a str {
    let needle = format!("resource \"{kind}\" \"{name}\"");
    let start = src
        .find(&needle)
        .unwrap_or_else(|| panic!("{kind}.{name} not found"));
    let rest = &src[start..];
    let open = rest.find('{').unwrap_or_else(|| panic!("{name}: no brace"));
    let body = balanced(rest, open, '{', '}');
    &rest[..open + body.len()]
}

/// Every `"service:Action"` string literal in a policy block.
fn actions(policy: &str) -> Vec<String> {
    let mut out = Vec::new();
    for piece in policy.split('"').skip(1).step_by(2) {
        let Some((service, action)) = piece.split_once(':') else {
            continue;
        };
        let service_ok = !service.is_empty()
            && service
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        let action_ok = action
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_uppercase() || c == '*')
            && action
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'*');
        if service_ok && action_ok {
            out.push(piece.to_string());
        }
    }
    out
}

/// The values of every `key = ...` attribute in `stmt`: a quoted string,
/// every quoted string of a `[...]` list, or the raw expression (for a
/// reference such as `aws_sns_topic.tv_alerts.arn`). `key` must start at a
/// word boundary, so `Action` never matches inside `NotAction`.
fn values_of(stmt: &str, key: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = stmt[from..].find(key) {
        let at = from + rel;
        from = at + key.len();
        let before_ok = stmt[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_' && c != '"');
        let after = stmt[from..].trim_start();
        if !before_ok || !after.starts_with('=') {
            continue;
        }
        let value = after[1..].trim_start();
        let offset = stmt.len() - value.len();
        if value.starts_with('[') {
            let list = balanced(stmt, offset, '[', ']');
            out.extend(list.split('"').skip(1).step_by(2).map(str::to_string));
        } else if let Some(rest) = value.strip_prefix('"') {
            out.push(rest.split('"').next().unwrap_or("").to_string());
        } else {
            out.push(
                value
                    .split(|c: char| c.is_whitespace() || c == ',' || c == '}')
                    .next()
                    .unwrap_or("")
                    .to_string(),
            );
        }
    }
    out
}

/// The `{...}` statements inside the policy's `Statement = [...]` list.
fn statements(policy: &str) -> Vec<&str> {
    let start = policy.find("Statement").expect("no Statement list");
    let open = start + policy[start..].find('[').expect("Statement is not a list");
    let list = balanced(policy, open, '[', ']');
    let mut out = Vec::new();
    let mut i = 1;
    while let Some(rel) = list[i..].find('{') {
        let s = balanced(list, i + rel, '{', '}');
        out.push(s);
        i += rel + s.len();
    }
    out
}

const ALLOWED: &[&str] = &[
    "ec2:DescribeVolumes",
    "ec2:DescribeSnapshots",
    "ec2:DescribeAddresses",
    "ce:GetCostAndUsage",
    "s3:ListBucket",
    "s3:GetObject",
    "s3:PutObject",
    "sns:Publish",
    // No logs:CreateLogGroup (2026-11-11 review): terraform creates the
    // group, and the role may write only its own group.
    "logs:CreateLogStream",
    "logs:PutLogEvents",
];

/// The one resource a `logs:` action may name: the waste check's own log
/// group (2026-11-11 review: `arn:aws:logs:<region>:*:*` let the role write
/// coded ERROR lines into the app log groups and trip their pages).
const OWN_LOG_GROUP: &str = "${aws_cloudwatch_log_group.aws_waste_check.arn}:*";

/// The reads that have no resource-level permissions and so may take `*`.
const STAR_RESOURCE_OK: &[&str] = &[
    "ec2:DescribeVolumes",
    "ec2:DescribeSnapshots",
    "ec2:DescribeAddresses",
    "ce:GetCostAndUsage",
];

/// Verbs that change or remove a resource. Any action starting with one of
/// these on any service fails the build, whatever the allowlist says.
const FORBIDDEN_VERBS: &[&str] = &[
    "Delete",
    "Release",
    "Modify",
    "Stop",
    "Terminate",
    "Detach",
    "Disassociate",
    "Deregister",
    "Reboot",
    "Update",
    "Remove",
];

fn forbidden(action: &str) -> bool {
    let verb = action.split_once(':').map(|(_, a)| a).unwrap_or(action);
    verb.contains('*') || FORBIDDEN_VERBS.iter().any(|v| verb.starts_with(v))
}

fn policy() -> String {
    let src = code_only(&terraform("waste-check-lambda.tf"));
    block(&src, "aws_iam_role_policy", "aws_waste_check").to_string()
}

/// Every reason the policy text breaks the "can only look" contract.
fn policy_violations(policy: &str) -> Vec<String> {
    let mut bad = Vec::new();
    for word in ["NotAction", "NotResource", "NotPrincipal"] {
        if policy.contains(word) {
            bad.push(format!("{word} is not allowed"));
        }
    }
    let stmts = statements(policy);
    if stmts.is_empty() {
        bad.push("no statements parsed".to_string());
    }
    let mut star_statements = 0usize;
    for stmt in &stmts {
        let acts = values_of(stmt, "Action");
        if acts.is_empty() {
            bad.push(format!("statement with no Action parsed: {stmt}"));
        }
        for effect in values_of(stmt, "Effect") {
            if effect != "Allow" {
                bad.push(format!("unexpected Effect {effect}"));
            }
        }
        for a in &acts {
            if forbidden(a) || a == "*" {
                bad.push(format!("mutating or wildcard action {a}"));
            }
            if !ALLOWED.contains(&a.as_str()) {
                bad.push(format!("action {a} is not on the enumerated list"));
            }
        }
        let res = values_of(stmt, "Resource");
        if res.iter().any(|r| r == "*") {
            star_statements += 1;
            for a in &acts {
                if !STAR_RESOURCE_OK.contains(&a.as_str()) {
                    bad.push(format!("{a} on Resource \"*\""));
                }
            }
        }
        if res.iter().any(|r| r.contains("tv_cold.arn}/*")) {
            bad.push("a grant on the whole cold bucket".to_string());
        }
        if acts.iter().any(|a| a.starts_with("logs:")) && res != [OWN_LOG_GROUP] {
            bad.push(format!(
                "logs actions must name only {OWN_LOG_GROUP}, got {res:?}"
            ));
        }
    }
    if star_statements != 2 {
        bad.push(format!(
            "{star_statements} statements use Resource \"*\"; only the Describe reads and the \
             Cost Explorer query may"
        ));
    }
    for a in actions(policy) {
        if !ALLOWED.contains(&a.as_str()) || forbidden(&a) {
            bad.push(format!("action string {a} anywhere in the policy"));
        }
    }
    bad
}

#[test]
fn the_waste_check_role_can_only_look() {
    let p = policy();
    let violations = policy_violations(&p);
    assert!(
        violations.is_empty(),
        "noise lock 2.18 REJECT: the waste check role may only look: {violations:?}"
    );
    let found = actions(&p);
    for a in ALLOWED {
        assert!(found.iter().any(|f| f == a), "{a} missing from the policy");
    }
}

#[test]
fn the_waste_check_writes_are_scoped_to_its_own_targets() {
    let p = policy();
    let stmts = statements(&p);
    let with = |action: &str| -> Vec<String> {
        stmts
            .iter()
            .filter(|s| values_of(s, "Action").iter().any(|a| a == action))
            .flat_map(|s| values_of(s, "Resource"))
            .collect()
    };
    assert_eq!(
        with("s3:PutObject"),
        vec![
            "${aws_s3_bucket.tv_cold.arn}/waste-check/state.json",
            "${aws_s3_bucket.tv_cold.arn}/waste-check/claims/*",
            "${aws_s3_bucket.tv_cold.arn}/cost-audit/*",
        ],
        "PutObject only on the state object, the day claims and the audit prefix"
    );
    assert_eq!(
        with("s3:GetObject"),
        vec!["${aws_s3_bucket.tv_cold.arn}/waste-check/state.json"],
        "GetObject only on the state object"
    );
    assert_eq!(with("s3:ListBucket"), vec!["aws_s3_bucket.tv_cold.arn"]);
    let list_stmt = stmts
        .iter()
        .find(|s| values_of(s, "Action").iter().any(|a| a == "s3:ListBucket"))
        .expect("ListBucket statement");
    assert!(
        list_stmt.contains("StringEquals = { \"s3:prefix\" = \"waste-check/state.json\" }"),
        "the list grant must be limited to the one state key: {list_stmt}"
    );
    assert_eq!(with("sns:Publish"), vec!["aws_sns_topic.tv_alerts.arn"]);
}

/// Every IAM grant resource in `src` that names the waste check's role,
/// other than its one inline policy, plus a role block carrying policies.
fn grants_naming_the_role(src: &str) -> Vec<String> {
    let mut hits = Vec::new();
    let mut from = 0;
    while let Some(rel) = src[from..].find("resource \"") {
        let at = from + rel;
        let header_end = at + src[at..].find('{').expect("resource without a body");
        let header = src[at..header_end].trim().to_string();
        let body = balanced(src, header_end, '{', '}');
        from = header_end + body.len();
        let names_role = body.contains("aws_iam_role.aws_waste_check")
            || body.contains("tv-${var.environment}-aws-waste-check-lambda");
        let is_own_policy = header == "resource \"aws_iam_role_policy\" \"aws_waste_check\"";
        let is_iam_grant =
            header.contains("aws_iam_role_polic") || header.contains("aws_iam_policy_attachment");
        if names_role && is_iam_grant && !is_own_policy {
            hits.push(header.clone());
        }
        if header == "resource \"aws_iam_role\" \"aws_waste_check\""
            && (body.contains("managed_policy_arns") || body.contains("inline_policy"))
        {
            hits.push(format!("{header} carries its own policies"));
        }
    }
    hits
}

#[test]
fn no_other_policy_reaches_the_waste_check_role() {
    let mut hits = Vec::new();
    let mut files = 0usize;
    let dir = std::fs::read_dir(terraform_dir()).expect("terraform dir");
    for entry in dir.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "tf") {
            continue;
        }
        files += 1;
        let src = code_only(&std::fs::read_to_string(&path).expect("read tf"));
        hits.extend(grants_naming_the_role(&src));
    }
    assert!(files > 10, "the terraform scan read only {files} files");
    assert!(
        hits.is_empty(),
        "noise lock 2.18: only the one read-only inline policy may reach the waste \
         check role; found {hits:?}"
    );
}

/// The IST minute of day of a daily `cron(m h * * ? *)` expression, or
/// `None` when it is not one fixed time every day.
fn daily_cron_ist_minute(expr: &str) -> Option<u32> {
    let inner = expr.strip_prefix("cron(")?.strip_suffix(')')?;
    let f: Vec<&str> = inner.split_whitespace().collect();
    if f.len() != 6 || f[2..] != ["*", "*", "?", "*"] {
        return None;
    }
    let minute: u32 = f[0].parse().ok().filter(|m| *m < 60)?;
    let hour: u32 = f[1].parse().ok().filter(|h| *h < 24)?;
    Some((hour * 60 + minute + 5 * 60 + 30) % (24 * 60))
}

/// Outside 09:00-15:40 IST.
fn outside_market_hours(ist_minute: u32) -> bool {
    !(9 * 60..=15 * 60 + 40).contains(&ist_minute)
}

#[test]
fn the_waste_check_runs_once_a_day_outside_market_hours() {
    let src = code_only(&terraform("waste-check-lambda.tf"));
    let rule = block(&src, "aws_cloudwatch_event_rule", "aws_waste_check");
    let expr = values_of(rule, "schedule_expression");
    assert_eq!(expr.len(), 1, "{rule}");
    let ist = daily_cron_ist_minute(&expr[0])
        .unwrap_or_else(|| panic!("not one fixed time every day: {}", expr[0]));
    assert!(
        outside_market_hours(ist),
        "{} runs at {:02}:{:02} IST, inside 09:00-15:40",
        expr[0],
        ist / 60,
        ist % 60
    );
    assert_eq!(
        src.matches("resource \"aws_cloudwatch_event_rule\"")
            .count(),
        1,
        "one schedule only"
    );
}

#[test]
fn a_failed_waste_check_is_never_retried() {
    let src = code_only(&terraform("waste-check-lambda.tf"));
    let cfg = block(
        &src,
        "aws_lambda_function_event_invoke_config",
        "aws_waste_check",
    );
    assert!(cfg.contains("maximum_retry_attempts = 0"), "{cfg}");
}

#[test]
fn the_waste_check_has_only_its_did_it_run_alarm() {
    let src = code_only(&terraform("waste-check-lambda.tf"));
    let not_invoked = block(
        &src,
        "aws_cloudwatch_metric_alarm",
        "aws_waste_check_not_invoked",
    );
    assert!(not_invoked.contains("metric_name         = \"Invocations\""));
    assert!(not_invoked.contains("treat_missing_data  = \"breaching\""));
    assert!(not_invoked.contains("ok_actions    = []"));
    assert_eq!(
        src.matches("resource \"aws_cloudwatch_metric_alarm\"")
            .count(),
        1,
        "noise lock 2.18: no CloudWatch alarm beyond its own not-invoked check"
    );
}

#[test]
fn the_guard_parsers_bite() {
    let fixture = r#"Action = ["ec2:DescribeVolumes", "ec2:DeleteVolume"]
        Resource = "arn:aws:s3:::b/cost-audit/*"
        Sid = "Read" "#;
    let found = actions(fixture);
    assert_eq!(found, vec!["ec2:DescribeVolumes", "ec2:DeleteVolume"]);
    assert!(forbidden("ec2:DeleteVolume"));
    assert!(forbidden("ec2:ReleaseAddress"));
    assert!(forbidden("ec2:ModifyVolume"));
    assert!(forbidden("ec2:StopInstances"));
    assert!(forbidden("ec2:*"));
    assert!(forbidden("ec2:Describe*"));
    assert!(!forbidden("ec2:DescribeVolumes"));
    assert!(!forbidden("ce:GetCostAndUsage"));
    assert_eq!(code_only("a # b\nc"), "a \nc");
    assert_eq!(
        values_of("NotAction = \"x\"", "Action"),
        Vec::<String>::new()
    );
    assert_eq!(
        values_of("Resource = aws_sns_topic.t.arn\n", "Resource"),
        vec!["aws_sns_topic.t.arn"]
    );
}

/// The shapes the 2026-11-11 review showed slipping past the first guard.
#[test]
fn test_regression_the_role_guard_catches_bare_star_lists_notaction_and_attachments() {
    let base = |extra: &str| {
        format!(
            r#"policy = jsonencode({{
    Statement = [
      {{ Effect = "Allow", Action = ["ec2:DescribeVolumes"], Resource = "*" }},
      {{ Effect = "Allow", Action = ["ce:GetCostAndUsage"], Resource = "*" }},
      {extra}
    ]
  }})"#
        )
    };
    let fine = base(
        r#"{ Effect = "Allow", Action = "sns:Publish", Resource = aws_sns_topic.tv_alerts.arn }"#,
    );
    assert_eq!(policy_violations(&fine), Vec::<String>::new());
    let own_logs = base(
        r#"{ Effect = "Allow", Action = ["logs:CreateLogStream", "logs:PutLogEvents"], Resource = "${aws_cloudwatch_log_group.aws_waste_check.arn}:*" }"#,
    );
    assert_eq!(policy_violations(&own_logs), Vec::<String>::new());
    for (what, extra) in [
        (
            "bare star action",
            r#"{ Effect = "Allow", Action = "*", Resource = ["*"] }"#,
        ),
        (
            "star in a list",
            r#"{ Effect = "Allow", Action = ["*"], Resource = "x" }"#,
        ),
        (
            "resource star in a list",
            r#"{ Effect = "Allow", Action = ["s3:GetObject"], Resource = ["*"] }"#,
        ),
        (
            "NotAction",
            r#"{ Effect = "Allow", NotAction = ["iam:*"], Resource = "x" }"#,
        ),
        (
            "delete",
            r#"{ Effect = "Allow", Action = "ec2:DeleteVolume", Resource = "x" }"#,
        ),
        (
            "unlisted read",
            r#"{ Effect = "Allow", Action = "ec2:DescribeInstances", Resource = "x" }"#,
        ),
        (
            "whole bucket",
            r#"{ Effect = "Allow", Action = "s3:GetObject", Resource = "${aws_s3_bucket.tv_cold.arn}/*" }"#,
        ),
        (
            "logs on every group in the region",
            r#"{ Effect = "Allow", Action = ["logs:CreateLogStream", "logs:PutLogEvents"], Resource = "arn:aws:logs:${var.aws_region}:*:*" }"#,
        ),
        (
            "logs on the app log group",
            r#"{ Effect = "Allow", Action = ["logs:PutLogEvents"], Resource = "${aws_cloudwatch_log_group.app.arn}:*" }"#,
        ),
        (
            "CreateLogGroup",
            r#"{ Effect = "Allow", Action = ["logs:CreateLogGroup"], Resource = "${aws_cloudwatch_log_group.aws_waste_check.arn}:*" }"#,
        ),
    ] {
        assert!(
            !policy_violations(&base(extra)).is_empty(),
            "{what} must be caught"
        );
    }
    let attachment = r#"resource "aws_iam_role_policy_attachment" "x" {
  role       = aws_iam_role.aws_waste_check.name
  policy_arn = "arn:aws:iam::aws:policy/AdministratorAccess"
}
resource "aws_iam_role_policy" "second" {
  role = aws_iam_role.aws_waste_check.id
}
resource "aws_iam_role_policy" "aws_waste_check" {
  role = aws_iam_role.aws_waste_check.id
}
resource "aws_iam_role" "aws_waste_check" {
  managed_policy_arns = ["arn:aws:iam::aws:policy/AdministratorAccess"]
}
resource "aws_iam_policy_attachment" "group" {
  roles = [aws_iam_role.aws_waste_check.name]
}
resource "aws_iam_role_policy_attachment" "other_role" {
  role = aws_iam_role.someone_else.name
}"#;
    assert_eq!(
        grants_naming_the_role(attachment),
        vec![
            "resource \"aws_iam_role_policy_attachment\" \"x\"".to_string(),
            "resource \"aws_iam_role_policy\" \"second\"".to_string(),
            "resource \"aws_iam_role\" \"aws_waste_check\" carries its own policies".to_string(),
            "resource \"aws_iam_policy_attachment\" \"group\"".to_string(),
        ]
    );
}

#[test]
fn test_regression_the_schedule_check_reads_the_cron_it_is_given() {
    // The first guard added up literal constants, so a cron moved into
    // market hours passed as long as the pinned string moved with it.
    assert_eq!(
        daily_cron_ist_minute("cron(37 12 * * ? *)"),
        Some(18 * 60 + 7)
    );
    assert!(outside_market_hours(18 * 60 + 7));
    let ist = daily_cron_ist_minute("cron(0 5 * * ? *)").unwrap();
    assert_eq!(ist, 10 * 60 + 30);
    assert!(
        !outside_market_hours(ist),
        "10:30 IST is inside market hours"
    );
    assert!(!outside_market_hours(
        daily_cron_ist_minute("cron(10 10 * * ? *)").unwrap()
    ));
    assert!(outside_market_hours(
        daily_cron_ist_minute("cron(0 2 * * ? *)").unwrap()
    ));
    assert_eq!(
        daily_cron_ist_minute("cron(0/5 12 * * ? *)"),
        None,
        "more than once a day"
    );
    assert_eq!(daily_cron_ist_minute("cron(37 12 ? * MON-FRI *)"), None);
    assert_eq!(daily_cron_ist_minute("rate(1 day)"), None);
}

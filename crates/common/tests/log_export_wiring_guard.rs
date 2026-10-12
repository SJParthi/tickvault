//! Ratchet: every terraform-managed log group (except the metrics scrape group)
//! is copied to the cold S3 bucket, and the copy can never delete anything.
//!
//! ADDED 2026-10-11 (DA-11) with `deploy/aws/terraform/log-export.tf`.
//! Authority: `dhan-rest-only-noise-lock-2026-07-14.md` §2.10-a, where the
//! owner tapped "Logs and alarms" on the card "Prepare which AWS cost changes
//! from the audit as code?". The export is a data copy, not a page.
//!
//! What this pins:
//! 1. Every `aws_cloudwatch_log_group` resource in the terraform module has a
//!    subscription filter in `log-export.tf`, except the two named exemptions
//!    (the raw metrics scrapes, and Firehose's own error log, which would loop).
//!    A new log group added without an export fails here.
//! 2. The Firehose role's policy has no `Delete` action, no lifecycle or
//!    versioning action, and no wildcard bucket; it writes under `logs/` and
//!    `logs-errors/` of the cold bucket only (Quote 27: nothing captured is
//!    ever deleted).
//! 3. Both streams write under `logs/`, and both are `UNCOMPRESSED` (CloudWatch
//!    Logs already gzips the batch).
//!
//! Honest limits: this reads the terraform text. It does not prove the live
//! account matches (no AWS read), and it does not prove Firehose delivers.

use std::fs;
use std::path::{Path, PathBuf};

const LOG_EXPORT_TF: &str = "deploy/aws/terraform/log-export.tf";
const TF_DIR: &str = "deploy/aws/terraform";

/// Log groups deliberately NOT exported, each with its reason.
const NOT_EXPORTED: &[(&str, &str)] = &[
    (
        "tv_metrics",
        "raw 60 s metric scrapes; volume unknown, a later decision (§2.10-a)",
    ),
    (
        "log_export_firehose",
        "Firehose's own delivery-error log; exporting it into the stream that writes it would loop",
    ),
];

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

/// Drop `#` comments, so prose in a header can neither satisfy nor trip a pin.
fn strip_comments(src: &str) -> String {
    src.lines()
        .map(|line| line.split('#').next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The text of one `resource "<kind>" "<name>"` block, comments stripped.
fn resource_block(src: &str, kind: &str, name: &str) -> String {
    let header = format!("resource \"{kind}\" \"{name}\"");
    let start = src
        .find(&header)
        .unwrap_or_else(|| panic!("{LOG_EXPORT_TF} must declare {header}")); // APPROVED: test
    let rest = &src[start + header.len()..];
    let end = rest.find("\nresource ").unwrap_or(rest.len());
    strip_comments(&src[start..start + header.len() + end])
}

/// Every `aws_cloudwatch_log_group` resource name in the terraform module.
fn managed_log_groups() -> Vec<String> {
    let dir = workspace_root().join(TF_DIR);
    let mut out = Vec::new();
    let entries = fs::read_dir(&dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())); // APPROVED: test
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("tf") {
            continue;
        }
        let src = strip_comments(&fs::read_to_string(&path).unwrap_or_default());
        for line in src.lines() {
            let Some(rest) = line
                .trim()
                .strip_prefix("resource \"aws_cloudwatch_log_group\" \"")
            else {
                continue;
            };
            if let Some(name) = rest.split('"').next() {
                out.push(name.to_owned());
            }
        }
    }
    out.sort();
    out
}

/// The managed log groups `export_src` does not export and does not exempt.
fn unexported_groups(groups: &[String], export_src: &str) -> Vec<String> {
    let code = strip_comments(export_src);
    groups
        .iter()
        .filter(|g| !NOT_EXPORTED.iter().any(|(n, _)| n == g))
        .filter(|g| !code.contains(&format!("aws_cloudwatch_log_group.{g}.name")))
        .cloned()
        .collect()
}

#[test]
fn every_managed_log_group_except_the_exemptions_is_exported() {
    let groups = managed_log_groups();
    assert!(
        groups.len() >= 14,
        "found only {} aws_cloudwatch_log_group resources; the scanner has \
         probably stopped matching, which would make this guard vacuous: {groups:?}",
        groups.len()
    );
    for (name, reason) in NOT_EXPORTED {
        assert!(
            groups.iter().any(|g| g == name),
            "the exemption {name} ({reason}) names no log group any more; drop it"
        );
    }
    let missing = unexported_groups(&groups, &read(LOG_EXPORT_TF));
    assert!(
        missing.is_empty(),
        "log groups with NO export to S3: {missing:?}. Add each to \
         local.lambda_log_export_groups in {LOG_EXPORT_TF} (or, for a non-Lambda \
         group, its own subscription filter), or exempt it in NOT_EXPORTED with \
         the reason."
    );
}

#[test]
fn bite_a_dropped_lambda_group_is_caught() {
    let groups = managed_log_groups();
    let src = read(LOG_EXPORT_TF);
    assert!(unexported_groups(&groups, &src).is_empty());
    let line = src
        .lines()
        .find(|l| l.contains("aws_cloudwatch_log_group.telegram_webhook.name"))
        .expect("bite setup: the telegram webhook entry moved"); // APPROVED: test
    let narrowed = src.replacen(line, "", 1);
    assert_eq!(
        unexported_groups(&groups, &narrowed),
        vec!["telegram_webhook".to_owned()],
        "removing one Lambda group from the export must be caught"
    );
}

#[test]
fn the_metrics_scrape_group_is_not_exported() {
    let code = strip_comments(&read(LOG_EXPORT_TF));
    assert!(
        !code.contains("aws_cloudwatch_log_group.tv_metrics") && !code.contains("/metrics"),
        "the raw metrics scrapes are not exported (§2.10-a); exporting them \
         needs its own dated row first"
    );
}

#[test]
fn every_subscription_filter_sends_every_event_to_a_firehose_stream() {
    let code = strip_comments(&read(LOG_EXPORT_TF));
    let filters = code
        .matches("resource \"aws_cloudwatch_log_subscription_filter\"")
        .count();
    assert_eq!(
        filters, 2,
        "expected two subscription filter resources (the app group, and one \
         for_each over the Lambda groups)"
    );
    assert_eq!(
        code.matches("filter_pattern  = \"\"").count(),
        2,
        "every subscription filter must use the empty pattern (every event)"
    );
    assert_eq!(
        code.matches("destination_arn = aws_kinesis_firehose_delivery_stream.")
            .count(),
        2,
        "every subscription filter must point at a delivery stream in this file"
    );
}

#[test]
fn the_firehose_role_can_write_logs_and_delete_nothing() {
    let src = read(LOG_EXPORT_TF);
    let policy = resource_block(&src, "aws_iam_role_policy", "firehose_logs_to_s3");
    for banned in [
        "Delete",
        "Lifecycle",
        "Versioning",
        "s3:*",
        "s3:Put*",
        "arn:aws:s3:::*",
        "\"*\"",
    ] {
        assert!(
            !policy.contains(banned),
            "the Firehose role policy must not contain {banned:?} (Quote 27: \
             nothing captured is ever deleted, and no wildcard bucket):\n{policy}"
        );
    }
    for needed in [
        "\"s3:PutObject\"",
        "\"${aws_s3_bucket.tv_cold.arn}/logs/*\"",
        "\"${aws_s3_bucket.tv_cold.arn}/logs-errors/*\"",
        "Resource = aws_s3_bucket.tv_cold.arn",
    ] {
        assert!(
            policy.contains(needed),
            "the Firehose role policy must carry {needed:?}:\n{policy}"
        );
    }
    // Every object ARN the role can write is under one of the two prefixes.
    let object_arns = policy.matches("${aws_s3_bucket.tv_cold.arn}/").count();
    assert_eq!(
        object_arns, 2,
        "the Firehose role must write under logs/ and logs-errors/ only:\n{policy}"
    );
}

#[test]
fn the_subscription_role_can_only_put_records_into_the_two_streams() {
    let src = read(LOG_EXPORT_TF);
    let policy = resource_block(&src, "aws_iam_role_policy", "logs_to_firehose");
    assert!(
        !policy.contains("\"*\"") && !policy.contains("firehose:*"),
        "the subscription role must name its actions and streams:\n{policy}"
    );
    for needed in [
        "\"firehose:PutRecord\"",
        "\"firehose:PutRecordBatch\"",
        "aws_kinesis_firehose_delivery_stream.app_logs_to_s3.arn",
        "aws_kinesis_firehose_delivery_stream.lambda_logs_to_s3.arn",
    ] {
        assert!(
            policy.contains(needed),
            "the subscription role policy must carry {needed:?}:\n{policy}"
        );
    }
    let trust = resource_block(&src, "aws_iam_role", "logs_to_firehose");
    assert!(
        trust.contains("\"aws:SourceArn\"") && trust.contains("local.log_export_source_groups"),
        "the subscription role must trust only the exported log groups:\n{trust}"
    );
}

#[test]
fn both_streams_write_under_logs_uncompressed() {
    let code = strip_comments(&read(LOG_EXPORT_TF));
    let prefixes: Vec<&str> = code
        .lines()
        .filter_map(|l| l.trim().strip_prefix("prefix"))
        .filter_map(|rest| rest.trim_start().strip_prefix('='))
        .filter_map(|rest| rest.trim().strip_prefix('"'))
        .filter_map(|rest| rest.split('"').next())
        .collect();
    assert_eq!(
        prefixes,
        vec![
            "logs/app/!{timestamp:yyyy/MM/dd}/",
            "logs/lambda/!{timestamp:yyyy/MM/dd}/"
        ],
        "each stream must write under logs/ in its own folder"
    );
    let error_prefixes = code.matches("error_output_prefix = \"logs-errors/").count();
    assert_eq!(
        error_prefixes, 2,
        "each stream's failed records must go under logs-errors/"
    );
    assert_eq!(
        code.matches("compression_format = \"UNCOMPRESSED\"")
            .count(),
        2,
        "both streams must be UNCOMPRESSED: CloudWatch Logs already gzips each batch"
    );
    assert_eq!(
        code.matches("bucket_arn = aws_s3_bucket.tv_cold.arn")
            .count(),
        2,
        "both streams must write to the cold bucket"
    );
}

//! Guard for the Brutex read-only role (owner Quote 60, 2026-11-14): trusted
//! by this account only, list + get + get-attributes on three prefixes of the
//! cold bucket, nothing else.

const ROLE_TF: &str = include_str!("../../../deploy/aws/terraform/brutex-readonly-role.tf");
const OUTPUTS_TF: &str = include_str!("../../../deploy/aws/terraform/outputs.tf");

/// The terraform text without comment lines.
fn code() -> String {
    ROLE_TF
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// [`code`] with every whitespace character removed.
fn squeezed() -> String {
    code().chars().filter(|ch| !ch.is_whitespace()).collect()
}

#[test]
fn test_brutex_role_trusts_only_this_account_root() {
    let c = code();
    assert!(c.contains(
        "Principal = { AWS = \"arn:aws:iam::${data.aws_caller_identity.current.account_id}:root\" }"
    ));
    assert_eq!(c.matches("Principal").count(), 1, "one trusted principal");
    assert_eq!(c.matches("\"sts:AssumeRole\"").count(), 1);
    assert!(!c.contains("Service ="), "no service may assume it");
    assert!(!c.contains("Federated"), "no outside identity provider");
}

#[test]
fn test_brutex_role_actions_are_exactly_list_get_getattributes() {
    let c = code();
    let actions: Vec<&str> = c
        .match_indices("\"s3:")
        .map(|(i, _)| {
            let rest = &c[i + 1..];
            &rest[..rest.find('"').unwrap_or(rest.len())]
        })
        .filter(|a| *a != "s3:prefix")
        .collect();
    assert_eq!(
        actions,
        vec!["s3:ListBucket", "s3:GetObject", "s3:GetObjectAttributes"]
    );
    let s = squeezed();
    assert_eq!(
        s.matches("Effect=\"Allow\"").count(),
        3,
        "trust + two statements"
    );
    assert_eq!(s.matches("Effect=").count(), 3, "no other statement");
}

#[test]
fn test_brutex_role_prefixes_are_exactly_three() {
    let c = code();
    assert!(c.contains(
        "\"s3:prefix\" = [\"questdb-partitions/*\", \"instrument-snapshots/*\", \"raw-frames/*\"]"
    ));
    for p in ["questdb-partitions", "instrument-snapshots", "raw-frames"] {
        assert!(c.contains(&format!(
            "\"arn:aws:s3:::tv-${{var.environment}}-cold/{p}/*\","
        )));
    }
    assert_eq!(
        c.matches("arn:aws:s3:::").count(),
        4,
        "the bucket plus three prefixes"
    );
    assert_eq!(
        c.matches("tv-${var.environment}-cold").count(),
        4,
        "only the cold bucket"
    );
}

#[test]
fn test_brutex_role_has_no_put_delete_or_wildcard() {
    let c = code();
    for banned in [
        "Put",
        "Delete",
        "Tagging",
        "Acl",
        "Policy\"",
        "\"s3:*\"",
        "\"*\"",
        "kms:",
        "iam:Pass",
        "aws_iam_user",
        "aws_iam_access_key",
        "login_profile",
    ] {
        assert!(!c.contains(banned), "the role must not carry `{banned}`");
    }
    assert!(!c.contains("-cold/*\""), "no whole-bucket object grant");
}

#[test]
fn test_brutex_role_max_session_is_12h() {
    let s = squeezed();
    assert!(s.contains("max_session_duration=43200"));
    assert!(s.contains("name=\"tv-${var.environment}-brutex-readonly\""));
}

#[test]
fn test_brutex_role_arn_is_an_output() {
    assert!(OUTPUTS_TF.contains("output \"brutex_readonly_role_arn\""));
    assert!(OUTPUTS_TF.contains("value       = aws_iam_role.brutex_readonly.arn"));
}

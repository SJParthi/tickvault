//! Guard for the live paper fill report's cold-bucket prefix (plan item LF-7,
//! owner Quote 57, 2026-11-12): `s3://tv-<env>-cold/live-fill-logs/`.
//!
//! The box role's whole-bucket grant already allows Get/Put/List there and
//! has no Delete action. LF-7 adds one explicit Deny of DeleteObject and
//! DeleteObjectVersion on the prefix (the only statement that adds real
//! enforcement), and no lifecycle rule may ever expire the report copies.
//! Honest limit: a true "PutObject only on that prefix" would need the
//! whole-bucket grant split, which is out of LF-7's scope.

const MAIN_TF: &str = include_str!("../../../deploy/aws/terraform/main.tf");

/// The statement block (between its `{` and `},`) that names `needle`.
fn statement_with(needle: &str) -> &'static str {
    let at = MAIN_TF.find(needle).expect("statement present"); // APPROVED: test
    let start = MAIN_TF[..at].rfind("      {\n").expect("statement start"); // APPROVED: test
    let end = at + MAIN_TF[at..].find("\n      },").expect("statement end"); // APPROVED: test
    &MAIN_TF[start..end]
}

#[test]
fn test_live_fill_logs_put_statement_present() {
    // The whole-bucket statement covers PutObject on the prefix and holds no
    // Delete action; LF-7 deliberately adds no broader (or redundant) Allow.
    let whole = statement_with("\"arn:aws:s3:::tv-${var.environment}-cold/*\",");
    assert!(whole.contains("Effect = \"Allow\""), "{whole}");
    assert!(whole.contains("\"s3:PutObject\""), "{whole}");
    assert!(!whole.contains("Delete"), "{whole}");
    assert_eq!(
        MAIN_TF.matches("cold/live-fill-logs/*").count(),
        1,
        "exactly one statement names the prefix (the Deny)"
    );
}

#[test]
fn test_live_fill_logs_delete_denied() {
    let deny = statement_with("cold/live-fill-logs/*\"");
    assert!(deny.contains("Effect = \"Deny\""), "{deny}");
    assert!(deny.contains("\"s3:DeleteObject\""), "{deny}");
    assert!(deny.contains("\"s3:DeleteObjectVersion\""), "{deny}");
    assert!(!deny.contains("Effect = \"Allow\""), "{deny}");
}

#[test]
fn test_no_lifecycle_expiration_reaches_live_fill_logs() {
    let at = MAIN_TF
        .find("resource \"aws_s3_bucket_lifecycle_configuration\" \"tv_cold\"")
        .expect("cold bucket lifecycle"); // APPROVED: test
    let rest = &MAIN_TF[at..];
    let end = rest.find("\n}\n").expect("resource end"); // APPROVED: test
    let lifecycle = &rest[..end];
    assert!(!lifecycle.contains("expiration {"), "{lifecycle}");
    assert!(
        !lifecycle.contains("noncurrent_version_expiration"),
        "{lifecycle}"
    );
}

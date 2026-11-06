//! Plan PR MV (2026-11-06): pins the shape of the live-view bucket and the two
//! IAM grants on it, and that the app and the Lambda agree on its name and key.
//!
//! The bucket holds a disposable view (the published movers universe), so it
//! must stay unversioned and expiring, private, encrypted and TLS-only, and
//! the grants must name the ONE object, never the bucket or a wildcard.

use std::path::{Path, PathBuf};

use tickvault_common::operator_movers_universe::{
    MOVERS_UNIVERSE_OBJECT_KEY, live_view_bucket_for,
};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .expect("workspace root must exist above crates/common") // APPROVED: test
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(workspace_root().join(rel))
        .unwrap_or_else(|e| panic!("{rel} must be readable: {e}")) // APPROVED: test
}

/// The block that starts at `header`, comments stripped, up to the next
/// top-level `resource`/`data` block.
fn block(content: &str, header: &str) -> String {
    let start = content
        .find(header)
        .unwrap_or_else(|| panic!("terraform must declare {header}")); // APPROVED: test
    let rest = &content[start + header.len()..];
    let end = ["\nresource ", "\ndata "]
        .iter()
        .filter_map(|m| rest.find(m))
        .min()
        .unwrap_or(rest.len());
    content[start..start + header.len() + end]
        .lines()
        .map(|line| line.split('#').next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

const TF: &str = "deploy/aws/terraform/live-view-bucket.tf";

#[test]
fn test_live_view_bucket_is_named_from_the_environment_and_matches_the_code() {
    let tf = read(TF);
    let bucket = block(&tf, "resource \"aws_s3_bucket\" \"tv_live_view\"");
    assert!(bucket.contains("bucket = \"tv-${var.environment}-live-view\""));
    assert_eq!(live_view_bucket_for("prod"), "tv-prod-live-view");
    assert_eq!(MOVERS_UNIVERSE_OBJECT_KEY, "movers/universe.json");
}

#[test]
fn test_live_view_bucket_is_unversioned_expiring_private_encrypted_and_tls_only() {
    let tf = read(TF);
    assert!(
        !tf.contains("aws_s3_bucket_versioning"),
        "the live-view bucket must never be versioned (a 5 s overwrite would keep every version)"
    );
    let lifecycle = block(
        &tf,
        "resource \"aws_s3_bucket_lifecycle_configuration\" \"tv_live_view\"",
    );
    assert!(lifecycle.contains("expiration {") && lifecycle.contains("days = 2"));
    let pab = block(
        &tf,
        "resource \"aws_s3_bucket_public_access_block\" \"tv_live_view\"",
    );
    for flag in [
        "block_public_acls       = true",
        "block_public_policy     = true",
        "ignore_public_acls      = true",
        "restrict_public_buckets = true",
    ] {
        assert!(pab.contains(flag), "missing `{flag}`");
    }
    let sse = block(
        &tf,
        "resource \"aws_s3_bucket_server_side_encryption_configuration\" \"tv_live_view\"",
    );
    assert!(sse.contains("sse_algorithm = \"AES256\""));
    let tls = block(
        &tf,
        "data \"aws_iam_policy_document\" \"tv_live_view_tls_only\"",
    );
    assert!(tls.contains("effect  = \"Deny\""));
    assert!(tls.contains("variable = \"aws:SecureTransport\""));
    assert!(tls.contains("values   = [\"false\"]"));
    let policy = block(&tf, "resource \"aws_s3_bucket_policy\" \"tv_live_view\"");
    assert!(policy.contains("data.aws_iam_policy_document.tv_live_view_tls_only.json"));
}

#[test]
fn test_live_view_grants_name_the_one_object_only() {
    let main = read("deploy/aws/terraform/main.tf");
    let lines: Vec<&str> = main
        .lines()
        .filter(|l| l.contains("live-view") && !l.trim_start().starts_with('#'))
        .collect();
    assert_eq!(
        lines,
        [
            "        Resource = \"arn:aws:s3:::tv-${var.environment}-live-view/movers/universe.json\""
        ],
        "the instance role may write the movers object only"
    );
    let put = main
        .find("-live-view/movers/universe.json")
        .expect("instance grant"); // APPROVED: test
    let stanza = &main[main[..put].rfind("      {\n").unwrap_or(0)..put]; // APPROVED: test
    assert!(stanza.contains("Action   = [\"s3:PutObject\"]"), "{stanza}");

    let lambda = read("deploy/aws/terraform/operator-control-lambda.tf");
    let read_grant = block(
        &lambda,
        "data \"aws_iam_policy_document\" \"operator_control_permissions\"",
    );
    let start = read_grant
        .find("sid       = \"ReadMoversUniverse\"")
        .expect("Lambda read grant"); // APPROVED: test
    let stanza = &read_grant[start
        ..read_grant[start..]
            .find("\n  }")
            .map_or(read_grant.len(), |e| start + e)];
    assert!(
        stanza.contains("actions   = [\"s3:GetObject\"]"),
        "{stanza}"
    );
    assert!(
        stanza.contains("resources = [\"${aws_s3_bucket.tv_live_view.arn}/movers/universe.json\"]")
    );
    assert!(lambda.contains("LIVE_VIEW_BUCKET = aws_s3_bucket.tv_live_view.bucket"));
    assert!(
        !lambda.contains("tv_live_view.arn}/*") && !main.contains("live-view/*"),
        "no wildcard grant on the live-view bucket"
    );
}

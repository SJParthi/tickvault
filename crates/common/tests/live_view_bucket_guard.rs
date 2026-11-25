//! Plan PR MV (2026-11-06): pins the shape of the live-view bucket and the four
//! one-key IAM grants on it, and that the app and the Lambda agree on its name and key.
//!
//! The bucket holds a disposable view (the published movers universe), so it
//! must stay unversioned and expiring, private, encrypted and TLS-only, and
//! the grants must name ONE object per action (the box writes the universe
//! and reads the viewer heartbeat; the Lambda reads the universe and writes
//! the heartbeat), never the bucket or a wildcard.

use std::path::{Path, PathBuf};

use tickvault_common::operator_movers_universe::{
    MOVERS_UNIVERSE_OBJECT_KEY, MOVERS_VIEWER_OBJECT_KEY, live_view_bucket_for,
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
    assert!(
        lifecycle.contains("abort_incomplete_multipart_upload {")
            && lifecycle.contains("days_after_initiation = 1"),
        "an abandoned multipart upload must not linger"
    );
    let ownership = block(
        &tf,
        "resource \"aws_s3_bucket_ownership_controls\" \"tv_live_view\"",
    );
    assert!(ownership.contains("object_ownership = \"BucketOwnerEnforced\""));
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

/// The `{ ... }` statement in `main.tf` that ends at `resource_line`.
fn main_stanza<'a>(main: &'a str, resource_line: &str) -> &'a str {
    let at = main
        .find(resource_line)
        .unwrap_or_else(|| panic!("instance grant {resource_line}")); // APPROVED: test
    &main[main[..at].rfind("      {\n").unwrap_or(0)..at] // APPROVED: test
}

/// The `statement { ... }` block of the Lambda policy with this sid.
fn lambda_statement(lambda: &str, sid: &str) -> String {
    let policy = block(
        lambda,
        "data \"aws_iam_policy_document\" \"operator_control_permissions\"",
    );
    let start = policy
        .find(&format!("sid       = \"{sid}\""))
        .unwrap_or_else(|| panic!("Lambda statement {sid}")); // APPROVED: test
    policy[start
        ..policy[start..]
            .find("\n  }")
            .map_or(policy.len(), |e| start + e)]
        .to_string()
}

#[test]
fn test_live_view_grants_name_one_object_per_action_only() {
    let main = read("deploy/aws/terraform/main.tf");
    let universe_line =
        "        Resource = \"arn:aws:s3:::tv-${var.environment}-live-view/movers/universe.json\"";
    let viewer_line =
        "        Resource = \"arn:aws:s3:::tv-${var.environment}-live-view/movers/viewer.json\"";
    let lines: Vec<&str> = main
        .lines()
        .filter(|l| l.contains("live-view") && !l.trim_start().starts_with('#'))
        .collect();
    assert_eq!(
        lines,
        [universe_line, viewer_line],
        "the instance role may write the universe and read the heartbeat only"
    );
    let put = main_stanza(&main, universe_line);
    assert!(put.contains("Action   = [\"s3:PutObject\"]"), "{put}");
    let get = main_stanza(&main, viewer_line);
    assert!(get.contains("Action   = [\"s3:GetObject\"]"), "{get}");

    let lambda = read("deploy/aws/terraform/operator-control-lambda.tf");
    let read_grant = lambda_statement(&lambda, "ReadMoversUniverse");
    assert!(
        read_grant.contains("actions   = [\"s3:GetObject\"]"),
        "{read_grant}"
    );
    assert!(
        read_grant
            .contains("resources = [\"${aws_s3_bucket.tv_live_view.arn}/movers/universe.json\"]")
    );
    let write_grant = lambda_statement(&lambda, "WriteMoversViewer");
    assert!(
        write_grant.contains("actions   = [\"s3:PutObject\"]"),
        "{write_grant}"
    );
    assert!(
        write_grant
            .contains("resources = [\"${aws_s3_bucket.tv_live_view.arn}/movers/viewer.json\"]")
    );
    assert_eq!(
        lambda.matches("aws_s3_bucket.tv_live_view.arn}/").count(),
        2,
        "exactly the two one-key grants"
    );
    assert!(lambda.contains("LIVE_VIEW_BUCKET = aws_s3_bucket.tv_live_view.bucket"));
    assert!(lambda.contains("BUDGET_ACCOUNT_ID = data.aws_caller_identity.current.account_id"));
    assert!(
        !lambda.contains("tv_live_view.arn}/*") && !main.contains("live-view/*"),
        "no wildcard grant on the live-view bucket"
    );
    assert_eq!(MOVERS_VIEWER_OBJECT_KEY, "movers/viewer.json");
}

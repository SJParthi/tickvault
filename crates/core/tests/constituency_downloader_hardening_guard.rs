//! Source-scan ratchet pinning the NTM Sub-PR #3 constituency downloader
//! hardening (mirrors the §18 hardening contract that `csv_downloader.rs`
//! documents). Reads `downloader.rs` as text — no feature flag needed —
//! and fails the build if any hardening property is removed.
//!
//! Threats each pinned token defends: redirect-none (DNS-poison MITM),
//! https_only (downgrade), body cap (DoS), connect/read timeouts (hung
//! TCP / black-hole DNS), Content-Type allowlist (HTML 404 / JSON error
//! body fed to the parser), browser User-Agent (niftyindices 403 bot-wall).

use std::fs;
use std::path::PathBuf;

fn downloader_src() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src/instrument/index_constituency/downloader.rs");
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read constituency downloader source {path:?}: {e}"))
}

const REQUIRED_HARDENING_TOKENS: &[(&str, &str)] = &[
    (
        "redirect::Policy::none()",
        "redirect policy must be none (DNS-poison MITM)",
    ),
    (
        "https_only(true)",
        "client must be HTTPS-only (downgrade attack)",
    ),
    (
        "connect_timeout(",
        "connect timeout must be set (black-hole DNS)",
    ),
    (".timeout(", "whole-response timeout must be set (hung TCP)"),
    ("MAX_CSV_BODY_BYTES", "response body must be capped (DoS)"),
    (
        "INDEX_CONSTITUENCY_USER_AGENT",
        "browser User-Agent must be sent (403 bot-wall)",
    ),
    (
        "ALLOWED_CONTENT_TYPES",
        "Content-Type allowlist must be enforced",
    ),
];

#[test]
fn constituency_downloader_retains_all_hardening() {
    let src = downloader_src();
    for (token, why) in REQUIRED_HARDENING_TOKENS {
        assert!(
            src.contains(token),
            "constituency downloader hardening regressed — missing `{token}`: {why}"
        );
    }
}

#[test]
fn constituency_downloader_rejects_html_content_type() {
    // The allowlist must NOT contain text/html (the 404 / Cloudflare page).
    let src = downloader_src();
    assert!(
        src.contains("\"text/csv\"") && !src.contains("\"text/html\""),
        "constituency downloader must allow text/csv and never allow text/html"
    );
}

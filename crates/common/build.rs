//! Build script for `tickvault-common` — B9 deploy provenance.
//!
//! Resolves the git commit SHA at compile time and embeds it as the
//! `TICKVAULT_GIT_SHA` compile-time env var, surfaced to the workspace via
//! `tickvault_common::build_info::BUILD_GIT_SHA`. Resolution order:
//!
//! 1. `GITHUB_SHA` env var (GitHub Actions — EXACT for CI-built artifacts).
//! 2. `git rev-parse HEAD` run from the crate's manifest dir (local dev —
//!    may lag HEAD until this build script re-runs; honest envelope
//!    documented in `.claude/rules/project/deploy-provenance.md`).
//! 3. The literal `unknown` (no git, no `.git` dir, invalid output).
//!
//! The resolved value is validated as exactly 40 lowercase-hex characters;
//! anything else degrades to `unknown` — the binary NEVER embeds garbage
//! and the build NEVER fails because of provenance.
//!
//! NOTE: `println!("cargo:...")` is the mandatory cargo build-script
//! protocol — build scripts are not the lib target, so the workspace
//! `print_stdout` deny does not apply here.

use std::path::Path;
use std::process::Command;

/// True iff `s` is exactly 40 lowercase-hex characters (a full git SHA).
fn is_full_lower_hex_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// SHA from the CI environment (`GITHUB_SHA`), if set, non-empty and valid.
fn sha_from_env() -> Option<String> {
    match std::env::var("GITHUB_SHA") {
        Ok(raw) => {
            let trimmed = raw.trim().to_ascii_lowercase();
            if is_full_lower_hex_sha(&trimmed) {
                Some(trimmed)
            } else {
                None
            }
        }
        Err(_) => None,
    }
}

/// SHA from `git rev-parse HEAD`, run from the manifest dir (git walks up
/// to the repo root). Any failure — no git binary, not a repo, bad output —
/// returns `None` instead of failing the build.
fn sha_from_git(manifest_dir: &str) -> Option<String> {
    if manifest_dir.is_empty() {
        return None;
    }
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(manifest_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let trimmed = text.trim().to_ascii_lowercase();
    if is_full_lower_hex_sha(&trimmed) {
        Some(trimmed)
    } else {
        None
    }
}

fn main() {
    // Re-resolve when the CI-provided SHA changes.
    println!("cargo:rerun-if-env-changed=GITHUB_SHA"); // APPROVED: cargo build-script protocol requires println!("cargo:...")

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();

    // Re-resolve when local HEAD moves (repo root = manifest dir's
    // grandparent). Keep it simple: HEAD only — a branch-ref-file change
    // that leaves HEAD untouched is covered the next time anything else
    // triggers a rebuild (documented limitation, not hidden).
    if !manifest_dir.is_empty() {
        let head = Path::new(&manifest_dir).join("../../.git/HEAD");
        if head.exists() {
            println!("cargo:rerun-if-changed={}", head.display()); // APPROVED: cargo build-script protocol requires println!("cargo:...")
        }
    }

    let sha = sha_from_env()
        .or_else(|| sha_from_git(&manifest_dir))
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=TICKVAULT_GIT_SHA={sha}"); // APPROVED: cargo build-script protocol requires println!("cargo:...")
}

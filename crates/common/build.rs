//! Build script for `tickvault-common` — B9 deploy provenance.
//!
//! Resolves the git commit SHA at compile time and embeds it as the
//! `TICKVAULT_GIT_SHA` compile-time env var, surfaced to the workspace via
//! `tickvault_common::build_info::BUILD_GIT_SHA`. Resolution order:
//!
//! 1. `TICKVAULT_BUILD_GIT_SHA` env var (set ONLY by the deploy workflow's
//!    build steps in `.github/workflows/deploy-aws.yml` — EXACT for the
//!    shipped artifacts).
//! 2. `git rev-parse HEAD` run from the crate's manifest dir (local dev —
//!    may lag HEAD until this build script re-runs; honest envelope
//!    documented in `.claude/rules/project/deploy-provenance.md`).
//! 3. The literal `unknown` (no git, no `.git` dir, invalid output).
//!
//! WHY a dedicated env var instead of GitHub's own `GITHUB_SHA`
//! (2026-07-03 adversarial-review HIGH fix): GitHub Actions sets
//! `GITHUB_SHA` in EVERY workflow and it changes on every commit, so a
//! `rerun-if-env-changed` registration on GITHUB_SHA invalidated
//! tickvault-common in ANY restored target-dir cache — Build & Verify and
//! every other workflow paid a full workspace rebuild per commit. The
//! equally-bad alternative (dropping the rerun registration but still
//! reading GITHUB_SHA) would let a cached CI build silently embed a STALE
//! sha. The dedicated `TICKVAULT_BUILD_GIT_SHA` var resolves the tradeoff:
//! only the deploy build sets it (so only the deploy build re-runs this
//! script and always embeds the exact sha), while all other workflows never
//! set it and keep their caches valid.
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

/// Dedicated env var carrying the exact build sha (deploy workflow only).
const BUILD_SHA_ENV: &str = "TICKVAULT_BUILD_GIT_SHA";

/// True iff `s` is exactly 40 lowercase-hex characters (a full git SHA).
fn is_full_lower_hex_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// SHA from the dedicated `TICKVAULT_BUILD_GIT_SHA` env var, if set,
/// non-empty and valid; otherwise falls through to git / `unknown`.
fn sha_from_env() -> Option<String> {
    match std::env::var(BUILD_SHA_ENV) {
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
    // Re-resolve when the deploy-only build sha changes. Deliberately NOT
    // registered on GitHub's GITHUB_SHA — see the module comment for the
    // CI-cache tradeoff (2026-07-03 adversarial-review HIGH fix).
    println!("cargo:rerun-if-env-changed={BUILD_SHA_ENV}"); // APPROVED: cargo build-script protocol requires println!("cargo:...")

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();

    // Re-resolve when local HEAD moves (repo root = manifest dir's
    // grandparent). Two registrations (2026-07-03 adversarial-review MEDIUM
    // fix — a plain `git commit` does NOT rewrite .git/HEAD, only the branch
    // ref file it points at):
    //   1. `.git/HEAD` itself — covers checkouts / detached-HEAD moves.
    //   2. The resolved branch ref file (e.g. `.git/refs/heads/main`) when
    //      HEAD is symbolic — covers ordinary commits on the current branch.
    // Honest envelope: worktrees (where `.git` is a FILE, not a dir) are
    // still skipped — a stale sha there is corrected the next time anything
    // else triggers a rebuild (documented limitation, not hidden). Packed
    // refs (branch ref file absent) fall back to registration 1 only.
    if !manifest_dir.is_empty() {
        let git_dir = Path::new(&manifest_dir).join("../../.git");
        let head = git_dir.join("HEAD");
        if head.exists() {
            println!("cargo:rerun-if-changed={}", head.display()); // APPROVED: cargo build-script protocol requires println!("cargo:...")
            if let Ok(contents) = std::fs::read_to_string(&head)
                && let Some(ref_path) = contents.trim().strip_prefix("ref: ")
            {
                let branch_ref = git_dir.join(ref_path.trim());
                if branch_ref.exists() {
                    println!("cargo:rerun-if-changed={}", branch_ref.display()); // APPROVED: cargo build-script protocol requires println!("cargo:...")
                }
            }
        }
    }

    let sha = sha_from_env()
        .or_else(|| sha_from_git(&manifest_dir))
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=TICKVAULT_GIT_SHA={sha}"); // APPROVED: cargo build-script protocol requires println!("cargo:...")
}

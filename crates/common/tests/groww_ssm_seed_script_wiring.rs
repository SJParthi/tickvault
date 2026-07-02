//! Ratchet: `scripts/aws-seed-ssm-parameters.sh` must NEVER write any
//! `/tickvault/<env>/groww/*` SSM parameter.
//!
//! Shared token-minter lock 2026-07-02
//! (`.claude/rules/project/groww-shared-token-minter-2026-07-02.md`): the
//! bruteX-owned `groww-token-minter` Lambda is the SOLE minter and the sole
//! owner of the `groww/api-key` + `groww/totp-secret` credential parameters;
//! it writes the daily `groww/access-token`. TickVault is a READ-ONLY consumer
//! of the access-token parameter — an uncoordinated TickVault write (a seed
//! script overwriting the real credentials, or any token write) can break the
//! shared token for BOTH repos. This ratchet supersedes the pre-2026-07-02
//! version of this file, which pinned the OPPOSITE (that the seed script DID
//! seed the two credential params — the flow the minter architecture retired).
//!
//! Mirrors `dhan_ip_registration_script_wiring.rs`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

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

#[test]
fn test_seed_script_exists_and_is_executable() {
    let path = workspace_root().join("scripts/aws-seed-ssm-parameters.sh");
    assert!(
        path.exists(),
        "Z+ L1 DETECT ratchet: scripts/aws-seed-ssm-parameters.sh is missing — \
         the operator cannot seed any SSM secrets (Dhan)."
    );
    let meta = fs::metadata(&path).unwrap_or_else(|e| panic!("stat: {e}")); // APPROVED: test
    assert!(
        meta.permissions().mode() & 0o111 != 0,
        "Z+ L1 DETECT ratchet: aws-seed-ssm-parameters.sh is not executable."
    );
}

#[test]
fn test_seed_script_never_writes_any_groww_param() {
    // The HARD rule: no put_param (or any aws ssm put-parameter) against a
    // groww/* path. The Lambda owns every /tickvault/<env>/groww/* parameter.
    let body = read("scripts/aws-seed-ssm-parameters.sh");
    for line in body.lines() {
        let l = line.trim();
        if l.starts_with('#') {
            continue; // documentation may NAME the paths to explain the ban
        }
        let lower = l.to_ascii_lowercase();
        assert!(
            !(lower.contains("put_param") && lower.contains("/groww/")),
            "shared token-minter lock 2026-07-02 VIOLATED: the seed script \
             writes a /tickvault/<env>/groww/* parameter — those are owned by \
             the bruteX groww-token-minter Lambda. Offending line: {l}"
        );
        assert!(
            !(lower.contains("put-parameter") && lower.contains("/groww/")),
            "shared token-minter lock 2026-07-02 VIOLATED: raw aws ssm \
             put-parameter against a groww/* path. Offending line: {l}"
        );
    }
}

#[test]
fn test_seed_script_documents_the_minter_ownership() {
    // The script must EXPLAIN the ban in place (so a future editor sees the
    // contract at the exact spot they would re-add the seeding).
    let body = read("scripts/aws-seed-ssm-parameters.sh");
    assert!(
        body.contains("groww-token-minter"),
        "the seed script must document that the bruteX groww-token-minter \
         Lambda owns the groww/* parameters (shared token-minter lock \
         2026-07-02)."
    );
    assert!(
        body.contains("read-only") || body.contains("READ-ONLY"),
        "the seed script must state TickVault's read-only consumer role for \
         the groww access-token parameter."
    );
}

#[test]
fn test_seed_script_no_groww_credential_env_vars() {
    // The old --from-env credential vars must not silently return.
    let body = read("scripts/aws-seed-ssm-parameters.sh");
    for var in &["TV_GROWW_API_KEY", "TV_GROWW_TOTP_SECRET"] {
        assert!(
            !body.contains(var),
            "shared token-minter lock 2026-07-02: the seed script references \
             {var} — Groww credentials must never pass through TickVault."
        );
    }
}

#[test]
fn test_rust_reader_targets_the_access_token_param_only() {
    // The Rust SSM reader must read groww/access-token and must NOT read the
    // Lambda-only credential params.
    let reader = read("crates/core/src/auth/secret_manager.rs");
    assert!(
        reader.contains("GROWW_ACCESS_TOKEN_SECRET"),
        "secret_manager.rs no longer reads the groww access-token parameter — \
         the read-only consumer path is broken."
    );
    for banned in &["GROWW_API_KEY_SECRET", "GROWW_TOTP_SECRET"] {
        assert!(
            !reader.contains(banned),
            "shared token-minter lock 2026-07-02: secret_manager.rs references \
             {banned} — the credential parameters are Lambda-only."
        );
    }
}

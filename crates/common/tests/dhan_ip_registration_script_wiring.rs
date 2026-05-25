//! Ratchet: `scripts/register-dhan-ip.sh` exists, is executable, and
//! references the canonical Dhan IP API paths + SSM secret paths.
//!
//! Without this guard, someone renaming the SSM parameter path in
//! `aws-seed-ssm-parameters.sh` would silently break the IP
//! registration script, and the operator would only discover this at
//! 09:00 IST on go-live day when orders start getting rejected by
//! the exchange.

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
fn test_register_dhan_ip_script_exists_and_is_executable() {
    let path = workspace_root().join("scripts/register-dhan-ip.sh");
    assert!(
        path.exists(),
        "Z+ L1 DETECT ratchet: scripts/register-dhan-ip.sh is missing. \
         Operator cannot register the EC2 EIP with Dhan's static-IP whitelist."
    );
    let meta = fs::metadata(&path).unwrap_or_else(|e| panic!("stat: {e}")); // APPROVED: test
    let mode = meta.permissions().mode();
    assert!(
        mode & 0o111 != 0,
        "Z+ L1 DETECT ratchet: scripts/register-dhan-ip.sh is not executable \
         (mode = {mode:o}). Operator would have to `bash scripts/...` instead \
         of running it as a command."
    );
}

#[test]
fn test_register_dhan_ip_script_uses_canonical_endpoints() {
    let body = read("scripts/register-dhan-ip.sh");
    // The three Dhan IP API endpoints — must match what the Rust app
    // in crates/core/src/network/ip_verifier.rs hits.
    for path in &["/ip/getIP", "/ip/setIP", "/ip/modifyIP"] {
        assert!(
            body.contains(path),
            "Z+ L2 VERIFY ratchet: register-dhan-ip.sh missing the canonical \
             Dhan endpoint '{path}'. The Rust crates/core/src/network/ip_verifier.rs \
             uses this exact path — drift here means the script + the app would \
             call different endpoints."
        );
    }
}

#[test]
fn test_register_dhan_ip_script_uses_canonical_credential_sources() {
    let body = read("scripts/register-dhan-ip.sh");
    let seed_body = read("scripts/aws-seed-ssm-parameters.sh");

    // The script reads ONE SSM secret — client-id. It MUST match the
    // path declared in scripts/aws-seed-ssm-parameters.sh. If someone
    // renames the SSM path in either file but not the other, the
    // script will get "ParameterNotFound" at runtime.
    let client_id_path = "/tickvault/${ENVIRONMENT}/dhan/client-id";
    assert!(
        seed_body.contains(client_id_path),
        "Z+ L3 RECONCILE ratchet: SSM path '{client_id_path}' not declared in \
         scripts/aws-seed-ssm-parameters.sh. Seeding workflow is out of sync \
         with register-dhan-ip.sh."
    );
    assert!(
        body.contains(client_id_path),
        "Z+ L3 RECONCILE ratchet: register-dhan-ip.sh's SSM_CLIENT_ID_PATH \
         default does not match the canonical path used by aws-seed-ssm-parameters.sh"
    );

    // The Dhan JWT is NOT in SSM — it's the app's runtime cache. The
    // script's TOKEN_CACHE_FILE default MUST match the constant in
    // crates/common/src/constants.rs::TOKEN_CACHE_FILE_PATH.
    let token_cache_path = "data/cache/tv-token-cache";
    let constants_body = read("crates/common/src/constants.rs");
    assert!(
        constants_body.contains(&format!("\"{token_cache_path}\"")),
        "Z+ L3 RECONCILE ratchet: token cache path '{token_cache_path}' not \
         declared as TOKEN_CACHE_FILE_PATH in crates/common/src/constants.rs. \
         The Rust constant is the single source of truth."
    );
    assert!(
        body.contains(token_cache_path),
        "Z+ L3 RECONCILE ratchet: register-dhan-ip.sh's TOKEN_CACHE_FILE \
         default does not match the Rust constant TOKEN_CACHE_FILE_PATH \
         in crates/common/src/constants.rs."
    );

    // Operator env override must be wired so first-run-on-fresh-host
    // (where the local JWT cache does not yet exist) is unblocked.
    assert!(
        body.contains("TV_DHAN_ACCESS_TOKEN"),
        "Z+ L4 PREVENT ratchet: register-dhan-ip.sh must accept a \
         TV_DHAN_ACCESS_TOKEN env override so the operator can paste a JWT \
         when the local cache is absent."
    );
}

#[test]
fn test_register_dhan_ip_script_is_dry_run_by_default() {
    let body = read("scripts/register-dhan-ip.sh");
    // L4 PREVENT — destructive PUT/POST must not happen without explicit
    // operator flag. The dry-run-by-default invariant prevents an
    // accidental `bash register-dhan-ip.sh` from triggering a 7-day
    // cooldown lockout.
    assert!(
        body.contains("DRY_RUN=true"),
        "Z+ L4 PREVENT ratchet: register-dhan-ip.sh must default to DRY_RUN=true. \
         Without this default, running the script without --apply could \
         trigger an accidental Dhan API modifyIP and lock the operator out \
         for 7 days."
    );
}

#[test]
fn test_register_dhan_ip_script_requires_explicit_modify_flag() {
    let body = read("scripts/register-dhan-ip.sh");
    // L4 PREVENT — even with --apply, calling PUT /v2/ip/modifyIP must
    // require explicit --modify to acknowledge the 7-day cooldown risk.
    assert!(
        body.contains("ALLOW_MODIFY=false"),
        "Z+ L4 PREVENT ratchet: register-dhan-ip.sh must default \
         ALLOW_MODIFY=false. modifyIP triggers a 7-day cooldown and MUST \
         require explicit operator acknowledgement via --modify."
    );
    assert!(
        body.contains("--modify"),
        "Z+ L4 PREVENT ratchet: register-dhan-ip.sh must accept a --modify \
         flag that explicitly acknowledges the 7-day cooldown risk."
    );
}

#[test]
fn test_register_dhan_ip_unit_tests_exist() {
    let path = workspace_root().join("scripts/test-register-dhan-ip.sh");
    assert!(
        path.exists(),
        "Z+ L5 AUDIT ratchet: scripts/test-register-dhan-ip.sh is missing. \
         The pure-function helpers (is_valid_ipv4, days_since_iso_date) \
         have no regression coverage."
    );
}

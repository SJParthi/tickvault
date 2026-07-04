//! Ratchet: TickVault NEVER mints a Groww access token, NEVER touches the
//! Groww credential parameters, and always consumes the shared minted token
//! read-only.
//!
//! Shared token-minter architecture (operator lock 2026-07-02 —
//! `.claude/rules/project/groww-shared-token-minter-2026-07-02.md`): the
//! bruteX-owned AWS Lambda `groww-token-minter` is the SOLE minter (daily
//! EventBridge ~06:05 IST). It reads `/tickvault/prod/groww/api-key` +
//! `/totp-secret` and writes `/tickvault/prod/groww/access-token`. TickVault
//! reads ONLY the access-token parameter (IAM reader role
//! `groww-token-minter-reader-tickvault`). An uncoordinated TickVault mint
//! trips Groww's token-issuance rate limit and can invalidate the shared
//! token MID-SESSION for BruteX — so every mint-shaped code path is a
//! build failure here, not a review comment.

use std::fs;
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

/// Recursively collect every `.rs` file under `dir` (production sources).
fn rust_sources_under(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Skip test trees — THIS guard and other tests may name the banned
            // strings in assertions/messages.
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or(""); // APPROVED: test
            if name == "tests" || name == "target" {
                continue;
            }
            rust_sources_under(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

#[test]
fn test_no_mint_call_sites_in_rust_production_code() {
    // The Groww token-mint surface must not exist anywhere in crates/*/src:
    // no mint endpoint, no SDK mint call, no Groww-TOTP generation.
    let mut sources = Vec::new();
    rust_sources_under(&workspace_root().join("crates"), &mut sources);
    assert!(
        sources.len() > 100,
        "sanity: expected the workspace's production sources, got {}",
        sources.len()
    );
    // NOTE: the generic `get_access_token` name is NOT banned workspace-wide —
    // the Dhan OMS token bridge legitimately exposes a method of that name
    // (`trading_pipeline.rs`). The GROWW mint surface is pinned by its own
    // distinct names + the mint endpoint path.
    let banned = [
        "/v1/token/api/access",      // the Groww mint endpoint
        "obtain_groww_access_token", // the deleted Rust mint fn
        "fetch_groww_credentials",   // the deleted credential SSM read
        "GrowwCredentials",          // the deleted credential type
    ];
    for path in &sources {
        let body = fs::read_to_string(path).unwrap_or_default();
        for needle in &banned {
            assert!(
                !body.contains(needle),
                "shared token-minter lock 2026-07-02 VIOLATED: `{needle}` found \
                 in {} — TickVault never mints a Groww token and never touches \
                 the Groww credentials.",
                path.display()
            );
        }
    }
}

#[test]
fn test_no_groww_credential_param_references_in_rust_production_code() {
    // The Lambda-only credential parameter names must not be referenced by any
    // production source (the doc-comment in constants.rs explains the ban but
    // does not name the string forms below).
    let mut sources = Vec::new();
    rust_sources_under(&workspace_root().join("crates"), &mut sources);
    for path in &sources {
        let body = fs::read_to_string(path).unwrap_or_default();
        for needle in ["groww/api-key", "groww/totp-secret"] {
            assert!(
                !body.contains(needle),
                "token-minter lock VIOLATED: credential param path `{needle}` \
                 referenced in {} — those parameters are Lambda-only by IAM \
                 design.",
                path.display()
            );
        }
    }
}

#[test]
fn test_sidecar_reads_token_never_mints() {
    let sidecar = read("scripts/groww-sidecar/groww_sidecar.py");
    // Must read the shared token (SSM param path env + WithDecryption read).
    assert!(
        sidecar.contains("GROWW_SSM_TOKEN_PARAM"),
        "the sidecar must read the SSM parameter path env (GROWW_SSM_TOKEN_PARAM)."
    );
    assert!(
        sidecar.contains("WithDecryption=True"),
        "the sidecar must GetParameter the token with decryption (SecureString)."
    );
    // Must NEVER mint or hold credentials.
    for needle in [
        "get_access_token(", // the SDK mint CALL form (comments reworded away)
        "import pyotp",
        "pyotp.",
        "GROWW_API_KEY",
        "GROWW_TOTP_SECRET",
        "/v1/token/api/access",
    ] {
        assert!(
            !sidecar.contains(needle),
            "token-minter lock VIOLATED: `{needle}` found in groww_sidecar.py — \
             the sidecar reads the minted token read-only and never mints."
        );
    }
    // The bounded-stale alert chain must exist: 60s auth pacing + the 10-min
    // edge-triggered REJECTED marker the supervisor routes to Telegram.
    assert!(
        sidecar.contains("AUTH_RETRY_FLOOR_SECS = 60"),
        "auth-stale retries must be paced at >=60s (rides the 06:00->06:05 mint gap)."
    );
    assert!(
        sidecar.contains("TOKEN_STALE_ALERT_SECS = 600"),
        "the token-stale alert bound must be ~10 minutes."
    );
    assert!(
        sidecar.contains("GROWW LIVE FEED REJECTED: access token stale"),
        "the >10min stale alert marker line must exist (supervisor routes it to \
         feed-health + Telegram)."
    );
}

#[test]
fn test_smoke_script_never_mints() {
    let smoke = read("scripts/groww-sidecar/groww_smoke.py");
    for needle in [
        "get_access_token",
        "pyotp",
        "GROWW_API_KEY",
        "GROWW_TOTP_SECRET",
    ] {
        assert!(
            !smoke.contains(needle),
            "token-minter lock VIOLATED: `{needle}` found in groww_smoke.py."
        );
    }
}

#[test]
fn test_requirements_pins_growwapi_sdk_version() {
    // Session-B fix #2 (operator go 2026-07-04): the sidecar's WIRE PROTOCOL
    // is the growwapi==1.5.0 wheel — the native-Rust proto/subject extraction
    // (docs/groww-ref/) and the shadow-client parity contract were derived
    // from EXACTLY that version. A silent SDK bump can change the NATS
    // subject grammar / protobuf schema and previously passed every test.
    // This ratchet makes any growwapi version change a BUILD FAILURE that
    // forces a fresh wheel extraction + a deliberate pin update.
    let req = read("scripts/groww-sidecar/requirements.txt");
    assert!(
        req.lines().any(|l| l.trim() == "growwapi==1.5.0"),
        "requirements.txt must pin the EXACT line `growwapi==1.5.0` — the \
         native proto/subjects were extracted from that wheel; bumping the \
         SDK requires re-extracting the wire protocol and updating this pin \
         deliberately (Session-B fix #2, 2026-07-04)."
    );
    // Every version stays exact-pinned (`==`) per the project pinning rule —
    // no `>=` / `~=` / unpinned lines may sneak into the manifest.
    for line in req.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        assert!(
            t.contains("=="),
            "requirements.txt line `{t}` is not exact-pinned (`==`) — \
             `^`/`~`/`>=`/unpinned dependencies are banned project-wide."
        );
    }
}

#[test]
fn test_requirements_manifest_has_boto3_not_pyotp() {
    let req = read("scripts/groww-sidecar/requirements.txt");
    assert!(
        req.contains("boto3=="),
        "requirements.txt must pin boto3 (the read-only SSM token consumer)."
    );
    assert!(
        !req.contains("pyotp=="),
        "requirements.txt must not pin pyotp — the TOTP mint path is deleted."
    );
}

#[test]
fn test_rust_reader_and_constant_exist() {
    // The read-only consumer path must exist: the constant + the fetch fn.
    let constants = read("crates/common/src/constants.rs");
    assert!(
        constants.contains("GROWW_ACCESS_TOKEN_SECRET"),
        "constants.rs must define GROWW_ACCESS_TOKEN_SECRET (\"access-token\")."
    );
    let sm = read("crates/core/src/auth/secret_manager.rs");
    assert!(
        sm.contains("fetch_groww_access_token"),
        "secret_manager.rs must expose the read-only fetch_groww_access_token."
    );
    // And the reader module must never WRITE a parameter.
    assert!(
        !sm.to_ascii_lowercase().contains("put_parameter"),
        "secret_manager.rs must remain read-only — no SSM put_parameter, ever."
    );
}

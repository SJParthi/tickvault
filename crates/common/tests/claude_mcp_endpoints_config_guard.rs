//! Guard test — Claude MCP endpoints configuration invariants.
//!
//! Enforces that `config/claude-mcp-endpoints.toml` is:
//!   1. Present in the repo (committed, not gitignored)
//!   2. Parseable as TOML
//!   3. Has an `active` top-level string pointing at a valid profile
//!   4. Has the three canonical profiles (local, mac-dev, aws-prod)
//!   5. Every profile has all required keys (2 URL + 2 non-URL)
//!   6. Every URL-typed key is a valid-looking HTTP(S) URL
//!
//! Milestone 1 of `.claude/plans/autonomous-operations-100pct.md`.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

const REQUIRED_PROFILES: &[&str] = &["local", "mac-dev", "aws-prod"];
const REQUIRED_URL_KEYS: &[&str] = &["questdb_url", "tickvault_api_url"];
const REQUIRED_NON_URL_KEYS: &[&str] = &["logs_source", "logs_dir_local"];

#[derive(Debug, Deserialize)]
struct EndpointsConfig {
    active: String,
    profiles: HashMap<String, Profile>,
}

#[derive(Debug, Deserialize)]
struct Profile {
    questdb_url: String,
    tickvault_api_url: String,
    logs_source: String,
    logs_dir_local: String,
}

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = <repo>/crates/common → walk up two levels
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap().parent().unwrap().to_path_buf()
}

fn load_config() -> (String, EndpointsConfig) {
    let path = repo_root().join("config/claude-mcp-endpoints.toml");
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "config/claude-mcp-endpoints.toml is missing or unreadable: {err}\n\
             Path checked: {}",
            path.display()
        )
    });
    let parsed: EndpointsConfig = toml::from_str(&raw)
        .unwrap_or_else(|err| panic!("config/claude-mcp-endpoints.toml failed to parse: {err}"));
    (raw, parsed)
}

#[test]
fn config_file_exists_and_parses() {
    // Just calling load_config performs both checks; panics carry context.
    let _ = load_config();
}

#[test]
fn active_profile_key_must_reference_an_existing_profile() {
    let (_, cfg) = load_config();
    assert!(
        cfg.profiles.contains_key(&cfg.active),
        "active='{}' references a profile not present in [profiles.*]; \
         available profiles: {:?}",
        cfg.active,
        cfg.profiles.keys().collect::<Vec<_>>()
    );
}

#[test]
fn all_required_profiles_are_present() {
    let (_, cfg) = load_config();
    for name in REQUIRED_PROFILES {
        assert!(
            cfg.profiles.contains_key(*name),
            "[profiles.{name}] section is required but missing from \
             config/claude-mcp-endpoints.toml"
        );
    }
}

#[test]
fn every_profile_has_all_required_keys() {
    let (raw, _cfg) = load_config();
    // Re-parse loosely so we can surface missing-key errors per profile
    // with clearer diagnostics than the derived Deserialize gives.
    let loose: toml::Value = toml::from_str(&raw).expect("already parses");
    let profiles = loose
        .get("profiles")
        .and_then(|v| v.as_table())
        .expect("[profiles.*] must be a table");
    for name in REQUIRED_PROFILES {
        let profile = profiles
            .get(*name)
            .and_then(|v| v.as_table())
            .unwrap_or_else(|| panic!("[profiles.{name}] missing"));
        for key in REQUIRED_URL_KEYS {
            assert!(
                profile.contains_key(*key),
                "[profiles.{name}] missing required URL key '{key}'"
            );
        }
        for key in REQUIRED_NON_URL_KEYS {
            assert!(
                profile.contains_key(*key),
                "[profiles.{name}] missing required key '{key}'"
            );
        }
    }
}

#[test]
fn all_url_keys_look_like_http_or_https() {
    let (_, cfg) = load_config();
    for (name, profile) in &cfg.profiles {
        for (key, value) in [
            ("questdb_url", &profile.questdb_url),
            ("tickvault_api_url", &profile.tickvault_api_url),
        ] {
            assert!(
                value.starts_with("http://") || value.starts_with("https://"),
                "[profiles.{name}].{key} must start with http:// or https://, got: {value}"
            );
        }
    }
}

#[test]
fn logs_source_is_http_or_local_everywhere() {
    let (_, cfg) = load_config();
    for (name, profile) in &cfg.profiles {
        assert!(
            profile.logs_source == "http" || profile.logs_source == "local",
            "[profiles.{name}].logs_source must be 'http' or 'local', got '{}'",
            profile.logs_source
        );
        assert!(
            !profile.logs_dir_local.trim().is_empty(),
            "[profiles.{name}].logs_dir_local must be non-empty (used \
             as filesystem fallback when logs_source='local')"
        );
    }
}

#[test]
fn local_profile_uses_localhost_not_tailscale() {
    // The "local" fallback profile MUST point at 127.0.0.1 so that Mode A
    // (Claude on Mac + tickvault on Mac) works with no tunnel and no
    // committed Tailscale hostname. Prevents accidental overwrites of the
    // local profile with real hostnames.
    let (_, cfg) = load_config();
    let local = cfg
        .profiles
        .get("local")
        .expect("local profile presence already asserted");
    for url in [&local.questdb_url, &local.tickvault_api_url] {
        assert!(
            url.contains("127.0.0.1") || url.contains("localhost"),
            "[profiles.local] URLs must point at localhost, got: {url}"
        );
    }
}

#[test]
fn tunnel_install_scripts_exist_and_are_executable() {
    // Hand-in-glove with the config: the tunnel install scripts must exist
    // so the one-command setup in the runbook actually works. If someone
    // deletes install-mac.sh or install-aws.sh without updating the runbook,
    // this test fails the build.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let root = repo_root();
        for name in ["install-mac.sh", "install-aws.sh", "doctor.sh"] {
            let path = root.join("scripts/tv-tunnel").join(name);
            let meta = std::fs::metadata(&path)
                .unwrap_or_else(|err| panic!("tunnel script missing: {} ({err})", path.display()));
            let mode = meta.permissions().mode();
            // Owner execute bit: 0o100
            assert!(
                mode & 0o100 != 0,
                "{} exists but is not executable (mode: {:o})",
                path.display(),
                mode
            );
        }
        // plist + service unit don't need to be executable, just present.
        for name in ["com.tickvault.tunnel.plist", "tickvault-tunnel.service"] {
            let path = root.join("scripts/tv-tunnel").join(name);
            assert!(
                path.is_file(),
                "tunnel unit template missing: {}",
                path.display()
            );
        }
    }
    #[cfg(not(unix))]
    {
        // On non-unix, just assert files exist.
        let root = repo_root();
        for name in [
            "install-mac.sh",
            "install-aws.sh",
            "doctor.sh",
            "com.tickvault.tunnel.plist",
            "tickvault-tunnel.service",
        ] {
            let path = root.join("scripts/tv-tunnel").join(name);
            assert!(path.is_file(), "tunnel file missing: {}", path.display());
        }
    }
}

// 2026-07-18 (rust-only phase 2c, CUTOVER DONE): server.py is DELETED
// from git — the committed-config contract now binds the Rust server
// that .mcp.json actually launches. The python `_endpoint_url` pin is
// replaced by its post-cutover truth (no live python consumer remains;
// the Rust twin below carries the resolver pins) WITHOUT weakening.
#[test]
fn config_file_is_consumed_by_the_rust_mcp_server_only() {
    // The config header must name its real (rust) consumer, and no
    // git-tracked python MCP server may exist to bypass the contract.
    let (raw, _) = load_config();
    assert!(
        raw.contains("crates/tickvault-logs-mcp"),
        "config/claude-mcp-endpoints.toml header must name the Rust MCP \
         server as its consumer (post-cutover truth)"
    );
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root())
        .args(["ls-files", "--", "scripts/mcp-servers/tickvault-logs"])
        .output()
        .expect("run git ls-files");
    assert!(out.status.success(), "git ls-files failed");
    let tracked = String::from_utf8_lossy(&out.stdout);
    assert!(
        tracked.trim().is_empty(),
        "a git-tracked python MCP server reappeared — the committed-config \
         contract is pinned on the Rust side only:\n{tracked}"
    );
}

// The Rust MCP server consumes the SAME committed config file through
// the SAME profile keys server.py did (resolver parity, frozen by the
// parity harness against the pinned git-history reference).
#[test]
fn rust_mcp_port_reads_config_file_before_env_vars() {
    // The URL profile keys are consulted at the tool call sites
    // (tools.rs passes them into config::endpoint_url); the logs keys
    // are consulted inside the resolver (config.rs). Scan both.
    let root = repo_root();
    let mut src = String::new();
    for rel in [
        "crates/tickvault-logs-mcp/src/config.rs",
        "crates/tickvault-logs-mcp/src/tools.rs",
    ] {
        let path = root.join(rel);
        src.push_str(
            &std::fs::read_to_string(&path)
                .unwrap_or_else(|_| panic!("Rust MCP port source missing at {}", path.display())),
        );
    }
    for kind in [
        "questdb_url",
        "tickvault_api_url",
        "logs_source",
        "logs_dir_local",
    ] {
        assert!(
            src.contains(&format!("\"{kind}\"")),
            "Rust MCP port must reference profile key '{kind}' \
             (endpoint_url/logs resolver parity with server.py)"
        );
    }
    // And the canonical committed-config filename itself.
    assert!(
        src.contains("claude-mcp-endpoints.toml"),
        "Rust MCP port must load config/claude-mcp-endpoints.toml"
    );
}

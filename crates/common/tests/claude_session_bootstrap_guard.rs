//! Guard test — every Claude session (local, Mac-tunnel, AWS-tunnel, cowork)
//! auto-attaches to the tickvault runtime.
//!
//! Enforces that the auto-attach chain is wired end-to-end:
//!   1. `scripts/claude-session-bootstrap.sh` exists and is executable.
//!   2. Bootstrap reads the active profile from `config/claude-mcp-endpoints.toml`
//!      and writes `.claude/.session-env` with the 5 `TICKVAULT_*_URL` vars
//!      plus the 5 `*_STATUS` probe results.
//!   3. `.claude/hooks/session-sanity.sh` invokes the bootstrap and sources
//!      `.session-env` on every SessionStart.
//!   4. `.claude/settings.json` has a PreCompact hook that re-invokes the
//!      bootstrap so the post-compaction session still has runtime context.
//!   5. `scripts/doctor.sh` accepts `--gate` to treat runtime SKIP as FAIL.
//!   6. `.gitignore` excludes the generated `.claude/.session-env` so it
//!      never lands in a commit.
//!
//! If any link in this chain breaks, every future Claude session regresses
//! to "no log/DB/metric access" silently. This test fails the build first.

use std::fs;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap().parent().unwrap().to_path_buf()
}

fn read(path: &str) -> String {
    let p = repo_root().join(path);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("failed to read {}: {e}", p.display()))
}

#[test]
fn bootstrap_script_exists_and_is_executable() {
    let p = repo_root().join("scripts/claude-session-bootstrap.sh");
    assert!(p.exists(), "scripts/claude-session-bootstrap.sh missing");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&p).unwrap().permissions().mode();
        assert!(
            mode & 0o111 != 0,
            "bootstrap script is not executable (mode={mode:o})"
        );
    }
}

#[test]
fn bootstrap_writes_all_five_endpoint_vars() {
    let src = read("scripts/claude-session-bootstrap.sh");
    for var in [
        "TICKVAULT_PROMETHEUS_URL",
        "TICKVAULT_ALERTMANAGER_URL",
        "TICKVAULT_QUESTDB_URL",
        "TICKVAULT_GRAFANA_URL",
        "TICKVAULT_API_URL",
    ] {
        assert!(
            src.contains(&format!("export {var}")),
            "bootstrap does not export {var}"
        );
    }
}

#[test]
fn bootstrap_writes_all_five_probe_status_vars() {
    let src = read("scripts/claude-session-bootstrap.sh");
    for var in [
        "TICKVAULT_PROM_STATUS",
        "TICKVAULT_ALERT_STATUS",
        "TICKVAULT_QDB_STATUS",
        "TICKVAULT_GRAF_STATUS",
        "TICKVAULT_API_STATUS",
    ] {
        assert!(
            src.contains(&format!("export {var}")),
            "bootstrap does not export {var}"
        );
    }
}

#[test]
fn bootstrap_reads_active_profile_from_config() {
    let src = read("scripts/claude-session-bootstrap.sh");
    assert!(
        src.contains("config/claude-mcp-endpoints.toml"),
        "bootstrap must read config/claude-mcp-endpoints.toml"
    );
    assert!(
        src.contains("TICKVAULT_MCP_PROFILE"),
        "bootstrap must honor TICKVAULT_MCP_PROFILE override"
    );
}

#[test]
fn session_sanity_hook_invokes_bootstrap_and_sources_env() {
    let hook = read(".claude/hooks/session-sanity.sh");
    assert!(
        hook.contains("scripts/claude-session-bootstrap.sh"),
        "session-sanity.sh must invoke bootstrap"
    );
    assert!(
        hook.contains(".claude/.session-env"),
        "session-sanity.sh must reference .claude/.session-env"
    );
}

#[test]
fn session_sanity_hook_tails_live_errors_jsonl() {
    let hook = read(".claude/hooks/session-sanity.sh");
    assert!(
        hook.contains("errors.jsonl"),
        "session-sanity must tail data/logs/errors.jsonl so every new Claude \
         session sees real ERROR events without a manual tool call"
    );
}

#[test]
fn precompact_hook_reinvokes_bootstrap() {
    let settings = read(".claude/settings.json");
    let precompact_start = settings
        .find("\"PreCompact\"")
        .expect("PreCompact hook missing");
    let postcompact_start = settings[precompact_start..]
        .find("\"PostCompact\"")
        .expect("PostCompact hook missing (PreCompact section unbounded)");
    let precompact_block = &settings[precompact_start..precompact_start + postcompact_start];
    assert!(
        precompact_block.contains("claude-session-bootstrap.sh"),
        "PreCompact hook must re-invoke claude-session-bootstrap.sh so the \
         post-compact session retains runtime endpoint context"
    );
}

#[test]
fn doctor_accepts_gate_flag() {
    let doctor = read("scripts/doctor.sh");
    assert!(
        doctor.contains("--gate"),
        "scripts/doctor.sh must accept --gate to fail (not SKIP) when runtime \
         endpoints are unreachable during a live session"
    );
    assert!(
        doctor.contains("GATE_FAIL") || doctor.contains("GATE_MODE"),
        "doctor --gate must actually mutate exit behaviour, not just accept the flag"
    );
}

#[test]
fn session_env_is_gitignored() {
    let gitignore = read(".gitignore");
    assert!(
        gitignore.contains(".claude/.session-env"),
        ".claude/.session-env must be gitignored — it contains host-specific \
         endpoints and probe results that must not land in commits"
    );
}

#[test]
fn mcp_server_has_placeholder_aware_env_resolution() {
    // PR #288 review finding: Claude Code's MCP launcher passes literal
    // `${TICKVAULT_LOGS_DIR}` strings when the shell env var is missing.
    // The server must treat those as unset and fall through to the TOML.
    let src = read("scripts/mcp-servers/tickvault-logs/server.py");
    assert!(
        src.contains("_is_resolved"),
        "server.py must define _is_resolved() helper that rejects ${{...}} \
         placeholder strings — otherwise MCP log tools return ${{...}}/file \
         not found on every fresh Claude session"
    );
    assert!(
        src.contains("startswith(\"${\")"),
        "_is_resolved() must explicitly check for the `${{` placeholder prefix"
    );
    // Every TICKVAULT_ env accessor must go through _is_resolved, not a
    // bare truthy check.
    for accessor in [
        "_active_profile",
        "_endpoint_url",
        "_logs_dir",
        "_logs_source",
    ] {
        let pos = src
            .find(&format!("def {accessor}"))
            .unwrap_or_else(|| panic!("server.py missing {accessor}"));
        let body_end = src[pos..]
            .find("\n\n\n")
            .map(|n| pos + n)
            .unwrap_or(src.len());
        let body = &src[pos..body_end];
        assert!(
            body.contains("_is_resolved"),
            "{accessor} must use _is_resolved() gatekeeper — raw env truthy \
             check lets ${{...}} placeholders through"
        );
    }
}

#[test]
fn mcp_server_placeholder_fallback_test_exists() {
    let p = repo_root().join("scripts/mcp-servers/tickvault-logs/test_placeholder_fallback.py");
    assert!(p.exists(), "placeholder fallback test missing");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&p).unwrap().permissions().mode();
        assert!(
            mode & 0o111 != 0,
            "placeholder fallback test must be executable (mode={mode:o})"
        );
    }
}

#[test]
fn validate_automation_runs_placeholder_fallback_test() {
    let s = read("scripts/validate-automation.sh");
    assert!(
        s.contains("test_placeholder_fallback.py"),
        "validate-automation.sh must run the placeholder fallback test so \
         every audit catches regressions"
    );
}

#[test]
fn active_profile_is_one_of_the_known_profiles() {
    let cfg = read("config/claude-mcp-endpoints.toml");
    let active_line = cfg
        .lines()
        .find(|l| l.trim_start().starts_with("active"))
        .expect("config/claude-mcp-endpoints.toml missing `active = ...`");
    let active = active_line
        .split('=')
        .nth(1)
        .unwrap()
        .trim()
        .trim_matches('"');
    assert!(
        ["local", "mac-dev", "aws-prod"].contains(&active),
        "active profile must be one of local|mac-dev|aws-prod — got {active:?}"
    );
}

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
fn session_sanity_hook_pulls_prometheus_when_reachable() {
    // PR #288 (#9b): when Prometheus is REACHABLE, SessionStart must pull
    // the zero-tick-loss-adjacent counters so operators and Claude both see
    // the real numbers at session start. The pull must be conditional
    // (REACHABLE only) to keep SessionStart fast when the stack is down.
    let hook = read(".claude/hooks/session-sanity.sh");
    assert!(
        hook.contains("TICKVAULT_PROM_STATUS") && hook.contains("REACHABLE"),
        "session-sanity must gate the Prometheus pull on PROM_STATUS=REACHABLE"
    );
    assert!(
        hook.contains("tv_ticks_dropped_total"),
        "session-sanity must pull tv_ticks_dropped_total"
    );
    assert!(
        hook.contains("tv_depth_sequence_holes_total"),
        "session-sanity must pull tv_depth_sequence_holes_total (PR #288 #5)"
    );
    assert!(
        hook.contains("tv_instrument_registry_cross_segment_collisions"),
        "session-sanity must pull the registry collision gauge (I-P1-11)"
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
fn bootstrap_supports_profile_auto_switch() {
    // PR #288 (#10): bootstrap auto-switches from a dead profile to a
    // reachable one. The logic must be in the script, must respect the
    // operator's explicit override, and must expose the switched-from
    // state via TICKVAULT_AUTO_SWITCHED_FROM so session-sanity can surface it.
    let src = read("scripts/claude-session-bootstrap.sh");
    assert!(
        src.contains("AUTO_SWITCH_MIN_REACHABLE"),
        "bootstrap must define AUTO_SWITCH_MIN_REACHABLE threshold"
    );
    assert!(
        src.contains("AUTO_SWITCH_ENABLED"),
        "bootstrap must track whether auto-switch is enabled (disabled under operator override)"
    );
    assert!(
        src.contains("AUTO_SWITCHED_FROM"),
        "bootstrap must expose which profile it switched from for operator visibility"
    );
    assert!(
        src.contains("TICKVAULT_AUTO_SWITCHED_FROM"),
        "bootstrap must export TICKVAULT_AUTO_SWITCHED_FROM env var so session-sanity can announce the switch"
    );
    assert!(
        src.contains("OVERRIDE_PROFILE"),
        "bootstrap must respect TICKVAULT_MCP_PROFILE operator override and disable auto-switch when set"
    );
}

#[test]
fn bootstrap_override_rejects_placeholder_string() {
    // PR #288 (#10): belt-and-suspenders — when TICKVAULT_MCP_PROFILE is
    // set but contains the literal `${...}` placeholder from .mcp.json,
    // treat it as unset. Otherwise auto-switch silently breaks on any
    // session where the placeholder leaks through.
    let src = read("scripts/claude-session-bootstrap.sh");
    assert!(
        src.contains("'${'*'}'"),
        "bootstrap must reject the literal ${{...}} placeholder string as an override"
    );
}

#[test]
fn bootstrap_has_auto_switch_logic() {
    // PR #288 (#10): bootstrap must auto-switch to a reachable profile when
    // the configured profile has <3 of 5 endpoints reachable. Operator
    // override (TICKVAULT_MCP_PROFILE) disables auto-switch.
    let src = read("scripts/claude-session-bootstrap.sh");
    assert!(
        src.contains("AUTO_SWITCH_MIN_REACHABLE"),
        "bootstrap must define the auto-switch threshold"
    );
    assert!(
        src.contains("AUTO_SWITCHED_FROM") || src.contains("auto-switched"),
        "bootstrap must record when auto-switch fired"
    );
    assert!(
        src.contains("OVERRIDE_PROFILE"),
        "bootstrap must honor TICKVAULT_MCP_PROFILE override (disables auto-switch)"
    );
}

#[test]
fn bootstrap_rejects_placeholder_profile_override() {
    // Same bug class as the MCP server: if the shell didn't expand
    // ${TICKVAULT_MCP_PROFILE}, the literal string leaks through and
    // auto-switch is incorrectly suppressed.
    let src = read("scripts/claude-session-bootstrap.sh");
    assert!(
        src.contains("'${'*'}'")
            || src.contains("${*}")
            || src.contains("case \"$OVERRIDE_PROFILE\""),
        "bootstrap must treat literal ${{...}} placeholder override as empty"
    );
}

#[test]
fn fuzz_workflow_supports_tunable_duration() {
    // PR #288 (#8): scheduled runs use 900s (15 min, 3x prior), manual
    // dispatch can override up to 3600s. Respects cost-budget per
    // aws-budget.md (~30 min/week stays well under free-tier 2000 min/mo).
    let wf = read(".github/workflows/fuzz.yml");
    assert!(
        wf.contains("fuzz_duration_secs"),
        "fuzz workflow must expose fuzz_duration_secs dispatch input"
    );
    assert!(
        wf.contains("'3600'") || wf.contains("\"3600\""),
        "fuzz workflow default must be 3600s (1 hour — Phase 12.3)"
    );
    assert!(
        !wf.contains("-max_total_time=300 "),
        "fuzz workflow should no longer hardcode 300s (5 min)"
    );
}

#[test]
fn subscription_planner_emits_per_id_collision_gauge() {
    // PR #288 (#5b): subscription_planner.rs must emit one gauge per
    // collision pair (labelled security_id + losing + winning) so the
    // operator can see WHICH ids collided in Grafana.
    let src = read("crates/core/src/instrument/subscription_planner.rs");
    assert!(
        src.contains("tv_instrument_registry_collision_pair"),
        "subscription_planner must emit tv_instrument_registry_collision_pair gauge"
    );
    assert!(
        src.contains("collision_pairs()"),
        "subscription_planner must call registry.collision_pairs()"
    );
}

#[test]
fn tick_processor_uses_segment_aware_sequence_tracker() {
    // PR #288 (#1 wiring): tick_processor must use DepthSequenceTracker
    // (segment-aware) instead of the old HashMap<(u32, u8), u32>
    // (not segment-aware, violates I-P1-11).
    let src = read("crates/core/src/pipeline/tick_processor.rs");
    assert!(
        src.contains("DepthSequenceTracker"),
        "tick_processor must use DepthSequenceTracker"
    );
    assert!(
        !src.contains("let mut deep_depth_seq_tracker: std::collections::HashMap<(u32, u8), u32>"),
        "old non-segment-aware HashMap must be removed"
    );
    assert!(
        !src.contains("tv_depth_20lvl_sequence_gaps_total"),
        "old single-metric name must be removed (replaced by 3 labelled metrics)"
    );
}

#[test]
fn chaos_nightly_workflow_wires_ignored_tests() {
    // PR #288 (#6/#7): the `#[ignore]`d chaos tests already exist
    // (4,723 lines across 16 files) but had no CI wiring. This workflow
    // spins up docker compose, runs `cargo test -- --ignored chaos_*`,
    // and opens a GitHub issue on failure. Weekly cadence respects the
    // GH Actions free-tier budget per .claude/rules/project/aws-budget.md.
    let wf = read(".github/workflows/chaos-nightly.yml");
    assert!(
        wf.contains("docker compose"),
        "chaos-nightly must bring up the docker compose stack"
    );
    assert!(
        wf.contains("--ignored"),
        "chaos-nightly must pass --ignored so the chaos tests actually run"
    );
    assert!(
        wf.contains("chaos_*") || wf.contains("chaos_"),
        "chaos-nightly must scope to the chaos_* test binaries"
    );
    assert!(
        wf.contains("chaos-regression"),
        "chaos-nightly must label regression issues as chaos-regression"
    );
    // Weekly cadence (cron '30 18 * * 6') — daily would blow the free-tier budget.
    assert!(
        wf.contains("* * 6") || wf.contains("* * * *"),
        "chaos-nightly must run on a schedule"
    );
}

#[test]
fn prometheus_alerts_include_depth_sequence_rules() {
    // PR #288 (#2): sustained + burst alerts for depth sequence holes.
    let rules = read("deploy/docker/prometheus/rules/tickvault-alerts.yml");
    assert!(
        rules.contains("DepthSequenceHolesSustained"),
        "tickvault-alerts.yml must include DepthSequenceHolesSustained"
    );
    assert!(
        rules.contains("DepthSequenceHolesBurst"),
        "tickvault-alerts.yml must include DepthSequenceHolesBurst"
    );
    assert!(
        rules.contains("tv_depth_sequence_holes_total"),
        "alerts must reference tv_depth_sequence_holes_total"
    );
}

#[test]
fn operator_dashboard_has_depth_sequence_and_collision_panels() {
    // PR #288 (#1): operator-health dashboard must have panels for the
    // new depth sequence metrics + the collision-pair table so they are
    // visible without grepping Prometheus.
    let dash = read("deploy/docker/grafana/dashboards/operator-health.json");
    assert!(
        dash.contains("tv_depth_sequence_holes_total"),
        "operator-health dashboard must visualize tv_depth_sequence_holes_total"
    );
    assert!(
        dash.contains("tv_instrument_registry_collision_pair"),
        "operator-health dashboard must visualize collision pairs"
    );
}

#[test]
fn chaos_valkey_kill_test_exists() {
    // PR #288 (#3): dedicated Valkey chaos test closes the one gap in the
    // existing 16-file chaos suite.
    let p = repo_root().join("crates/storage/tests/chaos_valkey_kill.rs");
    assert!(p.exists(), "chaos_valkey_kill.rs missing");
    let src = fs::read_to_string(&p).unwrap();
    assert!(
        src.contains("docker compose") && src.contains("pause"),
        "chaos_valkey_kill must shell out to docker compose pause"
    );
    assert!(
        src.contains("#[ignore"),
        "chaos_valkey_kill tests must be #[ignore]'d (weekly workflow)"
    );
}

#[test]
fn loom_depth_sequence_tracker_test_exists() {
    let p = repo_root().join("crates/core/tests/loom_depth_sequence_tracker.rs");
    assert!(p.exists(), "loom_depth_sequence_tracker.rs missing");
    let src = fs::read_to_string(&p).unwrap();
    assert!(
        src.contains("loom::model"),
        "loom test must use loom::model"
    );
    assert!(
        src.contains("concurrent") || src.contains("thread"),
        "loom test must exercise concurrent access"
    );
}

#[test]
fn dhat_depth_sequence_tracker_test_exists() {
    let p = repo_root().join("crates/core/tests/dhat_depth_sequence_tracker.rs");
    assert!(p.exists(), "dhat_depth_sequence_tracker.rs missing");
    let src = fs::read_to_string(&p).unwrap();
    assert!(
        src.contains("dhat::Profiler") || src.contains("dhat::Alloc"),
        "DHAT test must use dhat profiler"
    );
    assert!(
        src.contains("zero-alloc") || src.contains("zero alloc") || src.contains("bytes_delta, 0"),
        "DHAT test must assert zero allocation"
    );
}

#[test]
fn health_slash_command_exists_and_wires_all_layers() {
    // Every new or existing Claude Code session must be able to type
    // `/health` and get the full automation + observability snapshot
    // with zero manual setup. The command file is version-controlled
    // so it works identically on every clone.
    let cmd = read(".claude/commands/health.md");
    for required in [
        "validate-automation.sh",
        "doctor.sh",
        "100pct-audit.sh",
        "tail_errors",
        "summary_snapshot",
        "list_novel_signatures",
        "prometheus_query",
        "questdb_sql",
        "list_active_alerts",
        "session-env",
    ] {
        assert!(
            cmd.contains(required),
            "/health command must invoke {required} to be a complete \
             automation snapshot"
        );
    }
}

// -----------------------------------------------------------------------
// PR #384 — auto-up Docker on local profile when all 4 services OFFLINE.
// Closes "Honest gap #1": operator should not have to manually run
// `make docker-up` after every laptop reboot. Strict guards keep this
// safe (local-only, all-OFFLINE-only, CI-skipped, opt-out via env var,
// fire-and-forget). The 5 ratchets below pin every safety guard so a
// future regression cannot silently fire `docker compose up -d` from a
// CI runner or an AWS profile.
// -----------------------------------------------------------------------

#[test]
fn bootstrap_auto_up_only_fires_on_local_profile() {
    let src = read("scripts/claude-session-bootstrap.sh");
    assert!(
        src.contains(r#"PROFILE" = "local""#),
        "auto-up must guard on PROFILE=local — never touch AWS or mac-dev"
    );
}

#[test]
fn bootstrap_auto_up_skipped_in_ci() {
    let src = read("scripts/claude-session-bootstrap.sh");
    for env_var in ["${CI:-}", "${GITHUB_ACTIONS:-}", "${GITLAB_CI:-}"] {
        assert!(
            src.contains(env_var),
            "auto-up must skip when {env_var} is set — CI runners must \
             never spin up Docker via the SessionStart hook"
        );
    }
}

#[test]
fn bootstrap_auto_up_supports_operator_opt_out() {
    let src = read("scripts/claude-session-bootstrap.sh");
    assert!(
        src.contains("TICKVAULT_NO_AUTO_UP"),
        "auto-up must honor TICKVAULT_NO_AUTO_UP=1 escape hatch — operator \
         debugging a partially-up stack must be able to disable auto-start"
    );
}

#[test]
fn bootstrap_auto_up_requires_every_service_offline() {
    let src = read("scripts/claude-session-bootstrap.sh");
    // A partially-up stack means the operator is mid-debug — never
    // disturb. Auto-up only fires when EVERY probed local service is
    // OFFLINE (a fresh laptop boot scenario).
    for status_var in ["PROM_S", "QDB_S", "GRAF_S", "API_S"] {
        assert!(
            src.contains(&format!(r#""${status_var}" = "OFFLINE""#)),
            "auto-up must require {status_var}=OFFLINE — partial-up state \
             must NOT trigger docker compose up"
        );
    }
}

#[test]
fn bootstrap_auto_up_is_fire_and_forget() {
    let src = read("scripts/claude-session-bootstrap.sh");
    assert!(
        src.contains("nohup docker compose"),
        "auto-up must use nohup so SessionStart never blocks on Docker"
    );
    assert!(
        src.contains("disown"),
        "auto-up must disown the background docker compose process"
    );
    assert!(
        src.contains("deploy/docker/docker-compose.yml"),
        "auto-up must reference the canonical compose file path"
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

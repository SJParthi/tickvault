//! Meta-guard for Milestones 3 + 4 of autonomous-operations-100pct.md.
//!
//! Enforces:
//!   1. Every `auto-fix-*.sh` script has a matching `*-rollback.sh`.
//!   2. Every rollback script accepts `<correlation_id> [--dry-run]`.
//!   3. Every `auto_fix_script` referenced by the triage YAML points at
//!      a file that actually exists AND is executable.
//!   4. M4 harness scripts `scripts/triage/verify.sh` and `rollback.sh`
//!      exist, are executable, and satisfy the contract (usage line,
//!      exit-code comments).

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let Ok(meta) = std::fs::metadata(path) else {
            return false;
        };
        // Owner execute bit
        meta.permissions().mode() & 0o100 != 0
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn list_autofix_scripts(scripts_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(scripts_dir) else {
        return out;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name();
        let Some(n) = name.to_str() else { continue };
        if n.starts_with("auto-fix-") && n.ends_with(".sh") && !n.ends_with("-rollback.sh") {
            out.push(entry.path());
        }
    }
    out.sort();
    out
}

#[test]
fn every_auto_fix_script_has_matching_rollback() {
    let root = repo_root();
    let scripts = root.join("scripts");
    let fixes = list_autofix_scripts(&scripts);
    assert!(
        !fixes.is_empty(),
        "expected at least one auto-fix-*.sh in scripts/"
    );
    let mut missing: Vec<String> = Vec::new();
    for fix in &fixes {
        let stem = fix
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".sh"))
            .unwrap_or("");
        let rollback_name = format!("{stem}-rollback.sh");
        let rollback_path = scripts.join(&rollback_name);
        if !rollback_path.is_file() {
            missing.push(format!(
                "{} has no rollback (expected {})",
                fix.display(),
                rollback_path.display()
            ));
        } else if !is_executable(&rollback_path) {
            missing.push(format!(
                "{} exists but is not executable",
                rollback_path.display()
            ));
        }
    }
    assert!(
        missing.is_empty(),
        "Auto-fix scripts missing rollback pairs:\n  {}\n\n\
         Every auto-fix-*.sh MUST have a matching *-rollback.sh so M4's \
         verify+rollback loop can revert the fix if it fails verification.",
        missing.join("\n  ")
    );
}

#[test]
fn every_rollback_script_accepts_correlation_id() {
    let root = repo_root();
    let scripts = root.join("scripts");
    let mut violations: Vec<String> = Vec::new();
    let Ok(entries) = std::fs::read_dir(&scripts) else {
        panic!("scripts/ dir missing");
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !name.ends_with("-rollback.sh") || !name.starts_with("auto-fix-") {
            continue;
        }
        let path = entry.path();
        let Ok(src) = std::fs::read_to_string(&path) else {
            violations.push(format!("{} unreadable", path.display()));
            continue;
        };
        // Contract: must include the exact usage line
        if !src.contains("usage: $0 <correlation_id> [--dry-run]") {
            violations.push(format!(
                "{} missing `usage: $0 <correlation_id> [--dry-run]` line",
                path.display()
            ));
        }
        // Contract: must validate $# >= 1 (correlation_id required)
        if !src.contains("$# -lt 1") {
            violations.push(format!(
                "{} missing arg count check ($# -lt 1)",
                path.display()
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "Rollback script contract violations:\n  {}",
        violations.join("\n  ")
    );
}

#[test]
fn verify_harness_script_exists_and_is_executable() {
    let path = repo_root().join("scripts/triage/verify.sh");
    assert!(path.is_file(), "scripts/triage/verify.sh missing");
    assert!(
        is_executable(&path),
        "scripts/triage/verify.sh not executable"
    );
    let src = std::fs::read_to_string(&path).unwrap();
    // Contract: usage signature
    assert!(
        src.contains("usage: $0 <correlation_id> <metric_predicate>"),
        "verify.sh usage signature regressed"
    );
    // Contract: reads the /metrics exporter endpoint (the Prometheus
    // PromQL path was retired in #O5 2026-05-30 — Prometheus container
    // removed in #O3; verify.sh now scrapes the app's /metrics exporter).
    assert!(
        src.contains("TICKVAULT_METRICS_URL"),
        "verify.sh must read TICKVAULT_METRICS_URL env var"
    );
    // Contract: 3 exit codes documented
    for code in &["exit 0", "exit 1", "exit 2"] {
        assert!(
            src.contains(code),
            "verify.sh missing explicit `{code}` path"
        );
    }
}

#[test]
fn rollback_harness_dispatcher_exists_and_is_executable() {
    let path = repo_root().join("scripts/triage/rollback.sh");
    assert!(path.is_file(), "scripts/triage/rollback.sh missing");
    assert!(
        is_executable(&path),
        "scripts/triage/rollback.sh not executable"
    );
    let src = std::fs::read_to_string(&path).unwrap();
    assert!(
        src.contains("usage: $0 <fix_name> <correlation_id>"),
        "rollback.sh dispatcher usage signature regressed"
    );
    // Contract: looks up scripts/<fix_name>-rollback.sh
    assert!(
        src.contains("-rollback.sh"),
        "rollback.sh dispatcher must look up *-rollback.sh"
    );
}

#[test]
fn triage_yaml_auto_fix_targets_all_exist_and_executable() {
    // Re-validate the same invariant as
    // triage_rules_full_coverage_guard::auto_fix_actions_must_reference_an_existing_script
    // but with a stricter "must be executable" check. Prevents someone
    // from adding a script without chmod +x.
    let root = repo_root();
    let yaml_path = root.join(".claude/triage/error-rules.yaml");
    let yaml = std::fs::read_to_string(&yaml_path).expect("triage yaml present");
    let mut errors: Vec<String> = Vec::new();
    for line in yaml.lines() {
        let Some(rest) = line.trim_start().strip_prefix("auto_fix_script: ") else {
            continue;
        };
        let script_rel = rest.trim();
        let script_path = root.join(script_rel);
        if !script_path.is_file() {
            errors.push(format!("{script_rel} referenced but missing"));
            continue;
        }
        if !is_executable(&script_path) {
            errors.push(format!("{script_rel} exists but is not executable"));
        }
    }
    assert!(
        errors.is_empty(),
        "Triage YAML auto_fix_script errors:\n  {}",
        errors.join("\n  ")
    );
}

#[test]
fn auto_fix_scripts_accept_dry_run_flag() {
    // Every auto-fix-*.sh MUST support a `--dry-run` mode so Claude
    // can preview the action before executing. Enforced by scanning
    // the source for the conventional flag handling.
    let root = repo_root();
    let scripts = root.join("scripts");
    let mut missing: Vec<String> = Vec::new();
    for fix in list_autofix_scripts(&scripts) {
        let src = std::fs::read_to_string(&fix).unwrap();
        if !src.contains("--dry-run") {
            missing.push(format!("{} does not handle --dry-run", fix.display()));
        }
        // Soft contract: a dry-run-only branch should exit 0
        if !src.contains("dry_run_ok") && !src.contains("DRY_RUN=0") {
            missing.push(format!(
                "{} doesn't appear to have a DRY_RUN=0 var or dry_run_ok log marker",
                fix.display()
            ));
        }
    }
    assert!(
        missing.is_empty(),
        "Auto-fix --dry-run contract violations:\n  {}",
        missing.join("\n  ")
    );
}

fn read_loop_runbook() -> String {
    let path = repo_root().join(".claude/triage/claude-loop-prompt.md");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("loop runbook missing at {}", path.display()))
}

#[test]
fn loop_runbook_wires_verify_and_rollback() {
    // M4's fix->verify->rollback contract must be documented in the runbook
    // that drives remediation. Before 2026-06-14 the runbook ran auto-fix and
    // never referenced verify.sh / rollback.sh — the loop was unwired.
    let runbook = read_loop_runbook();
    assert!(
        runbook.contains("scripts/triage/verify.sh"),
        "loop runbook must reference scripts/triage/verify.sh (the M4 verify step)"
    );
    assert!(
        runbook.contains("scripts/triage/rollback.sh"),
        "loop runbook must reference scripts/triage/rollback.sh (the M4 rollback step)"
    );
}

#[test]
fn loop_runbook_states_escalate_only_invariant() {
    // The operator's M3 lock (Parthiban 2026-06-10, PR #1087) keeps triage
    // auto-execution escalate-only. The runbook MUST state this so a future
    // edit can't silently re-introduce live auto-execution of fix scripts.
    let runbook = read_loop_runbook();
    assert!(
        runbook.to_lowercase().contains("escalate-only"),
        "loop runbook must state the escalate-only lock (M3 operator decision)"
    );
}

#[test]
fn loop_runbook_has_no_retired_rebuild_endpoint() {
    // POST /api/instruments/rebuild was retired in PR #6b; the API is
    // read-only. The runbook must not present it as a live remediation path.
    let runbook = read_loop_runbook();
    let retired = "POST /api/instruments/rebuild";
    // Allowed only inside an explicit "retired" sentence; reject any other use.
    for (i, line) in runbook.lines().enumerate() {
        if line.contains(retired) {
            assert!(
                line.to_lowercase().contains("retired"),
                "loop runbook line {} references the retired endpoint without \
                 marking it retired: {line:?}",
                i + 1
            );
        }
    }
}

#[test]
fn rollback_dispatcher_self_reference_names_real_guard() {
    // rollback.sh's header comment must point at the guard file that actually
    // exists (this file), not the stale autonomous_ops_m4_guard.rs name.
    let path = repo_root().join("scripts/triage/rollback.sh");
    let src = std::fs::read_to_string(&path).unwrap();
    assert!(
        src.contains("autonomous_ops_m3_m4_guard.rs"),
        "rollback.sh must reference the real guard file autonomous_ops_m3_m4_guard.rs"
    );
    // The real name is `autonomous_ops_m3_m4_guard.rs`, which does NOT contain
    // the substring `autonomous_ops_m4_guard.rs`, so this cleanly rejects the
    // stale reference without a false positive on the correct name.
    assert!(
        !src.contains("autonomous_ops_m4_guard.rs"),
        "rollback.sh must not reference the non-existent autonomous_ops_m4_guard.rs"
    );
}

#[test]
fn auto_fix_scripts_emit_correlation_id_in_logs() {
    // Every auto-fix-*.sh MUST emit a `corr_id=...` token in its
    // audit log output so M4's verify+rollback loop can correlate.
    let root = repo_root();
    let scripts = root.join("scripts");
    let mut missing: Vec<String> = Vec::new();
    for fix in list_autofix_scripts(&scripts) {
        let src = std::fs::read_to_string(&fix).unwrap();
        let fname = fix.file_name().unwrap().to_string_lossy().into_owned();
        // Two old pre-M3 scripts (clear-spill, refresh-instruments)
        // pre-date the correlation_id contract. Skip them for this test —
        // they're exempt until the next refactor. (restart-depth was
        // retired 2026-06-10: depth-20/200 is deleted forever per
        // websocket-connection-scope-lock.md, so its endpoint can never
        // exist and the script was unreferenced by any triage rule.)
        const PRE_M3_EXEMPT: &[&str] =
            &["auto-fix-clear-spill.sh", "auto-fix-refresh-instruments.sh"];
        if PRE_M3_EXEMPT.contains(&fname.as_str()) {
            continue;
        }
        if !src.contains("CORR_ID=") || !src.contains("corr_id=") {
            missing.push(format!(
                "{fname} missing CORR_ID variable or corr_id log marker"
            ));
        }
    }
    assert!(
        missing.is_empty(),
        "Auto-fix correlation-id contract violations (M3+):\n  {}",
        missing.join("\n  ")
    );
}

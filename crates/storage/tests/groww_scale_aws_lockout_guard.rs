//! AWS scale-lockout guard — **operator lock 2026-07-06**.
//!
//! Operator verbatim (2026-07-06): "wipe off this entire dynamic scaling
//! connections of 86 connections and 86k instruments... stick to our one
//! active dhan and one active groww connections and its subscriptions
//! alone" + "NOWHERE these dynamic scalable changes should ever go to AWS".
//!
//! This is a mechanical, build-failing tripwire: because `All Green`
//! (the ci.yml fan-in) is a GitHub-enforced required check on `main`,
//! any future PR that tries to route the Groww dynamic-scaling
//! experiment (the 86-connection fleet / scale lab) toward AWS fails CI
//! and the merge button is physically blocked. Specifically it fails if:
//!
//!  1. Any `config/*.toml` sets `enabled = true` inside a
//!     `[feeds.groww.scale]` section (the deploy workflow refreshes the
//!     repo clone on the AWS box and copies `config/` — a flipped flag
//!     in ANY tracked config IS the AWS activation path).
//!  2. The `GrowwScaleConfig` serde/Default `enabled` default drifts
//!     away from `false` (a true default would activate the fleet on
//!     every boot with zero config change).
//!  3. The scale-lab probe marker `deploy/local/probe-once.date` or the
//!     scale-lab autopilot script `scripts/local-autopilot.sh` (both
//!     live ONLY on the `local-runtime` branch) ever land on `main`.
//!  4. Anything under `deploy/` or the AWS deploy workflow sets the
//!     scale flag (`GROWW_SCALE_ENABLED=true`, `scale.enabled = true`,
//!     or a `[feeds.groww.scale]` section with `enabled = true`).
//!  5. The authoritative rule file
//!     `.claude/rules/project/groww-scale-aws-lockout-2026-07-06.md`
//!     disappears or stops carrying the dated operator quote.
//!
//! See:
//! - `.claude/rules/project/groww-scale-aws-lockout-2026-07-06.md`
//! - `.claude/rules/project/groww-second-feed-scope-2026-06-19.md` §34
//! - `.claude/rules/project/groww-scale-error-codes.md`
//! - `.claude/rules/project/merge-gate-lock-2026-07-04.md` (All Green)

#![cfg(test)]

use std::path::{Path, PathBuf};

const LOCK_MSG: &str = "operator lock 2026-07-06 — Groww scale must never reach AWS; \
     see .claude/rules/project/groww-scale-aws-lockout-2026-07-06.md";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/storage parent")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

/// Strip a trailing `#`-comment from a TOML line (good enough for the
/// simple `key = value` lines this guard inspects — none of the keys we
/// check carry `#` inside a string value).
fn strip_toml_comment(line: &str) -> &str {
    match line.find('#') {
        Some(idx) => &line[..idx],
        None => line,
    }
}

/// Returns `true` if `body` (TOML text) sets `enabled = true` inside a
/// `[feeds.groww.scale]` section. Tracks the current section header
/// line-by-line; ignores comments and other sections.
fn toml_enables_groww_scale(body: &str) -> bool {
    let mut in_scale_section = false;
    for raw_line in body.lines() {
        let line = strip_toml_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            in_scale_section = line == "[feeds.groww.scale]";
            continue;
        }
        if !in_scale_section {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            if key.trim() == "enabled" && value.trim() == "true" {
                return true;
            }
        }
    }
    false
}

/// Recursively collect every file under `dir`.
fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {} failed: {e}", dir.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| panic!("dir entry under {}: {e}", dir.display()));
        let path = entry.path();
        if path.is_dir() {
            walk_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// Section 1 — no tracked config file may flip the Groww scale flag on.
/// The AWS deploy workflow copies `config/` from the repo clone to the
/// box, so a `true` here IS the AWS activation vector.
#[test]
fn no_config_file_enables_groww_scale() {
    let config_dir = repo_root().join("config");
    let mut files = Vec::new();
    walk_files(&config_dir, &mut files);
    let mut saw_scale_section = false;
    for path in files {
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let body = read(&path);
        if body.contains("[feeds.groww.scale]") {
            saw_scale_section = true;
        }
        assert!(
            !toml_enables_groww_scale(&body),
            "{} sets enabled = true inside [feeds.groww.scale] — {LOCK_MSG}",
            path.display()
        );
    }
    // Sanity: the section exists (in config/base.toml with enabled = false)
    // so this guard is scanning the real surface, not passing vacuously.
    assert!(
        saw_scale_section,
        "no config/*.toml carries a [feeds.groww.scale] section — this guard's \
         scan surface changed; re-verify the scale config location. {LOCK_MSG}"
    );
}

/// Section 2 — the compiled-in default for `GrowwScaleConfig.enabled`
/// must stay `false`: the field must carry the BARE `#[serde(default)]`
/// (bool::default() == false, never a `default = "fn"` that could return
/// true), and the hand-written `Default` impl must set `enabled: false`.
#[test]
fn groww_scale_config_default_is_off() {
    let body = read(&repo_root().join("crates/common/src/config.rs"));

    // (a) The struct block: locate `pub struct GrowwScaleConfig` and the
    // `enabled` field inside it; the attribute immediately above the field
    // must be the bare `#[serde(default)]`.
    let struct_start = body
        .find("pub struct GrowwScaleConfig")
        .unwrap_or_else(|| panic!("GrowwScaleConfig struct missing from config.rs — {LOCK_MSG}"));
    let struct_block = &body[struct_start..];
    let struct_block = &struct_block[..struct_block
        .find("\n}")
        .unwrap_or_else(|| panic!("GrowwScaleConfig struct block unterminated — {LOCK_MSG}"))];
    let enabled_pos = struct_block
        .find("pub enabled: bool")
        .unwrap_or_else(|| panic!("GrowwScaleConfig.enabled field missing — {LOCK_MSG}"));
    let before_enabled = &struct_block[..enabled_pos];
    let last_attr_line = before_enabled
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| l.starts_with("#["))
        .unwrap_or_else(|| panic!("GrowwScaleConfig.enabled has no serde attribute — {LOCK_MSG}"));
    assert_eq!(
        last_attr_line, "#[serde(default)]",
        "GrowwScaleConfig.enabled must use the BARE #[serde(default)] (bool -> false); \
         found `{last_attr_line}` — {LOCK_MSG}"
    );

    // (b) The Default impl: `enabled: false` and never `enabled: true`.
    let default_start = body
        .find("impl Default for GrowwScaleConfig")
        .unwrap_or_else(|| panic!("Default impl for GrowwScaleConfig missing — {LOCK_MSG}"));
    let default_block = &body[default_start..];
    let default_block = &default_block[..default_block
        .find("\n}")
        .unwrap_or_else(|| panic!("GrowwScaleConfig Default impl unterminated — {LOCK_MSG}"))];
    assert!(
        default_block.contains("enabled: false"),
        "GrowwScaleConfig::default() must set enabled: false — {LOCK_MSG}"
    );
    assert!(
        !default_block.contains("enabled: true"),
        "GrowwScaleConfig::default() sets enabled: true — {LOCK_MSG}"
    );
}

/// Section 3 — the scale-lab probe marker lives ONLY on the
/// `local-runtime` branch. Its appearance on `main` means scale-lab
/// state is leaking into the branch that deploys to AWS.
#[test]
fn probe_once_marker_absent_from_main() {
    let marker = repo_root().join("deploy/local/probe-once.date");
    assert!(
        !marker.exists(),
        "{} exists on main (scale-lab probe marker) — {LOCK_MSG}",
        marker.display()
    );
}

/// Section 3b — the scale-lab autopilot script (SCALE_WINDOW_START/END
/// harness) lives ONLY on the `local-runtime` branch; `deploy/local/` as
/// a whole must not appear on main.
#[test]
fn scale_lab_scripts_absent_from_main() {
    let root = repo_root();
    for rel in ["scripts/local-autopilot.sh", "deploy/local"] {
        let path = root.join(rel);
        assert!(
            !path.exists(),
            "{} exists on main (scale-lab artifact) — {LOCK_MSG}",
            path.display()
        );
    }
}

/// Section 4 — nothing under `deploy/` (terraform, docker, systemd,
/// grafana-cloud) nor the AWS deploy workflow may set the Groww scale
/// flag. This closes the "env var / provisioning script flips the flag
/// on the box" vector.
#[test]
fn deploy_path_never_sets_scale_flag() {
    let root = repo_root();
    let mut files = Vec::new();
    walk_files(&root.join("deploy"), &mut files);
    files.push(root.join(".github/workflows/deploy-aws.yml"));
    files.push(root.join(".github/workflows/deploy-aws-after-close.yml"));

    for path in files {
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue; // binary / non-UTF-8 file — cannot carry a TOML/env flag textually
        };
        let lower = body.to_lowercase();
        for banned in [
            "groww_scale_enabled=true",
            "groww_scale_enabled = true",
            "feeds.groww.scale.enabled=true",
            "feeds.groww.scale.enabled = true",
            "scale.enabled=true",
            "scale.enabled = true",
        ] {
            assert!(
                !lower.contains(banned),
                "{} contains `{banned}` — {LOCK_MSG}",
                path.display()
            );
        }
        if body.contains("[feeds.groww.scale]") {
            assert!(
                !toml_enables_groww_scale(&body),
                "{} carries a [feeds.groww.scale] section with enabled = true — {LOCK_MSG}",
                path.display()
            );
        }
    }
}

/// Section 5 — the authoritative rule file must exist and carry the
/// dated verbatim operator quote, so future sessions find the lock from
/// this guard and vice versa.
#[test]
fn lockout_rule_file_exists_and_pins_the_operator_quote() {
    let path = repo_root().join(".claude/rules/project/groww-scale-aws-lockout-2026-07-06.md");
    assert!(
        path.exists(),
        "authoritative rule file missing at {} — {LOCK_MSG}",
        path.display()
    );
    let body = read(&path);
    for required in [
        "2026-07-06",
        "wipe off this entire dynamic scaling connections",
        "NOWHERE these dynamic scalable changes should ever go to AWS",
        "groww_scale_aws_lockout_guard",
    ] {
        assert!(
            body.contains(required),
            "rule file lost required phrase `{required}` — {LOCK_MSG}"
        );
    }
}

/// Self-test — the TOML section scanner actually detects a flipped flag
/// (guards this guard against a vacuous-pass regression of its parser).
#[test]
fn toml_scanner_detects_flipped_flag() {
    let flipped = "[feeds]\ndhan_enabled = true\n\n[feeds.groww.scale]\nenabled = true\n";
    assert!(
        toml_enables_groww_scale(flipped),
        "scanner must detect enabled = true"
    );
    let off = "[feeds.groww.scale]\nenabled = false\n# enabled = true (comment)\n";
    assert!(
        !toml_enables_groww_scale(off),
        "scanner must not false-positive"
    );
    let other_section = "[feeds]\nenabled = true\n[feeds.groww.scale]\nenabled = false\n";
    assert!(
        !toml_enables_groww_scale(other_section),
        "scanner must scope to the [feeds.groww.scale] section only"
    );
}

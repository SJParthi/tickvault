//! The ONLY shell the 2026-10-22 rebuild sends to the box (plan item 66).
//!
//! One SSM `AWS-RunShellScript` element per box phase: fetch the deployed
//! binary of the commit the workflow runs on, then run its Rust
//! `rebuild-box` subcommand, which does all the work. Every value spliced in
//! is validated first (a 40-hex commit id, a fixed phase and mode word, a
//! manifest key of the one shape `manifest_key` produces), so nothing a
//! workflow input carries can reach the shell.
//!
//! Budgeted by `crates/common/tests/shell_budget_guard.rs`
//! (`REBUILD_SSM_SHELL_SOURCES`, measured exactly) under the dated (approved 2026-10-22)
//! section §0.13 of `docs/claude-rules-full/project/rust-only-forever-lock-2026-07-19.md`,
//! which the owner approved on 2026-10-22. This file holds no other
//! string literal in its production half, so the budget measures the shell
//! and nothing else.

use crate::rebuild_2026_10_22::{Mode, Phase, is_commit_sha, is_manifest_key};

/// The one element. `{SHA}`, `{PHASE}`, `{MODE}` and `{MANIFEST}` are
/// replaced with validated values.
const BOX_PHASE_COMMAND: &str = "set -eu; aws s3 cp --only-show-errors --region ap-south-1 s3://tv-prod-cold/deploys/{SHA}/tickvault /var/tmp/tv-rebuild-tickvault; chmod 0755 /var/tmp/tv-rebuild-tickvault; exec /var/tmp/tv-rebuild-tickvault rebuild-box {PHASE} {MODE} {SHA} {MANIFEST}";

/// Stands in for "no manifest" (the copy phase writes one; the others read it).
const NO_MANIFEST: &str = "-";

/// Why a command could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandRefused {
    /// The commit id is not 40 lowercase hex digits.
    BadSha,
    /// The phase does not run on the box.
    NotABoxPhase,
    /// The manifest key is not one `manifest_key` produces.
    BadManifest,
}

/// The SSM `commands` list for one box phase.
///
/// # Errors
/// [`CommandRefused`] when any spliced value fails validation.
pub fn box_phase_commands(
    sha: &str,
    phase: Phase,
    mode: Mode,
    manifest: Option<&str>,
) -> Result<Vec<String>, CommandRefused> {
    if !is_commit_sha(sha) {
        return Err(CommandRefused::BadSha);
    }
    if !phase.runs_on_box() {
        return Err(CommandRefused::NotABoxPhase);
    }
    let manifest = match manifest {
        None => NO_MANIFEST,
        Some(m) if is_manifest_key(m) => m,
        Some(_) => return Err(CommandRefused::BadManifest),
    };
    Ok(vec![
        BOX_PHASE_COMMAND
            .replace("{SHA}", sha)
            .replace("{PHASE}", phase.as_str())
            .replace("{MODE}", mode.as_str())
            .replace("{MANIFEST}", manifest),
    ])
}

/// The manifest argument as the box subcommand receives it.
#[must_use]
pub fn manifest_arg(arg: &str) -> Option<&str> {
    (arg != NO_MANIFEST).then_some(arg)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "f92a6472f0000000000000000000000000000000";

    #[test]
    fn test_box_phase_commands_splices_only_validated_values() {
        let cmds = box_phase_commands(SHA, Phase::Copy, Mode::Dry, None).expect("builds");
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            cmds[0],
            format!(
                "set -eu; aws s3 cp --only-show-errors --region ap-south-1 \
                 s3://tv-prod-cold/deploys/{SHA}/tickvault /var/tmp/tv-rebuild-tickvault; \
                 chmod 0755 /var/tmp/tv-rebuild-tickvault; exec /var/tmp/tv-rebuild-tickvault \
                 rebuild-box copy dry {SHA} -"
            )
        );
        let key = "rebuild-2026-10-22/manifests/copy-20261022T133500Z.json";
        let cmds =
            box_phase_commands(SHA, Phase::PrepareNew, Mode::Apply, Some(key)).expect("builds");
        assert!(cmds[0].ends_with(&format!("rebuild-box prepare-new apply {SHA} {key}")));
    }

    #[test]
    fn test_box_phase_commands_refuses_bad_input() {
        assert_eq!(
            box_phase_commands("abc", Phase::Copy, Mode::Dry, None),
            Err(CommandRefused::BadSha)
        );
        assert_eq!(
            box_phase_commands(SHA, Phase::Swap, Mode::Apply, None),
            Err(CommandRefused::NotABoxPhase)
        );
        assert_eq!(
            box_phase_commands(SHA, Phase::PrepareNew, Mode::Apply, Some("x; reboot")),
            Err(CommandRefused::BadManifest)
        );
    }

    #[test]
    fn test_manifest_arg_maps_the_placeholder_to_none() {
        assert_eq!(manifest_arg("-"), None);
        assert_eq!(manifest_arg("k"), Some("k"));
    }
}

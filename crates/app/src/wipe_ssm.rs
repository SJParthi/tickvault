//! The ONLY shell the 2026-10-31 wipe sends to the box (plan item 67,
//! operator Quote 35).
//!
//! One SSM `AWS-RunShellScript` element: fetch the deployed binary of the
//! commit the workflow runs on, then run its Rust `wipe-box` subcommand,
//! which does all the work. The two values spliced in are validated first
//! (a 40-hex commit id, a fixed mode word), so nothing a workflow input
//! carries can reach the shell.
//!
//! Budgeted by `crates/common/tests/shell_budget_guard.rs`
//! (`WIPE_SSM_SHELL_SOURCES`, measured exactly) under the dated section §0.14
//! of `docs/claude-rules-full/project/rust-only-forever-lock-2026-07-19.md`
//! (approved by the owner 2026-10-22; the workflow may run only inside the
//! tool's 31 Oct night window). This file holds no other string literal in its production
//! half, so the budget measures the shell and nothing else.

use crate::wipe_2026_10_31::{Mode, is_commit_sha};

/// The one element. `{SHA}` and `{MODE}` are replaced with validated values.
const WIPE_BOX_COMMAND: &str = "set -eu; aws s3 cp --only-show-errors --region ap-south-1 s3://tv-prod-cold/deploys/{SHA}/tickvault /var/tmp/tv-wipe-tickvault; chmod 0755 /var/tmp/tv-wipe-tickvault; exec /var/tmp/tv-wipe-tickvault wipe-box {MODE} {SHA}";

/// The SSM `commands` list for the box phase, or `None` when the commit id
/// is not 40 lowercase hex digits.
#[must_use]
pub fn wipe_box_commands(sha: &str, mode: Mode) -> Option<Vec<String>> {
    if !is_commit_sha(sha) {
        return None;
    }
    Some(vec![
        WIPE_BOX_COMMAND
            .replace("{SHA}", sha)
            .replace("{MODE}", mode.as_str()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "d1b4835470000000000000000000000000000000";

    #[test]
    fn test_wipe_box_commands_splices_only_validated_values() {
        let cmds = wipe_box_commands(SHA, Mode::Dry).expect("builds");
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            cmds[0],
            format!(
                "set -eu; aws s3 cp --only-show-errors --region ap-south-1 \
                 s3://tv-prod-cold/deploys/{SHA}/tickvault /var/tmp/tv-wipe-tickvault; \
                 chmod 0755 /var/tmp/tv-wipe-tickvault; exec /var/tmp/tv-wipe-tickvault \
                 wipe-box dry {SHA}"
            )
        );
        let apply = wipe_box_commands(SHA, Mode::Apply).expect("builds");
        assert!(apply[0].ends_with(&format!("wipe-box apply {SHA}")));
        assert_eq!(wipe_box_commands("abc; reboot", Mode::Dry), None);
        assert_eq!(wipe_box_commands(&SHA.to_uppercase(), Mode::Dry), None);
    }
}

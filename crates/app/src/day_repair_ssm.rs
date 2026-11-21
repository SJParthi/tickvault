//! The ONLY shell the 2026-11-20 day repair sends to the box (daily-universe
//! Quote 63, plan item UTC-REPAIR).
//!
//! One SSM `AWS-RunShellScript` element: fetch the deployed binary of the commit the
//! workflow runs on, then run its Rust `day-repair` subcommand, which does
//! all the work. The two values spliced in are validated first (a 40-hex
//! commit id, a fixed mode word), so nothing a workflow input carries can
//! reach the shell.
//!
//! Budgeted by `crates/common/tests/shell_budget_guard.rs`
//! (`DAY_REPAIR_SSM_SHELL_SOURCES`, measured exactly) under the dated section
//! §0.21 of `docs/claude-rules-full/project/rust-only-forever-lock-2026-07-19.md`
//! (approved by the owner 2026-11-21). Deleted with the runner and the
//! workflow by the first PR after a passed verify. This file holds no other
//! string literal in its production half, so the budget measures the shell
//! and nothing else.

use crate::day_repair::Mode;

/// The one element. `{SHA}` and `{MODE}` are replaced with validated values.
const DAY_REPAIR_COMMAND: &str = "set -eu; aws s3 cp --only-show-errors --region ap-south-1 s3://tv-prod-cold/deploys/{SHA}/tickvault /var/tmp/tv-day-repair-tickvault; chmod 0755 /var/tmp/tv-day-repair-tickvault; exec /var/tmp/tv-day-repair-tickvault day-repair {MODE} {SHA}";

/// The SSM `commands` list, or `None` when the commit id is not 40
/// lowercase hex digits.
#[must_use]
pub fn day_repair_commands(sha: &str, mode: Mode) -> Option<Vec<String>> {
    let valid = sha.len() == 40
        && sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !valid {
        return None;
    }
    Some(vec![
        DAY_REPAIR_COMMAND
            .replace("{SHA}", sha)
            .replace("{MODE}", mode.as_str()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "d1b4835470000000000000000000000000000000";

    #[test]
    fn test_day_repair_commands_splices_only_validated_values() {
        let cmds = day_repair_commands(SHA, Mode::Dry).expect("builds");
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            cmds[0],
            format!(
                "set -eu; aws s3 cp --only-show-errors --region ap-south-1 \
                 s3://tv-prod-cold/deploys/{SHA}/tickvault /var/tmp/tv-day-repair-tickvault; \
                 chmod 0755 /var/tmp/tv-day-repair-tickvault; exec \
                 /var/tmp/tv-day-repair-tickvault day-repair dry {SHA}"
            )
        );
        let apply = day_repair_commands(SHA, Mode::Apply).expect("builds");
        assert!(apply[0].ends_with(&format!("day-repair apply {SHA}")));
        let verify = day_repair_commands(SHA, Mode::Verify).expect("builds");
        assert!(verify[0].ends_with(&format!("day-repair verify {SHA}")));
        assert_eq!(day_repair_commands("abc; reboot", Mode::Dry), None);
        assert_eq!(day_repair_commands(&SHA.to_uppercase(), Mode::Dry), None);
        assert_eq!(day_repair_commands("", Mode::Dry), None);
    }

    #[test]
    fn test_day_repair_commands_parse_back_through_the_box_parser() {
        for mode in [Mode::Dry, Mode::Apply, Mode::Verify] {
            let cmd = day_repair_commands(SHA, mode).expect("builds").remove(0);
            let tail = cmd.split("exec ").nth(1).expect("exec");
            let args: Vec<String> = tail.split(' ').map(str::to_owned).collect();
            assert_eq!(
                crate::day_repair::parse_args(&args),
                Ok((mode, SHA.to_owned()))
            );
        }
    }
}

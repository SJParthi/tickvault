//! A calendar Muhurat date is a capture day for every workflow guard (owner
//! 2026-10-30: "go ahead and fix the muhurat risk dude").
//!
//! Every workflow that refuses to deploy, apply terraform, stop, downsize,
//! recover or wipe "during market hours" checked Monday to Friday only, so on
//! a Sunday Muhurat session (2026-11-08) a merge would deploy and restart the
//! app mid-session, and a failed deploy would stop the box. Each guard now
//! also treats the WHOLE IST date as closed when `config/base.toml` lists it
//! under `[[trading.muhurat_trading_dates]]` (the one source of the Muhurat
//! session). This ratchet fails the build when a weekday check loses that arm
//! or a guard job loses the file it reads.

use std::path::PathBuf;

/// The exact shell the guards run: true when today's IST date is listed.
const MUHURAT_SNIPPET: &str = "grep -A1 -xF '[[trading.muhurat_trading_dates]]' config/base.toml | grep -qxF \"date = \\\"$(TZ=Asia/Kolkata date +%F)\\\"\"";

/// Workflows whose market-hours guard must honour a Muhurat date.
const GUARDED_WORKFLOWS: [&str; 6] = [
    ".github/workflows/aws-control.yml",
    ".github/workflows/deploy-aws.yml",
    ".github/workflows/downsize-instance.yml",
    ".github/workflows/emergency-fs-recover.yml",
    ".github/workflows/terraform-apply.yml",
    ".github/workflows/wipe-log-streams.yml",
];

/// Weekday checks that deliberately do NOT carry the Muhurat arm, with why.
/// `deploy-aws.yml` UP_WINDOW only decides whether a STOPPED box should be
/// started for an auto push; on a Muhurat date the deploy job is refused by
/// the guard above it unless an operator dispatches with the override, and a
/// dispatch already sets SHOULD_BE_UP.
const EXEMPT_LINES: [&str; 1] = ["then UP_WINDOW=yes; fi"];

fn read(rel: &str) -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

/// Lines holding a Monday-to-Friday test (`-le 5 ]`).
fn weekday_check_lines(text: &str) -> Vec<(usize, &str)> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| l.contains("-le 5 ]"))
        .collect()
}

#[test]
fn every_weekday_market_check_also_closes_on_a_muhurat_date() {
    for wf in GUARDED_WORKFLOWS {
        let text = read(wf);
        let lines: Vec<&str> = text.lines().collect();
        let checks = weekday_check_lines(&text);
        assert!(!checks.is_empty(), "{wf}: no weekday market check found");
        for (i, line) in checks {
            if EXEMPT_LINES.iter().any(|e| line.contains(e)) {
                continue;
            }
            // Either the condition itself ORs the Muhurat flag, or (a shell
            // function) the line just before it returns early on the date.
            let ors_flag = line.contains("[ \"$MUHURAT\" = \"yes\" ] ||");
            let returns_early = (i.saturating_sub(2)..i).any(|j| {
                lines
                    .get(j)
                    .is_some_and(|l| l.contains(MUHURAT_SNIPPET) && l.contains("&& return 0"))
            });
            assert!(
                ors_flag || returns_early,
                "{wf}:{} weekday market check has no Muhurat arm: `{}`",
                i + 1,
                line.trim()
            );
        }
    }
}

#[test]
fn every_muhurat_flag_is_computed_from_the_calendar_file() {
    for wf in GUARDED_WORKFLOWS {
        let text = read(wf);
        let uses = text.matches("[ \"$MUHURAT\" = \"yes\" ]").count();
        let sets = text
            .matches(&format!("MUHURAT=no; {MUHURAT_SNIPPET} && MUHURAT=yes"))
            .count();
        assert!(
            uses == 0 || sets >= 1,
            "{wf} reads $MUHURAT but never computes it from config/base.toml"
        );
        assert!(
            text.contains(MUHURAT_SNIPPET),
            "{wf} does not read the calendar's Muhurat dates"
        );
    }
}

/// A guard job with no checkout would grep a missing file and read every day
/// as "not Muhurat" (fail-open), so each one checks out config/base.toml.
#[test]
fn every_guard_job_has_the_calendar_file() {
    for wf in GUARDED_WORKFLOWS {
        let text = read(wf);
        assert!(
            text.contains("sparse-checkout: config/base.toml"),
            "{wf}: the Muhurat guard step must check out config/base.toml"
        );
    }
    // The two jobs that already check out the whole repository.
    let deploy = read(".github/workflows/deploy-aws.yml");
    assert!(deploy.matches("actions/checkout@").count() >= 3);
    let apply = read(".github/workflows/terraform-apply.yml");
    assert!(apply.matches("actions/checkout@").count() >= 3);
}

#[test]
fn guard_self_test_detects_a_weekday_check_without_the_muhurat_arm() {
    let bad = "if [ \"$DOW\" -le 5 ] && [ \"$HHMM\" -ge 900 ]; then";
    assert!(!bad.contains("[ \"$MUHURAT\" = \"yes\" ] ||"));
    assert_eq!(weekday_check_lines(bad).len(), 1);
    let good = "if [ \"$MUHURAT\" = \"yes\" ] || { [ \"$DOW\" -le 5 ]; }; then";
    assert!(good.contains("[ \"$MUHURAT\" = \"yes\" ] ||"));
}

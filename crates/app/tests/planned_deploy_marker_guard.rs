//! Planned-deploy-stop marker lockstep guard
//! (`dhan-rest-only-noise-lock-2026-07-14.md` §2.3x, 2026-09-23).
//!
//! Every deploy restart paged the operator "Unexpected stop", because the
//! SIGTERM landed outside the 17:25–17:45 quiet window and nothing in-process
//! can tell a deploy SIGTERM from a manual one. The deploy now writes a
//! timestamped marker immediately before each stop it CHOOSES, and the app
//! consumes it at shutdown (`shutdown_class::take_planned_deploy_marker`).
//!
//! The two halves live in different files — a workflow and a Rust const — so
//! they can drift silently: a renamed path on either side restores the false
//! page with every test green. This guard pins them together.

#![cfg(test)]

use std::path::PathBuf;

use tickvault_app::shutdown_class::PLANNED_DEPLOY_MARKER_PATH;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root") // APPROVED: test
        .to_path_buf()
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(repo_root().join(rel))
        .unwrap_or_else(|e| panic!("read {rel} failed: {e}"))
}

/// The absolute marker path the workflow must write: the systemd
/// `WorkingDirectory` joined with the app's relative const.
fn box_marker_path() -> String {
    let unit = read("deploy/systemd/tickvault.service");
    let workdir = unit
        .lines()
        .find_map(|l| l.trim().strip_prefix("WorkingDirectory="))
        .expect("tickvault.service must declare WorkingDirectory") // APPROVED: test
        .trim()
        .trim_end_matches('/')
        .to_string();
    format!("{workdir}/{PLANNED_DEPLOY_MARKER_PATH}")
}

/// Line numbers of every stop the workflow CHOOSES that the app would see as
/// a SIGTERM: the release restart and the out-of-window instance stop. The
/// failure-path `systemctl stop tickvault` is deliberately NOT here — a
/// failed deploy SHOULD page.
fn chosen_stop_lines(wf: &str) -> Vec<(usize, String)> {
    wf.lines()
        .enumerate()
        .filter(|(_, l)| {
            let t = l.trim_start();
            !t.starts_with('#')
                && (t.contains("\"systemctl restart tickvault --no-block")
                    || t.starts_with("aws ec2 stop-instances"))
        })
        .map(|(i, l)| (i, l.trim().to_string()))
        .collect()
}

/// For each chosen stop, is a marker write for `marker` within the preceding
/// `window` lines? Returns the stops that are NOT announced.
fn unannounced_stops(wf: &str, marker: &str, window: usize) -> Vec<String> {
    let lines: Vec<&str> = wf.lines().collect();
    chosen_stop_lines(wf)
        .into_iter()
        .filter(|(idx, _)| {
            let from = idx.saturating_sub(window);
            !lines[from..*idx]
                .iter()
                .any(|l| l.contains(&format!("> {marker}")))
        })
        .map(|(idx, l)| format!("line {}: {l}", idx + 1))
        .collect()
}

const ANNOUNCE_WINDOW_LINES: usize = 25;

#[test]
fn every_stop_the_deploy_chooses_is_announced_with_the_apps_marker_path() {
    let wf = read(".github/workflows/deploy-aws.yml");
    let marker = box_marker_path();
    let stops = chosen_stop_lines(&wf);
    assert_eq!(
        stops.len(),
        2,
        "expected exactly the release restart and the out-of-window stop, got: {stops:#?}"
    );
    let missing = unannounced_stops(&wf, &marker, ANNOUNCE_WINDOW_LINES);
    assert!(
        missing.is_empty(),
        "deploy-aws.yml stops the app without first writing `{marker}` — each one \
         pages the operator as an UNEXPECTED stop: {missing:#?}"
    );
}

#[test]
fn the_marker_path_resolves_under_the_units_working_directory() {
    assert_eq!(
        box_marker_path(),
        "/opt/tickvault/data/planned-restart.marker"
    );
}

#[test]
fn the_announcement_scan_bites_on_a_bare_restart() {
    let marker = "/opt/tickvault/data/planned-restart.marker";
    let bare = "steps:\n              \"systemctl restart tickvault --no-block || exit 1\",\n          aws ec2 stop-instances --instance-ids x\n";
    assert_eq!(
        unannounced_stops(bare, marker, ANNOUNCE_WINDOW_LINES).len(),
        2
    );

    let announced = format!(
        "              \"date +%s > {marker}\",\n              \"systemctl restart tickvault --no-block || exit 1\",\n            \"date +%s > {marker}\"\n          aws ec2 stop-instances --instance-ids x\n"
    );
    assert!(unannounced_stops(&announced, marker, ANNOUNCE_WINDOW_LINES).is_empty());

    // A marker written to a DIFFERENT path announces nothing the app reads.
    let wrong = "              \"date +%s > /tmp/elsewhere\",\n              \"systemctl restart tickvault --no-block\",\n";
    assert_eq!(
        unannounced_stops(wrong, marker, ANNOUNCE_WINDOW_LINES).len(),
        1
    );
}

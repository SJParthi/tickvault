//! Source-scan ratchet — systemd boot-notify coverage on EVERY boot path
//! (boot-notify hotfix, folded 2026-07-13 from PR #1515's app-side commit
//! `c1a50504` per the coordinator fast-path decision).
//!
//! **The incident this pins closed:** tickvault.service is `Type=notify`
//! with `WatchdogSec=60`. Both pre-existing `notify_systemd_ready()` sites
//! were Dhan-gated (the FAST crash-recovery arm + inside `start_dhan_lane`),
//! so the Phase-A `dhan_enabled=false` flip (#1496) left every prod boot
//! stuck `activating` forever — `systemctl restart tickvault` never
//! returned and 5 consecutive deploys hung into the auto-stop. Worse, both
//! `WATCHDOG=1` pingers (`spawn_heartbeat_watchdog`) were equally
//! Dhan-gated (fast arm + lane-owned, aborted on lane teardown), so fixing
//! READY alone would have traded the hang for a SIGABRT at t+60s and a
//! restart loop.
//!
//! Pins (comment lines are stripped before matching so a doc-comment
//! mention can never vacuously satisfy a pin):
//! 1. `infra::notify_systemd_ready();` appears on ALL THREE boot paths —
//!    ≥3 real code sites, one of them inside the Dhan-OFF arm's
//!    `if !config.feeds.dhan_enabled` block between the lane gate's
//!    else-arm and the process run-loop.
//! 2. The PROCESS-GLOBAL `WATCHDOG=1` pinger spawn lives in the SHARED
//!    boot prefix — in source order BEFORE the fast/slow arm split, BEFORE
//!    the Dhan lane gate, and BEFORE every READY site — and is NOT gated
//!    on `config.feeds.dhan_enabled` (no ON-gate exists anywhere above
//!    it inside `main()`).
//! 3. The pinger body really pings (`infra::notify_systemd_watchdog();` on
//!    a `WATCHDOG_INTERVAL_SECS` interval) — never a stub (Rule 14).
//! 4. The code cadence honors the unit file: `WATCHDOG_INTERVAL_SECS × 2 ≤
//!    WatchdogSec` parsed from `deploy/systemd/tickvault.service`, which
//!    must stay `Type=notify`.

use std::fs;
use std::path::PathBuf;

use tickvault_app::boot_helpers::WATCHDOG_INTERVAL_SECS;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .map(PathBuf::from)
        .expect("workspace root must exist above crates/app") // APPROVED: test
}

fn read(rel: &str) -> String {
    let path = workspace_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Strip `//`-comment lines (incl. `///` docs) and `#` unit-file comment
/// lines so every needle below must match CODE/config, never prose (house
/// pattern of `dhan_live_off_phase_a_guard.rs`).
fn strip_line_comments(src: &str) -> String {
    src.lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with("//") && !t.starts_with('#')
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Non-vacuity self-test for the stripper: a commented-out needle must NOT
/// survive into the scanned text (so a pin can never be satisfied by prose).
#[test]
fn test_comment_stripper_removes_commented_needles() {
    let sample = "code_line();\n// let _process_global_watchdog_pinger = tokio::spawn(\n/// infra::notify_systemd_ready();\n# WatchdogSec=60\nreal();";
    let stripped = strip_line_comments(sample);
    assert!(stripped.contains("code_line();") && stripped.contains("real();"));
    assert!(!stripped.contains("_process_global_watchdog_pinger"));
    assert!(!stripped.contains("notify_systemd_ready"));
    assert!(!stripped.contains("WatchdogSec"));
}

const READY_NEEDLE: &str = "infra::notify_systemd_ready();";
const PINGER_SPAWN_NEEDLE: &str = "let _process_global_watchdog_pinger = tokio::spawn(";

/// Pin 1 — READY=1 is provably reached on all three boot paths: ≥3 real
/// code sites, with the Dhan-OFF site positioned inside the Dhan-OFF arm
/// region (after the lane gate's `DHAN LANE SKIPPED` else-arm, before the
/// process run-loop) and gated `if !config.feeds.dhan_enabled`.
#[test]
fn test_ready_notify_covers_all_boot_paths() {
    let main_src = strip_line_comments(&read("crates/app/src/main.rs"));

    let ready_positions: Vec<usize> = main_src
        .match_indices(READY_NEEDLE)
        .map(|(pos, _)| pos)
        .collect();
    assert!(
        ready_positions.len() >= 3,
        "expected >= 3 real `{READY_NEEDLE}` sites (FAST arm + start_dhan_lane \
         + the Dhan-OFF arm); found {} — losing the Dhan-OFF site re-opens the \
         2026-07-13 deploy hang (Type=notify start job never released, 5 \
         consecutive prod deploys burned their SSM wait budget)",
        ready_positions.len()
    );

    // The Dhan-OFF site: between the lane gate's else-arm log and the
    // process run-loop, inside an `if !config.feeds.dhan_enabled` block.
    let skipped_pos = main_src
        .find("DHAN LANE SKIPPED")
        .expect("the DHAN LANE SKIPPED log must exist in main.rs"); // APPROVED: test
    let runloop_pos = main_src
        .find("run_process_runloop(")
        .expect("run_process_runloop must exist in main.rs"); // APPROVED: test
    assert!(
        skipped_pos < runloop_pos,
        "source-order assumption broken: DHAN LANE SKIPPED must precede the \
         first run_process_runloop call"
    );
    let dhan_off_ready = ready_positions
        .iter()
        .copied()
        .find(|&p| p > skipped_pos && p < runloop_pos)
        .unwrap_or_else(|| {
            panic!(
                "no `{READY_NEEDLE}` site between DHAN LANE SKIPPED \
                 @{skipped_pos} and run_process_runloop @{runloop_pos} — the \
                 Dhan-OFF boot arm lost its READY=1 (every dhan_enabled=false \
                 boot leaves systemd `activating` forever; deploy hang class \
                 2026-07-13)"
            ) // APPROVED: test
        });
    // The site must sit directly under its `if !config.feeds.dhan_enabled {`
    // gate (READY on the Dhan-ON slow path is owned by start_dhan_lane).
    let gate_pos = main_src[..dhan_off_ready]
        .rfind("if !config.feeds.dhan_enabled {")
        .expect("the Dhan-OFF READY site must be gated on !dhan_enabled"); // APPROVED: test
    assert!(
        dhan_off_ready - gate_pos < 200,
        "the Dhan-OFF READY site must sit directly inside its \
         `if !config.feeds.dhan_enabled` gate (gate @{gate_pos}, notify \
         @{dhan_off_ready} — gap {} chars; a drifted gate means the notify \
         may no longer be on the Dhan-OFF path)",
        dhan_off_ready - gate_pos
    );
}

/// Pin 2 + Pin 3 — the process-global WATCHDOG=1 pinger spawn lives in the
/// SHARED boot prefix (before the fast/slow split, the lane gate, and every
/// READY site), is NOT feed-gated, and its body really pings.
#[test]
fn test_watchdog_pinger_is_shared_prefix_feed_unconditional_and_real() {
    let main_src = strip_line_comments(&read("crates/app/src/main.rs"));

    let spawn_positions: Vec<usize> = main_src
        .match_indices(PINGER_SPAWN_NEEDLE)
        .map(|(pos, _)| pos)
        .collect();
    assert_eq!(
        spawn_positions.len(),
        1,
        "expected exactly 1 `{PINGER_SPAWN_NEEDLE}` site in main.rs; found {} \
         — the process-global pinger must be spawned once, in the shared boot \
         prefix (a second site would hide a re-gated copy)",
        spawn_positions.len()
    );
    let spawn_pos = spawn_positions[0];

    // Shared prefix: before the fast/slow arm split...
    let fast_split_pos = main_src
        .find("fast_cache.filter(|_| is_market_hours && config.feeds.dhan_enabled)")
        .expect("the FAST-arm gate must exist in main.rs"); // APPROVED: test
    // ...before the Dhan lane gate...
    let lane_gate_pos = main_src
        .find("let dhan_lane: Option<DhanLaneRunHandles> = if config.feeds.dhan_enabled {")
        .expect("the Dhan lane gate must exist in main.rs"); // APPROVED: test
    // ...and before EVERY READY site (the watchdog must be armed before any
    // path can send READY=1 — READY with no pinger = SIGABRT at t+60s).
    let first_ready = main_src
        .find(READY_NEEDLE)
        .expect("notify_systemd_ready sites must exist (Pin 1)"); // APPROVED: test
    assert!(
        spawn_pos < fast_split_pos && spawn_pos < lane_gate_pos && spawn_pos < first_ready,
        "the WATCHDOG=1 pinger spawn must live in the SHARED boot prefix: \
         spawn @{spawn_pos} must precede the FAST-arm split @{fast_split_pos}, \
         the lane gate @{lane_gate_pos}, and the first READY site \
         @{first_ready} — otherwise some boot path sends READY=1 with no \
         pinger and systemd SIGABRTs the box 60s later (restart loop into \
         StartLimitBurst hard-fail)"
    );

    // NOT feed-gated: no `if config.feeds.dhan_enabled` (the ON-gate form —
    // deliberately distinct from the Dhan-OFF `if !config...` info block)
    // may exist anywhere above the spawn inside main(). Combined with the
    // source-order pins above (both dhan-gated regions START after the
    // spawn), the spawn is provably unconditional on feed config.
    let main_fn_pos = main_src
        .find("async fn main()")
        .expect("async fn main must exist"); // APPROVED: test
    assert!(
        main_fn_pos < spawn_pos,
        "the pinger spawn must live inside main() (spawn @{spawn_pos}, \
         main() @{main_fn_pos})"
    );
    assert!(
        !main_src[main_fn_pos..spawn_pos].contains("if config.feeds.dhan_enabled"),
        "found an `if config.feeds.dhan_enabled` gate ABOVE the pinger spawn \
         inside main() — the process-global WATCHDOG=1 pinger must be \
         UNCONDITIONAL on feed config (a feed-gated pinger re-opens the \
         Dhan-OFF SIGABRT class)"
    );

    // Pin 3 — the spawn body really pings on the shared cadence constant
    // (never a stub; Rule 14). The block is small — scan a bounded window.
    let window_end = (spawn_pos + 900).min(main_src.len());
    let body = &main_src[spawn_pos..window_end];
    assert!(
        body.contains("infra::notify_systemd_watchdog();"),
        "the pinger body must call `infra::notify_systemd_watchdog();` — a \
         spawn that never pings is a stub that still passes the position pins"
    );
    assert!(
        body.contains("WATCHDOG_INTERVAL_SECS"),
        "the pinger body must tick on `WATCHDOG_INTERVAL_SECS` — a hardcoded \
         cadence would drift from the unit-file budget pin below"
    );
}

/// Pin 4 — the unit file stays Type=notify with a WatchdogSec, and the code
/// cadence pings at least twice per watchdog window (interval × 2 ≤
/// WatchdogSec). Parses the REAL unit file so the two sides cannot drift
/// apart silently.
#[test]
fn test_pinger_interval_within_unit_watchdog_budget() {
    let unit = strip_line_comments(&read("deploy/systemd/tickvault.service"));
    assert!(
        unit.contains("Type=notify"),
        "tickvault.service must stay Type=notify — the READY=1 sites and \
         this whole guard exist because of it"
    );
    let watchdog_sec: u64 = unit
        .lines()
        .find_map(|l| l.trim().strip_prefix("WatchdogSec="))
        .expect("tickvault.service must carry WatchdogSec=") // APPROVED: test
        .trim()
        .parse()
        .expect("WatchdogSec must be a plain integer seconds value"); // APPROVED: test
    assert!(
        WATCHDOG_INTERVAL_SECS > 0,
        "the ping cadence must be non-zero"
    );
    assert!(
        WATCHDOG_INTERVAL_SECS * 2 <= watchdog_sec,
        "WATCHDOG_INTERVAL_SECS ({WATCHDOG_INTERVAL_SECS}s) x 2 must fit \
         inside the unit's WatchdogSec ({watchdog_sec}s) so a healthy process \
         always lands >= 2 pings per watchdog window — a single missed tick \
         must never SIGABRT the box"
    );
}

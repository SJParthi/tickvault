//! Source-scan ratchet — Dhan live WS feed OFF, Phase A (flip + REST-only
//! bootstrap).
//!
//! **Operator directive (2026-07-13, verbatim, relayed via the coordinator
//! session):** "now remove this entire Dhan live websocket feed instruments
//! subscription even entire live websocket feed itself... As of now only
//! Groww and Dhan historical api pull as we discussed last night along with
//! option chain." Rationale (verbatim): "when we checked the live websocket
//! feed candles and historical data api candles for Dhan has a massive major
//! mismatches... that's why I want to remove this. For Groww let us have
//! live websocket feed api as of now."
//!
//! Pins (each is a build-failing regression guard; comment lines are
//! stripped before matching so a doc-comment mention can never vacuously
//! satisfy a pin):
//! 1. `config/base.toml` + `config/production.toml` both carry
//!    `dhan_enabled = false` (and never `= true`) — the live WS lane is OFF
//!    by default on every boot profile.
//! 2. main.rs spawns the Dhan REST-only stack exactly ONCE, inside the
//!    Dhan-OFF branch (after the lane gate, before the process run-loop) —
//!    the mutual-exclusion-by-construction contract with the lane.
//! 3. The feed-state overlay AND-gate exists in
//!    `crates/api/src/feed_state_persist.rs` (a stale
//!    `data/feed-state.json` with `dhan_enabled: true` can never resurrect
//!    the retired lane) + the suppression predicate + the boot-site warn.
//! 4. The runtime cold-start supervisor carries the refusal (a POST
//!    /api/feeds/dhan enable must not cold-start the full lane against the
//!    REST-only stack's SSM lock).
//! 5. Stub-guard (audit-findings Rule 14): `dhan_rest_stack.rs` really
//!    brings the retained REST surface up — lock-before-mint, token init,
//!    renewal, watchdog, canary, spot, chain, gauge.
//!
//! Flipping `dhan_enabled` back to `true` (Phase-A rollback) requires a
//! fresh dated operator quote + updating this guard and
//! `crates/common/tests/production_config_wiring.rs` in the same PR.

use std::fs;
use std::path::PathBuf;

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

/// Strip `//`-comment lines (incl. `///` docs) and `#` TOML comment lines so
/// every needle below must match CODE/config, never prose (house pattern of
/// the option_chain_1m strike guard's comment stripper).
fn strip_line_comments(src: &str) -> String {
    src.lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with("//") && !t.starts_with('#')
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Pin 1 — both boot-profile TOMLs lock the Dhan live WS feed OFF.
#[test]
fn test_configs_lock_dhan_live_ws_off() {
    for rel in ["config/base.toml", "config/production.toml"] {
        let content = strip_line_comments(&read(rel));
        assert!(
            content.contains("dhan_enabled = false"),
            "{rel} must carry `dhan_enabled = false` — the Dhan live WS lane \
             is retired by operator directive 2026-07-13 (Dhan is REST-only; \
             Groww is the live feed)"
        );
        assert!(
            !content.contains("dhan_enabled = true"),
            "{rel} must NOT carry `dhan_enabled = true` — re-enabling the \
             retired live WS lane requires a fresh dated operator quote + \
             updating this guard in the same PR"
        );
    }
    // Groww stays THE live feed in prod.
    let prod = strip_line_comments(&read("config/production.toml"));
    assert!(
        prod.contains("groww_enabled = true"),
        "config/production.toml must keep `groww_enabled = true` — Groww is \
         the live feed (operator directive 2026-07-13)"
    );
}

/// Pin 2 — main.rs spawns the REST-only stack exactly once, inside the
/// Dhan-OFF branch: after the lane gate's `else` (the "DHAN LANE SKIPPED"
/// arm) and before the process run-loop.
#[test]
fn test_main_spawns_rest_stack_in_dhan_off_branch() {
    let main_src = strip_line_comments(&read("crates/app/src/main.rs"));

    const SPAWN_CALL: &str = "tickvault_app::dhan_rest_stack::spawn_dhan_rest_stack(";
    let spawn_positions: Vec<usize> = main_src
        .match_indices(SPAWN_CALL)
        .map(|(pos, _)| pos)
        .collect();
    assert_eq!(
        spawn_positions.len(),
        1,
        "expected exactly 1 `{SPAWN_CALL}` call site in main.rs (the Dhan-OFF \
         branch); found {} — a second site would double-spawn the REST stack \
         or break the lane mutual exclusion",
        spawn_positions.len()
    );
    let spawn_pos = spawn_positions[0];

    // The lane gate whose `else` hosts the spawn.
    let lane_gate_pos = main_src
        .find("let dhan_lane: Option<DhanLaneRunHandles> = if config.feeds.dhan_enabled {")
        .expect("the Dhan lane gate must exist in main.rs"); // APPROVED: test
    // The Dhan-OFF arm's own log line (a code string literal, survives the
    // comment strip) — the spawn must sit in the SAME arm, after it.
    let skipped_pos = main_src
        .find("DHAN LANE SKIPPED")
        .expect("the DHAN LANE SKIPPED log must exist in main.rs"); // APPROVED: test
    let runloop_pos = main_src
        .find("run_process_runloop(")
        .expect("run_process_runloop must exist in main.rs"); // APPROVED: test
    assert!(
        lane_gate_pos < skipped_pos && skipped_pos < spawn_pos && spawn_pos < runloop_pos,
        "the REST-stack spawn must live INSIDE the Dhan-OFF else-arm: lane \
         gate @{lane_gate_pos} < DHAN LANE SKIPPED @{skipped_pos} < spawn \
         @{spawn_pos} < run_process_runloop @{runloop_pos}"
    );
}

/// Pin 3 — the feed-state overlay is narrow-only for Dhan (config-off is
/// authoritative for the retired lane) and the boot site warns on a
/// suppressed widening overlay.
#[test]
fn test_overlay_and_gate_and_suppression_warn_exist() {
    let persist_src = strip_line_comments(&read("crates/api/src/feed_state_persist.rs"));
    assert!(
        persist_src.contains("dhan_enabled: config.dhan_enabled && p.dhan_enabled"),
        "feed_state_persist.rs lost the Dhan overlay AND-gate — a stale \
         data/feed-state.json with dhan_enabled=true could resurrect the \
         retired live WS lane (operator directive 2026-07-13)"
    );
    assert!(
        persist_src.contains("pub fn dhan_overlay_suppressed("),
        "feed_state_persist.rs lost the dhan_overlay_suppressed predicate — \
         the boot-site suppression warn depends on it"
    );
    // Groww overlay semantics UNCHANGED (persisted wins both directions).
    assert!(
        persist_src.contains("groww_enabled: p.groww_enabled"),
        "feed_state_persist.rs must keep the Groww overlay semantics \
         unchanged (persisted groww choice wins both directions)"
    );

    let main_src = strip_line_comments(&read("crates/app/src/main.rs"));
    assert!(
        main_src.contains("dhan_overlay_suppressed("),
        "main.rs must consult dhan_overlay_suppressed at the overlay \
         application site (the one-shot boot warn)"
    );
    assert!(
        main_src.contains("overlay SUPPRESSED"),
        "main.rs lost the overlay-suppression warn naming the 2026-07-13 \
         directive (the suppression must never be silent)"
    );
}

/// Pin 4 — the runtime cold-start supervisor refuses to start the retired
/// lane while the boot config says Dhan is off (a POST /api/feeds/dhan
/// enable would otherwise collide with the REST-only stack's dual-instance
/// SSM lock: AlreadyHeld → DHAN-LANE-03 retry loop + pages).
#[test]
fn test_runtime_cold_start_refusal_exists() {
    let main_src = strip_line_comments(&read("crates/app/src/main.rs"));
    assert!(
        main_src.contains("if !ctx.config.feeds.dhan_enabled {"),
        "run_dhan_lane_runtime_supervisor lost the config gate on the \
         cold-start spawn (operator directive 2026-07-13)"
    );
    assert!(
        main_src.contains("Dhan live WS lane retired by operator directive 2026-07-13"),
        "main.rs lost the runtime-enable refusal notice — the refusal must \
         be logged (edge-latched), never silent"
    );
}

/// Pin 5 — stub-guard (Rule 14): the REST-only stack module really brings
/// the retained surface up (lock-before-mint → token → renewal + watchdog →
/// canary + spot + chain + up-gauge), not a skeleton.
#[test]
fn test_rest_stack_module_is_not_a_stub() {
    let module_src = strip_line_comments(&read("crates/app/src/dhan_rest_stack.rs"));
    for needle in [
        // Lock BEFORE mint (dual-instance-lock-2026-07-04.md §2).
        "try_acquire_instance_lock(",
        "spawn_instance_lock_heartbeat(",
        // Token stack with the RESILIENCE-03 tripwire flag.
        "TokenManager::initialize(",
        "Some(Arc::clone(&instance_lock_held))",
        "set_global_token_manager(",
        "set_live_token_manager(",
        // Renewal + mid-session watchdog.
        "spawn_renewal_task()",
        "spawn_mid_session_profile_watchdog(",
        // The three retained REST subsystems.
        "run_rest_canary(",
        "spawn_supervised_spot_1m_rest(",
        "spawn_supervised_option_chain_1m(",
        "run_option_chain_1m_probe(",
        // Their existing config gates are respected.
        "config.spot_1m_rest.enabled",
        "config.option_chain_1m.enabled",
        "config.option_chain_1m.probe_and_report",
        // Observability: the 0/1 up-gauge (Rule 11 — bring-up reads 0).
        "tv_dhan_rest_stack_up",
    ] {
        assert!(
            module_src.contains(needle),
            "dhan_rest_stack.rs lost its `{needle}` wiring — the REST-only \
             stack must bring the retained Dhan REST surface up for real \
             (Rule 14: no skeleton PRs)"
        );
    }
    // The lock ordering is structural: the acquire must precede the mint.
    let lock_pos = module_src
        .find("try_acquire_instance_lock(")
        .expect("lock acquire present (asserted above)"); // APPROVED: test
    let mint_pos = module_src
        .find("TokenManager::initialize(")
        .expect("token init present (asserted above)"); // APPROVED: test
    assert!(
        lock_pos < mint_pos,
        "lock-before-mint violated: try_acquire_instance_lock @{lock_pos} \
         must precede TokenManager::initialize @{mint_pos} \
         (dual-instance-lock-2026-07-04.md §2)"
    );
}

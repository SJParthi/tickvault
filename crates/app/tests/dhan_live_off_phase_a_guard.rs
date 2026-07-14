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
//! 4. The runtime cold-start supervisor carries the refusal, keyed off the
//!    RAW pre-overlay TOML value (round-2 FIX A) — a POST /api/feeds/dhan
//!    enable must not cold-start the full lane against the REST-only
//!    stack's SSM lock, and the spawn must live inside the gate's else arm
//!    (brace-matched — round-2 FIX C).
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

/// Naive brace matcher for the Pin 4 structural scan: returns the offset of
/// the `}` closing the `{` at `open`. Honest residual: it counts every brace
/// byte, so a `{`/`}` inside a string literal in the scanned block would
/// skew the depth — acceptable here because the supervisor's gate/else
/// blocks are small, contain no braces in their string literals today, and
/// a skew FAILS the assertions loudly (never a vacuous pass).
fn find_matching_brace(src: &str, open: usize) -> usize {
    assert_eq!(
        src.as_bytes()[open],
        b'{',
        "brace scan must start at an opening brace"
    );
    let mut depth = 0usize;
    for (i, b) in src.bytes().enumerate().skip(open) {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces from offset {open}"); // APPROVED: test
}

/// Pin 4 — the runtime cold-start supervisor refuses to start the retired
/// lane while the RAW boot TOML says Dhan is off (a POST /api/feeds/dhan
/// enable would otherwise collide with the REST-only stack's dual-instance
/// SSM lock: AlreadyHeld → DHAN-LANE-03 retry loop + pages). Round-2 FIX A
/// (2026-07-13): the gate reads `feed_runtime.is_dhan_config_enabled()`
/// (seeded PRE-overlay), never the overlaid `ctx.config.feeds` — a
/// persisted runtime-OFF overlay must not permanently retire a config-ON
/// lane.
///
/// Strengthened 2026-07-13 round 2 (review LOW — the round-1 order-only
/// scan still passed if the spawn was hoisted UNCONDITIONAL after the gate
/// closed): the scan now brace-matches the gate block and requires
/// (a) the refusal warn INSIDE the gate block (an emptied gate fails),
/// (b) the gate to close directly into an `else` arm (an unconditional
/// spawn after the gate fails — no else), and
/// (c) the region's ONLY cold-start spawn to live INSIDE that else arm
/// (a second/hoisted spawn site or a gate moved off the spawn path fails).
/// An inverted gate (`if feed_runtime.is_dhan_config_enabled()`) fails the
/// needle match itself. Honest residual (source-scan-inherent): a
/// restructure that keeps this exact if/else shape but changes the RUNTIME
/// meaning of `is_dhan_config_enabled()` is caught by the feed_state unit
/// tests, not by this scan.
#[test]
fn test_runtime_cold_start_refusal_exists() {
    let main_src = strip_line_comments(&read("crates/app/src/main.rs"));

    // Extract the supervisor function region: from its `pub async fn` to
    // the next top-level fn (the cold-start driver it spawns).
    let region_start = main_src
        .find("pub async fn run_dhan_lane_runtime_supervisor(")
        .expect("run_dhan_lane_runtime_supervisor must exist in main.rs"); // APPROVED: test
    let region_end = main_src
        .find("pub async fn run_dhan_lane_cold_start(")
        .expect("run_dhan_lane_cold_start must exist in main.rs"); // APPROVED: test
    assert!(
        region_start < region_end,
        "the supervisor must be defined before the cold-start driver \
         (source-order assumption of this scan)"
    );
    let region = &main_src[region_start..region_end];

    // Round-2 FIX A: the gate must read the RAW config snapshot...
    const GATE_NEEDLE: &str = "if !feed_runtime.is_dhan_config_enabled() {";
    let gate_pos = region.find(GATE_NEEDLE).expect(
        "run_dhan_lane_runtime_supervisor lost the RAW-config gate on the \
         cold-start spawn (operator directive 2026-07-13 + round-2 FIX A)",
    ); // APPROVED: test
    // ...and must never regress to the OVERLAID ctx.config value (a
    // persisted runtime-OFF overlay would permanently 409-lock a config-ON
    // boot out of the PR-E disable→restart→re-enable round trip).
    assert!(
        !region.contains("if !ctx.config.feeds.dhan_enabled {"),
        "the refusal gate must not regress to the OVERLAID config value \
         (round-2 FIX A, 2026-07-13)"
    );

    // (a) The refusal warn lives INSIDE the gate block.
    let gate_open = gate_pos + GATE_NEEDLE.len() - 1;
    let gate_close = find_matching_brace(region, gate_open);
    let warn_pos = region
        .find("Dhan live WS lane retired by operator directive 2026-07-13")
        .expect(
            "the supervisor lost the runtime-enable refusal notice — the \
             refusal must be logged (edge-latched), never silent",
        ); // APPROVED: test
    assert!(
        gate_open < warn_pos && warn_pos < gate_close,
        "the refusal notice must live INSIDE the gate block (warn \
         @{warn_pos} not in ({gate_open}, {gate_close})) — an emptied or \
         relocated gate body fails here"
    );

    // (b) The gate closes directly into an `else` arm (no unconditional
    // fall-through to the spawn, no `else if` re-gate).
    let after_gate = &region[gate_close + 1..];
    let ws = after_gate.len() - after_gate.trim_start().len();
    let else_kw = gate_close + 1 + ws;
    assert!(
        region[else_kw..].starts_with("else"),
        "the refusal gate must close into an `else` arm hosting the spawn — \
         an unconditional spawn hoisted after the gate fails here"
    );
    let else_open = region[else_kw..]
        .find('{')
        .map(|o| else_kw + o)
        .expect("the else arm must open a block"); // APPROVED: test
    assert!(
        region[else_kw + "else".len()..else_open].trim().is_empty(),
        "the else arm must open its block directly (an `else if` re-gate \
         would change the refusal semantics)"
    );

    // (c) The region's ONLY cold-start spawn lives INSIDE the else arm.
    const SPAWN_NEEDLE: &str = "tokio::spawn(run_dhan_lane_cold_start(";
    let spawn_positions: Vec<usize> = region
        .match_indices(SPAWN_NEEDLE)
        .map(|(pos, _)| pos)
        .collect();
    assert_eq!(
        spawn_positions.len(),
        1,
        "expected exactly 1 cold-start spawn in the supervisor region; a \
         second (unconditional/hoisted) spawn site bypassing the refusal \
         gate fails here"
    );
    let spawn_pos = spawn_positions[0];
    let else_close = find_matching_brace(region, else_open);
    assert!(
        else_open < spawn_pos && spawn_pos < else_close,
        "the cold-start spawn must live INSIDE the refusal gate's else arm \
         (spawn @{spawn_pos} not in ({else_open}, {else_close})) — a spawn \
         moved off the gated path fails here"
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
        // The retained REST subsystems (the REST canary was deleted
        // 2026-07-14 per the operator Dhan noise lock — the legs
        // self-detect REST death via their own escalation edges).
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

//! Source-scan ratchet — per-minute spot 1m REST pipeline wired into the
//! shared post-market spawn seam (operator grant 2026-07-12, PR-2).
//!
//! Pins (house pattern: `ws_rate_limit_cooldown_wiring_guard.rs`):
//! 1. main.rs carries exactly ONE `spawn_supervised_spot_1m_rest(` call
//!    site, INSIDE `spawn_post_market_tasks` (the rest_canary seam —
//!    invoked from BOTH boot paths, once-guarded), and that call is gated
//!    on `config.spot_1m_rest.enabled` (fail-safe: absent section = off).
//! 2. Stub-guard: `spot_1m_rest_boot.rs` really does the work — posts to
//!    the intraday path, re-loads the token handle per fire, drives the
//!    bounded re-poll ladder from the constant schedule, and persists via
//!    the storage writer (no skeleton PR — audit-findings Rule 14).
//!
//! Runbook: `.claude/rules/project/rest-1m-pipeline-error-codes.md`.

use std::fs;
use std::path::PathBuf;

fn read_app_src(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The `(` suffix distinguishes CALL sites from doc-comment mentions; the
/// fn DEFINITION lives in spot_1m_rest_boot.rs, not main.rs, so every
/// main.rs hit is a call site.
const SPAWN_CALL: &str = "spawn_supervised_spot_1m_rest(";
const CONFIG_GATE: &str = "config.spot_1m_rest.enabled";

// PR-C2 re-home (2026-07-13, operator retirement directive —
// websocket-connection-scope-lock.md "2026-07-13 Amendment"): the spot leg
// moved OUT of the deleted main.rs `spawn_post_market_tasks` seam INTO
// `dhan_rest_stack.rs` (the sole surviving Dhan surface, which owns the
// token + the spot→chain sequencing). The seam for the pin below is
// therefore the dhan_rest_stack module's PRODUCTION region (cut at its
// in-file `#[cfg(test)]` tests so a test literal can never satisfy a pin).
fn read_stack_production() -> String {
    let full = read_app_src("src/dhan_rest_stack.rs");
    match full.find("#[cfg(test)]") {
        Some(cut) => full[..cut].to_string(),
        None => full,
    }
}

#[test]
fn ratchet_spot1m_spawn_is_config_gated_inside_post_market_seam() {
    let main_src = read_stack_production();

    let spawn_positions: Vec<usize> = main_src
        .match_indices(SPAWN_CALL)
        .map(|(pos, _)| pos)
        .collect();
    assert_eq!(
        spawn_positions.len(),
        1,
        "expected exactly 1 `{SPAWN_CALL}` call site in dhan_rest_stack.rs \
         (the sole surviving Dhan surface since PR-C2); found {} — a second \
         site would double-spawn the per-minute fetcher",
        spawn_positions.len()
    );
    let spawn_pos = spawn_positions[0];

    let gate_positions: Vec<usize> = main_src
        .match_indices(CONFIG_GATE)
        .map(|(pos, _)| pos)
        .collect();
    assert!(
        !gate_positions.is_empty(),
        "dhan_rest_stack.rs must gate the spot_1m spawn on `{CONFIG_GATE}` \
         (fail-safe: an absent [spot_1m_rest] section disables it)"
    );
    assert!(
        gate_positions
            .iter()
            .any(|gate| { *gate < spawn_pos && spawn_pos - gate < 2_048 }),
        "the `{CONFIG_GATE}` gate must immediately precede the spawn call \
         inside dhan_rest_stack.rs (gates at {gate_positions:?}, \
         spawn at {spawn_pos})"
    );
}

/// Stub-guard (Rule 14 — no skeleton): the boot module must actually fetch
/// + parse + persist, not just define pub fns.
#[test]
fn ratchet_spot1m_boot_module_is_not_a_stub() {
    let module_src = read_app_src("src/spot_1m_rest_boot.rs");
    for needle in [
        // The Dhan endpoint is reached via the shared path constant +
        // join_api_url (the DHAN-REST-400 trailing-slash lesson).
        "DHAN_CHARTS_INTRADAY_PATH",
        "join_api_url(",
        // The JWT is re-loaded from the live handle EVERY fire (24h
        // rotation) — never copied once at spawn.
        "params.token_handle.load()",
        // The bounded in-minute re-poll ladder is driven from the constant
        // schedule (never an unbounded retry).
        "SPOT_1M_REST_RETRY_OFFSETS_MS",
        // Successes persist through the storage writer + flush.
        "Spot1mRestWriter",
        "writer.flush()",
        // The scheduler sleeps to computed minute boundaries (never a
        // drifting interval(60s)).
        "next_minute_close_fire(",
        // Edge-triggered escalation + recovery events are wired.
        "Spot1mFetchDegraded",
        "Spot1mFetchRecovered",
        // 2026-07-12 hostile-review fixes stay wired: the hard per-SID
        // ladder budget (H2), the missed-boundary loud accounting (H2),
        // the same-second re-fire horizon (H1), and the streamed body cap
        // (security M).
        "SPOT_1M_REST_SID_BUDGET_SECS",
        "tv_spot1m_boundary_skipped_total",
        "next_fire_after(",
        "SPOT_1M_REST_MAX_BODY_BYTES",
    ] {
        assert!(
            module_src.contains(needle),
            "spot_1m_rest_boot.rs lost its `{needle}` wiring — the module \
             must fetch, parse, persist and page for real (Rule 14: no \
             skeleton PRs)"
        );
    }
}

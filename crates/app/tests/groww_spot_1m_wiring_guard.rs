//! Source-scan ratchet — Groww per-minute spot 1m REST leg wired
//! PROCESS-GLOBAL in main.rs (operator grant 2026-07-13, PR-2 of the Groww
//! per-minute REST plan).
//!
//! Pins (house pattern: `spot_1m_rest_wiring_guard.rs`):
//! 1. main.rs carries exactly ONE `spawn_supervised_groww_spot_1m(` call
//!    site, gated on `config.groww_spot_1m.enabled` (fail-safe: absent
//!    section = off) and OUTSIDE the Dhan-gated `spawn_post_market_tasks`
//!    seam — a Dhan-off (Groww-only) session must still run this leg.
//! 2. Stub-guard: `groww_spot_1m_boot.rs` really does the work — hits the
//!    Groww candles endpoint, reads the shared-minter token READ-ONLY,
//!    drives the bounded ladder from the constant schedule, persists via
//!    the feed-parameterized storage writer, backfills + sweeps (the #1499
//!    patterns), and emits the per-fetch forensics rows (no skeleton PR —
//!    audit-findings Rule 14).
//! 3. No `unwrap(`/`expect(`/`println!` in the module's PRODUCTION region
//!    (split at `#[cfg(test)]` so test assertions never satisfy/violate
//!    the scan).
//!
//! Runbook: `.claude/rules/project/rest-1m-pipeline-error-codes.md`.

use std::fs;
use std::path::PathBuf;

fn read_app_src(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The `(` suffix distinguishes CALL sites from doc-comment mentions; the
/// fn DEFINITION lives in groww_spot_1m_boot.rs, not main.rs, so every
/// main.rs hit is a call site.
const SPAWN_CALL: &str = "spawn_supervised_groww_spot_1m(";
const CONFIG_GATE: &str = "config.groww_spot_1m.enabled";
const POST_MARKET_FN: &str = "fn spawn_post_market_tasks(";

#[test]
fn ratchet_groww_spot1m_spawn_is_config_gated_and_process_global() {
    let main_src = read_app_src("src/main.rs");

    let spawn_positions: Vec<usize> = main_src
        .match_indices(SPAWN_CALL)
        .map(|(pos, _)| pos)
        .collect();
    assert_eq!(
        spawn_positions.len(),
        1,
        "expected exactly 1 `{SPAWN_CALL}` call site in main.rs (the \
         process-global boot prefix — runs once regardless of feed lanes); \
         found {} — a second site would double-spawn the per-minute fetcher",
        spawn_positions.len()
    );
    let spawn_pos = spawn_positions[0];

    // The spawn must be OUTSIDE the Dhan-gated spawn_post_market_tasks
    // seam: that seam's two call sites are BOTH Dhan-gated, so nesting the
    // Groww leg there would silently kill it on a Groww-only session.
    let seam_pos = main_src
        .find(POST_MARKET_FN)
        .expect("spawn_post_market_tasks must exist in main.rs");
    let seam_end = main_src[seam_pos + POST_MARKET_FN.len()..]
        .find("\nfn ")
        .map_or(main_src.len(), |off| seam_pos + POST_MARKET_FN.len() + off);
    assert!(
        !(spawn_pos > seam_pos && spawn_pos < seam_end),
        "the groww_spot_1m spawn at byte {spawn_pos} must NOT live inside \
         the Dhan-gated spawn_post_market_tasks seam (fn spans bytes \
         {seam_pos}..{seam_end}) — this leg is Dhan-independent by contract"
    );

    let gate_positions: Vec<usize> = main_src
        .match_indices(CONFIG_GATE)
        .map(|(pos, _)| pos)
        .collect();
    assert!(
        !gate_positions.is_empty(),
        "main.rs must gate the groww_spot_1m spawn on `{CONFIG_GATE}` \
         (fail-safe: an absent [groww_spot_1m] section disables it)"
    );
    assert!(
        gate_positions
            .iter()
            .any(|gate| *gate < spawn_pos && spawn_pos - gate < 2_048),
        "the `{CONFIG_GATE}` gate must immediately precede the spawn call \
         (gates at {gate_positions:?}, spawn at {spawn_pos})"
    );
}

/// Stub-guard (Rule 14 — no skeleton): the boot module must actually
/// fetch + parse + persist + backfill + sweep + audit, not just define
/// pub fns.
#[test]
fn ratchet_groww_spot1m_boot_module_is_not_a_stub() {
    let module_src = read_app_src("src/groww_spot_1m_boot.rs");
    for needle in [
        // The Groww endpoint + SDK-verified identity/interval constants.
        "GROWW_HISTORICAL_CANDLES_URL",
        "GROWW_CANDLE_INTERVAL_1MIN",
        "GROWW_API_VERSION_HEADER",
        // The shared-minter token is read READ-ONLY from SSM — never
        // minted (token-minter lock 2026-07-02), re-read paced ≥60s.
        "fetch_groww_access_token",
        "GROWW_SPOT_1M_TOKEN_REREAD_FLOOR_SECS",
        // The bounded in-minute re-poll ladder is driven from the constant
        // schedule (never an unbounded retry) under the hard budget.
        "GROWW_SPOT_1M_RETRY_OFFSETS_MS",
        "GROWW_SPOT_1M_SYMBOL_BUDGET_SECS",
        // Successes persist through the feed-parameterized storage writer
        // + flush; the live-lane canonical Groww index id is REUSED.
        "Spot1mRestWriter::new_with_feed",
        "SPOT_1M_REST_FEED_GROWW",
        "stable_index_security_id",
        "writer.flush()",
        // The scheduler sleeps to computed minute boundaries (never a
        // drifting interval(60s)) with the H1/H2 invariants.
        "next_fire_after(",
        "count_missed_boundaries(",
        "tv_groww_spot1m_boundary_skipped_total",
        // The #1499 patterns are real code paths from day one.
        "backfill_minute_nanos(",
        "sweep_missing_minutes(",
        "run_post_session_sweep(",
        "PersistTracker",
        // Per-fetch forensics rows (the rest_fetch_audit table) — success
        // AND failure — with the named-gap contract.
        "RestFetchAuditWriter",
        "build_fetch_audit_row(",
        "record_named_gaps(",
        // Edge-triggered escalation + recovery events are wired.
        "GrowwSpot1mFetchDegraded",
        "GrowwSpot1mFetchRecovered",
        // The streamed body cap (security §18 pattern).
        "GROWW_SPOT_1M_MAX_BODY_BYTES",
        // The dual defensive timestamp parse + the wire-form probe.
        "tv_groww_spot1m_ts_form_total",
        // The close→data honesty histogram (operator Quote 2).
        "tv_groww_spot1m_close_to_data_ms",
    ] {
        assert!(
            module_src.contains(needle),
            "groww_spot_1m_boot.rs lost its `{needle}` wiring — the module \
             must fetch, parse, persist, backfill, sweep, audit and page \
             for real (Rule 14: no skeleton PRs)"
        );
    }
}

/// No `unwrap(` / `expect(` / `println!` in the module's PRODUCTION
/// region (the compile-time lints deny them too — this pins the split so
/// a future `#[allow]` can't sneak one in silently).
#[test]
fn ratchet_groww_spot1m_module_production_region_is_panic_free() {
    let module_src = read_app_src("src/groww_spot_1m_boot.rs");
    let production = module_src
        .split("#[cfg(test)]")
        .next()
        .unwrap_or(&module_src);
    for banned in [".unwrap(", ".expect(", "println!", "dbg!("] {
        assert!(
            !production.contains(banned),
            "groww_spot_1m_boot.rs production region contains `{banned}` — \
             every fault path must degrade typed + coded, never panic/print"
        );
    }
}

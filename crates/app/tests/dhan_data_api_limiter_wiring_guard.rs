//! Source-scan ratchet — shared self-tuning Dhan Data-API rate limiter
//! (operator pacing directive 2026-07-14: pace Dhan to 3 requests/sec —
//! tunable DOWN to 2 — spread overflow into the next second(s), route the
//! option-chain API through the SAME limiter).
//!
//! Pins (house pattern: `spot_1m_rest_wiring_guard.rs`):
//! 1. BOTH Data-API fetch choke points route through the shared limiter:
//!    `spot_1m_rest_boot::spot_1m_fetch_once` (covers the per-minute
//!    fires, the ladder re-polls, the 15:33:30 sweep AND the #1524
//!    diagnostic probes — they all funnel through it) and
//!    `option_chain_1m_boot::chain_fetch_once` (covers the per-minute
//!    chain fires + expirylist warmup/probe) each carry exactly one
//!    `acquire()` call.
//! 2. BOTH fetch fns feed observed 429s to the self-tuner from the REAL
//!    `StatusCode::TOO_MANY_REQUESTS` (never a substring scan).
//! 3. BOTH Dhan spawn seams (main.rs `spawn_post_market_tasks` +
//!    `dhan_rest_stack.rs`) configure the limiter cap from
//!    `config.dhan_data_api.target_rps` BEFORE spawning the REST tasks.
//! 4. The chain leg's 1-unique-per-3s per-underlying min-gap survives —
//!    it stays LAYERED ON TOP of the limiter, unchanged.
//! 5. The 2026-07-14 retry shaping is wired: the ladder consults the
//!    stale-watermark cutoff, and the run loop drives the adaptive
//!    degrade FSM + the fetch-mode dispatch.
//!
//! Runbook: `.claude/rules/project/rest-1m-pipeline-error-codes.md`
//! (2026-07-14 section).

use std::fs;
use std::path::PathBuf;

fn read_app_src(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The fn body of `name` (from its `async fn name(` to the next top-level
/// `\nasync fn ` / `\nfn ` / `\npub` boundary) — coarse but stable for
/// needle containment checks.
fn fn_body<'a>(src: &'a str, needle_decl: &str) -> &'a str {
    let start = src
        .find(needle_decl)
        .unwrap_or_else(|| panic!("`{needle_decl}` must exist"));
    let rest = &src[start..];
    let end = rest[needle_decl.len()..]
        .find("\nasync fn ")
        .or_else(|| rest[needle_decl.len()..].find("\npub "))
        .or_else(|| rest[needle_decl.len()..].find("\nfn "))
        .map_or(rest.len(), |off| needle_decl.len() + off);
    &rest[..end]
}

const ACQUIRE: &str = "shared_dhan_data_api_limiter()";
const RECORD_429: &str = ".record_429()";
const CONFIGURE: &str = "configure_shared_dhan_data_api_limiter(";
const CONFIG_KEY: &str = "config.dhan_data_api.target_rps";

#[test]
fn ratchet_spot_fetch_once_routes_through_shared_limiter() {
    let src = read_app_src("src/spot_1m_rest_boot.rs");
    // The PACED wrapper: acquires the shared permit and feeds the
    // classified rate-limited verdict to the self-tuner. Since the
    // cadence refactor the REAL StatusCode classification lives in the
    // `*_unpaced` inner — the wrapper consumes its typed
    // `failure.rate_limited` flag (never a substring scan).
    let body = fn_body(&src, "async fn spot_1m_fetch_once(");
    assert!(
        body.contains(ACQUIRE) && body.contains(".acquire()"),
        "spot_1m_fetch_once must acquire a permit from the shared Dhan \
         Data-API limiter — it is the choke point every spot-1m request \
         (fires, ladder re-polls, sweep, diagnostic probes) funnels through"
    );
    assert!(
        body.contains(RECORD_429) && body.contains("failure.rate_limited"),
        "spot_1m_fetch_once must feed the typed rate_limited verdict to \
         the self-tuner (the wrapper half of the 429→tuner chain)"
    );
    // The UNPACED inner: the sole place the rate-limited flag is derived —
    // from the REAL StatusCode, never a substring scan.
    let inner = fn_body(&src, "async fn spot_1m_fetch_once_unpaced(");
    assert!(
        inner.contains("TOO_MANY_REQUESTS"),
        "spot_1m_fetch_once_unpaced must classify 429 from the REAL \
         StatusCode::TOO_MANY_REQUESTS (the inner half of the chain)"
    );
}

#[test]
fn ratchet_chain_fetch_once_routes_through_shared_limiter() {
    let src = read_app_src("src/option_chain_1m_boot.rs");
    // Same two-half pin as the spot leg: paced wrapper acquires + feeds
    // the typed verdict; unpaced inner derives it from the REAL status.
    let body = fn_body(&src, "async fn chain_fetch_once(");
    assert!(
        body.contains(ACQUIRE) && body.contains(".acquire()"),
        "chain_fetch_once must acquire a permit from the SAME shared Dhan \
         Data-API limiter (the operator's 2026-07-14 directive: route the \
         option-chain API through the SAME limiter)"
    );
    assert!(
        body.contains(RECORD_429) && body.contains("failure.rate_limited"),
        "chain_fetch_once must feed the typed rate_limited verdict to the \
         self-tuner (the wrapper half of the 429→tuner chain)"
    );
    let inner = fn_body(&src, "async fn chain_fetch_once_unpaced(");
    assert!(
        inner.contains("TOO_MANY_REQUESTS"),
        "chain_fetch_once_unpaced must classify 429 from the REAL \
         StatusCode::TOO_MANY_REQUESTS (the inner half of the chain)"
    );
}

#[test]
fn ratchet_chain_min_gap_layer_survives() {
    let src = read_app_src("src/option_chain_1m_boot.rs");
    assert!(
        src.contains("min_gap_wait_ms("),
        "the chain leg's 1-unique-per-3s per-underlying min-gap must stay \
         LAYERED ON TOP of the shared limiter (2026-07-14 directive: \
         'stays layered on top, unchanged')"
    );
}

#[test]
fn ratchet_both_spawn_seams_configure_the_limiter_from_config() {
    // PR-C2 (2026-07-14 merge note): the lane's `spawn_post_market_tasks`
    // seam in main.rs — the guard's original SECOND spawn seam — was
    // DELETED with the Dhan live-WS lane, so `dhan_rest_stack.rs` is the
    // SOLE spot/chain spawn seam (and therefore the sole configure site).
    // The test name is kept (test-count ratchet stability); the loop now
    // pins exactly the surviving seam. A re-added main.rs seam MUST be
    // added back here in the same PR.
    for rel in ["src/dhan_rest_stack.rs"] {
        let src = read_app_src(rel);
        let configure_pos = src
            .find(CONFIGURE)
            .unwrap_or_else(|| panic!("{rel} must call `{CONFIGURE}`"));
        assert!(
            src[configure_pos..]
                .lines()
                .take(4)
                .any(|l| l.contains(CONFIG_KEY)),
            "{rel}: the configure call must take `{CONFIG_KEY}` (the \
             [dhan_data_api] target_rps cap)"
        );
        // The configure must precede the spot spawn call in source order
        // (limiter cap set BEFORE any REST task can fire).
        let spawn_pos = src
            .find("spawn_supervised_spot_1m_rest(")
            .unwrap_or_else(|| panic!("{rel} must spawn the spot task"));
        assert!(
            configure_pos < spawn_pos,
            "{rel}: the limiter must be configured BEFORE the spot task \
             spawns (source order)"
        );
    }
}

#[test]
fn ratchet_retry_shaping_is_wired_into_the_ladder_and_run_loop() {
    let src = read_app_src("src/spot_1m_rest_boot.rs");
    // Stale-watermark cutoff consulted INSIDE the ladder.
    let ladder = fn_body(&src, "async fn fetch_minute_with_ladder(");
    assert!(
        ladder.contains("ladder_watermark_repeated(")
            && ladder.contains("tv_spot1m_ladder_watermark_cutoff_total"),
        "the ladder must consult the stale-watermark cutoff and count stops"
    );
    // Adaptive degrade drives the attempt count and the run loop applies
    // the edge-triggered transitions.
    assert!(
        src.contains("ladder_attempt_count(degraded)"),
        "fire_one_minute must derive the attempt count from the degrade state"
    );
    assert!(
        src.contains("degrade.record_minute(ok_count > 0)")
            && src.contains("tv_spot1m_ladder_degraded"),
        "the run loop must drive the LadderDegrade FSM (edge-triggered \
         transitions + gauge)"
    );
    // The fetch-mode dispatch exists and batch mode reuses the shared
    // sweep helper (a thin mode wrapper, not new fetch logic).
    assert!(
        src.contains("SpotFetchMode::BatchCatchup") && src.contains("run_batch_catchup_loop("),
        "run_spot_1m_rest must dispatch on [spot_1m_rest] fetch_mode"
    );
    let batch = fn_body(&src, "async fn run_batch_catchup_loop(");
    assert!(
        batch.contains("sweep_sids_above_watermark("),
        "batch mode must reuse the shared sweep helper (never new fetch logic)"
    );
    let sweep = fn_body(&src, "async fn run_post_session_sweep(");
    assert!(
        sweep.contains("sweep_sids_above_watermark("),
        "the post-session sweep must reuse the SAME shared helper"
    );
}

#[test]
fn ratchet_limiter_module_is_the_single_pacing_authority() {
    let src = read_app_src("src/dhan_data_api_limiter.rs");
    for needle in [
        "ArcSwap",
        "until_ready()",
        "DHAN_DATA_API_RPS_FLOOR",
        "tv_dhan_data_api_rps",
        "tv_dhan_data_api_tuner_transitions_total",
    ] {
        assert!(
            src.contains(needle),
            "dhan_data_api_limiter.rs must keep `{needle}` — pre-built \
             governor cells swapped via ArcSwap, async overflow spill, the \
             2 rps floor and the edge-logged transition observability are \
             the operator-directed contract"
        );
    }
}

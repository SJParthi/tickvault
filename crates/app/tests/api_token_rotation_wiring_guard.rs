//! Source-scan ratchet — API bearer-token SSM re-read/rotation wired
//! end-to-end (W2#7, 2026-07-10 — audit row 13).
//!
//! The token was previously read ONCE at boot and held for the process
//! lifetime — rotating `/tickvault/<env>/api/bearer-token` in SSM required
//! an app restart. This guard pins the rotation chain in lockstep so a
//! rename / deletion on ANY side fails the build:
//!
//! 1. BOTH main.rs boot arms (fast crash-recovery + slow) spawn the
//!    supervised reload task right after constructing the auth config and
//!    BEFORE handing it to the router.
//! 2. The reload loop (`crates/app/src/api_token_rotation.rs`) actually
//!    reads SSM (`fetch_api_bearer_token` — READ-ONLY) and rotates via
//!    `rotate_bearer_token`, and carries the outcome counter
//!    `tv_api_token_reloads_total`.
//! 3. The middleware's WELL-FORMED-but-mismatched bearer arm (and ONLY
//!    that arm) hints the out-of-band re-read (`request_oob_reload`),
//!    floored inside the api crate so 401 spam cannot hammer SSM.
//!
//! House precedents: `auth_failed_alarm_wiring_guard.rs` /
//! `seal_drop_paging_wiring_guard.rs` (production-region scan + comment
//! stripper). Runbook: `.claude/rules/project/gap-enforcement.md`
//! (GAP-SEC-01, "2026-07-10 Update — token rotation without restart").

use std::fs;
use std::path::PathBuf;

fn read_repo_file(rel_from_app: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel_from_app);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display())) // APPROVED: test
}

/// Strip `//`-line-comments (treating `://` URL scheme separators as code —
/// the `http_client_fallback_guard.rs` precedent) so a comment mentioning a
/// needle can never satisfy the scan.
fn strip_line_comments(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    for line in body.lines() {
        let bytes = line.as_bytes();
        let mut cut = line.len();
        let mut i = 0;
        while i + 1 < bytes.len() {
            if bytes[i] == b'/' && bytes[i + 1] == b'/' && (i == 0 || bytes[i - 1] != b':') {
                cut = i;
                break;
            }
            i += 1;
        }
        out.push_str(&line[..cut]);
        out.push('\n');
    }
    out
}

/// Production region only: everything before the `#[cfg(test)]` module, so
/// a test-module mention can never satisfy a production-wiring needle
/// (the shadow-writer vacuous-scan lesson).
fn production_region(body: &str) -> String {
    // Cut at the test MODULE declaration (not any doc-comment mention of
    // the attribute — middleware.rs documents `#[cfg(test)]` in prose).
    let cut = body.find("#[cfg(test)]\nmod tests").unwrap_or(body.len());
    strip_line_comments(&body[..cut])
}

#[test]
fn ratchet_main_spawns_reload_at_both_boot_arms() {
    let main_src = production_region(&read_repo_file("src/main.rs"));
    let spawn_calls = main_src
        .matches("spawn_supervised_api_token_reload(")
        .count();
    assert!(
        spawn_calls >= 2,
        "BOTH main.rs boot arms (fast crash-recovery + slow) must spawn the \
         supervised API token reload task in the production region; found \
         {spawn_calls} call site(s)"
    );
    // Each spawn must be gated on `enabled` — a disabled dev config
    // (empty token, dry-run) must never get a reload task.
    for arm in ["api_auth_config_fast.enabled", "api_auth_config.enabled"] {
        assert!(
            main_src.contains(arm),
            "the reload spawn must be gated on `{arm}` (disabled dev config \
             gets no task)"
        );
    }
}

#[test]
fn ratchet_reload_loop_reads_ssm_and_rotates() {
    let loop_src = production_region(&read_repo_file("src/api_token_rotation.rs"));
    assert!(
        loop_src.contains("fetch_api_bearer_token()"),
        "the reload loop must re-read the token via the READ-ONLY SSM \
         fetch_api_bearer_token path"
    );
    assert!(
        loop_src.contains(".rotate_bearer_token("),
        "the reload loop must swap via ApiAuthConfig::rotate_bearer_token \
         (the fail-open validated swap)"
    );
    assert!(
        loop_src.contains(".wait_oob_reload()"),
        "the reload loop must consume the 401-triggered out-of-band hint \
         alongside the periodic tick"
    );
    assert!(
        loop_src.contains("\"tv_api_token_reloads_total\""),
        "the reload loop must carry the tv_api_token_reloads_total outcome \
         counter"
    );
    assert!(
        loop_src.contains("\"tv_api_token_reload_respawn_total\""),
        "the supervisor must carry the respawn counter"
    );
    // First-sample-baseline discipline: all four outcome series
    // pre-registered at 0.
    for outcome in ["\"ok\"", "\"unchanged\"", "\"failed\"", "\"rejected\""] {
        assert!(
            loop_src.contains(outcome),
            "outcome series {outcome} must exist (pre-registration + emit)"
        );
    }
    // READ-ONLY contract: the rotation module must never write SSM.
    for banned in ["put_parameter", "put_param", "PutParameter"] {
        assert!(
            !loop_src.contains(banned),
            "the token rotation module must NEVER write SSM (found `{banned}`)"
        );
    }
}

#[test]
fn ratchet_middleware_mismatch_arm_requests_oob_reload() {
    let mw_src = production_region(&read_repo_file("../api/src/middleware.rs"));
    assert!(
        mw_src.contains(".request_oob_reload()"),
        "the middleware must hint the out-of-band re-read on a mismatched \
         bearer"
    );
    // Exactly ONE hint site: the well-formed-but-mismatched arm. The
    // missing/malformed-header arms (the cheapest probes) must NOT hint.
    let hint_sites = mw_src.matches(".request_oob_reload()").count();
    assert_eq!(
        hint_sites, 1,
        "exactly one OOB hint site is allowed (the mismatched-bearer arm); \
         found {hint_sites}"
    );
    // The floor constant must exist and be referenced by the pure decision
    // fn — 401 spam is bounded to ≤1 SSM read per floor window.
    assert!(
        mw_src.contains("OOB_RELOAD_FLOOR_SECS"),
        "the 60s OOB floor constant must gate the hint path"
    );
    // The rotation-visible read must be the hot-swap load, not a frozen
    // field read.
    assert!(
        mw_src.contains("fn load_bearer_token"),
        "require_bearer_auth must read the CURRENT token via the hot-swap \
         holder load"
    );
}

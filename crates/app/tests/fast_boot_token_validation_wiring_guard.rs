//! Source-scan ratchet — AUTH-GAP-06 fast-boot cached-token validation
//! wired BEFORE any WebSocket spawn (2026-07-08).
//!
//! Operator incident (2026-07-07, THIRD occurrence): the FAST
//! crash-recovery boot arm trusted yesterday's CACHED Dhan token blindly —
//! Dhan had killed it (one active token at a time), so the feed sat dead
//! 08:32–09:06 IST until manual intervention. The fix validates the cached
//! token with ONE `GET /v2/profile` BEFORE the WS-GAP-08 cooldown wait and
//! the pool create; a prefix-anchored 401/403 forces a re-mint through the
//! EXISTING TokenManager machinery.
//!
//! This guard pins, in the main.rs PRODUCTION region (everything strictly
//! before the first top-level `#[cfg(test)]` — robust to code after
//! `mod tests`), with comments stripped (a comment mentioning the call can
//! never satisfy the scan):
//!
//! 1. EXACTLY ONE `validate_cached_token_at_fast_boot(` call site
//!    ("exactly one profile check per boot").
//! 2. That call precedes the FIRST `wait_out_persisted_ws_rate_limit_cooldown().await`
//!    AND the FIRST `match create_websocket_pool(` (the fast arm's pool
//!    site) — the mandated ordering: validation → cooldown wait → pool.
//! 3. Stub-guard: the core helper module actually calls the profile
//!    endpoint (`DHAN_USER_PROFILE_PATH`) and the existing remint
//!    machinery (`force_renewal(`) in ITS production region — the helper
//!    cannot silently degrade to a no-op while this wiring stays green.
//!
//! House precedents: `ws_rate_limit_cooldown_wiring_guard.rs` (fast-arm
//! source-order pairing) + `http_client_fallback_guard.rs` (comment
//! stripper that treats `://` as code, with a self-test).
//! Runbook: `.claude/rules/project/wave-4-error-codes.md` §AUTH-GAP-06.

use std::fs;
use std::path::PathBuf;

fn read_main_rs() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn read_core_helper() -> String {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../core/src/auth/fast_boot_validation.rs");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Returns the code portion of a line — everything before the first `//`
/// that is NOT the `://` of a URL scheme inside a string literal (the
/// `http_client_fallback_guard.rs` precedent).
fn code_portion(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'/' && bytes[i + 1] == b'/' {
            let preceded_by_colon = i > 0 && bytes[i - 1] == b':';
            if !preceded_by_colon {
                return &line[..i];
            }
            // `://` — a URL scheme separator, not a comment. Skip past it.
            i += 2;
        } else {
            i += 1;
        }
    }
    line
}

/// Comment-stripped source: every line reduced to its code portion.
fn strip_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for line in src.lines() {
        out.push_str(code_portion(line));
        out.push('\n');
    }
    out
}

/// The PRODUCTION region: everything strictly before the first top-level
/// `#[cfg(test)]` marker. Robust to code after `mod tests` in the sense
/// that anything at-or-after the marker is EXCLUDED — a call site moved
/// below the test module can never satisfy this guard.
fn production_region(src: &str) -> &str {
    match src.find("#[cfg(test)]") {
        Some(pos) => &src[..pos],
        None => src,
    }
}

const VALIDATION_CALL: &str = "validate_cached_token_at_fast_boot(";
/// The `().await` suffix distinguishes CALL sites from the fn definition.
const COOLDOWN_CALL: &str = "wait_out_persisted_ws_rate_limit_cooldown().await";
/// Both pool CALL sites are `match create_websocket_pool(`; the fn
/// definition is `fn create_websocket_pool(` and never matches this needle.
const POOL_CALL: &str = "match create_websocket_pool(";

#[test]
fn ratchet_fast_boot_validation_precedes_pool_create_and_cooldown() {
    let full = read_main_rs();
    let stripped = strip_comments(&full);
    let production = production_region(&stripped);

    let validation_positions: Vec<usize> = production
        .match_indices(VALIDATION_CALL)
        .map(|(pos, _)| pos)
        .collect();
    assert_eq!(
        validation_positions.len(),
        1,
        "expected EXACTLY ONE `{VALIDATION_CALL}` call site in the main.rs \
         production region (one profile check per boot — AUTH-GAP-06); \
         found {} — a removed call re-opens the 2026-07-07 dead-cached-token \
         outage class; a second call violates the one-check contract",
        validation_positions.len()
    );
    let validation = validation_positions[0];

    let first_cooldown = production.find(COOLDOWN_CALL).unwrap_or_else(|| {
        panic!(
            "no `{COOLDOWN_CALL}` call in the main.rs production region — \
             the WS-GAP-08 wiring this guard orders against has moved; \
             update both ratchets together"
        )
    });
    let first_pool = production.find(POOL_CALL).unwrap_or_else(|| {
        panic!(
            "no `{POOL_CALL}` call in the main.rs production region — \
             the fast-arm pool site this guard orders against has moved; \
             update this ratchet"
        )
    });

    assert!(
        validation < first_cooldown,
        "AUTH-GAP-06 ordering broken: `{VALIDATION_CALL}` at byte {validation} \
         must precede the first `{COOLDOWN_CALL}` at byte {first_cooldown} — \
         the mandated fast-arm order is cached-token load → validation \
         (→ re-mint if rejected) → cooldown wait → create_websocket_pool"
    );
    assert!(
        validation < first_pool,
        "AUTH-GAP-06 ordering broken: `{VALIDATION_CALL}` at byte {validation} \
         must precede the first `{POOL_CALL}` at byte {first_pool} — the \
         cached token must be validated (and re-minted if Dhan rejected it) \
         BEFORE any WebSocket spawns"
    );
}

/// Stub-guard: the core helper must actually probe the profile endpoint
/// and reach the existing remint machinery — in CODE (comments stripped),
/// in its PRODUCTION region (its own `#[cfg(test)]` module excluded, so
/// the assertions below can never be satisfied by test literals).
#[test]
fn ratchet_validation_helper_calls_profile_endpoint_and_remint_machinery() {
    let full = read_core_helper();
    let stripped = strip_comments(&full);
    let production = production_region(&stripped).to_string();

    // Non-vacuity: the helper module must actually carry a test module so
    // the production split above excluded something.
    assert!(
        full.contains("#[cfg(test)]"),
        "fast_boot_validation.rs lost its #[cfg(test)] module — the \
         production-region split is scanning the whole file (vacuous)"
    );

    for (needle, why) in [
        (
            "DHAN_USER_PROFILE_PATH",
            "the probe must target GET /v2/profile via the shared constant",
        ),
        (
            ".send()",
            "the probe must actually issue the HTTP request (not a stub)",
        ),
        (
            "force_renewal(",
            "the Remint arm must re-mint via the EXISTING TokenManager \
             machinery (RESILIENCE-03 tripwire + Dhan mint semantics live \
             inside force_renewal/renew_with_fallback)",
        ),
        (
            "initialize_deferred(",
            "the Remint arm must construct the TokenManager via the \
             existing initialize_deferred (SSM fetch + client_id check)",
        ),
        (
            "classify_probe_result(",
            "the orchestrator must route through the unit-tested pure \
             classifier — not an ad-hoc inline match",
        ),
    ] {
        assert!(
            production.contains(needle),
            "fast_boot_validation.rs production region lost `{needle}` — {why}"
        );
    }
}

/// Self-test for the guard's own comment stripper: a URL scheme separator
/// (`://`) inside a string literal must NOT truncate the scan, while a
/// genuine `//` comment must be stripped.
#[test]
fn stripper_self_test_url_scheme_is_not_a_comment() {
    let url_line = r#"let u = "https://api.dhan.co/v2/profile"; let x = 1;"#;
    assert_eq!(
        code_portion(url_line),
        url_line,
        "stripper must not treat '://' as a comment start"
    );
    let commented = "let x = 1; // validate_cached_token_at_fast_boot( in a comment";
    assert_eq!(
        code_portion(commented),
        "let x = 1; ",
        "stripper must strip genuine comments"
    );
    // And the production-region split must actually cut at #[cfg(test)].
    let src = "fn a() {}\n#[cfg(test)]\nmod tests { fn b() {} }\n";
    assert_eq!(production_region(src), "fn a() {}\n");
}

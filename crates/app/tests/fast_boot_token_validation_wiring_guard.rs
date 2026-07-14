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
//! Pins (comments stripped so a comment mention can never satisfy a scan):
//!
//! 1.+2. RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
//!    retirement directive per websocket-connection-scope-lock.md
//!    "2026-07-13 Amendment" §B): the main.rs fast-arm ordering pins
//!    (validate → cooldown wait → create_websocket_pool) died with the
//!    fast arm, the WS-GAP-08 cooldown wait, and the pool — see the dated
//!    retirement block above the surviving stub-guard below.
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

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md "2026-07-13
// Amendment" §B): `ratchet_fast_boot_validation_precedes_pool_create_and_cooldown`
// pinned the FAST crash-recovery arm's ordering (cached-token load →
// validate_cached_token_at_fast_boot → WS-GAP-08 cooldown wait →
// create_websocket_pool). The fast arm, the cooldown wait, and the pool
// were ALL deleted with the lane — main.rs boots from cold via
// dhan_rest_stack (which mints/validates through the TokenManager
// machinery under the SSM lock), so the 2026-07-07 dead-cached-token
// outage class is structurally gone: no boot path trusts a cached token
// to spawn a market-data WS. The core `fast_boot_validation.rs` MODULE is
// retained un-consumed pending the Phase C auth-surface cleanup; the
// stub-guard below keeps pinning its internals so it cannot silently
// degrade before that decision lands.

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

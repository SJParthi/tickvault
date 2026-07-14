//! Source-scan ratchets for the Cluster F Dhan order-path error taxonomy,
//! the fail-closed order-readiness gate and the DH-904 backoff ladder.
//!
//! House pattern (`api_token_rotation_wiring_guard.rs` /
//! `sandbox_enforcement_guard.rs`): scan the PRODUCTION region only (everything
//! before the `#[cfg(test)]` module) with `//`-line-comments stripped, so a
//! comment or a test-module mention can never satisfy a production-wiring
//! needle. Five invariants are pinned forever:
//!
//! 1. Live place/modify/cancel go through the `*_with_policy` client wrappers
//!    (never the bare `api_client.<verb>_order(` call).
//! 2. In `place_order`, the readiness gate runs AFTER the circuit-breaker check
//!    and BEFORE the token fetch; `modify_order` also gates; `cancel_order`
//!    does NOT (R4 — cancel is exposure-reducing).
//! 3. The DH-904 ladder constants (10/20/40/80 = 150s, 4 attempts) and the
//!    ladder wiring (`compute_dh904_backoff` + `check_stop_all_latch`).
//! 4. The DATA-805 STOP-ALL cooldown is 60s (annexure rule 12) + comment pin.
//! 5. The readiness staleness/pre-market constant relations.
//!
//! Runbook: `.claude/rules/project/order-readiness-error-codes.md`.

use std::fs;
use std::path::PathBuf;

use tickvault_common::constants::{
    DATA_805_STOP_ALL_COOLDOWN_SECS, DH904_BACKOFF_SECS, DH904_MAX_RETRY_ATTEMPTS,
    ORDER_READINESS_MAX_AGE_SECS, ORDER_READINESS_PREMARKET_TRIGGER_SECS_OF_DAY_IST,
    ORDER_READINESS_REFRESH_INTERVAL_SECS,
};

fn read_repo_file(rel_from_trading: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel_from_trading);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display())) // APPROVED: test
}

/// Strip `//`-line-comments, treating `://` URL scheme separators as code (the
/// `http_client_fallback_guard.rs` precedent), so a comment mentioning a needle
/// can never satisfy the scan.
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

/// Production region only: everything before the `#[cfg(test)]` module.
fn production_region(body: &str) -> String {
    let cut = body.find("#[cfg(test)]").unwrap_or(body.len());
    strip_line_comments(&body[..cut])
}

/// Whitespace fully removed — so a method call split across lines by rustfmt is
/// still one contiguous token (`.api_client\n.place_order(` → `.api_client.place_order(`).
fn no_whitespace(body: &str) -> String {
    body.chars().filter(|c| !c.is_whitespace()).collect()
}

/// The `place_order` function body slice (from its `fn` to `modify_order`'s).
fn slice(body: &str, from: &str, to: &str) -> String {
    let start = body
        .find(from)
        .unwrap_or_else(|| panic!("marker not found: {from}"));
    let rest = &body[start..];
    let end = rest.find(to).map_or(rest.len(), |e| e);
    rest[..end].to_owned()
}

// ---------------------------------------------------------------------------
// Ratchet 1 — live order calls use the *_with_policy wrappers
// ---------------------------------------------------------------------------

#[test]
fn ratchet_engine_live_order_calls_use_with_policy_wrappers() {
    let prod = no_whitespace(&production_region(&read_repo_file("src/oms/engine.rs")));
    for wrapper in [
        ".api_client.place_order_with_policy(",
        ".api_client.modify_order_with_policy(",
        ".api_client.cancel_order_with_policy(",
    ] {
        assert!(
            prod.contains(wrapper),
            "the live order path must call {wrapper} (STOP-ALL latch + DH-904 ladder)"
        );
    }
    for bare in [
        ".api_client.place_order(",
        ".api_client.modify_order(",
        ".api_client.cancel_order(",
    ] {
        assert!(
            !prod.contains(bare),
            "the live path must NOT bypass the policy wrapper via {bare}"
        );
    }
}

// ---------------------------------------------------------------------------
// Ratchet 2 — gate is after the breaker + before the token fetch; cancel exempt
// ---------------------------------------------------------------------------

#[test]
fn ratchet_readiness_gate_after_circuit_breaker_before_token_fetch() {
    let prod = production_region(&read_repo_file("src/oms/engine.rs"));

    let place = slice(&prod, "async fn place_order(", "async fn modify_order(");
    let cb = place
        .find(".circuit_breaker.check")
        .expect("place_order must check the circuit breaker");
    let gate = place
        .find("check_live_order_gates(")
        .expect("place_order must run the readiness gate");
    let token = place
        .find(".get_access_token(")
        .expect("place_order must fetch the token");
    assert!(
        cb < gate && gate < token,
        "order must be circuit_breaker.check < check_live_order_gates < get_access_token \
         (got cb={cb}, gate={gate}, token={token})"
    );

    let modify = slice(&prod, "async fn modify_order(", "async fn cancel_order(");
    assert!(
        modify.contains("check_live_order_gates(OrderEndpoint::Modify"),
        "modify_order must run the readiness gate"
    );

    let cancel = slice(&prod, "async fn cancel_order(", "fn handle_order_update(");
    assert!(
        !cancel.contains("check_live_order_gates("),
        "cancel_order must be EXEMPT from the readiness/halt gate (R4)"
    );
}

// ---------------------------------------------------------------------------
// Ratchet 3 — DH-904 ladder constants + wiring
// ---------------------------------------------------------------------------

#[test]
fn ratchet_dh904_ladder_constants_and_wiring() {
    assert_eq!(
        DH904_BACKOFF_SECS.iter().sum::<u64>(),
        150,
        "DH-904 ladder must total 150s (10+20+40+80)"
    );
    assert_eq!(
        DH904_MAX_RETRY_ATTEMPTS,
        DH904_BACKOFF_SECS.len(),
        "DH904_MAX_RETRY_ATTEMPTS must equal the ladder length"
    );

    let prod = production_region(&read_repo_file("src/oms/api_client.rs"));
    assert!(
        prod.contains("fn run_order_ladder"),
        "api_client must define run_order_ladder"
    );
    for needle in ["compute_dh904_backoff(", "check_stop_all_latch("] {
        assert!(
            prod.contains(needle),
            "run_order_ladder wiring must call {needle}"
        );
    }
}

// ---------------------------------------------------------------------------
// Ratchet 4 — DATA-805 STOP-ALL cooldown = 60s (annexure rule 12)
// ---------------------------------------------------------------------------

#[test]
fn ratchet_stop_all_cooldown_is_60s_per_annexure_rule_12() {
    assert_eq!(
        DATA_805_STOP_ALL_COOLDOWN_SECS, 60,
        "DATA-805 STOP-ALL cooldown must be 60s per annexure rule 12"
    );
    let consts = read_repo_file("../common/src/constants.rs");
    assert!(
        consts.contains("DATA_805_STOP_ALL_COOLDOWN_SECS") && consts.contains("STOP ALL"),
        "the STOP-ALL cooldown const must carry the annexure-rule-12 STOP-ALL comment"
    );
}

// ---------------------------------------------------------------------------
// Ratchet 5 — readiness staleness / pre-market constant relations
// ---------------------------------------------------------------------------

#[test]
fn ratchet_readiness_staleness_constants_relation() {
    assert!(
        ORDER_READINESS_MAX_AGE_SECS > 2 * ORDER_READINESS_REFRESH_INTERVAL_SECS,
        "the staleness bound must exceed two full re-probe intervals (fail-closed)"
    );
    assert_eq!(
        ORDER_READINESS_PREMARKET_TRIGGER_SECS_OF_DAY_IST, 31_500,
        "the pre-market readiness probe must trigger at 08:45 IST (31,500 secs-of-day)"
    );
}

// ---------------------------------------------------------------------------
// Self-test — the comment stripper actually strips a `//` needle but keeps `://`
// ---------------------------------------------------------------------------

#[test]
fn comment_stripper_self_test() {
    let stripped =
        strip_line_comments("let x = 1; // .api_client.place_order(\nlet u = \"http://x\";\n");
    assert!(
        !stripped.contains(".api_client.place_order("),
        "a needle inside a // comment must be stripped"
    );
    assert!(
        stripped.contains("http://x"),
        "a :// URL scheme separator must be treated as code, not a comment"
    );
}

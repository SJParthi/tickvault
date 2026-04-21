//! Ratchet: pre-market HALT must embed `/v2/profile` + `/v2/ip/getIP`
//! diagnostics directly in the CRITICAL Telegram message (queue item I12).
//!
//! # Why (2026-04-21)
//!
//! Before I12, when the pre-market `pre_market_check` failed the
//! Telegram alert told the operator to "run
//! `curl -H access-token: $TOKEN https://api.dhan.co/v2/profile`"
//! — a manual step performed under time pressure with the 09:15 IST
//! market-open deadline ticking. I12 moves that step INTO the boot
//! sequence: on HALT, we fetch both endpoints automatically and
//! embed the (redacted) responses in the same Telegram message.
//!
//! These ratchets source-scan `main.rs` so the auto-diagnostic
//! plumbing cannot be accidentally removed.

use std::fs;
use std::path::PathBuf;

fn read(rel: &str) -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

#[test]
fn main_rs_calls_build_pre_market_diagnostics_on_halt() {
    let src = read("crates/app/src/main.rs");
    assert!(
        src.contains("build_pre_market_diagnostics"),
        "main.rs HALT branch MUST call build_pre_market_diagnostics so \
         the Telegram message carries /v2/profile + /v2/ip/getIP \
         responses alongside the failure reason. Regression = operator \
         must run curl manually, wasting minutes during market-open \
         pressure."
    );
}

#[test]
fn main_rs_fetches_both_profile_and_ip_endpoints() {
    let src = read("crates/app/src/main.rs");
    assert!(
        src.contains("get_user_profile()"),
        "build_pre_market_diagnostics MUST call token_manager.get_user_profile() \
         so the dataPlan + activeSegment + tokenValidity land in the \
         Telegram message."
    );
    assert!(
        src.contains("ip_verifier::get_ip("),
        "build_pre_market_diagnostics MUST call ip_verifier::get_ip so \
         the IP allowlist state + ipFlag + modifyDatePrimary land in \
         the Telegram message. Without this, the operator can't \
         distinguish a dataPlan issue from an IP issue."
    );
}

#[test]
fn main_rs_redacts_ip_in_diagnostic_output() {
    let src = read("crates/app/src/main.rs");
    assert!(
        src.contains("redact_ip_last_octet"),
        "build_pre_market_diagnostics MUST redact the raw IP (keep only \
         the last octet) before embedding in Telegram. Full IPs in \
         Telegram chat history = leak vector."
    );
}

#[test]
fn main_rs_diagnostic_summary_has_snapshot_header() {
    let src = read("crates/app/src/main.rs");
    assert!(
        src.contains("--- Diagnostic snapshot (auto-fetched) ---"),
        "build_pre_market_diagnostics MUST include the header line so \
         the operator can visually separate the auto-fetched diagnostic \
         from the upstream failure reason in the Telegram message."
    );
}

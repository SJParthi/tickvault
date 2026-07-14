//! Ratchet test: pre-market profile check HALTS the boot during market
//! hours and fires CRITICAL Telegram regardless of when the failure is
//! detected.
//!
//! # Why this exists (Parthiban directive 2026-04-21)
//!
//! On 2026-04-21 the app booted, WebSocket reported "connected" on all
//! 5 connections, but Dhan was not streaming data during market hours.
//! The operator discovered hours later via Grafana. Root cause was
//! almost certainly a dataPlan / activeSegment / token-validity issue
//! on the Dhan side — but the app had no HALT gate, so it kept running
//! with a broken configuration.
//!
//! This PR wires the existing `pre_market_check` into a proper HALT +
//! Telegram escalation:
//!
//! | Time window | Failure action |
//! |---|---|
//! | Off-hours / non-trading | skip check |
//! | 08:00–09:14 IST | CRITICAL Telegram, boot continues |
//! | 09:15–15:30 IST | CRITICAL Telegram + HALT boot |
//!
//! These ratchets scan the source so the behaviour cannot regress
//! silently. Complementary to `crates/core/src/auth/token_manager.rs`
//! unit tests that validate the check's decision logic.

use std::fs;
use std::path::PathBuf;

fn repo_path(rel: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push(rel);
    p
}

fn read(rel: &str) -> String {
    let path = repo_path(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn events_rs_declares_pre_market_profile_check_failed() {
    let src = read("crates/core/src/notification/events.rs");
    assert!(
        src.contains("PreMarketProfileCheckFailed"),
        "events.rs must declare PreMarketProfileCheckFailed variant \
         — without it, the halt event has no Telegram message."
    );
    assert!(
        src.contains("within_market_hours: bool"),
        "PreMarketProfileCheckFailed must carry within_market_hours \
         so the Telegram message can distinguish 'HALT fired' from \
         'operator has 75min'."
    );
}

#[test]
fn pre_market_profile_check_failed_is_critical_severity() {
    let src = read("crates/core/src/notification/events.rs");
    assert!(
        src.contains("Self::PreMarketProfileCheckFailed { .. } => Severity::Critical"),
        "PreMarketProfileCheckFailed MUST be Critical severity — the \
         operator MUST be paged immediately. Downgrading this to High \
         or Medium means the page might be delayed or lost in SNS quiet \
         hours policies."
    );
}

#[test]
fn pre_market_profile_check_failed_to_message_mentions_diagnostics() {
    let src = read("crates/core/src/notification/events.rs");
    // The message must guide the operator to the exact curl commands
    // they need to run. Without this, the operator has to remember the
    // Dhan API layout under pressure.
    assert!(
        src.contains("dataPlan == \\\"Active\\\""),
        "PreMarketProfileCheckFailed message must tell the operator to \
         check dataPlan — else the operator has to re-read the Dhan \
         docs under pressure."
    );
    assert!(
        src.contains("Derivative"),
        "PreMarketProfileCheckFailed message must call out the \
         Derivative segment check."
    );
    assert!(
        src.contains("BOOT HALTED"),
        "PreMarketProfileCheckFailed message must distinguish the \
         HALT branch from the pre-market warn-only branch."
    );
}

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md "2026-07-13
// Amendment" §B): the three main.rs pins
// (`main_rs_halts_on_profile_failure_during_market_hours`,
// `main_rs_fires_critical_telegram_on_profile_failure`,
// `main_rs_covers_both_pre_market_and_market_hours_windows`) died with the
// lane's boot-time pre-market gate — there is no market-data boot to HALT,
// so the "boots with a bad profile → silent data loss" class is
// structurally gone. The SURVIVING surface for the 2026-04-21 incident
// class is the 900s mid-session profile watchdog
// (`crates/core/src/auth/mid_session_watchdog.rs` — calls
// `pre_market_check`, emits `PreMarketProfileCheckFailed` CRITICAL,
// never halts), spawned by `dhan_rest_stack` and pinned by the re-pointed
// token_health guard in `crates/core/src/auth/secret_manager.rs`. The
// events.rs variant/severity/message pins above stay live.

//! Mutation-killer tests for `tickvault-core`.
//!
//! Targets the Stage-C primitives that `cargo-mutants` is most
//! likely to silently break:
//!
//!   - `ActivityWatchdog` threshold constants (off-by-one would
//!     let the watchdog fire before Dhan's server ping timeout,
//!     causing false-positive reconnects)
//!   - `WebSocketError::WatchdogFired` structural fields (a swap
//!     between `silent_secs` and `threshold_secs` would produce
//!     a misleading alert)
//!   - `DisconnectCode` reconnectability classification (a mutant
//!     flipping `is_reconnectable` on code 805 would cause the
//!     pool to reconnect into a ban window)
//!
//! Companion to `crates/storage/tests/mutation_killer.rs`.

#![cfg(test)]
// Every `assert!` in this file tests a CONSTANT relationship between
// compile-time constants. Clippy's `assertions_on_constants` lint fires
// on those — but that's the WHOLE POINT of a mutation killer: the
// assertion exists so a mutant that flips a constant is caught at
// test time. Suppressing the lint is intentional.
#![allow(clippy::assertions_on_constants)] // APPROVED: constant-relationship mutation killers

use tickvault_core::websocket::activity_watchdog::{
    WATCHDOG_POLL_INTERVAL_SECS, WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS,
    WATCHDOG_THRESHOLD_ORDER_UPDATE_SECS,
};
use tickvault_core::websocket::types::{DisconnectCode, WebSocketError};

// ============================================================================
// Watchdog threshold constants — mutation targets: ±1, *2, /2, swap
// ============================================================================

#[test]
fn mutation_watchdog_threshold_live_and_depth_is_exactly_50_not_40_or_60() {
    // Dhan server timeout is 40s. Watchdog must be > 40 so it never
    // fires before the server itself would close the socket. Must
    // be < 60 so it catches truly-dead sockets promptly.
    assert_eq!(WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS, 50);
    assert!(WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS > 40);
    assert!(WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS < 60);
}

#[test]
fn mutation_watchdog_threshold_order_update_is_exactly_660_not_600() {
    // 600s tolerance + 60s safety margin. A mutant using 600 (the
    // tolerance alone) would let the watchdog fire during legitimate
    // no-order windows, spamming Telegram.
    assert_eq!(WATCHDOG_THRESHOLD_ORDER_UPDATE_SECS, 660);
    assert!(WATCHDOG_THRESHOLD_ORDER_UPDATE_SECS > 600);
}

#[test]
fn mutation_watchdog_thresholds_are_strictly_ordered_live_lt_order() {
    // Swap mutation check — live-feed must be SHORTER than order-update.
    assert!(WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS < WATCHDOG_THRESHOLD_ORDER_UPDATE_SECS);
}

#[test]
fn mutation_watchdog_poll_interval_is_strictly_less_than_threshold() {
    // Poll interval must give every threshold at least 2 sample
    // windows. A mutant that sets poll >= threshold would cause
    // the watchdog to miss transient stalls.
    assert!(WATCHDOG_POLL_INTERVAL_SECS < WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS);
    assert!(WATCHDOG_POLL_INTERVAL_SECS * 2 <= WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS);
}

// ============================================================================
// WebSocketError::WatchdogFired — mutation targets: field swap, wrong type
// ============================================================================

#[test]
fn mutation_watchdog_fired_error_preserves_label_silent_and_threshold() {
    let err = WebSocketError::WatchdogFired {
        label: "live-feed-3".to_string(),
        silent_secs: 55,
        threshold_secs: 50,
    };
    let msg = format!("{err}");
    // A mutant swapping silent_secs and threshold_secs in the Display
    // impl would show "55s of silence (threshold=50s)" vs
    // "50s of silence (threshold=55s)" — materially wrong alert.
    assert!(msg.contains("live-feed-3"), "label must be in error msg");
    assert!(msg.contains("55s of silence"), "silent_secs must render");
    assert!(
        msg.contains("threshold=50s"),
        "threshold_secs must render correctly: {msg}"
    );
}

// ============================================================================
// DisconnectCode — mutation targets: bool flip, code arithmetic
// ============================================================================

#[test]
fn mutation_disconnect_code_805_is_not_reconnectable() {
    // Code 805 = too many connections / rate limit. Reconnecting
    // immediately would make things WORSE (deeper into the ban
    // window). A mutant flipping this to true would cause a storm.
    let code = DisconnectCode::from_u16(805);
    assert!(
        !code.is_reconnectable(),
        "805 (TooManyRequests) MUST NOT be reconnectable"
    );
}

#[test]
fn mutation_disconnect_code_807_is_reconnectable_and_requires_token_refresh() {
    // 807 = AccessTokenExpired. Must reconnect AFTER a token
    // refresh. A mutant dropping the refresh path would cause an
    // infinite 807 loop.
    let code = DisconnectCode::from_u16(807);
    assert!(code.is_reconnectable());
    assert!(
        code.requires_token_refresh(),
        "807 MUST trigger token refresh"
    );
}

#[test]
fn mutation_disconnect_code_800_is_reconnectable_and_no_token_refresh() {
    // 800 = InternalServerError. Reconnect without token refresh.
    let code = DisconnectCode::from_u16(800);
    assert!(code.is_reconnectable());
    assert!(
        !code.requires_token_refresh(),
        "800 MUST NOT trigger token refresh"
    );
}

#[test]
fn mutation_disconnect_code_808_is_not_reconnectable() {
    // 808 = AuthenticationFailed. No amount of reconnecting fixes
    // a bad token. Must NOT be reconnectable.
    let code = DisconnectCode::from_u16(808);
    assert!(!code.is_reconnectable());
}

#[test]
fn mutation_disconnect_code_unknown_is_reconnectable_default() {
    // Unknown codes default to reconnectable (conservative). A
    // mutant flipping this to non-reconnectable would cause silent
    // permanent disconnects on any new Dhan code.
    let code = DisconnectCode::from_u16(9999);
    assert!(code.is_reconnectable());
}

#[test]
fn mutation_disconnect_code_from_u16_exact_roundtrip() {
    // Mutation target: off-by-one in code lookup. Every known code
    // must roundtrip u16 → variant → u16 exactly.
    for raw in [800, 804, 805, 806, 807, 808, 809, 810, 811, 812, 813, 814] {
        let code = DisconnectCode::from_u16(raw);
        assert_eq!(
            code.as_u16(),
            raw,
            "roundtrip failed for code {raw}: got {}",
            code.as_u16()
        );
    }
}

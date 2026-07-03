//! PR #458 ratchet — `WebSocketPoolOnline` MUST NOT fire if any
//! connection is not in `ConnectionState::Connected`.
//!
//! Pre-PR #458, `emit_websocket_connected_alerts` reported
//! `connected: handle_count, total: handle_count` — the SPAWN count,
//! not the count of actually-connected sockets. That was a false-OK:
//! the operator received "WS pool online: 5/5 connected" Telegram
//! while no Dhan handshake had completed yet. PR #458 rewires the
//! emit logic to poll `pool.health()` and emit `WebSocketPoolOnline`
//! ONLY when every slot has reached `Connected`.
//!
//! This test ratchets that contract on the format-string contract:
//! given a fully-connected pool snapshot, the message says "live"; given
//! a partial snapshot via `WebSocketPoolPartialAfterDeadline`, the
//! message says "partially online" with stuck-slot breakdown. A future
//! regression that swaps these wordings or merges the two variants will
//! fail this test.
//!
//! Run: `cargo test -p tickvault-app --test ws_pool_online_truthful_emit_guard`

use tickvault_core::notification::events::{BootPathLabel, NotificationEvent};

#[test]
fn pool_online_message_for_fully_connected_pool_says_live() {
    let event = NotificationEvent::WebSocketPoolOnline {
        connected: 5,
        total: 5,
        per_connection: vec![
            (5_000, 5_000, Some(1)),
            (5_000, 5_000, Some(1)),
            (5_000, 5_000, Some(2)),
            (5_000, 5_000, Some(2)),
            (4_324, 5_000, Some(2)),
        ],
        boot_path: BootPathLabel::Slow,
        boot_wall_clock_secs: 11.2,
        last_real_tick_age_secs: Some(3),
        feeds: vec![],
    };
    let msg = event.to_message();
    assert!(
        msg.contains("✅") && msg.contains("live"),
        "WebSocketPoolOnline must signal verdict + liveness in the first \
         line; got: {msg}"
    );
    assert!(
        msg.contains("All 5 of 5"),
        "header MUST report connected/total = 5/5 from the truthful \
         payload; got: {msg}"
    );
    assert!(
        msg.contains("24,324"),
        "comma-formatted aggregate subscription count is required; got: \
         {msg}"
    );
    assert!(
        msg.contains("Normal start"),
        "boot_path must be surfaced in human form; got: {msg}"
    );
}

#[test]
fn pool_partial_after_deadline_says_partially_online() {
    let event = NotificationEvent::WebSocketPoolPartialAfterDeadline {
        connected: 3,
        total: 5,
        per_connection: vec![
            (5_000, 5_000, Some(1)),
            (5_000, 5_000, Some(2)),
            (4_847, 5_000, Some(3)),
            (0, 5_000, None),
            (0, 5_000, None),
        ],
        stuck: vec![
            (3, "state=Connecting".to_string()),
            (4, "state=Disconnected".to_string()),
        ],
        boot_path: BootPathLabel::Slow,
    };
    let msg = event.to_message();
    assert!(
        msg.contains("⚠️") && msg.contains("partially online"),
        "PartialAfterDeadline must signal warning + partial wording; \
         got: {msg}"
    );
    assert!(
        msg.contains("3 of 5"),
        "header MUST report connected/total = 3/5 from the truthful \
         payload; got: {msg}"
    );
    assert!(
        msg.contains("Working feeds:") && msg.contains("Broken feeds:"),
        "PartialAfterDeadline must split active vs stuck rows; got: {msg}"
    );
    assert!(
        msg.contains("state=Connecting") && msg.contains("state=Disconnected"),
        "stuck reasons must surface verbatim; got: {msg}"
    );
}

#[test]
fn pool_online_severity_is_low_partial_is_high() {
    // 2026-05-09: WebSocketPoolOnline demoted Medium → Low so the boot
    // success summary renders green ✅. Dispatch is still immediate via
    // the new `DispatchPolicy::Immediate` mechanism (decoupled from
    // severity color). The partial-after-deadline event stays High
    // because it IS a degraded state requiring operator attention.
    use tickvault_core::notification::events::{DispatchPolicy, Severity};
    let online = NotificationEvent::WebSocketPoolOnline {
        connected: 5,
        total: 5,
        per_connection: vec![(5_000, 5_000, Some(1)); 5],
        boot_path: BootPathLabel::Slow,
        boot_wall_clock_secs: 10.0,
        last_real_tick_age_secs: Some(1),
        feeds: vec![],
    };
    let partial = NotificationEvent::WebSocketPoolPartialAfterDeadline {
        connected: 0,
        total: 5,
        per_connection: vec![(0, 5_000, None); 5],
        stuck: vec![],
        boot_path: BootPathLabel::Slow,
    };
    assert_eq!(online.severity(), Severity::Low);
    assert_eq!(online.dispatch_policy(), DispatchPolicy::Immediate);
    assert_eq!(partial.severity(), Severity::High);
}

#[test]
fn ws_connected_payload_renders_capacity_percent_and_ping() {
    let event = NotificationEvent::WebSocketConnected {
        connection_index: 0,
        subscribed_count: 5_000,
        capacity: 5_000,
        last_activity_secs_ago: Some(1),
    };
    let msg = event.to_message();
    assert!(
        msg.contains("Feed 1"),
        "1-indexed Feed N display required; got: {msg}"
    );
    assert!(
        msg.contains("100% full") || msg.contains("100.0% full") || msg.contains("100%"),
        "100% capacity rendering required; got: {msg}"
    );
    assert!(
        msg.contains("1s ago ✓"),
        "ping freshness checkmark required for fresh frames; got: {msg}"
    );
}

#[test]
fn ws_connected_renders_no_data_yet_when_first_frame_pending() {
    let event = NotificationEvent::WebSocketConnected {
        connection_index: 4,
        subscribed_count: 4_324,
        capacity: 5_000,
        last_activity_secs_ago: None,
    };
    let msg = event.to_message();
    assert!(
        msg.contains("Feed 5"),
        "1-indexed Feed N display required; got: {msg}"
    );
    assert!(
        msg.contains("—"),
        "em-dash required when no first frame received yet; got: {msg}"
    );
}

#[test]
fn boot_path_label_human_strings_are_distinct() {
    assert_ne!(BootPathLabel::Slow.human(), BootPathLabel::Fast.human());
    assert!(BootPathLabel::Slow.human().contains("Normal"));
    assert!(BootPathLabel::Fast.human().contains("crash recovery"));
}

// 2026-06-02 false-OK fix: the "live and ready" pool-online message must report
// REAL-tick freshness (not raw frame freshness, which counts Dhan keep-alive
// pings). These ratchets pin both paths so a future edit cannot silently drop
// the real-tick line and let "live" look green on a ping-only feed.
#[test]
fn pool_online_shows_real_tick_age_when_ticks_flowing() {
    let event = NotificationEvent::WebSocketPoolOnline {
        connected: 1,
        total: 1,
        per_connection: vec![(243, 5_000, Some(0))],
        boot_path: BootPathLabel::Slow,
        boot_wall_clock_secs: 0.9,
        last_real_tick_age_secs: Some(2),
        feeds: vec![],
    };
    let msg = event.to_message();
    assert!(
        msg.contains("Real market ticks: last one 2s ago"),
        "pool-online must report real-tick freshness when ticks are flowing; got: {msg}"
    );
}

#[test]
fn pool_online_warns_when_no_real_ticks_only_pings() {
    let event = NotificationEvent::WebSocketPoolOnline {
        connected: 1,
        total: 1,
        // Frame freshness Some(0) ("0s ago") would look healthy, but zero real
        // ticks => the message MUST warn, not read green.
        per_connection: vec![(243, 5_000, Some(0))],
        boot_path: BootPathLabel::Slow,
        boot_wall_clock_secs: 0.9,
        last_real_tick_age_secs: None,
        feeds: vec![],
    };
    let msg = event.to_message();
    assert!(
        msg.contains("NONE captured yet") && msg.contains("keep-alive pings"),
        "pool-online must warn (not look green) when zero real ticks captured; got: {msg}"
    );
}

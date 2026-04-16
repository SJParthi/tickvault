//! P4 + P5 — Proof tests that notification events are factual and include identifiers.
//!
//! These tests verify the **contracts** from the zero-loss plan:
//! - QuestDB alert text carries real buffer data, never hardcoded zeros
//! - QuestDB alert text does not promise misleading auto-reconnect cadence
//! - All 4 WS notification events include identifiers (connection_id, security_id, etc.)
//! - Depth-200 events include security_id for Dhan support correlation

use tickvault_core::notification::events::NotificationEvent;

// ─── P4.3: QuestDB alert carries real buffer size ───

/// Verify that `QuestDbDisconnected` alert includes the actual `signal` value
/// (never hardcoded to 0) and a `signal_kind` description.
#[test]
fn test_questdb_alert_carries_real_buffer_size() {
    let event = NotificationEvent::QuestDbDisconnected {
        writer: "tick".to_string(),
        signal: 42,
        signal_kind: "consecutive liveness failures".to_string(),
    };
    let msg = event.to_message();

    // Signal value must appear in the message — proves it's not hardcoded 0.
    assert!(
        msg.contains("42"),
        "Alert message must contain the actual signal value (42), got: {msg}"
    );
    assert!(
        msg.contains("consecutive liveness failures"),
        "Alert message must contain signal_kind, got: {msg}"
    );
    // Must NOT contain hardcoded "buffer_size: 0" from old code.
    assert!(
        !msg.contains("buffer_size: 0"),
        "Alert must not contain hardcoded 'buffer_size: 0', got: {msg}"
    );
}

// ─── P4.4: QuestDB alert does not promise misleading auto-reconnect ───

/// The old alert said "Auto-reconnect every 30s" which was a lie when the
/// writer's actual reconnect cadence was different. This test ensures the
/// alert text does NOT contain that false promise.
#[test]
fn test_questdb_alert_text_no_misleading_auto_reconnect_30s() {
    let event = NotificationEvent::QuestDbDisconnected {
        writer: "tick".to_string(),
        signal: 3,
        signal_kind: "consecutive liveness failures".to_string(),
    };
    let msg = event.to_message();

    assert!(
        !msg.to_lowercase().contains("auto-reconnect"),
        "Alert must not promise auto-reconnect cadence, got: {msg}"
    );
    assert!(
        !msg.contains("30s"),
        "Alert must not mention hardcoded 30s cadence, got: {msg}"
    );
}

// ─── P4.4: QuestDB disconnected alert text is factual ───

/// The alert must be factual: mention the writer, the signal, and actionable
/// investigation steps. No hardcoded lies, no false reassurance.
#[test]
fn test_questdb_disconnected_alert_text_is_factual() {
    let event = NotificationEvent::QuestDbDisconnected {
        writer: "depth".to_string(),
        signal: 7,
        signal_kind: "ticks dropped by broadcast lag".to_string(),
    };
    let msg = event.to_message();

    // Must mention the writer name.
    assert!(
        msg.contains("depth"),
        "Alert must mention writer name, got: {msg}"
    );
    // Must mention the signal value.
    assert!(
        msg.contains("7"),
        "Alert must mention signal value, got: {msg}"
    );
    // Must mention the signal kind.
    assert!(
        msg.contains("ticks dropped by broadcast lag"),
        "Alert must mention signal_kind, got: {msg}"
    );
    // Must mention WAL durability (so operator knows ticks aren't lost).
    assert!(
        msg.to_lowercase().contains("wal") || msg.to_lowercase().contains("durable"),
        "Alert must mention WAL durability guarantee, got: {msg}"
    );
    // Must mention investigation steps.
    assert!(
        msg.to_lowercase().contains("investigate") || msg.to_lowercase().contains("check"),
        "Alert must include investigation guidance, got: {msg}"
    );
}

// ─── P5.1/P5.2: Depth-200 events include security_id ───

/// Depth-200 connected event must include security_id in both the struct
/// and the Telegram message text, so operators can quote the exact instrument
/// when escalating to Dhan support.
#[test]
fn test_depth_200_logs_include_security_id_on_connect() {
    let event = NotificationEvent::DepthTwoHundredConnected {
        contract: "NIFTY-Apr2026-22500-CE".to_string(),
        security_id: 54816,
    };
    let msg = event.to_message();

    assert!(
        msg.contains("54816"),
        "Depth-200 connected message must include security_id, got: {msg}"
    );
    assert!(
        msg.contains("NIFTY-Apr2026-22500-CE"),
        "Depth-200 connected message must include contract label, got: {msg}"
    );
}

/// Depth-200 disconnected event must include security_id AND the reason.
#[test]
fn test_depth_200_disconnected_includes_security_id_and_reason() {
    let event = NotificationEvent::DepthTwoHundredDisconnected {
        contract: "BANKNIFTY-Apr2026-48000-PE".to_string(),
        security_id: 54817,
        reason: "TCP ResetWithoutClosingHandshake".to_string(),
    };
    let msg = event.to_message();

    assert!(
        msg.contains("54817"),
        "Must include security_id, got: {msg}"
    );
    assert!(
        msg.contains("BANKNIFTY-Apr2026-48000-PE"),
        "Must include contract label, got: {msg}"
    );
    assert!(
        msg.contains("ResetWithoutClosingHandshake"),
        "Must include disconnect reason, got: {msg}"
    );
}

// ─── P5.6: All 4 WS notification events include identifiers ───

/// Every WebSocket type's connected/disconnected notification must carry
/// enough context for the operator to identify the exact connection from
/// the Telegram alert alone.
#[test]
fn test_all_4_ws_notification_events_include_identifiers() {
    // Live Market Feed — connection_index.
    let live_disc = NotificationEvent::WebSocketDisconnected {
        connection_index: 3,
        reason: "transport error".to_string(),
    };
    let msg = live_disc.to_message();
    assert!(
        msg.contains("3") || msg.contains("#3"),
        "Live feed disconnect must include connection_index, got: {msg}"
    );

    // Depth 20 — underlying.
    let depth20_disc = NotificationEvent::DepthTwentyDisconnected {
        underlying: "NIFTY".to_string(),
        reason: "code 805".to_string(),
    };
    let msg = depth20_disc.to_message();
    assert!(
        msg.contains("NIFTY"),
        "Depth-20 disconnect must include underlying, got: {msg}"
    );

    // Depth 200 — contract + security_id.
    let depth200_disc = NotificationEvent::DepthTwoHundredDisconnected {
        contract: "NIFTY-ATM-CE".to_string(),
        security_id: 63422,
        reason: "TCP reset".to_string(),
    };
    let msg = depth200_disc.to_message();
    assert!(
        msg.contains("63422"),
        "Depth-200 disconnect must include security_id, got: {msg}"
    );

    // Order Update — reason classification.
    let ou_disc = NotificationEvent::OrderUpdateDisconnected {
        reason: "auth failure: DH-901".to_string(),
    };
    let msg = ou_disc.to_message();
    assert!(
        msg.contains("DH-901"),
        "Order update disconnect must include classified reason, got: {msg}"
    );
}

// ─── QuestDB reconnected event is factual ───

/// QuestDB reconnected alert must include the writer name and drained count.
#[test]
fn test_questdb_reconnected_event_includes_drained_count() {
    let event = NotificationEvent::QuestDbReconnected {
        writer: "tick".to_string(),
        drained_count: 1500,
    };
    let msg = event.to_message();

    assert!(
        msg.contains("1500"),
        "Must include drained count, got: {msg}"
    );
    assert!(msg.contains("tick"), "Must include writer name, got: {msg}");
}

// ─── WebSocket reconnection exhausted includes attempts ───

#[test]
fn test_ws_reconnection_exhausted_includes_connection_and_attempts() {
    let event = NotificationEvent::WebSocketReconnectionExhausted {
        connection_index: 2,
        attempts: 10,
    };
    let msg = event.to_message();

    assert!(
        msg.contains("2") || msg.contains("#2"),
        "Must include connection index, got: {msg}"
    );
    assert!(msg.contains("10"), "Must include attempt count, got: {msg}");
}

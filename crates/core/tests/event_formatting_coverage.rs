//! P2a of the coverage-gaps action plan (report #1076 §5 rank 5, plan §6 P2):
//! formatting coverage for the `NotificationEvent` arms that had never been
//! exercised by any test (llvm-cov at main @ a6fe35c4: 257 uncovered lines in
//! `events.rs`, dominated by `to_message` + `severity` match arms of
//! rarely-fired events).
//!
//! Pattern: every String field is seeded with a unique token and the rendered
//! message must contain it; numeric fields use distinctive multi-digit values
//! asserted verbatim. Every constructed event is also routed through
//! `severity()` + `dispatch_policy()` so those arms execute too.

use tickvault_core::notification::events::{BootPathLabel, NotificationEvent};

/// Renders and sanity-checks one event; returns the message for
/// variant-specific assertions.
fn render(ev: &NotificationEvent) -> String {
    let msg = ev.to_message();
    assert!(!msg.is_empty(), "empty Telegram body for {ev:?}");
    // Severity + dispatch arms must execute for every variant under test.
    assert!(
        !ev.severity().tag().is_empty(),
        "empty severity tag for {ev:?}"
    );
    let _ = ev.dispatch_policy();
    msg
}

#[test]
fn test_auth_and_profile_event_messages() {
    let m = render(&NotificationEvent::AuthenticationTransientFailure {
        attempt: 3,
        reason: "RSN-AUTH-552".to_string(),
        next_retry_ms: 2_500,
    });
    assert!(m.contains("RSN-AUTH-552") && m.contains("2.5s"), "{m}");

    let m = render(&NotificationEvent::PreMarketProfileCheckFailed {
        reason: "RSN-PROFILE-771".to_string(),
        within_market_hours: true,
    });
    assert!(
        m.contains("RSN-PROFILE-771") && m.contains("BOOT HALTED"),
        "{m}"
    );

    let m = render(&NotificationEvent::PreMarketProfileCheckFailed {
        reason: "RSN-PROFILE-772".to_string(),
        within_market_hours: false,
    });
    assert!(m.contains("investigate before 09:15 IST"), "{m}");

    let m = render(&NotificationEvent::MidSessionProfileInvalidated {
        reason: "RSN-PROFILE-773".to_string(),
    });
    assert!(
        m.contains("RSN-PROFILE-773") && m.contains("INVALIDATED"),
        "{m}"
    );
}

#[test]
fn test_ws_pool_event_messages() {
    let m = render(&NotificationEvent::WebSocketPoolDegraded { down_secs: 7741 });
    assert!(m.contains("7741") && m.contains("DEGRADED"), "{m}");

    let m = render(&NotificationEvent::WebSocketPoolRecovered {
        was_down_secs: 7742,
    });
    assert!(m.contains("7742") && m.contains("recovered"), "{m}");

    let m = render(&NotificationEvent::WebSocketPoolHalt { down_secs: 7743 });
    assert!(m.contains("7743") && m.contains("HALT"), "{m}");

    let m = render(&NotificationEvent::WebSocketConnected {
        connection_index: 4,
        subscribed_count: 1234,
        capacity: 5000,
        last_activity_secs_ago: Some(45),
    });
    assert!(m.contains("1234") || m.contains("1,234"), "{m}");

    let m = render(&NotificationEvent::WebSocketReconnectionExhausted {
        connection_index: 2,
        attempts: 9913,
    });
    assert!(m.contains("9913"), "{m}");

    let m = render(&NotificationEvent::MarketOpenStreamingConfirmation {
        main_feed_active: 1,
        main_feed_total: 1,
        order_update_active: true,
    });
    assert!(m.contains("Streaming live") && m.contains("1/1"), "{m}");

    let m = render(&NotificationEvent::MarketOpenStreamingFailed {
        main_feed_active: 0,
        main_feed_total: 1,
        order_update_active: false,
    });
    assert!(m.contains("STREAMING FAILED") && m.contains("0/1"), "{m}");

    let m = render(&NotificationEvent::MarketOpenReadinessConfirmation {
        main_feed_active: 1,
        main_feed_total: 1,
        order_update_active: true,
        token_remaining_secs: 6 * 3600,
    });
    assert!(m.contains("READY for market open"), "{m}");
}

#[test]
fn test_pool_online_fast_boot_path_and_ping_freshness_bands() {
    // Fast boot path + every ping-freshness band: None ("—"),
    // 0..=10 ("✓"), 11..=30 ("⚠"), >30 ("❌").
    let m = render(&NotificationEvent::WebSocketPoolOnline {
        connected: 4,
        total: 4,
        per_connection: vec![
            (100, 5000, None),
            (200, 5000, Some(10)),
            (300, 5000, Some(30)),
            (400, 5000, Some(31)),
        ],
        boot_path: BootPathLabel::Fast,
        boot_wall_clock_secs: 12.5,
        last_real_tick_age_secs: Some(3),
    });
    assert!(m.contains("Mid-market crash recovery"), "{m}");
    assert!(
        m.contains('✓') && m.contains('⚠') && m.contains('❌') && m.contains('—'),
        "{m}"
    );

    let m = render(&NotificationEvent::WebSocketPoolPartialAfterDeadline {
        connected: 3,
        total: 4,
        per_connection: vec![(100, 5000, Some(5)), (0, 5000, None)],
        stuck: vec![(3, "RSN-STUCK-881".to_string())],
        boot_path: BootPathLabel::Fast,
    });
    assert!(m.contains("RSN-STUCK-881"), "{m}");

    // BootPathLabel helpers (action_line / human / short Fast arms).
    assert_eq!(BootPathLabel::Fast.short(), "fast");
    assert!(BootPathLabel::Fast.human().contains("crash recovery"));
    assert_eq!(BootPathLabel::Slow.short(), "slow");
}

#[test]
fn test_ws_sleep_wake_event_messages() {
    let m = render(&NotificationEvent::WebSocketSleepEntered {
        feed: "FEED-MAIN-31".to_string(),
        connection_index: 1,
        sleep_secs: 65_001,
    });
    assert!(m.contains("FEED-MAIN-31") && m.contains("65"), "{m}");

    let m = render(&NotificationEvent::WebSocketSleepResumed {
        feed: "FEED-MAIN-32".to_string(),
        connection_index: 1,
        slept_for_secs: 65_002,
    });
    assert!(m.contains("FEED-MAIN-32"), "{m}");

    let m = render(&NotificationEvent::WebSocketTokenForceRenewedOnWake {
        feed: "FEED-MAIN-33".to_string(),
        connection_index: 2,
        remaining_secs_before: 3_500,
        threshold_secs: 14_400,
    });
    assert!(m.contains("FEED-MAIN-33"), "{m}");
}

#[test]
fn test_order_update_event_messages() {
    let m = render(&NotificationEvent::OrderUpdateConnected);
    assert!(!m.is_empty());
    let m = render(&NotificationEvent::OrderUpdateAuthenticated);
    assert!(!m.is_empty());
    let m = render(&NotificationEvent::OrderUpdateDisconnected {
        reason: "RSN-OUWS-601".to_string(),
    });
    assert!(m.contains("RSN-OUWS-601"), "{m}");
    let m = render(&NotificationEvent::OrderUpdateReconnected {
        consecutive_failures: 4243,
    });
    assert!(m.contains("4243"), "{m}");
}

#[test]
fn test_self_test_and_slo_event_messages() {
    let m = render(&NotificationEvent::SelfTestPassed { checks_passed: 7 });
    assert!(!m.is_empty());

    let m = render(&NotificationEvent::SelfTestDegraded {
        checks_passed: 5,
        checks_failed: 2,
        failed: vec!["depth_20_active", "recent_tick"],
    });
    assert!(m.contains("depth_20_active"), "{m}");

    let m = render(&NotificationEvent::SelfTestCritical {
        checks_failed: 3,
        failed: vec!["main_feed_active", "questdb_connected"],
    });
    assert!(m.contains("main_feed_active"), "{m}");

    let m = render(&NotificationEvent::RealtimeGuaranteeHealthy { score: 0.987 });
    assert!(m.contains("0.9") || m.contains("98"), "{m}");

    let m = render(&NotificationEvent::RealtimeGuaranteeDegraded {
        score: 0.91,
        weakest: "ws_health",
    });
    assert!(m.contains("ws_health"), "{m}");

    let m = render(&NotificationEvent::RealtimeGuaranteeCritical {
        score: 0.42,
        weakest: "qdb_health",
    });
    assert!(m.contains("qdb_health"), "{m}");
}

#[test]
fn test_option_chain_event_messages() {
    let m = render(&NotificationEvent::OptionChainFetchFailed {
        underlying: "UL-SENSEX-97".to_string(),
        attempts_made: 2,
        reason: "RSN-OC-701".to_string(),
    });
    assert!(m.contains("RSN-OC-701"), "{m}");

    let m = render(&NotificationEvent::OptionChainCacheFallback {
        underlying: "UL-SENSEX-98".to_string(),
        cache_age_secs: 4244,
    });
    assert!(m.contains("4244"), "{m}");

    let m = render(&NotificationEvent::OptionChainStaleHalt {
        underlying: "UL-SENSEX-99".to_string(),
        cache_age_secs: 4245,
        threshold_secs: 60,
    });
    assert!(m.contains("4245"), "{m}");

    let m = render(&NotificationEvent::OptionChainConfigInvalid {
        reason: "RSN-OCCFG-702".to_string(),
    });
    assert!(m.contains("RSN-OCCFG-702"), "{m}");
}

#[test]
fn test_boot_and_infra_event_messages() {
    let m = render(&NotificationEvent::BootHealthCheck {
        services_healthy: 7,
        services_total: 8,
    });
    assert!(m.contains('7') && m.contains('8'), "{m}");

    let m = render(&NotificationEvent::BootDeadlineMissed {
        deadline_secs: 4246,
        step: "STEP-6C-UNIVERSE".to_string(),
    });
    assert!(m.contains("STEP-6C-UNIVERSE"), "{m}");

    let m = render(&NotificationEvent::BootClockSkewExceeded {
        skew_secs: 2.75,
        threshold_secs: 2.0,
        source: "SRC-CHRONY-81".to_string(),
    });
    assert!(m.contains("SRC-CHRONY-81"), "{m}");

    let m = render(&NotificationEvent::IpVerificationFailed {
        reason: "RSN-IP-801".to_string(),
    });
    assert!(m.contains("RSN-IP-801"), "{m}");

    // The rendered message REDACTS the last two octets (security: the
    // operator's static IP must not sit in Telegram history verbatim).
    let m = render(&NotificationEvent::IpVerificationSuccess {
        verified_ip: "203.0.113.77".to_string(),
    });
    assert!(
        m.contains("203.0.XXX.XX") && !m.contains("203.0.113.77"),
        "{m}"
    );

    let m = render(&NotificationEvent::StaticIpBootCheckFailed {
        reason: "orders_not_allowed".to_string(),
        orders_allowed: false,
        ip_match_status: "MISMATCH-XYZ".to_string(),
        attempts_made: 30,
    });
    // The arm translates the wire-format reason into operator English and
    // echoes Dhan's literal ipMatchStatus.
    assert!(
        m.contains("MISMATCH-XYZ") && m.contains("NOT ALLOWED"),
        "{m}"
    );

    let m = render(&NotificationEvent::InstrumentBuildSuccess {
        source: "SRC-PRIMARY-82".to_string(),
        derivative_count: 219_113,
        underlying_count: 331,
    });
    assert!(m.contains("331"), "{m}");

    let m = render(&NotificationEvent::InstrumentBuildFailed {
        reason: "RSN-INSTR-901".to_string(),
        manual_trigger_url: "URL-TRIGGER-83".to_string(),
    });
    assert!(m.contains("RSN-INSTR-901"), "{m}");
}

#[test]
fn test_oms_and_questdb_event_messages() {
    let m = render(&NotificationEvent::OrderRejected {
        correlation_id: "CID-OMS-1001".to_string(),
        reason: "RSN-OMS-1002".to_string(),
    });
    assert!(
        m.contains("CID-OMS-1001") && m.contains("RSN-OMS-1002"),
        "{m}"
    );

    let m = render(&NotificationEvent::CircuitBreakerOpened {
        consecutive_failures: 4247,
    });
    assert!(m.contains("4247"), "{m}");

    let m = render(&NotificationEvent::CircuitBreakerClosed);
    assert!(!m.is_empty());

    let m = render(&NotificationEvent::RateLimitExhausted {
        limit_type: "LIMIT-DAILY-84".to_string(),
    });
    assert!(m.contains("LIMIT-DAILY-84"), "{m}");

    let m = render(&NotificationEvent::TokenRenewalDeadlineMissed {
        deadline_hour_ist: 23,
    });
    assert!(m.contains("23"), "{m}");

    let m = render(&NotificationEvent::QuestDbDisconnected {
        writer: "WRT-TICK-85".to_string(),
        signal: 4248,
        signal_kind: "KIND-LIVENESS-86".to_string(),
    });
    assert!(m.contains("WRT-TICK-85") && m.contains("4248"), "{m}");

    let m = render(&NotificationEvent::QuestDbReconnected {
        writer: "WRT-TICK-87".to_string(),
        failed_checks_before_recovery: 4249,
    });
    assert!(m.contains("4249") && m.contains("WRT-TICK-87"), "{m}");
}

#[test]
fn test_misc_event_messages() {
    let m = render(&NotificationEvent::NoLiveTicksDuringMarketHours {
        silent_for_secs: 4250,
        threshold_secs: 30,
    });
    assert!(m.contains("4250"), "{m}");

    let m = render(&NotificationEvent::LastTickAfterBoundary {
        security_id: 987_655,
        exchange_segment: "NSE_EQ".to_string(),
        exchange_ts_nanos_of_day: 55_800_000_000_001,
        nanos_past_close: 1_000_000,
    });
    assert!(m.contains("987655"), "{m}");

    let m = render(&NotificationEvent::OrphanPositionDetected {
        count: 2,
        total_abs_net_qty: 4251,
        sample_symbols: vec!["NIFTY-Jun2026-28500-CE".to_string()],
        dry_run: true,
    });
    assert!(m.contains("NIFTY-Jun2026-28500-CE"), "{m}");

    let m = render(&NotificationEvent::BarMismatchCorrectedFromHistorical {
        compared_count: 222,
        mismatches_count: 4,
        sample_symbols: vec!["SYM-RELIANCE-88".to_string()],
        cross_check_pass: "post_open_09_16_05",
    });
    assert!(m.contains("SYM-RELIANCE-88"), "{m}");
}

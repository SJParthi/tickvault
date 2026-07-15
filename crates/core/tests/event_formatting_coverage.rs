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

    // MidSessionProfileInvalidated render coverage DELETED 2026-07-14 with
    // the variant (Dhan noise lock) — the watchdog runs silently; a
    // terminal re-mint failure routes through AuthenticationFailed.
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
        feeds: vec![
            tickvault_core::notification::events::FeedStatusLine {
                name: "Dhan".to_string(),
                instruments: Some(776),
                last_tick_age_secs: Some(1),
            },
            tickvault_core::notification::events::FeedStatusLine {
                name: "Groww".to_string(),
                instruments: None,
                last_tick_age_secs: None,
            },
        ],
    });
    assert!(m.contains("Mid-market crash recovery"), "{m}");
    // 2026-07-03 feed parity: every enabled feed appears by name, with
    // honest "unknown" wording when no health data is wired yet.
    assert!(m.contains("Dhan: 776 instruments"), "{m}");
    assert!(
        m.contains("Groww: instruments unknown — no tick seen yet"),
        "{m}"
    );
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

// 2026-06-28: test_option_chain_event_messages REMOVED — the 4 OptionChain*
// NotificationEvent variants were deleted with the option_chain REST subsystem
// (operator directive 2026-06-28).

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

    // RETIRED 2026-06-12: TokenRenewalDeadlineMissed variant deleted (redundant
    // with the live TokenRenewalFailed Critical path).

    let m = render(&NotificationEvent::QuestDbDisconnected {
        writer: "WRT-TICK-85".to_string(),
        signal: 4248,
        signal_kind: "KIND-LIVENESS-86".to_string(),
    });
    assert!(m.contains("WRT-TICK-85") && m.contains("4248"), "{m}");

    let m = render(&NotificationEvent::QuestDbReconnected {
        writer: "WRT-TICK-87".to_string(),
        failed_checks_before_recovery: 6,
    });
    assert!(
        m.contains('6') && m.contains("WRT-TICK-87") && m.contains("RECOVERED"),
        "{m}"
    );
}

#[test]
fn test_misc_event_messages() {
    // RETIRED 2026-07-14: NoLiveTicksDuringMarketHours deleted with the
    // no-tick watchdog (operator Dhan noise lock).

    // RETIRED 2026-06-12: LastTickAfterBoundary deleted (redundant with the live
    // tv_late_tick_after_boundary_total counter; wiring it = hot-path risk).

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

#[test]
fn test_feed_down_and_recovered_message_coverage() {
    // 2026-07-06 Groww feed-down alerting: exercise to_message + severity +
    // dispatch_policy for both new variants (in-market + off-hours forms).
    let m = render(&NotificationEvent::FeedDown {
        feed: "Groww".to_string(),
        reason: "RSN-FEEDDOWN-901".to_string(),
        market_open: true,
        operator_initiated: false,
    });
    assert!(
        m.contains("RSN-FEEDDOWN-901") && m.contains("will not flow"),
        "{m}"
    );

    let m = render(&NotificationEvent::FeedDown {
        feed: "Groww".to_string(),
        reason: "RSN-FEEDDOWN-902".to_string(),
        market_open: false,
        operator_initiated: false,
    });
    assert!(
        m.contains("RSN-FEEDDOWN-902") && m.contains("idle is normal"),
        "{m}"
    );

    // Operator-initiated forms (2026-07-06 fix): the body names the
    // re-enable action instead of a false auto-retry claim.
    let m = render(&NotificationEvent::FeedDown {
        feed: "Groww".to_string(),
        reason: "RSN-FEEDDOWN-903".to_string(),
        market_open: true,
        operator_initiated: true,
    });
    assert!(
        m.contains("RSN-FEEDDOWN-903") && m.contains("re-enable it from the feeds page"),
        "{m}"
    );

    let m = render(&NotificationEvent::FeedDown {
        feed: "Groww".to_string(),
        reason: "RSN-FEEDDOWN-904".to_string(),
        market_open: false,
        operator_initiated: true,
    });
    assert!(
        m.contains("RSN-FEEDDOWN-904")
            && m.contains("idle is normal")
            && m.contains("re-enable it from the feeds page"),
        "{m}"
    );

    let m = render(&NotificationEvent::FeedRecovered {
        feed: "Groww".to_string(),
        down_secs: 4253,
    });
    assert!(m.contains("4253") && m.contains("streaming again"), "{m}");
}

#[test]
fn test_tf_consistency_summary_message_variants() {
    use tickvault_core::notification::events::Severity;

    // Clean PASS day → Info, PASS wording, both dates + counts rendered.
    // H1: the Groww tail carve-out is NAMED on the pass wording (buckets
    // never sealed on the prod schedule — not verified, never a page).
    let clean = NotificationEvent::TfConsistencySummary {
        dhan_date_ist: "2026-07-13".to_string(),
        groww_date_ist: "2026-07-10".to_string(),
        instruments: 343,
        buckets_compared: 88_311,
        mismatches: 0,
        missing_tf_rows: 0,
        no_coverage: 0,
        off_grid: 0,
        duplicates: 0,
        tail_unsealed: 19,
        degraded: false,
        truncated: false,
        status_label: "pass".to_string(),
        top_detail: vec![],
    };
    let m = render(&clean);
    // 2026-07-15 cleanliness overhaul: a green daily check is ONE line —
    // verdict-first, comma-formatted count, H1 tail carve-out inline.
    assert!(
        m.starts_with('\u{2705}'),
        "pass leads with the OK emoji: {m}"
    );
    // G3 (fix round 2): the one-liner carries the compact verified date
    // (the run's Dhan-side target day) so a forced past-day backfill's
    // PASS card is distinguishable from today's daily check.
    assert!(m.contains("Timeframe check 3:40 PM \u{b7} 13 Jul"), "{m}");
    assert!(m.contains("88,311 candles"), "{m}");
    assert!(m.contains("all match"), "{m}");
    assert_eq!(m.lines().count(), 1, "pass body is exactly one line: {m}");
    assert!(
        m.contains("(19 end-of-day candles unverified)"),
        "H1 tail carve-out must ride the pass one-liner: {m}"
    );
    assert_eq!(clean.severity(), Severity::Info);
    assert_eq!(clean.topic(), "TfConsistencySummary");

    // Zero tail_unsealed → the note vanishes entirely.
    let clean_no_tail = NotificationEvent::TfConsistencySummary {
        dhan_date_ist: "2026-07-13".to_string(),
        groww_date_ist: "2026-07-10".to_string(),
        instruments: 343,
        buckets_compared: 88_311,
        mismatches: 0,
        missing_tf_rows: 0,
        no_coverage: 0,
        off_grid: 0,
        duplicates: 0,
        tail_unsealed: 0,
        degraded: false,
        truncated: false,
        status_label: "pass".to_string(),
        top_detail: vec![],
    };
    let m = render(&clean_no_tail);
    assert!(
        !m.contains("unverified"),
        "no tail note when tail_unsealed == 0: {m}"
    );
    assert_eq!(m.lines().count(), 1, "pass body is exactly one line: {m}");

    // Feed-off no_data day → Info, "nothing to check" wording, never PASS.
    let no_data = NotificationEvent::TfConsistencySummary {
        dhan_date_ist: "2026-07-13".to_string(),
        groww_date_ist: "2026-07-10".to_string(),
        instruments: 0,
        buckets_compared: 0,
        mismatches: 0,
        missing_tf_rows: 0,
        no_coverage: 0,
        off_grid: 0,
        duplicates: 0,
        tail_unsealed: 0,
        degraded: false,
        truncated: false,
        status_label: "no_data".to_string(),
        top_detail: vec![],
    };
    let m = render(&no_data);
    assert!(m.contains("nothing to check"), "{m}");
    assert!(!m.contains("PASS"), "no_data must never claim PASS: {m}");
    assert_eq!(no_data.severity(), Severity::Info);

    // BLIND day (rows expected, zero compared) → High, BLIND wording.
    // Refuter round 2: the Groww tail note must ride the BLIND wording
    // too when tail_unsealed > 0.
    let blind = NotificationEvent::TfConsistencySummary {
        dhan_date_ist: "2026-07-13".to_string(),
        groww_date_ist: "2026-07-10".to_string(),
        instruments: 0,
        buckets_compared: 0,
        mismatches: 0,
        missing_tf_rows: 0,
        no_coverage: 0,
        off_grid: 0,
        duplicates: 0,
        tail_unsealed: 3,
        degraded: true,
        truncated: false,
        status_label: "blind".to_string(),
        top_detail: vec![],
    };
    let m = render(&blind);
    assert!(m.contains("BLIND") && m.contains("not a pass"), "{m}");
    assert!(
        m.contains("3 Groww end-of-day buckets are not sealed by design"),
        "the Groww tail note must render on the BLIND wording too: {m}"
    );
    assert_eq!(blind.severity(), Severity::High);

    // L6 pin: the blind-WITHOUT-degrade edge (rows seen, zero compared,
    // zero paging counts, degraded=false). A count-derived re-check would
    // have called the counts clean; the status_label verdict must win —
    // High + BLIND wording, never Info, never PASS.
    let blind_no_degrade = NotificationEvent::TfConsistencySummary {
        dhan_date_ist: "2026-07-13".to_string(),
        groww_date_ist: "2026-07-10".to_string(),
        instruments: 12,
        buckets_compared: 0,
        mismatches: 0,
        missing_tf_rows: 0,
        no_coverage: 0,
        off_grid: 0,
        duplicates: 0,
        tail_unsealed: 0,
        degraded: false,
        truncated: false,
        status_label: "blind".to_string(),
        top_detail: vec![],
    };
    let m = render(&blind_no_degrade);
    assert!(
        m.contains("BLIND") && !m.contains("PASS"),
        "blind-without-degrade must render BLIND, never PASS: {m}"
    );
    assert_eq!(blind_no_degrade.severity(), Severity::High);

    // L6 pin: a `degraded` verdict with clean-LOOKING counts (paging 0,
    // compared > 0, degraded flag false) must still be High + needs
    // attention — the label is the flush-adjusted truth.
    let degraded_label = NotificationEvent::TfConsistencySummary {
        dhan_date_ist: "2026-07-13".to_string(),
        groww_date_ist: "2026-07-10".to_string(),
        instruments: 343,
        buckets_compared: 88_311,
        mismatches: 0,
        missing_tf_rows: 0,
        no_coverage: 0,
        off_grid: 0,
        duplicates: 0,
        tail_unsealed: 0,
        degraded: false,
        truncated: false,
        status_label: "degraded".to_string(),
        top_detail: vec![],
    };
    let m = render(&degraded_label);
    assert!(
        m.contains("NEEDS ATTENTION") && !m.contains("PASS"),
        "a degraded status_label must never render PASS: {m}"
    );
    assert_eq!(degraded_label.severity(), Severity::High);

    // Findings day → High, counts + top_detail lines rendered (escaped),
    // truncated note present.
    let findings = NotificationEvent::TfConsistencySummary {
        dhan_date_ist: "2026-07-13".to_string(),
        groww_date_ist: "2026-07-10".to_string(),
        instruments: 343,
        buckets_compared: 88_311,
        mismatches: 4262,
        missing_tf_rows: 4263,
        no_coverage: 4264,
        off_grid: 4265,
        duplicates: 4266,
        tail_unsealed: 7,
        degraded: true,
        truncated: true,
        status_label: "mismatch".to_string(),
        top_detail: vec!["DETAIL-TFV-901 <tag>".to_string()],
    };
    let m = render(&findings);
    assert!(
        m.contains("NEEDS ATTENTION")
            && m.contains("4262")
            && m.contains("4263")
            && m.contains("4264")
            && m.contains("4265")
            && m.contains("4266"),
        "{m}"
    );
    assert!(
        m.contains("DETAIL-TFV-901") && m.contains("&lt;tag&gt;"),
        "top_detail must render html-escaped: {m}"
    );
    assert!(m.contains("PARTIAL"), "degraded coverage note missing: {m}");
    assert!(
        m.contains("exceed the stored detail"),
        "truncated note missing: {m}"
    );
    assert!(
        m.contains("7 Groww end-of-day buckets are not sealed by design"),
        "H1 tail carve-out note missing on the findings wording: {m}"
    );
    assert_eq!(findings.severity(), Severity::High);
}

#[test]
fn test_tf_consistency_aborted_message() {
    use tickvault_core::notification::events::Severity;
    let ev = NotificationEvent::TfConsistencyAborted {
        detail: "RSN-TFV-777 <b>".to_string(),
    };
    let m = render(&ev);
    assert!(
        m.contains("RSN-TFV-777") && m.contains("did NOT run") && m.contains("&lt;b&gt;"),
        "{m}"
    );
    assert_eq!(ev.severity(), Severity::High);
    assert_eq!(ev.topic(), "TfConsistencyAborted");
}

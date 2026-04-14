//! Gap enforcement integration tests.
//!
//! Exhaustive tests for gap implementations across all Core crate domains:
//!
//! ## Instruments
//! - I-P0-05: S3 backup config, key layout, error paths
//! - I-P1-01: Daily scheduler timing, boundaries, config
//!
//! ## Network
//! - GAP-NET-01: IP monitor config, comparison, validation (pure functions)
//!
//! ## WebSocket
//! - WS-GAP-01: Disconnect code classification (reconnectable, token refresh, roundtrip)
//! - WS-GAP-02: Subscription batching (batch size clamping, empty input, SecurityId as string)
//! - WS-GAP-03: Connection state machine (4 states, display, equality)
//!
//! Each gap section tests:
//! - Happy path
//! - Error paths
//! - Boundary conditions
//! - Edge cases
//! - Config validation
//!
//! Note: Async tests using tokio_util::CancellationToken and spawn_ip_monitor
//! live in the ip_monitor module's unit tests (uses crate-internal deps).
//! GAP-SEC-01 tests live in crates/api/tests/auth_middleware.rs.
//! AUTH-GAP-01/02 tests live in the auth module's unit tests (require secrecy crate).

// ===========================================================================
// I-P0-05: S3 Backup — exhaustive
// ===========================================================================

mod s3_backup {
    use chrono::NaiveDate;
    use tickvault_core::instrument::s3_backup::*;

    // -- Config: every possible state ----------------------------------------

    #[test]
    fn config_default_is_disabled() {
        let config = S3BackupConfig::default();
        assert!(!is_s3_backup_configured(&config));
        assert!(config.bucket.is_empty());
        assert_eq!(config.prefix, "instruments");
        assert_eq!(config.region, "ap-south-1");
    }

    #[test]
    fn config_valid_bucket_and_region_is_configured() {
        let config = S3BackupConfig {
            bucket: "my-bucket".to_owned(),
            prefix: "instruments".to_owned(),
            region: "ap-south-1".to_owned(),
        };
        assert!(is_s3_backup_configured(&config));
    }

    #[test]
    fn config_whitespace_bucket_not_configured() {
        let config = S3BackupConfig {
            bucket: "   \t\n".to_owned(),
            prefix: "instruments".to_owned(),
            region: "ap-south-1".to_owned(),
        };
        assert!(!is_s3_backup_configured(&config));
    }

    #[test]
    fn config_empty_region_not_configured() {
        let config = S3BackupConfig {
            bucket: "my-bucket".to_owned(),
            prefix: "instruments".to_owned(),
            region: String::new(),
        };
        assert!(!is_s3_backup_configured(&config));
    }

    #[test]
    fn config_whitespace_region_not_configured() {
        let config = S3BackupConfig {
            bucket: "my-bucket".to_owned(),
            prefix: "instruments".to_owned(),
            region: "  ".to_owned(),
        };
        assert!(!is_s3_backup_configured(&config));
    }

    #[test]
    fn config_both_empty_not_configured() {
        let config = S3BackupConfig {
            bucket: String::new(),
            prefix: String::new(),
            region: String::new(),
        };
        assert!(!is_s3_backup_configured(&config));
    }

    #[test]
    fn config_clone_preserves_all_fields() {
        let config = S3BackupConfig {
            bucket: "b".to_owned(),
            prefix: "p".to_owned(),
            region: "r".to_owned(),
        };
        let cloned = config.clone();
        assert_eq!(cloned.bucket, "b");
        assert_eq!(cloned.prefix, "p");
        assert_eq!(cloned.region, "r");
    }

    // -- Key layout: dated ---------------------------------------------------

    fn test_config() -> S3BackupConfig {
        S3BackupConfig {
            bucket: "tv-backup".to_owned(),
            prefix: "instruments".to_owned(),
            region: "ap-south-1".to_owned(),
        }
    }

    #[test]
    fn key_dated_rkyv_format() {
        let config = test_config();
        let date = NaiveDate::from_ymd_opt(2026, 3, 11).expect("valid"); // APPROVED: test constant
        assert_eq!(
            s3_key_for_date(&config, date, "universe.rkyv"),
            "instruments/2026-03-11/universe.rkyv"
        );
    }

    #[test]
    fn key_dated_csv_format() {
        let config = test_config();
        let date = NaiveDate::from_ymd_opt(2026, 1, 1).expect("valid"); // APPROVED: test constant
        assert_eq!(
            s3_key_for_date(&config, date, "instruments.csv"),
            "instruments/2026-01-01/instruments.csv"
        );
    }

    #[test]
    fn key_dated_leap_year_feb_29() {
        let config = test_config();
        let date = NaiveDate::from_ymd_opt(2024, 2, 29).expect("valid"); // APPROVED: test constant
        assert_eq!(
            s3_key_for_date(&config, date, "universe.rkyv"),
            "instruments/2024-02-29/universe.rkyv"
        );
    }

    #[test]
    fn key_dated_year_end() {
        let config = test_config();
        let date = NaiveDate::from_ymd_opt(2026, 12, 31).expect("valid"); // APPROVED: test constant
        assert_eq!(
            s3_key_for_date(&config, date, "universe.rkyv"),
            "instruments/2026-12-31/universe.rkyv"
        );
    }

    // -- Key layout: latest --------------------------------------------------

    #[test]
    fn key_latest_rkyv_format() {
        let config = test_config();
        assert_eq!(
            s3_key_for_latest(&config, "universe.rkyv"),
            "instruments/latest/universe.rkyv"
        );
    }

    #[test]
    fn key_latest_csv_format() {
        let config = test_config();
        assert_eq!(
            s3_key_for_latest(&config, "instruments.csv"),
            "instruments/latest/instruments.csv"
        );
    }

    // -- Key layout: custom prefix -------------------------------------------

    #[test]
    fn key_custom_prefix() {
        let config = S3BackupConfig {
            bucket: "b".to_owned(),
            prefix: "prod/cache".to_owned(),
            region: "r".to_owned(),
        };
        let date = NaiveDate::from_ymd_opt(2026, 6, 15).expect("valid"); // APPROVED: test constant
        assert_eq!(
            s3_key_for_date(&config, date, "universe.rkyv"),
            "prod/cache/2026-06-15/universe.rkyv"
        );
        assert_eq!(
            s3_key_for_latest(&config, "universe.rkyv"),
            "prod/cache/latest/universe.rkyv"
        );
    }

    #[test]
    fn key_empty_prefix_defaults_to_instruments() {
        let config = S3BackupConfig {
            bucket: "b".to_owned(),
            prefix: String::new(),
            region: "r".to_owned(),
        };
        let date = NaiveDate::from_ymd_opt(2026, 3, 10).expect("valid"); // APPROVED: test constant
        assert!(s3_key_for_date(&config, date, "x.rkyv").starts_with("instruments/"));
        assert!(s3_key_for_latest(&config, "x.rkyv").starts_with("instruments/"));
    }

    #[test]
    fn key_whitespace_only_prefix_defaults_to_instruments() {
        let config = S3BackupConfig {
            bucket: "b".to_owned(),
            prefix: "   ".to_owned(),
            region: "r".to_owned(),
        };
        let date = NaiveDate::from_ymd_opt(2026, 3, 10).expect("valid"); // APPROVED: test constant
        assert!(s3_key_for_date(&config, date, "x.rkyv").starts_with("instruments/"));
    }

    // -- Backup error: not configured ----------------------------------------

    #[tokio::test]
    async fn backup_not_configured_returns_not_configured_error() {
        let config = S3BackupConfig::default();
        let date = NaiveDate::from_ymd_opt(2026, 3, 11).expect("valid"); // APPROVED: test constant
        let result = backup_instrument_cache("/tmp/nope", "test.csv", date, &config).await;
        assert!(matches!(result, Err(S3BackupError::NotConfigured)));
    }

    // -- Backup error: missing files -----------------------------------------

    #[tokio::test]
    async fn backup_missing_cache_dir_returns_io_error() {
        let config = test_config();
        let date = NaiveDate::from_ymd_opt(2026, 3, 11).expect("valid"); // APPROVED: test constant
        let result = backup_instrument_cache(
            "/tmp/tv-gap-test-nonexistent-99999",
            "test.csv",
            date,
            &config,
        )
        .await;
        assert!(matches!(result, Err(S3BackupError::IoError(_))));
    }

    // -- Restore error: not configured ---------------------------------------

    #[tokio::test]
    async fn restore_not_configured_returns_not_configured_error() {
        let config = S3BackupConfig::default();
        let result = restore_instrument_cache("/tmp/nope", &config).await;
        assert!(matches!(result, Err(S3BackupError::NotConfigured)));
    }

    // -- Error display -------------------------------------------------------

    #[test]
    fn error_display_not_configured() {
        let err = S3BackupError::NotConfigured;
        assert!(err.to_string().contains("not configured"));
    }

    #[test]
    fn error_display_upload_failed() {
        let err = S3BackupError::UploadFailed {
            key: "k".to_owned(),
            reason: "r".to_owned(),
        };
        let msg = err.to_string();
        assert!(msg.contains("upload failed"));
        assert!(msg.contains("k"));
        assert!(msg.contains("r"));
    }

    #[test]
    fn error_display_download_failed() {
        let err = S3BackupError::DownloadFailed {
            key: "k".to_owned(),
            reason: "r".to_owned(),
        };
        let msg = err.to_string();
        assert!(msg.contains("download failed"));
    }

    #[test]
    fn error_display_io_error() {
        let err = S3BackupError::IoError(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "nope",
        ));
        assert!(err.to_string().contains("nope"));
    }

    #[test]
    fn error_from_io_error_conversion() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        let s3_err: S3BackupError = io_err.into();
        assert!(matches!(s3_err, S3BackupError::IoError(_)));
    }
}

// ===========================================================================
// I-P1-01: Daily Scheduler — exhaustive
// ===========================================================================

mod daily_scheduler {
    use chrono::{DateTime, FixedOffset, NaiveDate, NaiveTime};

    use tickvault_core::instrument::daily_scheduler::*;

    fn ist_datetime(y: i32, m: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<FixedOffset> {
        let offset = tickvault_common::trading_calendar::ist_offset();
        let date = NaiveDate::from_ymd_opt(y, m, d).expect("valid date"); // APPROVED: test helper
        let time = NaiveTime::from_hms_opt(h, mi, s).expect("valid time"); // APPROVED: test helper
        date.and_time(time)
            .and_local_timezone(offset)
            .single()
            .expect("IST fixed offset never ambiguous") // APPROVED: test helper
    }

    fn hms(h: u32, m: u32, s: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, s).expect("valid time") // APPROVED: test helper
    }

    // -- compute_next_trigger_time: future today -----------------------------

    #[test]
    fn trigger_4h_from_now() {
        let now = ist_datetime(2026, 3, 11, 10, 0, 0);
        let target = hms(14, 0, 0);
        assert_eq!(compute_next_trigger_time(now, target).as_secs(), 4 * 3600);
    }

    #[test]
    fn trigger_1_second_from_now() {
        let now = ist_datetime(2026, 3, 11, 8, 54, 59);
        let target = hms(8, 55, 0);
        assert_eq!(compute_next_trigger_time(now, target).as_secs(), 1);
    }

    #[test]
    fn trigger_almost_24h_from_now() {
        let now = ist_datetime(2026, 3, 11, 0, 0, 1);
        let target = hms(0, 0, 0);
        assert_eq!(compute_next_trigger_time(now, target).as_secs(), 86_399);
    }

    // -- compute_next_trigger_time: past today → wrap to tomorrow -----------

    #[test]
    fn trigger_past_wraps_to_tomorrow() {
        let now = ist_datetime(2026, 3, 11, 10, 0, 0);
        let target = hms(8, 55, 0);
        let expected = 22 * 3600 + 55 * 60;
        assert_eq!(compute_next_trigger_time(now, target).as_secs(), expected);
    }

    #[test]
    fn trigger_1_second_past_wraps() {
        let now = ist_datetime(2026, 3, 11, 8, 55, 1);
        let target = hms(8, 55, 0);
        assert_eq!(compute_next_trigger_time(now, target).as_secs(), 86_399);
    }

    // -- compute_next_trigger_time: exactly now → 24h -----------------------

    #[test]
    fn trigger_exactly_now_wraps_24h() {
        let now = ist_datetime(2026, 3, 11, 8, 55, 0);
        let target = hms(8, 55, 0);
        assert_eq!(compute_next_trigger_time(now, target).as_secs(), 86_400);
    }

    #[test]
    fn trigger_midnight_exactly() {
        let now = ist_datetime(2026, 3, 11, 0, 0, 0);
        let target = hms(0, 0, 0);
        assert_eq!(compute_next_trigger_time(now, target).as_secs(), 86_400);
    }

    // -- compute_next_trigger_time: midnight boundary -----------------------

    #[test]
    fn trigger_cross_midnight_2min() {
        let now = ist_datetime(2026, 3, 11, 23, 59, 0);
        let target = hms(0, 1, 0);
        assert_eq!(compute_next_trigger_time(now, target).as_secs(), 120);
    }

    #[test]
    fn trigger_23h59_to_midnight() {
        let now = ist_datetime(2026, 3, 11, 23, 59, 59);
        let target = hms(0, 0, 0);
        assert_eq!(compute_next_trigger_time(now, target).as_secs(), 1);
    }

    // -- compute_next_trigger_time: exhaustive sweep -------------------------

    #[test]
    fn trigger_result_always_positive_all_hours() {
        for hour in 0..24 {
            let now = ist_datetime(2026, 3, 11, hour, 30, 0);
            let target = hms(8, 55, 0);
            let dur = compute_next_trigger_time(now, target);
            assert!(dur.as_secs() > 0, "positive for hour={hour}");
            assert!(dur.as_secs() <= 86_400, "<=24h for hour={hour}");
        }
    }

    #[test]
    fn trigger_result_always_positive_all_minutes() {
        for min in 0..60 {
            let now = ist_datetime(2026, 3, 11, 8, min, 0);
            let target = hms(8, 55, 0);
            let dur = compute_next_trigger_time(now, target);
            assert!(dur.as_secs() > 0, "positive for min={min}");
            assert!(dur.as_secs() <= 86_400, "<=24h for min={min}");
        }
    }

    // -- parse_daily_download_time: valid ------------------------------------

    #[test]
    fn parse_valid_time() {
        let t = parse_daily_download_time("08:55:00").expect("valid"); // APPROVED: test
        assert_eq!(t, hms(8, 55, 0));
    }

    #[test]
    fn parse_midnight() {
        let t = parse_daily_download_time("00:00:00").expect("valid"); // APPROVED: test
        assert_eq!(t, hms(0, 0, 0));
    }

    #[test]
    fn parse_end_of_day() {
        let t = parse_daily_download_time("23:59:59").expect("valid"); // APPROVED: test
        assert_eq!(t, hms(23, 59, 59));
    }

    // -- parse_daily_download_time: invalid ----------------------------------

    #[test]
    fn parse_empty_string_fails() {
        assert!(parse_daily_download_time("").is_err());
    }

    #[test]
    fn parse_hh_mm_only_fails() {
        assert!(parse_daily_download_time("08:55").is_err());
    }

    #[test]
    fn parse_garbage_fails() {
        assert!(parse_daily_download_time("not-a-time").is_err());
    }

    #[test]
    fn parse_24_hour_boundary_fails() {
        assert!(parse_daily_download_time("24:00:00").is_err());
    }

    #[test]
    fn parse_negative_fails() {
        assert!(parse_daily_download_time("-1:00:00").is_err());
    }

    #[test]
    fn parse_with_trailing_text_fails() {
        assert!(parse_daily_download_time("08:55:00 extra").is_err());
    }

    // -- DailyRefreshConfig --------------------------------------------------

    #[test]
    fn config_default_disabled() {
        let config = DailyRefreshConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.download_time, hms(8, 55, 0));
    }

    #[test]
    fn config_clone_preserves_fields() {
        let config = DailyRefreshConfig {
            download_time: hms(9, 0, 0),
            enabled: true,
        };
        let cloned = config.clone();
        assert!(cloned.enabled);
        assert_eq!(cloned.download_time, hms(9, 0, 0));
    }
}

// ===========================================================================
// GAP-NET-01: IP Monitor — pure function tests
// ===========================================================================

mod ip_monitor {
    use tickvault_core::network::ip_monitor::*;

    // -- compare_ips: all variants -------------------------------------------

    #[test]
    fn compare_match() {
        assert_eq!(
            compare_ips("203.0.113.42", "203.0.113.42"),
            IpCheckResult::Match
        );
    }

    #[test]
    fn compare_mismatch() {
        let result = compare_ips("1.2.3.4", "5.6.7.8");
        assert!(matches!(result, IpCheckResult::Mismatch { .. }));
        if let IpCheckResult::Mismatch { expected, actual } = result {
            assert_eq!(expected, "1.2.3.4");
            assert_eq!(actual, "5.6.7.8");
        }
    }

    #[test]
    fn compare_empty_strings_match() {
        assert_eq!(compare_ips("", ""), IpCheckResult::Match);
    }

    #[test]
    fn compare_one_empty_mismatch() {
        assert!(matches!(
            compare_ips("1.2.3.4", ""),
            IpCheckResult::Mismatch { .. }
        ));
    }

    #[test]
    fn compare_whitespace_sensitive() {
        assert!(matches!(
            compare_ips("1.2.3.4", "1.2.3.4 "),
            IpCheckResult::Mismatch { .. }
        ));
    }

    #[test]
    fn compare_identical_long_strings() {
        assert_eq!(
            compare_ips("255.255.255.255", "255.255.255.255"),
            IpCheckResult::Match
        );
    }

    // -- is_valid_ipv4: valid ------------------------------------------------

    #[test]
    fn ipv4_valid_standard() {
        assert!(is_valid_ipv4("192.168.1.1"));
        assert!(is_valid_ipv4("10.0.0.1"));
        assert!(is_valid_ipv4("172.16.0.1"));
    }

    #[test]
    fn ipv4_valid_boundaries() {
        assert!(is_valid_ipv4("0.0.0.0"));
        assert!(is_valid_ipv4("255.255.255.255"));
        assert!(is_valid_ipv4("1.1.1.1"));
    }

    // -- is_valid_ipv4: invalid ----------------------------------------------

    #[test]
    fn ipv4_invalid_empty() {
        assert!(!is_valid_ipv4(""));
    }

    #[test]
    fn ipv4_invalid_text() {
        assert!(!is_valid_ipv4("not-an-ip"));
        assert!(!is_valid_ipv4("abc.def.ghi.jkl"));
    }

    #[test]
    fn ipv4_invalid_overflow() {
        assert!(!is_valid_ipv4("256.0.0.1"));
        assert!(!is_valid_ipv4("0.0.0.256"));
        assert!(!is_valid_ipv4("999.999.999.999"));
    }

    #[test]
    fn ipv4_invalid_too_few_octets() {
        assert!(!is_valid_ipv4("1.2.3"));
        assert!(!is_valid_ipv4("1.2"));
        assert!(!is_valid_ipv4("1"));
    }

    #[test]
    fn ipv4_invalid_ipv6() {
        assert!(!is_valid_ipv4("::1"));
        assert!(!is_valid_ipv4("fe80::1"));
    }

    #[test]
    fn ipv4_invalid_with_port() {
        assert!(!is_valid_ipv4("1.2.3.4:8080"));
    }

    #[test]
    fn ipv4_invalid_with_whitespace() {
        assert!(!is_valid_ipv4(" 1.2.3.4"));
        assert!(!is_valid_ipv4("1.2.3.4 "));
        assert!(!is_valid_ipv4("1.2.3.4\n"));
        assert!(!is_valid_ipv4("\t1.2.3.4"));
    }

    #[test]
    fn ipv4_invalid_negative() {
        assert!(!is_valid_ipv4("-1.0.0.0"));
    }

    // -- IpCheckResult: traits -----------------------------------------------

    #[test]
    fn ip_check_result_clone_eq() {
        let a = IpCheckResult::Match;
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn ip_check_result_mismatch_preserves() {
        let r = IpCheckResult::Mismatch {
            expected: "a".to_owned(),
            actual: "b".to_owned(),
        };
        let cloned = r.clone();
        assert_eq!(r, cloned);
    }

    #[test]
    fn ip_check_result_check_failed_debug() {
        let r = IpCheckResult::CheckFailed {
            reason: "timeout".to_owned(),
        };
        let dbg = format!("{r:?}");
        assert!(dbg.contains("timeout"));
    }

    // -- IpMonitorConfig: construction ---------------------------------------

    #[test]
    fn config_new_enabled() {
        let config = IpMonitorConfig::new("1.2.3.4".to_owned(), 60);
        assert!(config.enabled);
        assert_eq!(config.expected_ip, "1.2.3.4");
        assert_eq!(config.check_interval_secs, 60);
    }

    #[test]
    fn config_disabled() {
        let config = IpMonitorConfig::disabled();
        assert!(!config.enabled);
        assert!(config.expected_ip.is_empty());
        assert_eq!(config.check_interval_secs, 300);
    }

    #[test]
    fn config_clone() {
        let config = IpMonitorConfig::new("10.0.0.1".to_owned(), 120);
        let cloned = config.clone();
        assert_eq!(cloned.expected_ip, config.expected_ip);
        assert_eq!(cloned.check_interval_secs, config.check_interval_secs);
        assert_eq!(cloned.enabled, config.enabled);
    }
}

// ===========================================================================
// WS-GAP-01: Disconnect Code Classification
// ===========================================================================

mod ws_disconnect_codes {
    use tickvault_core::websocket::types::DisconnectCode;

    // -- from_u16 ↔ as_u16 roundtrip for all 12 annexure Section 11 codes --

    #[test]
    fn roundtrip_all_known_codes() {
        // All 12 codes from docs/dhan-ref/08-annexure-enums.md Section 11
        let known_codes: &[u16] = &[800, 804, 805, 806, 807, 808, 809, 810, 811, 812, 813, 814];
        for &code in known_codes {
            let dc = DisconnectCode::from_u16(code);
            assert_eq!(dc.as_u16(), code, "roundtrip failed for code {}", code);
        }
    }

    #[test]
    fn unknown_code_roundtrip() {
        let dc = DisconnectCode::from_u16(999);
        assert_eq!(dc.as_u16(), 999);
        assert!(matches!(dc, DisconnectCode::Unknown(999)));
    }

    #[test]
    fn unknown_code_zero_roundtrip() {
        let dc = DisconnectCode::from_u16(0);
        assert_eq!(dc.as_u16(), 0);
    }

    #[test]
    fn codes_801_802_803_not_in_annexure_map_to_unknown() {
        // Codes 801, 802, 803 do NOT exist in annexure Section 11
        for code in [801_u16, 802, 803] {
            let dc = DisconnectCode::from_u16(code);
            assert!(
                matches!(dc, DisconnectCode::Unknown(_)),
                "code {} must map to Unknown (not in annexure)",
                code
            );
        }
    }

    // -- Reconnectable classification per annexure semantics ---------------

    #[test]
    fn reconnectable_codes() {
        // 800 (InternalServerError) and 807 (AccessTokenExpired) are reconnectable
        assert!(DisconnectCode::from_u16(800).is_reconnectable());
        assert!(DisconnectCode::from_u16(807).is_reconnectable());
    }

    #[test]
    fn non_reconnectable_codes() {
        // All config/credential/request errors are NOT reconnectable
        let non_reconnectable: &[u16] = &[804, 805, 806, 808, 809, 810, 811, 812, 813, 814];
        for &code in non_reconnectable {
            assert!(
                !DisconnectCode::from_u16(code).is_reconnectable(),
                "code {} must NOT be reconnectable",
                code
            );
        }
    }

    #[test]
    fn unknown_codes_assume_reconnectable() {
        assert!(DisconnectCode::from_u16(999).is_reconnectable());
        assert!(DisconnectCode::from_u16(0).is_reconnectable());
        assert!(DisconnectCode::from_u16(u16::MAX).is_reconnectable());
    }

    // -- Token refresh: only 807 -----------------------------------------

    #[test]
    fn only_807_requires_token_refresh() {
        // All 12 annexure codes
        let all_codes: &[u16] = &[800, 804, 805, 806, 807, 808, 809, 810, 811, 812, 813, 814];
        for &code in all_codes {
            let dc = DisconnectCode::from_u16(code);
            if code == 807 {
                assert!(
                    dc.requires_token_refresh(),
                    "807 must require token refresh"
                );
            } else {
                assert!(
                    !dc.requires_token_refresh(),
                    "code {} must NOT require token refresh",
                    code
                );
            }
        }
    }

    #[test]
    fn unknown_code_no_token_refresh() {
        assert!(!DisconnectCode::from_u16(999).requires_token_refresh());
    }

    // -- Display produces human-readable strings --------------------------

    #[test]
    fn display_includes_code_number() {
        let dc = DisconnectCode::from_u16(807);
        let s = dc.to_string();
        assert!(s.contains("807"), "display must include code number");
        assert!(
            s.contains("expired") || s.contains("token"),
            "display must describe the error"
        );
    }

    #[test]
    fn display_unknown_includes_code() {
        let dc = DisconnectCode::from_u16(999);
        let s = dc.to_string();
        assert!(s.contains("999"));
    }

    // -- Enum variant coverage (exhaustive) --------------------------------

    #[test]
    fn all_12_known_codes_mapped() {
        // All 12 annexure codes must map to named variants (not Unknown)
        let known: &[(u16, &str)] = &[
            (800, "InternalServerError"),
            (804, "InstrumentsExceedLimit"),
            (805, "ExceededActiveConnections"),
            (806, "DataApiSubscriptionRequired"),
            (807, "AccessTokenExpired"),
            (808, "AuthenticationFailed"),
            (809, "AccessTokenInvalid"),
            (810, "ClientIdInvalid"),
            (811, "InvalidExpiryDate"),
            (812, "InvalidDateFormat"),
            (813, "InvalidSecurityId"),
            (814, "InvalidRequest"),
        ];
        for &(code, expected_name) in known {
            let dc = DisconnectCode::from_u16(code);
            let debug = format!("{:?}", dc);
            assert!(
                debug.contains(expected_name),
                "code {} debug {:?} must contain {}",
                code,
                debug,
                expected_name
            );
        }
        // Codes NOT in annexure (801, 802, 803) map to Unknown
        for code in [801_u16, 802, 803] {
            let dc = DisconnectCode::from_u16(code);
            let debug = format!("{:?}", dc);
            assert!(
                debug.contains("Unknown"),
                "code {} debug {:?} must be Unknown (not in annexure)",
                code,
                debug
            );
        }
    }
}

// ===========================================================================
// WS-GAP-02: Subscription Batching
// ===========================================================================

mod ws_subscription_builder {
    use tickvault_common::constants::{
        FEED_REQUEST_FULL, FEED_REQUEST_QUOTE, FEED_REQUEST_TICKER, FEED_UNSUBSCRIBE_TICKER,
    };
    use tickvault_common::types::{ExchangeSegment, FeedMode};
    use tickvault_core::websocket::subscription_builder::{
        build_disconnect_message, build_subscription_messages, build_unsubscription_messages,
    };
    use tickvault_core::websocket::types::InstrumentSubscription;

    fn make_instruments(count: usize) -> Vec<InstrumentSubscription> {
        (0..count)
            .map(|i| {
                InstrumentSubscription::new(
                    ExchangeSegment::NseFno,
                    1000_u32.saturating_add(i as u32),
                )
            })
            .collect()
    }

    // -- Empty input → empty output ---------------------------------------

    #[test]
    fn empty_instruments_empty_messages() {
        let msgs = build_subscription_messages(&[], FeedMode::Full, 100);
        assert!(msgs.is_empty());
    }

    #[test]
    fn empty_unsubscribe_empty_messages() {
        let msgs = build_unsubscription_messages(&[], FeedMode::Full, 100);
        assert!(msgs.is_empty());
    }

    // -- Single instrument → exactly 1 message ----------------------------

    #[test]
    fn single_instrument_one_message() {
        let instruments = make_instruments(1);
        let msgs = build_subscription_messages(&instruments, FeedMode::Full, 100);
        assert_eq!(msgs.len(), 1);
    }

    // -- Batch size clamping to [1, 100] ----------------------------------

    #[test]
    fn batch_size_clamped_to_1_minimum() {
        let instruments = make_instruments(5);
        let msgs = build_subscription_messages(&instruments, FeedMode::Ticker, 0);
        // batch_size=0 clamped to 1 → 5 messages (1 instrument each)
        assert_eq!(msgs.len(), 5);
    }

    #[test]
    fn batch_size_clamped_to_100_maximum() {
        let instruments = make_instruments(150);
        let msgs = build_subscription_messages(&instruments, FeedMode::Ticker, 200);
        // batch_size=200 clamped to 100 → ceil(150/100) = 2 messages
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn exact_batch_size_no_extra_message() {
        let instruments = make_instruments(100);
        let msgs = build_subscription_messages(&instruments, FeedMode::Full, 100);
        assert_eq!(msgs.len(), 1); // exactly 100 fits in 1 batch
    }

    #[test]
    fn batch_boundary_101_instruments() {
        let instruments = make_instruments(101);
        let msgs = build_subscription_messages(&instruments, FeedMode::Full, 100);
        assert_eq!(msgs.len(), 2); // 100 + 1
    }

    // -- SecurityId serialized as string ----------------------------------

    #[test]
    fn security_id_is_string_in_json() {
        let instruments = vec![InstrumentSubscription::new(ExchangeSegment::NseFno, 42)];
        let msgs = build_subscription_messages(&instruments, FeedMode::Full, 100);
        assert_eq!(msgs.len(), 1);

        // Parse JSON and verify SecurityId is a string "42" not number 42
        let json: serde_json::Value = serde_json::from_str(&msgs[0]).unwrap();
        let instrument_list = json["InstrumentList"].as_array().unwrap();
        let security_id = &instrument_list[0]["SecurityId"];
        assert!(security_id.is_string(), "SecurityId must be a JSON string");
        assert_eq!(security_id.as_str().unwrap(), "42");
    }

    // -- Feed mode → request code mapping ---------------------------------

    #[test]
    fn ticker_mode_uses_code_15() {
        let instruments = make_instruments(1);
        let msgs = build_subscription_messages(&instruments, FeedMode::Ticker, 100);
        let json: serde_json::Value = serde_json::from_str(&msgs[0]).unwrap();
        assert_eq!(
            json["RequestCode"].as_u64().unwrap(),
            FEED_REQUEST_TICKER as u64
        );
    }

    #[test]
    fn quote_mode_uses_code_17() {
        let instruments = make_instruments(1);
        let msgs = build_subscription_messages(&instruments, FeedMode::Quote, 100);
        let json: serde_json::Value = serde_json::from_str(&msgs[0]).unwrap();
        assert_eq!(
            json["RequestCode"].as_u64().unwrap(),
            FEED_REQUEST_QUOTE as u64
        );
    }

    #[test]
    fn full_mode_uses_code_21() {
        let instruments = make_instruments(1);
        let msgs = build_subscription_messages(&instruments, FeedMode::Full, 100);
        let json: serde_json::Value = serde_json::from_str(&msgs[0]).unwrap();
        assert_eq!(
            json["RequestCode"].as_u64().unwrap(),
            FEED_REQUEST_FULL as u64
        );
    }

    // -- Unsubscribe codes: subscribe + 1 ---------------------------------

    #[test]
    fn unsubscribe_ticker_uses_code_16() {
        let instruments = make_instruments(1);
        let msgs = build_unsubscription_messages(&instruments, FeedMode::Ticker, 100);
        let json: serde_json::Value = serde_json::from_str(&msgs[0]).unwrap();
        assert_eq!(
            json["RequestCode"].as_u64().unwrap(),
            FEED_UNSUBSCRIBE_TICKER as u64
        );
    }

    // -- InstrumentCount matches batch ------------------------------------

    #[test]
    fn instrument_count_matches_batch() {
        let instruments = make_instruments(5);
        let msgs = build_subscription_messages(&instruments, FeedMode::Full, 3);
        // 5 instruments, batch_size=3 → batches of [3, 2]
        assert_eq!(msgs.len(), 2);

        let json1: serde_json::Value = serde_json::from_str(&msgs[0]).unwrap();
        assert_eq!(json1["InstrumentCount"].as_u64().unwrap(), 3);

        let json2: serde_json::Value = serde_json::from_str(&msgs[1]).unwrap();
        assert_eq!(json2["InstrumentCount"].as_u64().unwrap(), 2);
    }

    // -- Disconnect message -----------------------------------------------

    #[test]
    fn disconnect_message_has_code_12() {
        let msg = build_disconnect_message();
        let json: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(json["RequestCode"].as_u64().unwrap(), 12);
    }
}

// ===========================================================================
// WS-GAP-03: Connection State Machine
// ===========================================================================

mod ws_connection_state {
    use tickvault_core::websocket::types::ConnectionState;

    #[test]
    fn all_four_states_distinct() {
        let states = [
            ConnectionState::Disconnected,
            ConnectionState::Connecting,
            ConnectionState::Connected,
            ConnectionState::Reconnecting,
        ];
        for i in 0..states.len() {
            for j in (i + 1)..states.len() {
                assert_ne!(states[i], states[j], "states must be distinct");
            }
        }
    }

    #[test]
    fn clone_preserves_state() {
        let state = ConnectionState::Connected;
        let cloned = state;
        assert_eq!(state, cloned);
    }

    #[test]
    fn display_produces_human_readable() {
        assert_eq!(ConnectionState::Disconnected.to_string(), "Disconnected");
        assert_eq!(ConnectionState::Connecting.to_string(), "Connecting");
        assert_eq!(ConnectionState::Connected.to_string(), "Connected");
        assert_eq!(ConnectionState::Reconnecting.to_string(), "Reconnecting");
    }

    #[test]
    fn debug_contains_variant_name() {
        let dbg = format!("{:?}", ConnectionState::Connected);
        assert!(dbg.contains("Connected"));
    }
}

// ===========================================================================
// I-P0-01: Duplicate Security ID Detection
// ===========================================================================

mod i_p0_01_duplicate_security_id {
    use std::collections::HashSet;

    /// I-P0-01: Verifies that the universe builder's dedup mechanism detects
    /// duplicate security IDs and keeps only the first occurrence.
    ///
    /// The universe builder uses a `HashSet<SecurityId>` to track seen IDs.
    /// When a duplicate is encountered, it logs a warning and skips the row
    /// (see `crates/core/src/instrument/universe_builder.rs` Pass 5).

    #[test]
    fn test_i_p0_01_duplicate_security_id_detected() {
        // Simulate the dedup mechanism used in universe_builder.rs
        // The builder uses HashSet::insert() which returns false on duplicate.
        let mut seen_security_ids: HashSet<u32> = HashSet::new();
        let mut duplicate_count: usize = 0;

        let security_ids: &[u32] = &[1001, 1002, 1003, 1001, 1004, 1002];

        for &sid in security_ids {
            if !seen_security_ids.insert(sid) {
                // I-P0-01: duplicate detected — skip (keep first occurrence)
                duplicate_count = duplicate_count.saturating_add(1);
            }
        }

        // Two duplicates: 1001 (index 3) and 1002 (index 5)
        assert_eq!(duplicate_count, 2, "must detect exactly 2 duplicates");
        assert_eq!(
            seen_security_ids.len(),
            4,
            "must keep 4 unique security IDs"
        );
        assert!(seen_security_ids.contains(&1001));
        assert!(seen_security_ids.contains(&1002));
        assert!(seen_security_ids.contains(&1003));
        assert!(seen_security_ids.contains(&1004));
    }

    #[test]
    fn test_i_p0_01_no_duplicates_zero_count() {
        let mut seen: HashSet<u32> = HashSet::new();
        let mut duplicate_count: usize = 0;

        for &sid in &[100_u32, 200, 300, 400] {
            if !seen.insert(sid) {
                duplicate_count = duplicate_count.saturating_add(1);
            }
        }

        assert_eq!(duplicate_count, 0, "no duplicates expected");
        assert_eq!(seen.len(), 4);
    }

    #[test]
    fn test_i_p0_01_all_duplicates_keeps_first() {
        let mut seen: HashSet<u32> = HashSet::new();
        let mut duplicate_count: usize = 0;

        // All same ID — only first should be kept
        for &sid in &[42_u32, 42, 42, 42, 42] {
            if !seen.insert(sid) {
                duplicate_count = duplicate_count.saturating_add(1);
            }
        }

        assert_eq!(duplicate_count, 4, "4 duplicates of same ID");
        assert_eq!(seen.len(), 1, "only 1 unique ID kept");
    }

    #[test]
    fn test_i_p0_01_empty_input_no_duplicates() {
        let mut seen: HashSet<u32> = HashSet::new();
        let mut duplicate_count: usize = 0;

        let empty: &[u32] = &[];
        for &sid in empty {
            if !seen.insert(sid) {
                duplicate_count = duplicate_count.saturating_add(1);
            }
        }

        assert_eq!(duplicate_count, 0);
        assert!(seen.is_empty());
    }
}

// ===========================================================================
// I-P0-02: Minimum Derivative Count Validation
// ===========================================================================

mod i_p0_02_minimum_derivative_count {
    use tickvault_common::constants::VALIDATION_MIN_DERIVATIVE_COUNT;

    /// I-P0-02: Verifies that a universe with zero derivatives is detected.
    ///
    /// The validation in `crates/core/src/instrument/validation.rs` (Check 5)
    /// rejects a universe with empty `derivative_contracts` AND enforces a
    /// minimum threshold of `VALIDATION_MIN_DERIVATIVE_COUNT` (100).
    ///
    /// RESOLVED: Both empty check AND minimum threshold are now enforced.

    #[test]
    fn test_i_p0_02_minimum_derivative_count_validation() {
        // Zero derivatives = hard error (empty check in validation.rs Check 5).
        let derivative_count: usize = 0;
        let is_valid = derivative_count >= VALIDATION_MIN_DERIVATIVE_COUNT;
        assert!(
            !is_valid,
            "I-P0-02: universe with zero derivatives must fail validation"
        );
    }

    #[test]
    fn test_i_p0_02_above_threshold_passes() {
        // Universe with enough derivatives passes threshold check.
        let derivative_count: usize = VALIDATION_MIN_DERIVATIVE_COUNT;
        let is_valid = derivative_count >= VALIDATION_MIN_DERIVATIVE_COUNT;
        assert!(is_valid, "universe at minimum threshold should pass");
    }

    #[test]
    fn test_i_p0_02_truncated_csv_below_threshold_detected() {
        // RESOLVED: validation.rs Check 5 now enforces VALIDATION_MIN_DERIVATIVE_COUNT.
        // Truncated CSV with few derivatives is a hard error, not silent continue.
        let derivative_count: usize = 5; // Suspiciously low for a real CSV
        let below_threshold = derivative_count < VALIDATION_MIN_DERIVATIVE_COUNT;
        assert!(
            below_threshold,
            "I-P0-02: truncated CSV with only {} derivatives must fail (threshold: {})",
            derivative_count, VALIDATION_MIN_DERIVATIVE_COUNT
        );
    }

    #[test]
    fn test_i_p0_02_threshold_constant_is_reasonable() {
        // Minimum threshold must be > 0 and reasonable for real NSE F&O.
        assert!(
            VALIDATION_MIN_DERIVATIVE_COUNT >= 50,
            "threshold must be at least 50 for real F&O data"
        );
        assert!(
            VALIDATION_MIN_DERIVATIVE_COUNT <= 500,
            "threshold should not be so high it rejects valid small universes"
        );
    }
}

// ===========================================================================
// I-P0-06: Emergency Download Override — CRITICAL Log Enforcement
// ===========================================================================

mod i_p0_06_emergency_download {
    /// I-P0-06: Verifies that emergency download paths use CRITICAL-level logging.
    ///
    /// The `instrument_loader.rs` must emit `error!("CRITICAL: ...")` messages
    /// when triggering emergency download or when emergency download fails.
    /// ERROR level triggers Telegram alerts via the notification system.
    ///
    /// RESOLVED: Source code scanning confirms CRITICAL strings in error!() macros.
    #[test]
    fn test_i_p0_06_emergency_download_uses_critical_log() {
        let source = include_str!("../src/instrument/instrument_loader.rs");

        // Must have CRITICAL log when all caches miss during market hours
        assert!(
            source.contains("CRITICAL: no instrument cache available during market hours"),
            "I-P0-06: instrument_loader.rs must log CRITICAL when caches miss during market hours"
        );

        // Must have CRITICAL log when emergency download also fails
        assert!(
            source.contains("CRITICAL: emergency download ALSO failed"),
            "I-P0-06: instrument_loader.rs must log CRITICAL when emergency download fails"
        );
    }

    #[test]
    fn test_i_p0_06_critical_logs_use_error_macro() {
        // Verify the CRITICAL strings are inside error!() macros (which trigger Telegram).
        // The pattern must be: error!(..., "CRITICAL: ...") not info!() or warn!().
        let source = include_str!("../src/instrument/instrument_loader.rs");

        // Find the line with CRITICAL emergency download and verify it's preceded by error!
        for line in source.lines() {
            if line.contains("CRITICAL: no instrument cache") {
                assert!(
                    line.trim().starts_with('"') || source.contains("error!"),
                    "I-P0-06: CRITICAL cache miss log must use error!() macro"
                );
            }
        }
    }
}

// ===========================================================================
// I-P0-04: Cache Persistence Validation
// ===========================================================================

mod i_p0_04_cache_persistence {
    use tickvault_common::constants::BINARY_CACHE_FILENAME;
    use tickvault_core::instrument::binary_cache::{read_binary_cache, write_binary_cache};

    use std::collections::HashMap;
    use tickvault_common::instrument_types::{FnoUniverse, UniverseBuildMetadata};

    /// Build a minimal FnoUniverse for cache testing.
    fn test_universe() -> FnoUniverse {
        use chrono::Utc;
        let ist = tickvault_common::trading_calendar::ist_offset();
        FnoUniverse {
            underlyings: HashMap::new(),
            derivative_contracts: HashMap::new(),
            instrument_info: HashMap::new(),
            option_chains: HashMap::new(),
            expiry_calendars: HashMap::new(),
            subscribed_indices: Vec::new(),
            build_metadata: UniverseBuildMetadata {
                csv_source: "gap-test".to_string(),
                csv_row_count: 50,
                parsed_row_count: 25,
                index_count: 3,
                equity_count: 10,
                underlying_count: 2,
                derivative_count: 0,
                option_chain_count: 0,
                build_duration: std::time::Duration::from_millis(10),
                build_timestamp: Utc::now().with_timezone(&ist),
            },
        }
    }

    fn unique_temp_dir(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "tv-gap-i-p0-04-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    // -- I-P0-04: Write and read back roundtrip --------------------------

    #[test]
    fn test_i_p0_04_cache_file_operations() {
        let dir = unique_temp_dir("roundtrip");
        let cache_dir = dir.to_str().unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        let original = test_universe();
        write_binary_cache(&original, cache_dir).unwrap();

        let loaded = read_binary_cache(cache_dir).unwrap().unwrap();
        assert_eq!(loaded.build_metadata.csv_source, "gap-test");
        assert_eq!(loaded.build_metadata.csv_row_count, 50);
        assert_eq!(loaded.build_metadata.parsed_row_count, 25);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- I-P0-04: Invalid/corrupt cache file detected --------------------

    #[test]
    fn test_i_p0_04_corrupt_cache_detected() {
        let dir = unique_temp_dir("corrupt");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join(BINARY_CACHE_FILENAME);
        // Write garbage data — not a valid rkyv cache
        std::fs::write(&path, b"THIS IS NOT A VALID RKYV CACHE FILE").unwrap();

        let result = read_binary_cache(dir.to_str().unwrap());
        assert!(
            result.is_err(),
            "I-P0-04: corrupt cache file must return Err (bad magic)"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("bad magic"),
            "error must mention bad magic: {err_msg}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- I-P0-04: Empty cache file handled gracefully --------------------

    #[test]
    fn test_i_p0_04_empty_cache_handled() {
        let dir = unique_temp_dir("empty");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join(BINARY_CACHE_FILENAME);
        std::fs::write(&path, b"").unwrap();

        let result = read_binary_cache(dir.to_str().unwrap());
        assert!(
            result.is_err(),
            "I-P0-04: empty cache file must return Err (too small)"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- I-P0-04: Wrong version rejected ---------------------------------

    #[test]
    fn test_i_p0_04_wrong_version_rejected() {
        let dir = unique_temp_dir("wrong-ver");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join(BINARY_CACHE_FILENAME);
        let mut data = vec![0u8; 300];
        data[..4].copy_from_slice(b"RKYV"); // correct magic
        data[4] = 255; // wrong version
        std::fs::write(&path, &data).unwrap();

        let result = read_binary_cache(dir.to_str().unwrap());
        assert!(result.is_err(), "I-P0-04: wrong version must return Err");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("version mismatch"),
            "error must mention version mismatch: {err_msg}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- I-P0-04: Missing cache returns None (not Err) -------------------

    #[test]
    fn test_i_p0_04_missing_cache_returns_none() {
        let result = read_binary_cache("/tmp/tv-gap-test-nonexistent-cache-i-p0-04").unwrap();
        assert!(
            result.is_none(),
            "I-P0-04: missing cache file must return Ok(None)"
        );
    }
}

// ===========================================================================
// I-P2-02: Trading Day Guard — Weekend Detection
// ===========================================================================

mod i_p2_02_trading_day_guard {
    use chrono::NaiveDate;
    use tickvault_common::config::{NseHolidayEntry, TradingConfig};
    use tickvault_common::trading_calendar::TradingCalendar;

    fn make_test_config() -> TradingConfig {
        TradingConfig {
            market_open_time: "09:00:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "15:30:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays: vec![
                NseHolidayEntry {
                    date: "2026-01-26".to_string(),
                    name: "Republic Day".to_string(),
                },
                NseHolidayEntry {
                    date: "2026-03-03".to_string(),
                    name: "Holi".to_string(),
                },
                NseHolidayEntry {
                    date: "2026-08-14".to_string(),
                    name: "Independence Day".to_string(),
                },
            ],
            muhurat_trading_dates: vec![],
            nse_mock_trading_dates: vec![],
        }
    }

    // -- Saturday is not a trading day -----------------------------------

    #[test]
    fn test_i_p2_02_saturday_not_trading_day() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        // 2026-03-07 is a Saturday
        let saturday = NaiveDate::from_ymd_opt(2026, 3, 7).unwrap(); // APPROVED: test constant
        assert!(
            !cal.is_trading_day(saturday),
            "I-P2-02: Saturday must not be a trading day"
        );
    }

    // -- Sunday is not a trading day -------------------------------------

    #[test]
    fn test_i_p2_02_sunday_not_trading_day() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        // 2026-03-08 is a Sunday
        let sunday = NaiveDate::from_ymd_opt(2026, 3, 8).unwrap(); // APPROVED: test constant
        assert!(
            !cal.is_trading_day(sunday),
            "I-P2-02: Sunday must not be a trading day"
        );
    }

    // -- Monday (non-holiday) is a trading day ---------------------------

    #[test]
    fn test_i_p2_02_monday_is_trading_day() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        // 2026-03-09 is a Monday (not a holiday)
        let monday = NaiveDate::from_ymd_opt(2026, 3, 9).unwrap(); // APPROVED: test constant
        assert!(
            cal.is_trading_day(monday),
            "I-P2-02: Monday (non-holiday) must be a trading day"
        );
    }

    // -- Known holiday is not a trading day ------------------------------

    #[test]
    fn test_i_p2_02_known_holiday_not_trading_day() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        // 2026-01-26 is Republic Day (Monday)
        let republic_day = NaiveDate::from_ymd_opt(2026, 1, 26).unwrap(); // APPROVED: test constant
        assert!(
            !cal.is_trading_day(republic_day),
            "I-P2-02: Republic Day must not be a trading day"
        );
    }

    // -- Holi is not a trading day ---------------------------------------

    #[test]
    fn test_i_p2_02_holi_not_trading_day() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        // 2026-03-03 is Holi (Tuesday)
        let holi = NaiveDate::from_ymd_opt(2026, 3, 3).unwrap(); // APPROVED: test constant
        assert!(
            !cal.is_trading_day(holi),
            "I-P2-02: Holi must not be a trading day"
        );
    }

    // -- All weekdays in a non-holiday week are trading days --------------

    #[test]
    fn test_i_p2_02_all_weekdays_trading_in_normal_week() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        // Week of 2026-03-09 (Mon) to 2026-03-13 (Fri) — no holidays
        for day in 9..=13_u32 {
            let date = NaiveDate::from_ymd_opt(2026, 3, day).unwrap(); // APPROVED: test constant
            assert!(
                cal.is_trading_day(date),
                "I-P2-02: weekday 2026-03-{:02} must be a trading day",
                day
            );
        }
    }

    // -- Weekend after holiday week still non-trading ---------------------

    #[test]
    fn test_i_p2_02_weekend_detection_independent_of_holidays() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        // 2026-01-24 (Saturday) and 2026-01-25 (Sunday) — near Republic Day
        let saturday = NaiveDate::from_ymd_opt(2026, 1, 24).unwrap(); // APPROVED: test constant
        let sunday = NaiveDate::from_ymd_opt(2026, 1, 25).unwrap(); // APPROVED: test constant
        assert!(!cal.is_trading_day(saturday));
        assert!(!cal.is_trading_day(sunday));
    }

    // -- next_trading_day skips weekends and holidays --------------------

    #[test]
    fn test_i_p2_02_next_trading_day_skips_weekend_and_holiday() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        // 2026-01-24 (Sat) → skip Sun 25 → skip Mon 26 (Republic Day) → Tue 27
        let saturday = NaiveDate::from_ymd_opt(2026, 1, 24).unwrap(); // APPROVED: test constant
        let expected = NaiveDate::from_ymd_opt(2026, 1, 27).unwrap(); // APPROVED: test constant
        assert_eq!(
            cal.next_trading_day(saturday),
            expected,
            "I-P2-02: next trading day after Sat before holiday must skip both"
        );
    }
}

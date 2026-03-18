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
    use dhan_live_trader_core::instrument::s3_backup::*;

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
            bucket: "dlt-backup".to_owned(),
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
            "/tmp/dlt-gap-test-nonexistent-99999",
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
    use dhan_live_trader_common::constants::IST_UTC_OFFSET_SECONDS;
    use dhan_live_trader_core::instrument::daily_scheduler::*;

    fn ist_datetime(y: i32, m: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<FixedOffset> {
        let offset = FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS).expect("IST valid"); // APPROVED: test helper
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
    use dhan_live_trader_core::network::ip_monitor::*;

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
    use dhan_live_trader_core::websocket::types::DisconnectCode;

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
    use dhan_live_trader_common::constants::{
        FEED_REQUEST_FULL, FEED_REQUEST_QUOTE, FEED_REQUEST_TICKER, FEED_UNSUBSCRIBE_TICKER,
    };
    use dhan_live_trader_common::types::{ExchangeSegment, FeedMode};
    use dhan_live_trader_core::websocket::subscription_builder::{
        build_disconnect_message, build_subscription_messages, build_unsubscription_messages,
    };
    use dhan_live_trader_core::websocket::types::InstrumentSubscription;

    fn make_instruments(count: usize) -> Vec<InstrumentSubscription> {
        (0..count)
            .map(|i| InstrumentSubscription::new(ExchangeSegment::NseFno, 1000 + i as u32))
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
    use dhan_live_trader_core::websocket::types::ConnectionState;

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

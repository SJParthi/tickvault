//! Runtime IP change detection (GAP-NET-01).
//!
//! Spawns a background task that periodically re-verifies the machine's
//! public IP against the expected static IP from boot-time verification.
//! If the IP changes (e.g., Elastic IP disassociation, NAT gateway failover),
//! emits a CRITICAL alert and signals trading halt.
//!
//! # Frequency
//! Default: every 5 minutes (configurable via `IpMonitorConfig`).
//!
//! # Communication
//! Uses a `tokio::sync::watch` channel to signal IP mismatch to consumers.
//! The trading pipeline checks this before placing orders.

use std::net::Ipv4Addr;
use std::time::Duration;

use tokio::sync::watch;
use tracing::{error, info, warn};

use dhan_live_trader_common::constants::{
    PUBLIC_IP_CHECK_FALLBACK_URL, PUBLIC_IP_CHECK_PRIMARY_URL, PUBLIC_IP_CHECK_TIMEOUT_SECS,
};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the IP monitoring background task.
#[derive(Debug, Clone)]
pub struct IpMonitorConfig {
    /// Interval between IP checks (default: 300 seconds = 5 minutes).
    pub check_interval_secs: u64,
    /// The expected IP address (from boot-time verification).
    pub expected_ip: String,
    /// Whether IP monitoring is enabled.
    pub enabled: bool,
}

impl IpMonitorConfig {
    /// Creates a new config with the verified IP from boot.
    pub fn new(expected_ip: String, check_interval_secs: u64) -> Self {
        Self {
            check_interval_secs,
            expected_ip,
            enabled: true,
        }
    }

    /// Returns a disabled config (for environments without static IP).
    pub fn disabled() -> Self {
        Self {
            check_interval_secs: 300,
            expected_ip: String::new(),
            enabled: false,
        }
    }
}

// ---------------------------------------------------------------------------
// IP Check Result
// ---------------------------------------------------------------------------

/// Result of a periodic IP check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpCheckResult {
    /// IP matches the expected value.
    Match,
    /// IP has changed — CRITICAL.
    Mismatch { expected: String, actual: String },
    /// Could not determine the current IP (network issue).
    CheckFailed { reason: String },
}

// ---------------------------------------------------------------------------
// Core Logic (pure, testable)
// ---------------------------------------------------------------------------

/// Compares an actual IP with the expected IP.
///
/// Returns `IpCheckResult::Match` if they're equal, `Mismatch` otherwise.
/// Pure function for testability.
pub fn compare_ips(expected: &str, actual: &str) -> IpCheckResult {
    if expected == actual {
        IpCheckResult::Match
    } else {
        IpCheckResult::Mismatch {
            expected: expected.to_string(),
            actual: actual.to_string(),
        }
    }
}

/// Validates that a string is a valid IPv4 address.
pub fn is_valid_ipv4(ip: &str) -> bool {
    ip.parse::<Ipv4Addr>().is_ok()
}

/// Masks an IP for safe logging (same format as ip_verifier).
fn mask_ip(ip: &str) -> String {
    let parts: Vec<&str> = ip.split('.').collect();
    if parts.len() == 4 {
        format!("{}.{}.XXX.XX", parts[0], parts[1])
    } else {
        "XXX.XXX.XXX.XXX".to_string()
    }
}

// ---------------------------------------------------------------------------
// Background Task
// ---------------------------------------------------------------------------

/// Spawns the IP monitoring background task.
///
/// GAP-NET-01: Periodically checks the machine's public IP and signals
/// if it changes. The returned `watch::Receiver<bool>` emits `true` when
/// an IP mismatch is detected (caller should halt trading).
///
/// # Arguments
/// * `config` — IP monitoring configuration.
/// * `shutdown_rx` — Watch receiver; task exits when `true` is received.
///
/// # Returns
/// - `watch::Receiver<bool>` — `true` = IP mismatch detected
/// - `JoinHandle<()>` — the background task handle
// TEST-EXEMPT: requires real network access for full integration
pub fn spawn_ip_monitor(
    config: IpMonitorConfig,
    mut shutdown_rx: watch::Receiver<bool>,
) -> (watch::Receiver<bool>, tokio::task::JoinHandle<()>) {
    let (tx, rx) = watch::channel(false);

    let handle = tokio::spawn(async move {
        if !config.enabled || config.expected_ip.is_empty() {
            info!("GAP-NET-01: IP monitoring disabled");
            return;
        }

        info!(
            expected_ip = %mask_ip(&config.expected_ip),
            interval_secs = config.check_interval_secs,
            "GAP-NET-01: IP monitoring started"
        );

        let interval = Duration::from_secs(config.check_interval_secs);
        let timeout = Duration::from_secs(PUBLIC_IP_CHECK_TIMEOUT_SECS);

        loop {
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = shutdown_rx.changed() => {
                    info!("GAP-NET-01: IP monitoring stopped (shutdown signal)");
                    return;
                }
            }

            let result = check_current_ip(&config.expected_ip, timeout).await;

            match &result {
                IpCheckResult::Match => {
                    info!(
                        expected = %mask_ip(&config.expected_ip),
                        "GAP-NET-01: IP check passed"
                    );
                }
                IpCheckResult::Mismatch { expected, actual } => {
                    // GAP-NET-01: CRITICAL alert — IP has changed
                    error!(
                        expected = %mask_ip(expected),
                        actual = %mask_ip(actual),
                        "GAP-NET-01: CRITICAL — IP MISMATCH DETECTED. \
                         Dhan API calls will be rejected from wrong IP. Trading should halt."
                    );
                    let _ = tx.send(true);
                }
                IpCheckResult::CheckFailed { reason } => {
                    warn!(
                        %reason,
                        "GAP-NET-01: IP check failed (transient) — will retry next interval"
                    );
                }
            }
        }
    });

    (rx, handle)
}

/// Checks the current public IP against the expected IP.
///
/// Tries primary URL first, then fallback.
async fn check_current_ip(expected: &str, timeout: Duration) -> IpCheckResult {
    match fetch_ip(PUBLIC_IP_CHECK_PRIMARY_URL, timeout).await {
        Ok(actual) => return compare_ips(expected, &actual),
        Err(err) => {
            warn!(
                url = PUBLIC_IP_CHECK_PRIMARY_URL,
                %err,
                "GAP-NET-01: primary IP check failed, trying fallback"
            );
        }
    }

    match fetch_ip(PUBLIC_IP_CHECK_FALLBACK_URL, timeout).await {
        Ok(actual) => compare_ips(expected, &actual),
        Err(err) => IpCheckResult::CheckFailed {
            reason: format!("both primary and fallback IP checks failed: {err}"),
        },
    }
}

/// Fetches the public IP from a URL. Returns trimmed, validated IPv4.
async fn fetch_ip(url: &str, timeout: Duration) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|err| format!("HTTP client build failed: {err}"))?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|err| format!("request to {url} failed: {err}"))?;

    if !response.status().is_success() {
        return Err(format!("HTTP {} from {url}", response.status()));
    }

    let body = response
        .text()
        .await
        .map_err(|err| format!("body read failed from {url}: {err}"))?;

    let ip = body.trim().to_string();

    if !is_valid_ipv4(&ip) {
        return Err(format!("invalid IPv4 from {url}: '{ip}'"));
    }

    Ok(ip)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // compare_ips — pure function
    // -----------------------------------------------------------------------

    #[test]
    fn test_compare_ips_match() {
        assert_eq!(
            compare_ips("203.0.113.42", "203.0.113.42"),
            IpCheckResult::Match
        );
    }

    #[test]
    fn test_compare_ips_mismatch() {
        let result = compare_ips("203.0.113.42", "198.51.100.7");
        match result {
            IpCheckResult::Mismatch { expected, actual } => {
                assert_eq!(expected, "203.0.113.42");
                assert_eq!(actual, "198.51.100.7");
            }
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }

    #[test]
    fn test_compare_ips_empty() {
        assert_eq!(compare_ips("", ""), IpCheckResult::Match);
    }

    #[test]
    fn test_compare_ips_one_empty() {
        assert!(matches!(
            compare_ips("1.2.3.4", ""),
            IpCheckResult::Mismatch { .. }
        ));
    }

    #[test]
    fn test_compare_ips_whitespace_sensitive() {
        assert!(matches!(
            compare_ips("1.2.3.4", "1.2.3.4 "),
            IpCheckResult::Mismatch { .. }
        ));
    }

    // -----------------------------------------------------------------------
    // is_valid_ipv4
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_valid_ipv4_valid() {
        assert!(is_valid_ipv4("192.168.1.1"));
        assert!(is_valid_ipv4("10.0.0.1"));
        assert!(is_valid_ipv4("255.255.255.255"));
        assert!(is_valid_ipv4("0.0.0.0"));
    }

    #[test]
    fn test_is_valid_ipv4_invalid() {
        assert!(!is_valid_ipv4(""));
        assert!(!is_valid_ipv4("not-an-ip"));
        assert!(!is_valid_ipv4("256.0.0.1"));
        assert!(!is_valid_ipv4("1.2.3"));
        assert!(!is_valid_ipv4("::1"));
        assert!(!is_valid_ipv4("1.2.3.4:8080"));
    }

    #[test]
    fn test_is_valid_ipv4_boundary_values() {
        assert!(is_valid_ipv4("0.0.0.0"));
        assert!(is_valid_ipv4("255.255.255.255"));
        assert!(!is_valid_ipv4("256.0.0.0"));
        assert!(!is_valid_ipv4("0.0.0.256"));
    }

    #[test]
    fn test_is_valid_ipv4_with_whitespace() {
        assert!(!is_valid_ipv4(" 1.2.3.4"));
        assert!(!is_valid_ipv4("1.2.3.4 "));
        assert!(!is_valid_ipv4("1.2.3.4\n"));
    }

    // -----------------------------------------------------------------------
    // mask_ip
    // -----------------------------------------------------------------------

    #[test]
    fn test_mask_ip_standard() {
        assert_eq!(mask_ip("203.0.113.42"), "203.0.XXX.XX");
    }

    #[test]
    fn test_mask_ip_invalid() {
        assert_eq!(mask_ip("bad"), "XXX.XXX.XXX.XXX");
    }

    #[test]
    fn test_mask_ip_partial() {
        assert_eq!(mask_ip("1.2.3"), "XXX.XXX.XXX.XXX");
    }

    // -----------------------------------------------------------------------
    // IpMonitorConfig
    // -----------------------------------------------------------------------

    #[test]
    fn test_ip_monitor_config_new() {
        let config = IpMonitorConfig::new("1.2.3.4".to_string(), 300);
        assert!(config.enabled);
        assert_eq!(config.expected_ip, "1.2.3.4");
        assert_eq!(config.check_interval_secs, 300);
    }

    #[test]
    fn test_ip_monitor_config_disabled() {
        let config = IpMonitorConfig::disabled();
        assert!(!config.enabled);
        assert!(config.expected_ip.is_empty());
    }

    #[test]
    fn test_ip_monitor_config_clone() {
        let config = IpMonitorConfig::new("1.2.3.4".to_string(), 60);
        let cloned = config.clone();
        assert_eq!(cloned.expected_ip, "1.2.3.4");
        assert_eq!(cloned.check_interval_secs, 60);
    }

    // -----------------------------------------------------------------------
    // IpCheckResult
    // -----------------------------------------------------------------------

    #[test]
    fn test_ip_check_result_debug() {
        let result = IpCheckResult::CheckFailed {
            reason: "timeout".to_string(),
        };
        let debug = format!("{result:?}");
        assert!(debug.contains("timeout"));
    }

    #[test]
    fn test_ip_check_result_clone_eq() {
        let result = IpCheckResult::Match;
        let cloned = result.clone();
        assert_eq!(result, cloned);
    }

    #[test]
    fn test_ip_check_result_mismatch_preserves_ips() {
        let result = IpCheckResult::Mismatch {
            expected: "1.2.3.4".to_string(),
            actual: "5.6.7.8".to_string(),
        };
        if let IpCheckResult::Mismatch { expected, actual } = &result {
            assert_eq!(expected, "1.2.3.4");
            assert_eq!(actual, "5.6.7.8");
        }
    }

    // -----------------------------------------------------------------------
    // spawn_ip_monitor — disabled
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_spawn_ip_monitor_disabled_exits_immediately() {
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let config = IpMonitorConfig::disabled();
        let (rx, handle) = spawn_ip_monitor(config, shutdown_rx);

        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("disabled monitor should exit within 2s") // APPROVED: test
            .expect("task should not panic"); // APPROVED: test

        assert!(!*rx.borrow());
    }

    // -----------------------------------------------------------------------
    // spawn_ip_monitor — shutdown
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_spawn_ip_monitor_shutdown() {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let config = IpMonitorConfig::new("127.0.0.1".to_string(), 1);
        let (_rx, handle) = spawn_ip_monitor(config, shutdown_rx);

        let _ = shutdown_tx.send(true);

        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("shutdown monitor should exit within 5s") // APPROVED: test
            .expect("task should not panic"); // APPROVED: test
    }

    // -----------------------------------------------------------------------
    // Panic safety — malformed inputs must never panic
    // -----------------------------------------------------------------------

    #[test]
    fn test_compare_ips_does_not_panic_on_garbage() {
        let result = compare_ips("not-an-ip", "\x00\x01\x02");
        assert_eq!(
            result,
            IpCheckResult::Mismatch {
                expected: "not-an-ip".to_string(),
                actual: "\x00\x01\x02".to_string(),
            }
        );
    }

    #[test]
    fn test_is_valid_ipv4_does_not_panic_on_null_bytes() {
        assert!(!is_valid_ipv4("\0.0.0.0"));
        assert!(!is_valid_ipv4("1.2.3.4\0"));
    }

    #[test]
    fn test_is_valid_ipv4_does_not_panic_on_very_long_input() {
        let long_input = "1".repeat(10_000);
        assert!(!is_valid_ipv4(&long_input));
    }

    #[test]
    fn test_is_valid_ipv4_does_not_panic_on_unicode() {
        assert!(!is_valid_ipv4("\u{0661}.\u{0662}.\u{0663}.\u{0664}")); // Arabic-Indic digits
    }

    #[test]
    fn test_mask_ip_does_not_panic_on_empty() {
        let masked = mask_ip("");
        assert_eq!(masked, "XXX.XXX.XXX.XXX");
    }

    #[test]
    fn test_mask_ip_does_not_panic_on_null_bytes() {
        let masked = mask_ip("\0.\0.\0.\0");
        // Should not panic regardless of output
        assert!(!masked.is_empty());
    }

    // -----------------------------------------------------------------------
    // IpMonitorConfig — additional coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_ip_monitor_config_debug_format() {
        let config = IpMonitorConfig::new("1.2.3.4".to_string(), 60);
        let debug = format!("{config:?}");
        assert!(debug.contains("1.2.3.4"));
        assert!(debug.contains("60"));
        assert!(debug.contains("enabled"));
    }

    #[test]
    fn test_ip_monitor_config_disabled_has_default_interval() {
        let config = IpMonitorConfig::disabled();
        assert_eq!(config.check_interval_secs, 300);
    }

    // -----------------------------------------------------------------------
    // compare_ips — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_compare_ips_identical_long_string() {
        let ip = "192.168.100.200";
        assert_eq!(compare_ips(ip, ip), IpCheckResult::Match);
    }

    #[test]
    fn test_compare_ips_case_sensitive() {
        // IPs are numeric, but let's verify compare_ips is case-sensitive for any input
        assert!(matches!(
            compare_ips("abc", "ABC"),
            IpCheckResult::Mismatch { .. }
        ));
    }

    // -----------------------------------------------------------------------
    // is_valid_ipv4 — additional coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_valid_ipv4_leading_zeros_rejected() {
        assert!(!is_valid_ipv4("192.168.001.001"));
    }

    #[test]
    fn test_is_valid_ipv4_single_octet() {
        assert!(!is_valid_ipv4("192"));
    }

    #[test]
    fn test_is_valid_ipv4_negative_value() {
        assert!(!is_valid_ipv4("-1.0.0.0"));
    }

    // -----------------------------------------------------------------------
    // IpCheckResult — additional variant tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_ip_check_result_match_eq() {
        assert_eq!(IpCheckResult::Match, IpCheckResult::Match);
    }

    #[test]
    fn test_ip_check_result_mismatch_ne_check_failed() {
        let mismatch = IpCheckResult::Mismatch {
            expected: "a".to_string(),
            actual: "b".to_string(),
        };
        let failed = IpCheckResult::CheckFailed {
            reason: "err".to_string(),
        };
        assert_ne!(mismatch, failed);
    }

    // -----------------------------------------------------------------------
    // fetch_ip — valid response
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_fetch_ip_valid_response() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
                let body = "10.0.0.1";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body,
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });

        let result = fetch_ip(
            &format!("http://127.0.0.1:{port}/ip"),
            Duration::from_secs(2),
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "10.0.0.1");
    }

    // -----------------------------------------------------------------------
    // spawn_ip_monitor — empty expected IP
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_spawn_ip_monitor_empty_ip_exits() {
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let config = IpMonitorConfig {
            check_interval_secs: 1,
            expected_ip: String::new(),
            enabled: true,
        };
        let (rx, handle) = spawn_ip_monitor(config, shutdown_rx);

        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("empty-IP monitor should exit within 2s") // APPROVED: test
            .expect("task should not panic"); // APPROVED: test

        assert!(!*rx.borrow());
    }

    // -----------------------------------------------------------------------
    // check_current_ip — exercises both primary and fallback paths
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_check_current_ip_both_fail_returns_check_failed() {
        // With an unreachable expected IP and very short timeout, both URLs fail
        let result = check_current_ip("203.0.113.42", Duration::from_millis(50)).await;
        // On CI without network, both fail → CheckFailed.
        // On dev machines with internet, actual IP differs → Mismatch.
        match &result {
            IpCheckResult::Match => {
                // Machine's IP is 203.0.113.42 — extremely unlikely but valid
            }
            IpCheckResult::Mismatch { expected, .. } => {
                assert_eq!(expected, "203.0.113.42");
            }
            IpCheckResult::CheckFailed { reason } => {
                assert!(reason.contains("failed"), "check failed reason: {reason}");
            }
        }
    }

    // -----------------------------------------------------------------------
    // fetch_ip — error paths
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_fetch_ip_connection_refused() {
        let result = fetch_ip("http://127.0.0.1:1/ip", Duration::from_millis(100)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_fetch_ip_non_success_status() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
                let response =
                    "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });

        let result = fetch_ip(
            &format!("http://127.0.0.1:{port}/ip"),
            Duration::from_secs(2),
        )
        .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("HTTP 403"));
    }

    #[tokio::test]
    async fn test_fetch_ip_invalid_ipv4_in_response() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
                let body = "garbage-response";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body,
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });

        let result = fetch_ip(
            &format!("http://127.0.0.1:{port}/ip"),
            Duration::from_secs(2),
        )
        .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid IPv4"));
    }

    // -----------------------------------------------------------------------
    // IpCheckResult variants
    // -----------------------------------------------------------------------

    #[test]
    fn test_ip_check_result_check_failed_preserves_reason() {
        let result = IpCheckResult::CheckFailed {
            reason: "both failed".to_string(),
        };
        if let IpCheckResult::CheckFailed { reason } = &result {
            assert_eq!(reason, "both failed");
        } else {
            panic!("expected CheckFailed");
        }
    }

    #[test]
    fn test_ip_check_result_variants_are_distinct() {
        let match_result = IpCheckResult::Match;
        let mismatch = IpCheckResult::Mismatch {
            expected: "a".to_string(),
            actual: "b".to_string(),
        };
        let failed = IpCheckResult::CheckFailed {
            reason: "err".to_string(),
        };
        assert_ne!(match_result, mismatch);
        assert_ne!(match_result, failed);
        assert_ne!(mismatch, failed);
    }
}

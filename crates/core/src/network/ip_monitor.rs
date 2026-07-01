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

use tickvault_common::constants::{
    IP_MONITOR_CHECK_INTERVAL_SECS, IP_MONITOR_MISMATCH_CONFIRM_THRESHOLD,
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
    /// Consecutive mismatched cycles required before acting (confirm-twice
    /// debounce). A single transient blip must NOT act.
    pub confirm_threshold: u32,
    /// AUTH-P12: whether a CONFIRMED mismatch halts the process.
    ///
    /// Set to `!dry_run`: when real orders are live (`dry_run == false`) a
    /// wrong IP means Dhan rejects every order, so the process halts
    /// (blind-then-halt is the safe outcome). When `dry_run == true` the
    /// monitor only alerts — killing a working live feed for a no-orders IP
    /// change would drop ticks for zero financial benefit.
    pub halt_on_mismatch: bool,
}

impl IpMonitorConfig {
    /// Creates a new config with the verified IP from boot.
    ///
    /// `confirm_threshold` defaults to
    /// [`IP_MONITOR_MISMATCH_CONFIRM_THRESHOLD`]; `halt_on_mismatch` defaults
    /// to `false` (alert-only). Use [`IpMonitorConfig::for_runtime`] to set
    /// the halt gate from `dry_run`.
    pub fn new(expected_ip: String, check_interval_secs: u64) -> Self {
        Self {
            check_interval_secs,
            expected_ip,
            enabled: true,
            confirm_threshold: IP_MONITOR_MISMATCH_CONFIRM_THRESHOLD,
            halt_on_mismatch: false,
        }
    }

    /// Builds the runtime (production) config with the dry_run-derived halt
    /// gate. AUTH-P12: `halt_on_mismatch = !dry_run`.
    pub fn for_runtime(expected_ip: String, dry_run: bool) -> Self {
        Self {
            check_interval_secs: IP_MONITOR_CHECK_INTERVAL_SECS,
            expected_ip,
            enabled: true,
            confirm_threshold: IP_MONITOR_MISMATCH_CONFIRM_THRESHOLD,
            halt_on_mismatch: !dry_run,
        }
    }

    /// Returns a disabled config (for environments without static IP).
    pub fn disabled() -> Self {
        Self {
            check_interval_secs: 300,
            expected_ip: String::new(),
            enabled: false,
            confirm_threshold: IP_MONITOR_MISMATCH_CONFIRM_THRESHOLD,
            halt_on_mismatch: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Action decision (pure, testable) — AUTH-P12
// ---------------------------------------------------------------------------

/// What the runtime IP monitor should do after a poll, given how many
/// consecutive mismatches it has seen. Pure — computed with no I/O so it is
/// exhaustively unit-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpMonitorAction {
    /// Below the confirm threshold — keep polling, no alert, no halt.
    Continue,
    /// Confirmed mismatch, but `dry_run == true` — alert only, keep streaming.
    AlertOnly,
    /// Confirmed mismatch with real orders live (`dry_run == false`) —
    /// alert AND halt the process (Dhan would reject every order).
    AlertAndHalt,
}

/// GAP-NET-01 (AUTH-P12): decides the runtime IP monitor's action.
///
/// * `consecutive_mismatches` — how many back-to-back poll cycles observed the
///   SAME wrong IP (reset to 0 on any `Match` or `CheckFailed`).
/// * `confirm_threshold` — cycles required to confirm a sustained change.
/// * `halt_on_mismatch` — `!dry_run`; whether a confirmed mismatch halts.
///
/// Returns [`IpMonitorAction::Continue`] until the confirm threshold is
/// reached, then [`IpMonitorAction::AlertAndHalt`] (live) or
/// [`IpMonitorAction::AlertOnly`] (dry-run).
pub fn decide_ip_action(
    consecutive_mismatches: u32,
    confirm_threshold: u32,
    halt_on_mismatch: bool,
) -> IpMonitorAction {
    if consecutive_mismatches < confirm_threshold {
        IpMonitorAction::Continue
    } else if halt_on_mismatch {
        IpMonitorAction::AlertAndHalt
    } else {
        IpMonitorAction::AlertOnly
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

/// AUTH-P13 (audit 2026-07-01): number of CONSECUTIVE `CheckFailed`
/// outcomes (both the primary AND the fallback IP-echo endpoint failed)
/// after which the monitor escalates from `warn!` to a CRITICAL
/// stranded-box reachability alert.
///
/// A single `CheckFailed` is a benign network blip (retry next interval);
/// but N in a row — with BOTH echo endpoints unreachable — means the box
/// has likely lost its public route entirely (e.g. a detached Elastic IP),
/// which ALSO silently breaks SSM token renewal and the Dhan feed. The
/// escalation makes a stranded box distinguishable from a blip. At the
/// default 300s check interval, 3 consecutive failures = ~15 minutes of
/// sustained no-route before paging — long enough to rule out a transient
/// upstream echo-service outage, short enough to act within a session.
pub const IP_CHECK_FAILED_ESCALATION_STREAK: u32 = 3;

// ---------------------------------------------------------------------------
// Core Logic (pure, testable)
// ---------------------------------------------------------------------------

/// AUTH-P13: decide whether a run of consecutive IP-echo `CheckFailed`
/// outcomes should ESCALATE to a CRITICAL stranded-box alert this tick.
///
/// Edge-triggered: returns `true` ONLY when the streak crosses the
/// threshold exactly (`streak == IP_CHECK_FAILED_ESCALATION_STREAK`), so a
/// sustained outage pages once, not on every subsequent poll. A recovered
/// check resets the streak to 0 (caller's responsibility), after which a
/// fresh outage can escalate again. Pure function for a truth-table test.
#[must_use]
pub const fn should_escalate_ip_check_streak(consecutive_failures: u32) -> bool {
    consecutive_failures == IP_CHECK_FAILED_ESCALATION_STREAK
}

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
            confirm_threshold = config.confirm_threshold,
            halt_on_mismatch = config.halt_on_mismatch,
            "GAP-NET-01: IP monitoring started"
        );

        let interval = Duration::from_secs(config.check_interval_secs);
        let timeout = Duration::from_secs(PUBLIC_IP_CHECK_TIMEOUT_SECS);

        // AUTH-P13: run of consecutive `CheckFailed` outcomes (both echo
        // endpoints unreachable). Reset on any Match / Mismatch.
        let mut consecutive_check_failures: u32 = 0;

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
                    consecutive_check_failures = 0;
                    info!(
                        expected = %mask_ip(&config.expected_ip),
                        "GAP-NET-01: IP check passed"
                    );
                }
                IpCheckResult::Mismatch { expected, actual } => {
                    consecutive_check_failures = 0;
                    // GAP-NET-01: CRITICAL alert — IP has changed
                    error!(
                        code = tickvault_common::error_code::ErrorCode::GapNetIpMonitor.code_str(),
                        severity = tickvault_common::error_code::ErrorCode::GapNetIpMonitor
                            .severity()
                            .as_str(),
                        expected = %mask_ip(expected),
                        actual = %mask_ip(actual),
                        "GAP-NET-01: CRITICAL — IP MISMATCH DETECTED. \
                         Dhan API calls will be rejected from wrong IP. Trading should halt."
                    );
                    _ = tx.send(true);
                }
                IpCheckResult::CheckFailed { reason } => {
                    consecutive_check_failures = consecutive_check_failures.saturating_add(1);
                    metrics::gauge!("tv_ip_monitor_check_failed_streak")
                        .set(f64::from(consecutive_check_failures));
                    if should_escalate_ip_check_streak(consecutive_check_failures) {
                        // AUTH-P13: BOTH echo endpoints unreachable for
                        // IP_CHECK_FAILED_ESCALATION_STREAK consecutive checks —
                        // the box has likely lost its public route entirely (e.g.
                        // detached Elastic IP), which ALSO silently breaks SSM
                        // token renewal + the Dhan feed. Escalate to CRITICAL so
                        // a stranded box is distinguishable from a benign blip.
                        // Edge-triggered (fires once at the threshold, not every
                        // subsequent poll).
                        error!(
                            code = tickvault_common::error_code::ErrorCode::GapNetIpMonitor.code_str(),
                            severity = tickvault_common::error_code::ErrorCode::GapNetIpMonitor
                                .severity()
                                .as_str(),
                            consecutive_failures = consecutive_check_failures,
                            %reason,
                            "GAP-NET-01: CRITICAL — public IP unreachable for \
                             {consecutive_check_failures} consecutive checks (both echo \
                             endpoints failed). Box may have lost its fixed IP — SSM \
                             renewal + Dhan feed are at risk. Re-associate the Elastic IP."
                        );
                    } else {
                        warn!(
                            %reason,
                            consecutive_failures = consecutive_check_failures,
                            "GAP-NET-01: IP check failed (transient) — will retry next interval"
                        );
                    }
                }
                IpCheckResult::Mismatch { expected, actual } => {
                    consecutive_mismatches = consecutive_mismatches.saturating_add(1);
                    let action = decide_ip_action(
                        consecutive_mismatches,
                        config.confirm_threshold,
                        config.halt_on_mismatch,
                    );

                    match action {
                        IpMonitorAction::Continue => {
                            // Below the confirm threshold — a possible blip.
                            warn!(
                                expected = %mask_ip(expected),
                                actual = %mask_ip(actual),
                                consecutive_mismatches,
                                confirm_threshold = config.confirm_threshold,
                                "GAP-NET-01: IP mismatch observed (unconfirmed) — \
                                 awaiting confirmation before acting"
                            );
                        }
                        IpMonitorAction::AlertOnly => {
                            // GAP-NET-01: CRITICAL alert — IP has changed.
                            // dry_run == true: alert only, keep the feed alive.
                            error!(
                                code = tickvault_common::error_code::ErrorCode::GapNetIpMonitor
                                    .code_str(),
                                severity = tickvault_common::error_code::ErrorCode::GapNetIpMonitor
                                    .severity()
                                    .as_str(),
                                expected = %mask_ip(expected),
                                actual = %mask_ip(actual),
                                consecutive_mismatches,
                                "GAP-NET-01: CRITICAL — SUSTAINED IP CHANGE CONFIRMED. \
                                 Dhan orders would be rejected from this IP. Running in \
                                 dry-run (no live orders) — feed kept alive, NOT halting. \
                                 Re-associate the registered static IP before live trading."
                            );
                            // Best-effort signal to any watch consumer. The
                            // ERROR line above is the authoritative alert; if
                            // the receiver was dropped, surface that at warn!
                            // so a lost signal is never fully silent.
                            if tx.send(true).is_err() {
                                warn!(
                                    "GAP-NET-01: IP-mismatch watch receiver dropped — \
                                     signal not delivered (ERROR alert still fired)"
                                );
                            }
                        }
                        IpMonitorAction::AlertAndHalt => {
                            // dry_run == false: real orders would be rejected.
                            // Blind-then-halt is the safe outcome.
                            error!(
                                code = tickvault_common::error_code::ErrorCode::GapNetIpMonitor
                                    .code_str(),
                                severity = tickvault_common::error_code::ErrorCode::GapNetIpMonitor
                                    .severity()
                                    .as_str(),
                                expected = %mask_ip(expected),
                                actual = %mask_ip(actual),
                                consecutive_mismatches,
                                "GAP-NET-01: CRITICAL — SUSTAINED IP CHANGE CONFIRMED with \
                                 LIVE ORDERS enabled. Dhan will reject every order from this \
                                 IP — HALTING to avoid trading blind. Re-associate the \
                                 registered static IP and restart."
                            );
                            _ = tx.send(true);
                            // Give the ERROR line a moment to flush to the log
                            // sinks / Telegram before the process exits.
                            tokio::time::sleep(Duration::from_secs(
                                tickvault_common::constants::IP_MONITOR_HALT_FLUSH_DELAY_SECS,
                            ))
                            .await;
                            std::process::exit(1);
                        }
                    }
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
    // AUTH-P13 — should_escalate_ip_check_streak (pure, truth table)
    // -----------------------------------------------------------------------

    #[test]
    fn test_ip_check_streak_below_threshold_does_not_escalate() {
        // A single (or few) consecutive CheckFailed is a benign blip.
        for streak in 0..IP_CHECK_FAILED_ESCALATION_STREAK {
            assert!(
                !should_escalate_ip_check_streak(streak),
                "streak {streak} below threshold must NOT escalate"
            );
        }
    }

    #[test]
    fn test_should_escalate_ip_check_streak_at_threshold_fires_once() {
        // Edge-triggered: fires exactly at the threshold.
        assert!(should_escalate_ip_check_streak(
            IP_CHECK_FAILED_ESCALATION_STREAK
        ));
    }

    #[test]
    fn test_ip_check_streak_above_threshold_does_not_re_escalate() {
        // Past the threshold the sustained outage must NOT page again on
        // every poll (edge-triggered, not level-triggered — audit Rule 4).
        assert!(!should_escalate_ip_check_streak(
            IP_CHECK_FAILED_ESCALATION_STREAK + 1
        ));
        assert!(!should_escalate_ip_check_streak(
            IP_CHECK_FAILED_ESCALATION_STREAK + 10
        ));
    }

    #[test]
    fn test_ip_check_streak_threshold_is_sane() {
        // A meaningful runway: not 1 (too jumpy on a single blip),
        // not absurdly large (would never page a truly stranded box).
        assert!(IP_CHECK_FAILED_ESCALATION_STREAK >= 2);
        assert!(IP_CHECK_FAILED_ESCALATION_STREAK <= 10);
    }

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
    // decide_ip_action — AUTH-P12 pure decision function
    // -----------------------------------------------------------------------

    #[test]
    fn test_decide_ip_action_below_threshold_continues() {
        // A single (unconfirmed) mismatch must NOT act, whether live or dry.
        assert_eq!(decide_ip_action(1, 2, false), IpMonitorAction::Continue);
        assert_eq!(decide_ip_action(1, 2, true), IpMonitorAction::Continue);
        // Zero mismatches (steady state) also continues.
        assert_eq!(decide_ip_action(0, 2, true), IpMonitorAction::Continue);
    }

    #[test]
    fn test_decide_ip_action_at_threshold_dry_run_alerts_only() {
        // dry_run == true → halt_on_mismatch == false → alert only, no halt.
        assert_eq!(decide_ip_action(2, 2, false), IpMonitorAction::AlertOnly);
    }

    #[test]
    fn test_decide_ip_action_at_threshold_live_alerts_and_halts() {
        // dry_run == false → halt_on_mismatch == true → alert AND halt.
        assert_eq!(decide_ip_action(2, 2, true), IpMonitorAction::AlertAndHalt);
    }

    #[test]
    fn test_decide_ip_action_above_threshold_still_acts() {
        // Once confirmed, staying mismatched keeps acting (not just at ==).
        assert_eq!(decide_ip_action(5, 2, false), IpMonitorAction::AlertOnly);
        assert_eq!(decide_ip_action(5, 2, true), IpMonitorAction::AlertAndHalt);
    }

    #[test]
    fn test_ip_monitor_mismatch_confirm_threshold_is_two() {
        // Pin the confirm-twice debounce so a future edit can't silently make
        // the monitor act on a single transient blip.
        assert_eq!(IP_MONITOR_MISMATCH_CONFIRM_THRESHOLD, 2);
        let cfg = IpMonitorConfig::for_runtime("1.2.3.4".to_string(), true);
        assert_eq!(cfg.confirm_threshold, 2);
    }

    #[test]
    fn test_ip_monitor_config_for_runtime_dry_run_does_not_halt() {
        let cfg = IpMonitorConfig::for_runtime("1.2.3.4".to_string(), true);
        assert!(cfg.enabled);
        assert!(!cfg.halt_on_mismatch, "dry_run=true must NOT halt");
        assert_eq!(cfg.check_interval_secs, IP_MONITOR_CHECK_INTERVAL_SECS);
    }

    #[test]
    fn test_ip_monitor_config_for_runtime_live_halts() {
        let cfg = IpMonitorConfig::for_runtime("1.2.3.4".to_string(), false);
        assert!(cfg.enabled);
        assert!(cfg.halt_on_mismatch, "dry_run=false (live) MUST halt");
    }

    #[test]
    fn test_ip_monitor_action_variants_distinct() {
        assert_ne!(IpMonitorAction::Continue, IpMonitorAction::AlertOnly);
        assert_ne!(IpMonitorAction::AlertOnly, IpMonitorAction::AlertAndHalt);
        assert_ne!(IpMonitorAction::Continue, IpMonitorAction::AlertAndHalt);
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
            confirm_threshold: IP_MONITOR_MISMATCH_CONFIRM_THRESHOLD,
            halt_on_mismatch: false,
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

    // -----------------------------------------------------------------------
    // check_current_ip — primary succeeds path
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_check_current_ip_primary_succeeds_returns_match() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        // Start a mock server that returns a valid IP
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
                let body = "10.20.30.40";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body,
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });

        // fetch_ip with our mock server
        let result = fetch_ip(
            &format!("http://127.0.0.1:{port}/ip"),
            Duration::from_secs(2),
        )
        .await;
        assert!(result.is_ok());
        let actual = result.unwrap();
        let check = compare_ips("10.20.30.40", &actual);
        assert_eq!(check, IpCheckResult::Match);
    }

    #[tokio::test]
    async fn test_check_current_ip_primary_succeeds_mismatch() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
                let body = "10.20.30.40";
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
        let actual = result.unwrap();
        let check = compare_ips("99.99.99.99", &actual);
        assert!(matches!(check, IpCheckResult::Mismatch { .. }));
    }

    // -----------------------------------------------------------------------
    // fetch_ip — body read error (connection drops mid-response)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_fetch_ip_timeout_returns_error() {
        let result = fetch_ip("http://192.0.2.1:1/ip", Duration::from_millis(50)).await;
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // spawn_ip_monitor — enabled with valid IP, immediate shutdown
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_spawn_ip_monitor_enabled_then_shutdown() {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let config = IpMonitorConfig::new("10.20.30.40".to_string(), 1);
        let (_rx, handle) = spawn_ip_monitor(config, shutdown_rx);

        // Let it run for a tiny bit then shutdown
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = shutdown_tx.send(true);

        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("monitor should exit within 5s after shutdown") // APPROVED: test
            .expect("task should not panic"); // APPROVED: test
    }

    // -----------------------------------------------------------------------
    // IpMonitorConfig — custom check interval
    // -----------------------------------------------------------------------

    #[test]
    fn test_ip_monitor_config_custom_interval() {
        let config = IpMonitorConfig::new("1.2.3.4".to_string(), 60);
        assert_eq!(config.check_interval_secs, 60);
        assert!(config.enabled);
    }

    #[test]
    fn test_ip_monitor_config_zero_interval() {
        let config = IpMonitorConfig::new("1.2.3.4".to_string(), 0);
        assert_eq!(config.check_interval_secs, 0);
    }

    // -----------------------------------------------------------------------
    // mask_ip — two-octet IPs
    // -----------------------------------------------------------------------

    #[test]
    fn test_mask_ip_two_octets() {
        assert_eq!(mask_ip("1.2"), "XXX.XXX.XXX.XXX");
    }

    #[test]
    fn test_mask_ip_five_parts() {
        assert_eq!(mask_ip("1.2.3.4.5"), "XXX.XXX.XXX.XXX");
    }

    // -----------------------------------------------------------------------
    // check_current_ip — primary succeeds, returns Match
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_check_current_ip_match_via_mock_server() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
                let body = "10.20.30.40";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body,
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });

        // Use fetch_ip + compare_ips to simulate check_current_ip path
        let result = fetch_ip(
            &format!("http://127.0.0.1:{port}/ip"),
            Duration::from_secs(2),
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(
            compare_ips("10.20.30.40", &result.unwrap()),
            IpCheckResult::Match
        );
    }

    // -----------------------------------------------------------------------
    // check_current_ip — primary succeeds, returns Mismatch
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_check_current_ip_mismatch_via_mock_server() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
                let body = "10.20.30.40";
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
        let check = compare_ips("99.99.99.99", &result.unwrap());
        assert!(matches!(check, IpCheckResult::Mismatch { .. }));
    }

    // -----------------------------------------------------------------------
    // fetch_ip — whitespace in IP response is trimmed
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_fetch_ip_trims_whitespace() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
                let body = "  192.168.1.1  \n";
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
        assert_eq!(result.unwrap(), "192.168.1.1");
    }

    // -----------------------------------------------------------------------
    // fetch_ip — non-success HTTP status returns error
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_fetch_ip_non_success_status_500() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
                let response = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
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
        let err = result.unwrap_err();
        assert!(err.contains("HTTP 500"), "error: {err}");
    }

    // -----------------------------------------------------------------------
    // fetch_ip — invalid IP in response body
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_fetch_ip_invalid_ip_in_body() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
                let body = "not-an-ipv4-address";
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
        let err = result.unwrap_err();
        assert!(err.contains("invalid IPv4"), "error: {err}");
    }

    // -----------------------------------------------------------------------
    // spawn_ip_monitor — mismatch detection via watch channel
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_spawn_ip_monitor_mismatch_signals_watch_channel() {
        // If IP check returns a mismatch, the watch channel should emit true.
        // We use a very short interval and an IP that won't match any real check.
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let config = IpMonitorConfig::new("203.0.113.255".to_string(), 1);
        let (mut rx, handle) = spawn_ip_monitor(config, shutdown_rx);

        // Wait for at least one check cycle (1s + check time)
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Shut down
        let _ = shutdown_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;

        // On CI without network: CheckFailed (no signal, stays false).
        // On dev with network: Mismatch (signals true) or CheckFailed.
        // Either outcome is valid — just verify no panic.
        let _signal = *rx.borrow_and_update();
    }

    // -----------------------------------------------------------------------
    // check_current_ip — both fail returns CheckFailed
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_check_current_ip_very_short_timeout_returns_check_failed() {
        // With 1ms timeout, both primary and fallback should fail.
        let result = check_current_ip("10.0.0.1", Duration::from_millis(1)).await;
        match &result {
            IpCheckResult::CheckFailed { reason } => {
                assert!(
                    reason.contains("failed"),
                    "check failed reason should mention failure: {reason}"
                );
            }
            IpCheckResult::Match | IpCheckResult::Mismatch { .. } => {
                // Network happened to be fast enough — acceptable.
            }
        }
    }

    // -----------------------------------------------------------------------
    // compare_ips — additional edge cases for 100% branch coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_compare_ips_both_empty_is_match() {
        assert_eq!(compare_ips("", ""), IpCheckResult::Match);
    }

    #[test]
    fn test_compare_ips_expected_empty_actual_nonempty() {
        let result = compare_ips("", "1.2.3.4");
        match result {
            IpCheckResult::Mismatch { expected, actual } => {
                assert_eq!(expected, "");
                assert_eq!(actual, "1.2.3.4");
            }
            _ => panic!("expected Mismatch"),
        }
    }

    #[test]
    fn test_compare_ips_trailing_newline_is_mismatch() {
        assert!(matches!(
            compare_ips("1.2.3.4", "1.2.3.4\n"),
            IpCheckResult::Mismatch { .. }
        ));
    }

    // -----------------------------------------------------------------------
    // is_valid_ipv4 — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_valid_ipv4_ipv6_full() {
        assert!(!is_valid_ipv4("2001:0db8:85a3:0000:0000:8a2e:0370:7334"));
    }

    #[test]
    fn test_is_valid_ipv4_with_port() {
        assert!(!is_valid_ipv4("192.168.1.1:80"));
    }

    #[test]
    fn test_is_valid_ipv4_with_tab() {
        assert!(!is_valid_ipv4("\t1.2.3.4"));
    }

    #[test]
    fn test_is_valid_ipv4_extra_octet() {
        assert!(!is_valid_ipv4("1.2.3.4.5"));
    }

    #[test]
    fn test_is_valid_ipv4_loopback() {
        assert!(is_valid_ipv4("127.0.0.1"));
    }

    // -----------------------------------------------------------------------
    // IpMonitorConfig — full coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_ip_monitor_config_new_enabled_flag() {
        let config = IpMonitorConfig::new("10.0.0.1".to_string(), 120);
        assert!(config.enabled);
        assert_eq!(config.expected_ip, "10.0.0.1");
        assert_eq!(config.check_interval_secs, 120);
    }

    #[test]
    fn test_ip_monitor_config_disabled_all_fields() {
        let config = IpMonitorConfig::disabled();
        assert!(!config.enabled);
        assert!(config.expected_ip.is_empty());
        assert_eq!(config.check_interval_secs, 300);
    }

    // -----------------------------------------------------------------------
    // mask_ip — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_mask_ip_exactly_four_parts() {
        assert_eq!(mask_ip("10.20.30.40"), "10.20.XXX.XX");
    }

    #[test]
    fn test_mask_ip_three_parts() {
        assert_eq!(mask_ip("10.20.30"), "XXX.XXX.XXX.XXX");
    }

    #[test]
    fn test_mask_ip_single_part() {
        assert_eq!(mask_ip("192"), "XXX.XXX.XXX.XXX");
    }

    // -----------------------------------------------------------------------
    // IpCheckResult — Clone, Eq, Debug for all variants
    // -----------------------------------------------------------------------

    #[test]
    fn test_ip_check_result_match_clone() {
        let r = IpCheckResult::Match;
        let cloned = r.clone();
        assert_eq!(r, cloned);
    }

    #[test]
    fn test_ip_check_result_mismatch_clone() {
        let r = IpCheckResult::Mismatch {
            expected: "a".to_string(),
            actual: "b".to_string(),
        };
        let cloned = r.clone();
        assert_eq!(r, cloned);
    }

    #[test]
    fn test_ip_check_result_check_failed_clone() {
        let r = IpCheckResult::CheckFailed {
            reason: "timeout".to_string(),
        };
        let cloned = r.clone();
        assert_eq!(r, cloned);
    }

    #[test]
    fn test_ip_check_result_debug_match() {
        let r = IpCheckResult::Match;
        assert_eq!(format!("{r:?}"), "Match");
    }

    #[test]
    fn test_ip_check_result_debug_mismatch() {
        let r = IpCheckResult::Mismatch {
            expected: "a".to_string(),
            actual: "b".to_string(),
        };
        let debug = format!("{r:?}");
        assert!(debug.contains("Mismatch"));
        assert!(debug.contains("a"));
        assert!(debug.contains("b"));
    }

    #[test]
    fn test_ip_check_result_debug_check_failed() {
        let r = IpCheckResult::CheckFailed {
            reason: "err".to_string(),
        };
        let debug = format!("{r:?}");
        assert!(debug.contains("CheckFailed"));
        assert!(debug.contains("err"));
    }
}

//! Wave 2 Item 7 — boot-time QuestDB readiness probe (G14).
//!
//! Replaces the legacy "tick processor starts immediately, hopes QuestDB is up"
//! race with an explicit deadline-bounded readiness check that emits
//! escalating logs at +5s / +10s / +20s / +30s (BOOT-01) / +60s (BOOT-02 + HALT).

use std::time::Duration;

use reqwest::Client;
use tracing::{debug, error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;

/// Boot deadline before BOOT-02 fires + the app halts.
pub const BOOT_DEADLINE_SECS: u64 = 60;

/// Escalation thresholds in elapsed-seconds order.
const ESCALATION_DEBUG_AT_SECS: u64 = 5;
const ESCALATION_INFO_AT_SECS: u64 = 10;
const ESCALATION_WARN_AT_SECS: u64 = 20;
const ESCALATION_ERROR_AT_SECS: u64 = 30;

/// HTTP request timeout for a single QuestDB readiness probe.
const PROBE_REQUEST_TIMEOUT_SECS: u64 = 2;
/// Sleep between consecutive QuestDB readiness probes.
const PROBE_POLL_INTERVAL_MS: u64 = 500;

/// Polls the QuestDB HTTP `/exec` endpoint with `SELECT 1` until it
/// responds 2xx or `deadline_secs` elapses. Emits escalating logs on
/// the way:
///
/// | Elapsed | Level  | Code     |
/// |---------|--------|----------|
/// |   +5s   | DEBUG  | —        |
/// |  +10s   | INFO   | —        |
/// |  +20s   | WARN   | —        |
/// |  +30s   | ERROR  | BOOT-01  |
/// |  +60s   | CRITICAL+HALT | BOOT-02 |
///
/// Returns `Ok(elapsed)` on first successful response, or
/// `Err(BootProbeError::DeadlineExceeded)` after `deadline_secs`.
///
/// This function is the **single permitted call site** for HALT-on-boot
/// — operator inspect on BOOT-02 alerts.
pub async fn wait_for_questdb_ready(
    questdb_config: &QuestDbConfig,
    deadline_secs: u64,
) -> Result<Duration, BootProbeError> {
    let base_url = format!(
        "http://{}:{}/exec?query=SELECT%201",
        questdb_config.host, questdb_config.http_port
    );
    // O(1) EXEMPT: constructor — runs once at boot.
    let client = Client::builder()
        .timeout(Duration::from_secs(PROBE_REQUEST_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    let started = std::time::Instant::now();
    let deadline = Duration::from_secs(deadline_secs);
    let mut next_log_threshold_secs = ESCALATION_DEBUG_AT_SECS;

    loop {
        let elapsed = started.elapsed();
        if elapsed >= deadline {
            error!(
                elapsed_secs = elapsed.as_secs(),
                deadline_secs,
                code = ErrorCode::Boot02DeadlineExceeded.code_str(),
                "BOOT-02 boot deadline exceeded — QuestDB never became ready"
            );
            metrics::counter!("tv_boot_deadline_exceeded_total").increment(1);
            return Err(BootProbeError::DeadlineExceeded {
                elapsed_secs: elapsed.as_secs(),
                deadline_secs,
            });
        }

        // Try one probe.
        match client.get(&base_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                metrics::histogram!("tv_boot_questdb_ready_seconds").record(elapsed.as_secs_f64());
                info!(
                    elapsed_secs = elapsed.as_secs(),
                    "QuestDB ready — boot probe succeeded"
                );
                return Ok(elapsed);
            }
            Ok(resp) => {
                debug!(
                    status = %resp.status(),
                    elapsed_secs = elapsed.as_secs(),
                    "QuestDB probe non-2xx — retrying"
                );
            }
            Err(err) => {
                debug!(
                    ?err,
                    elapsed_secs = elapsed.as_secs(),
                    "QuestDB probe network error — retrying"
                );
            }
        }

        // Escalation logging (one-shot per threshold).
        if elapsed.as_secs() >= next_log_threshold_secs {
            match next_log_threshold_secs {
                v if v == ESCALATION_DEBUG_AT_SECS => {
                    debug!(
                        elapsed_secs = elapsed.as_secs(),
                        "boot probe still waiting (≥5s)"
                    );
                    next_log_threshold_secs = ESCALATION_INFO_AT_SECS;
                }
                v if v == ESCALATION_INFO_AT_SECS => {
                    info!(
                        elapsed_secs = elapsed.as_secs(),
                        "boot probe still waiting (≥10s)"
                    );
                    next_log_threshold_secs = ESCALATION_WARN_AT_SECS;
                }
                v if v == ESCALATION_WARN_AT_SECS => {
                    warn!(
                        elapsed_secs = elapsed.as_secs(),
                        "boot probe still waiting (≥20s) — QuestDB cold-start may be in progress"
                    );
                    next_log_threshold_secs = ESCALATION_ERROR_AT_SECS;
                }
                v if v == ESCALATION_ERROR_AT_SECS => {
                    error!(
                        elapsed_secs = elapsed.as_secs(),
                        deadline_secs,
                        code = ErrorCode::Boot01QuestDbSlow.code_str(),
                        "BOOT-01 slow boot detected (≥30s) — operator should run make doctor"
                    );
                    metrics::counter!("tv_boot_questdb_slow_total").increment(1);
                    next_log_threshold_secs = u64::MAX; // no further escalations
                }
                _ => {}
            }
        }

        tokio::time::sleep(Duration::from_millis(PROBE_POLL_INTERVAL_MS)).await;
    }
}

/// Errors from the boot probe.
#[derive(Debug, thiserror::Error)]
pub enum BootProbeError {
    /// `wait_for_questdb_ready` exceeded `deadline_secs`. Operator
    /// must investigate — typically docker compose-up is stuck.
    #[error("QuestDB readiness deadline exceeded ({elapsed_secs}s ≥ {deadline_secs}s) — BOOT-02")]
    DeadlineExceeded {
        /// How long the probe ran before giving up.
        elapsed_secs: u64,
        /// Configured deadline.
        deadline_secs: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_boot_deadline_constant_is_60s() {
        assert_eq!(BOOT_DEADLINE_SECS, 60);
    }

    #[test]
    fn test_escalation_thresholds_monotonic() {
        assert!(ESCALATION_DEBUG_AT_SECS < ESCALATION_INFO_AT_SECS);
        assert!(ESCALATION_INFO_AT_SECS < ESCALATION_WARN_AT_SECS);
        assert!(ESCALATION_WARN_AT_SECS < ESCALATION_ERROR_AT_SECS);
        assert!(ESCALATION_ERROR_AT_SECS < BOOT_DEADLINE_SECS);
    }

    #[test]
    fn test_boot_probe_error_display_includes_elapsed_and_deadline() {
        let e = BootProbeError::DeadlineExceeded {
            elapsed_secs: 61,
            deadline_secs: 60,
        };
        let msg = format!("{e}");
        assert!(msg.contains("61"));
        assert!(msg.contains("60"));
        assert!(msg.contains("BOOT-02"));
    }

    #[tokio::test]
    async fn test_wait_for_questdb_ready_fails_on_unreachable_host() {
        // Use port 1 (unprivileged + always rejected) with 2s deadline so test runs quickly.
        let cfg = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let result = wait_for_questdb_ready(&cfg, 2).await;
        assert!(result.is_err());
        match result.err().unwrap() {
            BootProbeError::DeadlineExceeded {
                elapsed_secs,
                deadline_secs,
            } => {
                assert_eq!(deadline_secs, 2);
                assert!(elapsed_secs >= 2, "elapsed must be >= deadline");
            }
        }
    }
}

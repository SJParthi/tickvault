//! Typed notification events for structured Telegram messages.
//!
//! Every system event that should produce a Telegram alert is represented
//! here. Callers pass events to `NotificationService::notify` — message
//! formatting lives in this module, not at callsites.
//!
//! Defense-in-depth: `to_message()` redacts URL query parameters from
//! `AuthenticationFailed` and `TokenRenewalFailed` reasons to prevent
//! credential leaks in Telegram even if callers pass unsanitized strings.

use dhan_live_trader_common::sanitize::redact_url_params;

/// Masks the last two octets of an IPv4 address for safe display in
/// Telegram messages. Prevents full IP exposure while still confirming
/// the network prefix for verification.
///
/// Example: `"59.92.114.17"` → `"59.92.XXX.XX"`
fn mask_ip_for_notification(ip: &str) -> String {
    let parts: Vec<&str> = ip.split('.').collect();
    if parts.len() == 4 {
        format!("{}.{}.XXX.XX", parts[0], parts[1])
    } else {
        "XXX.XXX.XXX.XXX".to_string()
    }
}

/// Alert severity level — determines which notification channels fire.
///
/// `Critical` and `High` → Telegram + SNS SMS.
/// `Medium`, `Low`, `Info` → Telegram only.
///
/// Ordered for comparison: `Info < Low < Medium < High < Critical`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Lifecycle events — startup complete, shutdown complete.
    Info,
    /// Normal operations — auth success, token renewed, WS connected.
    Low,
    /// Notable state changes — WS reconnected, shutdown initiated.
    Medium,
    /// Degraded state — WS disconnected, custom alerts.
    High,
    /// System cannot trade — auth failure, token renewal exhausted.
    Critical,
}

/// All events that produce a Telegram alert.
///
/// Adding a new event: add a variant here, add its message arm in
/// `NotificationEvent::to_message`, add a callsite in `main.rs`.
#[derive(Debug, Clone)]
pub enum NotificationEvent {
    /// Application boot completed successfully.
    StartupComplete {
        /// "LIVE" or "OFFLINE".
        mode: &'static str,
    },

    /// Dhan authentication token acquired at boot.
    AuthenticationSuccess,

    /// Dhan authentication failed at boot — system started in offline mode.
    AuthenticationFailed { reason: String },

    /// JWT token renewed successfully by background task.
    TokenRenewed,

    /// Token renewal failed — background task will retry.
    TokenRenewalFailed { attempts: u32, reason: String },

    /// WebSocket connection established.
    WebSocketConnected { connection_index: usize },

    /// WebSocket disconnected (unexpected, will reconnect).
    WebSocketDisconnected {
        connection_index: usize,
        reason: String,
    },

    /// WebSocket reconnected after disconnection.
    WebSocketReconnected { connection_index: usize },

    /// Graceful shutdown initiated.
    ShutdownInitiated,

    /// Application stopped.
    ShutdownComplete,

    /// Instrument build succeeded (first build of the day).
    InstrumentBuildSuccess {
        /// CSV source: "primary", "fallback", or "cache".
        source: String,
        /// Total derivative contracts built.
        derivative_count: usize,
        /// Total F&O underlyings built.
        underlying_count: usize,
    },

    /// Instrument build failed — includes manual trigger URL for retry.
    InstrumentBuildFailed {
        /// Error description.
        reason: String,
        /// URL for manual one-shot retry.
        manual_trigger_url: String,
    },

    /// Historical candle fetch completed successfully (all instruments OK).
    HistoricalFetchComplete {
        /// Number of instruments fetched successfully.
        instruments_fetched: usize,
        /// Number of instruments skipped (derivatives, already fetched).
        instruments_skipped: usize,
        /// Total candles ingested across all timeframes.
        total_candles: usize,
        /// Number of QuestDB write failures (candles lost during persist).
        persist_failures: usize,
    },

    /// Historical candle fetch completed with failures.
    HistoricalFetchFailed {
        /// Number of instruments that succeeded.
        instruments_fetched: usize,
        /// Number of instruments that failed.
        instruments_failed: usize,
        /// Total candles ingested.
        total_candles: usize,
        /// Number of QuestDB write failures (candles lost during persist).
        persist_failures: usize,
        /// Symbol names of failed instruments (up to 50).
        failed_instruments: Vec<String>,
        /// Breakdown of failure reasons: "token_expired", "network_or_api", "persist".
        failure_reasons: std::collections::HashMap<String, usize>,
    },

    /// Candle cross-verification passed — all timeframes have expected coverage.
    CandleVerificationPassed {
        /// Instruments checked across all timeframes.
        instruments_checked: usize,
        /// Total candles in QuestDB.
        total_candles: usize,
        /// Per-timeframe breakdown (pre-formatted lines).
        timeframe_details: String,
        /// OHLC violations found (high < low).
        ohlc_violations: usize,
        /// Data violations (non-positive prices).
        data_violations: usize,
        /// Timestamp violations (outside market hours).
        timestamp_violations: usize,
        /// Weekend violations (candles on Saturday/Sunday).
        weekend_violations: usize,
    },

    /// Candle cross-verification found gaps in stored data.
    CandleVerificationFailed {
        /// Instruments checked.
        instruments_checked: usize,
        /// Instruments with gaps.
        instruments_with_gaps: usize,
        /// Per-timeframe breakdown (pre-formatted lines).
        timeframe_details: String,
        /// OHLC violations found (high < low).
        ohlc_violations: usize,
        /// Data violations (non-positive prices).
        data_violations: usize,
        /// Timestamp violations (outside market hours).
        timestamp_violations: usize,
        /// Pre-formatted OHLC violation detail lines for Telegram.
        ohlc_details: Vec<String>,
        /// Pre-formatted data violation detail lines for Telegram.
        data_details: Vec<String>,
        /// Pre-formatted timestamp violation detail lines for Telegram.
        timestamp_details: Vec<String>,
        /// Weekend violations (candles on Saturday/Sunday).
        weekend_violations: usize,
        /// Pre-formatted weekend violation detail lines for Telegram.
        weekend_details: Vec<String>,
    },

    /// Historical vs Live candle cross-match passed — all OHLCV values match.
    CandleCrossMatchPassed {
        /// Number of timeframes compared.
        timeframes_checked: usize,
        /// Total candles compared.
        candles_compared: usize,
    },

    /// Historical vs Live candle cross-match found mismatches.
    CandleCrossMatchFailed {
        /// Total candles compared.
        candles_compared: usize,
        /// Total mismatches found.
        mismatches: usize,
        /// Historical candle exists but no live data (WebSocket missed ticks).
        missing_live: usize,
        /// Pre-formatted mismatch detail lines for Telegram.
        mismatch_details: Vec<String>,
    },

    /// Public IP verification failed — static IP mismatch or detection failure.
    IpVerificationFailed {
        /// Human-readable reason for the failure.
        reason: String,
    },

    /// Public IP verification succeeded at boot.
    IpVerificationSuccess {
        /// The verified public IP address.
        verified_ip: String,
    },

    /// Boot health check completed — infrastructure services verified.
    BootHealthCheck {
        /// Number of services that passed health check.
        services_healthy: usize,
        /// Total services checked.
        services_total: usize,
    },

    /// Boot deadline missed — system did not complete startup within allowed time.
    BootDeadlineMissed {
        /// Deadline in seconds that was exceeded.
        deadline_secs: u64,
        /// Step that was running when deadline hit.
        step: String,
    },

    /// Order rejected by Dhan API or OMS validation.
    OrderRejected {
        /// Correlation ID of the rejected order.
        correlation_id: String,
        /// Reason for rejection.
        reason: String,
    },

    /// OMS circuit breaker opened — order API calls halted.
    CircuitBreakerOpened {
        /// Number of consecutive failures that triggered the open.
        consecutive_failures: u64,
    },

    /// OMS circuit breaker closed — order API calls resumed.
    CircuitBreakerClosed,

    /// OMS rate limit exhausted — order rejected due to SEBI limits.
    RateLimitExhausted {
        /// Which limit was hit (e.g., "per_second", "daily").
        limit_type: String,
    },

    /// Risk engine halted trading — daily loss breach or position limit.
    RiskHalt {
        /// Reason for the halt (e.g., "daily_loss_breach", "position_limit").
        reason: String,
    },

    /// WebSocket reconnection exhausted — all retry attempts failed.
    WebSocketReconnectionExhausted {
        /// Connection index that failed.
        connection_index: usize,
        /// Total reconnection attempts made.
        attempts: u64,
    },

    /// Token renewal deadline missed — renewal failed past safe window.
    TokenRenewalDeadlineMissed {
        /// IST hour when the deadline was crossed.
        deadline_hour_ist: u32,
    },

    /// Custom alert from any component.
    Custom { message: String },
}

impl NotificationEvent {
    /// Formats the event as a Telegram message.
    ///
    /// HTML parse_mode is used by the sender, so basic `<b>` tags are safe.
    /// Keep messages short — they appear as phone notifications.
    pub fn to_message(&self) -> String {
        match self {
            Self::StartupComplete { mode } => {
                format!(
                    "<b>dhan-live-trader started</b>\nMode: {mode}\n\n\
                     Grafana: http://localhost:3000\n\
                     Prometheus: http://localhost:9090\n\
                     Jaeger: http://localhost:16686\n\
                     QuestDB: http://localhost:9000"
                )
            }
            Self::AuthenticationSuccess => "<b>Auth OK</b> — Dhan JWT acquired".to_string(),
            Self::AuthenticationFailed { reason } => {
                format!(
                    "<b>Auth FAILED</b> — offline mode\n{}",
                    redact_url_params(reason)
                )
            }
            Self::TokenRenewed => "<b>Token renewed</b>".to_string(),
            Self::TokenRenewalFailed { attempts, reason } => {
                format!(
                    "<b>Token renewal FAILED</b> (attempt {attempts})\n{}",
                    redact_url_params(reason)
                )
            }
            Self::WebSocketConnected { connection_index } => {
                format!("<b>WebSocket #{connection_index} connected</b>")
            }
            Self::WebSocketDisconnected {
                connection_index,
                reason,
            } => {
                format!("<b>WebSocket #{connection_index} disconnected</b>\n{reason}")
            }
            Self::WebSocketReconnected { connection_index } => {
                format!("<b>WebSocket #{connection_index} reconnected</b>")
            }
            Self::InstrumentBuildSuccess {
                source,
                derivative_count,
                underlying_count,
            } => {
                format!(
                    "<b>Instruments OK</b>\nSource: {source}\nDerivatives: {derivative_count}\nUnderlyings: {underlying_count}"
                )
            }
            Self::InstrumentBuildFailed {
                reason,
                manual_trigger_url,
            } => {
                format!("<b>Instruments FAILED</b>\n{reason}\n\nRetry: {manual_trigger_url}")
            }
            Self::HistoricalFetchComplete {
                instruments_fetched,
                instruments_skipped,
                total_candles,
                persist_failures,
            } => {
                let mut msg = format!(
                    "<b>Historical candles OK</b>\nFetched: {instruments_fetched}\nSkipped: {instruments_skipped}\nCandles: {total_candles}\nTimeframes: 1m, 5m, 15m, 60m, 1d"
                );
                if *persist_failures > 0 {
                    msg.push_str(&format!("\nPersist errors: {persist_failures}"));
                }
                msg
            }
            Self::HistoricalFetchFailed {
                instruments_fetched,
                instruments_failed,
                total_candles,
                persist_failures,
                failed_instruments,
                failure_reasons,
            } => {
                let mut msg = format!(
                    "<b>Historical candle fetch — partial failure</b>\nFetched: {instruments_fetched}\nFailed: {instruments_failed}\nCandles: {total_candles}"
                );
                if *persist_failures > 0 {
                    msg.push_str(&format!("\nPersist errors: {persist_failures}"));
                }
                if !failure_reasons.is_empty() {
                    msg.push_str("\n\n<b>Failure breakdown:</b>");
                    for (reason, count) in failure_reasons {
                        msg.push_str(&format!("\n\u{2022} {reason}: {count}"));
                    }
                }
                if !failed_instruments.is_empty() {
                    msg.push_str("\n\n<b>Failed instruments:</b>");
                    let show_count = failed_instruments.len().min(10);
                    for name in &failed_instruments[..show_count] {
                        msg.push_str(&format!("\n\u{2022} {name}"));
                    }
                    if failed_instruments.len() > 10 {
                        let remaining = failed_instruments.len().saturating_sub(10);
                        msg.push_str(&format!("\n... +{remaining} more"));
                    }
                }
                msg
            }
            Self::CandleVerificationPassed {
                instruments_checked,
                total_candles,
                timeframe_details,
                ohlc_violations,
                data_violations,
                timestamp_violations,
                weekend_violations,
            } => {
                let mut msg = format!(
                    "<b>Candle verification OK</b>\nInstruments: {instruments_checked}\nTotal candles: {total_candles}"
                );
                if !timeframe_details.is_empty() {
                    msg.push_str("\n\n<b>Timeframes:</b>\n");
                    msg.push_str(timeframe_details);
                }
                if *ohlc_violations == 0
                    && *data_violations == 0
                    && *timestamp_violations == 0
                    && *weekend_violations == 0
                {
                    msg.push_str("\n\nChecks: OHLC \u{2713} | Data \u{2713} | Timestamps \u{2713} | Weekends \u{2713}");
                } else {
                    if *ohlc_violations > 0 {
                        msg.push_str(&format!("\nOHLC violations: {ohlc_violations}"));
                    }
                    if *data_violations > 0 {
                        msg.push_str(&format!(
                            "\nData violations: {data_violations} (non-blocking)"
                        ));
                    }
                    if *timestamp_violations > 0 {
                        msg.push_str(&format!(
                            "\nTimestamp violations: {timestamp_violations} (non-blocking)"
                        ));
                    }
                    if *weekend_violations > 0 {
                        msg.push_str(&format!(
                            "\nWeekend violations: {weekend_violations} (CRITICAL)"
                        ));
                    }
                }
                msg
            }
            Self::CandleVerificationFailed {
                instruments_checked,
                instruments_with_gaps,
                timeframe_details,
                ohlc_violations,
                data_violations,
                timestamp_violations,
                ohlc_details,
                data_details,
                timestamp_details,
                weekend_violations,
                weekend_details,
            } => {
                let mut msg = if *instruments_checked == 0 {
                    "<b>Candle verification FAILED</b>\nChecked: 0\n\nNo instrument data found \u{2014} fetch may have completely failed".to_string()
                } else {
                    format!(
                        "<b>Candle verification FAILED</b>\nChecked: {instruments_checked} | Gaps: {instruments_with_gaps}"
                    )
                };

                // OHLC violations with details
                if *ohlc_violations > 0 {
                    msg.push_str(&format!("\n\n<b>OHLC violations ({ohlc_violations}):</b>"));
                    append_detail_lines(&mut msg, ohlc_details, *ohlc_violations);
                }

                // Data violations with details
                if *data_violations > 0 {
                    msg.push_str(&format!("\n\n<b>Data violations ({data_violations}):</b>"));
                    append_detail_lines(&mut msg, data_details, *data_violations);
                }

                // Timestamp violations with details
                if *timestamp_violations > 0 {
                    msg.push_str(&format!(
                        "\n\n<b>Timestamp violations ({timestamp_violations}):</b>"
                    ));
                    append_detail_lines(&mut msg, timestamp_details, *timestamp_violations);
                }

                // Weekend violations with details (CRITICAL — NSE closed on Sat/Sun)
                if *weekend_violations > 0 {
                    msg.push_str(&format!(
                        "\n\n<b>WEEKEND violations ({weekend_violations}) — CRITICAL:</b>"
                    ));
                    append_detail_lines(&mut msg, weekend_details, *weekend_violations);
                }

                if *instruments_checked > 0 && !timeframe_details.is_empty() {
                    msg.push_str("\n\n<b>Timeframes:</b>\n");
                    msg.push_str(timeframe_details);
                }
                msg
            }
            Self::CandleCrossMatchPassed {
                timeframes_checked,
                candles_compared,
            } => {
                format!(
                    "<b>Historical vs Live cross-match OK</b>\nTimeframes: {timeframes_checked} | Candles compared: {candles_compared}\nAll OHLCV values match (epsilon tolerance, volume ±10%, OI ±10%)"
                )
            }
            Self::CandleCrossMatchFailed {
                candles_compared,
                mismatches,
                missing_live,
                mismatch_details,
            } => {
                let mut msg = format!(
                    "<b>Historical vs Live cross-match FAILED</b>\nCompared: {candles_compared} | Mismatches: {mismatches}"
                );
                if *missing_live > 0 {
                    msg.push_str(&format!("\nMissing live: {missing_live}"));
                }
                if !mismatch_details.is_empty() {
                    msg.push_str("\n\n<b>Mismatches:</b>");
                    let show_count = mismatch_details.len().min(10);
                    for line in &mismatch_details[..show_count] {
                        msg.push_str(&format!("\n{line}"));
                    }
                    if mismatch_details.len() > 10 {
                        let remaining = mismatch_details.len().saturating_sub(10);
                        msg.push_str(&format!("\n... +{remaining} more"));
                    }
                }
                msg
            }
            Self::IpVerificationFailed { reason } => {
                format!(
                    "<b>IP VERIFICATION FAILED</b>\n{reason}\n\nBoot blocked — no Dhan API calls will be made."
                )
            }
            Self::IpVerificationSuccess { verified_ip } => {
                // SECURITY: Mask the last two octets to prevent IP exposure
                // in Telegram messages. Show only network prefix for confirmation.
                let masked = mask_ip_for_notification(verified_ip);
                format!("<b>IP verified</b> — {masked}")
            }
            Self::BootHealthCheck {
                services_healthy,
                services_total,
            } => {
                format!("<b>Boot health check</b>\nHealthy: {services_healthy}/{services_total}")
            }
            Self::BootDeadlineMissed {
                deadline_secs,
                step,
            } => {
                format!(
                    "<b>BOOT DEADLINE MISSED</b>\nDeadline: {deadline_secs}s\nBlocked at: {step}"
                )
            }
            Self::ShutdownInitiated => "<b>Shutdown initiated</b>".to_string(),
            Self::ShutdownComplete => "<b>dhan-live-trader stopped</b>".to_string(),
            Self::OrderRejected {
                correlation_id,
                reason,
            } => {
                format!("<b>Order REJECTED</b>\nCorrelation: {correlation_id}\n{reason}")
            }
            Self::CircuitBreakerOpened {
                consecutive_failures,
            } => {
                format!(
                    "<b>Circuit breaker OPENED</b>\nConsecutive failures: {consecutive_failures}\nOrder API calls halted"
                )
            }
            Self::CircuitBreakerClosed => {
                "<b>Circuit breaker CLOSED</b>\nOrder API calls resumed".to_string()
            }
            Self::RateLimitExhausted { limit_type } => {
                format!("<b>Rate limit EXHAUSTED</b>\nLimit: {limit_type}")
            }
            Self::RiskHalt { reason } => {
                format!("<b>RISK HALT</b>\nTrading stopped: {reason}")
            }
            Self::WebSocketReconnectionExhausted {
                connection_index,
                attempts,
            } => {
                format!(
                    "<b>WebSocket #{connection_index} RECONNECTION EXHAUSTED</b>\nAttempts: {attempts}\nNo market data"
                )
            }
            Self::TokenRenewalDeadlineMissed { deadline_hour_ist } => {
                format!(
                    "<b>TOKEN RENEWAL DEADLINE MISSED</b>\nPast {deadline_hour_ist}:00 IST — token not renewed"
                )
            }
            Self::Custom { message } => message.clone(),
        }
    }

    /// Returns the severity level for this event.
    ///
    /// Severity drives channel selection in `NotificationService::notify`:
    /// - `Critical` / `High` → Telegram + SNS SMS
    /// - `Medium` / `Low` / `Info` → Telegram only
    pub fn severity(&self) -> Severity {
        match self {
            Self::IpVerificationFailed { .. } => Severity::Critical,
            Self::BootDeadlineMissed { .. } => Severity::Critical,
            Self::AuthenticationFailed { .. } => Severity::Critical,
            Self::TokenRenewalFailed { .. } => Severity::Critical,
            Self::InstrumentBuildFailed { .. } => Severity::High,
            Self::WebSocketDisconnected { .. } => Severity::High,
            Self::HistoricalFetchFailed { .. } => Severity::High,
            Self::CandleVerificationFailed { .. } => Severity::High,
            Self::CandleCrossMatchFailed { .. } => Severity::High,
            Self::HistoricalFetchComplete { .. } => Severity::Low,
            Self::CandleVerificationPassed { .. } => Severity::Low,
            Self::CandleCrossMatchPassed { .. } => Severity::Low,
            Self::Custom { .. } => Severity::High,
            Self::RiskHalt { .. } => Severity::Critical,
            Self::WebSocketReconnectionExhausted { .. } => Severity::Critical,
            Self::TokenRenewalDeadlineMissed { .. } => Severity::Critical,
            Self::CircuitBreakerOpened { .. } => Severity::High,
            Self::OrderRejected { .. } => Severity::High,
            Self::RateLimitExhausted { .. } => Severity::High,
            Self::WebSocketReconnected { .. } => Severity::Medium,
            Self::ShutdownInitiated => Severity::Medium,
            Self::CircuitBreakerClosed => Severity::Medium,
            Self::WebSocketConnected { .. } => Severity::Low,
            Self::TokenRenewed => Severity::Low,
            Self::IpVerificationSuccess { .. } => Severity::Low,
            Self::AuthenticationSuccess => Severity::Low,
            Self::InstrumentBuildSuccess { .. } => Severity::Low,
            Self::BootHealthCheck { .. } => Severity::Low,
            Self::StartupComplete { .. } => Severity::Info,
            Self::ShutdownComplete => Severity::Info,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Appends violation detail lines to a message, with "+N more" truncation.
fn append_detail_lines(msg: &mut String, details: &[String], total_count: usize) {
    let show_count = details.len().min(10);
    for line in &details[..show_count] {
        msg.push_str(&format!("\n{line}"));
    }
    if total_count > show_count {
        let remaining = total_count.saturating_sub(show_count);
        msg.push_str(&format!("\n... +{remaining} more"));
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_startup_complete_live_message() {
        let event = NotificationEvent::StartupComplete { mode: "LIVE" };
        let msg = event.to_message();
        assert!(msg.contains("LIVE"));
        assert!(msg.contains("started"));
        assert!(msg.contains("Grafana: http://localhost:3000"));
        assert!(msg.contains("Prometheus: http://localhost:9090"));
        assert!(msg.contains("Jaeger: http://localhost:16686"));
        assert!(msg.contains("QuestDB: http://localhost:9000"));
    }

    #[test]
    fn test_startup_complete_offline_message() {
        let event = NotificationEvent::StartupComplete { mode: "OFFLINE" };
        let msg = event.to_message();
        assert!(msg.contains("OFFLINE"));
        assert!(msg.contains("Grafana"));
    }

    #[test]
    fn test_auth_success_message() {
        let event = NotificationEvent::AuthenticationSuccess;
        let msg = event.to_message();
        assert!(msg.contains("Auth OK"));
    }

    #[test]
    fn test_auth_failed_includes_reason() {
        let event = NotificationEvent::AuthenticationFailed {
            reason: "HTTP 401 Unauthorized".to_string(),
        };
        let msg = event.to_message();
        assert!(msg.contains("HTTP 401 Unauthorized"));
        assert!(msg.contains("FAILED"));
    }

    #[test]
    fn test_auth_failed_redacts_credentials_in_url() {
        let event = NotificationEvent::AuthenticationFailed {
            reason: "generateAccessToken request failed: error sending request for url (https://auth.dhan.co/app/generateAccessToken?dhanClientId=0000000000&pin=000000&totp=000000)".to_string(),
        };
        let msg = event.to_message();
        assert!(!msg.contains("0000000000"), "client ID leaked: {msg}");
        assert!(!msg.contains("pin=000000"), "PIN leaked: {msg}");
        assert!(!msg.contains("totp=000000"), "TOTP leaked: {msg}");
        assert!(msg.contains("FAILED"));
        assert!(msg.contains("[REDACTED]"));
    }

    #[test]
    fn test_token_renewal_failed_redacts_credentials() {
        let event = NotificationEvent::TokenRenewalFailed {
            attempts: 3,
            reason: "request for url (https://auth.dhan.co/app/generateAccessToken?pin=123456&totp=654321)".to_string(),
        };
        let msg = event.to_message();
        assert!(!msg.contains("123456"), "PIN leaked: {msg}");
        assert!(!msg.contains("654321"), "TOTP leaked: {msg}");
    }

    #[test]
    fn test_token_renewed_message() {
        let event = NotificationEvent::TokenRenewed;
        let msg = event.to_message();
        assert!(msg.contains("renewed"));
    }

    #[test]
    fn test_token_renewal_failed_includes_attempts_and_reason() {
        let event = NotificationEvent::TokenRenewalFailed {
            attempts: 3,
            reason: "timeout".to_string(),
        };
        let msg = event.to_message();
        assert!(msg.contains("3"));
        assert!(msg.contains("timeout"));
    }

    #[test]
    fn test_websocket_connected_includes_index() {
        let event = NotificationEvent::WebSocketConnected {
            connection_index: 2,
        };
        let msg = event.to_message();
        assert!(msg.contains("2"));
        assert!(msg.contains("connected"));
    }

    #[test]
    fn test_websocket_disconnected_includes_index_and_reason() {
        let event = NotificationEvent::WebSocketDisconnected {
            connection_index: 1,
            reason: "connection reset by peer".to_string(),
        };
        let msg = event.to_message();
        assert!(msg.contains("1"));
        assert!(msg.contains("connection reset by peer"));
    }

    #[test]
    fn test_websocket_reconnected_includes_index() {
        let event = NotificationEvent::WebSocketReconnected {
            connection_index: 0,
        };
        let msg = event.to_message();
        assert!(msg.contains("0"));
        assert!(msg.contains("reconnected"));
    }

    #[test]
    fn test_shutdown_initiated_message() {
        let event = NotificationEvent::ShutdownInitiated;
        let msg = event.to_message();
        assert!(msg.contains("Shutdown"));
    }

    #[test]
    fn test_shutdown_complete_message() {
        let event = NotificationEvent::ShutdownComplete;
        let msg = event.to_message();
        assert!(msg.contains("stopped"));
    }

    #[test]
    fn test_custom_message_passthrough() {
        let event = NotificationEvent::Custom {
            message: "custom alert payload".to_string(),
        };
        let msg = event.to_message();
        assert_eq!(msg, "custom alert payload");
    }

    #[test]
    fn test_instrument_build_success_message() {
        let event = NotificationEvent::InstrumentBuildSuccess {
            source: "primary".to_string(),
            derivative_count: 96948,
            underlying_count: 214,
        };
        let msg = event.to_message();
        assert!(msg.contains("Instruments OK"));
        assert!(msg.contains("primary"));
        assert!(msg.contains("96948"));
        assert!(msg.contains("214"));
    }

    #[test]
    fn test_instrument_build_failed_message() {
        let event = NotificationEvent::InstrumentBuildFailed {
            reason: "HTTP 503 Service Unavailable".to_string(),
            manual_trigger_url: "http://0.0.0.0:3001/api/instruments/rebuild".to_string(),
        };
        let msg = event.to_message();
        assert!(msg.contains("Instruments FAILED"));
        assert!(msg.contains("HTTP 503"));
        assert!(msg.contains("/api/instruments/rebuild"));
    }

    #[test]
    fn test_historical_fetch_failed_message() {
        let event = NotificationEvent::HistoricalFetchFailed {
            instruments_fetched: 200,
            instruments_failed: 9,
            total_candles: 180000,
            persist_failures: 0,
            failed_instruments: vec![],
            failure_reasons: std::collections::HashMap::new(),
        };
        let msg = event.to_message();
        assert!(msg.contains("partial failure"));
        assert!(msg.contains("200"));
        assert!(msg.contains("9"));
        assert!(msg.contains("180000"));
    }

    #[test]
    fn test_historical_fetch_failed_is_high() {
        let event = NotificationEvent::HistoricalFetchFailed {
            instruments_fetched: 200,
            instruments_failed: 9,
            total_candles: 180000,
            persist_failures: 0,
            failed_instruments: vec![],
            failure_reasons: std::collections::HashMap::new(),
        };
        assert_eq!(event.severity(), Severity::High);
    }

    #[test]
    fn test_historical_fetch_failed_shows_instrument_names() {
        let event = NotificationEvent::HistoricalFetchFailed {
            instruments_fetched: 229,
            instruments_failed: 3,
            total_candles: 172125,
            persist_failures: 0,
            failed_instruments: vec![
                "RELIANCE (NSE_EQ)".to_string(),
                "NIFTY50 (IDX_I)".to_string(),
                "BANKNIFTY (IDX_I)".to_string(),
            ],
            failure_reasons: std::collections::HashMap::new(),
        };
        let msg = event.to_message();
        assert!(msg.contains("Failed instruments:"));
        assert!(msg.contains("RELIANCE (NSE_EQ)"));
        assert!(msg.contains("NIFTY50 (IDX_I)"));
        assert!(msg.contains("BANKNIFTY (IDX_I)"));
    }

    #[test]
    fn test_historical_fetch_failed_truncates_long_list() {
        let names: Vec<String> = (0..15).map(|i| format!("INST_{i} (NSE_EQ)")).collect();
        let event = NotificationEvent::HistoricalFetchFailed {
            instruments_fetched: 217,
            instruments_failed: 15,
            total_candles: 160000,
            persist_failures: 0,
            failed_instruments: names,
            failure_reasons: std::collections::HashMap::new(),
        };
        let msg = event.to_message();
        assert!(msg.contains("INST_0 (NSE_EQ)"));
        assert!(msg.contains("INST_9 (NSE_EQ)"));
        assert!(
            !msg.contains("INST_10 (NSE_EQ)"),
            "11th item should be truncated"
        );
        assert!(msg.contains("+5 more"));
    }

    #[test]
    fn test_historical_fetch_failed_shows_persist_errors() {
        let event = NotificationEvent::HistoricalFetchFailed {
            instruments_fetched: 200,
            instruments_failed: 0,
            total_candles: 180000,
            persist_failures: 42,
            failed_instruments: vec![],
            failure_reasons: std::collections::HashMap::new(),
        };
        let msg = event.to_message();
        assert!(msg.contains("Persist errors: 42"));
    }

    #[test]
    fn test_historical_fetch_failed_shows_failure_reasons() {
        let mut reasons = std::collections::HashMap::new();
        reasons.insert("token_expired".to_string(), 5);
        reasons.insert("network_or_api".to_string(), 3);
        let event = NotificationEvent::HistoricalFetchFailed {
            instruments_fetched: 224,
            instruments_failed: 8,
            total_candles: 168000,
            persist_failures: 0,
            failed_instruments: vec![],
            failure_reasons: reasons,
        };
        let msg = event.to_message();
        assert!(msg.contains("Failure breakdown:"));
        assert!(msg.contains("token_expired: 5"));
        assert!(msg.contains("network_or_api: 3"));
    }

    #[test]
    fn test_historical_fetch_complete_shows_persist_warnings() {
        let event = NotificationEvent::HistoricalFetchComplete {
            instruments_fetched: 232,
            instruments_skipped: 1050,
            total_candles: 187458,
            persist_failures: 42,
        };
        let msg = event.to_message();
        assert!(msg.contains("Historical candles OK"));
        assert!(msg.contains("Persist errors: 42"));
    }

    #[test]
    fn test_candle_verification_failed_message() {
        let event = NotificationEvent::CandleVerificationFailed {
            instruments_checked: 209,
            instruments_with_gaps: 3,
            timeframe_details: "1m: 78,000 (207 instruments)\n5m: 15,600 (209 instruments)"
                .to_string(),
            ohlc_violations: 0,
            data_violations: 0,
            timestamp_violations: 0,
            ohlc_details: vec![],
            data_details: vec![],
            timestamp_details: vec![],
            weekend_violations: 0,
            weekend_details: vec![],
        };
        let msg = event.to_message();
        assert!(msg.contains("verification FAILED"));
        assert!(msg.contains("209"));
        assert!(msg.contains("Gaps: 3"));
        assert!(msg.contains("Timeframes:"));
        assert!(msg.contains("1m: 78,000"));
    }

    #[test]
    fn test_candle_verification_failed_shows_ohlc_details() {
        let event = NotificationEvent::CandleVerificationFailed {
            instruments_checked: 232,
            instruments_with_gaps: 0,
            timeframe_details: String::new(),
            ohlc_violations: 2,
            data_violations: 0,
            timestamp_violations: 0,
            ohlc_details: vec![
                "\u{2022} RELIANCE (NSE_EQ) 1m @ 2026-03-18 10:15 IST\n  H=2440.0 < L=2450.0"
                    .to_string(),
            ],
            data_details: vec![],
            timestamp_details: vec![],
            weekend_violations: 0,
            weekend_details: vec![],
        };
        let msg = event.to_message();
        assert!(msg.contains("OHLC violations (2)"));
        assert!(msg.contains("RELIANCE"));
        assert!(msg.contains("H=2440.0 < L=2450.0"));
    }

    #[test]
    fn test_candle_verification_failed_shows_all_violations() {
        let event = NotificationEvent::CandleVerificationFailed {
            instruments_checked: 232,
            instruments_with_gaps: 3,
            timeframe_details: "1m: 85125 (229 inst)".to_string(),
            ohlc_violations: 2,
            data_violations: 5,
            timestamp_violations: 8,
            ohlc_details: vec!["ohlc line".to_string()],
            data_details: vec!["data line".to_string()],
            timestamp_details: vec!["ts line".to_string()],
            weekend_violations: 0,
            weekend_details: vec![],
        };
        let msg = event.to_message();
        assert!(msg.contains("OHLC violations (2)"));
        assert!(msg.contains("Data violations (5)"));
        assert!(msg.contains("Timestamp violations (8)"));
    }

    #[test]
    fn test_candle_verification_failed_zero_instruments() {
        let event = NotificationEvent::CandleVerificationFailed {
            instruments_checked: 0,
            instruments_with_gaps: 0,
            timeframe_details: String::new(),
            ohlc_violations: 0,
            data_violations: 0,
            timestamp_violations: 0,
            ohlc_details: vec![],
            data_details: vec![],
            timestamp_details: vec![],
            weekend_violations: 0,
            weekend_details: vec![],
        };
        let msg = event.to_message();
        assert!(msg.contains("Checked: 0"));
        assert!(msg.contains("No instrument data found"));
    }

    #[test]
    fn test_candle_verification_failed_is_high() {
        let event = NotificationEvent::CandleVerificationFailed {
            instruments_checked: 209,
            instruments_with_gaps: 3,
            timeframe_details: String::new(),
            ohlc_violations: 0,
            data_violations: 0,
            timestamp_violations: 0,
            ohlc_details: vec![],
            data_details: vec![],
            timestamp_details: vec![],
            weekend_violations: 0,
            weekend_details: vec![],
        };
        assert_eq!(event.severity(), Severity::High);
    }

    #[test]
    fn test_candle_verification_passed_shows_check_marks() {
        let event = NotificationEvent::CandleVerificationPassed {
            instruments_checked: 232,
            total_candles: 187500,
            timeframe_details: "1m: 86250 (232 inst)".to_string(),
            ohlc_violations: 0,
            data_violations: 0,
            timestamp_violations: 0,
            weekend_violations: 0,
        };
        let msg = event.to_message();
        assert!(msg.contains("Candle verification OK"));
        assert!(msg.contains("OHLC"));
        assert!(msg.contains("Data"));
        assert!(msg.contains("Timestamps"));
    }

    #[test]
    fn test_candle_verification_passed_shows_warnings_if_any() {
        let event = NotificationEvent::CandleVerificationPassed {
            instruments_checked: 232,
            total_candles: 187500,
            timeframe_details: String::new(),
            ohlc_violations: 0,
            data_violations: 2,
            timestamp_violations: 1,
            weekend_violations: 0,
        };
        let msg = event.to_message();
        assert!(msg.contains("Data violations: 2 (non-blocking)"));
        assert!(msg.contains("Timestamp violations: 1 (non-blocking)"));
    }

    #[test]
    fn test_cross_match_passed_message() {
        let event = NotificationEvent::CandleCrossMatchPassed {
            timeframes_checked: 5,
            candles_compared: 187500,
        };
        let msg = event.to_message();
        assert!(msg.contains("cross-match OK"));
        assert!(msg.contains("187500"));
        assert!(msg.contains("tolerance"));
    }

    #[test]
    fn test_cross_match_failed_shows_details() {
        let event = NotificationEvent::CandleCrossMatchFailed {
            candles_compared: 187500,
            mismatches: 12,
            missing_live: 8,
            mismatch_details: vec![
                "\u{2022} RELIANCE (NSE_EQ) 1m @ 2026-03-18 10:15 IST\n  Hist: O=2450.0 H=2465.0\n  Live: O=2450.0 H=2463.5\n  Diff: H(-1.5)".to_string(),
            ],
        };
        let msg = event.to_message();
        assert!(msg.contains("cross-match FAILED"));
        assert!(msg.contains("Mismatches: 12"));
        assert!(msg.contains("Missing live: 8"));
        assert!(msg.contains("RELIANCE"));
        assert!(msg.contains("H(-1.5)"));
    }

    #[test]
    fn test_cross_match_failed_is_high() {
        let event = NotificationEvent::CandleCrossMatchFailed {
            candles_compared: 187500,
            mismatches: 12,
            missing_live: 8,
            mismatch_details: vec![],
        };
        assert_eq!(event.severity(), Severity::High);
    }

    #[test]
    fn test_cross_match_passed_is_low() {
        let event = NotificationEvent::CandleCrossMatchPassed {
            timeframes_checked: 5,
            candles_compared: 187500,
        };
        assert_eq!(event.severity(), Severity::Low);
    }

    // -- Severity tests --

    #[test]
    fn test_auth_failed_is_critical() {
        let event = NotificationEvent::AuthenticationFailed {
            reason: "timeout".to_string(),
        };
        assert_eq!(event.severity(), Severity::Critical);
    }

    #[test]
    fn test_token_renewal_failed_is_critical() {
        let event = NotificationEvent::TokenRenewalFailed {
            attempts: 3,
            reason: "timeout".to_string(),
        };
        assert_eq!(event.severity(), Severity::Critical);
    }

    #[test]
    fn test_ws_disconnected_is_high() {
        let event = NotificationEvent::WebSocketDisconnected {
            connection_index: 0,
            reason: "reset".to_string(),
        };
        assert_eq!(event.severity(), Severity::High);
    }

    #[test]
    fn test_custom_is_high() {
        let event = NotificationEvent::Custom {
            message: "alert".to_string(),
        };
        assert_eq!(event.severity(), Severity::High);
    }

    #[test]
    fn test_startup_complete_is_info() {
        let event = NotificationEvent::StartupComplete { mode: "LIVE" };
        assert_eq!(event.severity(), Severity::Info);
    }

    #[test]
    fn test_shutdown_complete_is_info() {
        let event = NotificationEvent::ShutdownComplete;
        assert_eq!(event.severity(), Severity::Info);
    }

    // -- IP Verification event tests --

    #[test]
    fn test_ip_verification_failed_message() {
        let event = NotificationEvent::IpVerificationFailed {
            reason: "IP mismatch — expected 203.0.XXX.XX, got 198.51.XXX.XX".to_string(),
        };
        let msg = event.to_message();
        assert!(msg.contains("IP VERIFICATION FAILED"));
        assert!(msg.contains("IP mismatch"));
        assert!(msg.contains("Boot blocked"));
    }

    #[test]
    fn test_ip_verification_failed_is_critical() {
        let event = NotificationEvent::IpVerificationFailed {
            reason: "SSM unreachable".to_string(),
        };
        assert_eq!(event.severity(), Severity::Critical);
    }

    #[test]
    fn test_ip_verification_success_message_masks_ip() {
        let event = NotificationEvent::IpVerificationSuccess {
            verified_ip: "203.0.113.42".to_string(),
        };
        let msg = event.to_message();
        assert!(msg.contains("IP verified"));
        // SECURITY: Full IP must NOT appear in Telegram message
        assert!(
            !msg.contains("203.0.113.42"),
            "full IP must be masked in notification: {msg}"
        );
        // Only network prefix should be visible
        assert!(msg.contains("203.0.XXX.XX"), "masked IP expected: {msg}");
    }

    #[test]
    fn test_ip_verification_success_is_low() {
        let event = NotificationEvent::IpVerificationSuccess {
            verified_ip: "10.0.0.1".to_string(),
        };
        assert_eq!(event.severity(), Severity::Low);
    }

    #[test]
    fn test_mask_ip_for_notification_standard() {
        assert_eq!(mask_ip_for_notification("59.92.114.17"), "59.92.XXX.XX");
        assert_eq!(mask_ip_for_notification("10.0.0.1"), "10.0.XXX.XX");
    }

    #[test]
    fn test_mask_ip_for_notification_invalid() {
        assert_eq!(mask_ip_for_notification("not-an-ip"), "XXX.XXX.XXX.XXX");
        assert_eq!(mask_ip_for_notification(""), "XXX.XXX.XXX.XXX");
    }

    #[test]
    fn test_historical_fetch_complete_message() {
        let event = NotificationEvent::HistoricalFetchComplete {
            instruments_fetched: 50,
            instruments_skipped: 200,
            total_candles: 187500,
            persist_failures: 0,
        };
        let msg = event.to_message();
        assert!(msg.contains("Historical candles OK"));
        assert!(msg.contains("50"));
        assert!(msg.contains("200"));
        assert!(msg.contains("187500"));
        assert!(msg.contains("1m, 5m, 15m, 60m, 1d"));
    }

    #[test]
    fn test_historical_fetch_complete_is_low() {
        let event = NotificationEvent::HistoricalFetchComplete {
            instruments_fetched: 50,
            instruments_skipped: 200,
            total_candles: 187500,
            persist_failures: 0,
        };
        assert_eq!(event.severity(), Severity::Low);
    }

    #[test]
    fn test_candle_verification_passed_message() {
        let event = NotificationEvent::CandleVerificationPassed {
            instruments_checked: 50,
            total_candles: 187500,
            timeframe_details: "1m: 18,750 (50 inst)\n5m: 3,750 (50 inst)\n15m: 1,250 (50 inst)\n60m: 312 (50 inst)\n1d: 50 (50 inst)".to_string(),
            ohlc_violations: 0,
            data_violations: 0,
            timestamp_violations: 0,
            weekend_violations: 0,
        };
        let msg = event.to_message();
        assert!(msg.contains("Candle verification OK"));
        assert!(msg.contains("50"));
        assert!(msg.contains("187500"));
        assert!(msg.contains("Timeframes:"));
        assert!(msg.contains("1m: 18,750"));
        assert!(msg.contains("1d: 50"));
    }

    #[test]
    fn test_candle_verification_passed_is_low() {
        let event = NotificationEvent::CandleVerificationPassed {
            instruments_checked: 50,
            total_candles: 187500,
            timeframe_details: String::new(),
            ohlc_violations: 0,
            data_violations: 0,
            timestamp_violations: 0,
            weekend_violations: 0,
        };
        assert_eq!(event.severity(), Severity::Low);
    }

    #[test]
    fn test_severity_ordering() {
        assert!(Severity::Critical > Severity::High);
        assert!(Severity::High > Severity::Medium);
        assert!(Severity::Medium > Severity::Low);
        assert!(Severity::Low > Severity::Info);
    }

    // -- OMS notification event tests --

    #[test]
    fn test_order_rejected_message() {
        let event = NotificationEvent::OrderRejected {
            correlation_id: "ORD-12345".to_string(),
            reason: "DH-906: Invalid order quantity".to_string(),
        };
        let msg = event.to_message();
        assert!(msg.contains("Order REJECTED"));
        assert!(msg.contains("ORD-12345"));
        assert!(msg.contains("DH-906"));
    }

    #[test]
    fn test_oms_event_severity() {
        let rejected = NotificationEvent::OrderRejected {
            correlation_id: "X".to_string(),
            reason: "bad".to_string(),
        };
        assert_eq!(rejected.severity(), Severity::High);

        let cb_open = NotificationEvent::CircuitBreakerOpened {
            consecutive_failures: 5,
        };
        assert_eq!(cb_open.severity(), Severity::High);

        let cb_close = NotificationEvent::CircuitBreakerClosed;
        assert_eq!(cb_close.severity(), Severity::Medium);

        let rate_limit = NotificationEvent::RateLimitExhausted {
            limit_type: "per_second".to_string(),
        };
        assert_eq!(rate_limit.severity(), Severity::High);
    }

    #[test]
    fn test_oms_event_formatting() {
        let cb = NotificationEvent::CircuitBreakerOpened {
            consecutive_failures: 3,
        };
        let msg = cb.to_message();
        assert!(msg.contains("Circuit breaker OPENED"));
        assert!(msg.contains("3"));

        let rl = NotificationEvent::RateLimitExhausted {
            limit_type: "daily".to_string(),
        };
        let msg = rl.to_message();
        assert!(msg.contains("Rate limit EXHAUSTED"));
        assert!(msg.contains("daily"));
    }

    #[test]
    fn test_circuit_breaker_notify_on_open() {
        let event = NotificationEvent::CircuitBreakerOpened {
            consecutive_failures: 5,
        };
        let msg = event.to_message();
        assert!(msg.contains("OPENED"));
        assert!(msg.contains("halted"));
        assert_eq!(event.severity(), Severity::High);
    }

    #[test]
    fn test_risk_halt_notification() {
        let event = NotificationEvent::RiskHalt {
            reason: "daily_loss_breach: -25000.00 exceeds threshold".to_string(),
        };
        let msg = event.to_message();
        assert!(msg.contains("RISK HALT"));
        assert!(msg.contains("daily_loss_breach"));
        assert_eq!(event.severity(), Severity::Critical);
    }

    #[test]
    fn test_ws_reconnection_exhausted_notification() {
        let event = NotificationEvent::WebSocketReconnectionExhausted {
            connection_index: 2,
            attempts: 10,
        };
        let msg = event.to_message();
        assert!(msg.contains("RECONNECTION EXHAUSTED"));
        assert!(msg.contains("#2"));
        assert!(msg.contains("10"));
        assert_eq!(event.severity(), Severity::Critical);
    }

    #[test]
    fn test_token_renewal_deadline_missed_notification() {
        let event = NotificationEvent::TokenRenewalDeadlineMissed {
            deadline_hour_ist: 14,
        };
        let msg = event.to_message();
        assert!(msg.contains("DEADLINE MISSED"));
        assert!(msg.contains("14:00 IST"));
        assert_eq!(event.severity(), Severity::Critical);
    }

    // =====================================================================
    // Additional coverage: severity for all remaining variants, boot events,
    // append_detail_lines helper, edge cases in message formatting
    // =====================================================================

    #[test]
    fn test_boot_health_check_message_and_severity() {
        let event = NotificationEvent::BootHealthCheck {
            services_healthy: 7,
            services_total: 8,
        };
        let msg = event.to_message();
        assert!(msg.contains("Boot health check"));
        assert!(msg.contains("7/8"));
        assert_eq!(event.severity(), Severity::Low);
    }

    #[test]
    fn test_boot_deadline_missed_message_and_severity() {
        let event = NotificationEvent::BootDeadlineMissed {
            deadline_secs: 120,
            step: "QuestDB DDL".to_string(),
        };
        let msg = event.to_message();
        assert!(msg.contains("BOOT DEADLINE MISSED"));
        assert!(msg.contains("120s"));
        assert!(msg.contains("QuestDB DDL"));
        assert_eq!(event.severity(), Severity::Critical);
    }

    #[test]
    fn test_instrument_build_success_severity() {
        let event = NotificationEvent::InstrumentBuildSuccess {
            source: "primary".to_string(),
            derivative_count: 100,
            underlying_count: 10,
        };
        assert_eq!(event.severity(), Severity::Low);
    }

    #[test]
    fn test_instrument_build_failed_severity() {
        let event = NotificationEvent::InstrumentBuildFailed {
            reason: "test".to_string(),
            manual_trigger_url: "http://test".to_string(),
        };
        assert_eq!(event.severity(), Severity::High);
    }

    #[test]
    fn test_ws_connected_severity() {
        let event = NotificationEvent::WebSocketConnected {
            connection_index: 0,
        };
        assert_eq!(event.severity(), Severity::Low);
    }

    #[test]
    fn test_ws_reconnected_severity() {
        let event = NotificationEvent::WebSocketReconnected {
            connection_index: 0,
        };
        assert_eq!(event.severity(), Severity::Medium);
    }

    #[test]
    fn test_shutdown_initiated_severity() {
        assert_eq!(
            NotificationEvent::ShutdownInitiated.severity(),
            Severity::Medium
        );
    }

    #[test]
    fn test_token_renewed_severity() {
        assert_eq!(NotificationEvent::TokenRenewed.severity(), Severity::Low);
    }

    #[test]
    fn test_auth_success_severity() {
        assert_eq!(
            NotificationEvent::AuthenticationSuccess.severity(),
            Severity::Low
        );
    }

    #[test]
    fn test_circuit_breaker_closed_message() {
        let event = NotificationEvent::CircuitBreakerClosed;
        let msg = event.to_message();
        assert!(msg.contains("Circuit breaker CLOSED"));
        assert!(msg.contains("resumed"));
    }

    #[test]
    fn test_historical_fetch_complete_no_persist_failures() {
        let event = NotificationEvent::HistoricalFetchComplete {
            instruments_fetched: 232,
            instruments_skipped: 1050,
            total_candles: 187458,
            persist_failures: 0,
        };
        let msg = event.to_message();
        assert!(!msg.contains("Persist errors"));
    }

    #[test]
    fn test_historical_fetch_failed_empty_reasons_and_instruments() {
        let event = NotificationEvent::HistoricalFetchFailed {
            instruments_fetched: 0,
            instruments_failed: 0,
            total_candles: 0,
            persist_failures: 0,
            failed_instruments: vec![],
            failure_reasons: std::collections::HashMap::new(),
        };
        let msg = event.to_message();
        assert!(msg.contains("partial failure"));
        assert!(!msg.contains("Failure breakdown"));
        assert!(!msg.contains("Failed instruments"));
    }

    #[test]
    fn test_cross_match_failed_no_missing_live() {
        let event = NotificationEvent::CandleCrossMatchFailed {
            candles_compared: 1000,
            mismatches: 5,
            missing_live: 0,
            mismatch_details: vec![],
        };
        let msg = event.to_message();
        assert!(!msg.contains("Missing live"));
    }

    #[test]
    fn test_cross_match_failed_no_mismatch_details_section() {
        let event = NotificationEvent::CandleCrossMatchFailed {
            candles_compared: 1000,
            mismatches: 0,
            missing_live: 0,
            mismatch_details: vec![],
        };
        let msg = event.to_message();
        // The header always contains "Mismatches: N", but the details section should be absent
        assert!(!msg.contains("<b>Mismatches:</b>"));
    }

    #[test]
    fn test_candle_verification_passed_empty_timeframe_details() {
        let event = NotificationEvent::CandleVerificationPassed {
            instruments_checked: 10,
            total_candles: 1000,
            timeframe_details: String::new(),
            ohlc_violations: 0,
            data_violations: 0,
            timestamp_violations: 0,
            weekend_violations: 0,
        };
        let msg = event.to_message();
        assert!(!msg.contains("Timeframes:"));
    }

    #[test]
    fn test_candle_verification_passed_ohlc_violations_only() {
        let event = NotificationEvent::CandleVerificationPassed {
            instruments_checked: 10,
            total_candles: 1000,
            timeframe_details: String::new(),
            ohlc_violations: 3,
            data_violations: 0,
            timestamp_violations: 0,
            weekend_violations: 0,
        };
        let msg = event.to_message();
        assert!(msg.contains("OHLC violations: 3"));
        assert!(!msg.contains("Data violations"));
        assert!(!msg.contains("Timestamp violations"));
    }

    #[test]
    fn test_append_detail_lines_truncation() {
        let details: Vec<String> = (0..15).map(|i| format!("line {i}")).collect();
        let mut msg = String::new();
        super::append_detail_lines(&mut msg, &details, 15);
        // Should show first 10 and "+5 more"
        assert!(msg.contains("line 0"));
        assert!(msg.contains("line 9"));
        assert!(!msg.contains("line 10"));
        assert!(msg.contains("+5 more"));
    }

    #[test]
    fn test_append_detail_lines_no_truncation() {
        let details: Vec<String> = (0..5).map(|i| format!("line {i}")).collect();
        let mut msg = String::new();
        super::append_detail_lines(&mut msg, &details, 5);
        assert!(msg.contains("line 0"));
        assert!(msg.contains("line 4"));
        assert!(!msg.contains("more"));
    }

    #[test]
    fn test_append_detail_lines_empty() {
        let mut msg = String::new();
        super::append_detail_lines(&mut msg, &[], 0);
        assert!(msg.is_empty());
    }

    #[test]
    fn test_severity_equality() {
        assert_eq!(Severity::Critical, Severity::Critical);
        assert_ne!(Severity::Critical, Severity::High);
    }

    #[test]
    fn test_severity_debug() {
        let debug = format!("{:?}", Severity::Critical);
        assert_eq!(debug, "Critical");
    }

    #[test]
    fn test_notification_event_clone() {
        let event = NotificationEvent::TokenRenewed;
        let cloned = event.clone();
        assert_eq!(cloned.to_message(), event.to_message());
    }

    #[test]
    fn test_cross_match_failed_truncates_long_mismatch_details() {
        let details: Vec<String> = (0..20).map(|i| format!("mismatch {i}")).collect();
        let event = NotificationEvent::CandleCrossMatchFailed {
            candles_compared: 100,
            mismatches: 20,
            missing_live: 0,
            mismatch_details: details,
        };
        let msg = event.to_message();
        assert!(msg.contains("mismatch 0"));
        assert!(msg.contains("mismatch 9"));
        assert!(!msg.contains("mismatch 10"));
        assert!(msg.contains("+10 more"));
    }

    // =====================================================================
    // Additional coverage: Severity Copy/Clone, Debug impls, event Debug,
    // append_detail_lines total_count > details.len(), edge cases
    // =====================================================================

    #[test]
    fn test_severity_is_copy() {
        let s = Severity::High;
        let copy = s;
        // Both should be usable after copy (Copy trait)
        assert_eq!(s, copy);
        assert_eq!(s, Severity::High);
    }

    #[test]
    fn test_severity_clone() {
        let s = Severity::Medium;
        #[allow(clippy::clone_on_copy)]
        let cloned = s.clone();
        assert_eq!(s, cloned);
    }

    #[test]
    fn test_severity_all_variants_debug() {
        let variants = [
            Severity::Info,
            Severity::Low,
            Severity::Medium,
            Severity::High,
            Severity::Critical,
        ];
        let expected = ["Info", "Low", "Medium", "High", "Critical"];
        for (variant, name) in variants.iter().zip(expected.iter()) {
            assert_eq!(format!("{variant:?}"), *name);
        }
    }

    #[test]
    fn test_severity_ord_covers_all_pairs() {
        let ordered = [
            Severity::Info,
            Severity::Low,
            Severity::Medium,
            Severity::High,
            Severity::Critical,
        ];
        for i in 0..ordered.len() {
            for j in (i + 1)..ordered.len() {
                assert!(
                    ordered[i] < ordered[j],
                    "{:?} should be < {:?}",
                    ordered[i],
                    ordered[j]
                );
            }
        }
    }

    #[test]
    fn test_notification_event_debug_impl() {
        let event = NotificationEvent::TokenRenewed;
        let debug_str = format!("{event:?}");
        assert!(debug_str.contains("TokenRenewed"));
    }

    #[test]
    fn test_notification_event_debug_with_fields() {
        let event = NotificationEvent::WebSocketDisconnected {
            connection_index: 3,
            reason: "timeout".to_string(),
        };
        let debug_str = format!("{event:?}");
        assert!(debug_str.contains("WebSocketDisconnected"));
        assert!(debug_str.contains("3"));
        assert!(debug_str.contains("timeout"));
    }

    #[test]
    fn test_notification_event_clone_with_complex_fields() {
        let event = NotificationEvent::HistoricalFetchFailed {
            instruments_fetched: 100,
            instruments_failed: 5,
            total_candles: 50000,
            persist_failures: 2,
            failed_instruments: vec!["RELIANCE".to_string(), "TCS".to_string()],
            failure_reasons: {
                let mut m = std::collections::HashMap::new();
                m.insert("network".to_string(), 3);
                m.insert("timeout".to_string(), 2);
                m
            },
        };
        let cloned = event.clone();
        assert_eq!(event.to_message(), cloned.to_message());
        assert_eq!(event.severity(), cloned.severity());
    }

    #[test]
    fn test_append_detail_lines_total_greater_than_details_len() {
        // total_count=50 but only 3 detail strings provided
        // Should show all 3 lines and "+47 more"
        let details = vec![
            "line A".to_string(),
            "line B".to_string(),
            "line C".to_string(),
        ];
        let mut msg = String::new();
        super::append_detail_lines(&mut msg, &details, 50);
        assert!(msg.contains("line A"));
        assert!(msg.contains("line B"));
        assert!(msg.contains("line C"));
        assert!(msg.contains("+47 more"));
    }

    #[test]
    fn test_append_detail_lines_exactly_10_no_truncation() {
        let details: Vec<String> = (0..10).map(|i| format!("detail {i}")).collect();
        let mut msg = String::new();
        super::append_detail_lines(&mut msg, &details, 10);
        assert!(msg.contains("detail 0"));
        assert!(msg.contains("detail 9"));
        assert!(!msg.contains("more"));
    }

    #[test]
    fn test_append_detail_lines_11_triggers_truncation() {
        let details: Vec<String> = (0..11).map(|i| format!("detail {i}")).collect();
        let mut msg = String::new();
        super::append_detail_lines(&mut msg, &details, 11);
        assert!(msg.contains("detail 0"));
        assert!(msg.contains("detail 9"));
        assert!(!msg.contains("detail 10"));
        assert!(msg.contains("+1 more"));
    }

    #[test]
    fn test_candle_verification_failed_data_violations_only() {
        let event = NotificationEvent::CandleVerificationFailed {
            instruments_checked: 50,
            instruments_with_gaps: 0,
            timeframe_details: String::new(),
            ohlc_violations: 0,
            data_violations: 7,
            timestamp_violations: 0,
            ohlc_details: vec![],
            data_details: vec!["bad data line".to_string()],
            timestamp_details: vec![],
            weekend_violations: 0,
            weekend_details: vec![],
        };
        let msg = event.to_message();
        assert!(msg.contains("Data violations (7)"));
        assert!(msg.contains("bad data line"));
        assert!(!msg.contains("OHLC violations"));
        assert!(!msg.contains("Timestamp violations"));
    }

    #[test]
    fn test_candle_verification_failed_timestamp_violations_only() {
        let event = NotificationEvent::CandleVerificationFailed {
            instruments_checked: 50,
            instruments_with_gaps: 0,
            timeframe_details: String::new(),
            ohlc_violations: 0,
            data_violations: 0,
            timestamp_violations: 4,
            ohlc_details: vec![],
            data_details: vec![],
            timestamp_details: vec!["ts violation".to_string()],
            weekend_violations: 0,
            weekend_details: vec![],
        };
        let msg = event.to_message();
        assert!(msg.contains("Timestamp violations (4)"));
        assert!(msg.contains("ts violation"));
        assert!(!msg.contains("OHLC violations"));
        assert!(!msg.contains("Data violations"));
    }

    #[test]
    fn test_risk_halt_severity_is_critical() {
        let event = NotificationEvent::RiskHalt {
            reason: "position_limit".to_string(),
        };
        assert_eq!(event.severity(), Severity::Critical);
    }

    #[test]
    fn test_ws_reconnection_exhausted_severity_is_critical() {
        let event = NotificationEvent::WebSocketReconnectionExhausted {
            connection_index: 0,
            attempts: 5,
        };
        assert_eq!(event.severity(), Severity::Critical);
    }

    #[test]
    fn test_token_renewal_deadline_missed_severity_is_critical() {
        let event = NotificationEvent::TokenRenewalDeadlineMissed {
            deadline_hour_ist: 10,
        };
        assert_eq!(event.severity(), Severity::Critical);
    }

    #[test]
    fn test_rate_limit_exhausted_message_format() {
        let event = NotificationEvent::RateLimitExhausted {
            limit_type: "per_second".to_string(),
        };
        let msg = event.to_message();
        assert!(msg.contains("Rate limit EXHAUSTED"));
        assert!(msg.contains("per_second"));
    }

    #[test]
    fn test_risk_halt_message_format() {
        let event = NotificationEvent::RiskHalt {
            reason: "position_limit_breach".to_string(),
        };
        let msg = event.to_message();
        assert!(msg.contains("RISK HALT"));
        assert!(msg.contains("position_limit_breach"));
    }

    #[test]
    fn test_every_severity_variant_is_reachable() {
        // Verify each severity level is returned by at least one event variant
        let info_event = NotificationEvent::StartupComplete { mode: "LIVE" };
        assert_eq!(info_event.severity(), Severity::Info);

        let low_event = NotificationEvent::TokenRenewed;
        assert_eq!(low_event.severity(), Severity::Low);

        let medium_event = NotificationEvent::ShutdownInitiated;
        assert_eq!(medium_event.severity(), Severity::Medium);

        let high_event = NotificationEvent::Custom {
            message: "x".to_string(),
        };
        assert_eq!(high_event.severity(), Severity::High);

        let critical_event = NotificationEvent::RiskHalt {
            reason: "x".to_string(),
        };
        assert_eq!(critical_event.severity(), Severity::Critical);
    }
}

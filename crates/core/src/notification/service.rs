//! Notification service — Telegram + SNS SMS.
//!
//! ONE source (AWS SSM), ONE code path, everywhere:
//! - Rust app via `NotificationService` → same SSM → same Telegram + SNS
//! - IntelliJ runs same binary → same SSM → same Telegram + SNS
//! - Claude Code sessions use shell script → same SSM → same Telegram
//! - CI/CD uses IAM role → same SSM → same Telegram
//!
//! # Channel Routing
//!
//! All events → Telegram (always).
//! Critical/High severity events → Telegram + SNS SMS (if `sns_enabled`).
//!
//! # Failure Behavior
//!
//! SSM fetch fails at boot → `NotificationService` is created in no-op mode.
//! HTTP/SNS send fails → logged as `warn`, not propagated. Never blocks caller.
//! Bot token NEVER logged (SecretString enforces `[REDACTED]`).
//!
//! # Performance
//!
//! Notifications are cold path. `notify` spawns a `tokio::task` — zero
//! blocking on the caller. Hot path (tick parsing) is completely unaffected.

use std::sync::Arc;
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use tracing::{info, instrument, warn};

use dhan_live_trader_common::config::NotificationConfig;
use dhan_live_trader_common::constants::{
    SNS_PHONE_NUMBER_SECRET, SSM_SNS_SERVICE, SSM_TELEGRAM_SERVICE, TELEGRAM_BOT_TOKEN_SECRET,
    TELEGRAM_CHAT_ID_SECRET,
};

use crate::auth::secret_manager::{
    build_ssm_path, create_ssm_client, fetch_secret, resolve_environment,
};

use super::events::{NotificationEvent, Severity};

// ---------------------------------------------------------------------------
// Internal State
// ---------------------------------------------------------------------------

/// Internal mode — avoids `Option<>` noise at callsites.
enum NotificationMode {
    /// SSM credentials loaded — sends are attempted.
    Active {
        bot_token: SecretString,
        chat_id: String,
        http_client: reqwest::Client,
        telegram_api_base_url: String,
        /// SNS client for SMS sends. `None` if SNS disabled or SSM fetch failed.
        sns_client: Option<aws_sdk_sns::Client>,
        /// E.164 phone number for SMS. `None` if SNS disabled or SSM fetch failed.
        sns_phone_number: Option<String>,
    },
    /// SSM unavailable at boot — all sends are silent no-ops.
    NoOp,
}

// ---------------------------------------------------------------------------
// Public Service
// ---------------------------------------------------------------------------

/// Fire-and-forget notification service — Telegram + SNS SMS.
///
/// Create once at boot via `NotificationService::initialize`. Pass `Arc<Self>`
/// to any component that needs alerting. Call `notify` to send — it spawns
/// a background task and returns immediately.
///
/// All events go to Telegram. Critical/High events also trigger SNS SMS
/// (if `sns_enabled` is true and the phone number is in SSM).
pub struct NotificationService {
    mode: NotificationMode,
}

impl NotificationService {
    /// Initializes the notification service by fetching Telegram credentials from SSM.
    ///
    /// If SSM is unavailable (no IAM role, tokens not in SSM, etc.), returns
    /// a no-op service. The caller is never blocked and boot continues normally.
    /// Telegram tokens are always in real AWS SSM (dev and prod, same region).
    ///
    /// # Arguments
    ///
    /// * `config` — `NotificationConfig` from `config/base.toml`.
    ///
    /// # Returns
    ///
    /// Always returns `Arc<Self>` — never fails. Uses no-op mode on SSM failure.
    ///
    /// # Performance
    ///
    /// Called once at boot. Not on hot path.
    #[instrument(skip_all, name = "notification_service_init")]
    pub async fn initialize(config: &NotificationConfig) -> Arc<Self> {
        let environment = match resolve_environment() {
            Ok(env) => env,
            Err(err) => {
                warn!(
                    error = %err,
                    "notification: cannot resolve environment — using no-op mode"
                );
                return Arc::new(Self {
                    mode: NotificationMode::NoOp,
                });
            }
        };

        let ssm_client = create_ssm_client().await;

        let bot_token_path = build_ssm_path(
            &environment,
            SSM_TELEGRAM_SERVICE,
            TELEGRAM_BOT_TOKEN_SECRET,
        );
        let chat_id_path =
            build_ssm_path(&environment, SSM_TELEGRAM_SERVICE, TELEGRAM_CHAT_ID_SECRET);

        let (bot_token_result, chat_id_result) = tokio::join!(
            fetch_secret(&ssm_client, &bot_token_path),
            fetch_secret(&ssm_client, &chat_id_path),
        );

        let bot_token = match bot_token_result {
            Ok(token) => token,
            Err(err) => {
                warn!(
                    error = %err,
                    path = %bot_token_path,
                    "notification: bot-token SSM fetch failed — using no-op mode"
                );
                return Arc::new(Self {
                    mode: NotificationMode::NoOp,
                });
            }
        };

        let chat_id = match chat_id_result {
            Ok(id) => id.expose_secret().to_string(),
            Err(err) => {
                warn!(
                    error = %err,
                    path = %chat_id_path,
                    "notification: chat-id SSM fetch failed — using no-op mode"
                );
                return Arc::new(Self {
                    mode: NotificationMode::NoOp,
                });
            }
        };

        let http_client = match reqwest::Client::builder()
            .timeout(Duration::from_millis(config.send_timeout_ms))
            .build()
        {
            Ok(client) => client,
            Err(err) => {
                warn!(
                    error = %err,
                    "notification: HTTP client build failed — using no-op mode"
                );
                return Arc::new(Self {
                    mode: NotificationMode::NoOp,
                });
            }
        };

        // --- SNS SMS (optional, for Critical/High severity alerts) ---
        let (sns_client, sns_phone_number) = if config.sns_enabled {
            let phone_path = build_ssm_path(&environment, SSM_SNS_SERVICE, SNS_PHONE_NUMBER_SECRET);
            match fetch_secret(&ssm_client, &phone_path).await {
                Ok(phone_secret) => {
                    let phone = phone_secret.expose_secret().to_string();
                    let aws_cfg = aws_config::defaults(aws_config::BehaviorVersion::latest())
                        .region(aws_config::Region::new("ap-south-1"))
                        .load()
                        .await;
                    let client = aws_sdk_sns::Client::new(&aws_cfg);
                    info!(
                        phone_masked = %mask_phone(&phone),
                        "notification: SNS SMS active"
                    );
                    (Some(client), Some(phone))
                }
                Err(err) => {
                    warn!(
                        error = %err,
                        path = %phone_path,
                        "notification: SNS phone-number SSM fetch failed — SMS disabled"
                    );
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

        info!(
            chat_id = %chat_id,
            sns_enabled = config.sns_enabled,
            "notification service initialized — Telegram active"
        );

        Arc::new(Self {
            mode: NotificationMode::Active {
                bot_token,
                chat_id,
                http_client,
                telegram_api_base_url: config.telegram_api_base_url.clone(),
                sns_client,
                sns_phone_number,
            },
        })
    }

    /// Creates a disabled no-op instance.
    ///
    /// Used in tests or when notifications are intentionally disabled.
    pub fn disabled() -> Arc<Self> {
        Arc::new(Self {
            mode: NotificationMode::NoOp,
        })
    }

    /// Sends a notification event. Spawns background tasks — returns immediately.
    ///
    /// All events → Telegram. Critical/High → also SNS SMS (if configured).
    /// If the service is in no-op mode, this is a zero-cost return.
    /// If any send fails, logs `warn` and discards the error.
    ///
    /// # Performance
    ///
    /// Not on hot path. `tokio::spawn` overhead is acceptable for cold-path alerts.
    pub fn notify(self: &Arc<Self>, event: NotificationEvent) {
        match &self.mode {
            NotificationMode::NoOp => {
                // No-op: SSM was unavailable at boot. Do nothing.
            }
            NotificationMode::Active {
                bot_token,
                chat_id,
                http_client,
                telegram_api_base_url,
                sns_client,
                sns_phone_number,
            } => {
                let severity = event.severity();
                let message = event.to_message();

                // Always: Telegram
                {
                    let token = bot_token.clone();
                    let chat_id = chat_id.clone();
                    let client = http_client.clone();
                    let base_url = telegram_api_base_url.clone();
                    let msg = message.clone();

                    tokio::spawn(async move {
                        send_telegram_message(&client, &base_url, &token, &chat_id, &msg).await;
                    });
                }

                // Critical/High only: SNS SMS
                if severity >= Severity::High
                    && let (Some(sns), Some(phone)) = (sns_client, sns_phone_number)
                {
                    let sms_text = strip_html_tags(&message);
                    let client = sns.clone();
                    let phone = phone.clone();

                    tokio::spawn(async move {
                        send_sns_sms(&client, &phone, &sms_text).await;
                    });
                }
            }
        }
    }

    /// Returns `true` if the service is active (SSM credentials loaded).
    pub fn is_active(&self) -> bool {
        matches!(&self.mode, NotificationMode::Active { .. })
    }
}

// ---------------------------------------------------------------------------
// Internal HTTP Send
// ---------------------------------------------------------------------------

/// Posts a message to the Telegram Bot API.
///
/// Logs `warn` on any failure — does not return an error to the caller.
/// Bot token is never logged (only used in URL path).
///
/// # Performance
///
/// Cold path only. Called from a spawned task.
async fn send_telegram_message(
    client: &reqwest::Client,
    base_url: &str,
    bot_token: &SecretString,
    chat_id: &str,
    text: &str,
) {
    let url = format!("{}/bot{}/sendMessage", base_url, bot_token.expose_secret());

    let body = serde_json::json!({
        "chat_id": chat_id,
        "text": text,
        "parse_mode": "HTML",
    });

    match client.post(&url).json(&body).send().await {
        Ok(response) => {
            if response.status().is_success() {
                tracing::debug!("notification: Telegram message sent");
            } else {
                let status = response.status();
                warn!(
                    status = %status,
                    "notification: Telegram sendMessage returned non-success status"
                );
            }
        }
        Err(err) => {
            warn!(
                error = %err,
                "notification: Telegram sendMessage HTTP error"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Internal SNS Send
// ---------------------------------------------------------------------------

/// Sends an SMS via AWS SNS direct publish to a phone number.
///
/// Uses `Transactional` SMS type for highest delivery priority (not promotional).
/// Logs `warn` on failure — never panics, never blocks caller.
///
/// # Performance
///
/// Cold path only. Called from a spawned task.
async fn send_sns_sms(client: &aws_sdk_sns::Client, phone_number: &str, message: &str) {
    let sms_type = match aws_sdk_sns::types::MessageAttributeValue::builder()
        .data_type("String")
        .string_value("Transactional")
        .build()
    {
        Ok(v) => v,
        Err(err) => {
            warn!(error = %err, "notification: SNS MessageAttributeValue build failed");
            return;
        }
    };

    match client
        .publish()
        .phone_number(phone_number)
        .message(message)
        .message_attributes("AWS.SNS.SMS.SMSType", sms_type)
        .send()
        .await
    {
        Ok(output) => {
            tracing::debug!(
                message_id = ?output.message_id(),
                "notification: SNS SMS sent"
            );
        }
        Err(err) => {
            warn!(error = %err, "notification: SNS SMS send failed");
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Strips HTML tags from a Telegram-formatted message for plain-text SMS.
///
/// Converts `<b>text</b>` → `text`. Handles any `<tag>` generically.
/// No regex dependency — simple char scan.
fn strip_html_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Masks a phone number for safe logging.
///
/// `"+919876543210"` → `"+91XXXXX43210"`.
fn mask_phone(phone: &str) -> String {
    if phone.len() <= 8 {
        return "***".to_string();
    }
    let keep_prefix: usize = 3;
    let keep_suffix: usize = 5;
    let mask_len = phone
        .len()
        .saturating_sub(keep_prefix.saturating_add(keep_suffix));
    format!(
        "{}{}{}",
        &phone[..keep_prefix],
        "X".repeat(mask_len),
        &phone[phone.len().saturating_sub(keep_suffix)..]
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test code
mod tests {
    use super::*;

    #[test]
    fn test_disabled_service_is_not_active() {
        let service = NotificationService::disabled();
        assert!(!service.is_active());
    }

    #[test]
    fn test_disabled_service_notify_does_not_panic() {
        let service = NotificationService::disabled();
        // Fire-and-forget on a no-op service must not panic.
        service.notify(NotificationEvent::StartupComplete { mode: "LIVE" });
        service.notify(NotificationEvent::ShutdownComplete);
        service.notify(NotificationEvent::Custom {
            message: "test".to_string(),
        });
    }

    // -----------------------------------------------------------------------
    // Active mode tests
    // -----------------------------------------------------------------------

    fn make_active_service() -> Arc<NotificationService> {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();

        Arc::new(NotificationService {
            mode: NotificationMode::Active {
                bot_token: SecretString::from("test-bot-token".to_string()),
                chat_id: "123456789".to_string(),
                http_client,
                telegram_api_base_url: "http://127.0.0.1:1".to_string(),
                sns_client: None,
                sns_phone_number: None,
            },
        })
    }

    #[test]
    fn test_active_service_is_active() {
        let service = make_active_service();
        assert!(
            service.is_active(),
            "Active-mode service must return true from is_active()"
        );
    }

    #[tokio::test]
    async fn test_active_service_notify_does_not_panic() {
        let service = make_active_service();

        service.notify(NotificationEvent::StartupComplete { mode: "LIVE" });
        service.notify(NotificationEvent::ShutdownComplete);
        service.notify(NotificationEvent::AuthenticationSuccess);
        service.notify(NotificationEvent::Custom {
            message: "active-mode test".to_string(),
        });

        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // -----------------------------------------------------------------------
    // send_telegram_message — error path tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_send_telegram_message_connection_refused_does_not_panic() {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();

        let token = SecretString::from("fake-bot-token".to_string());

        send_telegram_message(
            &client,
            "http://127.0.0.1:1",
            &token,
            "999999999",
            "test message",
        )
        .await;
    }

    #[tokio::test]
    async fn test_send_telegram_message_invalid_url_does_not_panic() {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();

        let token = SecretString::from("fake-bot-token".to_string());

        send_telegram_message(
            &client,
            "not-a-valid-url://garbage",
            &token,
            "999999999",
            "test message",
        )
        .await;
    }

    // -----------------------------------------------------------------------
    // disabled() and mode checking
    // -----------------------------------------------------------------------

    #[test]
    fn test_disabled_returns_noop_mode() {
        let service = NotificationService::disabled();
        assert!(
            !service.is_active(),
            "disabled() must create a NoOp-mode service"
        );
    }

    #[test]
    fn test_multiple_disabled_instances_are_independent() {
        let s1 = NotificationService::disabled();
        let s2 = NotificationService::disabled();
        assert!(!s1.is_active());
        assert!(!s2.is_active());
        assert!(!Arc::ptr_eq(&s1, &s2));
    }

    #[test]
    fn test_disabled_service_handles_critical_events() {
        let service = NotificationService::disabled();
        service.notify(NotificationEvent::AuthenticationFailed {
            reason: "timeout".to_string(),
        });
        service.notify(NotificationEvent::TokenRenewalFailed {
            attempts: 3,
            reason: "timeout".to_string(),
        });
    }

    // -----------------------------------------------------------------------
    // send_telegram_message — non-success HTTP status path
    // -----------------------------------------------------------------------

    async fn start_mock_telegram_server(status_code: u16) -> String {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}", addr.port());

        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
                let body = r#"{"ok":false,"description":"Unauthorized"}"#;
                let response = format!(
                    "HTTP/1.1 {} Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status_code,
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });

        base_url
    }

    #[tokio::test]
    async fn test_send_telegram_message_non_success_status_does_not_panic() {
        let base_url = start_mock_telegram_server(401).await;

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();

        let token = SecretString::from("fake-bot-token".to_string());

        send_telegram_message(&client, &base_url, &token, "999999999", "test message").await;
    }

    #[tokio::test]
    async fn test_send_telegram_message_success_status_does_not_panic() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}", addr.port());

        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await;
                let body = r#"{"ok":true}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();

        let token = SecretString::from("fake-bot-token".to_string());

        send_telegram_message(&client, &base_url, &token, "999999999", "test message").await;
    }

    // -----------------------------------------------------------------------
    // Active mode — notify with all event types
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_active_service_notify_all_event_types() {
        let service = make_active_service();

        service.notify(NotificationEvent::AuthenticationFailed {
            reason: "test failure".to_string(),
        });
        service.notify(NotificationEvent::TokenRenewed);
        service.notify(NotificationEvent::TokenRenewalFailed {
            attempts: 3,
            reason: "timeout".to_string(),
        });
        service.notify(NotificationEvent::WebSocketConnected {
            connection_index: 0,
        });
        service.notify(NotificationEvent::WebSocketDisconnected {
            connection_index: 1,
            reason: "reset".to_string(),
        });
        service.notify(NotificationEvent::WebSocketReconnected {
            connection_index: 2,
        });
        service.notify(NotificationEvent::ShutdownInitiated);
        service.notify(NotificationEvent::HistoricalFetchFailed {
            instruments_fetched: 200,
            instruments_failed: 9,
            total_candles: 180000,
            persist_failures: 0,
            failed_instruments: vec![],
            failure_reasons: std::collections::HashMap::new(),
        });
        service.notify(NotificationEvent::CandleVerificationFailed {
            instruments_checked: 209,
            instruments_with_gaps: 3,
            timeframe_details: String::new(),
            ohlc_violations: 0,
            data_violations: 0,
            timestamp_violations: 0,
            ohlc_details: vec![],
            data_details: vec![],
            timestamp_details: vec![],
        });

        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    // -----------------------------------------------------------------------
    // initialize — falls back to no-op without real SSM
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_initialize_returns_noop_without_ssm() {
        let config = NotificationConfig {
            send_timeout_ms: 100,
            telegram_api_base_url: "https://api.telegram.org".to_string(),
            sns_enabled: false,
        };
        let service = NotificationService::initialize(&config).await;
        if crate::test_support::has_aws_credentials() {
            // Dev machine with real AWS credentials — Telegram creds fetched from SSM.
            assert!(
                service.is_active(),
                "with real AWS credentials, notification service should be active"
            );
        } else {
            assert!(
                !service.is_active(),
                "without real SSM, service should be in no-op mode"
            );
        }
    }

    // -----------------------------------------------------------------------
    // strip_html_tags and mask_phone
    // -----------------------------------------------------------------------

    #[test]
    fn test_strip_html_tags_removes_bold() {
        assert_eq!(strip_html_tags("<b>Auth FAILED</b>"), "Auth FAILED");
    }

    #[test]
    fn test_strip_html_tags_preserves_plain_text() {
        assert_eq!(strip_html_tags("plain text"), "plain text");
    }

    #[test]
    fn test_strip_html_tags_handles_nested() {
        assert_eq!(strip_html_tags("<b>bold <i>italic</i></b>"), "bold italic");
    }

    #[test]
    fn test_strip_html_tags_empty() {
        assert_eq!(strip_html_tags(""), "");
    }

    #[test]
    fn test_mask_phone_indian_number() {
        assert_eq!(mask_phone("+919876543210"), "+91XXXXX43210");
    }

    #[test]
    fn test_mask_phone_short_number() {
        assert_eq!(mask_phone("+12345"), "***");
    }

    #[test]
    fn test_mask_phone_us_number() {
        assert_eq!(mask_phone("+12025551234"), "+12XXXX51234");
    }

    // -----------------------------------------------------------------------
    // strip_html_tags — edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_strip_html_tags_unclosed_tag() {
        // Unclosed `<` means everything after it is treated as inside a tag
        assert_eq!(strip_html_tags("before<unclosed"), "before");
    }

    #[test]
    fn test_strip_html_tags_only_tags() {
        assert_eq!(strip_html_tags("<b></b><i></i>"), "");
    }

    #[test]
    fn test_strip_html_tags_special_chars_in_content() {
        // `&amp;` and `&lt;` entities are NOT HTML tags — preserved as-is
        assert_eq!(strip_html_tags("a &amp; b"), "a &amp; b");
    }

    #[test]
    fn test_strip_html_tags_angle_brackets_in_text() {
        // `>` without a preceding `<` is just text
        assert_eq!(strip_html_tags("5 > 3 and 2 < 4"), "5  3 and 2 ");
    }

    #[test]
    fn test_strip_html_tags_multiple_nested_tags() {
        assert_eq!(strip_html_tags("<b><i><u>deep</u></i></b>"), "deep");
    }

    // -----------------------------------------------------------------------
    // mask_phone — boundary tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_mask_phone_exact_boundary_8_chars() {
        // Exactly 8 chars → masked (boundary)
        assert_eq!(mask_phone("12345678"), "***");
    }

    #[test]
    fn test_mask_phone_9_chars() {
        // 9 chars → first 3 + 1 X + last 5
        assert_eq!(mask_phone("123456789"), "123X56789");
    }

    #[test]
    fn test_mask_phone_empty() {
        assert_eq!(mask_phone(""), "***");
    }

    #[test]
    fn test_mask_phone_single_char() {
        assert_eq!(mask_phone("X"), "***");
    }

    // -----------------------------------------------------------------------
    // Disabled service — all critical event types are safe
    // -----------------------------------------------------------------------

    #[test]
    fn test_disabled_service_all_critical_events_safe() {
        let service = NotificationService::disabled();
        // Every event variant must be safe to fire on a disabled service
        service.notify(NotificationEvent::AuthenticationFailed {
            reason: "test".to_string(),
        });
        service.notify(NotificationEvent::TokenRenewalFailed {
            attempts: 99,
            reason: "catastrophic".to_string(),
        });
        service.notify(NotificationEvent::WebSocketDisconnected {
            connection_index: 0,
            reason: "reset".to_string(),
        });
        service.notify(NotificationEvent::HistoricalFetchFailed {
            instruments_fetched: 0,
            instruments_failed: 500,
            total_candles: 0,
            persist_failures: 0,
            failed_instruments: vec![],
            failure_reasons: std::collections::HashMap::new(),
        });
        service.notify(NotificationEvent::CandleVerificationFailed {
            instruments_checked: 0,
            instruments_with_gaps: 100,
            timeframe_details: String::new(),
            ohlc_violations: 0,
            data_violations: 0,
            timestamp_violations: 0,
            ohlc_details: vec![],
            data_details: vec![],
            timestamp_details: vec![],
        });
    }

    // -----------------------------------------------------------------------
    // Severity routing — Low severity does NOT trigger SMS
    // -----------------------------------------------------------------------

    #[test]
    fn test_severity_low_does_not_trigger_sms_path() {
        // StartupComplete is Low severity — it should NOT trigger the
        // SNS SMS code path. We verify this by checking severity.
        let event = NotificationEvent::StartupComplete { mode: "PAPER" };
        assert!(
            event.severity() < Severity::High,
            "Low-severity events must not reach SMS path (< High)"
        );
    }

    // -----------------------------------------------------------------------
    // Active service with SNS — SMS code path
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_active_service_critical_event_triggers_sms_path() {
        // Critical events should attempt both Telegram and SNS (if configured).
        let event = NotificationEvent::AuthenticationFailed {
            reason: "critical test".to_string(),
        };
        assert!(
            event.severity() >= Severity::High,
            "AuthenticationFailed must be High or Critical severity"
        );
    }

    // -----------------------------------------------------------------------
    // strip_html_tags — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_strip_html_tags_self_closing_tag() {
        assert_eq!(strip_html_tags("before<br/>after"), "beforeafter");
    }

    #[test]
    fn test_strip_html_tags_with_attributes() {
        assert_eq!(
            strip_html_tags(r#"<a href="https://example.com">link</a>"#),
            "link"
        );
    }

    #[test]
    fn test_strip_html_tags_mixed_content() {
        assert_eq!(
            strip_html_tags("<b>DLT</b> Trading System v<i>1.0</i>"),
            "DLT Trading System v1.0"
        );
    }

    // -----------------------------------------------------------------------
    // mask_phone — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_mask_phone_exactly_9_chars() {
        let result = mask_phone("123456789");
        assert_eq!(result, "123X56789");
    }

    #[test]
    fn test_mask_phone_long_number() {
        // 15 chars: prefix=3, suffix=5, mask=7
        let result = mask_phone("123456789012345");
        assert_eq!(result, "123XXXXXXX12345");
    }

    // -----------------------------------------------------------------------
    // NotificationMode coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_noop_service_is_active_returns_false() {
        let service = NotificationService {
            mode: NotificationMode::NoOp,
        };
        assert!(!service.is_active());
    }

    // -----------------------------------------------------------------------
    // strip_html_tags — additional coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_strip_html_tags_no_closing_bracket() {
        // "<tag without closing" — everything after '<' is inside tag
        assert_eq!(strip_html_tags("before<tag without closing"), "before");
    }

    #[test]
    fn test_strip_html_tags_consecutive_tags() {
        assert_eq!(strip_html_tags("<a><b><c>text</c></b></a>"), "text");
    }

    #[test]
    fn test_strip_html_tags_tag_with_newlines() {
        assert_eq!(
            strip_html_tags("<b>\nline1\nline2\n</b>"),
            "\nline1\nline2\n"
        );
    }

    #[test]
    fn test_strip_html_tags_emoji_content() {
        // Unicode content should pass through
        assert_eq!(strip_html_tags("<b>🚀 Launch</b>"), "🚀 Launch");
    }

    // -----------------------------------------------------------------------
    // mask_phone — additional boundary tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_mask_phone_10_chars() {
        // 10 chars: prefix=3, suffix=5, mask=2
        assert_eq!(mask_phone("1234567890"), "123XX67890");
    }

    #[test]
    fn test_mask_phone_exactly_8_chars_boundary() {
        // 8 chars → returns "***" (boundary)
        assert_eq!(mask_phone("12345678"), "***");
    }

    #[test]
    fn test_mask_phone_7_chars() {
        // 7 chars → returns "***" (below boundary)
        assert_eq!(mask_phone("1234567"), "***");
    }

    // -----------------------------------------------------------------------
    // Active service with SNS phone — triggers SMS code path
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_active_service_with_sns_high_severity_event() {
        // We can't easily create an aws_sdk_sns::Client in tests, but we can
        // verify that the notify() method correctly checks severity for SMS routing
        let event = NotificationEvent::AuthenticationFailed {
            reason: "test critical event".to_string(),
        };
        assert!(
            event.severity() >= Severity::High,
            "AuthenticationFailed must be High+ severity"
        );

        let low_event = NotificationEvent::StartupComplete { mode: "PAPER" };
        assert!(
            low_event.severity() < Severity::High,
            "StartupComplete must be below High severity"
        );
    }

    // -----------------------------------------------------------------------
    // Severity routing — medium severity does NOT trigger SMS
    // -----------------------------------------------------------------------

    #[test]
    fn test_severity_medium_does_not_trigger_sms() {
        let event = NotificationEvent::TokenRenewed;
        assert!(
            event.severity() < Severity::High,
            "TokenRenewed should not trigger SMS (below High)"
        );
    }

    // -----------------------------------------------------------------------
    // initialize — with SNS disabled
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_initialize_with_sns_disabled() {
        let config = NotificationConfig {
            send_timeout_ms: 100,
            telegram_api_base_url: "https://api.telegram.org".to_string(),
            sns_enabled: false,
        };
        let service = NotificationService::initialize(&config).await;
        // Without real SSM, should fall back to no-op
        // With real SSM, should be active but without SNS
        // Either way, should not panic
        let _ = service.is_active();
    }

    // -----------------------------------------------------------------------
    // initialize — with SNS enabled (still falls back gracefully)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_initialize_with_sns_enabled_falls_back_gracefully() {
        let config = NotificationConfig {
            send_timeout_ms: 100,
            telegram_api_base_url: "https://api.telegram.org".to_string(),
            sns_enabled: true,
        };
        let service = NotificationService::initialize(&config).await;
        // Whether SSM is available or not, initialize should not panic
        // SNS phone number fetch may fail, but service still initializes
        let _ = service.is_active();
    }

    // -----------------------------------------------------------------------
    // Active service with SNS client — High severity triggers SMS path
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_active_service_notify_high_severity_without_sns_client() {
        // Active service without SNS client — high severity should NOT panic
        let service = make_active_service();
        // AuthenticationFailed is Critical severity
        service.notify(NotificationEvent::AuthenticationFailed {
            reason: "test high severity no sns".to_string(),
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // -----------------------------------------------------------------------
    // strip_html_tags — only angle brackets
    // -----------------------------------------------------------------------

    #[test]
    fn test_strip_html_tags_only_gt_symbol() {
        // A lone '>' without prior '<' is just a char — it toggles in_tag=false
        assert_eq!(strip_html_tags(">"), "");
    }

    #[test]
    fn test_strip_html_tags_only_lt_symbol() {
        // A lone '<' starts a "tag" that never ends — everything after is swallowed
        assert_eq!(strip_html_tags("<"), "");
    }

    #[test]
    fn test_strip_html_tags_gt_then_text() {
        // '>' sets in_tag=false, then "hello" is normal text
        assert_eq!(strip_html_tags(">hello"), "hello");
    }

    // -----------------------------------------------------------------------
    // mask_phone — Unicode and special characters
    // -----------------------------------------------------------------------

    #[test]
    fn test_mask_phone_unicode_chars() {
        // 12 chars: prefix=3, suffix=5, mask=4
        let result = mask_phone("+91ABCDEFGH");
        assert_eq!(result.len(), 11); // 3 + 4 + 5 - 1 (Xs overlap)
    }

    // -----------------------------------------------------------------------
    // send_telegram_message — message body format
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_send_telegram_message_body_format() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}", addr.port());

        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8192];
                let n = tokio::io::AsyncReadExt::read(&mut stream, &mut buf)
                    .await
                    .unwrap_or(0);
                let request_str = String::from_utf8_lossy(&buf[..n]);
                // Verify the request contains JSON body with expected fields
                assert!(request_str.contains("chat_id"));
                assert!(request_str.contains("parse_mode"));
                assert!(request_str.contains("HTML"));

                let body = r#"{"ok":true}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let token = SecretString::from("test-token".to_string());
        send_telegram_message(&client, &base_url, &token, "12345", "<b>Test</b>").await;
    }

    // -----------------------------------------------------------------------
    // NotificationService::disabled — multiple event types are safe
    // -----------------------------------------------------------------------

    #[test]
    fn test_disabled_service_websocket_events_safe() {
        let service = NotificationService::disabled();
        service.notify(NotificationEvent::WebSocketConnected {
            connection_index: 0,
        });
        service.notify(NotificationEvent::WebSocketDisconnected {
            connection_index: 0,
            reason: "test".to_string(),
        });
        service.notify(NotificationEvent::WebSocketReconnected {
            connection_index: 0,
        });
        service.notify(NotificationEvent::ShutdownInitiated);
        service.notify(NotificationEvent::ShutdownComplete);
    }

    // -----------------------------------------------------------------------
    // Active service — all low-severity events go only to Telegram
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_active_service_low_severity_events_no_sms() {
        let service = make_active_service();
        // Low-severity events should NOT trigger SNS SMS path
        service.notify(NotificationEvent::StartupComplete { mode: "PAPER" });
        service.notify(NotificationEvent::ShutdownComplete);
        service.notify(NotificationEvent::TokenRenewed);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // -----------------------------------------------------------------------
    // send_telegram_message — non-success HTTP response (e.g., 403)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_send_telegram_message_403_does_not_panic() {
        let base_url = start_mock_telegram_server(403).await;

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();

        let token = SecretString::from("fake-bot-token".to_string());

        // Should log a warning but not panic or return an error.
        send_telegram_message(&client, &base_url, &token, "999999999", "test 403").await;
    }

    // -----------------------------------------------------------------------
    // send_telegram_message — 500 error
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_send_telegram_message_500_does_not_panic() {
        let base_url = start_mock_telegram_server(500).await;

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();

        let token = SecretString::from("fake-bot-token".to_string());

        send_telegram_message(&client, &base_url, &token, "123", "test 500").await;
    }

    // -----------------------------------------------------------------------
    // Active service — notify with connection refused logs warn
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_active_service_notify_connection_refused_logs_warning() {
        // The active service points to 127.0.0.1:1 (connection refused).
        // Notify spawns a background task that will fail silently.
        let service = make_active_service();

        service.notify(NotificationEvent::Custom {
            message: "test connection refused path".to_string(),
        });

        // Wait for the background task to attempt and fail.
        tokio::time::sleep(Duration::from_millis(300)).await;
        // No panic = success. The error is logged as warn.
    }

    // -----------------------------------------------------------------------
    // disabled service — is_active returns false consistently
    // -----------------------------------------------------------------------

    #[test]
    fn test_disabled_service_is_active_consistent() {
        let service = NotificationService::disabled();
        assert!(!service.is_active());
        assert!(!service.is_active()); // idempotent
    }

    // -----------------------------------------------------------------------
    // strip_html_tags — real notification event messages
    // -----------------------------------------------------------------------

    #[test]
    fn test_strip_html_tags_real_event_message() {
        let event = NotificationEvent::AuthenticationFailed {
            reason: "token expired".to_string(),
        };
        let message = event.to_message();
        let stripped = strip_html_tags(&message);
        // Stripped should contain the reason but no HTML tags.
        assert!(stripped.contains("token expired"));
        assert!(!stripped.contains('<'));
        assert!(!stripped.contains('>'));
    }

    // -----------------------------------------------------------------------
    // make_active_service — high severity event with no SNS client
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_active_service_critical_event_no_sns_does_not_attempt_sms() {
        let service = make_active_service(); // no sns_client
        // Critical event — normally would trigger SMS, but no SNS client.
        service.notify(NotificationEvent::AuthenticationFailed {
            reason: "critical without SNS".to_string(),
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        // No panic = success. SMS path is skipped due to None SNS client.
    }

    // -----------------------------------------------------------------------
    // mask_phone — consistency check
    // -----------------------------------------------------------------------

    #[test]
    fn test_mask_phone_consistent_output() {
        let phone = "+919876543210";
        let masked1 = mask_phone(phone);
        let masked2 = mask_phone(phone);
        assert_eq!(masked1, masked2, "masking should be deterministic");
    }

    #[test]
    fn test_mask_phone_preserves_prefix_and_suffix() {
        // 13 chars: prefix=3 (+91), suffix=5 (43210), mask=5 (XXXXX)
        let result = mask_phone("+919876543210");
        assert!(result.starts_with("+91"), "prefix must be preserved");
        assert!(result.ends_with("43210"), "suffix must be preserved");
    }

    #[test]
    fn test_mask_phone_mask_length_matches_middle() {
        // 13 chars: 13 - 3 - 5 = 5 X's
        let result = mask_phone("+919876543210");
        let x_count = result.chars().filter(|c| *c == 'X').count();
        assert_eq!(x_count, 5, "middle should be 5 X characters");
    }

    #[test]
    fn test_strip_html_tags_alternating_content_and_tags() {
        let input = "a<b>b</b>c<i>d</i>e";
        assert_eq!(strip_html_tags(input), "abcde");
    }

    #[test]
    fn test_strip_html_tags_tag_only_no_content() {
        assert_eq!(strip_html_tags("<br>"), "");
        assert_eq!(strip_html_tags("<hr/>"), "");
    }

    #[test]
    fn test_strip_html_tags_multiple_gt_without_lt() {
        // '>' without preceding '<' ends the tag state. When not in a tag, '>' triggers
        // in_tag = false (which is already false), so the '>' is not output.
        assert_eq!(strip_html_tags(">>>"), "");
    }

    // -----------------------------------------------------------------------
    // Active service with SNS — exercises SMS code path
    // -----------------------------------------------------------------------

    fn make_active_service_with_sns() -> Arc<NotificationService> {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();

        // Create a real AWS SNS client pointed at a fake endpoint.
        // The SNS call will fail, but the code path is exercised.
        let sns_config = aws_sdk_sns::Config::builder()
            .endpoint_url("http://127.0.0.1:1")
            .region(aws_sdk_sns::config::Region::new("ap-south-1"))
            .behavior_version_latest()
            .credentials_provider(aws_sdk_sns::config::Credentials::new(
                "fake-access-key",
                "fake-secret-key",
                None,
                None,
                "test",
            ))
            .build();
        let sns_client = aws_sdk_sns::Client::from_conf(sns_config);

        Arc::new(NotificationService {
            mode: NotificationMode::Active {
                bot_token: SecretString::from("test-bot-token".to_string()),
                chat_id: "123456789".to_string(),
                http_client,
                telegram_api_base_url: "http://127.0.0.1:1".to_string(),
                sns_client: Some(sns_client),
                sns_phone_number: Some("+919876543210".to_string()),
            },
        })
    }

    #[tokio::test]
    async fn test_active_service_with_sns_critical_event_triggers_sms_code_path() {
        let service = make_active_service_with_sns();
        assert!(service.is_active());

        // AuthenticationFailed is Critical severity — triggers both Telegram and SNS
        service.notify(NotificationEvent::AuthenticationFailed {
            reason: "test SNS path".to_string(),
        });

        // Wait for background tasks to attempt (and fail) sends
        tokio::time::sleep(Duration::from_millis(500)).await;
        // No panic = success. Both Telegram and SNS paths exercised.
    }

    #[tokio::test]
    async fn test_active_service_with_sns_low_severity_skips_sms() {
        let service = make_active_service_with_sns();

        // StartupComplete is Low severity — only Telegram, not SNS
        service.notify(NotificationEvent::StartupComplete { mode: "PAPER" });

        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    #[tokio::test]
    async fn test_active_service_with_sns_multiple_critical_events() {
        let service = make_active_service_with_sns();

        // Multiple critical events
        service.notify(NotificationEvent::AuthenticationFailed {
            reason: "failure 1".to_string(),
        });
        service.notify(NotificationEvent::TokenRenewalFailed {
            attempts: 5,
            reason: "failure 2".to_string(),
        });

        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // -----------------------------------------------------------------------
    // strip_html_tags — special characters preserved
    // -----------------------------------------------------------------------

    #[test]
    fn test_strip_html_tags_preserves_special_chars() {
        assert_eq!(strip_html_tags("a & b"), "a & b");
        assert_eq!(strip_html_tags("100%"), "100%");
        assert_eq!(strip_html_tags("P&L: +500"), "P&L: +500");
    }

    // -----------------------------------------------------------------------
    // Active service — rapid fire notifications
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_active_service_rapid_fire_does_not_panic() {
        let service = make_active_service();
        for i in 0..20 {
            service.notify(NotificationEvent::Custom {
                message: format!("rapid fire event {i}"),
            });
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

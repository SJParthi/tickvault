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
    let keep_prefix = 3;
    let keep_suffix = 5;
    let mask_len = phone.len().saturating_sub(keep_prefix + keep_suffix);
    format!(
        "{}{}{}",
        &phone[..keep_prefix],
        "X".repeat(mask_len),
        &phone[phone.len() - keep_suffix..]
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
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

    #[test]
    fn test_disabled_service_handles_critical_events() {
        let service = NotificationService::disabled();
        // Critical events on no-op service must not panic (no SNS client).
        service.notify(NotificationEvent::AuthenticationFailed {
            reason: "timeout".to_string(),
        });
        service.notify(NotificationEvent::TokenRenewalFailed {
            attempts: 3,
            reason: "timeout".to_string(),
        });
    }

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
}

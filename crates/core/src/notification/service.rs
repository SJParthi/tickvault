//! Telegram notification service.
//!
//! ONE source (AWS SSM), ONE code path, everywhere:
//! - Rust app via `NotificationService` → same SSM → same Telegram
//! - IntelliJ runs same binary → same SSM → same Telegram
//! - Claude Code sessions use shell script → same SSM → same Telegram
//! - CI/CD uses IAM role → same SSM → same Telegram
//!
//! # Failure Behavior
//!
//! SSM fetch fails at boot → `NotificationService` is created in no-op mode.
//! HTTP send fails → logged as `warn`, not propagated. Never blocks caller.
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
    SSM_TELEGRAM_SERVICE, TELEGRAM_BOT_TOKEN_SECRET, TELEGRAM_CHAT_ID_SECRET,
};

use crate::auth::secret_manager::{
    build_ssm_path, create_ssm_client, fetch_secret, resolve_environment,
};

use super::events::NotificationEvent;

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
    },
    /// SSM unavailable at boot — all sends are silent no-ops.
    NoOp,
}

// ---------------------------------------------------------------------------
// Public Service
// ---------------------------------------------------------------------------

/// Fire-and-forget Telegram notification service.
///
/// Create once at boot via `NotificationService::initialize`. Pass `Arc<Self>`
/// to any component that needs alerting. Call `notify` to send — it spawns
/// a background task and returns immediately.
pub struct NotificationService {
    mode: NotificationMode,
}

impl NotificationService {
    /// Initializes the notification service by fetching Telegram credentials from SSM.
    ///
    /// If SSM is unavailable (no IAM role, tokens not in SSM, etc.), returns
    /// a no-op service. The caller is never blocked and boot continues normally.
    /// In dev: Telegram tokens in LocalStack SSM → active mode (if real tokens seeded).
    /// In prod: Telegram tokens in real AWS SSM → active mode.
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

        info!(
            chat_id = %chat_id,
            "notification service initialized — Telegram active"
        );

        Arc::new(Self {
            mode: NotificationMode::Active {
                bot_token,
                chat_id,
                http_client,
                telegram_api_base_url: config.telegram_api_base_url.clone(),
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

    /// Sends a notification event. Spawns a background task — returns immediately.
    ///
    /// If the service is in no-op mode, this is a zero-cost return.
    /// If the HTTP send fails, logs `warn` and discards the error.
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
            } => {
                let message = event.to_message();
                let token = bot_token.clone();
                let chat_id = chat_id.clone();
                let client = http_client.clone();
                let base_url = telegram_api_base_url.clone();

                tokio::spawn(async move {
                    send_telegram_message(&client, &base_url, &token, &chat_id, &message).await;
                });
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
}

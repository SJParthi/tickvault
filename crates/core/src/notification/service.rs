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

    // -----------------------------------------------------------------------
    // Active mode tests
    // -----------------------------------------------------------------------

    /// Helper: creates an Active-mode service with a deliberately unreachable
    /// base URL. The service is "active" (credentials loaded) but any HTTP
    /// send will fail with a connection error — safe for unit tests.
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
        // Active-mode notify spawns a tokio task that will fail on HTTP send.
        // The key invariant: notify() itself must not panic, and the spawned
        // task must not propagate the error (it logs warn and discards).
        let service = make_active_service();

        service.notify(NotificationEvent::StartupComplete { mode: "LIVE" });
        service.notify(NotificationEvent::ShutdownComplete);
        service.notify(NotificationEvent::AuthenticationSuccess);
        service.notify(NotificationEvent::Custom {
            message: "active-mode test".to_string(),
        });

        // Give spawned tasks time to complete (they will fail on connect).
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // -----------------------------------------------------------------------
    // send_telegram_message — error path tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_send_telegram_message_connection_refused_does_not_panic() {
        // Unreachable host — connection refused. Must log warn, not panic.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();

        let token = SecretString::from("fake-bot-token".to_string());

        // This will fail with a connection error — the function must absorb it.
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
        // Completely invalid URL — should fail at the HTTP layer.
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
        // Each is an independent Arc — different pointers.
        assert!(!Arc::ptr_eq(&s1, &s2));
    }

    // -----------------------------------------------------------------------
    // send_telegram_message — non-success HTTP status path
    // -----------------------------------------------------------------------

    /// Helper: starts a minimal HTTP server returning a specific status code.
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
        // Server returns 401 — the function must log warn, not panic.
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
        // Server returns 200 — the function should log debug.
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

        // Give spawned tasks time to complete (they will fail on connect).
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    // -----------------------------------------------------------------------
    // initialize — falls back to no-op without real SSM
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_initialize_returns_noop_without_ssm() {
        // Without real AWS SSM, initialize should return a no-op service.
        let config = NotificationConfig {
            send_timeout_ms: 100,
            telegram_api_base_url: "https://api.telegram.org".to_string(),
        };
        let service = NotificationService::initialize(&config).await;
        // Without SSM, it should fall back to no-op mode.
        assert!(
            !service.is_active(),
            "without real SSM, service should be in no-op mode"
        );
    }
}

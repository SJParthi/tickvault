//! Typed notification events for structured Telegram messages.
//!
//! Every system event that should produce a Telegram alert is represented
//! here. Callers pass events to `NotificationService::notify` — message
//! formatting lives in this module, not at callsites.

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
                format!("<b>dhan-live-trader started</b>\nMode: {mode}")
            }
            Self::AuthenticationSuccess => "<b>Auth OK</b> — Dhan JWT acquired".to_string(),
            Self::AuthenticationFailed { reason } => {
                format!("<b>Auth FAILED</b> — offline mode\n{reason}")
            }
            Self::TokenRenewed => "<b>Token renewed</b>".to_string(),
            Self::TokenRenewalFailed { attempts, reason } => {
                format!("<b>Token renewal FAILED</b> (attempt {attempts})\n{reason}")
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
            Self::ShutdownInitiated => "<b>Shutdown initiated</b>".to_string(),
            Self::ShutdownComplete => "<b>dhan-live-trader stopped</b>".to_string(),
            Self::Custom { message } => message.clone(),
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
    fn test_startup_complete_live_message() {
        let event = NotificationEvent::StartupComplete { mode: "LIVE" };
        let msg = event.to_message();
        assert!(msg.contains("LIVE"));
        assert!(msg.contains("started"));
    }

    #[test]
    fn test_startup_complete_offline_message() {
        let event = NotificationEvent::StartupComplete { mode: "OFFLINE" };
        let msg = event.to_message();
        assert!(msg.contains("OFFLINE"));
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
}

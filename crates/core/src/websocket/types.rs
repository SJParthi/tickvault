//! WebSocket domain types for Dhan Live Market Feed.
//!
//! Covers connection state, disconnect codes, subscription payloads,
//! and WebSocket-specific error variants.

use std::fmt;

use dhan_live_trader_common::constants::{
    DISCONNECT_ACCESS_TOKEN_EXPIRED, DISCONNECT_AUTH_FAILED,
    DISCONNECT_DATA_API_SUBSCRIPTION_REQUIRED, DISCONNECT_EXCEEDED_ACTIVE_CONNECTIONS,
    DISCONNECT_INVALID_CLIENT_ID,
};
use dhan_live_trader_common::types::ExchangeSegment;
use serde::{Deserialize, Serialize};

/// Unique identifier for a WebSocket connection within the pool (0–4).
pub type ConnectionId = u8;

// ---------------------------------------------------------------------------
// Connection State
// ---------------------------------------------------------------------------

/// State machine for a single WebSocket connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// Not connected. Initial state or after clean disconnect.
    Disconnected,
    /// TCP + TLS handshake in progress.
    Connecting,
    /// WebSocket established, subscriptions active, receiving data.
    Connected,
    /// Lost connection, attempting to reconnect with backoff.
    Reconnecting,
}

impl fmt::Display for ConnectionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disconnected => f.write_str("Disconnected"),
            Self::Connecting => f.write_str("Connecting"),
            Self::Connected => f.write_str("Connected"),
            Self::Reconnecting => f.write_str("Reconnecting"),
        }
    }
}

// ---------------------------------------------------------------------------
// Disconnect Code
// ---------------------------------------------------------------------------

/// Dhan WebSocket disconnect error codes (805–809).
///
/// Source: DhanHQ Python SDK v2 on_close handler.
/// Each code maps to a specific recovery action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectCode {
    /// 805 — Active WebSocket connections exceeded (max 5 per account).
    ExceededActiveConnections,
    /// 806 — Data API subscription required (plan/subscription issue).
    DataApiSubscriptionRequired,
    /// 807 — Access token expired. Refresh token, then reconnect.
    AccessTokenExpired,
    /// 808 — Invalid client ID. Check SSM credentials.
    InvalidClientId,
    /// 809 — Authentication failed. Invalid credentials.
    AuthenticationFailed,
    /// Unknown disconnect code not in the Dhan V2 SDK.
    Unknown(u16),
}

impl DisconnectCode {
    /// Parse a disconnect code from the raw u16 value in the binary frame.
    pub fn from_u16(code: u16) -> Self {
        match code {
            DISCONNECT_EXCEEDED_ACTIVE_CONNECTIONS => Self::ExceededActiveConnections,
            DISCONNECT_DATA_API_SUBSCRIPTION_REQUIRED => Self::DataApiSubscriptionRequired,
            DISCONNECT_ACCESS_TOKEN_EXPIRED => Self::AccessTokenExpired,
            DISCONNECT_INVALID_CLIENT_ID => Self::InvalidClientId,
            DISCONNECT_AUTH_FAILED => Self::AuthenticationFailed,
            other => Self::Unknown(other),
        }
    }

    /// Returns the raw u16 code value.
    pub fn as_u16(&self) -> u16 {
        match self {
            Self::ExceededActiveConnections => DISCONNECT_EXCEEDED_ACTIVE_CONNECTIONS,
            Self::DataApiSubscriptionRequired => DISCONNECT_DATA_API_SUBSCRIPTION_REQUIRED,
            Self::AccessTokenExpired => DISCONNECT_ACCESS_TOKEN_EXPIRED,
            Self::InvalidClientId => DISCONNECT_INVALID_CLIENT_ID,
            Self::AuthenticationFailed => DISCONNECT_AUTH_FAILED,
            Self::Unknown(code) => *code,
        }
    }

    /// Whether this disconnect code allows automatic reconnection.
    ///
    /// Only token-expired (807) is auto-reconnectable after refresh.
    /// All others indicate configuration/credential/plan issues.
    pub fn is_reconnectable(&self) -> bool {
        match self {
            Self::AccessTokenExpired => true,
            Self::ExceededActiveConnections
            | Self::DataApiSubscriptionRequired
            | Self::InvalidClientId
            | Self::AuthenticationFailed => false,
            Self::Unknown(_) => true, // assume transient for unknown codes
        }
    }

    /// Whether this disconnect code requires a token refresh before reconnect.
    pub fn requires_token_refresh(&self) -> bool {
        matches!(self, Self::AccessTokenExpired)
    }
}

impl fmt::Display for DisconnectCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExceededActiveConnections => {
                write!(f, "805: Active connections exceeded")
            }
            Self::DataApiSubscriptionRequired => {
                write!(f, "806: Data API subscription required")
            }
            Self::AccessTokenExpired => write!(f, "807: Access token expired"),
            Self::InvalidClientId => write!(f, "808: Invalid client ID"),
            Self::AuthenticationFailed => write!(f, "809: Authentication failed"),
            Self::Unknown(code) => write!(f, "{code}: Unknown disconnect"),
        }
    }
}

// ---------------------------------------------------------------------------
// Subscription Types
// ---------------------------------------------------------------------------

/// A single instrument subscription entry for the JSON request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstrumentSubscription {
    /// Exchange segment string (e.g., "NSE_FNO", "IDX_I").
    #[serde(rename = "ExchangeSegment")]
    pub exchange_segment: String,
    /// Security ID as string (Dhan requires string in JSON).
    #[serde(rename = "SecurityId")]
    pub security_id: String,
}

impl InstrumentSubscription {
    /// Creates a new subscription entry from typed values.
    pub fn new(segment: ExchangeSegment, security_id: u32) -> Self {
        Self {
            exchange_segment: segment.as_str().to_string(),
            security_id: security_id.to_string(),
        }
    }
}

/// JSON subscription request sent to Dhan WebSocket after connection.
#[derive(Debug, Serialize, Deserialize)]
pub struct SubscriptionRequest {
    /// Feed request code: 15 (Ticker), 17 (Quote), 21 (Full), 12 (Unsubscribe).
    #[serde(rename = "RequestCode")]
    pub request_code: u8,
    /// Number of instruments in this message.
    #[serde(rename = "InstrumentCount")]
    pub instrument_count: usize,
    /// List of instruments to subscribe/unsubscribe.
    #[serde(rename = "InstrumentList")]
    pub instrument_list: Vec<InstrumentSubscription>,
}

// ---------------------------------------------------------------------------
// WebSocket Error
// ---------------------------------------------------------------------------

/// Errors specific to WebSocket connection management.
#[derive(Debug, thiserror::Error)]
pub enum WebSocketError {
    /// WebSocket connection failed.
    #[error("WebSocket connection failed to {url}: {source}")]
    ConnectionFailed {
        url: String,
        source: tokio_tungstenite::tungstenite::Error,
    },

    /// No valid token available for authentication.
    #[error("No valid access token available for WebSocket authentication")]
    NoTokenAvailable,

    /// Dhan sent a disconnect frame with an error code.
    #[error("Dhan disconnected: {code}")]
    DhanDisconnect { code: DisconnectCode },

    /// Non-reconnectable disconnect — configuration or credential error.
    #[error("Non-reconnectable disconnect: {code}")]
    NonReconnectableDisconnect { code: DisconnectCode },

    /// All reconnection attempts exhausted.
    #[error("Reconnection failed after {attempts} attempts for connection {connection_id}")]
    ReconnectionExhausted {
        connection_id: ConnectionId,
        attempts: u32,
    },

    /// Subscription message could not be sent.
    #[error("Failed to send subscription on connection {connection_id}: {reason}")]
    SubscriptionFailed {
        connection_id: ConnectionId,
        reason: String,
    },

    /// Capacity exceeded — too many instruments for available connections.
    #[error("Instrument count {requested} exceeds total capacity {capacity}")]
    CapacityExceeded { requested: usize, capacity: usize },

    /// TLS configuration failed.
    #[error("TLS configuration failed: {reason}")]
    TlsConfigurationFailed { reason: String },
}

// ---------------------------------------------------------------------------
// Connection Health
// ---------------------------------------------------------------------------

/// Health snapshot for a single WebSocket connection.
#[derive(Debug, Clone)]
pub struct ConnectionHealth {
    /// Connection identifier (0–4).
    pub connection_id: ConnectionId,
    /// Current state.
    pub state: ConnectionState,
    /// Number of instruments subscribed on this connection.
    pub subscribed_count: usize,
    /// Total reconnections since startup.
    pub total_reconnections: u64,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- DisconnectCode ---

    #[test]
    fn test_disconnect_code_from_u16_all_known_codes() {
        assert_eq!(
            DisconnectCode::from_u16(805),
            DisconnectCode::ExceededActiveConnections
        );
        assert_eq!(
            DisconnectCode::from_u16(806),
            DisconnectCode::DataApiSubscriptionRequired
        );
        assert_eq!(
            DisconnectCode::from_u16(807),
            DisconnectCode::AccessTokenExpired
        );
        assert_eq!(
            DisconnectCode::from_u16(808),
            DisconnectCode::InvalidClientId
        );
        assert_eq!(
            DisconnectCode::from_u16(809),
            DisconnectCode::AuthenticationFailed
        );
    }

    #[test]
    fn test_disconnect_code_unknown_value() {
        assert_eq!(DisconnectCode::from_u16(999), DisconnectCode::Unknown(999));
        assert_eq!(DisconnectCode::from_u16(0), DisconnectCode::Unknown(0));
    }

    #[test]
    fn test_disconnect_code_roundtrip() {
        let codes: &[u16] = &[805, 806, 807, 808, 809];
        for &code in codes {
            let parsed = DisconnectCode::from_u16(code);
            assert_eq!(parsed.as_u16(), code, "roundtrip failed for code {code}");
        }
    }

    #[test]
    fn test_disconnect_code_reconnectable() {
        // Reconnectable: only 807 (token expired)
        assert!(DisconnectCode::AccessTokenExpired.is_reconnectable());

        // NOT reconnectable: 805, 806, 808, 809
        assert!(!DisconnectCode::ExceededActiveConnections.is_reconnectable());
        assert!(!DisconnectCode::DataApiSubscriptionRequired.is_reconnectable());
        assert!(!DisconnectCode::InvalidClientId.is_reconnectable());
        assert!(!DisconnectCode::AuthenticationFailed.is_reconnectable());

        // Unknown: assume reconnectable (transient)
        assert!(DisconnectCode::Unknown(999).is_reconnectable());
    }

    #[test]
    fn test_disconnect_code_requires_token_refresh() {
        assert!(DisconnectCode::AccessTokenExpired.requires_token_refresh());

        assert!(!DisconnectCode::ExceededActiveConnections.requires_token_refresh());
        assert!(!DisconnectCode::DataApiSubscriptionRequired.requires_token_refresh());
        assert!(!DisconnectCode::InvalidClientId.requires_token_refresh());
        assert!(!DisconnectCode::AuthenticationFailed.requires_token_refresh());
        assert!(!DisconnectCode::Unknown(999).requires_token_refresh());
    }

    #[test]
    fn test_disconnect_code_display() {
        assert_eq!(
            DisconnectCode::AuthenticationFailed.to_string(),
            "809: Authentication failed"
        );
        assert_eq!(
            DisconnectCode::Unknown(999).to_string(),
            "999: Unknown disconnect"
        );
    }

    // --- ConnectionState ---

    #[test]
    fn test_connection_state_display() {
        assert_eq!(ConnectionState::Disconnected.to_string(), "Disconnected");
        assert_eq!(ConnectionState::Connecting.to_string(), "Connecting");
        assert_eq!(ConnectionState::Connected.to_string(), "Connected");
        assert_eq!(ConnectionState::Reconnecting.to_string(), "Reconnecting");
    }

    #[test]
    fn test_connection_state_equality() {
        assert_eq!(ConnectionState::Connected, ConnectionState::Connected);
        assert_ne!(ConnectionState::Connected, ConnectionState::Disconnected);
    }

    #[test]
    fn test_connection_state_clone_and_copy() {
        let state = ConnectionState::Connected;
        let cloned = state.clone();
        let copied = state; // Copy trait
        assert_eq!(state, cloned);
        assert_eq!(state, copied);
    }

    // --- DisconnectCode (additional) ---

    #[test]
    fn test_disconnect_code_clone_and_copy() {
        let code = DisconnectCode::AccessTokenExpired;
        let cloned = code.clone();
        let copied = code; // Copy trait
        assert_eq!(code, cloned);
        assert_eq!(code, copied);
    }

    #[test]
    fn test_disconnect_code_display_all_known() {
        assert!(
            DisconnectCode::ExceededActiveConnections
                .to_string()
                .contains("805")
        );
        assert!(
            DisconnectCode::DataApiSubscriptionRequired
                .to_string()
                .contains("806")
        );
        assert!(
            DisconnectCode::AccessTokenExpired
                .to_string()
                .contains("807")
        );
        assert!(DisconnectCode::InvalidClientId.to_string().contains("808"));
        assert!(
            DisconnectCode::AuthenticationFailed
                .to_string()
                .contains("809")
        );
    }

    #[test]
    fn test_disconnect_code_unknown_zero_roundtrip() {
        let code = DisconnectCode::from_u16(0);
        assert_eq!(code, DisconnectCode::Unknown(0));
        assert_eq!(code.as_u16(), 0);
        assert!(code.is_reconnectable()); // unknown = assume transient
    }

    #[test]
    fn test_disconnect_code_unknown_u16_max_roundtrip() {
        let code = DisconnectCode::from_u16(u16::MAX);
        assert_eq!(code, DisconnectCode::Unknown(u16::MAX));
        assert_eq!(code.as_u16(), u16::MAX);
    }

    // --- InstrumentSubscription ---

    #[test]
    fn test_instrument_subscription_new() {
        let sub = InstrumentSubscription::new(ExchangeSegment::NseFno, 52432);
        assert_eq!(sub.exchange_segment, "NSE_FNO");
        assert_eq!(sub.security_id, "52432");
    }

    #[test]
    fn test_instrument_subscription_idx_i() {
        let sub = InstrumentSubscription::new(ExchangeSegment::IdxI, 13);
        assert_eq!(sub.exchange_segment, "IDX_I");
        assert_eq!(sub.security_id, "13");
    }

    #[test]
    fn test_instrument_subscription_bse_fno_segment() {
        let sub = InstrumentSubscription::new(ExchangeSegment::BseFno, 99999);
        assert_eq!(sub.exchange_segment, "BSE_FNO");
        assert_eq!(sub.security_id, "99999");
    }

    #[test]
    fn test_instrument_subscription_nse_equity_segment() {
        let sub = InstrumentSubscription::new(ExchangeSegment::NseEquity, 2885);
        assert_eq!(sub.exchange_segment, "NSE_EQ");
        assert_eq!(sub.security_id, "2885");
    }

    #[test]
    fn test_instrument_subscription_serialize_json() {
        let sub = InstrumentSubscription::new(ExchangeSegment::NseFno, 52432);
        let json = serde_json::to_string(&sub).unwrap();
        assert!(json.contains("\"ExchangeSegment\":\"NSE_FNO\""));
        assert!(json.contains("\"SecurityId\":\"52432\""));
    }

    #[test]
    fn test_instrument_subscription_deserialize_roundtrip() {
        let sub = InstrumentSubscription::new(ExchangeSegment::IdxI, 13);
        let json = serde_json::to_string(&sub).unwrap();
        let deserialized: InstrumentSubscription = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.exchange_segment, "IDX_I");
        assert_eq!(deserialized.security_id, "13");
    }

    // --- SubscriptionRequest ---

    #[test]
    fn test_subscription_request_serialize() {
        let request = SubscriptionRequest {
            request_code: 15,
            instrument_count: 2,
            instrument_list: vec![
                InstrumentSubscription::new(ExchangeSegment::IdxI, 13),
                InstrumentSubscription::new(ExchangeSegment::NseFno, 52432),
            ],
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"RequestCode\":15"));
        assert!(json.contains("\"InstrumentCount\":2"));
        assert!(json.contains("\"InstrumentList\""));
    }

    #[test]
    fn test_subscription_request_deserialize_roundtrip() {
        let request = SubscriptionRequest {
            request_code: 17,
            instrument_count: 1,
            instrument_list: vec![InstrumentSubscription::new(ExchangeSegment::NseFno, 5000)],
        };
        let json = serde_json::to_string(&request).unwrap();
        let deserialized: SubscriptionRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.request_code, 17);
        assert_eq!(deserialized.instrument_count, 1);
        assert_eq!(deserialized.instrument_list.len(), 1);
        assert_eq!(deserialized.instrument_list[0].security_id, "5000");
    }

    #[test]
    fn test_subscription_request_security_id_is_string_in_json() {
        let request = SubscriptionRequest {
            request_code: 21,
            instrument_count: 1,
            instrument_list: vec![InstrumentSubscription::new(
                ExchangeSegment::NseEquity,
                2885,
            )],
        };
        let json = serde_json::to_string(&request).unwrap();
        // SecurityId MUST be a string in JSON, not a number
        assert!(json.contains("\"SecurityId\":\"2885\""));
        assert!(!json.contains("\"SecurityId\":2885"));
    }

    // --- WebSocketError ---

    #[test]
    fn test_websocket_error_no_token_display() {
        let err = WebSocketError::NoTokenAvailable;
        assert!(err.to_string().contains("No valid access token"));
    }

    #[test]
    fn test_websocket_error_capacity_exceeded_display() {
        let err = WebSocketError::CapacityExceeded {
            requested: 30000,
            capacity: 25000,
        };
        assert!(err.to_string().contains("30000"));
        assert!(err.to_string().contains("25000"));
    }

    #[test]
    fn test_websocket_error_reconnection_exhausted_display() {
        let err = WebSocketError::ReconnectionExhausted {
            connection_id: 2,
            attempts: 10,
        };
        assert!(err.to_string().contains("10 attempts"));
        assert!(err.to_string().contains("connection 2"));
    }

    #[test]
    fn test_websocket_error_connection_failed_display() {
        let err = WebSocketError::ConnectionFailed {
            url: "wss://api-feed.dhan.co".to_string(),
            source: tokio_tungstenite::tungstenite::Error::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "connection timed out",
            )),
        };
        assert!(err.to_string().contains("wss://api-feed.dhan.co"));
    }

    #[test]
    fn test_websocket_error_dhan_disconnect_display() {
        let err = WebSocketError::DhanDisconnect {
            code: DisconnectCode::AccessTokenExpired,
        };
        assert!(err.to_string().contains("807"));
    }

    #[test]
    fn test_websocket_error_non_reconnectable_display() {
        let err = WebSocketError::NonReconnectableDisconnect {
            code: DisconnectCode::InvalidClientId,
        };
        assert!(err.to_string().contains("808"));
    }

    #[test]
    fn test_websocket_error_subscription_failed_display() {
        let err = WebSocketError::SubscriptionFailed {
            connection_id: 1,
            reason: "write error".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("connection 1"));
        assert!(msg.contains("write error"));
    }

    #[test]
    fn test_websocket_error_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        // This compiles only if WebSocketError implements Send + Sync
        // (required for tokio::spawn across await points)
        assert_send_sync::<WebSocketError>();
    }

    // --- ConnectionHealth ---

    #[test]
    fn test_connection_health_clone() {
        let health = ConnectionHealth {
            connection_id: 2,
            state: ConnectionState::Connected,
            subscribed_count: 500,
            total_reconnections: 3,
        };
        let cloned = health.clone();
        assert_eq!(cloned.connection_id, 2);
        assert_eq!(cloned.state, ConnectionState::Connected);
        assert_eq!(cloned.subscribed_count, 500);
        assert_eq!(cloned.total_reconnections, 3);
    }

    #[test]
    fn test_connection_health_default_values() {
        let health = ConnectionHealth {
            connection_id: 0,
            state: ConnectionState::Disconnected,
            subscribed_count: 0,
            total_reconnections: 0,
        };
        assert_eq!(health.connection_id, 0);
        assert_eq!(health.state, ConnectionState::Disconnected);
        assert_eq!(health.subscribed_count, 0);
    }
}

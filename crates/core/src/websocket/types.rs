//! WebSocket domain types for Dhan Live Market Feed.
//!
//! Covers connection state, disconnect codes, subscription payloads,
//! and WebSocket-specific error variants.

use std::fmt;

use dhan_live_trader_common::constants::{
    DISCONNECT_AUTH_FAILED, DISCONNECT_EXCEEDED_MAX_CONNECTIONS,
    DISCONNECT_EXCEEDED_MAX_INSTRUMENTS, DISCONNECT_FORCE_KILLED, DISCONNECT_INVALID_CLIENT_ID,
    DISCONNECT_INVALID_SUBSCRIPTION, DISCONNECT_PING_TIMEOUT, DISCONNECT_SERVER_MAINTENANCE,
    DISCONNECT_SUBSCRIPTION_LIMIT, DISCONNECT_TOKEN_EXPIRED,
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

/// Dhan WebSocket disconnect error codes (801–814).
///
/// Each code maps to a specific recovery action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectCode {
    /// 801 — Exceeded max connections (5 per account). Do NOT reconnect.
    ExceededMaxConnections,
    /// 802 — Exceeded max instruments per connection (5000). Do NOT reconnect.
    ExceededMaxInstruments,
    /// 803 — Authentication failed. Refresh token, then reconnect.
    AuthenticationFailed,
    /// 804 — Token expired. Refresh token, then reconnect.
    TokenExpired,
    /// 805 — Server maintenance. Backoff + reconnect.
    ServerMaintenance,
    /// 806 — No ping received in 40s. Bug in ping logic — fix + reconnect.
    PingTimeout,
    /// 807 — Invalid subscription request format. Do NOT reconnect (fix request).
    InvalidSubscription,
    /// 808 — Total subscription limit exceeded (25000). Do NOT reconnect.
    SubscriptionLimitExceeded,
    /// 810 — Force-disconnected by Dhan. Wait + retry later.
    ForceKilled,
    /// 814 — Invalid client ID. Check SSM credentials.
    InvalidClientId,
    /// Unknown disconnect code not in the Dhan spec.
    Unknown(u16),
}

impl DisconnectCode {
    /// Parse a disconnect code from the raw u16 value in the binary frame.
    pub fn from_u16(code: u16) -> Self {
        match code {
            DISCONNECT_EXCEEDED_MAX_CONNECTIONS => Self::ExceededMaxConnections,
            DISCONNECT_EXCEEDED_MAX_INSTRUMENTS => Self::ExceededMaxInstruments,
            DISCONNECT_AUTH_FAILED => Self::AuthenticationFailed,
            DISCONNECT_TOKEN_EXPIRED => Self::TokenExpired,
            DISCONNECT_SERVER_MAINTENANCE => Self::ServerMaintenance,
            DISCONNECT_PING_TIMEOUT => Self::PingTimeout,
            DISCONNECT_INVALID_SUBSCRIPTION => Self::InvalidSubscription,
            DISCONNECT_SUBSCRIPTION_LIMIT => Self::SubscriptionLimitExceeded,
            DISCONNECT_FORCE_KILLED => Self::ForceKilled,
            DISCONNECT_INVALID_CLIENT_ID => Self::InvalidClientId,
            other => Self::Unknown(other),
        }
    }

    /// Returns the raw u16 code value.
    pub fn as_u16(&self) -> u16 {
        match self {
            Self::ExceededMaxConnections => DISCONNECT_EXCEEDED_MAX_CONNECTIONS,
            Self::ExceededMaxInstruments => DISCONNECT_EXCEEDED_MAX_INSTRUMENTS,
            Self::AuthenticationFailed => DISCONNECT_AUTH_FAILED,
            Self::TokenExpired => DISCONNECT_TOKEN_EXPIRED,
            Self::ServerMaintenance => DISCONNECT_SERVER_MAINTENANCE,
            Self::PingTimeout => DISCONNECT_PING_TIMEOUT,
            Self::InvalidSubscription => DISCONNECT_INVALID_SUBSCRIPTION,
            Self::SubscriptionLimitExceeded => DISCONNECT_SUBSCRIPTION_LIMIT,
            Self::ForceKilled => DISCONNECT_FORCE_KILLED,
            Self::InvalidClientId => DISCONNECT_INVALID_CLIENT_ID,
            Self::Unknown(code) => *code,
        }
    }

    /// Whether this disconnect code allows automatic reconnection.
    ///
    /// Capacity/format errors (801, 802, 807, 808, 814) indicate
    /// configuration bugs — reconnecting would just fail again.
    pub fn is_reconnectable(&self) -> bool {
        match self {
            Self::AuthenticationFailed
            | Self::TokenExpired
            | Self::ServerMaintenance
            | Self::PingTimeout
            | Self::ForceKilled => true,
            Self::ExceededMaxConnections
            | Self::ExceededMaxInstruments
            | Self::InvalidSubscription
            | Self::SubscriptionLimitExceeded
            | Self::InvalidClientId => false,
            Self::Unknown(_) => true, // assume transient for unknown codes
        }
    }

    /// Whether this disconnect code requires a token refresh before reconnect.
    pub fn requires_token_refresh(&self) -> bool {
        matches!(self, Self::AuthenticationFailed | Self::TokenExpired)
    }
}

impl fmt::Display for DisconnectCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExceededMaxConnections => write!(f, "801: Exceeded max connections"),
            Self::ExceededMaxInstruments => write!(f, "802: Exceeded max instruments"),
            Self::AuthenticationFailed => write!(f, "803: Authentication failed"),
            Self::TokenExpired => write!(f, "804: Token expired"),
            Self::ServerMaintenance => write!(f, "805: Server maintenance"),
            Self::PingTimeout => write!(f, "806: Ping timeout"),
            Self::InvalidSubscription => write!(f, "807: Invalid subscription"),
            Self::SubscriptionLimitExceeded => write!(f, "808: Subscription limit exceeded"),
            Self::ForceKilled => write!(f, "810: Force killed"),
            Self::InvalidClientId => write!(f, "814: Invalid client ID"),
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

    /// Ping send failed.
    #[error("Failed to send ping on connection {connection_id}: {reason}")]
    PingSendFailed {
        connection_id: ConnectionId,
        reason: String,
    },

    /// Capacity exceeded — too many instruments for available connections.
    #[error("Instrument count {requested} exceeds total capacity {capacity}")]
    CapacityExceeded { requested: usize, capacity: usize },
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
    /// Consecutive pong failures (0 = healthy).
    pub consecutive_pong_failures: u32,
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
            DisconnectCode::from_u16(801),
            DisconnectCode::ExceededMaxConnections
        );
        assert_eq!(
            DisconnectCode::from_u16(802),
            DisconnectCode::ExceededMaxInstruments
        );
        assert_eq!(
            DisconnectCode::from_u16(803),
            DisconnectCode::AuthenticationFailed
        );
        assert_eq!(DisconnectCode::from_u16(804), DisconnectCode::TokenExpired);
        assert_eq!(
            DisconnectCode::from_u16(805),
            DisconnectCode::ServerMaintenance
        );
        assert_eq!(DisconnectCode::from_u16(806), DisconnectCode::PingTimeout);
        assert_eq!(
            DisconnectCode::from_u16(807),
            DisconnectCode::InvalidSubscription
        );
        assert_eq!(
            DisconnectCode::from_u16(808),
            DisconnectCode::SubscriptionLimitExceeded
        );
        assert_eq!(DisconnectCode::from_u16(810), DisconnectCode::ForceKilled);
        assert_eq!(
            DisconnectCode::from_u16(814),
            DisconnectCode::InvalidClientId
        );
    }

    #[test]
    fn test_disconnect_code_unknown_value() {
        assert_eq!(DisconnectCode::from_u16(999), DisconnectCode::Unknown(999));
        assert_eq!(DisconnectCode::from_u16(0), DisconnectCode::Unknown(0));
    }

    #[test]
    fn test_disconnect_code_roundtrip() {
        let codes: &[u16] = &[801, 802, 803, 804, 805, 806, 807, 808, 810, 814];
        for &code in codes {
            let parsed = DisconnectCode::from_u16(code);
            assert_eq!(parsed.as_u16(), code, "roundtrip failed for code {code}");
        }
    }

    #[test]
    fn test_disconnect_code_reconnectable() {
        // Reconnectable: 803, 804, 805, 806, 810
        assert!(DisconnectCode::AuthenticationFailed.is_reconnectable());
        assert!(DisconnectCode::TokenExpired.is_reconnectable());
        assert!(DisconnectCode::ServerMaintenance.is_reconnectable());
        assert!(DisconnectCode::PingTimeout.is_reconnectable());
        assert!(DisconnectCode::ForceKilled.is_reconnectable());

        // NOT reconnectable: 801, 802, 807, 808, 814
        assert!(!DisconnectCode::ExceededMaxConnections.is_reconnectable());
        assert!(!DisconnectCode::ExceededMaxInstruments.is_reconnectable());
        assert!(!DisconnectCode::InvalidSubscription.is_reconnectable());
        assert!(!DisconnectCode::SubscriptionLimitExceeded.is_reconnectable());
        assert!(!DisconnectCode::InvalidClientId.is_reconnectable());

        // Unknown: assume reconnectable
        assert!(DisconnectCode::Unknown(999).is_reconnectable());
    }

    #[test]
    fn test_disconnect_code_requires_token_refresh() {
        assert!(DisconnectCode::AuthenticationFailed.requires_token_refresh());
        assert!(DisconnectCode::TokenExpired.requires_token_refresh());

        assert!(!DisconnectCode::ServerMaintenance.requires_token_refresh());
        assert!(!DisconnectCode::PingTimeout.requires_token_refresh());
        assert!(!DisconnectCode::ForceKilled.requires_token_refresh());
        assert!(!DisconnectCode::Unknown(999).requires_token_refresh());
    }

    #[test]
    fn test_disconnect_code_display() {
        assert_eq!(
            DisconnectCode::AuthenticationFailed.to_string(),
            "803: Authentication failed"
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
    fn test_instrument_subscription_serialize_json() {
        let sub = InstrumentSubscription::new(ExchangeSegment::NseFno, 52432);
        let json = serde_json::to_string(&sub).unwrap();
        assert!(json.contains("\"ExchangeSegment\":\"NSE_FNO\""));
        assert!(json.contains("\"SecurityId\":\"52432\""));
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

    // --- ConnectionHealth ---

    #[test]
    fn test_connection_health_default_values() {
        let health = ConnectionHealth {
            connection_id: 0,
            state: ConnectionState::Disconnected,
            subscribed_count: 0,
            consecutive_pong_failures: 0,
            total_reconnections: 0,
        };
        assert_eq!(health.connection_id, 0);
        assert_eq!(health.state, ConnectionState::Disconnected);
        assert_eq!(health.subscribed_count, 0);
    }
}

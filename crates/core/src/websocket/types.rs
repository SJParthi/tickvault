//! WebSocket domain types for Dhan Live Market Feed.
//!
//! Covers connection state, disconnect codes, subscription payloads,
//! and WebSocket-specific error variants.

use std::fmt;

use serde::{Deserialize, Serialize};
use tickvault_common::constants::{
    DATA_API_ACCESS_TOKEN_EXPIRED, DATA_API_ACCESS_TOKEN_INVALID, DATA_API_AUTHENTICATION_FAILED,
    DATA_API_CLIENT_ID_INVALID, DATA_API_EXCEEDED_ACTIVE_CONNECTIONS,
    DATA_API_INSTRUMENTS_EXCEED_LIMIT, DATA_API_INTERNAL_SERVER_ERROR,
    DATA_API_INVALID_DATE_FORMAT, DATA_API_INVALID_EXPIRY_DATE, DATA_API_INVALID_REQUEST,
    DATA_API_INVALID_SECURITY_ID, DATA_API_NOT_SUBSCRIBED,
};
use tickvault_common::types::ExchangeSegment;

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

/// Dhan Data API error codes (800–814) used in WebSocket disconnect packets
/// and REST error responses.
///
/// Source: docs/dhan-ref/08-annexure-enums.md Section 11.
/// All 12 documented codes have named variants. Unknown codes are preserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectCode {
    /// 800 — Internal Server Error. Retry with backoff.
    InternalServerError,
    /// 804 — Requested number of instruments exceeds limit.
    InstrumentsExceedLimit,
    /// 805 — Too many requests/connections — may result in user being blocked.
    ExceededActiveConnections,
    /// 806 — Data APIs not subscribed (plan/subscription issue).
    DataApiSubscriptionRequired,
    /// 807 — Access token expired. Refresh token, then reconnect.
    AccessTokenExpired,
    /// 808 — Authentication Failed — Client ID or Access Token invalid.
    /// Per annexure: 808 = "Authentication Failed" (NOT "Invalid Client ID").
    AuthenticationFailed,
    /// 809 — Access token invalid.
    /// Per annexure: 809 = "Access token invalid" (NOT "Authentication failed").
    AccessTokenInvalid,
    /// 810 — Client ID invalid.
    /// Per annexure: 810 = "Client ID invalid" (NOT 808).
    ClientIdInvalid,
    /// 811 — Invalid Expiry Date.
    InvalidExpiryDate,
    /// 812 — Invalid Date Format.
    InvalidDateFormat,
    /// 813 — Invalid SecurityId.
    InvalidSecurityId,
    /// 814 — Invalid Request.
    InvalidRequest,
    /// Unknown disconnect code not in annexure Section 11.
    Unknown(u16),
}

impl DisconnectCode {
    /// Parse a disconnect code from the raw u16 value in the binary frame.
    ///
    /// All 12 codes from annexure Section 11 map to named variants.
    /// Any other value maps to `Unknown(code)` — no panic.
    pub fn from_u16(code: u16) -> Self {
        match code {
            DATA_API_INTERNAL_SERVER_ERROR => Self::InternalServerError,
            DATA_API_INSTRUMENTS_EXCEED_LIMIT => Self::InstrumentsExceedLimit,
            DATA_API_EXCEEDED_ACTIVE_CONNECTIONS => Self::ExceededActiveConnections,
            DATA_API_NOT_SUBSCRIBED => Self::DataApiSubscriptionRequired,
            DATA_API_ACCESS_TOKEN_EXPIRED => Self::AccessTokenExpired,
            DATA_API_AUTHENTICATION_FAILED => Self::AuthenticationFailed,
            DATA_API_ACCESS_TOKEN_INVALID => Self::AccessTokenInvalid,
            DATA_API_CLIENT_ID_INVALID => Self::ClientIdInvalid,
            DATA_API_INVALID_EXPIRY_DATE => Self::InvalidExpiryDate,
            DATA_API_INVALID_DATE_FORMAT => Self::InvalidDateFormat,
            DATA_API_INVALID_SECURITY_ID => Self::InvalidSecurityId,
            DATA_API_INVALID_REQUEST => Self::InvalidRequest,
            other => Self::Unknown(other),
        }
    }

    /// Returns the raw u16 code value.
    pub fn as_u16(&self) -> u16 {
        match self {
            Self::InternalServerError => DATA_API_INTERNAL_SERVER_ERROR,
            Self::InstrumentsExceedLimit => DATA_API_INSTRUMENTS_EXCEED_LIMIT,
            Self::ExceededActiveConnections => DATA_API_EXCEEDED_ACTIVE_CONNECTIONS,
            Self::DataApiSubscriptionRequired => DATA_API_NOT_SUBSCRIBED,
            Self::AccessTokenExpired => DATA_API_ACCESS_TOKEN_EXPIRED,
            Self::AuthenticationFailed => DATA_API_AUTHENTICATION_FAILED,
            Self::AccessTokenInvalid => DATA_API_ACCESS_TOKEN_INVALID,
            Self::ClientIdInvalid => DATA_API_CLIENT_ID_INVALID,
            Self::InvalidExpiryDate => DATA_API_INVALID_EXPIRY_DATE,
            Self::InvalidDateFormat => DATA_API_INVALID_DATE_FORMAT,
            Self::InvalidSecurityId => DATA_API_INVALID_SECURITY_ID,
            Self::InvalidRequest => DATA_API_INVALID_REQUEST,
            Self::Unknown(code) => *code,
        }
    }

    /// Whether this disconnect code allows automatic reconnection.
    ///
    /// Reconnectable: 800 (transient server error), 807 (token expired — refresh first).
    /// NOT reconnectable: all others (config/credential/request errors — fix the root cause).
    pub fn is_reconnectable(&self) -> bool {
        match self {
            Self::InternalServerError | Self::AccessTokenExpired => true,
            Self::InstrumentsExceedLimit
            | Self::ExceededActiveConnections
            | Self::DataApiSubscriptionRequired
            | Self::AuthenticationFailed
            | Self::AccessTokenInvalid
            | Self::ClientIdInvalid
            | Self::InvalidExpiryDate
            | Self::InvalidDateFormat
            | Self::InvalidSecurityId
            | Self::InvalidRequest => false,
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
            Self::InternalServerError => write!(f, "800: Internal server error"),
            Self::InstrumentsExceedLimit => write!(f, "804: Instruments exceed limit"),
            Self::ExceededActiveConnections => {
                write!(f, "805: Active connections exceeded")
            }
            Self::DataApiSubscriptionRequired => {
                write!(f, "806: Data API subscription required")
            }
            Self::AccessTokenExpired => write!(f, "807: Access token expired"),
            Self::AuthenticationFailed => write!(f, "808: Authentication failed"),
            Self::AccessTokenInvalid => write!(f, "809: Access token invalid"),
            Self::ClientIdInvalid => write!(f, "810: Client ID invalid"),
            Self::InvalidExpiryDate => write!(f, "811: Invalid expiry date"),
            Self::InvalidDateFormat => write!(f, "812: Invalid date format"),
            Self::InvalidSecurityId => write!(f, "813: Invalid security ID"),
            Self::InvalidRequest => write!(f, "814: Invalid request"),
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
    // O(1) EXEMPT: begin — subscription constructor, runs at subscribe time, not per tick
    pub fn new(segment: ExchangeSegment, security_id: u32) -> Self {
        Self {
            exchange_segment: segment.as_str().to_string(),
            security_id: security_id.to_string(),
        }
    }
    // O(1) EXEMPT: end
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

/// JSON subscription request for 200-level depth (flat structure, no InstrumentList).
///
/// 200-level depth supports only 1 instrument per connection and uses a
/// different JSON structure from the standard feed and 20-level depth.
///
/// ```json
/// { "RequestCode": 23, "ExchangeSegment": "NSE_EQ", "SecurityId": "1333" }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TwoHundredDepthSubscriptionRequest {
    /// Feed request code (23 = subscribe, 25 = unsubscribe).
    #[serde(rename = "RequestCode")]
    pub request_code: u8,
    /// Exchange segment string (e.g., "NSE_EQ", "NSE_FNO").
    #[serde(rename = "ExchangeSegment")]
    pub exchange_segment: String,
    /// Security ID as string (Dhan requires string in JSON).
    #[serde(rename = "SecurityId")]
    pub security_id: String,
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

    /// WebSocket read timed out — no data received within the expected interval.
    /// Indicates a zombie connection (TCP alive but no frames from server).
    #[error("WebSocket read timeout on connection {connection_id} after {timeout_secs}s")]
    ReadTimeout {
        connection_id: ConnectionId,
        timeout_secs: u64,
    },
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
    fn test_disconnect_code_from_u16_all_12_known_codes() {
        // All 12 codes from annexure Section 11
        assert_eq!(
            DisconnectCode::from_u16(800),
            DisconnectCode::InternalServerError
        );
        assert_eq!(
            DisconnectCode::from_u16(804),
            DisconnectCode::InstrumentsExceedLimit
        );
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
            DisconnectCode::AuthenticationFailed
        );
        assert_eq!(
            DisconnectCode::from_u16(809),
            DisconnectCode::AccessTokenInvalid
        );
        assert_eq!(
            DisconnectCode::from_u16(810),
            DisconnectCode::ClientIdInvalid
        );
        assert_eq!(
            DisconnectCode::from_u16(811),
            DisconnectCode::InvalidExpiryDate
        );
        assert_eq!(
            DisconnectCode::from_u16(812),
            DisconnectCode::InvalidDateFormat
        );
        assert_eq!(
            DisconnectCode::from_u16(813),
            DisconnectCode::InvalidSecurityId
        );
        assert_eq!(
            DisconnectCode::from_u16(814),
            DisconnectCode::InvalidRequest
        );
    }

    #[test]
    fn test_disconnect_code_unknown_value() {
        assert_eq!(DisconnectCode::from_u16(999), DisconnectCode::Unknown(999));
        assert_eq!(DisconnectCode::from_u16(0), DisconnectCode::Unknown(0));
        // Codes 801, 802, 803 are NOT in annexure Section 11 — map to Unknown
        assert_eq!(DisconnectCode::from_u16(801), DisconnectCode::Unknown(801));
        assert_eq!(DisconnectCode::from_u16(802), DisconnectCode::Unknown(802));
        assert_eq!(DisconnectCode::from_u16(803), DisconnectCode::Unknown(803));
    }

    #[test]
    fn test_disconnect_code_roundtrip() {
        let codes: &[u16] = &[800, 804, 805, 806, 807, 808, 809, 810, 811, 812, 813, 814];
        for &code in codes {
            let parsed = DisconnectCode::from_u16(code);
            assert_eq!(parsed.as_u16(), code, "roundtrip failed for code {code}");
        }
    }

    #[test]
    fn test_disconnect_code_reconnectable() {
        // Reconnectable: 800 (transient server error), 807 (token expired)
        assert!(DisconnectCode::InternalServerError.is_reconnectable());
        assert!(DisconnectCode::AccessTokenExpired.is_reconnectable());

        // NOT reconnectable: all config/credential/request errors
        assert!(!DisconnectCode::InstrumentsExceedLimit.is_reconnectable());
        assert!(!DisconnectCode::ExceededActiveConnections.is_reconnectable());
        assert!(!DisconnectCode::DataApiSubscriptionRequired.is_reconnectable());
        assert!(!DisconnectCode::AuthenticationFailed.is_reconnectable());
        assert!(!DisconnectCode::AccessTokenInvalid.is_reconnectable());
        assert!(!DisconnectCode::ClientIdInvalid.is_reconnectable());
        assert!(!DisconnectCode::InvalidExpiryDate.is_reconnectable());
        assert!(!DisconnectCode::InvalidDateFormat.is_reconnectable());
        assert!(!DisconnectCode::InvalidSecurityId.is_reconnectable());
        assert!(!DisconnectCode::InvalidRequest.is_reconnectable());

        // Unknown: assume reconnectable (transient)
        assert!(DisconnectCode::Unknown(999).is_reconnectable());
    }

    #[test]
    fn test_disconnect_code_requires_token_refresh() {
        assert!(DisconnectCode::AccessTokenExpired.requires_token_refresh());

        // All others do NOT require token refresh
        assert!(!DisconnectCode::InternalServerError.requires_token_refresh());
        assert!(!DisconnectCode::InstrumentsExceedLimit.requires_token_refresh());
        assert!(!DisconnectCode::ExceededActiveConnections.requires_token_refresh());
        assert!(!DisconnectCode::DataApiSubscriptionRequired.requires_token_refresh());
        assert!(!DisconnectCode::AuthenticationFailed.requires_token_refresh());
        assert!(!DisconnectCode::AccessTokenInvalid.requires_token_refresh());
        assert!(!DisconnectCode::ClientIdInvalid.requires_token_refresh());
        assert!(!DisconnectCode::InvalidExpiryDate.requires_token_refresh());
        assert!(!DisconnectCode::InvalidDateFormat.requires_token_refresh());
        assert!(!DisconnectCode::InvalidSecurityId.requires_token_refresh());
        assert!(!DisconnectCode::InvalidRequest.requires_token_refresh());
        assert!(!DisconnectCode::Unknown(999).requires_token_refresh());
    }

    #[test]
    fn test_disconnect_code_display() {
        assert_eq!(
            DisconnectCode::AuthenticationFailed.to_string(),
            "808: Authentication failed"
        );
        assert_eq!(
            DisconnectCode::AccessTokenInvalid.to_string(),
            "809: Access token invalid"
        );
        assert_eq!(
            DisconnectCode::ClientIdInvalid.to_string(),
            "810: Client ID invalid"
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
        let cloned = state;
        let copied = state; // Copy trait
        assert_eq!(state, cloned);
        assert_eq!(state, copied);
    }

    // --- DisconnectCode (additional) ---

    #[test]
    fn test_disconnect_code_clone_and_copy() {
        let code = DisconnectCode::AccessTokenExpired;
        let cloned = code;
        let copied = code; // Copy trait
        assert_eq!(code, cloned);
        assert_eq!(code, copied);
    }

    #[test]
    fn test_disconnect_code_display_all_known() {
        assert!(
            DisconnectCode::InternalServerError
                .to_string()
                .contains("800")
        );
        assert!(
            DisconnectCode::InstrumentsExceedLimit
                .to_string()
                .contains("804")
        );
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
        assert!(
            DisconnectCode::AuthenticationFailed
                .to_string()
                .contains("808")
        );
        assert!(
            DisconnectCode::AccessTokenInvalid
                .to_string()
                .contains("809")
        );
        assert!(DisconnectCode::ClientIdInvalid.to_string().contains("810"));
        assert!(
            DisconnectCode::InvalidExpiryDate
                .to_string()
                .contains("811")
        );
        assert!(
            DisconnectCode::InvalidDateFormat
                .to_string()
                .contains("812")
        );
        assert!(
            DisconnectCode::InvalidSecurityId
                .to_string()
                .contains("813")
        );
        assert!(DisconnectCode::InvalidRequest.to_string().contains("814"));
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
            code: DisconnectCode::ClientIdInvalid,
        };
        assert!(err.to_string().contains("810"));
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
    fn test_websocket_error_read_timeout_display() {
        let err = WebSocketError::ReadTimeout {
            connection_id: 3,
            timeout_secs: 40,
        };
        let msg = err.to_string();
        assert!(msg.contains("connection 3"));
        assert!(msg.contains("40s"));
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

    // --- TwoHundredDepthSubscriptionRequest ---

    #[test]
    fn test_two_hundred_depth_subscription_serialize() {
        let request = TwoHundredDepthSubscriptionRequest {
            request_code: 23,
            exchange_segment: "NSE_EQ".to_string(),
            security_id: "1333".to_string(),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"RequestCode\":23"));
        assert!(json.contains("\"ExchangeSegment\":\"NSE_EQ\""));
        assert!(json.contains("\"SecurityId\":\"1333\""));
        // Must NOT contain InstrumentList or InstrumentCount
        assert!(!json.contains("InstrumentList"));
        assert!(!json.contains("InstrumentCount"));
    }

    #[test]
    fn test_two_hundred_depth_subscription_deserialize_roundtrip() {
        let request = TwoHundredDepthSubscriptionRequest {
            request_code: 23,
            exchange_segment: "NSE_FNO".to_string(),
            security_id: "52432".to_string(),
        };
        let json = serde_json::to_string(&request).unwrap();
        let deserialized: TwoHundredDepthSubscriptionRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.request_code, 23);
        assert_eq!(deserialized.exchange_segment, "NSE_FNO");
        assert_eq!(deserialized.security_id, "52432");
    }

    #[test]
    fn test_two_hundred_depth_unsubscribe_request_code_25() {
        let request = TwoHundredDepthSubscriptionRequest {
            request_code: 25,
            exchange_segment: "NSE_EQ".to_string(),
            security_id: "2885".to_string(),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"RequestCode\":25"));
    }

    #[test]
    fn test_two_hundred_depth_security_id_is_string() {
        let request = TwoHundredDepthSubscriptionRequest {
            request_code: 23,
            exchange_segment: "NSE_EQ".to_string(),
            security_id: "2885".to_string(),
        };
        let json = serde_json::to_string(&request).unwrap();
        // SecurityId must be string, not number
        assert!(json.contains("\"SecurityId\":\"2885\""));
        assert!(!json.contains("\"SecurityId\":2885"));
    }

    // --- DisconnectCode Display exact strings ---

    #[test]
    fn test_disconnect_code_display_exact_all_known() {
        assert_eq!(
            DisconnectCode::InternalServerError.to_string(),
            "800: Internal server error"
        );
        assert_eq!(
            DisconnectCode::InstrumentsExceedLimit.to_string(),
            "804: Instruments exceed limit"
        );
        assert_eq!(
            DisconnectCode::ExceededActiveConnections.to_string(),
            "805: Active connections exceeded"
        );
        assert_eq!(
            DisconnectCode::DataApiSubscriptionRequired.to_string(),
            "806: Data API subscription required"
        );
        assert_eq!(
            DisconnectCode::AccessTokenExpired.to_string(),
            "807: Access token expired"
        );
        assert_eq!(
            DisconnectCode::InvalidExpiryDate.to_string(),
            "811: Invalid expiry date"
        );
        assert_eq!(
            DisconnectCode::InvalidDateFormat.to_string(),
            "812: Invalid date format"
        );
        assert_eq!(
            DisconnectCode::InvalidSecurityId.to_string(),
            "813: Invalid security ID"
        );
        assert_eq!(
            DisconnectCode::InvalidRequest.to_string(),
            "814: Invalid request"
        );
    }

    #[test]
    fn test_disconnect_code_display_unknown_shows_raw_code() {
        assert_eq!(
            DisconnectCode::Unknown(42).to_string(),
            "42: Unknown disconnect"
        );
        assert_eq!(
            DisconnectCode::Unknown(0).to_string(),
            "0: Unknown disconnect"
        );
        assert_eq!(
            DisconnectCode::Unknown(u16::MAX).to_string(),
            "65535: Unknown disconnect"
        );
    }

    // --- ConnectionState Display ---

    #[test]
    fn test_connection_state_display_via_format_macro() {
        // Exercises Display through format! (different code path than .to_string())
        assert_eq!(
            format!("state={}", ConnectionState::Disconnected),
            "state=Disconnected"
        );
        assert_eq!(
            format!("state={}", ConnectionState::Connecting),
            "state=Connecting"
        );
        assert_eq!(
            format!("state={}", ConnectionState::Connected),
            "state=Connected"
        );
        assert_eq!(
            format!("state={}", ConnectionState::Reconnecting),
            "state=Reconnecting"
        );
    }

    // --- ConnectionState Debug ---

    #[test]
    fn test_connection_state_debug_all_variants() {
        assert_eq!(
            format!("{:?}", ConnectionState::Disconnected),
            "Disconnected"
        );
        assert_eq!(format!("{:?}", ConnectionState::Connecting), "Connecting");
        assert_eq!(format!("{:?}", ConnectionState::Connected), "Connected");
        assert_eq!(
            format!("{:?}", ConnectionState::Reconnecting),
            "Reconnecting"
        );
    }

    // --- DisconnectCode as_u16 for Unknown preserves value ---

    #[test]
    fn test_disconnect_code_unknown_as_u16_preserves_arbitrary_values() {
        for code in [1, 100, 801, 802, 803, 815, 900, 1000, u16::MAX] {
            let dc = DisconnectCode::from_u16(code);
            assert_eq!(dc, DisconnectCode::Unknown(code));
            assert_eq!(dc.as_u16(), code);
        }
    }

    // --- WebSocketError TlsConfigurationFailed display ---

    #[test]
    fn test_websocket_error_tls_configuration_failed_display() {
        let err = WebSocketError::TlsConfigurationFailed {
            reason: "missing CA cert".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("TLS configuration failed"));
        assert!(msg.contains("missing CA cert"));
    }

    // --- ConnectionState Display: f.write_str path ---

    #[test]
    fn test_connection_state_display_write_str_all_variants() {
        // Exercises Display via std::fmt::Write on a String buffer
        use std::fmt::Write;
        let variants = [
            (ConnectionState::Disconnected, "Disconnected"),
            (ConnectionState::Connecting, "Connecting"),
            (ConnectionState::Connected, "Connected"),
            (ConnectionState::Reconnecting, "Reconnecting"),
        ];
        for (state, expected) in variants {
            let mut buf = String::new();
            write!(buf, "{state}").unwrap();
            assert_eq!(buf, expected);
        }
    }

    // --- DisconnectCode Display: all 12 + Unknown via write! ---

    #[test]
    fn test_disconnect_code_display_all_variants_via_write() {
        use std::fmt::Write;
        let known = [
            (
                DisconnectCode::InternalServerError,
                "800: Internal server error",
            ),
            (
                DisconnectCode::InstrumentsExceedLimit,
                "804: Instruments exceed limit",
            ),
            (
                DisconnectCode::ExceededActiveConnections,
                "805: Active connections exceeded",
            ),
            (
                DisconnectCode::DataApiSubscriptionRequired,
                "806: Data API subscription required",
            ),
            (
                DisconnectCode::AccessTokenExpired,
                "807: Access token expired",
            ),
            (
                DisconnectCode::AuthenticationFailed,
                "808: Authentication failed",
            ),
            (
                DisconnectCode::AccessTokenInvalid,
                "809: Access token invalid",
            ),
            (DisconnectCode::ClientIdInvalid, "810: Client ID invalid"),
            (
                DisconnectCode::InvalidExpiryDate,
                "811: Invalid expiry date",
            ),
            (
                DisconnectCode::InvalidDateFormat,
                "812: Invalid date format",
            ),
            (
                DisconnectCode::InvalidSecurityId,
                "813: Invalid security ID",
            ),
            (DisconnectCode::InvalidRequest, "814: Invalid request"),
            (DisconnectCode::Unknown(123), "123: Unknown disconnect"),
        ];
        for (code, expected) in known {
            let mut buf = String::new();
            write!(buf, "{code}").unwrap();
            assert_eq!(buf, expected);
        }
    }

    // --- ConnectionHealth Debug ---

    #[test]
    fn test_connection_health_debug_format() {
        let health = ConnectionHealth {
            connection_id: 4,
            state: ConnectionState::Reconnecting,
            subscribed_count: 1000,
            total_reconnections: 7,
        };
        let debug = format!("{health:?}");
        assert!(debug.contains("ConnectionHealth"));
        assert!(debug.contains("Reconnecting"));
        assert!(debug.contains("1000"));
        assert!(debug.contains("7"));
    }

    // --- InstrumentSubscription Debug + Clone ---

    #[test]
    fn test_instrument_subscription_debug_format() {
        let sub = InstrumentSubscription::new(ExchangeSegment::NseFno, 52432);
        let debug = format!("{sub:?}");
        assert!(debug.contains("InstrumentSubscription"));
        assert!(debug.contains("NSE_FNO"));
        assert!(debug.contains("52432"));
    }

    #[test]
    fn test_instrument_subscription_clone() {
        let sub = InstrumentSubscription::new(ExchangeSegment::NseEquity, 1333);
        let cloned = sub.clone();
        assert_eq!(cloned.exchange_segment, "NSE_EQ");
        assert_eq!(cloned.security_id, "1333");
    }

    // --- DisconnectCode Debug ---

    #[test]
    fn test_disconnect_code_debug_known_variant() {
        let code = DisconnectCode::AccessTokenExpired;
        let debug = format!("{code:?}");
        assert_eq!(debug, "AccessTokenExpired");
    }

    #[test]
    fn test_disconnect_code_debug_unknown_variant() {
        let code = DisconnectCode::Unknown(42);
        let debug = format!("{code:?}");
        assert!(debug.contains("Unknown"));
        assert!(debug.contains("42"));
    }

    #[test]
    fn test_disconnect_code_debug_all_known_variants() {
        let variants = [
            (DisconnectCode::InternalServerError, "InternalServerError"),
            (
                DisconnectCode::InstrumentsExceedLimit,
                "InstrumentsExceedLimit",
            ),
            (
                DisconnectCode::ExceededActiveConnections,
                "ExceededActiveConnections",
            ),
            (
                DisconnectCode::DataApiSubscriptionRequired,
                "DataApiSubscriptionRequired",
            ),
            (DisconnectCode::AccessTokenExpired, "AccessTokenExpired"),
            (DisconnectCode::AuthenticationFailed, "AuthenticationFailed"),
            (DisconnectCode::AccessTokenInvalid, "AccessTokenInvalid"),
            (DisconnectCode::ClientIdInvalid, "ClientIdInvalid"),
            (DisconnectCode::InvalidExpiryDate, "InvalidExpiryDate"),
            (DisconnectCode::InvalidDateFormat, "InvalidDateFormat"),
            (DisconnectCode::InvalidSecurityId, "InvalidSecurityId"),
            (DisconnectCode::InvalidRequest, "InvalidRequest"),
        ];
        for (code, expected) in variants {
            let debug = format!("{code:?}");
            assert_eq!(debug, expected, "Debug mismatch for {code}");
        }
    }

    // WS-GAP-01: exhaustive from_u16 for every gap code (801, 802, 803 are NOT in annexure)
    #[test]
    fn test_disconnect_code_from_u16_gap_codes_801_802_803_are_unknown() {
        for code in [801, 802, 803] {
            let dc = DisconnectCode::from_u16(code);
            assert_eq!(dc, DisconnectCode::Unknown(code));
            assert!(
                dc.is_reconnectable(),
                "unknown codes should be reconnectable"
            );
            assert!(
                !dc.requires_token_refresh(),
                "only 807 requires token refresh"
            );
        }
    }

    #[test]
    fn test_disconnect_code_from_u16_999_is_unknown() {
        let dc = DisconnectCode::from_u16(999);
        assert_eq!(dc, DisconnectCode::Unknown(999));
        assert_eq!(dc.as_u16(), 999);
    }

    #[test]
    fn test_disconnect_code_all_known_as_u16_roundtrip_exhaustive() {
        let all_known: [(DisconnectCode, u16); 12] = [
            (DisconnectCode::InternalServerError, 800),
            (DisconnectCode::InstrumentsExceedLimit, 804),
            (DisconnectCode::ExceededActiveConnections, 805),
            (DisconnectCode::DataApiSubscriptionRequired, 806),
            (DisconnectCode::AccessTokenExpired, 807),
            (DisconnectCode::AuthenticationFailed, 808),
            (DisconnectCode::AccessTokenInvalid, 809),
            (DisconnectCode::ClientIdInvalid, 810),
            (DisconnectCode::InvalidExpiryDate, 811),
            (DisconnectCode::InvalidDateFormat, 812),
            (DisconnectCode::InvalidSecurityId, 813),
            (DisconnectCode::InvalidRequest, 814),
        ];
        for (variant, expected_code) in all_known {
            assert_eq!(variant.as_u16(), expected_code);
            assert_eq!(DisconnectCode::from_u16(expected_code), variant);
        }
    }

    #[test]
    fn test_disconnect_code_is_reconnectable_exhaustive_all_variants() {
        // Exhaustive: every named variant + Unknown
        let reconnectable = [
            DisconnectCode::InternalServerError,
            DisconnectCode::AccessTokenExpired,
            DisconnectCode::Unknown(0),
            DisconnectCode::Unknown(42),
            DisconnectCode::Unknown(u16::MAX),
        ];
        for dc in reconnectable {
            assert!(dc.is_reconnectable(), "{dc} should be reconnectable");
        }

        let not_reconnectable = [
            DisconnectCode::InstrumentsExceedLimit,
            DisconnectCode::ExceededActiveConnections,
            DisconnectCode::DataApiSubscriptionRequired,
            DisconnectCode::AuthenticationFailed,
            DisconnectCode::AccessTokenInvalid,
            DisconnectCode::ClientIdInvalid,
            DisconnectCode::InvalidExpiryDate,
            DisconnectCode::InvalidDateFormat,
            DisconnectCode::InvalidSecurityId,
            DisconnectCode::InvalidRequest,
        ];
        for dc in not_reconnectable {
            assert!(!dc.is_reconnectable(), "{dc} should NOT be reconnectable");
        }
    }

    #[test]
    fn test_disconnect_code_requires_token_refresh_exhaustive() {
        // Only 807 requires token refresh
        assert!(DisconnectCode::AccessTokenExpired.requires_token_refresh());

        let no_refresh = [
            DisconnectCode::InternalServerError,
            DisconnectCode::InstrumentsExceedLimit,
            DisconnectCode::ExceededActiveConnections,
            DisconnectCode::DataApiSubscriptionRequired,
            DisconnectCode::AuthenticationFailed,
            DisconnectCode::AccessTokenInvalid,
            DisconnectCode::ClientIdInvalid,
            DisconnectCode::InvalidExpiryDate,
            DisconnectCode::InvalidDateFormat,
            DisconnectCode::InvalidSecurityId,
            DisconnectCode::InvalidRequest,
            DisconnectCode::Unknown(0),
            DisconnectCode::Unknown(807), // Unknown(807) is NOT AccessTokenExpired variant
        ];
        for dc in no_refresh {
            assert!(
                !dc.requires_token_refresh(),
                "{dc} should NOT require token refresh"
            );
        }
    }

    // ConnectionHealth — verify all field access paths
    #[test]
    fn test_connection_health_all_fields_accessible() {
        let health = ConnectionHealth {
            connection_id: 4,
            state: ConnectionState::Reconnecting,
            subscribed_count: 5000,
            total_reconnections: 99,
        };
        assert_eq!(health.connection_id, 4);
        assert_eq!(health.state, ConnectionState::Reconnecting);
        assert_eq!(health.subscribed_count, 5000);
        assert_eq!(health.total_reconnections, 99);
    }

    #[test]
    fn test_connection_health_clone_preserves_all_fields() {
        let original = ConnectionHealth {
            connection_id: 0,
            state: ConnectionState::Connecting,
            subscribed_count: 100,
            total_reconnections: 1,
        };
        let cloned = original.clone();
        assert_eq!(cloned.connection_id, original.connection_id);
        assert_eq!(cloned.state, original.state);
        assert_eq!(cloned.subscribed_count, original.subscribed_count);
        assert_eq!(cloned.total_reconnections, original.total_reconnections);
    }

    // --- WebSocketError Debug ---

    #[test]
    fn test_websocket_error_no_token_debug() {
        let err = WebSocketError::NoTokenAvailable;
        let debug = format!("{err:?}");
        assert!(debug.contains("NoTokenAvailable"));
    }

    #[test]
    fn test_websocket_error_all_variants_debug() {
        // Verify Debug impl exists and is non-empty for every variant
        let err1 = WebSocketError::CapacityExceeded {
            requested: 1,
            capacity: 0,
        };
        assert!(!format!("{err1:?}").is_empty());

        let err2 = WebSocketError::ReconnectionExhausted {
            connection_id: 0,
            attempts: 0,
        };
        assert!(!format!("{err2:?}").is_empty());

        let err3 = WebSocketError::SubscriptionFailed {
            connection_id: 0,
            reason: String::new(),
        };
        assert!(!format!("{err3:?}").is_empty());

        let err4 = WebSocketError::TlsConfigurationFailed {
            reason: String::new(),
        };
        assert!(!format!("{err4:?}").is_empty());

        let err5 = WebSocketError::ReadTimeout {
            connection_id: 0,
            timeout_secs: 0,
        };
        assert!(!format!("{err5:?}").is_empty());

        let err6 = WebSocketError::DhanDisconnect {
            code: DisconnectCode::InternalServerError,
        };
        assert!(!format!("{err6:?}").is_empty());

        let err7 = WebSocketError::NonReconnectableDisconnect {
            code: DisconnectCode::InvalidRequest,
        };
        assert!(!format!("{err7:?}").is_empty());
    }

    // --- TwoHundredDepthSubscriptionRequest Clone + Debug ---

    #[test]
    fn test_two_hundred_depth_subscription_clone() {
        let request = TwoHundredDepthSubscriptionRequest {
            request_code: 23,
            exchange_segment: "NSE_EQ".to_string(),
            security_id: "1333".to_string(),
        };
        let cloned = request.clone();
        assert_eq!(cloned.request_code, 23);
        assert_eq!(cloned.exchange_segment, "NSE_EQ");
        assert_eq!(cloned.security_id, "1333");
    }

    #[test]
    fn test_two_hundred_depth_subscription_debug() {
        let request = TwoHundredDepthSubscriptionRequest {
            request_code: 23,
            exchange_segment: "NSE_FNO".to_string(),
            security_id: "52432".to_string(),
        };
        let debug = format!("{request:?}");
        assert!(debug.contains("TwoHundredDepthSubscriptionRequest"));
        assert!(debug.contains("NSE_FNO"));
    }
}

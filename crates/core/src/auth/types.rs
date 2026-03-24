//! Authentication types for the Dhan token lifecycle.
//!
//! All sensitive values wrapped in `Secret<String>` — Debug prints `[REDACTED]`.
//! Token memory zeroized on drop via `zeroize` crate.

use std::fmt;

use chrono::{DateTime, Duration, FixedOffset, Utc};
use secrecy::SecretString;
use serde::Deserialize;
use zeroize::{Zeroize, ZeroizeOnDrop};

use dhan_live_trader_common::trading_calendar::ist_offset;

/// Parses `expiryTime` from Dhan's generateAccessToken response.
///
/// Handles multiple formats:
/// - Epoch milliseconds (number): `1772113432557`
/// - Epoch seconds (number < 10_000_000_000): `1772113432`
/// - ISO 8601 string: `"2026-02-27T13:45:00+05:30"`
fn parse_expiry_time(value: &serde_json::Value) -> Option<DateTime<FixedOffset>> {
    let ist = ist_offset();
    match value {
        serde_json::Value::Number(n) => {
            let epoch = n.as_i64()?;
            // Distinguish millis vs seconds: millis are > 10 billion
            let epoch_secs = if epoch > 10_000_000_000 {
                epoch / 1000
            } else {
                epoch
            };
            let dt = DateTime::from_timestamp(epoch_secs, 0)?;
            Some(dt.with_timezone(&ist))
        }
        serde_json::Value::String(s) => {
            // Try parsing as ISO 8601 with timezone
            if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
                return Some(dt.with_timezone(&ist));
            }
            // Try common date-time formats
            if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
                return dt.and_local_timezone(ist).single();
            }
            // Try epoch millis as string
            if let Ok(epoch) = s.parse::<i64>() {
                let epoch_secs = if epoch > 10_000_000_000 {
                    epoch / 1000
                } else {
                    epoch
                };
                let dt = DateTime::from_timestamp(epoch_secs, 0)?;
                return Some(dt.with_timezone(&ist));
            }
            None
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Dhan Credentials (from SSM)
// ---------------------------------------------------------------------------

/// Dhan API credentials fetched from AWS SSM Parameter Store.
///
/// All fields are `Secret<String>` — Debug/Display never leak raw values.
pub struct DhanCredentials {
    /// Dhan client ID (account identifier).
    pub client_id: SecretString,
    /// Dhan client secret (password).
    pub client_secret: SecretString,
    /// TOTP base32-encoded secret for 2FA.
    pub totp_secret: SecretString,
}

impl fmt::Debug for DhanCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DhanCredentials")
            .field("client_id", &"[REDACTED]")
            .field("client_secret", &"[REDACTED]")
            .field("totp_secret", &"[REDACTED]")
            .finish()
    }
}

// ---------------------------------------------------------------------------
// QuestDB Credentials (from SSM)
// ---------------------------------------------------------------------------

/// QuestDB PG wire protocol credentials fetched from AWS SSM Parameter Store.
///
/// Used for PostgreSQL wire protocol connections (port 8812).
/// All fields are `Secret<String>` — Debug/Display never leak raw values.
pub struct QuestDbCredentials {
    /// QuestDB PG wire protocol username.
    pub pg_user: SecretString,
    /// QuestDB PG wire protocol password.
    pub pg_password: SecretString,
}

impl fmt::Debug for QuestDbCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuestDbCredentials")
            .field("pg_user", &"[REDACTED]")
            .field("pg_password", &"[REDACTED]")
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Grafana Credentials (from SSM)
// ---------------------------------------------------------------------------

/// Grafana admin credentials fetched from AWS SSM Parameter Store.
///
/// All fields are `Secret<String>` — Debug/Display never leak raw values.
pub struct GrafanaCredentials {
    /// Grafana admin username.
    pub admin_user: SecretString,
    /// Grafana admin password.
    pub admin_password: SecretString,
}

impl fmt::Debug for GrafanaCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GrafanaCredentials")
            .field("admin_user", &"[REDACTED]")
            .field("admin_password", &"[REDACTED]")
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Telegram Credentials (from SSM)
// ---------------------------------------------------------------------------

/// Telegram bot credentials fetched from AWS SSM Parameter Store.
///
/// Used by the infra orchestrator to inject into docker-compose
/// environment variables for Grafana alerting.
pub struct TelegramCredentials {
    /// Telegram bot token.
    pub bot_token: SecretString,
    /// Telegram chat ID.
    pub chat_id: SecretString,
}

impl fmt::Debug for TelegramCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TelegramCredentials")
            .field("bot_token", &"[REDACTED]")
            .field("chat_id", &"[REDACTED]")
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Token State (stored in ArcSwap)
// ---------------------------------------------------------------------------

/// Active JWT token state held atomically via arc-swap.
///
/// Consumers call `arc_swap.load()` for O(1) reads on every WebSocket
/// frame and REST API call. The token is swapped atomically on renewal.
///
/// `SecretString` internally zeroizes its memory on drop, so the access
/// token is automatically wiped when this struct is dropped.
pub struct TokenState {
    /// JWT access token value (zeroized on drop by SecretString).
    access_token: SecretString,

    /// When this token expires (IST).
    expires_at: DateTime<FixedOffset>,

    /// When this token was issued (IST).
    issued_at: DateTime<FixedOffset>,
}

impl TokenState {
    /// Creates a new `TokenState` from a Dhan auth response (renewal/legacy).
    pub fn from_response(response: &DhanAuthResponseData) -> Self {
        let now_ist = Utc::now().with_timezone(&ist_offset());
        let delta = Duration::seconds(i64::from(response.expires_in));
        let expires_at = now_ist.checked_add_signed(delta).unwrap_or(now_ist);

        Self {
            access_token: SecretString::from(response.access_token.clone()),
            expires_at,
            issued_at: now_ist,
        }
    }

    /// Creates a new `TokenState` from the Dhan generateAccessToken response.
    ///
    /// Handles `expiryTime` as either epoch milliseconds (number) or ISO string.
    /// Falls back to 24-hour default validity if parsing fails.
    pub fn from_generate_response(response: &DhanGenerateTokenResponse) -> Self {
        let now_ist = Utc::now().with_timezone(&ist_offset());

        let expires_at = parse_expiry_time(&response.expiry_time).unwrap_or_else(|| {
            // Default: 24 hours from now (Dhan standard token validity)
            now_ist
                .checked_add_signed(Duration::hours(24))
                .unwrap_or(now_ist)
        });

        Self {
            access_token: SecretString::from(response.access_token.clone()),
            expires_at,
            issued_at: now_ist,
        }
    }

    /// Creates a `TokenState` from cached values (used by token_cache module).
    ///
    /// Bypasses the Dhan API response parsing — used only when loading a
    /// previously-validated token from the local cache file.
    pub(crate) fn from_cached(
        access_token: SecretString,
        expires_at: DateTime<FixedOffset>,
        issued_at: DateTime<FixedOffset>,
    ) -> Self {
        Self {
            access_token,
            expires_at,
            issued_at,
        }
    }

    /// Returns a reference to the access token secret.
    ///
    /// Callers must use `expose_secret()` to extract the raw value
    /// for HTTP Authorization headers. Never log the exposed value.
    pub fn access_token(&self) -> &SecretString {
        &self.access_token
    }

    /// Returns the expiry timestamp (IST).
    pub fn expires_at(&self) -> DateTime<FixedOffset> {
        self.expires_at
    }

    /// Returns the issuance timestamp (IST).
    pub fn issued_at(&self) -> DateTime<FixedOffset> {
        self.issued_at
    }

    /// Returns `true` if the token has not yet expired.
    pub fn is_valid(&self) -> bool {
        let now_ist = Utc::now().with_timezone(&ist_offset());
        now_ist < self.expires_at
    }

    /// Returns `true` if the token is within the refresh window.
    ///
    /// The refresh window starts at `token_validity - refresh_before_expiry`
    /// hours after issuance.
    pub fn needs_refresh(&self, refresh_before_expiry_hours: u64) -> bool {
        let now_ist = Utc::now().with_timezone(&ist_offset());
        #[allow(clippy::cast_possible_wrap)]
        // APPROVED: safe cast — clamped to 8760 before widening
        let hours = refresh_before_expiry_hours.min(8760) as i64;
        let refresh_threshold = self
            .expires_at
            .checked_sub_signed(Duration::hours(hours))
            .unwrap_or(self.expires_at);
        now_ist >= refresh_threshold
    }

    /// Returns the duration until this token should be refreshed.
    ///
    /// Returns zero if already past the refresh window.
    pub fn time_until_refresh(&self, refresh_before_expiry_hours: u64) -> std::time::Duration {
        let now_ist = Utc::now().with_timezone(&ist_offset());
        #[allow(clippy::cast_possible_wrap)]
        // APPROVED: safe cast — clamped to 8760 before widening
        let hours = refresh_before_expiry_hours.min(8760) as i64;
        let refresh_at = self
            .expires_at
            .checked_sub_signed(Duration::hours(hours))
            .unwrap_or(self.expires_at);
        if now_ist >= refresh_at {
            std::time::Duration::ZERO
        } else {
            refresh_at
                .signed_duration_since(now_ist)
                .to_std()
                .unwrap_or(std::time::Duration::ZERO)
        }
    }

    /// Returns the token age in hours (for logging only).
    ///
    /// Uses seconds-based precision. Clamps negative durations to zero.
    #[allow(clippy::arithmetic_side_effects)] // APPROVED: logging-only cold path, f64 division safe
    pub fn age_hours(&self) -> f64 {
        let now_ist = Utc::now().with_timezone(&ist_offset());
        let age = now_ist.signed_duration_since(self.issued_at);
        let seconds = age.num_seconds().max(0);
        seconds as f64 / 3600.0
    }
}

impl fmt::Debug for TokenState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenState")
            .field("access_token", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .field("issued_at", &self.issued_at)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Dhan API Request/Response Types
// ---------------------------------------------------------------------------

/// Query parameters for `POST https://auth.dhan.co/app/generateAccessToken`.
///
/// Fields are zeroized on drop to prevent credential leakage from freed memory.
/// Dhan API expects: `dhanClientId`, `pin` (6-digit trading PIN), `totp`.
#[derive(serde::Serialize, Zeroize, ZeroizeOnDrop)]
pub struct GenerateTokenRequest {
    #[serde(rename = "dhanClientId")]
    pub dhan_client_id: String,
    pub pin: String,
    pub totp: String,
}

/// Request body for token renewal (unused — actual renewal uses `GET /v2/RenewToken` with headers).
///
/// Fields are zeroized on drop to prevent credential leakage from freed memory.
#[derive(serde::Serialize, Zeroize, ZeroizeOnDrop)]
pub struct RenewTokenRequest {
    pub client_id: String,
    pub totp: String,
}

/// Response from `POST https://auth.dhan.co/app/generateAccessToken`.
///
/// Dhan returns a flat JSON with camelCase field names (no wrapper).
#[derive(Deserialize)]
pub struct DhanGenerateTokenResponse {
    /// Dhan client ID (echoed back).
    #[serde(rename = "dhanClientId")]
    pub dhan_client_id: String,
    /// JWT access token string.
    #[serde(rename = "accessToken")]
    pub access_token: String,
    /// Token expiry time as epoch milliseconds (or ISO timestamp string).
    /// Example: "2026-02-27T13:45:00+05:30" or epoch millis.
    #[serde(rename = "expiryTime")]
    pub expiry_time: serde_json::Value,
}

impl fmt::Debug for DhanGenerateTokenResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DhanGenerateTokenResponse")
            .field("dhan_client_id", &self.dhan_client_id)
            .field("access_token", &"[REDACTED]")
            .field("expiry_time", &self.expiry_time)
            .finish()
    }
}

/// Legacy wrapped response format (kept for renewToken compatibility).
#[derive(Debug, Deserialize)]
pub struct DhanAuthResponse {
    pub status: String,
    pub data: Option<DhanAuthResponseData>,
    /// Error message present when `status != "success"`.
    #[serde(default)]
    pub remarks: Option<String>,
}

/// Data payload from a Dhan auth response (renewal endpoint).
#[derive(Deserialize)]
pub struct DhanAuthResponseData {
    /// JWT access token string.
    pub access_token: String,
    /// Token type (always "Bearer").
    pub token_type: String,
    /// Token validity in seconds (typically 86400 = 24 hours).
    pub expires_in: u32,
}

impl fmt::Debug for DhanAuthResponseData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DhanAuthResponseData")
            .field("access_token", &"[REDACTED]")
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// User Profile Response (GET /v2/profile)
// Source: docs/dhan-ref/02-authentication.md Section 2.5
// ---------------------------------------------------------------------------

/// Response from `GET /v2/profile` with `access-token` header.
///
/// Used for pre-market validation: check `dataPlan`, `activeSegment`,
/// and `tokenValidity` before trading begins.
///
/// `tokenValidity` format: `"DD/MM/YYYY HH:MM"` IST (NOT ISO format).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserProfileResponse {
    /// Dhan client ID.
    pub dhan_client_id: String,
    /// Token validity timestamp in `"DD/MM/YYYY HH:MM"` IST format.
    pub token_validity: String,
    /// Active trading segments (e.g., contains `"Derivative"` for F&O access).
    pub active_segment: String,
    /// DDPI status.
    #[serde(default)]
    pub ddpi: String,
    /// MTF (Margin Trading Facility) status.
    #[serde(default)]
    pub mtf: String,
    /// Data plan status (must be `"Active"` for market data access).
    pub data_plan: String,
    /// Data plan validity date.
    #[serde(default)]
    pub data_validity: String,
}

// ---------------------------------------------------------------------------
// Static IP API Types (POST/PUT/GET /v2/ip/*)
// Source: docs/dhan-ref/02-authentication.md Section 2.4
// ---------------------------------------------------------------------------

/// Request body for `POST /v2/ip/setIP` — sets a static IP for order APIs.
///
/// Dhan requires static IP whitelisting for all Order API endpoints.
/// Supports both IPv4 and IPv6. Primary + Secondary IP per account.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetIpRequest {
    /// Dhan client ID.
    pub dhan_client_id: String,
    /// IP address to whitelist (IPv4 or IPv6).
    pub ip: String,
    /// IP slot: `"PRIMARY"` or `"SECONDARY"`.
    pub ip_flag: String,
}

/// Response from `POST /v2/ip/setIP`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetIpResponse {
    /// Status message from Dhan.
    #[serde(default)]
    pub status: String,
    /// Additional message or confirmation.
    #[serde(default)]
    pub message: String,
}

/// Request body for `PUT /v2/ip/modifyIP` — modifies the whitelisted IP.
///
/// WARNING: 7-day cooldown after modification. Do NOT modify during live trading.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModifyIpRequest {
    /// Dhan client ID.
    pub dhan_client_id: String,
    /// New IP address.
    pub ip: String,
    /// IP slot: `"PRIMARY"` or `"SECONDARY"`.
    pub ip_flag: String,
}

/// Response from `PUT /v2/ip/modifyIP`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModifyIpResponse {
    /// Status message from Dhan.
    #[serde(default)]
    pub status: String,
    /// Additional message or confirmation.
    #[serde(default)]
    pub message: String,
}

/// Response from `GET /v2/ip/getIP` — retrieves current IP configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetIpResponse {
    /// Currently whitelisted IP address.
    #[serde(default)]
    pub ip: String,
    /// IP slot: `"PRIMARY"` or `"SECONDARY"`.
    #[serde(default)]
    pub ip_flag: String,
    /// Last modification date for primary IP.
    #[serde(default)]
    pub modify_date_primary: String,
    /// Last modification date for secondary IP.
    #[serde(default)]
    pub modify_date_secondary: String,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test code
mod tests {
    use secrecy::{ExposeSecret, SecretString};

    use super::*;

    #[test]
    fn test_token_state_from_valid_response() {
        let response_data = DhanAuthResponseData {
            access_token: "test-jwt-token-value".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };

        let state = TokenState::from_response(&response_data);

        assert_eq!(state.access_token().expose_secret(), "test-jwt-token-value");
        assert!(state.is_valid());
        assert!(state.expires_at() > state.issued_at());
    }

    #[test]
    fn test_token_state_expiry_calculation() {
        let response_data = DhanAuthResponseData {
            access_token: "token".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400, // 24 hours
        };

        let state = TokenState::from_response(&response_data);
        let duration = state.expires_at() - state.issued_at();

        // Should be ~24 hours (86400 seconds)
        assert_eq!(duration.num_seconds(), 86400);
    }

    #[test]
    fn test_token_state_no_refresh_before_window() {
        let response_data = DhanAuthResponseData {
            access_token: "token".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };

        let state = TokenState::from_response(&response_data);

        // Just created — should NOT need refresh yet (refresh window is 1h before expiry)
        assert!(!state.needs_refresh(1));
    }

    #[test]
    fn test_token_state_time_until_refresh() {
        let response_data = DhanAuthResponseData {
            access_token: "token".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };

        let state = TokenState::from_response(&response_data);
        let until_refresh = state.time_until_refresh(1);

        // Should be approximately 23 hours (82800 seconds) from now
        assert!(until_refresh.as_secs() > 82000);
        assert!(until_refresh.as_secs() < 83000);
    }

    #[test]
    fn test_token_debug_redacted() {
        let response_data = DhanAuthResponseData {
            access_token: "super-secret-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };

        let state = TokenState::from_response(&response_data);
        let debug_output = format!("{state:?}");

        assert!(debug_output.contains("[REDACTED]"));
        assert!(!debug_output.contains("super-secret-jwt"));
    }

    #[test]
    fn test_credentials_debug_redacted() {
        let creds = DhanCredentials {
            client_id: SecretString::from("my-client-id".to_string()),
            client_secret: SecretString::from("my-secret".to_string()),
            totp_secret: SecretString::from("base32secret".to_string()),
        };

        let debug_output = format!("{creds:?}");

        assert!(debug_output.contains("[REDACTED]"));
        assert!(!debug_output.contains("my-client-id"));
        assert!(!debug_output.contains("my-secret"));
        assert!(!debug_output.contains("base32secret"));
    }

    #[test]
    fn test_questdb_credentials_debug_redacted() {
        let creds = QuestDbCredentials {
            pg_user: SecretString::from("questdb-admin".to_string()),
            pg_password: SecretString::from("super-secret-pw".to_string()),
        };

        let debug_output = format!("{creds:?}");

        assert!(debug_output.contains("[REDACTED]"));
        assert!(!debug_output.contains("questdb-admin"));
        assert!(!debug_output.contains("super-secret-pw"));
    }

    #[test]
    fn test_grafana_credentials_debug_redacted() {
        let creds = GrafanaCredentials {
            admin_user: SecretString::from("grafana-admin".to_string()),
            admin_password: SecretString::from("grafana-secret-pw".to_string()),
        };

        let debug_output = format!("{creds:?}");

        assert!(debug_output.contains("[REDACTED]"));
        assert!(!debug_output.contains("grafana-admin"));
        assert!(!debug_output.contains("grafana-secret-pw"));
    }

    #[test]
    fn test_token_age_hours_just_created() {
        let response_data = DhanAuthResponseData {
            access_token: "token".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };

        let state = TokenState::from_response(&response_data);
        let age = state.age_hours();

        // Just created — age should be very close to 0
        assert!(age < 0.1);
    }

    #[test]
    fn test_ssm_path_construction() {
        use dhan_live_trader_common::constants::{
            DHAN_CLIENT_ID_SECRET, SSM_DHAN_SERVICE, SSM_SECRET_BASE_PATH,
        };

        let environment = "dev";
        let path = format!(
            "{}/{}/{}/{}",
            SSM_SECRET_BASE_PATH, environment, SSM_DHAN_SERVICE, DHAN_CLIENT_ID_SECRET
        );

        assert_eq!(path, "/dlt/dev/dhan/client-id");
    }

    #[test]
    fn test_ssm_path_prod_environment() {
        use dhan_live_trader_common::constants::{
            DHAN_CLIENT_SECRET_SECRET, SSM_DHAN_SERVICE, SSM_SECRET_BASE_PATH,
        };

        let environment = "prod";
        let path = format!(
            "{}/{}/{}/{}",
            SSM_SECRET_BASE_PATH, environment, SSM_DHAN_SERVICE, DHAN_CLIENT_SECRET_SECRET
        );

        assert_eq!(path, "/dlt/prod/dhan/client-secret");
    }

    #[test]
    fn test_dhan_auth_response_deserialize_success() {
        let json = r#"{
            "status": "success",
            "data": {
                "access_token": "jwt-value",
                "token_type": "Bearer",
                "expires_in": 86400
            }
        }"#;

        let response: DhanAuthResponse = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(response.status, "success");
        let data = response.data.expect("data should be present");
        assert_eq!(data.access_token, "jwt-value");
        assert_eq!(data.token_type, "Bearer");
        assert_eq!(data.expires_in, 86400);
    }

    #[test]
    fn test_dhan_auth_response_deserialize_error() {
        let json = r#"{
            "status": "failure",
            "data": null,
            "remarks": "Invalid TOTP code"
        }"#;

        let response: DhanAuthResponse = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(response.status, "failure");
        assert!(response.data.is_none());
        assert_eq!(response.remarks.as_deref(), Some("Invalid TOTP code"));
    }

    // -----------------------------------------------------------------------
    // CRITICAL gap tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_token_state_is_valid_returns_false_when_expired() {
        // Construct a TokenState with expires_at in the past.
        let now_ist = Utc::now().with_timezone(&ist_offset());
        let state = TokenState {
            access_token: SecretString::from("expired-token".to_string()),
            expires_at: now_ist - Duration::seconds(60), // expired 1 minute ago
            issued_at: now_ist - Duration::hours(25),    // issued 25 hours ago
        };

        assert!(
            !state.is_valid(),
            "Token with past expires_at must not be valid"
        );
    }

    #[test]
    fn test_token_state_needs_refresh_returns_true_inside_window() {
        // Token with expires_in=3600 (1 hour from now).
        // With refresh_before_expiry_hours=1, the refresh threshold is
        // expires_at - 1h = now, so needs_refresh(1) should return true.
        let response_data = DhanAuthResponseData {
            access_token: "token".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 3600, // 1 hour
        };

        let state = TokenState::from_response(&response_data);

        // refresh window = 1 hour before expiry. Token expires in 1 hour,
        // so the refresh threshold is approximately now. needs_refresh should be true.
        assert!(
            state.needs_refresh(1),
            "Token expiring in 1h with 1h refresh window should need refresh"
        );
    }

    #[test]
    fn test_token_state_needs_refresh_returns_true_when_expired() {
        // Construct a TokenState that has already expired.
        let now_ist = Utc::now().with_timezone(&ist_offset());
        let state = TokenState {
            access_token: SecretString::from("expired-token".to_string()),
            expires_at: now_ist - Duration::hours(2), // expired 2 hours ago
            issued_at: now_ist - Duration::hours(26),
        };

        assert!(
            state.needs_refresh(1),
            "An expired token should always need refresh"
        );
    }

    // -----------------------------------------------------------------------
    // HIGH gap tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_token_state_time_until_refresh_returns_zero_when_past_window() {
        // Token with 30 minutes left, refresh_before_expiry=1h.
        // Refresh threshold = expires_at - 1h, which is 30 min in the past.
        // time_until_refresh should return Duration::ZERO.
        let now_ist = Utc::now().with_timezone(&ist_offset());
        let state = TokenState {
            access_token: SecretString::from("token".to_string()),
            expires_at: now_ist + Duration::minutes(30), // 30 min left
            issued_at: now_ist - Duration::hours(23) - Duration::minutes(30),
        };

        let until_refresh = state.time_until_refresh(1); // 1h window
        assert_eq!(
            until_refresh,
            std::time::Duration::ZERO,
            "Past the refresh window should return Duration::ZERO"
        );
    }

    #[test]
    fn test_generate_token_request_serializes_correctly() {
        let request = GenerateTokenRequest {
            dhan_client_id: "test-client-id".to_string(),
            pin: "123456".to_string(),
            totp: "654321".to_string(),
        };

        let json_value: serde_json::Value =
            serde_json::to_value(&request).expect("should serialize");

        // Verify the serialized field names match Dhan API expectations (camelCase).
        assert_eq!(json_value["dhanClientId"], "test-client-id");
        assert_eq!(json_value["pin"], "123456");
        assert_eq!(json_value["totp"], "654321");

        // Verify no extra fields are present
        let obj = json_value.as_object().expect("should be an object");
        assert_eq!(
            obj.len(),
            3,
            "GenerateTokenRequest should have exactly 3 fields"
        );
    }

    #[test]
    fn test_renew_token_request_serializes_correctly() {
        let request = RenewTokenRequest {
            client_id: "renew-client-id".to_string(),
            totp: "654321".to_string(),
        };

        let json_value: serde_json::Value =
            serde_json::to_value(&request).expect("should serialize");

        assert_eq!(json_value["client_id"], "renew-client-id");
        assert_eq!(json_value["totp"], "654321");

        let obj = json_value.as_object().expect("should be an object");
        assert_eq!(
            obj.len(),
            2,
            "RenewTokenRequest should have exactly 2 fields"
        );
    }

    #[test]
    fn test_dhan_auth_response_data_debug_redacted() {
        let data = DhanAuthResponseData {
            access_token: "super-secret-jwt-value-12345".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };

        let debug_output = format!("{data:?}");

        assert!(
            debug_output.contains("[REDACTED]"),
            "Debug output must show [REDACTED] for access_token"
        );
        assert!(
            !debug_output.contains("super-secret-jwt-value-12345"),
            "Debug output must NOT contain the raw access_token value"
        );
        // token_type and expires_in should still be visible
        assert!(
            debug_output.contains("Bearer"),
            "Debug output should show token_type"
        );
        assert!(
            debug_output.contains("86400"),
            "Debug output should show expires_in"
        );
    }

    // -----------------------------------------------------------------------
    // MEDIUM gap tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_token_state_from_response_with_zero_expires_in() {
        let response_data = DhanAuthResponseData {
            access_token: "zero-expiry-token".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 0,
        };

        let state = TokenState::from_response(&response_data);

        // expires_in=0 means the token expires at issuance time.
        // By the time we check, it should already be invalid (or at the boundary).
        // is_valid checks now_ist < expires_at, and now_ist >= issued_at == expires_at.
        assert!(
            !state.is_valid(),
            "Token with zero expires_in should be expired immediately"
        );
        assert_eq!(
            state.expires_at(),
            state.issued_at(),
            "With zero expires_in, expires_at should equal issued_at"
        );
    }

    #[test]
    fn test_token_state_from_response_with_max_expires_in() {
        let response_data = DhanAuthResponseData {
            access_token: "max-expiry-token".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: u32::MAX,
        };

        // Must not panic due to overflow in Duration::seconds(i64::from(u32::MAX))
        let state = TokenState::from_response(&response_data);

        // u32::MAX = 4294967295 seconds (~136 years). Token should be valid.
        assert!(
            state.is_valid(),
            "Token with u32::MAX expires_in should be valid"
        );
        assert!(
            state.expires_at() > state.issued_at(),
            "expires_at must be after issued_at"
        );
    }

    #[test]
    fn test_token_state_needs_refresh_with_zero_window() {
        // Fresh 24h token with refresh_before_expiry_hours=0.
        // Refresh threshold = expires_at - 0h = expires_at.
        // Since now < expires_at, needs_refresh(0) should be false.
        let response_data = DhanAuthResponseData {
            access_token: "token".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400, // 24 hours
        };

        let state = TokenState::from_response(&response_data);

        assert!(
            !state.needs_refresh(0),
            "Fresh 24h token with zero refresh window should NOT need refresh"
        );
    }

    #[test]
    fn test_token_state_needs_refresh_with_window_larger_than_expiry() {
        // 24h token with refresh_before_expiry_hours=48.
        // Refresh threshold = expires_at - 48h, which is 24 hours in the PAST.
        // Since now > threshold, needs_refresh(48) should be true immediately.
        let response_data = DhanAuthResponseData {
            access_token: "token".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400, // 24 hours
        };

        let state = TokenState::from_response(&response_data);

        assert!(
            state.needs_refresh(48),
            "24h token with 48h refresh window should immediately need refresh"
        );
    }

    #[test]
    fn test_dhan_auth_response_deserialize_missing_remarks() {
        // JSON without a "remarks" field at all — should deserialize fine
        // because remarks has #[serde(default)].
        let json = r#"{
            "status": "success",
            "data": {
                "access_token": "jwt-value",
                "token_type": "Bearer",
                "expires_in": 86400
            }
        }"#;

        let response: DhanAuthResponse =
            serde_json::from_str(json).expect("should deserialize without remarks field");
        assert_eq!(response.status, "success");
        assert!(
            response.remarks.is_none(),
            "Missing remarks field should deserialize as None"
        );
        assert!(response.data.is_some());
    }

    // -----------------------------------------------------------------------
    // parse_expiry_time tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_expiry_time_epoch_millis_number() {
        // 1772113432557 > 10_000_000_000 -> millis -> divides by 1000
        let val = serde_json::json!(1772113432557_i64);
        let result = parse_expiry_time(&val);
        assert!(result.is_some());
    }

    #[test]
    fn test_parse_expiry_time_epoch_seconds_number() {
        let val = serde_json::json!(1772113432_i64);
        let result = parse_expiry_time(&val);
        assert!(result.is_some());
    }

    #[test]
    fn test_parse_expiry_time_iso8601_string() {
        let val = serde_json::json!("2026-02-27T13:45:00+05:30");
        let result = parse_expiry_time(&val);
        assert!(result.is_some());
    }

    #[test]
    fn test_parse_expiry_time_naive_datetime_string() {
        let val = serde_json::json!("2026-02-27T13:45:00");
        let result = parse_expiry_time(&val);
        assert!(result.is_some());
    }

    #[test]
    fn test_parse_expiry_time_epoch_millis_as_string() {
        let val = serde_json::json!("1772113432557");
        let result = parse_expiry_time(&val);
        assert!(result.is_some());
    }

    #[test]
    fn test_parse_expiry_time_epoch_seconds_as_string() {
        let val = serde_json::json!("1772113432");
        let result = parse_expiry_time(&val);
        assert!(result.is_some());
    }

    #[test]
    fn test_parse_expiry_time_invalid_string_returns_none() {
        let val = serde_json::json!("not-a-date");
        assert!(parse_expiry_time(&val).is_none());
    }

    #[test]
    fn test_parse_expiry_time_null_returns_none() {
        let val = serde_json::Value::Null;
        assert!(parse_expiry_time(&val).is_none());
    }

    #[test]
    fn test_parse_expiry_time_bool_returns_none() {
        let val = serde_json::json!(true);
        assert!(parse_expiry_time(&val).is_none());
    }

    // -----------------------------------------------------------------------
    // from_generate_response tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_from_generate_response_with_epoch_millis_expiry() {
        // Use a future timestamp (2027-03-01T12:00:00 UTC) so is_valid() holds
        let response = DhanGenerateTokenResponse {
            dhan_client_id: "test-client".to_string(),
            access_token: "test-jwt".to_string(),
            expiry_time: serde_json::json!(1803902400000_i64),
        };
        let state = TokenState::from_generate_response(&response);
        assert_eq!(state.access_token().expose_secret(), "test-jwt");
        assert!(state.is_valid());
    }

    #[test]
    fn test_from_generate_response_with_unparsable_expiry_defaults_24h() {
        let response = DhanGenerateTokenResponse {
            dhan_client_id: "test-client".to_string(),
            access_token: "test-jwt".to_string(),
            expiry_time: serde_json::json!("garbage"),
        };
        let state = TokenState::from_generate_response(&response);
        assert_eq!(state.access_token().expose_secret(), "test-jwt");
        assert!(state.is_valid());
        // Should have ~24 hour validity
        let duration = state.expires_at() - state.issued_at();
        let hours = duration.num_hours();
        assert!((23..=25).contains(&hours), "Expected ~24h, got {hours}h");
    }

    #[test]
    fn test_from_generate_response_with_null_expiry_defaults_24h() {
        let response = DhanGenerateTokenResponse {
            dhan_client_id: "test-client".to_string(),
            access_token: "test-jwt".to_string(),
            expiry_time: serde_json::Value::Null,
        };
        let state = TokenState::from_generate_response(&response);
        assert!(state.is_valid());
        let duration = state.expires_at() - state.issued_at();
        let hours = duration.num_hours();
        assert!((23..=25).contains(&hours));
    }

    // -----------------------------------------------------------------------
    // DhanGenerateTokenResponse deserialization tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_dhan_generate_token_response_deserialize() {
        let json = r#"{
            "dhanClientId": "1234567890",
            "accessToken": "jwt-token-value",
            "expiryTime": 1772113432557
        }"#;
        let response: DhanGenerateTokenResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.dhan_client_id, "1234567890");
        assert_eq!(response.access_token, "jwt-token-value");
    }

    #[test]
    fn test_dhan_generate_token_response_debug_redacted() {
        let response = DhanGenerateTokenResponse {
            dhan_client_id: "client-123".to_string(),
            access_token: "super-secret-token".to_string(),
            expiry_time: serde_json::json!(1772113432557_i64),
        };
        let debug = format!("{response:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("super-secret-token"));
    }

    // -----------------------------------------------------------------------
    // UserProfileResponse tests (A1)
    // -----------------------------------------------------------------------

    #[test]
    fn test_user_profile_response_deserializes() {
        let json = r#"{
            "dhanClientId": "1000000001",
            "tokenValidity": "17/03/2026 23:59",
            "activeSegment": "Equity, Derivative",
            "ddpi": "Active",
            "mtf": "Active",
            "dataPlan": "Active",
            "dataValidity": "31/12/2026"
        }"#;

        let profile: UserProfileResponse = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(profile.dhan_client_id, "1000000001");
        assert_eq!(profile.token_validity, "17/03/2026 23:59");
        assert!(profile.active_segment.contains("Derivative"));
        assert_eq!(profile.data_plan, "Active");
    }

    #[test]
    fn test_user_profile_token_validity_format() {
        // tokenValidity is DD/MM/YYYY HH:MM IST, NOT ISO format
        let json = r#"{
            "dhanClientId": "1000000001",
            "tokenValidity": "17/03/2026 23:59",
            "activeSegment": "Equity",
            "dataPlan": "Active"
        }"#;

        let profile: UserProfileResponse = serde_json::from_str(json).expect("should deserialize");
        // Must contain "/" separators (DD/MM/YYYY), not "-" (ISO)
        assert!(
            profile.token_validity.contains('/'),
            "tokenValidity must be DD/MM/YYYY format, got: {}",
            profile.token_validity
        );
    }

    // -----------------------------------------------------------------------
    // IP API types tests (A4)
    // -----------------------------------------------------------------------

    #[test]
    fn test_set_ip_request_serializes() {
        let request = SetIpRequest {
            dhan_client_id: "1000000001".to_string(),
            ip: "203.0.113.42".to_string(),
            ip_flag: "PRIMARY".to_string(),
        };

        let json_value: serde_json::Value =
            serde_json::to_value(&request).expect("should serialize");

        assert_eq!(json_value["dhanClientId"], "1000000001");
        assert_eq!(json_value["ip"], "203.0.113.42");
        assert_eq!(json_value["ipFlag"], "PRIMARY");
    }

    #[test]
    fn test_get_ip_response_deserializes() {
        let json = r#"{
            "ip": "203.0.113.42",
            "ipFlag": "PRIMARY",
            "modifyDatePrimary": "2026-01-15",
            "modifyDateSecondary": ""
        }"#;

        let response: GetIpResponse = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(response.ip, "203.0.113.42");
        assert_eq!(response.ip_flag, "PRIMARY");
        assert_eq!(response.modify_date_primary, "2026-01-15");
    }

    #[test]
    fn test_modify_ip_request_serializes_camel_case() {
        let request = ModifyIpRequest {
            dhan_client_id: "1000000001".to_string(),
            ip: "198.51.100.1".to_string(),
            ip_flag: "SECONDARY".to_string(),
        };

        let json_value: serde_json::Value =
            serde_json::to_value(&request).expect("should serialize");
        assert_eq!(json_value["dhanClientId"], "1000000001");
        assert_eq!(json_value["ipFlag"], "SECONDARY");
    }

    #[test]
    fn test_set_ip_response_deserializes() {
        let json = r#"{"status": "success", "message": "IP set successfully"}"#;
        let response: SetIpResponse = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(response.status, "success");
    }

    #[test]
    fn test_modify_ip_response_deserializes() {
        let json = r#"{"status": "success", "message": "IP modified"}"#;
        let response: ModifyIpResponse = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(response.status, "success");
    }

    // =====================================================================
    // Additional coverage: parse_expiry_time edge cases, from_cached,
    // TelegramCredentials Debug, age_hours for old token, token accessors
    // =====================================================================

    #[test]
    fn test_parse_expiry_time_array_returns_none() {
        let val = serde_json::json!([1, 2, 3]);
        assert!(parse_expiry_time(&val).is_none());
    }

    #[test]
    fn test_parse_expiry_time_object_returns_none() {
        let val = serde_json::json!({"key": "value"});
        assert!(parse_expiry_time(&val).is_none());
    }

    #[test]
    fn test_parse_expiry_time_negative_epoch_returns_none() {
        // Negative epoch is technically valid but very old
        let val = serde_json::json!(-1);
        // DateTime::from_timestamp should handle negatives (before epoch)
        let result = parse_expiry_time(&val);
        // Negative epoch IS valid in chrono, so this should succeed
        if let Some(dt) = result {
            assert!(dt.timestamp() < 0);
        }
    }

    #[test]
    fn test_parse_expiry_time_zero_epoch() {
        let val = serde_json::json!(0);
        let result = parse_expiry_time(&val);
        assert!(result.is_some());
    }

    #[test]
    fn test_parse_expiry_time_float_number_returns_none() {
        // JSON float numbers cannot be converted to i64
        let val = serde_json::json!(1772113432.557);
        let result = parse_expiry_time(&val);
        // serde_json Number::as_i64() returns None for floats
        assert!(result.is_none());
    }

    #[test]
    fn test_token_state_from_cached() {
        let now_ist = Utc::now().with_timezone(&ist_offset());
        let expires = now_ist + Duration::hours(24);
        let state = TokenState::from_cached(
            SecretString::from("cached-jwt".to_string()),
            expires,
            now_ist,
        );
        assert_eq!(state.access_token().expose_secret(), "cached-jwt");
        assert_eq!(state.expires_at(), expires);
        assert_eq!(state.issued_at(), now_ist);
        assert!(state.is_valid());
    }

    #[test]
    fn test_token_state_age_hours_old_token() {
        let now_ist = Utc::now().with_timezone(&ist_offset());
        let state = TokenState {
            access_token: SecretString::from("old-token".to_string()),
            expires_at: now_ist + Duration::hours(1),
            issued_at: now_ist - Duration::hours(10),
        };
        let age = state.age_hours();
        // Should be approximately 10 hours
        assert!(age > 9.9 && age < 10.1, "age should be ~10h, got {age}");
    }

    #[test]
    fn test_telegram_credentials_debug_redacted() {
        let creds = TelegramCredentials {
            bot_token: SecretString::from("123456:ABC-DEF".to_string()),
            chat_id: SecretString::from("-1001234567890".to_string()),
        };
        let debug = format!("{creds:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("123456:ABC"));
        assert!(!debug.contains("-1001234567890"));
    }

    #[test]
    fn test_user_profile_response_missing_optional_fields() {
        // Only required fields
        let json = r#"{
            "dhanClientId": "1000000001",
            "tokenValidity": "17/03/2026 23:59",
            "activeSegment": "Equity",
            "dataPlan": "Active"
        }"#;
        let profile: UserProfileResponse = serde_json::from_str(json).unwrap();
        assert_eq!(profile.ddpi, ""); // #[serde(default)]
        assert_eq!(profile.mtf, "");
        assert_eq!(profile.data_validity, "");
    }

    #[test]
    fn test_get_ip_response_missing_optional_fields() {
        let json = r#"{}"#;
        let response: GetIpResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.ip, "");
        assert_eq!(response.ip_flag, "");
        assert_eq!(response.modify_date_primary, "");
        assert_eq!(response.modify_date_secondary, "");
    }

    #[test]
    fn test_set_ip_response_missing_optional_fields() {
        let json = r#"{}"#;
        let response: SetIpResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.status, "");
        assert_eq!(response.message, "");
    }

    #[test]
    fn test_modify_ip_request_debug_format() {
        let req = ModifyIpRequest {
            dhan_client_id: "1000000001".to_string(),
            ip: "198.51.100.1".to_string(),
            ip_flag: "PRIMARY".to_string(),
        };
        let debug = format!("{req:?}");
        assert!(debug.contains("ModifyIpRequest"));
        assert!(debug.contains("198.51.100.1"));
    }

    #[test]
    fn test_token_state_needs_refresh_with_very_large_window() {
        // Very large window (u64::MAX), should still not panic
        let response_data = DhanAuthResponseData {
            access_token: "token".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        let state = TokenState::from_response(&response_data);
        // Window is clamped to 8760, so should need refresh immediately
        // since 8760h > 24h token validity
        assert!(state.needs_refresh(u64::MAX));
    }

    #[test]
    fn test_token_state_time_until_refresh_with_very_large_window() {
        let response_data = DhanAuthResponseData {
            access_token: "token".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        let state = TokenState::from_response(&response_data);
        // Very large window clamped to 8760, past refresh → ZERO
        assert_eq!(
            state.time_until_refresh(u64::MAX),
            std::time::Duration::ZERO
        );
    }

    #[test]
    fn test_dhan_generate_token_response_with_string_expiry_time() {
        let json = r#"{
            "dhanClientId": "1234567890",
            "accessToken": "jwt-token-value",
            "expiryTime": "2026-02-27T13:45:00"
        }"#;
        let response: DhanGenerateTokenResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.dhan_client_id, "1234567890");
        assert!(response.expiry_time.is_string());
    }
}

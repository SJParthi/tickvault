//! Authentication types for the Dhan token lifecycle.
//!
//! All sensitive values wrapped in `Secret<String>` — Debug prints `[REDACTED]`.
//! Token memory zeroized on drop via `zeroize` crate.

use std::fmt;

use chrono::{DateTime, Duration, FixedOffset, Utc};
use secrecy::SecretString;
use serde::Deserialize;
use zeroize::{Zeroize, ZeroizeOnDrop};

use dhan_live_trader_common::constants::IST_UTC_OFFSET_SECONDS;

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
// Token State (stored in ArcSwap)
// ---------------------------------------------------------------------------

/// Active JWT token state held atomically via arc-swap.
///
/// Consumers call `arc_swap.load()` for O(1) reads on every WebSocket
/// frame and REST API call. The token is swapped atomically on renewal.
///
/// Memory is zeroized on drop to prevent token leakage from freed memory.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct TokenState {
    /// JWT access token value.
    #[zeroize(skip)]
    access_token: SecretString,

    /// When this token expires (IST).
    #[zeroize(skip)]
    expires_at: DateTime<FixedOffset>,

    /// When this token was issued (IST).
    #[zeroize(skip)]
    issued_at: DateTime<FixedOffset>,
}

impl TokenState {
    /// Creates a new `TokenState` from a Dhan auth response.
    pub fn from_response(response: &DhanAuthResponseData) -> Self {
        let ist =
            FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS).expect("IST offset is always valid");
        let now_ist = Utc::now().with_timezone(&ist);
        let expires_at = now_ist + Duration::seconds(i64::from(response.expires_in));

        Self {
            access_token: SecretString::from(response.access_token.clone()),
            expires_at,
            issued_at: now_ist,
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
        let ist =
            FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS).expect("IST offset is always valid");
        let now_ist = Utc::now().with_timezone(&ist);
        now_ist < self.expires_at
    }

    /// Returns `true` if the token is within the refresh window.
    ///
    /// The refresh window starts at `token_validity - refresh_before_expiry`
    /// hours after issuance.
    pub fn needs_refresh(&self, refresh_before_expiry_hours: u64) -> bool {
        let ist =
            FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS).expect("IST offset is always valid");
        let now_ist = Utc::now().with_timezone(&ist);
        let refresh_threshold =
            self.expires_at - Duration::hours(i64::from(refresh_before_expiry_hours as u32));
        now_ist >= refresh_threshold
    }

    /// Returns the duration until this token should be refreshed.
    ///
    /// Returns zero if already past the refresh window.
    pub fn time_until_refresh(&self, refresh_before_expiry_hours: u64) -> std::time::Duration {
        let ist =
            FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS).expect("IST offset is always valid");
        let now_ist = Utc::now().with_timezone(&ist);
        let refresh_at =
            self.expires_at - Duration::hours(i64::from(refresh_before_expiry_hours as u32));
        if now_ist >= refresh_at {
            std::time::Duration::ZERO
        } else {
            (refresh_at - now_ist)
                .to_std()
                .unwrap_or(std::time::Duration::ZERO)
        }
    }

    /// Returns the token age in hours (for logging only).
    pub fn age_hours(&self) -> f64 {
        let ist =
            FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS).expect("IST offset is always valid");
        let now_ist = Utc::now().with_timezone(&ist);
        let age = now_ist - self.issued_at;
        age.num_minutes() as f64 / 60.0
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

/// Request body for `POST /v2/generateAccessToken`.
#[derive(serde::Serialize)]
pub struct GenerateTokenRequest {
    pub client_id: String,
    pub client_secret: String,
    pub totp: String,
}

/// Request body for `POST /v2/renewToken`.
#[derive(serde::Serialize)]
pub struct RenewTokenRequest {
    pub client_id: String,
    pub totp: String,
}

/// Top-level response from Dhan auth endpoints.
#[derive(Debug, Deserialize)]
pub struct DhanAuthResponse {
    pub status: String,
    pub data: Option<DhanAuthResponseData>,
    /// Error message present when `status != "success"`.
    #[serde(default)]
    pub remarks: Option<String>,
}

/// Data payload from a successful Dhan auth response.
#[derive(Debug, Deserialize)]
pub struct DhanAuthResponseData {
    /// JWT access token string.
    pub access_token: String,
    /// Token type (always "Bearer").
    pub token_type: String,
    /// Token validity in seconds (typically 86400 = 24 hours).
    pub expires_in: u32,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
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
}

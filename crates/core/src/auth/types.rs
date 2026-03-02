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
// IST Helper
// ---------------------------------------------------------------------------

/// Returns the IST fixed offset (UTC+5:30).
///
/// Uses the compile-time constant `IST_UTC_OFFSET_SECONDS` (19800).
/// This always succeeds — the value is within `FixedOffset`'s valid range.
#[allow(clippy::expect_used)] // APPROVED: compile-time provable — 19800 always valid
fn ist_offset() -> FixedOffset {
    FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS).expect("IST offset 19800s is always valid") // APPROVED: compile-time provable constant
}

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
        let expires_at = now_ist + Duration::seconds(i64::from(response.expires_in));

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
            now_ist + Duration::hours(24)
        });

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
        let refresh_threshold = self.expires_at - Duration::hours(hours);
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
        let refresh_at = self.expires_at - Duration::hours(hours);
        if now_ist >= refresh_at {
            std::time::Duration::ZERO
        } else {
            (refresh_at - now_ist)
                .to_std()
                .unwrap_or(std::time::Duration::ZERO)
        }
    }

    /// Returns the token age in hours (for logging only).
    ///
    /// Uses seconds-based precision. Clamps negative durations to zero.
    pub fn age_hours(&self) -> f64 {
        let now_ist = Utc::now().with_timezone(&ist_offset());
        let age = now_ist - self.issued_at;
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

/// Request body for `POST /v2/renewToken` (unused — renewal uses GET with headers).
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
    fn test_from_generate_response_with_unparseable_expiry_defaults_24h() {
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
}

//! TOTP code generation for Dhan 2FA authentication.
//!
//! Generates 6-digit time-based one-time passwords from a base32-encoded
//! secret stored in AWS SSM. Used during initial authentication and
//! token renewal to satisfy Dhan's mandatory 2FA requirement.

use secrecy::{ExposeSecret, SecretString};
use totp_rs::{Algorithm, Secret as TotpSecret, TOTP};
use tracing::instrument;

use dhan_live_trader_common::constants::{TOTP_DIGITS, TOTP_PERIOD_SECS, TOTP_SKEW};
use dhan_live_trader_common::error::ApplicationError;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Generates a TOTP code from the base32-encoded secret.
///
/// Uses SHA-1 algorithm with 6-digit codes and 30-second periods
/// as required by Dhan's authentication system.
///
/// # Arguments
///
/// * `totp_secret` - Base32-encoded TOTP secret from SSM Parameter Store.
///
/// # Returns
///
/// A 6-digit string code valid for the current 30-second window.
///
/// # Errors
///
/// Returns `ApplicationError::TotpGenerationFailed` if the secret is invalid
/// or code generation fails.
#[instrument(skip_all)]
pub fn generate_totp_code(totp_secret: &SecretString) -> Result<String, ApplicationError> {
    let secret_bytes = TotpSecret::Encoded(totp_secret.expose_secret().to_string())
        .to_bytes()
        .map_err(|err| ApplicationError::TotpGenerationFailed {
            reason: format!("invalid base32 secret: {err}"),
        })?;

    let totp = TOTP::new(
        Algorithm::SHA1,
        TOTP_DIGITS,
        TOTP_SKEW,
        TOTP_PERIOD_SECS,
        secret_bytes,
        Some("dhan-live-trader".to_string()),
        "dhan-auth".to_string(),
    )
    .map_err(|err| ApplicationError::TotpGenerationFailed {
        reason: format!("TOTP initialization failed: {err}"),
    })?;

    let code = totp
        .generate_current()
        .map_err(|err| ApplicationError::TotpGenerationFailed {
            reason: format!("code generation failed: {err}"),
        })?;

    Ok(code)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use secrecy::SecretString;

    use super::*;

    /// A valid base32 secret for testing (32 characters = 160 bits).
    const TEST_TOTP_SECRET: &str = "OBWGC2LOFVZXI4TJNZTS243FMNZGK5BN";

    #[test]
    fn test_totp_generates_six_digit_code() {
        let secret = SecretString::from(TEST_TOTP_SECRET.to_string());
        let code = generate_totp_code(&secret).expect("should generate code");

        assert_eq!(code.len(), TOTP_DIGITS);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn test_totp_code_is_deterministic_within_window() {
        let secret = SecretString::from(TEST_TOTP_SECRET.to_string());
        let code1 = generate_totp_code(&secret).expect("should generate code");
        let code2 = generate_totp_code(&secret).expect("should generate code");

        // Two calls within the same 30-second window should produce the same code
        assert_eq!(code1, code2);
    }

    #[test]
    fn test_totp_invalid_base32_returns_error() {
        let secret = SecretString::from("!!!not-valid-base32!!!".to_string());
        let result = generate_totp_code(&secret);

        assert!(result.is_err());
        match result.unwrap_err() {
            ApplicationError::TotpGenerationFailed { reason } => {
                assert!(reason.contains("invalid base32"));
            }
            other => panic!("expected TotpGenerationFailed, got: {other}"),
        }
    }

    #[test]
    fn test_totp_empty_secret_returns_error() {
        let secret = SecretString::from(String::new());
        let result = generate_totp_code(&secret);

        assert!(result.is_err());
    }
}

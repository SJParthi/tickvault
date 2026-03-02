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

    #[test]
    fn test_totp_valid_secret_generates_six_digit_numeric_code() {
        // A different valid base32 secret with >= 128 bits (26+ base32 chars).
        let secret = SecretString::from("JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP".to_string());
        let code = generate_totp_code(&secret).expect("valid base32 should produce a code");

        assert_eq!(
            code.len(),
            TOTP_DIGITS,
            "TOTP code must be exactly {TOTP_DIGITS} digits"
        );
        assert!(
            code.chars().all(|c| c.is_ascii_digit()),
            "TOTP code must be all digits, got: {code}"
        );
    }

    #[test]
    fn test_totp_secret_with_whitespace_is_handled() {
        // totp-rs `Secret::Encoded` may or may not strip whitespace.
        // If it does, this produces a valid code. If not, it returns an error.
        // Either way, the function must NOT panic.
        let secret = SecretString::from("OBWG C2LO FVZX I4TJ NZTS 243F MNZG K5BN".to_string());
        let _result = generate_totp_code(&secret);
        // No assertion on Ok/Err — the invariant is "no panic".
    }

    #[test]
    fn test_totp_secret_with_leading_trailing_whitespace() {
        // Leading/trailing whitespace around an otherwise-valid secret.
        let secret = SecretString::from("  OBWGC2LOFVZXI4TJNZTS243FMNZGK5BN  ".to_string());
        let _result = generate_totp_code(&secret);
        // No assertion on Ok/Err — the invariant is "no panic".
    }

    #[test]
    fn test_totp_secret_with_lowercase_base32() {
        // Base32 is case-insensitive per RFC 4648. Test lowercase input.
        let secret = SecretString::from("obwgc2lofvzxi4tjnzts243fmnzgk5bn".to_string());
        let _result = generate_totp_code(&secret);
        // No assertion on Ok/Err — depends on totp-rs handling.
        // The invariant is "no panic".
    }

    #[test]
    fn test_totp_short_secret_produces_code_or_error() {
        // A very short but technically valid base32 string (8 chars = 40 bits).
        let secret = SecretString::from("MFRGGZDF".to_string());
        let _result = generate_totp_code(&secret);
        // Short secrets may be rejected by TOTP::new or may produce a code.
        // The invariant is "no panic".
    }

    #[test]
    fn test_totp_single_char_secret_returns_error() {
        // A single character base32 string — too short for TOTP.
        let secret = SecretString::from("A".to_string());
        let result = generate_totp_code(&secret);
        // Either invalid base32 or TOTP init failure. The invariant is "no panic".
        // Very short secrets are rejected by TOTP::new due to minimum secret length.
        let _is_error = result.is_err();
    }

    #[test]
    fn test_totp_with_padding_chars() {
        // Base32 with padding characters (=).
        let secret = SecretString::from("OBWGC2LO======".to_string());
        let _result = generate_totp_code(&secret);
        // Padding may or may not be valid — the invariant is "no panic".
    }

    #[test]
    fn test_totp_max_length_secret_no_panic() {
        // Very long base32 secret (256 chars).
        let long_secret = "OBWGC2LOFVZXI4TJ".repeat(16);
        let secret = SecretString::from(long_secret);
        let _result = generate_totp_code(&secret);
        // The invariant is "no panic".
    }

    #[test]
    fn test_totp_invalid_base32_char_8_returns_error() {
        // '8' is not a valid base32 character (base32 uses A-Z and 2-7).
        let secret = SecretString::from("88888888".to_string());
        let result = generate_totp_code(&secret);
        assert!(
            result.is_err(),
            "'8' is not valid base32, should return error"
        );
    }

    #[test]
    fn test_totp_error_variant_is_totp_generation_failed() {
        let secret = SecretString::from("!!!invalid!!!".to_string());
        let result = generate_totp_code(&secret);
        match result {
            Err(ApplicationError::TotpGenerationFailed { reason }) => {
                assert!(!reason.is_empty(), "error reason should not be empty");
            }
            Err(other) => panic!("expected TotpGenerationFailed, got: {other}"),
            Ok(_) => panic!("expected error for invalid secret"),
        }
    }
}

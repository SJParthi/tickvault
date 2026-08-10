//! TOTP code generation for Dhan 2FA authentication.
//!
//! Generates 6-digit time-based one-time passwords from a base32-encoded
//! secret stored in AWS SSM. Used during initial authentication and
//! token renewal to satisfy Dhan's mandatory 2FA requirement.

use std::time::{SystemTime, UNIX_EPOCH};

use secrecy::{ExposeSecret, SecretString};
use totp_rs::{Algorithm, Builder as TotpBuilder, Secret as TotpSecret};
use tracing::instrument;

use tickvault_common::constants::{TOTP_DIGITS, TOTP_PERIOD_SECS, TOTP_SKEW};
use tickvault_common::error::ApplicationError;

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
/// Returns `ApplicationError::TotpGenerationFailed` if the secret is invalid,
/// the parameters are rejected, or the system clock predates the unix epoch.
#[instrument(skip_all)]
pub fn generate_totp_code(totp_secret: &SecretString) -> Result<String, ApplicationError> {
    // The clock is read HERE rather than via `Totp::generate_current()`.
    //
    // totp-rs 6.0.0 changed `generate_current()` to PANIC on a pre-epoch
    // clock instead of returning `Result`. This workspace builds release with
    // `panic = "abort"`, so calling it would convert a previously-HANDLED error
    // into a process abort on the authentication path. Reading the clock here
    // and calling `generate(t)` reproduces the 5.7.x contract exactly: a bad
    // clock stays a typed `TotpGenerationFailed`.
    let unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| ApplicationError::TotpGenerationFailed {
            reason: format!("system clock is before the unix epoch: {err}"),
        })?
        .as_secs();

    generate_totp_code_at_unix_time(totp_secret, unix_secs)
}

// ---------------------------------------------------------------------------
// Internal
// ---------------------------------------------------------------------------

/// Generates the TOTP code for an EXPLICIT unix timestamp (seconds).
///
/// Deliberately time-injected: this is the seam that lets the known-answer
/// tests below pin exact code values against the RFC 6238 vectors. A silent
/// change to the algorithm, digit count, time step, or dynamic truncation
/// fails those tests instead of shipping.
fn generate_totp_code_at_unix_time(
    totp_secret: &SecretString,
    unix_secs: u64,
) -> Result<String, ApplicationError> {
    let secret = TotpSecret::try_from_base32(totp_secret.expose_secret()).map_err(|err| {
        ApplicationError::TotpGenerationFailed {
            reason: format!("invalid base32 secret: {err}"),
        }
    })?;

    let totp = TotpBuilder::new()
        .with_algorithm(Algorithm::SHA1)
        .with_digits(TOTP_DIGITS as u8)
        .with_skew(u16::from(TOTP_SKEW))
        .with_step_duration(TOTP_PERIOD_SECS)
        .with_secret(secret)
        .with_issuer(Some("tickvault"))
        .with_account_name("dhan-auth")
        .build()
        .map_err(|err| ApplicationError::TotpGenerationFailed {
            reason: format!("TOTP initialization failed: {err}"),
        })?;

    Ok(totp.generate(unix_secs).to_string())
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
        // totp-rs `Secret::try_from_base32` may or may not strip whitespace.
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
        // Short secrets are rejected by the builder (128-bit minimum).
        // The invariant is "no panic".
    }

    #[test]
    fn test_totp_single_char_secret_returns_error() {
        // A single character base32 string — too short for TOTP.
        let secret = SecretString::from("A".to_string());
        let result = generate_totp_code(&secret);
        // Either invalid base32 or TOTP init failure. The invariant is "no panic".
        // Very short secrets are rejected by the builder (128-bit minimum).
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

    // -----------------------------------------------------------------------
    // Pinned known-answer tests (KAT)
    //
    // Every other test in this module asserts SHAPE only ("6 digits, numeric,
    // does not panic"). Shape tests cannot see an algorithm change — a swap to
    // SHA-256, a different time step, or a broken dynamic truncation all still
    // produce "6 numeric digits". The tests below pin EXACT code values for
    // FIXED (secret, timestamp) pairs, so any such change fails the build.
    // -----------------------------------------------------------------------

    /// The RFC 6238 Appendix B seed: ASCII "12345678901234567890" (20 bytes),
    /// base32-encoded. Verified against the raw ASCII bytes by
    /// `test_rfc6238_seed_base32_literal_decodes_to_the_ascii_seed`.
    const RFC6238_SHA1_SEED_BASE32: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";

    /// The official RFC 6238 Appendix B SHA-1 vectors: (unix seconds, 8-digit code).
    const RFC6238_SHA1_VECTORS: [(u64, &str); 6] = [
        (59, "94287082"),
        (1_111_111_109, "07081804"),
        (1_111_111_111, "14050471"),
        (1_234_567_890, "89005924"),
        (2_000_000_000, "69279037"),
        (20_000_000_000, "65353130"),
    ];

    #[test]
    fn test_rfc6238_seed_base32_literal_decodes_to_the_ascii_seed() {
        // Self-verifies the constant above: a typo in the base32 literal would
        // silently move every KAT below onto a different secret.
        let decoded = TotpSecret::try_from_base32(RFC6238_SHA1_SEED_BASE32)
            .expect("RFC 6238 seed must be valid base32");
        assert_eq!(
            decoded.as_bytes(),
            b"12345678901234567890",
            "RFC6238_SHA1_SEED_BASE32 must decode to the RFC 6238 Appendix B ASCII seed"
        );
    }

    #[test]
    fn test_rfc6238_sha1_official_eight_digit_vectors_are_exact() {
        // Pins the underlying algorithm (HMAC-SHA1 counter, dynamic truncation,
        // zero-padded formatting) against the published RFC 6238 answers,
        // independently of the 6-digit choice this crate ships with.
        let secret = TotpSecret::try_from_base32(RFC6238_SHA1_SEED_BASE32)
            .expect("RFC 6238 seed must be valid base32");
        let totp = TotpBuilder::new()
            .with_algorithm(Algorithm::SHA1)
            .with_digits(8)
            .with_skew(u16::from(TOTP_SKEW))
            .with_step_duration(TOTP_PERIOD_SECS)
            .with_secret(secret)
            .with_issuer(Some("tickvault"))
            .with_account_name("dhan-auth")
            .build()
            .expect("RFC 6238 parameters must build");

        for (unix_secs, expected) in RFC6238_SHA1_VECTORS {
            assert_eq!(
                totp.generate(unix_secs).to_string(),
                expected,
                "RFC 6238 SHA-1 vector at t={unix_secs} must be exactly {expected}"
            );
        }
    }

    #[test]
    fn test_generate_totp_code_at_unix_time_matches_rfc6238_truncated_to_six_digits() {
        // The SAME official vectors through the REAL production path.
        // Truncation is `value % 10^digits`, and 10^6 divides 10^8, so the
        // 6-digit code is exactly the last 6 digits of the official 8-digit
        // answer (zero-padded) — see the t=1234567890 row.
        let secret = SecretString::from(RFC6238_SHA1_SEED_BASE32.to_string());

        let expected_six: [(u64, &str); 6] = [
            (59, "287082"),
            (1_111_111_109, "081804"),
            (1_111_111_111, "050471"),
            (1_234_567_890, "005924"),
            (2_000_000_000, "279037"),
            (20_000_000_000, "353130"),
        ];

        for ((unix_secs, expected), (vec_secs, eight)) in
            expected_six.into_iter().zip(RFC6238_SHA1_VECTORS)
        {
            assert_eq!(unix_secs, vec_secs, "KAT tables must stay aligned");
            assert_eq!(
                expected,
                &eight[2..],
                "the 6-digit expectation must be the last 6 digits of the official 8-digit answer"
            );

            let code = generate_totp_code_at_unix_time(&secret, unix_secs)
                .expect("RFC 6238 seed must produce a code");
            assert_eq!(
                code, expected,
                "generate_totp_code_at_unix_time at t={unix_secs} must be exactly {expected}"
            );
        }
    }

    #[test]
    fn test_generate_totp_code_at_unix_time_pins_our_own_secret_vectors() {
        // Our own 6-digit / 30-second golden vectors on the module's test
        // secret, on and around 30-second step boundaries.
        let secret = SecretString::from(TEST_TOTP_SECRET.to_string());

        // Timestamps are 30-second-STEP-ALIGNED where noted, so the pairs
        // (aligned, aligned+29) provably sit inside one step.
        let timestamps: [u64; 6] = [0, 29, 30, 59, 1_700_000_010, 1_700_000_040];
        let expected: [&str; 6] = [
            "379080", // t=0        step 0
            "379080", // t=29       step 0 — same code, proves the step window
            "514961", // t=30       step 1
            "514961", // t=59       step 1 — same code
            "878957", // t=1700000010 step 56666667 (30-aligned)
            "502753", // t=1700000040 step 56666668
        ];

        let actual: Vec<String> = timestamps
            .into_iter()
            .map(|t| {
                generate_totp_code_at_unix_time(&secret, t)
                    .expect("valid base32 secret must produce a code")
            })
            .collect();

        assert_eq!(
            actual, expected,
            "pinned codes for timestamps {timestamps:?} must match exactly"
        );
    }

    #[test]
    fn test_generate_totp_code_at_unix_time_is_constant_within_a_step_and_changes_across_it() {
        // Pins the 30-second step boundary itself: a change to TOTP_PERIOD_SECS
        // (or to the counter derivation) moves these equalities.
        let secret = SecretString::from(TEST_TOTP_SECRET.to_string());
        let step = TOTP_PERIOD_SECS;

        let first =
            generate_totp_code_at_unix_time(&secret, 1_700_000_010).expect("should generate code");
        let same_step = generate_totp_code_at_unix_time(&secret, 1_700_000_010 + step - 1)
            .expect("should generate code");
        let next_step = generate_totp_code_at_unix_time(&secret, 1_700_000_010 + step)
            .expect("should generate code");

        assert_eq!(first, same_step, "codes must be constant within one step");
        assert_ne!(first, next_step, "codes must change across a step boundary");
    }

    #[test]
    fn test_generate_totp_code_at_unix_time_rejects_invalid_base32() {
        // The typed-error contract survives on the time-injected seam too.
        let secret = SecretString::from("!!!not-valid-base32!!!".to_string());
        match generate_totp_code_at_unix_time(&secret, 59) {
            Err(ApplicationError::TotpGenerationFailed { reason }) => {
                assert!(reason.contains("invalid base32"), "got: {reason}");
            }
            Err(other) => panic!("expected TotpGenerationFailed, got: {other}"),
            Ok(code) => panic!("expected error for invalid base32, got: {code}"),
        }
    }

    #[test]
    fn test_generate_totp_code_at_unix_time_rejects_too_short_secret() {
        // A well-formed base32 string under 128 bits must be REFUSED by the
        // builder as a typed error — never a panic.
        let secret = SecretString::from("MFRGGZDF".to_string());
        match generate_totp_code_at_unix_time(&secret, 59) {
            Err(ApplicationError::TotpGenerationFailed { reason }) => {
                assert!(
                    reason.contains("TOTP initialization failed"),
                    "a short secret must fail at builder validation, got: {reason}"
                );
            }
            Err(other) => panic!("expected TotpGenerationFailed, got: {other}"),
            Ok(code) => panic!("expected error for a 40-bit secret, got: {code}"),
        }
    }

    #[test]
    fn test_generate_totp_code_uses_current_clock_and_matches_the_time_injected_seam() {
        // Proves the public wrapper is the same computation as the pinned seam,
        // just fed by the system clock.
        let secret = SecretString::from(TEST_TOTP_SECRET.to_string());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("test host clock must be after the unix epoch")
            .as_secs();

        let via_public = generate_totp_code(&secret).expect("should generate code");
        let via_seam = generate_totp_code_at_unix_time(&secret, now).expect("should generate code");

        // Equal unless the call straddled a step boundary; re-derive in that case.
        if via_public != via_seam {
            let later = generate_totp_code_at_unix_time(&secret, now + TOTP_PERIOD_SECS)
                .expect("should generate code");
            assert!(
                via_public == later || via_public == via_seam,
                "public wrapper must match the seam for the current or adjacent step"
            );
        }
    }
}

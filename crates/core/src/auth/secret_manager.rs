//! AWS SSM Parameter Store credential retrieval.
//!
//! Fetches Dhan API credentials (client-id, client-secret, totp-secret)
//! and Telegram tokens from real AWS SSM (ap-south-1). Same code path
//! in dev and prod — secrets are managed via AWS Console.

use aws_config::Region;
use aws_sdk_ssm::Client as SsmClient;
use secrecy::SecretString;
use tracing::{info, instrument};

use dhan_live_trader_common::constants::{
    DEFAULT_SSM_ENVIRONMENT, DHAN_CLIENT_ID_SECRET, DHAN_CLIENT_SECRET_SECRET, DHAN_TOTP_SECRET,
    SSM_DHAN_SERVICE, SSM_SECRET_BASE_PATH,
};
use dhan_live_trader_common::error::ApplicationError;

use super::types::DhanCredentials;

// ---------------------------------------------------------------------------
// SSM Path Construction
// ---------------------------------------------------------------------------

/// Constructs the full SSM parameter path.
///
/// Format: `/dlt/<environment>/<service>/<secret_name>`
pub(crate) fn build_ssm_path(environment: &str, service: &str, secret_name: &str) -> String {
    format!(
        "{}/{}/{}/{}",
        SSM_SECRET_BASE_PATH, environment, service, secret_name
    )
}

/// Returns the SSM environment from the `ENVIRONMENT` env var,
/// falling back to `DEFAULT_SSM_ENVIRONMENT` ("dev").
///
/// Validates that the environment string contains only alphanumeric
/// characters and hyphens to prevent path traversal.
pub fn resolve_environment() -> Result<String, ApplicationError> {
    let env = std::env::var("ENVIRONMENT").unwrap_or_else(|_| DEFAULT_SSM_ENVIRONMENT.to_string());

    if env.is_empty() || !env.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(ApplicationError::Configuration(format!(
            "ENVIRONMENT '{}' contains invalid characters (only alphanumeric and hyphen allowed)",
            env
        )));
    }

    Ok(env)
}

// ---------------------------------------------------------------------------
// SSM Client Initialization
// ---------------------------------------------------------------------------

/// Creates an AWS SSM client targeting real AWS SSM (ap-south-1).
///
/// Uses default AWS SDK configuration — reads credentials from
/// `~/.aws/credentials` (dev Mac) or IAM role (prod AWS).
pub(crate) async fn create_ssm_client() -> SsmClient {
    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(Region::new("ap-south-1"))
        .load()
        .await;

    SsmClient::new(&config)
}

// ---------------------------------------------------------------------------
// Secret Retrieval
// ---------------------------------------------------------------------------

/// Fetches a single secret value from SSM Parameter Store.
///
/// Returns the raw string value wrapped in `SecretString`.
pub(crate) async fn fetch_secret(
    ssm_client: &SsmClient,
    path: &str,
) -> Result<SecretString, ApplicationError> {
    let result = ssm_client
        .get_parameter()
        .name(path)
        .with_decryption(true)
        .send()
        .await
        .map_err(|err| ApplicationError::SecretRetrieval {
            path: path.to_string(),
            source: anyhow::anyhow!("{err}"),
        })?;

    let parameter = result
        .parameter()
        .ok_or_else(|| ApplicationError::SecretRetrieval {
            path: path.to_string(),
            source: anyhow::anyhow!("parameter response contained no parameter"),
        })?;

    let value = parameter
        .value()
        .ok_or_else(|| ApplicationError::SecretRetrieval {
            path: path.to_string(),
            source: anyhow::anyhow!("parameter has no value"),
        })?;

    Ok(SecretString::from(value.to_string()))
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Fetches all Dhan authentication credentials from AWS SSM Parameter Store.
///
/// Retrieves client-id, client-secret, and totp-secret, all wrapped in
/// `SecretString` for safety. Uses the `ENVIRONMENT` env var to determine
/// the SSM path prefix (defaults to "dev").
///
/// # Errors
///
/// Returns `ApplicationError::SecretRetrieval` if any secret cannot be fetched.
#[instrument(skip_all, fields(environment))]
pub async fn fetch_dhan_credentials() -> Result<DhanCredentials, ApplicationError> {
    let environment = resolve_environment()?;
    tracing::Span::current().record("environment", environment.as_str());

    let ssm_client = create_ssm_client().await;

    let client_id_path = build_ssm_path(&environment, SSM_DHAN_SERVICE, DHAN_CLIENT_ID_SECRET);
    let client_secret_path =
        build_ssm_path(&environment, SSM_DHAN_SERVICE, DHAN_CLIENT_SECRET_SECRET);
    let totp_secret_path = build_ssm_path(&environment, SSM_DHAN_SERVICE, DHAN_TOTP_SECRET);

    info!(
        client_id_path = %client_id_path,
        client_secret_path = %client_secret_path,
        totp_secret_path = %totp_secret_path,
        "fetching Dhan credentials from SSM"
    );

    // Fetch all three secrets concurrently — 3x faster than sequential.
    let (client_id, client_secret, totp_secret) = tokio::try_join!(
        fetch_secret(&ssm_client, &client_id_path),
        fetch_secret(&ssm_client, &client_secret_path),
        fetch_secret(&ssm_client, &totp_secret_path),
    )?;

    info!("all Dhan credentials fetched successfully from SSM");

    Ok(DhanCredentials {
        client_id,
        client_secret,
        totp_secret,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_ssm_path_dev() {
        let path = build_ssm_path("dev", SSM_DHAN_SERVICE, DHAN_CLIENT_ID_SECRET);
        assert_eq!(path, "/dlt/dev/dhan/client-id");
    }

    #[test]
    fn test_build_ssm_path_prod() {
        let path = build_ssm_path("prod", SSM_DHAN_SERVICE, DHAN_CLIENT_SECRET_SECRET);
        assert_eq!(path, "/dlt/prod/dhan/client-secret");
    }

    #[test]
    fn test_build_ssm_path_totp() {
        let path = build_ssm_path("dev", SSM_DHAN_SERVICE, DHAN_TOTP_SECRET);
        assert_eq!(path, "/dlt/dev/dhan/totp-secret");
    }

    #[test]
    fn test_resolve_environment_default() {
        // When ENVIRONMENT env var is not set, should default to "dev"
        // Uses the shared mutex to avoid races with other env-var tests.
        with_environment_var(None, || {
            let env = resolve_environment().expect("resolve_environment should succeed");
            assert!(!env.is_empty());
        });
    }

    // -----------------------------------------------------------------------
    // resolve_environment — validation & default value tests
    // -----------------------------------------------------------------------
    //
    // NOTE: These tests mutate process-wide env vars via `std::env::set_var`
    // / `std::env::remove_var`. In Rust 2024, these are `unsafe` because
    // concurrent threads may observe partial writes. `cargo test` by default
    // runs tests in separate threads. A `serial_test` crate could enforce
    // serial execution, but it is not in the Tech Stack Bible. We use a
    // static Mutex instead so only one env-var test runs at a time.

    use std::sync::Mutex;
    static ENVIRONMENT_MUTEX: Mutex<()> = Mutex::new(());

    /// Helper: run a closure with `ENVIRONMENT` set to a specific value,
    /// then restore it to unset. Holds `ENVIRONMENT_MUTEX` for the duration
    /// to prevent parallel env-var tests from interfering.
    fn with_environment_var<F, R>(value: Option<&str>, test_body: F) -> R
    where
        F: FnOnce() -> R,
    {
        let _guard = ENVIRONMENT_MUTEX.lock().expect("env mutex poisoned");
        // SAFETY: env var mutation in test-only code. The Mutex ensures no
        //         concurrent test touches ENVIRONMENT at the same time.
        match value {
            Some(val) => unsafe { std::env::set_var("ENVIRONMENT", val) },
            None => unsafe { std::env::remove_var("ENVIRONMENT") },
        }
        let result = test_body();
        // Always restore to unset after the test.
        unsafe {
            std::env::remove_var("ENVIRONMENT");
        }
        result
    }

    #[test]
    fn test_resolve_environment_default_value_is_dev() {
        with_environment_var(None, || {
            let env =
                resolve_environment().expect("resolve_environment should succeed with default");
            assert_eq!(env, "dev", "default environment must be \"dev\"");
        });
    }

    #[test]
    fn test_resolve_environment_with_empty_string_returns_error() {
        with_environment_var(Some(""), || {
            let result = resolve_environment();
            assert!(result.is_err(), "empty ENVIRONMENT string must be rejected");
            let err_msg = result.unwrap_err().to_string();
            assert!(
                err_msg.contains("invalid characters"),
                "error message should mention invalid characters, got: {err_msg}"
            );
        });
    }

    #[test]
    fn test_resolve_environment_with_path_traversal_returns_error() {
        // "../../etc" contains dots and slashes — both rejected by validation.
        with_environment_var(Some("../../etc"), || {
            let result = resolve_environment();
            assert!(result.is_err(), "path-traversal string must be rejected");
            let err_msg = result.unwrap_err().to_string();
            assert!(
                err_msg.contains("invalid characters"),
                "error message should mention invalid characters, got: {err_msg}"
            );
        });
    }

    #[test]
    fn test_resolve_environment_with_spaces_returns_error() {
        // "has spaces" contains space characters — rejected by validation.
        with_environment_var(Some("has spaces"), || {
            let result = resolve_environment();
            assert!(
                result.is_err(),
                "environment string with spaces must be rejected"
            );
            let err_msg = result.unwrap_err().to_string();
            assert!(
                err_msg.contains("invalid characters"),
                "error message should mention invalid characters, got: {err_msg}"
            );
        });
    }

    // -----------------------------------------------------------------------
    // build_ssm_path — exhaustive secret-type × environment coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_ssm_path_all_secret_types_dev() {
        assert_eq!(
            build_ssm_path("dev", SSM_DHAN_SERVICE, DHAN_CLIENT_ID_SECRET),
            "/dlt/dev/dhan/client-id"
        );
        assert_eq!(
            build_ssm_path("dev", SSM_DHAN_SERVICE, DHAN_CLIENT_SECRET_SECRET),
            "/dlt/dev/dhan/client-secret"
        );
        assert_eq!(
            build_ssm_path("dev", SSM_DHAN_SERVICE, DHAN_TOTP_SECRET),
            "/dlt/dev/dhan/totp-secret"
        );
    }

    #[test]
    fn test_build_ssm_path_all_secret_types_prod() {
        assert_eq!(
            build_ssm_path("prod", SSM_DHAN_SERVICE, DHAN_CLIENT_ID_SECRET),
            "/dlt/prod/dhan/client-id"
        );
        assert_eq!(
            build_ssm_path("prod", SSM_DHAN_SERVICE, DHAN_CLIENT_SECRET_SECRET),
            "/dlt/prod/dhan/client-secret"
        );
        assert_eq!(
            build_ssm_path("prod", SSM_DHAN_SERVICE, DHAN_TOTP_SECRET),
            "/dlt/prod/dhan/totp-secret"
        );
    }

    // -----------------------------------------------------------------------
    // resolve_environment — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolve_environment_with_valid_custom_value() {
        with_environment_var(Some("staging"), || {
            let env = resolve_environment().expect("'staging' should be valid");
            assert_eq!(env, "staging");
        });
    }

    #[test]
    fn test_resolve_environment_with_hyphenated_value() {
        with_environment_var(Some("pre-prod"), || {
            let env = resolve_environment().expect("'pre-prod' should be valid");
            assert_eq!(env, "pre-prod");
        });
    }

    #[test]
    fn test_resolve_environment_with_numeric_value() {
        with_environment_var(Some("env1"), || {
            let env = resolve_environment().expect("'env1' should be valid");
            assert_eq!(env, "env1");
        });
    }

    #[test]
    fn test_resolve_environment_with_uppercase() {
        with_environment_var(Some("PROD"), || {
            let env = resolve_environment().expect("'PROD' should be valid");
            assert_eq!(env, "PROD");
        });
    }

    #[test]
    fn test_resolve_environment_with_underscore_returns_error() {
        // Underscores are NOT in the allowed set (alphanumeric + hyphen only).
        with_environment_var(Some("pre_prod"), || {
            let result = resolve_environment();
            assert!(
                result.is_err(),
                "underscore in environment string must be rejected"
            );
        });
    }

    #[test]
    fn test_resolve_environment_with_special_chars_returns_error() {
        with_environment_var(Some("dev;rm -rf /"), || {
            let result = resolve_environment();
            assert!(result.is_err(), "special characters must be rejected");
        });
    }

    #[test]
    fn test_resolve_environment_with_unicode_returns_error() {
        with_environment_var(Some("dev\u{00e9}"), || {
            let result = resolve_environment();
            assert!(result.is_err(), "non-ASCII characters must be rejected");
        });
    }

    // -----------------------------------------------------------------------
    // create_ssm_client — smoke test
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_create_ssm_client_returns_without_panic() {
        // create_ssm_client constructs an AWS SDK client targeting ap-south-1.
        // It does not make any network calls — it just builds config.
        // This test verifies it completes without panic regardless of
        // whether AWS credentials are available.
        let _client = create_ssm_client().await;
    }

    // -----------------------------------------------------------------------
    // build_ssm_path — Telegram service paths
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_ssm_path_telegram_bot_token() {
        use dhan_live_trader_common::constants::{SSM_TELEGRAM_SERVICE, TELEGRAM_BOT_TOKEN_SECRET};
        let path = build_ssm_path("dev", SSM_TELEGRAM_SERVICE, TELEGRAM_BOT_TOKEN_SECRET);
        assert_eq!(path, "/dlt/dev/telegram/bot-token");
    }

    #[test]
    fn test_build_ssm_path_telegram_chat_id() {
        use dhan_live_trader_common::constants::{SSM_TELEGRAM_SERVICE, TELEGRAM_CHAT_ID_SECRET};
        let path = build_ssm_path("prod", SSM_TELEGRAM_SERVICE, TELEGRAM_CHAT_ID_SECRET);
        assert_eq!(path, "/dlt/prod/telegram/chat-id");
    }

    // -----------------------------------------------------------------------
    // fetch_secret — error path (no real SSM available)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_fetch_secret_returns_error_without_real_ssm() {
        // create_ssm_client points at real AWS. Without valid credentials
        // or connectivity, fetch_secret should return SecretRetrieval error.
        let ssm_client = create_ssm_client().await;
        let result = fetch_secret(&ssm_client, "/dlt/test/nonexistent/secret").await;
        assert!(
            result.is_err(),
            "fetch_secret must fail without real SSM connectivity"
        );
    }

    // -----------------------------------------------------------------------
    // fetch_dhan_credentials — error path (no real SSM available)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_fetch_dhan_credentials_returns_error_without_real_ssm() {
        // Without real AWS SSM connectivity, fetch_dhan_credentials must fail
        // with a SecretRetrieval error.
        with_environment_var(Some("test-env"), || {});
        let result = fetch_dhan_credentials().await;
        assert!(
            result.is_err(),
            "fetch_dhan_credentials must fail without real SSM"
        );
    }

    // -----------------------------------------------------------------------
    // build_ssm_path — edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_ssm_path_with_custom_environment() {
        let path = build_ssm_path("staging-v2", SSM_DHAN_SERVICE, DHAN_CLIENT_ID_SECRET);
        assert_eq!(path, "/dlt/staging-v2/dhan/client-id");
    }

    #[test]
    fn test_build_ssm_path_with_custom_service() {
        let path = build_ssm_path("dev", "custom-svc", "api-key");
        assert_eq!(path, "/dlt/dev/custom-svc/api-key");
    }

    // -----------------------------------------------------------------------
    // resolve_environment — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolve_environment_with_single_char() {
        with_environment_var(Some("a"), || {
            let env = resolve_environment().expect("single char should be valid");
            assert_eq!(env, "a");
        });
    }

    #[test]
    fn test_resolve_environment_with_all_digits() {
        with_environment_var(Some("123"), || {
            let env = resolve_environment().expect("all-digit string should be valid");
            assert_eq!(env, "123");
        });
    }

    #[test]
    fn test_resolve_environment_with_leading_hyphen() {
        with_environment_var(Some("-dev"), || {
            let env = resolve_environment().expect("leading hyphen should be valid");
            assert_eq!(env, "-dev");
        });
    }

    #[test]
    fn test_resolve_environment_with_newline_returns_error() {
        with_environment_var(Some("dev\ninjection"), || {
            let result = resolve_environment();
            assert!(
                result.is_err(),
                "newline in environment string must be rejected"
            );
        });
    }

    #[test]
    fn test_resolve_environment_with_tab_returns_error() {
        with_environment_var(Some("dev\ttest"), || {
            let result = resolve_environment();
            assert!(
                result.is_err(),
                "tab in environment string must be rejected"
            );
        });
    }
}

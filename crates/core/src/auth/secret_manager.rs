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
    GRAFANA_ADMIN_PASSWORD_SECRET, GRAFANA_ADMIN_USER_SECRET, QUESTDB_PG_PASSWORD_SECRET,
    QUESTDB_PG_USER_SECRET, SSM_DHAN_SERVICE, SSM_GRAFANA_SERVICE, SSM_QUESTDB_SERVICE,
    SSM_SECRET_BASE_PATH,
};
use dhan_live_trader_common::error::ApplicationError;

use super::types::{DhanCredentials, GrafanaCredentials, QuestDbCredentials, TelegramCredentials};

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

/// Validates an environment string. Only alphanumeric + hyphens allowed.
fn validate_environment(env: &str) -> Result<String, ApplicationError> {
    if env.is_empty() || !env.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(ApplicationError::Configuration(format!(
            "ENVIRONMENT '{}' contains invalid characters (only alphanumeric and hyphen allowed)",
            env
        )));
    }

    Ok(env.to_string())
}

/// Returns the SSM environment from the `ENVIRONMENT` env var,
/// falling back to `DEFAULT_SSM_ENVIRONMENT` ("dev").
///
/// Validates that the environment string contains only alphanumeric
/// characters and hyphens to prevent path traversal.
pub fn resolve_environment() -> Result<String, ApplicationError> {
    let env = std::env::var("ENVIRONMENT").unwrap_or_else(|_| DEFAULT_SSM_ENVIRONMENT.to_string());
    validate_environment(&env)
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

/// Fetches QuestDB PG wire protocol credentials from AWS SSM Parameter Store.
///
/// Retrieves pg-user and pg-password, both wrapped in `SecretString`.
/// Used for PostgreSQL wire protocol connections (port 8812) and
/// injected into docker-compose via environment variables.
///
/// # Errors
///
/// Returns `ApplicationError::SecretRetrieval` if any secret cannot be fetched.
#[instrument(skip_all, fields(environment))]
pub async fn fetch_questdb_credentials() -> Result<QuestDbCredentials, ApplicationError> {
    let environment = resolve_environment()?;
    tracing::Span::current().record("environment", environment.as_str());

    let ssm_client = create_ssm_client().await;

    let pg_user_path = build_ssm_path(&environment, SSM_QUESTDB_SERVICE, QUESTDB_PG_USER_SECRET);
    let pg_password_path = build_ssm_path(
        &environment,
        SSM_QUESTDB_SERVICE,
        QUESTDB_PG_PASSWORD_SECRET,
    );

    info!(
        pg_user_path = %pg_user_path,
        pg_password_path = %pg_password_path,
        "fetching QuestDB credentials from SSM"
    );

    let (pg_user, pg_password) = tokio::try_join!(
        fetch_secret(&ssm_client, &pg_user_path),
        fetch_secret(&ssm_client, &pg_password_path),
    )?;

    info!("all QuestDB credentials fetched successfully from SSM");

    Ok(QuestDbCredentials {
        pg_user,
        pg_password,
    })
}

/// Fetches Grafana admin credentials from AWS SSM Parameter Store.
///
/// Retrieves admin-user and admin-password, both wrapped in `SecretString`.
/// Injected into docker-compose via environment variables.
///
/// # Errors
///
/// Returns `ApplicationError::SecretRetrieval` if any secret cannot be fetched.
#[instrument(skip_all, fields(environment))]
pub async fn fetch_grafana_credentials() -> Result<GrafanaCredentials, ApplicationError> {
    let environment = resolve_environment()?;
    tracing::Span::current().record("environment", environment.as_str());

    let ssm_client = create_ssm_client().await;

    let admin_user_path =
        build_ssm_path(&environment, SSM_GRAFANA_SERVICE, GRAFANA_ADMIN_USER_SECRET);
    let admin_password_path = build_ssm_path(
        &environment,
        SSM_GRAFANA_SERVICE,
        GRAFANA_ADMIN_PASSWORD_SECRET,
    );

    info!(
        admin_user_path = %admin_user_path,
        admin_password_path = %admin_password_path,
        "fetching Grafana credentials from SSM"
    );

    let (admin_user, admin_password) = tokio::try_join!(
        fetch_secret(&ssm_client, &admin_user_path),
        fetch_secret(&ssm_client, &admin_password_path),
    )?;

    info!("all Grafana credentials fetched successfully from SSM");

    Ok(GrafanaCredentials {
        admin_user,
        admin_password,
    })
}

/// Fetches Telegram bot credentials from AWS SSM Parameter Store.
///
/// Retrieves bot-token and chat-id, both wrapped in `SecretString`.
/// Used by the infra orchestrator to inject into docker-compose
/// environment variables for Grafana alerting.
///
/// # Errors
///
/// Returns `ApplicationError::SecretRetrieval` if any secret cannot be fetched.
#[instrument(skip_all, fields(environment))]
pub async fn fetch_telegram_credentials() -> Result<TelegramCredentials, ApplicationError> {
    use dhan_live_trader_common::constants::{
        SSM_TELEGRAM_SERVICE, TELEGRAM_BOT_TOKEN_SECRET, TELEGRAM_CHAT_ID_SECRET,
    };

    let environment = resolve_environment()?;
    tracing::Span::current().record("environment", environment.as_str());

    let ssm_client = create_ssm_client().await;

    let bot_token_path = build_ssm_path(
        &environment,
        SSM_TELEGRAM_SERVICE,
        TELEGRAM_BOT_TOKEN_SECRET,
    );
    let chat_id_path = build_ssm_path(&environment, SSM_TELEGRAM_SERVICE, TELEGRAM_CHAT_ID_SECRET);

    info!(
        bot_token_path = %bot_token_path,
        chat_id_path = %chat_id_path,
        "fetching Telegram credentials from SSM"
    );

    let (bot_token, chat_id) = tokio::try_join!(
        fetch_secret(&ssm_client, &bot_token_path),
        fetch_secret(&ssm_client, &chat_id_path),
    )?;

    info!("all Telegram credentials fetched successfully from SSM");

    Ok(TelegramCredentials { bot_token, chat_id })
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

    // -----------------------------------------------------------------------
    // validate_environment — pure validation (no env var mutation needed)
    // -----------------------------------------------------------------------

    #[test]
    fn test_validate_environment_dev() {
        let env = validate_environment("dev").expect("'dev' should be valid");
        assert_eq!(env, "dev");
    }

    #[test]
    fn test_validate_environment_prod() {
        let env = validate_environment("prod").expect("'prod' should be valid");
        assert_eq!(env, "prod");
    }

    #[test]
    fn test_validate_environment_empty_string_returns_error() {
        let result = validate_environment("");
        assert!(result.is_err(), "empty string must be rejected");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("invalid characters"),
            "error message should mention invalid characters, got: {err_msg}"
        );
    }

    #[test]
    fn test_validate_environment_path_traversal_returns_error() {
        let result = validate_environment("../../etc");
        assert!(result.is_err(), "path-traversal string must be rejected");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("invalid characters"),
            "error message should mention invalid characters, got: {err_msg}"
        );
    }

    #[test]
    fn test_validate_environment_spaces_returns_error() {
        let result = validate_environment("has spaces");
        assert!(
            result.is_err(),
            "environment string with spaces must be rejected"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("invalid characters"),
            "error message should mention invalid characters, got: {err_msg}"
        );
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
    // validate_environment — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_validate_environment_staging() {
        let env = validate_environment("staging").expect("'staging' should be valid");
        assert_eq!(env, "staging");
    }

    #[test]
    fn test_validate_environment_hyphenated() {
        let env = validate_environment("pre-prod").expect("'pre-prod' should be valid");
        assert_eq!(env, "pre-prod");
    }

    #[test]
    fn test_validate_environment_numeric() {
        let env = validate_environment("env1").expect("'env1' should be valid");
        assert_eq!(env, "env1");
    }

    #[test]
    fn test_validate_environment_uppercase() {
        let env = validate_environment("PROD").expect("'PROD' should be valid");
        assert_eq!(env, "PROD");
    }

    #[test]
    fn test_validate_environment_underscore_returns_error() {
        let result = validate_environment("pre_prod");
        assert!(
            result.is_err(),
            "underscore in environment string must be rejected"
        );
    }

    #[test]
    fn test_validate_environment_special_chars_returns_error() {
        let result = validate_environment("dev;rm -rf /");
        assert!(result.is_err(), "special characters must be rejected");
    }

    #[test]
    fn test_validate_environment_unicode_returns_error() {
        let result = validate_environment("dev\u{00e9}");
        assert!(result.is_err(), "non-ASCII characters must be rejected");
    }

    // -----------------------------------------------------------------------
    // create_ssm_client — smoke test
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_create_ssm_client_returns_without_panic() {
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
    // build_ssm_path — QuestDB secret paths
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_ssm_path_questdb_dev() {
        assert_eq!(
            build_ssm_path("dev", SSM_QUESTDB_SERVICE, QUESTDB_PG_USER_SECRET),
            "/dlt/dev/questdb/pg-user"
        );
        assert_eq!(
            build_ssm_path("dev", SSM_QUESTDB_SERVICE, QUESTDB_PG_PASSWORD_SECRET),
            "/dlt/dev/questdb/pg-password"
        );
    }

    #[test]
    fn test_build_ssm_path_questdb_prod() {
        assert_eq!(
            build_ssm_path("prod", SSM_QUESTDB_SERVICE, QUESTDB_PG_USER_SECRET),
            "/dlt/prod/questdb/pg-user"
        );
        assert_eq!(
            build_ssm_path("prod", SSM_QUESTDB_SERVICE, QUESTDB_PG_PASSWORD_SECRET),
            "/dlt/prod/questdb/pg-password"
        );
    }

    // -----------------------------------------------------------------------
    // build_ssm_path — Grafana secret paths
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_ssm_path_grafana_dev() {
        assert_eq!(
            build_ssm_path("dev", SSM_GRAFANA_SERVICE, GRAFANA_ADMIN_USER_SECRET),
            "/dlt/dev/grafana/admin-user"
        );
        assert_eq!(
            build_ssm_path("dev", SSM_GRAFANA_SERVICE, GRAFANA_ADMIN_PASSWORD_SECRET),
            "/dlt/dev/grafana/admin-password"
        );
    }

    #[test]
    fn test_build_ssm_path_grafana_prod() {
        assert_eq!(
            build_ssm_path("prod", SSM_GRAFANA_SERVICE, GRAFANA_ADMIN_USER_SECRET),
            "/dlt/prod/grafana/admin-user"
        );
        assert_eq!(
            build_ssm_path("prod", SSM_GRAFANA_SERVICE, GRAFANA_ADMIN_PASSWORD_SECRET),
            "/dlt/prod/grafana/admin-password"
        );
    }

    // -----------------------------------------------------------------------
    // fetch_secret — error path (no real SSM available)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_fetch_secret_returns_error_without_real_ssm() {
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
        let result = fetch_dhan_credentials().await;
        if crate::test_support::has_aws_credentials() {
            // Dev machine with real AWS credentials — SSM is reachable,
            // so fetch_dhan_credentials succeeds. Validate the happy path.
            assert!(
                result.is_ok(),
                "with real AWS credentials, fetch should succeed"
            );
        } else {
            // CI or machine without AWS credentials — SSM is unreachable.
            assert!(
                result.is_err(),
                "fetch_dhan_credentials must fail without real SSM"
            );
        }
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
    // resolve_environment — more edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_validate_environment_single_char() {
        let env = validate_environment("a").expect("single char should be valid");
        assert_eq!(env, "a");
    }

    #[test]
    fn test_validate_environment_all_digits() {
        let env = validate_environment("123").expect("all-digit string should be valid");
        assert_eq!(env, "123");
    }

    #[test]
    fn test_validate_environment_leading_hyphen() {
        let env = validate_environment("-dev").expect("leading hyphen should be valid");
        assert_eq!(env, "-dev");
    }

    #[test]
    fn test_validate_environment_newline_returns_error() {
        let result = validate_environment("dev\ninjection");
        assert!(
            result.is_err(),
            "newline in environment string must be rejected"
        );
    }

    #[test]
    fn test_validate_environment_tab_returns_error() {
        let result = validate_environment("dev\ttest");
        assert!(
            result.is_err(),
            "tab in environment string must be rejected"
        );
    }
}

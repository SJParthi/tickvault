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

use super::types::{DhanCredentials, GrafanaCredentials, QuestDbCredentials};

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
}

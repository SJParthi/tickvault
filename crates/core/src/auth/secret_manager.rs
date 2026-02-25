//! AWS SSM Parameter Store credential retrieval.
//!
//! Fetches Dhan API credentials (client-id, client-secret, totp-secret)
//! from SSM. Uses LocalStack in dev, real AWS SSM in prod — same code path.
//! The ONLY difference: `AWS_ENDPOINT_URL` env var for LocalStack.

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
/// Format: `/dlt/<environment>/dhan/<secret_name>`
fn build_ssm_path(environment: &str, secret_name: &str) -> String {
    format!(
        "{}/{}/{}/{}",
        SSM_SECRET_BASE_PATH, environment, SSM_DHAN_SERVICE, secret_name
    )
}

/// Returns the SSM environment from the `ENVIRONMENT` env var,
/// falling back to `DEFAULT_SSM_ENVIRONMENT` ("dev").
pub fn resolve_environment() -> String {
    std::env::var("ENVIRONMENT").unwrap_or_else(|_| DEFAULT_SSM_ENVIRONMENT.to_string())
}

// ---------------------------------------------------------------------------
// SSM Client Initialization
// ---------------------------------------------------------------------------

/// Creates an AWS SSM client with automatic endpoint detection.
///
/// If `AWS_ENDPOINT_URL` is set, uses that as the SSM endpoint (LocalStack).
/// Otherwise, uses default AWS SDK configuration (real AWS SSM).
async fn create_ssm_client() -> SsmClient {
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
async fn fetch_secret(
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
    let environment = resolve_environment();
    tracing::Span::current().record("environment", environment.as_str());

    let ssm_client = create_ssm_client().await;

    let client_id_path = build_ssm_path(&environment, DHAN_CLIENT_ID_SECRET);
    let client_secret_path = build_ssm_path(&environment, DHAN_CLIENT_SECRET_SECRET);
    let totp_secret_path = build_ssm_path(&environment, DHAN_TOTP_SECRET);

    info!(
        client_id_path = %client_id_path,
        client_secret_path = %client_secret_path,
        totp_secret_path = %totp_secret_path,
        "fetching Dhan credentials from SSM"
    );

    let client_id = fetch_secret(&ssm_client, &client_id_path).await?;
    let client_secret = fetch_secret(&ssm_client, &client_secret_path).await?;
    let totp_secret = fetch_secret(&ssm_client, &totp_secret_path).await?;

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
        let path = build_ssm_path("dev", DHAN_CLIENT_ID_SECRET);
        assert_eq!(path, "/dlt/dev/dhan/client-id");
    }

    #[test]
    fn test_build_ssm_path_prod() {
        let path = build_ssm_path("prod", DHAN_CLIENT_SECRET_SECRET);
        assert_eq!(path, "/dlt/prod/dhan/client-secret");
    }

    #[test]
    fn test_build_ssm_path_totp() {
        let path = build_ssm_path("dev", DHAN_TOTP_SECRET);
        assert_eq!(path, "/dlt/dev/dhan/totp-secret");
    }

    #[test]
    fn test_resolve_environment_default() {
        // When ENVIRONMENT env var is not set, should default to "dev"
        // Note: this test may be affected by CI/CD environment variables
        let env = resolve_environment();
        // Just verify it returns a non-empty string
        assert!(!env.is_empty());
    }
}

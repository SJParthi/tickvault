//! AWS SSM Parameter Store credential retrieval.
//!
//! Fetches Dhan API credentials (client-id, client-secret, totp-secret)
//! and Telegram tokens from real AWS SSM (ap-south-1). Same code path
//! in dev and prod — secrets are managed via AWS Console.

use std::sync::atomic::{AtomicBool, Ordering};

use aws_config::Region;
use aws_sdk_ssm::Client as SsmClient;
use secrecy::SecretString;
use tracing::{info, instrument, warn};

use tickvault_common::constants::{
    DEFAULT_SSM_ENVIRONMENT, DHAN_CLIENT_ID_SECRET, DHAN_CLIENT_SECRET_SECRET,
    DHAN_SANDBOX_CLIENT_ID_SECRET, DHAN_SANDBOX_TOKEN_SECRET, DHAN_TOTP_SECRET,
    GROWW_ACCESS_TOKEN_SECRET, QUESTDB_PG_PASSWORD_SECRET, QUESTDB_PG_USER_SECRET,
    SSM_DHAN_SERVICE, SSM_GROWW_SERVICE, SSM_QUESTDB_SERVICE, SSM_SECRET_BASE_PATH,
};
use tickvault_common::error::ApplicationError;

use super::types::{DhanCredentials, QuestDbCredentials, TelegramCredentials};

// ---------------------------------------------------------------------------
// SSM Path Construction
// ---------------------------------------------------------------------------

/// Constructs the full SSM parameter path.
///
/// Format: `/tickvault/<environment>/<service>/<secret_name>`
///
/// `pub` so boot/activation diagnostics (e.g. the Groww auth-failure hint) can
/// name the EXACT path read — never the value — instead of a `<env>` placeholder.
pub fn build_ssm_path(environment: &str, service: &str, secret_name: &str) -> String {
    if environment.is_empty() || service.is_empty() || secret_name.is_empty() {
        // Empty components yield a malformed path (`/tickvault//groww/...`) that
        // would 404 on every SSM read. We do NOT panic/debug_assert — an existing
        // test calls this with empty strings — but we WARN so a real boot-time
        // misconfig is visible in the log. The returned string is unchanged.
        warn!(
            environment_empty = environment.is_empty(),
            service_empty = service.is_empty(),
            secret_name_empty = secret_name.is_empty(),
            "build_ssm_path called with an empty path component — the resulting SSM \
             path is malformed and will not resolve"
        );
    }
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

/// Returns the SSM environment from the `TV_ENVIRONMENT` env var (preferred,
/// set by the systemd unit on AWS), falling back to the legacy `ENVIRONMENT`
/// var, then to `DEFAULT_SSM_ENVIRONMENT` ("prod").
///
/// Precedence (first non-empty wins): `TV_ENVIRONMENT` → `ENVIRONMENT` → `"prod"`.
/// Unifying on `TV_ENVIRONMENT` keeps the SSM secret prefix
/// (`/tickvault/<env>/...`) consistent with the config-file env selection in
/// `boot_helpers::resolve_config_env` — both read the same variable, so the
/// deployed box's `TV_ENVIRONMENT=prod` drives BOTH the config merge and
/// the SSM prefix. The default is `prod`: dev/staging were collapsed into the
/// single prod env (operator 2026-06-30); NO real orders are placed —
/// `production.toml` locks `dry_run=true`. The legacy `ENVIRONMENT` fallback
/// is preserved.
///
/// Validates that the environment string contains only alphanumeric
/// characters and hyphens to prevent path traversal.
pub fn resolve_environment() -> Result<String, ApplicationError> {
    let env = std::env::var("TV_ENVIRONMENT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("ENVIRONMENT")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or_else(|| {
            // Neither var was set / non-empty — we fall back to the compiled
            // default. Warn ONCE per process so the operator knows every secret
            // is being read from `/tickvault/<default>/*` (e.g. on a box that
            // forgot to export TV_ENVIRONMENT). The returned value is unchanged.
            static WARNED: AtomicBool = AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                warn!(
                    default_env = DEFAULT_SSM_ENVIRONMENT,
                    "TV_ENVIRONMENT/ENVIRONMENT unset — defaulting SSM environment; \
                     secrets read from /tickvault/<default>/*"
                );
            }
            DEFAULT_SSM_ENVIRONMENT.to_string()
        });
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

/// Public wrapper around [`create_ssm_client`] for callers outside this
/// module (e.g. `instance_lock`). Behaviour is identical; the indirection
/// only exists so the internal helper can stay `pub(crate)` while a
/// stable export point lives at the crate root.
// TEST-EXEMPT: one-line delegation to the TEST-EXEMPT create_ssm_client; constructing aws_sdk_ssm::Client needs the live SSM endpoint.
pub async fn create_ssm_client_public() -> SsmClient {
    create_ssm_client().await
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

/// Fetches the Groww ACCESS TOKEN from AWS SSM Parameter Store — READ-ONLY
/// (shared token-minter architecture, operator lock 2026-07-02 —
/// `.claude/rules/project/groww-shared-token-minter-2026-07-02.md`).
///
/// The token at `/tickvault/<env>/groww/access-token` is minted DAILY by the
/// bruteX-owned `groww-token-minter` Lambda (~06:05 IST, EventBridge);
/// TickVault is a read-only consumer via IAM role
/// `groww-token-minter-reader-tickvault` (`ssm:GetParameter` on this ONE
/// parameter + `kms:Decrypt`, delivered through the default AWS credential
/// chain). TickVault NEVER mints a Groww token, NEVER reads the
/// `api-key`/`totp-secret` credential parameters (Lambda-only by IAM design),
/// and NEVER writes any `/tickvault/*/groww/*` parameter. Only called when the
/// Groww feed is enabled (`feeds.groww_enabled`); the Dhan path is untouched.
///
/// # Errors
///
/// Returns `ApplicationError::SecretRetrieval` if the token cannot be fetched.
#[instrument(skip_all, fields(environment))]
// TEST-EXEMPT: live AWS SSM fetch — mirrors the TEST-EXEMPT fetch_dhan_credentials; path construction is covered by build_ssm_path tests.
pub async fn fetch_groww_access_token() -> Result<SecretString, ApplicationError> {
    let environment = resolve_environment()?;
    tracing::Span::current().record("environment", environment.as_str());

    let ssm_client = create_ssm_client().await;

    let token_path = build_ssm_path(&environment, SSM_GROWW_SERVICE, GROWW_ACCESS_TOKEN_SECRET);

    info!(
        token_path = %token_path,
        "fetching Groww access token from SSM (read-only; minted by the bruteX groww-token-minter Lambda)"
    );

    let token = fetch_secret(&ssm_client, &token_path).await?;

    info!("Groww access token fetched successfully from SSM");

    Ok(token)
}

/// Fetches Dhan Sandbox credentials from AWS SSM Parameter Store.
///
/// Sandbox uses a separate client ID and pre-generated token (30-day validity).
/// No TOTP or client-secret needed — token generated manually on DevPortal.
///
/// SSM paths:
/// - `/tickvault/{env}/dhan/sandbox-client-id`
/// - `/tickvault/{env}/dhan/sandbox-token`
///
/// # Errors
///
/// Returns `ApplicationError::SecretRetrieval` if either secret cannot be fetched.
#[instrument(skip_all, fields(environment))]
// WIRING-EXEMPT: reserved for staging profile; wire when staging gets its own SSM path
pub async fn fetch_sandbox_credentials()
-> Result<super::types::SandboxCredentials, ApplicationError> {
    let environment = resolve_environment()?;
    tracing::Span::current().record("environment", environment.as_str());

    let ssm_client = create_ssm_client().await;

    let client_id_path = build_ssm_path(
        &environment,
        SSM_DHAN_SERVICE,
        DHAN_SANDBOX_CLIENT_ID_SECRET,
    );
    let token_path = build_ssm_path(&environment, SSM_DHAN_SERVICE, DHAN_SANDBOX_TOKEN_SECRET);

    info!(
        client_id_path = %client_id_path,
        token_path = %token_path,
        "fetching Dhan sandbox credentials from SSM"
    );

    let (client_id, access_token) = tokio::try_join!(
        fetch_secret(&ssm_client, &client_id_path),
        fetch_secret(&ssm_client, &token_path),
    )?;

    info!("all Dhan sandbox credentials fetched successfully from SSM");

    Ok(super::types::SandboxCredentials {
        client_id,
        access_token,
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
    use tickvault_common::constants::{
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

// `fetch_valkey_password` DELETED in #O4 (2026-05-24) — Valkey itself
// removed from the runtime. The dual-instance lock moved to SSM
// (`/tickvault/<env>/instance-lock`) in PR #764; no other production
// caller needed the Valkey AUTH password.

/// Fetches the API bearer token from AWS SSM Parameter Store.
///
/// Returns `SecretString` (zeroize on drop, `[REDACTED]` Display).
///
/// 2026-04-25 security audit (PR #357 follow-up): the API bearer token
/// was previously read from a process env var (`TV_API_TOKEN`) which is
/// plaintext on disk in Docker `environment:` blocks and visible via
/// `docker inspect` / `/proc/<pid>/environ`. Project mandate per
/// `.claude/rules/project/rust-code.md` is "All secrets from AWS SSM
/// Parameter Store". This function moves the bearer token to the same
/// SSM source as Dhan, QuestDB, Grafana, and Telegram credentials.
///
/// SSM path: `/tickvault/<env>/api/bearer-token`
///
/// # Errors
///
/// Returns `ApplicationError::SecretRetrieval` if the secret cannot be
/// fetched. Callers in `crates/app/src/main.rs` are expected to fall
/// back to the legacy env-var path (with a WARN) for local dev where
/// AWS credentials may be absent, and to halt with CRITICAL in live mode.
#[instrument(skip_all, fields(environment))]
pub async fn fetch_api_bearer_token() -> Result<SecretString, ApplicationError> {
    use tickvault_common::constants::{API_BEARER_TOKEN_SECRET, SSM_API_SERVICE};

    let environment = resolve_environment()?;
    tracing::Span::current().record("environment", environment.as_str());

    let ssm_client = create_ssm_client().await;

    let bearer_token_path = build_ssm_path(&environment, SSM_API_SERVICE, API_BEARER_TOKEN_SECRET);

    info!(
        bearer_token_path = %bearer_token_path,
        "fetching API bearer token from SSM"
    );

    let bearer_token = fetch_secret(&ssm_client, &bearer_token_path).await?;

    info!("API bearer token fetched successfully from SSM");

    Ok(bearer_token)
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
        assert_eq!(path, "/tickvault/dev/dhan/client-id");
    }

    #[test]
    fn test_build_ssm_path_prod() {
        let path = build_ssm_path("prod", SSM_DHAN_SERVICE, DHAN_CLIENT_SECRET_SECRET);
        assert_eq!(path, "/tickvault/prod/dhan/client-secret");
    }

    #[test]
    fn test_build_ssm_path_totp() {
        let path = build_ssm_path("dev", SSM_DHAN_SERVICE, DHAN_TOTP_SECRET);
        assert_eq!(path, "/tickvault/dev/dhan/totp-secret");
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
            "/tickvault/dev/dhan/client-id"
        );
        assert_eq!(
            build_ssm_path("dev", SSM_DHAN_SERVICE, DHAN_CLIENT_SECRET_SECRET),
            "/tickvault/dev/dhan/client-secret"
        );
        assert_eq!(
            build_ssm_path("dev", SSM_DHAN_SERVICE, DHAN_TOTP_SECRET),
            "/tickvault/dev/dhan/totp-secret"
        );
    }

    #[test]
    fn test_build_ssm_path_all_secret_types_prod() {
        assert_eq!(
            build_ssm_path("prod", SSM_DHAN_SERVICE, DHAN_CLIENT_ID_SECRET),
            "/tickvault/prod/dhan/client-id"
        );
        assert_eq!(
            build_ssm_path("prod", SSM_DHAN_SERVICE, DHAN_CLIENT_SECRET_SECRET),
            "/tickvault/prod/dhan/client-secret"
        );
        assert_eq!(
            build_ssm_path("prod", SSM_DHAN_SERVICE, DHAN_TOTP_SECRET),
            "/tickvault/prod/dhan/totp-secret"
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
        use tickvault_common::constants::{SSM_TELEGRAM_SERVICE, TELEGRAM_BOT_TOKEN_SECRET};
        let path = build_ssm_path("dev", SSM_TELEGRAM_SERVICE, TELEGRAM_BOT_TOKEN_SECRET);
        assert_eq!(path, "/tickvault/dev/telegram/bot-token");
    }

    #[test]
    fn test_build_ssm_path_telegram_chat_id() {
        use tickvault_common::constants::{SSM_TELEGRAM_SERVICE, TELEGRAM_CHAT_ID_SECRET};
        let path = build_ssm_path("prod", SSM_TELEGRAM_SERVICE, TELEGRAM_CHAT_ID_SECRET);
        assert_eq!(path, "/tickvault/prod/telegram/chat-id");
    }

    // -----------------------------------------------------------------------
    // build_ssm_path — QuestDB secret paths
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_ssm_path_questdb_dev() {
        assert_eq!(
            build_ssm_path("dev", SSM_QUESTDB_SERVICE, QUESTDB_PG_USER_SECRET),
            "/tickvault/dev/questdb/pg-user"
        );
        assert_eq!(
            build_ssm_path("dev", SSM_QUESTDB_SERVICE, QUESTDB_PG_PASSWORD_SECRET),
            "/tickvault/dev/questdb/pg-password"
        );
    }

    #[test]
    fn test_build_ssm_path_questdb_prod() {
        assert_eq!(
            build_ssm_path("prod", SSM_QUESTDB_SERVICE, QUESTDB_PG_USER_SECRET),
            "/tickvault/prod/questdb/pg-user"
        );
        assert_eq!(
            build_ssm_path("prod", SSM_QUESTDB_SERVICE, QUESTDB_PG_PASSWORD_SECRET),
            "/tickvault/prod/questdb/pg-password"
        );
    }

    // -----------------------------------------------------------------------
    // 2026-04-25 security audit: API bearer token SSM path
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_ssm_path_api_bearer_token_dev() {
        use tickvault_common::constants::{API_BEARER_TOKEN_SECRET, SSM_API_SERVICE};
        assert_eq!(
            build_ssm_path("dev", SSM_API_SERVICE, API_BEARER_TOKEN_SECRET),
            "/tickvault/dev/api/bearer-token"
        );
    }

    #[test]
    fn test_build_ssm_path_api_bearer_token_prod() {
        use tickvault_common::constants::{API_BEARER_TOKEN_SECRET, SSM_API_SERVICE};
        assert_eq!(
            build_ssm_path("prod", SSM_API_SERVICE, API_BEARER_TOKEN_SECRET),
            "/tickvault/prod/api/bearer-token"
        );
    }

    /// Smoke: SSM_API_SERVICE constant is non-empty + lowercase + matches
    /// the project's existing service-name convention (alpha + hyphen).
    #[test]
    fn test_ssm_api_service_constant_is_well_formed() {
        use tickvault_common::constants::SSM_API_SERVICE;
        assert!(
            !SSM_API_SERVICE.is_empty(),
            "SSM_API_SERVICE must not be empty"
        );
        assert!(
            SSM_API_SERVICE
                .chars()
                .all(|c| c.is_ascii_lowercase() || c == '-'),
            "SSM_API_SERVICE must be ascii-lowercase + hyphens, got {}",
            SSM_API_SERVICE
        );
    }

    /// fetch_api_bearer_token must error without real SSM connectivity in
    /// the same way the other fetch helpers do (matches fetch_secret error
    /// surface). Asserts the typed error variant for downstream pattern
    /// matching in main.rs.
    #[tokio::test]
    async fn test_fetch_api_bearer_token_errors_without_real_ssm() {
        let result = fetch_api_bearer_token().await;
        assert!(
            result.is_err(),
            "fetch_api_bearer_token must fail without real SSM connectivity"
        );
        match result.err().unwrap() {
            ApplicationError::SecretRetrieval { path, .. } => {
                assert!(
                    path.contains("/api/bearer-token"),
                    "error path must include /api/bearer-token, got {}",
                    path
                );
            }
            other => panic!("expected SecretRetrieval error, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // #O4 (2026-05-24): Valkey-password SSM tests + the future-wiring
    // ratchet (`test_valkey_wiring_in_main_must_use_ssm`) DELETED along
    // with the `fetch_valkey_password` function. Valkey is no longer
    // part of the runtime; the dual-instance lock moved to SSM in
    // PR #764. The compile-time absence of `ValkeyPool::new` from
    // main.rs is mechanically enforced by the workspace-wide
    // banned-pattern scanner + the deleted `crates/storage/src/valkey_cache.rs`
    // module — any reintroduction of a ValkeyPool would fail to compile
    // because the type no longer exists.
    // -----------------------------------------------------------------------

    /// Phase 0 Item 19d meta-guard (RE-POINTED PR-C2, 2026-07-13): the
    /// dual-instance lock boot gate + heartbeat moved from the deleted
    /// `start_dhan_lane` (main.rs) into `dhan_rest_stack.rs` with the Dhan
    /// live-WS lane retirement (`dual-instance-lock-2026-07-04.md` §3.5).
    /// Splitting the gate from the heartbeat lets the lock expire after the
    /// 90s TTL and a peer silently acquire — same regression class as before,
    /// new home.
    #[test]
    fn test_instance_lock_boot_gate_and_heartbeat_are_wired_together() {
        let stack_rs = std::fs::read_to_string("../app/src/dhan_rest_stack.rs")
            .or_else(|_| std::fs::read_to_string("crates/app/src/dhan_rest_stack.rs"))
            .expect("dhan_rest_stack.rs must be readable from the test working dir");

        let calls_try_acquire = stack_rs.contains("try_acquire_instance_lock");
        let calls_spawn_heartbeat = stack_rs.contains("spawn_instance_lock_heartbeat");
        assert!(
            calls_try_acquire && calls_spawn_heartbeat,
            "dhan_rest_stack.rs must wire BOTH the instance-lock boot gate \
             (try_acquire_instance_lock = {calls_try_acquire}) AND the \
             heartbeat (spawn_instance_lock_heartbeat = \
             {calls_spawn_heartbeat}) — the gate without the heartbeat lets \
             the lock expire after the 90s TTL; the heartbeat without the \
             gate renews a key we never claimed. See \
             .claude/rules/project/dual-instance-lock-2026-07-04.md §3.5."
        );
    }

    /// Dual-instance lock hardening ratchet 1: LOCK-BEFORE-MINT
    /// (RE-POINTED PR-C2, 2026-07-13 — the mint site moved from the deleted
    /// `start_dhan_lane` into `dhan_rest_stack.rs` with the lane retirement;
    /// `dual-instance-lock-2026-07-04.md` §3.5 keeps the contract
    /// byte-identical). The SSM dual-instance lock MUST be acquired BEFORE
    /// the `TokenManager::initialize` token mint — Dhan enforces one active
    /// token at a time, so a mint that precedes the lock lets a losing
    /// second instance invalidate the winner's JWT. Comment-stripped
    /// source-order scan (doc comments cite `TokenManager::initialize`
    /// hundreds of lines before the code site).
    #[test]
    fn test_instance_lock_acquired_before_token_mint() {
        let stack_rs = std::fs::read_to_string("../app/src/dhan_rest_stack.rs")
            .or_else(|_| std::fs::read_to_string("crates/app/src/dhan_rest_stack.rs"))
            .expect("dhan_rest_stack.rs must be readable from the test working dir");
        let stripped: String = stack_rs
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && !t.starts_with("///") && !t.starts_with("//!")
            })
            .collect::<Vec<_>>()
            .join("\n");

        let lock_idx = stripped
            .find("try_acquire_instance_lock")
            .expect("dhan_rest_stack.rs must acquire the dual-instance lock");
        let mint_idx = stripped
            .find("TokenManager::initialize(")
            .expect("dhan_rest_stack.rs must call TokenManager::initialize (the mint)");
        assert!(
            lock_idx < mint_idx,
            "LOCK-BEFORE-MINT regression: `try_acquire_instance_lock` (stripped byte \
             {lock_idx}) must appear BEFORE `TokenManager::initialize(` (stripped byte \
             {mint_idx}) in crates/app/src/dhan_rest_stack.rs — see \
             .claude/rules/project/dual-instance-lock-2026-07-04.md §3.5"
        );
    }

    /// Dual-instance lock hardening ratchet 2: ALWAYS-ON (RE-POINTED PR-C2,
    /// 2026-07-13 — moved to `dhan_rest_stack.rs` with the lane retirement).
    /// The lock MUST NOT be gated on `is_live()` — every Dhan-enabled boot
    /// mints the shared one-active-token JWT. Pins (a) the always-on
    /// LOCK BEFORE MINT marker, (b) the absence of any `is_live()` gate in
    /// the stack, and (c) the RESILIENCE-03 tripwire flag riding into the
    /// TokenManager.
    #[test]
    fn test_instance_lock_not_gated_on_live_mode() {
        let stack_rs = std::fs::read_to_string("../app/src/dhan_rest_stack.rs")
            .or_else(|_| std::fs::read_to_string("crates/app/src/dhan_rest_stack.rs"))
            .expect("dhan_rest_stack.rs must be readable from the test working dir");

        assert!(
            stack_rs.contains("LOCK BEFORE MINT"),
            "dhan_rest_stack.rs lost the `LOCK BEFORE MINT` marker — the \
             dual-instance lock must stay always-on + pre-mint per \
             .claude/rules/project/dual-instance-lock-2026-07-04.md §3.5"
        );
        assert!(
            !stack_rs.contains("is_live()"),
            "the dual-instance lock regressed to a live-mode gate — it must \
             run for EVERY Dhan-enabled boot (sandbox included) per \
             .claude/rules/project/dual-instance-lock-2026-07-04.md"
        );
        assert!(
            stack_rs.contains("instance_lock_held"),
            "dhan_rest_stack.rs lost the RESILIENCE-03 mint-tripwire flag \
             (`instance_lock_held`) that rides into TokenManager::initialize — \
             see .claude/rules/project/dual-instance-lock-2026-07-04.md"
        );
    }

    // #T2b (2026-05-20): test_auth_renewal_audit_is_wired_into_token_manager
    // removed — the auth_renewal_audit table + write_renewal_audit_row
    // wiring it guarded were dropped in the QuestDB table cleanup.

    /// Phase 0 Item 22a meta-guard: the OMS HTTP timeout MUST be
    /// pinned at 5s and the DH-904 retry ladder MUST be wired into
    /// the production OMS construction site.
    ///
    /// 5s is the operator's contract per
    /// `topic-PHASE-0-LEAN-LOCKED.md` Item 22a — "Order placement
    /// 5s REST timeout + DH-904 retry policy". A future tuning that
    /// silently changes the constant (e.g. to 2s for "faster fails")
    /// would surprise the operator at runtime.
    ///
    /// Rule 13: the `compute_dh904_backoff` helper is defined +
    /// tested in `crates/trading/src/oms/dh904_backoff.rs`. This
    /// guard asserts the constant pin so the helper's math contract
    /// stays meaningful.
    #[test]
    fn test_oms_http_timeout_is_pinned_at_5s() {
        assert_eq!(
            tickvault_common::constants::OMS_HTTP_TIMEOUT_SECS,
            5,
            "OMS_HTTP_TIMEOUT_SECS must stay at 5s per Phase 0 Item 22a. \
             Changing this is operator-visible behaviour — propagate to \
             `topic-PHASE-0-LEAN-LOCKED.md` Item 22a before adjusting."
        );
        // The DH-904 backoff ladder lives in the same constants
        // file; pin its total wait here so a future edit that
        // breaks the ladder semantics is caught at build time.
        let total: u64 = tickvault_common::constants::DH904_BACKOFF_SECS.iter().sum();
        assert_eq!(
            total, 150,
            "DH-904 retry ladder total worst-case wait must be 150s \
             (10+20+40+80) per dhan-api-introduction.md rule 8."
        );
    }

    /// Phase 0 Item 21 meta-guard: the indicator engine MUST call
    /// `sanitize_nan_inf` on every snapshot before returning. Without
    /// this, a poisoned indicator (e.g. NaN bollinger from negative
    /// Welford variance accumulation) silently breaks every downstream
    /// strategy because `close < NaN` is always false.
    ///
    /// Rule 13 (audit-findings 2026-04-17): "if a method is defined +
    /// tested but never called, it is a bug". This source-scan guard
    /// fails the build if a future refactor removes the call site in
    /// `crates/trading/src/indicator/engine.rs`.
    #[test]
    fn test_indicator_snapshot_nan_guard_is_wired_into_engine() {
        let engine_rs = std::fs::read_to_string("../trading/src/indicator/engine.rs")
            .or_else(|_| std::fs::read_to_string("crates/trading/src/indicator/engine.rs"))
            .expect("engine.rs must be readable from secret_manager test working dir");

        assert!(
            engine_rs.contains("sanitize_nan_inf"),
            "indicator/engine.rs MUST call `IndicatorSnapshot::sanitize_nan_inf` \
             on every snapshot before returning. Without this clamp, a poisoned \
             indicator (NaN from Welford variance, EMA seeded from corrupt state, \
             missed upstream div-by-zero guard) silently breaks every downstream \
             strategy because `close < NaN` is always false. See Phase 0 Item 21 \
             in .claude/plans/friday-may-15-mega/topic-PHASE-0-LEAN-LOCKED.md."
        );
        assert!(
            engine_rs.contains("tv_indicator_nan_guard_fired_total"),
            "indicator/engine.rs MUST emit the `tv_indicator_nan_guard_fired_total` \
             counter when sanitize_nan_inf clears > 0 fields. Without the counter, \
             a NaN bug elsewhere in the engine math is silently absorbed without \
             operator visibility."
        );
    }

    // RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
    // retirement directive per websocket-connection-scope-lock.md
    // "2026-07-13 Amendment" §B): test_end_of_day_digest_is_wired_into_main died with the machinery it pinned
    // — the 15:31:30 IST EndOfDayDigest scheduler was deleted with
    // `spawn_post_market_tasks` (its digest read the deleted Dhan pool's
    // connection state). The `NotificationEvent::EndOfDayDigest` variant is
    // DORMANT pending the Phase C follow-up (variant cleanup or a
    // Groww-scorecard re-home) — flagged in the PR-C2 plan.

    // RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
    // retirement directive per websocket-connection-scope-lock.md
    // "2026-07-13 Amendment" §B): test_market_open_readiness_confirmation_is_wired_into_main died with the machinery it pinned
    // — the pre-open readiness scheduler was deleted with the lane
    // (its checks read the deleted Dhan pool/universe state). The
    // `MarketOpenReadinessConfirmation` variant is DORMANT pending the
    // Phase C follow-up. The market-open positive signal for the surviving
    // Groww feed is the 15:45 scorecard + feed-health surfaces.

    // #T2b (2026-05-20): test_last_tick_audit_table_is_wired_into_boot_ddl
    // removed — the last_tick_audit table + boot DDL it guarded were
    // dropped in the QuestDB table cleanup.

    // #T2b (2026-05-20): test_orphan_position_audit_table_is_wired_into_boot_ddl
    // removed — the orphan_position_audit table + boot DDL it guarded
    // were dropped in the QuestDB table cleanup. (The watchdog
    // evaluator + Telegram variants are unaffected and stay.)

    // test_option_chain_minute_snapshot_scheduler_is_wired_into_main REMOVED
    // 2026-06-28: the entire option_chain REST subsystem (scheduler + client +
    // expiry warmup + caches + config + error codes + notifications) was deleted
    // per operator directive 2026-06-28 ("drop the option chain entire
    // implementations and its table also"). It was disabled since 2026-06-02
    // with no live consumer; its QuestDB table was dropped 2026-06-23. There is
    // no scheduler spawn to assert anymore.

    /// Phase 0 Item 19f meta-guard: main.rs MUST wire the
    /// `shutdown_notify` -> heartbeat-`Notify` bridge so the
    /// `GracefulRelease` lifecycle audit row + the Valkey DEL run
    /// on Ctrl-C / 15:30 IST close instead of leaving the lock to
    /// expire silently after 90s TTL.
    ///
    /// The bridge clones `instance_lock_shutdown_chain` (an
    /// `Option<Arc<Notify>>`), spawns a small bridge task that
    /// awaits the broader `shutdown_notify`, and triggers the
    /// heartbeat's own Notify. Without this chain, a graceful
    /// shutdown would not surface the heartbeat exit path; the lock
    /// would only get released by TTL expiry, and the
    /// `live_instance_lock` audit table would be missing the
    /// `GracefulRelease` row for every clean exit.
    ///
    /// BUG-1 fix (2026-07-05): the bridge alone was NOT enough — the
    /// heartbeat JoinHandle was dropped at spawn time, so the release
    /// (SSM DeleteParameter) RACED process exit (the live 15:35 IST
    /// graceful EOD stop left the lock in SSM; the 17:59 boot found it
    /// stale at age 8622s). The chain is now `.clone()`d into the bridge
    /// AND threaded (with the heartbeat handle) into `DhanLaneRunHandles`
    /// so `teardown_dhan_lane_tasks` can `notify_one()` it directly and
    /// bound-await the release before the process exits.
    #[test]
    fn test_instance_lock_shutdown_chain_is_wired() {
        let main_rs = std::fs::read_to_string("../app/src/main.rs")
            .or_else(|_| std::fs::read_to_string("crates/app/src/main.rs"))
            .expect("main.rs must be readable from secret_manager test working dir");

        let calls_spawn_heartbeat = main_rs.contains("spawn_instance_lock_heartbeat");
        let declares_chain = main_rs.contains("instance_lock_shutdown_chain");
        // R8-EDGE-1 (2026-07-07): the shutdown Notify now rides in the
        // notify-on-drop `InstanceLockHeartbeatGuard`; the Item 19f bridge
        // clones it via the guard's accessor instead of a bare local.
        let bridges_chain = main_rs.contains("instance_lock_guard.shutdown_handle()");
        let threads_chain_into_lane =
            main_rs.contains("instance_lock_shutdown: instance_lock_shutdown_chain");
        let awaits_release = main_rs.contains("INSTANCE_LOCK_RELEASE_TIMEOUT_SECS");

        if calls_spawn_heartbeat {
            assert!(
                declares_chain,
                "main.rs spawns the instance-lock heartbeat but does NOT declare \
                 `instance_lock_shutdown_chain` — the broader shutdown_notify cannot \
                 bridge into the heartbeat's own Notify, so Ctrl-C / 15:30 IST close \
                 will leave the lock to TTL-expire silently. Wire the bridge per \
                 Phase 0 Item 19f."
            );
            assert!(
                bridges_chain,
                "main.rs must bridge the heartbeat's shutdown Notify \
                 (`instance_lock_guard.shutdown_handle()`) into the bridge task \
                 at the point `shutdown_notify` is constructed; the heartbeat \
                 would never observe the broader shutdown signal."
            );
            assert!(
                threads_chain_into_lane && awaits_release,
                "main.rs must thread `instance_lock_shutdown_chain` (and the \
                 heartbeat JoinHandle) into `DhanLaneRunHandles` and bound-await \
                 the release (INSTANCE_LOCK_RELEASE_TIMEOUT_SECS) in \
                 `teardown_dhan_lane_tasks` — otherwise the SSM DeleteParameter \
                 races process exit and every graceful stop leaks the lock \
                 (BUG-1, live proof 2026-07-05 15:35 IST)."
            );
        }
    }

    // -----------------------------------------------------------------------
    // fetch_secret — error path (no real SSM available)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_fetch_secret_returns_error_without_real_ssm() {
        let ssm_client = create_ssm_client().await;
        let result = fetch_secret(&ssm_client, "/tickvault/test/nonexistent/secret").await;
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
        assert_eq!(path, "/tickvault/staging-v2/dhan/client-id");
    }

    #[test]
    fn test_build_ssm_path_with_custom_service() {
        let path = build_ssm_path("dev", "custom-svc", "api-key");
        assert_eq!(path, "/tickvault/dev/custom-svc/api-key");
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

    // =====================================================================
    // Additional coverage: resolve_environment, build_ssm_path edge cases,
    // validate_environment boundary, error variant assertions
    // =====================================================================

    #[test]
    fn test_validate_environment_only_hyphens() {
        let env = validate_environment("---").expect("only hyphens should be valid");
        assert_eq!(env, "---");
    }

    #[test]
    fn test_validate_environment_long_string() {
        let long = "a".repeat(200);
        let env = validate_environment(&long).expect("long alphanumeric should be valid");
        assert_eq!(env.len(), 200);
    }

    #[test]
    fn test_validate_environment_slash_returns_error() {
        let result = validate_environment("dev/prod");
        assert!(result.is_err(), "slash must be rejected");
    }

    #[test]
    fn test_validate_environment_at_sign_returns_error() {
        let result = validate_environment("dev@prod");
        assert!(result.is_err(), "@ must be rejected");
    }

    #[test]
    fn test_build_ssm_path_empty_strings() {
        // Even if inputs are empty, the function still builds the path
        let path = build_ssm_path("", "", "");
        assert_eq!(path, "/tickvault///");
    }

    #[test]
    fn test_validate_environment_error_type_is_configuration() {
        let result = validate_environment("");
        match result.unwrap_err() {
            ApplicationError::Configuration(msg) => {
                assert!(msg.contains("invalid characters"));
            }
            other => panic!("expected Configuration error, got: {other}"),
        }
    }

    #[test]
    fn test_build_ssm_path_preserves_case() {
        let path = build_ssm_path("DEV", "DHAN", "CLIENT-ID");
        assert_eq!(path, "/tickvault/DEV/DHAN/CLIENT-ID");
    }

    #[test]
    fn test_resolve_environment_defaults_to_prod() {
        // Single-prod-env consolidation (operator 2026-06-30): the default SSM
        // environment is now `prod` (dev/staging retired). This test depends on
        // ENVIRONMENT not being set, which is racy in parallel test execution,
        // so it tests the underlying validate_environment + the pinned default
        // constant, which is the core logic.
        let result = validate_environment(DEFAULT_SSM_ENVIRONMENT);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "prod");
        assert_eq!(DEFAULT_SSM_ENVIRONMENT, "prod");
    }

    #[test]
    fn test_validate_environment_error_contains_input_value() {
        let result = validate_environment("bad/value");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("bad/value"),
            "error should contain the invalid input, got: {err_msg}"
        );
    }

    #[test]
    fn test_validate_environment_dot_returns_error() {
        let result = validate_environment(".");
        assert!(result.is_err(), "dot must be rejected");
    }

    #[test]
    fn test_validate_environment_double_dot_returns_error() {
        let result = validate_environment("..");
        assert!(result.is_err(), "double dot must be rejected");
    }

    #[test]
    fn test_build_ssm_path_with_hyphenated_env() {
        let path = build_ssm_path("dev-us-east-1", SSM_DHAN_SERVICE, DHAN_CLIENT_ID_SECRET);
        assert_eq!(path, "/tickvault/dev-us-east-1/dhan/client-id");
    }

    #[tokio::test]
    async fn test_fetch_questdb_credentials_returns_error_without_real_ssm() {
        let result = fetch_questdb_credentials().await;
        if crate::test_support::has_aws_credentials() {
            assert!(result.is_ok(), "with real AWS creds, fetch should succeed");
        } else {
            assert!(result.is_err(), "must fail without real SSM");
        }
    }

    #[tokio::test]
    async fn test_fetch_telegram_credentials_returns_error_without_real_ssm() {
        let result = fetch_telegram_credentials().await;
        if crate::test_support::has_aws_credentials() {
            assert!(result.is_ok(), "with real AWS creds, fetch should succeed");
        } else {
            assert!(result.is_err(), "must fail without real SSM");
        }
    }

    /// Source-scan ratchet (audit-findings Rule 13):
    /// `main.rs` MUST spawn the two `day_ohlc_orchestrator` tasks at boot.
    /// Without these wires, `DayOhlcTracker::update_tick()` is dead code and
    /// the operator's day OHLC for the 4 IDX_I SIDs never advances.
    ///
    /// Per operator directive 2026-05-26, the explicit `spawn_market_open_arm_task`
    /// was removed alongside the pre-market buffer — `day_open` is now the
    /// first observed live tick LTP after the midnight reset.
    #[test]
    fn test_day_ohlc_orchestrator_is_wired_into_main() {
        let main_rs = std::fs::read_to_string("../app/src/main.rs")
            .or_else(|_| std::fs::read_to_string("crates/app/src/main.rs"))
            .expect("main.rs must be readable");
        assert!(
            main_rs.contains("spawn_day_ohlc_tick_consumer("),
            "main.rs MUST call `day_ohlc_orchestrator::spawn_day_ohlc_tick_consumer` \
             from the boot path. Without it, day_open/high/low/close never advance \
             from the live tick stream."
        );
        assert!(
            main_rs.contains("spawn_supervised_midnight_reset_task("),
            "main.rs MUST call `day_ohlc_orchestrator::spawn_supervised_midnight_reset_task` \
             from the boot path (CCL-02). Without the SUPERVISED wrapper, a panic in the \
             IST-midnight reset task silently takes the daily reset offline and yesterday's \
             day OHLC carries over past IST midnight with no operator signal (INDEX-OHLC-02)."
        );
        assert!(
            main_rs.contains("DayOhlcTracker::new()"),
            "main.rs MUST construct an Arc<DayOhlcTracker> at boot to thread \
             through the two spawn sites."
        );
    }

    /// BP-07 (PROC-01): `main.rs` MUST spawn the supervised OOM-kill monitor
    /// at boot. Without this wire, an OOM kill has no dedicated signal — it is
    /// only caught indirectly (process dies → systemd → missing-SLO page), so
    /// an OOM-loop is indistinguishable from a panic-loop with zero OOM
    /// attribution. The SUPERVISED wrapper (mirrors DISK-WATCHER-01) ensures a
    /// panic in the monitor respawns instead of silently taking OOM detection
    /// offline.
    #[test]
    fn test_oom_monitor_is_wired_into_main() {
        let main_rs = std::fs::read_to_string("../app/src/main.rs")
            .or_else(|_| std::fs::read_to_string("crates/app/src/main.rs"))
            .expect("main.rs must be readable");
        assert!(
            main_rs.contains("spawn_supervised_oom_monitor("),
            "main.rs MUST call `oom_monitor::spawn_supervised_oom_monitor` from \
             the boot path (BP-07 / PROC-01). Without it, an OOM kill fires no \
             dedicated signal and an OOM-loop is indistinguishable from a \
             panic-loop."
        );
    }

    /// Dual-feed scoreboard PR-A (operator 2026-07-10) meta-guard: main.rs
    /// MUST spawn the process-global scoreboard tasks (the boot-time
    /// process-death reconciler + the 15:45 IST daily aggregation) and emit
    /// the `DualFeedDailyScorecard` Telegram from the outer supervisor.
    /// Rule 13 (audit-findings 2026-04-17): a variant/module defined +
    /// tested but never called IS a bug.
    ///
    /// PR-C2 re-shape (2026-07-13): the FAST crash-recovery boot arm was
    /// DELETED with the Dhan live-WS lane, so the old dual-arm
    /// source-order split (one spawn before `return run_shutdown_fast(`,
    /// one after) collapsed to a SINGLE process-global spawn site; the
    /// `create_websocket_pool` anchor ordering retired with the pool. The
    /// process-start anchor threading + the emit/reconciler pins are
    /// unchanged.
    #[test]
    fn test_feed_scoreboard_task_is_wired_into_main() {
        let main_rs = std::fs::read_to_string("../app/src/main.rs")
            .or_else(|_| std::fs::read_to_string("crates/app/src/main.rs"))
            .expect("main.rs must be readable");
        // Call sites (rustfmt wraps the arg list, so the needle is the call
        // head), EXCLUDING the fn definition itself.
        let spawn_sites: Vec<usize> = main_rs
            .match_indices("spawn_feed_scoreboard_tasks(")
            .map(|(i, _)| i)
            .filter(|&i| !main_rs[..i].ends_with("fn "))
            .collect();
        assert_eq!(
            spawn_sites.len(),
            1,
            "main.rs must spawn the dual-feed scoreboard tasks at EXACTLY 1 \
             site (the process-global boot prefix — single boot path since \
             PR-C2); found {}. Without it the feed_scoreboard_boot module + \
             the DualFeedDailyScorecard event are dead code.",
            spawn_sites.len()
        );
        // Every call site must thread the PROCESS-START anchor (round-2
        // HIGH fix 2026-07-10): the reconciler's boot_ts must be the
        // process-start instant, never a Utc::now() stamped when the
        // spawned task starts.
        for &i in &spawn_sites {
            let window = &main_rs[i..main_rs.len().min(i + 240)];
            assert!(
                window.contains("process_start_ist_nanos"),
                "every spawn_feed_scoreboard_tasks call site must thread the \
                 process-start anchor (round-2 hostile review 2026-07-10): {window}"
            );
        }
        // The anchor must be captured BEFORE the spawn site (source order) —
        // a connect row stamped before the anchor would classify as PRE-boot.
        let anchor = main_rs
            .find("let process_start_ist_nanos")
            .expect("main.rs must capture the process-start scoreboard anchor");
        assert!(
            anchor < spawn_sites[0],
            "the process-start scoreboard anchor MUST be captured before the \
             spawn_feed_scoreboard_tasks site — otherwise this boot's own \
             connect rows classify as PRE-boot and the process-death \
             reconciler synthesizes nothing."
        );
        assert!(
            main_rs.contains("NotificationEvent::DualFeedDailyScorecard {"),
            "main.rs MUST emit `NotificationEvent::DualFeedDailyScorecard` from \
             the outer scoreboard supervisor — the daily operator digest per \
             the 2026-07-10 dual-feed directive."
        );
        assert!(
            main_rs.contains("NotificationEvent::DualFeedScorecardAborted {"),
            "main.rs MUST emit `DualFeedScorecardAborted` on the Err/panic arms \
             of the outer scoreboard supervisor — the daily signal must never \
             be silently dropped (audit Rule 11)."
        );
        assert!(
            main_rs.contains("reconcile_process_death_episodes("),
            "main.rs MUST run the boot-time process-death reconciler — without \
             it a mid-session process death leaves NO episode row and the \
             month verdict silently under-counts our own restarts."
        );
    }

    /// BRUTEX-XVERIFY (2026-07-12): `main.rs` MUST spawn the BruteX↔TickVault
    /// daily cross-verify runner
    /// (`brutex_crossverify_boot::spawn_brutex_crossverify_task`) from the
    /// process-global boot prefix. PR-C2 (2026-07-13, merged 2026-07-14): the
    /// FAST crash-recovery arm — this guard's original SECOND spawn path —
    /// was DELETED with the Dhan live-WS lane, so main.rs has a SINGLE boot
    /// path and the guard pins EXACTLY 1 call site (the scoreboard-guard
    /// precedent): a mid-market crash-restart now boots through the same
    /// prefix, so the day's 15:50 IST cross-verify still fires. The spawn is
    /// config-gated (`[brutex_crossverify] enabled`, default OFF).
    /// The boot module must also keep its Telegram emit sites
    /// (`BrutexCrossverifySummary` on success, `BrutexCrossverifyAborted` on
    /// the Err/panic arms) — the daily signal must never be silently dropped
    /// (audit Rule 11).
    #[test]
    fn test_brutex_crossverify_is_wired_into_main() {
        let main_rs = std::fs::read_to_string("../app/src/main.rs")
            .or_else(|_| std::fs::read_to_string("crates/app/src/main.rs"))
            .expect("main.rs must be readable");
        assert!(
            main_rs.contains("brutex_crossverify_boot"),
            "main.rs MUST reference the brutex_crossverify_boot module — \
             without it the BruteX cross-verify runner is dead code."
        );
        // Call sites (call head only), excluding any fn definition.
        let spawn_sites: Vec<usize> = main_rs
            .match_indices("spawn_brutex_crossverify_task(")
            .map(|(i, _)| i)
            .filter(|&i| !main_rs[..i].ends_with("fn "))
            .collect();
        assert_eq!(
            spawn_sites.len(),
            1,
            "main.rs must call spawn_brutex_crossverify_task( at EXACTLY 1 \
             site (the process-global boot prefix — single boot path since \
             PR-C2); found {} call site(s). Without it the runner is dead \
             code on every boot.",
            spawn_sites.len()
        );
        // The boot module keeps its Telegram emit sites (never a silent day).
        let boot_rs = std::fs::read_to_string("../app/src/brutex_crossverify_boot.rs")
            .or_else(|_| std::fs::read_to_string("crates/app/src/brutex_crossverify_boot.rs"))
            .expect("brutex_crossverify_boot.rs must be readable");
        assert!(
            boot_rs.contains("NotificationEvent::BrutexCrossverifySummary {"),
            "brutex_crossverify_boot.rs MUST emit \
             `NotificationEvent::BrutexCrossverifySummary` — the daily \
             operator digest per the 2026-07-12 BRUTEX-XVERIFY directive."
        );
        assert!(
            boot_rs.contains("NotificationEvent::BrutexCrossverifyAborted {"),
            "brutex_crossverify_boot.rs MUST emit `BrutexCrossverifyAborted` \
             on the Err/panic arms — the daily signal must never be silently \
             dropped (audit Rule 11)."
        );
    }

    /// W2 PR#6 (WAL-SUSPEND-01, 2026-07-10, audit follow-up row 10):
    /// `main.rs` MUST spawn the supervised per-table QuestDB WAL-suspension
    /// probe from the process-global monitor block. Without this wire, a
    /// WAL-suspended `ticks` / `candles_1m` table (post disk-full / apply
    /// error) keeps ACKing ILP rows while they silently stop becoming
    /// visible — with ZERO signal: the boot probe + questdb_health check
    /// reachability/connection, never per-table WAL apply. The SUPERVISED
    /// wrapper (mirrors DISK-WATCHER-01) also covers the gauge going stale
    /// on a probe-task death: the respawned watcher re-sets
    /// `tv_questdb_wal_suspended_tables` on its first successful probe.
    #[test]
    fn test_wal_suspension_watcher_is_wired_into_main() {
        let main_rs = std::fs::read_to_string("../app/src/main.rs")
            .or_else(|_| std::fs::read_to_string("crates/app/src/main.rs"))
            .expect("main.rs must be readable");
        assert!(
            main_rs.contains("spawn_supervised_wal_suspension_watcher("),
            "main.rs MUST call `wal_suspension_watcher::\
             spawn_supervised_wal_suspension_watcher` from the process-global \
             monitor block (W2 PR#6 / WAL-SUSPEND-01). Without it, a \
             WAL-suspended QuestDB table silently stops applying writes with \
             zero operator signal — the exact silent-data-visibility-loss \
             class the 2026-07-09 audit flagged."
        );
    }

    // RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
    // retirement directive per websocket-connection-scope-lock.md
    // "2026-07-13 Amendment" §B): test_slo_publisher_supervisor_is_wired_into_main died with the machinery it pinned
    // — the SLO evaluator/publisher was deleted per the operator PARK
    // ruling recorded in the PR-C2 plan (commit 09a41e7e) — its six
    // dimensions were dominated by the deleted Dhan pool/tick-freshness
    // inputs. Re-introduction requires the wave-3-d SLO contract re-scoped
    // to the Groww-only feed set.

    /// W2 PR#5 (2026-07-10, audit follow-up row 15): `main.rs` MUST spawn
    /// the holiday-calendar coverage-horizon staleness watchdog from the
    /// COMMON boot prefix. Without this wire, the `nse_holidays` calendar
    /// (one calendar year at a time; `is_holiday` has NO year bound)
    /// silently runs off its year-end cliff — every un-listed weekday
    /// holiday reads as a trading day, the box auto-starts and burns a full
    /// billable session on a market-closed day — with zero runtime signal
    /// (the CI-only Jan-1 red-build ratchets are reactive, post-cliff).
    /// The watchdog pages the operator (Severity::High, one page per
    /// process per IST day) when < 60 days of coverage remain.
    #[test]
    fn test_calendar_staleness_watchdog_is_wired_into_main() {
        let main_rs = std::fs::read_to_string("../app/src/main.rs")
            .or_else(|_| std::fs::read_to_string("crates/app/src/main.rs"))
            .expect("main.rs must be readable");
        // Comment-stripped scan (hostile-review H1, 2026-07-10): a raw
        // `contains` would still pass with the spawn commented out. Line
        // comments suffice here — main.rs call sites are line-oriented and
        // the anchor string never appears inside a string literal.
        let non_comment: String = main_rs
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            non_comment.contains("spawn_calendar_staleness_watchdog("),
            "main.rs MUST call \
             `calendar_staleness::spawn_calendar_staleness_watchdog` from the \
             common boot prefix (audit follow-up row 15), NOT commented out. \
             Without it, the holiday calendar runs off its year-end cliff \
             with zero runtime warning and un-listed holidays are treated as \
             trading days."
        );
        // Anti-vacuous: the spawn must receive the trading calendar AND the
        // lazily-filled notifier slot (a stub call with neither could
        // satisfy a bare substring check). Scan the comment-stripped
        // call-site region for both argument identifiers.
        let idx = non_comment
            .find("spawn_calendar_staleness_watchdog(")
            .expect("checked above"); // APPROVED: test
        // Char-boundary-safe window end (string literals in main.rs carry
        // non-ASCII; a raw byte slice could split a multi-byte char).
        let mut end = non_comment.len().min(idx + 400);
        while !non_comment.is_char_boundary(end) {
            end -= 1;
        }
        let window = &non_comment[idx..end];
        assert!(
            window.contains("trading_calendar"),
            "the calendar-staleness watchdog call site must pass the \
             process trading_calendar (found call without it)"
        );
        assert!(
            window.contains("notifier_slot"),
            "the calendar-staleness watchdog call site must pass the \
             lazily-filled notifier slot so the High Telegram page can \
             actually be delivered (found call without it)"
        );
    }

    /// Scoreboard PR-D meta-guard: main.rs MUST (a) init the per-instrument
    /// presence registry in the process-global boot prefix BEFORE the Groww
    /// feed spawns (the boot-read fold gate; single boot path since PR-C2,
    /// 2026-07-13 — the fast crash-recovery arm was deleted with the Dhan
    /// live-WS lane) and (b) reset the presence bitsets in the IST-midnight
    /// task next to the day-lag histogram reset.
    // RETIRED (2026-07-18, stage-4 dead-producer sweep):
    // test_feed_presence_is_wired_into_main died with the machinery it
    // pinned — the per-instrument presence registry
    // (pipeline/feed_presence.rs) was deleted; its record/register
    // producers died with the live feeds (Dhan 2026-07-13, Groww
    // 2026-07-15), so the wired init armed a registry nothing could feed.

    // RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
    // retirement directive per websocket-connection-scope-lock.md
    // "2026-07-13 Amendment" §B): test_feed_lag_publisher_supervisor_is_wired_into_main died with the machinery it pinned
    // — the Dhan exchange-lag publisher (tv_dhan_exchange_lag_p99_seconds)
    // retired with the Dhan live feed — no Dhan ticks exist to measure. The
    // Groww lag publisher + its guard retired 2026-07-15 with the Groww
    // live feed (no Groww live ticks exist to measure).

    /// AUTH-GAP-05 (2026-07-06): the mid-session watchdog MUST carry the
    /// forced re-mint trigger — the pure `decide_remint` decision core AND
    /// the `force_renewal` call site. Removing either silently regresses
    /// the token self-heal: a Dhan-killed token would page CRITICAL every
    /// 15 minutes with NO automatic re-mint until the 23h renewal loop.
    #[test]
    fn test_mid_session_remint_trigger_call_site_exists() {
        let watchdog = std::fs::read_to_string("src/auth/mid_session_watchdog.rs")
            .or_else(|_| std::fs::read_to_string("crates/core/src/auth/mid_session_watchdog.rs"))
            .expect("mid_session_watchdog.rs must be readable");
        // COV-1 fix (2026-07-06): scan the PRODUCTION region only — a
        // whole-file scan is vacuous because `decide_remint(` also appears
        // in ~20 test-module call literals, so deleting the production
        // routing would leave the assertion green (the same false-OK class
        // the 2026-07-06 AGGREGATOR-SEAL-01 ratchet hardening closed —
        // wave-6-error-codes.md §1(c)).
        //
        // R7-CPLX-1 fix (2026-07-07) + F8 hardening (2026-07-08): split via
        // the SHARED `source_scan::production_region` helper — robust to the
        // item-level `#[cfg(test)]` attributes on test-only wrappers (e.g.
        // `classify_check_result`) BEFORE the real test module (the old
        // first-literal split truncated the scanned region to ~1/3 of the
        // file, excluding the production classifiers + decision core), AND
        // to any production code that may ever land AFTER the test module.
        let production = tickvault_common::source_scan::production_region(&watchdog).expect(
            "mid_session_watchdog.rs must contain a #[cfg(test)]-gated \
             top-level `mod tests` module — without it this \
             production-region split is scanning the whole file (vacuous)",
        );
        // Split-boundary self-checks: the region must extend past the inline
        // #[cfg(test)] items to the LAST production fns (the decision core).
        for tail_fn in ["fn decide_remint(", "fn apply_remint_decision("] {
            assert!(
                production.contains(tail_fn),
                "mid_session_watchdog.rs production region (everything before \
                 `mod tests`) must include the tail production item \
                 `{tail_fn}` — the split boundary no longer follows the last \
                 production fn (R7-CPLX-1)"
            );
        }
        assert!(
            production.contains("force_renewal("),
            "mid_session_watchdog.rs (production region) MUST call \
             `force_renewal` — the AUTH-GAP-05 forced re-mint trigger \
             reuses the EXISTING TokenManager renewal machinery (never a \
             fork)."
        );
        // COV-R6-1 (2026-07-07): the loop binds the decision once
        // (`let decision = decide_remint(`), applies the retry-once episode
        // latch + cooldown stamp through the pure `apply_remint_decision`,
        // and only then matches on the decision — the previous
        // `match decide_remint(` shape carried the latch as two untested
        // loop-body assignments whose deletion left every test green while
        // the watchdog re-minted (or re-paged RESILIENCE-01) every 900s.
        assert!(
            production.contains("let decision = decide_remint("),
            "mid_session_watchdog.rs (production region) MUST route the \
             re-mint through the pure `decide_remint` decision fn \
             (`let decision = decide_remint(` — threshold -> latch -> \
             lock-fail-closed -> cooldown ordering, AUTH-GAP-05)."
        );
        assert!(
            production.contains("apply_remint_decision(&mut state, &mut last_remint_at, decision)"),
            "mid_session_watchdog.rs (production region) MUST apply the \
             retry-once episode latch + cooldown stamp via the pure \
             `apply_remint_decision(&mut state, &mut last_remint_at, \
             decision)` call (COV-R6-1) — without it a sustained dead-token \
             episode re-mints (one HIGH Telegram + one generateAccessToken \
             attempt) OR re-pages RESILIENCE-01 every 900s cycle, violating \
             the operator-locked once-per-episode bound."
        );
        assert!(
            production.contains("match decision {"),
            "mid_session_watchdog.rs (production region) MUST act on the \
             SAME bound decision (`match decision {{`) that was passed to \
             `apply_remint_decision` — re-calling decide_remint would let \
             the latch and the action diverge."
        );
        // Non-vacuity self-check (R7-CPLX-1 / F8): the shared helper only
        // returns Some when it found a #[cfg(test)]-gated `mod tests` block
        // to excise; assert the excision actually happened.
        assert!(
            !production.contains("\nmod tests {"),
            "mid_session_watchdog.rs production region must have its \
             #[cfg(test)] mod tests block excised (blanked) — the \
             production-region split boundary is no longer the test module"
        );
    }

    /// AUTH-GAP-05 (2026-07-06) / GAP-06 re-home (2026-07-14, Dhan noise
    /// lock): the live token-health gauge poller keeps
    /// `tv_token_remaining_seconds` LIVE and publishes the AND-composed
    /// `tv_token_valid` — without it both regress to the frozen mint-time
    /// snapshots (false-OK, audit Rule 11) and the family-(4)
    /// `tv-<env>-token-remaining-low` alarm goes blind.
    ///
    /// GAP-06 (2026-07-14): the poller's HOME is `dhan_rest_stack.rs`
    /// Phase 3 — the one Dhan bring-up path (sharing the stack watchdog's
    /// profile-truth flag). PR-C2 (2026-07-13, merged 2026-07-14): the lane
    /// + fast arm — the poller's original two main.rs spawn sites — are
    /// DELETED, so the old lane-handle registration assertions retired with
    /// them. Surviving pins: (a) the stack spawn EXISTS exactly once in the
    /// production region, (b) main.rs's production region has ZERO
    /// gauge-poller spawn sites (a re-added spawn would interleave with the
    /// stack's poller — gauge flapping every ≤15s), (c) the mid-session
    /// profile watchdog (AUTH-GAP-05's self-heal driver) rides along in the
    /// stack.
    #[test]
    fn test_token_health_gauge_poller_wired_into_main() {
        let main_rs = std::fs::read_to_string("../app/src/main.rs")
            .or_else(|_| std::fs::read_to_string("crates/app/src/main.rs"))
            .expect("main.rs must be readable");
        let production = tickvault_common::source_scan::production_region(&main_rs).expect(
            "main.rs must contain a #[cfg(test)]-gated top-level `mod tests` \
             module — without it this production-region split would scan the \
             whole file (vacuous)",
        );
        let spawn_count = production
            .matches("spawn_token_health_gauge_poller(")
            .count();
        assert_eq!(
            spawn_count, 0,
            "main.rs (production region) must have ZERO token-health \
             gauge-poller spawn sites — GAP-06 (2026-07-14) homes the \
             poller in dhan_rest_stack.rs Phase 3; a re-added main.rs \
             spawn would interleave with the stack's poller (gauge \
             flapping every ≤15s). Found {spawn_count}."
        );
        let stack_rs = std::fs::read_to_string("../app/src/dhan_rest_stack.rs")
            .or_else(|_| std::fs::read_to_string("crates/app/src/dhan_rest_stack.rs"))
            .expect("dhan_rest_stack.rs must be readable");
        let stack_production = tickvault_common::source_scan::production_region(&stack_rs).expect(
            "dhan_rest_stack.rs must contain a #[cfg(test)]-gated top-level \
                 `mod tests` module — without it this production-region split \
                 would scan the whole file (vacuous)",
        );
        assert_eq!(
            stack_production
                .matches("spawn_token_health_gauge_poller(")
                .count(),
            1,
            "dhan_rest_stack.rs (production region) MUST spawn the live \
             token-health gauge poller exactly once (GAP-06 re-home) — \
             deleting it blinds the tv-<env>-token-remaining-low alarm \
             (family-(4)) after a renewal-loop circuit-breaker halt."
        );
        assert!(
            stack_production.contains("spawn_mid_session_profile_watchdog("),
            "dhan_rest_stack.rs (production region) MUST spawn the \
             mid-session profile watchdog (AUTH-GAP-05 forced-remint \
             self-heal) alongside the gauge poller."
        );
        // Non-vacuity self-check: the shared helper only returns Some when
        // it found a #[cfg(test)]-gated `mod tests` block to excise.
        assert!(
            !production.contains("\nmod tests {"),
            "main.rs production region must have its #[cfg(test)] mod tests \
             block excised (blanked) — the split found no test module \
             (vacuous whole-file scan)"
        );
    }

    // RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
    // retirement directive per websocket-connection-scope-lock.md
    // "2026-07-13 Amendment" §B): test_lane_handles_constructed_before_boot_completed_await died with the machinery it pinned
    // — `DhanLaneRunHandles` + `start_dhan_lane` were deleted with the
    // lane; the surviving Dhan surface (dhan_rest_stack.rs) owns its task
    // handles directly.

    // (PR-C2, 2026-07-13: the `main_rs_production_region` helper retired with
    // its last consumers — the lane wiring guards above.)

    // RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
    // retirement directive per websocket-connection-scope-lock.md
    // "2026-07-13 Amendment" §B): test_pre_lane_abort_guard_covers_early_spawn_windows died with the machinery it pinned
    // — `PreLaneAbortGuard` + the lane spawn windows were deleted with
    // `start_dhan_lane`.

    // RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
    // retirement directive per websocket-connection-scope-lock.md
    // "2026-07-13 Amendment" §B): test_instance_lock_heartbeat_guard_covers_pre_lane_window died with the machinery it pinned
    // — `InstanceLockHeartbeatGuard` + the pre-lane window were deleted
    // with `start_dhan_lane`; the instance-lock heartbeat now lives in
    // dhan_rest_stack.rs (see test_instance_lock_boot_gate_and_heartbeat_
    // are_wired_together, re-pointed there).

    // (PR-C2, 2026-07-13: the `no_ws` helper retired with its last consumers.)

    // RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
    // retirement directive per websocket-connection-scope-lock.md
    // "2026-07-13 Amendment" §B): test_market_open_one_shots_latched_and_gated died with the machinery it pinned
    // — the three market-open one-shots (streaming confirmation /
    // self-test / readiness) were deleted with the lane — their verdicts
    // read the deleted Dhan pool state. The FirstSeenSet midnight reset
    // they co-guarded was lane-scoped and retired with them.

    // RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
    // retirement directive per websocket-connection-scope-lock.md
    // "2026-07-13 Amendment" §B): test_ip_monitor_and_heartbeat_watchdog_are_lane_owned died with the machinery it pinned
    // — the runtime IP monitor + 30s heartbeat watchdog were lane-owned
    // and retired with `start_dhan_lane`/`DhanLaneRunHandles`. The static-IP
    // boot gate for the surviving Dhan REST surface lives in
    // dhan_rest_stack.rs.
}

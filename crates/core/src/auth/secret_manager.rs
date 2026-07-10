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

    /// Phase 0 Item 19d meta-guard: main.rs MUST wire the
    /// instance-lock boot gate + heartbeat together.
    ///
    /// If a future refactor splits the gate from the heartbeat (e.g.,
    /// removes `spawn_instance_lock_heartbeat` while keeping
    /// `try_acquire_instance_lock`), the lock would expire after 90s
    /// and the next process would silently acquire — defeating the
    /// dual-instance protection. This test blocks that regression.
    ///
    /// Symmetrically, removing the boot gate while keeping the
    /// heartbeat would leave a dormant heartbeat task running with
    /// nothing to renew, churning Valkey for no reason. Also blocked.
    #[test]
    fn test_instance_lock_boot_gate_and_heartbeat_are_wired_together() {
        let main_rs = std::fs::read_to_string("../app/src/main.rs")
            .or_else(|_| std::fs::read_to_string("crates/app/src/main.rs"))
            .expect("main.rs must be readable from secret_manager test working dir");

        let calls_try_acquire = main_rs.contains("try_acquire_instance_lock");
        let calls_spawn_heartbeat = main_rs.contains("spawn_instance_lock_heartbeat");

        // Either both or neither — splitting them is the regression class.
        assert_eq!(
            calls_try_acquire, calls_spawn_heartbeat,
            "main.rs wires the instance-lock boot gate and heartbeat \
             asymmetrically: try_acquire_instance_lock = {calls_try_acquire}, \
             spawn_instance_lock_heartbeat = {calls_spawn_heartbeat}. They must \
             be wired together — the gate without the heartbeat lets the lock \
             expire after 90 s (TTL); the heartbeat without the gate runs against \
             a key we never claimed. See \
             .claude/rules/project/wave-4-error-codes.md::RESILIENCE-01."
        );
    }

    /// Dual-instance lock hardening (operator "go" 2026-07-04) ratchet 1:
    /// LOCK-BEFORE-MINT. Inside `start_dhan_lane` the SSM dual-instance
    /// lock MUST be acquired BEFORE the Step 6 `TokenManager::initialize`
    /// token mint. Dhan enforces one active token at a time, so a mint
    /// that precedes the lock lets a losing second instance invalidate
    /// the winner's JWT before it is blocked — the exact 2026-07-04
    /// coexistence-audit incident class. Source-order scan: the first
    /// `try_acquire_instance_lock` call site must appear BEFORE the
    /// first `TokenManager::initialize(` call site in main.rs.
    #[test]
    fn test_instance_lock_acquired_before_token_mint() {
        let main_rs = std::fs::read_to_string("../app/src/main.rs")
            .or_else(|_| std::fs::read_to_string("crates/app/src/main.rs"))
            .expect("main.rs must be readable from secret_manager test working dir");

        let lock_idx = main_rs
            .find("try_acquire_instance_lock")
            .expect("main.rs must acquire the dual-instance lock");
        let mint_idx = main_rs
            .find("TokenManager::initialize(")
            .expect("main.rs must call TokenManager::initialize (slow-boot mint)");
        assert!(
            lock_idx < mint_idx,
            "LOCK-BEFORE-MINT regression: `try_acquire_instance_lock` (byte {lock_idx}) \
             must appear BEFORE `TokenManager::initialize(` (byte {mint_idx}) in \
             crates/app/src/main.rs — see \
             .claude/rules/project/dual-instance-lock-2026-07-04.md"
        );
    }

    /// Dual-instance lock hardening (operator "go" 2026-07-04) ratchet 2:
    /// ALWAYS-ON. The lock block MUST NOT be gated on `is_live()` — the
    /// pre-2026-07-04 gate left it permanently OFF (prod + local both run
    /// mode = "sandbox") while every Dhan-enabled boot still mints the
    /// shared one-active-token JWT. Pins (a) the always-on marker comment
    /// and (b) the absence of the retired live-mode gate expression, and
    /// (c) the RESILIENCE-03 tripwire flag riding into the TokenManager.
    #[test]
    fn test_instance_lock_not_gated_on_live_mode() {
        let main_rs = std::fs::read_to_string("../app/src/main.rs")
            .or_else(|_| std::fs::read_to_string("crates/app/src/main.rs"))
            .expect("main.rs must be readable from secret_manager test working dir");

        assert!(
            main_rs.contains("LOCK BEFORE MINT"),
            "main.rs lost the `LOCK BEFORE MINT` Step 6a-prime marker — the \
             dual-instance lock must stay always-on + pre-mint per \
             .claude/rules/project/dual-instance-lock-2026-07-04.md"
        );
        assert!(
            !main_rs
                .contains("instance_lock_handle: Option<tokio::task::JoinHandle<()>> = if trading_mode.is_live()"),
            "the dual-instance lock regressed to the retired `is_live()` gate — \
             it must run for EVERY Dhan-enabled boot (sandbox included) per \
             .claude/rules/project/dual-instance-lock-2026-07-04.md"
        );
        assert!(
            main_rs.contains("instance_lock_held"),
            "main.rs lost the RESILIENCE-03 mint-tripwire flag (`instance_lock_held`) \
             that rides into TokenManager::initialize — see \
             .claude/rules/project/dual-instance-lock-2026-07-04.md"
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

    /// Phase 0 Item 22d meta-guard: main.rs MUST wire the end-of-day
    /// digest scheduler that fires the `EndOfDayDigest` Telegram
    /// once per trading day at 15:31:30 IST.
    ///
    /// The variant exists in `NotificationEvent` and the test suite
    /// already pins its message + severity, but defining the event
    /// without emitting it is exactly Rule 13 (audit-findings 2026-04-17):
    /// "If a method exists + is tested but is never called, it IS a bug."
    /// This source-scan guard fails the build if a future refactor
    /// removes the call site in main.rs.
    #[test]
    fn test_end_of_day_digest_is_wired_into_main() {
        let main_rs = std::fs::read_to_string("../app/src/main.rs")
            .or_else(|_| std::fs::read_to_string("crates/app/src/main.rs"))
            .expect("main.rs must be readable from secret_manager test working dir");

        assert!(
            main_rs.contains("NotificationEvent::EndOfDayDigest {"),
            "main.rs MUST emit `NotificationEvent::EndOfDayDigest` from the \
             scheduler spawned in the boot path. The variant defined in \
             `crates/core/src/notification/events.rs` is dead code without it. \
             See Phase 0 Item 22d in \
             .claude/plans/friday-may-15-mega/topic-PHASE-0-LEAN-LOCKED.md."
        );
        assert!(
            main_rs.contains("15, 31, 30"),
            "main.rs MUST schedule the end-of-day digest at 15:31:30 IST. \
             Earlier/later timings break the contract: the 90s offset after \
             15:30 close lets the market-close shutdown signal settle before \
             the digest reads final connection state."
        );
    }

    /// Phase 0 Item 22c meta-guard: main.rs MUST emit
    /// `NotificationEvent::MarketOpenReadinessConfirmation` from the
    /// once-per-trading-day pre-open scheduler. This is the operator's
    /// positive "we are READY for the open" Telegram that closes the
    /// false-OK gap from audit-findings-2026-04-17.md Rule 11.
    ///
    /// Operator-facing runbook `docs/runbooks/operator-daily-startup.md`
    /// names this Telegram as check #1 of the 5 mandatory pre-market
    /// checks. If the wiring is removed, the operator's 08:30 IST
    /// checklist breaks silently — no error, just a missing positive
    /// signal that takes 45 minutes (until 09:15:30 IST
    /// `MarketOpenStreamingConfirmation`) to escalate as a gap.
    ///
    /// This source-scan guard fails the build if a future refactor
    /// removes the call site in main.rs.
    #[test]
    fn test_market_open_readiness_confirmation_is_wired_into_main() {
        let main_rs = std::fs::read_to_string("../app/src/main.rs")
            .or_else(|_| std::fs::read_to_string("crates/app/src/main.rs"))
            .expect("main.rs must be readable from secret_manager test working dir");

        assert!(
            main_rs.contains("NotificationEvent::MarketOpenReadinessConfirmation {"),
            "main.rs MUST emit `NotificationEvent::MarketOpenReadinessConfirmation` \
             from the pre-open scheduler spawned in the boot path. The variant \
             defined in `crates/core/src/notification/events.rs` is dead code \
             without it. See Phase 0 Item 22c in \
             .claude/plans/friday-may-15-mega/topic-PHASE-0-LEAN-LOCKED.md and \
             `docs/runbooks/operator-daily-startup.md` check #1."
        );
        assert!(
            main_rs.contains("readiness_notifier"),
            "main.rs MUST hold a `readiness_notifier` handle for the pre-open \
             scheduler task. Removing the handle decouples the scheduler from \
             the notification service and would silently lose the readiness \
             Telegram for every trading day."
        );
    }

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
    /// process-death reconciler + the 15:45 IST daily aggregation) on BOTH
    /// boot paths and emit the `DualFeedDailyScorecard` Telegram from the
    /// outer supervisor. Rule 13 (audit-findings 2026-04-17): a
    /// variant/module defined + tested but never called IS a bug — this
    /// source-scan fails the build if a future refactor removes the call
    /// sites.
    ///
    /// Hostile review 2026-07-10 (CRITICAL): the FAST crash-recovery arm
    /// `return run_shutdown_fast(...)`s and never reaches the slow-path
    /// spawn — yet a mid-market process death restarts through EXACTLY that
    /// arm, the one boot that must synthesize the process_death episode and
    /// own the day's 15:45 scorecard. This ratchet therefore pins TWO call
    /// sites by SOURCE ORDER (the per-boot-path source-order-scan pattern):
    /// one BEFORE the fast arm's `return run_shutdown_fast(` and one AFTER
    /// it (the slow/process-global prefix).
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
        // Every call site must thread the PROCESS-START anchor (round-2
        // HIGH fix 2026-07-10): the reconciler's boot_ts must be the
        // process-start instant, never a Utc::now() stamped when the
        // spawned task starts — on the fast arm that ran AFTER the feeds
        // already connected, so every connect row classified PRE-boot and
        // the fast arm synthesized nothing.
        for &i in &spawn_sites {
            let window = &main_rs[i..main_rs.len().min(i + 240)];
            assert!(
                window.contains("process_start_ist_nanos"),
                "every spawn_feed_scoreboard_tasks call site must thread the \
                 process-start anchor (round-2 hostile review 2026-07-10): {window}"
            );
        }
        // The anchor must be captured BEFORE any WebSocket pool can be
        // created on ANY boot path (source-order scan): a connect row
        // stamped before the anchor would classify as PRE-boot.
        let anchor = main_rs
            .find("let process_start_ist_nanos")
            .expect("main.rs must capture the process-start scoreboard anchor");
        let first_pool = main_rs
            .find("create_websocket_pool(")
            .expect("main.rs must reference create_websocket_pool");
        assert!(
            anchor < first_pool,
            "the process-start scoreboard anchor MUST be captured before every \
             create_websocket_pool site (fast arm included) — otherwise this \
             boot's own connect rows classify as PRE-boot and the process-death \
             reconciler synthesizes nothing (round-2 hostile review 2026-07-10)."
        );
        // The CODE form of the fast-arm return (the call's arg list opens on
        // the next line) — a prose mention in a comment carries `(...)` on
        // the same line and must NOT anchor the split.
        let fast_return = main_rs
            .match_indices("return run_shutdown_fast(")
            .map(|(i, m)| (i, &main_rs[i + m.len()..]))
            .find(|(_, rest)| rest.starts_with('\n') || rest.starts_with('\r'))
            .map(|(i, _)| i)
            .expect("main.rs must contain the fast-boot `return run_shutdown_fast(` arm");
        assert!(
            spawn_sites.iter().any(|&i| i < fast_return),
            "main.rs MUST spawn the dual-feed scoreboard tasks on the FAST \
             crash-recovery boot arm (BEFORE `return run_shutdown_fast(`) — \
             a mid-market process death restarts through that arm, and \
             without the spawn the flagship crash-restart day records NO \
             process_death episode, NO 15:45 scorecard and NO Aborted page. \
             See .claude/plans/active-plan-dual-feed-scoreboard.md."
        );
        assert!(
            spawn_sites.iter().any(|&i| i > fast_return),
            "main.rs MUST spawn the dual-feed scoreboard tasks from the \
             slow-boot process-global prefix too (next to \
             spawn_daily_tick_conservation_task) — without it the \
             feed_scoreboard_boot module + the DualFeedDailyScorecard event \
             are dead code on normal boots."
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

    /// SLO-03 (live incident 2026-07-03 10:35 IST): `main.rs` MUST spawn the
    /// SLO evaluator/publisher through the SUPERVISED wrapper, never as a bare
    /// `tokio::spawn`. The bare spawn died silently mid-market — last
    /// `tv_realtime_guarantee_score` datapoint 10:35 IST, no `error!`, no
    /// respawn — and the guarantee-critical alarm false-OK'd on missing data
    /// (missing→NonBreaching). The SUPERVISED wrapper (mirrors
    /// DISK-WATCHER-01 / PROC-01 / WS-GAP-05) logs `error!(code = "SLO-03")`,
    /// increments `tv_slo_publisher_respawn_total{reason}`, and respawns with
    /// bounded backoff so the guarantee-score stream can never vanish
    /// silently again.
    #[test]
    fn test_slo_publisher_supervisor_is_wired_into_main() {
        let main_rs = std::fs::read_to_string("../app/src/main.rs")
            .or_else(|_| std::fs::read_to_string("crates/app/src/main.rs"))
            .expect("main.rs must be readable");
        assert!(
            main_rs.contains("spawn_supervised_slo_publisher("),
            "main.rs MUST spawn the SLO evaluator/publisher via \
             `spawn_supervised_slo_publisher` (SLO-03). A bare tokio::spawn \
             regresses the 2026-07-03 10:35 IST silent-death incident: the \
             tv_realtime_guarantee_score stream stops with no error!, no \
             counter, no respawn, and the guarantee-critical alarm false-OKs \
             on missing data."
        );
        // The supervisor must be the ONLY spawn path for the publisher: the
        // inner task fn must have exactly one non-doc call site (inside the
        // supervisor loop), so nobody re-introduces a second, unsupervised
        // spawn of the same loop.
        let inner_call_sites = main_rs
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//")
                    && !t.starts_with("///")
                    && t.contains("spawn_slo_publisher_task(")
            })
            .count();
        assert_eq!(
            inner_call_sites,
            2, // 1 definition (`fn spawn_slo_publisher_task(`) + 1 supervisor call site
            "spawn_slo_publisher_task must be called ONLY from the SLO-03 \
             supervisor loop (found {inner_call_sites} non-comment mentions; \
             expected exactly 2: the fn definition + the supervisor call site)"
        );
    }

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

    /// Silent-feed hardening Item 4 (2026-07-06 incident): the Dhan
    /// exchange-lag p99 publisher MUST be spawned via its supervisor
    /// (respawn + counter — WS-GAP-05 / SLO-03 pattern). A bare
    /// `tokio::spawn` would regress the exact silent-task-death class the
    /// SLO-03 incident proved: the `tv_dhan_exchange_lag_p99_seconds`
    /// stream stops with no error!, no counter, no respawn, and the lag
    /// alarm false-OKs on missing data (`notBreaching`).
    #[test]
    fn test_feed_lag_publisher_supervisor_is_wired_into_main() {
        let main_rs = std::fs::read_to_string("../app/src/main.rs")
            .or_else(|_| std::fs::read_to_string("crates/app/src/main.rs"))
            .expect("main.rs must be readable");
        assert!(
            main_rs.contains("spawn_supervised_feed_lag_publisher("),
            "main.rs MUST spawn the Dhan exchange-lag publisher via \
             `spawn_supervised_feed_lag_publisher` (silent-feed hardening \
             Item 4). A bare tokio::spawn regresses the SLO-03 \
             silent-death class for the lag gauge."
        );
        // Round-1 fix (2026-07-07, findings 1/6/9): the supervisor MUST be
        // spawned from BOTH boot arms — the FAST crash-recovery arm
        // (which `return run_shutdown_fast(...)`s before `start_dhan_lane`
        // is ever reached) AND the slow lane (`start_dhan_lane`). One-site
        // wiring left the lag gauge dark for the whole session after any
        // mid-market crash restart (WS-GAP-09 exit(2) → systemd restart)
        // while the lag alarm silently read notBreaching on missing data.
        // Exactly 2 non-comment, non-definition call sites, each behind the
        // once-per-process guard.
        let supervisor_call_sites = main_rs
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//")
                    && !t.starts_with("///")
                    && !t.contains("fn spawn_supervised_feed_lag_publisher")
                    && t.contains("spawn_supervised_feed_lag_publisher(")
            })
            .count();
        assert_eq!(
            supervisor_call_sites, 2,
            "spawn_supervised_feed_lag_publisher must have EXACTLY 2 call \
             sites in main.rs (fast crash-recovery arm + start_dhan_lane); \
             found {supervisor_call_sites}. Removing the fast-boot spawn \
             silently darkens the lag alarm for every mid-market crash \
             restart session (notBreaching on missing data)."
        );
        let guarded_sites = main_rs
            .matches("FEED_LAG_PUBLISHER_SUPERVISOR_SPAWNED.swap(true")
            .count();
        assert_eq!(
            guarded_sites, 2,
            "both feed-lag publisher spawn sites must sit behind the \
             once-per-process FEED_LAG_PUBLISHER_SUPERVISOR_SPAWNED guard \
             (found {guarded_sites} guarded sites; expected 2)"
        );
        // The supervisor must be the ONLY spawn path for the publisher
        // loop: exactly one non-comment call site of the inner loop fn
        // (inside the supervisor), so nobody re-introduces a second,
        // unsupervised spawn.
        let inner_call_sites = main_rs
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//")
                    && !t.starts_with("///")
                    && t.contains("run_dhan_lag_publisher(")
            })
            .count();
        assert_eq!(
            inner_call_sites, 1,
            "run_dhan_lag_publisher must be called ONLY from the feed-lag \
             supervisor loop (found {inner_call_sites} non-comment mentions; \
             expected exactly 1: the supervisor call site)"
        );
    }

    /// Session-B fix #1 (operator go 2026-07-04): the Groww scale-FLEET
    /// spawn in `main.rs` MUST be gated by the fleet dual-instance SSM lock
    /// (`acquire_groww_scale_fleet_lock`). A scale-test boot runs
    /// `dhan_enabled=false` and never reaches the Dhan RESILIENCE-01 lock,
    /// so removing this gate silently re-opens the class where two hosts
    /// (Mac + AWS, or two Macs) scale the SAME Groww account simultaneously
    /// and the failure masquerades as provider throttle (GROWW-SCALE-05).
    #[test]
    fn test_groww_scale_fleet_lock_is_wired_into_main() {
        let main_rs = std::fs::read_to_string("../app/src/main.rs")
            .or_else(|_| std::fs::read_to_string("crates/app/src/main.rs"))
            .expect("main.rs must be readable");
        assert!(
            main_rs.contains("acquire_groww_scale_fleet_lock("),
            "main.rs MUST gate the Groww scale-fleet spawn behind \
             `groww_scale_lock::acquire_groww_scale_fleet_lock` \
             (GROWW-SCALE-05, Session-B fix 2026-07-04). Without it, two \
             tickvault instances can scale the SAME Groww account and the \
             failure masquerades as provider throttle."
        );
        // The lock decision must happen BEFORE the fleet spawn in source
        // order — gating after the spawn would be a false gate.
        let lock_idx = main_rs
            .find("acquire_groww_scale_fleet_lock(")
            .expect("lock call site present (asserted above)");
        let fleet_idx = main_rs
            .find("spawn_groww_scale_fleet(")
            .expect("main.rs must still spawn the scale fleet somewhere");
        assert!(
            lock_idx < fleet_idx,
            "the fleet dual-instance lock must be acquired BEFORE \
             spawn_groww_scale_fleet in main.rs source order \
             (lock at byte {lock_idx}, fleet spawn at byte {fleet_idx})"
        );
    }

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

    /// AUTH-GAP-05 (2026-07-06): `main.rs` MUST spawn the live token-health
    /// gauge poller. Without it, `tv_token_remaining_seconds` regresses to
    /// the frozen mint-time snapshot (stale ~86,395 for up to 23h after a
    /// killed token) and the honest AND-composed `tv_token_valid` gauge
    /// never exists — a false-OK class regression (audit Rule 11).
    ///
    /// COV-R2-1 hardening (2026-07-06): the design deliberately has TWO
    /// production spawn sites — the SLOW lane (`start_dhan_lane`) AND the
    /// FAST crash-recovery boot arm (EDGE-2) — so the scan (a) covers the
    /// PRODUCTION region only (a future main.rs test mentioning the fn
    /// would otherwise make a whole-file `contains` fully vacuous), (b)
    /// requires BOTH sites (a single `contains` stayed green when either
    /// one was deleted — in particular the fast-arm spawn, silently
    /// re-opening the round-1 "single spawn site in the SLOW lane only"
    /// finding), (c) pins the SLOW lane's AG5-R1 LANE-OWNERSHIP
    /// registration (`token_health_gauge_handle: Some(` +
    /// `mid_session_watchdog_handle: Some(`) — a revert to a detached
    /// `let _ = tokio::spawn` would keep the spawn count green while
    /// restoring the round-1 leaked-poller false-tv_token_valid=1.0
    /// finding — and (d) (COV-R3-1, 2026-07-06) pins the FAST arm's
    /// lane-ownership BINDING (`let token_health_gauge_handle =
    /// token_manager.as_ref().map(`): the fast arm registers its handle
    /// positionally via `run_shutdown_fast` shorthand field init (never
    /// the `: Some(` text), so without this assertion a regression that
    /// keeps the fast-arm spawn call but discards the handle
    /// (`let _ = token_manager.as_ref().map(...)` + passing `None`) stayed
    /// green on (a)-(c) while re-opening the leaked-poller class on the
    /// fast arm.
    #[test]
    fn test_token_health_gauge_poller_wired_into_main() {
        let main_rs = std::fs::read_to_string("../app/src/main.rs")
            .or_else(|_| std::fs::read_to_string("crates/app/src/main.rs"))
            .expect("main.rs must be readable");
        // R7-CPLX-1 idiom hardening (2026-07-07): split at the test MODULE
        // (`mod tests`), not the first raw `#[cfg(test)]` occurrence — an
        // inline item-level #[cfg(test)] attribute (or a doc-comment
        // mention) added earlier in main.rs would silently truncate the
        // scanned "production region" (the token_manager.rs false-OK class).
        let production = main_rs_production_region(&main_rs);
        let spawn_count = production
            .matches("spawn_token_health_gauge_poller(")
            .count();
        assert!(
            spawn_count >= 2,
            "main.rs (production region) MUST spawn the live token-health \
             gauge poller at BOTH boot arms — the slow lane \
             (start_dhan_lane) AND the FAST crash-recovery arm (EDGE-2). \
             Found {spawn_count} spawn site(s); removing either re-opens \
             the round-1 single-spawn-site gap (AUTH-GAP-05)."
        );
        assert!(
            production.contains("token_health_gauge_handle: Some("),
            "main.rs (production region) MUST register the slow lane's \
             gauge-poller handle lane-owned \
             (`token_health_gauge_handle: Some(`) in DhanLaneRunHandles — \
             a detached spawn survives the lane teardown and publishes \
             tv_token_valid=1.0 for up to ~24h while Dhan is deliberately \
             OFF (AG5-R1, audit Rule 11)."
        );
        assert!(
            production.contains("mid_session_watchdog_handle: Some("),
            "main.rs (production region) MUST register the mid-session \
             watchdog handle lane-owned \
             (`mid_session_watchdog_handle: Some(`) in DhanLaneRunHandles — \
             a leaked watchdog keeps hitting /v2/profile with the dead \
             lane's token and fires false RESILIENCE-01 pages (AG5-R1)."
        );
        // COV-R3-1: the FAST arm's handle registration is positional
        // (shorthand field init into `run_shutdown_fast`), so the `Some(`
        // pins above cover the SLOW lane only. Pin the fast-arm BINDING —
        // a `let _ = token_manager.as_ref().map(...)` regression (spawn
        // kept, handle discarded, `None` passed into run_shutdown_fast)
        // would otherwise stay green while leaking a non-lane-owned poller
        // publishing tv_token_valid=1.0 from a torn-down lane's
        // TokenManager.
        assert!(
            production.contains("let token_health_gauge_handle = token_manager.as_ref().map("),
            "main.rs (production region) MUST bind the FAST crash-recovery \
             arm's gauge-poller handle \
             (`let token_health_gauge_handle = token_manager.as_ref().map(`) \
             so it rides into DhanLaneRunHandles via run_shutdown_fast — \
             discarding the handle detaches the fast-arm poller from lane \
             ownership (COV-R3-1, AG5-R1 class)."
        );
        // SEC-R4-1 / R4-CPLX-1/2 (2026-07-06): the last two detached spawns
        // in start_dhan_lane — the MINT-CAPABLE 4h token sweep
        // (force_renewal_if_stale → acquire_token fallback) and the 5-min
        // periodic health check — are lane-owned now. Pin the registrations:
        // a revert to a bare `tokio::spawn` (handle discarded) leaks one
        // immortal copy per D2b lane cold-start cycle; the leaked sweep
        // keeps a deliberately-disabled lane's JWT alive via RenewToken and
        // fires a false RESILIENCE-03 "peer owns the session" Critical page
        // every 4h after a lane stop/restart.
        assert!(
            production.contains("token_sweep_handle: Some("),
            "main.rs (production region) MUST register the 4h token-sweep \
             handle lane-owned (`token_sweep_handle: Some(`) in \
             DhanLaneRunHandles — a detached mint-capable sweep survives \
             the lane teardown and fires false RESILIENCE-03 pages \
             (SEC-R4-1 / R4-CPLX-1)."
        );
        assert!(
            production.contains("periodic_health_handle: Some("),
            "main.rs (production region) MUST register the periodic \
             health-check handle lane-owned \
             (`periodic_health_handle: Some(`) in DhanLaneRunHandles — a \
             detached copy per lane cold-start cycle multiplies alert \
             traffic with independent cooldown state (R4-CPLX-2)."
        );
        // COV-C4-1 (2026-07-07): the R8-EDGE-2 pool-watchdog lane-ownership
        // has TWO production halves and only the SLOW one was ratcheted
        // (test_pre_lane_abort_guard_covers_early_spawn_windows scopes its
        // needles to start_dhan_lane's body). Pin the FAST crash-recovery
        // arm's BINDING + its ride into run_shutdown_fast — a regression
        // that keeps the fast-arm spawn but discards the handle
        // (`let _ = spawn_pool_watchdog_task(...)` + passing `None`)
        // compiles silently (main.rs lacks the lib.rs deny(unused_must_use)
        // blanket, and `let _ =` defeats must_use anyway) and would re-open
        // the leaked-watchdog class on a fast-boot process: an immortal
        // stale writer of the shared /health + feed-health Dhan slots and a
        // possible false CRITICAL WebSocketPoolHalt after a runtime Dhan
        // disable.
        assert!(
            production.contains("let fast_pool_watchdog_handle = spawn_pool_watchdog_task("),
            "main.rs (production region) MUST bind the FAST crash-recovery \
             arm's pool-watchdog handle \
             (`let fast_pool_watchdog_handle = spawn_pool_watchdog_task(`) — \
             discarding it detaches the watchdog from lane ownership \
             (COV-C4-1, R8-EDGE-2 class)."
        );
        assert!(
            production.contains("(handles, Some(pool_arc), Some(fast_pool_watchdog_handle))"),
            "main.rs (production region) MUST thread the FAST arm's \
             pool-watchdog handle out of the pool-spawn block \
             (`Some(fast_pool_watchdog_handle)`) so it rides into \
             run_shutdown_fast → DhanLaneRunHandles (COV-C4-1)."
        );
        // R9-EDGE-1/2 + R10-CPLX-1 + COV-C4-2 (2026-07-07): pin the three
        // remaining formerly-detached start_dhan_lane spawns' lane-ownership
        // registrations — a revert to `let _ =` / anonymous `tokio::spawn`
        // leaks one immortal midnight-rollover loop (+ a potentially
        // immortal 5-min prev_oi QuestDB poller + a forever-parked
        // OrderUpdateAuthenticated listener on its attempt-private Notify)
        // per D2b lane cold-start cycle.
        assert!(
            production.contains("order_update_auth_listener_handle: Some("),
            "main.rs (production region) MUST register the \
             OrderUpdateAuthenticated listener handle lane-owned \
             (`order_update_auth_listener_handle: Some(`) in \
             DhanLaneRunHandles — a detached listener parks FOREVER on the \
             attempt-private auth Notify once the order-update task is \
             aborted (R9-EDGE-1 / R10-CPLX-1)."
        );
        assert!(
            production
                .contains("midnight_rollover_handle: midnight_rollover_guard.into_inner().pop()"),
            "main.rs (production region) MUST defuse the midnight-rollover \
             PreLaneAbortGuard into DhanLaneRunHandles \
             (`midnight_rollover_handle: midnight_rollover_guard.into_inner().pop()`) \
             — a detached rollover loop is IMMORTAL, one leaked copy per \
             lane cycle (R9-EDGE-2 / COV-C4-2)."
        );
        assert!(
            production.contains("prev_oi_refresh_handle: prev_oi_refresh_guard.into_inner().pop()"),
            "main.rs (production region) MUST defuse the prev_oi-refresh \
             PreLaneAbortGuard into DhanLaneRunHandles \
             (`prev_oi_refresh_handle: prev_oi_refresh_guard.into_inner().pop()`) \
             — a detached poller may never self-exit while candles_1d stays \
             empty (R9-EDGE-2 / COV-C4-2)."
        );
        // Non-vacuity self-check: `main_rs_production_region` already
        // asserted the split boundary is the #[cfg(test)]-gated `mod tests`
        // seam (R7-CPLX-1 idiom hardening).
        assert!(
            !production.contains("\nmod tests {"),
            "main.rs production region must have its #[cfg(test)] mod tests \
             block excised (blanked) — the split found no test module \
             (vacuous whole-file scan)"
        );
    }

    /// COV-R4-1 (2026-07-06): source-order ratchet for the AG5-R3-1
    /// early-construction fix — `let lane = DhanLaneRunHandles {` MUST be
    /// constructed inside `start_dhan_lane` BEFORE the first post-spawn
    /// top-level `.await` (`emit_boot_completed_when_feed_live`, which can
    /// park for the full liveness wait and is `.abort()`ed by the D2b
    /// supervisor on a disable-during-Starting). The wiring ratchet above
    /// asserts only position-independent `contains` needles, so a refactor
    /// moving the construction back BELOW that await (its pre-fix position)
    /// would stay green there while fully re-opening the round-3 leak: a
    /// lane cancelled mid-Starting detaches the remint-capable watchdog +
    /// gauge poller + token sweep before they reach the H8 Drop floor.
    /// House pattern: the byte-offset source-order scan of
    /// `ratchet_tick_processor_spawns_before_reinject_await`
    /// (crates/app/src/wal_reinject.rs).
    #[test]
    fn test_lane_handles_constructed_before_boot_completed_await() {
        let main_rs = std::fs::read_to_string("../app/src/main.rs")
            .or_else(|_| std::fs::read_to_string("crates/app/src/main.rs"))
            .expect("main.rs must be readable");
        // R7-CPLX-1 idiom hardening (2026-07-07): test-MODULE split, not
        // first-#[cfg(test)]-occurrence split (see main_rs_production_region).
        let production = main_rs_production_region(&main_rs);
        let start_lane_off = production
            .find("async fn start_dhan_lane(")
            .expect("start_dhan_lane must exist in main.rs production region");
        // COV-C2-1 fix (2026-07-07): CODE-SHAPED needle — the leading newline
        // + exact 4-space statement indentation cannot be satisfied by a
        // `// …` comment mention of the construction. The old bare
        // `find("let lane = DhanLaneRunHandles {")` anchored on the
        // R7-EDGE-1 doc COMMENT at the processor-guard site (which quotes
        // that text verbatim ~1,200 lines BEFORE the real construction),
        // making both assertions position-independent/vacuous: renaming the
        // binding or moving the construction back BELOW the
        // emit_boot_completed await — the exact regression this test exists
        // to catch — stayed green. Uniqueness is asserted so a second match
        // can never silently re-introduce the ambiguity. (Sibling
        // discipline: test_pre_lane_abort_guard_covers_early_spawn_windows
        // already uses code-shaped needles for the same reason.)
        let lane_needle = "\n    let lane = DhanLaneRunHandles {";
        let mut lane_matches = production.match_indices(lane_needle);
        let lane_off = lane_matches
            .next()
            .expect(
                "start_dhan_lane must construct `let lane = DhanLaneRunHandles {` \
                 as a top-level statement (the AG5-R3-1 early construction)",
            )
            .0;
        assert!(
            lane_matches.next().is_none(),
            "the `let lane = DhanLaneRunHandles {{` construction statement must \
             be UNIQUE in the production region — a second occurrence makes \
             this source-order anchor ambiguous (COV-C2-1)"
        );
        assert!(
            lane_off > start_lane_off,
            "the `let lane = DhanLaneRunHandles` early construction must live \
             INSIDE start_dhan_lane (found at byte {lane_off}, before the fn \
             at byte {start_lane_off})"
        );
        // The construction must PRECEDE the emit_boot_completed await: an
        // `emit_boot_completed_when_feed_live(` call must still occur AFTER
        // it in source order. If the construction is moved back below that
        // await (the pre-AG5-R3-1 position at the end of start_dhan_lane),
        // no later call exists in the production region and this fails.
        assert!(
            production[lane_off..].contains("emit_boot_completed_when_feed_live("),
            "AG5-R3-1 ordering regression: `let lane = DhanLaneRunHandles` \
             must be constructed BEFORE start_dhan_lane's \
             `emit_boot_completed_when_feed_live(...).await` — a cancelled \
             cold-start would otherwise DETACH the remint-capable watchdog + \
             gauge poller + token sweep (COV-R4-1)"
        );
        assert!(
            !production.contains("\nmod tests {"),
            "main.rs production region must have its #[cfg(test)] mod tests \
             block excised (blanked) — the split found no test module \
             (vacuous whole-file scan)"
        );
    }

    /// R7-CPLX-1 idiom hardening (2026-07-07): shared production-region
    /// splitter for the main.rs source scans above. The old
    /// `.split("#[cfg(test)]").next()` idiom truncated the scanned region at
    /// the FIRST raw occurrence of that literal — which in token_manager.rs
    /// was a doc-comment mention + item-level attributes on test-only
    /// constructors ~100 lines BEFORE the real test module, silently
    /// excluding genuine production code from the scan (a false-OK direction
    /// hole for exactly-once assertions). Splitting at the top-level
    /// `mod tests` block is robust to inline #[cfg(test)] items appended to
    /// the production body; the preamble check asserts the block we split at
    /// really is the #[cfg(test)]-gated test module.
    /// F8 hardening (2026-07-08): delegates to the SHARED
    /// `tickvault_common::source_scan::production_region` — which (a) splits
    /// at the #[cfg(test)]-gated `mod tests` MODULE via CODE-classified brace
    /// matching (a string/comment mention of the seam cannot derail it), and
    /// (b) KEEPS the ~460 lines of production code AFTER main.rs's test
    /// module (`spawn_post_market_tasks`, `spawn_daily_tick_conservation_task`,
    /// …) that the old prefix-only split was blind to. The excised test
    /// module is blanked with spaces (newlines kept) so byte-order
    /// assertions against the region stay meaningful.
    fn main_rs_production_region(main_rs: &str) -> String {
        tickvault_common::source_scan::production_region(main_rs).expect(
            "main.rs must contain a #[cfg(test)]-gated top-level `mod tests` \
             module — without it these production-region splits would scan \
             the whole file (vacuous)",
        )
    }

    /// COV-R7-1 / R7-EDGE-1 (2026-07-07): source-order ratchet for the
    /// pre-`DhanLaneRunHandles` abort-on-drop guards inside
    /// `start_dhan_lane`. The COV-R4-1 ordering ratchet above pins only
    /// construction-before-`emit_boot_completed_when_feed_live`; it cannot
    /// see the EARLIER detach windows — the tick processor is spawned before
    /// the slow-boot STAGE-C.2b WAL re-injection await (unbounded total by
    /// design on a WAL backlog) and the main-feed WS pool handles are bound
    /// before the ≤30s `emit_websocket_connected_alerts` health-poll await.
    /// A D2b supervisor `.abort()` parked on either await previously dropped
    /// the BARE handles, DETACHING the pool + processor: on the next runtime
    /// re-enable the detached dormant pool reconnected alongside the fresh
    /// lane's pool (2 live Dhan main-feed connections — a
    /// websocket-connection-scope-lock violation) and the detached processor
    /// persisted the duplicate stream (capture_seq is IN the `ticks` DEDUP
    /// key, so duplicate rows are NOT collapsed). This ratchet pins:
    /// (a) the processor guard wrap BEFORE the slow-boot
    ///     `reinject_wal_frames(` await,
    /// (b) the WS-handles guard wrap BETWEEN `spawn_websocket_connections(`
    ///     and `emit_websocket_connected_alerts(`,
    /// (c) both guards DEFUSED into the lane struct's H8 floor, and
    /// (d) the guard type's abort-on-Drop impl.
    #[test]
    fn test_pre_lane_abort_guard_covers_early_spawn_windows() {
        let main_rs = std::fs::read_to_string("../app/src/main.rs")
            .or_else(|_| std::fs::read_to_string("crates/app/src/main.rs"))
            .expect("main.rs must be readable");
        let production = main_rs_production_region(&main_rs);
        // Scope every byte-offset check to start_dhan_lane's body (the fast
        // crash-recovery boot arm in main() has its own reinject/emit sites
        // that are NOT supervisor-abortable).
        let start_lane_off = production
            .find("async fn start_dhan_lane(")
            .expect("start_dhan_lane must exist in main.rs production region");
        let lane_body = &production[start_lane_off..];
        // (a) processor guard wrap precedes the slow-boot reinject await.
        let processor_guard_off = lane_body
            .find("let processor_guard = PreLaneAbortGuard::new(processor_handle")
            .expect(
                "start_dhan_lane must wrap the tick-processor handle in \
                 PreLaneAbortGuard (R7-EDGE-1 / COV-R7-1) — a bare handle \
                 across the WAL-reinject await detaches the processor on a \
                 cancel-mid-Starting",
            );
        let reinject_off = lane_body
            .find("reinject_wal_frames(")
            .expect("start_dhan_lane must contain the slow-boot WAL reinject await");
        assert!(
            processor_guard_off < reinject_off,
            "the PreLaneAbortGuard wrap of processor_handle (byte \
             {processor_guard_off}) must PRECEDE the slow-boot \
             reinject_wal_frames await (byte {reinject_off}) in \
             start_dhan_lane — otherwise the detach window re-opens \
             (COV-R7-1)"
        );
        // (b) WS-handles guard wrap sits between the pool spawn and the
        //     connected-alerts health-poll await. Code-shaped needles
        //     (`let handles = …` / the call-plus-first-arg text) are used so
        //     COMMENT mentions of the fn names can never satisfy the scan.
        let ws_spawn_off = lane_body
            .find("let handles = spawn_websocket_connections(")
            .expect("start_dhan_lane must bind the main-feed WS pool handles");
        let ws_guard_off = lane_body
            .find("let handles_guard = PreLaneAbortGuard::new(handles);")
            .expect(
                "start_dhan_lane must wrap the just-spawned WS pool handles \
                 in PreLaneAbortGuard (R7-EDGE-1) — a bare Vec<JoinHandle> \
                 across the emit_websocket_connected_alerts await detaches \
                 the pool on a cancel-mid-Starting",
            );
        assert!(
            ws_spawn_off < ws_guard_off,
            "the PreLaneAbortGuard wrap of the WS pool handles (byte \
             {ws_guard_off}) must FOLLOW the spawn_websocket_connections \
             binding (byte {ws_spawn_off}) in start_dhan_lane (R7-EDGE-1)"
        );
        // The real emit call (not a comment mention) must occur AFTER the
        // guard wrap — i.e. the ≤30s health-poll await is covered.
        assert!(
            lane_body[ws_guard_off..].contains("emit_websocket_connected_alerts(\n"),
            "start_dhan_lane must call emit_websocket_connected_alerts AFTER \
             the PreLaneAbortGuard wrap of the WS pool handles — the ≤30s \
             health-poll await must sit inside the guard's coverage window \
             (R7-EDGE-1)"
        );
        // (b2) R8-EDGE-2 / SEC-C2-1 (2026-07-07): the pool WATCHDOG is
        //      spawned in the same pre-construction window (before the ≤30s
        //      connected-alerts await) and its only in-loop exit is a
        //      `shutdown_notify` nothing can fire before the lane struct
        //      exists — so it must be bound + guard-wrapped too. A leaked
        //      watchdog writes the PROCESS-SHARED /health + feed-health Dhan
        //      slots every 5s from the DEAD pool and fires false
        //      WebSocketPoolDegraded / CRITICAL WebSocketPoolHalt pages
        //      after a runtime re-enable.
        let watchdog_spawn_off = lane_body
            .find("let dhan_pool_watchdog_handle = spawn_pool_watchdog_task(")
            .expect(
                "start_dhan_lane must BIND the pool-watchdog JoinHandle \
                 (`let dhan_pool_watchdog_handle = spawn_pool_watchdog_task(`) \
                 — a fire-and-forget spawn leaks the watchdog on a \
                 cancel-mid-Starting (R8-EDGE-2 / SEC-C2-1)",
            );
        let watchdog_guard_off = lane_body
            .find("let pool_watchdog_guard = PreLaneAbortGuard::new(vec![dhan_pool_watchdog_handle]);")
            .expect(
                "start_dhan_lane must wrap the pool-watchdog handle in \
                 PreLaneAbortGuard (R8-EDGE-2 / SEC-C2-1) — a bare handle \
                 across the emit_websocket_connected_alerts await detaches \
                 the watchdog on a cancel-mid-Starting",
            );
        assert!(
            watchdog_spawn_off < watchdog_guard_off,
            "the PreLaneAbortGuard wrap of the pool-watchdog handle (byte \
             {watchdog_guard_off}) must FOLLOW its spawn binding (byte \
             {watchdog_spawn_off}) in start_dhan_lane (R8-EDGE-2)"
        );
        assert!(
            lane_body[watchdog_guard_off..].contains("emit_websocket_connected_alerts(\n"),
            "start_dhan_lane must call emit_websocket_connected_alerts AFTER \
             the PreLaneAbortGuard wrap of the pool-watchdog handle — the \
             ≤30s health-poll await must sit inside the guard's coverage \
             window (R8-EDGE-2 / SEC-C2-1)"
        );
        // (c) all three guards are defused into the lane struct's H8 floor.
        assert!(
            lane_body.contains("ws_handles: ws_handles_guard.into_inner()"),
            "the lane construction must defuse the WS guard into \
             DhanLaneRunHandles.ws_handles (R7-EDGE-1)"
        );
        assert!(
            lane_body.contains("processor_handle: processor_guard.into_inner().pop()"),
            "the lane construction must defuse the processor guard into \
             DhanLaneRunHandles.processor_handle (COV-R7-1)"
        );
        assert!(
            lane_body.contains("pool_watchdog_handle: pool_watchdog_guard.into_inner().pop()"),
            "the lane construction must defuse the pool-watchdog guard into \
             DhanLaneRunHandles.pool_watchdog_handle (R8-EDGE-2 / SEC-C2-1)"
        );
        // (d) the guard type aborts on Drop (not merely drops the handles).
        let guard_impl_off = production
            .find("impl<T> Drop for PreLaneAbortGuard<T>")
            .expect("PreLaneAbortGuard must implement Drop (abort-on-drop floor)");
        assert!(
            production[guard_impl_off..]
                .lines()
                .take(8)
                .any(|line| line.contains("handle.abort()")),
            "PreLaneAbortGuard's Drop impl must abort() the wrapped handles — \
             dropping them bare would detach the tasks (the exact leak the \
             guard exists to close)"
        );
        // (e) lane-teardown floor for the watchdog: teardown + the H8 Drop
        //     must abort() it — the notify_waiters() stop signal alone is
        //     Rule-16 lost-wake prone (the watchdog's tick body awaits a ≤2s
        //     QuestDB probe).
        assert!(
            production.contains("if let Some(handle) = lane.pool_watchdog_handle.take() {"),
            "teardown_dhan_lane_tasks must take + abort the pool-watchdog \
             handle directly (R8-EDGE-2 / SEC-C2-1 — notify alone is Rule-16 \
             lost-wake prone)"
        );
        assert!(
            production.contains("if let Some(handle) = self.pool_watchdog_handle.as_ref() {"),
            "DhanLaneRunHandles::drop must abort the pool-watchdog handle \
             (R8-EDGE-2 / SEC-C2-1)"
        );
        // (f) SEC-C3-1 / R9-CPLX-1 / COV-R8-2 (2026-07-07): the Item 19f
        //     heartbeat-shutdown BRIDGE task parks on the PER-ATTEMPT
        //     shutdown_notify, whose only notify_waiters() lives in
        //     teardown — which never runs when the lane struct is never
        //     constructed. A detached bridge therefore leaks one immortal
        //     parked task + two Arc<Notify> per cancelled/failed cold-start
        //     attempt. It must be guard-wrapped at the spawn site, defused
        //     into the lane struct, and aborted by teardown + the H8 Drop
        //     (abort is safe: teardown/Drop deliver its only pending
        //     action, the heartbeat notify_one, directly).
        let bridge_guard_off = lane_body
            .find("PreLaneAbortGuard::new(heartbeat_bridge_handle.into_iter().collect::<Vec<_>>())")
            .expect(
                "start_dhan_lane must wrap the Item 19f heartbeat-shutdown \
                 bridge JoinHandle in PreLaneAbortGuard (SEC-C3-1 / \
                 R9-CPLX-1 / COV-R8-2) — a bare tokio::spawn detaches the \
                 bridge, which parks forever on the attempt-private \
                 shutdown_notify after a cancel-mid-Starting",
            );
        assert!(
            lane_body[bridge_guard_off..].contains("emit_websocket_connected_alerts(\n"),
            "the Item 19f bridge guard wrap must PRECEDE the \
             emit_websocket_connected_alerts await — the bridge is spawned \
             in the pre-lane window and must be covered across every \
             subsequent supervisor-abortable await (SEC-C3-1)"
        );
        assert!(
            lane_body
                .contains("heartbeat_bridge_handle: heartbeat_bridge_guard.into_inner().pop()"),
            "the lane construction must defuse the Item 19f bridge guard \
             into DhanLaneRunHandles.heartbeat_bridge_handle (SEC-C3-1 / \
             COV-R8-2)"
        );
        assert!(
            production.contains("if let Some(handle) = lane.heartbeat_bridge_handle.take() {"),
            "teardown_dhan_lane_tasks must take + abort the Item 19f bridge \
             handle — a survivor parks forever on the dead lane's \
             attempt-private shutdown_notify (SEC-C3-1 / R9-CPLX-1)"
        );
        assert!(
            production.contains("if let Some(handle) = self.heartbeat_bridge_handle.as_ref() {"),
            "DhanLaneRunHandles::drop must abort the Item 19f bridge handle \
             (SEC-C3-1 / COV-R8-2)"
        );
    }

    /// R8-EDGE-1 / R8-CPLX-1 (2026-07-07): the dual-instance SSM lock
    /// heartbeat is spawned at Step 6a-prime — ~2,500 lines before the lane
    /// struct that owns it — and its only exits are its own shutdown Notify
    /// or a foreign takeover that never happens (the next acquire REFUSES
    /// instead of overwriting). On the D2b runtime path, a supervisor
    /// `.abort()` on ANY intermediate await (Step 6 auth, IP gate, QuestDB
    /// DDL, the §4 infinite-retry universe fetch, WAL reinject, WS spawn) —
    /// or any ordinary `return Err(StartLaneError::…)` after the spawn —
    /// previously DETACHED the still-renewing heartbeat: the zombie renewed
    /// the SSM lock every 30s forever, the 90s TTL never cleared it, and
    /// every retry / re-enable failed AlreadyHeld → a RESILIENCE-01 Critical
    /// page per bounded-backoff retry + a PERMANENTLY wedged Dhan lane until
    /// full process restart. This ratchet pins the notify-on-drop guard:
    /// (a) the heartbeat rides in `InstanceLockHeartbeatGuard` (code-shaped
    ///     binding needle),
    /// (b) the guard's Drop fires `notify_one()` (graceful SSM release) and
    ///     NEVER `abort()`s (BUG-1: an abort would kill the in-flight
    ///     DeleteParameter),
    /// (c) the guard is defused into the lane struct's BUG-1 fields via
    ///     `into_parts()` at the construction site.
    #[test]
    fn test_instance_lock_heartbeat_guard_covers_pre_lane_window() {
        let main_rs = std::fs::read_to_string("../app/src/main.rs")
            .or_else(|_| std::fs::read_to_string("crates/app/src/main.rs"))
            .expect("main.rs must be readable");
        let production = main_rs_production_region(&main_rs);
        let start_lane_off = production
            .find("async fn start_dhan_lane(")
            .expect("start_dhan_lane must exist in main.rs production region");
        let lane_body = &production[start_lane_off..];
        // (a) the guard binding wraps the heartbeat spawn (code-shaped
        //     needles — comments cannot satisfy them).
        let guard_binding_off = lane_body
            .find("let instance_lock_guard: InstanceLockHeartbeatGuard = {")
            .expect(
                "start_dhan_lane must hold the instance-lock heartbeat in \
                 InstanceLockHeartbeatGuard (R8-EDGE-1 / R8-CPLX-1) — bare \
                 locals detach the still-renewing heartbeat on a \
                 cancel-mid-Starting or an Err return, permanently wedging \
                 the lane with AlreadyHeld/RESILIENCE-01",
            );
        let guard_ctor_off = lane_body
            .find("InstanceLockHeartbeatGuard::new(heartbeat_handle, heartbeat_shutdown_for_chain)")
            .expect(
                "the Step 6a-prime block must construct the guard from the \
                 spawned heartbeat handle + its shutdown Notify (R8-EDGE-1)",
            );
        assert!(
            guard_binding_off < guard_ctor_off,
            "the guard binding (byte {guard_binding_off}) must precede its \
             constructor (byte {guard_ctor_off}) — i.e. the guard is created \
             AT the spawn site, before any supervisor-abortable await"
        );
        // (c) defused into the lane struct BEFORE the construction statement
        //     (source order: into_parts precedes the code-shaped lane needle).
        let into_parts_off = lane_body
            .find(
                "let (instance_lock_handle, instance_lock_shutdown_chain) = \
                 instance_lock_guard.into_parts();",
            )
            .expect(
                "the lane construction site must defuse the heartbeat guard \
                 via into_parts() so the BUG-1 teardown bound-await still \
                 owns the release (R8-EDGE-1)",
            );
        let lane_ctor_off = lane_body
            .find("\n    let lane = DhanLaneRunHandles {")
            .expect("the AG5-R3-1 early lane construction must exist");
        assert!(
            into_parts_off < lane_ctor_off,
            "into_parts() (byte {into_parts_off}) must precede the lane \
             construction (byte {lane_ctor_off}) so the heartbeat fields ride \
             into DhanLaneRunHandles"
        );
        // (b) the guard's Drop notify-releases and never aborts.
        let drop_impl_off = production
            .find("impl Drop for InstanceLockHeartbeatGuard")
            .expect("InstanceLockHeartbeatGuard must implement Drop (notify-on-drop floor)");
        let drop_impl_end = production[drop_impl_off..]
            .find("\n}\n")
            .map(|o| drop_impl_off + o)
            .expect("InstanceLockHeartbeatGuard Drop impl must have a body end");
        let drop_body = &production[drop_impl_off..drop_impl_end];
        assert!(
            drop_body.contains("shutdown.notify_one()"),
            "InstanceLockHeartbeatGuard::drop must notify_one() the \
             heartbeat's shutdown (permit-storing graceful SSM release) — \
             without it a cancelled/failed cold-start leaves a zombie \
             renewer that wedges every re-acquire (R8-EDGE-1 / R8-CPLX-1)"
        );
        assert!(
            !drop_body.contains(".abort()"),
            "InstanceLockHeartbeatGuard::drop must NEVER abort() the \
             heartbeat handle — an abort kills the in-flight SSM \
             DeleteParameter release (the BUG-1 contract)"
        );
    }

    /// Whitespace-collapsed view for formatting-proof code-needle counts.
    fn no_ws(s: &str) -> String {
        s.chars().filter(|c| !c.is_whitespace()).collect()
    }

    /// F4 (2026-07-08): the three market-open ONE-SHOT schedulers (09:14
    /// readiness / 09:15:30 streaming heartbeat / 09:16 self-test) MUST be
    /// (a) once-per-process latched (a per-lane-cycle spawn accumulates N
    /// pending copies across D2b enable/disable cycles → N duplicate
    /// verdicts per trigger) and (b) fire-time gated on the live Dhan flag
    /// (a pending one-shot outliving a deliberate runtime disable fires
    /// FALSE High MarketOpenStreamingFailed / Critical SELFTEST-02 pages
    /// off the dead pool's frozen state). Also pins the FirstSeenSet
    /// midnight-reset once-per-process latch (F13 — an infinite loop over a
    /// process-global singleton).
    ///
    /// COMMENT-STRIPPED (F9 class): a comment mention of any needle can
    /// never satisfy these counts — the previous latch/gate fixes were
    /// reverted-silently-green precisely because nothing ratcheted them.
    #[test]
    fn test_market_open_one_shots_latched_and_gated() {
        let main_rs = std::fs::read_to_string("../app/src/main.rs")
            .or_else(|_| std::fs::read_to_string("crates/app/src/main.rs"))
            .expect("main.rs must be readable");
        let production_raw = main_rs_production_region(&main_rs);
        let production = tickvault_common::source_scan::strip_rust_comments(&production_raw);
        let flat = no_ws(&production);
        // (a) exactly ONE latch swap site for the market-open one-shots…
        assert_eq!(
            flat.matches("MARKET_OPEN_ONE_SHOTS_SPAWNED.swap(true")
                .count(),
            1,
            "main.rs (production region, comment-stripped) must consult the \
             MARKET_OPEN_ONE_SHOTS_SPAWNED once-per-process latch exactly \
             once — removing it re-opens the N-duplicate-verdicts leak (F4)"
        );
        // …and one for the FirstSeenSet midnight reset.
        assert_eq!(
            flat.matches("FIRST_SEEN_RESET_SPAWNED.swap(true").count(),
            1,
            "main.rs (production region, comment-stripped) must consult the \
             FIRST_SEEN_RESET_SPAWNED once-per-process latch exactly once — \
             removing it leaks one immortal midnight-reset loop per lane \
             cycle (F4/F13)"
        );
        // (b) all THREE one-shot spawn blocks are latch-guarded (the negated
        // `if !…` info-log arm does not match this needle).
        assert!(
            flat.matches("ifmarket_open_one_shots_first_spawn").count() >= 3,
            "all three market-open one-shot spawn blocks must be guarded by \
             `if market_open_one_shots_first_spawn` (F4)"
        );
        // (c) all THREE tasks re-check the Dhan flag AT FIRE TIME through
        // the shared pure gate (call-shaped needle — the fn definition has
        // a different shape).
        assert_eq!(
            flat.matches("!market_open_fire_gate_dhan_enabled(&")
                .count(),
            3,
            "each of the three market-open one-shots must consult \
             market_open_fire_gate_dhan_enabled(&…) after its sleep — a \
             pending one-shot outliving a runtime Dhan disable must not \
             fire a verdict off dead state (F4)"
        );
        // (d) the gate actually reads the atomic (not a hardcoded true).
        let gate_off = production
            .find("fn market_open_fire_gate_dhan_enabled(")
            .expect("the fire-time gate fn must exist in main.rs");
        assert!(
            production[gate_off..gate_off + 400].contains("dhan_enabled.load("),
            "market_open_fire_gate_dhan_enabled must load the shared Dhan \
             atomic — a constant-true stub silently disables the gate (F4)"
        );
    }

    /// F5 / F13 / F14 (2026-07-08): the runtime IP monitor + the 30s
    /// heartbeat watchdog are LANE-OWNED (guard-wrapped across the
    /// pre-lane awaits, defused into DhanLaneRunHandles, taken + stopped by
    /// teardown, aborted by the H8 Drop floor), and the teardown publishes
    /// the honest lane-off /health + feed-health surfaces after bounded-
    /// joining the pool watchdog (their sole writer). COMMENT-STRIPPED so
    /// comment mentions can never satisfy the scan.
    #[test]
    fn test_ip_monitor_and_heartbeat_watchdog_are_lane_owned() {
        let main_rs = std::fs::read_to_string("../app/src/main.rs")
            .or_else(|_| std::fs::read_to_string("crates/app/src/main.rs"))
            .expect("main.rs must be readable");
        let production_raw = main_rs_production_region(&main_rs);
        let production = tickvault_common::source_scan::strip_rust_comments(&production_raw);
        let flat = no_ws(&production);
        // The zombie-maker must never come back: the watch Sender is HELD
        // (lane-owned), never mem::forget-pinned for the process lifetime.
        assert!(
            !flat.contains("mem::forget(ip_monitor_shutdown_tx)"),
            "main.rs must NOT mem::forget the IP monitor's shutdown Sender — \
             that pinned a zombie monitor (stale IP baseline → false CRITICAL \
             GAP-NET-01 / live-mode HALT) per lane cycle (F5)"
        );
        // Guard wraps at the spawn sites…
        assert!(
            flat.contains("PreLaneAbortGuard::new(vec![ip_monitor_handle])"),
            "the IP monitor handle must ride a PreLaneAbortGuard across the \
             pre-lane awaits (F5)"
        );
        assert!(
            flat.contains("PreLaneAbortGuard::new(vec![spawn_heartbeat_watchdog("),
            "the slow-boot heartbeat watchdog must ride a PreLaneAbortGuard \
             — a dropped bare handle detaches an immortal 30s loop firing \
             false AUTH-GAP-01 pages off the dead lane's token_handle (F13)"
        );
        // …defused into the lane struct…
        assert!(
            flat.contains("ip_monitor_handle:ip_monitor_guard.into_inner().pop()"),
            "the lane construction must defuse the IP-monitor guard into \
             DhanLaneRunHandles.ip_monitor_handle (F5)"
        );
        assert!(
            flat.contains("heartbeat_watchdog_handle:heartbeat_watchdog_guard.into_inner().pop()"),
            "the lane construction must defuse the heartbeat-watchdog guard \
             into DhanLaneRunHandles.heartbeat_watchdog_handle (F13)"
        );
        // …stopped by teardown (Sender dropped + handles aborted)…
        for needle in [
            "lane.ip_monitor_shutdown.take()",
            "lane.ip_monitor_handle.take()",
            "lane.heartbeat_watchdog_handle.take()",
        ] {
            assert!(
                flat.contains(needle),
                "teardown_dhan_lane_tasks must take + stop `{needle}` (F5/F13)"
            );
        }
        // …and floored by the H8 Drop impl.
        for needle in [
            "self.ip_monitor_handle.as_ref()",
            "self.heartbeat_watchdog_handle.as_ref()",
        ] {
            assert!(
                flat.contains(needle),
                "DhanLaneRunHandles::drop must abort `{needle}` (F5/F13)"
            );
        }
        // F14: after bounded-joining the pool watchdog, teardown publishes
        // the honest lane-off surfaces its death would otherwise freeze.
        assert!(
            flat.contains("health.set_websocket_connections(0)"),
            "teardown must reset /health websocket_connections to 0 after \
             aborting its sole writer (the pool watchdog) — otherwise the \
             count freezes at the last live value after a deliberate \
             runtime disable (F14)"
        );
        assert!(
            flat.contains("feed_health.set_connected(tickvault_common::feed::Feed::Dhan,false)"),
            "teardown must reset the feed-health Dhan-connected slot to \
             false after aborting its sole writer (F14)"
        );
    }
}

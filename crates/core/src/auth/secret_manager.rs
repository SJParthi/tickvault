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
        let bridges_chain = main_rs.contains("instance_lock_shutdown_chain.clone()");
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
                "main.rs declares `instance_lock_shutdown_chain` but does not \
                 `.clone()` it into the bridge task at the point `shutdown_notify` \
                 is constructed; the heartbeat would never observe the broader \
                 shutdown signal."
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
}

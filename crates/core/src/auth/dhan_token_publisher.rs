//! Publishes the minted Dhan access token to SSM so peer consumers READ it
//! instead of minting their own.
//!
//! # Why this module exists
//!
//! Dhan permits **exactly one active access token per account**
//! (`docs/dhan-ref/02-authentication.md:216` — *"One active token at a time —
//! generating new token invalidates the old one"*). tickvault mints a Dhan
//! token via TOTP and, before this module, never published it — it lived only
//! in process memory (`arc-swap`) plus the local crash cache
//! (`token_cache.rs` → `/tmp/tv-token-cache`, 0600, container-local).
//!
//! Any second consumer of the same Dhan account therefore had to mint its own
//! token, which instantly invalidated tickvault's. tickvault's mid-session
//! watchdog then re-minted, invalidating the peer's — a flapping re-mint war
//! in which *last-to-mint wins* and both sides intermittently 401.
//!
//! Groww already solved exactly this shape: ONE minter publishes to SSM and
//! everyone else reads (`.claude/rules/project/groww-shared-token-minter-2026-07-02.md`).
//! This module gives Dhan the same contract with the roles reversed —
//! **tickvault is the sole Dhan minter and publisher**; peers read.
//!
//! | Broker | Minter | Publishes to SSM | tickvault's role |
//! |---|---|---|---|
//! | Groww  | bruteX Lambda | yes | reader (`secret_manager::fetch_groww_access_token`) |
//! | Dhan   | **tickvault** | **yes — this module** | minter + publisher |
//!
//! Operator directive 2026-08-08, recorded in that rule file's §9.
//!
//! # Why a separate module (not `secret_manager.rs`)
//!
//! `crates/common/tests/groww_no_mint_guard.rs:129` is a build-failing ratchet
//! asserting `secret_manager.rs` contains no `put_parameter` — *"must remain
//! read-only — no SSM put_parameter, ever."* That pin is correct and stays
//! green; the single SSM WRITE in the auth tree lives here instead.
//!
//! # Fail-soft contract (non-negotiable)
//!
//! The publish is strictly best-effort and off the critical path. tickvault
//! already holds the token in memory, so a publish failure must never degrade
//! tickvault's own auth. Concretely this module:
//!
//! - is **spawned, never awaited** by the mint path (a slow/hanging SSM call
//!   can never delay a token mint or stall the Step-6 boot deadline);
//! - never panics (no `unwrap`/`expect`; every error is matched);
//! - never retries in a loop — the next mint/renewal simply republishes;
//! - **never logs the token value**, only the parameter path.
//!
//! Ratchet: `crates/core/tests/dhan_token_publish_guard.rs`.

use aws_sdk_ssm::types::ParameterType;
use secrecy::ExposeSecret;
use tickvault_common::constants::{DHAN_ACCESS_TOKEN_SECRET, SSM_DHAN_SERVICE};
use tracing::{debug, error};

use super::secret_manager::{build_ssm_path, create_ssm_client_public, resolve_environment};
use super::types::TokenState;

/// Spawns a best-effort publish of the current Dhan access token to SSM.
///
/// Returns immediately — the SSM round-trip happens on a detached task so the
/// mint path is never blocked. Call this directly after a successful mint or
/// renewal, alongside the existing crash-cache save.
///
/// A failure is logged and counted; it is deliberately NOT propagated, because
/// tickvault's own auth does not depend on the published copy.
pub fn spawn_publish_dhan_token(token_state: &TokenState) {
    // Copy the value out synchronously so the spawned task owns it and we never
    // hold a lock/guard across an await point.
    let token = token_state.access_token().expose_secret().to_owned();

    tokio::spawn(async move {
        publish_dhan_token(token).await;
    });
}

/// Performs the SSM `PutParameter` write. Never panics; never propagates.
///
/// Separated from the spawn wrapper so it is directly unit-testable.
async fn publish_dhan_token(token: String) {
    let environment = match resolve_environment() {
        Ok(env) => env,
        Err(e) => {
            // No token value in the log — only the failure class.
            error!(
                error = %e,
                "Dhan token publish skipped: could not resolve environment. \
                 tickvault's own token is unaffected (in-memory); peers may \
                 continue to see a stale published token until the next mint."
            );
            metrics::counter!("tv_dhan_token_publish_total", "outcome" => "error").increment(1);
            return;
        }
    };

    let path = build_ssm_path(&environment, SSM_DHAN_SERVICE, DHAN_ACCESS_TOKEN_SECRET);
    let client = create_ssm_client_public().await;

    let result = client
        .put_parameter()
        .name(&path)
        .value(token)
        .r#type(ParameterType::SecureString)
        .overwrite(true)
        .send()
        .await;

    match result {
        Ok(_) => {
            // Path only — never the value.
            debug!(
                param = %path,
                "Dhan access token published to SSM for peer consumers"
            );
            metrics::counter!("tv_dhan_token_publish_total", "outcome" => "ok").increment(1);
        }
        Err(e) => {
            error!(
                param = %path,
                error = %e,
                "Dhan token publish FAILED (fail-soft). tickvault's own token is \
                 unaffected (held in memory); a peer reading this parameter may \
                 use a stale token until the next mint republishes. Check the \
                 instance role has ssm:PutParameter + kms:Encrypt on this parameter."
            );
            metrics::counter!("tv_dhan_token_publish_total", "outcome" => "error").increment(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::types::DhanAuthResponseData;

    fn sample_token_state() -> TokenState {
        TokenState::from_response(&DhanAuthResponseData {
            access_token: "test-jwt-value".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86_400,
        })
    }

    /// The publish must RETURN IMMEDIATELY — it is spawned, never awaited.
    ///
    /// This is the contract that keeps a hanging SSM call from delaying a token
    /// mint or stalling the Step-6 boot deadline. The spawned task will fail
    /// (no AWS credentials in a test environment) and that is the point: the
    /// failure is absorbed on the detached task, never surfaced to the caller.
    #[tokio::test]
    async fn test_spawn_publish_dhan_token_returns_immediately_and_never_panics() {
        let state = sample_token_state();

        let started = std::time::Instant::now();
        spawn_publish_dhan_token(&state);
        let elapsed = started.elapsed();

        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "spawn_publish_dhan_token must return immediately (spawned, never \
             awaited) — took {elapsed:?}. Blocking here would delay every token \
             mint and can stall boot."
        );
    }

    /// Calling it repeatedly must stay panic-free — renewals republish on every
    /// cycle, so this runs for the life of the process.
    #[tokio::test]
    async fn test_spawn_publish_dhan_token_is_safe_to_call_repeatedly() {
        let state = sample_token_state();
        for _ in 0..3 {
            spawn_publish_dhan_token(&state);
        }
    }

    /// The published parameter path must be the one terraform already reserved
    /// (`dhan_access_token_ssm_param` → `/tickvault/<env>/dhan/access-token`).
    #[test]
    fn test_publish_path_matches_reserved_terraform_param() {
        let path = build_ssm_path("prod", SSM_DHAN_SERVICE, DHAN_ACCESS_TOKEN_SECRET);
        assert_eq!(
            path, "/tickvault/prod/dhan/access-token",
            "publish path must match the reserved terraform variable default"
        );
    }

    /// Environment scoping must hold — a dev boot must never overwrite the prod
    /// token (that would hand peers a sandbox token and 401 the real feed).
    #[test]
    fn test_publish_path_is_environment_scoped() {
        let dev = build_ssm_path("dev", SSM_DHAN_SERVICE, DHAN_ACCESS_TOKEN_SECRET);
        let prod = build_ssm_path("prod", SSM_DHAN_SERVICE, DHAN_ACCESS_TOKEN_SECRET);
        assert_ne!(dev, prod, "dev must never write the prod parameter");
        assert!(dev.starts_with("/tickvault/dev/"));
        assert!(prod.starts_with("/tickvault/prod/"));
    }

    /// The Dhan publish must be scoped by its SERVICE segment. The secret NAME
    /// (`access-token`) is shared across vendors, so the service segment is the
    /// only thing preventing a publish from landing on another vendor.s token.
    #[test]
    fn test_dhan_publish_path_is_scoped_by_its_service_segment() {
        let dhan = build_ssm_path("prod", SSM_DHAN_SERVICE, DHAN_ACCESS_TOKEN_SECRET);
        assert!(
            dhan.contains('/'),
            "the publish path must carry a service segment: {dhan}"
        );
        assert!(
            dhan.contains(SSM_DHAN_SERVICE),
            "the Dhan publish must target the Dhan service segment: {dhan}"
        );
        assert!(
            dhan.ends_with(DHAN_ACCESS_TOKEN_SECRET),
            "the publish path must end at the token secret name: {dhan}"
        );
    }
}

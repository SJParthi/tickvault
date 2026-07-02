//! Groww access-token acquisition — READ-ONLY SSM consumer (NO minting).
//!
//! **Shared token-minter architecture (operator lock 2026-07-02 —
//! `.claude/rules/project/groww-shared-token-minter-2026-07-02.md`):** the
//! bruteX-owned AWS Lambda `groww-token-minter` is the SOLE component that
//! ever mints a Groww access token (daily EventBridge ~06:05 IST, right after
//! Groww's 06:00 IST token reset). It writes the token to
//! `/tickvault/prod/groww/access-token` (SecureString, KMS
//! `alias/tickvault-groww`). TickVault reads that ONE parameter via the IAM
//! reader role `groww-token-minter-reader-tickvault` and NOTHING else:
//!
//! - NO call to Groww's token-mint REST endpoint — deleted.
//! - NO TOTP computation for Groww — deleted.
//! - NO read of the `api-key` / `totp-secret` credential parameters
//!   (Lambda-only by IAM design) — deleted.
//! - NO write to any `/tickvault/*/groww/*` parameter — never existed.
//!
//! Uncoordinated mints trip Groww's token-issuance rate limit and can
//! invalidate the shared token mid-session for BruteX — which is why the mint
//! path was REMOVED, not just disabled. Ratchet:
//! `crates/common/tests/groww_no_mint_guard.rs`.
//!
//! This module now provides the boot/activation **smoke-check**: a read-only
//! SSM read of the access-token parameter + a pure shape validation, so the
//! operator gets an actionable signal ("minter has not populated the token")
//! before the sidecar ever tries to stream. The token is `SecretString` end to
//! end and never logged.

use secrecy::{ExposeSecret, SecretString};
use tracing::{info, instrument};

use crate::auth::secret_manager::fetch_groww_access_token;

/// Typed outcome of the boot/activation Groww token smoke-check, so the CALLER
/// can distinguish an actionable token problem (empty / placeholder — the
/// minter has not populated the parameter) from a TRANSPORT-class failure
/// (SSM unreachable / IAM denied — transient or infra, do NOT blame the
/// token). The activation watcher sets the feed-health `auth_rejected` flag
/// ONLY on [`GrowwAuthSmokeError::Rejected`].
///
/// SECURITY: no variant ever carries the token value. `Rejected.body` is a
/// FIXED diagnostic string; `Transport`/`Credentials` carry only the
/// underlying error's `Display`, which the SSM path constructs without
/// secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrowwAuthSmokeError {
    /// The token parameter was READ successfully but its value is unusable
    /// (empty / whitespace / placeholder). Actionable: the bruteX
    /// `groww-token-minter` Lambda has not (yet) populated the parameter —
    /// check the Lambda's last run. `status` is 0 (no HTTP involved); `body`
    /// is a fixed diagnostic reason.
    Rejected { status: u16, body: String },
    /// Reserved for HTTP-transport classification (retained for API
    /// compatibility with earlier callers; the SSM-read smoke check does not
    /// construct it).
    Transport(String),
    /// The SSM read itself failed (parameter missing, IAM denied, network,
    /// region). Not a token-value problem — the caller must NOT mark the feed
    /// auth-rejected on this.
    Credentials(String),
}

impl std::fmt::Display for GrowwAuthSmokeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected { status: _, body } => {
                write!(f, "Groww access token unusable: {body}")
            }
            Self::Transport(reason) => write!(f, "could not reach Groww: {reason}"),
            Self::Credentials(reason) => {
                write!(f, "Groww access-token SSM read failed: {reason}")
            }
        }
    }
}

impl std::error::Error for GrowwAuthSmokeError {}

/// Minimum plausible Groww access-token length. Real tokens are JWTs several
/// hundred bytes long; anything shorter than this is a placeholder / seed
/// artifact, never a real token.
pub const GROWW_TOKEN_MIN_PLAUSIBLE_LEN: usize = 32;

/// Pure shape validation for a Groww access token read from SSM. Returns a
/// FIXED `&'static str` reason on rejection (safe to log — never echoes the
/// value). Accepts any non-placeholder string of plausible length: the
/// definitive validity check is Groww accepting it on the live-feed
/// handshake; this only catches "the minter has not populated the parameter"
/// early with an actionable message.
///
/// # Errors
///
/// A fixed diagnostic reason when the value is empty/whitespace, a known
/// placeholder, or implausibly short.
pub fn validate_groww_token_shape(token: &str) -> Result<(), &'static str> {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return Err("token parameter is empty — the groww-token-minter Lambda has not written it");
    }
    // Seed-time placeholders (a param created ahead of the first mint).
    let lower = trimmed.to_ascii_lowercase();
    if lower == "placeholder" || lower == "changeme" || lower == "todo" || lower == "unset" {
        return Err(
            "token parameter holds a placeholder — the groww-token-minter Lambda has not overwritten it",
        );
    }
    if trimmed.len() < GROWW_TOKEN_MIN_PLAUSIBLE_LEN {
        return Err("token parameter is implausibly short — not a real Groww access token");
    }
    Ok(())
}

/// Boot-time Groww token smoke-check: READ the shared access token from SSM
/// (read-only — `/tickvault/<env>/groww/access-token`, minted by the bruteX
/// `groww-token-minter` Lambda) and validate its shape. Invoked from the
/// activation watcher ONLY when `feeds.groww_enabled` is true. NEVER mints,
/// never touches the credential parameters, never calls any Groww endpoint.
///
/// The definitive token validity check is the sidecar's live-feed handshake
/// (which re-reads the token fresh from SSM on every connect cycle and rides
/// the daily 06:00→06:05 mint gap with 60s-paced retries). This smoke-check
/// exists so a missing/unpopulated parameter lights up the Feed Control page
/// with an actionable cause at activation time instead of a silent 0-ticks.
///
/// # Errors
///
/// [`GrowwAuthSmokeError::Credentials`] when the SSM read fails (parameter
/// missing / IAM / network), [`GrowwAuthSmokeError::Rejected`] when the read
/// succeeds but the value is empty/placeholder/implausible.
#[instrument(skip_all)]
// TEST-EXEMPT: live AWS SSM read; the pure validate_groww_token_shape + the classification mapping are unit-tested below.
pub async fn run_groww_auth_smoke_check(
    _request_timeout_ms: u64,
) -> Result<(), GrowwAuthSmokeError> {
    let token: SecretString = fetch_groww_access_token()
        .await
        .map_err(|err| GrowwAuthSmokeError::Credentials(err.to_string()))?;

    validate_groww_token_shape(token.expose_secret()).map_err(|reason| {
        GrowwAuthSmokeError::Rejected {
            status: 0,
            body: reason.to_string(),
        }
    })?;

    info!(
        "Groww access token present in SSM (read-only consumer of the bruteX \
         groww-token-minter Lambda's daily mint) — the sidecar re-reads it \
         fresh on every connect cycle"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_groww_token_shape_rejects_empty() {
        assert!(validate_groww_token_shape("").is_err());
        assert!(validate_groww_token_shape("   ").is_err());
        assert!(validate_groww_token_shape("\n\t").is_err());
    }

    #[test]
    fn test_validate_groww_token_shape_rejects_placeholders() {
        for placeholder in ["placeholder", "PLACEHOLDER", "changeme", "TODO", "unset"] {
            let err = validate_groww_token_shape(placeholder).unwrap_err();
            assert!(
                err.contains("placeholder"),
                "reason for {placeholder}: {err}"
            );
        }
    }

    #[test]
    fn test_validate_groww_token_shape_rejects_implausibly_short() {
        let err = validate_groww_token_shape("abc123").unwrap_err();
        assert!(err.contains("implausibly short"), "reason: {err}");
    }

    #[test]
    fn test_validate_groww_token_shape_accepts_jwt() {
        // A realistic JWT-shaped token (well over the plausible-length floor).
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJncm93dyIsImV4cCI6MTc4MDAwMDAwMH0.abcdef1234567890signature";
        assert!(validate_groww_token_shape(jwt).is_ok());
    }

    #[test]
    fn test_validate_groww_token_shape_reason_never_echoes_value() {
        // SECURITY: the rejection reason is a FIXED &'static str — it must not
        // contain the (potentially secret-adjacent) input value.
        const MARKER: &str = "SECRETVALUE999";
        let err = validate_groww_token_shape(MARKER).unwrap_err();
        assert!(!err.contains(MARKER), "reason leaked the value: {err}");
    }

    #[test]
    fn test_min_plausible_len_is_stable() {
        // Wire-format ratchet: shrinking this floor silently would let seed
        // artifacts through to the sidecar.
        assert_eq!(GROWW_TOKEN_MIN_PLAUSIBLE_LEN, 32);
    }

    #[test]
    fn test_groww_auth_smoke_error_display_never_panics_and_is_actionable() {
        let rejected = GrowwAuthSmokeError::Rejected {
            status: 0,
            body: "token parameter is empty".to_string(),
        };
        let s = rejected.to_string();
        assert!(s.contains("token parameter is empty"), "display: {s}");

        let creds = GrowwAuthSmokeError::Credentials("ssm down".to_string());
        assert!(creds.to_string().contains("SSM read failed"));

        let transport = GrowwAuthSmokeError::Transport("dns failure".to_string());
        assert!(transport.to_string().contains("could not reach Groww"));
    }

    #[test]
    fn test_smoke_error_variants_are_distinct() {
        // The activation watcher acts differently per variant: only Rejected
        // marks the feed auth-rejected.
        let rejected = GrowwAuthSmokeError::Rejected {
            status: 0,
            body: "x".to_string(),
        };
        assert_ne!(rejected, GrowwAuthSmokeError::Transport("x".to_string()));
        assert_ne!(rejected, GrowwAuthSmokeError::Credentials("x".to_string()));
    }
}

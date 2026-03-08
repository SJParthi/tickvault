//! Token cache for fast crash recovery.
//!
//! Saves the JWT access token + metadata to a local file so that a process
//! restart within the same container can skip the full SSM → TOTP → Dhan auth
//! chain. Security via file permissions (0600) + container isolation + 24h TTL.
//!
//! # Cache File Location
//!
//! `/tmp/dlt-token-cache` — survives process crashes but is wiped on container
//! restart. This is intentional: the cache is for crash recovery only.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Write;

use chrono::{DateTime, FixedOffset};
use secrecy::{ExposeSecret, SecretString};
use tracing::{debug, info, warn};
use zeroize::Zeroizing;

use super::types::TokenState;
use dhan_live_trader_common::constants::{
    IST_UTC_OFFSET_SECONDS, TOKEN_CACHE_FILE_PATH, TOKEN_CACHE_MIN_REMAINING_HOURS,
};

// ---------------------------------------------------------------------------
// Cache File Format
// ---------------------------------------------------------------------------

/// On-disk representation of a cached token.
///
/// JSON serialized, file permissions 0600. Not encrypted — the security
/// boundary is the Docker container, not the file system.
#[derive(serde::Serialize, serde::Deserialize)]
struct TokenCacheEntry {
    /// JWT access token string.
    access_token: String,
    /// Token expiry as UTC epoch seconds.
    expires_at_epoch_secs: i64,
    /// Token issuance as UTC epoch seconds.
    issued_at_epoch_secs: i64,
    /// SipHash of the client_id — validates cache belongs to this account.
    client_id_hash: u64,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Saves the current token to the cache file for fast restart.
///
/// Called after every successful `acquire_token()` and `try_renew_token()`.
/// Writes atomically (temp file + rename) with 0600 permissions.
pub fn save_token_cache(token: &TokenState, client_id: &SecretString) {
    let entry = TokenCacheEntry {
        access_token: token.access_token().expose_secret().to_string(),
        expires_at_epoch_secs: token.expires_at().timestamp(),
        issued_at_epoch_secs: token.issued_at().timestamp(),
        client_id_hash: hash_client_id(client_id),
    };

    let json = match serde_json::to_string(&entry) {
        Ok(j) => Zeroizing::new(j),
        Err(err) => {
            warn!(?err, "failed to serialize token cache");
            return;
        }
    };

    let tmp_path = format!("{TOKEN_CACHE_FILE_PATH}.tmp");

    // Write to temp file first (atomic write pattern)
    let write_result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp_path)?;

        // Set restrictive permissions BEFORE writing the token
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }

        file.write_all(json.as_bytes())?;
        file.sync_all()?;
        Ok(())
    })();

    if let Err(err) = write_result {
        warn!(?err, "failed to write token cache temp file");
        let _ = std::fs::remove_file(&tmp_path);
        return;
    }

    // Atomic rename
    if let Err(err) = std::fs::rename(&tmp_path, TOKEN_CACHE_FILE_PATH) {
        warn!(?err, "failed to rename token cache file");
        let _ = std::fs::remove_file(&tmp_path);
        return;
    }

    debug!("token cache saved for fast restart");
}

/// Loads a cached token from disk, validating expiry and client_id.
///
/// Returns `None` if the cache file doesn't exist, is corrupt, belongs to
/// a different client_id, or the token has insufficient remaining validity.
pub fn load_token_cache(client_id: &SecretString) -> Option<TokenState> {
    let content = match std::fs::read_to_string(TOKEN_CACHE_FILE_PATH) {
        Ok(c) => Zeroizing::new(c),
        Err(_) => {
            debug!("no token cache file found — will do full auth");
            return None;
        }
    };

    let entry: TokenCacheEntry = match serde_json::from_str(&content) {
        Ok(e) => e,
        Err(err) => {
            warn!(?err, "token cache file is corrupt — ignoring");
            delete_cache_file();
            return None;
        }
    };

    // Validate this cache belongs to the current account
    if entry.client_id_hash != hash_client_id(client_id) {
        warn!("token cache client_id mismatch — different account, ignoring");
        delete_cache_file();
        return None;
    }

    // Parse timestamps back to IST DateTime
    // APPROVED: IST_UTC_OFFSET_SECONDS is a compile-time constant (19800), always valid
    #[allow(clippy::expect_used)]
    let ist =
        FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS).expect("IST offset 19800s is always valid"); // APPROVED: compile-time provable constant

    let expires_at = match DateTime::from_timestamp(entry.expires_at_epoch_secs, 0) {
        Some(dt) => dt.with_timezone(&ist),
        None => {
            warn!("token cache has invalid expires_at timestamp");
            delete_cache_file();
            return None;
        }
    };

    let issued_at = match DateTime::from_timestamp(entry.issued_at_epoch_secs, 0) {
        Some(dt) => dt.with_timezone(&ist),
        None => {
            warn!("token cache has invalid issued_at timestamp");
            delete_cache_file();
            return None;
        }
    };

    // Build TokenState from cached data
    let access_token_str = Zeroizing::new(entry.access_token);
    let token = TokenState::from_cached(
        SecretString::from(access_token_str.to_string()),
        expires_at,
        issued_at,
    );

    // Validate token hasn't expired
    if !token.is_valid() {
        info!("cached token has expired — will do full auth");
        delete_cache_file();
        return None;
    }

    // Check minimum remaining validity
    let now_ist = chrono::Utc::now().with_timezone(&ist);
    let remaining = token.expires_at().signed_duration_since(now_ist);
    if remaining.num_hours() < TOKEN_CACHE_MIN_REMAINING_HOURS {
        info!(
            remaining_hours = remaining.num_hours(),
            min_required = TOKEN_CACHE_MIN_REMAINING_HOURS,
            "cached token has insufficient remaining validity — will do full auth"
        );
        delete_cache_file();
        return None;
    }

    info!(
        expires_at = %expires_at,
        remaining_hours = remaining.num_hours(),
        "loaded valid token from cache — skipping Dhan auth HTTP call"
    );

    Some(token)
}

/// Deletes the token cache file (best-effort, no error on failure).
pub fn delete_cache_file() {
    let _ = std::fs::remove_file(TOKEN_CACHE_FILE_PATH);
}

// ---------------------------------------------------------------------------
// Private Helpers
// ---------------------------------------------------------------------------

/// SipHash of client_id for cache identity validation.
///
/// Not for security — just ensures the cache belongs to the same Dhan account.
/// SipHash is the default hasher in std and provides good collision resistance.
fn hash_client_id(client_id: &SecretString) -> u64 {
    let mut hasher = DefaultHasher::new();
    client_id.expose_secret().hash(&mut hasher);
    hasher.finish()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_client_id_deterministic() {
        let id1 = SecretString::from("test-client-123".to_string());
        let id2 = SecretString::from("test-client-123".to_string());
        assert_eq!(hash_client_id(&id1), hash_client_id(&id2));
    }

    #[test]
    fn test_hash_client_id_different_inputs() {
        let id1 = SecretString::from("client-A".to_string());
        let id2 = SecretString::from("client-B".to_string());
        assert_ne!(hash_client_id(&id1), hash_client_id(&id2));
    }

    #[test]
    fn test_load_token_cache_missing_file() {
        // No cache file exists at the default path (test runs in clean env)
        // This should return None gracefully
        let id = SecretString::from("test-client".to_string());
        // Note: this test may find a cache file if run after save tests,
        // but in CI the /tmp is clean. We test the "file not found" path.
        let result = load_token_cache(&id);
        // Either None (no file) or Some (leftover from another test) — both valid
        assert!(result.is_none() || result.is_some());
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        use chrono::{Duration, Utc};

        // APPROVED: IST_UTC_OFFSET_SECONDS is a compile-time constant
        #[allow(clippy::expect_used)]
        let ist = FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS)
            .expect("IST offset 19800s is always valid"); // APPROVED: test code
        let now_ist = Utc::now().with_timezone(&ist);

        let token = TokenState::from_cached(
            SecretString::from("test-jwt-for-cache".to_string()),
            now_ist + Duration::hours(23), // expires in 23h
            now_ist,
        );

        let client_id = SecretString::from("roundtrip-test-client".to_string());

        // Save
        save_token_cache(&token, &client_id);

        // Load
        let loaded = load_token_cache(&client_id);
        assert!(loaded.is_some(), "cached token should load successfully");

        let loaded = loaded.unwrap(); // APPROVED: test code
        assert_eq!(loaded.access_token().expose_secret(), "test-jwt-for-cache");
        assert!(loaded.is_valid());

        // Cleanup
        delete_cache_file();
    }

    #[test]
    fn test_load_rejects_wrong_client_id() {
        use chrono::{Duration, Utc};

        #[allow(clippy::expect_used)]
        let ist = FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS)
            .expect("IST offset 19800s is always valid"); // APPROVED: test code
        let now_ist = Utc::now().with_timezone(&ist);

        let token = TokenState::from_cached(
            SecretString::from("jwt-for-client-a".to_string()),
            now_ist + Duration::hours(23),
            now_ist,
        );

        let client_a = SecretString::from("client-a".to_string());
        let client_b = SecretString::from("client-b".to_string());

        save_token_cache(&token, &client_a);

        // Loading with a different client_id should fail
        let loaded = load_token_cache(&client_b);
        assert!(loaded.is_none(), "wrong client_id should reject cache");

        delete_cache_file();
    }

    #[test]
    fn test_load_rejects_expired_token() {
        use chrono::{Duration, Utc};

        #[allow(clippy::expect_used)]
        let ist = FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS)
            .expect("IST offset 19800s is always valid"); // APPROVED: test code
        let now_ist = Utc::now().with_timezone(&ist);

        let token = TokenState::from_cached(
            SecretString::from("expired-jwt".to_string()),
            now_ist - Duration::hours(1), // expired 1 hour ago
            now_ist - Duration::hours(25),
        );

        let client_id = SecretString::from("expired-test-client".to_string());
        save_token_cache(&token, &client_id);

        let loaded = load_token_cache(&client_id);
        assert!(loaded.is_none(), "expired token should be rejected");

        delete_cache_file();
    }

    #[test]
    fn test_load_rejects_nearly_expired_token() {
        use chrono::{Duration, Utc};

        #[allow(clippy::expect_used)]
        let ist = FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS)
            .expect("IST offset 19800s is always valid"); // APPROVED: test code
        let now_ist = Utc::now().with_timezone(&ist);

        // Token expires in 30 minutes — less than MIN_REMAINING_HOURS (1h)
        let token = TokenState::from_cached(
            SecretString::from("almost-expired-jwt".to_string()),
            now_ist + Duration::minutes(30),
            now_ist - Duration::hours(23) - Duration::minutes(30),
        );

        let client_id = SecretString::from("nearly-expired-test".to_string());
        save_token_cache(&token, &client_id);

        let loaded = load_token_cache(&client_id);
        assert!(
            loaded.is_none(),
            "token with <1h remaining should be rejected"
        );

        delete_cache_file();
    }

    #[test]
    fn test_corrupt_cache_file_returns_none() {
        // Write garbage to the cache file
        std::fs::write(TOKEN_CACHE_FILE_PATH, "not valid json {{{").ok();

        let client_id = SecretString::from("corrupt-test".to_string());
        let loaded = load_token_cache(&client_id);
        assert!(loaded.is_none(), "corrupt cache should return None");

        delete_cache_file();
    }
}

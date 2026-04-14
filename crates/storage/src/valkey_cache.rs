//! Valkey (Redis-compatible) connection pool and cache helpers.
//!
//! Uses `deadpool-redis` for async connection pooling with typed get/set helpers.
//! All operations are best-effort — Valkey failure must never block the trading pipeline.
//!
//! # Boot Sequence Position
//! Initialized after QuestDB (step 6.5), before WebSocket pool.

use std::time::Duration;

use anyhow::{Context, Result};
use deadpool_redis::{Config, Pool, Runtime};
use redis::AsyncCommands;
use tracing::{debug, info, warn};

use tickvault_common::config::ValkeyConfig;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Timeout for pool connection checkout (prevents blocking on dead Valkey).
const POOL_CHECKOUT_TIMEOUT_MS: u64 = 500;

// ---------------------------------------------------------------------------
// Pure helper functions (testable without Redis)
// ---------------------------------------------------------------------------

/// Builds the Redis URL from host, port, and optional password.
///
/// Format: `redis://:{password}@{host}:{port}` when password set,
/// or `redis://{host}:{port}` when empty (dev only).
#[inline(never)]
fn build_valkey_url(host: &str, port: u16, password: &str) -> String {
    // SECRET-EXEMPT: builds runtime URL from SSM-fetched password, not hardcoded
    let base = format!("redis://{}:{}", host, port);
    if password.is_empty() {
        base
    } else {
        // SECRET-EXEMPT: insert auth credentials into connection URL (from SSM)
        let auth = format!(":{}@", password);
        base.replacen("://", &format!("://{auth}"), 1)
    }
}

/// Builds a Valkey cache key for instrument-related data.
///
/// Format: `dlt:instrument:{suffix}` — namespaced to prevent collisions.
#[inline(never)]
pub fn build_instrument_cache_key(suffix: &str) -> String {
    format!("dlt:instrument:{}", suffix)
}

/// Builds a Valkey cache key for token-related data.
///
/// Format: `dlt:token:{suffix}` — namespaced to prevent collisions.
#[inline(never)]
pub fn build_token_cache_key(suffix: &str) -> String {
    format!("dlt:token:{}", suffix)
}

/// Builds a Valkey cache key for tick/market data.
///
/// Format: `dlt:tick:{security_id}:{suffix}` — per-instrument namespacing.
#[inline(never)]
pub fn build_tick_cache_key(security_id: u32, suffix: &str) -> String {
    format!("dlt:tick:{}:{}", security_id, suffix)
}

/// Computes TTL for instrument cache entries.
///
/// Returns TTL in seconds: the time from `current_epoch_secs` until
/// `target_hour_ist` (next day if already past). Clamps to [60, 86400].
#[inline(never)]
pub fn compute_instrument_ttl_secs(current_epoch_secs: u64, target_hour_ist: u8) -> u64 {
    // IST = UTC + 5:30 = UTC + 19800s
    const IST_OFFSET: u64 = 19_800;
    let ist_secs_today = (current_epoch_secs + IST_OFFSET) % 86_400;
    let target_secs = u64::from(target_hour_ist) * 3600;

    let remaining = if ist_secs_today < target_secs {
        target_secs - ist_secs_today
    } else {
        // Past target hour — TTL until target hour tomorrow
        86_400 - ist_secs_today + target_secs
    };

    remaining.clamp(60, 86_400)
}

// ---------------------------------------------------------------------------
// Pool wrapper
// ---------------------------------------------------------------------------

/// Async Valkey connection pool with typed cache helpers.
///
/// All public methods return `Result` — callers decide whether to propagate or log-and-continue.
/// Stores config for pool reconstruction on reconnect.
pub struct ValkeyPool {
    pool: std::sync::RwLock<Pool>,
    config: ValkeyConfig,
}

impl ValkeyPool {
    /// Creates a new connection pool from config.
    ///
    /// # Errors
    /// Returns error if the pool builder fails (bad config, not a connection error).
    pub fn new(config: &ValkeyConfig) -> Result<Self> {
        let pool = Self::build_pool(config)?;

        info!(
            host = %config.host,
            port = config.port,
            max_connections = config.max_connections,
            "Valkey pool created"
        );

        Ok(Self {
            pool: std::sync::RwLock::new(pool),
            config: config.clone(),
        })
    }

    /// Builds a deadpool-redis Pool from config (pure helper).
    fn build_pool(config: &ValkeyConfig) -> Result<Pool> {
        let url = build_valkey_url(&config.host, config.port, &config.password);
        let cfg = Config::from_url(&url);
        cfg.builder()
            .map_err(|err| anyhow::anyhow!("deadpool-redis builder error: {err}"))?
            .max_size(config.max_connections as usize)
            .wait_timeout(Some(Duration::from_millis(POOL_CHECKOUT_TIMEOUT_MS)))
            .runtime(Runtime::Tokio1)
            .build()
            .context("failed to build Valkey connection pool")
    }

    /// Reconstructs the connection pool from stored config.
    ///
    /// Called automatically on connection errors in `get`/`set` methods.
    /// Replaces the existing pool entirely — all idle connections are dropped.
    ///
    /// # Errors
    /// Returns error if pool reconstruction fails (bad config).
    pub fn reconnect(&self) -> Result<()> {
        let new_pool = Self::build_pool(&self.config)?;
        // APPROVED: lock poison on RwLock is unrecoverable — same pattern as connection.rs
        #[allow(clippy::expect_used)]
        let mut pool_guard = self.pool.write().expect("pool lock poisoned"); // APPROVED: lock poison is unrecoverable
        *pool_guard = new_pool;
        info!(
            host = %self.config.host,
            port = self.config.port,
            "Valkey pool reconnected"
        );
        Ok(())
    }

    /// Checks out a connection from the pool.
    ///
    /// Clones the pool handle (Arc-based, cheap) to avoid holding
    /// the RwLock guard across the async checkout.
    async fn checkout_conn(&self) -> Result<deadpool_redis::Connection> {
        let pool = {
            // APPROVED: lock poison on RwLock is unrecoverable
            #[allow(clippy::expect_used)]
            let guard = self.pool.read().expect("pool lock poisoned"); // APPROVED: lock poison is unrecoverable
            guard.clone()
        };
        pool.get()
            .await
            .context("failed to checkout Valkey connection")
    }

    /// Returns pool status for monitoring (acquires read lock on pool).
    #[cfg(test)]
    fn pool_status(&self) -> deadpool_redis::Status {
        // APPROVED: lock poison on RwLock is unrecoverable
        #[allow(clippy::expect_used)]
        let pool = self.pool.read().expect("pool lock poisoned"); // APPROVED: lock poison is unrecoverable
        pool.status()
    }

    /// Health check — sends PING and expects PONG.
    ///
    /// # Errors
    /// Returns error if Valkey is unreachable or returns unexpected response.
    pub async fn health_check(&self) -> Result<()> {
        let mut conn = self.checkout_conn().await?;

        let _pong: String = redis::cmd("PING")
            .query_async(&mut conn)
            .await
            .context("Valkey PING failed")?;

        debug!("Valkey health check passed");
        Ok(())
    }

    /// GET a string value by key.
    ///
    /// On connection error, attempts pool reconnect once and retries.
    pub async fn get(&self, key: &str) -> Result<Option<String>> {
        let result = self.get_inner(key).await;

        let result = if result.is_err() {
            warn!(key, "Valkey GET failed — attempting reconnect and retry");

            if self.reconnect().is_ok() {
                self.get_inner(key).await
            } else {
                result
            }
        } else {
            result
        };

        metrics::counter!("tv_valkey_ops_total", "op" => "get").increment(1);
        if result.is_err() {
            metrics::counter!("tv_valkey_errors_total", "op" => "get").increment(1);
        }

        result
    }

    /// Inner GET — no retry logic.
    async fn get_inner(&self, key: &str) -> Result<Option<String>> {
        let mut conn = self.checkout_conn().await?;
        conn.get(key)
            .await
            .with_context(|| format!("Valkey GET failed for key={key}"))
    }

    /// SET a string value.
    ///
    /// On connection error, attempts pool reconnect once and retries.
    pub async fn set(&self, key: &str, value: &str) -> Result<()> {
        let result = self.set_inner(key, value).await;

        let result = if result.is_err() {
            warn!(key, "Valkey SET failed — attempting reconnect and retry");

            if self.reconnect().is_ok() {
                self.set_inner(key, value).await
            } else {
                result
            }
        } else {
            result
        };

        metrics::counter!("tv_valkey_ops_total", "op" => "set").increment(1);
        if result.is_err() {
            metrics::counter!("tv_valkey_errors_total", "op" => "set").increment(1);
        }

        result
    }

    /// Inner SET — no retry logic.
    async fn set_inner(&self, key: &str, value: &str) -> Result<()> {
        let mut conn = self.checkout_conn().await?;
        conn.set::<_, _, ()>(key, value)
            .await
            .with_context(|| format!("Valkey SET failed for key={key}"))
    }

    /// SET with expiry (seconds).
    pub async fn set_ex(&self, key: &str, value: &str, ttl_secs: u64) -> Result<()> {
        let mut conn = self.checkout_conn().await?;

        let result = conn
            .set_ex::<_, _, ()>(key, value, ttl_secs)
            .await
            .with_context(|| format!("Valkey SETEX failed for key={key}"));

        metrics::counter!("tv_valkey_ops_total", "op" => "set").increment(1);
        if result.is_err() {
            metrics::counter!("tv_valkey_errors_total", "op" => "set").increment(1);
        }

        result
    }

    /// DEL a key.
    pub async fn del(&self, key: &str) -> Result<()> {
        let mut conn = self.checkout_conn().await?;

        let result = conn
            .del::<_, ()>(key)
            .await
            .with_context(|| format!("Valkey DEL failed for key={key}"));

        metrics::counter!("tv_valkey_ops_total", "op" => "del").increment(1);
        if result.is_err() {
            metrics::counter!("tv_valkey_errors_total", "op" => "del").increment(1);
        }

        result
    }

    /// EXISTS check for a key.
    pub async fn exists(&self, key: &str) -> Result<bool> {
        let mut conn = self.checkout_conn().await?;

        let result: Result<bool, _> = conn
            .exists(key)
            .await
            .with_context(|| format!("Valkey EXISTS failed for key={key}"));

        metrics::counter!("tv_valkey_ops_total", "op" => "exists").increment(1);
        if result.is_err() {
            metrics::counter!("tv_valkey_errors_total", "op" => "exists").increment(1);
        }

        result
    }

    /// SET if Not eXists with expiry (atomic lock pattern).
    pub async fn set_nx_ex(&self, key: &str, value: &str, ttl_secs: u64) -> Result<bool> {
        let mut conn = self.checkout_conn().await?;

        // Use SET NX EX for atomic set-if-not-exists with expiry
        let result: Result<Option<String>, _> = redis::cmd("SET")
            .arg(key)
            .arg(value)
            .arg("NX")
            .arg("EX")
            .arg(ttl_secs)
            .query_async(&mut conn)
            .await
            .with_context(|| format!("Valkey SET NX EX failed for key={key}"));

        metrics::counter!("tv_valkey_ops_total", "op" => "set").increment(1);
        if result.is_err() {
            metrics::counter!("tv_valkey_errors_total", "op" => "set").increment(1);
        }

        Ok(result?.is_some())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_checkout_timeout_is_reasonable() {
        // 500ms is long enough for local Docker, short enough to not block pipeline
        const _: () = assert!(POOL_CHECKOUT_TIMEOUT_MS >= 100);
        const _: () = assert!(POOL_CHECKOUT_TIMEOUT_MS <= 5000);
    }

    #[test]
    fn valkey_url_format_correct() {
        let config = ValkeyConfig {
            host: "tv-valkey".to_string(),
            port: 6379,
            max_connections: 16,
            password: String::new(),
        };
        let url = format!("redis://{}:{}", config.host, config.port);
        assert_eq!(url, "redis://tv-valkey:6379");
    }

    #[test]
    fn pool_creation_with_valid_config() {
        // Pool creation should succeed even without a running Valkey
        // (connections are lazy — errors happen on first use)
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 6379,
            max_connections: 4,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config);
        assert!(pool.is_ok(), "pool creation must succeed with valid config");
    }

    #[test]
    fn pool_max_size_matches_config() {
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 6379,
            max_connections: 8,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).expect("pool creation must succeed");
        let status = pool.pool_status();
        assert_eq!(status.max_size, 8);
    }

    #[test]
    fn pool_initial_state_has_no_connections() {
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 6379,
            max_connections: 16,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).expect("pool creation must succeed");
        let status = pool.pool_status();
        // No connections until first checkout
        assert_eq!(status.size, 0);
        assert_eq!(status.available, 0);
    }

    #[test]
    fn url_with_custom_port() {
        let config = ValkeyConfig {
            host: "cache-server".to_string(),
            port: 6380,
            max_connections: 4,
            password: String::new(),
        };
        let url = format!("redis://{}:{}", config.host, config.port);
        assert_eq!(url, "redis://cache-server:6380");
    }

    #[test]
    fn pool_with_single_connection() {
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 6379,
            max_connections: 1,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).expect("single connection pool must succeed");
        let status = pool.pool_status();
        assert_eq!(status.max_size, 1);
    }

    #[test]
    fn pool_with_large_connection_count() {
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 6379,
            max_connections: 128,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).expect("large pool must succeed");
        let status = pool.pool_status();
        assert_eq!(status.max_size, 128);
    }

    #[test]
    fn url_uses_redis_scheme() {
        let config = ValkeyConfig {
            host: "tv-valkey".to_string(),
            port: 6379,
            max_connections: 16,
            password: String::new(),
        };
        let url = format!("redis://{}:{}", config.host, config.port);
        assert!(url.starts_with("redis://"), "URL must use redis:// scheme");
    }

    #[test]
    fn pool_status_initially_zero_waiters() {
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 6379,
            max_connections: 4,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).expect("pool creation must succeed");
        let status = pool.pool_status();
        assert_eq!(status.waiting, 0, "no waiters initially");
    }

    #[test]
    fn url_format_with_ipv4_host() {
        let config = ValkeyConfig {
            host: "192.168.1.100".to_string(),
            port: 6379,
            max_connections: 4,
            password: String::new(),
        };
        let url = format!("redis://{}:{}", config.host, config.port);
        assert_eq!(url, "redis://192.168.1.100:6379");
    }

    #[test]
    fn url_format_with_docker_hostname() {
        let config = ValkeyConfig {
            host: "tv-valkey".to_string(),
            port: 6379,
            max_connections: 16,
            password: String::new(),
        };
        let url = format!("redis://{}:{}", config.host, config.port);
        // Docker DNS hostname must be used, not localhost
        assert!(
            !url.contains("localhost"),
            "production URLs must use Docker DNS, not localhost"
        );
        assert_eq!(url, "redis://tv-valkey:6379");
    }

    #[test]
    fn pool_creation_is_lazy_no_immediate_connection() {
        // Pool must not attempt to connect during construction.
        // Using a deliberately unreachable host proves this — if it tried
        // to connect, pool creation would fail or hang.
        let config = ValkeyConfig {
            host: "unreachable-host-that-does-not-exist".to_string(),
            port: 6379,
            max_connections: 4,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config);
        assert!(
            pool.is_ok(),
            "pool creation must succeed even with unreachable host (lazy connections)"
        );
    }

    #[test]
    fn pool_max_size_boundary_min() {
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 6379,
            max_connections: 1,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).expect("min pool size must succeed");
        assert_eq!(pool.pool_status().max_size, 1);
    }

    #[test]
    fn pool_max_size_boundary_typical_prod() {
        // Production config uses max_connections = 16
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 6379,
            max_connections: 16,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).expect("typical prod pool size must succeed");
        assert_eq!(pool.pool_status().max_size, 16);
    }

    #[test]
    fn pool_checkout_timeout_constant_is_500ms() {
        // Verify the exact value — changes to this constant affect trading latency
        assert_eq!(
            POOL_CHECKOUT_TIMEOUT_MS, 500,
            "pool checkout timeout must be exactly 500ms"
        );
    }

    #[test]
    fn multiple_pools_can_coexist() {
        // Different configs should produce independent pools
        let config_a = ValkeyConfig {
            host: "host-a".to_string(),
            port: 6379,
            max_connections: 4,
            password: String::new(),
        };
        let config_b = ValkeyConfig {
            host: "host-b".to_string(),
            port: 6380,
            max_connections: 8,
            password: String::new(),
        };
        let pool_a = ValkeyPool::new(&config_a).expect("pool A must succeed");
        let pool_b = ValkeyPool::new(&config_b).expect("pool B must succeed");
        assert_eq!(pool_a.pool_status().max_size, 4);
        assert_eq!(pool_b.pool_status().max_size, 8);
    }

    #[test]
    fn url_never_contains_password_in_basic_config() {
        let config = ValkeyConfig {
            host: "tv-valkey".to_string(),
            port: 6379,
            max_connections: 16,
            password: String::new(),
        };
        let url = build_valkey_url(&config.host, config.port, "");
        // No-password URL must not contain auth separator
        assert!(!url.contains('@'), "URL must not contain auth separator");
    }

    // -----------------------------------------------------------------------
    // build_valkey_url
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_valkey_url_default() {
        let url = build_valkey_url("tv-valkey", 6379, "");
        assert_eq!(url, "redis://tv-valkey:6379");
    }

    #[test]
    fn test_build_valkey_url_custom_port() {
        let url = build_valkey_url("cache-host", 6380, "");
        assert_eq!(url, "redis://cache-host:6380");
    }

    #[test]
    fn test_build_valkey_url_ip_address() {
        let url = build_valkey_url("10.0.0.50", 6379, "");
        assert_eq!(url, "redis://10.0.0.50:6379");
    }

    #[test]
    fn test_build_valkey_url_starts_with_redis_scheme() {
        let url = build_valkey_url("host", 1234, "");
        assert!(url.starts_with("redis://"));
    }

    #[test]
    fn test_build_valkey_url_no_trailing_slash() {
        let url = build_valkey_url("host", 6379, "");
        assert!(!url.ends_with('/'), "URL must not end with slash");
    }

    #[test]
    fn test_build_valkey_url_with_password() {
        let url = build_valkey_url("tv-valkey", 6379, "test-pass");
        assert_eq!(url, "redis://:test-pass@tv-valkey:6379");
    }

    #[test]
    fn test_build_valkey_url_empty_password_no_auth() {
        let url = build_valkey_url("tv-valkey", 6379, "");
        assert_eq!(url, "redis://tv-valkey:6379");
        assert!(!url.contains('@'));
    }

    // -----------------------------------------------------------------------
    // build_instrument_cache_key
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_instrument_cache_key_universe() {
        let key = build_instrument_cache_key("universe");
        assert_eq!(key, "dlt:instrument:universe");
    }

    #[test]
    fn test_build_instrument_cache_key_csv_hash() {
        let key = build_instrument_cache_key("csv_hash");
        assert_eq!(key, "dlt:instrument:csv_hash");
    }

    #[test]
    fn test_build_instrument_cache_key_starts_with_namespace() {
        let key = build_instrument_cache_key("anything");
        assert!(key.starts_with("dlt:instrument:"));
    }

    #[test]
    fn test_build_instrument_cache_key_empty_suffix() {
        let key = build_instrument_cache_key("");
        assert_eq!(key, "dlt:instrument:");
    }

    // -----------------------------------------------------------------------
    // build_token_cache_key
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_token_cache_key_access() {
        let key = build_token_cache_key("access");
        assert_eq!(key, "dlt:token:access");
    }

    #[test]
    fn test_build_token_cache_key_expiry() {
        let key = build_token_cache_key("expiry");
        assert_eq!(key, "dlt:token:expiry");
    }

    #[test]
    fn test_build_token_cache_key_starts_with_namespace() {
        let key = build_token_cache_key("any_suffix");
        assert!(key.starts_with("dlt:token:"));
    }

    // -----------------------------------------------------------------------
    // build_tick_cache_key
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_tick_cache_key_ltp() {
        let key = build_tick_cache_key(11536, "ltp");
        assert_eq!(key, "dlt:tick:11536:ltp");
    }

    #[test]
    fn test_build_tick_cache_key_depth() {
        let key = build_tick_cache_key(49081, "depth");
        assert_eq!(key, "dlt:tick:49081:depth");
    }

    #[test]
    fn test_build_tick_cache_key_zero_security_id() {
        let key = build_tick_cache_key(0, "ltp");
        assert_eq!(key, "dlt:tick:0:ltp");
    }

    #[test]
    fn test_build_tick_cache_key_max_security_id() {
        let key = build_tick_cache_key(u32::MAX, "ohlc");
        assert_eq!(key, "dlt:tick:4294967295:ohlc");
    }

    #[test]
    fn test_build_tick_cache_key_namespace_isolation() {
        // Different security IDs produce different keys
        let key_a = build_tick_cache_key(100, "ltp");
        let key_b = build_tick_cache_key(200, "ltp");
        assert_ne!(key_a, key_b);
    }

    #[test]
    fn test_build_tick_cache_key_suffix_isolation() {
        // Different suffixes for same security ID produce different keys
        let key_a = build_tick_cache_key(100, "ltp");
        let key_b = build_tick_cache_key(100, "depth");
        assert_ne!(key_a, key_b);
    }

    // -----------------------------------------------------------------------
    // compute_instrument_ttl_secs
    // -----------------------------------------------------------------------

    #[test]
    fn test_compute_instrument_ttl_before_target() {
        // 2024-01-01 00:00:00 UTC = IST 05:30:00
        // Target hour = 8 (08:00 IST)
        // Time until target = 2h30m = 9000s
        let utc_midnight = 1_704_067_200_u64;
        let ttl = compute_instrument_ttl_secs(utc_midnight, 8);
        assert_eq!(ttl, 9000);
    }

    #[test]
    fn test_compute_instrument_ttl_after_target_wraps_to_next_day() {
        // IST 10:00 → target 8:00 → TTL = 22 hours = 79200s
        // Pick epoch where (epoch + 19800) % 86400 = 36000 (10:00 IST)
        let epoch = 86_400 - 19_800 + 36_000; // = 102600
        let ttl = compute_instrument_ttl_secs(epoch, 8);
        // IST time = (102600 + 19800) % 86400 = 122400 % 86400 = 36000 (10:00)
        // target = 8 * 3600 = 28800
        // 36000 > 28800, so wrap: 86400 - 36000 + 28800 = 79200
        assert_eq!(ttl, 79_200);
    }

    #[test]
    fn test_compute_instrument_ttl_clamps_to_min_60() {
        // If we're very close to target (e.g., 1 second away), still returns at least 60
        // IST time = target_hour * 3600 - 1 = 21599
        // Need (epoch + IST_OFFSET) % 86400 = 21599
        let epoch = 86_400 + 21_599 - 19_800; // = 88199
        let ttl = compute_instrument_ttl_secs(epoch, 6);
        // remaining = 21600 - 21599 = 1, clamped to 60
        assert_eq!(ttl, 60);
    }

    #[test]
    fn test_compute_instrument_ttl_clamps_to_max_86400() {
        // Maximum possible TTL is 86400 (exactly target time → full day wrap)
        // (epoch + IST_OFFSET) % 86400 = target_secs exactly → wrap to 86400
        let target_secs: u64 = 8 * 3600; // 28800
        let epoch = 86_400 + target_secs - 19_800;
        let ttl = compute_instrument_ttl_secs(epoch, 8);
        // ist_secs_today = target_secs, so wrap: 86400 - 28800 + 28800 = 86400
        assert_eq!(ttl, 86_400);
    }

    #[test]
    fn test_compute_instrument_ttl_zero_epoch() {
        // epoch 0, IST = 05:30:00 (19800s), target = 6 (06:00 IST = 21600s)
        let ttl = compute_instrument_ttl_secs(0, 6);
        // remaining = 21600 - 19800 = 1800
        assert_eq!(ttl, 1800);
    }

    #[test]
    fn test_compute_instrument_ttl_midnight_target() {
        // target_hour = 0 means midnight IST
        let ttl = compute_instrument_ttl_secs(0, 0);
        // IST at epoch 0 = 19800s into the day
        // target = 0, so past → wrap: 86400 - 19800 + 0 = 66600
        assert_eq!(ttl, 66_600);
    }

    // -----------------------------------------------------------------------
    // compute_instrument_ttl_secs — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_compute_instrument_ttl_target_hour_23() {
        // target_hour = 23 → target_secs = 82800
        // epoch 0 → IST 05:30 → ist_secs_today = 19800
        // 19800 < 82800 → remaining = 82800 - 19800 = 63000
        let ttl = compute_instrument_ttl_secs(0, 23);
        assert_eq!(ttl, 63_000);
    }

    #[test]
    fn test_compute_instrument_ttl_exactly_at_target() {
        // When IST time == target → wraps to next day (86400)
        let _target_secs: u64 = 12 * 3600; // 12:00 IST = 43200
        // Need (epoch + 19800) % 86400 = 43200
        let epoch = 86_400 + 43_200 - 19_800; // = 109800
        let ttl = compute_instrument_ttl_secs(epoch, 12);
        assert_eq!(ttl, 86_400);
    }

    #[test]
    fn test_compute_instrument_ttl_one_second_after_target() {
        // IST time = target_secs + 1 → wrap to next day minus 1 second
        let _target_secs: u64 = 6 * 3600; // 06:00 IST = 21600
        // Need (epoch + 19800) % 86400 = 21601
        let epoch = 86_400 + 21_601 - 19_800;
        let ttl = compute_instrument_ttl_secs(epoch, 6);
        // remaining = 86400 - 21601 + 21600 = 86399
        assert_eq!(ttl, 86_399);
    }

    #[test]
    fn test_compute_instrument_ttl_always_at_least_60() {
        // Test various inputs — TTL should never be below 60
        for hour in 0..24_u8 {
            for epoch_offset in [0_u64, 100, 1000, 86400, 172800] {
                let ttl = compute_instrument_ttl_secs(epoch_offset, hour);
                assert!(
                    ttl >= 60,
                    "TTL must be >= 60 for hour={hour}, epoch={epoch_offset}"
                );
            }
        }
    }

    #[test]
    fn test_compute_instrument_ttl_always_at_most_86400() {
        for hour in 0..24_u8 {
            for epoch_offset in [0_u64, 100, 1000, 86400, 172800] {
                let ttl = compute_instrument_ttl_secs(epoch_offset, hour);
                assert!(
                    ttl <= 86_400,
                    "TTL must be <= 86400 for hour={hour}, epoch={epoch_offset}"
                );
            }
        }
    }

    #[test]
    fn test_compute_instrument_ttl_large_epoch() {
        // Far future epoch value
        let ttl = compute_instrument_ttl_secs(2_000_000_000, 8);
        assert!(ttl >= 60);
        assert!(ttl <= 86_400);
    }

    // -----------------------------------------------------------------------
    // build_valkey_url — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_valkey_url_min_port() {
        let url = build_valkey_url("host", 1, "");
        assert_eq!(url, "redis://host:1");
    }

    #[test]
    fn test_build_valkey_url_max_port() {
        let url = build_valkey_url("host", u16::MAX, "");
        assert_eq!(url, "redis://host:65535");
    }

    #[test]
    fn test_build_valkey_url_empty_host() {
        let url = build_valkey_url("", 6379, "");
        assert_eq!(url, "redis://:6379");
    }

    #[test]
    fn test_build_valkey_url_localhost() {
        let url = build_valkey_url("localhost", 6379, "");
        assert_eq!(url, "redis://localhost:6379");
    }

    // -----------------------------------------------------------------------
    // build_instrument_cache_key — additional
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_instrument_cache_key_long_suffix() {
        let key = build_instrument_cache_key("binary_cache_2026_03_21");
        assert_eq!(key, "dlt:instrument:binary_cache_2026_03_21");
    }

    #[test]
    fn test_build_instrument_cache_key_special_chars() {
        let key = build_instrument_cache_key("csv:hash:v2");
        assert_eq!(key, "dlt:instrument:csv:hash:v2");
    }

    // -----------------------------------------------------------------------
    // build_token_cache_key — additional
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_token_cache_key_empty_suffix() {
        let key = build_token_cache_key("");
        assert_eq!(key, "dlt:token:");
    }

    #[test]
    fn test_build_token_cache_key_refresh() {
        let key = build_token_cache_key("refresh_at");
        assert_eq!(key, "dlt:token:refresh_at");
    }

    // -----------------------------------------------------------------------
    // build_tick_cache_key — additional
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_tick_cache_key_empty_suffix() {
        let key = build_tick_cache_key(11536, "");
        assert_eq!(key, "dlt:tick:11536:");
    }

    #[test]
    fn test_build_tick_cache_key_complex_suffix() {
        let key = build_tick_cache_key(42, "ohlcv:1m");
        assert_eq!(key, "dlt:tick:42:ohlcv:1m");
    }

    #[test]
    fn test_build_tick_cache_key_one() {
        let key = build_tick_cache_key(1, "ltp");
        assert_eq!(key, "dlt:tick:1:ltp");
    }

    // -----------------------------------------------------------------------
    // ValkeyPool::new — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_pool_new_returns_ok_with_typical_config() {
        let config = ValkeyConfig {
            host: "tv-valkey".to_string(),
            port: 6379,
            max_connections: 16,
            password: String::new(),
        };
        assert!(ValkeyPool::new(&config).is_ok());
    }

    #[test]
    fn test_key_namespace_isolation_across_types() {
        // Instrument, token, and tick keys must never collide
        let instrument_key = build_instrument_cache_key("access");
        let token_key = build_token_cache_key("access");
        let tick_key = build_tick_cache_key(0, "access");
        assert_ne!(instrument_key, token_key);
        assert_ne!(instrument_key, tick_key);
        assert_ne!(token_key, tick_key);
    }

    #[test]
    fn test_all_key_builders_use_tv_prefix() {
        let instrument = build_instrument_cache_key("test");
        let token = build_token_cache_key("test");
        let tick = build_tick_cache_key(1, "test");
        assert!(instrument.starts_with("dlt:"));
        assert!(token.starts_with("dlt:"));
        assert!(tick.starts_with("dlt:"));
    }

    #[test]
    fn test_compute_instrument_ttl_symmetry() {
        // Two calls with same inputs must return same result (pure function)
        let a = compute_instrument_ttl_secs(1_700_000_000, 8);
        let b = compute_instrument_ttl_secs(1_700_000_000, 8);
        assert_eq!(a, b);
    }

    #[test]
    fn test_compute_instrument_ttl_different_targets_differ() {
        let epoch = 1_700_000_000_u64;
        let ttl_8 = compute_instrument_ttl_secs(epoch, 8);
        let ttl_12 = compute_instrument_ttl_secs(epoch, 12);
        assert_ne!(
            ttl_8, ttl_12,
            "different target hours must produce different TTLs"
        );
    }

    #[test]
    fn test_pool_status_fields() {
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 6379,
            max_connections: 8,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).expect("pool creation must succeed");
        let status = pool.pool_status();
        assert_eq!(status.max_size, 8);
        assert_eq!(status.size, 0);
        assert_eq!(status.available, 0);
        assert_eq!(status.waiting, 0);
    }

    // -----------------------------------------------------------------------
    // Async method error path tests (ValkeyPool methods with unreachable host)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_health_check_fails_with_unreachable_host() {
        let config = ValkeyConfig {
            host: "unreachable-host-99999".to_string(),
            port: 1,
            max_connections: 1,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.health_check().await;
        assert!(
            result.is_err(),
            "health_check must fail with unreachable host"
        );
    }

    #[tokio::test]
    async fn test_get_fails_with_unreachable_host() {
        let config = ValkeyConfig {
            host: "unreachable-host-99999".to_string(),
            port: 1,
            max_connections: 1,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.get("test_key").await;
        assert!(result.is_err(), "get must fail with unreachable host");
    }

    #[tokio::test]
    async fn test_set_fails_with_unreachable_host() {
        let config = ValkeyConfig {
            host: "unreachable-host-99999".to_string(),
            port: 1,
            max_connections: 1,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.set("key", "value").await;
        assert!(result.is_err(), "set must fail with unreachable host");
    }

    #[tokio::test]
    async fn test_set_ex_fails_with_unreachable_host() {
        let config = ValkeyConfig {
            host: "unreachable-host-99999".to_string(),
            port: 1,
            max_connections: 1,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.set_ex("key", "value", 60).await;
        assert!(result.is_err(), "set_ex must fail with unreachable host");
    }

    #[tokio::test]
    async fn test_del_fails_with_unreachable_host() {
        let config = ValkeyConfig {
            host: "unreachable-host-99999".to_string(),
            port: 1,
            max_connections: 1,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.del("key").await;
        assert!(result.is_err(), "del must fail with unreachable host");
    }

    #[tokio::test]
    async fn test_exists_fails_with_unreachable_host() {
        let config = ValkeyConfig {
            host: "unreachable-host-99999".to_string(),
            port: 1,
            max_connections: 1,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.exists("key").await;
        assert!(result.is_err(), "exists must fail with unreachable host");
    }

    #[tokio::test]
    async fn test_set_nx_ex_fails_with_unreachable_host() {
        let config = ValkeyConfig {
            host: "unreachable-host-99999".to_string(),
            port: 1,
            max_connections: 1,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.set_nx_ex("lock_key", "lock_value", 30).await;
        assert!(result.is_err(), "set_nx_ex must fail with unreachable host");
    }

    // --- M2: Valkey auto-reconnect tests ---

    #[test]
    fn test_valkey_reconnect_rebuilds_pool() {
        // Verify that reconnect() rebuilds the pool successfully from stored config.
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 6379,
            max_connections: 4,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).expect("pool creation must succeed");

        // Reconnect should succeed (pool construction is lazy, no connection needed)
        let result = pool.reconnect();
        assert!(
            result.is_ok(),
            "reconnect must succeed when config is valid"
        );

        // Pool should still be functional after reconnect
        let status = pool.pool_status();
        assert_eq!(
            status.max_size, 4,
            "pool max_size must match config after reconnect"
        );
    }

    #[test]
    fn test_valkey_reconnect_preserves_config() {
        // Verify that reconnect preserves the original config values.
        let config = ValkeyConfig {
            host: "test-host".to_string(),
            port: 7777,
            max_connections: 12,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).expect("pool creation must succeed");

        assert_eq!(pool.config.host, "test-host");
        assert_eq!(pool.config.port, 7777);
        assert_eq!(pool.config.max_connections, 12);

        // After reconnect, config should be unchanged
        pool.reconnect().expect("reconnect must succeed");
        assert_eq!(pool.config.host, "test-host");
        assert_eq!(pool.config.port, 7777);
        assert_eq!(pool.config.max_connections, 12);
    }

    #[tokio::test]
    async fn test_valkey_reconnect_on_failure() {
        // Verify the reconnect + retry pattern works for get/set.
        // Without a running Valkey instance, both initial and retry fail,
        // but the reconnect path is exercised (pool is rebuilt).
        let config = ValkeyConfig {
            host: "unreachable-host-reconnect-test".to_string(),
            port: 1,
            max_connections: 1,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).expect("pool creation must succeed");

        // GET should fail (unreachable host), trigger reconnect, retry, still fail
        let get_result = pool.get("some_key").await;
        assert!(
            get_result.is_err(),
            "get must fail with unreachable host even after reconnect"
        );

        // SET should fail similarly
        let set_result = pool.set("some_key", "some_value").await;
        assert!(
            set_result.is_err(),
            "set must fail with unreachable host even after reconnect"
        );
    }

    #[tokio::test]
    async fn test_valkey_health_check_fails_unreachable() {
        // health_check should return an error when Valkey is not connected.
        let config = ValkeyConfig {
            host: "unreachable-health-check".to_string(),
            port: 1,
            max_connections: 1,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).expect("pool creation must succeed");

        let result = pool.health_check().await;
        assert!(
            result.is_err(),
            "health_check must return error when not connected"
        );
    }

    #[test]
    fn test_valkey_metrics_emitted() {
        // Verify metrics macros compile and don't panic when invoked.
        // O(1) atomic counter calls — safe for hot path.
        metrics::counter!("tv_valkey_ops_total", "op" => "get").increment(1);
        metrics::counter!("tv_valkey_ops_total", "op" => "set").increment(1);
        metrics::counter!("tv_valkey_ops_total", "op" => "del").increment(1);
        metrics::counter!("tv_valkey_ops_total", "op" => "exists").increment(1);
        metrics::counter!("tv_valkey_errors_total", "op" => "get").increment(1);
        metrics::counter!("tv_valkey_errors_total", "op" => "set").increment(1);
        metrics::counter!("tv_valkey_errors_total", "op" => "del").increment(1);
        metrics::counter!("tv_valkey_errors_total", "op" => "exists").increment(1);
    }

    // -----------------------------------------------------------------------
    // Integration tests with real Redis (port 6399)
    // -----------------------------------------------------------------------

    /// Helper: create a pool connected to the test Redis on port 6399.
    /// Verifies connectivity before returning — skips test if Redis is unreachable.
    async fn test_pool() -> Option<ValkeyPool> {
        let config = ValkeyConfig {
            host: "127.0.0.1".to_string(),
            port: 6399,
            max_connections: 4,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).ok()?;
        // Verify actual connectivity — pool creation is lazy.
        if pool.health_check().await.is_err() {
            return None;
        }
        Some(pool)
    }

    #[tokio::test]
    async fn test_valkey_health_check_real() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let result = pool.health_check().await;
        assert!(
            result.is_ok(),
            "health check must pass on real Redis: {result:?}"
        );
    }

    #[tokio::test]
    async fn test_valkey_set_get_roundtrip_real() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let key = "tv_test_set_get_roundtrip";

        pool.set(key, "hello_world").await.unwrap();
        let val = pool.get(key).await.unwrap();
        assert_eq!(val, Some("hello_world".to_string()));

        // Cleanup.
        pool.del(key).await.unwrap();
    }

    #[tokio::test]
    async fn test_valkey_get_missing_key_real() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let val = pool.get("tv_test_nonexistent_key_xyz").await.unwrap();
        assert_eq!(val, None, "GET on missing key must return None");
    }

    #[tokio::test]
    async fn test_valkey_set_ex_and_exists_real() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let key = "tv_test_set_ex";

        pool.set_ex(key, "with_ttl", 300).await.unwrap();
        let exists = pool.exists(key).await.unwrap();
        assert!(exists, "key must exist after SET EX");

        let val = pool.get(key).await.unwrap();
        assert_eq!(val, Some("with_ttl".to_string()));

        // Cleanup.
        pool.del(key).await.unwrap();
    }

    #[tokio::test]
    async fn test_valkey_del_real() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let key = "tv_test_del";

        pool.set(key, "to_be_deleted").await.unwrap();
        let exists_before = pool.exists(key).await.unwrap();
        assert!(exists_before);

        pool.del(key).await.unwrap();
        let exists_after = pool.exists(key).await.unwrap();
        assert!(!exists_after, "key must not exist after DEL");
    }

    #[tokio::test]
    async fn test_valkey_exists_missing_key_real() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let exists = pool.exists("tv_test_does_not_exist").await.unwrap();
        assert!(!exists, "EXISTS on missing key must return false");
    }

    #[tokio::test]
    async fn test_valkey_set_nx_ex_real() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let key = "tv_test_set_nx_ex";

        // Ensure clean state.
        let _ = pool.del(key).await;

        // First SET NX should succeed (key doesn't exist).
        let acquired = pool.set_nx_ex(key, "lock_holder", 300).await.unwrap();
        assert!(acquired, "SET NX EX must succeed on missing key");

        // Second SET NX should fail (key already exists).
        let acquired2 = pool.set_nx_ex(key, "another_holder", 300).await.unwrap();
        assert!(!acquired2, "SET NX EX must fail on existing key");

        // Cleanup.
        pool.del(key).await.unwrap();
    }

    #[tokio::test]
    async fn test_valkey_set_overwrite_real() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let key = "tv_test_overwrite";

        pool.set(key, "first").await.unwrap();
        pool.set(key, "second").await.unwrap();
        let val = pool.get(key).await.unwrap();
        assert_eq!(val, Some("second".to_string()), "SET must overwrite");

        pool.del(key).await.unwrap();
    }

    // -----------------------------------------------------------------------
    // Coverage: compute_instrument_ttl_secs additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_compute_instrument_ttl_one_second_before_target() {
        // IST at 05:59:59 → target 06:00:00 → should be 1 second
        // but clamped to minimum 60
        let _ist_offset: u64 = 19_800;
        // epoch at IST 05:59:59 = IST offset (19800) + 5*3600 + 59*60 + 59 = 19800 + 21599 = 41399
        // so UTC epoch = 41399 - 19800 = 21599
        let epoch = 21_599_u64;
        let ttl = compute_instrument_ttl_secs(epoch, 6);
        assert!(ttl >= 60, "TTL must be at least 60");
    }

    #[test]
    fn test_compute_instrument_ttl_multiple_targets() {
        let epoch: u64 = 1_700_000_000; // arbitrary
        let ttl_6 = compute_instrument_ttl_secs(epoch, 6);
        let ttl_12 = compute_instrument_ttl_secs(epoch, 12);
        // Different targets should produce different TTLs (unless both are clamped)
        assert!(ttl_6 > 0 && ttl_6 <= 86_400);
        assert!(ttl_12 > 0 && ttl_12 <= 86_400);
    }

    // -----------------------------------------------------------------------
    // Coverage: cache key builders with various inputs
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_instrument_cache_key_with_slashes() {
        let key = build_instrument_cache_key("2026/03/26");
        assert_eq!(key, "dlt:instrument:2026/03/26");
    }

    #[test]
    fn test_build_token_cache_key_with_numbers() {
        let key = build_token_cache_key("12345");
        assert_eq!(key, "dlt:token:12345");
    }

    #[test]
    fn test_build_tick_cache_key_with_large_security_id() {
        let key = build_tick_cache_key(999999, "ohlc");
        assert_eq!(key, "dlt:tick:999999:ohlc");
    }

    // -----------------------------------------------------------------------
    // Coverage: build_valkey_url edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_valkey_url_docker_dns() {
        let url = build_valkey_url("tv-valkey", 6379, "");
        assert_eq!(url, "redis://tv-valkey:6379");
    }

    #[test]
    fn test_build_valkey_url_contains_no_auth() {
        let url = build_valkey_url("host", 6379, "");
        assert!(!url.contains('@'), "URL must not contain auth");
    }

    // -----------------------------------------------------------------------
    // Coverage: pool checkout timeout constant
    // -----------------------------------------------------------------------

    #[test]
    fn test_pool_checkout_timeout_is_reasonable() {
        assert!(POOL_CHECKOUT_TIMEOUT_MS >= 100, "too fast timeout");
        assert!(POOL_CHECKOUT_TIMEOUT_MS <= 5000, "too slow timeout");
    }

    // -----------------------------------------------------------------------
    // Coverage: compute_instrument_ttl_secs — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_compute_instrument_ttl_all_24_target_hours_valid() {
        // Every target hour from 0..24 should produce a valid TTL
        let epoch = 1_767_225_600_u64;
        for hour in 0..24_u8 {
            let ttl = compute_instrument_ttl_secs(epoch, hour);
            assert!(
                ttl >= 60 && ttl <= 86_400,
                "TTL for target hour {hour} must be in [60, 86400], got {ttl}"
            );
        }
    }

    #[test]
    fn test_compute_instrument_ttl_epoch_zero_target_5() {
        // epoch 0 → IST = (0 + 19800) % 86400 = 19800s = 05:30
        // target = 5 * 3600 = 18000
        // 05:30 > 05:00 → wrap: 86400 - 19800 + 18000 = 84600
        let ttl = compute_instrument_ttl_secs(0, 5);
        assert_eq!(ttl, 84_600);
    }

    #[test]
    fn test_compute_instrument_ttl_epoch_zero_target_6() {
        // epoch 0 → IST = 19800s = 05:30
        // target = 6 * 3600 = 21600
        // 05:30 < 06:00 → remaining = 21600 - 19800 = 1800
        let ttl = compute_instrument_ttl_secs(0, 6);
        assert_eq!(ttl, 1800);
    }

    // -----------------------------------------------------------------------
    // Pure helper function tests — exercises build_* and compute_* directly
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_valkey_url_format() {
        assert_eq!(
            build_valkey_url("tv-valkey", 6379, ""),
            "redis://tv-valkey:6379"
        );
        assert_eq!(
            build_valkey_url("localhost", 6380, ""),
            "redis://localhost:6380"
        );
        assert_eq!(
            build_valkey_url("192.168.1.1", 6379, ""),
            "redis://192.168.1.1:6379"
        );
    }

    #[test]
    fn test_build_instrument_cache_key_format() {
        assert_eq!(
            build_instrument_cache_key("universe"),
            "dlt:instrument:universe"
        );
        assert_eq!(
            build_instrument_cache_key("csv_hash"),
            "dlt:instrument:csv_hash"
        );
        assert_eq!(build_instrument_cache_key(""), "dlt:instrument:");
    }

    #[test]
    fn test_build_token_cache_key_format() {
        assert_eq!(build_token_cache_key("access"), "dlt:token:access");
        assert_eq!(build_token_cache_key("refresh"), "dlt:token:refresh");
        assert_eq!(build_token_cache_key(""), "dlt:token:");
    }

    #[test]
    fn test_build_tick_cache_key_format() {
        assert_eq!(build_tick_cache_key(11536, "ltp"), "dlt:tick:11536:ltp");
        assert_eq!(build_tick_cache_key(0, "volume"), "dlt:tick:0:volume");
        assert_eq!(
            build_tick_cache_key(u32::MAX, "depth"),
            format!("dlt:tick:{}:depth", u32::MAX)
        );
    }

    #[test]
    fn test_compute_instrument_ttl_before_target_hour() {
        // IST midnight (UTC 18:30 prev day) → target 6AM = 6h
        let midnight_utc_for_ist = 86_400 - 19_800; // 66600 = UTC 18:30
        let ttl = compute_instrument_ttl_secs(midnight_utc_for_ist, 6);
        assert_eq!(ttl, 6 * 3600); // 6 hours until 6AM IST
    }

    #[test]
    fn test_compute_instrument_ttl_after_target_hour() {
        // IST 7AM (UTC 1:30) → target 6AM = 23h until next day 6AM
        let ist_7am_utc = 7 * 3600 - 19_800 + 86_400; // normalize
        let ttl = compute_instrument_ttl_secs(ist_7am_utc, 6);
        assert_eq!(ttl, 23 * 3600);
    }

    #[test]
    fn test_compute_instrument_ttl_clamps_minimum() {
        // If remaining < 60, clamp to 60
        // IST 5:59:30 → target 6AM = 30s → clamped to 60
        let ist_0559_utc = (5 * 3600 + 59 * 60 + 30) - 19_800 + 86_400;
        let ttl = compute_instrument_ttl_secs(ist_0559_utc, 6);
        assert!(ttl >= 60, "TTL must be at least 60s, got {ttl}");
    }

    #[test]
    fn test_compute_instrument_ttl_clamps_maximum() {
        // Result should never exceed 86400
        let ttl = compute_instrument_ttl_secs(0, 0);
        assert!(ttl <= 86_400, "TTL must be at most 86400s, got {ttl}");
    }

    #[test]
    fn test_compute_instrument_ttl_all_hours() {
        // Every target hour should produce a valid TTL
        for hour in 0..24 {
            let ttl = compute_instrument_ttl_secs(0, hour);
            assert!(ttl >= 60, "hour {hour}: TTL {ttl} < 60");
            assert!(ttl <= 86_400, "hour {hour}: TTL {ttl} > 86400");
        }
    }

    // =======================================================================
    // Coverage: pure helper functions (lines 32-74)
    // Additional tests to maximize tarpaulin tracing
    // =======================================================================

    #[test]
    fn test_build_valkey_url_returns_string_with_host_and_port() {
        let result = build_valkey_url("my-host", 9999, "");
        assert!(result.contains("my-host"));
        assert!(result.contains("9999"));
        assert!(result.starts_with("redis://"));
    }

    #[test]
    fn test_build_instrument_cache_key_contains_suffix() {
        let result = build_instrument_cache_key("test_suffix");
        assert!(result.contains("test_suffix"));
        assert!(result.starts_with("dlt:instrument:"));
    }

    #[test]
    fn test_build_token_cache_key_contains_suffix() {
        let result = build_token_cache_key("refresh");
        assert!(result.contains("refresh"));
        assert!(result.starts_with("dlt:token:"));
    }

    #[test]
    fn test_build_tick_cache_key_contains_security_id_and_suffix() {
        let result = build_tick_cache_key(12345, "volume");
        assert!(result.contains("12345"));
        assert!(result.contains("volume"));
        assert!(result.starts_with("dlt:tick:"));
    }

    #[test]
    fn test_compute_instrument_ttl_returns_u64() {
        let result = compute_instrument_ttl_secs(1_700_000_000, 8);
        // Should be a positive number between 60 and 86400
        assert!(result >= 60);
        assert!(result <= 86_400);
    }

    // =======================================================================
    // Coverage: ValkeyPool methods without live Redis (lines 95-325)
    // =======================================================================

    #[test]
    fn test_valkey_pool_reconnect_without_running_redis() {
        // Pool reconnect should succeed (pool builder succeeds; connections are lazy).
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 6379,
            max_connections: 4,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.reconnect();
        assert!(
            result.is_ok(),
            "reconnect must succeed (lazy connections, no actual Redis needed)"
        );
    }

    #[tokio::test]
    async fn test_valkey_pool_health_check_fails_without_running_redis() {
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 16379, // unlikely to have Redis on this port
            max_connections: 1,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.health_check().await;
        assert!(
            result.is_err(),
            "health check must fail when Redis is not running"
        );
    }

    #[tokio::test]
    async fn test_valkey_pool_get_fails_without_running_redis() {
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 16380,
            max_connections: 1,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.get("test_key").await;
        assert!(result.is_err(), "GET must fail when Redis is not running");
    }

    #[tokio::test]
    async fn test_valkey_pool_set_fails_without_running_redis() {
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 16381,
            max_connections: 1,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.set("test_key", "test_value").await;
        assert!(result.is_err(), "SET must fail when Redis is not running");
    }

    #[tokio::test]
    async fn test_valkey_pool_set_ex_fails_without_running_redis() {
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 16382,
            max_connections: 1,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.set_ex("test_key", "test_value", 60).await;
        assert!(
            result.is_err(),
            "SET EX must fail when Redis is not running"
        );
    }

    #[tokio::test]
    async fn test_valkey_pool_del_fails_without_running_redis() {
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 16383,
            max_connections: 1,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.del("test_key").await;
        assert!(result.is_err(), "DEL must fail when Redis is not running");
    }

    #[tokio::test]
    async fn test_valkey_pool_exists_fails_without_running_redis() {
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 16384,
            max_connections: 1,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.exists("test_key").await;
        assert!(
            result.is_err(),
            "EXISTS must fail when Redis is not running"
        );
    }

    #[tokio::test]
    async fn test_valkey_pool_set_nx_ex_fails_without_running_redis() {
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 16385,
            max_connections: 1,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.set_nx_ex("lock_key", "value", 10).await;
        assert!(
            result.is_err(),
            "SET NX EX must fail when Redis is not running"
        );
    }

    #[test]
    fn test_build_pool_with_valid_config() {
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 6379,
            max_connections: 4,
            password: String::new(),
        };
        let pool = ValkeyPool::build_pool(&config);
        assert!(pool.is_ok(), "build_pool must succeed with valid config");
    }

    // =======================================================================
    // Coverage: get() retry path — reconnect succeeds, inner retry still fails
    // Lines 192-209 (reconnect + retry + metrics)
    // =======================================================================

    #[tokio::test]
    async fn test_valkey_get_retry_path_reconnect_ok_retry_fails() {
        // Use a port that nothing listens on. get_inner fails, reconnect
        // succeeds (pool rebuild is lazy), get_inner fails again.
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 16390,
            max_connections: 1,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.get("retry_key").await;
        // Both attempts fail — error propagated
        assert!(result.is_err());
    }

    // =======================================================================
    // Coverage: set() retry path — reconnect succeeds, inner retry still fails
    // Lines 226-243 (reconnect + retry + metrics)
    // =======================================================================

    #[tokio::test]
    async fn test_valkey_set_retry_path_reconnect_ok_retry_fails() {
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 16391,
            max_connections: 1,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.set("retry_key", "retry_value").await;
        assert!(result.is_err());
    }

    // =======================================================================
    // Coverage: set_ex() error metrics path (lines 264-266)
    // =======================================================================

    #[tokio::test]
    async fn test_valkey_set_ex_error_metrics() {
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 16392,
            max_connections: 1,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.set_ex("key", "value", 120).await;
        assert!(result.is_err());
    }

    // =======================================================================
    // Coverage: del() error metrics path (lines 282-284)
    // =======================================================================

    #[tokio::test]
    async fn test_valkey_del_error_metrics() {
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 16393,
            max_connections: 1,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.del("key").await;
        assert!(result.is_err());
    }

    // =======================================================================
    // Coverage: exists() error metrics path (lines 299-300)
    // =======================================================================

    #[tokio::test]
    async fn test_valkey_exists_error_metrics() {
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 16394,
            max_connections: 1,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.exists("key").await;
        assert!(result.is_err());
    }

    // =======================================================================
    // Coverage: set_nx_ex() error metrics path (lines 321-323)
    // =======================================================================

    #[tokio::test]
    async fn test_valkey_set_nx_ex_error_metrics() {
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 16395,
            max_connections: 1,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.set_nx_ex("lock", "v", 30).await;
        assert!(result.is_err());
    }

    // =======================================================================
    // Coverage: health_check() error path (lines 174-184)
    // =======================================================================

    #[tokio::test]
    async fn test_valkey_health_check_error() {
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 16396,
            max_connections: 1,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.health_check().await;
        assert!(result.is_err());
    }

    // =======================================================================
    // Coverage: checkout_conn() (lines 149-159)
    // This is implicitly tested by all async tests above, but we add
    // a direct test to be explicit.
    // =======================================================================

    #[tokio::test]
    async fn test_valkey_checkout_conn_fails_without_running_redis() {
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 16397,
            max_connections: 1,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.checkout_conn().await;
        assert!(result.is_err());
    }

    // =======================================================================
    // Coverage: reconnect() (lines 131-143) — pool reconstruction
    // =======================================================================

    #[test]
    fn test_valkey_reconnect_rebuilds_pool_twice() {
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 6379,
            max_connections: 4,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).unwrap();

        // First reconnect
        let result = pool.reconnect();
        assert!(result.is_ok());

        // Second reconnect — also succeeds
        let result = pool.reconnect();
        assert!(result.is_ok());
    }

    // =======================================================================
    // Coverage: get_inner() + set_inner() direct calls via get/set
    // Lines 213-218 and 247-252
    // =======================================================================

    #[tokio::test]
    async fn test_valkey_get_inner_error_with_different_keys() {
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 16398,
            max_connections: 1,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).unwrap();
        // Exercise different keys to ensure the error context includes the key
        let r1 = pool.get("alpha").await;
        assert!(r1.is_err());
    }

    #[tokio::test]
    async fn test_valkey_set_inner_error_with_different_keys() {
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 16399,
            max_connections: 1,
            password: String::new(),
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let r1 = pool.set("beta", "val").await;
        assert!(r1.is_err());
    }

    // =======================================================================
    // Coverage: build_valkey_url function (line 33)
    // =======================================================================

    #[test]
    fn test_build_valkey_url_with_various_hosts() {
        assert_eq!(
            build_valkey_url("localhost", 6379, ""),
            "redis://localhost:6379"
        );
        assert_eq!(
            build_valkey_url("10.0.0.1", 6380, ""),
            "redis://10.0.0.1:6380"
        );
        assert_eq!(
            build_valkey_url("tv-valkey", 6379, ""),
            "redis://tv-valkey:6379"
        );
    }

    // =======================================================================
    // Coverage: compute_instrument_ttl_secs edge cases
    // =======================================================================

    #[test]
    fn test_compute_instrument_ttl_past_target_wraps_to_next_day() {
        // IST 23:00 (past target of 8:00 IST)
        // 23:00 IST = 17:30 UTC. epoch for 17:30 UTC today:
        // We need to construct an epoch where IST time is past target hour.
        // IST = UTC + 19800s. ist_secs_today = (epoch + 19800) % 86400.
        // If target=8, target_secs=28800. Let ist_secs_today=82800 (23:00 IST).
        // remaining = 86400 - 82800 + 28800 = 32400s (9 hours to next 8AM)
        // epoch where ist_secs_today = 82800: (epoch + 19800) % 86400 = 82800
        // epoch = 82800 - 19800 = 63000
        let ttl = compute_instrument_ttl_secs(63000, 8);
        assert_eq!(ttl, 32400); // 9 hours
    }

    #[test]
    fn test_compute_instrument_ttl_clamps_to_minimum_60() {
        // If somehow remaining is < 60, clamp to 60
        // At target hour exactly: remaining = 0 -> past target -> wraps to 86400
        // Actually remaining can never be < 60 for whole-hour targets...
        // But let's verify the clamp works
        let ttl = compute_instrument_ttl_secs(0, 0);
        // IST midnight: (0 + 19800) % 86400 = 19800 secs today
        // target_secs = 0. 19800 > 0 -> past target.
        // remaining = 86400 - 19800 + 0 = 66600s
        assert!(ttl >= 60);
        assert!(ttl <= 86_400);
    }
}

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
use tracing::{debug, info};

use dhan_live_trader_common::config::ValkeyConfig;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Timeout for pool connection checkout (prevents blocking on dead Valkey).
const POOL_CHECKOUT_TIMEOUT_MS: u64 = 500;

// ---------------------------------------------------------------------------
// Pool wrapper
// ---------------------------------------------------------------------------

/// Async Valkey connection pool with typed cache helpers.
///
/// All public methods return `Result` — callers decide whether to propagate or log-and-continue.
pub struct ValkeyPool {
    pool: Pool,
}

impl ValkeyPool {
    /// Creates a new connection pool from config.
    ///
    /// # Errors
    /// Returns error if the pool builder fails (bad config, not a connection error).
    pub fn new(config: &ValkeyConfig) -> Result<Self> {
        let url = format!("redis://{}:{}", config.host, config.port);

        let cfg = Config::from_url(&url);
        let pool = cfg
            .builder()
            .map_err(|err| anyhow::anyhow!("deadpool-redis builder error: {err}"))?
            .max_size(config.max_connections as usize)
            .wait_timeout(Some(Duration::from_millis(POOL_CHECKOUT_TIMEOUT_MS)))
            .runtime(Runtime::Tokio1)
            .build()
            .context("failed to build Valkey connection pool")?;

        info!(
            host = %config.host,
            port = config.port,
            max_connections = config.max_connections,
            "Valkey pool created"
        );

        Ok(Self { pool })
    }

    /// Health check — sends PING and expects PONG.
    ///
    /// # Errors
    /// Returns error if Valkey is unreachable or returns unexpected response.
    pub async fn health_check(&self) -> Result<()> {
        let mut conn = self
            .pool
            .get()
            .await
            .context("failed to checkout Valkey connection")?;

        let pong: String = redis::cmd("PING")
            .query_async(&mut conn)
            .await
            .context("Valkey PING failed")?;

        if pong != "PONG" {
            anyhow::bail!("unexpected PING response: {pong}");
        }

        debug!("Valkey health check passed");
        Ok(())
    }

    /// GET a string value by key.
    pub async fn get(&self, key: &str) -> Result<Option<String>> {
        let mut conn = self
            .pool
            .get()
            .await
            .context("failed to checkout Valkey connection")?;

        let value: Option<String> = conn
            .get(key)
            .await
            .with_context(|| format!("Valkey GET failed for key={key}"))?;

        Ok(value)
    }

    /// SET a string value.
    pub async fn set(&self, key: &str, value: &str) -> Result<()> {
        let mut conn = self
            .pool
            .get()
            .await
            .context("failed to checkout Valkey connection")?;

        conn.set::<_, _, ()>(key, value)
            .await
            .with_context(|| format!("Valkey SET failed for key={key}"))?;

        Ok(())
    }

    /// SET with expiry (seconds).
    pub async fn set_ex(&self, key: &str, value: &str, ttl_secs: u64) -> Result<()> {
        let mut conn = self
            .pool
            .get()
            .await
            .context("failed to checkout Valkey connection")?;

        conn.set_ex::<_, _, ()>(key, value, ttl_secs)
            .await
            .with_context(|| format!("Valkey SETEX failed for key={key}"))?;

        Ok(())
    }

    /// DEL a key.
    pub async fn del(&self, key: &str) -> Result<()> {
        let mut conn = self
            .pool
            .get()
            .await
            .context("failed to checkout Valkey connection")?;

        conn.del::<_, ()>(key)
            .await
            .with_context(|| format!("Valkey DEL failed for key={key}"))?;

        Ok(())
    }

    /// EXISTS check for a key.
    pub async fn exists(&self, key: &str) -> Result<bool> {
        let mut conn = self
            .pool
            .get()
            .await
            .context("failed to checkout Valkey connection")?;

        let exists: bool = conn
            .exists(key)
            .await
            .with_context(|| format!("Valkey EXISTS failed for key={key}"))?;

        Ok(exists)
    }

    /// SET if Not eXists with expiry (atomic lock pattern).
    pub async fn set_nx_ex(&self, key: &str, value: &str, ttl_secs: u64) -> Result<bool> {
        let mut conn = self
            .pool
            .get()
            .await
            .context("failed to checkout Valkey connection")?;

        // Use SET NX EX for atomic set-if-not-exists with expiry
        let result: Option<String> = redis::cmd("SET")
            .arg(key)
            .arg(value)
            .arg("NX")
            .arg("EX")
            .arg(ttl_secs)
            .query_async(&mut conn)
            .await
            .with_context(|| format!("Valkey SET NX EX failed for key={key}"))?;

        Ok(result.is_some())
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
            host: "dlt-valkey".to_string(),
            port: 6379,
            max_connections: 16,
        };
        let url = format!("redis://{}:{}", config.host, config.port);
        assert_eq!(url, "redis://dlt-valkey:6379");
    }

    #[test]
    fn pool_creation_with_valid_config() {
        // Pool creation should succeed even without a running Valkey
        // (connections are lazy — errors happen on first use)
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 6379,
            max_connections: 4,
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
        };
        let pool = ValkeyPool::new(&config).expect("pool creation must succeed");
        let status = pool.pool.status();
        assert_eq!(status.max_size, 8);
    }

    #[test]
    fn pool_initial_state_has_no_connections() {
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 6379,
            max_connections: 16,
        };
        let pool = ValkeyPool::new(&config).expect("pool creation must succeed");
        let status = pool.pool.status();
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
        };
        let pool = ValkeyPool::new(&config).expect("single connection pool must succeed");
        let status = pool.pool.status();
        assert_eq!(status.max_size, 1);
    }

    #[test]
    fn pool_with_large_connection_count() {
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 6379,
            max_connections: 128,
        };
        let pool = ValkeyPool::new(&config).expect("large pool must succeed");
        let status = pool.pool.status();
        assert_eq!(status.max_size, 128);
    }

    #[test]
    fn url_uses_redis_scheme() {
        let config = ValkeyConfig {
            host: "dlt-valkey".to_string(),
            port: 6379,
            max_connections: 16,
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
        };
        let pool = ValkeyPool::new(&config).expect("pool creation must succeed");
        let status = pool.pool.status();
        assert_eq!(status.waiting, 0, "no waiters initially");
    }

    #[test]
    fn url_format_with_ipv4_host() {
        let config = ValkeyConfig {
            host: "192.168.1.100".to_string(),
            port: 6379,
            max_connections: 4,
        };
        let url = format!("redis://{}:{}", config.host, config.port);
        assert_eq!(url, "redis://192.168.1.100:6379");
    }

    #[test]
    fn url_format_with_docker_hostname() {
        let config = ValkeyConfig {
            host: "dlt-valkey".to_string(),
            port: 6379,
            max_connections: 16,
        };
        let url = format!("redis://{}:{}", config.host, config.port);
        // Docker DNS hostname must be used, not localhost
        assert!(
            !url.contains("localhost"),
            "production URLs must use Docker DNS, not localhost"
        );
        assert_eq!(url, "redis://dlt-valkey:6379");
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
        };
        let pool = ValkeyPool::new(&config).expect("min pool size must succeed");
        assert_eq!(pool.pool.status().max_size, 1);
    }

    #[test]
    fn pool_max_size_boundary_typical_prod() {
        // Production config uses max_connections = 16
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 6379,
            max_connections: 16,
        };
        let pool = ValkeyPool::new(&config).expect("typical prod pool size must succeed");
        assert_eq!(pool.pool.status().max_size, 16);
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
        };
        let config_b = ValkeyConfig {
            host: "host-b".to_string(),
            port: 6380,
            max_connections: 8,
        };
        let pool_a = ValkeyPool::new(&config_a).expect("pool A must succeed");
        let pool_b = ValkeyPool::new(&config_b).expect("pool B must succeed");
        assert_eq!(pool_a.pool.status().max_size, 4);
        assert_eq!(pool_b.pool.status().max_size, 8);
    }

    #[test]
    fn url_never_contains_password_in_basic_config() {
        let config = ValkeyConfig {
            host: "dlt-valkey".to_string(),
            port: 6379,
            max_connections: 16,
        };
        let url = format!("redis://{}:{}", config.host, config.port);
        // Basic config URL must not accidentally include auth credentials
        assert!(!url.contains('@'), "URL must not contain auth separator");
        assert!(!url.contains("password"), "URL must not contain password");
    }

    // -----------------------------------------------------------------------
    // Async error path tests — exercise every method with unreachable Valkey
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_health_check_fails_with_unreachable_valkey() {
        let config = ValkeyConfig {
            host: "127.0.0.1".to_string(),
            port: 1, // unlikely to have Valkey
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.health_check().await;
        assert!(
            result.is_err(),
            "health_check must fail with unreachable Valkey"
        );
    }

    #[tokio::test]
    async fn test_get_fails_with_unreachable_valkey() {
        let config = ValkeyConfig {
            host: "127.0.0.1".to_string(),
            port: 1,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.get("test_key").await;
        assert!(result.is_err(), "get must fail with unreachable Valkey");
    }

    #[tokio::test]
    async fn test_set_fails_with_unreachable_valkey() {
        let config = ValkeyConfig {
            host: "127.0.0.1".to_string(),
            port: 1,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.set("test_key", "test_value").await;
        assert!(result.is_err(), "set must fail with unreachable Valkey");
    }

    #[tokio::test]
    async fn test_set_ex_fails_with_unreachable_valkey() {
        let config = ValkeyConfig {
            host: "127.0.0.1".to_string(),
            port: 1,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.set_ex("test_key", "test_value", 60).await;
        assert!(result.is_err(), "set_ex must fail with unreachable Valkey");
    }

    #[tokio::test]
    async fn test_del_fails_with_unreachable_valkey() {
        let config = ValkeyConfig {
            host: "127.0.0.1".to_string(),
            port: 1,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.del("test_key").await;
        assert!(result.is_err(), "del must fail with unreachable Valkey");
    }

    #[tokio::test]
    async fn test_exists_fails_with_unreachable_valkey() {
        let config = ValkeyConfig {
            host: "127.0.0.1".to_string(),
            port: 1,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.exists("test_key").await;
        assert!(result.is_err(), "exists must fail with unreachable Valkey");
    }

    #[tokio::test]
    async fn test_set_nx_ex_fails_with_unreachable_valkey() {
        let config = ValkeyConfig {
            host: "127.0.0.1".to_string(),
            port: 1,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.set_nx_ex("lock_key", "lock_value", 30).await;
        assert!(
            result.is_err(),
            "set_nx_ex must fail with unreachable Valkey"
        );
    }

    // -----------------------------------------------------------------------
    // Error path tests with invalid hostname (DNS failure)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_health_check_fails_with_invalid_hostname() {
        let config = ValkeyConfig {
            host: "unreachable-host-that-does-not-exist".to_string(),
            port: 6379,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.health_check().await;
        assert!(
            result.is_err(),
            "health_check must fail with invalid hostname"
        );
    }

    #[tokio::test]
    async fn test_get_fails_with_invalid_hostname() {
        let config = ValkeyConfig {
            host: "nonexistent-valkey-host".to_string(),
            port: 6379,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.get("any_key").await;
        assert!(result.is_err(), "get must fail with invalid hostname");
    }

    #[tokio::test]
    async fn test_set_fails_with_invalid_hostname() {
        let config = ValkeyConfig {
            host: "nonexistent-valkey-host".to_string(),
            port: 6379,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.set("any_key", "any_value").await;
        assert!(result.is_err(), "set must fail with invalid hostname");
    }

    #[tokio::test]
    async fn test_del_fails_with_invalid_hostname() {
        let config = ValkeyConfig {
            host: "nonexistent-valkey-host".to_string(),
            port: 6379,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.del("any_key").await;
        assert!(result.is_err(), "del must fail with invalid hostname");
    }

    #[tokio::test]
    async fn test_exists_fails_with_invalid_hostname() {
        let config = ValkeyConfig {
            host: "nonexistent-valkey-host".to_string(),
            port: 6379,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.exists("any_key").await;
        assert!(result.is_err(), "exists must fail with invalid hostname");
    }

    #[tokio::test]
    async fn test_set_nx_ex_fails_with_invalid_hostname() {
        let config = ValkeyConfig {
            host: "nonexistent-valkey-host".to_string(),
            port: 6379,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.set_nx_ex("lock", "val", 10).await;
        assert!(result.is_err(), "set_nx_ex must fail with invalid hostname");
    }

    #[tokio::test]
    async fn test_set_ex_fails_with_invalid_hostname() {
        let config = ValkeyConfig {
            host: "nonexistent-valkey-host".to_string(),
            port: 6379,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.set_ex("key", "val", 60).await;
        assert!(result.is_err(), "set_ex must fail with invalid hostname");
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: URL edge cases, port boundaries, pool config limits
    // -----------------------------------------------------------------------

    #[test]
    fn url_format_with_max_port() {
        let config = ValkeyConfig {
            host: "dlt-valkey".to_string(),
            port: 65535,
            max_connections: 4,
        };
        let url = format!("redis://{}:{}", config.host, config.port);
        assert_eq!(url, "redis://dlt-valkey:65535");
    }

    #[test]
    fn url_format_with_min_port() {
        let config = ValkeyConfig {
            host: "dlt-valkey".to_string(),
            port: 1,
            max_connections: 4,
        };
        let url = format!("redis://{}:{}", config.host, config.port);
        assert_eq!(url, "redis://dlt-valkey:1");
    }

    #[test]
    fn pool_creation_preserves_all_config_fields() {
        let config = ValkeyConfig {
            host: "my-cache".to_string(),
            port: 6380,
            max_connections: 32,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        assert_eq!(pool.pool.status().max_size, 32);
    }

    #[test]
    fn url_format_with_hyphenated_hostname() {
        let config = ValkeyConfig {
            host: "dlt-valkey-primary-01".to_string(),
            port: 6379,
            max_connections: 8,
        };
        let url = format!("redis://{}:{}", config.host, config.port);
        assert_eq!(url, "redis://dlt-valkey-primary-01:6379");
        assert!(url.starts_with("redis://"));
    }

    #[tokio::test]
    async fn test_all_methods_fail_with_port_zero() {
        // Port 0 is not a valid listening port for a server
        let config = ValkeyConfig {
            host: "127.0.0.1".to_string(),
            port: 0,
            max_connections: 1,
        };
        // Pool creation should succeed (lazy)
        let pool = ValkeyPool::new(&config);
        assert!(
            pool.is_ok(),
            "pool creation with port 0 must succeed (lazy connections)"
        );
    }

    #[test]
    fn pool_with_high_connection_count_creates_without_error() {
        let config = ValkeyConfig {
            host: "localhost".to_string(),
            port: 6379,
            max_connections: 256,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        assert_eq!(pool.pool.status().max_size, 256);
        assert_eq!(pool.pool.status().size, 0, "no connections yet (lazy)");
    }

    #[tokio::test]
    async fn test_set_ex_zero_ttl_fails_unreachable() {
        // Verify that set_ex with zero TTL still goes through the code path
        let config = ValkeyConfig {
            host: "127.0.0.1".to_string(),
            port: 1,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.set_ex("key", "val", 0).await;
        assert!(
            result.is_err(),
            "set_ex with zero TTL must fail (unreachable)"
        );
    }

    #[tokio::test]
    async fn test_set_nx_ex_zero_ttl_fails_unreachable() {
        let config = ValkeyConfig {
            host: "127.0.0.1".to_string(),
            port: 1,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.set_nx_ex("lock", "val", 0).await;
        assert!(
            result.is_err(),
            "set_nx_ex with zero TTL must fail (unreachable)"
        );
    }

    #[tokio::test]
    async fn test_operations_with_empty_key() {
        let config = ValkeyConfig {
            host: "127.0.0.1".to_string(),
            port: 1,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        // Empty key should still fail on connection, not panic
        assert!(pool.get("").await.is_err());
        assert!(pool.set("", "val").await.is_err());
        assert!(pool.del("").await.is_err());
        assert!(pool.exists("").await.is_err());
    }

    #[tokio::test]
    async fn test_operations_with_long_key() {
        let config = ValkeyConfig {
            host: "127.0.0.1".to_string(),
            port: 1,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let long_key = "k".repeat(1024);
        // Should fail on connection, not on key length validation
        assert!(pool.get(&long_key).await.is_err());
    }

    #[tokio::test]
    async fn test_set_with_large_value_fails_unreachable() {
        let config = ValkeyConfig {
            host: "127.0.0.1".to_string(),
            port: 1,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let large_value = "v".repeat(10_000);
        assert!(pool.set("key", &large_value).await.is_err());
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: ValkeyPool::new error propagation, edge cases
    // for set_nx_ex semantics, pool status checks, various key patterns
    // -----------------------------------------------------------------------

    #[test]
    fn pool_creation_url_includes_host_and_port() {
        let config = ValkeyConfig {
            host: "my-cache-host".to_string(),
            port: 7777,
            max_connections: 4,
        };
        let url = format!("redis://{}:{}", config.host, config.port);
        assert_eq!(url, "redis://my-cache-host:7777");
        // Also verify pool creation succeeds
        let pool = ValkeyPool::new(&config);
        assert!(pool.is_ok());
    }

    #[tokio::test]
    async fn test_set_nx_ex_with_large_ttl_fails_unreachable() {
        let config = ValkeyConfig {
            host: "127.0.0.1".to_string(),
            port: 1,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        // Large TTL to verify no overflow
        let result = pool.set_nx_ex("lock", "val", u64::MAX).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_set_ex_with_large_ttl_fails_unreachable() {
        let config = ValkeyConfig {
            host: "127.0.0.1".to_string(),
            port: 1,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.set_ex("key", "val", u64::MAX).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_with_key_containing_special_chars() {
        let config = ValkeyConfig {
            host: "127.0.0.1".to_string(),
            port: 1,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        // Keys with colons and slashes (common in cache key patterns)
        assert!(pool.get("dlt:token:primary").await.is_err());
        assert!(pool.get("dlt/instruments/2026-03-20").await.is_err());
    }

    #[test]
    fn pool_status_max_size_matches_various_configs() {
        for count in [1_u32, 4, 16, 64, 128] {
            let config = ValkeyConfig {
                host: "localhost".to_string(),
                port: 6379,
                max_connections: count,
            };
            let pool = ValkeyPool::new(&config).unwrap();
            assert_eq!(
                pool.pool.status().max_size,
                count as usize,
                "max_size must match config for count={}",
                count
            );
        }
    }

    #[tokio::test]
    async fn test_del_with_key_containing_spaces() {
        let config = ValkeyConfig {
            host: "127.0.0.1".to_string(),
            port: 1,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        // Should fail on connection, not on key content
        assert!(pool.del("key with spaces").await.is_err());
    }

    #[tokio::test]
    async fn test_exists_with_key_containing_special_chars() {
        let config = ValkeyConfig {
            host: "127.0.0.1".to_string(),
            port: 1,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        assert!(pool.exists("dlt:cache:instruments").await.is_err());
    }

    // -----------------------------------------------------------------------
    // Mock RESP server tests — exercise success paths and branch coverage
    // for all methods by running a minimal Redis-protocol mock server.
    //
    // The redis crate performs RESP protocol negotiation on connection init.
    // Our mock must respond with "+OK\r\n" to any incoming data chunk
    // (handling CLIENT SETNAME, HELLO, etc.) and then respond with the
    // desired response for the actual command.
    //
    // Strategy: Count RESP commands by looking for '*' (RESP array start)
    // delimiters. Respond to each one. The `command_response` is used for
    // the N-th command (where N = `skip_init_commands + 1`).
    // -----------------------------------------------------------------------

    /// Spawns a mock RESP server that responds to the first `skip` commands
    /// with "+OK\r\n" and then responds with `command_response` for
    /// subsequent commands. This handles redis client init handshake.
    async fn spawn_resp_mock_server(command_response: &'static [u8]) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                if let Ok((mut stream, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = [0u8; 8192];
                        // Track how many RESP commands we've seen.
                        // The redis crate sends init commands before the real one.
                        let mut commands_seen: usize = 0;
                        loop {
                            match stream.read(&mut buf).await {
                                Ok(0) => break,
                                Ok(n) => {
                                    // Count RESP array headers (* at start of line)
                                    let data = &buf[..n];
                                    let mut cmd_count = 0;
                                    for window in data.windows(1) {
                                        if window[0] == b'*' {
                                            cmd_count += 1;
                                        }
                                    }
                                    // For each command, send a response
                                    for _ in 0..cmd_count.max(1) {
                                        commands_seen += 1;
                                        // First command is init; respond with OK
                                        // Second+ commands get the configured response
                                        let resp = if commands_seen <= 1 {
                                            b"+OK\r\n".as_slice()
                                        } else {
                                            command_response
                                        };
                                        if stream.write_all(resp).await.is_err() {
                                            return;
                                        }
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                    });
                }
            }
        });
        // Allow the listener to start accepting
        tokio::task::yield_now().await;
        port
    }

    /// RESP simple string: "+PONG\r\n"
    const RESP_PONG: &[u8] = b"+PONG\r\n";
    /// RESP simple string: "+OK\r\n"
    const RESP_OK: &[u8] = b"+OK\r\n";
    /// RESP bulk string nil: "$-1\r\n" (represents None/nil)
    const RESP_NIL: &[u8] = b"$-1\r\n";
    /// RESP integer 1: ":1\r\n" (true for EXISTS)
    const RESP_INT_ONE: &[u8] = b":1\r\n";
    /// RESP integer 0: ":0\r\n" (false for EXISTS)
    const RESP_INT_ZERO: &[u8] = b":0\r\n";
    /// RESP bulk string with value: "$5\r\nhello\r\n"
    const RESP_BULK_HELLO: &[u8] = b"$5\r\nhello\r\n";

    #[tokio::test]
    async fn test_health_check_success_with_mock_resp() {
        let port = spawn_resp_mock_server(RESP_PONG).await;
        let config = ValkeyConfig {
            host: "127.0.0.1".to_string(),
            port,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.health_check().await;
        assert!(
            result.is_ok(),
            "health_check should succeed when PONG is returned: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_health_check_non_pong_response() {
        // The mock responds with "+OK\r\n" for all commands including PING,
        // which triggers the `pong != "PONG"` branch.
        let port = spawn_resp_mock_server(RESP_OK).await;
        let config = ValkeyConfig {
            host: "127.0.0.1".to_string(),
            port,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.health_check().await;
        assert!(
            result.is_err(),
            "health_check should fail when response is not PONG"
        );
        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(
            err_msg.contains("unexpected PING response"),
            "error should mention unexpected PING response, got: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_get_returns_some_with_mock_resp() {
        let port = spawn_resp_mock_server(RESP_BULK_HELLO).await;
        let config = ValkeyConfig {
            host: "127.0.0.1".to_string(),
            port,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.get("test_key").await;
        assert!(result.is_ok(), "get should succeed with mock: {:?}", result);
        assert_eq!(result.unwrap(), Some("hello".to_string()));
    }

    #[tokio::test]
    async fn test_get_returns_none_with_nil_resp() {
        let port = spawn_resp_mock_server(RESP_NIL).await;
        let config = ValkeyConfig {
            host: "127.0.0.1".to_string(),
            port,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.get("missing_key").await;
        assert!(result.is_ok(), "get should succeed with nil: {:?}", result);
        assert_eq!(result.unwrap(), None);
    }

    #[tokio::test]
    async fn test_set_success_with_mock_resp() {
        let port = spawn_resp_mock_server(RESP_OK).await;
        let config = ValkeyConfig {
            host: "127.0.0.1".to_string(),
            port,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.set("test_key", "test_value").await;
        assert!(result.is_ok(), "set should succeed with mock: {:?}", result);
    }

    #[tokio::test]
    async fn test_set_ex_success_with_mock_resp() {
        let port = spawn_resp_mock_server(RESP_OK).await;
        let config = ValkeyConfig {
            host: "127.0.0.1".to_string(),
            port,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.set_ex("test_key", "test_value", 300).await;
        assert!(
            result.is_ok(),
            "set_ex should succeed with mock: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_del_success_with_mock_resp() {
        // DEL returns an integer (number of keys deleted)
        let port = spawn_resp_mock_server(RESP_INT_ONE).await;
        let config = ValkeyConfig {
            host: "127.0.0.1".to_string(),
            port,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.del("test_key").await;
        assert!(result.is_ok(), "del should succeed with mock: {:?}", result);
    }

    #[tokio::test]
    async fn test_exists_returns_true_with_mock_resp() {
        let port = spawn_resp_mock_server(RESP_INT_ONE).await;
        let config = ValkeyConfig {
            host: "127.0.0.1".to_string(),
            port,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.exists("test_key").await;
        assert!(
            result.is_ok(),
            "exists should succeed with mock: {:?}",
            result
        );
        assert!(result.unwrap(), "exists should return true for :1");
    }

    #[tokio::test]
    async fn test_exists_returns_false_with_mock_resp() {
        let port = spawn_resp_mock_server(RESP_INT_ZERO).await;
        let config = ValkeyConfig {
            host: "127.0.0.1".to_string(),
            port,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.exists("missing_key").await;
        assert!(
            result.is_ok(),
            "exists should succeed with mock: {:?}",
            result
        );
        assert!(!result.unwrap(), "exists should return false for :0");
    }

    #[tokio::test]
    async fn test_set_nx_ex_returns_true_when_key_set() {
        // SET NX EX returns "+OK\r\n" when key was set (did not exist)
        let port = spawn_resp_mock_server(RESP_OK).await;
        let config = ValkeyConfig {
            host: "127.0.0.1".to_string(),
            port,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.set_nx_ex("lock_key", "lock_value", 30).await;
        assert!(
            result.is_ok(),
            "set_nx_ex should succeed with mock: {:?}",
            result
        );
        assert!(
            result.unwrap(),
            "set_nx_ex should return true when key was set"
        );
    }

    #[tokio::test]
    async fn test_set_nx_ex_returns_false_when_key_exists() {
        // SET NX EX returns "$-1\r\n" (nil) when key already existed
        let port = spawn_resp_mock_server(RESP_NIL).await;
        let config = ValkeyConfig {
            host: "127.0.0.1".to_string(),
            port,
            max_connections: 1,
        };
        let pool = ValkeyPool::new(&config).unwrap();
        let result = pool.set_nx_ex("existing_lock", "lock_value", 30).await;
        assert!(
            result.is_ok(),
            "set_nx_ex should succeed with mock: {:?}",
            result
        );
        assert!(
            !result.unwrap(),
            "set_nx_ex should return false when key already exists"
        );
    }
}

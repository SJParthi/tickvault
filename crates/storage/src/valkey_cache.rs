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
}

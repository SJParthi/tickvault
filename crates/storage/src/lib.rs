// Compile-time lint enforcement — defense-in-depth with CLI clippy flags
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]
#![deny(clippy::dbg_macro)]
// Phase 0.2: no dropped Result/JoinHandle/must-use values (silent error swallowing).
#![cfg_attr(not(test), deny(unused_must_use))]
#![cfg_attr(not(test), warn(clippy::let_underscore_must_use))]
#![allow(missing_docs)] // TODO: enforce after adding docs to all public items

//! Data persistence layer — QuestDB for time-series, Valkey for caching.
//!
//! # Modules
//! - `instrument_persistence` — daily instrument snapshot to QuestDB (Block 01.1)
//! - `tick_persistence` — batched ILP writer for live ticks + market depth
//! - `candle_persistence` — 1-minute candle persistence from historical fetch
//! - `valkey_cache` — deadpool-redis async connection pool with typed helpers
//!
//! # Boot Sequence Position
//! OMS -> **QuestDB -> Valkey** -> HTTP API

pub mod calendar_persistence;
pub mod candle_persistence;
pub mod constituency_persistence;
pub mod deep_depth_persistence;
pub mod greeks_persistence;
pub mod indicator_snapshot_persistence;
pub mod instrument_persistence;
pub mod materialized_views;
pub mod movers_persistence;
pub mod obi_persistence;
pub mod partition_manager;
pub mod questdb_health;
pub mod tick_persistence;
pub mod valkey_cache;
pub mod ws_frame_spill;

/// Test support: re-exports internal functions for DHAT and benchmark tests.
pub mod tick_persistence_testing {
    /// Re-export of `f32_to_f64_clean` for DHAT and Criterion benchmarks.
    pub fn f32_to_f64_clean_pub(v: f32) -> f64 {
        crate::tick_persistence::f32_to_f64_clean(v)
    }
}

/// Shared process-wide mutex for tests that touch the real global
/// `data/spill/` directory (TICK_SPILL_DIR / CANDLE_SPILL_DIR).
///
/// The two recovery tests
///   - `tick_persistence::tests::test_recover_skips_current_active_spill`
///   - `candle_persistence::tests::test_live_candle_recover_stale_spill_files`
/// both call `recover_stale_spill_files()`, which drains EVERY matching
/// `{ticks,candles}-*.bin` file under the global spill directory. When
/// cargo runs these tests in parallel inside the same binary, they race
/// on filesystem state: one test drains the other test's artefacts
/// before the assertion runs, causing flaky CI failures under
/// `cargo test --workspace`.
///
/// Any test that creates files under `data/spill/` MUST acquire this
/// lock first. Holding it for the duration of the test serializes
/// access without needing a new dev-dep.
#[cfg(test)]
pub(crate) fn spill_dir_test_lock() -> &'static std::sync::Mutex<()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::tick_persistence_testing::f32_to_f64_clean_pub;

    #[test]
    fn test_f32_to_f64_clean_pub_preserves_price() {
        assert_eq!(f32_to_f64_clean_pub(24500.5), 24500.5);
    }

    #[test]
    fn test_f32_to_f64_clean_pub_zero() {
        assert_eq!(f32_to_f64_clean_pub(0.0), 0.0);
    }

    #[test]
    fn test_f32_to_f64_clean_pub_nan() {
        assert!(f32_to_f64_clean_pub(f32::NAN).is_nan());
    }

    #[test]
    fn test_f32_to_f64_clean_pub_infinity() {
        assert_eq!(f32_to_f64_clean_pub(f32::INFINITY), f64::INFINITY);
        assert_eq!(f32_to_f64_clean_pub(f32::NEG_INFINITY), f64::NEG_INFINITY);
    }

    #[test]
    fn test_f32_to_f64_clean_pub_dhan_index_price() {
        // STORAGE-GAP-02: f32→f64 precision test via public wrapper.
        // 21004.95_f32 must produce 21004.95_f64, not 21004.94921875.
        let result = f32_to_f64_clean_pub(21004.95_f32);
        assert_eq!(result, 21004.95_f64);
    }

    #[test]
    fn test_f32_to_f64_clean_pub_negative() {
        assert_eq!(f32_to_f64_clean_pub(-100.5), -100.5);
    }

    // -----------------------------------------------------------------------
    // Cache helper function tests (exercised outside valkey_cache module
    // so they are NOT skipped by `--skip valkey`)
    // -----------------------------------------------------------------------

    #[test]
    fn test_cache_instrument_key_format() {
        let key = crate::valkey_cache::build_instrument_cache_key("universe");
        assert_eq!(key, "dlt:instrument:universe");
    }

    #[test]
    fn test_cache_instrument_key_csv_hash() {
        let key = crate::valkey_cache::build_instrument_cache_key("csv_hash");
        assert_eq!(key, "dlt:instrument:csv_hash");
    }

    #[test]
    fn test_cache_instrument_key_namespace() {
        let key = crate::valkey_cache::build_instrument_cache_key("anything");
        assert!(key.starts_with("dlt:instrument:"));
    }

    #[test]
    fn test_cache_instrument_key_empty_suffix() {
        let key = crate::valkey_cache::build_instrument_cache_key("");
        assert_eq!(key, "dlt:instrument:");
    }

    #[test]
    fn test_cache_token_key_access() {
        let key = crate::valkey_cache::build_token_cache_key("access");
        assert_eq!(key, "dlt:token:access");
    }

    #[test]
    fn test_cache_token_key_expiry() {
        let key = crate::valkey_cache::build_token_cache_key("expiry");
        assert_eq!(key, "dlt:token:expiry");
    }

    #[test]
    fn test_cache_token_key_namespace() {
        let key = crate::valkey_cache::build_token_cache_key("any_suffix");
        assert!(key.starts_with("dlt:token:"));
    }

    #[test]
    fn test_cache_tick_key_ltp() {
        let key = crate::valkey_cache::build_tick_cache_key(11536, "ltp");
        assert_eq!(key, "dlt:tick:11536:ltp");
    }

    #[test]
    fn test_cache_tick_key_depth() {
        let key = crate::valkey_cache::build_tick_cache_key(49081, "depth");
        assert_eq!(key, "dlt:tick:49081:depth");
    }

    #[test]
    fn test_cache_tick_key_zero_id() {
        let key = crate::valkey_cache::build_tick_cache_key(0, "ltp");
        assert_eq!(key, "dlt:tick:0:ltp");
    }

    #[test]
    fn test_cache_tick_key_max_id() {
        let key = crate::valkey_cache::build_tick_cache_key(u32::MAX, "ohlc");
        assert_eq!(key, "dlt:tick:4294967295:ohlc");
    }

    #[test]
    fn test_cache_tick_key_isolation() {
        let a = crate::valkey_cache::build_tick_cache_key(100, "ltp");
        let b = crate::valkey_cache::build_tick_cache_key(200, "ltp");
        assert_ne!(a, b);
    }

    #[test]
    fn test_cache_ttl_before_target() {
        // IST 05:30 → target 08:00 → 9000s
        let utc_midnight = 1_704_067_200_u64;
        let ttl = crate::valkey_cache::compute_instrument_ttl_secs(utc_midnight, 8);
        assert_eq!(ttl, 9000);
    }

    #[test]
    fn test_cache_ttl_after_target_wraps() {
        // IST 10:00 → target 08:00 → 79200s
        let epoch = 86_400 - 19_800 + 36_000;
        let ttl = crate::valkey_cache::compute_instrument_ttl_secs(epoch, 8);
        assert_eq!(ttl, 79_200);
    }

    #[test]
    fn test_cache_ttl_clamps_min_60() {
        // Very close to target → clamped to 60s minimum
        let epoch = 86_400 + 21_599 - 19_800;
        let ttl = crate::valkey_cache::compute_instrument_ttl_secs(epoch, 6);
        assert_eq!(ttl, 60);
    }

    #[test]
    fn test_cache_ttl_exact_target_wraps_to_next_day() {
        // IST exactly at target hour → 86400s (next day)
        let epoch = 86_400 + 28_800 - 19_800;
        let ttl = crate::valkey_cache::compute_instrument_ttl_secs(epoch, 8);
        assert_eq!(ttl, 86_400);
    }

    #[test]
    fn test_cache_pool_creation() {
        let config = tickvault_common::config::ValkeyConfig {
            host: "unreachable-host".to_string(),
            port: 6379,
            max_connections: 4,
            password: String::new(),
        };
        // Pool creation is lazy — should succeed even with unreachable host
        let pool = crate::valkey_cache::ValkeyPool::new(&config);
        assert!(pool.is_ok());
    }

    #[test]
    fn test_cache_pool_reconnect() {
        let config = tickvault_common::config::ValkeyConfig {
            host: "unreachable-host".to_string(),
            port: 6379,
            max_connections: 4,
            password: String::new(),
        };
        let pool = crate::valkey_cache::ValkeyPool::new(&config).unwrap();
        // Reconnect should succeed (rebuilds pool, doesn't try to connect)
        let result = pool.reconnect();
        assert!(result.is_ok());
    }
}

// Compile-time lint enforcement — defense-in-depth with CLI clippy flags
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]
#![deny(clippy::dbg_macro)]
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
pub mod greeks_persistence;
pub mod instrument_persistence;
pub mod materialized_views;
pub mod tick_persistence;
pub mod valkey_cache;

/// Test support: re-exports internal functions for DHAT and benchmark tests.
pub mod tick_persistence_testing {
    /// Re-export of `f32_to_f64_clean` for DHAT and Criterion benchmarks.
    pub fn f32_to_f64_clean_pub(v: f32) -> f64 {
        crate::tick_persistence::f32_to_f64_clean(v)
    }
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
}

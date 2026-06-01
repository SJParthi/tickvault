//! Always-on (no market-hours filter) instrument set — operator lock
//! 2026-06-01 §30 (GIFT Nifty exemption).
//!
//! Most instruments only trade 09:15–15:30 IST, so ticks/candles outside
//! that window are dropped (`tick_processor` persist gates + the candle
//! aggregator window gate). GIFT Nifty (`GIFTNIFTY`, an NSE-IX index)
//! trades ~21 h/day, so its ticks/candles MUST NOT be dropped by that
//! filter. This module carries the boot-computed set of
//! `(security_id, exchange_segment_code)` pairs that are exempt.
//!
//! ## Why a process-global transport
//!
//! The set is computed ONCE at boot from the day's universe
//! (`DailyUniverse::always_on_segments`), but the tick processor and the
//! candle aggregator are spawned in deep, separate scopes of
//! `crates/app/src/main.rs` — threading an `Arc` through ~20 function
//! signatures would be huge churn. Instead boot calls
//! [`init_always_on_segments`] once, and each spawn site reads
//! [`current`] to obtain the `Arc` it passes EXPLICITLY into
//! `run_tick_processor(..)` / `MultiTfAggregator::with_always_on(..)`.
//!
//! The processor + aggregator take the set as an explicit argument (NOT
//! by reading this global), so their gate logic stays fully unit-testable
//! in isolation — tests construct their own set and never touch this
//! `OnceLock`.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};

/// Boot-set-once exempt set. `(security_id, exchange_segment_code)`.
/// Production sets this exactly once after the daily universe is built;
/// it is read-only thereafter.
static ALWAYS_ON: OnceLock<Arc<HashSet<(u32, u8)>>> = OnceLock::new();

/// Install the boot-computed always-on set. Idempotent: the first call
/// wins; later calls are ignored (boot runs once). Safe to never call —
/// [`current`] then returns an empty set (today's behavior: nothing is
/// exempt).
// TEST-EXEMPT: covered by tests::current_before_init_is_empty_then_reflects_init.
pub fn init_always_on_segments(set: HashSet<(u32, u8)>) {
    // First call wins; a second call returns Err(rejected_set) which we
    // intentionally discard (boot runs once). `drop` uses the #[must_use].
    drop(ALWAYS_ON.set(Arc::new(set)));
}

/// The current always-on set as a cheap-to-clone `Arc`. Returns an empty
/// set if [`init_always_on_segments`] was never called (e.g. the
/// `Indices4Only` scope, or any test that does not boot the universe).
// TEST-EXEMPT: covered by tests::current_before_init_is_empty_then_reflects_init.
#[must_use]
pub fn current() -> Arc<HashSet<(u32, u8)>> {
    ALWAYS_ON
        .get()
        .cloned()
        .unwrap_or_else(|| Arc::new(HashSet::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_before_init_is_empty_then_reflects_init() {
        // NOTE: a single test owns the OnceLock to avoid cross-test races.
        // Before init → empty.
        assert!(
            current().is_empty(),
            "uninitialised always-on set must be empty"
        );

        // After init → reflects the set (GIFT Nifty sid 5024, IDX_I=0).
        let mut set = HashSet::new();
        set.insert((5024_u32, 0_u8));
        init_always_on_segments(set);

        let now = current();
        assert!(now.contains(&(5024, 0)), "GIFT Nifty must be exempt");
        assert!(!now.contains(&(13, 0)), "NIFTY must NOT be exempt");

        // Set-once: a second init is ignored.
        let mut other = HashSet::new();
        other.insert((99, 9));
        init_always_on_segments(other);
        assert!(current().contains(&(5024, 0)), "first init wins");
        assert!(!current().contains(&(99, 9)), "second init ignored");
    }
}

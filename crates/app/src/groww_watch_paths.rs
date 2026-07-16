//! Pure path/timer helpers for the daily Groww watch file
//! (`data/groww/groww-watch-<YYYY-MM-DD>.json`).
//!
//! Re-homed out of the shadow-client module (2026-07-15 Groww live-feed
//! retirement, plan `.claude/plans/active-plan-groww-live-off.md` Item 3):
//! the KEEP spot-1m REST leg (`groww_spot_1m_boot.rs` VIX resolution) and the
//! `[groww_universe]` daily-build rider consume these primitives, so they
//! must not die with the shadow module.

use std::path::{Path, PathBuf};

use tickvault_common::constants::IST_UTC_OFFSET_SECONDS_I64;

/// Grace period past IST midnight before re-reading the watch file, so the
/// daily watch build has landed.
pub const DAY_ROLL_GRACE_SECS: i64 = 120;

/// Today's watch-file path under `cache_dir` — the exact naming the builder
/// writes (`groww-watch-<YYYY-MM-DD>.json`). Pure.
#[must_use]
pub fn watch_file_path_for(cache_dir: &Path, trading_date_ist: &str) -> PathBuf {
    cache_dir.join(format!("groww-watch-{trading_date_ist}.json"))
}

/// Seconds until the NEXT session recycle instant (IST midnight + grace) for
/// a given UTC epoch second. Pure — the day-roll timer is testable math.
#[must_use]
pub fn secs_until_day_roll(now_utc_secs: i64) -> u64 {
    let ist_secs = now_utc_secs + IST_UTC_OFFSET_SECONDS_I64;
    let ist_midnight_next = (ist_secs.div_euclid(86_400) + 1) * 86_400;
    let target = ist_midnight_next + DAY_ROLL_GRACE_SECS;
    u64::try_from(target - ist_secs).unwrap_or(86_400)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reader-side path matches the builder's naming byte-for-byte
    /// (`instruments.rs` writes `cache_dir/groww-watch-<date>.json`).
    #[test]
    fn test_watch_file_path_for_matches_builder_naming() {
        let p = watch_file_path_for(Path::new("data/groww"), "2026-07-04");
        assert_eq!(p, PathBuf::from("data/groww/groww-watch-2026-07-04.json"));
    }

    /// The day-roll timer targets IST midnight + grace, from any UTC instant.
    #[test]
    fn test_secs_until_day_roll() {
        // 2026-07-03 18:30:00 UTC == IST midnight exactly → next roll is a
        // full day + grace away.
        let ist_midnight_utc = 1_783_103_400i64;
        assert_eq!(
            secs_until_day_roll(ist_midnight_utc),
            86_400 + u64::try_from(DAY_ROLL_GRACE_SECS).unwrap_or(0)
        );
        // One second before IST midnight → 1s + grace.
        assert_eq!(
            secs_until_day_roll(ist_midnight_utc - 1),
            1 + u64::try_from(DAY_ROLL_GRACE_SECS).unwrap_or(0)
        );
        // Result is always positive and bounded by a day + grace.
        for offset in [0i64, 1, 3_600, 43_200, 86_399] {
            let s = secs_until_day_roll(ist_midnight_utc + offset);
            assert!(s >= 1);
            assert!(s <= 86_400 + u64::try_from(DAY_ROLL_GRACE_SECS).unwrap_or(0));
        }
    }
}

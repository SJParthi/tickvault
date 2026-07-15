//! IST day-number derivation — the SURVIVOR of the deleted Dhan-side
//! presence-registry registration module.
//!
//! PR-C3 (2026-07-14, operator retirement directive 2026-07-13 per
//! `websocket-connection-scope-lock.md` "2026-07-13 Amendment" §A/§B): the
//! Dhan slot-build halves (`dhan_presence_registrations` +
//! `register_dhan_presence_from_universe`, scoreboard PR-D) DIED with the
//! deleted `DailyUniverse` — there is no Dhan subscription plan to derive
//! presence slots from. [`ist_day_from_date`] is the scope-lock §B KEEP
//! item ("presence_registration::ist_day_from_date"): the shared IST
//! day-number convention consumed by the Groww presence registration
//! (`groww_activation.rs`) and the 15:45 scoreboard drain.

/// Days from CE (0001-01-01 = day 1) of the Unix epoch 1970-01-01 —
/// pinned by `test_ist_day_from_date_epoch_anchor`.
const EPOCH_DAYS_FROM_CE: i32 = 719_163;

/// IST day number (days since 1970-01-01) for an IST-naive date — the SAME
/// day-number convention the 15:45 scoreboard task uses
/// (`ist_secs.div_euclid(86_400)`), so registration days and drain targets
/// always agree.
#[must_use]
pub fn ist_day_from_date(d: chrono::NaiveDate) -> u64 {
    use chrono::Datelike;
    u64::try_from(d.num_days_from_ce().saturating_sub(EPOCH_DAYS_FROM_CE)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ist_day_from_date_epoch_anchor() {
        let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1);
        assert_eq!(epoch.map(ist_day_from_date), Some(0));
        // 2026-07-10 = 20_644 days after the epoch (cross-checked against
        // the scoreboard task's div_euclid(86_400) convention).
        let day = chrono::NaiveDate::from_ymd_opt(2026, 7, 10);
        assert_eq!(day.map(ist_day_from_date), Some(20_644));
    }

    /// Wiring ratchet: the two Dhan persist arms in tick_processor.rs must
    /// fold presence (mirror of the feed_lag producer-site ratchet). Kept
    /// through PR-C3: `feed_presence` + the tick-processor fold survive the
    /// Dhan chain deletion (they serve WAL-replay + any future feed's
    /// frames routed through the shared processor).
    #[test]
    fn test_dhan_presence_fold_sites_wired_into_tick_processor() {
        let src = include_str!("../pipeline/tick_processor.rs");
        let needle = "feed_presence::record_presence(";
        let count = src.matches(needle).count();
        assert_eq!(
            count, 2,
            "tick_processor.rs must fold presence at BOTH Dhan persist arms \
             (Ticker/Quote + Full) — found {count} call(s)"
        );
    }
}

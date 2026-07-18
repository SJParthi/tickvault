//! IST time math — exact parity with the Python heredocs.
//!
//! The market-hours gate heredoc computed IST as a fixed UTC+5:30 offset
//! (`IST = timedelta(hours=5, minutes=30)`), NOT via a tz database. India
//! has no DST, so the fixed offset is correct forever; we mirror the fixed
//! offset (not chrono-tz) so the port is line-for-line comparable.

use chrono::{DateTime, FixedOffset, Offset, Utc};

/// The IST offset in seconds east of UTC (+05:30).
pub const IST_OFFSET_SECS: i32 = 5 * 3600 + 30 * 60;

/// The fixed IST offset. `FixedOffset::east_opt` is only `None` for
/// out-of-range offsets; 19800s is statically in range, so the fallback
/// arm is unreachable (kept to avoid `unwrap` per the crate lints).
fn ist() -> FixedOffset {
    FixedOffset::east_opt(IST_OFFSET_SECS).unwrap_or_else(|| Utc.fix())
}

/// Today's IST date as `YYYY-MM-DD` — Python parity:
/// `(datetime.now(timezone.utc) + IST).strftime('%Y-%m-%d')`.
pub fn today_ist_string(now_utc: DateTime<Utc>) -> String {
    now_utc.with_timezone(&ist()).format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn ist_offset_is_5h30m() {
        assert_eq!(IST_OFFSET_SECS, 19800);
    }

    #[test]
    fn today_ist_matches_python_utc_plus_offset() {
        // 2026-07-17 20:00 UTC = 2026-07-18 01:30 IST — date rolls over.
        let t = Utc.with_ymd_and_hms(2026, 7, 17, 20, 0, 0).unwrap();
        assert_eq!(today_ist_string(t), "2026-07-18");
    }

    #[test]
    fn today_ist_same_day_before_1830_utc() {
        // 18:29:59 UTC is 23:59:59 IST — still the same IST date.
        let t = Utc.with_ymd_and_hms(2026, 7, 17, 18, 29, 59).unwrap();
        assert_eq!(today_ist_string(t), "2026-07-17");
        // 18:30:00 UTC crosses IST midnight.
        let t = Utc.with_ymd_and_hms(2026, 7, 17, 18, 30, 0).unwrap();
        assert_eq!(today_ist_string(t), "2026-07-18");
    }
}

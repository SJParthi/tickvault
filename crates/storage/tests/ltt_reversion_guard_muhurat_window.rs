//! Review fix 2026-11-21 (noise lock §2.24-a): the trade-time reversion guard
//! watches the PUBLISHED Muhurat session, not a fixed 18:00–19:30 window.
//!
//! Before the fix, a Muhurat session outside 09:00–15:40 and 18:00–19:30 (for
//! example 17:00–18:15) got no reversion page, so `reversion_pages` never rose
//! and the app never auto-held the day (daily-universe Quote 63) while wrongly
//! shifted rows were stored. This binary installs a calendar with such a
//! session and drives the production `LttController`.
//!
//! Its own binary because the calendar `OnceLock` is process-wide.

use std::sync::Arc;

use tickvault_common::config::{LttUtcAutocorrect, MuhuratTradingEntry, TradingConfig};
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_storage::ltt_correction::{LttController, SEGMENT_IDX_I, SEGMENT_NSE_EQ};

const IST_OFFSET_SECS: i64 = 19_800;

/// The shipped `[trading]` section.
fn shipped_trading_config() -> TradingConfig {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/base.toml");
    let raw = std::fs::read_to_string(path).expect("config/base.toml");
    let table: toml::Table = raw.parse().expect("base.toml parses");
    table
        .get("trading")
        .expect("[trading]")
        .clone()
        .try_into()
        .expect("[trading] deserializes")
}

/// UTC seconds of `hh:mm` IST on 2026-11-22.
fn utc_secs_at(hh: u32, mm: u32) -> i64 {
    chrono::NaiveDate::from_ymd_opt(2026, 11, 22)
        .and_then(|d| d.and_hms_opt(hh, mm, 0))
        .map(|t| t.and_utc().timestamp())
        .expect("valid time")
        - IST_OFFSET_SECS
}

/// `windows` vote windows of 20 frames, each with 5 distinct prints carrying
/// the OLD (IST-epoch) trade time, so every window reads `Normal`.
fn reverted_frames(ctl: &mut LttController, start_utc_secs: i64, windows: i64, seg: u8) {
    for s in 0..windows * 20 {
        let t = start_utc_secs + s;
        let receipt = t * 1_000_000_000;
        ctl.on_frame(receipt);
        let raw = u32::try_from(t + IST_OFFSET_SECS).expect("in u32");
        for i in 0..5u64 {
            ctl.observe(seg, i, raw, receipt);
        }
    }
}

/// Regression, 2026-11-21 review: a reversion at 17:10 IST inside a published
/// 17:00–18:15 Muhurat session pages and counts for the hold; 16:30 (before
/// the 15-minute lead) and 18:30 (past the 10-minute tail, though inside the
/// old fixed 18:00–19:30 window) do not.
///
/// Bite: restore the fixed `MUHURAT_PERSIST_*` range in `in_guard_window` and
/// the 17:10 assert fails (`reversion_pages` stays 0).
#[test]
fn test_regression_reversion_page_follows_the_published_muhurat_session() {
    let mut trading = shipped_trading_config();
    trading.muhurat_trading_dates.push(MuhuratTradingEntry {
        date: "2026-11-22".to_string(),
        name: "synthetic late session".to_string(),
        session_start_ist: "17:00".to_string(),
        session_end_ist: "18:15".to_string(),
    });
    let calendar = TradingCalendar::from_config(&trading).expect("calendar");
    let date = chrono::NaiveDate::from_ymd_opt(2026, 11, 22).expect("d");
    assert_eq!(
        calendar.muhurat_session_secs(date),
        Some((17 * 3_600, 18 * 3_600 + 15 * 60))
    );
    assert!(tickvault_common::market_hours::set_market_calendar_for_session(Arc::new(calendar)));

    let mut ctl = LttController::new(LttUtcAutocorrect::Act);
    reverted_frames(&mut ctl, utc_secs_at(16, 30), 3, SEGMENT_IDX_I);
    assert!(
        ctl.counts().normal_verdicts > 0,
        "the votes reached a verdict"
    );
    assert_eq!(ctl.counts().reversion_pages, 0, "16:30 is before 16:45");

    reverted_frames(&mut ctl, utc_secs_at(17, 10), 3, SEGMENT_IDX_I);
    assert_eq!(
        ctl.counts().reversion_pages,
        1,
        "17:10 is inside the session"
    );
    assert_eq!(ctl.counts().reversion_reports, 1);

    // The stock segment has its own once-a-day latch, so only the window
    // keeps 18:30 quiet.
    reverted_frames(&mut ctl, utc_secs_at(18, 30), 3, SEGMENT_NSE_EQ);
    assert_eq!(
        ctl.counts().reversion_pages,
        1,
        "18:30 is past 18:25 although inside the old fixed window"
    );
}

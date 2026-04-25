//! Boot Mode Detection — selects the right F&O subscription strategy based
//! on the wall-clock time at app start.
//!
//! ## The 4 modes
//!
//! | Mode | Time window IST | Stock-ATM source |
//! |---|---|---|
//! | `PreMarket` | 00:00 – 09:00 | Pre-open buffer (09:00–09:12 captures) |
//! | `MidPreMarket` | 09:00 – 09:13 | Partial pre-open buffer + REST fallback at 09:12:55 |
//! | `MidMarket` | 09:13 – 15:30 | **Live cash-equity ticks** (primary) → REST fallback (stragglers) → QuestDB previous close (last resort) |
//! | `PostMarket` | 15:30 – 24:00 | QuestDB previous close (no live data) |
//!
//! ## Why a separate module
//!
//! `detect_boot_mode()` is a pure function of `now_ist_secs_of_day` (no I/O,
//! no globals). This makes it trivially testable across every minute of the
//! day. Every other path in the boot sequence calls this once and branches.
//!
//! The function is `pub` and `const`-safe in the sense that the boundaries
//! are statically defined — no runtime config can shift them.
//!
//! ## Boundaries
//!
//! - `PRE_MARKET_END_SECS_IST = 9 * 3600 = 32_400` (09:00:00 IST)
//! - `MID_PRE_MARKET_END_SECS_IST = 9 * 3600 + 13 * 60 = 33_180` (09:13:00 IST)
//! - `MID_MARKET_END_SECS_IST = 15 * 3600 + 30 * 60 = 55_800` (15:30:00 IST)
//!
//! 09:13:00 boundary is taken from the existing pre-open buffer rule
//! (PR #337 Fix #1): pre-open buffer captures slots 09:00..=09:12 inclusive,
//! so 09:13:00 is the first second the buffer's last slot is fully closed.
//!
//! 15:30:00 is `TICK_PERSIST_END_SECS_OF_DAY_IST` from `tickvault_common`.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Pre-market window end (09:00:00 IST in seconds-of-day).
pub const PRE_MARKET_END_SECS_IST: u32 = 9 * 3600;

/// Mid-pre-market window end (09:13:00 IST in seconds-of-day).
///
/// 09:13:00 is the Phase 2 dispatch trigger from PR #337. At 09:12:00 the
/// pre-open buffer's last slot (09:12:00..09:12:59) is still being populated;
/// reading at 09:13:00 guarantees the slot is closed.
pub const MID_PRE_MARKET_END_SECS_IST: u32 = 9 * 3600 + 13 * 60;

/// Mid-market window end (15:30:00 IST in seconds-of-day).
///
/// Mirrors `tickvault_common::constants::TICK_PERSIST_END_SECS_OF_DAY_IST`.
/// After 15:30 NSE has closed and live ticks stop arriving.
pub const MID_MARKET_END_SECS_IST: u32 = 15 * 3600 + 30 * 60;

/// The 4 boot modes the app supports.
///
/// Each mode selects a different stock-ATM resolution strategy. Indices,
/// cash equities, and display indices are subscribed at boot in every mode
/// (no ATM math required for full-chain or non-derivative subscriptions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BootMode {
    /// Boot before 09:00 IST. Stocks not yet trading. Pre-open buffer will
    /// capture 09:00–09:12 closes; Phase 2 dispatches at 09:13:00.
    PreMarket,
    /// Boot during 09:00–09:13 IST. Pre-open ticks are arriving but Phase 2
    /// hasn't dispatched yet. Same end-of-flow as `PreMarket`.
    MidPreMarket,
    /// Boot during 09:13–15:30 IST. Live cash-equity ticks are flowing.
    /// Stock ATM resolved from `SharedSpotPrices` (live ticks) → REST
    /// `/marketfeed/ltp` for stragglers → QuestDB previous close as last
    /// resort. Stock F&O subscribed within ~30 seconds.
    MidMarket,
    /// Boot after 15:30 IST. NSE closed. No live ticks expected. Stock ATM
    /// resolved from QuestDB previous close (defensive — system is preparing
    /// for next trading day).
    PostMarket,
}

impl BootMode {
    /// Returns `true` if the mode requires the pre-open buffer to capture
    /// stock prices before Phase 2 dispatch can compute ATM.
    pub const fn waits_for_preopen_buffer(self) -> bool {
        matches!(self, Self::PreMarket | Self::MidPreMarket)
    }

    /// Returns `true` if the mode should resolve stock ATM from live
    /// cash-equity ticks (rather than the pre-open buffer or QuestDB).
    pub const fn uses_live_tick_resolver(self) -> bool {
        matches!(self, Self::MidMarket)
    }

    /// Returns `true` if the mode is during NSE trading hours (09:13–15:30).
    pub const fn is_within_trading_hours(self) -> bool {
        matches!(self, Self::MidMarket)
    }
}

impl fmt::Display for BootMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::PreMarket => "PreMarket",
            Self::MidPreMarket => "MidPreMarket",
            Self::MidMarket => "MidMarket",
            Self::PostMarket => "PostMarket",
        };
        f.write_str(s)
    }
}

/// Pure function: classify the current IST seconds-of-day into a `BootMode`.
///
/// `now_ist_secs_of_day` MUST be in `[0, 86400)`. Values outside this range
/// are clamped via modulo (defensive — should never happen in practice).
///
/// # Examples
///
/// ```
/// use tickvault_core::instrument::boot_mode::{detect_boot_mode, BootMode};
///
/// // 08:30 IST → PreMarket
/// assert_eq!(detect_boot_mode(8 * 3600 + 30 * 60), BootMode::PreMarket);
/// // 09:05 IST → MidPreMarket
/// assert_eq!(detect_boot_mode(9 * 3600 + 5 * 60), BootMode::MidPreMarket);
/// // 11:30 IST → MidMarket
/// assert_eq!(detect_boot_mode(11 * 3600 + 30 * 60), BootMode::MidMarket);
/// // 16:00 IST → PostMarket
/// assert_eq!(detect_boot_mode(16 * 3600), BootMode::PostMarket);
/// ```
pub fn detect_boot_mode(now_ist_secs_of_day: u32) -> BootMode {
    // Defensive clamp: should always be < 86400 in practice (24h * 3600s).
    let secs = now_ist_secs_of_day % 86_400;
    if secs < PRE_MARKET_END_SECS_IST {
        BootMode::PreMarket
    } else if secs < MID_PRE_MARKET_END_SECS_IST {
        BootMode::MidPreMarket
    } else if secs < MID_MARKET_END_SECS_IST {
        BootMode::MidMarket
    } else {
        BootMode::PostMarket
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_boot_mode_pre_market_at_midnight_ist() {
        assert_eq!(detect_boot_mode(0), BootMode::PreMarket);
    }

    #[test]
    fn test_boot_mode_pre_market_at_0830_ist() {
        let secs = 8 * 3600 + 30 * 60;
        assert_eq!(detect_boot_mode(secs), BootMode::PreMarket);
    }

    #[test]
    fn test_boot_mode_pre_market_at_0859_59_ist() {
        let secs = 8 * 3600 + 59 * 60 + 59;
        assert_eq!(detect_boot_mode(secs), BootMode::PreMarket);
    }

    #[test]
    fn test_boot_mode_mid_pre_market_at_0900_exact() {
        let secs = 9 * 3600;
        assert_eq!(detect_boot_mode(secs), BootMode::MidPreMarket);
    }

    #[test]
    fn test_boot_mode_mid_pre_market_at_0905_ist() {
        let secs = 9 * 3600 + 5 * 60;
        assert_eq!(detect_boot_mode(secs), BootMode::MidPreMarket);
    }

    #[test]
    fn test_boot_mode_mid_pre_market_at_0912_59_ist() {
        let secs = 9 * 3600 + 12 * 60 + 59;
        assert_eq!(detect_boot_mode(secs), BootMode::MidPreMarket);
    }

    #[test]
    fn test_boot_mode_mid_market_at_0913_exact() {
        let secs = 9 * 3600 + 13 * 60;
        assert_eq!(detect_boot_mode(secs), BootMode::MidMarket);
    }

    #[test]
    fn test_boot_mode_mid_market_at_0915_ist() {
        let secs = 9 * 3600 + 15 * 60;
        assert_eq!(detect_boot_mode(secs), BootMode::MidMarket);
    }

    #[test]
    fn test_boot_mode_mid_market_at_1130_ist() {
        let secs = 11 * 3600 + 30 * 60;
        assert_eq!(detect_boot_mode(secs), BootMode::MidMarket);
    }

    #[test]
    fn test_boot_mode_mid_market_at_1529_59_ist() {
        let secs = 15 * 3600 + 29 * 60 + 59;
        assert_eq!(detect_boot_mode(secs), BootMode::MidMarket);
    }

    #[test]
    fn test_boot_mode_post_market_at_1530_exact() {
        let secs = 15 * 3600 + 30 * 60;
        assert_eq!(detect_boot_mode(secs), BootMode::PostMarket);
    }

    #[test]
    fn test_boot_mode_post_market_at_1600_ist() {
        let secs = 16 * 3600;
        assert_eq!(detect_boot_mode(secs), BootMode::PostMarket);
    }

    #[test]
    fn test_boot_mode_post_market_at_2359_59_ist() {
        let secs = 23 * 3600 + 59 * 60 + 59;
        assert_eq!(detect_boot_mode(secs), BootMode::PostMarket);
    }

    /// Clamp test — values >= 86400 wrap. Should never happen, but the
    /// defensive `% 86400` keeps the function total.
    #[test]
    fn test_boot_mode_clamp_overflow() {
        // 86400 wraps to 0 → PreMarket
        assert_eq!(detect_boot_mode(86_400), BootMode::PreMarket);
        // 86400 + 11*3600 = 11:00 next "day" → MidMarket
        assert_eq!(detect_boot_mode(86_400 + 11 * 3600), BootMode::MidMarket);
    }

    #[test]
    fn test_boot_mode_waits_for_preopen_buffer() {
        assert!(BootMode::PreMarket.waits_for_preopen_buffer());
        assert!(BootMode::MidPreMarket.waits_for_preopen_buffer());
        assert!(!BootMode::MidMarket.waits_for_preopen_buffer());
        assert!(!BootMode::PostMarket.waits_for_preopen_buffer());
    }

    #[test]
    fn test_boot_mode_uses_live_tick_resolver() {
        assert!(!BootMode::PreMarket.uses_live_tick_resolver());
        assert!(!BootMode::MidPreMarket.uses_live_tick_resolver());
        assert!(BootMode::MidMarket.uses_live_tick_resolver());
        assert!(!BootMode::PostMarket.uses_live_tick_resolver());
    }

    #[test]
    fn test_boot_mode_is_within_trading_hours() {
        assert!(!BootMode::PreMarket.is_within_trading_hours());
        assert!(!BootMode::MidPreMarket.is_within_trading_hours());
        assert!(BootMode::MidMarket.is_within_trading_hours());
        assert!(!BootMode::PostMarket.is_within_trading_hours());
    }

    #[test]
    fn test_boot_mode_display_strings() {
        assert_eq!(format!("{}", BootMode::PreMarket), "PreMarket");
        assert_eq!(format!("{}", BootMode::MidPreMarket), "MidPreMarket");
        assert_eq!(format!("{}", BootMode::MidMarket), "MidMarket");
        assert_eq!(format!("{}", BootMode::PostMarket), "PostMarket");
    }

    /// Sweep every minute of the day and confirm the partition is total
    /// (every minute classifies into exactly one mode).
    #[test]
    fn test_boot_mode_classification_is_total_across_24h() {
        for minute in 0..(24 * 60) {
            let secs = minute * 60;
            let _mode = detect_boot_mode(secs);
            // No assertion — just confirms no panic across the full day.
        }
    }

    /// Boundary parity: the 4 transition points must yield the documented
    /// downstream mode (08:59:59 → PreMarket, 09:00:00 → MidPreMarket, etc).
    #[test]
    fn test_boot_mode_boundary_parity() {
        // 08:59:59 → PreMarket; 09:00:00 → MidPreMarket
        assert_eq!(
            detect_boot_mode(PRE_MARKET_END_SECS_IST - 1),
            BootMode::PreMarket
        );
        assert_eq!(
            detect_boot_mode(PRE_MARKET_END_SECS_IST),
            BootMode::MidPreMarket
        );

        // 09:12:59 → MidPreMarket; 09:13:00 → MidMarket
        assert_eq!(
            detect_boot_mode(MID_PRE_MARKET_END_SECS_IST - 1),
            BootMode::MidPreMarket
        );
        assert_eq!(
            detect_boot_mode(MID_PRE_MARKET_END_SECS_IST),
            BootMode::MidMarket
        );

        // 15:29:59 → MidMarket; 15:30:00 → PostMarket
        assert_eq!(
            detect_boot_mode(MID_MARKET_END_SECS_IST - 1),
            BootMode::MidMarket
        );
        assert_eq!(
            detect_boot_mode(MID_MARKET_END_SECS_IST),
            BootMode::PostMarket
        );
    }

    #[test]
    fn test_boot_mode_serde_roundtrip() {
        for mode in [
            BootMode::PreMarket,
            BootMode::MidPreMarket,
            BootMode::MidMarket,
            BootMode::PostMarket,
        ] {
            let json = serde_json::to_string(&mode).expect("serialize");
            let back: BootMode = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(mode, back);
        }
    }
}

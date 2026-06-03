//! Phase 0 Item 20 — orphan position 15:25 IST watchdog.
//!
//! Strategy is intraday option-buying — NO overnight derivative
//! positions allowed. At 15:25:00 IST every trading day this watchdog:
//!
//!  1. Queries Dhan REST `GET /v2/positions`.
//!  2. Filters rows where `net_qty != 0` (open positions).
//!  3. Writes one audit row per orphan (DEDUP key
//!     `(trading_date_ist, security_id, exchange_segment, ts)`) plus
//!     one `NoOrphans` green-path marker row when the account is flat.
//!  4. Emits Telegram CRITICAL `OrphanPositionDetected` on rising
//!     edge (audit-findings Rule 4).
//!  5. **Phase 0 (dry-run):** stops after the audit + Telegram.
//!  6. **Phase 1+ (live):** cancels Super Order legs + places market
//!     exit, upgrades audit row outcome from `Detected` →
//!     `AutoClosed` / `ExitFailed`.
//!
//! The 5-minute headroom before 15:30 close lets the full DETECT →
//! AUDIT → Telegram → exit chain complete before NSE rejects late
//! orders.

use std::time::Duration;

use tickvault_common::constants::{
    IST_UTC_OFFSET_SECONDS, ORPHAN_POSITION_WATCHDOG_TIME_SECS_IST, SECONDS_PER_DAY,
};

use crate::oms::types::DhanPositionResponse;

/// Verdict returned by the pure-function evaluator. Carries the
/// list of orphan positions (zero-allocation when none) so the
/// supervisor task can drive both the audit-row write loop AND the
/// single coalesced Telegram event off the same data.
#[derive(Debug, Clone, PartialEq)]
pub enum OrphanPositionVerdict {
    /// Account is flat at 15:25 IST — green path. Supervisor writes
    /// one `NoOrphans` audit row (with `security_id = 0`) so the
    /// Grafana panel can confirm the watchdog ran today.
    NoOrphans,
    /// At least one position has `net_qty != 0`. Supervisor writes
    /// one audit row per orphan + emits Telegram CRITICAL.
    OrphansDetected {
        /// Number of orphans. Mirrors `orphans.len()` — kept as a
        /// separate field so callers don't need to walk the vec to
        /// size the Telegram payload.
        count: usize,
        /// Sum of `|net_qty|` across all orphans — useful operator
        /// metric ("how many lots are open?").
        total_abs_net_qty: i64,
        /// Per-orphan details. Owned (not borrowed) so the
        /// supervisor can drop the original `DhanPositionResponse`
        /// vec and still drive the audit + Telegram loops.
        orphans: Vec<OrphanPositionInfo>,
    },
}

/// One orphan position. Borrowed from `DhanPositionResponse` at
/// evaluation time and copied into an owned snapshot so the
/// supervisor can drop the upstream vec.
#[derive(Debug, Clone, PartialEq)]
pub struct OrphanPositionInfo {
    /// Numeric SecurityId. Parsed from the Dhan response's string
    /// field; `0` if the parse failed (still emitted to keep the
    /// audit row + Telegram payload complete — operator can cross-
    /// reference the trading symbol manually).
    pub security_id: i64,
    /// Exchange segment string from Dhan (`NSE_FNO`, `NSE_EQ`, ...).
    /// Stored verbatim — no enum coercion — so the audit row reflects
    /// exactly what Dhan returned.
    pub exchange_segment: String,
    /// Trading symbol from Dhan (e.g. `NIFTY-Mar2026-24500-CE`).
    pub trading_symbol: String,
    /// Product type (`INTRADAY`, `MARGIN`, `CNC`, `MTF`).
    pub product_type: String,
    /// Net quantity (buy - sell). Negative for short positions.
    pub net_qty: i64,
    /// Buy quantity.
    pub buy_qty: i64,
    /// Sell quantity.
    pub sell_qty: i64,
    /// Booked P&L.
    pub realized_profit: f64,
    /// Open / mark-to-market P&L.
    pub unrealized_profit: f64,
}

/// Pure-function evaluator. Inspects every position and returns a
/// verdict. No I/O, no side effects, fully testable.
///
/// **Allocation policy:** this function runs ONCE PER TRADING DAY at
/// 15:25:00 IST (per `ORPHAN_POSITION_WATCHDOG_TIME_SECS_IST`). It is
/// NOT a hot path — it is a daily compliance gate. The `Vec::new()` +
/// `.clone()` calls below are intentional: the verdict owns its data
/// so the supervisor task can drop the upstream `DhanPositionResponse`
/// vec (returned by `reqwest::json()`) and still drive the audit-row
/// write loop + Telegram emit. Pre-allocating `with_capacity(positions.len())`
/// would still produce the same allocation in the common path where over
/// 50% of positions are orphans; the empty-set fast path
/// short-circuits via `if pos.net_qty == 0 { continue; }`.
// APPROVED: daily compliance gate — see paragraph above. Not hot path.
#[must_use]
pub fn evaluate_orphan_positions(positions: &[DhanPositionResponse]) -> OrphanPositionVerdict {
    // APPROVED: daily compliance gate — see fn docstring. Not hot path.
    let mut orphans: Vec<OrphanPositionInfo> = Vec::new();
    let mut total_abs_net_qty: i64 = 0;
    for pos in positions {
        if pos.net_qty == 0 {
            continue;
        }
        total_abs_net_qty = total_abs_net_qty.saturating_add(pos.net_qty.unsigned_abs() as i64);
        orphans.push(OrphanPositionInfo {
            security_id: pos.security_id.parse::<i64>().unwrap_or(0),
            // APPROVED: daily compliance gate (15:25 IST, once/day).
            exchange_segment: pos.exchange_segment.clone(),
            // APPROVED: daily compliance gate (15:25 IST, once/day).
            trading_symbol: pos.trading_symbol.clone(),
            // APPROVED: daily compliance gate (15:25 IST, once/day).
            product_type: pos.product_type.clone(),
            net_qty: pos.net_qty,
            buy_qty: pos.buy_qty,
            sell_qty: pos.sell_qty,
            realized_profit: pos.realized_profit,
            unrealized_profit: pos.unrealized_profit,
        });
    }
    if orphans.is_empty() {
        OrphanPositionVerdict::NoOrphans
    } else {
        let count = orphans.len();
        OrphanPositionVerdict::OrphansDetected {
            count,
            total_abs_net_qty,
            orphans,
        }
    }
}

/// Pure-function clock helper: seconds remaining until the next
/// 15:25:00 IST boundary, given the current IST seconds-of-day. If
/// the current time is already past 15:25, returns the duration
/// until 15:25 tomorrow (`SECONDS_PER_DAY - now + 15:25`). Returns
/// `0` only when `now == 15:25:00` exactly.
#[must_use]
pub fn seconds_until_orphan_watchdog_ist(now_secs_of_day_ist: u32) -> u32 {
    if now_secs_of_day_ist <= ORPHAN_POSITION_WATCHDOG_TIME_SECS_IST {
        ORPHAN_POSITION_WATCHDOG_TIME_SECS_IST - now_secs_of_day_ist
    } else {
        SECONDS_PER_DAY - now_secs_of_day_ist + ORPHAN_POSITION_WATCHDOG_TIME_SECS_IST
    }
}

/// Convert a `chrono::Utc::now()`-style UTC timestamp (seconds since
/// epoch) into IST seconds-of-day. Lifted out so the supervisor task
/// and tests share the same clock arithmetic.
#[must_use]
pub fn ist_seconds_of_day_from_utc(now_utc_secs: i64) -> u32 {
    let ist = now_utc_secs.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
    ist.rem_euclid(i64::from(SECONDS_PER_DAY)) as u32
}

/// Sleep duration until the next 15:25 IST boundary from the
/// supervisor task's current wall clock.
#[must_use]
pub fn sleep_duration_until_orphan_watchdog(now_utc_secs: i64) -> Duration {
    let now_ist_sec = ist_seconds_of_day_from_utc(now_utc_secs);
    Duration::from_secs(u64::from(seconds_until_orphan_watchdog_ist(now_ist_sec)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position(security_id: &str, segment: &str, net_qty: i64) -> DhanPositionResponse {
        DhanPositionResponse {
            dhan_client_id: "1106656882".to_string(),
            security_id: security_id.to_string(),
            exchange_segment: segment.to_string(),
            product_type: "INTRADAY".to_string(),
            position_type: if net_qty > 0 {
                "LONG"
            } else if net_qty < 0 {
                "SHORT"
            } else {
                "CLOSED"
            }
            .to_string(),
            buy_qty: net_qty.max(0),
            sell_qty: (-net_qty).max(0),
            net_qty,
            buy_avg: 100.0,
            sell_avg: 0.0,
            realized_profit: 0.0,
            unrealized_profit: -1250.50,
            trading_symbol: format!("NIFTY-Mar2026-{}-CE", security_id),
            cost_price: 100.0,
            multiplier: 50,
            drv_expiry_date: "2026-03-27".to_string(),
            drv_option_type: "CALL".to_string(),
            drv_strike_price: 24500.0,
            rbi_reference_rate: 0.0,
            carry_forward_buy_qty: 0,
            carry_forward_sell_qty: 0,
            carry_forward_buy_value: 0.0,
            carry_forward_sell_value: 0.0,
            day_buy_qty: net_qty.max(0),
            day_sell_qty: (-net_qty).max(0),
            day_buy_value: 0.0,
            day_sell_value: 0.0,
            cross_currency: false,
        }
    }

    #[test]
    fn test_evaluate_orphan_positions_empty_returns_no_orphans() {
        let verdict = evaluate_orphan_positions(&[]);
        assert_eq!(verdict, OrphanPositionVerdict::NoOrphans);
    }

    #[test]
    fn test_evaluate_orphan_positions_all_closed_returns_no_orphans() {
        let positions = vec![
            position("72271", "NSE_FNO", 0),
            position("72272", "NSE_FNO", 0),
        ];
        let verdict = evaluate_orphan_positions(&positions);
        assert_eq!(verdict, OrphanPositionVerdict::NoOrphans);
    }

    #[test]
    fn test_evaluate_orphan_positions_one_long_detected() {
        let positions = vec![position("72271", "NSE_FNO", 50)];
        match evaluate_orphan_positions(&positions) {
            OrphanPositionVerdict::OrphansDetected {
                count,
                total_abs_net_qty,
                orphans,
            } => {
                assert_eq!(count, 1);
                assert_eq!(total_abs_net_qty, 50);
                assert_eq!(orphans.len(), 1);
                assert_eq!(orphans[0].security_id, 72271);
                assert_eq!(orphans[0].net_qty, 50);
                assert_eq!(orphans[0].exchange_segment, "NSE_FNO");
            }
            v => panic!("expected OrphansDetected, got {:?}", v),
        }
    }

    #[test]
    fn test_evaluate_one_short_orphan_detected_with_abs_qty() {
        let positions = vec![position("72271", "NSE_FNO", -75)];
        match evaluate_orphan_positions(&positions) {
            OrphanPositionVerdict::OrphansDetected {
                count,
                total_abs_net_qty,
                orphans,
            } => {
                assert_eq!(count, 1);
                // total_abs_net_qty must use unsigned_abs — short
                // positions contribute their magnitude, not their
                // signed value.
                assert_eq!(total_abs_net_qty, 75);
                assert_eq!(orphans[0].net_qty, -75);
            }
            v => panic!("expected OrphansDetected, got {:?}", v),
        }
    }

    #[test]
    fn test_evaluate_mixed_skips_closed_keeps_open() {
        let positions = vec![
            position("72271", "NSE_FNO", 50),  // orphan
            position("72272", "NSE_FNO", 0),   // closed — skip
            position("72273", "NSE_FNO", -25), // orphan (short)
        ];
        match evaluate_orphan_positions(&positions) {
            OrphanPositionVerdict::OrphansDetected {
                count,
                total_abs_net_qty,
                orphans,
            } => {
                assert_eq!(count, 2);
                assert_eq!(total_abs_net_qty, 75); // 50 + 25
                assert_eq!(orphans.len(), 2);
                // Order is preserved from input slice.
                assert_eq!(orphans[0].security_id, 72271);
                assert_eq!(orphans[1].security_id, 72273);
            }
            v => panic!("expected OrphansDetected, got {:?}", v),
        }
    }

    #[test]
    fn test_evaluate_unparseable_security_id_defaults_to_zero() {
        // Dhan should never send this, but defensive coverage: a
        // garbage SecurityId still produces a valid audit row so
        // the operator can cross-reference via trading_symbol.
        let mut pos = position("72271", "NSE_FNO", 25);
        pos.security_id = "garbage".to_string();
        let verdict = evaluate_orphan_positions(&[pos]);
        match verdict {
            OrphanPositionVerdict::OrphansDetected { orphans, .. } => {
                assert_eq!(orphans[0].security_id, 0);
            }
            v => panic!("expected OrphansDetected, got {:?}", v),
        }
    }

    // --- clock helpers ---

    #[test]
    fn test_seconds_until_orphan_watchdog_ist_at_midnight() {
        // 00:00:00 IST → 15:25:00 IST = 15*3600 + 25*60 = 55_500.
        assert_eq!(seconds_until_orphan_watchdog_ist(0), 55_500);
    }

    #[test]
    fn test_seconds_until_watchdog_at_market_open() {
        // 09:00:00 IST → 15:25:00 IST = 6h25m = 23_100.
        let now = 9 * 3600;
        assert_eq!(seconds_until_orphan_watchdog_ist(now), 55_500 - now);
    }

    #[test]
    fn test_seconds_until_watchdog_exactly_at_boundary() {
        // At exactly 15:25:00 IST the watchdog should fire immediately.
        assert_eq!(seconds_until_orphan_watchdog_ist(55_500), 0);
    }

    #[test]
    fn test_seconds_until_watchdog_after_boundary_rolls_to_tomorrow() {
        // 15:30:00 IST (close) → next 15:25:00 IST tomorrow.
        // = SECONDS_PER_DAY - 55_800 + 55_500 = 86_400 - 300 = 86_100.
        let now = 15 * 3600 + 30 * 60;
        assert_eq!(seconds_until_orphan_watchdog_ist(now), 86_100);
    }

    #[test]
    fn test_seconds_until_watchdog_late_night() {
        // 23:59:59 IST → tomorrow 15:25 = 1s + 55_500 = 55_501.
        let now = 24 * 3600 - 1;
        assert_eq!(seconds_until_orphan_watchdog_ist(now), 55_501);
    }

    #[test]
    fn test_ist_seconds_of_day_from_utc_at_utc_midnight() {
        // UTC 00:00 → IST 05:30 = 5*3600 + 30*60 = 19_800.
        assert_eq!(ist_seconds_of_day_from_utc(0), 19_800);
    }

    #[test]
    fn test_ist_seconds_of_day_wraps_across_utc_midnight() {
        // UTC 19:30 → IST 01:00 next day = 3_600.
        let utc = 19 * 3600 + 30 * 60;
        assert_eq!(ist_seconds_of_day_from_utc(utc as i64), 3_600);
    }

    #[test]
    fn test_sleep_duration_until_orphan_watchdog_at_known_clock() {
        // UTC 04:25 → IST 09:55. Next IST 15:25 = 5h30m = 19_800s.
        let utc = 4 * 3600 + 25 * 60;
        let dur = sleep_duration_until_orphan_watchdog(utc);
        assert_eq!(dur.as_secs(), 19_800);
    }

    /// I-P1-11 ratchet: the OrphanPositionInfo struct MUST carry
    /// `exchange_segment` alongside `security_id` so the downstream
    /// audit-row write uses the composite key.
    #[test]
    fn test_orphan_position_info_carries_exchange_segment() {
        let positions = vec![position("72271", "NSE_FNO", 50)];
        match evaluate_orphan_positions(&positions) {
            OrphanPositionVerdict::OrphansDetected { orphans, .. } => {
                assert!(
                    !orphans[0].exchange_segment.is_empty(),
                    "exchange_segment MUST be populated for I-P1-11 \
                     composite-key audit"
                );
            }
            v => panic!("expected OrphansDetected, got {:?}", v),
        }
    }
}

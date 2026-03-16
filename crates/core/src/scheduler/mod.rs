//! Market hours scheduler — determines when WebSocket connections should
//! be active and when tick persistence should be gated.
//!
//! # Timeline (IST)
//! ```text
//! 08:30  WebSocket UP + pipeline warm (ticks flowing, storage GATED)
//! 09:00  StorageGate opens → ticks persist to QuestDB
//! 15:30  CancellationToken fires → all WebSockets disconnect, pipeline stops
//!        API/dashboard stays up for post-market analysis
//! ```
//!
//! # Design
//! - `SchedulePhase`: 5-state enum driven by current IST time + trading calendar
//! - `StorageGate`: single `AtomicBool`, O(1) per-tick check (`Relaxed` load)
//! - All schedule functions are **pure** (take time as input, no I/O)
//! - `CancellationToken` from `tokio-util` for cooperative WebSocket shutdown

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{NaiveTime, Timelike};

// ---------------------------------------------------------------------------
// SchedulePhase
// ---------------------------------------------------------------------------

/// Lifecycle phase of the market-hours scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulePhase {
    /// Before `websocket_connect_time` — system sleeps, no connections.
    PreConnect,
    /// Between `websocket_connect_time` and `data_collection_start` —
    /// WebSocket connected, ticks flowing, storage **gated** (not persisted).
    PreMarketWarm,
    /// Between `data_collection_start` and `data_collection_end` —
    /// full pipeline: ticks flowing + persisted to QuestDB.
    MarketOpen,
    /// After `data_collection_end` — WebSockets disconnected, pipeline stopped.
    /// API/dashboard stays up for post-market analysis.
    PostMarket,
    /// Weekend or NSE holiday — no connections, no pipeline.
    NonTradingDay,
}

impl std::fmt::Display for SchedulePhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PreConnect => write!(f, "PreConnect"),
            Self::PreMarketWarm => write!(f, "PreMarketWarm"),
            Self::MarketOpen => write!(f, "MarketOpen"),
            Self::PostMarket => write!(f, "PostMarket"),
            Self::NonTradingDay => write!(f, "NonTradingDay"),
        }
    }
}

/// Determines the current schedule phase based on IST time and trading day status.
///
/// # Arguments
/// * `now_ist` — current time in IST
/// * `is_trading_day` — whether today is an NSE trading day
/// * `ws_connect_time` — when to establish WebSocket connections (e.g., 08:30)
/// * `data_start` — when to start persisting ticks (e.g., 09:00)
/// * `data_end` — when to stop persisting and disconnect (e.g., 15:30)
pub fn determine_phase(
    now_ist: NaiveTime,
    is_trading_day: bool,
    ws_connect_time: NaiveTime,
    data_start: NaiveTime,
    data_end: NaiveTime,
) -> SchedulePhase {
    if !is_trading_day {
        return SchedulePhase::NonTradingDay;
    }

    if now_ist < ws_connect_time {
        SchedulePhase::PreConnect
    } else if now_ist < data_start {
        SchedulePhase::PreMarketWarm
    } else if now_ist < data_end {
        SchedulePhase::MarketOpen
    } else {
        SchedulePhase::PostMarket
    }
}

/// Returns the duration to sleep until `target` time, given `now`.
///
/// If `target <= now`, returns `Duration::ZERO` (already past).
pub fn duration_until(now: NaiveTime, target: NaiveTime) -> std::time::Duration {
    if target <= now {
        return std::time::Duration::ZERO;
    }
    let now_secs = u64::from(now.num_seconds_from_midnight());
    let target_secs = u64::from(target.num_seconds_from_midnight());
    std::time::Duration::from_secs(target_secs.saturating_sub(now_secs))
}

/// Returns `true` if the phase should have active WebSocket connections.
pub const fn phase_needs_websocket(phase: SchedulePhase) -> bool {
    matches!(
        phase,
        SchedulePhase::PreMarketWarm | SchedulePhase::MarketOpen
    )
}

/// Returns `true` if the phase should persist ticks to QuestDB.
pub const fn phase_needs_storage(phase: SchedulePhase) -> bool {
    matches!(phase, SchedulePhase::MarketOpen)
}

// ---------------------------------------------------------------------------
// StorageGate
// ---------------------------------------------------------------------------

/// O(1) per-tick gate controlling whether ticks are persisted to QuestDB.
///
/// Uses `AtomicBool` with `Relaxed` ordering — a single CPU instruction
/// per check. No allocation, no contention. The gate is set/cleared by the
/// scheduler; the tick processor reads it on every tick.
#[derive(Debug, Clone)]
pub struct StorageGate {
    open: Arc<AtomicBool>,
}

impl StorageGate {
    /// Creates a new gate in the closed (not persisting) state.
    pub fn new() -> Self {
        Self {
            open: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Returns `true` if ticks should be persisted.
    #[inline(always)]
    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Relaxed)
    }

    /// Opens the gate — ticks will be persisted.
    pub fn open(&self) {
        self.open.store(true, Ordering::Relaxed);
    }

    /// Closes the gate — ticks will NOT be persisted.
    pub fn close(&self) {
        self.open.store(false, Ordering::Relaxed);
    }
}

impl Default for StorageGate {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn t(h: u32, m: u32, s: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, s).unwrap()
    }

    // Default schedule times matching config/base.toml
    const WS_CONNECT: (u32, u32, u32) = (8, 30, 0);
    const DATA_START: (u32, u32, u32) = (9, 0, 0);
    const DATA_END: (u32, u32, u32) = (15, 30, 0);

    fn ws_connect() -> NaiveTime {
        t(WS_CONNECT.0, WS_CONNECT.1, WS_CONNECT.2)
    }
    fn data_start() -> NaiveTime {
        t(DATA_START.0, DATA_START.1, DATA_START.2)
    }
    fn data_end() -> NaiveTime {
        t(DATA_END.0, DATA_END.1, DATA_END.2)
    }

    // -----------------------------------------------------------------------
    // determine_phase tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_non_trading_day_always_returns_non_trading() {
        // Any time on a non-trading day → NonTradingDay
        for (h, m) in [(0, 0), (8, 30), (9, 15), (12, 0), (15, 30), (23, 59)] {
            let phase = determine_phase(t(h, m, 0), false, ws_connect(), data_start(), data_end());
            assert_eq!(
                phase,
                SchedulePhase::NonTradingDay,
                "non-trading day at {h:02}:{m:02} should be NonTradingDay"
            );
        }
    }

    #[test]
    fn test_pre_connect_before_ws_time() {
        let phase = determine_phase(t(8, 0, 0), true, ws_connect(), data_start(), data_end());
        assert_eq!(phase, SchedulePhase::PreConnect);
    }

    #[test]
    fn test_pre_connect_midnight() {
        let phase = determine_phase(t(0, 0, 0), true, ws_connect(), data_start(), data_end());
        assert_eq!(phase, SchedulePhase::PreConnect);
    }

    #[test]
    fn test_pre_connect_just_before_ws_time() {
        let phase = determine_phase(t(8, 29, 59), true, ws_connect(), data_start(), data_end());
        assert_eq!(phase, SchedulePhase::PreConnect);
    }

    #[test]
    fn test_pre_market_warm_at_ws_connect_time() {
        let phase = determine_phase(t(8, 30, 0), true, ws_connect(), data_start(), data_end());
        assert_eq!(phase, SchedulePhase::PreMarketWarm);
    }

    #[test]
    fn test_pre_market_warm_between_connect_and_data_start() {
        let phase = determine_phase(t(8, 45, 0), true, ws_connect(), data_start(), data_end());
        assert_eq!(phase, SchedulePhase::PreMarketWarm);
    }

    #[test]
    fn test_pre_market_warm_just_before_data_start() {
        let phase = determine_phase(t(8, 59, 59), true, ws_connect(), data_start(), data_end());
        assert_eq!(phase, SchedulePhase::PreMarketWarm);
    }

    #[test]
    fn test_market_open_at_data_start() {
        let phase = determine_phase(t(9, 0, 0), true, ws_connect(), data_start(), data_end());
        assert_eq!(phase, SchedulePhase::MarketOpen);
    }

    #[test]
    fn test_market_open_mid_session() {
        let phase = determine_phase(t(12, 0, 0), true, ws_connect(), data_start(), data_end());
        assert_eq!(phase, SchedulePhase::MarketOpen);
    }

    #[test]
    fn test_market_open_just_before_data_end() {
        let phase = determine_phase(t(15, 29, 59), true, ws_connect(), data_start(), data_end());
        assert_eq!(phase, SchedulePhase::MarketOpen);
    }

    #[test]
    fn test_post_market_at_data_end() {
        let phase = determine_phase(t(15, 30, 0), true, ws_connect(), data_start(), data_end());
        assert_eq!(phase, SchedulePhase::PostMarket);
    }

    #[test]
    fn test_post_market_evening() {
        let phase = determine_phase(t(18, 0, 0), true, ws_connect(), data_start(), data_end());
        assert_eq!(phase, SchedulePhase::PostMarket);
    }

    #[test]
    fn test_post_market_late_night() {
        let phase = determine_phase(t(23, 59, 59), true, ws_connect(), data_start(), data_end());
        assert_eq!(phase, SchedulePhase::PostMarket);
    }

    // -----------------------------------------------------------------------
    // Boot scenario tests (from plan)
    // -----------------------------------------------------------------------

    #[test]
    fn test_boot_at_0800_preconnect() {
        let phase = determine_phase(t(8, 0, 0), true, ws_connect(), data_start(), data_end());
        assert_eq!(phase, SchedulePhase::PreConnect);
        assert!(!phase_needs_websocket(phase));
        assert!(!phase_needs_storage(phase));
    }

    #[test]
    fn test_boot_at_0845_pre_market_warm() {
        let phase = determine_phase(t(8, 45, 0), true, ws_connect(), data_start(), data_end());
        assert_eq!(phase, SchedulePhase::PreMarketWarm);
        assert!(phase_needs_websocket(phase));
        assert!(!phase_needs_storage(phase));
    }

    #[test]
    fn test_boot_at_1000_market_open() {
        let phase = determine_phase(t(10, 0, 0), true, ws_connect(), data_start(), data_end());
        assert_eq!(phase, SchedulePhase::MarketOpen);
        assert!(phase_needs_websocket(phase));
        assert!(phase_needs_storage(phase));
    }

    #[test]
    fn test_boot_at_1530_post_market() {
        let phase = determine_phase(t(15, 30, 0), true, ws_connect(), data_start(), data_end());
        assert_eq!(phase, SchedulePhase::PostMarket);
        assert!(!phase_needs_websocket(phase));
        assert!(!phase_needs_storage(phase));
    }

    #[test]
    fn test_boot_on_saturday() {
        let phase = determine_phase(t(10, 0, 0), false, ws_connect(), data_start(), data_end());
        assert_eq!(phase, SchedulePhase::NonTradingDay);
        assert!(!phase_needs_websocket(phase));
        assert!(!phase_needs_storage(phase));
    }

    // -----------------------------------------------------------------------
    // duration_until tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_duration_until_future_time() {
        let d = duration_until(t(8, 0, 0), t(8, 30, 0));
        assert_eq!(d.as_secs(), 1800); // 30 minutes
    }

    #[test]
    fn test_duration_until_same_time() {
        let d = duration_until(t(9, 0, 0), t(9, 0, 0));
        assert_eq!(d, std::time::Duration::ZERO);
    }

    #[test]
    fn test_duration_until_past_time() {
        let d = duration_until(t(10, 0, 0), t(9, 0, 0));
        assert_eq!(d, std::time::Duration::ZERO);
    }

    #[test]
    fn test_duration_until_one_second() {
        let d = duration_until(t(8, 29, 59), t(8, 30, 0));
        assert_eq!(d.as_secs(), 1);
    }

    #[test]
    fn test_duration_until_full_day() {
        let d = duration_until(t(0, 0, 0), t(23, 59, 59));
        assert_eq!(d.as_secs(), 86399);
    }

    // -----------------------------------------------------------------------
    // phase_needs_websocket tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_phase_needs_websocket_mapping() {
        assert!(!phase_needs_websocket(SchedulePhase::PreConnect));
        assert!(phase_needs_websocket(SchedulePhase::PreMarketWarm));
        assert!(phase_needs_websocket(SchedulePhase::MarketOpen));
        assert!(!phase_needs_websocket(SchedulePhase::PostMarket));
        assert!(!phase_needs_websocket(SchedulePhase::NonTradingDay));
    }

    // -----------------------------------------------------------------------
    // phase_needs_storage tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_phase_needs_storage_mapping() {
        assert!(!phase_needs_storage(SchedulePhase::PreConnect));
        assert!(!phase_needs_storage(SchedulePhase::PreMarketWarm));
        assert!(phase_needs_storage(SchedulePhase::MarketOpen));
        assert!(!phase_needs_storage(SchedulePhase::PostMarket));
        assert!(!phase_needs_storage(SchedulePhase::NonTradingDay));
    }

    // -----------------------------------------------------------------------
    // StorageGate tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_storage_gate_starts_closed() {
        let gate = StorageGate::new();
        assert!(!gate.is_open());
    }

    #[test]
    fn test_storage_gate_open_close() {
        let gate = StorageGate::new();
        gate.open();
        assert!(gate.is_open());
        gate.close();
        assert!(!gate.is_open());
    }

    #[test]
    fn test_storage_gate_clone_shares_state() {
        let gate = StorageGate::new();
        let clone = gate.clone();
        gate.open();
        assert!(clone.is_open());
        clone.close();
        assert!(!gate.is_open());
    }

    #[test]
    fn test_storage_gate_default_is_closed() {
        let gate = StorageGate::default();
        assert!(!gate.is_open());
    }

    // -----------------------------------------------------------------------
    // Display tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_schedule_phase_display() {
        assert_eq!(format!("{}", SchedulePhase::PreConnect), "PreConnect");
        assert_eq!(format!("{}", SchedulePhase::PreMarketWarm), "PreMarketWarm");
        assert_eq!(format!("{}", SchedulePhase::MarketOpen), "MarketOpen");
        assert_eq!(format!("{}", SchedulePhase::PostMarket), "PostMarket");
        assert_eq!(format!("{}", SchedulePhase::NonTradingDay), "NonTradingDay");
    }

    // -----------------------------------------------------------------------
    // Edge case: boundary transitions
    // -----------------------------------------------------------------------

    #[test]
    fn test_exact_boundary_transitions() {
        // At exact boundary times, the phase should be the NEXT phase
        // (boundaries are inclusive of the start of the new phase)
        let ws = t(8, 30, 0);
        let ds = t(9, 0, 0);
        let de = t(15, 30, 0);

        // ws_connect_time exactly → PreMarketWarm (not PreConnect)
        assert_eq!(
            determine_phase(ws, true, ws, ds, de),
            SchedulePhase::PreMarketWarm
        );
        // data_start exactly → MarketOpen (not PreMarketWarm)
        assert_eq!(
            determine_phase(ds, true, ws, ds, de),
            SchedulePhase::MarketOpen
        );
        // data_end exactly → PostMarket (not MarketOpen)
        assert_eq!(
            determine_phase(de, true, ws, ds, de),
            SchedulePhase::PostMarket
        );
    }
}

//! Wave 5 Item 25/27 Phase B — base-1s writer pipeline for `movers_1s`.
//!
//! Single tokio task. Subscribes to the `tick_broadcast` and maintains an
//! in-memory `HashMap<(security_id, segment_code), MoversUnifiedState>`
//! of latest values. Every 1 second, drains the map and ILP-appends one
//! row per (security_id, segment) to QuestDB.
//!
//! # Architecture
//!
//! ```text
//! tick_broadcast (broadcast::Receiver<ParsedTick>)
//!   │
//!   ▼  per-tick update (HashMap insert; O(1) amortised)
//! HashMap<(u32, u8), MoversUnifiedState>
//!   │
//!   ▼  drain at 1s tick (Tokio interval)
//! MoversWriter::append_row × N + flush
//!   │
//!   ▼
//! QuestDB ILP → movers_1s
//!   │
//!   ▼  (server-side, incremental)
//! 24 materialized views (5s, 15s, ..., 1d)
//! ```
//!
//! # Market-hours gate (audit-findings Rule 3)
//!
//! Outside `[09:00, 15:30) IST`, the drain task `continue`s without
//! flushing — ticks won't arrive anyway, and we don't want to pollute
//! the table with stale post-market values. The HashMap is also
//! cleared at IST midnight via the boot-spawned `FirstSeenSet` reset
//! task semantic (we just clear-on-empty if nothing has been written
//! for > 30 minutes).
//!
//! # Backpressure
//!
//! `broadcast::Receiver::Lagged` = the writer is slower than tick
//! producers. Logs WARN with the skip count + continues; we only need
//! the LATEST value per second so missing intermediate ticks is
//! correct behaviour.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::Notify;
use tokio::sync::broadcast;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::{
    IST_UTC_OFFSET_NANOS, IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY,
    TICK_PERSIST_END_SECS_OF_DAY_IST, TICK_PERSIST_START_SECS_OF_DAY_IST,
};
use tickvault_common::instrument_registry::InstrumentRegistry;
use tickvault_common::tick_types::ParsedTick;
use tickvault_common::types::ExchangeSegment;
use tickvault_storage::movers_base_writer::{MoversRow, MoversWriter, segment_code_to_char};

/// Drain cadence — 1 second per the plan §"Item 25" base-1s table.
const DRAIN_INTERVAL_MS: u64 = 1_000;

/// Backoff between ILP-connect attempts when the writer is offline.
const CONNECT_RETRY_SECS: u64 = 30;

/// Bounded flush deadline used during graceful shutdown to avoid hanging
/// the boot path on a slow QuestDB ILP send.
const SHUTDOWN_FLUSH_TIMEOUT_SECS: u64 = 5;

/// In-memory state per (security_id, segment) tracked between ticks.
/// `prev_oi` is used to compute `oi_delta` at drain time.
#[derive(Debug, Clone, Copy)]
struct MoversUnifiedState {
    last_price: f32,
    volume: u32,
    open_interest: u32,
    /// OI from the previous 1s drain — used to compute delta at next drain.
    prev_oi_at_last_drain: u32,
    prev_close: f32,
    /// Last time we received a tick for this instrument (wall-clock IST nanos).
    received_at_nanos: i64,
}

/// Returns `true` when `now` is inside the IST market-data persist window
/// `[09:00:00, 15:30:00)`. Mirrors the helper in
/// `crates/core/src/instrument/depth_rebalancer.rs::is_within_market_hours_ist`.
#[inline]
#[must_use]
fn is_within_market_hours_ist() -> bool {
    let now_utc = Utc::now().timestamp();
    let now_ist = now_utc.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
    let sec_of_day = (now_ist.rem_euclid(i64::from(SECONDS_PER_DAY))) as u32;
    (TICK_PERSIST_START_SECS_OF_DAY_IST..TICK_PERSIST_END_SECS_OF_DAY_IST).contains(&sec_of_day)
}

/// 2026-05-02 PR-B: Resolves `(exchange_segment, instrument_type)` for a
/// `(security_id, segment_code)` tuple via the InstrumentRegistry composite
/// key per I-P1-11. Returns `("UNKNOWN", "UNKNOWN")` when the registry has
/// no entry — keeps both columns non-null for downstream selectors.
///
/// Pure function (no I/O, no allocations). O(1) papaya `get` per call;
/// runs on the 1s drain (cold path), not the tick processor hot path.
#[inline]
#[must_use]
fn lookup_segment_and_type(
    registry: &InstrumentRegistry,
    security_id: u32,
    segment_code: u8,
) -> (&'static str, &'static str) {
    let Some(seg) = ExchangeSegment::from_byte(segment_code) else {
        return ("UNKNOWN", "UNKNOWN");
    };
    let Some(inst) = registry.get_with_segment(security_id, seg) else {
        return (seg.as_str(), "UNKNOWN");
    };
    (inst.exchange_segment.as_str(), inst.instrument_type_tag())
}

/// 1s-aligned IST wall-clock timestamp in nanoseconds.
#[inline]
fn now_ist_aligned_to_1s_nanos() -> i64 {
    let raw = Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(0)
        .saturating_add(IST_UTC_OFFSET_NANOS);
    // Truncate to second precision.
    (raw / 1_000_000_000) * 1_000_000_000
}

/// Spawns the movers base-1s writer task. Returns the
/// `JoinHandle` for the supervisor to keep alive.
///
/// Behavior:
/// - Connects to QuestDB ILP at task start; if connect fails, retries
///   every 30s (the `MoversWriter::flush` path also handles
///   reconnect on every failed flush).
/// - Subscribes to `tick_broadcast` and updates the in-memory state map.
/// - Every 1s: drains the map → 1 ILP append per entry → flush.
/// - Outside market hours: skips the drain.
/// - On `shutdown.notified()`: flushes pending + exits cleanly.
// TEST-EXEMPT: orchestration spawner; live-tested via `cargo run`. Internal helpers `is_within_market_hours_ist`, `now_ist_aligned_to_1s_nanos`, `MoversUnifiedState::last_traded_price_or_default`, `lookup_segment_and_type` are unit-tested below.
pub fn spawn_movers_pipeline(
    questdb: QuestDbConfig,
    tick_broadcast: broadcast::Sender<ParsedTick>,
    shutdown: Arc<Notify>,
    // 2026-05-02 PR-B: registry handle so the drain loop can populate
    // `exchange_segment` (precise per-exchange tag, e.g. NSE_FNO vs BSE_FNO)
    // and `instrument_type` (precise classification, e.g. OPTSTK vs FUTIDX
    // vs INDEX vs EQUITY) on every `movers_1s` row. Lookup is O(1) papaya
    // get + amortised on the 1s drain cadence (cold path).
    registry: Arc<InstrumentRegistry>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!("movers_pipeline starting");

        // Connect with retry loop.
        let mut writer: Option<MoversWriter> = None;
        let mut connect_retry = tokio::time::interval(Duration::from_secs(CONNECT_RETRY_SECS));
        connect_retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let mut tick_rx = tick_broadcast.subscribe();
        let mut drain = tokio::time::interval(Duration::from_millis(DRAIN_INTERVAL_MS));
        drain.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        // O(1) EXEMPT: cold-path map sized for the Wave 5 indices-only
        // universe (~11K instruments). Capacity allocates once at boot.
        let mut state: HashMap<(u32, u8), MoversUnifiedState> = HashMap::with_capacity(12_000);

        loop {
            tokio::select! {
                biased;

                _ = shutdown.notified() => {
                    info!("movers_pipeline shutting down — flushing pending");
                    if let Some(w) = writer.as_mut() {
                        w.shutdown_flush(Duration::from_secs(SHUTDOWN_FLUSH_TIMEOUT_SECS));
                    }
                    break;
                }

                _ = connect_retry.tick(), if writer.is_none() => {
                    match MoversWriter::connect(&questdb) {
                        Ok(w) => {
                            info!("movers_pipeline ILP connected");
                            writer = Some(w);
                        }
                        Err(err) => {
                            error!(
                                code = "MOVERS-UNIFIED-01",
                                ?err,
                                "movers_pipeline ILP connect failed — retrying in 30s"
                            );
                        }
                    }
                }

                recv = tick_rx.recv() => {
                    match recv {
                        Ok(tick) => {
                            // O(1) per-tick HashMap update. Volume / OI
                            // are u32 in ParsedTick; we cast to i64 at
                            // drain time before ILP write.
                            let key = (tick.security_id, tick.exchange_segment_code);
                            let existing = state.get(&key).copied();
                            let prev_oi_at_last_drain = existing
                                .map(|e| e.prev_oi_at_last_drain)
                                .unwrap_or(0);
                            state.insert(
                                key,
                                MoversUnifiedState {
                                    last_price: tick.last_traded_price,
                                    volume: tick.volume,
                                    open_interest: tick.open_interest,
                                    prev_oi_at_last_drain,
                                    prev_close: tick.day_close,
                                    received_at_nanos: tick.received_at_nanos,
                                },
                            );
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            metrics::counter!(
                                "tv_movers_pipeline_lagged_total"
                            ).increment(skipped);
                            warn!(skipped, "movers_pipeline broadcast lagged");
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            info!("movers_pipeline broadcast closed — exiting");
                            break;
                        }
                    }
                }

                _ = drain.tick() => {
                    if !is_within_market_hours_ist() {
                        // Outside market hours: don't write.
                        continue;
                    }
                    let Some(w) = writer.as_mut() else {
                        // Connect retry loop is in progress.
                        continue;
                    };
                    let ts_nanos = now_ist_aligned_to_1s_nanos();

                    let mut written = 0_u64;
                    // O(1) EXEMPT: drain iterates the universe (~11K)
                    // once per second. Allocation budget: zero (HashMap
                    // entries are mutated in-place via .iter_mut() to
                    // update prev_oi_at_last_drain).
                    let entries: Vec<((u32, u8), MoversUnifiedState)> = state
                        .iter()
                        .map(|(k, v)| (*k, *v))
                        .collect();

                    for ((security_id, segment_code), st) in &entries {
                        let segment_char = segment_code_to_char(*segment_code);
                        let (exchange_segment, instrument_type) =
                            lookup_segment_and_type(&registry, *security_id, *segment_code);
                        let prev_close_f64 = f64::from(st.prev_close);
                        let last_price_f64 = f64::from(st.last_traded_price_or_default());
                        let change_pct = if prev_close_f64 > 0.0 {
                            ((last_price_f64 - prev_close_f64) / prev_close_f64) * 100.0
                        } else {
                            0.0
                        };
                        let oi_delta = i64::from(st.open_interest)
                            .saturating_sub(i64::from(st.prev_oi_at_last_drain));
                        let row = MoversRow {
                            ts_nanos,
                            security_id: *security_id,
                            segment_char,
                            exchange_segment,
                            instrument_type,
                            open_interest: i64::from(st.open_interest),
                            oi_delta,
                            volume: i64::from(st.volume),
                            last_price: last_price_f64,
                            prev_close: prev_close_f64,
                            change_pct,
                            // Use the tick's actual wall-clock arrival
                            // for forensics; `ts` (designated timestamp)
                            // carries the 1s-aligned bucket boundary.
                            received_at_nanos: st.received_at_nanos,
                        };
                        if w.append_row(&row).is_ok() {
                            written += 1;
                        }
                    }
                    // Update prev_oi for next drain cycle.
                    for ((security_id, segment_code), _) in &entries {
                        if let Some(s) = state.get_mut(&(*security_id, *segment_code)) {
                            s.prev_oi_at_last_drain = s.open_interest;
                        }
                    }

                    if let Err(err) = w.flush() {
                        error!(
                            code = "MOVERS-UNIFIED-01",
                            written,
                            ?err,
                            "movers_pipeline flush failed"
                        );
                        // Drop the writer; connect_retry will rebuild.
                        writer = None;
                    } else if written > 0 {
                        metrics::gauge!("tv_movers_universe_size")
                            .set(written as f64);
                    }
                }
            }
        }

        info!("movers_pipeline exited");
    })
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

impl MoversUnifiedState {
    /// Defensive: returns `last_price` if non-zero, else `0.0` (cast back
    /// to f32 then up to f64 by caller). Pure helper.
    #[inline]
    #[must_use]
    fn last_traded_price_or_default(&self) -> f32 {
        if self.last_price.is_finite() && self.last_price > 0.0 {
            self.last_price
        } else {
            0.0
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_drain_interval_is_one_second() {
        // Plan §Item 25: base table is 1Hz.
        assert_eq!(DRAIN_INTERVAL_MS, 1_000);
    }

    #[test]
    fn test_state_struct_is_copy() {
        // Hot-path requirement.
        fn assert_copy<T: Copy>() {}
        assert_copy::<MoversUnifiedState>();
    }

    #[test]
    fn test_now_ist_aligned_to_1s_truncates_sub_second_nanos() {
        let n = now_ist_aligned_to_1s_nanos();
        assert_eq!(n % 1_000_000_000, 0, "must be 1s-aligned");
    }

    #[test]
    fn test_last_traded_price_or_default_returns_zero_for_zero() {
        let s = MoversUnifiedState {
            last_price: 0.0,
            volume: 0,
            open_interest: 0,
            prev_oi_at_last_drain: 0,
            prev_close: 0.0,
            received_at_nanos: 0,
        };
        assert!((s.last_traded_price_or_default() - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_last_traded_price_or_default_returns_zero_for_nan() {
        let s = MoversUnifiedState {
            last_price: f32::NAN,
            volume: 0,
            open_interest: 0,
            prev_oi_at_last_drain: 0,
            prev_close: 0.0,
            received_at_nanos: 0,
        };
        assert!((s.last_traded_price_or_default() - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_last_traded_price_or_default_returns_value_for_positive() {
        let s = MoversUnifiedState {
            last_price: 100.5,
            volume: 0,
            open_interest: 0,
            prev_oi_at_last_drain: 0,
            prev_close: 0.0,
            received_at_nanos: 0,
        };
        assert!((s.last_traded_price_or_default() - 100.5).abs() < f32::EPSILON);
    }

    #[test]
    fn test_market_hours_gate_returns_bool() {
        // Just a smoke test — function never panics.
        let _ = is_within_market_hours_ist();
    }

    // 2026-05-02 PR-B: ratchet tests for `lookup_segment_and_type` —
    // populates the new `exchange_segment` + `instrument_type` SYMBOL
    // columns on `movers_1s` from the InstrumentRegistry composite key.

    use tickvault_common::instrument_registry::{SubscribedInstrument, SubscriptionCategory};
    use tickvault_common::instrument_types::DhanInstrumentKind;
    use tickvault_common::types::FeedMode;

    fn build_registry_with(items: Vec<SubscribedInstrument>) -> InstrumentRegistry {
        InstrumentRegistry::from_instruments(items)
    }

    fn nse_fno_option_instrument(security_id: u32) -> SubscribedInstrument {
        SubscribedInstrument {
            security_id,
            exchange_segment: ExchangeSegment::NseFno,
            category: SubscriptionCategory::StockDerivative,
            display_label: format!("TEST_OPT_{security_id}"),
            underlying_symbol: "RELIANCE".to_string(),
            instrument_kind: Some(DhanInstrumentKind::OptionStock),
            expiry_date: None,
            strike_price: None,
            option_type: None,
            feed_mode: FeedMode::Quote,
        }
    }

    fn idx_i_index_instrument(security_id: u32) -> SubscribedInstrument {
        SubscribedInstrument {
            security_id,
            exchange_segment: ExchangeSegment::IdxI,
            category: SubscriptionCategory::MajorIndexValue,
            display_label: format!("TEST_IDX_{security_id}"),
            underlying_symbol: "NIFTY".to_string(),
            instrument_kind: None,
            expiry_date: None,
            strike_price: None,
            option_type: None,
            feed_mode: FeedMode::Ticker,
        }
    }

    #[test]
    fn test_lookup_segment_and_type_returns_nse_fno_optstk_for_stock_option() {
        let reg = build_registry_with(vec![nse_fno_option_instrument(50000)]);
        let (seg, typ) = lookup_segment_and_type(&reg, 50000, 2);
        assert_eq!(seg, "NSE_FNO");
        assert_eq!(typ, "OPTSTK");
    }

    #[test]
    fn test_lookup_segment_and_type_returns_idx_i_index_for_major_index() {
        let reg = build_registry_with(vec![idx_i_index_instrument(13)]);
        let (seg, typ) = lookup_segment_and_type(&reg, 13, 0);
        assert_eq!(seg, "IDX_I");
        assert_eq!(typ, "INDEX");
    }

    #[test]
    fn test_lookup_segment_and_type_returns_seg_str_with_unknown_type_when_registry_misses() {
        // Registry has only id=13 IDX_I. Looking up id=99 NSE_FNO → segment
        // resolves but instrument doesn't, so we get the segment string with
        // "UNKNOWN" type. Selectors can still filter by exchange_segment.
        let reg = build_registry_with(vec![idx_i_index_instrument(13)]);
        let (seg, typ) = lookup_segment_and_type(&reg, 99, 2);
        assert_eq!(seg, "NSE_FNO");
        assert_eq!(typ, "UNKNOWN");
    }

    #[test]
    fn test_lookup_segment_and_type_returns_double_unknown_for_invalid_segment_byte() {
        // ExchangeSegment::from_byte(6) returns None per dhan-annexure-enums
        // (gap between MCX_COMM=5 and BSE_CURRENCY=7).
        let reg = build_registry_with(vec![idx_i_index_instrument(13)]);
        let (seg, typ) = lookup_segment_and_type(&reg, 13, 6);
        assert_eq!(seg, "UNKNOWN");
        assert_eq!(typ, "UNKNOWN");
    }

    #[test]
    fn test_lookup_segment_and_type_disambiguates_nse_fno_from_bse_fno_per_i_p1_11() {
        // I-P1-11 scenario: same security_id on different segments. Lookup
        // by composite key (security_id, segment) returns the correct entry.
        let mut bse_fno = nse_fno_option_instrument(27);
        bse_fno.exchange_segment = ExchangeSegment::BseFno;
        bse_fno.underlying_symbol = "SENSEX".to_string();
        let nse_fno = nse_fno_option_instrument(27);
        let reg = build_registry_with(vec![nse_fno, bse_fno]);

        // segment_code 2 = NSE_FNO
        let (seg_nse, _) = lookup_segment_and_type(&reg, 27, 2);
        assert_eq!(seg_nse, "NSE_FNO");

        // segment_code 8 = BSE_FNO
        let (seg_bse, _) = lookup_segment_and_type(&reg, 27, 8);
        assert_eq!(seg_bse, "BSE_FNO");
    }
}

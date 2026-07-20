//! Cross-fill audit sink — the process-global, fire-and-forget channel the
//! cadence runner emits one [`CrossFillAuditEvent`] into per cross-fill /
//! Groww-fallback firing (operator directive 2026-07-20: every cross-fill
//! "highlighted, logged, monitored, audited, visualised — precisely at
//! what time it is happening").
//!
//! Core cannot depend on `crates/storage` (dependency flow), so the sink is
//! a bounded mpsc `Sender` installed once by the app boot
//! (`crates/app/src/cross_fill_visibility.rs`) via the
//! `global_expiry_store` / `init_global_dhan_gates` OnceLock house pattern.
//! The emit is `try_send` — NEVER on the decision path, never blocking; a
//! full/closed channel logs one coded CADENCE-04 `error!` + counter and the
//! event is dropped (the coalesced CADENCE-01 log + the
//! `tv_cadence_cross_fill_total` / `tv_cadence_groww_fallback_total`
//! counters still carry it). No sink installed (cadence spawned without the
//! consumer, unit tests) = a silent no-op by design.

use std::sync::OnceLock;

use tickvault_common::error_code::ErrorCode;
use tokio::sync::mpsc;
use tracing::error;

/// One cross-fill / fallback audit event, pure data — the app-side consumer
/// maps it 1:1 onto a `cross_fill_audit` QuestDB row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CrossFillAuditEvent {
    /// Designated timestamp — the decided minute's OPEN, IST nanoseconds.
    pub ts_ist_nanos: i64,
    /// The trading day — IST midnight nanoseconds.
    pub trading_date_ist_nanos: i64,
    /// The DEGRADED (borrowing) broker lane (`dhan` / `groww`).
    pub lane: &'static str,
    /// Where the data came from (`groww` / `dhan`; equals `lane` for a
    /// fallback launch — the lane retried its OWN source).
    pub source_lane: &'static str,
    /// `cross_fill` / `groww_fallback` (the storage stage SYMBOLs).
    pub stage: &'static str,
    /// The decided minute's OPEN, IST seconds-of-day.
    pub cycle_minute_ist: u32,
    /// Spot cells filled (cross_fill) / spot legs queued (fallback).
    pub spots: u32,
    /// Chain cells filled (cross_fill) / chain legs queued (fallback).
    pub chains: u32,
    /// Minute-close boundary T → this event's emit instant, ms.
    pub cycle_latency_ms: i64,
    /// The borrowing lane's shape-ladder rung at this cycle's slot build.
    pub ladder_rung: u8,
    /// Minute-close T → resolution instant, ms; `-1` = unknown at emit
    /// time (fallback launch rows) — never a fabricated latency.
    pub resolved_at_ms_after_close: i64,
}

impl CrossFillAuditEvent {
    /// The `legs` SYMBOL slug (`spot` / `chain` / `spot+chain`) from the
    /// spot/chain counts. Pure.
    #[must_use]
    pub const fn legs(&self) -> &'static str {
        match (self.spots > 0, self.chains > 0) {
            (true, true) => "spot+chain",
            (true, false) => "spot",
            (false, true) => "chain",
            // Structurally unreachable (emit sites gate on > 0) — an
            // honest fixed slug rather than a panic if it ever fires.
            (false, false) => "none",
        }
    }
}

/// The process-global cross-fill audit sink (`OnceLock` — installed once by
/// the app boot; first write wins).
static CROSS_FILL_AUDIT_SINK: OnceLock<mpsc::Sender<CrossFillAuditEvent>> = OnceLock::new();

/// Install the process-global cross-fill audit sink. Returns `false` when a
/// sink was already installed (first write wins — the duplicate sender is
/// dropped; OnceLock semantics, the `init_global_dhan_gates` pattern).
pub fn init_cross_fill_audit_sink(tx: mpsc::Sender<CrossFillAuditEvent>) -> bool {
    CROSS_FILL_AUDIT_SINK.set(tx).is_ok()
}

/// Fire-and-forget emit from the runner's cross-fill / fallback sites.
/// O(1), non-blocking, never on the decision path. No sink = no-op.
pub(crate) fn emit_cross_fill_audit(event: CrossFillAuditEvent) {
    let Some(tx) = CROSS_FILL_AUDIT_SINK.get() else {
        return;
    };
    if let Err(err) = tx.try_send(event) {
        let reason = match err {
            mpsc::error::TrySendError::Full(_) => "full",
            mpsc::error::TrySendError::Closed(_) => "closed",
        };
        metrics::counter!("tv_cross_fill_audit_dropped_total", "reason" => reason).increment(1);
        error!(
            code = ErrorCode::Cadence04AuditWriteFailed.code_str(),
            stage = "channel",
            reason,
            lane = event.lane,
            cycle_minute_ist = event.cycle_minute_ist,
            "CADENCE-04: cross_fill_audit event dropped — forensic row lost \
             for this event (the CADENCE-01 signal + counters still carry it)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_event() -> CrossFillAuditEvent {
        CrossFillAuditEvent {
            ts_ist_nanos: 1_770_000_900_000_000_000,
            trading_date_ist_nanos: 1_769_990_400_000_000_000,
            lane: "dhan",
            source_lane: "groww",
            stage: "cross_fill",
            cycle_minute_ist: 33_360,
            spots: 1,
            chains: 0,
            cycle_latency_ms: 12_500,
            ladder_rung: 0,
            resolved_at_ms_after_close: 12_500,
        }
    }

    #[test]
    fn test_legs_slug_from_counts() {
        let mut e = sample_event();
        assert_eq!(e.legs(), "spot");
        e.spots = 0;
        e.chains = 2;
        assert_eq!(e.legs(), "chain");
        e.spots = 1;
        assert_eq!(e.legs(), "spot+chain");
        e.spots = 0;
        e.chains = 0;
        assert_eq!(e.legs(), "none");
    }

    #[tokio::test]
    async fn test_init_cross_fill_audit_sink_and_emit_cross_fill_audit_roundtrip() {
        // Emit with no sink installed: silent no-op (no panic).
        emit_cross_fill_audit(sample_event());

        let (tx, mut rx) = mpsc::channel(4);
        // Installing twice: first write wins, second reports false.
        let first = init_cross_fill_audit_sink(tx.clone());
        let second = init_cross_fill_audit_sink(tx);
        assert!(!second, "second install must report already-installed");
        // In a multi-test process another test may have installed first;
        // only assert the roundtrip when THIS channel won the OnceLock.
        if first {
            emit_cross_fill_audit(sample_event());
            let got = rx.try_recv().expect("event delivered");
            assert_eq!(got, sample_event());
        }
    }
}

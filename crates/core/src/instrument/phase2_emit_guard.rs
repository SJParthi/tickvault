//! Phase 2 outcome-emit guard (Wave 1 Item 1, gap G7).
//!
//! Enforces the invariant that **every** Phase 2 dispatcher invocation MUST
//! emit exactly one of:
//!
//! - `NotificationEvent::Phase2Complete`
//! - `NotificationEvent::Phase2Failed`
//! - `NotificationEvent::Phase2Skipped`
//!
//! before the dispatcher returns. The intermediate
//! `Phase2Started` / `Phase2RunImmediate` events are informational only —
//! they do not satisfy the outcome contract.
//!
//! # Why
//!
//! `phase2_scheduler::run_phase2_scheduler` has 5 distinct early-return
//! paths today (skip / empty plan / dispatch failed / success / retries
//! exhausted). Adding a 6th path in a future edit is a real risk; a
//! silent drop-without-emit means the operator never sees a Phase 2
//! outcome on Telegram and gets no signal that the F&O subscription
//! never happened.
//!
//! `Phase2EmitGuard` closes that hole at runtime via a `Drop` impl:
//!
//! - In `cfg(debug_assertions)` (debug builds, tests) the drop **panics**
//!   so a regression is caught at the first test that exercises the
//!   missing path. The panic message names the invariant.
//! - In release builds the drop emits `error!(code = "PHASE2-02", ...)`
//!   so Loki + Telegram fire on the rising edge — the operator sees the
//!   regression even on production builds.
//!
//! The three `emit_*` methods set the internal `emitted` flag before
//! invoking the notifier; once set the `Drop` impl is a no-op.

use std::sync::Arc;

use metrics::counter;
// `tracing::error!` is used inside `Drop` only on release builds — gate
// the import so the unused-import warning doesn't fire under
// `cfg(debug_assertions)` / `cargo test`.
#[cfg(not(debug_assertions))]
use tracing::error;

use crate::notification::NotificationService;
use crate::notification::events::NotificationEvent;

/// Outcome-emit guard for `run_phase2_scheduler`.
///
/// Construction captures the `NotificationService`. The user calls one
/// of `emit_complete` / `emit_failed` / `emit_skipped` exactly once
/// before letting the guard drop. Forgetting to call any of those is
/// caught by the `Drop` impl.
pub struct Phase2EmitGuard {
    notifier: Arc<NotificationService>,
    emitted: bool,
}

impl Phase2EmitGuard {
    /// Constructs a fresh guard. The guard captures an `Arc` clone of
    /// the notifier so the dispatcher can keep using `notifier` for the
    /// intermediate `Phase2Started` / `Phase2RunImmediate` events.
    pub fn new(notifier: Arc<NotificationService>) -> Self {
        Self {
            notifier,
            emitted: false,
        }
    }

    /// Emits the success outcome and marks the guard as fired.
    pub fn emit_complete(
        &mut self,
        added_count: usize,
        duration_ms: u64,
        depth_20_underlyings: usize,
        depth_200_contracts: usize,
    ) {
        self.notifier.notify(NotificationEvent::Phase2Complete {
            added_count,
            duration_ms,
            depth_20_underlyings,
            depth_200_contracts,
        });
        self.emitted = true;
        Self::audit_async(
            "complete",
            i64::try_from(added_count).unwrap_or(i64::MAX),
            0,
            0,
            "Phase 2 dispatch succeeded",
        );
    }

    /// Emits the failure outcome and marks the guard as fired.
    pub fn emit_failed(&mut self, reason: String, attempts: u32) {
        let diag = format!("attempts={attempts}; {reason}");
        self.notifier
            .notify(NotificationEvent::Phase2Failed { reason, attempts });
        self.emitted = true;
        Self::audit_async("failed", 0, 0, 0, &diag);
    }

    /// Emits the skipped outcome and marks the guard as fired.
    pub fn emit_skipped(&mut self, reason: String) {
        let diag = reason.clone();
        self.notifier
            .notify(NotificationEvent::Phase2Skipped { reason });
        self.emitted = true;
        Self::audit_async("skipped", 0, 0, 0, &diag);
    }

    /// Wave 2 Item 9 (AUDIT-01) — best-effort audit row. If the global
    /// QuestDB config is not installed (e.g., unit tests), this is a
    /// no-op. Always spawns onto the runtime so the caller's hot path
    /// is unaffected.
    fn audit_async(
        outcome: &'static str,
        stocks_added: i64,
        stocks_skipped: i64,
        buffer_entries: i64,
        diagnostic: &str,
    ) {
        let Some(qcfg) = tickvault_storage::global_questdb_config() else {
            return;
        };
        let qcfg = qcfg.clone();
        let diag = diagnostic.to_string();
        tokio::spawn(async move {
            let now_ist_nanos = chrono::Utc::now()
                .timestamp_nanos_opt()
                .unwrap_or(0)
                .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS);
            let trading_date = now_ist_nanos - now_ist_nanos.rem_euclid(86_400_000_000_000);
            if let Err(e) = tickvault_storage::phase2_audit_persistence::append_phase2_audit_row(
                &qcfg,
                now_ist_nanos,
                trading_date,
                outcome,
                stocks_added,
                stocks_skipped,
                buffer_entries,
                &diag,
            )
            .await
            {
                tracing::error!(
                    ?e,
                    code = tickvault_common::error_code::ErrorCode::Audit01Phase2WriteFailed
                        .code_str(),
                    "AUDIT-01 phase2 audit row write failed"
                );
            }
        });
    }

    /// Returns `true` if any of the three `emit_*` methods has been
    /// called on this guard.
    pub fn has_emitted(&self) -> bool {
        self.emitted
    }
}

impl Drop for Phase2EmitGuard {
    fn drop(&mut self) {
        if self.emitted {
            return;
        }
        // Increment the Prometheus counter on BOTH debug and release
        // paths — the alert tv-phase2-02-emit-guard-dropped + the
        // operator-health Grafana panel both query
        // `tv_phase2_emit_guard_dropped_total`. Without this increment
        // the alert would silently never fire even though the guard
        // caught the regression. Lives at the top of the drop branch
        // so it fires before the debug panic unwinds.
        counter!("tv_phase2_emit_guard_dropped_total").increment(1);
        // Debug builds (tests) — panic so missing-emit regressions show
        // up on the first execution that exercises the dropped path.
        #[cfg(debug_assertions)]
        {
            panic!(
                "Phase2EmitGuard dropped without emit_complete / emit_failed \
                 / emit_skipped — the Phase 2 dispatcher returned without \
                 firing a Phase 2 outcome notification (G7 invariant violated). \
                 ErrorCode: PHASE2-02"
            );
        }
        // Release builds — log ERROR so Loki routes it to Telegram.
        // The operator sees the rising edge and can investigate.
        #[cfg(not(debug_assertions))]
        {
            error!(
                code = "PHASE2-02",
                "Phase2EmitGuard dropped without firing a Phase 2 outcome \
                 notification — please report (Wave 1 Item 1 / G7)"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
// APPROVED: test-only — unwrap on construction of the test notification
// service is canonical. The deliberate-panic test must use `should_panic`
// because the Drop impl panics under cfg(debug_assertions).
mod tests {
    use super::*;
    use crate::notification::NotificationService;

    fn test_notifier() -> Arc<NotificationService> {
        // `NotificationService::disabled()` already returns `Arc<Self>`,
        // so no extra wrapping needed. notify(...) becomes a no-op
        // routed to local logging only — fine for unit tests.
        NotificationService::disabled()
    }

    /// Calling `emit_complete` flips `has_emitted` and the subsequent
    /// drop is a no-op (no panic, no error log).
    #[test]
    fn test_phase2_emit_guard_emit_complete_then_has_emitted_returns_true() {
        let mut guard = Phase2EmitGuard::new(test_notifier());
        assert!(
            !guard.has_emitted(),
            "fresh guard must report has_emitted=false"
        );
        guard.emit_complete(123, 42, 2, 4);
        assert!(
            guard.has_emitted(),
            "guard must report has_emitted=true after emit_complete"
        );
        // Drop fires here without panic.
    }

    /// `emit_failed` also satisfies the invariant.
    #[test]
    fn test_phase2_emit_guard_emit_failed_satisfies_invariant() {
        let mut guard = Phase2EmitGuard::new(test_notifier());
        guard.emit_failed("test reason".to_string(), 3);
        assert!(guard.has_emitted());
    }

    /// `emit_skipped` also satisfies the invariant.
    #[test]
    fn test_phase2_emit_guard_emit_skipped_satisfies_invariant() {
        let mut guard = Phase2EmitGuard::new(test_notifier());
        guard.emit_skipped("non-trading day".to_string());
        assert!(guard.has_emitted());
    }

    /// In debug builds (tests run under `cfg(debug_assertions)`),
    /// dropping a guard without calling any `emit_*` method MUST panic.
    /// This is the central G7 invariant — without it, a future edit
    /// adding a new early-return path could silently skip the outcome
    /// notification.
    #[test]
    #[should_panic(expected = "Phase2EmitGuard dropped without")]
    fn test_phase2_emit_guard_panics_in_debug_when_dropped_without_emit() {
        let _guard = Phase2EmitGuard::new(test_notifier());
        // Guard goes out of scope here — Drop fires — panic expected.
    }

    /// Calling emit twice is safe (`emitted` is already true; second
    /// call is just an additional notification). No panic.
    #[test]
    fn test_phase2_emit_guard_double_emit_does_not_panic() {
        let mut guard = Phase2EmitGuard::new(test_notifier());
        guard.emit_complete(1, 100, 0, 0);
        // Second emit on the same guard — flag already true, this is
        // the "operator was loud" path. The guard's job is to make
        // sure AT LEAST one outcome fired; multiple is non-fatal.
        guard.emit_failed("extra".to_string(), 1);
        assert!(guard.has_emitted());
    }
}

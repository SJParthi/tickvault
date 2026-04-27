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
    }

    /// Emits the failure outcome and marks the guard as fired.
    pub fn emit_failed(&mut self, reason: String, attempts: u32) {
        self.notifier
            .notify(NotificationEvent::Phase2Failed { reason, attempts });
        self.emitted = true;
    }

    /// Emits the skipped outcome and marks the guard as fired.
    pub fn emit_skipped(&mut self, reason: String) {
        self.notifier
            .notify(NotificationEvent::Phase2Skipped { reason });
        self.emitted = true;
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
    fn test_phase2_emit_guard_emit_complete_satisfies_invariant() {
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

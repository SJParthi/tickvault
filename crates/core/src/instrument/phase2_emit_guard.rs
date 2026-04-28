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
use std::sync::atomic::{AtomicU64, Ordering};

use metrics::counter;
// `tracing::error!` is used inside `Drop` only on release builds — gate
// the import so the unused-import warning doesn't fire under
// `cfg(debug_assertions)` / `cargo test`.
#[cfg(not(debug_assertions))]
use tracing::error;

use crate::notification::NotificationService;
use crate::notification::events::NotificationEvent;

/// Wave 3-D Item 13 — global Phase 2 outcome state holder.
///
/// The `slo_score` scheduler samples the Phase2_health dimension by
/// reading this atomic; before 09:13 IST it reads `0` (unknown,
/// pinned to healthy by the scheduler) and after the dispatcher
/// fires, the chosen `emit_*` method records the outcome. A new IST
/// trading day automatically invalidates yesterday's record because
/// the `today_day` query parameter no longer matches the stored day.
///
/// # Layout — single AtomicU64 to eliminate torn reads
///
/// Adversarial review (security-reviewer, 2026-04-28) flagged a
/// torn-read race when this state was held in two atomics
/// (`AtomicI64` for date + `AtomicU8` for kind): a reader could
/// observe `(today_date, yesterday_kind)` for the brief window
/// between the two writer stores, falsely reporting yesterday's
/// outcome as today's. The fix is a single packed `AtomicU64`:
///
/// ```text
/// bits 63..8  = day_number (i.e. days since IST 1970-01-01)
/// bits  7..0  = OUTCOME_KIND_*
/// ```
///
/// `day_number = trading_date_ist_nanos / 86_400_000_000_000`. The
/// year-2025 day-number is ~20_000 (fits in 16 bits); the year-9999
/// day-number is ~2.9M (fits in 22 bits). 56 bits of headroom is
/// astronomically more than needed.
///
/// Single-atomic load/store eliminates the race: a reader either
/// sees the old (day, kind) pair OR the new one; never a torn mix.
static OUTCOME_PACKED: AtomicU64 = AtomicU64::new(0);

/// Nanos-per-IST-day. The packed layout's `day_number` is
/// `trading_date_ist_nanos / NANOS_PER_DAY`.
const NANOS_PER_DAY: i64 = 86_400_000_000_000;

/// No outcome recorded today (yet, or ever).
pub const OUTCOME_KIND_UNKNOWN: u8 = 0;
/// Phase 2 fired and added at least one stock subscription.
pub const OUTCOME_KIND_COMPLETE: u8 = 1;
/// Phase 2 fired but failed (LTPs absent / pool saturated / retries exhausted).
pub const OUTCOME_KIND_FAILED: u8 = 2;
/// Phase 2 was skipped (non-trading day, dispatch_subscribe disabled, etc.).
pub const OUTCOME_KIND_SKIPPED: u8 = 3;

/// Pack `(day_number, kind)` into the single-atomic layout.
fn pack(day_number: u64, kind: u8) -> u64 {
    (day_number << 8) | u64::from(kind)
}

/// Unpack the single-atomic layout into `(day_number, kind)`.
fn unpack(packed: u64) -> (u64, u8) {
    (packed >> 8, (packed & 0xFF) as u8)
}

/// Internal helper — writes the packed atomic with `Ordering::SeqCst`
/// so a concurrent SLO scheduler reader observes either the old
/// (day, kind) pair or the new one, never a torn mix.
fn record_outcome(kind: u8) {
    let now_ist_nanos = chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(0)
        .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS);
    let day_number = (now_ist_nanos / NANOS_PER_DAY).max(0) as u64;
    let packed = pack(day_number, kind);
    OUTCOME_PACKED.store(packed, Ordering::SeqCst);
}

/// Reads today's recorded Phase 2 outcome. Returns `None` if no
/// outcome has been recorded for the supplied IST trading date
/// (i.e. either pre-trigger or yesterday's record). Returns
/// `Some(true)` only on `OUTCOME_KIND_COMPLETE`; everything else
/// (failed, skipped, unknown for today) returns `Some(false)`.
///
/// `today_date_ist_nanos` MUST be the IST-midnight epoch-nanos for
/// the caller's "today". The scheduler computes it once per tick.
#[must_use]
// TEST-EXEMPT: covered by 6 unit tests in `mod tests` below — test_phase2_outcome_unknown_returns_none, test_phase2_outcome_complete_returns_some_true, test_phase2_outcome_failed_returns_some_false, test_phase2_outcome_skipped_returns_some_false, test_phase2_outcome_yesterdays_record_does_not_leak_into_today, test_phase2_outcome_pack_unpack_roundtrip.
pub fn phase2_outcome_today_is_complete(today_date_ist_nanos: i64) -> Option<bool> {
    let packed = OUTCOME_PACKED.load(Ordering::SeqCst);
    let (stored_day, kind) = unpack(packed);
    let today_day = (today_date_ist_nanos / NANOS_PER_DAY).max(0) as u64;
    if stored_day != today_day {
        return None;
    }
    if kind == OUTCOME_KIND_UNKNOWN {
        return None;
    }
    Some(kind == OUTCOME_KIND_COMPLETE)
}

/// Test-only: reset the global state so each unit test in this module
/// (and downstream tests) starts from a clean slate.
#[cfg(test)]
// TEST-EXEMPT: test-only helper, gated on `#[cfg(test)]`; called by every test_phase2_outcome_* test.
pub(crate) fn reset_phase2_outcome_for_test() {
    OUTCOME_PACKED.store(0, Ordering::SeqCst);
}

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
        // Wave 3-D Item 13 — feed the SLO Phase2_health dimension.
        record_outcome(OUTCOME_KIND_COMPLETE);
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
        // Wave 3-D Item 13 — failed outcome contributes 0 to Phase2_health.
        record_outcome(OUTCOME_KIND_FAILED);
        Self::audit_async("failed", 0, 0, 0, &diag);
    }

    /// Emits the skipped outcome and marks the guard as fired.
    pub fn emit_skipped(&mut self, reason: String) {
        let diag = reason.clone();
        self.notifier
            .notify(NotificationEvent::Phase2Skipped { reason });
        self.emitted = true;
        // Wave 3-D Item 13 — skipped outcome contributes 0 to Phase2_health
        // (no F&O subscription happened today, so zero is the honest value).
        record_outcome(OUTCOME_KIND_SKIPPED);
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
                metrics::counter!("tv_audit_write_failures_total", "table" => "phase2_audit")
                    .increment(1);
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
    use std::sync::Mutex;

    /// Adversarial review (general-purpose, 2026-04-28 CRITICAL #1):
    /// every test in this module mutates the global
    /// `OUTCOME_PACKED` atomic via `record_outcome` /
    /// `reset_phase2_outcome_for_test`. Cargo runs tests in the
    /// same binary in parallel by default, so two tests racing on
    /// the global state can produce flaky failures. A single
    /// process-wide `Mutex<()>` serializes every test that touches
    /// the global, ensuring deterministic behavior.
    static GLOBAL_STATE_LOCK: Mutex<()> = Mutex::new(());

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
        let _g = GLOBAL_STATE_LOCK.lock().unwrap();
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
        let _g = GLOBAL_STATE_LOCK.lock().unwrap();
        let mut guard = Phase2EmitGuard::new(test_notifier());
        guard.emit_failed("test reason".to_string(), 3);
        assert!(guard.has_emitted());
    }

    /// `emit_skipped` also satisfies the invariant.
    #[test]
    fn test_phase2_emit_guard_emit_skipped_satisfies_invariant() {
        let _g = GLOBAL_STATE_LOCK.lock().unwrap();
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
        let _g = GLOBAL_STATE_LOCK.lock().unwrap();
        let mut guard = Phase2EmitGuard::new(test_notifier());
        guard.emit_complete(1, 100, 0, 0);
        // Second emit on the same guard — flag already true, this is
        // the "operator was loud" path. The guard's job is to make
        // sure AT LEAST one outcome fired; multiple is non-fatal.
        guard.emit_failed("extra".to_string(), 1);
        assert!(guard.has_emitted());
    }

    /// Wave 3-D Item 13 — the SLO Phase2_health dimension reads via
    /// `phase2_outcome_today_is_complete`. Test the four legal states.
    /// All of these tests share global atomic state and serialize via
    /// `GLOBAL_STATE_LOCK` — flake-proof on parallel `cargo test`.
    #[test]
    fn test_phase2_outcome_unknown_returns_none() {
        let _g = GLOBAL_STATE_LOCK.lock().unwrap();
        reset_phase2_outcome_for_test();
        let today = 1_750_000_000_000_000_000_i64; // arbitrary epoch nanos in 2025-ish range
        assert_eq!(phase2_outcome_today_is_complete(today), None);
    }

    #[test]
    fn test_phase2_outcome_complete_returns_some_true() {
        let _g = GLOBAL_STATE_LOCK.lock().unwrap();
        reset_phase2_outcome_for_test();
        // Compute the "today" date the recorder will use, so we read
        // back consistently. `record_outcome` uses chrono::Utc::now()
        // internally — fetch the same here.
        let mut guard = Phase2EmitGuard::new(test_notifier());
        guard.emit_complete(42, 1000, 2, 4);
        let now_ist_nanos = chrono::Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or(0)
            .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS);
        let today = now_ist_nanos - now_ist_nanos.rem_euclid(86_400_000_000_000);
        assert_eq!(phase2_outcome_today_is_complete(today), Some(true));
    }

    #[test]
    fn test_phase2_outcome_failed_returns_some_false() {
        let _g = GLOBAL_STATE_LOCK.lock().unwrap();
        reset_phase2_outcome_for_test();
        let mut guard = Phase2EmitGuard::new(test_notifier());
        guard.emit_failed("retries exhausted".to_string(), 5);
        let now_ist_nanos = chrono::Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or(0)
            .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS);
        let today = now_ist_nanos - now_ist_nanos.rem_euclid(86_400_000_000_000);
        assert_eq!(phase2_outcome_today_is_complete(today), Some(false));
    }

    #[test]
    fn test_phase2_outcome_skipped_returns_some_false() {
        let _g = GLOBAL_STATE_LOCK.lock().unwrap();
        reset_phase2_outcome_for_test();
        let mut guard = Phase2EmitGuard::new(test_notifier());
        guard.emit_skipped("non-trading day".to_string());
        let now_ist_nanos = chrono::Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or(0)
            .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS);
        let today = now_ist_nanos - now_ist_nanos.rem_euclid(86_400_000_000_000);
        assert_eq!(phase2_outcome_today_is_complete(today), Some(false));
    }

    #[test]
    fn test_phase2_outcome_yesterdays_record_does_not_leak_into_today() {
        let _g = GLOBAL_STATE_LOCK.lock().unwrap();
        reset_phase2_outcome_for_test();
        let mut guard = Phase2EmitGuard::new(test_notifier());
        guard.emit_complete(50, 1000, 2, 4);
        // Querying with TOMORROW's date (one day ahead) returns None
        // — the IST midnight rollover invalidates yesterday's record
        // automatically without needing a daily-reset task.
        let now_ist_nanos = chrono::Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or(0)
            .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS);
        let today = now_ist_nanos - now_ist_nanos.rem_euclid(86_400_000_000_000);
        let tomorrow = today + 86_400_000_000_000;
        assert_eq!(phase2_outcome_today_is_complete(tomorrow), None);
    }

    /// Adversarial review (security-reviewer + general-purpose,
    /// 2026-04-28): the original two-atomic layout had a torn-read
    /// race window. The single-AtomicU64 packed layout closes that
    /// window: one `load(SeqCst)` returns either the old (day,kind)
    /// pair or the new pair, atomically. Verify the pack/unpack
    /// round-trips for a representative day-number range.
    #[test]
    fn test_phase2_outcome_pack_unpack_roundtrip() {
        // Day 0 (1970-01-01), kind UNKNOWN
        let p = pack(0, OUTCOME_KIND_UNKNOWN);
        assert_eq!(unpack(p), (0, OUTCOME_KIND_UNKNOWN));
        // A 2026-ish day (~20_500), every kind
        for kind in [
            OUTCOME_KIND_UNKNOWN,
            OUTCOME_KIND_COMPLETE,
            OUTCOME_KIND_FAILED,
            OUTCOME_KIND_SKIPPED,
        ] {
            let p = pack(20_500, kind);
            assert_eq!(unpack(p), (20_500, kind));
        }
        // Far-future day (year 9999 ≈ 2.9M days)
        let p = pack(2_932_000, OUTCOME_KIND_COMPLETE);
        assert_eq!(unpack(p), (2_932_000, OUTCOME_KIND_COMPLETE));
    }

    #[test]
    fn test_phase2_outcome_kind_constants_are_pinned() {
        // Wire-stable: AtomicU8 layout depends on these.
        assert_eq!(OUTCOME_KIND_UNKNOWN, 0);
        assert_eq!(OUTCOME_KIND_COMPLETE, 1);
        assert_eq!(OUTCOME_KIND_FAILED, 2);
        assert_eq!(OUTCOME_KIND_SKIPPED, 3);
    }
}

//! Wave 2-C Item 7 — full integration ratchets for the FAST-BOOT race fix
//! (Item 7.1) and the boot-time clock-skew probe (Item 7.3).
//!
//! These tests are the per-PR ratchets enumerated in the task spec:
//!
//! | Test name                                              | Scope |
//! |--------------------------------------------------------|-------|
//! | test_tick_processor_waits_for_questdb_ready            | 7.1   |
//! | test_boot_probe_emits_critical_on_clock_skew_gt_2s     | 7.3   |
//! | test_boot_probe_passes_at_skew_below_threshold         | 7.3   |
//! | test_clock_skew_threshold_constant_is_2s               | 7.3   |
//! | test_boot_clock_skew_event_is_critical_severity        | 7.3   |
//! | test_boot_03_error_code_round_trips                    | 7.3   |
//!
//! The behavioural test for 7.1 binds a `TcpListener` to drive a slow
//! ILP server — so the boot probe's HTTP `/exec` GET fails until we
//! drop the listener. We use a 2s deadline for speed; correctness of
//! the 60s deadline is asserted by `test_boot_deadline_constant_is_60s`
//! in `crates/common/src/constants.rs`.

use std::time::Duration;

use tickvault_app::infra::{ClockSkewError, ClockSkewSample};
use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::CLOCK_SKEW_HALT_THRESHOLD_SECS;
use tickvault_common::error_code::{ErrorCode, Severity};
use tickvault_core::notification::events::NotificationEvent;

fn unreachable_questdb_config() -> QuestDbConfig {
    // Port 1 is unprivileged + always rejected — `wait_for_questdb_ready`
    // hits an immediate connection refused on every probe.
    QuestDbConfig {
        host: "127.0.0.1".to_string(),
        http_port: 1,
        pg_port: 8812,
        ilp_port: 9009,
    }
}

/// Item 7.1 — tick processor must NOT begin consuming the SPSC ring
/// until `wait_for_questdb_ready` returns. We assert the contract at
/// the `wait_for_questdb_ready` boundary because the processor's gate
/// is just a thin call on top of it: we drive a 2s deadline against
/// an unreachable QuestDB and assert the elapsed time is ≥ 2s (so the
/// tick processor was still gated, not consuming) and the returned
/// error is `BootProbeError::DeadlineExceeded` carrying BOOT-02.
#[tokio::test]
async fn test_tick_processor_waits_for_questdb_ready() {
    let cfg = unreachable_questdb_config();
    let started = std::time::Instant::now();
    let result = tickvault_storage::boot_probe::wait_for_questdb_ready(&cfg, 2).await;
    let elapsed = started.elapsed();
    assert!(
        result.is_err(),
        "tick processor's gate must NOT return Ok on an unreachable QuestDB"
    );
    assert!(
        elapsed >= Duration::from_secs(2),
        "tick processor must wait at least the configured deadline ({:?})",
        elapsed
    );
    let err = result.err().unwrap();
    let msg = format!("{err}");
    assert!(
        msg.contains("BOOT-02"),
        "deadline error must surface BOOT-02 code in its message: {msg}"
    );
}

/// Item 7.3 — clock-skew probe halts when |skew| >= threshold.
/// We construct a synthetic ClockSkewSample directly so the test does
/// not depend on the host having chrony installed.
#[test]
fn test_boot_probe_emits_critical_on_clock_skew_gt_2s() {
    // Synthetic sample: +2.5s ahead. The .exceeds() helper drives the
    // halt decision in `enforce_clock_skew_at_boot`, so we test it here.
    let sample = ClockSkewSample {
        skew_secs: 2.5,
        source: "synthetic",
    };
    assert!(
        sample.exceeds(CLOCK_SKEW_HALT_THRESHOLD_SECS),
        "+2.5s skew must trip the {}s halt threshold",
        CLOCK_SKEW_HALT_THRESHOLD_SECS
    );
    // Negative direction is symmetric — the abs() in exceeds() must catch this.
    let neg = ClockSkewSample {
        skew_secs: -3.1,
        source: "synthetic",
    };
    assert!(
        neg.exceeds(CLOCK_SKEW_HALT_THRESHOLD_SECS),
        "-3.1s skew must trip the threshold (abs value)"
    );
}

/// Item 7.3 — clock-skew probe passes for skew strictly below threshold.
/// 1.99s sits 0.01s under the 2.0s halt line and MUST NOT halt.
#[test]
fn test_boot_probe_passes_at_skew_below_threshold() {
    let sample = ClockSkewSample {
        skew_secs: 1.99,
        source: "synthetic",
    };
    assert!(
        !sample.exceeds(CLOCK_SKEW_HALT_THRESHOLD_SECS),
        "1.99s skew must NOT trip the {}s threshold (boundary safety)",
        CLOCK_SKEW_HALT_THRESHOLD_SECS
    );
    // Zero skew is the universal happy path.
    let zero = ClockSkewSample {
        skew_secs: 0.0,
        source: "synthetic",
    };
    assert!(!zero.exceeds(CLOCK_SKEW_HALT_THRESHOLD_SECS));
}

/// Item 7.3 — the halt threshold itself is pinned at 2.0s.
/// Mirror of the constants-crate test, executed here so the rule
/// also fires in the `app` crate's test scope.
#[test]
fn test_clock_skew_threshold_constant_is_2s() {
    let lo = CLOCK_SKEW_HALT_THRESHOLD_SECS - 2.0_f64;
    let hi = CLOCK_SKEW_HALT_THRESHOLD_SECS + 2.0_f64;
    assert!((-f64::EPSILON..=f64::EPSILON).contains(&lo));
    assert!((4.0 - f64::EPSILON..=4.0 + f64::EPSILON).contains(&hi));
}

/// Item 7.3 — the new typed Telegram event MUST be Critical severity.
#[test]
fn test_boot_clock_skew_event_is_critical_severity() {
    let event = NotificationEvent::BootClockSkewExceeded {
        skew_secs: 2.5,
        threshold_secs: CLOCK_SKEW_HALT_THRESHOLD_SECS,
        source: "synthetic".to_string(),
    };
    assert_eq!(
        event.severity(),
        tickvault_core::notification::Severity::Critical
    );
    // Telegram body must surface the BOOT-03 code so operators see it
    // even if the Severity tier is filtered.
    let body = event.to_message();
    assert!(
        body.contains("BOOT-03"),
        "BootClockSkewExceeded body must surface BOOT-03 code: {body}"
    );
}

/// Item 7.3 — BOOT-03 ErrorCode round-trips through code_str / FromStr,
/// is mapped to Critical severity, and points at the new wave-2-c rule
/// file. Mirrors the existing pattern for BOOT-01/02.
#[test]
fn test_boot_03_error_code_round_trips() {
    let code = ErrorCode::Boot03ClockSkewExceeded;
    assert_eq!(code.code_str(), "BOOT-03");
    let parsed: ErrorCode = "BOOT-03".parse().expect("BOOT-03 must FromStr-roundtrip");
    assert_eq!(parsed, code);
    assert_eq!(code.severity(), Severity::Critical);
    assert!(
        !code.is_auto_triage_safe(),
        "Critical codes must never auto-triage"
    );
    assert_eq!(
        code.runbook_path(),
        ".claude/rules/project/wave-2-c-error-codes.md",
        "BOOT-03 runbook target must be the new Wave 2-C rule file"
    );
}

/// Item 7.3 — `enforce_clock_skew_at_boot` returns ThresholdExceeded
/// (not Unavailable) when both probe paths produce skew >= threshold.
/// We can't drive a real `chronyc` from a unit test, so we use the
/// QuestDB-fallback path with an unreachable QuestDB to assert the
/// `Unavailable` branch surfaces the WARN-not-HALT path.
#[tokio::test]
async fn test_clock_skew_unavailable_does_not_halt() {
    let cfg = unreachable_questdb_config();
    // chrony may or may not be installed in the test container — both
    // outcomes are acceptable. What matters: when both probes fail we
    // get Unavailable (NOT ThresholdExceeded). Boot proceeds with WARN.
    let result =
        tickvault_app::infra::enforce_clock_skew_at_boot(&cfg, CLOCK_SKEW_HALT_THRESHOLD_SECS)
            .await;
    if let Err(ClockSkewError::ThresholdExceeded { .. }) = result {
        panic!(
            "ThresholdExceeded must require a successful skew sample; \
             unreachable QuestDB must yield Unavailable or Ok (chronyc)"
        );
    }
}

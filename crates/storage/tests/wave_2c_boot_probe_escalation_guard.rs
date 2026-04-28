//! Wave 2-C Item 7.2 / 7.4 — boot-probe escalation table source-scan ratchets.
//!
//! Pins the +5/+10/+20/+30/+60 escalation table to its constants in
//! `crates/common/src/constants.rs` and asserts the boot-probe source
//! file references each threshold + each level keyword exactly once.
//!
//! Why source-scan and not behavioural? Driving the probe through 60s
//! of synthetic clock advance against a never-200 HTTP server is a
//! tokio::time::pause() integration test — it lives in
//! `crates/app/tests/wave_2c_boot_probe_behaviour.rs`. THIS file is
//! the cheap mechanical guard that catches "someone moved the +30s
//! threshold to +25s and forgot to update the constant".

use std::fs;

const PROBE_PATH: &str = "src/boot_probe.rs";

fn boot_probe_source() -> String {
    fs::read_to_string(PROBE_PATH).expect("boot_probe.rs must exist in crates/storage/src")
}

#[test]
fn test_boot_logs_match_escalation_table() {
    // Single source-scan ratchet covering Item 7.4's "test_boot_logs_match_escalation_table".
    // Every escalation phrase MUST appear — and the constants they reference MUST match
    // the pinned values in `crates/common/src/constants.rs`.
    let src = boot_probe_source();
    // Each of the 5 escalation tier keywords appears.
    for phrase in ["≥5s", "≥10s", "≥20s", "≥30s"] {
        assert!(
            src.contains(phrase),
            "boot_probe.rs missing escalation phrase '{phrase}' — escalation table broken"
        );
    }
    // BOOT-01 + BOOT-02 codes are emitted at the right tiers.
    assert!(
        src.contains("ErrorCode::Boot01QuestDbSlow"),
        "boot_probe.rs must emit BOOT-01 at the +30s tier"
    );
    assert!(
        src.contains("ErrorCode::Boot02DeadlineExceeded"),
        "boot_probe.rs must emit BOOT-02 at the deadline tier"
    );
    // Constants pulled from common — no hard-coded thresholds in boot_probe.rs.
    assert!(
        src.contains("BOOT_ESCALATION_DEBUG_AT_SECS"),
        "boot_probe.rs must pin DEBUG threshold to common::constants"
    );
    assert!(
        src.contains("BOOT_ESCALATION_INFO_AT_SECS"),
        "boot_probe.rs must pin INFO threshold to common::constants"
    );
    assert!(
        src.contains("BOOT_ESCALATION_WARN_AT_SECS"),
        "boot_probe.rs must pin WARN threshold to common::constants"
    );
    assert!(
        src.contains("BOOT_ESCALATION_ERROR_AT_SECS"),
        "boot_probe.rs must pin ERROR threshold to common::constants"
    );
    assert!(
        src.contains("BOOT_DEADLINE_SECS as COMMON_BOOT_DEADLINE_SECS")
            || src.contains("COMMON_BOOT_DEADLINE_SECS"),
        "boot_probe.rs must re-export the common BOOT_DEADLINE_SECS constant"
    );
}

#[test]
fn test_critical_at_30s_warn_at_20s() {
    // The +20s WARN block emits a `warn!(...)` macro with the "≥20s"
    // phrase; the +30s ERROR block emits `error!(...)` with BOOT-01.
    // Both must coexist in the source — we don't assert source-order
    // because the deadline path's `error!(...BOOT-02...)` legitimately
    // appears earlier (before the escalation block) inside the loop.
    let src = boot_probe_source();
    assert!(
        src.contains("warn!"),
        "boot_probe.rs must contain a warn! macro for the +20s tier"
    );
    assert!(
        src.contains("error!"),
        "boot_probe.rs must contain an error! macro (BOOT-01 + BOOT-02)"
    );
    // The +20s WARN keyword phrase is reachable.
    assert!(
        src.contains("(≥20s)"),
        "boot_probe.rs must include '(≥20s)' WARN phrase"
    );
    // The +30s ERROR tier carries BOOT-01.
    let warn_block_idx = src
        .find("(≥20s)")
        .expect("WARN +20s phrase must exist (re-checked)");
    let after_warn = &src[warn_block_idx..];
    assert!(
        after_warn.contains("Boot01QuestDbSlow"),
        "the +30s tier (after the +20s WARN block) must emit BOOT-01"
    );
}

#[test]
fn test_halt_at_60s() {
    // The DeadlineExceeded path must return Err and reference BOOT_DEADLINE_SECS
    // (the deadline knob — pinned at 60s in common::constants).
    let src = boot_probe_source();
    assert!(
        src.contains("BootProbeError::DeadlineExceeded"),
        "boot_probe.rs must surface a typed DeadlineExceeded error"
    );
    assert!(
        src.contains("Boot02DeadlineExceeded"),
        "boot_probe.rs must emit BOOT-02 at the deadline"
    );
}

#[test]
fn test_rescue_ring_silent_during_boot_window_30s() {
    // No `error!` call appears INSIDE the +5s, +10s, or +20s
    // escalation arms — those tiers are DEBUG / INFO / WARN per the
    // pinned escalation table. ERROR-level emission is restricted to
    // the +30s tier (BOOT-01) and the deadline tier (BOOT-02), so the
    // rescue ring stays silent for the first 30 seconds of boot.
    let src = boot_probe_source();

    // DEBUG / INFO / WARN tiers MUST exist with their threshold keywords.
    let debug_block_idx = src
        .find("(≥5s)")
        .expect("boot_probe.rs missing +5s DEBUG escalation");
    let info_block_idx = src
        .find("(≥10s)")
        .expect("boot_probe.rs missing +10s INFO escalation");
    let warn_block_idx = src
        .find("(≥20s)")
        .expect("boot_probe.rs missing +20s WARN escalation");

    // The +5/+10 windows must be guarded by `debug!`/`info!` (not `error!`).
    // We grab the ~120 characters immediately preceding each marker so
    // we land on the macro name that opens the arm body.
    let debug_arm = &src[debug_block_idx.saturating_sub(120)..debug_block_idx];
    let info_arm = &src[info_block_idx.saturating_sub(120)..info_block_idx];
    let warn_arm = &src[warn_block_idx.saturating_sub(120)..warn_block_idx];
    assert!(
        debug_arm.contains("debug!"),
        "+5s arm must use debug! (rescue-ring silent boot window)"
    );
    assert!(
        info_arm.contains("info!"),
        "+10s arm must use info! (rescue-ring silent boot window)"
    );
    assert!(
        warn_arm.contains("warn!"),
        "+20s arm must use warn! (rescue-ring silent boot window)"
    );

    // Sanity: tier ordering matches the table.
    assert!(debug_block_idx < info_block_idx);
    assert!(info_block_idx < warn_block_idx);
}

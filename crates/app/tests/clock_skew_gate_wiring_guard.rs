//! Source-scan ratchet (Z+ L4 PREVENT / L5 AUDIT) pinning that the BOOT-03
//! clock-skew boot gate is wired into `build_shared_infra` — the single
//! every-boot-path shared-infra build.
//!
//! Background (2026-07-16 audit): `infra::enforce_clock_skew_at_boot` (the
//! documented BOOT-03 Critical HALT gate, `wave-2-c-error-codes.md`) and its
//! sole gauge emitter `probe_clock_skew` had ZERO production callers — only the
//! fn def + `wave_2c_item7_boot_race_and_clock_skew.rs`. So the documented
//! `>2s wall-clock skew → HALT boot` gate NEVER fired, `tv_clock_skew_seconds`
//! never published, and the CloudWatch alarm on it was false-green forever. A
//! >2s skew can silently split/merge trading days across QuestDB DEDUP keys.
//! The fix wires the gate into `build_shared_infra` before the seal-writer
//! spawn; this guard fails the build if that wiring silently dies again.
//!
//! Mirrors the codebase's `*_is_wired` guard pattern
//! (`tick_conservation_wiring_guard.rs`, `orphan_position_watchdog_wiring_guard.rs`).
//! Reads `main.rs` SOURCE text, so it runs on the default build independent of
//! any feature flag.

use std::fs;
use std::path::PathBuf;

fn main_rs_source() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn test_clock_skew_gate_is_wired_into_main() {
    let src = main_rs_source();
    assert!(
        src.contains("enforce_clock_skew_at_boot("),
        "main.rs must call enforce_clock_skew_at_boot(...) in build_shared_infra — \
         otherwise the documented BOOT-03 clock-skew HALT gate never fires and \
         tv_clock_skew_seconds never publishes."
    );
}

#[test]
fn test_clock_skew_gate_halts_on_breach() {
    // The ThresholdExceeded arm must HALT boot the same way BOOT-01/BOOT-02 do
    // (return Err from build_shared_infra) AND page the Critical operator alert.
    let src = main_rs_source();
    assert!(
        src.contains("NotificationEvent::BootClockSkewExceeded"),
        "the clock-skew breach arm must fire the Critical \
         NotificationEvent::BootClockSkewExceeded operator alert."
    );
    assert!(
        src.contains("ClockSkewError::ThresholdExceeded"),
        "the gate must destructure ClockSkewError::ThresholdExceeded to HALT on \
         a real skew breach (not on the non-halting Unavailable arm)."
    );
    assert!(
        src.contains("ErrorCode::Boot03ClockSkewExceeded"),
        "the breach log line must carry the BOOT-03 error code."
    );
}

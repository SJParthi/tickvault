//! Audit Finding #7 (2026-05-03): per-step boot timeouts must be
//! consistent with the umbrella `BOOT_TIMEOUT_SECS` deadline.
//!
//! Background: the boot sequence in `crates/app/src/main.rs` has a
//! global umbrella check at the end — if the total boot time exceeds
//! `BOOT_TIMEOUT_SECS` (= 120s), a CRITICAL Telegram fires. Several
//! individual boot steps have their own per-step timeouts. Those MUST
//! NOT exceed the umbrella, otherwise the umbrella will alert
//! mid-step (false positive) while the step is still legitimately
//! running.
//!
//! The audit caught one instance: `TOKEN_INIT_TIMEOUT_SECS = 300s` on
//! umbrella `BOOT_TIMEOUT_SECS = 120s`. Step 6 could legitimately take
//! up to 300s but the umbrella would page CRITICAL at 120s and confuse
//! the operator into thinking step 6 had hung.
//!
//! This file ratchets the invariant `every per-step timeout <= umbrella`
//! so any future step that adds its own timeout cannot reintroduce the
//! conflict.

use tickvault_common::constants::{BOOT_DEADLINE_SECS, BOOT_TIMEOUT_SECS, TOKEN_INIT_TIMEOUT_SECS};

#[test]
fn token_init_timeout_does_not_exceed_umbrella_boot_timeout() {
    // Step 6 (Dhan auth) — per-step timeout must respect the umbrella.
    assert!(
        TOKEN_INIT_TIMEOUT_SECS <= BOOT_TIMEOUT_SECS,
        "TOKEN_INIT_TIMEOUT_SECS ({TOKEN_INIT_TIMEOUT_SECS}s) must be \
         <= BOOT_TIMEOUT_SECS ({BOOT_TIMEOUT_SECS}s) — otherwise the \
         umbrella boot deadline alerts BEFORE step 6 has had a chance \
         to complete on a slow Dhan auth path. Audit Finding #7 \
         (2026-05-03)."
    );
}

#[test]
fn boot_questdb_deadline_does_not_exceed_umbrella_boot_timeout() {
    // Step 7 (QuestDB DDL) — per-step deadline must respect the umbrella.
    assert!(
        BOOT_DEADLINE_SECS <= BOOT_TIMEOUT_SECS,
        "BOOT_DEADLINE_SECS ({BOOT_DEADLINE_SECS}s) must be <= \
         BOOT_TIMEOUT_SECS ({BOOT_TIMEOUT_SECS}s) for the same reason \
         as TOKEN_INIT_TIMEOUT_SECS — the umbrella must outlast every \
         per-step deadline."
    );
}

#[test]
fn umbrella_boot_timeout_is_at_least_90s() {
    // Sanity: the umbrella must be generous enough to cover a realistic
    // cold boot (Docker pull + Dhan CSV download + universe build +
    // QuestDB DDL). 90s is the lower bound.
    assert!(
        BOOT_TIMEOUT_SECS >= 90,
        "BOOT_TIMEOUT_SECS ({BOOT_TIMEOUT_SECS}s) must be at least 90s \
         to cover a realistic cold boot."
    );
}

#[test]
fn token_init_timeout_is_at_least_30s() {
    // Sanity: per-step timeouts must be generous enough to cover
    // single-digit-second normal cases plus reasonable network blips.
    assert!(
        TOKEN_INIT_TIMEOUT_SECS >= 30,
        "TOKEN_INIT_TIMEOUT_SECS ({TOKEN_INIT_TIMEOUT_SECS}s) must be \
         >= 30s to cover network blips during Dhan auth."
    );
}

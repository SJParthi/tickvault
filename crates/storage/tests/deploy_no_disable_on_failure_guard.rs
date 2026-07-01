//! AUTO-START GUARANTEE ratchet (2026-06-09).
//!
//! Source-scan guard pinning the fix for the 2026-06-09 "will it auto-start
//! tomorrow?" incident class. The `deploy-aws.yml` deploy-failure path used to
//! run BOTH `systemctl stop tickvault` AND `systemctl disable tickvault`. The
//! `disable` was the auto-start killer:
//!
//!  1. A disabled unit does NOT start at the next 08:30 IST boot — systemd only
//!     auto-starts ENABLED units. So one failed evening deploy silently killed
//!     the next morning's auto-start.
//!  2. Worse, `scripts/aws-autopilot.sh` (the every-15-min self-healer) reads a
//!     DISABLED unit as an INTENTIONAL kill-switch and REFUSES to restart it
//!     (see its "unit disabled" branch). So the disable also defeated the one
//!     mechanism that would otherwise have self-healed the box.
//!
//! The fix keeps the `systemctl stop` (which alone breaks the systemd
//! `Restart=always` "Auth OK" Telegram spam loop for the current boot — an
//! explicit stop suppresses Restart=always until the next boot) and DROPS the
//! `systemctl disable`. The unit therefore stays ENABLED on a failed deploy, so
//! systemd auto-starts it next boot and the autopilot self-heals an
//! enabled-but-inactive unit. The ONLY remaining source of a disabled unit is
//! the intentional kill-switch (disable via AWS Control), so the autopilot's
//! `disabled == intentional` semantics stay correct.
//!
//! This guard fails the build if the `disable` ever returns to the deploy
//! failure path, OR if the spam-loop-breaking `stop` is removed, OR if the
//! autopilot's enabled-but-inactive self-heal restart is deleted.

#![cfg(test)]

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/storage parent")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

/// Section A — the deploy-failure path MUST NOT disable the tickvault unit.
/// `disable tickvault` anywhere in the workflow (the failure-path SSM command
/// element was the only site) re-introduces the auto-start-killer.
#[test]
fn deploy_failure_path_never_disables_tickvault_unit() {
    let body = read(&repo_root().join(".github/workflows/deploy-aws.yml"));
    assert!(
        !body.contains("disable tickvault"),
        "deploy-aws.yml must NEVER `systemctl disable tickvault` — it poisons \
         next-morning auto-start AND tricks aws-autopilot.sh into treating the \
         unit as an intentional kill-switch. Keep `stop`, drop `disable`."
    );
}

/// Section B — the spam-loop-breaking `systemctl stop` MUST stay. Removing it
/// would re-open the systemd Restart=always "Auth OK" Telegram-per-minute loop
/// the disable+stop pair originally fixed.
#[test]
fn deploy_failure_path_still_stops_to_break_restart_spam_loop() {
    let body = read(&repo_root().join(".github/workflows/deploy-aws.yml"));
    assert!(
        body.contains("systemctl stop tickvault || true"),
        "deploy-aws.yml failure path must still `systemctl stop tickvault || true` \
         to break the Restart=always spam loop for the current boot"
    );
}

/// Section C — the auto-start guarantee now RELIES on aws-autopilot.sh
/// restarting an enabled-but-inactive unit. Pin that self-heal so a future edit
/// cannot silently delete the mechanism this fix depends on.
#[test]
fn autopilot_still_self_heals_enabled_but_inactive_unit() {
    let body = read(&repo_root().join("scripts/aws-autopilot.sh"));
    assert!(
        body.contains("is-enabled tickvault"),
        "aws-autopilot.sh must still check `systemctl is-enabled tickvault` to \
         distinguish an intentional kill-switch from an enabled-but-inactive unit"
    );
    assert!(
        body.contains("systemctl start tickvault"),
        "aws-autopilot.sh must still `systemctl start tickvault` to self-heal \
         an enabled-but-inactive unit — the auto-start guarantee depends on it. \
         (BP-09: the recovery is now `reset-failed` + `start`, not a bare \
         `restart`, so this pins `start`, the launch step, not `restart`.)"
    );
}

/// Section D (BP-09, 2026-07-01) — the self-heal restart MUST `reset-failed`
/// BEFORE it starts the unit. `deploy/systemd/tickvault.service` sets
/// StartLimitBurst=8 / StartLimitIntervalSec=600, so after > 8 restarts in 10
/// min systemd flips the unit to `failed` and a bare `systemctl restart`/`start`
/// returns "Start request repeated too quickly" and is a NO-OP until the
/// start-limit counter is cleared with `systemctl reset-failed`. Without this,
/// autopilot pages but cannot recover a crash-loop — the box stays down until a
/// human runs reset-failed (BP-09). This guard fails the build if the recovery
/// ever regresses to a bare `restart`/`start` that omits `reset-failed`.
#[test]
fn autopilot_resets_failed_before_start_to_recover_crash_loop() {
    let body = read(&repo_root().join("scripts/aws-autopilot.sh"));
    assert!(
        body.contains("reset-failed tickvault"),
        "aws-autopilot.sh must `systemctl reset-failed tickvault` before starting \
         the unit, or a StartLimit-`failed` crash-loop (StartLimitBurst=8 in \
         tickvault.service) cannot be recovered — a bare restart/start on a \
         `failed` unit is a no-op (BP-09)"
    );
    // reset-failed alone does not launch the app; the recovery MUST also start it.
    assert!(
        body.contains("systemctl start tickvault"),
        "aws-autopilot.sh must `systemctl start tickvault` after `reset-failed` — \
         reset-failed only clears the start-limit counter, it does not launch the \
         unit (BP-09)"
    );
}

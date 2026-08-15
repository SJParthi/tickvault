//! Pins that host tuning is applied on EVERY BOOT and EVERY DEPLOY — not once
//! per instance.
//!
//! # The defect this guard exists to prevent recurring
//!
//! The tuning scripts were correct, and their headers said exactly the right
//! thing: *"applied at every boot"* and *"must be re-applied on every boot"*.
//! But the only caller was `user-data.sh.tftpl`, and EC2 user-data runs **once
//! per instance**, not once per boot.
//!
//! So the intent was per-boot and the wiring was per-instance. Nobody noticed,
//! because that mismatch produces no error anywhere: the box boots, the app
//! starts, the logs are clean. The only symptoms are worse tail latency and a
//! higher chance of the socket receive buffer overflowing into vendor-side
//! drops — neither of which points at its cause.
//!
//! It was not theoretical. This box stops and starts **every weekday**, and
//! transparent huge pages are a `/sys` write that does not survive a reboot.
//! Every trading day after an instance was created ran on distro defaults, and
//! any tuning merged after the last instance replacement had never been applied
//! at all.
//!
//! # What is pinned
//!
//! 1. The boot unit exists and is ordered before the app.
//! 2. It re-installs and re-applies the sysctl file (a deploy that changes
//!    tuning must take effect without an instance replacement).
//! 3. It runs the non-sysctl half — THP and the clock check.
//! 4. `user-data` ENABLES it, so it arms on a fresh instance.
//! 5. The deploy workflow installs AND restarts it, so tuning lands without
//!    waiting for the next stop/start.
//! 6. Tuning failure never gates the app — a slower host beats no host.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("host_tuning_guard: cannot canonicalize repo root")
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(repo_root().join(rel))
        .unwrap_or_else(|e| panic!("host_tuning_guard: {rel} must be readable: {e}"))
}

const UNIT: &str = "deploy/systemd/tickvault-host-tuning.service";
const USER_DATA: &str = "deploy/aws/terraform/user-data.sh.tftpl";
const DEPLOY: &str = ".github/workflows/deploy-aws.yml";

#[test]
fn boot_unit_exists_and_is_ordered_before_the_app() {
    let unit = read(UNIT);

    assert!(
        unit.contains("Before=tickvault.service"),
        "the tuning unit must be ordered BEFORE tickvault.service — the app must \
         never start on an untuned host"
    );
    assert!(
        unit.contains("WantedBy=multi-user.target"),
        "the unit must be WantedBy=multi-user.target, or `systemctl enable` \
         arms nothing and it never runs at boot — which is the exact \
         per-instance-vs-per-boot gap this guard exists to close"
    );
    assert!(
        unit.contains("Type=oneshot"),
        "tuning is a run-once-per-boot action, not a daemon"
    );
}

#[test]
fn boot_unit_applies_both_halves_of_the_tuning() {
    let unit = read(UNIT);

    // The sysctl half must be RE-INSTALLED from the repo, not merely re-applied
    // from whatever is already on disk. Otherwise a deploy that changes the
    // tuning file has no effect until the instance is replaced — the same class
    // of bug one level down.
    assert!(
        unit.contains("99-tickvault-net.conf"),
        "the unit must re-install the sysctl file from the repo, so a tuning \
         change takes effect without an instance replacement"
    );
    assert!(
        unit.contains("sysctl --system"),
        "the unit must apply the sysctl file after installing it"
    );

    // The non-sysctl half is the whole reason a BOOT unit is required: THP is a
    // /sys write and is lost on every reboot.
    assert!(
        unit.contains("apply-host-tuning.sh"),
        "the unit must run the non-sysctl half (THP + clock) — THP is a /sys \
         write that does NOT survive a reboot, which is the single strongest \
         reason this unit exists"
    );
    assert!(
        unit.contains("verify-net-tuning.sh"),
        "the unit must verify what it applied — applying without verifying is \
         how a silently-ineffective setting survives"
    );
}

#[test]
fn tuning_failure_never_blocks_the_trading_app() {
    let unit = read(UNIT);

    // A host with default buffers is slower. A host with no app is blind.
    // The unit must encode that ordering of harms.
    assert!(
        unit.contains("ExecStart=-"),
        "at least one ExecStart must be `-` prefixed so a tuning failure does \
         not fail the unit and cascade into the app"
    );
    assert!(
        !unit.contains("Requires=tickvault.service"),
        "the tuning unit must not make the app REQUIRE it — ordering only"
    );
}

#[test]
fn user_data_enables_the_boot_unit() {
    let ud = read(USER_DATA);

    assert!(
        ud.contains("tickvault-host-tuning.service"),
        "user-data must install the tuning unit on a fresh instance"
    );
    assert!(
        ud.contains("systemctl enable tickvault-host-tuning"),
        "user-data must ENABLE the unit. Copying it without enabling leaves it \
         inert — present on disk, running never, and invisible in every log"
    );
}

#[test]
fn deploy_installs_and_applies_tuning_without_waiting_for_a_reboot() {
    let deploy = read(DEPLOY);

    assert!(
        deploy.contains("tickvault-host-tuning.service"),
        "the deploy workflow must ship the unit file, or a tuning change never \
         reaches the box between instance replacements"
    );
    assert!(
        deploy.contains("systemctl enable tickvault-host-tuning"),
        "deploy must enable the unit so it arms for future boots"
    );
    assert!(
        deploy.contains("systemctl restart tickvault-host-tuning"),
        "deploy must RESTART the unit so THIS deploy's tuning applies now. \
         Enable-only would mean tuning changes silently wait for the next \
         stop/start — per-deploy intent with per-boot delivery, the same \
         mismatch one level up"
    );
}

#[test]
fn guard_is_not_vacuous() {
    // Every assertion above is a `contains`, which passes trivially against a
    // file that failed to load. `read` panics on failure, but prove the files
    // are substantial rather than empty stubs.
    for rel in [UNIT, USER_DATA, DEPLOY] {
        let body = read(rel);
        assert!(
            body.len() > 500,
            "{rel} is implausibly short ({} bytes) — the scanner would pass \
             vacuously",
            body.len()
        );
    }

    // And prove the sysctl file the unit installs actually carries the
    // load-bearing settings, rather than being an empty file the unit
    // faithfully installs and applies to no effect.
    let sysctl = read("deploy/aws/sysctl/99-tickvault-net.conf");
    for key in [
        "net.core.rmem_max",
        "net.ipv4.tcp_rmem",
        "net.core.netdev_max_backlog",
        "vm.dirty_ratio",
        "vm.swappiness",
    ] {
        assert!(
            sysctl.contains(key),
            "the sysctl file is missing `{key}` — the unit would apply nothing \
             of consequence while reporting success"
        );
    }
}

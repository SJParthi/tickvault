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

// ---------------------------------------------------------------------------
// QuestDB memory ceiling — the silent-3% foot-gun
// ---------------------------------------------------------------------------

/// The database memory cap must match the host it runs on.
///
/// # The defect this pins
///
/// `docker-compose.yml` reads `${QDB_MEM_LIMIT:-12g}`. That default applies
/// ONLY when the variable is unset — and the 2026-07-15 downsize wrote
/// `QDB_MEM_LIMIT=1g` into the on-box `.env` for a 4 GiB t4g.medium. The host
/// is now a 32 GiB r8g.xlarge, so a surviving `1g` caps the database at **3%
/// of the machine** while every config file in the repo reads 12g.
///
/// Nothing anywhere would report it. The container starts, the app connects,
/// and the only symptom is a database quietly thrashing inside a ceiling
/// nobody set on purpose.
///
/// `downsize-instance.yml` is the specific hazard: it is manually
/// dispatchable, it writes that `.env` over SSM, and after the Quote-15 sweep
/// it targets **r8g.xlarge** while still writing the 4 GiB-era value — a
/// workflow that would throttle a 32 GiB box to 1 GiB in one click.
#[test]
fn questdb_memory_cap_matches_the_locked_host_size() {
    let compose = read("deploy/docker/docker-compose.yml");
    let downsize = read(".github/workflows/downsize-instance.yml");

    assert!(
        compose.contains("QDB_MEM_LIMIT:-12g"),
        "the compose default is no longer 12g — on a 32 GiB host that is the \
         value a box with no .env override must land on"
    );

    // The hazard is a WRITER of the on-box override disagreeing with the
    // compose default. Any `QDB_MEM_LIMIT=<value>` written by automation must
    // match, or the override silently wins over every file in the repo.
    for (idx, line) in downsize.lines().enumerate() {
        if let Some(pos) = line.find("QDB_MEM_LIMIT=") {
            let tail = &line[pos + "QDB_MEM_LIMIT=".len()..];
            let value: String = tail
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            if value.is_empty() {
                continue; // a grep/sed pattern, not an assignment
            }
            assert_eq!(
                value,
                "12g",
                "downsize-instance.yml:{} writes QDB_MEM_LIMIT={} to the on-box \
                 .env, but the host is 32 GiB and compose defaults to 12g.\n\n\
                 That override WINS over the compose default, so this one line \
                 caps the database at a fraction of the machine — silently, \
                 with no error anywhere. This workflow targets r8g.xlarge; the \
                 value must match the host it now flips to.",
                idx + 1,
                value
            );
        }
    }
}

#[test]
fn questdb_cap_guard_is_not_vacuous() {
    // The loop above passes trivially if no assignment is ever found.
    let downsize = read(".github/workflows/downsize-instance.yml");
    let assignments = downsize
        .lines()
        .filter(|l| {
            // Assignments are now DERIVED (`QDB_MEM_LIMIT=${QDB_G}g`), so the
            // old "next char is alphanumeric" test — written when the value was
            // the literal `12g` — matched nothing and this non-vacuity check
            // failed the very change that improved the thing it guards. Accept
            // either shape: what matters is that an assignment exists to scan.
            // ANY occurrence on the line, not just the first. A real
            // assignment line reads
            //   grep -q "^QDB_MEM_LIMIT=" … && sed -i "s/…/QDB_MEM_LIMIT=${QDB_G}g/"
            // so the FIRST match is the grep pattern, whose next character is a
            // quote. Testing only the first occurrence found nothing and made
            // this non-vacuity check fail on a file that plainly has three.
            l.match_indices("QDB_MEM_LIMIT=").any(|(p, m)| {
                l[p + m.len()..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '$')
            })
        })
        .count();
    assert!(
        assignments >= 2,
        "found {assignments} QDB_MEM_LIMIT assignments in downsize-instance.yml \
         — expected at least 2 (repo .env + on-box .env). The scanner is not \
         matching, so the check above would pass vacuously"
    );
}

/// The deploy path must SELF-HEAL a stale on-box memory cap.
///
/// # Why asserting the value in the repo is not enough
///
/// The previous test stops a bad value being WRITTEN. It cannot repair one
/// already sitting in the on-box `.env` — and one plausibly is, left by the
/// July downsize when the host was 4 GiB. compose only supplies its `12g`
/// default when the variable is UNSET, so a stale line wins forever and no
/// deploy, restart or reboot would ever notice.
///
/// That is a config value silently outliving the machine it was sized for:
/// the same shape as the per-instance-vs-per-boot bug this whole file exists
/// to close, one layer up. The fix is the same in kind — make the repo the
/// source of truth on every deploy, so a stale value cannot survive one.
#[test]
fn deploy_self_heals_a_stale_questdb_memory_cap() {
    let deploy = read(DEPLOY);

    // ANCHORED ON THE DERIVATION, not on a literal size.
    //
    // This assertion originally read `deploy.contains("QDB_MEM_LIMIT=12g")`,
    // and on 2026-08-15 it did exactly what a literal-anchored guard does: it
    // FAILED the fix that removed the hardcoded 12g in favour of a value
    // derived from the host's MemTotal — the strictly better version of the
    // property it was written to protect. Left as it was, it would have held
    // a 32 GiB-sized constant in place across every future instance change,
    // which is the very failure the surrounding file exists to prevent.
    //
    // The property that actually matters is "the deploy asserts a cap sized
    // for THIS host, on every deploy". That is what is asserted now.
    assert!(
        deploy.contains("QDB_MEM_LIMIT=$QDB_WANT"),
        "the deploy path no longer asserts a QuestDB memory cap on the box. A \
         stale 1g from the 4 GiB era would then cap the database at 3% of a \
         32 GiB host, silently and permanently"
    );
    assert!(
        deploy.contains("MemTotal"),
        "the deploy's QuestDB cap is not derived from the host. A hardcoded \
         value is right for exactly one instance size and wrong for every \
         other — and a rollback to a 4 GiB box would cap the database ABOVE \
         the host's own RAM"
    );

    // Writing the value without recreating the container would fix the FILE and
    // leave the RUNNING database throttled until something else happened to
    // restart it — a fix that reports success while changing nothing.
    assert!(
        deploy.contains("--force-recreate questdb"),
        "the deploy path corrects the .env but never recreates questdb, so the \
         corrected cap would not take effect on the running container"
    );

    // And it must only recreate when the value actually CHANGED — recreating the
    // database on every deploy would be a self-inflicted outage in the name of a
    // no-op correction.
    assert!(
        deploy.contains("QDB_BEFORE") && deploy.contains("QDB_AFTER"),
        "the self-heal must compare before/after and recreate ONLY on a real \
         change — an unconditional recreate restarts the database on every \
         deploy for no reason"
    );
}

/// The QuestDB memory cap must be DERIVED from the host, and every check of it
/// must test the same derived value.
///
/// # The two defects this pins shut (adversarial review, 2026-08-15)
///
/// 1. **A write that fails its own verification.** `downsize-instance.yml`
///    step 8 wrote `QDB_MEM_LIMIT=12g` and then asserted the resulting
///    container limit equalled `1073741824` bytes — which is 1g, not 12g. Both
///    cannot be true, so the step FATALed on every run. A second, workflow-level
///    check demanded the same 1g literal. That step is the ROLLBACK path of an
///    instance flip: the thing reached for when the box is already in trouble.
///
/// 2. **A cap sized for the wrong machine.** Both that workflow and the deploy
///    self-heal hardcoded a 32 GiB-appropriate `12g`. The downsize workflow's
///    own header describes rolling back to a 4 GiB `t4g.medium`, where a 12g
///    cgroup ceiling is a 3x overcommit that invites the OOM killer — the same
///    class as the frame ring that was still sized for a 4 GiB host four
///    instance upgrades later.
///
/// Deriving from `MemTotal` makes both impossible: a 32 GiB box still lands on
/// exactly 12g (no behaviour change), a 4 GiB box lands on 1g, and an
/// unreadable `/proc/meminfo` lands on the 1g floor.
#[test]
fn test_the_questdb_memory_cap_is_derived_and_self_consistent() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");

    for wf in ["deploy-aws.yml", "downsize-instance.yml"] {
        let path = root.join(".github/workflows").join(wf);
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

        if !src.contains("QDB_MEM_LIMIT") {
            continue;
        }

        assert!(
            src.contains("MemTotal"),
            "{wf} sets QDB_MEM_LIMIT without reading the host's MemTotal. A \
             hardcoded cap is correct for exactly one instance size and wrong \
             for every other one — and this repo has changed instance size four \
             times"
        );

        // No assignment may carry a bare literal size.
        for (n, line) in src.lines().enumerate() {
            // These commands live inside a JSON array, so a comment line
            // starts with `"#`, not `#`. A first draft checked only `#` and
            // duly reported a COMMENT as a hardcoded assignment — the exact
            // false-positive class this file's other guards warn about.
            let code = line.trim_start().trim_start_matches('"').trim_start();
            if line.contains("QDB_MEM_LIMIT=") && !code.starts_with('#') {
                assert!(
                    !line.contains("QDB_MEM_LIMIT=12g") && !line.contains("QDB_MEM_LIMIT=1g"),
                    "{wf}:{} assigns a hardcoded QDB_MEM_LIMIT. Derive it from \
                     MemTotal so a rollback to a smaller instance cannot leave \
                     the database capped above the host's own RAM.\n  {line}",
                    n + 1
                );
            }
        }

        // And no verification may compare against a bare byte literal — that is
        // exactly how the write and its check came to disagree.
        for (n, line) in src.lines().enumerate() {
            if line.contains("QDB_LIMIT_BYTES") && line.contains("!=") {
                assert!(
                    !line.contains("1073741824\\\"") && !line.contains("\\\"1073741824"),
                    "{wf}:{} verifies the container memory limit against a \
                     hardcoded byte literal. The write is derived, so a literal \
                     check can only ever agree with it by luck — and it did not: \
                     it demanded 1g while the write said 12g.\n  {line}",
                    n + 1
                );
            }
        }
    }
}

//! Guard: the kernel tuning must keep fitting the LOCKED instance and the
//! SIXTEEN WebSockets it is sized for.
//!
//! ## The rot this exists to stop
//!
//! `QDB_MEM_LIMIT` sat at `1g` for weeks after the host grew — sized for a
//! 4-SID universe, silently starving a machine with four times the RAM. Nothing
//! failed; the number just stopped meaning anything. Kernel tuning rots the same
//! way and worse, because its failure mode is *dropped market data* rather than
//! a slow query: when the receive queue fills, TCP shrinks the advertised window
//! and the SERVER discards updates. No write-ahead log, ring buffer or spill
//! file can recover data that never arrived.
//!
//! The specific rot already caught once: `99-tickvault-net.conf` carried a
//! warning block ending "do not bring up all 16 connections on a 4 GiB box",
//! written when the box was t4g.medium. The instance lock moved to r8g.xlarge
//! (4 vCPU / 32 GiB) on 2026-08-08 and the warning stayed — so the file told
//! anyone reading it that the 16-socket revival was unsafe, when the arithmetic
//! that made it unsafe no longer applied.
//!
//! ## How this guard avoids being rot itself
//!
//! It does not hardcode the budget. It RECOMPUTES it every run from the three
//! files that actually decide it:
//!
//! - `deploy/aws/sysctl/99-tickvault-net.conf`  — the per-socket ceiling
//! - `deploy/docker/docker-compose.yml`         — QuestDB's memory cap
//! - `deploy/aws/terraform/variables.tf`        — the locked instance type
//!
//! Change any one of them in a direction that breaks the fit and this fails.
//! The only hardcoded input is the instance-type → RAM table, which is a
//! property of AWS, not of this repo.

use std::path::PathBuf;
use std::process::Command;

// ---------------------------------------------------------------------------
// The 16 sockets, from websocket-connection-scope-lock.md (operator, 2026-08-09)
// ---------------------------------------------------------------------------

/// 5 main-feed + 5 depth-20 + 5 depth-200 + 1 order-update.
const AUTHORIZED_WEBSOCKETS: u64 = 16;

/// Dhan pings every 10 s and CLOSES if the client has not answered within 40 s.
/// Documented for BOTH the live feed (`docs/dhan-ref/03`) and full market depth
/// (`docs/dhan-ref/04`), so it binds all fifteen market-data sockets.
const DHAN_PING_DEADLINE_SECS: u64 = 40;

/// The app's own idle-reconnect trigger (`websocket/idle_watchdog`).
const APP_IDLE_WATCHDOG_SECS: u64 = 27;

/// RAM per instance type, in GiB. A property of AWS, not of this repo — the only
/// hardcoded input here, and the reason a type not in this table is a hard fail
/// rather than a guess.
const INSTANCE_RAM_GIB: &[(&str, u64)] = &[
    ("t4g.medium", 4),
    ("t4g.large", 8),
    ("m8g.large", 8),
    ("r8g.large", 16),
    ("r8g.xlarge", 32),
    ("r8g.2xlarge", 64),
];

/// Everything on the host that is NOT QuestDB and NOT socket buffers, at the TOP
/// of its range, in GiB. Sourced from the sizing table in
/// `deploy/aws/terraform/variables.tf` and `daily-universe-scope-expansion`
/// §7 Rule 2: a day of ticks resident (7.2) + app remainder (4.0) + OS and page
/// cache (4.0) + candle slots and seal ring (0.1) = 15.3, ROUNDED UP to 16.
///
/// Deliberately the WORST case, and deliberately rounded against ourselves. A
/// budget that only balances on optimistic assumptions is not a budget. This is
/// why the total here (30 GiB) is slightly above the 29.3 GiB the conf file
/// states: the conf shows the real arithmetic, the guard leaves no rounding
/// slack to hide in.
const NON_QUESTDB_NON_SOCKET_GIB_HIGH: u64 = 16;

const SYSCTL_CONF: &str = "deploy/aws/sysctl/99-tickvault-net.conf";
const VERIFIER: &str = "deploy/aws/sysctl/verify-net-tuning.sh";
const USER_DATA: &str = "deploy/aws/terraform/user-data.sh.tftpl";
const COMPOSE: &str = "deploy/docker/docker-compose.yml";
const TF_VARS: &str = "deploy/aws/terraform/variables.tf";

fn repo_root() -> PathBuf {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .expect("kernel_tuning_16ws_guard: `git rev-parse` failed (needs a git checkout)");
    assert!(out.status.success(), "not a git checkout");
    PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(repo_root().join(rel))
        .unwrap_or_else(|e| panic!("kernel_tuning_16ws_guard: cannot read {rel}: {e}"))
}

/// Pull `key = value` out of a sysctl conf, ignoring `#` comments.
///
/// A commented-out setting must read as ABSENT, not as set — otherwise the
/// rationale prose in that file (which quotes every value it explains) would
/// satisfy checks the live settings no longer do.
fn sysctl_value(conf: &str, key: &str) -> Option<u64> {
    conf.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .find_map(|line| {
            let (k, v) = line.split_once('=')?;
            (k.trim() == key).then(|| v.trim().parse::<u64>().ok())?
        })
}

/// Parse a docker `mem_limit: ${VAR:-12g}` default into bytes.
fn compose_mem_limit_bytes(compose: &str, service_hint: &str) -> Option<u64> {
    let idx = compose.find(service_hint)?;
    let rest = &compose[idx..];
    let line = rest
        .lines()
        .find(|l| l.trim_start().starts_with("mem_limit:"))?;
    // Take the text after the last `-` (the shell default) or after the colon.
    let raw = line
        .rsplit_once(":-")
        .map(|(_, r)| r.trim_end_matches(['}', ' ']))
        .unwrap_or_else(|| line.split_once("mem_limit:").map_or("", |(_, r)| r).trim());
    let raw = raw.trim();
    let (digits, unit) = raw.split_at(raw.find(|c: char| !c.is_ascii_digit())?);
    let n: u64 = digits.parse().ok()?;
    match unit.trim().to_ascii_lowercase().as_str() {
        "g" | "gb" | "gi" | "gib" => Some(n * 1024 * 1024 * 1024),
        "m" | "mb" | "mi" | "mib" => Some(n * 1024 * 1024),
        _ => None,
    }
}

/// Pull the terraform `instance_type` default.
fn locked_instance_type(tf: &str) -> Option<String> {
    let idx = tf.find("variable \"instance_type\"")?;
    let rest = &tf[idx..];
    let line = rest
        .lines()
        .find(|l| l.trim_start().starts_with("default"))?;
    let (_, v) = line.split_once('=')?;
    Some(v.trim().trim_matches('"').to_string())
}

const GIB: u64 = 1024 * 1024 * 1024;

// ---------------------------------------------------------------------------
// The arithmetic — the whole point of this file
// ---------------------------------------------------------------------------

#[test]
fn sixteen_sockets_of_receive_buffer_fit_the_locked_instance() {
    let conf = read(SYSCTL_CONF);
    let rmem_max = sysctl_value(&conf, "net.core.rmem_max")
        .expect("99-tickvault-net.conf no longer sets net.core.rmem_max");

    let instance = locked_instance_type(&read(TF_VARS))
        .expect("variables.tf no longer declares an instance_type default");
    let host_gib = INSTANCE_RAM_GIB
        .iter()
        .find(|(name, _)| *name == instance)
        .map(|(_, gib)| *gib)
        .unwrap_or_else(|| {
            panic!(
                "instance type `{instance}` is not in this guard's RAM table. \
                 Add it WITH its real GiB — the socket-buffer budget below is \
                 meaningless without knowing how much RAM the box has, and \
                 guessing is how the QDB_MEM_LIMIT=1g rot happened."
            )
        });

    // Worst case: every authorized socket pinned at the per-socket ceiling.
    let socket_worst_case = rmem_max * AUTHORIZED_WEBSOCKETS;

    let questdb = compose_mem_limit_bytes(&read(COMPOSE), "tv-questdb")
        .expect("docker-compose.yml no longer declares a QuestDB mem_limit default");

    let total = socket_worst_case + questdb + NON_QUESTDB_NON_SOCKET_GIB_HIGH * GIB;
    let host = host_gib * GIB;

    assert!(
        total <= host,
        "MEMORY BUDGET BLOWN on {instance} ({host_gib} GiB).\n\
         \n\
           {sockets} sockets x {per} MiB per-socket ceiling = {sock} MiB\n\
           QuestDB container cap                           = {qdb} MiB\n\
           App + OS + page cache, worst case               = {other} MiB\n\
           ------------------------------------------------------------\n\
           TOTAL                                           = {tot} MiB\n\
           HOST                                            = {hst} MiB\n\
         \n\
         One of four things changed: the instance shrank, QuestDB's cap grew, \
         the per-socket ceiling grew, or the authorized socket count grew. \
         Whichever it was, the kernel can now be asked for more receive buffer \
         than the box has — and the way that failure presents is DROPPED MARKET \
         DATA at the 09:15 open, not an OOM you can read in a log.\n\
         \n\
         The fix is NOT to lower rmem_max blindly: that caps the single-socket \
         burst absorption the file exists to provide. Set SO_RCVBUF per \
         connection app-side to (budget / actual connection count) so the total \
         is bounded by design (PR #1738), or move to a bigger instance under a \
         dated operator quote.",
        sockets = AUTHORIZED_WEBSOCKETS,
        per = rmem_max / (1024 * 1024),
        sock = socket_worst_case / (1024 * 1024),
        qdb = questdb / (1024 * 1024),
        other = NON_QUESTDB_NON_SOCKET_GIB_HIGH * 1024,
        tot = total / (1024 * 1024),
        hst = host / (1024 * 1024),
    );

    // The budget must not merely fit — it must fit with room the operator can
    // see. A budget that balances to the last byte is one rounding error from
    // an OOM kill of QuestDB mid-session.
    let headroom = host - total;
    assert!(
        headroom >= GIB,
        "budget fits {instance} with only {} MiB of headroom — under 1 GiB is \
         not a margin, it is a coincidence",
        headroom / (1024 * 1024)
    );
}

#[test]
fn sysctl_file_names_the_locked_instance_it_was_sized_for() {
    // Without this, an instance change leaves the file's arithmetic silently
    // describing a machine that no longer exists — exactly the state the
    // 4 GiB warning block was found in.
    let conf = read(SYSCTL_CONF);
    let instance = locked_instance_type(&read(TF_VARS)).expect("no instance_type default");
    assert!(
        conf.contains(&instance),
        "99-tickvault-net.conf does not mention the locked instance type \
         `{instance}`. Its memory budget is stated per-host, so it must name \
         the host it was computed for; otherwise the next instance change \
         leaves the arithmetic quietly wrong."
    );
}

#[test]
fn the_retired_four_gib_warning_is_not_resurrected() {
    // The literal rot this guard was written after. Kept as its own assertion
    // because a future merge could re-introduce the block wholesale.
    let conf = read(SYSCTL_CONF);
    assert!(
        !conf.contains("do not bring up all 16 connections"),
        "99-tickvault-net.conf still carries the retired t4g.medium (4 GiB) \
         warning against bringing up all 16 connections. That arithmetic was \
         superseded by the r8g.xlarge lock (2026-08-08). A stale prohibition is \
         as damaging as a stale permission: it makes the operator distrust the \
         file, or blocks authorized work for a reason that no longer holds."
    );
}

#[test]
fn keepalive_ladder_outlasts_both_application_deadlines() {
    let conf = read(SYSCTL_CONF);
    let time = sysctl_value(&conf, "net.ipv4.tcp_keepalive_time").expect("no keepalive_time");
    let intvl = sysctl_value(&conf, "net.ipv4.tcp_keepalive_intvl").expect("no keepalive_intvl");
    let probes = sysctl_value(&conf, "net.ipv4.tcp_keepalive_probes").expect("no keepalive_probes");
    let ladder = time + intvl * probes;

    assert!(
        ladder > DHAN_PING_DEADLINE_SECS,
        "TCP keepalive declares a dead peer after {ladder}s, which is at or \
         inside Dhan's {DHAN_PING_DEADLINE_SECS}s ping deadline. The kernel \
         would tear down sockets the application layer was still managing, \
         turning one recoverable stall into a simultaneous reconnect storm \
         across all {AUTHORIZED_WEBSOCKETS} connections."
    );
    assert!(
        ladder > APP_IDLE_WATCHDOG_SECS,
        "keepalive ladder ({ladder}s) races the app's {APP_IDLE_WATCHDOG_SECS}s \
         idle watchdog — the app must always detect and re-dial on its own terms"
    );
    assert!(
        time <= 7200 / 2,
        "keepalive_time = {time}s is at or near the stock 7200s default, which \
         leaves the half-open-path backstop effectively disabled. Bigger is NOT \
         better for this one setting."
    );
}

// ---------------------------------------------------------------------------
// The verifier — a check nobody runs is not a check
// ---------------------------------------------------------------------------

#[test]
fn user_data_runs_the_verifier_after_applying_the_sysctl_file() {
    let ud = read(USER_DATA);
    assert!(
        ud.contains("99-tickvault-net.conf"),
        "user-data no longer installs the sysctl file"
    );
    assert!(
        ud.contains("verify-net-tuning.sh"),
        "user-data no longer runs verify-net-tuning.sh. Applying tuning without \
         verifying it lands is the false-OK this whole chain exists to prevent."
    );
    let apply = ud
        .find("sysctl --system")
        .expect("user-data no longer applies sysctls");
    let verify = ud.find("verify-net-tuning.sh").expect("no verifier call");
    assert!(
        apply < verify,
        "user-data verifies the tuning BEFORE applying it — the verdict would \
         describe the pre-tuning kernel"
    );
}

#[test]
fn verifier_checks_every_load_bearing_setting_the_sysctl_file_sets() {
    // The original user-data check read ONE value (rmem_max) and reported
    // "APPLIED". Eight of the nine settings could have silently defaulted
    // behind a green verdict. This pins that the verifier's coverage tracks
    // the file it verifies.
    let verifier = read(VERIFIER);
    for key in [
        "net.core.rmem_max",
        "net.core.rmem_default",
        "net.core.wmem_max",
        "net.core.netdev_max_backlog",
        "net.core.netdev_budget",
        "net.core.somaxconn",
        "net.ipv4.tcp_keepalive_time",
        "net.ipv4.tcp_rmem",
        "vm.max_map_count",
        "vm.min_free_kbytes",
    ] {
        assert!(
            verifier.contains(key),
            "verify-net-tuning.sh does not check `{key}`, but \
             99-tickvault-net.conf sets it. A verifier that covers a subset \
             reports success it has not established."
        );
    }
}

#[test]
fn verifier_exits_nonzero_when_the_kernel_is_untuned() {
    // Runs the real script against THIS host, which is a stock CI container and
    // therefore genuinely untuned. If the script ever returned 0 here it would
    // be reporting a tuning that provably is not present.
    let path = repo_root().join(VERIFIER);
    let out = Command::new("bash")
        .arg(&path)
        .output()
        .expect("could not execute verify-net-tuning.sh");
    let stdout = String::from_utf8_lossy(&out.stdout);

    // A tuned host would legitimately pass; only assert the failure shape when
    // this host is actually untuned, so the guard cannot false-fail on a box
    // that has the settings applied.
    if stdout.contains("BELOW") || stdout.contains("UNREADABLE") {
        assert!(
            !out.status.success(),
            "verify-net-tuning.sh reported settings BELOW target yet exited 0 — \
             a non-zero exit is the only part of this a caller can act on:\n{stdout}"
        );
        assert!(
            stdout.contains("NOT APPLIED"),
            "a failing verification must say so in plain words, not just in an \
             exit code nobody reads:\n{stdout}"
        );
    } else {
        assert!(
            out.status.success(),
            "no failures reported yet exited non-zero"
        );
    }
}

#[test]
fn verifier_is_executable_and_syntactically_valid() {
    let path = repo_root().join(VERIFIER);
    let out = Command::new("bash")
        .args(["-n", path.to_str().expect("path")])
        .output()
        .expect("bash -n failed to run");
    assert!(
        out.status.success(),
        "verify-net-tuning.sh has a shell syntax error:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---------------------------------------------------------------------------
// Parser self-tests — a silent parse failure makes every assertion above vacuous
// ---------------------------------------------------------------------------

#[test]
fn parsers_are_self_tested() {
    let conf = "# net.core.rmem_max = 1\nnet.core.rmem_max = 134217728\nx = 5\n";
    assert_eq!(sysctl_value(conf, "net.core.rmem_max"), Some(134_217_728));
    assert_eq!(sysctl_value(conf, "absent.key"), None);
    assert_eq!(
        sysctl_value("# net.core.rmem_max = 999\n", "net.core.rmem_max"),
        None,
        "a commented line must not read as a setting — the conf file quotes \
         every value it explains in prose"
    );

    let compose = "  tv-questdb:\n    image: x\n    mem_limit: ${QDB_MEM_LIMIT:-12g}\n";
    assert_eq!(
        compose_mem_limit_bytes(compose, "tv-questdb"),
        Some(12 * GIB)
    );
    assert_eq!(
        compose_mem_limit_bytes("  tv-questdb:\n    mem_limit: 512m\n", "tv-questdb"),
        Some(512 * 1024 * 1024)
    );
    assert_eq!(compose_mem_limit_bytes("nothing here", "tv-questdb"), None);

    let tf = "variable \"instance_type\" {\n  type = string\n  default = \"r8g.xlarge\"\n}\n";
    assert_eq!(locked_instance_type(tf), Some("r8g.xlarge".to_string()));
    assert_eq!(locked_instance_type("variable \"other\" {}"), None);
}

//! `tickvault host-tuning <bbr|verify|apply>`: the per-boot host tuning that
//! used to be two shell scripts and an inline `/bin/sh -c` (audit-plan D6b).
//!
//! `deploy/systemd/tickvault-host-tuning.service` runs these as root, ordered
//! before `tickvault.service`, on every boot and on every deploy:
//!
//! 1. `bbr` (was the unit's `/bin/sh -c 'modprobe tcp_bbr && …'`): load the
//!    BBR module and, only if that worked, write the sysctl and modules-load
//!    drop-ins. Writing the sysctl value without the module would make
//!    `sysctl --system` fail wholesale and strip the receive buffers with it.
//! 2. `verify` (was `deploy/aws/sysctl/verify-net-tuning.sh`): check every
//!    load-bearing value of `99-tickvault-net.conf` actually landed, write the
//!    verdict to `/opt/tickvault/net-tuning.status`, exit 1 when anything is
//!    missing or below target. Reports, never halts the box.
//! 3. `apply` (was `deploy/aws/host-tuning/apply-host-tuning.sh`): transparent
//!    hugepages to `madvise`, the clock-discipline check, NIC IRQ steering, and
//!    the two systemd drop-ins that keep the app's CPU and memory limits
//!    satisfiable on a smaller host. Always exits 0.
//!
//! The behaviour is the scripts', check for check and message for message.
//! Three deliberate differences, each in the safe direction:
//!
//! - `verify` reads `/proc/sys` directly instead of running `sysctl -n` (the
//!   same file the `sysctl` binary reads), and a value that is present but not
//!   an integer now counts as UNREADABLE. The script's `[ "$got" -lt … ]`
//!   errored on such a value and fell through to "ok".
//! - The verdict line carries the journald priority prefix `<3>` (error) when
//!   tuning is NOT applied, instead of a separate `logger -p daemon.err` call.
//!   The unit's `StandardOutput=journal` turns the prefix into the priority.
//! - The default-route interface is read from `/proc/net/route` instead of
//!   `ip -o route show default`; the core count comes from
//!   `std::thread::available_parallelism` instead of `nproc` (equal under this
//!   unit, which sets no CPU quota or affinity).
//!
//! Cold path: runs once per boot as its own process, before the trading app
//! starts, and returns before any tokio runtime is built.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

/// The CLI word that selects this module: `tickvault host-tuning <action>`.
pub const HOST_TUNING_SUBCOMMAND: &str = "host-tuning";

/// Where `verify` leaves its verdict for the operator and the deploy log.
const NET_TUNING_STATUS_PATH: &str = "/opt/tickvault/net-tuning.status";
/// Journald tag the verifier's verdict line carries.
const NET_TUNING_TAG: &str = "tickvault-net-tuning";

/// `key`, minimum. Every value is load-bearing for the 16-WebSocket feed; see
/// `deploy/aws/sysctl/99-tickvault-net.conf` for the arithmetic behind each.
/// Kept in lockstep with that file by `crates/app/tests/kernel_tuning_16ws_guard.rs`.
const SCALAR_MINIMUMS: [(&str, u64); 8] = [
    ("net.core.rmem_max", 134_217_728),
    ("net.core.rmem_default", 16_777_216),
    ("net.core.wmem_max", 16_777_216),
    ("net.core.netdev_max_backlog", 65_536),
    ("net.core.netdev_budget", 1_200),
    ("net.core.somaxconn", 4_096),
    ("vm.max_map_count", 1_048_576),
    ("vm.min_free_kbytes", 262_144),
];
/// `net.ipv4.tcp_rmem` is "min default max"; this is the floor for its MAX.
const TCP_RMEM_MAX_MINIMUM: u64 = 134_217_728;
/// Keepalive must fire within this many seconds of idleness …
const KEEPALIVE_TIME_MAX_SECS: u64 = 60;
/// … but the whole ladder must outlast Dhan's 40 s ping deadline.
const KEEPALIVE_LADDER_MIN_EXCLUSIVE_SECS: u64 = 40;
/// `vm.dirty_ratio` ceiling; the stock 20 arms a multi-GiB writeback stall.
const DIRTY_RATIO_MAX: u64 = 10;

const BBR_SYSCTL_PATH: &str = "/etc/sysctl.d/99-tickvault-bbr.conf";
const BBR_SYSCTL_LINE: &str = "net.ipv4.tcp_congestion_control = bbr\n";
const BBR_MODULES_PATH: &str = "/etc/modules-load.d/tickvault-bbr.conf";
const BBR_MODULES_LINE: &str = "tcp_bbr\n";

const THP_ENABLED_PATH: &str = "/sys/kernel/mm/transparent_hugepage/enabled";
const THP_DEFRAG_PATH: &str = "/sys/kernel/mm/transparent_hugepage/defrag";
/// THP only for code that asks: QuestDB benefits, the feed path allocates
/// almost nothing steady-state. `never` would trade one tenant for the other.
const THP_MODE: &str = "madvise";
/// A fresh boot may not have reached a time source yet.
const CHRONY_SETTLE_SECS: u64 = 2;

/// `tickvault.service` carries `AllowedCPUs=1-2`; that set needs this many cores.
const TV_APP_CPUS_MIN_CORES: usize = 3;
const CPU_DROPIN_PATH: &str = "/etc/systemd/system/tickvault.service.d/90-cpu-guard.conf";
const MEMORY_DROPIN_PATH: &str = "/etc/systemd/system/tickvault.service.d/91-memory-guard.conf";
/// The installed unit, read for its `MemoryHigh=` value (never restated here).
const INSTALLED_APP_UNIT_PATH: &str = "/etc/systemd/system/tickvault.service";
/// QuestDB's share of RAM: the formula deploy-aws.yml writes into
/// `QDB_MEM_LIMIT` (4/10 of MemTotal, floor 1 GiB, cap 12 GiB).
const QDB_SHARE_NUMERATOR: u64 = 4;
const QDB_SHARE_DENOMINATOR: u64 = 10;
const QDB_SHARE_MAX_G: u64 = 12;
/// OS and page-cache floor kept out of the app's lowered ceiling.
const OS_FLOOR_G: u64 = 1;
const KIB_PER_GIB: u64 = 1_048_576;

/// Dispatch `tickvault host-tuning <action>`. `None` when `args` is not this
/// subcommand, so the caller continues to the normal app boot; otherwise the
/// process exit code.
pub fn run_cli(args: &[String]) -> Option<i32> {
    if args.get(1).map(String::as_str) != Some(HOST_TUNING_SUBCOMMAND) {
        return None;
    }
    Some(match args.get(2).map(String::as_str) {
        Some("bbr") => run_bbr(),
        Some("verify") => run_verify(),
        Some("apply") => run_apply(),
        other => {
            emit(&format!(
                "host-tuning: unknown action {other:?}; expected bbr, verify or apply"
            ));
            2
        }
    })
}

/// One line to stdout, which the unit sends to journald. A failed write is
/// ignored: there is nowhere else to report it.
fn emit(line: &str) {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}").unwrap_or_default();
}

// ============================ bbr ============================

fn run_bbr() -> i32 {
    if !command_succeeds("modprobe", &["tcp_bbr"]) {
        emit(
            "host-tuning: tcp_bbr module unavailable — congestion control left at the kernel default",
        );
        return 1;
    }
    let sysctl_ok = std::fs::write(BBR_SYSCTL_PATH, BBR_SYSCTL_LINE).is_ok();
    let modules_ok = std::fs::write(BBR_MODULES_PATH, BBR_MODULES_LINE).is_ok();
    if sysctl_ok && modules_ok {
        emit("host-tuning: BBR enabled (module loaded, drop-ins written)");
        0
    } else {
        emit(&format!(
            "host-tuning: WARNING BBR module loaded but a drop-in write failed \
             ({BBR_SYSCTL_PATH} ok={sysctl_ok}, {BBR_MODULES_PATH} ok={modules_ok})"
        ));
        1
    }
}

// ============================ verify ============================

/// Result of checking every load-bearing kernel value.
#[derive(Debug, PartialEq, Eq)]
struct Verification {
    failures: usize,
    rows: Vec<String>,
}

fn row(label: &str, text: &str) -> String {
    format!("{label:<10} {text}")
}

/// Parse a non-negative integer sysctl value; anything else is `None`.
fn parse_u64(raw: &str) -> Option<u64> {
    raw.trim().parse::<u64>().ok()
}

/// Check every value through `read` (a `sysctl` key to its raw text, `None`
/// when unreadable). Pure: the real reader is `read_proc_sys`.
fn verify_values(read: &dyn Fn(&str) -> Option<String>) -> Verification {
    let mut v = Verification {
        failures: 0,
        rows: Vec::with_capacity(SCALAR_MINIMUMS.len() + 3),
    };
    for (key, want) in SCALAR_MINIMUMS {
        match read(key).as_deref().and_then(parse_u64) {
            None => {
                v.rows
                    .push(row("UNREADABLE", &format!("{key} (wanted >= {want})")));
                v.failures += 1;
            }
            Some(got) if got < want => {
                v.rows
                    .push(row("BELOW", &format!("{key} = {got} (wanted >= {want})")));
                v.failures += 1;
            }
            Some(got) => v.rows.push(row("ok", &format!("{key} = {got}"))),
        }
    }

    // tcp_rmem is "min default max": the THIRD field caps a busy socket.
    let rmem_max =
        read("net.ipv4.tcp_rmem").and_then(|s| s.split_whitespace().nth(2).and_then(parse_u64));
    match rmem_max {
        Some(m) if m >= TCP_RMEM_MAX_MINIMUM => {
            v.rows
                .push(row("ok", &format!("net.ipv4.tcp_rmem max = {m}")));
        }
        other => {
            let shown = other.map_or_else(|| "unreadable".to_string(), |m| m.to_string());
            v.rows.push(row(
                "BELOW",
                &format!("net.ipv4.tcp_rmem max = {shown} (wanted >= {TCP_RMEM_MAX_MINIMUM})"),
            ));
            v.failures += 1;
        }
    }

    // Keepalive needs a WINDOW: the stock 7200 s would pass any ">= minimum".
    let num = |key: &str| read(key).as_deref().and_then(parse_u64).unwrap_or(0);
    let ka_time = num("net.ipv4.tcp_keepalive_time");
    let ka_intvl = num("net.ipv4.tcp_keepalive_intvl");
    let ka_probes = num("net.ipv4.tcp_keepalive_probes");
    let ka_total = ka_time.saturating_add(ka_intvl.saturating_mul(ka_probes));
    if ka_time > KEEPALIVE_TIME_MAX_SECS || ka_time == 0 {
        v.rows.push(row(
            "BELOW",
            &format!(
                "net.ipv4.tcp_keepalive_time = {ka_time} (wanted <= {KEEPALIVE_TIME_MAX_SECS}; stock 7200 disables the backstop)"
            ),
        ));
        v.failures += 1;
    } else if ka_total <= KEEPALIVE_LADDER_MIN_EXCLUSIVE_SECS {
        v.rows.push(row(
            "BELOW",
            &format!("keepalive ladder = {ka_total}s (must exceed Dhan's 40 s ping deadline)"),
        ));
        v.failures += 1;
    } else {
        v.rows.push(row(
            "ok",
            &format!(
                "keepalive ladder = {ka_total}s (fires after Dhan's 40 s, after the app's 27 s)"
            ),
        ));
    }

    // Writeback ratios are CEILINGS for the same reason.
    let dirty = num("vm.dirty_ratio");
    let dirty_bg = num("vm.dirty_background_ratio");
    if dirty == 0 || dirty > DIRTY_RATIO_MAX {
        v.rows.push(row(
            "BELOW",
            &format!(
                "vm.dirty_ratio = {dirty} (wanted 1..{DIRTY_RATIO_MAX}; stock 20 arms a multi-GiB writeback stall)"
            ),
        ));
        v.failures += 1;
    } else if dirty_bg == 0 || dirty_bg >= dirty {
        v.rows.push(row(
            "BELOW",
            &format!(
                "vm.dirty_background_ratio = {dirty_bg} (must be >0 and below vm.dirty_ratio = {dirty})"
            ),
        ));
        v.failures += 1;
    } else {
        v.rows.push(row(
            "ok",
            &format!("writeback ratios = {dirty_bg}% background / {dirty}% synchronous"),
        ));
    }
    v
}

fn verdict(failures: usize) -> String {
    if failures == 0 {
        "APPLIED — all kernel tuning verified for the 16-WebSocket feed".to_string()
    } else {
        format!(
            "NOT APPLIED — {failures} setting(s) missing or below target. The kernel \
             may discard market data under load and no downstream buffer can recover it. \
             Re-run: cp /opt/tickvault/repo/deploy/aws/sysctl/99-tickvault-net.conf \
             /etc/sysctl.d/ && sysctl --system"
        )
    }
}

/// The status file: a timestamped verdict, a blank line, then one row per check.
fn status_file_text(utc_stamp: &str, v: &Verification) -> String {
    let mut s = format!("{utc_stamp}: {}\n", verdict(v.failures));
    for r in &v.rows {
        s.push('\n');
        s.push_str(r);
    }
    s.push('\n');
    s
}

/// `net.core.rmem_max` -> `/proc/sys/net/core/rmem_max`, read and trimmed.
fn read_proc_sys(key: &str) -> Option<String> {
    let path = format!("/proc/sys/{}", key.replace('.', "/"));
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// Run the verifier against this host's kernel and return what it found,
/// without writing the status file. Public so the kernel-tuning guard can
/// run the real check on a stock CI host.
pub fn verify_this_host() -> (usize, String) {
    let v = verify_values(&read_proc_sys);
    let text = status_file_text("now", &v);
    (v.failures, text)
}

fn run_verify() -> i32 {
    let v = verify_values(&read_proc_sys);
    let stamp = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let written = Path::new(NET_TUNING_STATUS_PATH)
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(NET_TUNING_STATUS_PATH, status_file_text(&stamp, &v)));
    if let Err(err) = written {
        emit(&format!(
            "<3>{NET_TUNING_TAG}: could not write {NET_TUNING_STATUS_PATH}: {err}"
        ));
    }
    // `<3>` = journald priority "err" for a verdict the operator must see.
    let priority = if v.failures == 0 { "" } else { "<3>" };
    emit(&format!(
        "{priority}{NET_TUNING_TAG}: {}",
        verdict(v.failures)
    ));
    for r in &v.rows {
        emit(r);
    }
    i32::from(v.failures != 0)
}

// ============================ apply ============================

fn run_apply() -> i32 {
    apply_thp();
    check_clock();
    steer_nic_irqs();
    guard_app_cpus();
    guard_app_memory();
    // Never non-zero: a host with default tuning is slower, a host whose boot
    // aborted has no app at all.
    0
}

fn apply_thp() {
    if std::fs::OpenOptions::new()
        .write(true)
        .open(THP_ENABLED_PATH)
        .is_err()
    {
        emit("host-tuning: WARNING transparent_hugepage not writable — left at distro default");
        return;
    }
    for path in [THP_ENABLED_PATH, THP_DEFRAG_PATH] {
        if let Err(err) = std::fs::write(path, THP_MODE) {
            emit(&format!(
                "host-tuning: WARNING could not write {path}: {err}"
            ));
        }
    }
    let now = std::fs::read_to_string(THP_ENABLED_PATH)
        .map_or_else(|_| "unreadable".to_string(), |s| s.trim().to_string());
    emit(&format!("host-tuning: THP = {now}"));
}

/// `chronyc tracking` output -> "Reference ID value; System time value; ".
fn summarise_chrony_tracking(out: &str) -> String {
    let mut s = String::new();
    for line in out.lines() {
        if (line.contains("Reference ID") || line.contains("System time"))
            && let Some((_, value)) = line.split_once(": ")
        {
            s.push_str(value);
            s.push_str("; ");
        }
    }
    s
}

fn check_clock() {
    // Every latency number is (receive instant − exchange timestamp); on an
    // undisciplined clock that measures skew and reports it as feed latency.
    if !command_exists("chronyc", &["-v"]) {
        emit(
            "host-tuning: WARNING chrony absent — host clock is UNDISCIPLINED and every latency metric is untrustworthy",
        );
        return;
    }
    let _ = command_succeeds("systemctl", &["enable", "--now", "chronyd"]);
    std::thread::sleep(Duration::from_secs(CHRONY_SETTLE_SECS));
    match command_stdout("chronyc", &["tracking"]) {
        Some(out) => emit(&format!(
            "host-tuning: clock OK — {}",
            summarise_chrony_tracking(&out)
        )),
        None => emit(
            "host-tuning: WARNING chronyd present but not yet tracking a source — latency numbers may be skewed",
        ),
    }
}

/// The interface of the first up IPv4 default route in `/proc/net/route`.
fn default_route_iface(route_table: &str) -> Option<String> {
    const RTF_UP: u32 = 0x1;
    route_table.lines().skip(1).find_map(|line| {
        let f: Vec<&str> = line.split_whitespace().collect();
        let (iface, dest, flags, mask) = (f.first()?, f.get(1)?, f.get(3)?, f.get(7)?);
        let up = u32::from_str_radix(flags, 16).is_ok_and(|v| v & RTF_UP != 0);
        (*dest == "00000000" && *mask == "00000000" && up).then(|| (*iface).to_string())
    })
}

/// IRQ numbers whose `/proc/interrupts` line names `nic` (ENA names its
/// vectors after the interface, so every queue vector is caught).
fn nic_irqs(interrupts: &str, nic: &str) -> Vec<u32> {
    interrupts
        .lines()
        .filter(|l| l.contains(nic))
        .filter_map(|l| l.split_whitespace().next()?.strip_suffix(':')?.parse().ok())
        .collect()
}

fn steer_nic_irqs() {
    let steer = std::env::var("TV_IRQ_STEER").unwrap_or_else(|_| "on".to_string());
    let irq_cpu = std::env::var("TV_IRQ_CPU").unwrap_or_else(|_| "0".to_string());
    if steer == "off" {
        emit(
            "host-tuning: IRQ steering DISABLED by TV_IRQ_STEER=off — NIC interrupts left to the kernel/irqbalance",
        );
        return;
    }
    // irqbalance re-spreads interrupts on a timer; static masks beside it
    // would LOOK applied and silently revert.
    if command_succeeds("systemctl", &["is-active", "--quiet", "irqbalance"]) {
        if command_succeeds("systemctl", &["disable", "--now", "irqbalance"]) {
            emit(
                "host-tuning: irqbalance STOPPED — static IRQ affinity would otherwise be re-spread on its timer",
            );
        } else {
            emit(
                "host-tuning: WARNING could not stop irqbalance — static IRQ affinity may be undone within minutes",
            );
        }
    } else {
        emit("host-tuning: irqbalance not running — static IRQ affinity will stick");
    }

    let nic = std::fs::read_to_string("/proc/net/route")
        .ok()
        .and_then(|t| default_route_iface(&t));
    let Some(nic) = nic else {
        emit(
            "host-tuning: WARNING could not resolve the default-route interface — NIC IRQs NOT steered",
        );
        return;
    };
    let interrupts = std::fs::read_to_string("/proc/interrupts").unwrap_or_default();
    let (mut moved, mut refused) = (0_usize, 0_usize);
    for irq in nic_irqs(&interrupts, &nic) {
        // Driver-managed vectors refuse writes; expected, not an error.
        if std::fs::write(format!("/proc/irq/{irq}/smp_affinity_list"), &irq_cpu).is_ok() {
            moved += 1;
        } else {
            refused += 1;
        }
    }
    emit(&format!(
        "host-tuning: NIC {nic} IRQs -> cpu {irq_cpu} (moved={moved} refused={refused})"
    ));
    if moved == 0 {
        emit(
            "host-tuning: WARNING no NIC IRQ was steered — softirq may still land on the app's core",
        );
    }
    // RPS stays disabled: softirq then runs on the core that took the IRQ.
}

/// What to do with one systemd drop-in.
#[derive(Debug, PartialEq, Eq)]
enum DropIn {
    /// Ensure the file holds exactly this text.
    Write(String),
    /// Delete it (the host grew back).
    Remove,
    /// Leave things as they are.
    Keep,
}

fn cpu_dropin_text(cores: usize) -> String {
    format!(
        "[Service]\n\
         # Written by `tickvault host-tuning apply`: this host has {cores} core(s), fewer than\n\
         # the {TV_APP_CPUS_MIN_CORES} the AllowedCPUs=1-2 partition needs. Empty assignment resets it.\n\
         AllowedCPUs=\n"
    )
}

/// Decide the CPU drop-in. Fewer cores than the partition names: run the app
/// UNCONFINED rather than pin it into a set the kernel cannot honour.
fn plan_cpu_guard(cores: usize, irq_cpu: &str, dropin_exists: bool) -> (DropIn, String) {
    if cores < TV_APP_CPUS_MIN_CORES {
        (
            DropIn::Write(cpu_dropin_text(cores)),
            format!(
                "host-tuning: WARNING only {cores} core(s) — app CPU confinement REMOVED (needs >= {TV_APP_CPUS_MIN_CORES})"
            ),
        )
    } else if dropin_exists {
        (
            DropIn::Remove,
            format!(
                "host-tuning: {cores} cores — removed the CPU-confinement escape hatch; AllowedCPUs=1-2 now applies"
            ),
        )
    } else {
        (
            DropIn::Keep,
            format!(
                "host-tuning: {cores} cores — app confined to CPUs 1-2, QuestDB to 2-3, IRQs on cpu {irq_cpu}"
            ),
        )
    }
}

/// The last `MemoryHigh=<n>G` value in a unit file.
fn unit_memory_high_g(unit: &str) -> Option<u64> {
    unit.lines()
        .filter_map(|l| {
            l.strip_prefix("MemoryHigh=")?
                .strip_suffix('G')?
                .parse()
                .ok()
        })
        .next_back()
}

/// `MemTotal` in KiB from `/proc/meminfo`.
fn meminfo_total_kib(meminfo: &str) -> Option<u64> {
    meminfo.lines().find_map(|l| {
        l.strip_prefix("MemTotal:")?
            .trim()
            .strip_suffix(" kB")?
            .trim()
            .parse()
            .ok()
    })
}

/// QuestDB's share in GiB: deploy-aws.yml's 4/10 of RAM, floor 1, cap 12.
fn questdb_share_g(mem_g: u64) -> u64 {
    (mem_g * QDB_SHARE_NUMERATOR / QDB_SHARE_DENOMINATOR).clamp(1, QDB_SHARE_MAX_G)
}

/// Decide the memory drop-in. It exists ONLY for a host where the unit's
/// `MemoryHigh` is at or above physical RAM: there the throttle, the WAL
/// catch-up stand-down (60 % of it) and the RESOURCE-02 page line (80 %) are
/// all inert. On a host where the unit value is reachable nothing is written,
/// so the locked host behaves exactly as the unit says.
fn plan_memory_guard(
    unit_high_g: Option<u64>,
    mem_g: u64,
    dropin_exists: bool,
) -> (DropIn, String) {
    let Some(unit_g) = unit_high_g.filter(|_| mem_g > 0) else {
        // An unreadable input changes NOTHING.
        let shown = unit_high_g.map_or_else(|| "unreadable".to_string(), |g| g.to_string());
        return (
            DropIn::Keep,
            format!(
                "host-tuning: memory guard SKIPPED (unit MemoryHigh='{shown}', MemTotal={mem_g}G)"
            ),
        );
    };
    if unit_g < mem_g {
        return if dropin_exists {
            (
                DropIn::Remove,
                format!(
                    "host-tuning: {mem_g}G host — removed the memory escape hatch; MemoryHigh={unit_g}G now applies"
                ),
            )
        } else {
            (
                DropIn::Keep,
                format!(
                    "host-tuning: {mem_g}G host — MemoryHigh={unit_g}G is reachable, no drop-in needed"
                ),
            )
        };
    }
    let qdb_g = questdb_share_g(mem_g);
    let want_g = mem_g
        .saturating_sub(qdb_g)
        .saturating_sub(OS_FLOOR_G)
        .max(1);
    let text = format!(
        "[Service]\n\
         # Written by `tickvault host-tuning apply`: this host has {mem_g}G, so the unit's\n\
         # MemoryHigh={unit_g}G can never be reached -- the throttle, the WAL\n\
         # catch-up stand-down and the RESOURCE-02 page line would all be inert.\n\
         # {mem_g}G total - {qdb_g}G QuestDB (the deploy formula) - {OS_FLOOR_G}G OS floor.\n\
         MemoryHigh={want_g}G\n"
    );
    (
        DropIn::Write(text),
        format!(
            "host-tuning: WARNING {mem_g}G host — MemoryHigh lowered {unit_g}G -> {want_g}G so the throttle and its derived alarms can fire"
        ),
    )
}

/// Carry out a drop-in decision, reloading systemd only when a file changed.
fn apply_dropin(path: &str, action: &DropIn) {
    let changed = match action {
        DropIn::Keep => false,
        DropIn::Remove => std::fs::remove_file(path).is_ok(),
        DropIn::Write(text) => {
            if std::fs::read_to_string(path).is_ok_and(|cur| cur == *text) {
                false
            } else {
                Path::new(path)
                    .parent()
                    .map_or(Ok(()), std::fs::create_dir_all)
                    .and_then(|()| std::fs::write(path, text))
                    .is_ok()
            }
        }
    };
    if changed {
        let _ = command_succeeds("systemctl", &["daemon-reload"]);
    }
}

fn guard_app_cpus() {
    let cores = std::thread::available_parallelism().map_or(0, std::num::NonZeroUsize::get);
    let irq_cpu = std::env::var("TV_IRQ_CPU").unwrap_or_else(|_| "0".to_string());
    let (action, message) = plan_cpu_guard(cores, &irq_cpu, Path::new(CPU_DROPIN_PATH).exists());
    apply_dropin(CPU_DROPIN_PATH, &action);
    emit(&message);
}

fn guard_app_memory() {
    let unit_g = std::fs::read_to_string(INSTALLED_APP_UNIT_PATH)
        .ok()
        .and_then(|u| unit_memory_high_g(&u));
    let mem_g = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| meminfo_total_kib(&m))
        .map_or(0, |kib| kib / KIB_PER_GIB);
    let (action, message) =
        plan_memory_guard(unit_g, mem_g, Path::new(MEMORY_DROPIN_PATH).exists());
    apply_dropin(MEMORY_DROPIN_PATH, &action);
    emit(&message);
}

// ============================ process helpers ============================

fn command_succeeds(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// True when `program` can be started at all (its exit code is ignored).
fn command_exists(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn command_stdout(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn tuned() -> BTreeMap<&'static str, &'static str> {
        BTreeMap::from([
            ("net.core.rmem_max", "134217728"),
            ("net.core.rmem_default", "16777216"),
            ("net.core.wmem_max", "16777216"),
            ("net.core.netdev_max_backlog", "65536"),
            ("net.core.netdev_budget", "1200"),
            ("net.core.somaxconn", "4096"),
            ("vm.max_map_count", "1048576"),
            ("vm.min_free_kbytes", "262144"),
            ("net.ipv4.tcp_rmem", "4096\t131072\t134217728"),
            ("net.ipv4.tcp_keepalive_time", "30"),
            ("net.ipv4.tcp_keepalive_intvl", "10"),
            ("net.ipv4.tcp_keepalive_probes", "3"),
            ("vm.dirty_ratio", "10"),
            ("vm.dirty_background_ratio", "5"),
        ])
    }

    fn run(map: &BTreeMap<&'static str, &'static str>) -> Verification {
        verify_values(&|k: &str| map.get(k).map(|v| (*v).to_string()))
    }

    #[test]
    fn test_run_cli_ignores_other_invocations() {
        assert_eq!(run_cli(&["tickvault".to_string()]), None);
        assert_eq!(
            run_cli(&["tickvault".to_string(), "--check-trading-day".to_string()]),
            None
        );
        assert_eq!(
            run_cli(&[
                "tickvault".to_string(),
                HOST_TUNING_SUBCOMMAND.to_string(),
                "bogus".to_string()
            ]),
            Some(2)
        );
        assert_eq!(
            run_cli(&["tickvault".to_string(), HOST_TUNING_SUBCOMMAND.to_string()]),
            Some(2)
        );
    }

    #[test]
    fn test_verify_passes_a_fully_tuned_kernel() {
        let v = run(&tuned());
        assert_eq!(v.failures, 0, "{:?}", v.rows);
        assert_eq!(v.rows.len(), SCALAR_MINIMUMS.len() + 3);
        assert!(v.rows.iter().all(|r| r.starts_with("ok ")));
        assert!(verdict(0).starts_with("APPLIED"));
    }

    #[test]
    fn test_verify_flags_each_scalar_below_or_unreadable() {
        for (key, _) in SCALAR_MINIMUMS {
            let mut m = tuned();
            m.insert(key, "1");
            let v = run(&m);
            assert_eq!(v.failures, 1, "{key}");
            assert!(
                v.rows
                    .iter()
                    .any(|r| r.starts_with("BELOW") && r.contains(key))
            );

            let mut m = tuned();
            m.remove(key);
            let v = run(&m);
            assert_eq!(v.failures, 1);
            assert!(
                v.rows
                    .iter()
                    .any(|r| r.starts_with("UNREADABLE") && r.contains(key))
            );

            // The script fell through to "ok" here; now it is a failure.
            let mut m = tuned();
            m.insert(key, "garbage");
            assert_eq!(run(&m).failures, 1, "non-integer {key} must fail");
        }
    }

    #[test]
    fn test_verify_reads_the_third_tcp_rmem_field() {
        let mut m = tuned();
        m.insert("net.ipv4.tcp_rmem", "134217728 134217728 4096");
        assert_eq!(run(&m).failures, 1, "the first field is not the ceiling");
        m.insert("net.ipv4.tcp_rmem", "4096 87380");
        let v = run(&m);
        assert_eq!(v.failures, 1);
        assert!(
            v.rows
                .iter()
                .any(|r| r.contains("tcp_rmem max = unreadable"))
        );
    }

    #[test]
    fn test_verify_keepalive_window_both_ends() {
        let mut m = tuned();
        m.insert("net.ipv4.tcp_keepalive_time", "7200");
        assert_eq!(run(&m).failures, 1, "stock 7200 disables the backstop");
        m.insert("net.ipv4.tcp_keepalive_time", "0");
        assert_eq!(run(&m).failures, 1);
        m.remove("net.ipv4.tcp_keepalive_time");
        assert_eq!(run(&m).failures, 1, "unreadable keepalive time");
        // 20 + 5 * 4 = 40: does not exceed Dhan's 40 s deadline.
        let mut m = tuned();
        m.insert("net.ipv4.tcp_keepalive_time", "20");
        m.insert("net.ipv4.tcp_keepalive_intvl", "5");
        m.insert("net.ipv4.tcp_keepalive_probes", "4");
        let v = run(&m);
        assert_eq!(v.failures, 1);
        assert!(v.rows.iter().any(|r| r.contains("keepalive ladder = 40s")));
        // 60 exactly is allowed.
        m.insert("net.ipv4.tcp_keepalive_time", "60");
        assert_eq!(run(&m).failures, 0);
    }

    #[test]
    fn test_verify_writeback_ratios_are_ceilings() {
        for (dirty, bg, fails) in [
            ("20", "10", 1),
            ("0", "0", 1),
            ("10", "10", 1),
            ("10", "0", 1),
            ("10", "9", 0),
            ("1", "0", 1),
        ] {
            let mut m = tuned();
            m.insert("vm.dirty_ratio", dirty);
            m.insert("vm.dirty_background_ratio", bg);
            assert_eq!(run(&m).failures, fails, "dirty={dirty} bg={bg}");
        }
    }

    #[test]
    fn test_verify_counts_every_failure_and_the_empty_kernel_fails_all() {
        let v = run(&BTreeMap::new());
        assert_eq!(v.failures, SCALAR_MINIMUMS.len() + 3);
        assert!(verdict(v.failures).starts_with("NOT APPLIED — 11 setting(s)"));
    }

    #[test]
    fn test_status_file_shape() {
        let v = run(&tuned());
        let text = status_file_text("2026-10-01T00:00:00Z", &v);
        let mut lines = text.lines();
        assert_eq!(
            lines.next(),
            Some(
                "2026-10-01T00:00:00Z: APPLIED — all kernel tuning verified for the 16-WebSocket feed"
            )
        );
        assert_eq!(lines.next(), Some(""));
        assert_eq!(
            lines.next(),
            Some("ok         net.core.rmem_max = 134217728")
        );
        assert!(text.ends_with('\n'));
        assert_eq!(row("BELOW", "x"), "BELOW      x");
        assert_eq!(row("UNREADABLE", "x"), "UNREADABLE x");
    }

    #[test]
    fn test_verify_this_host_reports_consistently() {
        let (failures, text) = verify_this_host();
        assert_eq!(
            failures == 0,
            text.contains("APPLIED —") && !text.contains("NOT APPLIED")
        );
    }

    #[test]
    fn test_chrony_summary() {
        let out = "Reference ID    : A9FEA97B (169.254.169.123)\nStratum         : 4\nSystem time     : 0.000001 seconds fast of NTP time\n";
        assert_eq!(
            summarise_chrony_tracking(out),
            "A9FEA97B (169.254.169.123); 0.000001 seconds fast of NTP time; "
        );
        assert_eq!(summarise_chrony_tracking(""), "");
    }

    #[test]
    fn test_default_route_iface() {
        let table = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
                     ens34\t0010A8C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0\n\
                     ens34\t00000000\t0110A8C0\t0003\t0\t0\t0\t00000000\t0\t0\t0\n";
        assert_eq!(default_route_iface(table), Some("ens34".to_string()));
        let down = "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\n\
                    eth0\t00000000\t0110A8C0\t0002\t0\t0\t0\t00000000\n";
        assert_eq!(default_route_iface(down), None, "a route that is not up");
        assert_eq!(default_route_iface(""), None);
        assert_eq!(default_route_iface("Iface\nshort line\n"), None);
    }

    #[test]
    fn test_nic_irqs() {
        let interrupts = "           CPU0       CPU1\n\
                          \x20 24:       100        0   PCI-MSIX-0000:00:05.0   0-edge      ens34-mgmnt-intr-pci:0000:00:05.0\n\
                          \x20 25:       200        0   PCI-MSIX-0000:00:05.0   1-edge      ens34-Tx-Rx-0\n\
                          \x20 26:         1        0   PCI-MSIX   2-edge   nvme0q0\n\
                          NMI:          0          0   Non-maskable interrupts\n";
        assert_eq!(nic_irqs(interrupts, "ens34"), vec![24, 25]);
        assert!(nic_irqs(interrupts, "eth9").is_empty());
    }

    #[test]
    fn test_cpu_guard_plans() {
        let (a, m) = plan_cpu_guard(2, "0", false);
        assert_eq!(a, DropIn::Write(cpu_dropin_text(2)));
        assert!(m.contains("WARNING only 2 core(s)"));
        assert!(
            cpu_dropin_text(2).ends_with("AllowedCPUs=\n"),
            "empty assignment resets the list"
        );
        assert_eq!(
            plan_cpu_guard(0, "0", true).0,
            DropIn::Write(cpu_dropin_text(0))
        );
        let (a, m) = plan_cpu_guard(4, "0", true);
        assert_eq!(a, DropIn::Remove);
        assert!(m.contains("removed the CPU-confinement escape hatch"));
        let (a, m) = plan_cpu_guard(TV_APP_CPUS_MIN_CORES, "1", false);
        assert_eq!(a, DropIn::Keep);
        assert!(m.contains("IRQs on cpu 1"));
    }

    #[test]
    fn test_memory_inputs_parse_like_the_script() {
        let unit =
            "[Service]\nMemoryHigh=15G\n# MemoryHigh=99G\nMemoryHigh=20G\nMemoryHigh=2048M\n";
        assert_eq!(
            unit_memory_high_g(unit),
            Some(20),
            "last G-valued line wins"
        );
        assert_eq!(unit_memory_high_g("MemoryHigh=infinity\n"), None);
        assert_eq!(
            meminfo_total_kib("MemTotal:       32212254 kB\nMemFree: 1 kB\n"),
            Some(32_212_254)
        );
        assert_eq!(meminfo_total_kib("MemFree: 1 kB\n"), None);
    }

    #[test]
    fn test_questdb_share_matches_the_deploy_formula() {
        assert_eq!(questdb_share_g(1), 1, "floor 1");
        assert_eq!(questdb_share_g(8), 3);
        assert_eq!(questdb_share_g(30), 12);
        assert_eq!(questdb_share_g(64), 12, "cap 12");
    }

    #[test]
    fn test_memory_guard_never_touches_a_host_that_can_reach_the_unit_value() {
        // r8g.xlarge: 32 GiB nominal reads as 30 GiB; the unit's 20G is reachable.
        let (a, m) = plan_memory_guard(Some(20), 30, false);
        assert_eq!(a, DropIn::Keep);
        assert!(m.contains("is reachable, no drop-in needed"));
        let (a, m) = plan_memory_guard(Some(20), 30, true);
        assert_eq!(a, DropIn::Remove);
        assert!(m.contains("removed the memory escape hatch"));
    }

    #[test]
    fn test_memory_guard_lowers_an_unreachable_ceiling() {
        // 15 GiB host: 15 - 6 (QuestDB) - 1 = 8.
        let (a, m) = plan_memory_guard(Some(20), 15, false);
        let DropIn::Write(text) = a else {
            panic!("expected a write, got {a:?}");
        };
        assert!(text.ends_with("MemoryHigh=8G\n"), "{text}");
        assert!(m.contains("MemoryHigh lowered 20G -> 8G"));
        // Equal is unreachable too (`-lt` in the script).
        assert!(matches!(
            plan_memory_guard(Some(20), 20, false).0,
            DropIn::Write(_)
        ));
        // A tiny host never goes below 1 G.
        let DropIn::Write(text) = plan_memory_guard(Some(20), 1, false).0 else {
            panic!("expected a write");
        };
        assert!(text.ends_with("MemoryHigh=1G\n"));
    }

    #[test]
    fn test_memory_guard_skips_on_unreadable_input() {
        let (a, m) = plan_memory_guard(None, 30, true);
        assert_eq!(a, DropIn::Keep);
        assert!(m.contains("SKIPPED (unit MemoryHigh='unreadable', MemTotal=30G)"));
        let (a, m) = plan_memory_guard(Some(20), 0, true);
        assert_eq!(a, DropIn::Keep);
        assert!(m.contains("MemoryHigh='20', MemTotal=0G"));
    }

    #[test]
    fn test_apply_dropin_writes_once_and_removes() {
        let dir = std::env::temp_dir().join(format!("tv-host-tuning-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("guard.conf");
        let p = path.to_string_lossy().into_owned();
        apply_dropin(&p, &DropIn::Write("a\n".to_string()));
        assert_eq!(std::fs::read_to_string(&path).ok().as_deref(), Some("a\n"));
        apply_dropin(&p, &DropIn::Write("a\n".to_string()));
        apply_dropin(&p, &DropIn::Keep);
        assert!(path.exists());
        apply_dropin(&p, &DropIn::Remove);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_process_helpers() {
        assert!(!command_succeeds("tv-no-such-program-d6b", &[]));
        assert!(!command_exists("tv-no-such-program-d6b", &[]));
        assert_eq!(command_stdout("tv-no-such-program-d6b", &[]), None);
        assert!(command_exists("true", &[]));
        assert!(command_succeeds("true", &[]));
        assert!(!command_succeeds("false", &[]));
        assert_eq!(command_stdout("false", &[]), None);
    }
}

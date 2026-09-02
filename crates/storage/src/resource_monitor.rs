//! BP-08 (audit 2026-07-01) — process-level resource early-warning monitor.
//!
//! Closes the RESOURCE-01/02/03 gaps from the 2026-07-01 permutation audit:
//! before this, RESOURCE-01/02/03 were RESERVED stubs — there was NO
//! file-descriptor-count monitor at all (a leaked WS / QuestDB socket could
//! exhaust `LimitNOFILE` with zero signal until `connect()` starts failing),
//! and no process-level RSS-vs-cgroup or spill-vs-free early alarm distinct
//! from the host-aggregate `mem_used_high` / `disk_used_high` CloudWatch
//! alarms. This monitor pages at 80% (fd / RSS) and at <20% free (spill) so
//! the operator acts BEFORE exhaustion.
//!
//! Design mirrors [`crate::oom_monitor`] + [`crate::disk_health_watcher`]:
//!   * PURE classifiers + parsers (unit-tested truth tables);
//!   * a thin fs-read probe wrapper;
//!   * a supervised poll loop (respawn on panic, DISK-WATCHER-01 pattern).
//!
//! Linux-only signals (`/proc`, cgroup). On a non-Linux host or when a source
//! is unreadable the probe returns a `ProbeFailed`/`None` value, the gauge is
//! left honest (no false-OK), and `tv_resource_monitor_probe_failed_total`
//! increments — never a panic, never a false page.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tracing::{debug, error, info, warn};

use crate::disk_health_watcher::{DiskHealthOutcome, classify_join_exit, probe_disk_free_bytes};

/// Cadence of the resource probe. 60s matches the OOM + disk watchers; slow-
/// moving resource pressure needs no tighter sampling and the cost is
/// negligible.
pub const RESOURCE_MONITOR_POLL_INTERVAL_SECS: u64 = 60;

/// Backoff between a monitor death and its respawn (mirrors DISK-WATCHER-01 /
/// OOM monitor): small so monitoring resumes quickly, non-zero so a monitor
/// that panics instantly on every start cannot busy-spin.
pub const RESOURCE_MONITOR_RESPAWN_BACKOFF_SECS: u64 = 5;

/// RESOURCE-01: page when open fd count reaches this fraction (percent) of
/// `LimitNOFILE`. 80% leaves headroom to act before `connect()` fails.
pub const FD_HIGH_PCT_THRESHOLD: u64 = 80;

/// RESOURCE-02: page when process VmRSS reaches this fraction (percent) of the
/// cgroup memory limit. 80% is well below the OOM-killer trip so PROC-01 is a
/// last resort, not the first signal.
pub const RSS_HIGH_PCT_THRESHOLD: u64 = 80;

/// RESOURCE-03: page when spill-dir free space drops BELOW this fraction
/// (percent) of total. 20% free is the early-warning floor.
pub const SPILL_FREE_LOW_PCT_THRESHOLD: u64 = 20;

/// Default `/proc/self/fd` directory (open file descriptors, one entry each).
pub const DEFAULT_PROC_SELF_FD_PATH: &str = "/proc/self/fd";
/// Default `/proc/self/limits` file (carries `Max open files`).
pub const DEFAULT_PROC_SELF_LIMITS_PATH: &str = "/proc/self/limits";
/// Default `/proc/self/status` file (carries `VmRSS`).
pub const DEFAULT_PROC_SELF_STATUS_PATH: &str = "/proc/self/status";
/// Default cgroup-v2 memory limit file.
pub const DEFAULT_CGROUP_V2_MEMORY_MAX_PATH: &str = "/sys/fs/cgroup/memory.max";

/// `/proc/meminfo` — the machine's own RAM. The RESOURCE-02 ceiling when no
/// cgroup limit is set, which is the production case: the systemd unit sets
/// no `MemoryMax=`.
pub const DEFAULT_PROC_MEMINFO_PATH: &str = "/proc/meminfo";

// ---------------------------------------------------------------------------
// PURE classifiers (truth tables)
// ---------------------------------------------------------------------------

/// PURE: is `used` at or above `pct_threshold`% of `limit`? Used for the fd
/// and RSS "high" checks. A `limit` of 0 (unknown / unlimited) returns `false`
/// (no denominator → no false page). Saturating arithmetic — never panics.
#[must_use]
pub fn is_at_or_above_pct(used: u64, limit: u64, pct_threshold: u64) -> bool {
    if limit == 0 {
        return false;
    }
    // used * 100 >= limit * pct_threshold  (avoid float; saturating so a huge
    // `used` cannot overflow-panic).
    used.saturating_mul(100) >= limit.saturating_mul(pct_threshold)
}

/// PURE: is free space BELOW `pct_threshold`% of `total`? Used for the spill
/// "low free" check. A `total` of 0 returns `false` (unknown → no page).
#[must_use]
pub fn is_free_below_pct(free: u64, total: u64, pct_threshold: u64) -> bool {
    if total == 0 {
        return false;
    }
    // free * 100 < total * pct_threshold
    free.saturating_mul(100) < total.saturating_mul(pct_threshold)
}

// ---------------------------------------------------------------------------
// PURE parsers
// ---------------------------------------------------------------------------

/// PURE parser of a `/proc/self/limits` file body for the soft `Max open
/// files` limit. The file is fixed-width columnar text:
///
/// ```text
/// Limit                     Soft Limit           Hard Limit           Units
/// Max open files            65536                65536                files
/// ```
///
/// Returns the SOFT limit (the effective `LimitNOFILE`) when the line is
/// present and parses; `None` otherwise. `unlimited` maps to `None` (no
/// denominator → no page).
#[must_use]
pub fn parse_max_open_files(limits_body: &str) -> Option<u64> {
    for line in limits_body.lines() {
        if let Some(rest) = line.strip_prefix("Max open files") {
            // The next whitespace-separated token is the soft limit.
            let soft = rest.split_whitespace().next()?;
            return soft.parse::<u64>().ok();
        }
    }
    None
}

/// Is resident memory at or above `pct` percent of the ceiling?
///
/// PURE and O(1). The one memory question every "stop before we die" guard in
/// this workspace asks, in ONE place, so two guards can never drift into two
/// different definitions of "close to the ceiling".
///
/// FAIL-OPEN by construction: an unknown RSS or an unknown ceiling returns
/// `false`, i.e. do not stop. That direction is deliberate — a guard that
/// cannot read the numbers must not silently halt WAL recovery, which is the
/// path that gets captured frames back into the database. A missing ceiling is
/// already reported by `RESOURCE-02`'s own ceiling_source field; this
/// predicate's job is the comparison, not the diagnosis.
///
/// The multiply is done in `u128` so `rss * 100` cannot overflow on a host
/// with more than ~184 exabytes of RSS — cheap, and it removes the need to
/// reason about the bound at all.
#[must_use]
pub const fn rss_at_or_above_fraction(
    rss_bytes: Option<u64>,
    ceiling_bytes: Option<u64>,
    pct: u64,
) -> bool {
    let (Some(rss), Some(ceiling)) = (rss_bytes, ceiling_bytes) else {
        return false;
    };
    if ceiling == 0 {
        return false;
    }
    (rss as u128) * 100 >= (ceiling as u128) * (pct as u128)
}

/// PURE parser of a `/proc/self/status` file body for `VmRSS` (in bytes). The
/// line is `VmRSS:\t   12345 kB` — value in KiB. Returns bytes.
#[must_use]
pub fn parse_vmrss_bytes(status_body: &str) -> Option<u64> {
    for line in status_body.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kib = rest.split_whitespace().next()?.parse::<u64>().ok()?;
            return Some(kib.saturating_mul(1024));
        }
    }
    None
}

/// PURE parser of a cgroup-v2 `memory.max` file body. The file is a single
/// token: either a byte count (`8589934592`) or the literal `max`
/// (unlimited). Returns `Some(bytes)` for a real limit, `None` for `max` /
/// unparseable (no denominator → RESOURCE-02 skipped).
#[must_use]
pub fn parse_cgroup_memory_max_bytes(memory_max_body: &str) -> Option<u64> {
    let token = memory_max_body.trim();
    if token == "max" || token.is_empty() {
        return None;
    }
    token.parse::<u64>().ok()
}

/// Parses `MemTotal:` out of `/proc/meminfo`, returning BYTES.
///
/// The line is `MemTotal:       32827080 kB` — a decimal count in KIBIBYTES,
/// whatever the unit says. A missing or malformed line yields `None` rather
/// than a guess.
#[must_use]
pub fn parse_meminfo_total_bytes(meminfo_body: &str) -> Option<u64> {
    meminfo_body
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|kib| kib.parse::<u64>().ok())
        .map(|kib| kib.saturating_mul(1024))
}

/// Where the ceiling RESOURCE-02 measures against came from.
///
/// Carrying the SOURCE rather than a bare `Option<u64>` is the point: an
/// operator reading a memory page needs to know whether 80% means 80% of a
/// container limit or 80% of the whole machine, and a caller needs to be able
/// to tell "no ceiling" apart from "ceiling of zero".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryCeiling {
    /// A real cgroup `memory.max` limit. The OOM killer enforces exactly this.
    Cgroup(u64),
    /// A cgroup `memory.high` throttle with NO hard `memory.max` (2026-09-02).
    /// The kernel does not kill at this line, it reclaims aggressively and
    /// stalls the allocator — which on the drain task is tick loss, not a
    /// page. So it is the ceiling RESOURCE-02 should warn against when a
    /// unit sets `MemoryHigh=` without `MemoryMax=`.
    CgroupHigh(u64),
    /// No cgroup limit — the MACHINE's own RAM is what binds.
    HostTotal(u64),
    /// Neither could be read. Nothing can be concluded about memory.
    Unknown,
}

impl MemoryCeiling {
    /// The ceiling in bytes, if one was resolved at all.
    #[must_use]
    pub const fn bytes(self) -> Option<u64> {
        match self {
            Self::Cgroup(b) | Self::CgroupHigh(b) | Self::HostTotal(b) => Some(b),
            Self::Unknown => None,
        }
    }

    /// A short label for the log line, so a page says which ceiling it means.
    #[must_use]
    pub const fn source(self) -> &'static str {
        match self {
            Self::Cgroup(_) => "cgroup",
            Self::CgroupHigh(_) => "cgroup_high",
            Self::HostTotal(_) => "host_total",
            Self::Unknown => "unknown",
        }
    }
}

/// PURE: resolve the ceiling from the three file BODIES, in priority order.
///
/// 1. `memory.max` numeric — the hard limit, the OOM killer's line.
/// 2. `memory.high` numeric — the throttle line, when there is no hard limit.
/// 3. `MemTotal` — the machine, when the cgroup binds nothing.
///
/// Each step is fail-soft: an absent or unparseable body (`None`, `max`,
/// garbage) falls through to the next rather than to `Unknown`, so a host
/// with a readable `/proc/meminfo` always resolves SOMETHING. `Unknown` is
/// only reached when all three are unusable.
#[must_use]
// TEST-EXEMPT: pinned by the three ceiling_order_* cases in ceiling_order_tests (max, high, MemTotal)
pub fn memory_ceiling_from_bodies(
    memory_max_body: Option<&str>,
    memory_high_body: Option<&str>,
    meminfo_body: Option<&str>,
) -> MemoryCeiling {
    if let Some(limit) = memory_max_body.and_then(parse_cgroup_memory_max_bytes) {
        return MemoryCeiling::Cgroup(limit);
    }
    // `memory.high` has the same single-token grammar as `memory.max`
    // (a byte count or the literal `max`), so the same parser applies.
    if let Some(high) = memory_high_body.and_then(parse_cgroup_memory_max_bytes) {
        return MemoryCeiling::CgroupHigh(high);
    }
    match meminfo_body.and_then(parse_meminfo_total_bytes) {
        Some(total) => MemoryCeiling::HostTotal(total),
        None => MemoryCeiling::Unknown,
    }
}

/// The `memory.high` file that sits beside a given `memory.max` path — same
/// cgroup directory, sibling file. Pure path arithmetic, no fs access.
#[must_use]
// TEST-EXEMPT: pinned by resolve_memory_ceiling_reads_memory_high_beside_memory_max
pub fn memory_high_path_beside(memory_max_path: &Path) -> PathBuf {
    memory_max_path.with_file_name("memory.high")
}

/// PURE: the `memory.max` path of OUR cgroup, from `/proc/self/cgroup`.
///
/// Delegates to [`crate::oom_monitor::memory_events_path_from_proc_cgroup`]
/// — the one resolver of "which cgroup directory is ours" — and swaps the
/// file name, so the two monitors can never disagree about which cgroup they
/// are reading. `None` means "use the fallback" (cgroup v1, or the root).
#[must_use]
// TEST-EXEMPT: pinned by cgroup_memory_max_path_resolves_the_unit_cgroup_not_the_root in ceiling_order_tests
pub fn cgroup_memory_max_path_from_proc_cgroup(proc_cgroup: &str) -> Option<PathBuf> {
    crate::oom_monitor::memory_events_path_from_proc_cgroup(proc_cgroup)
        .map(|events| events.with_file_name("memory.max"))
}

/// The `memory.max` path RESOURCE-02 (and the WAL byte budget) should read on
/// THIS host.
///
/// # The defect this closes (2026-09-02)
///
/// `platform_defaults` read [`DEFAULT_CGROUP_V2_MEMORY_MAX_PATH`] — the ROOT
/// cgroup's `memory.max` — which on a bare systemd host is the literal `max`
/// whether or not `tickvault.service` carries a `MemoryMax=`. So a limit set
/// on OUR unit was invisible to the monitor: it fell through to `MemTotal`
/// and measured the process against 32 GiB while the kernel would kill it at
/// the unit's line. `oom_monitor` already resolved the unit's cgroup from
/// `/proc/self/cgroup` for exactly this reason; this is the same resolution
/// applied to the sibling file.
///
/// Reads `/proc/self/cgroup` and delegates to
/// [`cgroup_memory_max_path_from_proc_cgroup`]; any failure falls back to
/// the root path, never to a panic.
// TEST-EXEMPT: thin fs-read wrapper over the fully-tested pure resolver; the
// fallback arm is `resolve_cgroup_memory_max_path_always_ends_in_memory_max`.
#[must_use]
pub fn resolve_cgroup_memory_max_path() -> PathBuf {
    std::fs::read_to_string(crate::oom_monitor::PROC_SELF_CGROUP_PATH)
        .ok()
        .and_then(|body| cgroup_memory_max_path_from_proc_cgroup(&body))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CGROUP_V2_MEMORY_MAX_PATH))
}

/// Resolves the memory ceiling, preferring a cgroup limit and falling back to
/// the machine's own RAM.
///
/// # The defect this closes (2026-09-01)
///
/// RESOURCE-02 is the process's memory early warning — the one signal that is
/// supposed to fire BEFORE the OOM killer. It was gated on a cgroup limit
/// alone:
///
/// ```text
/// if let Some(limit) = probe_cgroup_memory_max_bytes(..) && at_or_above(rss, limit) { page }
/// else { debug!("resource monitor: RSS ok") }
/// ```
///
/// `deploy/systemd/tickvault.service` sets `LimitNOFILE` and `LimitNPROC` and
/// **no `MemoryMax=`** — a tree-wide grep for `Memory(Max|Limit|High)` in that
/// unit returns nothing. So on the production box the probe returns `None`,
/// the condition is never evaluated, and the monitor takes the `else` branch:
/// it logs **"RSS ok"** having compared the process against nothing at all.
///
/// That is a false OK on the one alarm whose whole job is to fire before a
/// memory kill, on a host whose entire sizing argument is memory. The residual
/// signals are `mem_used_high` (host-level, 80%, three 300-second periods — so
/// fifteen minutes) and then the kernel, which selects the largest RSS: this
/// process. `oom_monitor.rs` then reports the kill about sixty seconds after
/// it happens, which is attribution, never prevention.
///
/// The machine's RAM is a genuine ceiling: with no cgroup limit the OOM killer
/// bounds the process at the host total, so measuring against it is measuring
/// against the real thing rather than skipping the check. On a container both
/// bind and the cgroup is smaller, so it stays preferred.
#[must_use]
pub fn resolve_memory_ceiling(
    cgroup_memory_max_path: &Path,
    proc_meminfo_path: &Path,
) -> MemoryCeiling {
    // 2026-09-02: `memory.high` (read beside `memory.max`) joins the order
    // between the hard limit and the machine total. The pure priority lives
    // in `memory_ceiling_from_bodies`; this is only the three fs reads.
    let max_body = std::fs::read_to_string(cgroup_memory_max_path).ok();
    let high_body = std::fs::read_to_string(memory_high_path_beside(cgroup_memory_max_path)).ok();
    let meminfo_body = std::fs::read_to_string(proc_meminfo_path).ok();
    memory_ceiling_from_bodies(
        max_body.as_deref(),
        high_body.as_deref(),
        meminfo_body.as_deref(),
    )
}

// ---------------------------------------------------------------------------
// Probes (thin fs-read wrappers)
// ---------------------------------------------------------------------------

/// Count the entries in `/proc/self/fd` = open file descriptors. Returns
/// `None` on a non-Linux host (dir missing / unreadable).
// The count logic is trivial and the None branch is exercised by
// `test_probe_fd_count_missing_dir_is_none`.
#[must_use]
// TEST-EXEMPT: thin fs-read wrapper; None branch covered by test_probe_fd_count_missing_dir_is_none.
pub fn probe_open_fd_count(fd_dir: &Path) -> Option<u64> {
    let entries = std::fs::read_dir(fd_dir).ok()?;
    Some(entries.filter(std::result::Result::is_ok).count() as u64)
}

/// Read + parse the soft `Max open files` limit. `None` on non-Linux / missing.
#[must_use]
// TEST-EXEMPT: thin fs-read + delegate to the fully-tested `parse_max_open_files`.
pub fn probe_max_open_files(limits_path: &Path) -> Option<u64> {
    let body = std::fs::read_to_string(limits_path).ok()?;
    parse_max_open_files(&body)
}

/// Read + parse VmRSS bytes. `None` on non-Linux / missing.
#[must_use]
// TEST-EXEMPT: thin fs-read + delegate to the fully-tested `parse_vmrss_bytes`.
pub fn probe_vmrss_bytes(status_path: &Path) -> Option<u64> {
    let body = std::fs::read_to_string(status_path).ok()?;
    parse_vmrss_bytes(&body)
}

/// Read + parse the cgroup memory limit. `None` on `max` / non-Linux / missing.
#[must_use]
// TEST-EXEMPT: thin fs-read + delegate to the fully-tested `parse_cgroup_memory_max_bytes`.
pub fn probe_cgroup_memory_max_bytes(memory_max_path: &Path) -> Option<u64> {
    let body = std::fs::read_to_string(memory_max_path).ok()?;
    parse_cgroup_memory_max_bytes(&body)
}

// ---------------------------------------------------------------------------
// Paths bundle
// ---------------------------------------------------------------------------

/// The set of source paths the monitor reads. Bundled so the production spawn
/// takes the platform defaults and tests can point at fixtures.
#[derive(Debug, Clone)]
pub struct ResourceMonitorPaths {
    /// `/proc/self/fd` — open fd entries.
    pub proc_self_fd: PathBuf,
    /// `/proc/self/limits` — `Max open files` soft limit.
    pub proc_self_limits: PathBuf,
    /// `/proc/self/status` — `VmRSS`.
    pub proc_self_status: PathBuf,
    /// cgroup-v2 `memory.max` — process memory ceiling.
    pub cgroup_memory_max: PathBuf,
    /// `/proc/meminfo` — the machine's own RAM, used as the RESOURCE-02
    /// ceiling when no cgroup limit exists. Without this the check silently
    /// does nothing on a bare systemd host; see [`resolve_memory_ceiling`].
    pub proc_meminfo: PathBuf,
    /// spill directory (free-percent probe reuses the disk-health `df` probe).
    pub spill_dir: PathBuf,
}

impl ResourceMonitorPaths {
    /// Production defaults (Linux `/proc` + cgroup-v2 + `data/spill`).
    #[must_use]
    pub fn platform_defaults(spill_dir: PathBuf) -> Self {
        Self {
            proc_self_fd: PathBuf::from(DEFAULT_PROC_SELF_FD_PATH),
            proc_self_limits: PathBuf::from(DEFAULT_PROC_SELF_LIMITS_PATH),
            proc_self_status: PathBuf::from(DEFAULT_PROC_SELF_STATUS_PATH),
            // OUR unit's cgroup, not the root's (2026-09-02) — see
            // `resolve_cgroup_memory_max_path` for the limit this used to miss.
            cgroup_memory_max: resolve_cgroup_memory_max_path(),
            proc_meminfo: PathBuf::from(DEFAULT_PROC_MEMINFO_PATH),
            spill_dir,
        }
    }
}

// ---------------------------------------------------------------------------
// Background task + supervisor
// ---------------------------------------------------------------------------

/// Spawn the background resource monitor. Idempotent — call once at boot. The
/// returned `JoinHandle` can be aborted on shutdown.
///
/// Every 60s it samples fd count, VmRSS, and spill free-space, updates the
/// `tv_open_fds` / `tv_process_rss_bytes` / `tv_spill_free_pct` gauges, and
/// emits RESOURCE-01/02/03 `error!` when a threshold is crossed. On a non-
/// Linux host each probe fails softly (`tv_resource_monitor_probe_failed_total`,
/// no page, no panic).
// The pure classifiers + parsers are fully unit-tested; this wrapper is a
// probe loop that needs an integration harness to test usefully.
#[must_use]
// TEST-EXEMPT: tokio task spawn — pure classifiers/parsers unit-tested; loop needs an integration harness.
pub fn spawn_resource_monitor(paths: ResourceMonitorPaths) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let m_fds = metrics::gauge!("tv_open_fds");
        let m_rss = metrics::gauge!("tv_process_rss_bytes");
        let m_spill_free_pct = metrics::gauge!("tv_spill_free_pct");
        let m_probe_failed = metrics::counter!("tv_resource_monitor_probe_failed_total");
        // Consecutive cycles with no readable memory ceiling. Used to throttle
        // the UNCHECKED warning to powers of two: the condition is persistent
        // by nature (an unreadable /proc/meminfo does not heal itself), so at a
        // 60s cadence an unthrottled line would be ~1,440 identical warnings a
        // day — which is how a real signal gets filtered out and ignored.
        // Powers of two give ~11 lines a day instead, and the FIRST one is
        // immediate.
        let mut ceiling_unreadable_streak: u64 = 0;

        info!(
            interval_secs = RESOURCE_MONITOR_POLL_INTERVAL_SECS,
            fd_pct = FD_HIGH_PCT_THRESHOLD,
            rss_pct = RSS_HIGH_PCT_THRESHOLD,
            spill_free_pct = SPILL_FREE_LOW_PCT_THRESHOLD,
            "resource monitor started (RESOURCE-01/02/03)"
        );

        let mut ticker =
            tokio::time::interval(Duration::from_secs(RESOURCE_MONITOR_POLL_INTERVAL_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;

            // RESOURCE-01 — open fd count vs LimitNOFILE.
            match (
                probe_open_fd_count(&paths.proc_self_fd),
                probe_max_open_files(&paths.proc_self_limits),
            ) {
                (Some(fds), limit_opt) => {
                    m_fds.set(fds as f64);
                    if let Some(limit) = limit_opt
                        && is_at_or_above_pct(fds, limit, FD_HIGH_PCT_THRESHOLD)
                    {
                        error!(
                            code = tickvault_common::error_code::ErrorCode::Resource01FdCountHigh
                                .code_str(),
                            open_fds = fds,
                            limit,
                            pct_threshold = FD_HIGH_PCT_THRESHOLD,
                            "RESOURCE-01: open file-descriptor count at/above \
                             {FD_HIGH_PCT_THRESHOLD}% of LimitNOFILE — inspect \
                             /proc/self/fd for a socket leak before connect() fails"
                        );
                    } else {
                        debug!(open_fds = fds, "resource monitor: fd count ok");
                    }
                }
                (None, _) => {
                    m_probe_failed.increment(1);
                    // A blind probe must not read like a healthy one. Until
                    // 2026-08-12 these three arms incremented a counter that
                    // was in no EMF selector and had no log, so an fd probe
                    // that could not read /proc looked exactly like an fd
                    // count comfortably under budget -- the monitor reported
                    // nothing and the operator concluded nothing was wrong.
                    // Sibling arms in this same match already log; only the
                    // failure arms were mute.
                    warn!(
                        "resource monitor: fd probe FAILED -- open-fd count is \
                         UNKNOWN this cycle, not known-good. A socket leak \
                         would be invisible until connect() starts failing."
                    );
                }
            }

            // RESOURCE-02 — VmRSS vs cgroup memory.max.
            match probe_vmrss_bytes(&paths.proc_self_status) {
                Some(rss) => {
                    m_rss.set(rss as f64);
                    // Ceiling, not "cgroup limit": with no cgroup limit the
                    // MACHINE's RAM is what the OOM killer enforces, so that
                    // is what RSS is measured against. Gating on the cgroup
                    // alone made this whole arm inert on the production box
                    // and then logged "RSS ok" — see `resolve_memory_ceiling`.
                    let ceiling =
                        resolve_memory_ceiling(&paths.cgroup_memory_max, &paths.proc_meminfo);
                    match ceiling.bytes() {
                        Some(limit) if is_at_or_above_pct(rss, limit, RSS_HIGH_PCT_THRESHOLD) => {
                            error!(
                                code =
                                    tickvault_common::error_code::ErrorCode::Resource02ResidentMemoryHigh
                                        .code_str(),
                                rss_bytes = rss,
                                limit_bytes = limit,
                                ceiling_source = ceiling.source(),
                                pct_threshold = RSS_HIGH_PCT_THRESHOLD,
                                "RESOURCE-02: process resident memory at/above \
                                 {RSS_HIGH_PCT_THRESHOLD}% of the {} ceiling — the OOM \
                                 killer (PROC-01) is imminent; right-size the workload",
                                ceiling.source()
                            );
                        }
                        Some(_) => {
                            ceiling_unreadable_streak = 0;
                            debug!(rss_bytes = rss, ceiling = ceiling.source(), "RSS ok");
                        }
                        // NOT "RSS ok". Nothing was compared, so nothing is
                        // known — saying otherwise is the false-OK this arm
                        // was just repaired for. Counted like every other
                        // failed probe in this monitor.
                        None => {
                            m_probe_failed.increment(1);
                            ceiling_unreadable_streak = ceiling_unreadable_streak.saturating_add(1);
                            // WARN, not DEBUG (2026-09-01, adversarial review).
                            //
                            // The arm below — an unreadable RSS — already warns,
                            // and this is the same class: nothing was compared,
                            // so the RSS alarm is not merely quiet, it is
                            // STRUCTURALLY unable to fire. At `debug!` that state
                            // was honest in the code and invisible in production,
                            // which is most of the way back to the false-OK this
                            // arm was repaired for. The counter it increments has
                            // no alarm either, so the log line is the only
                            // operator-reachable signal that exists.
                            if ceiling_unreadable_streak.is_power_of_two() {
                                warn!(
                                    rss_bytes = rss,
                                    consecutive_cycles = ceiling_unreadable_streak,
                                    "resource monitor: no memory ceiling readable — RSS \
                                     recorded but UNCHECKED, so the RSS threshold cannot \
                                     fire at all (throttled to powers of two)"
                                );
                            }
                        }
                    }
                }
                None => {
                    m_probe_failed.increment(1);
                    // Same class as the fd arm above: an unreadable RSS is
                    // UNKNOWN, not fine. Silence here would hide a memory
                    // climb on a host whose whole sizing argument is memory.
                    warn!(
                        "resource monitor: RSS probe FAILED -- process memory is \
                         UNKNOWN this cycle, not known-good."
                    );
                }
            }

            // RESOURCE-03 — spill-dir free percent (reuse the disk-health probe).
            match probe_disk_free_bytes(&paths.spill_dir) {
                DiskHealthOutcome::Ok {
                    free_bytes,
                    total_bytes,
                } => {
                    let free_pct = if total_bytes == 0 {
                        0.0
                    } else {
                        (free_bytes as f64 / total_bytes as f64) * 100.0
                    };
                    m_spill_free_pct.set(free_pct);
                    if is_free_below_pct(free_bytes, total_bytes, SPILL_FREE_LOW_PCT_THRESHOLD) {
                        error!(
                            code = tickvault_common::error_code::ErrorCode::Resource03SpillFreeLow
                                .code_str(),
                            free_bytes,
                            total_bytes,
                            free_pct,
                            pct_threshold = SPILL_FREE_LOW_PCT_THRESHOLD,
                            path = %paths.spill_dir.display(),
                            "RESOURCE-03: spill-dir free space below \
                             {SPILL_FREE_LOW_PCT_THRESHOLD}% — the zero-loss chain is at \
                             risk if QuestDB stays down; free disk / restore the drain"
                        );
                    } else {
                        debug!(free_pct, "resource monitor: spill free ok");
                    }
                }
                DiskHealthOutcome::ProbeFailed { .. } => {
                    m_probe_failed.increment(1);
                    // Same class again. A spill directory whose free space
                    // cannot be read is the one place silence is least
                    // affordable: the spill tier IS the durability floor when
                    // QuestDB is unreachable.
                    warn!(
                        "resource monitor: spill-dir free-space probe FAILED -- \
                         spill headroom is UNKNOWN this cycle, not known-good."
                    );
                }
            }
        }
    })
}

/// Supervise the resource monitor. [`spawn_resource_monitor`] runs an infinite
/// probe loop, so its `JoinHandle` resolves ONLY on a fatal event (panic or
/// external cancel). This supervisor mirrors the WS-GAP-05 pool supervisor +
/// the DISK-WATCHER-01 + OOM-monitor supervisors: on every monitor death it
/// logs, increments `tv_resource_monitor_respawn_total{reason}`, then respawns
/// after [`RESOURCE_MONITOR_RESPAWN_BACKOFF_SECS`] so resource monitoring can
/// never vanish silently. The returned `JoinHandle` is itself an infinite loop;
/// callers bind it to a `_`-prefixed name. The supervisor body has no panic
/// path of its own (pure-function classification, no `unwrap`/`expect`).
// O(1) EXEMPT: cold-path supervisor — one task per session, fires only on monitor death.
#[must_use]
pub fn spawn_supervised_resource_monitor(
    paths: ResourceMonitorPaths,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let handle = spawn_resource_monitor(paths.clone());
            let join_result = handle.await;
            let reason = classify_join_exit(&join_result);
            warn!(
                reason,
                backoff_secs = RESOURCE_MONITOR_RESPAWN_BACKOFF_SECS,
                "resource monitor exited — respawning so RESOURCE-01/02/03 \
                 monitoring continues"
            );
            metrics::counter!("tv_resource_monitor_respawn_total", "reason" => reason).increment(1);
            tokio::time::sleep(Duration::from_secs(RESOURCE_MONITOR_RESPAWN_BACKOFF_SECS)).await;
        }
    })
}

#[cfg(test)]
mod tests {

    // ---------------------------------------------------------------------
    // rss_at_or_above_fraction — the ONE definition of "close to the ceiling"
    // ---------------------------------------------------------------------

    /// The boundary is inclusive, and the arithmetic must not floor.
    ///
    /// Integer DIVISION would be the obvious way to write this and is wrong:
    /// `ceiling / 100 * pct` floors twice, and at a small ceiling can floor
    /// the threshold to zero — which would make the guard fire instantly on
    /// every host with a tiny cgroup, halting WAL recovery permanently. The
    /// multiply-then-compare form has no such edge.
    #[test]
    fn rss_fraction_is_inclusive_at_the_boundary_and_does_not_floor() {
        // Exactly on the line counts as over it: the guard exists to stop
        // BEFORE the ceiling, so "at" must not be treated as "under".
        assert!(rss_at_or_above_fraction(Some(60), Some(100), 60));
        assert!(!rss_at_or_above_fraction(Some(59), Some(100), 60));
        assert!(rss_at_or_above_fraction(Some(61), Some(100), 60));

        // A ceiling small enough that `ceiling / 100` floors to 0. A divide-
        // based implementation says "over" for ANY rss here; this must not.
        assert!(!rss_at_or_above_fraction(Some(1), Some(50), 60));
        assert!(rss_at_or_above_fraction(Some(30), Some(50), 60));
    }

    /// FAIL-OPEN on anything unmeasurable — the deliberate direction.
    ///
    /// Both callers use this to decide whether to STOP draining a backlog.
    /// Failing closed on an unreadable probe would turn a host we cannot
    /// measure into a host that never recovers its WAL, which is strictly
    /// worse than the memory risk the guard exists to bound.
    #[test]
    fn rss_fraction_fails_open_when_anything_is_unknown() {
        assert!(!rss_at_or_above_fraction(None, Some(100), 60));
        assert!(!rss_at_or_above_fraction(Some(99), None, 60));
        assert!(!rss_at_or_above_fraction(None, None, 60));
        // A zero ceiling is a nonsense reading, not a ceiling of zero bytes:
        // treating it literally would make every comparison true forever.
        assert!(!rss_at_or_above_fraction(Some(1), Some(0), 60));
    }

    /// The real numbers from the 2026-09-02 boot that produced this guard.
    ///
    /// Pinned as a fixture so the constant and the arithmetic are checked
    /// against the incident rather than against a round number: RSS 13.09 GiB
    /// against the unit's 15.0 GiB `MemoryHigh`, which is 87.2% — over the
    /// 60% stand-down line and over the 80% `RESOURCE-02` page line, which is
    /// exactly the ordering the two thresholds are meant to have.
    #[test]
    fn rss_fraction_matches_the_live_boot_that_produced_the_guard() {
        let rss = Some(14_050_361_344u64);
        let ceiling = Some(16_106_127_360u64);
        assert!(
            rss_at_or_above_fraction(rss, ceiling, 60),
            "stand-down line"
        );
        assert!(
            rss_at_or_above_fraction(rss, ceiling, 80),
            "RESOURCE-02 line"
        );
        assert!(!rss_at_or_above_fraction(rss, ceiling, 90));

        // u128 internally: a u64-only `rss * 100` would overflow and wrap for
        // a large RSS, silently reporting "under the line" at the worst moment.
        assert!(rss_at_or_above_fraction(
            Some(u64::MAX),
            Some(u64::MAX),
            100
        ));
    }

    use super::*;

    /// A unique scratch directory per test, the same shape the sibling
    /// persistence tests use. Deliberately not a new dev-dependency: adding a
    /// crate needs operator approval (CLAUDE.md CARGO), and the ceiling tests
    /// need nothing a nanosecond-tagged path cannot give them.
    /// A probe directory that is unique WITHOUT depending on the clock.
    ///
    /// Adversarial review (2026-09-01) found two problems with the previous
    /// nanos-only version, and the second is the one that bites:
    ///
    ///   1. `map_or(0, ..)` collapsed the tag to a CONSTANT `0` on any
    ///      `SystemTime` error, so every caller would share one directory.
    ///   2. Two call sites already passed the same tag (`"ceiling"`), so on
    ///      that collapse they would race writing DIFFERENT contents to the
    ///      same `memory.max` and the assertion outcome would depend on
    ///      thread scheduling.
    ///
    /// A monotonic per-process counter plus the pid makes uniqueness a
    /// property of the program rather than of the clock, so neither failure
    /// mode survives even if `SystemTime` returns an error every time.
    fn unique_probe_dir(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let pid = std::process::id();
        std::env::temp_dir().join(format!("tv-resource-monitor-{tag}-{pid}-{seq}"))
    }

    // -- thresholds sanity --

    #[test]
    fn test_thresholds_are_sane() {
        assert!((50..=95).contains(&FD_HIGH_PCT_THRESHOLD));
        assert!((50..=95).contains(&RSS_HIGH_PCT_THRESHOLD));
        assert!((5..=40).contains(&SPILL_FREE_LOW_PCT_THRESHOLD));
    }

    #[test]
    fn test_poll_interval_is_reasonable() {
        assert!(RESOURCE_MONITOR_POLL_INTERVAL_SECS >= 30);
        assert!(RESOURCE_MONITOR_POLL_INTERVAL_SECS <= 300);
    }

    #[test]
    fn test_respawn_backoff_is_small_but_nonzero() {
        assert!(RESOURCE_MONITOR_RESPAWN_BACKOFF_SECS >= 1);
        assert!(RESOURCE_MONITOR_RESPAWN_BACKOFF_SECS <= 30);
    }

    // -- is_at_or_above_pct — truth table --

    #[test]
    fn test_is_at_or_above_pct_below_threshold_is_false() {
        // 79 of 100 at 80% threshold → false.
        assert!(!is_at_or_above_pct(79, 100, 80));
    }

    #[test]
    fn test_is_at_or_above_pct_at_threshold_is_true() {
        // Exactly 80 of 100 at 80% → true (>=).
        assert!(is_at_or_above_pct(80, 100, 80));
    }

    #[test]
    fn test_is_at_or_above_pct_above_threshold_is_true() {
        assert!(is_at_or_above_pct(95, 100, 80));
        assert!(is_at_or_above_pct(100, 100, 80));
    }

    #[test]
    fn test_is_at_or_above_pct_zero_limit_is_false() {
        // Unknown/unlimited limit → no denominator → never page.
        assert!(!is_at_or_above_pct(1_000_000, 0, 80));
    }

    #[test]
    fn test_is_at_or_above_pct_realistic_fd() {
        // 52429 of 65536 = 80.0% → true; 52428 = 79.99% → false.
        assert!(is_at_or_above_pct(52_429, 65_536, 80));
        assert!(!is_at_or_above_pct(52_428, 65_536, 80));
    }

    #[test]
    fn test_is_at_or_above_pct_no_overflow_on_huge_used() {
        // Saturating: a huge `used` must not overflow-panic.
        assert!(is_at_or_above_pct(u64::MAX, 100, 80));
    }

    // -- is_free_below_pct — truth table --

    #[test]
    fn test_is_free_below_pct_above_floor_is_false() {
        // 21% free at 20% floor → NOT below → false.
        assert!(!is_free_below_pct(21, 100, 20));
    }

    #[test]
    fn test_is_free_below_pct_at_floor_is_false() {
        // Exactly 20% free → NOT strictly below → false.
        assert!(!is_free_below_pct(20, 100, 20));
    }

    #[test]
    fn test_is_free_below_pct_below_floor_is_true() {
        // 19% free → below 20% → true.
        assert!(is_free_below_pct(19, 100, 20));
        assert!(is_free_below_pct(0, 100, 20));
    }

    #[test]
    fn test_is_free_below_pct_zero_total_is_false() {
        assert!(!is_free_below_pct(0, 0, 20));
    }

    // -- parse_max_open_files --

    #[test]
    fn test_parse_max_open_files_happy_path() {
        let body = "Limit                     Soft Limit           Hard Limit           Units\n\
                    Max open files            65536                65536                files\n";
        assert_eq!(parse_max_open_files(body), Some(65536));
    }

    #[test]
    fn test_parse_max_open_files_missing_line_is_none() {
        assert_eq!(
            parse_max_open_files("Max locked memory  0  0  bytes\n"),
            None
        );
    }

    #[test]
    fn test_parse_max_open_files_unlimited_is_none() {
        // A literal `unlimited` soft limit does not parse as u64 → None.
        let body = "Max open files            unlimited            unlimited            files\n";
        assert_eq!(parse_max_open_files(body), None);
    }

    // -- parse_vmrss_bytes --

    #[test]
    fn test_parse_vmrss_bytes_happy_path() {
        // VmRSS: 12345 kB → 12345 * 1024 bytes.
        let body = "VmPeak:\t  100000 kB\nVmRSS:\t   12345 kB\nVmData:\t  5000 kB\n";
        assert_eq!(parse_vmrss_bytes(body), Some(12_345 * 1024));
    }

    #[test]
    fn test_parse_vmrss_bytes_missing_is_none() {
        assert_eq!(parse_vmrss_bytes("VmPeak:\t 100 kB\n"), None);
    }

    // -- parse_cgroup_memory_max_bytes --

    #[test]
    fn test_parse_cgroup_memory_max_bytes_numeric() {
        assert_eq!(
            parse_cgroup_memory_max_bytes("8589934592\n"),
            Some(8_589_934_592)
        );
    }

    #[test]
    fn test_parse_cgroup_memory_max_bytes_unlimited_is_none() {
        // `max` = unlimited → no denominator → RESOURCE-02 skipped.
        assert_eq!(parse_cgroup_memory_max_bytes("max\n"), None);
        assert_eq!(parse_cgroup_memory_max_bytes("  max  "), None);
    }

    #[test]
    fn test_parse_cgroup_memory_max_bytes_garbage_is_none() {
        assert_eq!(parse_cgroup_memory_max_bytes("not-a-number\n"), None);
        assert_eq!(parse_cgroup_memory_max_bytes(""), None);
    }

    // -- memory ceiling resolution (RESOURCE-02) --

    #[test]
    fn test_parse_meminfo_total_bytes_reads_kib_and_returns_bytes() {
        // Real shape, including the lines either side that must be skipped.
        let body = "MemFree:         1234 kB\nMemTotal:       32827080 kB\nBuffers: 1 kB\n";
        assert_eq!(
            parse_meminfo_total_bytes(body),
            Some(32_827_080 * 1024),
            "MemTotal is a KIBIBYTE count and must be widened to bytes"
        );
        assert_eq!(parse_meminfo_total_bytes("MemFree: 12 kB\n"), None);
        assert_eq!(parse_meminfo_total_bytes("MemTotal: garbage\n"), None);
        assert_eq!(parse_meminfo_total_bytes(""), None);
    }

    /// The exact production shape, and the reason this fix exists.
    ///
    /// `deploy/systemd/tickvault.service` sets no `MemoryMax=`, so the cgroup
    /// file reads `max` (or is absent). Before this change that made the whole
    /// RESOURCE-02 arm inert and it then logged "RSS ok" — a false OK on the
    /// one alarm meant to fire before an OOM kill.
    #[test]
    fn resolve_memory_ceiling_falls_back_to_the_machine_when_no_cgroup_limit() {
        let dir = unique_probe_dir("ceiling");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let cgroup = dir.join("memory.max");
        let meminfo = dir.join("meminfo");
        std::fs::write(&cgroup, "max\n").expect("write cgroup");
        std::fs::write(&meminfo, "MemTotal:       32827080 kB\n").expect("write meminfo");

        let ceiling = resolve_memory_ceiling(&cgroup, &meminfo);
        assert_eq!(
            ceiling,
            MemoryCeiling::HostTotal(32_827_080 * 1024),
            "with no cgroup limit the machine's own RAM is the real ceiling — \
             the OOM killer enforces it, so RESOURCE-02 must measure against it"
        );
        assert!(
            ceiling.bytes().is_some(),
            "a resolved ceiling is what makes the check run at all"
        );
        assert_eq!(ceiling.source(), "host_total");
    }

    #[test]
    fn resolve_memory_ceiling_prefers_the_cgroup_because_it_is_the_smaller_bound() {
        let dir = unique_probe_dir("ceiling-cgroup-max");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let cgroup = dir.join("memory.max");
        let meminfo = dir.join("meminfo");
        // Container limit far below the machine total.
        std::fs::write(&cgroup, "2147483648\n").expect("write cgroup");
        std::fs::write(&meminfo, "MemTotal:       32827080 kB\n").expect("write meminfo");
        assert_eq!(
            resolve_memory_ceiling(&cgroup, &meminfo),
            MemoryCeiling::Cgroup(2_147_483_648),
            "both bind and the cgroup is smaller; it is what the OOM killer uses"
        );
    }

    #[test]
    fn resolve_memory_ceiling_unknown_carries_no_number_and_never_reads_as_ok() {
        let dir = unique_probe_dir("absent");
        let ceiling =
            resolve_memory_ceiling(&dir.join("absent-cgroup"), &dir.join("absent-meminfo"));
        assert_eq!(ceiling, MemoryCeiling::Unknown);
        assert_eq!(
            ceiling.bytes(),
            None,
            "Unknown must carry no number — a caller that unwrapped a default \
             here would compare RSS against a fabricated ceiling"
        );
        assert_eq!(ceiling.source(), "unknown");
    }

    /// The threshold still bites once a ceiling exists — otherwise the fix
    /// would resolve a ceiling and still never page.
    #[test]
    fn the_host_total_ceiling_actually_triggers_the_threshold() {
        let host = 32_827_080_u64 * 1024;
        let ceiling = MemoryCeiling::HostTotal(host);
        let limit = ceiling.bytes().expect("host ceiling resolves");
        // 85% of the machine — over the 80% rail.
        let rss_high = host / 100 * 85;
        assert!(
            is_at_or_above_pct(rss_high, limit, RSS_HIGH_PCT_THRESHOLD),
            "RESOURCE-02 must fire at 85% of the machine when no cgroup exists"
        );
        // Half the machine — comfortably under.
        assert!(
            !is_at_or_above_pct(host / 2, limit, RSS_HIGH_PCT_THRESHOLD),
            "and must stay quiet at 50%, or it pages every day"
        );
    }

    // -- probe fd count (None branch) --

    #[test]
    fn test_probe_fd_count_missing_dir_is_none() {
        let out = probe_open_fd_count(std::path::Path::new(
            "/nonexistent-resource-monitor-fd-dir-xyz",
        ));
        assert_eq!(out, None);
    }

    #[test]
    fn test_platform_defaults_bundle_paths() {
        let p = ResourceMonitorPaths::platform_defaults(PathBuf::from("data/spill"));
        assert_eq!(p.proc_self_fd, PathBuf::from(DEFAULT_PROC_SELF_FD_PATH));
        assert_eq!(p.spill_dir, PathBuf::from("data/spill"));
    }

    // -- supervisor keeps running --

    #[tokio::test]
    async fn test_spawn_supervised_resource_monitor_keeps_running() {
        let handle = spawn_supervised_resource_monitor(ResourceMonitorPaths::platform_defaults(
            PathBuf::from("data/resource-monitor-test"),
        ));
        tokio::task::yield_now().await;
        assert!(
            !handle.is_finished(),
            "supervisor must keep running, not exit after spawning the monitor"
        );
        handle.abort();
    }
}

#[cfg(test)]
mod ceiling_order_tests {
    use super::{
        DEFAULT_CGROUP_V2_MEMORY_MAX_PATH, MemoryCeiling, cgroup_memory_max_path_from_proc_cgroup,
        memory_ceiling_from_bodies, memory_high_path_beside, resolve_cgroup_memory_max_path,
        resolve_memory_ceiling,
    };
    use std::path::{Path, PathBuf};

    const MEMINFO: &str = "MemTotal:       32827080 kB\nMemFree:        1234 kB\n";

    #[test]
    fn a_hard_max_wins_over_high_and_the_machine() {
        assert_eq!(
            memory_ceiling_from_bodies(Some("2147483648\n"), Some("1073741824\n"), Some(MEMINFO)),
            MemoryCeiling::Cgroup(2_147_483_648)
        );
    }

    #[test]
    fn memory_high_is_the_ceiling_when_max_is_unlimited() {
        let c = memory_ceiling_from_bodies(Some("max\n"), Some("1073741824\n"), Some(MEMINFO));
        assert_eq!(c, MemoryCeiling::CgroupHigh(1_073_741_824));
        assert_eq!(c.source(), "cgroup_high");
        assert_eq!(c.bytes(), Some(1_073_741_824));
    }

    #[test]
    fn the_machine_total_is_the_ceiling_when_both_cgroup_files_are_unusable() {
        // `max`, absent, and garbage each fall through — never to Unknown
        // while MemTotal is readable.
        for (max, high) in [
            (Some("max\n"), Some("max\n")),
            (None, None),
            (Some("not-a-number"), Some("")),
        ] {
            assert_eq!(
                memory_ceiling_from_bodies(max, high, Some(MEMINFO)),
                MemoryCeiling::HostTotal(32_827_080 * 1024),
                "max={max:?} high={high:?}"
            );
        }
        assert_eq!(
            memory_ceiling_from_bodies(None, None, None),
            MemoryCeiling::Unknown
        );
    }

    #[test]
    fn resolve_reads_memory_high_beside_memory_max() {
        let dir =
            std::env::temp_dir().join(format!("tv-resource-monitor-high-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("mkdir: {e}"));
        let max = dir.join("memory.max");
        let meminfo = dir.join("meminfo");
        std::fs::write(&max, "max\n").unwrap_or_else(|e| panic!("write: {e}"));
        std::fs::write(memory_high_path_beside(&max), "536870912\n")
            .unwrap_or_else(|e| panic!("write: {e}"));
        std::fs::write(&meminfo, MEMINFO).unwrap_or_else(|e| panic!("write: {e}"));
        assert_eq!(
            resolve_memory_ceiling(&max, &meminfo),
            MemoryCeiling::CgroupHigh(536_870_912),
            "the SIBLING memory.high in the same cgroup dir must be read"
        );
        assert_eq!(
            memory_high_path_beside(Path::new(
                "/sys/fs/cgroup/system.slice/x.service/memory.max"
            )),
            PathBuf::from("/sys/fs/cgroup/system.slice/x.service/memory.high")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_unit_cgroup_is_resolved_not_the_root() {
        // The production shape: systemd puts the service in its own cgroup.
        // Reading the ROOT memory.max here would miss a MemoryMax= on the unit.
        assert_eq!(
            cgroup_memory_max_path_from_proc_cgroup("0::/system.slice/tickvault.service\n"),
            Some(PathBuf::from(
                "/sys/fs/cgroup/system.slice/tickvault.service/memory.max"
            ))
        );
        // Root and cgroup-v1 shapes mean "use the fallback".
        assert_eq!(cgroup_memory_max_path_from_proc_cgroup("0::/\n"), None);
        assert_eq!(
            cgroup_memory_max_path_from_proc_cgroup("12:memory:/user.slice\n"),
            None
        );
    }

    #[test]
    fn resolve_cgroup_memory_max_path_always_ends_in_memory_max() {
        // Whatever this host's /proc/self/cgroup says (a container, a bare
        // box, macOS), the resolved path names the right FILE, and the
        // fallback is the documented root constant.
        let p = resolve_cgroup_memory_max_path();
        assert_eq!(p.file_name().and_then(|f| f.to_str()), Some("memory.max"));
        assert!(
            p.starts_with("/sys/fs/cgroup"),
            "{} must live under the cgroup-v2 mount",
            p.display()
        );
        assert_eq!(
            Path::new(DEFAULT_CGROUP_V2_MEMORY_MAX_PATH)
                .file_name()
                .and_then(|f| f.to_str()),
            Some("memory.max")
        );
    }
}

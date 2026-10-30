//! Boot-time verification of the kernel limits the market-data feed depends on.
//!
//! ## Why this exists
//!
//! `deploy/aws/sysctl/99-tickvault-net.conf` raises the socket receive buffer
//! from the stock ~212 KB to 128 MB, because that single setting is what decides
//! whether a consumer stall costs us milliseconds or costs us ticks. When the
//! kernel receive queue fills, TCP shrinks the advertised window and the SERVER
//! discards updates — nothing downstream can recover data that never arrived.
//!
//! But user-data applies that file exactly ONCE, at first boot. Everything after
//! that is assumption:
//!
//! - the instance is recreated from an AMI that predates the file
//! - someone edits `/etc/sysctl.d` on the box
//! - a `sysctl --system` fails partway and the file never lands
//! - the app runs somewhere user-data never ran at all (a container, a laptop)
//!
//! In every one of those cases the app would come up looking perfectly healthy
//! and quietly lose ticks under load. That is precisely the false-OK class the
//! charter forbids: the guarantee is written down in a deploy file, and nothing
//! checks that reality still matches it.
//!
//! So the app reads the limits it actually got, and says so out loud.
//!
//! ## What this is NOT
//!
//! This does not HALT the boot. The app is useful without tuned buffers — today
//! the runtime is REST-only, where a 212 KB receive buffer is irrelevant — and
//! refusing to start over a tuning miss would trade a real outage for a
//! theoretical one. It reports loudly and keeps going; the operator decides.
//!
//! ⚠ CORRECTED 2026-10-30 (plan item 2): "today the runtime is REST-only" is
//! stale. The live Dhan main feed has run since 2026-08-09, so a short receive
//! buffer matters now. Plan item 2 asks for a refusal on mismatch, and its
//! own failure-mode row says "warn for one release, then enforce". This
//! release still warns: refusing to start is a boot-blocker the owner decides,
//! not this module. Since 2026-10-30 the check also reads `vm.max_map_count`
//! and this process's soft open-file limit (`LimitNOFILE`).
//!
//! It also does not TUNE anything. Raising limits is the provisioner's job and
//! requires privileges the service user deliberately does not have.
//!
//! ## Cost note
//!
//! The gauges below are exported on `/metrics` but are deliberately NOT added to
//! the CloudWatch EMF selector: CloudWatch bills ~$0.30/custom-metric/month and
//! this is a boot-time constant, not a live signal. An operator who wants it
//! paged can add the name and its cost note — see
//! `deploy/aws/EMF-METRIC-SELECTOR-NOTES.md`.

use std::path::Path;

use tracing::{info, warn};

/// Receive buffer the feed sizing targets, in bytes (64 MiB; 128 MiB until
/// 2026-10-22, lowered for the 16 GiB r8g.large under operator Quote 38).
///
/// Must stay in lockstep with `net.core.rmem_max` in
/// `deploy/aws/sysctl/99-tickvault-net.conf`. Pinned by
/// `crates/app/tests/host_limits_lockstep_guard.rs`.
pub const REQUIRED_RMEM_MAX_BYTES: u64 = 67_108_864;

/// NIC-to-stack queue depth, in packets.
pub const REQUIRED_NETDEV_MAX_BACKLOG: u64 = 65_536;

/// Listen backlog. Small, but a stock 128 is a real ceiling on reconnect churn.
pub const REQUIRED_SOMAXCONN: u64 = 4_096;

/// Memory-map areas per process (plan item 2, 2026-10-30). QuestDB maps every
/// column file of every open partition, and the stock 65,530 is a real
/// ceiling on a box holding weeks of hour-partitioned tables: past it the
/// database fails its next `mmap` and the WAL apply stalls. QuestDB shares the
/// host kernel with this process, so this process reading it checks the
/// database's limit too.
///
/// Must stay in lockstep with `vm.max_map_count` in
/// `deploy/aws/sysctl/99-tickvault-net.conf`. Pinned by
/// `crates/app/tests/host_limits_lockstep_guard.rs`.
pub const REQUIRED_MAX_MAP_COUNT: u64 = 1_048_576;

/// Open files THIS process may hold (soft `RLIMIT_NOFILE`, plan item 2,
/// 2026-10-30): up to 26 sockets, the WAL segments, the spill and log files,
/// the QuestDB HTTP pool. A stock 1,024 runs out under a reconnect storm and
/// every new socket or file then fails with `EMFILE`.
///
/// Must stay in lockstep with `LimitNOFILE=` in
/// `deploy/systemd/tickvault.service`. Pinned by
/// `crates/app/tests/host_limits_lockstep_guard.rs`.
pub const REQUIRED_OPEN_FILES: u64 = 65_536;

/// The `/proc/self/limits` row that carries the open-file limit.
const OPEN_FILES_LIMITS_ROW: &str = "Max open files";

/// Where one limit's current value is read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LimitSource {
    /// A single-integer file under `/proc/sys`.
    ProcSysScalar(&'static str),
    /// The SOFT open-file limit of this process, from `/proc/self/limits`.
    /// The soft value is the one `open`/`socket` are refused against.
    OpenFilesSoft,
}

impl LimitSource {
    /// The file this limit is read from, for the "unreadable" log line.
    const fn path(self) -> &'static str {
        match self {
            Self::ProcSysScalar(path) => path,
            Self::OpenFilesSoft => "/proc/self/limits",
        }
    }
}

/// One kernel limit and the value the feed needs it to be at least.
struct LimitSpec {
    /// Where the value is read from at boot.
    source: LimitSource,
    /// Human-facing name used in the log line — the sysctl (or systemd
    /// directive) name the operator would type, never the file path.
    sysctl_name: &'static str,
    minimum: u64,
}

/// The limits checked at boot.
///
/// Deliberately a fixed array, not a builder or a config list: these are
/// properties of the feed's sizing arithmetic, not operator preferences, and a
/// configurable minimum would let the check be quietly tuned down to whatever
/// the box happens to have — which is the same as not checking.
const CHECKED_LIMITS: [LimitSpec; 5] = [
    LimitSpec {
        source: LimitSource::ProcSysScalar("/proc/sys/net/core/rmem_max"),
        sysctl_name: "net.core.rmem_max",
        minimum: REQUIRED_RMEM_MAX_BYTES,
    },
    LimitSpec {
        source: LimitSource::ProcSysScalar("/proc/sys/net/core/netdev_max_backlog"),
        sysctl_name: "net.core.netdev_max_backlog",
        minimum: REQUIRED_NETDEV_MAX_BACKLOG,
    },
    LimitSpec {
        source: LimitSource::ProcSysScalar("/proc/sys/net/core/somaxconn"),
        sysctl_name: "net.core.somaxconn",
        minimum: REQUIRED_SOMAXCONN,
    },
    LimitSpec {
        source: LimitSource::ProcSysScalar("/proc/sys/vm/max_map_count"),
        sysctl_name: "vm.max_map_count",
        minimum: REQUIRED_MAX_MAP_COUNT,
    },
    LimitSpec {
        source: LimitSource::OpenFilesSoft,
        sysctl_name: "LimitNOFILE",
        minimum: REQUIRED_OPEN_FILES,
    },
];

/// The verdict for a single limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitStatus {
    /// Kernel value meets or exceeds what the feed needs.
    Met { actual: u64 },
    /// Kernel value is below what the feed needs — tick loss is possible under
    /// load, and the operator needs to know.
    Below { actual: u64, required: u64 },
    /// The `/proc` entry could not be read or parsed. Reported as its own state
    /// rather than folded into `Below`: "we could not tell" and "it is wrong"
    /// call for different operator actions, and collapsing them would let a
    /// non-Linux dev box masquerade as a misconfigured prod box.
    Unknown,
}

impl LimitStatus {
    /// True only for `Met`. `Unknown` is deliberately NOT healthy — an
    /// unreadable limit is an unverified limit, and reporting it as fine is the
    /// false-OK this module exists to prevent.
    #[must_use]
    pub const fn is_met(self) -> bool {
        matches!(self, Self::Met { .. })
    }

    /// Stable label for logs and the gauge.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Met { .. } => "met",
            Self::Below { .. } => "below",
            Self::Unknown => "unknown",
        }
    }
}

/// Classify a read value against a minimum.
///
/// Pure — the whole decision is testable without a kernel.
#[must_use]
pub const fn classify(actual: Option<u64>, required: u64) -> LimitStatus {
    match actual {
        Some(value) if value >= required => LimitStatus::Met { actual: value },
        Some(value) => LimitStatus::Below {
            actual: value,
            required,
        },
        None => LimitStatus::Unknown,
    }
}

/// Parse a `/proc/sys` scalar.
///
/// These files hold a single integer with a trailing newline. Anything else —
/// a multi-value file like `tcp_rmem`, an empty read, a permission-denied stub
/// — parses to `None` and classifies as `Unknown` rather than guessing.
#[must_use]
pub fn parse_proc_scalar(raw: &str) -> Option<u64> {
    raw.trim().parse::<u64>().ok()
}

/// Read and parse a `/proc/sys` scalar, returning `None` if unreadable.
fn read_proc_scalar(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path)
        .ok()
        .as_deref()
        .and_then(parse_proc_scalar)
}

/// Parse the SOFT open-file limit out of a `/proc/self/limits` dump.
///
/// The row reads `Max open files  <soft>  <hard>  files`. `unlimited` reads as
/// `u64::MAX` (it meets any minimum). A missing row, a short row or a value
/// that is not a number parses to `None`, which classifies as `Unknown`
/// rather than guessing. Pure; O(lines) over a ~17-line file, boot only.
#[must_use]
pub fn parse_open_files_soft_limit(raw: &str) -> Option<u64> {
    let row = raw.lines().find_map(|line| {
        line.strip_prefix(OPEN_FILES_LIMITS_ROW)
            .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
    })?;
    let soft = row.split_whitespace().next()?;
    if soft == "unlimited" {
        return Some(u64::MAX);
    }
    soft.parse::<u64>().ok()
}

/// Read one limit's current value, `None` when it cannot be read.
fn read_limit(source: LimitSource) -> Option<u64> {
    match source {
        LimitSource::ProcSysScalar(path) => read_proc_scalar(Path::new(path)),
        LimitSource::OpenFilesSoft => std::fs::read_to_string(source.path())
            .ok()
            .as_deref()
            .and_then(parse_open_files_soft_limit),
    }
}

/// Check every limit, log the verdict, and publish gauges.
///
/// Called once during boot, after the metrics recorder is installed so the
/// gauges are not written into a no-op recorder.
///
/// Returns the number of limits that are NOT met (below or unreadable), so the
/// caller can react if it ever wants to — today nothing does, deliberately.
pub fn check_and_report_host_limits() -> usize {
    let mut unmet = 0_usize;

    for spec in &CHECKED_LIMITS {
        let status = classify(read_limit(spec.source), spec.minimum);

        match status {
            LimitStatus::Met { actual } => {
                info!(
                    sysctl = spec.sysctl_name,
                    actual, "host limit OK — meets the market-data feed requirement"
                );
            }
            LimitStatus::Below { actual, required } => {
                unmet += 1;
                warn!(
                    sysctl = spec.sysctl_name,
                    actual,
                    required,
                    "host limit BELOW the market-data feed requirement — the kernel \
                     may discard incoming data, or refuse a socket or file, under \
                     load, and no downstream buffer can recover it. Apply \
                     deploy/aws/sysctl/99-tickvault-net.conf and run `sysctl \
                     --system` (LimitNOFILE: the tickvault systemd unit)."
                );
            }
            LimitStatus::Unknown => {
                unmet += 1;
                warn!(
                    sysctl = spec.sysctl_name,
                    path = spec.source.path(),
                    "host limit UNREADABLE — cannot verify the market-data feed \
                     requirement. Expected on non-Linux hosts; on the trading box \
                     this means the check is blind, not that the limit is fine."
                );
            }
        }

        // Per-limit gauge, one per spec. The label value is a field, not a
        // literal, so the macro takes its allocating arm; this runs once at
        // boot, so that is fine.
        metrics::gauge!("tv_host_limit_met", "sysctl" => spec.sysctl_name)
            .set(if status.is_met() { 1.0 } else { 0.0 });
    }

    // Single roll-up an alarm could hang off without enumerating every limit.
    metrics::gauge!("tv_host_limits_unmet_total").set(unmet as f64);

    if unmet == 0 {
        info!(
            checked = CHECKED_LIMITS.len(),
            "host kernel limits verified — all meet the market-data feed requirements"
        );
    } else {
        warn!(
            unmet,
            checked = CHECKED_LIMITS.len(),
            "host kernel limits NOT fully verified — see the per-limit lines above"
        );
    }

    unmet
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_and_report_host_limits_is_total_and_counts_only_unmet() {
        // Runs the real reporting function against whatever /proc this machine
        // actually has. Two things are being pinned, and neither depends on
        // the host's tuning:
        //
        // 1. It never panics. It reads /proc paths that may be absent (a
        //    container, macOS, a hardened image), and boot calls it before
        //    anything else — a panic here would take down the process over a
        //    diagnostic.
        // 2. Its return value is bounded by the number of limits checked. It
        //    counts UNMET limits, so it can never exceed the list, and a
        //    return of 0 means every limit was both readable and satisfied.
        let unmet = check_and_report_host_limits();
        assert!(
            unmet <= CHECKED_LIMITS.len(),
            "reported {unmet} unmet limits but only {} are checked — the counter must \
             count limits, not something else",
            CHECKED_LIMITS.len()
        );

        // Idempotent: boot may retry, and a second call must not accumulate.
        assert_eq!(
            unmet,
            check_and_report_host_limits(),
            "the check must be a pure read of /proc — calling it twice must give the \
             same answer"
        );
    }

    #[test]
    fn classify_meets_when_equal_to_requirement() {
        // Boundary: exactly the required value MUST pass. An off-by-one here
        // would page every correctly-tuned box.
        assert_eq!(
            classify(Some(REQUIRED_RMEM_MAX_BYTES), REQUIRED_RMEM_MAX_BYTES),
            LimitStatus::Met {
                actual: REQUIRED_RMEM_MAX_BYTES
            }
        );
    }

    #[test]
    fn classify_meets_when_above_requirement() {
        assert!(classify(Some(REQUIRED_RMEM_MAX_BYTES * 2), REQUIRED_RMEM_MAX_BYTES).is_met());
    }

    #[test]
    fn classify_flags_the_stock_kernel_default() {
        // The real failure this module exists to catch: an untuned box sits at
        // roughly 212 KB, three orders of magnitude below the target.
        let stock_default = 212_992_u64;
        assert_eq!(
            classify(Some(stock_default), REQUIRED_RMEM_MAX_BYTES),
            LimitStatus::Below {
                actual: stock_default,
                required: REQUIRED_RMEM_MAX_BYTES
            }
        );
    }

    #[test]
    fn classify_one_byte_short_is_below_not_met() {
        assert_eq!(
            classify(Some(REQUIRED_RMEM_MAX_BYTES - 1), REQUIRED_RMEM_MAX_BYTES),
            LimitStatus::Below {
                actual: REQUIRED_RMEM_MAX_BYTES - 1,
                required: REQUIRED_RMEM_MAX_BYTES
            }
        );
    }

    #[test]
    fn unreadable_is_unknown_and_never_counts_as_healthy() {
        let status = classify(None, REQUIRED_RMEM_MAX_BYTES);
        assert_eq!(status, LimitStatus::Unknown);
        assert!(
            !status.is_met(),
            "an unverified limit must never report as met — that is the false-OK \
             this module exists to prevent"
        );
    }

    #[test]
    fn parse_proc_scalar_handles_the_real_file_shape() {
        // /proc/sys scalars carry a trailing newline.
        assert_eq!(parse_proc_scalar("134217728\n"), Some(134_217_728));
        assert_eq!(parse_proc_scalar("4096"), Some(4_096));
        assert_eq!(parse_proc_scalar("  8192  \n"), Some(8_192));
    }

    #[test]
    fn parse_proc_scalar_refuses_multi_value_and_junk() {
        // tcp_rmem is "min default max" — parsing its first field would report a
        // 4 KB buffer as the max and page a healthy box, so refuse it outright.
        assert_eq!(parse_proc_scalar("4096 1048576 134217728"), None);
        assert_eq!(parse_proc_scalar(""), None);
        assert_eq!(parse_proc_scalar("\n"), None);
        assert_eq!(parse_proc_scalar("not-a-number"), None);
        assert_eq!(
            parse_proc_scalar("-1"),
            None,
            "negative is not a valid limit"
        );
    }

    #[test]
    fn status_labels_are_stable() {
        assert_eq!(LimitStatus::Met { actual: 1 }.as_str(), "met");
        assert_eq!(
            LimitStatus::Below {
                actual: 1,
                required: 2
            }
            .as_str(),
            "below"
        );
        assert_eq!(LimitStatus::Unknown.as_str(), "unknown");
    }

    #[test]
    fn every_checked_limit_has_a_distinct_sysctl_name_and_path() {
        for (i, a) in CHECKED_LIMITS.iter().enumerate() {
            if let LimitSource::ProcSysScalar(path) = a.source {
                assert!(
                    path.starts_with("/proc/sys/"),
                    "{} must read from /proc/sys",
                    a.sysctl_name
                );
            }
            assert!(a.minimum > 0, "{} needs a real minimum", a.sysctl_name);
            for b in CHECKED_LIMITS.iter().skip(i + 1) {
                assert_ne!(a.sysctl_name, b.sysctl_name, "duplicate sysctl name");
                assert_ne!(a.source, b.source, "duplicate source");
            }
        }
    }

    #[test]
    fn test_plan_item_2_limits_are_all_checked() {
        // Plan item 2 names the open-file limit and the map count beside the
        // receive buffer. Each must be one row of the boot check.
        let names: Vec<&str> = CHECKED_LIMITS.iter().map(|s| s.sysctl_name).collect();
        for wanted in ["net.core.rmem_max", "vm.max_map_count", "LimitNOFILE"] {
            assert!(names.contains(&wanted), "{wanted} is not checked at boot");
        }
        assert_eq!(REQUIRED_MAX_MAP_COUNT, 1_048_576);
        assert_eq!(REQUIRED_OPEN_FILES, 65_536);
    }

    #[test]
    fn test_parse_open_files_soft_limit_reads_the_soft_column() {
        let raw = "Limit                     Soft Limit           Hard Limit           Units     \n\
                   Max cpu time              unlimited            unlimited            seconds   \n\
                   Max open files            1024                 524288               files     \n\
                   Max locked memory         8388608              8388608              bytes     \n";
        assert_eq!(parse_open_files_soft_limit(raw), Some(1_024));
        assert!(
            !classify(parse_open_files_soft_limit(raw), REQUIRED_OPEN_FILES).is_met(),
            "the stock 1,024 soft limit must read as below, even with a high hard limit"
        );
    }

    #[test]
    fn test_parse_open_files_soft_limit_unlimited_and_junk() {
        assert_eq!(
            parse_open_files_soft_limit("Max open files  unlimited  unlimited  files\n"),
            Some(u64::MAX)
        );
        assert_eq!(
            parse_open_files_soft_limit("Max open files  65536  65536  files\n"),
            Some(65_536)
        );
        assert_eq!(parse_open_files_soft_limit(""), None, "no row");
        assert_eq!(
            parse_open_files_soft_limit("Max open files\n"),
            None,
            "row with no value"
        );
        assert_eq!(
            parse_open_files_soft_limit("Max open files  lots  more  files\n"),
            None,
            "non-numeric soft value"
        );
        assert_eq!(
            parse_open_files_soft_limit("Max open filesystems  5\n"),
            None,
            "a different row that shares the prefix must not be read"
        );
    }

    #[test]
    fn test_limit_source_path_names_the_file_read() {
        assert_eq!(
            LimitSource::ProcSysScalar("/proc/sys/vm/max_map_count").path(),
            "/proc/sys/vm/max_map_count"
        );
        assert_eq!(LimitSource::OpenFilesSoft.path(), "/proc/self/limits");
    }

    #[test]
    fn check_runs_without_panicking_on_any_host() {
        // Runs in CI (Linux, untuned) and on a dev box alike. The contract is
        // that it never panics and never exceeds the number of limits checked —
        // NOT that the host is tuned, which CI's certainly is not.
        let unmet = check_and_report_host_limits();
        assert!(unmet <= CHECKED_LIMITS.len());
    }
}

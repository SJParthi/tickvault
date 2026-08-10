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

/// Receive buffer the feed sizing targets, in bytes (128 MB).
///
/// Must stay in lockstep with `net.core.rmem_max` in
/// `deploy/aws/sysctl/99-tickvault-net.conf`. Pinned by
/// `crates/app/tests/host_limits_lockstep_guard.rs`.
pub const REQUIRED_RMEM_MAX_BYTES: u64 = 134_217_728;

/// NIC-to-stack queue depth, in packets.
pub const REQUIRED_NETDEV_MAX_BACKLOG: u64 = 65_536;

/// Listen backlog. Small, but a stock 128 is a real ceiling on reconnect churn.
pub const REQUIRED_SOMAXCONN: u64 = 4_096;

/// One kernel limit and the value the feed needs it to be at least.
struct LimitSpec {
    /// `/proc/sys` path, read at boot.
    proc_path: &'static str,
    /// Human-facing name used in the log line — the sysctl name the operator
    /// would type, never the file path.
    sysctl_name: &'static str,
    minimum: u64,
}

/// The limits checked at boot.
///
/// Deliberately a fixed array, not a builder or a config list: these are
/// properties of the feed's sizing arithmetic, not operator preferences, and a
/// configurable minimum would let the check be quietly tuned down to whatever
/// the box happens to have — which is the same as not checking.
const CHECKED_LIMITS: [LimitSpec; 3] = [
    LimitSpec {
        proc_path: "/proc/sys/net/core/rmem_max",
        sysctl_name: "net.core.rmem_max",
        minimum: REQUIRED_RMEM_MAX_BYTES,
    },
    LimitSpec {
        proc_path: "/proc/sys/net/core/netdev_max_backlog",
        sysctl_name: "net.core.netdev_max_backlog",
        minimum: REQUIRED_NETDEV_MAX_BACKLOG,
    },
    LimitSpec {
        proc_path: "/proc/sys/net/core/somaxconn",
        sysctl_name: "net.core.somaxconn",
        minimum: REQUIRED_SOMAXCONN,
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
        let status = classify(read_proc_scalar(Path::new(spec.proc_path)), spec.minimum);

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
                     may discard incoming data under load, and no downstream buffer \
                     can recover it. Apply deploy/aws/sysctl/99-tickvault-net.conf \
                     and run `sysctl --system`."
                );
            }
            LimitStatus::Unknown => {
                unmet += 1;
                warn!(
                    sysctl = spec.sysctl_name,
                    path = spec.proc_path,
                    "host limit UNREADABLE — cannot verify the market-data feed \
                     requirement. Expected on non-Linux hosts; on the trading box \
                     this means the check is blind, not that the limit is fine."
                );
            }
        }

        // Per-limit gauge. Static label set (one per spec, known at compile
        // time), so this allocates nothing per emission.
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
            assert!(
                a.proc_path.starts_with("/proc/sys/"),
                "{} must read from /proc/sys",
                a.sysctl_name
            );
            assert!(a.minimum > 0, "{} needs a real minimum", a.sysctl_name);
            for b in CHECKED_LIMITS.iter().skip(i + 1) {
                assert_ne!(a.sysctl_name, b.sysctl_name, "duplicate sysctl name");
                assert_ne!(a.proc_path, b.proc_path, "duplicate proc path");
            }
        }
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

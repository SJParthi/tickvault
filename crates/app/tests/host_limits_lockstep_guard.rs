//! Guard: the boot-time limit check and the sysctl file it verifies must agree.
//!
//! `crates/app/src/host_limits.rs` asserts at boot that the kernel gave us the
//! buffers the feed needs. `deploy/aws/sysctl/99-tickvault-net.conf` is what
//! sets them. If those two drift, the check becomes theatre in one of two ways:
//!
//! - the constant drifts BELOW the file: a correctly-tuned box passes a check
//!   that is no longer measuring the real target;
//! - the constant drifts ABOVE the file: every correctly-provisioned box warns
//!   forever, and the operator learns to ignore the warning.
//!
//! The second is worse, and it is the one that happens in practice — someone
//! raises the requirement in Rust, forgets the deploy file, and the resulting
//! permanent warning trains everyone that this signal is noise.
//!
//! This guard also pins the call site, because a check that never runs is the
//! same as no check.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .expect("host_limits_lockstep_guard: `git rev-parse` failed");
    assert!(out.status.success(), "not a git checkout");
    PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn read(rel: &str) -> String {
    let path: PathBuf = repo_root().join(rel);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("host_limits_lockstep_guard: cannot read {rel}: {e}"))
}

/// Pull `key = value` out of the sysctl conf, ignoring `#` comments.
fn sysctl_value(conf: &str, key: &str) -> Option<u64> {
    conf.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .find_map(|line| {
            let (k, v) = line.split_once('=')?;
            if k.trim() != key {
                return None;
            }
            v.trim().parse::<u64>().ok()
        })
}

/// Pull a `pub const NAME: u64 = 123_456;` literal out of the Rust source.
fn rust_const(src: &str, name: &str) -> Option<u64> {
    let needle = format!("pub const {name}: u64 = ");
    let start = src.find(&needle)? + needle.len();
    let rest = &src[start..];
    let end = rest.find(';')?;
    rest[..end].replace('_', "").trim().parse::<u64>().ok()
}

#[test]
fn rust_constants_match_the_deployed_sysctl_file() {
    let conf = read("deploy/aws/sysctl/99-tickvault-net.conf");
    let src = read("crates/app/src/host_limits.rs");

    for (const_name, sysctl_key) in [
        ("REQUIRED_RMEM_MAX_BYTES", "net.core.rmem_max"),
        ("REQUIRED_NETDEV_MAX_BACKLOG", "net.core.netdev_max_backlog"),
        ("REQUIRED_SOMAXCONN", "net.core.somaxconn"),
    ] {
        let in_rust = rust_const(&src, const_name)
            .unwrap_or_else(|| panic!("host_limits.rs no longer defines {const_name}"));
        let in_conf = sysctl_value(&conf, sysctl_key)
            .unwrap_or_else(|| panic!("99-tickvault-net.conf no longer sets {sysctl_key}"));

        assert_eq!(
            in_rust, in_conf,
            "LOCKSTEP DRIFT: host_limits.rs {const_name} = {in_rust} but \
             99-tickvault-net.conf sets {sysctl_key} = {in_conf}.\n\
             Raise BOTH or neither. A Rust constant above the deployed value \
             makes every correctly-provisioned box warn forever, which teaches \
             the operator to ignore the warning — worse than no check at all."
        );
    }
}

#[test]
fn the_check_is_actually_called_at_boot() {
    let main = read("crates/app/src/main.rs");
    assert!(
        main.contains("host_limits::check_and_report_host_limits()"),
        "boot no longer calls check_and_report_host_limits — an uncalled check \
         is the same as no check"
    );
}

#[test]
fn the_check_runs_after_the_metrics_recorder_is_installed() {
    // Gauges written before the recorder is installed resolve to a no-op and
    // are silently discarded — the same first-sample trap main.rs documents for
    // its counter pre-registrations.
    let main = read("crates/app/src/main.rs");
    let install = main
        .find("observability::init_metrics")
        .expect("main.rs no longer installs the metrics recorder");
    let check = main
        .find("host_limits::check_and_report_host_limits()")
        .expect("main.rs no longer calls the host-limit check");
    assert!(
        install < check,
        "host-limit check runs BEFORE init_metrics — its gauges would be \
         written into a no-op recorder and silently lost"
    );
}

#[test]
fn user_data_installs_the_sysctl_file_it_checks() {
    // The check verifies a file that only exists on the box if user-data copies
    // it there. If that copy is dropped, every box warns and nobody knows why.
    let tftpl = read("deploy/aws/terraform/user-data.sh.tftpl");
    assert!(
        tftpl.contains("99-tickvault-net.conf"),
        "user-data no longer installs 99-tickvault-net.conf, but host_limits.rs \
         still checks for the limits it sets"
    );
    assert!(
        Path::new(&repo_root().join("deploy/aws/sysctl/99-tickvault-net.conf")).exists(),
        "the sysctl file user-data copies has been deleted"
    );
}

#[test]
fn parsers_are_self_tested() {
    // The two extractors above are the load-bearing part of this guard; a silent
    // parse failure would make every assertion vacuous.
    let conf = "# comment = 999\nnet.core.rmem_max = 134217728\nother = 5\n";
    assert_eq!(sysctl_value(conf, "net.core.rmem_max"), Some(134_217_728));
    assert_eq!(sysctl_value(conf, "missing.key"), None);
    assert_eq!(
        sysctl_value("# net.core.rmem_max = 1\n", "net.core.rmem_max"),
        None,
        "a commented-out line must not read as a setting"
    );

    let src = "pub const REQUIRED_RMEM_MAX_BYTES: u64 = 134_217_728;\n";
    assert_eq!(
        rust_const(src, "REQUIRED_RMEM_MAX_BYTES"),
        Some(134_217_728)
    );
    assert_eq!(rust_const(src, "NOT_THERE"), None);
}

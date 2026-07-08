//! §34 PR-3 (addendum A) — Mac scale-test PREFLIGHT, run in-app at boot
//! BEFORE the Groww auto-scale fleet spawns.
//!
//! The operator runs the 100-connection experiment on their Mac
//! (`make scale-test`). This module is the fail-closed gate between
//! "`[feeds.groww.scale] enabled = true` in config" and "N Python sidecars
//! actually spawn": it verifies the LOCAL capture environment (shards dir
//! writable, disk headroom, QuestDB ILP + HTTP reachable) and prints the
//! PROD-IS-UNTOUCHED banner so the boundary is explicit every run.
//!
//! Honest envelope:
//! - Every check is LOCAL. The ONLY AWS interaction the whole scale test
//!   performs is the READ-ONLY SSM `GetParameter` for the Groww token
//!   (authorized by `groww-shared-token-minter-2026-07-02.md`) — no AWS
//!   resource is created, written, deployed, or SSH'd.
//! - Probes that cannot run on this platform report `Skip` with a reason —
//!   never a silent pass, never a false fail (same doctrine as the ladder's
//!   host gates).
//! - On ANY `Fail`, the caller falls back to the SINGLE-CONNECTION Groww
//!   path — capture continues; only the multi-conn experiment is refused.

use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::Duration;

use tickvault_common::config::QuestDbConfig;
use tickvault_storage::disk_health_watcher::{DiskHealthOutcome, probe_disk_free_bytes};
use tracing::{error, info, warn};

/// One preflight check's outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreflightOutcome {
    /// Check ran and passed.
    Pass(String),
    /// Check cannot run on this platform / configuration — honestly skipped.
    Skip(String),
    /// Check ran and FAILED — the scale fleet must not spawn.
    Fail(String),
}

/// The full preflight report: `(check_name, outcome)` per check.
#[derive(Clone, Debug, Default)]
pub struct PreflightReport {
    /// Ordered check results.
    pub checks: Vec<(&'static str, PreflightOutcome)>,
}

impl PreflightReport {
    /// `true` when NO check failed (`Pass` + `Skip` are both acceptable —
    /// a skipped probe is an honest unknown, not a fault).
    #[must_use]
    pub fn all_ok(&self) -> bool {
        !self
            .checks
            .iter()
            .any(|(_, o)| matches!(o, PreflightOutcome::Fail(_)))
    }
}

/// TCP connect timeout for the QuestDB reachability probes.
pub const PREFLIGHT_TCP_TIMEOUT_SECS: u64 = 3;

/// Minimum free-disk percentage on the capture volume for the fleet to spawn
/// (mirrors the ladder's default disk gate).
pub const PREFLIGHT_MIN_DISK_FREE_PCT: f64 = 20.0;

/// Pure classification: is the QuestDB host local to this box? Used for the
/// banner wording (a LOCAL host is the expected Mac scale-test shape; a
/// non-local host gets a warning note, not a failure — prod's own QuestDB is
/// also 127.0.0.1, so this never blocks a legitimate deployment).
#[must_use]
pub fn is_local_host(host: &str) -> bool {
    // APPROVED: loopback CLASSIFIER for the banner wording — these literals
    // identify a local host, they are never used as a connection target.
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "0.0.0.0") // APPROVED: classifier, not a target
}

/// Pure check: probe outcome from a disk sample. `None` free-percent =
/// probe unavailable → `Skip`.
#[must_use]
pub fn classify_disk_check(free_pct: Option<f64>, min_free_pct: f64) -> PreflightOutcome {
    match free_pct {
        None => PreflightOutcome::Skip(
            "disk probe unavailable on this platform — ladder disk gate also skips".to_owned(),
        ),
        Some(pct) if pct >= min_free_pct => {
            PreflightOutcome::Pass(format!("{pct:.1}% free on the capture volume"))
        }
        Some(pct) => PreflightOutcome::Fail(format!(
            "only {pct:.1}% free on the capture volume (need ≥ {min_free_pct:.0}%) — \
             free disk before running the scale test"
        )),
    }
}

/// Filesystem check: the shards root is creatable + writable (a conn without
/// a writable shard dir could neither read its watch file nor capture).
fn check_shards_dir_writable(shards_root: &Path) -> PreflightOutcome {
    if let Err(e) = std::fs::create_dir_all(shards_root) {
        return PreflightOutcome::Fail(format!("cannot create shards dir: {e}"));
    }
    let probe = shards_root.join(".preflight-write-probe");
    match std::fs::write(&probe, b"ok") {
        Ok(()) => {
            // Best-effort probe cleanup — a leftover probe file is harmless.
            drop(std::fs::remove_file(&probe));
            PreflightOutcome::Pass(format!("{} writable", shards_root.display()))
        }
        Err(e) => PreflightOutcome::Fail(format!("shards dir not writable: {e}")),
    }
}

/// TCP reachability of one `host:port` with a bounded timeout.
fn check_tcp_reachable(host: &str, port: u16, what: &str) -> PreflightOutcome {
    let addr_iter = match (host, port).to_socket_addrs() {
        Ok(iter) => iter,
        Err(e) => return PreflightOutcome::Fail(format!("{what}: cannot resolve {host}: {e}")),
    };
    for addr in addr_iter {
        if TcpStream::connect_timeout(&addr, Duration::from_secs(PREFLIGHT_TCP_TIMEOUT_SECS))
            .is_ok()
        {
            return PreflightOutcome::Pass(format!("{what} reachable at {host}:{port}"));
        }
    }
    PreflightOutcome::Fail(format!(
        "{what} NOT reachable at {host}:{port} — is the local QuestDB container up? \
         (`make docker-up` / `make scale-test` starts it)"
    ))
}

/// Runs every preflight check and prints the PROD-IS-UNTOUCHED banner.
///
/// Returns the report; the caller gates the fleet spawn on
/// [`PreflightReport::all_ok`].
// TEST-EXEMPT: composition-only I/O runner; the decision pieces (classify_disk_check, is_local_host, PreflightReport::all_ok, the writable-dir check via its Fail/Pass surfaces) are unit-tested below with temp dirs + a live local listener.
pub fn run_scale_preflight(shards_root: &Path, qdb: &QuestDbConfig) -> PreflightReport {
    let mut report = PreflightReport::default();

    // ── The boundary banner, printed EVERY scale run. ──
    let locality = if is_local_host(&qdb.host) {
        "the LOCAL QuestDB on this machine"
    } else {
        "a NON-local QuestDB host (check this is intentional)"
    };
    info!(
        qdb_host = %qdb.host,
        "================ GROWW SCALE TEST PREFLIGHT ================\n\
         PROD IS UNTOUCHED: this run captures to {locality}. The ONLY AWS \n\
         interaction is the READ-ONLY SSM GetParameter for the Groww token \n\
         (token-minter lock) — no deploy, no SSH, no terraform, no AWS write.\n\
         ============================================================"
    );

    report.checks.push((
        "shards_dir_writable",
        check_shards_dir_writable(shards_root),
    ));

    let free_pct = match probe_disk_free_bytes(shards_root) {
        DiskHealthOutcome::Ok {
            free_bytes,
            total_bytes,
        } if total_bytes > 0 => {
            #[allow(clippy::cast_precision_loss)] // APPROVED: ratio display only
            Some((free_bytes as f64 / total_bytes as f64) * 100.0)
        }
        _ => None,
    };
    report.checks.push((
        "disk_free",
        classify_disk_check(free_pct, PREFLIGHT_MIN_DISK_FREE_PCT),
    ));

    report.checks.push((
        "questdb_ilp",
        check_tcp_reachable(&qdb.host, qdb.ilp_port, "QuestDB ILP"),
    ));
    report.checks.push((
        "questdb_http",
        check_tcp_reachable(&qdb.host, qdb.http_port, "QuestDB HTTP"),
    ));

    for (name, outcome) in &report.checks {
        match outcome {
            PreflightOutcome::Pass(detail) => info!(check = name, %detail, "preflight PASS"),
            PreflightOutcome::Skip(detail) => warn!(check = name, %detail, "preflight SKIP"),
            PreflightOutcome::Fail(detail) => error!(check = name, %detail, "preflight FAIL"),
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_preflight_report_all_ok_classification() {
        // Pass + Skip are both acceptable; a single Fail flips the verdict.
        let mut r = PreflightReport::default();
        assert!(r.all_ok(), "empty report is ok");
        r.checks
            .push(("a", PreflightOutcome::Pass("fine".to_owned())));
        r.checks
            .push(("b", PreflightOutcome::Skip("no probe".to_owned())));
        assert!(r.all_ok(), "pass + skip ⇒ ok");
        r.checks
            .push(("c", PreflightOutcome::Fail("broken".to_owned())));
        assert!(!r.all_ok(), "any fail ⇒ not ok");
    }

    #[test]
    fn test_classify_disk_check_boundaries() {
        assert!(matches!(
            classify_disk_check(None, 20.0),
            PreflightOutcome::Skip(_)
        ));
        assert!(matches!(
            classify_disk_check(Some(20.0), 20.0),
            PreflightOutcome::Pass(_)
        ));
        assert!(matches!(
            classify_disk_check(Some(19.9), 20.0),
            PreflightOutcome::Fail(_)
        ));
    }

    #[test]
    fn test_is_local_host_classification() {
        assert!(is_local_host("127.0.0.1"));
        assert!(is_local_host("localhost"));
        assert!(is_local_host("::1"));
        assert!(!is_local_host("10.0.0.5"));
        assert!(!is_local_host("questdb.internal"));
    }

    #[test]
    fn test_check_shards_dir_writable_pass_and_fail() {
        let dir = std::env::temp_dir().join(format!("tv-preflight-{}", std::process::id()));
        assert!(matches!(
            check_shards_dir_writable(&dir),
            PreflightOutcome::Pass(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        // A path that cannot be a directory (a FILE in the way) must fail.
        let blocker =
            std::env::temp_dir().join(format!("tv-preflight-file-{}", std::process::id()));
        std::fs::write(&blocker, b"x").expect("write blocker");
        assert!(matches!(
            check_shards_dir_writable(&blocker),
            PreflightOutcome::Fail(_)
        ));
        let _ = std::fs::remove_file(&blocker);
    }

    #[test]
    fn test_check_tcp_reachable_against_live_listener_and_dead_port() {
        // Live: bind an ephemeral local listener.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        assert!(matches!(
            check_tcp_reachable("127.0.0.1", port, "probe"),
            PreflightOutcome::Pass(_)
        ));
        drop(listener);
        // Dead: the just-freed port now refuses (bounded by the 3s timeout).
        assert!(matches!(
            check_tcp_reachable("127.0.0.1", port, "probe"),
            PreflightOutcome::Fail(_)
        ));
    }

    #[test]
    fn test_run_scale_preflight_reports_four_checks() {
        // Composition smoke over a temp dir + an unreachable QuestDB: the
        // report always carries the 4 named checks in order.
        let dir = std::env::temp_dir().join(format!("tv-preflight-run-{}", std::process::id()));
        let qdb = QuestDbConfig {
            host: "127.0.0.1".to_owned(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1, // port 1: reliably refused
        };
        let report = run_scale_preflight(&dir, &qdb);
        let names: Vec<&str> = report.checks.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            vec![
                "shards_dir_writable",
                "disk_free",
                "questdb_ilp",
                "questdb_http"
            ]
        );
        // QuestDB unreachable ⇒ the report must NOT be all_ok (fail-closed).
        assert!(!report.all_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! Coverage for the cold-path helpers in `infra`, `host_tuning`,
//! `dhan_rest_stack` and `dhan_universe` that are reachable through their
//! `pub` surface without touching the host kernel, AWS, Docker or Dhan.
//!
//! What is deliberately NOT driven here (and why):
//! - `host_tuning::run_cli` with `bbr` / `verify` / `apply`, and
//!   `apply_questdb_host_env`: they write `/etc`, `/sys` or
//!   `/opt/tickvault/...` on the host. Only the pure planner and the
//!   read-only verifier are exercised.
//! - `infra::ensure_infra_running` / `container_health_snapshot`: they shell
//!   out to Docker and read SSM.
//! - `dhan_rest_stack::spawn_dhan_rest_stack`: it reads SSM and mints a token.
//!
//! Logs are asserted through a scoped `tracing` subscriber writing into a
//! buffer, so a branch that only logs is still checked for WHAT it said.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tickvault_app::dhan_rest_stack::{
    LockReleaseAtShutdown, ShutdownLockGate, release_instance_lock_at_shutdown,
};
use tickvault_app::dhan_universe::{ist_secs_of_day, next_wait, spawn_dhan_universe_rider};
use tickvault_app::host_tuning::{
    HOST_TUNING_SUBCOMMAND, QDB_CORE_ISOLATION_KEY, plan_questdb_host_env, run_cli,
    verify_this_host,
};
use tickvault_app::infra::{
    ClockSkewError, ClockSkewSample, enforce_clock_skew_at_boot, notify_systemd_ready,
    notify_systemd_watchdog, open_in_browser, probe_clock_skew, spawn_clock_skew_poller,
};
use tickvault_common::config::{DhanUniverseConfig, QuestDbConfig};

// ---------------------------------------------------------------------------
// Log capture
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("capture").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("capture")).to_string()
    }
}

/// A subscriber scoped to the current thread for as long as the guard lives.
fn capture_logs() -> (Capture, tracing::subscriber::DefaultGuard) {
    let cap = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(cap.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (cap, guard)
}

/// Serialises every test in this binary that mutates process env vars.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir =
        std::env::temp_dir().join(format!("tv-cov-misc-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

// ---------------------------------------------------------------------------
// infra — clock skew
// ---------------------------------------------------------------------------

#[test]
fn test_infra_clock_skew_error_display_names_both_variants() {
    let unavailable = ClockSkewError::Unavailable {
        primary: "chronyc spawn failed: not found".to_owned(),
        fallback: "no fallback".to_owned(),
    };
    let text = unavailable.to_string();
    assert_eq!(
        text,
        "clock-skew probe unavailable: chronyc spawn failed: not found (primary), \
         no fallback (fallback)"
    );

    let exceeded = ClockSkewError::ThresholdExceeded {
        skew_secs: -2.5,
        threshold_secs: 2.0,
        source: "chronyc",
    };
    assert_eq!(
        exceeded.to_string(),
        "BOOT-03 clock skew -2.500s exceeds threshold 2.00s (source: chronyc)"
    );
    // Positive skews carry an explicit sign.
    let ahead = ClockSkewError::ThresholdExceeded {
        skew_secs: 3.25,
        threshold_secs: 0.5,
        source: "chronyc",
    };
    assert!(ahead.to_string().contains("+3.250s"));

    // It is a real std::error::Error with no inner source.
    let as_error: &dyn std::error::Error = &exceeded;
    assert!(as_error.source().is_none());
}

#[test]
fn test_infra_clock_skew_sample_exceeds_is_inclusive_and_sign_blind() {
    let at = ClockSkewSample {
        skew_secs: 2.0,
        source: "chronyc",
    };
    assert!(at.exceeds(2.0), "a skew equal to the threshold exceeds it");
    let behind = ClockSkewSample {
        skew_secs: -2.0,
        source: "chronyc",
    };
    assert!(
        behind.exceeds(2.0),
        "a clock BEHIND by the threshold also exceeds"
    );
    let fine = ClockSkewSample {
        skew_secs: 1.999,
        source: "chronyc",
    };
    assert!(!fine.exceeds(2.0));
    assert!(
        fine.exceeds(0.0),
        "a zero threshold is exceeded by any sample"
    );
}

/// Points `PATH` at an empty folder for as long as it lives, so `chronyc`
/// (or any launcher) cannot be found, and puts the old value back on drop.
/// Hold [`ENV_LOCK`] for the whole lifetime: `PATH` is process-wide.
struct EmptyPath {
    dir: std::path::PathBuf,
    saved: Option<std::ffi::OsString>,
}

impl EmptyPath {
    fn new(tag: &str) -> Self {
        let dir = unique_temp_dir(tag);
        let saved = std::env::var_os("PATH");
        // SAFETY: Rust 2024 marks env mutation unsafe; every caller holds
        // ENV_LOCK, which serialises every env change in this binary, and
        // the value is restored on drop.
        unsafe {
            std::env::set_var("PATH", &dir);
        }
        Self { dir, saved }
    }
}

impl Drop for EmptyPath {
    fn drop(&mut self) {
        // SAFETY: see `EmptyPath::new`.
        unsafe {
            match self.saved.take() {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The exact `Unavailable` shape when `chronyc` cannot be spawned.
fn assert_chronyc_unavailable(err: &ClockSkewError) {
    match err {
        ClockSkewError::Unavailable { primary, fallback } => {
            assert!(
                primary.starts_with("chronyc spawn failed:"),
                "the primary failure names chronyc and the spawn: {primary}"
            );
            assert!(
                fallback.starts_with("none:") && fallback.contains("cannot measure skew"),
                "there is no fallback source, and it says so: {fallback}"
            );
            assert!(
                err.to_string()
                    .starts_with("clock-skew probe unavailable: chronyc"),
                "{err}"
            );
        }
        other => panic!("a missing chronyc must be Unavailable, got {other:?}"),
    }
}

/// Polls `pred` every 20 ms for up to five seconds.
async fn eventually(mut pred: impl FnMut() -> bool) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if pred() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    pred()
}

#[tokio::test]
async fn test_infra_probe_clock_skew_without_chronyc_is_a_typed_unavailable() {
    let _env = ENV_LOCK.lock().await;
    let _path = EmptyPath::new("probe");
    let err = probe_clock_skew()
        .await
        .expect_err("no chronyc on PATH means no sample");
    assert_chronyc_unavailable(&err);
}

#[tokio::test]
async fn test_infra_enforce_clock_skew_at_boot_without_chronyc_is_unavailable_at_any_threshold() {
    let _env = ENV_LOCK.lock().await;
    let _path = EmptyPath::new("enforce");
    // A zero threshold would be exceeded by any sample and 1e9 s by none;
    // with no chronyc the probe fails first, so both report Unavailable and
    // neither invents a pass or a threshold verdict.
    for threshold in [0.0, 2.0, 1.0e9] {
        let err = enforce_clock_skew_at_boot(threshold)
            .await
            .expect_err("no sample, no pass");
        assert_chronyc_unavailable(&err);
    }
}

#[tokio::test]
async fn test_infra_spawn_clock_skew_poller_keeps_running_and_logs_failed_resamples() {
    let _env = ENV_LOCK.lock().await;
    let _path = EmptyPath::new("poller");
    let (cap, _guard) = capture_logs();
    let handle = spawn_clock_skew_poller(Duration::from_millis(20));
    // No chronyc: every re-sample fails, and the first failure (a power of
    // two) is logged with the BOOT-03 code.
    let reported = eventually(|| {
        let logs = cap.text();
        logs.contains("clock-skew re-sample FAILED") && logs.contains("BOOT-03")
    })
    .await;
    assert!(
        reported,
        "a failing re-sample must be reported with BOOT-03: {}",
        cap.text()
    );
    let logs = cap.text();
    assert!(
        logs.contains("probe_unavailable") && logs.contains("chronyc spawn failed"),
        "the line says why: {logs}"
    );
    assert!(
        !handle.is_finished(),
        "the supervised poller never returns on its own"
    );
    handle.abort();
    let err = handle.await.expect_err("aborted");
    assert!(err.is_cancelled());
}

// ---------------------------------------------------------------------------
// infra — sd_notify error arms and the browser launcher
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_infra_notify_and_browser_failure_arms_log_and_return() {
    let _env = ENV_LOCK.lock().await;
    let dir = unique_temp_dir("notify");
    let missing_socket = dir.join("no-such-notify.sock");
    let saved_path = std::env::var_os("PATH");

    let (cap, guard) = capture_logs();
    // SAFETY: Rust 2024 marks env mutation unsafe; ENV_LOCK serialises every
    // env change in this binary and the values are restored below.
    unsafe {
        std::env::set_var("NOTIFY_SOCKET", &missing_socket);
    }
    notify_systemd_watchdog();
    notify_systemd_ready();
    let logs = cap.text();
    assert!(
        logs.contains("failed to send WATCHDOG=1"),
        "a socket path nobody listens on fails the send: {logs}"
    );
    assert!(logs.contains("failed to send READY=1"));
    assert!(!logs.contains("READY=1 sent to systemd"));

    // An empty NOTIFY_SOCKET is "not under systemd": no attempt, no line.
    let before = cap.text().len();
    // SAFETY: see above.
    unsafe {
        std::env::set_var("NOTIFY_SOCKET", "");
    }
    notify_systemd_watchdog();
    notify_systemd_ready();
    assert_eq!(cap.text().len(), before, "an empty socket path is a no-op");

    // Browser launcher with no launcher on PATH: the spawn error is logged
    // and the call returns (best-effort, never blocks boot).
    // SAFETY: see above.
    unsafe {
        std::env::set_var("PATH", &dir);
    }
    open_in_browser("http://127.0.0.1:9/never-opened").await;
    let logs = cap.text();
    assert!(
        logs.contains("failed to launch browser command"),
        "a missing launcher is reported: {logs}"
    );
    drop(guard);

    // SAFETY: see above.
    unsafe {
        std::env::remove_var("NOTIFY_SOCKET");
        match saved_path {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// host_tuning — the pure QuestDB env planner + read-only verifier + CLI
// ---------------------------------------------------------------------------

fn value_of<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines()
        .filter_map(|l| l.strip_prefix(key)?.strip_prefix('='))
        .next_back()
}

#[test]
fn test_host_tuning_plan_isolation_on_saves_operator_values_and_sets_one_core() {
    let env = format!("{QDB_CORE_ISOLATION_KEY}=1\nTV_QDB_CPUSET=0-1\nTV_QDB_CPUS=2.0\nOTHER=keep");
    let planned = plan_questdb_host_env(2, 16, &env, false).expect("isolation changes the env");
    assert_eq!(value_of(&planned, "TV_QDB_CPUSET"), Some("0"));
    assert_eq!(value_of(&planned, "TV_QDB_CPUS"), Some("1.0"));
    assert_eq!(value_of(&planned, "TV_QDB_WAL_WORKERS"), Some("1"));
    assert_eq!(value_of(&planned, "TV_QDB_SHARED_WORKERS"), Some("1"));
    assert_eq!(
        value_of(&planned, "TV_QDB_CORE_ISOLATION_APPLIED"),
        Some("1")
    );
    // The operator's own values are kept for the switch-off.
    assert_eq!(
        value_of(&planned, "TV_QDB_CORE_ISOLATION_WAS_TV_QDB_CPUSET"),
        Some("0-1")
    );
    assert_eq!(
        value_of(&planned, "TV_QDB_CORE_ISOLATION_WAS_TV_QDB_CPUS"),
        Some("2.0")
    );
    // Unrelated lines survive, and the missing trailing newline was added.
    assert_eq!(value_of(&planned, "OTHER"), Some("keep"));
    assert!(planned.ends_with('\n'));
    // One assignment per key: the old CPUSET line was replaced, not kept.
    assert_eq!(
        planned
            .lines()
            .filter(|l| l.starts_with("TV_QDB_CPUSET="))
            .count(),
        1
    );

    // Idempotent: planning the planned text changes nothing.
    assert_eq!(plan_questdb_host_env(2, 16, &planned, false), None);

    // Switch off: exactly what isolation set is undone and the saved
    // operator values come back; keys with no saved value are removed.
    let off = planned.replace(
        &format!("{QDB_CORE_ISOLATION_KEY}=1"),
        &format!("{QDB_CORE_ISOLATION_KEY}=0"),
    );
    let restored = plan_questdb_host_env(2, 16, &off, false).expect("switch-off changes the env");
    assert_eq!(value_of(&restored, "TV_QDB_CPUSET"), Some("0-1"));
    assert_eq!(value_of(&restored, "TV_QDB_CPUS"), Some("2.0"));
    assert_eq!(value_of(&restored, "TV_QDB_WAL_WORKERS"), None);
    assert_eq!(value_of(&restored, "TV_QDB_SHARED_WORKERS"), None);
    assert_eq!(value_of(&restored, "TV_QDB_CORE_ISOLATION_APPLIED"), None);
    assert!(
        !restored.contains("TV_QDB_CORE_ISOLATION_WAS_"),
        "no saved key is left behind: {restored}"
    );
    assert_eq!(value_of(&restored, "OTHER"), Some("keep"));
}

#[test]
fn test_host_tuning_plan_isolation_switch_accepts_quoted_on_and_three_cores() {
    let env = format!("{QDB_CORE_ISOLATION_KEY}=\"ON\"\n");
    let planned = plan_questdb_host_env(3, 16, &env, false).expect("quoted ON switches it on");
    assert_eq!(value_of(&planned, "TV_QDB_CPUSET"), Some("0"));
    // No operator value existed, so nothing was saved.
    assert!(!planned.contains("TV_QDB_CORE_ISOLATION_WAS_"));

    // Switching it off with no saved values removes every isolation key and
    // replans the shared set for a 3-core host.
    let off = planned.replace("\"ON\"", "off");
    let restored = plan_questdb_host_env(3, 16, &off, false).expect("switch-off changes");
    assert_eq!(value_of(&restored, "TV_QDB_CPUSET"), Some("0-2"));
    assert_eq!(value_of(&restored, "TV_QDB_CPUS"), Some("3.0"));
    assert_eq!(value_of(&restored, "TV_QDB_CORE_ISOLATION_APPLIED"), None);
}

#[test]
fn test_host_tuning_plan_isolation_ignored_where_it_does_not_fit() {
    // Four cores: the default cpuset fits, isolation is not for this host.
    let env = format!("{QDB_CORE_ISOLATION_KEY}=1\n");
    assert_eq!(plan_questdb_host_env(4, 16, &env, false), None);
    // One core: isolation needs a second core for the readers.
    let one = plan_questdb_host_env(1, 16, &env, false).expect("1 core still needs a cpuset");
    assert_eq!(value_of(&one, "TV_QDB_CPUSET"), Some("0"));
    assert_eq!(value_of(&one, "TV_QDB_CPUS"), Some("1.0"));
    assert_eq!(value_of(&one, "TV_QDB_CORE_ISOLATION_APPLIED"), None);
}

#[test]
fn test_host_tuning_plan_unknown_core_count_never_undoes_isolation() {
    let applied = "TV_QDB_CORE_ISOLATION_APPLIED=1\nTV_QDB_CPUSET=0\n";
    // cores = 0 (unknown): nothing is planned, the applied set stays.
    assert_eq!(plan_questdb_host_env(0, 16, applied, false), None);
    // A known core count with the switch absent undoes it.
    let undone = plan_questdb_host_env(2, 16, applied, false).expect("undo");
    assert_eq!(value_of(&undone, "TV_QDB_CORE_ISOLATION_APPLIED"), None);
    assert_eq!(value_of(&undone, "TV_QDB_CPUSET"), Some("0-1"));
}

#[test]
fn test_host_tuning_plan_memory_share_is_four_tenths_floored_and_capped() {
    let small = plan_questdb_host_env(8, 1, "", true).expect("mem added");
    assert_eq!(value_of(&small, "QDB_MEM_LIMIT"), Some("1g"), "floor 1g");
    let mid = plan_questdb_host_env(8, 16, "", true).expect("mem added");
    assert_eq!(value_of(&mid, "QDB_MEM_LIMIT"), Some("6g"), "16 x 4/10 = 6");
    let big = plan_questdb_host_env(8, 100, "", true).expect("mem added");
    assert_eq!(value_of(&big, "QDB_MEM_LIMIT"), Some("12g"), "cap 12g");
    // An existing limit is the deploy's, never overwritten.
    assert_eq!(
        plan_questdb_host_env(8, 16, "QDB_MEM_LIMIT=3g\n", true),
        None
    );
    // Unknown RAM plans nothing.
    assert_eq!(plan_questdb_host_env(8, 0, "", true), None);
    // Not asked to set memory: nothing on a 4+ core host.
    assert_eq!(plan_questdb_host_env(8, 16, "", false), None);
}

#[test]
fn test_host_tuning_plan_replaces_unsatisfiable_or_malformed_cpu_values() {
    for bad_cpuset in ["2,3", "3-1", "x", "0,7", ""] {
        let env = format!("TV_QDB_CPUSET={bad_cpuset}\nTV_QDB_CPUS=1.5\n");
        let planned = plan_questdb_host_env(2, 16, &env, false)
            .unwrap_or_else(|| panic!("cpuset {bad_cpuset:?} must be replaced"));
        assert_eq!(value_of(&planned, "TV_QDB_CPUSET"), Some("0-1"));
        assert_eq!(
            value_of(&planned, "TV_QDB_CPUS"),
            Some("1.5"),
            "a fitting quota stays"
        );
    }
    for bad_quota in ["abc", "0", "-1", "2.5"] {
        let env = format!("TV_QDB_CPUSET=0,1\nTV_QDB_CPUS={bad_quota}\n");
        let planned = plan_questdb_host_env(2, 16, &env, false)
            .unwrap_or_else(|| panic!("quota {bad_quota:?} must be replaced"));
        assert_eq!(
            value_of(&planned, "TV_QDB_CPUSET"),
            Some("0,1"),
            "a fitting set stays"
        );
        assert_eq!(value_of(&planned, "TV_QDB_CPUS"), Some("2.0"));
    }
    // A fully satisfiable 2-core config plans nothing.
    assert_eq!(
        plan_questdb_host_env(2, 16, "TV_QDB_CPUSET=0-1\nTV_QDB_CPUS=2\n", false),
        None
    );
}

#[test]
fn test_host_tuning_run_cli_ignores_other_args_and_refuses_unknown_actions() {
    let args = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<String>>();
    assert_eq!(
        run_cli(&args(&["tickvault"])),
        None,
        "no subcommand → normal boot"
    );
    assert_eq!(run_cli(&args(&[])), None);
    assert_eq!(run_cli(&args(&["tickvault", "something-else"])), None);
    assert_eq!(
        run_cli(&args(&["tickvault", HOST_TUNING_SUBCOMMAND, "frobnicate"])),
        Some(2),
        "an unknown action exits 2"
    );
    assert_eq!(
        run_cli(&args(&["tickvault", HOST_TUNING_SUBCOMMAND])),
        Some(2),
        "a missing action exits 2"
    );
}

#[test]
fn test_host_tuning_verify_this_host_reports_a_verdict_matching_its_count() {
    let (failures, text) = verify_this_host();
    let first = text.lines().next().unwrap_or_default();
    assert!(
        first.starts_with("now: "),
        "stamped with the caller's stamp: {first}"
    );
    if failures == 0 {
        assert!(first.contains("APPLIED — all kernel tuning verified"));
    } else {
        assert!(
            first.contains(&format!("NOT APPLIED — {failures} setting(s)")),
            "the verdict names the failure count: {first}"
        );
    }
    assert!(text.ends_with('\n'));
}

// ---------------------------------------------------------------------------
// dhan_rest_stack — the shutdown lock release gate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_dhan_rest_stack_release_lock_unsafe_gate_is_skipped_and_logged() {
    let (cap, _guard) = capture_logs();
    let open_socket = ShutdownLockGate {
        sockets_left_open: 3,
        lane_stopped_cleanly: true,
    };
    assert!(!open_socket.is_safe());
    let outcome = release_instance_lock_at_shutdown(open_socket, Duration::from_millis(50)).await;
    assert_eq!(outcome, LockReleaseAtShutdown::SkippedUnsafe);
    assert_eq!(outcome.as_str(), "skipped_unsafe");
    let logs = cap.text();
    assert!(
        logs.contains("release_at_shutdown_skipped") && logs.contains("sockets_left_open=3"),
        "the skip names why: {logs}"
    );

    let overran = ShutdownLockGate {
        sockets_left_open: 0,
        lane_stopped_cleanly: false,
    };
    assert_eq!(
        release_instance_lock_at_shutdown(overran, Duration::from_millis(50)).await,
        LockReleaseAtShutdown::SkippedUnsafe,
        "a lane that overran its budget keeps the lock too"
    );

    // Safe gate, but no heartbeat was ever registered in this process.
    let safe = ShutdownLockGate {
        sockets_left_open: 0,
        lane_stopped_cleanly: true,
    };
    assert_eq!(
        release_instance_lock_at_shutdown(safe, Duration::from_millis(50)).await,
        LockReleaseAtShutdown::NotHeld
    );
}

// ---------------------------------------------------------------------------
// dhan_universe — the daily rider supervisor
// ---------------------------------------------------------------------------

fn loopback_questdb() -> QuestDbConfig {
    QuestDbConfig {
        host: "127.0.0.1".to_owned(),
        http_port: 9,
        pg_port: 9,
        ilp_port: 9,
    }
}

#[tokio::test]
async fn test_dhan_universe_rider_disabled_returns_at_once() {
    let (cap, _guard) = capture_logs();
    let cfg = DhanUniverseConfig::default();
    assert!(!cfg.enabled);
    let handle = spawn_dhan_universe_rider(cfg, loopback_questdb(), String::new());
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("a disabled rider ends immediately")
        .expect("no panic");
    assert!(
        cap.text()
            .contains("disabled — no instrument master download"),
        "the disabled rider says so: {}",
        cap.text()
    );
}

#[tokio::test]
async fn test_dhan_universe_rider_enabled_waits_for_its_target_hour() {
    const LAST_SECOND: u32 = 86_399;
    // The pure schedule the rider follows, at fixed instants (no clock): a
    // target still ahead is a sleep of exactly the gap, a target reached is
    // "build now", and a built day sleeps to the next day's target.
    assert_eq!(
        next_wait(0, LAST_SECOND, false),
        Some(Duration::from_secs(u64::from(LAST_SECOND)))
    );
    assert_eq!(
        next_wait(LAST_SECOND - 10, LAST_SECOND, false),
        Some(Duration::from_secs(10))
    );
    assert_eq!(next_wait(LAST_SECOND, LAST_SECOND, false), None);
    assert_eq!(
        next_wait(LAST_SECOND, LAST_SECOND, true),
        Some(Duration::from_secs(1 + u64::from(LAST_SECOND))),
        "already built: sleep to tomorrow's target"
    );

    // Target the last second of the IST day so the live rider SLEEPS rather
    // than building (a build would download from the vendor). In the final
    // ten seconds of the IST day the target could fall due during the test,
    // so the live half is skipped there; the schedule above still ran.
    let now_sod = ist_secs_of_day(chrono::Utc::now().timestamp());
    let wait = next_wait(now_sod, LAST_SECOND, false);
    if now_sod >= LAST_SECOND - 10 {
        assert!(
            wait.is_none_or(|w| w <= Duration::from_secs(10)),
            "inside the last ten seconds the target is at most ten seconds away: {wait:?}"
        );
        return;
    }
    assert!(
        wait.is_some_and(|w| w > Duration::from_secs(10)),
        "the rider will sleep longer than this test runs: {wait:?}"
    );
    let cfg = DhanUniverseConfig {
        enabled: true,
        target_secs_of_day_ist: LAST_SECOND,
        ..DhanUniverseConfig::default()
    };
    let handle = spawn_dhan_universe_rider(cfg, loopback_questdb(), String::new());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !handle.is_finished(),
        "an enabled rider is a forever loop, waiting for its build hour"
    );
    handle.abort();
    let err = handle.await.expect_err("aborted");
    assert!(err.is_cancelled());
}

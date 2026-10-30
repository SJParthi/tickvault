//! Coverage for `tickvault_app::disk_pressure_boot` — the supervised
//! disk-pressure loop body (`run_disk_pressure_loop`, reached only through
//! `spawn_supervised_disk_pressure_loop`) plus the pure helpers.
//!
//! The loop polls once a minute; every test runs it on a paused tokio clock
//! so an hour of polls costs milliseconds. The probe runs on the blocking
//! pool (auto-advance is held while it runs), against a private temp folder.
//! No archive bucket can be resolved (`archive_bucket` empty and an invalid
//! `TV_ENVIRONMENT`), so a pressure pass never reaches S3 or QuestDB: it
//! reports "not run", which is exactly the branch the loop must survive.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::Duration;

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use tickvault_app::disk_pressure_boot::{
    DISK_PRESSURE_POLL_INTERVAL_SECS, PRESSURE_PROBE_FAILING_POLLS, PressurePass,
    classify_pressure_pass, fold_pressure_pass, pressure_config, pressure_pass_due,
    spawn_supervised_disk_pressure_loop,
};
use tickvault_common::config::{PartitionRetentionConfig, QuestDbConfig};
use tickvault_storage::disk_pressure::{PressureAction, PressureState};
use tickvault_storage::partition_archive::ArchiveRunSummary;

/// One process-wide Prometheus recorder, installed before any loop runs so
/// the loop's counters and gauges land somewhere the tests can read.
fn recorder() -> &'static PrometheusHandle {
    static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
    HANDLE.get_or_init(|| {
        PrometheusBuilder::new()
            .install_recorder()
            .expect("install the test Prometheus recorder")
    })
}

/// Makes the archive bucket unresolvable for the whole test process: an
/// environment name with invalid characters resolves to `None` (fail-closed),
/// so no pressure pass can build an archiver or contact S3.
fn no_archive_bucket() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: set once, before any loop in this binary reads it.
        unsafe {
            std::env::set_var("TV_ENVIRONMENT", "not a valid env!");
        }
    });
}

/// A shared byte buffer the test's tracing subscriber writes into.
#[derive(Clone, Default)]
struct LogBuf(Arc<Mutex<Vec<u8>>>);

impl Write for LogBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl LogBuf {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("log buffer")).into_owned()
    }
}

fn capture_logs() -> (LogBuf, tracing::subscriber::DefaultGuard) {
    let buf = LogBuf::default();
    let writer = buf.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (buf, guard)
}

fn unique_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    std::env::temp_dir().join(format!(
        "tv-cov-disk-pressure-{tag}-{}-{nanos}",
        std::process::id()
    ))
}

fn questdb() -> QuestDbConfig {
    // Never contacted: no archive bucket resolves, so no pass is built.
    QuestDbConfig {
        host: "127.0.0.1".to_string(),
        http_port: 1,
        pg_port: 1,
        ilp_port: 1,
    }
}

fn cfg(high: u8, low: u8) -> PartitionRetentionConfig {
    PartitionRetentionConfig {
        pressure_archive_enabled: true,
        pressure_high_water_pct: high,
        pressure_low_water_pct: low,
        archive_bucket: String::new(),
        ..PartitionRetentionConfig::default()
    }
}

/// The data folder's fill as the loop computes it.
fn volume_used_pct(dir: &std::path::Path) -> u8 {
    match tickvault_storage::disk_health_watcher::probe_disk_free_bytes(dir) {
        tickvault_storage::disk_health_watcher::DiskHealthOutcome::Ok {
            free_bytes,
            total_bytes,
        } => tickvault_storage::disk_pressure::used_pct_from(free_bytes, total_bytes)
            .expect("a readable volume"),
        other => panic!("probe failed: {other:?}"),
    }
}

/// The value of an unlabelled counter line in a Prometheus render (0 when
/// absent).
fn counter_value(rendered: &str, name: &str) -> u64 {
    rendered
        .lines()
        .find_map(|l| l.strip_prefix(name)?.strip_prefix(' '))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

async fn run_polls(polls: u64) {
    tokio::time::sleep(Duration::from_secs(
        DISK_PRESSURE_POLL_INTERVAL_SECS * polls + 1,
    ))
    .await;
}

#[test]
fn test_disk_pressure_boot_spawn_disabled_returns_none() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let mut c = cfg(90, 70);
        c.pressure_archive_enabled = false;
        assert!(spawn_supervised_disk_pressure_loop(unique_dir("off"), questdb(), c).is_none());
    });
}

#[test]
fn test_disk_pressure_boot_spawn_insane_thresholds_returns_none() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        // low not strictly below high, high of 0, high above 100: all refused.
        for (high, low) in [(70_u8, 70_u8), (0, 0), (101, 50), (50, 60)] {
            assert!(
                spawn_supervised_disk_pressure_loop(unique_dir("bad"), questdb(), cfg(high, low))
                    .is_none(),
                "thresholds high={high} low={low} must be refused"
            );
        }
    });
}

/// High water at 100%: a real volume is below it, so every poll decides
/// `Idle`, publishes the used-percent gauge and keeps the loop alive.
#[tokio::test(start_paused = true)]
async fn test_disk_pressure_boot_loop_idle_volume_publishes_used_gauge_and_keeps_running() {
    let handle_metrics = recorder();
    no_archive_bucket();
    let dir = unique_dir("idle");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let (logs, _guard) = capture_logs();

    let handle = spawn_supervised_disk_pressure_loop(dir.clone(), questdb(), cfg(100, 99))
        .expect("a sane, enabled config spawns the loop");
    run_polls(3).await;
    assert!(
        !handle.is_finished(),
        "the supervised loop never ends on its own"
    );
    handle.abort();

    let text = logs.text();
    assert!(
        text.contains("disk-pressure archival ARMED"),
        "logs: {text}"
    );
    // The decision follows the measured fill: a volume reading below 100%
    // never starts an episode at a 100% high water; a volume reading 100%
    // (the shared build disk can be that full) does.
    let used = volume_used_pct(&dir);
    // At 99% the fill may have crossed during the polls; only the clear
    // cases are asserted.
    let started = text.contains("episode STARTED");
    if used < 99 {
        assert!(!started, "used_pct={used}: {text}");
    } else if used >= 100 {
        assert!(started, "used_pct={used}: {text}");
    }
    let rendered = handle_metrics.render();
    assert!(
        rendered.contains("tv_data_disk_used_pct"),
        "the used-percent gauge is published every readable poll: {rendered}"
    );
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

/// High water at 1%: the real volume is above it. With no archive bucket the
/// pass never runs (`NotRun`), so the episode spends its passes, escalates to
/// STORAGE-GAP-05, holds, and re-arms an hour later.
#[tokio::test(start_paused = true)]
async fn test_disk_pressure_boot_loop_over_high_water_starts_episode_then_escalates() {
    let handle_metrics = recorder();
    no_archive_bucket();
    let dir = unique_dir("episode");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let (logs, _guard) = capture_logs();

    let handle = spawn_supervised_disk_pressure_loop(dir.clone(), questdb(), cfg(1, 0))
        .expect("a sane, enabled config spawns the loop");
    // Start + 3 continue passes, then escalation, then holds past the
    // one-hour re-arm and one more re-armed pass.
    run_polls(70).await;
    assert!(!handle.is_finished());
    handle.abort();

    // The pass arm asked the spill/WAL reclaim sweep to run now.
    assert!(
        tickvault_app::reclaim_signal::wait_for_reclaim_or(Duration::from_millis(1)).await,
        "an episode start must request a reclaim sweep"
    );

    let text = logs.text();
    assert!(
        text.contains("disk-pressure episode STARTED"),
        "logs: {text}"
    );
    assert!(
        text.contains("no archive bucket resolvable"),
        "with no bucket the pass is skipped, never run: {text}"
    );
    assert!(
        text.contains("disk pressure could NOT be relieved"),
        "four not-run passes escalate: {text}"
    );
    let rendered = handle_metrics.render();
    // This loop alone: one episode; four passes then escalation; one
    // re-armed pass after the hour. The counters are process-wide and the
    // other loops here may also cross high water on a full disk, so these
    // are lower bounds.
    for (name, at_least) in [
        ("tv_disk_pressure_episodes_total", 1),
        ("tv_disk_pressure_passes_total", 5),
        ("tv_disk_pressure_unrelievable_total", 1),
    ] {
        let got = counter_value(&rendered, name);
        assert!(
            got >= at_least,
            "{name}={got}, want >= {at_least}: {rendered}"
        );
    }
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

/// A data folder that does not exist: every probe fails, nothing is acted
/// on, and after `PRESSURE_PROBE_FAILING_POLLS` polls the blind probe is
/// itself reported. Creating the folder then reports the recovery.
#[tokio::test(start_paused = true)]
async fn test_disk_pressure_boot_loop_blind_probe_reports_sustained_failure_then_recovery() {
    let handle_metrics = recorder();
    no_archive_bucket();
    let dir = unique_dir("blind");
    assert!(!dir.exists());
    let (logs, _guard) = capture_logs();

    let handle =
        spawn_supervised_disk_pressure_loop(dir.clone(), questdb(), cfg(100, 99)).expect("spawned");
    run_polls(u64::from(PRESSURE_PROBE_FAILING_POLLS) + 1).await;
    let blind = logs.text();
    assert!(
        blind.contains("disk-pressure probe failed"),
        "each blind poll warns: {blind}"
    );
    assert!(
        blind.contains("has FAILED on every poll"),
        "a sustained blind probe is reported once: {blind}"
    );
    assert!(
        !blind.contains("episode STARTED"),
        "a blind probe must never start an episode: {blind}"
    );

    std::fs::create_dir_all(&dir).expect("create the data folder");
    run_polls(2).await;
    handle.abort();
    let text = logs.text();
    assert!(
        text.contains("reading the volume again"),
        "the recovery is reported: {text}"
    );
    assert!(
        handle_metrics
            .render()
            .contains("tv_disk_pressure_probe_failed_total")
    );
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

#[test]
fn test_disk_pressure_boot_classify_pressure_pass_every_shape() {
    let not_run = ArchiveRunSummary::default();
    assert_eq!(classify_pressure_pass(&not_run), PressurePass::NotRun);
    let ran_zero = ArchiveRunSummary {
        pass_ran: true,
        ..ArchiveRunSummary::default()
    };
    assert_eq!(classify_pressure_pass(&ran_zero), PressurePass::Dropped(0));
    let ran_max = ArchiveRunSummary {
        pass_ran: true,
        dropped: u32::MAX,
        ..ArchiveRunSummary::default()
    };
    assert_eq!(
        classify_pressure_pass(&ran_max),
        PressurePass::Dropped(u64::from(u32::MAX))
    );
    let held = ArchiveRunSummary {
        pass_ran: true,
        held_deferred_depth: 1,
        ..ArchiveRunSummary::default()
    };
    assert_eq!(classify_pressure_pass(&held), PressurePass::StoodAside);
}

#[test]
fn test_disk_pressure_boot_fold_pressure_pass_not_run_and_stood_aside_continue() {
    let c = cfg(90, 70);
    let mut state = PressureState::default();
    fold_pressure_pass(
        &mut state,
        PressureAction::StartEpisode,
        0,
        PressurePass::NotRun,
    );
    let ep = state.episode.expect("episode opened");
    assert_eq!(ep.passes_used, 1);
    assert_eq!(ep.last_pass_dropped, None);
    // A stood-aside ContinuePass changes nothing.
    let before = state;
    fold_pressure_pass(
        &mut state,
        PressureAction::ContinuePass,
        60,
        PressurePass::StoodAside,
    );
    assert_eq!(state, before);
    // A stood-aside StartEpisode opens the episode but counts no pass.
    let mut fresh = PressureState::default();
    fold_pressure_pass(
        &mut fresh,
        PressureAction::StartEpisode,
        0,
        PressurePass::StoodAside,
    );
    assert_eq!(fresh.episode.map(|e| e.passes_used), Some(0));
    // A dropped pass records its count.
    fold_pressure_pass(
        &mut fresh,
        PressureAction::ContinuePass,
        60,
        PressurePass::Dropped(7),
    );
    assert_eq!(fresh.episode.and_then(|e| e.last_pass_dropped), Some(7));
    // pressure_pass_due: only ContinuePass after a stand-aside with a busy
    // gate is held.
    assert!(!pressure_pass_due(PressureAction::ContinuePass, true, true));
    assert!(pressure_pass_due(PressureAction::Escalate, true, true));
    // pressure_config only ever shortens.
    let p = pressure_config(&c);
    assert!(p.under_disk_pressure);
    assert!(p.market_data_hot_days <= c.market_data_hot_days);
    assert!(p.depth_hot_days <= c.depth_hot_days);
    assert!(p.intraday_hot_days <= c.intraday_hot_days);
    assert_eq!(p.retention_days, c.retention_days);
}

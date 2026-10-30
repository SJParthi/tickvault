//! Coverage for `tickvault_app::dhan_live_universe` — the boot-time resolve of
//! the live subscription set, the running-session widen, the degraded-universe
//! heartbeat and the boot wait for the daily mapping artifact.
//!
//! The artifacts live at a RELATIVE path (`data/instrument-cache/...`), so
//! every test here runs with the process working directory moved into its own
//! fresh temp folder and moved back afterwards. One async mutex serialises
//! every test in this binary: the working directory and the module's degraded
//! / widen-pending atomics are process-wide.
//!
//! Each test asserts the outcome the operator would see: the instruments
//! returned, the counter label that moved (read from an in-process
//! Prometheus recorder), the degraded reason, and the coded log line.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use tickvault_app::dhan_live_universe::{
    MAPPING_WAIT_COUNTER, MASTER_SOURCING_FALLBACK_COUNTER, NARROWED_SETTLE_COUNTER,
    NARROWING_FALLBACK_COUNTER, TodaysUniverseSource, await_mapping_artifact,
    finish_live_universe_widen, live_universe_degraded_reason, live_universe_widen_pending,
    mapping_wait_end_ist_secs, read_todays_live_universe, record_widen_list_overdue,
    report_spots_and_contracts_headroom, resolve_live_universe, run_degraded_universe_heartbeat,
};
use tickvault_app::dhan_universe::{
    fno_underlying_artifact_path, mapping_artifact_path, ntm_spot_artifact_path,
};
use tickvault_common::config::DhanUniverseConfig;
use tickvault_common::constants::SPOT_1M_REST_INDICES;
use tickvault_common::types::ExchangeSegment;
use tickvault_core::websocket::pool_supervisor::SubscribeInstrument;

const TODAY: &str = "2026-10-20";
const YESTERDAY: &str = "2026-10-19";

// ---------------------------------------------------------------------------
// Harness: serialised cwd, metrics recorder, log capture
// ---------------------------------------------------------------------------

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn metrics_handle() -> &'static PrometheusHandle {
    static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
    HANDLE.get_or_init(|| {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::set_global_recorder(recorder).expect("first recorder in this binary");
        handle
    })
}

/// The value of one series in the Prometheus render, 0 when absent.
fn series_value(name: &str, label: Option<(&str, &str)>) -> f64 {
    let rendered = metrics_handle().render();
    let prefix = match label {
        Some((k, v)) => format!("{name}{{{k}=\"{v}\"}} "),
        None => format!("{name} "),
    };
    rendered
        .lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(0.0)
}

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

/// A fresh working directory for one test; restores the old one on drop.
struct Sandbox {
    root: std::path::PathBuf,
    previous: std::path::PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "tv-cov-live-universe-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("data/instrument-cache")).expect("sandbox");
        let previous = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&root).expect("enter sandbox");
        Self { root, previous }
    }

    fn write(&self, rel: &std::path::Path, body: &str) {
        std::fs::write(self.root.join(rel), body).expect("write artifact");
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.previous);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Resets the module's process-wide degraded / pending state to "healthy".
fn reset_widen_state() {
    finish_live_universe_widen(false, 0);
}

fn index_universe() -> Vec<SubscribeInstrument> {
    SPOT_1M_REST_INDICES
        .iter()
        .map(|(id, _)| SubscribeInstrument {
            security_id: *id,
            segment: ExchangeSegment::IdxI,
        })
        .collect()
}

/// A mapping artifact body from `(security_id, segment byte, symbol)` rows.
fn artifact(rows: &[(u64, u8, &str)]) -> String {
    let mappings: Vec<serde_json::Value> = rows
        .iter()
        .map(|(id, seg, sym)| {
            serde_json::json!({"security_id": id, "exchange_segment": seg, "symbol": sym})
        })
        .collect();
    serde_json::json!({ "mappings": mappings }).to_string()
}

fn stocks(n: u64) -> Vec<(u64, u8, &'static str)> {
    (0..n).map(|i| (100_000 + i, 1u8, "STOCK")).collect()
}

fn master_cfg() -> DhanUniverseConfig {
    DhanUniverseConfig {
        enabled: true,
        live_subscription_from_master: true,
        ..DhanUniverseConfig::default()
    }
}

// ---------------------------------------------------------------------------
// resolve_live_universe
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_live_universe_resolve_master_off_returns_the_index_set_unchanged() {
    let _serial = SERIAL.lock().await;
    let _sb = Sandbox::new("off");
    let cfg = DhanUniverseConfig::default();
    let out = resolve_live_universe(&cfg, index_universe(), TODAY, 25_000, false);
    assert_eq!(out, index_universe());
}

#[tokio::test]
async fn test_live_universe_resolve_today_full_mapping_widens_and_publishes_size() {
    let _serial = SERIAL.lock().await;
    let sb = Sandbox::new("full");
    reset_widen_state();
    metrics_handle();
    sb.write(&mapping_artifact_path(TODAY), &artifact(&stocks(3)));
    let fallback_before = series_value(
        MASTER_SOURCING_FALLBACK_COUNTER,
        Some(("reason", "artifact_unreadable")),
    );
    let (cap, _g) = capture_logs();

    let out = resolve_live_universe(&master_cfg(), index_universe(), TODAY, 25_000, false);
    assert_eq!(out.len(), 7, "4 indices + 3 stocks: {out:?}");
    assert!(out[..4].iter().all(|i| i.segment == ExchangeSegment::IdxI));
    assert!(
        out[4..]
            .iter()
            .all(|i| i.segment == ExchangeSegment::NseEquity)
    );
    assert_eq!(
        series_value("tv_dhan_live_universe_instruments", None),
        7.0,
        "the success path publishes the subscribed size"
    );
    assert_eq!(
        series_value(
            MASTER_SOURCING_FALLBACK_COUNTER,
            Some(("reason", "artifact_unreadable"))
        ),
        fallback_before,
        "a healthy widen moves no fallback counter"
    );
    assert!(!live_universe_widen_pending());
    assert_eq!(live_universe_degraded_reason(), None);
    let logs = cap.text();
    assert!(logs.contains("widened from the resolved master"), "{logs}");
    assert!(logs.contains("spot_universe=\"full_master\""), "{logs}");
}

#[tokio::test]
async fn test_live_universe_resolve_ntm_artifact_wins_and_is_labelled_ntm() {
    let _serial = SERIAL.lock().await;
    let sb = Sandbox::new("ntm");
    reset_widen_state();
    sb.write(&ntm_spot_artifact_path(TODAY), &artifact(&stocks(2)));
    sb.write(&fno_underlying_artifact_path(TODAY), &artifact(&stocks(5)));
    sb.write(&mapping_artifact_path(TODAY), &artifact(&stocks(9)));
    let cfg = DhanUniverseConfig {
        spot_universe_ntm_only: true,
        spot_universe_fno_underlyings_only: true,
        ..master_cfg()
    };
    let (cap, _g) = capture_logs();
    let out = resolve_live_universe(&cfg, index_universe(), TODAY, 25_000, false);
    assert_eq!(out.len(), 6, "NTM (2 rows) wins over F&O and the full map");
    assert!(
        cap.text().contains("spot_universe=\"ntm\""),
        "{}",
        cap.text()
    );
}

#[tokio::test]
async fn test_live_universe_resolve_bad_ntm_and_missing_fno_fall_through_counted() {
    let _serial = SERIAL.lock().await;
    let sb = Sandbox::new("narrow-fail");
    reset_widen_state();
    metrics_handle();
    sb.write(&ntm_spot_artifact_path(TODAY), "{ not json");
    sb.write(&mapping_artifact_path(TODAY), &artifact(&stocks(4)));
    let cfg = DhanUniverseConfig {
        spot_universe_ntm_only: true,
        spot_universe_fno_underlyings_only: true,
        ..master_cfg()
    };
    let ntm_before = series_value(
        NARROWING_FALLBACK_COUNTER,
        Some(("reason", "ntm_artifact_unparseable")),
    );
    let fno_before = series_value(
        NARROWING_FALLBACK_COUNTER,
        Some(("reason", "fno_artifact_unreadable")),
    );
    let (cap, _g) = capture_logs();
    let out = resolve_live_universe(&cfg, index_universe(), TODAY, 25_000, false);
    assert_eq!(out.len(), 8, "falls through to the full map (4 + 4)");
    assert_eq!(
        series_value(
            NARROWING_FALLBACK_COUNTER,
            Some(("reason", "ntm_artifact_unparseable"))
        ),
        ntm_before + 1.0
    );
    assert_eq!(
        series_value(
            NARROWING_FALLBACK_COUNTER,
            Some(("reason", "fno_artifact_unreadable"))
        ),
        fno_before + 1.0
    );
    let logs = cap.text();
    assert!(logs.contains("NTM spot set"), "{logs}");
    assert!(logs.contains("narrowed spot set"), "{logs}");
    assert!(logs.contains("spot_universe=\"full_master\""), "{logs}");
}

#[tokio::test]
async fn test_live_universe_resolve_earlier_day_list_on_unexpected_boot_is_paged() {
    let _serial = SERIAL.lock().await;
    let sb = Sandbox::new("earlier-paged");
    reset_widen_state();
    metrics_handle();
    sb.write(&mapping_artifact_path(YESTERDAY), &artifact(&stocks(3)));
    let before = series_value(
        MASTER_SOURCING_FALLBACK_COUNTER,
        Some(("reason", "artifact_unreadable")),
    );
    let (cap, _g) = capture_logs();
    let out = resolve_live_universe(&master_cfg(), index_universe(), TODAY, 25_000, false);
    assert_eq!(
        out.len(),
        7,
        "yesterday's 3 stocks, not the 4 indices alone"
    );
    assert_eq!(
        series_value(
            MASTER_SOURCING_FALLBACK_COUNTER,
            Some(("reason", "artifact_unreadable"))
        ),
        before + 1.0
    );
    assert_eq!(live_universe_degraded_reason(), Some("artifact_unreadable"));
    assert!(
        live_universe_widen_pending(),
        "the running session must widen"
    );
    let logs = cap.text();
    assert!(logs.contains("source=\"earlier_day_artifact\""), "{logs}");
    assert!(logs.contains("artifact_date=2026-10-19"), "{logs}");
    reset_widen_state();
}

#[tokio::test]
async fn test_live_universe_resolve_earlier_day_list_on_expected_boot_is_not_paged() {
    let _serial = SERIAL.lock().await;
    let sb = Sandbox::new("earlier-expected");
    reset_widen_state();
    metrics_handle();
    // An NTM file from two days back: the lookback honours the precedence.
    let cfg = DhanUniverseConfig {
        spot_universe_ntm_only: true,
        ..master_cfg()
    };
    sb.write(&ntm_spot_artifact_path("2026-10-18"), &artifact(&stocks(2)));
    // Yesterday's file exists but is EMPTY: skipped, never returned.
    sb.write(&mapping_artifact_path(YESTERDAY), &artifact(&[]));
    let before = series_value(
        MASTER_SOURCING_FALLBACK_COUNTER,
        Some(("reason", "artifact_unreadable")),
    );
    let (cap, _g) = capture_logs();
    let out = resolve_live_universe(&cfg, index_universe(), TODAY, 25_000, true);
    assert_eq!(out.len(), 6);
    assert_eq!(
        series_value(
            MASTER_SOURCING_FALLBACK_COUNTER,
            Some(("reason", "artifact_unreadable"))
        ),
        before,
        "an expected pre-rider boot moves no paged counter"
    );
    assert_eq!(live_universe_degraded_reason(), None);
    assert!(live_universe_widen_pending());
    let logs = cap.text();
    assert!(logs.contains("source=\"pre_rider_boot\""), "{logs}");
    assert!(logs.contains("spot_universe=\"ntm\""), "{logs}");
    reset_widen_state();
}

#[tokio::test]
async fn test_live_universe_resolve_nothing_on_disk_expected_returns_indices_quietly() {
    let _serial = SERIAL.lock().await;
    let _sb = Sandbox::new("none-expected");
    reset_widen_state();
    metrics_handle();
    let (cap, _g) = capture_logs();
    let out = resolve_live_universe(&master_cfg(), index_universe(), TODAY, 25_000, true);
    assert_eq!(out, index_universe());
    assert_eq!(series_value("tv_dhan_live_universe_instruments", None), 4.0);
    assert_eq!(live_universe_degraded_reason(), None);
    assert!(live_universe_widen_pending());
    let logs = cap.text();
    assert!(logs.contains("source=\"pre_rider_boot\""), "{logs}");
    assert!(!logs.contains("fell_back_to_indices"), "{logs}");
    reset_widen_state();
}

#[tokio::test]
async fn test_live_universe_resolve_nothing_on_disk_unexpected_is_the_paged_collapse() {
    let _serial = SERIAL.lock().await;
    let _sb = Sandbox::new("none-paged");
    reset_widen_state();
    metrics_handle();
    let before = series_value(
        MASTER_SOURCING_FALLBACK_COUNTER,
        Some(("reason", "artifact_unreadable")),
    );
    let (cap, _g) = capture_logs();
    let out = resolve_live_universe(&master_cfg(), index_universe(), TODAY, 25_000, false);
    assert_eq!(out, index_universe());
    assert_eq!(
        series_value(
            MASTER_SOURCING_FALLBACK_COUNTER,
            Some(("reason", "artifact_unreadable"))
        ),
        before + 1.0
    );
    assert_eq!(live_universe_degraded_reason(), Some("artifact_unreadable"));
    let logs = cap.text();
    assert!(
        logs.contains("source=\"fell_back_to_indices\""),
        "the collapse line carries the alarm's source field: {logs}"
    );
    reset_widen_state();
}

#[tokio::test]
async fn test_live_universe_resolve_unparseable_mapping_is_its_own_reason() {
    let _serial = SERIAL.lock().await;
    let sb = Sandbox::new("garbage");
    reset_widen_state();
    metrics_handle();
    sb.write(&mapping_artifact_path(TODAY), r#"{"no_mappings_key": []}"#);
    let before = series_value(
        MASTER_SOURCING_FALLBACK_COUNTER,
        Some(("reason", "artifact_unparseable")),
    );
    let out = resolve_live_universe(&master_cfg(), index_universe(), TODAY, 25_000, false);
    assert_eq!(out, index_universe());
    assert_eq!(
        series_value(
            MASTER_SOURCING_FALLBACK_COUNTER,
            Some(("reason", "artifact_unparseable"))
        ),
        before + 1.0
    );
    assert_eq!(
        live_universe_degraded_reason(),
        Some("artifact_unparseable")
    );
    reset_widen_state();
}

#[tokio::test]
async fn test_live_universe_resolve_over_capacity_fills_by_priority_and_pages() {
    let _serial = SERIAL.lock().await;
    let sb = Sandbox::new("truncate");
    reset_widen_state();
    metrics_handle();
    sb.write(&mapping_artifact_path(TODAY), &artifact(&stocks(3)));
    let before = series_value(
        MASTER_SOURCING_FALLBACK_COUNTER,
        Some(("reason", "truncated_to_capacity")),
    );
    let (cap, _g) = capture_logs();
    let out = resolve_live_universe(&master_cfg(), index_universe(), TODAY, 5, false);
    assert_eq!(out.len(), 5, "filled to capacity, never collapsed");
    assert!(
        out[..4].iter().all(|i| i.segment == ExchangeSegment::IdxI),
        "indices fill first"
    );
    assert_eq!(out[4].security_id, 100_000, "then by (segment, id)");
    assert_eq!(
        series_value(
            MASTER_SOURCING_FALLBACK_COUNTER,
            Some(("reason", "truncated_to_capacity"))
        ),
        before + 1.0
    );
    assert_eq!(
        live_universe_degraded_reason(),
        Some("truncated_to_capacity")
    );
    let logs = cap.text();
    assert!(logs.contains("refused_over_capacity=2"), "{logs}");
    reset_widen_state();
}

#[tokio::test]
async fn test_live_universe_resolve_master_with_nothing_usable_is_not_a_widen() {
    let _serial = SERIAL.lock().await;
    let sb = Sandbox::new("unusable");
    reset_widen_state();
    metrics_handle();
    // id 0 and an unknown segment byte (6 is Dhan's gap): nothing usable.
    sb.write(
        &mapping_artifact_path(TODAY),
        &artifact(&[(0, 1, "ZERO"), (777, 6, "GAP")]),
    );
    let before = series_value(
        MASTER_SOURCING_FALLBACK_COUNTER,
        Some(("reason", "no_usable_widening")),
    );
    let (cap, _g) = capture_logs();
    let out = resolve_live_universe(&master_cfg(), index_universe(), TODAY, 25_000, false);
    assert_eq!(out, index_universe(), "nothing usable was added");
    assert_eq!(
        series_value(
            MASTER_SOURCING_FALLBACK_COUNTER,
            Some(("reason", "no_usable_widening"))
        ),
        before + 1.0
    );
    let logs = cap.text();
    assert!(logs.contains("refused_zero_id=1"), "{logs}");
    assert!(logs.contains("refused_unknown_segment=1"), "{logs}");
    assert!(logs.contains("deduped=0"), "{logs}");
    reset_widen_state();
}

#[tokio::test]
async fn test_live_universe_resolve_index_rows_missing_a_seed_are_named() {
    let _serial = SERIAL.lock().await;
    let sb = Sandbox::new("seed-missing");
    reset_widen_state();
    // The master carries NIFTY and an extra index but not BANKNIFTY or
    // INDIA VIX (by id or name). SENSEX is BSE and dropped quietly.
    let mut rows = vec![(13u64, 0u8, "NIFTY"), (999, 0, "NIFTY NEXT 50")];
    rows.extend(stocks(2));
    sb.write(&mapping_artifact_path(TODAY), &artifact(&rows));
    let (cap, _g) = capture_logs();
    let out = resolve_live_universe(&master_cfg(), index_universe(), TODAY, 25_000, false);
    let ids: Vec<u64> = out.iter().map(|i| i.security_id).collect();
    assert!(ids.contains(&25), "BANKNIFTY kept on its fixed id: {ids:?}");
    assert!(ids.contains(&999), "the master's own index is subscribed");
    assert!(!ids.contains(&51), "the BSE seed is dropped by the swap");
    let logs = cap.text();
    assert!(
        logs.contains("source=\"index_seed_missing_from_master\""),
        "{logs}"
    );
    assert!(logs.contains("BANKNIFTY"), "{logs}");
}

#[tokio::test]
async fn test_live_universe_resolve_low_headroom_at_boot_is_an_error_line() {
    let _serial = SERIAL.lock().await;
    let sb = Sandbox::new("headroom");
    reset_widen_state();
    sb.write(&mapping_artifact_path(TODAY), &artifact(&stocks(15)));
    let (cap, _g) = capture_logs();
    // 19 of 20: headroom 1 < 20/10 = 2.
    let out = resolve_live_universe(&master_cfg(), index_universe(), TODAY, 20, false);
    assert_eq!(out.len(), 19);
    let logs = cap.text();
    assert!(logs.contains("source=\"universe_headroom_low\""), "{logs}");
    assert!(logs.contains("stage=\"spots_at_boot\""), "{logs}");
    assert!(
        logs.contains("MAX_DAILY_UNIVERSE_SIZE disagree"),
        "a capacity other than the documented max is named: {logs}"
    );
}

// ---------------------------------------------------------------------------
// The running-session widen
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_live_universe_read_todays_list_present_missing_and_unusable() {
    let _serial = SERIAL.lock().await;
    let sb = Sandbox::new("todays");
    let source = TodaysUniverseSource {
        cfg: master_cfg(),
        index_universe: index_universe(),
        capacity: 25_000,
    };
    assert!(
        read_todays_live_universe(&source, TODAY).is_none(),
        "no file yet"
    );
    sb.write(&mapping_artifact_path(TODAY), &artifact(&stocks(2)));
    let (sel, kind) = read_todays_live_universe(&source, TODAY).expect("today's list");
    assert_eq!(kind, "full_master");
    assert_eq!(sel.instruments.len(), 6);

    // Over capacity still counts as today's list (filled by priority).
    let tight = TodaysUniverseSource {
        capacity: 5,
        ..source.clone()
    };
    let (sel, _) = read_todays_live_universe(&tight, TODAY).expect("truncated list");
    assert_eq!(sel.instruments.len(), 5);

    // A list that adds nothing is not today's list.
    sb.write(&mapping_artifact_path(YESTERDAY), &artifact(&[(0, 1, "Z")]));
    assert!(read_todays_live_universe(&source, YESTERDAY).is_none());
}

#[tokio::test]
async fn test_live_universe_finish_widen_truncated_pages_and_clean_clears() {
    let _serial = SERIAL.lock().await;
    reset_widen_state();
    metrics_handle();
    let before = series_value(
        MASTER_SOURCING_FALLBACK_COUNTER,
        Some(("reason", "truncated_to_capacity")),
    );
    finish_live_universe_widen(true, 25_000);
    assert!(!live_universe_widen_pending());
    assert_eq!(
        live_universe_degraded_reason(),
        Some("truncated_to_capacity")
    );
    assert_eq!(
        series_value(
            MASTER_SOURCING_FALLBACK_COUNTER,
            Some(("reason", "truncated_to_capacity"))
        ),
        before + 1.0
    );
    assert_eq!(
        series_value("tv_dhan_live_universe_instruments", None),
        25_000.0
    );

    finish_live_universe_widen(false, 870);
    assert_eq!(live_universe_degraded_reason(), None);
    assert_eq!(
        series_value("tv_dhan_live_universe_instruments", None),
        870.0
    );
}

#[tokio::test]
async fn test_live_universe_record_widen_list_overdue_pages_only_a_healthy_session() {
    let _serial = SERIAL.lock().await;
    reset_widen_state();
    metrics_handle();
    let before = series_value(
        MASTER_SOURCING_FALLBACK_COUNTER,
        Some(("reason", "artifact_unreadable")),
    );
    record_widen_list_overdue(4);
    assert_eq!(live_universe_degraded_reason(), Some("artifact_unreadable"));
    assert_eq!(
        series_value(
            MASTER_SOURCING_FALLBACK_COUNTER,
            Some(("reason", "artifact_unreadable"))
        ),
        before + 1.0
    );
    // Already degraded: a second call does not page again.
    record_widen_list_overdue(4);
    assert_eq!(
        series_value(
            MASTER_SOURCING_FALLBACK_COUNTER,
            Some(("reason", "artifact_unreadable"))
        ),
        before + 1.0
    );
    reset_widen_state();
}

#[tokio::test]
async fn test_live_universe_report_spots_and_contracts_headroom_warns_when_tight() {
    let _serial = SERIAL.lock().await;
    // Spots = 0 after the reset; 24,000 contracts of 25,000 leaves 1,000 < 2,500.
    reset_widen_state();
    let (cap, _g) = capture_logs();
    report_spots_and_contracts_headroom(24_000, 25_000);
    let logs = cap.text();
    assert!(logs.contains("stage=\"spots_and_contracts\""), "{logs}");
    assert!(logs.contains("source=\"universe_headroom_low\""), "{logs}");
    assert!(!logs.contains("disagree"), "25,000 is the documented max");

    let (cap, _g2) = capture_logs();
    report_spots_and_contracts_headroom(10, 25_000);
    let logs = cap.text();
    assert!(
        logs.contains("live universe headroom"),
        "roomy is info: {logs}"
    );
    assert!(!logs.contains("universe_headroom_low"), "{logs}");
}

#[tokio::test(start_paused = true)]
async fn test_live_universe_degraded_heartbeat_recounts_follows_reason_and_stops() {
    let _serial = SERIAL.lock().await;
    reset_widen_state();
    metrics_handle();
    record_widen_list_overdue(4); // degraded: artifact_unreadable
    let (cap, _g) = capture_logs();
    let unreadable = || {
        series_value(
            MASTER_SOURCING_FALLBACK_COUNTER,
            Some(("reason", "artifact_unreadable")),
        )
    };
    let start = unreadable();
    let heartbeat = tokio::spawn(run_degraded_universe_heartbeat("artifact_unreadable"));
    tokio::time::sleep(Duration::from_secs(61)).await;
    assert!(unreadable() >= start + 1.0, "one re-count per minute");

    // The reason changes while running: the heartbeat follows it.
    finish_live_universe_widen(true, 25_000);
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert!(
        cap.text().contains("degraded reason changed while running"),
        "{}",
        cap.text()
    );

    // Healthy again: the heartbeat returns at its next tick.
    finish_live_universe_widen(false, 870);
    tokio::time::sleep(Duration::from_secs(61)).await;
    let ended = tokio::time::timeout(Duration::from_secs(120), heartbeat).await;
    assert!(
        ended.is_ok(),
        "the heartbeat stops once the session is healthy"
    );
}

// ---------------------------------------------------------------------------
// await_mapping_artifact
// ---------------------------------------------------------------------------

fn wait_outcome(label: &str) -> f64 {
    series_value(MAPPING_WAIT_COUNTER, Some(("outcome", label)))
}

fn settle_outcome(label: &str) -> f64 {
    series_value(NARROWED_SETTLE_COUNTER, Some(("outcome", label)))
}

#[tokio::test]
async fn test_live_universe_await_mapping_not_requested_and_rider_disabled() {
    let _serial = SERIAL.lock().await;
    let _sb = Sandbox::new("await-gates");
    metrics_handle();
    let before = wait_outcome("not_requested");
    await_mapping_artifact(&DhanUniverseConfig::default(), TODAY, false, true).await;
    assert_eq!(wait_outcome("not_requested"), before + 1.0);

    let disabled = DhanUniverseConfig {
        enabled: false,
        ..master_cfg()
    };
    let before = wait_outcome("rider_disabled");
    let (cap, _g) = capture_logs();
    await_mapping_artifact(&disabled, TODAY, false, true).await;
    assert_eq!(wait_outcome("rider_disabled"), before + 1.0);
    assert!(cap.text().contains("the two flags"), "{}", cap.text());

    let before = wait_outcome("non_trading_day");
    await_mapping_artifact(&master_cfg(), TODAY, true, false).await;
    assert_eq!(wait_outcome("non_trading_day"), before + 1.0);
}

#[tokio::test]
async fn test_live_universe_await_mapping_present_settles_each_narrowing() {
    let _serial = SERIAL.lock().await;
    let sb = Sandbox::new("await-present");
    metrics_handle();
    sb.write(&mapping_artifact_path(TODAY), &artifact(&stocks(1)));

    // Not narrowed: nothing more to wait for.
    let present = wait_outcome("already_present");
    let not_narrowed = settle_outcome("not_narrowed");
    await_mapping_artifact(&master_cfg(), TODAY, false, true).await;
    assert_eq!(wait_outcome("already_present"), present + 1.0);
    assert_eq!(settle_outcome("not_narrowed"), not_narrowed + 1.0);

    // NTM narrowing whose file is already there.
    sb.write(&ntm_spot_artifact_path(TODAY), &artifact(&stocks(1)));
    let ntm_cfg = DhanUniverseConfig {
        spot_universe_ntm_only: true,
        ..master_cfg()
    };
    let already = settle_outcome("already_present");
    await_mapping_artifact(&ntm_cfg, TODAY, false, true).await;
    assert_eq!(settle_outcome("already_present"), already + 1.0);

    // F&O narrowing whose file lands a moment after the mapping.
    let fno_cfg = DhanUniverseConfig {
        spot_universe_fno_underlyings_only: true,
        ..master_cfg()
    };
    let late_file = sb.root.join(fno_underlying_artifact_path(TODAY));
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        std::fs::write(late_file, artifact(&stocks(1))).expect("late write");
    });
    let settled = settle_outcome("settled");
    let (cap, _g) = capture_logs();
    await_mapping_artifact(&fno_cfg, TODAY, false, true).await;
    writer.join().expect("writer");
    assert_eq!(settle_outcome("settled"), settled + 1.0);
    assert!(cap.text().contains("which=\"fno\""), "{}", cap.text());
}

#[tokio::test]
async fn test_live_universe_await_mapping_producer_too_far_is_classified_by_expectation() {
    let _serial = SERIAL.lock().await;
    let _sb = Sandbox::new("await-far");
    metrics_handle();
    let cfg = master_cfg();
    let now = tickvault_common::market_hours::now_ist_secs_of_day();
    // Only when the pure bound says "do not wait" can this run without
    // sleeping for the rider; that holds outside 08:10–09:10 IST.
    if mapping_wait_end_ist_secs(now, cfg.target_secs_of_day_ist).is_some() {
        return;
    }
    let before = wait_outcome("producer_too_far");
    let (cap, _g) = capture_logs();
    await_mapping_artifact(&cfg, TODAY, true, true).await;
    await_mapping_artifact(&cfg, TODAY, false, true).await;
    assert_eq!(wait_outcome("producer_too_far"), before + 2.0);
    let logs = cap.text();
    assert!(
        logs.contains("before the daily rider's build hour"),
        "expected boot: {logs}"
    );
    assert!(
        logs.contains("further away than this boot may stall for"),
        "unexpected boot: {logs}"
    );
}

// ---------------------------------------------------------------------------
// Pure helpers: the artifact parser's per-entry tolerance and the date list
// ---------------------------------------------------------------------------

#[test]
fn test_live_universe_parse_mapping_artifact_skips_bad_rows_and_fails_loud_on_bad_files() {
    use tickvault_app::dhan_live_universe::parse_mapping_artifact;

    let body = r#"{"mappings":[
        {"security_id": 11, "exchange_segment": 1, "symbol": "  infy  "},
        {"security_id": "12", "exchange_segment": 1},
        {"security_id": 13},
        {"security_id": 14, "exchange_segment": "NSE_EQ"},
        {"security_id": 15, "exchange_segment": 300},
        {"security_id": -16, "exchange_segment": 1},
        {"security_id": 17, "exchange_segment": 2}
    ]}"#;
    let rows = parse_mapping_artifact(body).expect("valid file");
    let ids: Vec<u64> = rows.iter().map(|r| r.security_id).collect();
    assert_eq!(
        ids,
        vec![11, 17],
        "only rows with a u64 id and a u8 segment"
    );
    assert_eq!(rows[0].symbol, "INFY", "symbol trimmed and uppercased");
    assert_eq!(rows[1].symbol, "", "absent symbol reads empty");
    assert_eq!(rows[1].exchange_segment_code, 2);

    assert_eq!(
        parse_mapping_artifact("not json").expect_err("garbage"),
        "mapping artifact is not valid JSON"
    );
    assert_eq!(
        parse_mapping_artifact(r#"{"mappings": {}}"#).expect_err("not an array"),
        "mapping artifact has no `mappings` array"
    );
    assert!(
        parse_mapping_artifact(r#"{"mappings": []}"#)
            .expect("empty is valid")
            .is_empty()
    );
}

#[test]
fn test_live_universe_earlier_dates_and_seed_names_edges() {
    use tickvault_app::dhan_live_universe::{earlier_ist_dates, seed_names_for};

    assert_eq!(
        earlier_ist_dates("2026-03-01", 2),
        vec!["2026-02-28".to_owned(), "2026-02-27".to_owned()],
        "crosses a month boundary"
    );
    assert!(earlier_ist_dates("2026-03-01", 0).is_empty());
    assert!(
        earlier_ist_dates("2026-03-01", -5).is_empty(),
        "negative is none"
    );
    assert!(
        earlier_ist_dates("01-03-2026", 3).is_empty(),
        "malformed date"
    );
    assert_eq!(seed_names_for(&[13, 424_242]), vec!["NIFTY", "?"]);
}

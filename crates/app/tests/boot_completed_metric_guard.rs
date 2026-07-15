//! Source-scan ratchet: the dedicated `tv_boot_completed` boot-success metric
//! must stay emitted on BOTH boot-completion paths and stay wired end-to-end to
//! the CloudWatch boot-heartbeat alarm.
//!
//! Background: PR #1278 added the boot-heartbeat alarm but had to page on a
//! PROXY (`tv_realtime_guarantee_score`) because `tv_boot_completed` did not
//! exist. This PR makes the ideal signal real: `emit_boot_completed()` sets the
//! gauge to 1.0 at the genuine boot-complete point (fast-boot + slow-boot),
//! the metric is added to the CloudWatch-agent scrape filter, and the alarm is
//! repointed onto it. These guards fail the build if any leg silently regresses
//! (audit-findings Rule 13: a metric defined but not actually wired is a bug).

use std::fs;
use std::path::PathBuf;

/// Read a file relative to the `tickvault-app` crate manifest dir.
fn read_app(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Read a file relative to the repo root (manifest dir is `crates/app`).
fn read_repo(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The metric-name string must stay stable: the emit site, the scrape filter,
/// and the alarm all hard-code it. Drift would silently break the page.
const BOOT_COMPLETED_METRIC: &str = "tv_boot_completed";

#[test]
fn test_emit_boot_completed_helper_exists_and_sets_the_gauge() {
    let main_src = read_app("src/main.rs");
    assert!(
        main_src.contains("fn emit_boot_completed()"),
        "main.rs must define `fn emit_boot_completed()` (the single boot-success emit helper)."
    );
    assert!(
        main_src.contains("metrics::gauge!(BOOT_COMPLETED_METRIC).set(1.0)")
            || main_src.contains(&format!(
                "metrics::gauge!(\"{BOOT_COMPLETED_METRIC}\").set(1.0)"
            )),
        "emit_boot_completed() must set the `{BOOT_COMPLETED_METRIC}` gauge to 1.0."
    );
    assert!(
        main_src.contains(&format!("\"{BOOT_COMPLETED_METRIC}\"")),
        "main.rs must reference the exact metric name string `{BOOT_COMPLETED_METRIC}`."
    );
}

#[test]
fn test_boot_completed_is_emitted_on_both_boot_paths() {
    let main_src = read_app("src/main.rs");
    // The `tv_boot_completed` gauge is published via the feed-liveness-gated
    // wrapper `emit_boot_completed_when_feed_live(...)`.
    // PR-C2 re-shape (2026-07-13, operator retirement directive —
    // websocket-connection-scope-lock.md "2026-07-13 Amendment"): the fast
    // crash-recovery boot arm was DELETED with the Dhan live-WS lane, so
    // main.rs has a SINGLE boot-completion path: exactly 1 definition site
    // (`async fn emit_boot_completed_when_feed_live(`) + 1 call site.
    let gated_sites = main_src
        .matches("emit_boot_completed_when_feed_live(")
        .count();
    assert!(
        gated_sites >= 2,
        "expected `emit_boot_completed_when_feed_live` to appear >= 2 times (1 def + the single \
         boot-path call), found {gated_sites} — the boot-completion path must publish \
         `{BOOT_COMPLETED_METRIC}` through the feed-liveness gate."
    );
    // The pure gauge-setter must still exist and be the thing the gate calls.
    assert!(
        main_src.contains("emit_boot_completed()"),
        "the pure `emit_boot_completed()` gauge-setter must remain (called by the gated wrapper)."
    );
    // Both emits must sit next to the boot-complete `notify_systemd_ready()`
    // markers, i.e. on the success path only (a halt never reaches them).
    assert!(
        main_src.contains("infra::notify_systemd_ready();"),
        "the boot-complete `notify_systemd_ready()` markers must remain so the emit stays on \
         the success path."
    );
}

/// The alive signal must be GATED on a live feed — closing the alerting hole
/// where a boot that reached the emit line with every enabled feed dead still
/// published `tv_boot_completed=1`, so the boot-heartbeat alarm never paged.
/// This ratchets that the emit is no longer a bare unconditional call: both
/// boot-path emits go through `emit_boot_completed_when_feed_live`, which is
/// driven by the pure `boot_completed_should_emit` verdict.
#[test]
fn boot_completed_emit_sites_are_gated_on_live_feed() {
    let main_src = read_app("src/main.rs");
    // The pure verdict helper must exist and be feed-agnostic (4 bool inputs:
    // per-feed enabled + per-feed running).
    assert!(
        main_src.contains("fn boot_completed_should_emit(")
            && main_src.contains("dhan_enabled")
            && main_src.contains("groww_enabled"),
        "main.rs must define the pure `boot_completed_should_emit(dhan_enabled, groww_enabled, \
         dhan_running, groww_running)` verdict helper — the feed-agnostic gate for the alive signal."
    );
    // The async wrapper that waits (bounded) for a live feed then emits/withholds.
    assert!(
        main_src.contains("async fn emit_boot_completed_when_feed_live("),
        "main.rs must define `emit_boot_completed_when_feed_live` — the bounded-wait feed-liveness \
         gate around the `emit_boot_completed()` gauge-setter."
    );
    // The wrapper must actually consult the pure verdict + the per-feed
    // lane-running signals (so a regression to an unconditional emit is caught).
    assert!(
        main_src.contains("boot_completed_should_emit(")
            && main_src.contains("is_dhan_lane_running()")
            && main_src.contains("is_groww_lane_running()"),
        "the feed-liveness gate must call `boot_completed_should_emit(...)` against \
         `is_dhan_lane_running()` / `is_groww_lane_running()` — a bare unconditional emit is a \
         regression that re-opens the alerting hole."
    );
    // The boot-path emit must go through the gate — no bare `emit_boot_completed();`
    // statement may remain on the success path (only the wrapper body calls it).
    // PR-C2 (2026-07-13): single boot path — 1 def + 1 call.
    let gated_sites = main_src
        .matches("emit_boot_completed_when_feed_live(")
        .count();
    assert!(
        gated_sites >= 2,
        "the boot path must emit via the feed-liveness gate (>= 1 def + 1 call), found \
         {gated_sites}."
    );
}

#[test]
fn test_metric_is_in_cloudwatch_scrape_filter() {
    let tftpl = read_repo("deploy/aws/terraform/user-data.sh.tftpl");
    // 2026-07-02 root-cause fix (B1): the metric-name regex now lives ONLY in
    // `metric_selectors`. The old shape duplicated it into `label_matcher`
    // with `source_labels: ["__name__"]` — but `__name__` is not a series
    // label at the emf_processor stage, so the declaration matched ZERO
    // metrics and Tickvault/Prod sat empty ~40 days. The correct shape:
    // `source_labels: ["host"]` matched against its literal value, and the
    // metric name filtered by `metric_selectors` alone. Full-shape pins live
    // in crates/common/tests/cloudwatch_app_alarms_wiring.rs.
    assert!(
        tftpl.matches(BOOT_COMPLETED_METRIC).count() >= 1,
        "`{BOOT_COMPLETED_METRIC}` must appear in the metric_selectors of the \
         CloudWatch-agent metric_declaration filter — without it the metric is \
         emitted by the app but NEVER shipped to CloudWatch."
    );
    assert!(
        tftpl.contains("\"source_labels\": [\"host\"]"),
        "the CW-agent metric_declaration must use source_labels [\"host\"] — a label \
         that actually exists on the scraped series (the [\"__name__\"] shape is the \
         40-day-empty-namespace root cause; see B1 analysis 2026-07-02)."
    );
}

#[test]
fn test_alarm_is_repointed_onto_the_dedicated_metric() {
    let alarm = read_repo("deploy/aws/terraform/boot-heartbeat-alarm.tf");
    assert!(
        alarm.contains(&format!(
            "metric_name         = \"{BOOT_COMPLETED_METRIC}\""
        )) || alarm.contains(&format!("metric_name = \"{BOOT_COMPLETED_METRIC}\"")),
        "the boot-heartbeat alarm `metric_name` must be `{BOOT_COMPLETED_METRIC}` (the dedicated \
         boot signal), not the old proxy."
    );
    // The alarm must NO LONGER use the proxy as its metric_name (it may still be
    // mentioned in prose explaining the repoint, but not as the alarm metric).
    assert!(
        !alarm.contains("metric_name         = \"tv_realtime_guarantee_score\"")
            && !alarm.contains("metric_name = \"tv_realtime_guarantee_score\""),
        "the boot-heartbeat alarm must no longer use the `tv_realtime_guarantee_score` proxy as \
         its metric_name."
    );
    // The intentional missing=page semantics must be preserved.
    assert!(
        alarm.contains("treat_missing_data = \"breaching\""),
        "the boot-heartbeat alarm must keep treat_missing_data=breaching (missing boot = page)."
    );
}

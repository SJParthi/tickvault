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
    let call_sites = main_src.matches("emit_boot_completed()").count();
    // 1 definition site (`fn emit_boot_completed()`) + 2 call sites
    // (fast-boot crash-recovery + slow-boot normal). The helper is called on
    // BOTH completion paths so a successful fast-boot never false-pages the alarm.
    assert!(
        call_sites >= 3,
        "expected `emit_boot_completed` to appear >= 3 times (1 def + 2 boot-path calls), \
         found {call_sites} — every boot-completion path must emit `{BOOT_COMPLETED_METRIC}`."
    );
    // Both emits must sit next to the boot-complete `notify_systemd_ready()`
    // markers, i.e. on the success path only (a halt never reaches them).
    assert!(
        main_src.contains("infra::notify_systemd_ready();"),
        "the boot-complete `notify_systemd_ready()` markers must remain so the emit stays on \
         the success path."
    );
}

#[test]
fn test_metric_is_in_cloudwatch_scrape_filter() {
    let tftpl = read_repo("deploy/aws/terraform/user-data.sh.tftpl");
    // The CW-agent emf_processor lists the metric twice (label_matcher +
    // metric_selectors). Without this, the metric is emitted by the app but
    // NEVER shipped to CloudWatch -> the alarm could never observe it.
    let occurrences = tftpl.matches(BOOT_COMPLETED_METRIC).count();
    assert!(
        occurrences >= 2,
        "`{BOOT_COMPLETED_METRIC}` must appear in BOTH the label_matcher and metric_selectors \
         of the CloudWatch-agent metric_declaration filter (found {occurrences})."
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

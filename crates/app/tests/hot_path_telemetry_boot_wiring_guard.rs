//! Telemetry startup errors must reach the installed subscriber while the
//! publisher retains its current proof writer and market-calendar wiring.

use tickvault_common::error_code::{ErrorCode, Severity};

const MAIN: &str = include_str!("../src/main.rs");

#[test]
fn telemetry_starts_after_tracing_and_metrics_before_live_supervisors() {
    let metrics = MAIN
        .find("    observability::init_metrics(&config.observability)")
        .expect("metrics install");
    let registry = MAIN
        .find("    tracing_subscriber::registry()")
        .expect("tracing registry");
    let tracing = registry
        + MAIN[registry..]
            .find("        .init();")
            .expect("subscriber install");
    let proof = MAIN.find("    let _proof_writer = tickvault_app::hot_path_proof_boot::start_hot_path_proof(&config.questdb);").expect("proof writer retained");
    let publisher = MAIN
        .find("    if let Err(err) = tickvault_storage::hot_path_telemetry::spawn_publisher(")
        .expect("publisher spawn");
    let watch = MAIN
        .find("    tickvault_app::task_watch_boot::start_task_watch(&config);")
        .expect("task watch retained");
    let runtime_probe = MAIN
        .find("    tokio::spawn(tickvault_storage::hot_path_telemetry::run_runtime_probe(")
        .expect("main runtime probe retained");
    let live = MAIN.find("    let dhan_feed_stack_monitor = tickvault_app::dhan_feed_stack::spawn_dhan_feed_stack(").expect("live supervisor");
    assert!(metrics < tracing && tracing < proof && proof < publisher);
    assert!(publisher < watch && watch < runtime_probe && runtime_probe < live);
    assert_eq!(
        MAIN.matches("hot_path_telemetry::spawn_publisher(").count(),
        1
    );
    assert_eq!(
        MAIN.matches("hot_path_telemetry::run_runtime_probe(")
            .count(),
        2
    );
    let startup = &MAIN[publisher..runtime_probe];
    assert!(
        startup.contains("&trading_calendar"),
        "current calendar-aware publisher API retained"
    );
    assert!(startup.contains("ErrorCode::HotPath04TelemetryNotStarted"));
    assert!(!startup.contains("ErrorCode::Boot02DeadlineExceeded"));
}

#[test]
fn telemetry_spawn_failure_has_a_specific_error_code_and_runbook() {
    let code = ErrorCode::HotPath04TelemetryNotStarted;
    assert_eq!(code.code_str(), "HOT-PATH-04");
    assert_eq!(code.severity(), Severity::Medium);
    assert_eq!(
        code.runbook_path(),
        "docs/error-runbooks/wave-1-error-codes.md"
    );
    assert!(ErrorCode::all().contains(&code));
}

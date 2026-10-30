//! Targeted tests for production lines in `tickvault-common` that the
//! existing suites never executed (llvm-cov, 0-hit lines on main c309f59).
//!
//! Every test drives a PUBLIC entry point (or the `coverage_file_floors`
//! binary through `CARGO_BIN_EXE_*`) and asserts the behaviour the targeted
//! line produces. Lines that cannot be reached from a test are listed in the
//! PR / workflow result with a reason, never faked here.

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

use figment::Figment;
use figment::providers::{Format, Toml};
use tickvault_common::config::{ApplicationConfig, TradingMode};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::ingest_shed::{
    IngestShedGate, MIN_BURN_OBSERVATION_SECS, ShedLevel, decide_shed_level_all_signals,
    decide_shed_level_by_exhaustion, decide_shed_level_with_runway,
};
use tickvault_common::instrument_registry::{
    InstrumentRegistry, SubscribedInstrument, SubscriptionCategory,
};
use tickvault_common::sanitize::{ILP_SYMBOL_MAX_BYTES, ilp_symbol_is_clean};
use tickvault_common::source_scan::strip_rust_comments;
use tickvault_common::types::{ExchangeSegment, FeedMode};

// ---------------------------------------------------------------------------
// A minimal capturing tracing subscriber (the `tracing` crate is already a
// normal dependency of this package; no new dependency).
//
// A `tracing` event's field expressions are only evaluated when a subscriber
// is interested, so lines inside `info!` / `warn!` argument lists are only
// executed (and only observable) with a subscriber installed.
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct CapturedEvent {
    fields: Vec<(String, String)>,
}

impl CapturedEvent {
    fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

struct FieldCollector<'a>(&'a mut Vec<(String, String)>);

impl tracing::field::Visit for FieldCollector<'_> {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.push((field.name().to_string(), value.to_string()));
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0
            .push((field.name().to_string(), format!("{value:?}")));
    }
}

#[derive(Clone, Default)]
struct CapturingSubscriber {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl tracing::Subscriber for CapturingSubscriber {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut fields = Vec::new();
        event.record(&mut FieldCollector(&mut fields));
        if let Ok(mut guard) = self.events.lock() {
            guard.push(CapturedEvent { fields });
        }
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

impl CapturingSubscriber {
    fn snapshot(&self) -> Vec<CapturedEvent> {
        self.events.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn load_base_config() -> ApplicationConfig {
    Figment::new()
        .merge(Toml::file("../../config/base.toml"))
        .extract()
        .expect("config/base.toml must deserialize into ApplicationConfig") // APPROVED: test
}

/// A unique temp file path, removed by the returned guard on drop.
struct TempFile(PathBuf);

impl TempFile {
    fn with_contents(tag: &str, contents: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!(
            "tv_cov_gaps_common_{}_{}_{}",
            std::process::id(),
            nanos,
            tag
        ));
        std::fs::write(&path, contents).expect("temp file must be writable"); // APPROVED: test
        Self(path)
    }

    fn path_str(&self) -> &str {
        self.0.to_str().expect("temp path is UTF-8") // APPROVED: test
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn run_floors(args: &[&str]) -> (i32, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_coverage_file_floors"))
        .args(args)
        .output()
        .expect("coverage_file_floors binary must run"); // APPROVED: test
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

const REPORT_JSON: &str = r#"{"data":[{"files":[
    {"filename":"/w/crates/app/src/main.rs","summary":{"lines":{"count":200,"covered":150}}},
    {"filename":"/w/crates/core/src/lib.rs","summary":{"lines":{"count":100,"covered":100}}}
]}]}"#;

// ---------------------------------------------------------------------------
// crates/common/src/bin/coverage_file_floors.rs — main() + parse_floors
// ---------------------------------------------------------------------------

#[test]
fn test_coverage_file_floors_binary_usage_error_exits_2() {
    let (code, stderr) = run_floors(&[]);
    assert_eq!(code, 2, "missing arguments is bad usage → exit 2");
    assert!(
        stderr.contains("usage: coverage_file_floors"),
        "usage text printed to stderr: {stderr}"
    );
}

#[test]
fn test_coverage_file_floors_binary_floor_holds_exits_0() {
    let report = TempFile::with_contents("report_ok.json", REPORT_JSON);
    let floors = TempFile::with_contents(
        "floors_ok.toml",
        "[files]\n\"crates/app/src/main.rs\" = 70\n\"crates/core/src/lib.rs\" = 99.5\n",
    );
    let (code, stderr) = run_floors(&[report.path_str(), floors.path_str()]);
    assert_eq!(code, 0, "every floor holds → exit 0; stderr: {stderr}");
    assert!(stderr.contains("ok: crates/app/src/main.rs 75.00% >= 70.00"));
    assert!(stderr.contains("ok: crates/core/src/lib.rs 100.00% >= 99.50"));
    assert!(stderr.contains("Largest 15 files by line count:"));
}

#[test]
fn test_coverage_file_floors_binary_below_floor_exits_1() {
    let report = TempFile::with_contents("report_below.json", REPORT_JSON);
    let floors = TempFile::with_contents(
        "floors_below.toml",
        "[files]\n\"crates/app/src/main.rs\" = 80.0\n",
    );
    let (code, stderr) = run_floors(&[report.path_str(), floors.path_str()]);
    assert_eq!(code, 1, "a floor failed → exit 1");
    assert!(stderr.contains("BELOW FLOOR: crates/app/src/main.rs 75.00% < 80.00"));
}

#[test]
fn test_coverage_file_floors_binary_no_files_table_is_report_only() {
    let report = TempFile::with_contents("report_nofiles.json", REPORT_JSON);
    let floors = TempFile::with_contents("floors_nofiles.toml", "[other]\nx = 1\n");
    let (code, stderr) = run_floors(&[report.path_str(), floors.path_str()]);
    assert_eq!(code, 0, "no [files] table → report only, exit 0");
    assert!(stderr.contains("No [files] floors set: report only."));
}

#[test]
fn test_coverage_file_floors_binary_unreadable_input_exits_2() {
    let floors = TempFile::with_contents("floors_unreadable.toml", "[files]\n");
    let missing = std::env::temp_dir().join(format!(
        "tv_cov_gaps_common_missing_{}.json",
        std::process::id()
    ));
    let missing_str = missing.to_str().expect("temp path is UTF-8"); // APPROVED: test
    let (code, stderr) = run_floors(&[missing_str, floors.path_str()]);
    assert_eq!(code, 2, "unreadable input → exit 2");
    assert!(stderr.contains("coverage_file_floors: cannot read"));
}

#[test]
fn test_coverage_file_floors_binary_bad_floor_value_exits_2() {
    let report = TempFile::with_contents("report_badfloor.json", REPORT_JSON);
    let floors = TempFile::with_contents(
        "floors_badfloor.toml",
        "[files]\n\"crates/app/src/main.rs\" = 101.0\n",
    );
    let (code, stderr) = run_floors(&[report.path_str(), floors.path_str()]);
    assert_eq!(code, 2, "an out-of-range floor is refused → exit 2");
    assert!(stderr.contains("must be 0 to 100 with at most two decimals"));
}

// ---------------------------------------------------------------------------
// crates/common/src/config.rs — ApplicationConfig::validate refusals + warn
// ---------------------------------------------------------------------------

#[test]
fn test_config_validate_rejects_zero_max_orders_per_second() {
    let mut config = load_base_config();
    config.trading.max_orders_per_second = 0;
    let err = config
        .validate()
        .expect_err("a zero order rate must be refused at boot"); // APPROVED: test
    assert!(
        err.to_string()
            .contains("trading.max_orders_per_second must be >= 1"),
        "wrong refusal: {err}"
    );
}

#[test]
fn test_config_validate_rejects_nan_and_zero_capital() {
    for capital in [f64::NAN, 0.0, -1.0, f64::INFINITY] {
        let mut config = load_base_config();
        config.strategy.capital = capital;
        let err = config
            .validate()
            .expect_err("a non-finite or non-positive capital must be refused"); // APPROVED: test
        assert!(
            err.to_string()
                .contains("strategy.capital must be a finite value > 0"),
            "capital {capital}: wrong refusal: {err}"
        );
    }
}

#[test]
fn test_config_validate_live_mode_refused_by_sandbox_guard() {
    let mut config = load_base_config();
    config.strategy.mode = TradingMode::Live;
    let err = config
        .validate()
        .expect_err("live mode before LIVE_TRADING_EARLIEST must be refused"); // APPROVED: test
    assert!(err.to_string().contains("SANDBOX GUARD"), "{err}");
}

#[test]
fn test_config_validate_warns_once_per_expired_configured_sandbox_gate() {
    let mut config = load_base_config();
    // A configured cutoff in the past that is neither the compiled default
    // nor the 1970-01-01 disable sentinel → the expired-gate warn fires.
    config.strategy.sandbox_only_until = "2020-01-02".to_string();
    let subscriber = CapturingSubscriber::default();
    let result = tracing::subscriber::with_default(subscriber.clone(), || config.validate());
    assert!(
        result.is_ok(),
        "an expired gate is a warn, not a boot refusal: {result:?}"
    );
    let events = subscriber.snapshot();
    let gates: Vec<&str> = events.iter().filter_map(|e| e.field("gate")).collect();
    assert_eq!(
        gates,
        vec!["sandbox_only_until_configured"],
        "exactly the configured gate is reported as expired"
    );
}

// ---------------------------------------------------------------------------
// crates/common/src/ingest_shed.rs
// ---------------------------------------------------------------------------

#[test]
fn test_ingest_shed_gate_default_sheds_nothing() {
    let gate = IngestShedGate::default();
    assert_eq!(gate.level(), ShedLevel::None);
    assert!(gate.allows_inline_depth());
    assert!(gate.set(ShedLevel::AllDepth), "a real change reports true");
    assert_eq!(gate.level(), ShedLevel::AllDepth);
}

#[test]
fn test_ingest_shed_runway_between_bars_holds_current_level() {
    // free_fraction 0.9 → fraction half restores to None; runway 1.6 sessions
    // sits between the 1.5 shed bar and the 1.75/2.0 restore bars, so the
    // runway half HOLDS the current level and the max keeps it.
    let free_bytes = 160;
    let burn = 100;
    assert_eq!(
        decide_shed_level_with_runway(ShedLevel::InlineDepth, 0.9, free_bytes, burn, false),
        ShedLevel::InlineDepth
    );
    assert_eq!(
        decide_shed_level_with_runway(ShedLevel::None, 0.9, free_bytes, burn, false),
        ShedLevel::None
    );
}

#[test]
fn test_ingest_shed_exhaustion_partial_recovery_from_all_depth() {
    // ratio 1.8 ∈ [1.75, 2.0) with the gate at AllDepth → one rung down.
    assert_eq!(
        decide_shed_level_by_exhaustion(ShedLevel::AllDepth, 1_800, 1_000),
        ShedLevel::InlineDepth
    );
}

#[test]
fn test_ingest_shed_all_signals_outside_capture_window_returns_configured() {
    // Anchor + a real burn rate, but the capture window has closed, so the
    // exhaustion half has nothing to say and the configured verdict stands.
    let anchor_free = 1_000_000_000u64;
    let anchor_secs = 1_000u64;
    let secs_now = anchor_secs + MIN_BURN_OBSERVATION_SECS;
    let free_now = anchor_free - 10_000_000; // a positive, plausible burn
    let after_close = u32::MAX;
    let level = decide_shed_level_all_signals(
        ShedLevel::None,
        0.9,
        free_now,
        0,
        false,
        Some((anchor_free, anchor_secs)),
        secs_now,
        after_close,
        0,
    );
    assert_eq!(level, ShedLevel::None);
    // Same inputs, gate already shedding: the healthy fraction restores it.
    let level = decide_shed_level_all_signals(
        ShedLevel::AllDepth,
        0.9,
        free_now,
        0,
        false,
        Some((anchor_free, anchor_secs)),
        secs_now,
        after_close,
        0,
    );
    assert_eq!(level, ShedLevel::None);
}

// ---------------------------------------------------------------------------
// crates/common/src/instrument_registry.rs — cross-segment collision INFO
// ---------------------------------------------------------------------------

fn instrument(id: u64, segment: ExchangeSegment, symbol: &str) -> SubscribedInstrument {
    SubscribedInstrument {
        security_id: id,
        exchange_segment: segment,
        category: SubscriptionCategory::StockEquity,
        display_label: symbol.to_string(),
        underlying_symbol: symbol.to_string(),
        instrument_kind: None,
        expiry_date: None,
        strike_price: None,
        option_type: None,
        feed_mode: FeedMode::Ticker,
    }
}

#[test]
fn test_instrument_registry_collision_logs_coded_info_event() {
    let subscriber = CapturingSubscriber::default();
    let registry = tracing::subscriber::with_default(subscriber.clone(), || {
        InstrumentRegistry::from_instruments(vec![
            instrument(27, ExchangeSegment::IdxI, "FINNIFTY"),
            instrument(27, ExchangeSegment::NseEquity, "SOMESTOCK"),
        ])
    });
    assert_eq!(registry.cross_segment_collisions(), 1);
    let events = subscriber.snapshot();
    let collision = events
        .iter()
        .find(|e| e.field("prev_symbol").is_some())
        .expect("the collision INFO event must be emitted"); // APPROVED: test
    let expected_code = ErrorCode::InstrumentP1CrossSegmentCollision.code_str();
    let expected_severity = ErrorCode::InstrumentP1CrossSegmentCollision
        .severity()
        .as_str();
    assert_eq!(collision.field("code"), Some(expected_code));
    assert_eq!(collision.field("severity"), Some(expected_severity));
    assert_eq!(collision.field("prev_symbol"), Some("FINNIFTY"));
    assert_eq!(collision.field("new_symbol"), Some("SOMESTOCK"));
}

// ---------------------------------------------------------------------------
// crates/common/src/sanitize.rs — ilp_symbol_is_clean length cap
// ---------------------------------------------------------------------------

#[test]
fn test_ilp_symbol_is_clean_rejects_over_cap_length() {
    let at_cap = "a".repeat(ILP_SYMBOL_MAX_BYTES);
    let over_cap = "a".repeat(ILP_SYMBOL_MAX_BYTES + 1);
    assert!(ilp_symbol_is_clean(&at_cap), "exactly at the cap is clean");
    assert!(
        !ilp_symbol_is_clean(&over_cap),
        "one byte over the cap must be refused"
    );
}

// ---------------------------------------------------------------------------
// crates/common/src/source_scan.rs — unterminated raw string runs to EOF
// ---------------------------------------------------------------------------

#[test]
fn test_strip_rust_comments_unterminated_raw_string_keeps_everything() {
    // The raw string never closes, so it extends to end-of-input and the
    // `//` inside it is string content, not a comment.
    let src = "let x = r#\"open // not a comment\nstill inside";
    assert_eq!(strip_rust_comments(src), src);

    // Control: a terminated raw string followed by a real comment strips the
    // comment bytes to spaces, preserving length.
    let closed = "let y = r#\"a\"#; // gone";
    let stripped = strip_rust_comments(closed);
    assert_eq!(stripped.len(), closed.len());
    assert_eq!(stripped.trim_end(), "let y = r#\"a\"#;");
}

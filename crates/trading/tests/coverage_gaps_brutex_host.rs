//! Coverage for the refusal branches of the brutex strategy host that the
//! in-module tests do not reach. Every test drives a public entry point and
//! asserts the exact refusal (or note) the branch produces. Temp folders live
//! under `std::env::temp_dir()` with a unique name and are removed at the end.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use tickvault_trading::brutex_host::audit::{AuditEvent, AuditKind, CollectAlerts, append};
use tickvault_trading::brutex_host::check_files::{parse_candles, parse_signals, parse_values};
use tickvault_trading::brutex_host::engine::{ReferenceDouble, StrategyEngine};
use tickvault_trading::brutex_host::fixture::{
    FIXTURE_COMMIT, toy_check_files, write_demo_fixture,
};
use tickvault_trading::brutex_host::loader::{LoadContext, LoadOutcome, load};
use tickvault_trading::brutex_host::manifest::{MANIFEST_NAME, manifest_json_for, verify_folder};
use tickvault_trading::brutex_host::recipe::{Recipe, parse_recipe};
use tickvault_trading::brutex_host::replay::replay;
use tickvault_trading::brutex_host::select::is_iso_date;
use tickvault_trading::brutex_host::{InstrumentKey, Refusal};

const SIGNAL_HEADER: &str = "ts_ist,security_id,exchange_segment,timeframe,signal,side,qty_lots,entry_price,stop_loss,target";
const CANDLE_HEADER: &str =
    "ts_ist,security_id,exchange_segment,timeframe,open,high,low,close,volume";

/// A unique scratch folder, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("tv-cov-brutex-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        Self(dir)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn known_all(_: InstrumentKey) -> bool {
    true
}

fn known_none(_: InstrumentKey) -> bool {
    false
}

fn run_load(
    root: &Path,
    trading_day: &str,
    is_known: &dyn Fn(InstrumentKey) -> bool,
) -> LoadOutcome {
    let ctx = LoadContext {
        built_commit: FIXTURE_COMMIT,
        trading_day,
        at_ist: "2026-10-29T08:30:00+05:30",
        is_known,
    };
    let mut engine = ReferenceDouble::new();
    let mut alerts = CollectAlerts::default();
    load(root, &ctx, &mut engine, None, &mut alerts)
}

fn refused_detail(out: &LoadOutcome) -> &str {
    assert!(out.live.is_none(), "expected a refusal, got {:?}", out.live);
    assert!(!out.changed);
    assert_eq!(out.events.len(), 1, "{:?}", out.events);
    assert_eq!(out.events[0].kind, AuditKind::Refused);
    &out.events[0].detail
}

fn loaded_detail(out: &LoadOutcome) -> &str {
    assert!(out.live.is_some(), "expected a load, got {:?}", out.events);
    assert!(out.changed);
    assert_eq!(out.events[0].kind, AuditKind::Loaded);
    &out.events[0].detail
}

fn demo(tag: &str) -> (Scratch, PathBuf, PathBuf) {
    let s = Scratch::new(tag);
    let dirs = write_demo_fixture(s.path()).expect("write fixture");
    (s, dirs[0].clone(), dirs[1].clone())
}

fn recipe_json(overrides: &str) -> String {
    // Base: a valid fixed recipe; `overrides` replaces fields via a merge.
    let mut base: serde_json::Value = serde_json::json!({
        "schema": "tickvault-strategy-recipe/1",
        "name": "s",
        "version": 1,
        "status": "approved",
        "approved_by": "t",
        "approved_at_ist": "2026-10-29T18:00:00+05:30",
        "brutex_commit": FIXTURE_COMMIT,
        "dynamic": false,
        "instruments": [{"security_id": 1, "exchange_segment": 1}],
        "timeframes": ["1m"],
        "settings": {},
        "backtest": {}
    });
    let patch: serde_json::Value = serde_json::from_str(overrides).expect("patch json");
    if let (Some(b), Some(p)) = (base.as_object_mut(), patch.as_object()) {
        for (k, v) in p {
            b.insert(k.clone(), v.clone());
        }
    }
    base.to_string()
}

// ---------------------------------------------------------------- audit.rs

fn sample_event() -> AuditEvent {
    AuditEvent {
        at_ist: "2026-10-29T08:30:00+05:30".to_owned(),
        kind: AuditKind::Loaded,
        strategy: "s".to_owned(),
        version: Some(1),
        params_date: None,
        universe_date: None,
        detail: "d".to_owned(),
    }
}

#[test]
fn audit_append_to_a_path_with_no_parent_fails_to_open() {
    // `Path::new("")` has no parent, so the create-dir step is skipped and the
    // open of the empty path fails.
    let err = append(Path::new(""), &[sample_event()]).expect_err("empty path must fail");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn audit_append_fails_when_the_parent_is_a_file() {
    let s = Scratch::new("audit-parent");
    let file = s.path().join("blocker");
    std::fs::write(&file, b"x").expect("write blocker");
    let err = append(&file.join("sub").join("audit.ndjson"), &[sample_event()]);
    assert!(err.is_err(), "create_dir_all under a file must fail");
    // Nothing was written into the blocker file.
    assert_eq!(std::fs::read(&file).expect("read blocker"), b"x");
}

// ---------------------------------------------------------- check_files.rs

#[test]
fn candles_with_an_empty_timeframe_are_refused() {
    let bytes = format!("{CANDLE_HEADER}\n2026-10-27T09:15:00,1,1,,1,1,1,1,1\n");
    let err = parse_candles("c.csv", bytes.as_bytes()).expect_err("empty timeframe");
    assert_eq!(
        err,
        Refusal::BadCheckFile {
            name: "c.csv".to_owned(),
            row: 2,
            detail: "timeframe empty".to_owned(),
        }
    );
}

#[test]
fn values_with_a_foreign_header_are_refused() {
    let err = parse_values("v.csv", b"foo,bar\n").expect_err("bad header");
    assert_eq!(
        err,
        Refusal::BadCheckFile {
            name: "v.csv".to_owned(),
            row: 0,
            detail: "header \"foo,bar\"".to_owned(),
        }
    );
}

#[test]
fn signals_with_a_foreign_header_are_refused() {
    let err = parse_signals("s.csv", b"a,b,c\n").expect_err("bad header");
    assert_eq!(
        err,
        Refusal::BadCheckFile {
            name: "s.csv".to_owned(),
            row: 0,
            detail: "header \"a,b,c\"".to_owned(),
        }
    );
}

#[test]
fn signals_with_the_wrong_column_count_are_refused() {
    let bytes = format!("{SIGNAL_HEADER}\n2026-10-27T09:15:00,1,1\n");
    let err = parse_signals("s.csv", bytes.as_bytes()).expect_err("3 columns");
    assert_eq!(
        err,
        Refusal::BadCheckFile {
            name: "s.csv".to_owned(),
            row: 2,
            detail: "3 columns, want 10".to_owned(),
        }
    );
}

// --------------------------------------------------------------- engine.rs

#[test]
fn reference_double_refuses_an_empty_universe() {
    let recipe: Recipe =
        parse_recipe(recipe_json("{}").as_bytes(), 1, FIXTURE_COMMIT).expect("valid recipe");
    let mut engine = ReferenceDouble::new();
    let err = engine
        .configure(&recipe, None, &[])
        .expect_err("empty universe");
    assert_eq!(err, Refusal::Engine("empty universe".to_owned()));
}

// -------------------------------------------------------------- fixture.rs

#[test]
fn fixture_fails_when_the_root_is_a_file() {
    let s = Scratch::new("fx-root");
    let root = s.path().join("root-file");
    std::fs::write(&root, b"x").expect("write");
    assert!(write_demo_fixture(&root).is_err());
}

#[test]
fn fixture_fails_when_the_dynamic_folder_is_a_file() {
    let s = Scratch::new("fx-dyn");
    std::fs::write(s.path().join("demo_dynamic"), b"x").expect("write");
    assert!(write_demo_fixture(s.path()).is_err());
    // The fixed folder was written before the failure.
    assert!(
        s.path()
            .join("demo_fixed")
            .join("v1")
            .join(MANIFEST_NAME)
            .is_file()
    );
}

#[test]
fn fixture_fails_when_the_params_folder_is_a_file() {
    let s = Scratch::new("fx-params");
    let v1 = s.path().join("demo_dynamic").join("v1");
    std::fs::create_dir_all(&v1).expect("mkdir");
    std::fs::write(v1.join("params"), b"x").expect("write");
    assert!(write_demo_fixture(s.path()).is_err());
    assert!(v1.join("recipe.json").is_file());
}

#[test]
fn fixture_fails_when_the_universe_folder_is_a_file() {
    let s = Scratch::new("fx-univ");
    let dynamic = s.path().join("demo_dynamic");
    std::fs::create_dir_all(&dynamic).expect("mkdir");
    std::fs::write(dynamic.join("universe"), b"x").expect("write");
    assert!(write_demo_fixture(s.path()).is_err());
    assert!(
        dynamic
            .join("v1")
            .join("params")
            .join("2026-10-29")
            .join("params.json")
            .is_file()
    );
}

// --------------------------------------------------------------- loader.rs

#[test]
fn load_of_a_missing_root_finds_no_version() {
    let s = Scratch::new("ld-missing");
    let out = run_load(&s.path().join("absent"), "2026-10-29", &known_all);
    assert_eq!(
        refused_detail(&out),
        format!("version: {}", Refusal::NoVersion)
    );
}

#[test]
fn load_refuses_a_recipe_whose_name_does_not_match_its_folder() {
    let (s, fixed, _) = demo("ld-name");
    let renamed = s.path().join("renamed");
    std::fs::rename(&fixed, &renamed).expect("rename");
    let out = run_load(&renamed, "2026-10-29", &known_all);
    let d = refused_detail(&out);
    assert_eq!(out.events[0].version, Some(1));
    assert_eq!(
        d,
        "recipe: recipe is malformed: name \"demo_fixed\" does not match its folder \"renamed\""
    );
}

#[test]
fn load_notes_malformed_version_folders() {
    let (_s, fixed, _) = demo("ld-malformed");
    std::fs::create_dir_all(fixed.join("v01")).expect("mkdir");
    let out = run_load(&fixed, "2026-10-29", &known_all);
    let d = loaded_detail(&out);
    assert!(d.ends_with("; ignored malformed folders [\"v01\"]"), "{d}");
}

#[test]
fn load_refuses_a_fixed_instrument_unknown_today() {
    let (_s, fixed, _) = demo("ld-unknown");
    let out = run_load(&fixed, "2026-10-29", &known_none);
    assert_eq!(
        refused_detail(&out),
        "recipe: gate refused: instrument 2885/1 is not in today's instrument list"
    );
}

#[test]
fn load_refuses_a_dynamic_strategy_with_no_usable_universe() {
    let (_s, _, dynamic) = demo("ld-nouniv");
    // Every universe snapshot is dated after this trading day.
    let out = run_load(&dynamic, "2026-10-01", &known_all);
    assert_eq!(
        refused_detail(&out),
        "universe snapshot: gate refused: no universe snapshot on or before the trading day"
    );
}

#[test]
fn load_notes_malformed_universe_and_params_folders() {
    let (_s, _, dynamic) = demo("ld-junk");
    std::fs::create_dir_all(dynamic.join("universe").join("notadate")).expect("mkdir");
    std::fs::create_dir_all(dynamic.join("v1").join("params").join("junk")).expect("mkdir");
    let out = run_load(&dynamic, "2026-10-29", &known_all);
    let d = loaded_detail(&out);
    assert!(
        d.contains("; ignored malformed universe folders [\"notadate\"]"),
        "{d}"
    );
    assert!(
        d.contains("; ignored malformed params folders [\"junk\"]"),
        "{d}"
    );
}

#[test]
fn load_refuses_a_universe_whose_date_differs_from_its_folder() {
    let (_s, _, dynamic) = demo("ld-udate");
    let u = dynamic.join("universe");
    std::fs::rename(u.join("2026-10-29"), u.join("2026-10-28")).expect("rename");
    let out = run_load(&dynamic, "2026-10-29", &known_all);
    assert_eq!(
        refused_detail(&out),
        "universe snapshot: gate refused: universe.json date 2026-10-29 is in folder 2026-10-28"
    );
}

#[test]
fn load_refuses_a_dynamic_strategy_with_no_parameter_snapshot() {
    let (_s, _, dynamic) = demo("ld-noparams");
    std::fs::remove_dir_all(dynamic.join("v1").join("params")).expect("rm params");
    let out = run_load(&dynamic, "2026-10-29", &known_all);
    assert_eq!(
        refused_detail(&out),
        "parameter snapshot: gate refused: no parameter snapshot on or before the trading day"
    );
}

#[test]
fn load_refuses_params_whose_date_differs_from_its_folder() {
    let (_s, _, dynamic) = demo("ld-pdate");
    let p = dynamic.join("v1").join("params");
    std::fs::remove_dir_all(p.join("2026-10-29")).expect("rm");
    std::fs::rename(p.join("2026-10-28"), p.join("2026-10-29")).expect("rename");
    let out = run_load(&dynamic, "2026-10-29", &known_all);
    assert_eq!(
        refused_detail(&out),
        "parameter snapshot: gate refused: params.json date 2026-10-28 is in folder 2026-10-29"
    );
}

// ------------------------------------------------------------- manifest.rs

fn write_manifest(dir: &Path, json: &str) {
    std::fs::create_dir_all(dir).expect("mkdir");
    std::fs::write(dir.join(MANIFEST_NAME), json).expect("write manifest");
}

#[test]
fn manifest_with_a_foreign_schema_is_refused() {
    let s = Scratch::new("mf-schema");
    write_manifest(s.path(), r#"{"schema":"other/9","files":[]}"#);
    assert_eq!(
        verify_folder(s.path()).expect_err("schema"),
        Refusal::BadManifest("schema \"other/9\"".to_owned())
    );
}

#[test]
fn manifest_with_a_bad_sha256_is_refused() {
    let s = Scratch::new("mf-sha");
    write_manifest(
        s.path(),
        r#"{"schema":"tickvault-strategy-manifest/1","files":[{"name":"a.csv","sha256":"zz","bytes":1}]}"#,
    );
    assert_eq!(
        verify_folder(s.path()).expect_err("sha"),
        Refusal::BadManifest("bad sha256 for a.csv".to_owned())
    );
}

#[test]
fn manifest_listing_no_files_is_refused() {
    let s = Scratch::new("mf-empty");
    write_manifest(s.path(), &manifest_json_for(&[]));
    assert_eq!(
        verify_folder(s.path()).expect_err("empty"),
        Refusal::BadManifest("lists no files".to_owned())
    );
}

#[test]
fn manifest_that_is_a_directory_is_unreadable_not_missing() {
    let s = Scratch::new("mf-dir");
    std::fs::create_dir_all(s.path().join(MANIFEST_NAME)).expect("mkdir");
    match verify_folder(s.path()).expect_err("unreadable") {
        Refusal::Unreadable { name, detail } => {
            assert_eq!(name, MANIFEST_NAME);
            assert!(!detail.is_empty());
        }
        other => panic!("expected Unreadable, got {other:?}"),
    }
}

// --------------------------------------------------------------- recipe.rs

fn recipe_err(overrides: &str) -> Refusal {
    parse_recipe(recipe_json(overrides).as_bytes(), 1, FIXTURE_COMMIT).expect_err("refused")
}

#[test]
fn recipe_with_a_foreign_schema_is_refused() {
    assert_eq!(
        recipe_err(r#"{"schema":"x/1"}"#),
        Refusal::BadRecipe("schema \"x/1\"".to_owned())
    );
}

#[test]
fn recipe_with_a_short_commit_is_refused() {
    assert_eq!(
        recipe_err(r#"{"brutex_commit":"abc123"}"#),
        Refusal::BadRecipe("brutex_commit \"abc123\" is not a 40-char sha".to_owned())
    );
}

#[test]
fn recipe_with_no_timeframes_is_refused() {
    assert_eq!(
        recipe_err(r#"{"timeframes":[]}"#),
        Refusal::BadRecipe("empty name or timeframes".to_owned())
    );
    assert_eq!(
        recipe_err(r#"{"name":""}"#),
        Refusal::BadRecipe("empty name or timeframes".to_owned())
    );
}

#[test]
fn fixed_recipe_with_no_instruments_is_refused() {
    assert_eq!(
        recipe_err(r#"{"instruments":[]}"#),
        Refusal::BadRecipe("a fixed strategy lists no instruments".to_owned())
    );
}

#[test]
fn recipe_with_a_negative_sanity_fraction_is_refused() {
    assert_eq!(
        recipe_err(r#"{"sanity_limits":{"max_param_change_fraction":-0.5}}"#),
        Refusal::BadRecipe(
            "sanity_limits.max_param_change_fraction must be finite and >= 0".to_owned()
        )
    );
}

// --------------------------------------------------------------- replay.rs

#[test]
fn replay_names_an_exit_the_check_file_called_an_entry() {
    let bars: &[(&str, u64, u8, &str, f64)] = &[
        ("2026-10-27T09:15:00", 1, 1, "1m", 100.0),
        ("2026-10-27T09:16:00", 1, 1, "1m", 99.0),
    ];
    let (c, v, s) = toy_check_files(bars);
    // The toy rule emits Exit/Sell on the falling bar; claim Entry/Buy instead.
    let s = s.replace(",exit,sell,", ",entry,buy,");
    let candles = parse_candles("check_candles.csv", c.as_bytes()).expect("candles");
    let values = parse_values("check_values.csv", v.as_bytes()).expect("values");
    let signals = parse_signals("check_signals.csv", s.as_bytes()).expect("signals");
    let recipe = parse_recipe(recipe_json("{}").as_bytes(), 1, FIXTURE_COMMIT).expect("recipe");
    let mut engine = ReferenceDouble::new();
    engine
        .configure(
            &recipe,
            None,
            &[InstrumentKey {
                security_id: 1,
                exchange_segment: 1,
            }],
        )
        .expect("configure");
    let err =
        replay(&mut engine, &recipe.timeframes, &candles, &values, &signals).expect_err("mismatch");
    match err {
        Refusal::Replay(m) => {
            assert_eq!(m.file, "check_signals.csv");
            assert_eq!(m.row, 3);
            assert_eq!(m.field, "signal");
            assert_eq!(m.expected, "entry");
            assert_eq!(m.got, "exit");
        }
        other => panic!("expected Replay, got {other:?}"),
    }
}

// --------------------------------------------------------------- select.rs

#[test]
fn iso_date_with_non_digits_is_rejected() {
    assert!(!is_iso_date("2026-1a-01"));
    assert!(!is_iso_date("abcd-10-01"));
    assert!(!is_iso_date("2026-10-0x"));
    assert!(is_iso_date("2026-10-01"));
}

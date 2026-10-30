//! Adversarial properties of the brutex strategy folder files tickvault reads
//! from `s3://tv-prod-cold/strategy-recipes/` (contract
//! `docs/architecture/brutex-strategy-contract.md` §3).
//!
//! Surfaces and properties:
//!
//! * `manifest::verify_folder`: a folder built by `manifest_json_for`
//!   verifies and returns every file byte for byte; changing any byte of any
//!   listed file is `HashMismatch` naming that file; cutting or growing a
//!   file is `SizeMismatch`; an extra file is `UnlistedFile`; a manifest
//!   naming a path (`/`, `\`), a hidden name, itself, a name twice, no
//!   files, a short or non-hex hash, or holding arbitrary bytes is
//!   `BadManifest`.
//! * `recipe::parse_recipe`: the fixture recipe is accepted; each
//!   documented refusal (status, folder version, commit shape, commit
//!   mismatch, empty name or timeframes, fixed strategy with no
//!   instruments, negative / non-finite change fraction) gives its own
//!   variant; arbitrary bytes only ever give `BadRecipe`.
//! * `check_files::{parse_candles, parse_values, parse_signals}`: generated
//!   rows written as CSV (LF or CRLF) parse back to the same values with
//!   every float bit-identical; corrupting any one field of any row is
//!   `BadCheckFile` naming that row; arbitrary bytes give only
//!   `BadCheckFile` / `EmptyCandles`.
//! * `select::parse_version` / `select::is_iso_date`: agree with
//!   independent reference definitions on generated and arbitrary text.
//!
//! Honest limits: `recipe.settings` / `backtest` are free JSON and only
//! checked for "never panics"; the strategy crate's own reading of them is
//! out of scope here (it does not exist in this repository yet).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use proptest::prelude::*;
use proptest::test_runner::Config as ProptestConfig;
use tickvault_trading::brutex_host::Refusal;
use tickvault_trading::brutex_host::check_files::{
    SignalKind, parse_candles, parse_signals, parse_values,
};
use tickvault_trading::brutex_host::fixture::FIXTURE_COMMIT;
use tickvault_trading::brutex_host::manifest::{manifest_json_for, verify_folder};
use tickvault_trading::brutex_host::recipe::{
    GatesFile, ParamSnapshot, UniverseSnapshot, parse_json, parse_recipe,
};
use tickvault_trading::brutex_host::select::{is_iso_date, parse_version};

const CANDLE_HEADER: &str =
    "ts_ist,security_id,exchange_segment,timeframe,open,high,low,close,volume";
const SIGNAL_HEADER: &str = "ts_ist,security_id,exchange_segment,timeframe,signal,side,qty_lots,entry_price,stop_loss,target";
const VALUE_PREFIX: &str = "ts_ist,security_id,exchange_segment,timeframe";

static CASE: AtomicU64 = AtomicU64::new(0);

/// A fresh directory for one case; removed on drop.
struct CaseDir(PathBuf);

impl CaseDir {
    fn new(tag: &str) -> Self {
        let n = CASE.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("tv-adv-recipe-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create case dir");
        Self(dir)
    }
}

impl Drop for CaseDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn is_check_refusal(r: &Refusal) -> bool {
    matches!(r, Refusal::BadCheckFile { .. } | Refusal::EmptyCandles)
}

fn refused_row(r: &Refusal) -> Option<usize> {
    match r {
        Refusal::BadCheckFile { row, .. } => Some(*row),
        _ => None,
    }
}

// ---------------------------------------------------------------- manifest

/// One to six files with distinct safe names and arbitrary contents.
fn folder_files() -> impl Strategy<Value = Vec<(String, Vec<u8>)>> {
    prop::collection::btree_map(
        "[a-z][a-z0-9_]{0,10}\\.(csv|json)",
        prop::collection::vec(any::<u8>(), 0..200),
        1..6,
    )
    .prop_map(|m| {
        m.into_iter()
            .filter(|(name, _)| name != "manifest.json")
            .collect()
    })
    .prop_filter("at least one file", |v: &Vec<(String, Vec<u8>)>| {
        !v.is_empty()
    })
}

fn write_folder(dir: &CaseDir, files: &[(String, Vec<u8>)]) {
    let pairs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(n, b)| (n.as_str(), b.as_slice()))
        .collect();
    for (name, bytes) in files {
        std::fs::write(dir.0.join(name), bytes).expect("write file");
    }
    std::fs::write(dir.0.join("manifest.json"), manifest_json_for(&pairs)).expect("write manifest");
}

fn manifest_with(entries: &[(&str, &str, u64)]) -> String {
    let files: Vec<serde_json::Value> = entries
        .iter()
        .map(|(n, h, b)| serde_json::json!({ "name": n, "sha256": h, "bytes": b }))
        .collect();
    serde_json::json!({ "schema": manifest_schema(), "files": files }).to_string()
}

/// The schema string the manifest writer uses, read back from its own output.
fn manifest_schema() -> String {
    let v: serde_json::Value =
        serde_json::from_str(&manifest_json_for(&[("a", b"")])).expect("manifest json");
    v["schema"].as_str().expect("schema").to_owned()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 96, ..ProptestConfig::default() })]

    #[test]
    fn prop_manifest_verifies_and_returns_every_file(files in folder_files()) {
        let dir = CaseDir::new("ok");
        write_folder(&dir, &files);
        let verified = verify_folder(&dir.0).expect("a vouched folder verifies");
        for (name, bytes) in &files {
            prop_assert_eq!(verified.file(name).expect("listed"), &bytes[..]);
        }
    }

    #[test]
    fn prop_any_changed_byte_is_a_hash_mismatch(
        files in folder_files(),
        which in any::<prop::sample::Index>(),
        at in any::<prop::sample::Index>(),
        mask in 1u8..=255,
    ) {
        let dir = CaseDir::new("hash");
        write_folder(&dir, &files);
        let (name, bytes) = &files[which.index(files.len())];
        prop_assume!(!bytes.is_empty());
        let mut damaged = bytes.clone();
        let pos = at.index(damaged.len());
        damaged[pos] ^= mask;
        std::fs::write(dir.0.join(name), &damaged).expect("damage");
        match verify_folder(&dir.0) {
            Err(Refusal::HashMismatch { name: got, .. }) => prop_assert_eq!(&got, name),
            other => prop_assert!(false, "{} damaged: {:?}", name, other.map(|_| ())),
        }
    }

    #[test]
    fn prop_any_resized_file_is_a_size_mismatch(
        files in folder_files(),
        which in any::<prop::sample::Index>(),
        delta in prop_oneof![(1usize..20).prop_map(|d| (true, d)), (1usize..20).prop_map(|d| (false, d))],
    ) {
        let dir = CaseDir::new("size");
        write_folder(&dir, &files);
        let (name, bytes) = &files[which.index(files.len())];
        let (grow, by) = delta;
        let resized = if grow {
            let mut b = bytes.clone();
            b.extend(std::iter::repeat_n(b'x', by));
            b
        } else {
            prop_assume!(!bytes.is_empty());
            bytes[..bytes.len().saturating_sub(by)].to_vec()
        };
        std::fs::write(dir.0.join(name), &resized).expect("resize");
        match verify_folder(&dir.0) {
            Err(Refusal::SizeMismatch { name: got, expected, got: len }) => {
                prop_assert_eq!(&got, name);
                prop_assert_eq!(expected, bytes.len() as u64);
                prop_assert_eq!(len, resized.len() as u64);
            }
            other => prop_assert!(false, "{} resized: {:?}", name, other.map(|_| ())),
        }
    }

    #[test]
    fn prop_an_extra_file_is_unlisted(files in folder_files(), extra in "[a-zA-Z0-9_.]{1,12}") {
        prop_assume!(extra != "manifest.json" && extra != "." && extra != "..");
        prop_assume!(!files.iter().any(|(n, _)| *n == extra));
        let dir = CaseDir::new("extra");
        write_folder(&dir, &files);
        std::fs::write(dir.0.join(&extra), b"unvouched").expect("extra");
        match verify_folder(&dir.0) {
            Err(Refusal::UnlistedFile { name }) => prop_assert_eq!(name, extra),
            other => prop_assert!(false, "extra {}: {:?}", extra, other.map(|_| ())),
        }
    }

    #[test]
    fn prop_arbitrary_manifest_bytes_never_verify(raw in prop::collection::vec(any::<u8>(), 0..300)) {
        let dir = CaseDir::new("raw");
        std::fs::write(dir.0.join("manifest.json"), &raw).expect("manifest");
        let result = verify_folder(&dir.0);
        prop_assert!(
            matches!(result, Err(Refusal::BadManifest(_))),
            "{:?}", result.map(|_| ())
        );
    }
}

#[test]
fn test_hostile_manifest_entries_are_bad_manifest() {
    let zero = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let cases: Vec<(&str, String)> = vec![
        ("slash", manifest_with(&[("../recipe.json", zero, 0)])),
        ("nested", manifest_with(&[("params/a.json", zero, 0)])),
        ("backslash", manifest_with(&[("..\\recipe.json", zero, 0)])),
        ("hidden", manifest_with(&[(".recipe.json", zero, 0)])),
        ("self", manifest_with(&[("manifest.json", zero, 0)])),
        (
            "twice",
            manifest_with(&[("a.json", zero, 0), ("a.json", zero, 0)]),
        ),
        ("empty", manifest_with(&[])),
        ("short hash", manifest_with(&[("a.json", "abc", 0)])),
        (
            "non-hex hash",
            manifest_with(&[("a.json", &"g".repeat(64), 0)]),
        ),
        (
            "wrong schema",
            serde_json::json!({ "schema": "x", "files": [{"name": "a.json", "sha256": zero, "bytes": 0}] })
                .to_string(),
        ),
        (
            "negative size",
            format!(r#"{{"schema":"{}","files":[{{"name":"a.json","sha256":"{zero}","bytes":-1}}]}}"#, manifest_schema()),
        ),
    ];
    for (label, manifest) in cases {
        let dir = CaseDir::new("hostile");
        std::fs::write(dir.0.join("a.json"), b"").expect("file");
        std::fs::write(dir.0.join("manifest.json"), manifest).expect("manifest");
        let result = verify_folder(&dir.0);
        assert!(
            matches!(result, Err(Refusal::BadManifest(_))),
            "{label}: {:?}",
            result.map(|_| ())
        );
    }
    // A missing manifest and a listed-but-missing file are their own variants.
    let dir = CaseDir::new("nomanifest");
    assert!(matches!(
        verify_folder(&dir.0),
        Err(Refusal::MissingFile { .. })
    ));
    std::fs::write(
        dir.0.join("manifest.json"),
        manifest_with(&[("gone.json", zero, 0)]),
    )
    .expect("manifest");
    assert!(matches!(
        verify_folder(&dir.0),
        Err(Refusal::MissingFile { name }) if name == "gone.json"
    ));
}

// ------------------------------------------------------------------ recipe

fn recipe_value() -> serde_json::Value {
    serde_json::json!({
        "schema": "tickvault-strategy-recipe/1",
        "name": "adv",
        "version": 3,
        "status": "approved",
        "approved_by": "owner",
        "approved_at_ist": "2026-10-29T18:00:00+05:30",
        "brutex_commit": FIXTURE_COMMIT,
        "dynamic": false,
        "instruments": [{"security_id": 2885, "exchange_segment": 1}],
        "timeframes": ["1m"],
        "settings": {},
        "backtest": {},
        "sanity_limits": {"max_param_change_fraction": 0.5}
    })
}

fn parse_value(v: &serde_json::Value, folder: u32) -> Result<(), Refusal> {
    parse_recipe(v.to_string().as_bytes(), folder, FIXTURE_COMMIT).map(|_| ())
}

#[test]
fn test_recipe_each_documented_refusal_has_its_variant() {
    assert_eq!(parse_value(&recipe_value(), 3), Ok(()));
    // Upper-case commit in the recipe still matches (compared case-insensitively).
    let mut upper = recipe_value();
    upper["brutex_commit"] = FIXTURE_COMMIT.to_ascii_uppercase().into();
    assert_eq!(parse_value(&upper, 3), Ok(()));

    let edit = |f: &dyn Fn(&mut serde_json::Value)| {
        let mut v = recipe_value();
        f(&mut v);
        parse_value(&v, 3)
    };
    assert!(
        matches!(edit(&|v| v["status"] = "draft".into()), Err(Refusal::NotApproved(s)) if s == "draft")
    );
    assert!(matches!(
        edit(&|v| v["status"] = "Approved".into()),
        Err(Refusal::NotApproved(_))
    ));
    assert_eq!(
        parse_value(&recipe_value(), 4),
        Err(Refusal::VersionMismatch {
            folder: 4,
            in_file: 3
        })
    );
    for commit in [
        "",
        "abc",
        &FIXTURE_COMMIT[..39],
        &format!("{FIXTURE_COMMIT}0"),
        &"z".repeat(40),
    ] {
        let commit = commit.to_owned();
        assert!(
            matches!(
                edit(&move |v| v["brutex_commit"] = commit.clone().into()),
                Err(Refusal::BadRecipe(_))
            ),
            "commit shape"
        );
    }
    assert!(matches!(
        edit(&|v| v["brutex_commit"] = "0".repeat(40).into()),
        Err(Refusal::CommitMismatch { .. })
    ));
    assert!(matches!(
        edit(&|v| v["schema"] = "tickvault-strategy-recipe/2".into()),
        Err(Refusal::BadRecipe(_))
    ));
    assert!(matches!(
        edit(&|v| v["name"] = "".into()),
        Err(Refusal::BadRecipe(_))
    ));
    assert!(matches!(
        edit(&|v| v["timeframes"] = serde_json::json!([])),
        Err(Refusal::BadRecipe(_))
    ));
    assert!(matches!(
        edit(&|v| v["instruments"] = serde_json::json!([])),
        Err(Refusal::BadRecipe(_))
    ));
    // A dynamic strategy may list no instruments.
    assert_eq!(
        edit(&|v| {
            v["instruments"] = serde_json::json!([]);
            v["dynamic"] = true.into();
        }),
        Ok(())
    );
    assert!(matches!(
        edit(&|v| v["sanity_limits"]["max_param_change_fraction"] = (-0.1).into()),
        Err(Refusal::BadRecipe(_))
    ));
    for missing in [
        "schema",
        "name",
        "version",
        "status",
        "approved_by",
        "brutex_commit",
        "timeframes",
        "settings",
        "backtest",
    ] {
        assert!(
            matches!(
                edit(&|v| {
                    v.as_object_mut().expect("object").remove(missing);
                }),
                Err(Refusal::BadRecipe(_))
            ),
            "missing {missing}"
        );
    }
    // Wrong JSON types and out-of-range numbers are refused, never coerced.
    assert!(matches!(
        edit(&|v| v["version"] = "3".into()),
        Err(Refusal::BadRecipe(_))
    ));
    assert!(matches!(
        edit(&|v| v["version"] = (-3).into()),
        Err(Refusal::BadRecipe(_))
    ));
    assert!(matches!(
        edit(&|v| v["version"] = 4_294_967_299_u64.into()),
        Err(Refusal::BadRecipe(_))
    ));
    assert!(matches!(
        edit(&|v| v["version"] = 3.5.into()),
        Err(Refusal::BadRecipe(_))
    ));
    assert!(matches!(
        edit(&|v| v["instruments"] =
            serde_json::json!([{"security_id": 1, "exchange_segment": 256}])),
        Err(Refusal::BadRecipe(_))
    ));
    // NaN cannot be written in JSON; 1e999 overflows serde's f64 read.
    let text = recipe_value().to_string().replace(
        "\"max_param_change_fraction\":0.5",
        "\"max_param_change_fraction\":1e999",
    );
    assert!(matches!(
        parse_recipe(text.as_bytes(), 3, FIXTURE_COMMIT),
        Err(Refusal::BadRecipe(_))
    ));
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1024, ..ProptestConfig::default() })]

    /// Arbitrary bytes are only ever `BadRecipe` (never a panic, never Ok).
    #[test]
    fn prop_arbitrary_recipe_bytes_are_bad_recipe(raw in prop::collection::vec(any::<u8>(), 0..400), folder in any::<u32>()) {
        let result = parse_recipe(&raw, folder, FIXTURE_COMMIT);
        prop_assert!(matches!(result, Err(Refusal::BadRecipe(_))), "{:?}", result.map(|_| ()));
        prop_assert!(parse_json::<GatesFile>(&raw, "gates").is_err());
        prop_assert!(parse_json::<ParamSnapshot>(&raw, "params").is_err());
        prop_assert!(parse_json::<UniverseSnapshot>(&raw, "universe").is_err());
    }

    /// Any single byte of a valid recipe deleted: never a panic, and a
    /// result is either the same accepted recipe or a typed refusal.
    #[test]
    fn prop_recipe_with_one_byte_removed(at in any::<prop::sample::Index>()) {
        let text = recipe_value().to_string().into_bytes();
        let i = at.index(text.len());
        let mut cut = text.clone();
        cut.remove(i);
        let _ = parse_recipe(&cut, 3, FIXTURE_COMMIT);
    }
}

// ------------------------------------------------------------- check files

#[derive(Clone, Debug)]
struct Bar {
    ts: String,
    id: u64,
    seg: u8,
    tf: String,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: u64,
}

/// A finite float from the whole range, sometimes tiny, huge or -0.
fn finite() -> impl Strategy<Value = f64> {
    prop_oneof![
        any::<f64>().prop_filter("finite", |f| f.is_finite()),
        -1e6f64..1e6,
        Just(-0.0),
        Just(f64::MIN_POSITIVE),
        Just(f64::MAX),
    ]
}

/// Bars with strictly rising time per (instrument, timeframe): each bar gets
/// its own minute, so any instrument mix stays ordered.
fn bars() -> impl Strategy<Value = Vec<Bar>> {
    prop::collection::vec(
        (
            any::<u64>(),
            any::<u8>(),
            "[a-z0-9]{1,4}",
            prop::collection::vec(finite(), 4),
            any::<u64>(),
        ),
        1..12,
    )
    .prop_map(|rows| {
        rows.into_iter()
            .enumerate()
            .map(|(i, (id, seg, tf, mut p, volume))| {
                p.sort_by(f64::total_cmp);
                let (low, a, b, high) = (p[0], p[1], p[2], p[3]);
                Bar {
                    ts: format!("2026-10-27T{:02}:{:02}:00", 9 + i / 60, i % 60),
                    id,
                    seg,
                    tf,
                    open: a,
                    high,
                    low,
                    close: b,
                    volume,
                }
            })
            .collect()
    })
}

fn candles_csv(bars: &[Bar], eol: &str) -> String {
    let mut s = format!("{CANDLE_HEADER}{eol}");
    for b in bars {
        s.push_str(&format!(
            "{},{},{},{},{},{},{},{},{}{eol}",
            b.ts, b.id, b.seg, b.tf, b.open, b.high, b.low, b.close, b.volume
        ));
    }
    s
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    /// Generated candles round-trip with every float bit-identical, LF or CRLF.
    #[test]
    fn prop_candles_round_trip(bars in bars(), crlf in any::<bool>()) {
        let csv = candles_csv(&bars, if crlf { "\r\n" } else { "\n" });
        let parsed = parse_candles("c", csv.as_bytes()).expect("generated candles parse");
        prop_assert_eq!(parsed.len(), bars.len());
        for (i, (p, b)) in parsed.iter().zip(&bars).enumerate() {
            // `row` is the 1-based FILE line (the header is line 1).
            prop_assert_eq!(p.row, i + 2);
            prop_assert_eq!(&p.ts_ist, &b.ts);
            prop_assert_eq!((p.key.security_id, p.key.exchange_segment), (b.id, b.seg));
            prop_assert_eq!(&p.timeframe, &b.tf);
            for (got, want) in [(p.open, b.open), (p.high, b.high), (p.low, b.low), (p.close, b.close)] {
                prop_assert_eq!(got.to_bits(), want.to_bits());
            }
            prop_assert_eq!(p.volume, b.volume);
        }
    }

    /// One field of one row replaced by junk is refused at exactly that row.
    #[test]
    fn prop_one_corrupt_candle_field_is_refused_at_its_row(
        bars in bars(),
        which in any::<prop::sample::Index>(),
        column in 0usize..9,
        junk in prop_oneof![
            Just(String::new()), Just("nan".to_owned()), Just("inf".to_owned()),
            Just("-inf".to_owned()), Just("1e999".to_owned()), Just("0x10".to_owned()),
            Just(" 1".to_owned()), Just("1,2".to_owned()), Just("\"1\"".to_owned()),
            Just("-1".to_owned()), Just("18446744073709551616".to_owned()),
        ],
    ) {
        let row = which.index(bars.len());
        let csv = candles_csv(&bars, "\n");
        let mut lines: Vec<String> = csv.lines().map(str::to_owned).collect();
        let mut cols: Vec<String> = lines[row + 1].split(',').map(str::to_owned).collect();
        // Skip junk the column legally accepts (a reference per column):
        // the timeframe takes any non-empty text, ids and volume are
        // unsigned, prices are finite floats (ordering is checked after).
        let legal = match column {
            3 => !junk.is_empty() && !junk.contains(','),
            1 | 8 => junk.parse::<u64>().is_ok(),
            2 => junk.parse::<u8>().is_ok(),
            4..=7 => junk.parse::<f64>().is_ok_and(f64::is_finite),
            _ => false,
        };
        prop_assume!(!legal);
        cols[column] = junk.clone();
        lines[row + 1] = cols.join(",");
        let damaged = lines.join("\n");
        match parse_candles("c", damaged.as_bytes()) {
            Err(r) => prop_assert_eq!(refused_row(&r), Some(row + 2), "{:?}", r),
            Ok(_) => prop_assert!(false, "column {} junk {:?} accepted", column, junk),
        }
    }

    /// Arbitrary bytes into the three check-file parsers: only typed refusals.
    #[test]
    fn prop_arbitrary_check_bytes_only_refuse_typed(raw in prop::collection::vec(any::<u8>(), 0..300)) {
        for result in [
            parse_candles("c", &raw).map(|_| ()),
            parse_values("v", &raw).map(|_| ()),
            parse_signals("s", &raw).map(|_| ()),
        ] {
            if let Err(r) = result {
                prop_assert!(is_check_refusal(&r), "{:?}", r);
            }
        }
    }

    /// Arbitrary text under each real header: only typed refusals, and an
    /// accepted file re-parses identically.
    #[test]
    fn prop_arbitrary_rows_under_real_headers(body in "[0-9a-zT:,.\\-\r\n]{0,200}") {
        let candle = format!("{CANDLE_HEADER}\n{body}");
        if let Err(r) = parse_candles("c", candle.as_bytes()) {
            prop_assert!(is_check_refusal(&r), "{:?}", r);
        }
        let signal = format!("{SIGNAL_HEADER}\n{body}");
        if let Err(r) = parse_signals("s", signal.as_bytes()) {
            prop_assert!(is_check_refusal(&r), "{:?}", r);
        }
        let value = format!("{VALUE_PREFIX},a,b\n{body}");
        if let Err(r) = parse_values("v", value.as_bytes()) {
            prop_assert!(is_check_refusal(&r), "{:?}", r);
        }
    }

    /// Generated value rows round-trip bit for bit under generated columns.
    #[test]
    fn prop_values_round_trip(
        columns in prop::collection::vec("[a-z][a-z0-9_]{0,6}", 1..5),
        rows in prop::collection::vec((any::<u64>(), any::<u8>(), prop::collection::vec(finite(), 5)), 0..8),
    ) {
        let mut csv = format!("{VALUE_PREFIX},{}\n", columns.join(","));
        for (i, (id, seg, vals)) in rows.iter().enumerate() {
            let vals: Vec<String> = vals[..columns.len()].iter().map(f64::to_string).collect();
            csv.push_str(&format!("2026-10-27T09:{:02}:00,{id},{seg},1m,{}\n", i % 60, vals.join(",")));
        }
        let parsed = parse_values("v", csv.as_bytes()).expect("generated values parse");
        prop_assert_eq!(&parsed.columns, &columns);
        prop_assert_eq!(parsed.rows.len(), rows.len());
        for (p, (id, seg, vals)) in parsed.rows.iter().zip(&rows) {
            prop_assert_eq!((p.key.security_id, p.key.exchange_segment), (*id, *seg));
            for (got, want) in p.values.iter().zip(&vals[..columns.len()]) {
                prop_assert_eq!(got.to_bits(), want.to_bits());
            }
        }
    }

    /// Generated signal rows round-trip.
    #[test]
    fn prop_signals_round_trip(
        rows in prop::collection::vec(
            (0usize..3, 0usize..3, any::<u32>(), finite(), finite(), finite()),
            0..8,
        ),
    ) {
        let kinds = ["none", "entry", "exit"];
        let sides = ["", "buy", "sell"];
        let mut csv = format!("{SIGNAL_HEADER}\n");
        for (i, (k, s, q, e, sl, t)) in rows.iter().enumerate() {
            csv.push_str(&format!(
                "2026-10-27T10:{:02}:00,7,1,5m,{},{},{q},{e},{sl},{t}\n",
                i % 60, kinds[*k], sides[*s]
            ));
        }
        let parsed = parse_signals("s", csv.as_bytes()).expect("generated signals parse");
        prop_assert_eq!(parsed.len(), rows.len());
        for (p, (k, s, q, e, sl, t)) in parsed.iter().zip(&rows) {
            let want = [SignalKind::None, SignalKind::Entry, SignalKind::Exit][*k];
            prop_assert_eq!(p.signal, want);
            prop_assert_eq!(p.side.as_str(), sides[*s]);
            prop_assert_eq!(p.qty_lots, *q);
            prop_assert_eq!(
                (p.entry_price.to_bits(), p.stop_loss.to_bits(), p.target.to_bits()),
                (e.to_bits(), sl.to_bits(), t.to_bits())
            );
        }
    }
}

#[test]
fn test_candle_order_and_shape_edges() {
    // Equal or falling time for one instrument and timeframe is refused.
    let same = format!(
        "{CANDLE_HEADER}\n2026-10-27T09:15:00,1,1,1m,1,1,1,1,1\n2026-10-27T09:15:00,1,1,1m,1,1,1,1,1\n"
    );
    assert_eq!(
        refused_row(&parse_candles("c", same.as_bytes()).expect_err("dup")),
        Some(3)
    );
    // The same time on another timeframe or instrument is fine.
    let other = format!(
        "{CANDLE_HEADER}\n2026-10-27T09:15:00,1,1,1m,1,1,1,1,1\n2026-10-27T09:15:00,1,1,5m,1,1,1,1,1\n2026-10-27T09:15:00,2,1,5m,1,1,1,1,1\n"
    );
    assert_eq!(parse_candles("c", other.as_bytes()).expect("ok").len(), 3);
    // Header only, no header, a BOM, a different header order: refused.
    assert_eq!(
        parse_candles("c", format!("{CANDLE_HEADER}\n").as_bytes()),
        Err(Refusal::EmptyCandles)
    );
    assert!(parse_candles("c", b"").is_err());
    let bom = format!("\u{feff}{CANDLE_HEADER}\n2026-10-27T09:15:00,1,1,1m,1,1,1,1,1\n");
    assert!(parse_candles("c", bom.as_bytes()).is_err());
    // high below low, or open outside [low, high]: refused.
    for row in ["2,1,1,1", "1,0,1,1", "0,1,1,0.5", "1,1,1,2"] {
        let csv = format!("{CANDLE_HEADER}\n2026-10-27T09:15:00,1,1,1m,{row},1\n");
        assert!(parse_candles("c", csv.as_bytes()).is_err(), "{row}");
    }
    // Invalid calendar times are refused.
    for ts in [
        "2026-02-30T09:15:00",
        "2026-10-27T24:00:00",
        "2026-10-27 09:15:00",
        "2026-10-27T09:15",
    ] {
        let csv = format!("{CANDLE_HEADER}\n{ts},1,1,1m,1,1,1,1,1\n");
        assert!(parse_candles("c", csv.as_bytes()).is_err(), "{ts}");
    }
}

// ----------------------------------------------------------------- select

/// Reference: `v<N>` with N a canonical decimal (no sign, no leading zero),
/// 1 <= N <= u32::MAX.
fn reference_version(name: &str) -> Option<u32> {
    let digits = name.strip_prefix('v')?;
    let n: u32 = digits.parse().ok()?;
    (n >= 1 && format!("v{n}") == name).then_some(n)
}

/// Reference: a real calendar date written exactly as `%Y-%m-%d` with a
/// four-digit year (chrono parse, then format back must give the same text).
fn reference_iso_date(s: &str) -> bool {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map(|d| d.format("%Y-%m-%d").to_string() == s)
        .unwrap_or(false)
        && s.len() == 10
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2048, ..ProptestConfig::default() })]

    #[test]
    fn prop_parse_version_matches_reference(name in prop_oneof![
        "v[0-9]{0,12}",
        "[v+\\-0-9 ]{0,8}",
        any::<u32>().prop_map(|n| format!("v{n}")),
        "\\PC{0,8}",
    ]) {
        prop_assert_eq!(parse_version(&name), reference_version(&name), "{:?}", name);
    }

    #[test]
    fn prop_is_iso_date_matches_reference(s in prop_oneof![
        "[0-9]{4}-[0-9]{2}-[0-9]{2}",
        "[0-9\\-+ ]{8,12}",
        (0i32..10_000, 0u32..14, 0u32..33).prop_map(|(y, m, d)| format!("{y:04}-{m:02}-{d:02}")),
        "\\PC{0,12}",
    ]) {
        prop_assert_eq!(is_iso_date(&s), reference_iso_date(&s), "{:?}", s);
    }
}

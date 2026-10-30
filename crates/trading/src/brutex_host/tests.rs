//! Extreme-case tests for the brutex strategy host (owner brief 2026-10-29):
//! hash mismatch, missing file, version gap, empty candles, one bar, a signal
//! mismatch on the last bar, two faults at once, out-of-the-box cases, the
//! snapshot gates, the swap rule and the audit log. Fixtures are written to a
//! fresh temp folder per test from the test double's documented toy rule.

// O(1) EXEMPT: begin — test module (compiled only under #[cfg(test)] from mod.rs).
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::audit::{AuditKind, CollectAlerts, append};
use super::check_files::parse_candles;
use super::engine::ReferenceDouble;
use super::loader::{Accepted, LoadContext, LoadOutcome, load};
use super::manifest::{manifest_json_for, verify_folder};
use super::recipe::RECIPE_FILE;
use super::select::{is_iso_date, parse_version, pick_version, usable_snapshot_dates};
use super::swap::{apply, may_swap};
use super::{InstrumentKey, Refusal};

const COMMIT: &str = "4bbd37df547b38f47a14d8e1b970df85540a395f";
const NAME: &str = "demo";

static SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_root() -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir()
        .join(format!("tv-brutex-host-{}-{n}", std::process::id()))
        .join(NAME);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// (ts, security_id, segment, timeframe, close)
type Bar = (&'static str, u64, u8, &'static str, f64);

/// The three check files the test double's rule produces for `bars`.
fn check_files(bars: &[Bar]) -> (String, String, String) {
    let mut c =
        String::from("ts_ist,security_id,exchange_segment,timeframe,open,high,low,close,volume\n");
    let mut v = String::from("ts_ist,security_id,exchange_segment,timeframe,close,prev_close\n");
    let mut s = String::from(
        "ts_ist,security_id,exchange_segment,timeframe,signal,side,qty_lots,entry_price,stop_loss,target\n",
    );
    let mut prev: std::collections::HashMap<(u64, u8, &str), f64> =
        std::collections::HashMap::with_capacity(8);
    for &(ts, id, seg, tf, close) in bars {
        c.push_str(&format!(
            "{ts},{id},{seg},{tf},{close},{},{},{close},1000\n",
            close + 1.0,
            close - 1.0
        ));
        let p = prev.insert((id, seg, tf), close);
        v.push_str(&format!(
            "{ts},{id},{seg},{tf},{close},{}\n",
            p.unwrap_or(0.0)
        ));
        match p {
            Some(p) if close > p => s.push_str(&format!(
                "{ts},{id},{seg},{tf},entry,buy,1,0,{},{}\n",
                close - 1.0,
                close + 2.0
            )),
            Some(p) if close < p => s.push_str(&format!(
                "{ts},{id},{seg},{tf},exit,sell,1,0,{},{}\n",
                close - 1.0,
                close + 2.0
            )),
            _ => s.push_str(&format!("{ts},{id},{seg},{tf},none,,0,0,0,0\n")),
        }
    }
    (c, v, s)
}

fn recipe(version: u32, dynamic: bool) -> String {
    let instruments = if dynamic {
        "[]"
    } else {
        r#"[{"security_id":2885,"exchange_segment":1}]"#
    };
    format!(
        r#"{{"schema":"tickvault-strategy-recipe/1","name":"{NAME}","version":{version},"status":"approved","approved_by":"Parthiban","approved_at_ist":"2026-10-29T18:00:00+05:30","brutex_commit":"{COMMIT}","dynamic":{dynamic},"instruments":{instruments},"timeframes":["1m","5m"],"settings":{{}},"backtest":{{"trades":3}},"sanity_limits":{{"max_param_change_fraction":0.5}}}}"#
    )
}

/// Writes files plus a manifest that vouches for them.
fn write_folder(dir: &Path, files: &[(&str, String)]) {
    std::fs::create_dir_all(dir).unwrap();
    let mut pairs: Vec<(&str, &[u8])> = Vec::new();
    for (n, b) in files {
        std::fs::write(dir.join(n), b).unwrap();
        pairs.push((n, b.as_bytes()));
    }
    std::fs::write(dir.join("manifest.json"), manifest_json_for(&pairs)).unwrap();
}

const BARS: &[Bar] = &[
    ("2026-10-27T09:15:00", 2885, 1, "1m", 100.5),
    ("2026-10-27T09:16:00", 2885, 1, "1m", 101.25),
    ("2026-10-27T09:17:00", 2885, 1, "1m", 100.75),
    ("2026-10-27T09:18:00", 2885, 1, "1m", 100.75),
];

fn write_version(root: &Path, version: u32, dynamic: bool, bars: &[Bar]) -> PathBuf {
    let (c, v, s) = check_files(bars);
    let dir = root.join(format!("v{version}"));
    write_folder(
        &dir,
        &[
            (RECIPE_FILE, recipe(version, dynamic)),
            ("check_candles.csv", c),
            ("check_values.csv", v),
            ("check_signals.csv", s),
        ],
    );
    dir
}

fn write_params(vdir: &Path, date: &str, k: f64, passed: bool) {
    let (c, v, s) = check_files(BARS);
    write_folder(
        &vdir.join("params").join(date),
        &[
            (
                "params.json",
                format!(r#"{{"date":"{date}","params":{{"k":{k}}}}}"#),
            ),
            (
                "gates.json",
                format!(
                    r#"{{"passed":{passed},"window_from":"2026-07-01","window_to":"2026-10-28"}}"#
                ),
            ),
            ("check_candles.csv", c),
            ("check_values.csv", v),
            ("check_signals.csv", s),
        ],
    );
}

fn write_universe(root: &Path, date: &str, instruments: &str) {
    write_folder(
        &root.join("universe").join(date),
        &[(
            "universe.json",
            format!(r#"{{"date":"{date}","instruments":{instruments}}}"#),
        )],
    );
}

fn known(_: InstrumentKey) -> bool {
    true
}

fn run(root: &Path, previous: Option<&Accepted>) -> (LoadOutcome, CollectAlerts) {
    let ctx = LoadContext {
        built_commit: COMMIT,
        trading_day: "2026-10-30",
        at_ist: "2026-10-30T08:40:00+05:30",
        is_known: &known,
    };
    let mut alerts = CollectAlerts::default();
    let mut engine = ReferenceDouble::new();
    let out = load(root, &ctx, &mut engine, previous, &mut alerts);
    (out, alerts)
}

fn refusal_text(out: &LoadOutcome) -> String {
    out.events
        .iter()
        .find(|e| e.kind == AuditKind::Refused)
        .map(|e| e.detail.clone())
        .unwrap_or_default()
}

fn accepted(root: &Path) -> Accepted {
    let (out, alerts) = run(root, None);
    assert!(alerts.alerts.is_empty(), "{:?}", alerts.alerts);
    out.live.expect("accepted")
}

#[test]
fn test_load_and_replay_accept_a_fixed_strategy_bit_for_bit() {
    let root = temp_root();
    write_version(&root, 1, false, BARS);
    let (out, alerts) = run(&root, None);
    assert!(alerts.alerts.is_empty());
    let live = out.live.unwrap();
    assert!(out.changed);
    assert_eq!(live.version, 1);
    assert_eq!(live.replayed.bars, 4);
    assert_eq!(live.replayed.values_checked, 8);
    assert_eq!(live.replayed.signals_checked, 4);
    assert_eq!(out.events[0].kind, AuditKind::Loaded);
}

#[test]
fn test_manifest_hash_mismatch_refuses_and_keeps_previous() {
    let root = temp_root();
    let v1 = write_version(&root, 1, false, BARS);
    let prev = accepted(&root);
    write_version(&root, 2, false, BARS);
    // Change one byte of v2's candles after the manifest was written (same length).
    let p = root.join("v2/check_candles.csv");
    let mut b = std::fs::read(&p).unwrap();
    let i = b.iter().rposition(|c| *c == b'5').unwrap();
    b[i] = b'6';
    std::fs::write(&p, b).unwrap();
    let (out, alerts) = run(&root, Some(&prev));
    assert!(
        refusal_text(&out).contains("sha256"),
        "{}",
        refusal_text(&out)
    );
    assert_eq!(out.live.as_ref().map(|a| a.version), Some(1));
    assert!(!out.changed);
    assert!(out.events.iter().any(|e| e.kind == AuditKind::KeptPrevious));
    assert_eq!(alerts.alerts.len(), 1);
    drop(v1);
}

#[test]
fn test_missing_file_refuses() {
    let root = temp_root();
    write_version(&root, 1, false, BARS);
    std::fs::remove_file(root.join("v1/check_signals.csv")).unwrap();
    let (out, _) = run(&root, None);
    assert!(out.live.is_none());
    assert!(
        refusal_text(&out).contains("check_signals.csv is listed in the manifest but missing"),
        "{}",
        refusal_text(&out)
    );
}

#[test]
fn test_version_gap_picks_newest_and_logs_gap() {
    let root = temp_root();
    write_version(&root, 1, false, BARS);
    write_version(&root, 3, false, BARS);
    let (out, _) = run(&root, None);
    assert_eq!(out.live.as_ref().map(|a| a.version), Some(3));
    assert!(
        out.events[0].detail.contains("version gap: [2]"),
        "{}",
        out.events[0].detail
    );
}

#[test]
fn test_empty_candle_file_refuses() {
    let root = temp_root();
    write_version(&root, 1, false, &[]);
    let (out, _) = run(&root, None);
    assert!(
        refusal_text(&out).contains("check candle file is empty"),
        "{}",
        refusal_text(&out)
    );
}

#[test]
fn test_one_bar_file_replays() {
    let root = temp_root();
    write_version(&root, 1, false, &BARS[..1]);
    let live = accepted(&root);
    assert_eq!(live.replayed.bars, 1);
    assert_eq!(live.replayed.signals_checked, 1);
}

#[test]
fn test_signal_mismatch_on_last_bar_names_bar_and_field() {
    let root = temp_root();
    let (c, v, s) = check_files(BARS);
    // Last bar is flat (no signal); claim an entry instead.
    let s = s.replace(
        "2026-10-27T09:18:00,2885,1,1m,none,,0,0,0,0",
        "2026-10-27T09:18:00,2885,1,1m,entry,buy,1,0,99.75,102.75",
    );
    write_folder(
        &root.join("v1"),
        &[
            (RECIPE_FILE, recipe(1, false)),
            ("check_candles.csv", c),
            ("check_values.csv", v),
            ("check_signals.csv", s),
        ],
    );
    let (out, _) = run(&root, None);
    let t = refusal_text(&out);
    assert!(t.contains("check_signals.csv row 5 bar 2026-10-27T09:18:00 instrument 2885/1 field signal: expected entry, got none"), "{t}");
}

#[test]
fn test_two_faults_at_once_report_the_first_check_that_fails() {
    let root = temp_root();
    let (c, v, s) = check_files(BARS);
    let s = s.replace("exit,sell", "entry,buy"); // replay fault
    write_folder(
        &root.join("v1"),
        &[
            (RECIPE_FILE, recipe(1, false)),
            ("check_candles.csv", c),
            ("check_values.csv", v),
            ("check_signals.csv", s),
        ],
    );
    std::fs::remove_file(root.join("v1/check_values.csv")).unwrap(); // manifest fault
    let (out, _) = run(&root, None);
    let t = refusal_text(&out);
    assert!(
        t.contains("check_values.csv is listed in the manifest but missing"),
        "{t}"
    );
    assert!(!t.contains("replay"), "{t}");
}

#[test]
fn test_out_of_box_unlisted_extra_file_refuses() {
    let root = temp_root();
    write_version(&root, 1, false, BARS);
    std::fs::write(root.join("v1/run.sh"), "rm -rf /").unwrap();
    let (out, _) = run(&root, None);
    assert!(
        refusal_text(&out).contains("run.sh is not listed"),
        "{}",
        refusal_text(&out)
    );
}

#[test]
fn test_out_of_box_negative_zero_differs_from_zero() {
    let root = temp_root();
    let (c, v, s) = check_files(&BARS[..1]);
    let v = v.replace(",100.5,0\n", ",100.5,-0\n");
    write_folder(
        &root.join("v1"),
        &[
            (RECIPE_FILE, recipe(1, false)),
            ("check_candles.csv", c),
            ("check_values.csv", v),
            ("check_signals.csv", s),
        ],
    );
    let (out, _) = run(&root, None);
    assert!(
        refusal_text(&out).contains("field prev_close: expected -0, got 0"),
        "{}",
        refusal_text(&out)
    );
}

#[test]
fn test_commit_mismatch_refuses() {
    let root = temp_root();
    let (c, v, s) = check_files(BARS);
    let r = recipe(1, false).replace(COMMIT, "0000000000000000000000000000000000000000");
    write_folder(
        &root.join("v1"),
        &[
            (RECIPE_FILE, r),
            ("check_candles.csv", c),
            ("check_values.csv", v),
            ("check_signals.csv", s),
        ],
    );
    let (out, _) = run(&root, None);
    assert!(
        refusal_text(&out).contains("tickvault is built against"),
        "{}",
        refusal_text(&out)
    );
}

#[test]
fn test_parse_recipe_refuses_unapproved_or_wrong_version() {
    let root = temp_root();
    let (c, v, s) = check_files(BARS);
    let r = recipe(1, false).replace(r#""status":"approved""#, r#""status":"draft""#);
    write_folder(
        &root.join("v1"),
        &[
            (RECIPE_FILE, r),
            ("check_candles.csv", c.clone()),
            ("check_values.csv", v.clone()),
            ("check_signals.csv", s.clone()),
        ],
    );
    assert!(refusal_text(&run(&root, None).0).contains("not \"approved\""));
    let root = temp_root();
    write_folder(
        &root.join("v2"),
        &[
            (RECIPE_FILE, recipe(1, false)),
            ("check_candles.csv", c),
            ("check_values.csv", v),
            ("check_signals.csv", s),
        ],
    );
    assert!(refusal_text(&run(&root, None).0).contains("recipe says version 1, its folder is v2"));
}

#[test]
fn test_no_version_with_nothing_live_alerts_once() {
    let root = temp_root();
    std::fs::create_dir_all(root.join("v0")).unwrap();
    let (out, alerts) = run(&root, None);
    assert!(out.live.is_none());
    assert_eq!(alerts.alerts.len(), 1);
    assert!(refusal_text(&out).contains("no approved version"));
}

#[test]
fn test_dynamic_strategy_accepts_newest_usable_snapshots_and_ignores_future_ones() {
    let root = temp_root();
    let vdir = write_version(&root, 1, true, BARS);
    write_params(&vdir, "2026-10-28", 1.0, true);
    write_params(&vdir, "2026-10-29", 1.2, true);
    write_params(&vdir, "2026-10-31", 99.0, true); // after the trading day: ignored
    write_universe(
        &root,
        "2026-10-29",
        r#"[{"security_id":2885,"exchange_segment":1}]"#,
    );
    let live = accepted(&root);
    assert_eq!(
        live.params.as_ref().map(|(d, _)| d.as_str()),
        Some("2026-10-29")
    );
    assert_eq!(live.universe_date.as_deref(), Some("2026-10-29"));
    assert_eq!(live.replayed.bars, 8);
}

#[test]
fn test_dynamic_snapshot_failing_sanity_keeps_previous_snapshot() {
    let root = temp_root();
    let vdir = write_version(&root, 1, true, BARS);
    write_params(&vdir, "2026-10-28", 1.0, true);
    write_universe(
        &root,
        "2026-10-28",
        r#"[{"security_id":2885,"exchange_segment":1}]"#,
    );
    let prev = accepted(&root);
    write_params(&vdir, "2026-10-29", 4.0, true); // +300% > 50%
    let (out, alerts) = run(&root, Some(&prev));
    assert!(
        refusal_text(&out).contains("parameter k moved 300.0%"),
        "{}",
        refusal_text(&out)
    );
    assert_eq!(
        out.live
            .as_ref()
            .and_then(|a| a.params.as_ref())
            .map(|(d, _)| d.as_str()),
        Some("2026-10-28")
    );
    assert_eq!(alerts.alerts.len(), 1);
}

#[test]
fn test_dynamic_winner_gate_failure_refuses() {
    let root = temp_root();
    let vdir = write_version(&root, 1, true, BARS);
    write_params(&vdir, "2026-10-29", 1.0, false);
    write_universe(
        &root,
        "2026-10-29",
        r#"[{"security_id":2885,"exchange_segment":1}]"#,
    );
    assert!(refusal_text(&run(&root, None).0).contains("Winner Gate failed"));
}

#[test]
fn test_dynamic_universe_empty_or_duplicate_refuses() {
    for (u, want) in [
        ("[]", "universe is empty"),
        (
            r#"[{"security_id":1,"exchange_segment":1},{"security_id":1,"exchange_segment":1}]"#,
            "listed twice",
        ),
    ] {
        let root = temp_root();
        let vdir = write_version(&root, 1, true, BARS);
        write_params(&vdir, "2026-10-29", 1.0, true);
        write_universe(&root, "2026-10-29", u);
        assert!(refusal_text(&run(&root, None).0).contains(want), "{want}");
    }
}

#[test]
fn test_same_id_in_two_segments_is_two_instruments() {
    // I-P1-11: 27 on IDX_I and 27 on NSE_EQ are different instruments, never duplicates.
    let root = temp_root();
    let vdir = write_version(&root, 1, true, BARS);
    write_params(&vdir, "2026-10-29", 1.0, true);
    write_universe(
        &root,
        "2026-10-29",
        r#"[{"security_id":27,"exchange_segment":0},{"security_id":27,"exchange_segment":1}]"#,
    );
    assert_eq!(accepted(&root).instruments.len(), 2);
}

#[test]
fn test_may_swap_and_apply_only_before_the_open_on_a_trading_day() {
    assert!(may_swap(true, 8 * 3600 + 59 * 60 + 59).is_ok());
    assert!(may_swap(true, 9 * 3600).is_err());
    assert!(may_swap(true, 12 * 3600).is_err());
    assert!(may_swap(false, 12 * 3600).is_ok());
    assert!(may_swap(false, 86_400).is_err());
    let root = temp_root();
    write_version(&root, 1, false, BARS);
    let (out, _) = run(&root, None);
    let (live, ev) = apply(out, None, true, 10 * 3600, "t");
    assert!(live.is_none());
    assert_eq!(ev.last().map(|e| e.kind), Some(AuditKind::SwapRefused));
    let (out, _) = run(&root, None);
    let (live, ev) = apply(out, None, true, 8 * 3600, "t");
    assert_eq!(live.map(|a| a.version), Some(1));
    assert_eq!(ev.last().map(|e| e.kind), Some(AuditKind::Swapped));
}

#[test]
fn test_reloading_the_same_version_is_not_a_change() {
    let root = temp_root();
    write_version(&root, 1, false, BARS);
    let prev = accepted(&root);
    let (out, _) = run(&root, Some(&prev));
    assert!(!out.changed);
    let (live, ev) = apply(out, Some(&prev), true, 8 * 3600, "t");
    assert_eq!(live.as_ref(), Some(&prev));
    assert!(ev.iter().all(|e| e.kind != AuditKind::Swapped));
}

#[test]
fn test_parse_version_and_pick_version_edges() {
    assert_eq!(parse_version("v1"), Some(1));
    assert_eq!(parse_version("v4294967295"), Some(u32::MAX));
    for bad in ["v0", "v01", "v", "v-1", "vx", "V1", "v4294967296", "v1 "] {
        assert_eq!(parse_version(bad), None, "{bad}");
    }
    let p = pick_version(["v2", "v5", "v01", "universe", "v5"]);
    assert_eq!(p.newest, Some(5));
    assert_eq!(p.gaps, vec![1, 3, 4]);
    assert_eq!(p.malformed, vec!["v01".to_owned()]);
    assert_eq!(pick_version(Vec::<&str>::new()).newest, None);
}

#[test]
fn test_usable_snapshot_dates_and_is_iso_date_edges() {
    let (ok, bad) = usable_snapshot_dates(
        [
            "2026-10-29",
            "2026-02-30",
            "2026-10-30",
            "2026-10-31",
            "latest",
            "2026-10-28",
        ],
        "2026-10-30",
    );
    assert_eq!(ok, vec!["2026-10-30", "2026-10-29", "2026-10-28"]);
    assert_eq!(bad, vec!["2026-02-30".to_owned(), "latest".to_owned()]);
    assert!(is_iso_date("2028-02-29"));
    assert!(!is_iso_date("2027-02-29"));
}

#[test]
fn test_verify_folder_and_manifest_json_for_refuse_traversal_duplicates_and_size_change() {
    let root = temp_root();
    let d = root.join("x");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("a.csv"), "x").unwrap();
    let mut m = manifest_json_for(&[("a.csv", b"x")]);
    std::fs::write(d.join("manifest.json"), m.replace("a.csv", "../a.csv")).unwrap();
    assert!(matches!(verify_folder(&d), Err(Refusal::BadManifest(_))));
    m = manifest_json_for(&[("a.csv", b"x"), ("a.csv", b"x")]);
    std::fs::write(d.join("manifest.json"), &m).unwrap();
    assert!(matches!(verify_folder(&d), Err(Refusal::BadManifest(_))));
    std::fs::write(
        d.join("manifest.json"),
        manifest_json_for(&[("a.csv", b"x")]),
    )
    .unwrap();
    std::fs::write(d.join("a.csv"), "xy").unwrap();
    assert!(matches!(
        verify_folder(&d),
        Err(Refusal::SizeMismatch { .. })
    ));
    std::fs::write(d.join("manifest.json"), "{not json").unwrap();
    assert!(matches!(verify_folder(&d), Err(Refusal::BadManifest(_))));
    std::fs::remove_file(d.join("manifest.json")).unwrap();
    assert!(matches!(
        verify_folder(&d),
        Err(Refusal::MissingFile { .. })
    ));
}

#[test]
fn test_parse_candles_refuses_bad_rows() {
    let h = "ts_ist,security_id,exchange_segment,timeframe,open,high,low,close,volume\n";
    for (row, want) in [
        (
            "2026-10-27T09:15:00,1,1,1m,1,2,0.5,NaN,1",
            "close is not finite",
        ),
        ("2026-10-27T09:15:00,1,1,1m,1,2,0.5,inf,1", "not finite"),
        ("2026-10-27T09:15:00,1,1,1m,3,2,0.5,1,1", "low <= open"),
        ("2026-10-27 09:15:00,1,1,1m,1,2,0.5,1,1", "ts_ist"),
        (
            "2026-10-27T09:15:00,1,256,1m,1,2,0.5,1,1",
            "exchange_segment",
        ),
        ("2026-10-27T09:15:00,1,1,1m,1,2,0.5,1", "columns"),
        ("2026-10-27T09:15:00,1,1,1m,1,2,0.5,abc,1", "not a number"),
    ] {
        let r = parse_candles("c", format!("{h}{row}\n").as_bytes())
            .unwrap_err()
            .to_string();
        assert!(r.contains(want), "{row}: {r}");
    }
    let dup = format!(
        "{h}2026-10-27T09:15:00,1,1,1m,1,2,0.5,1,1\n2026-10-27T09:15:00,1,1,1m,1,2,0.5,1,1\n"
    );
    assert!(
        parse_candles("c", dup.as_bytes())
            .unwrap_err()
            .to_string()
            .contains("does not rise")
    );
    let crlf = format!(
        "{}2026-10-27T09:15:00,1,1,1m,1,2,0.5,1,1\r\n",
        h.replace('\n', "\r\n")
    );
    assert_eq!(parse_candles("c", crlf.as_bytes()).unwrap().len(), 1);
    assert!(matches!(
        parse_candles("c", h.as_bytes()),
        Err(Refusal::EmptyCandles)
    ));
    assert!(parse_candles("c", b"").is_err());
}

#[test]
fn test_append_and_to_line_write_one_line_per_event() {
    let root = temp_root();
    write_version(&root, 1, false, BARS);
    let (out, _) = run(&root, None);
    let path = root.join("audit/strategy-audit.ndjson");
    append(&path, &out.events).unwrap();
    append(&path, &out.events).unwrap();
    append(&path, &[]).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text.lines().count(), 2 * out.events.len());
    assert!(
        text.lines()
            .all(|l| serde_json::from_str::<serde_json::Value>(l).is_ok())
    );
    assert!(text.contains("\"kind\":\"loaded\""));
}

#[test]
fn test_verified_folder_file_has_names_and_hex_lower() {
    let root = temp_root();
    write_version(&root, 1, false, BARS);
    let f = verify_folder(&root.join("v1")).unwrap();
    assert!(f.has("recipe.json"));
    assert!(!f.has("manifest.json"));
    assert_eq!(f.names().len(), 4);
    assert!(f.file("recipe.json").is_ok());
    assert!(matches!(
        f.file("nope.csv"),
        Err(Refusal::MissingFile { .. })
    ));
    assert_eq!(super::hex_lower(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
    assert_eq!(super::hex_lower(&[]), "");
}

#[test]
fn test_intent_sink_with_capacity_push_intents_overflowed_clear() {
    use super::engine::{IntentKind, IntentSink, OrderIntent, Side};
    let i = OrderIntent {
        key: InstrumentKey {
            security_id: 1,
            exchange_segment: 2,
        },
        timeframe: 0,
        kind: IntentKind::Entry,
        side: Side::Buy,
        qty_lots: 1,
        entry_price: 0.0,
        stop_loss: 1.0,
        target: 2.0,
        intent_id: 1,
        at_ist_secs: 0,
    };
    let mut s = IntentSink::with_capacity(2);
    assert!(s.push(i));
    assert!(s.push(i));
    assert!(!s.push(i));
    assert_eq!(s.intents().len(), 2);
    assert_eq!(s.overflowed(), 1);
    s.clear();
    assert!(s.intents().is_empty());
    let mut zero = IntentSink::with_capacity(0);
    assert!(!zero.push(i));
}

#[test]
fn test_same_bits_and_is_full_sha() {
    use super::recipe::is_full_sha;
    use super::replay::same_bits;
    assert!(same_bits(1.5, 1.5));
    assert!(!same_bits(0.0, -0.0));
    assert!(same_bits(f64::NAN, f64::NAN));
    assert!(!same_bits(0.1 + 0.2, 0.3));
    assert!(is_full_sha(COMMIT));
    assert!(!is_full_sha(&COMMIT[..39]));
    assert!(!is_full_sha(&format!("{}g", &COMMIT[..39])));
}

#[test]
fn test_parse_json_parse_values_and_parse_signals_refuse_bad_input() {
    use super::check_files::{parse_signals, parse_values};
    use super::recipe::{UniverseSnapshot, parse_json};
    assert!(parse_json::<UniverseSnapshot>(b"{", "u").is_err());
    assert!(
        parse_json::<UniverseSnapshot>(br#"{"date":"2026-10-29","instruments":[]}"#, "u").is_ok()
    );
    let vh = "ts_ist,security_id,exchange_segment,timeframe";
    assert!(parse_values("v", vh.as_bytes()).is_err()); // no value columns
    assert!(
        parse_values(
            "v",
            format!("{vh},a\n2026-10-27T09:15:00,1,1,1m\n").as_bytes()
        )
        .is_err()
    );
    assert_eq!(
        parse_values(
            "v",
            format!("{vh},a,b\n2026-10-27T09:15:00,1,1,1m,1,2\n").as_bytes()
        )
        .unwrap()
        .rows
        .len(),
        1
    );
    let sh = "ts_ist,security_id,exchange_segment,timeframe,signal,side,qty_lots,entry_price,stop_loss,target\n";
    for (row, want) in [
        ("2026-10-27T09:15:00,1,1,1m,buy,buy,1,0,1,2", "signal"),
        ("2026-10-27T09:15:00,1,1,1m,entry,long,1,0,1,2", "side"),
        ("2026-10-27T09:15:00,1,1,1m,entry,buy,-1,0,1,2", "qty_lots"),
    ] {
        let e = parse_signals("s", format!("{sh}{row}\n").as_bytes())
            .unwrap_err()
            .to_string();
        assert!(e.contains(want), "{row}: {e}");
    }
}

#[test]
fn test_winner_gate_sanity_and_universe_direct() {
    use super::gates::{sanity, universe, winner_gate};
    use super::recipe::{GatesFile, ParamSnapshot, SanityLimits, UniverseSnapshot};
    let g = |p| GatesFile {
        passed: p,
        window_from: "a".into(),
        window_to: "b".into(),
        detail: serde_json::Value::Null,
    };
    assert!(winner_gate(&g(true)).is_ok());
    assert!(winner_gate(&g(false)).is_err());
    let snap = |pairs: &[(&str, f64)]| ParamSnapshot {
        date: "d".into(),
        params: pairs.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect(),
    };
    let lim = SanityLimits {
        max_param_change_fraction: 0.5,
    };
    assert!(sanity(&snap(&[("k", 1.0)]), None, lim).is_ok());
    assert!(sanity(&snap(&[]), None, lim).is_err());
    assert!(sanity(&snap(&[("k", f64::INFINITY)]), None, lim).is_err());
    assert!(sanity(&snap(&[("k", 1.5)]), Some(&snap(&[("k", 1.0)])), lim).is_ok());
    assert!(sanity(&snap(&[("k", 1.51)]), Some(&snap(&[("k", 1.0)])), lim).is_err());
    assert!(
        sanity(&snap(&[("j", 1.0)]), Some(&snap(&[("k", 1.0)])), lim)
            .unwrap_err()
            .to_string()
            .contains("names changed")
    );
    // From zero any move is infinite relative change: refused.
    assert!(sanity(&snap(&[("k", 0.001)]), Some(&snap(&[("k", 0.0)])), lim).is_err());
    assert!(sanity(&snap(&[("k", 0.0)]), Some(&snap(&[("k", 0.0)])), lim).is_ok());
    let u = UniverseSnapshot {
        date: "d".into(),
        instruments: vec![InstrumentKey {
            security_id: 9,
            exchange_segment: 2,
        }],
    };
    assert!(universe(&u, &|_| true).is_ok());
    assert!(
        universe(&u, &|_| false)
            .unwrap_err()
            .to_string()
            .contains("not in today's instrument list")
    );
}

#[test]
fn test_toy_check_files_and_write_demo_fixture_load_cleanly() {
    use super::fixture::{toy_check_files, write_demo_fixture};
    let (c, v, s) = toy_check_files(&[
        ("2026-10-27T09:15:00", 1, 1, "1m", 10.0),
        ("2026-10-27T09:16:00", 1, 1, "1m", 9.5),
    ]);
    assert_eq!(c.lines().count(), 3);
    assert!(v.ends_with("9.5,10\n"));
    assert!(s.contains("exit,sell,1,0,8.5,11.5"));
    let root = temp_root();
    let paths = write_demo_fixture(&root).unwrap();
    assert_eq!(paths.len(), 2);
    for p in paths {
        let ctx = LoadContext {
            built_commit: super::fixture::FIXTURE_COMMIT,
            trading_day: "2026-10-30",
            at_ist: "t",
            is_known: &known,
        };
        let mut alerts = CollectAlerts::default();
        let out = load(&p, &ctx, &mut ReferenceDouble::new(), None, &mut alerts);
        assert!(out.live.is_some(), "{:?}", out.events);
    }
}

// ---- Direct replay harness: every mismatch branch names file, row and field.

mod replay_direct {
    use super::super::check_files::{
        CheckBar, CheckValues, SignalKind, SignalRow, ValueRowExpected,
    };
    use super::super::engine::{
        BarInput, IntentKind, IntentSink, OrderIntent, ReferenceDouble, Side, StrategyEngine,
        TickInput,
    };
    use super::super::recipe::{ParamSnapshot, Recipe};
    use super::super::replay::{Mismatch, replay};
    use super::super::{InstrumentKey, Refusal};

    const K: InstrumentKey = InstrumentKey {
        security_id: 7,
        exchange_segment: 1,
    };

    fn tfs() -> Vec<String> {
        vec!["1m".to_owned()]
    }

    fn bar(row: usize, ts: &str, close: f64) -> CheckBar {
        CheckBar {
            row,
            ts_ist: ts.to_owned(),
            key: K,
            timeframe: "1m".to_owned(),
            open: close,
            high: close,
            low: close,
            close,
            volume: 1,
        }
    }

    fn vrow(row: usize, ts: &str, close: f64, prev: f64) -> ValueRowExpected {
        ValueRowExpected {
            row,
            ts_ist: ts.to_owned(),
            key: K,
            timeframe: "1m".to_owned(),
            values: vec![close, prev],
        }
    }

    fn srow(row: usize, ts: &str, signal: SignalKind, side: &str, close: f64) -> SignalRow {
        SignalRow {
            row,
            ts_ist: ts.to_owned(),
            key: K,
            timeframe: "1m".to_owned(),
            signal,
            side: side.to_owned(),
            qty_lots: 1,
            entry_price: 0.0,
            stop_loss: close - 1.0,
            target: close + 2.0,
        }
    }

    const T1: &str = "2026-10-27T09:15:00";
    const T2: &str = "2026-10-27T09:16:00";

    /// Two bars, close 100 then 101: the double emits an entry/buy on bar 2.
    fn good() -> (Vec<CheckBar>, CheckValues, Vec<SignalRow>) {
        (
            vec![bar(2, T1, 100.0), bar(3, T2, 101.0)],
            CheckValues {
                columns: vec!["close".to_owned(), "prev_close".to_owned()],
                rows: vec![vrow(2, T1, 100.0, 0.0), vrow(3, T2, 101.0, 100.0)],
            },
            vec![
                srow(2, T1, SignalKind::None, "", 100.0),
                srow(3, T2, SignalKind::Entry, "buy", 101.0),
            ],
        )
    }

    fn mismatch(c: &[CheckBar], v: &CheckValues, s: &[SignalRow]) -> Mismatch {
        match replay(&mut ReferenceDouble::new(), &tfs(), c, v, s) {
            Err(Refusal::Replay(m)) => *m,
            other => panic!("expected a replay mismatch, got {other:?}"),
        }
    }

    #[test]
    fn test_replay_direct_good_pair_passes_and_counts() {
        let (c, v, s) = good();
        let r = replay(&mut ReferenceDouble::new(), &tfs(), &c, &v, &s).unwrap();
        assert_eq!((r.bars, r.values_checked, r.signals_checked), (2, 4, 2));
    }

    #[test]
    fn test_replay_header_timeframe_and_ts_refusals() {
        let (c, mut v, s) = good();
        v.columns = vec!["close".to_owned()];
        let m = mismatch(&c, &v, &s);
        assert_eq!((m.field.as_str(), m.row, m.key), ("header", 0, None));
        assert!(m.to_string().contains("instrument -"), "{m}");

        let (mut c, v, s) = good();
        c[0].timeframe = "7m".to_owned();
        let e = replay(&mut ReferenceDouble::new(), &tfs(), &c, &v, &s).unwrap_err();
        assert!(
            e.to_string().contains("timeframe 7m not in the recipe"),
            "{e}"
        );

        let (mut c, v, s) = good();
        c[1].ts_ist = "27/10/2026 09:16".to_owned();
        let e = replay(&mut ReferenceDouble::new(), &tfs(), &c, &v, &s).unwrap_err();
        assert!(matches!(e, Refusal::BadCheckFile { row: 3, .. }), "{e:?}");

        let (_, v, s) = good();
        assert_eq!(
            replay(&mut ReferenceDouble::new(), &tfs(), &[], &v, &s).unwrap_err(),
            Refusal::EmptyCandles
        );
    }

    #[test]
    fn test_replay_value_row_count_and_alignment_refusals() {
        let (c, mut v, s) = good();
        v.rows.truncate(1);
        let m = mismatch(&c, &v, &s);
        assert_eq!(
            (m.file.as_str(), m.field.as_str()),
            ("check_values.csv", "row count")
        );
        assert_eq!((m.expected.as_str(), m.got.as_str()), ("2", "1"));

        let (c, mut v, s) = good();
        v.rows.push(vrow(4, "2026-10-27T09:17:00", 1.0, 1.0));
        let m = mismatch(&c, &v, &s);
        assert_eq!((m.field.as_str(), m.row, m.key), ("row count", 4, None));

        let (c, mut v, s) = good();
        v.rows[1].ts_ist = "2026-10-27T09:17:00".to_owned();
        let m = mismatch(&c, &v, &s);
        assert_eq!((m.field.as_str(), m.row), ("bar alignment", 3));

        let (c, mut v, s) = good();
        v.rows[0].values[0] = 100.5;
        let m = mismatch(&c, &v, &s);
        assert_eq!(
            (m.field.as_str(), m.expected.as_str(), m.got.as_str()),
            ("close", "100.5", "100")
        );
    }

    #[test]
    fn test_replay_signal_row_count_and_alignment_refusals() {
        let (c, v, mut s) = good();
        s.truncate(1);
        let m = mismatch(&c, &v, &s);
        assert_eq!(
            (m.file.as_str(), m.field.as_str()),
            ("check_signals.csv", "row count")
        );

        let (c, v, mut s) = good();
        s.push(srow(4, "2026-10-27T09:17:00", SignalKind::None, "", 1.0));
        let m = mismatch(&c, &v, &s);
        assert_eq!(
            (m.file.as_str(), m.field.as_str(), m.row),
            ("check_signals.csv", "row count", 4)
        );

        let (c, v, mut s) = good();
        s[0].timeframe = "5m".to_owned();
        let m = mismatch(&c, &v, &s);
        assert_eq!((m.field.as_str(), m.row), ("bar alignment", 2));
    }

    #[test]
    fn test_replay_signal_field_refusals_name_each_field() {
        // Expected none, got an entry.
        let (c, v, mut s) = good();
        s[1] = srow(3, T2, SignalKind::None, "", 101.0);
        let m = mismatch(&c, &v, &s);
        assert_eq!(
            (m.field.as_str(), m.expected.as_str(), m.got.as_str()),
            ("signal", "none", "entry")
        );
        // Expected exit, got an entry.
        let (c, v, mut s) = good();
        s[1].signal = SignalKind::Exit;
        let m = mismatch(&c, &v, &s);
        assert_eq!((m.expected.as_str(), m.got.as_str()), ("exit", "entry"));
        // Side, quantity, stop-loss, target, entry price.
        let (c, v, mut s) = good();
        s[1].side = "sell".to_owned();
        assert_eq!(mismatch(&c, &v, &s).field, "side");
        let (c, v, mut s) = good();
        s[1].qty_lots = 2;
        assert_eq!(mismatch(&c, &v, &s).field, "qty_lots");
        let (c, v, mut s) = good();
        s[1].stop_loss = 99.0;
        assert_eq!(mismatch(&c, &v, &s).field, "stop_loss");
        let (c, v, mut s) = good();
        s[1].target = 104.0;
        assert_eq!(mismatch(&c, &v, &s).field, "target");
        let (c, v, mut s) = good();
        s[1].entry_price = -0.0;
        assert_eq!(mismatch(&c, &v, &s).field, "entry_price");
        // Falling close: the double exits; the file said exit/sell, so it passes.
        let mut c = vec![bar(2, T1, 100.0), bar(3, T2, 99.0)];
        let v = CheckValues {
            columns: vec!["close".to_owned(), "prev_close".to_owned()],
            rows: vec![vrow(2, T1, 100.0, 0.0), vrow(3, T2, 99.0, 100.0)],
        };
        let s = vec![
            srow(2, T1, SignalKind::None, "", 100.0),
            srow(3, T2, SignalKind::Exit, "sell", 99.0),
        ];
        assert!(replay(&mut ReferenceDouble::new(), &tfs(), &c, &v, &s).is_ok());
        // Flat close: no signal either side.
        c[1].close = 100.0;
        let v2 = CheckValues {
            columns: v.columns.clone(),
            rows: vec![vrow(2, T1, 100.0, 0.0), vrow(3, T2, 100.0, 100.0)],
        };
        let s2 = vec![
            srow(2, T1, SignalKind::None, "", 100.0),
            srow(3, T2, SignalKind::None, "", 100.0),
        ];
        assert!(replay(&mut ReferenceDouble::new(), &tfs(), &c, &v2, &s2).is_ok());
    }

    /// An engine that emits two intents per bar and has no values: the two
    /// remaining refusal branches.
    struct Noisy {
        cols: Vec<String>,
        values: bool,
    }

    impl StrategyEngine for Noisy {
        fn configure(
            &mut self,
            _: &Recipe,
            _: Option<&ParamSnapshot>,
            _: &[InstrumentKey],
        ) -> Result<(), Refusal> {
            Ok(())
        }
        fn on_tick(&mut self, _: &TickInput, _: &mut IntentSink) {}
        fn on_bar(&mut self, bar: &BarInput, out: &mut IntentSink) {
            for id in 0..2 {
                out.push(OrderIntent {
                    key: bar.key,
                    timeframe: bar.timeframe,
                    kind: IntentKind::Entry,
                    side: Side::Buy,
                    qty_lots: 1,
                    entry_price: 0.0,
                    stop_loss: 0.0,
                    target: 0.0,
                    intent_id: id,
                    at_ist_secs: bar.open_ist_secs,
                });
            }
        }
        fn value_columns(&self) -> &[String] {
            &self.cols
        }
        fn bar_values(&self, _: InstrumentKey, _: u8, out: &mut [f64]) -> bool {
            if self.values {
                out.iter_mut().for_each(|x| *x = 100.0);
            }
            self.values
        }
    }

    #[test]
    fn test_replay_engine_with_no_values_or_two_signals_refuses() {
        let (c, v, s) = good();
        let cols = v.columns.clone();
        let mut e = Noisy {
            cols: cols.clone(),
            values: false,
        };
        let Err(Refusal::Replay(m)) = replay(&mut e, &tfs(), &c, &v, &s) else {
            panic!("expected a mismatch");
        };
        assert_eq!((m.field.as_str(), m.got.as_str()), ("values", "none"));

        let one = CheckValues {
            columns: cols.clone(),
            rows: vec![ValueRowExpected {
                values: vec![100.0, 100.0],
                ..v.rows[0].clone()
            }],
        };
        let mut e = Noisy { cols, values: true };
        let sig = vec![srow(2, T1, SignalKind::Entry, "buy", 1.0)];
        let Err(Refusal::Replay(m)) = replay(&mut e, &tfs(), &c[..1], &one, &sig) else {
            panic!("expected a mismatch");
        };
        assert_eq!(
            (m.field.as_str(), m.expected.as_str(), m.got.as_str()),
            ("signal count", "1", "2")
        );
        e.on_tick(
            &TickInput {
                key: K,
                exchange_time_ist_secs: 0,
                receipt_ns: 0,
                ltp: 1.0,
                cumulative_volume: 0,
            },
            &mut IntentSink::with_capacity(1),
        );
    }

    #[test]
    fn test_reference_double_on_tick_and_bar_values_without_a_bar() {
        let mut d = ReferenceDouble::new();
        let mut sink = IntentSink::with_capacity(2);
        d.on_tick(
            &TickInput {
                key: K,
                exchange_time_ist_secs: 0,
                receipt_ns: 0,
                ltp: 1.0,
                cumulative_volume: 0,
            },
            &mut sink,
        );
        assert!(sink.intents().is_empty());
        let mut out = [0.0_f64; 2];
        assert!(!d.bar_values(K, 0, &mut out));
        let mut short = [0.0_f64; 1];
        d.on_bar(
            &BarInput {
                key: K,
                timeframe: 0,
                open_ist_secs: 0,
                open: 1.0,
                high: 1.0,
                low: 1.0,
                close: 1.0,
                volume: 0,
            },
            &mut sink,
        );
        assert!(
            !d.bar_values(K, 0, &mut short),
            "an output too short is refused"
        );
        assert!(d.bar_values(K, 0, &mut out));
        assert_eq!(out, [1.0, 0.0]);
    }

    #[test]
    fn test_every_refusal_variant_renders_a_readable_line() {
        let all = [
            Refusal::Unreadable {
                name: "a".into(),
                detail: "d".into(),
            },
            Refusal::BadManifest("m".into()),
            Refusal::MissingFile { name: "a".into() },
            Refusal::UnlistedFile { name: "a".into() },
            Refusal::SizeMismatch {
                name: "a".into(),
                expected: 1,
                got: 2,
            },
            Refusal::HashMismatch {
                name: "a".into(),
                expected: "e".into(),
                got: "g".into(),
            },
            Refusal::BadRecipe("r".into()),
            Refusal::NotApproved("draft".into()),
            Refusal::VersionMismatch {
                folder: 2,
                in_file: 3,
            },
            Refusal::CommitMismatch {
                recipe: "r".into(),
                built: "b".into(),
            },
            Refusal::BadCheckFile {
                name: "a".into(),
                row: 4,
                detail: "d".into(),
            },
            Refusal::EmptyCandles,
            Refusal::Gate("g".into()),
            Refusal::Engine("e".into()),
            Refusal::NoVersion,
            Refusal::Swap("s".into()),
        ];
        for r in all {
            assert!(!r.to_string().is_empty(), "{r:?}");
        }
    }
}
// O(1) EXEMPT: end

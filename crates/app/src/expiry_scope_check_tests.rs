//! Tests for `expiry_scope_check` (the expiry-scope guarantee, 2026-10-28).

use std::collections::{HashMap, HashSet};

use proptest::prelude::*;

use super::*;
use crate::dhan_contract_universe::select_contract_universe;

const TODAY: u32 = 2026_10_28;
const CURRENT: u32 = 2026_11_23;
const NEXT: u32 = 2026_12_29;
const NIFTY_WEEKLY: u32 = 2026_11_03;

fn opt(id: u64, class: &str, u: &str, e: u32, strike_rupees: i64, leg: &str) -> ContractRow {
    ContractRow {
        i: id,
        x: "NSE".into(),
        c: class.into(),
        e,
        s: strike_rupees * 100,
        l: leg.into(),
        u: u.into(),
        z: 75,
    }
}

/// A real chain: `strikes` strikes x both legs on one expiry.
fn chain(first_id: u64, class: &str, u: &str, e: u32, strikes: &[i64]) -> Vec<ContractRow> {
    let mut out = Vec::new();
    let mut id = first_id;
    for &k in strikes {
        for leg in ["CE", "PE"] {
            out.push(opt(id, class, u, e, k, leg));
            id += 1;
        }
    }
    out
}

fn inst(id: u64) -> SubscribeInstrument {
    SubscribeInstrument {
        security_id: id,
        segment: ExchangeSegment::NseFno,
    }
}

fn candle(id: i64, seg: &str, contract: Option<&str>) -> CandleContractRow {
    CandleContractRow {
        security_id: id,
        segment: seg.into(),
        contract: contract.map(str::to_owned),
    }
}

/// HAVELLS: a thin 30 Oct stub (2 legs), the current 23 Nov chain (ids
/// 100..), the 29 Dec chain (ids 200..), and one expired 27 Oct leg (id 300).
fn havells_file() -> Vec<ContractRow> {
    let mut rows = vec![
        opt(10, "OPTSTK", "HAVELLS", 2026_10_30, 1260, "CE"),
        opt(11, "OPTSTK", "HAVELLS", 2026_10_30, 1260, "PE"),
    ];
    rows.extend(chain(
        100,
        "OPTSTK",
        "HAVELLS",
        CURRENT,
        &[1240, 1260, 1280],
    ));
    rows.extend(chain(200, "OPTSTK", "HAVELLS", NEXT, &[1240, 1260, 1280]));
    rows.push(opt(300, "OPTSTK", "HAVELLS", 2026_10_27, 1260, "CE"));
    rows
}

// ---------------------------------------------------------------- parser

#[test]
fn test_parse_contract_label_reads_a_plain_name() {
    let p = parse_contract_label("SBIN-23Nov2026-1040-PE").expect("parses");
    assert_eq!(p.underlying, "SBIN");
    assert_eq!(p.expiry_ymd, 2026_11_23);
    assert_eq!(p.strike_paise, 104_000);
    assert_eq!(p.leg, "PE");
}

#[test]
fn test_parse_contract_label_keeps_a_hyphenated_underlying_whole() {
    let p = parse_contract_label("BAJAJ-AUTO-23Nov2026-9000-CE").expect("parses");
    assert_eq!(p.underlying, "BAJAJ-AUTO");
    assert_eq!(p.expiry_ymd, CURRENT);
    let p = parse_contract_label("M&M-FIN-X-03Nov2026-25.50-PE").expect("parses");
    assert_eq!(p.underlying, "M&M-FIN-X");
    assert_eq!(p.strike_paise, 2_550);
}

#[test]
fn test_parse_contract_label_decimal_strikes() {
    assert_eq!(
        parse_contract_label("IDEA-23Nov2026-7.50-CE").map(|p| p.strike_paise),
        Some(750)
    );
    assert_eq!(
        parse_contract_label("IDEA-23Nov2026-7.5-CE").map(|p| p.strike_paise),
        Some(750)
    );
    assert_eq!(
        parse_contract_label("IDEA-23Nov2026-7.05-CE").map(|p| p.strike_paise),
        Some(705)
    );
}

#[test]
fn test_parse_contract_label_refuses_every_malformed_shape() {
    for bad in [
        "",
        "SBIN",
        "SBIN-23Nov2026",
        "SBIN-23Nov2026-1040",
        "SBIN-23Nov2026-1040-XX",
        "SBIN-23Nov2026-1040-ce",
        "-23Nov2026-1040-CE",
        " -23Nov2026-1040-CE",
        "SBIN-31Nov2026-1040-CE",
        "SBIN-29Feb2027-1040-CE",
        "SBIN-23nov2026-1040-CE",
        "SBIN-20261123-1040-CE",
        "SBIN-3Nov2026-1040-CE",
        "SBIN-23Nov2026-10.405-CE",
        "SBIN-23Nov2026-.5-CE",
        "SBIN-23Nov2026-10.-CE",
        "SBIN-23Nov2026--1040-CE",
        "SBIN-23Nov2026-1e3-CE",
        "SBIN-23Nov2026-99999999999999999999-CE",
        "VERYLONGUNDERLYING-23Nov2026-1040-C…",
        "SBIN-23Növ2026-1040-CE",
    ] {
        assert!(
            parse_contract_label(bad).is_none(),
            "{bad:?} must not parse"
        );
    }
}

#[test]
fn test_parse_contract_label_accepts_a_leap_day() {
    assert_eq!(
        parse_contract_label("NIFTY-29Feb2028-24000-CE").map(|p| p.expiry_ymd),
        Some(2028_02_29)
    );
}

proptest! {
    /// Every name the renderer produces (and does not cap) parses back to
    /// exactly what it was rendered from.
    #[test]
    fn proptest_parse_contract_label_round_trips_the_renderer(
        underlying in "[A-Z][A-Z0-9&]{0,9}(-[A-Z]{1,5}){0,2}",
        year in 2000_i32..2100,
        month in 1_u32..=12,
        day in 1_u32..=28,
        strike in 1_i64..10_000_000_000,
        call in any::<bool>(),
    ) {
        let leg = if call { "CE" } else { "PE" };
        let ymd = u32::try_from(year).unwrap_or(0) * 10_000 + month * 100 + day;
        let label = contract_label(&underlying, ymd, strike, leg);
        prop_assume!(label.len() < crate::contract_underlying_map::MAX_CONTRACT_LABEL_BYTES);
        let p = parse_contract_label(&label).expect("a rendered name parses");
        prop_assert_eq!(p.underlying, underlying.as_str());
        prop_assert_eq!(p.expiry_ymd, ymd);
        prop_assert_eq!(p.strike_paise, strike);
        prop_assert_eq!(p.leg, leg);
    }

    /// No input panics the parser.
    #[test]
    fn proptest_parse_contract_label_never_panics(s in ".{0,80}") {
        let _ = parse_contract_label(&s);
    }
}

// ---------------------------------------------------------------- index

#[test]
fn test_build_expiry_index_skips_a_thin_stub_and_picks_the_real_chain() {
    let rows = havells_file();
    let index = build_expiry_index(&rows, TODAY, None);
    assert_eq!(
        index
            .nearest
            .get(&(ExchangeSegment::NseFno, false, "HAVELLS")),
        Some(&CURRENT)
    );
    assert_eq!(index.thin_expiries_skipped, 1, "the 30 Oct stub");
    assert_eq!(
        index.by_key.len(),
        rows.len(),
        "every option row is indexed"
    );
}

#[test]
fn test_build_expiry_index_counts_the_expiry_day_itself_as_current() {
    let rows = chain(1, "OPTIDX", "NIFTY", TODAY, &[24_000, 24_100]);
    let index = build_expiry_index(&rows, TODAY, None);
    assert_eq!(
        index.nearest.get(&(ExchangeSegment::NseFno, true, "NIFTY")),
        Some(&TODAY)
    );
}

#[test]
fn test_build_expiry_index_on_yesterdays_file_finds_nothing_current() {
    // Yesterday's file read after its last expiry: every row is expired.
    let rows = chain(1, "OPTSTK", "SBIN", 2026_10_27, &[1000, 1020, 1040]);
    let index = build_expiry_index(&rows, TODAY, None);
    assert!(index.nearest.is_empty());
    assert_eq!(index.by_key.len(), rows.len(), "expired rows still resolve");
}

#[test]
fn test_build_expiry_index_ignores_unusable_rows_when_counting_legs() {
    let mut rows = vec![
        opt(1, "OPTSTK", "SBIN", CURRENT, 0, "CE"),
        opt(2, "OPTSTK", "SBIN", CURRENT, 1000, ""),
        opt(3, "OPTSTK", " ", CURRENT, 1000, "CE"),
        opt(4, "FUTSTK", "SBIN", CURRENT, 0, ""),
        opt(0, "OPTSTK", "SBIN", CURRENT, 1000, "CE"),
        opt(5, "OPTSTK", "SBIN", 0, 1000, "CE"),
    ];
    let mut mcx = opt(6, "OPTSTK", "SBIN", CURRENT, 1000, "PE");
    mcx.x = "MCX".into();
    rows.push(mcx);
    rows.push(opt(7, "OPTSTK", "SBIN", CURRENT, 1000, "PE"));
    let index = build_expiry_index(&rows, TODAY, None);
    // No bucket reaches four legs (SBIN has two, ids 0 and 7; the
    // whitespace underlying " " has one, id 3): no real chain.
    assert!(index.nearest.is_empty());
    assert!(
        !index.by_key.contains_key(&(4, ExchangeSegment::NseFno)),
        "a future is not an option"
    );
    assert!(
        !index.by_key.contains_key(&(0, ExchangeSegment::NseFno)),
        "id 0 refused"
    );
}

#[test]
fn test_build_expiry_index_maps_bse_and_honours_the_only_filter() {
    let mut rows = chain(1, "OPTIDX", "SENSEX", CURRENT, &[80_000, 80_100]);
    for r in &mut rows {
        r.x = "BSE".into();
    }
    rows.push(opt(9, "OPTSTK", "SBIN", CURRENT, 1000, "CE"));
    let only: HashSet<ContractKey> = [(1, ExchangeSegment::BseFno)].into_iter().collect();
    let index = build_expiry_index(&rows, TODAY, Some(&only));
    assert_eq!(
        index
            .nearest
            .get(&(ExchangeSegment::BseFno, true, "SENSEX")),
        Some(&CURRENT)
    );
    assert_eq!(index.by_key.len(), 1);
    assert!(index.by_key.contains_key(&(1, ExchangeSegment::BseFno)));
}

#[test]
fn test_build_expiry_index_keeps_the_first_row_of_a_duplicate_key() {
    let mut rows = chain(1, "OPTSTK", "SBIN", CURRENT, &[1000, 1020]);
    rows.push(opt(1, "OPTSTK", "SBIN", NEXT, 1000, "CE"));
    let index = build_expiry_index(&rows, TODAY, None);
    assert_eq!(
        index.by_key.get(&(1, ExchangeSegment::NseFno)).map(|r| r.e),
        Some(CURRENT)
    );
    // Two expiries under one id: kept, but marked ambiguous.
    assert!(index.ambiguous.contains(&(1, ExchangeSegment::NseFno)));
}

#[test]
fn test_artifact_option_segment_maps_nse_and_bse_only() {
    assert_eq!(
        artifact_option_segment("NSE"),
        Some(ExchangeSegment::NseFno)
    );
    assert_eq!(
        artifact_option_segment("BSE"),
        Some(ExchangeSegment::BseFno)
    );
    for other in ["MCX", "nse", "", "NSE "] {
        assert_eq!(artifact_option_segment(other), None);
    }
}

// ------------------------------------------------------- subscribe time

#[test]
fn test_check_selected_expiries_passes_a_current_selection() {
    let rows = havells_file();
    let report = check_selected_expiries(&[inst(100), inst(101), inst(105)], &rows, TODAY);
    assert_eq!(report.checked, 3);
    assert!(report.refused.is_empty());
    assert_eq!(report.sample, None);
}

#[test]
fn test_check_selected_expiries_refuses_off_expiry_expired_unknown_and_the_stub() {
    let rows = havells_file();
    let selected = [inst(100), inst(200), inst(300), inst(999), inst(10)];
    let report = check_selected_expiries(&selected, &rows, TODAY);
    // 200 = next month, 10 = the thin stub (live, not the real chain).
    assert_eq!(report.off_expiry, 2);
    assert_eq!(report.expired, 1);
    assert_eq!(report.unknown, 1);
    assert_eq!(report.checked, 4);
    assert_eq!(
        report.refused,
        vec![inst(200), inst(300), inst(999), inst(10)]
    );
    assert_eq!(report.sample.as_deref(), Some("HAVELLS-29Dec2026-1240-CE"));
}

#[test]
fn test_check_selected_expiries_refuses_an_option_whose_underlying_has_no_real_chain() {
    let rows = vec![opt(1, "OPTSTK", "TINY", CURRENT, 10, "CE")];
    let report = check_selected_expiries(&[inst(1)], &rows, TODAY);
    assert_eq!(report.unknown, 1);
    assert_eq!(report.refused, vec![inst(1)]);
}

#[test]
fn test_check_selected_expiries_on_an_empty_selection_is_empty() {
    assert_eq!(
        check_selected_expiries(&[], &havells_file(), TODAY),
        SubscribeScopeReport::default()
    );
}

#[test]
fn test_enforce_subscribe_expiry_scope_removes_and_counts() {
    let mut rows = havells_file();
    rows.extend(chain(
        400,
        "OPTIDX",
        "NIFTY",
        NIFTY_WEEKLY,
        &[24_000, 24_100],
    ));
    rows.extend(chain(500, "OPTIDX", "NIFTY", CURRENT, &[24_000, 24_100]));
    let mut selection = ContractSelection {
        instruments: vec![inst(100), inst(200), inst(400), inst(500)],
        backup_rank: vec![inst(200), inst(400), inst(500)],
        index_options: 2,
        stock_options: 2,
        ..ContractSelection::default()
    };
    let report = enforce_subscribe_expiry_scope(&mut selection, &rows, TODAY);
    // 200 (HAVELLS December) and 500 (NIFTY monthly behind the weekly).
    assert_eq!(report.refused, vec![inst(200), inst(500)]);
    assert_eq!(selection.instruments, vec![inst(100), inst(400)]);
    assert_eq!(selection.backup_rank, vec![inst(400)]);
    assert_eq!(selection.index_options, 1);
    assert_eq!(selection.stock_options, 1);
    assert_eq!(selection.refused_off_expiry, 2);
    // A clean selection is left exactly as it was.
    let before = selection.clone();
    let again = enforce_subscribe_expiry_scope(&mut selection, &rows, TODAY);
    assert!(again.refused.is_empty());
    assert_eq!(selection, before);
}

#[test]
fn test_enforce_subscribe_expiry_scope_agrees_with_the_real_selector() {
    // The selector and this check compute "current expiry" by separate
    // passes. On a file with a stub, a weekly, a monthly and an expired row
    // they must agree: the selector's output is refused nothing.
    let mut rows = vec![
        opt(1, "OPTIDX", "NIFTY", 2026_10_30, 24_000, "CE"),
        opt(2, "OPTIDX", "NIFTY", 2026_10_27, 24_000, "CE"),
    ];
    // Ids clear of `havells_file`'s (10, 11, 100.., 200.., 300): a shared id
    // on two contracts is ambiguous and refused by design.
    rows.extend(chain(
        1_010,
        "OPTIDX",
        "NIFTY",
        NIFTY_WEEKLY,
        &[24_000, 24_050, 24_100],
    ));
    rows.extend(chain(
        1_030,
        "OPTIDX",
        "NIFTY",
        CURRENT,
        &[24_000, 24_050, 24_100],
    ));
    rows.extend(havells_file());
    let master: Vec<_> = rows.iter().map(ContractRow::to_master_row).collect();
    let spot: HashMap<String, i64> = [("HAVELLS".to_owned(), 126_000_i64)].into_iter().collect();
    let mut selection = select_contract_universe(&master, &spot, TODAY, 25_000);
    assert!(selection.index_options > 0 && selection.stock_options > 0);
    let report = enforce_subscribe_expiry_scope(&mut selection, &rows, TODAY);
    assert!(report.refused.is_empty(), "{report:?}");
    assert_eq!(report.checked, selection.instruments.len());
}

proptest! {
    /// For any file the selector accepts, the independent check refuses
    /// nothing the selector picked, and every picked contract is judged.
    #[test]
    fn proptest_selector_output_always_passes_the_expiry_scope_check(
        expiries in proptest::collection::vec(
            (prop_oneof![Just(2026_10_27_u32), Just(TODAY), Just(2026_10_30), Just(NIFTY_WEEKLY), Just(CURRENT), Just(NEXT)], 1_usize..6),
            1..6,
        ),
        stock_legs in proptest::collection::vec(
            (prop_oneof![Just(TODAY), Just(2026_10_30), Just(CURRENT), Just(NEXT)], 1_usize..5),
            1..5,
        ),
    ) {
        let mut rows = Vec::new();
        let mut id = 1_u64;
        for (e, strikes) in &expiries {
            let ks: Vec<i64> = (0..*strikes as i64).map(|k| 24_000 + k * 50).collect();
            let c = chain(id, "OPTIDX", "NIFTY", *e, &ks);
            id += c.len() as u64;
            rows.extend(c);
        }
        for (e, strikes) in &stock_legs {
            let ks: Vec<i64> = (0..*strikes as i64).map(|k| 1_240 + k * 20).collect();
            let c = chain(id, "OPTSTK", "HAVELLS", *e, &ks);
            id += c.len() as u64;
            rows.extend(c);
        }
        let master: Vec<_> = rows.iter().map(ContractRow::to_master_row).collect();
        let spot: HashMap<String, i64> = [("HAVELLS".to_owned(), 126_000_i64)].into_iter().collect();
        let mut selection = select_contract_universe(&master, &spot, TODAY, 25_000);
        let picked = selection.instruments.len();
        let report = enforce_subscribe_expiry_scope(&mut selection, &rows, TODAY);
        prop_assert!(report.refused.is_empty(), "{:?}", report);
        prop_assert_eq!(report.checked, picked);
    }
}

#[test]
fn test_record_subscribe_expiry_scope_and_subscribe_refused_for_day() {
    let _guard = RECORD_SCOPE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // A day no other test uses (the memory is process-wide).
    let day = 1_998_01_01;
    assert_eq!(subscribe_refused_for_day(day), None);
    let mut report = SubscribeScopeReport {
        checked: 3,
        ..SubscribeScopeReport::default()
    };
    record_subscribe_expiry_scope(&report, day);
    assert_eq!(subscribe_refused_for_day(day), Some(0));
    report.refused = vec![inst(1), inst(2)];
    report.off_expiry = 2;
    report.sample = Some("X-23Nov2026-1-CE".into());
    record_subscribe_expiry_scope(&report, day);
    assert_eq!(subscribe_refused_for_day(day), Some(2));
    // A later clean attempt does not hide that the day refused two.
    report.refused.clear();
    record_subscribe_expiry_scope(&report, day);
    assert_eq!(subscribe_refused_for_day(day), Some(2));
    // A new day starts fresh.
    record_subscribe_expiry_scope(&report, day + 1);
    assert_eq!(subscribe_refused_for_day(day + 1), Some(0));
    assert_eq!(subscribe_refused_for_day(day), None);
}

// ---------------------------------------------------------------- daily

#[test]
fn test_expiry_scope_sql_is_bounded_and_filtered() {
    let day = NaiveDate::from_ymd_opt(2026, 10, 28).expect("date");
    let sql = expiry_scope_sql(day);
    assert!(sql.starts_with("select distinct security_id, segment, contract from candles_1m "));
    assert!(sql.contains("ts in '2026-10-28'"));
    assert!(sql.contains("feed = 'dhan'"));
    assert!(sql.contains("segment in ('NSE_FNO', 'BSE_FNO')"));
    assert!(sql.ends_with(&format!("limit {}", EXPIRY_SCOPE_MAX_ROWS + 1)));
}

#[test]
fn test_parse_candle_contract_rows_reads_the_wire_format() {
    let body = r#"{"query":"x","columns":[],"dataset":[
        [114816,"NSE_FNO",null],
        [114817,"NSE_FNO","HAVELLS-23Nov2026-1260-CE"],
        ["114818","NSE_FNO","  "],
        ["oops","NSE_FNO","A-23Nov2026-1-CE"],
        [5],
        "not a row"
    ],"count":6}"#;
    let rows = parse_candle_contract_rows(body).expect("parses");
    assert_eq!(
        rows.len(),
        5,
        "a non-array row is skipped, a short one kept"
    );
    assert_eq!(rows[0], candle(114_816, "NSE_FNO", None));
    assert_eq!(
        rows[1],
        candle(114_817, "NSE_FNO", Some("HAVELLS-23Nov2026-1260-CE"))
    );
    assert_eq!(
        rows[2],
        candle(114_818, "NSE_FNO", None),
        "blank name is no name"
    );
    assert_eq!(rows[3].security_id, -1);
    assert_eq!(rows[4], candle(5, "", None));
}

#[test]
fn test_parse_candle_contract_rows_errors_without_a_dataset() {
    assert!(parse_candle_contract_rows("not json").is_err());
    assert!(parse_candle_contract_rows(r#"{"error":"table does not exist"}"#).is_err());
    assert_eq!(
        parse_candle_contract_rows(r#"{"dataset":[]}"#).map(|r| r.len()),
        Ok(0)
    );
}

#[test]
fn test_classify_candle_contracts_all_current_is_clean() {
    let file = havells_file();
    let index = build_expiry_index(&file, TODAY, None);
    let rows = [
        candle(100, "NSE_FNO", Some("HAVELLS-23Nov2026-1240-CE")),
        candle(103, "NSE_FNO", Some("HAVELLS-23Nov2026-1260-PE")),
    ];
    let (counts, sample) = classify_candle_contracts(&rows, &index, TODAY);
    assert_eq!(counts.contracts_checked, 2);
    assert_eq!(counts.violations(), 0);
    assert_eq!(counts.unverifiable(), 0);
    assert_eq!(sample, None);
    let inputs = ExpiryScopeInputs {
        artifact_read: true,
        rows_read_ok: true,
        truncated: false,
        live_not_final: None,
        counts,
    };
    assert_eq!(
        classify_expiry_scope(inputs),
        (ExpiryScopeVerdict::Clean, "all_current_expiry")
    );
}

#[test]
fn test_classify_candle_contracts_the_havells_unlabelled_candle_counts_once() {
    // The 2026-10-28 shape: one contract stored once with its name and once
    // without. One contract, judged once, flagged unlabelled.
    let file = havells_file();
    let index = build_expiry_index(&file, TODAY, None);
    let rows = [
        candle(103, "NSE_FNO", None),
        candle(103, "NSE_FNO", Some("HAVELLS-23Nov2026-1260-PE")),
    ];
    let (counts, _) = classify_candle_contracts(&rows, &index, TODAY);
    assert_eq!(counts.rows_read, 2);
    assert_eq!(counts.contracts_checked, 1);
    assert_eq!(counts.unlabelled, 1);
    assert_eq!(counts.violations(), 0);
    // Judged from the file, but a candle stored with no name means the
    // naming gap is back: never a clean day (2026-10-28 review).
    let (verdict, reason) = classify_expiry_scope(ExpiryScopeInputs {
        artifact_read: true,
        rows_read_ok: true,
        truncated: false,
        live_not_final: None,
        counts,
    });
    assert_eq!(
        (verdict, reason),
        (ExpiryScopeVerdict::Partial, "unlabelled_rows")
    );
}

#[test]
fn test_classify_candle_contracts_finds_off_expiry_and_expired_with_the_lowest_sample() {
    let file = havells_file();
    let index = build_expiry_index(&file, TODAY, None);
    let rows = [
        candle(205, "NSE_FNO", Some("HAVELLS-29Dec2026-1280-PE")),
        candle(200, "NSE_FNO", None),
        candle(300, "NSE_FNO", Some("HAVELLS-27Oct2026-1260-CE")),
        candle(100, "NSE_FNO", Some("HAVELLS-23Nov2026-1240-CE")),
    ];
    let (counts, sample) = classify_candle_contracts(&rows, &index, TODAY);
    assert_eq!(counts.off_expiry, 2);
    assert_eq!(counts.expired, 1);
    assert_eq!(counts.contracts_checked, 4);
    assert_eq!(counts.unlabelled, 1);
    // id 200 is the lowest offending key; it has no stored name, so the
    // sample is rendered from the file.
    assert_eq!(sample.as_deref(), Some("HAVELLS-29Dec2026-1240-CE"));
    let (verdict, reason) = classify_expiry_scope(ExpiryScopeInputs {
        artifact_read: true,
        rows_read_ok: true,
        truncated: false,
        live_not_final: None,
        counts,
    });
    assert_eq!(
        (verdict, reason),
        (ExpiryScopeVerdict::Violation, "expired_and_off_expiry")
    );
}

#[test]
fn test_classify_candle_contracts_label_disagreeing_with_the_file_is_judged_by_the_file() {
    let file = havells_file();
    let index = build_expiry_index(&file, TODAY, None);
    // The name says December; the file says id 100 is November (current).
    let (counts, sample) = classify_candle_contracts(
        &[candle(100, "NSE_FNO", Some("HAVELLS-29Dec2026-1240-CE"))],
        &index,
        TODAY,
    );
    assert_eq!(counts.label_mismatch, 1);
    assert_eq!(counts.off_expiry, 0);
    assert_eq!(sample, None);
    // Two names for one contract also disagree.
    let (counts, _) = classify_candle_contracts(
        &[
            candle(150, "NSE_FNO", Some("HAVELLS-23Nov2026-1240-CE")),
            candle(150, "NSE_FNO", Some("HAVELLS-29Dec2026-1240-CE")),
        ],
        &index,
        TODAY,
    );
    assert_eq!(counts.label_mismatch, 1);
    assert_eq!(counts.off_expiry, 0, "judged by the first name: current");
}

#[test]
fn test_classify_candle_contracts_counts_what_it_cannot_judge() {
    let file = havells_file();
    let index = build_expiry_index(&file, TODAY, None);
    let rows = [
        // Not in the file, name parses, underlying unknown.
        candle(900, "NSE_FNO", Some("NEWCO-23Nov2026-100-CE")),
        // Not in the file, no name.
        candle(901, "NSE_FNO", None),
        // Not in the file, name does not parse.
        candle(902, "NSE_FNO", Some("garbage")),
        // In the file, name does not parse: judged by the file.
        candle(101, "NSE_FNO", Some("HAVELLS-23Nov2026-1240-C…")),
        // Unusable id and segment.
        candle(-1, "NSE_FNO", Some("HAVELLS-23Nov2026-1240-CE")),
        candle(100, "MARS", Some("HAVELLS-23Nov2026-1240-CE")),
    ];
    let (counts, _) = classify_candle_contracts(&rows, &index, TODAY);
    assert_eq!(counts.unknown_underlying, 1);
    assert_eq!(
        counts.unresolved, 4,
        "901, 902 (unparsable, not in the file) and the two unusable rows"
    );
    assert_eq!(counts.malformed_labels, 2);
    assert_eq!(counts.unlabelled, 1);
    assert_eq!(counts.contracts_checked, 1, "only 101");
    let (verdict, reason) = classify_expiry_scope(ExpiryScopeInputs {
        artifact_read: true,
        rows_read_ok: true,
        truncated: false,
        live_not_final: None,
        counts,
    });
    assert_eq!(
        (verdict, reason),
        (ExpiryScopeVerdict::Partial, "unverifiable_rows")
    );
}

#[test]
fn test_classify_candle_contracts_with_yesterdays_file_reports_expired() {
    // Today's file missing and yesterday's used by mistake would read every
    // stored contract as expired, never as clean.
    let file = chain(1, "OPTSTK", "SBIN", 2026_10_27, &[1000, 1020]);
    let index = build_expiry_index(&file, TODAY, None);
    let (counts, _) = classify_candle_contracts(&[candle(1, "NSE_FNO", None)], &index, TODAY);
    assert_eq!(counts.expired, 1);
}

#[test]
fn test_classify_expiry_scope_every_combination() {
    // Every combination of the three gates and each count at 0 or 1.
    for artifact_read in [false, true] {
        for rows_read_ok in [false, true] {
            for truncated in [false, true] {
                for live_not_final in [None, Some("live_not_final")] {
                    for bits in 0_u32..(1 << 9) {
                        let b = |n: u32| ((bits >> n) & 1) as usize;
                        let counts = ExpiryScopeCounts {
                            rows_read: b(0),
                            contracts_checked: b(1),
                            off_expiry: b(2),
                            expired: b(3),
                            unlabelled: b(4),
                            unknown_underlying: b(5),
                            malformed_labels: b(6),
                            label_mismatch: b(7),
                            unresolved: b(8),
                        };
                        let i = ExpiryScopeInputs {
                            artifact_read,
                            rows_read_ok,
                            truncated,
                            live_not_final,
                            counts,
                        };
                        let (verdict, reason) = classify_expiry_scope(i);
                        if !artifact_read {
                            assert_eq!(
                                (verdict, reason),
                                (ExpiryScopeVerdict::Partial, "artifact_missing")
                            );
                            continue;
                        }
                        if !rows_read_ok {
                            assert_eq!(
                                (verdict, reason),
                                (ExpiryScopeVerdict::Partial, "read_failed")
                            );
                            continue;
                        }
                        if counts.violations() > 0 {
                            assert_eq!(verdict, ExpiryScopeVerdict::Violation, "{i:?}");
                            continue;
                        }
                        let clean = live_not_final.is_none()
                            && !truncated
                            && counts.rows_read > 0
                            && counts.contracts_checked > 0
                            && counts.unverifiable() == 0
                            && counts.label_mismatch == 0
                            && counts.malformed_labels == 0
                            && counts.unlabelled == 0;
                        assert_eq!(verdict == ExpiryScopeVerdict::Clean, clean, "{i:?}");
                        if let Some(r) = live_not_final {
                            assert_eq!(reason, r, "{i:?}");
                        } else if truncated {
                            assert_eq!(reason, "row_cap_hit");
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn test_expiry_scope_verdict_labels_are_stable() {
    assert_eq!(ExpiryScopeVerdict::Clean.as_str(), "clean");
    assert_eq!(ExpiryScopeVerdict::Violation.as_str(), "violation");
    assert_eq!(ExpiryScopeVerdict::Partial.as_str(), "partial");
}

#[test]
fn test_expiry_scope_counts_saturate_at_the_extremes() {
    let c = ExpiryScopeCounts {
        off_expiry: usize::MAX,
        expired: 1,
        unknown_underlying: usize::MAX,
        unresolved: 1,
        ..ExpiryScopeCounts::default()
    };
    assert_eq!(c.violations(), usize::MAX);
    assert_eq!(c.unverifiable(), usize::MAX);
}

#[test]
fn test_secs_until_expiry_scope_check() {
    assert_eq!(EXPIRY_SCOPE_CHECK_SECS_OF_DAY_IST, 15 * 3600 + 50 * 60);
    assert_eq!(secs_until_expiry_scope_check(0), 57_000);
    assert_eq!(
        secs_until_expiry_scope_check(EXPIRY_SCOPE_CHECK_SECS_OF_DAY_IST - 1),
        1
    );
    // Regression (2026-10-28 review): a process started after 15:50 IST on a
    // trading day (an evening restart) checks at once instead of skipping the
    // day; before, this returned `None` and the day had no row.
    assert_eq!(
        secs_until_expiry_scope_check(EXPIRY_SCOPE_CHECK_SECS_OF_DAY_IST),
        0
    );
    assert_eq!(secs_until_expiry_scope_check(86_399), 0);
    assert_eq!(secs_until_expiry_scope_check(u32::MAX), 0);
}

#[test]
fn test_ymd_of() {
    let day = NaiveDate::from_ymd_opt(2026, 10, 28).expect("date");
    assert_eq!(ymd_of(day), TODAY);
}

#[test]
fn test_pre_register_expiry_scope_metrics_never_panics() {
    pre_register_expiry_scope_metrics();
    pre_register_expiry_scope_metrics();
}

// ---------------------------------------------------------- source scans

fn production(src: &str) -> &str {
    src.split("\n#[cfg(test)]").next().unwrap_or(src)
}

#[test]
fn test_main_wires_spawn_expiry_scope_check() {
    let main = production(include_str!("main.rs"));
    assert!(main.contains("expiry_scope_check::spawn_expiry_scope_check("));
}

#[test]
fn test_the_daily_check_runs_on_its_own_task_never_the_frame_drain() {
    let me = production(include_str!("expiry_scope_check.rs"));
    let spawn = me
        .find("pub fn spawn_expiry_scope_check(")
        .expect("spawn fn");
    assert!(me[spawn..].contains("tokio::spawn(async move"));
    // The heavy work runs on blocking threads, the read under a timeout.
    assert!(me.contains("tokio::task::spawn_blocking"));
    assert!(me.contains("tokio::time::timeout("));
    // The frame drain never calls into this module.
    let drain = production(include_str!("dhan_feed_stack.rs"));
    assert!(!drain.contains("expiry_scope_check::"));
}

#[test]
fn test_the_attach_refuses_before_it_names_or_dials() {
    let universe = production(include_str!("dhan_contract_universe.rs"));
    let select = universe
        .find("select_contract_universe(&rows, &spot, today_ymd, capacity)")
        .expect("select");
    let enforce = universe
        .find("expiry_scope_check::enforce_subscribe_expiry_scope(")
        .expect("enforce");
    let record = universe
        .find("expiry_scope_check::record_subscribe_expiry_scope(")
        .expect("record");
    let labels = universe
        .find("labels_from_artifact_selected_first(")
        .expect("labels");
    let candle_names = universe
        .find("candle_labels.extend(tickvault_storage::off_worker::off_worker(")
        .expect("candle names");
    assert!(select < enforce && enforce < record && record < labels && labels < candle_names);
}

#[test]
fn test_every_error_carries_the_code() {
    let me = production(include_str!("expiry_scope_check.rs"));
    // `concat!` so the storage error-level guard does not read this test's
    // pattern as a log call.
    let errors = me.matches("error!(").count() + me.matches(concat!("warn", "!(")).count();
    let coded = me.matches("code = ErrorCode::").count();
    assert!(
        errors > 0 && coded >= errors,
        "{errors} error! vs {coded} coded"
    );
}

#[test]
fn test_classify_candle_contracts_a_malformed_name_the_file_identifies_is_judged() {
    let file = havells_file();
    let index = build_expiry_index(&file, TODAY, None);
    let (counts, _) = classify_candle_contracts(
        &[candle(101, "NSE_FNO", Some("HAVELLS-23Nov2026-1240-C…"))],
        &index,
        TODAY,
    );
    assert_eq!(counts.malformed_labels, 1);
    assert_eq!(counts.contracts_checked, 1);
    assert_eq!(counts.unverifiable(), 0);
    assert_eq!(
        classify_expiry_scope(ExpiryScopeInputs {
            artifact_read: true,
            rows_read_ok: true,
            truncated: false,
            live_not_final: None,
            counts,
        }),
        (ExpiryScopeVerdict::Partial, "malformed_labels")
    );
}

// ------------------------------------------------- 2026-10-28 review fixes

#[test]
fn test_build_expiry_index_marks_a_duplicate_key_on_two_expiries_ambiguous() {
    // Regression (2026-10-28 review): a composite key listed twice, once on
    // the next expiry and once on the current one. Which contract the vendor
    // streams under that id is unknown, so the subscribe check refuses it
    // (`unknown`) and the daily check never judges it (`unresolved`), instead
    // of picking whichever row is convenient.
    let mut rows = vec![opt(1, "OPTSTK", "SBIN", NEXT, 1000, "CE")];
    rows.extend(chain(1, "OPTSTK", "SBIN", CURRENT, &[1000, 1020]));
    rows.extend(chain(50, "OPTSTK", "SBIN", NEXT, &[1000, 1020]));
    let index = build_expiry_index(&rows, TODAY, None);
    assert!(index.ambiguous.contains(&(1, ExchangeSegment::NseFno)));
    assert!(!index.ambiguous.contains(&(2, ExchangeSegment::NseFno)));
    let report = check_selected_expiries(&[inst(1), inst(2)], &rows, TODAY);
    assert_eq!(report.unknown, 1, "{report:?}");
    assert_eq!(report.refused, vec![inst(1)]);
    let (counts, _) = classify_candle_contracts(
        &[candle(1, "NSE_FNO", Some("SBIN-23Nov2026-1000-CE"))],
        &index,
        TODAY,
    );
    assert_eq!(counts.unresolved, 1);
    assert_eq!(counts.contracts_checked, 0);
    assert_eq!(counts.violations(), 0);
}

#[test]
fn test_build_expiry_index_a_duplicate_key_on_one_contract_is_not_ambiguous() {
    // The same row listed twice (same underlying, class and expiry) is one
    // contract: judged normally.
    let mut rows = chain(1, "OPTSTK", "SBIN", CURRENT, &[1000, 1020]);
    rows.push(opt(1, "OPTSTK", "SBIN", CURRENT, 1000, "CE"));
    let index = build_expiry_index(&rows, TODAY, None);
    assert!(index.ambiguous.is_empty());
    let report = check_selected_expiries(&[inst(1)], &rows, TODAY);
    assert!(report.refused.is_empty(), "{report:?}");
}

#[test]
fn test_build_expiry_index_counts_an_id_zero_leg_like_the_selector() {
    // Regression (2026-10-28 review): the selector counts a leg whose id is 0
    // toward an expiry; the check skipped it, so an expiry with exactly four
    // legs, one of them id 0, was a stub here and real to the selector, and
    // every contract the selector picked on it was refused.
    let mut rows = vec![
        opt(0, "OPTSTK", "SBIN", CURRENT, 1000, "CE"),
        opt(2, "OPTSTK", "SBIN", CURRENT, 1000, "PE"),
        opt(3, "OPTSTK", "SBIN", CURRENT, 1020, "CE"),
        opt(4, "OPTSTK", "SBIN", CURRENT, 1020, "PE"),
    ];
    rows.extend(chain(50, "OPTSTK", "SBIN", NEXT, &[1000, 1020]));
    let index = build_expiry_index(&rows, TODAY, None);
    assert_eq!(
        index.nearest.get(&(ExchangeSegment::NseFno, false, "SBIN")),
        Some(&CURRENT)
    );
    // The id-0 row is never a key.
    assert!(!index.by_key.contains_key(&(0, ExchangeSegment::NseFno)));
    let report = check_selected_expiries(&[inst(2), inst(3)], &rows, TODAY);
    assert!(report.refused.is_empty(), "{report:?}");
}

#[test]
fn test_build_expiry_index_keeps_index_and_stock_buckets_apart() {
    // Regression (2026-10-28 review): the selector buckets index and stock
    // options separately. A shared underlying name with different nearest
    // expiries must not merge, or one class is judged against the other's
    // chain and refused.
    let mut rows = chain(1, "OPTIDX", "SAMENAME", NIFTY_WEEKLY, &[100, 110]);
    rows.extend(chain(20, "OPTSTK", "SAMENAME", CURRENT, &[100, 110]));
    let index = build_expiry_index(&rows, TODAY, None);
    assert_eq!(
        index
            .nearest
            .get(&(ExchangeSegment::NseFno, true, "SAMENAME")),
        Some(&NIFTY_WEEKLY)
    );
    assert_eq!(
        index
            .nearest
            .get(&(ExchangeSegment::NseFno, false, "SAMENAME")),
        Some(&CURRENT)
    );
    let report = check_selected_expiries(&[inst(1), inst(20)], &rows, TODAY);
    assert!(report.refused.is_empty(), "{report:?}");
    // A stored name alone cannot tell the two chains apart: unjudged.
    assert_eq!(
        index.current_expiry_by_name(ExchangeSegment::NseFno, "SAMENAME"),
        None
    );
}

#[test]
fn test_build_expiry_index_uses_the_underlying_as_the_file_spells_it() {
    // The selector does not trim the underlying, so `SBIN` and `SBIN `
    // are two buckets there; the check keeps them apart the same way.
    let mut rows = chain(1, "OPTSTK", "SBIN", CURRENT, &[1000, 1020]);
    rows.extend(chain(20, "OPTSTK", "SBIN ", NEXT, &[1000, 1020]));
    let index = build_expiry_index(&rows, TODAY, None);
    let report = check_selected_expiries(&[inst(1), inst(20)], &rows, TODAY);
    assert!(report.refused.is_empty(), "{report:?}");
    // By name the two buckets disagree, so a name-only contract is unjudged.
    assert_eq!(
        index.current_expiry_by_name(ExchangeSegment::NseFno, "SBIN"),
        None
    );
}

#[test]
fn test_expiry_index_current_expiry_of_and_current_expiry_by_name() {
    let rows = havells_file();
    let index = build_expiry_index(&rows, TODAY, None);
    let row = index
        .by_key
        .get(&(200, ExchangeSegment::NseFno))
        .copied()
        .expect("indexed");
    assert_eq!(
        index.current_expiry_of(row, ExchangeSegment::NseFno),
        Some(CURRENT)
    );
    assert_eq!(index.current_expiry_of(row, ExchangeSegment::BseFno), None);
    assert_eq!(
        index.current_expiry_by_name(ExchangeSegment::NseFno, " HAVELLS "),
        Some(CURRENT)
    );
    assert_eq!(
        index.current_expiry_by_name(ExchangeSegment::NseFno, "NOPE"),
        None
    );
}

#[test]
fn test_classify_candle_contracts_a_malformed_second_name_is_never_clean() {
    // Regression (2026-10-28 review): one good name and one malformed name for
    // the same contract used to count nothing, so the day read clean.
    let rows = havells_file();
    let index = build_expiry_index(&rows, TODAY, None);
    let candles = [
        candle(100, "NSE_FNO", Some("HAVELLS-23Nov2026-1240-CE")),
        candle(100, "NSE_FNO", Some("HAVELLS-garbage")),
    ];
    let (counts, sample) = classify_candle_contracts(&candles, &index, TODAY);
    assert_eq!(counts.malformed_labels, 1);
    assert_eq!(counts.contracts_checked, 1);
    assert_eq!(sample, None);
    let (verdict, reason) = classify_expiry_scope(ExpiryScopeInputs {
        artifact_read: true,
        rows_read_ok: true,
        truncated: false,
        live_not_final: None,
        counts,
    });
    assert_eq!(verdict, ExpiryScopeVerdict::Partial);
    assert_eq!(reason, "malformed_labels");
}

#[test]
fn test_classify_candle_contracts_a_name_only_contract_on_an_ambiguous_name_is_unjudged() {
    let mut rows = chain(1, "OPTIDX", "SAMENAME", NIFTY_WEEKLY, &[100, 110]);
    rows.extend(chain(20, "OPTSTK", "SAMENAME", CURRENT, &[100, 110]));
    let index = build_expiry_index(&rows, TODAY, None);
    // Id 999 is not in the file; its name points at an ambiguous underlying.
    let candles = [candle(999, "NSE_FNO", Some("SAMENAME-23Nov2026-100-CE"))];
    let (counts, _) = classify_candle_contracts(&candles, &index, TODAY);
    assert_eq!(counts.unknown_underlying, 1);
    assert_eq!(counts.contracts_checked, 0);
    assert_eq!(counts.violations(), 0);
}

#[test]
fn test_classify_candle_contracts_hyphenated_underlying_name_only_is_judged() {
    // BAJAJ-AUTO: the name is read from the right, so the hyphen survives
    // and a name-only contract is judged against the right chain.
    let rows = chain(1, "OPTSTK", "BAJAJ-AUTO", CURRENT, &[9000, 9100]);
    let index = build_expiry_index(&rows, TODAY, None);
    let candles = [
        candle(900, "NSE_FNO", Some("BAJAJ-AUTO-23Nov2026-9000-CE")),
        candle(901, "NSE_FNO", Some("BAJAJ-AUTO-29Dec2026-9000-CE")),
    ];
    let (counts, sample) = classify_candle_contracts(&candles, &index, TODAY);
    assert_eq!(counts.contracts_checked, 2);
    assert_eq!(counts.off_expiry, 1);
    assert_eq!(sample.as_deref(), Some("BAJAJ-AUTO-29Dec2026-9000-CE"));
}

#[test]
fn test_ymd_of_edges() {
    let leap = NaiveDate::from_ymd_opt(2028, 2, 29).expect("date");
    assert_eq!(ymd_of(leap), 2028_02_29);
    let first = NaiveDate::from_ymd_opt(2026, 1, 1).expect("date");
    assert_eq!(ymd_of(first), 2026_01_01);
    let last = NaiveDate::from_ymd_opt(2026, 12, 31).expect("date");
    assert_eq!(ymd_of(last), 2026_12_31);
}

#[test]
fn test_classify_candle_contracts_a_name_with_the_wrong_strike_or_leg_is_a_mismatch() {
    // Regression (2026-10-28 review): only underlying and expiry were
    // compared, so a name carried by the wrong contract on the same expiry
    // read clean.
    let file = havells_file();
    let index = build_expiry_index(&file, TODAY, None);
    // id 100 is 1240 CE; the names say 1260 CE and 1240 PE.
    for name in ["HAVELLS-23Nov2026-1260-CE", "HAVELLS-23Nov2026-1240-PE"] {
        let (counts, _) =
            classify_candle_contracts(&[candle(100, "NSE_FNO", Some(name))], &index, TODAY);
        assert_eq!(counts.label_mismatch, 1, "{name}");
        assert_eq!(counts.contracts_checked, 1, "{name}");
        assert_eq!(counts.violations(), 0, "{name}");
        let (verdict, reason) = classify_expiry_scope(ExpiryScopeInputs {
            artifact_read: true,
            rows_read_ok: true,
            truncated: false,
            live_not_final: None,
            counts,
        });
        assert_eq!(
            (verdict, reason),
            (ExpiryScopeVerdict::Partial, "label_mismatch")
        );
    }
    // The exact name: no mismatch.
    let (counts, _) = classify_candle_contracts(
        &[candle(100, "NSE_FNO", Some("HAVELLS-23Nov2026-1240-CE"))],
        &index,
        TODAY,
    );
    assert_eq!(counts.label_mismatch, 0);
}

#[test]
fn test_refusal_fingerprint_dedupes_one_set_per_day() {
    let a = check_selected_expiries(&[inst(300)], &havells_file(), TODAY);
    let b = check_selected_expiries(&[inst(200)], &havells_file(), TODAY);
    let both = check_selected_expiries(&[inst(200), inst(300)], &havells_file(), TODAY);
    let both_rev = check_selected_expiries(&[inst(300), inst(200)], &havells_file(), TODAY);
    assert_eq!(a.refused.len(), 1);
    assert_eq!(b.refused.len(), 1);
    let fa = refusal_fingerprint(&a, TODAY);
    assert_ne!(fa, 0);
    // The same set the same day: the same fingerprint (logged once).
    assert_eq!(fa, refusal_fingerprint(&a, TODAY));
    // Order does not matter; a different set or day does.
    assert_eq!(
        refusal_fingerprint(&both, TODAY),
        refusal_fingerprint(&both_rev, TODAY)
    );
    assert_ne!(fa, refusal_fingerprint(&b, TODAY));
    assert_ne!(fa, refusal_fingerprint(&both, TODAY));
    assert_ne!(fa, refusal_fingerprint(&a, TODAY + 1));
    // Nothing refused: 0, never logged.
    let clean = check_selected_expiries(&[inst(100)], &havells_file(), TODAY);
    assert_eq!(refusal_fingerprint(&clean, TODAY), 0);
}

#[test]
fn test_readiness_reason_is_clean_only_when_final_and_applied() {
    use crate::dhan_live_crossverify_boot::{Completeness, Durability, LiveReadiness};
    use tickvault_storage::dhan_live_crossverify_persistence::DhanLiveXverifyNotReady as N;
    let completeness = [
        Completeness::Final,
        Completeness::Frozen {
            sealed_through_secs_of_day: 55_000,
        },
        Completeness::Moving {
            sealed_through_secs_of_day: 55_000,
        },
        Completeness::Unknown,
    ];
    for c in completeness {
        let applied = readiness_reason(LiveReadiness {
            durability: Durability::Applied,
            completeness: c,
        });
        if c == Completeness::Final {
            assert_eq!(applied, None);
        } else {
            assert_eq!(applied, Some("live_not_final"), "{c:?}");
        }
        for (n, want) in [
            (N::SealsPending, "seals_pending"),
            (N::SealSpillParked, "seal_spill_parked"),
            (N::NotApplied, "live_not_applied"),
            (N::CompletenessUnknown, "live_not_final"),
        ] {
            let got = readiness_reason(LiveReadiness {
                durability: Durability::NotReady(n),
                completeness: c,
            });
            assert_eq!(got, Some(want), "{n:?} {c:?}");
        }
    }
}

#[test]
fn test_classify_expiry_scope_a_live_side_not_final_is_partial_but_a_violation_still_wins() {
    let clean_counts = ExpiryScopeCounts {
        rows_read: 2,
        contracts_checked: 2,
        ..ExpiryScopeCounts::default()
    };
    let inputs = |live_not_final, counts| ExpiryScopeInputs {
        artifact_read: true,
        rows_read_ok: true,
        truncated: false,
        live_not_final,
        counts,
    };
    assert_eq!(
        classify_expiry_scope(inputs(Some("seals_pending"), clean_counts)),
        (ExpiryScopeVerdict::Partial, "seals_pending")
    );
    let off = ExpiryScopeCounts {
        off_expiry: 1,
        ..clean_counts
    };
    assert_eq!(
        classify_expiry_scope(inputs(Some("live_not_applied"), off)),
        (ExpiryScopeVerdict::Violation, "off_expiry")
    );
    assert_eq!(
        classify_expiry_scope(inputs(None, clean_counts)),
        (ExpiryScopeVerdict::Clean, "all_current_expiry")
    );
}

#[tokio::test]
async fn test_live_side_readiness_is_never_ready_without_a_drained_seal_writer() {
    // No catch-up sweep, no seal-writer drain and an unreachable QuestDB in a
    // test process: the wait must end at its deadline and never report the
    // live side final and saved (the daily check then writes `partial`).
    let today = NaiveDate::from_ymd_opt(2026, 10, 28).expect("date");
    let day_start = today
        .and_hms_opt(0, 0, 0)
        .and_then(|dt| dt.and_utc().timestamp_nanos_opt())
        .expect("nanos");
    let readiness = crate::dhan_live_crossverify_boot::live_side_readiness(
        "http://tv-test-unreachable.invalid:9000/exec",
        today,
        day_start,
        tokio::time::Instant::now(),
    )
    .await;
    assert!(readiness_reason(readiness).is_some(), "{readiness:?}");
}

// ------------------------------------------- 2026-10-28 review, round 2

/// IST-epoch nanos of `ymd` at `secs` seconds of the day.
fn ist_at(day: NaiveDate, secs: u32) -> i64 {
    let start = day
        .and_hms_opt(0, 0, 0)
        .and_then(|dt| dt.and_utc().timestamp_nanos_opt())
        .expect("nanos");
    start + i64::from(secs) * NANOS_PER_SEC
}

#[test]
fn test_secs_until_after_checked_day_wakes_just_past_the_checked_days_end() {
    let d = NaiveDate::from_ymd_opt(2026, 10, 28).expect("date");
    // A normal run: checked at 15:50, finished at 16:20.
    let checked = ist_at(d, EXPIRY_SCOPE_CHECK_SECS_OF_DAY_IST);
    let now = ist_at(d, 16 * 3600 + 20 * 60);
    assert_eq!(
        secs_until_after_checked_day(checked, now),
        (86_400 - (16 * 3600 + 20 * 60)) + EXPIRY_SCOPE_NEXT_DAY_MARGIN_SECS
    );
    // A non-trading day looked at 08:00 and not run.
    let early = ist_at(d, 8 * 3600);
    assert_eq!(
        secs_until_after_checked_day(early, early),
        (86_400 - 8 * 3600) + EXPIRY_SCOPE_NEXT_DAY_MARGIN_SECS
    );
    // Exactly at the wake point: 0. One nanosecond before: rounds UP to 1,
    // so the wake never lands before the next day begins.
    let wake = ist_at(d, 0) + NANOS_PER_DAY + 5 * NANOS_PER_SEC;
    assert_eq!(secs_until_after_checked_day(checked, wake), 0);
    assert_eq!(secs_until_after_checked_day(checked, wake - 1), 1);
}

#[test]
fn test_secs_until_after_checked_day_a_run_across_midnight_does_not_skip_the_next_day() {
    // Reproduction (2026-10-28 review): an evening restart at 23:58 IST runs
    // the check at once; the readiness wait and the reads take it past
    // midnight to 00:30. The old loop slept "to just past the NEXT midnight"
    // measured from 00:30, so it woke on the day after next and the next
    // day had no check.
    let d = NaiveDate::from_ymd_opt(2026, 10, 28).expect("date");
    let next = d.succ_opt().expect("next day");
    let checked = ist_at(d, 23 * 3600 + 58 * 60);
    let after_run = ist_at(next, 30 * 60);
    // The old computation, kept here as the reproduction.
    let old_to_midnight = NANOS_PER_DAY - after_run.rem_euclid(NANOS_PER_DAY);
    let old_secs = u64::try_from(old_to_midnight / NANOS_PER_SEC).expect("secs") + 5;
    let old_wake = after_run + i64::try_from(old_secs).expect("secs") * NANOS_PER_SEC;
    assert_eq!(
        ist_date_of(old_wake),
        next.succ_opt(),
        "the old loop woke on the day after next"
    );
    // The fix: no sleep, so the next iteration reads the clock on `next`.
    assert_eq!(secs_until_after_checked_day(checked, after_run), 0);
    // A run that ends a few seconds after midnight, before the margin.
    let just_after = ist_at(next, 2);
    assert_eq!(
        secs_until_after_checked_day(checked, just_after),
        EXPIRY_SCOPE_NEXT_DAY_MARGIN_SECS - 2
    );
}

#[test]
fn test_secs_until_after_checked_day_visits_every_day_whatever_the_run_length() {
    // Simulates the loop: each iteration looks at the clock, "runs" for
    // `run_secs` (0 on a non-trading day), then sleeps by the rule. Every
    // calendar day must be looked at exactly once.
    let start_day = NaiveDate::from_ymd_opt(2026, 10, 28).expect("date");
    for &(start_secs, run_secs) in &[
        (0_u32, 0_i64),
        (EXPIRY_SCOPE_CHECK_SECS_OF_DAY_IST, 1_568),
        (23 * 3600 + 58 * 60, 1_920),
        (86_399, 3_600),
        (12 * 3600, 12 * 3600 - 1),
    ] {
        let mut now = ist_at(start_day, start_secs);
        let mut seen = Vec::new();
        for _ in 0..10 {
            let day = ist_date_of(now).expect("day");
            seen.push(day);
            let after = now + run_secs * NANOS_PER_SEC;
            let sleep = secs_until_after_checked_day(now, after);
            now = after + i64::try_from(sleep).expect("secs") * NANOS_PER_SEC;
        }
        for pair in seen.windows(2) {
            assert_eq!(
                pair[0].succ_opt(),
                Some(pair[1]),
                "start {start_secs} run {run_secs}: {seen:?}"
            );
        }
    }
}

#[test]
fn test_secs_until_after_checked_day_extremes_never_panic() {
    for &(a, b) in &[
        (i64::MIN, i64::MIN),
        (i64::MIN, i64::MAX),
        (i64::MAX, i64::MIN),
        (i64::MAX, i64::MAX),
        (0, 0),
        (-1, -1),
    ] {
        let _ = secs_until_after_checked_day(a, b);
    }
    assert_eq!(secs_until_after_checked_day(0, i64::MAX), 0);
    assert_eq!(
        secs_until_after_checked_day(0, 0),
        86_400 + EXPIRY_SCOPE_NEXT_DAY_MARGIN_SECS
    );
}

#[test]
fn test_build_expiry_index_an_expired_duplicate_never_makes_a_selected_contract_ambiguous() {
    // Regression (2026-10-28 review): Dhan reuses derivative ids. An expired
    // row (the selector never admits it) under the same id as a current
    // contract made the id "ambiguous", so the subscribe check refused a
    // contract the selector rightly picked. Both file orders.
    for expired_first in [true, false] {
        let expired = opt(1, "OPTIDX", "NIFTY", 2026_10_27, 24_000, "PE");
        let mut rows = Vec::new();
        if expired_first {
            rows.push(expired.clone());
        }
        rows.extend(chain(1, "OPTSTK", "SBIN", CURRENT, &[1000, 1020]));
        if !expired_first {
            rows.push(expired);
        }
        let index = build_expiry_index(&rows, TODAY, None);
        let key = (1, ExchangeSegment::NseFno);
        assert!(
            !index.ambiguous.contains(&key),
            "expired_first {expired_first}"
        );
        assert_eq!(
            index.by_key.get(&key).map(|r| (r.e, r.u.as_str())),
            Some((CURRENT, "SBIN"))
        );
        let report = check_selected_expiries(&[inst(1)], &rows, TODAY);
        assert!(report.refused.is_empty(), "{report:?}");
        assert_eq!(report.checked, 1);
        // The daily check judges it from the admissible row: current. The
        // stored name comes from the production name table, never hand-fed
        // (2026-10-28 review: a hand-fed name hid that the table took the
        // LAST row, the expired NIFTY one, when it sat later in the file).
        let names = crate::dhan_contract_universe::option_labels_from(&rows, TODAY);
        let stored = names.get(&(1, "NSE_FNO")).map(|s| s.to_string());
        assert_eq!(
            stored.as_deref(),
            Some("SBIN-23Nov2026-1000-CE"),
            "expired_first {expired_first}"
        );
        let (counts, _) =
            classify_candle_contracts(&[candle(1, "NSE_FNO", stored.as_deref())], &index, TODAY);
        assert_eq!(counts.contracts_checked, 1);
        assert_eq!(counts.unresolved, 0);
        assert_eq!(counts.label_mismatch, 0, "expired_first {expired_first}");
        assert_eq!(counts.violations(), 0);
    }
}

#[test]
fn test_option_labels_from_names_a_reused_id_as_the_row_the_selector_admits() {
    // Regression (2026-10-28 review): the expired twin sits LAST in the
    // file. The candle name for the id must be the current SBIN contract the
    // selector subscribed, not the expired NIFTY row.
    let mut rows = chain(1001, "OPTSTK", "SBIN", CURRENT, &[1000, 1020]);
    rows.push(opt(1001, "OPTIDX", "NIFTY", 2026_10_27, 24_000, "PE"));
    let names = crate::dhan_contract_universe::option_labels_from(&rows, TODAY);
    assert_eq!(
        names.get(&(1001, "NSE_FNO")).map(|s| &**s),
        Some("SBIN-23Nov2026-1000-CE")
    );
    // Every other id is still named once, and nothing is dropped.
    assert_eq!(names.len(), 4);
}

#[test]
fn test_preferred_option_rows_keeps_one_row_per_key_as_by_key_does() {
    // Every file order of a reused id (expired twin, leg-less twin, second
    // admissible twin) keeps exactly the row `build_expiry_index` keeps.
    let base = chain(1, "OPTSTK", "SBIN", CURRENT, &[1000, 1020]);
    let twins = [
        opt(1, "OPTIDX", "NIFTY", 2026_10_27, 24_000, "PE"),
        opt(1, "OPTSTK", "OTHER", NEXT, 1000, ""),
        opt(1, "OPTSTK", "SBIN", NEXT, 1000, "CE"),
    ];
    for twin in twins {
        for twin_first in [true, false] {
            let mut rows = Vec::new();
            if twin_first {
                rows.push(twin.clone());
            }
            rows.extend(base.iter().cloned());
            if !twin_first {
                rows.push(twin.clone());
            }
            // A future is left out; a zero-id option is kept (no key to
            // share).
            rows.push(opt(7, "FUTSTK", "SBIN", CURRENT, 0, ""));
            rows.push(opt(0, "OPTSTK", "SBIN", CURRENT, 1000, "CE"));
            let kept = preferred_option_rows(&rows, TODAY);
            let index = build_expiry_index(&rows, TODAY, None);
            let key = (1, ExchangeSegment::NseFno);
            let ones: Vec<&ContractRow> = kept.iter().copied().filter(|r| r.i == 1).collect();
            assert_eq!(ones.len(), 1, "{twin:?} first {twin_first}");
            assert!(
                index
                    .by_key
                    .get(&key)
                    .is_some_and(|held| std::ptr::eq(*held, ones[0])),
                "{twin:?} first {twin_first}"
            );
            assert!(kept.iter().all(|r| r.c != "FUTSTK"));
            assert!(kept.iter().any(|r| r.i == 0));
            // Ids 2, 3 and 4 of the chain are untouched.
            assert_eq!(kept.iter().filter(|r| (2..=4).contains(&r.i)).count(), 3);
        }
    }
}

#[test]
fn test_preferred_option_rows_maps_a_reused_id_to_the_admissible_underlying() {
    // The attach builds the contract-to-underlying map from these rows. The
    // raw file maps the reused id to the expired NIFTY row (last wins); the
    // preferred rows map it to SBIN, the contract the selector subscribed.
    let mut rows = chain(1, "OPTSTK", "SBIN", CURRENT, &[1000, 1020]);
    rows.push(opt(1, "OPTIDX", "NIFTY", 2026_10_27, 24_000, "PE"));
    let symbols: HashMap<String, (u64, u8)> = [
        (
            "SBIN".to_string(),
            (3045, ExchangeSegment::NseEquity.binary_code()),
        ),
        (
            "NIFTY".to_string(),
            (13, ExchangeSegment::IdxI.binary_code()),
        ),
    ]
    .into_iter()
    .collect();
    let underlying = |rows: &[ContractRow]| {
        let (legs, _) = crate::contract_underlying_map::legs_from_artifact(rows, &symbols);
        let (map, _) = crate::contract_underlying_map::build_snapshot(&legs);
        map.get(&(1, ExchangeSegment::NseFno))
            .map(|owner| (owner.underlying_id, owner.family))
    };
    assert_eq!(
        underlying(&rows),
        Some((13, crate::option_rank::OptionFamily::Index)),
        "the raw file is last-wins (the defect the attach now avoids)"
    );
    let preferred: Vec<ContractRow> = preferred_option_rows(&rows, TODAY)
        .into_iter()
        .cloned()
        .collect();
    assert_eq!(
        underlying(&preferred),
        Some((3045, crate::option_rank::OptionFamily::Stock))
    );
}

#[test]
fn test_preferred_option_rows_feeds_every_per_id_table_the_attach_builds() {
    let universe = production(include_str!("dhan_contract_universe.rs"));
    assert!(universe.contains("expiry_scope_check::preferred_option_rows(&contracts, today_ymd)"));
    assert!(universe.contains("legs_from_artifact(&named_rows, &symbols)"));
    let labels = universe
        .find("labels_from_artifact_selected_first(")
        .expect("labels");
    let after = universe.get(labels..labels + 80).unwrap_or("");
    assert!(after.contains("&named_rows"), "{after}");
    // The candle names apply the same rule inside `option_labels_from`.
    assert!(universe.contains("expiry_scope_check::preferred_option_rows(contracts, today_ymd)"));
}

#[test]
fn test_build_expiry_index_ignores_every_unusable_duplicate_for_ambiguity() {
    // No leg, no strike, no underlying, expiry 0: each is a row the selector
    // skips, so none makes the admissible contract under the same id
    // ambiguous.
    let unusable = [
        opt(1, "OPTSTK", "OTHER", NEXT, 1000, ""),
        opt(1, "OPTSTK", "OTHER", NEXT, 0, "CE"),
        opt(1, "OPTSTK", "", NEXT, 1000, "CE"),
        opt(1, "OPTIDX", "OTHER", 0, 1000, "CE"),
    ];
    for dup in unusable {
        let mut rows = vec![dup.clone()];
        rows.extend(chain(1, "OPTSTK", "SBIN", CURRENT, &[1000, 1020]));
        let index = build_expiry_index(&rows, TODAY, None);
        assert!(index.ambiguous.is_empty(), "{dup:?}");
        let report = check_selected_expiries(&[inst(1)], &rows, TODAY);
        assert!(report.refused.is_empty(), "{dup:?} {report:?}");
    }
}

#[test]
fn test_build_expiry_index_two_admissible_rows_stay_ambiguous_beside_an_unusable_one() {
    // The other direction: two contracts the selector could pick under one
    // id are still ambiguous (refused, never judged), whatever unusable row
    // sits between them.
    let mut rows = vec![opt(1, "OPTSTK", "SBIN", 2026_10_27, 1000, "CE")];
    rows.extend(chain(1, "OPTSTK", "SBIN", CURRENT, &[1000, 1020]));
    rows.extend(chain(50, "OPTSTK", "SBIN", NEXT, &[1000, 1020]));
    rows.push(opt(1, "OPTSTK", "SBIN", NEXT, 1000, "CE"));
    let index = build_expiry_index(&rows, TODAY, None);
    assert!(index.ambiguous.contains(&(1, ExchangeSegment::NseFno)));
    let report = check_selected_expiries(&[inst(1)], &rows, TODAY);
    assert_eq!(report.unknown, 1);
    assert_eq!(report.refused, vec![inst(1)]);
}

#[test]
fn test_build_expiry_index_a_key_with_only_unusable_rows_keeps_the_old_rule() {
    // No admissible row: the selector never picks the id, so ambiguity among
    // its unusable rows costs no selected contract and keeps the daily check
    // fail-closed (unresolved, never judged).
    let mut rows = vec![
        opt(1, "OPTSTK", "SBIN", 2026_10_27, 1000, "CE"),
        opt(1, "OPTSTK", "SBIN", 2026_10_20, 1000, "CE"),
    ];
    rows.extend(chain(10, "OPTSTK", "SBIN", CURRENT, &[1000, 1020]));
    let index = build_expiry_index(&rows, TODAY, None);
    assert!(index.ambiguous.contains(&(1, ExchangeSegment::NseFno)));
    // Two unusable rows naming ONE contract are not ambiguous.
    let same = vec![
        opt(2, "OPTSTK", "SBIN", 2026_10_27, 1000, "CE"),
        opt(2, "OPTSTK", "SBIN", 2026_10_27, 1000, "CE"),
    ];
    assert!(build_expiry_index(&same, TODAY, None).ambiguous.is_empty());
}

#[test]
fn test_enforce_subscribe_expiry_scope_agrees_with_the_real_selector_on_a_reused_id() {
    // The real selector picks the current SBIN chain; an expired NIFTY row in
    // the same file reuses one of its ids. The selector admits only the
    // current row, so the check must refuse nothing.
    let mut rows = vec![opt(1_001, "OPTIDX", "NIFTY", 2026_10_27, 24_000, "CE")];
    rows.extend(chain(1_000, "OPTSTK", "SBIN", CURRENT, &[1000, 1020, 1040]));
    let master: Vec<_> = rows.iter().map(ContractRow::to_master_row).collect();
    let spot: HashMap<String, i64> = [("SBIN".to_owned(), 102_000_i64)].into_iter().collect();
    let mut selection = select_contract_universe(&master, &spot, TODAY, 25_000);
    assert!(
        selection.instruments.iter().any(|i| i.security_id == 1_001),
        "the selector picks the reused id"
    );
    let picked = selection.instruments.len();
    let report = enforce_subscribe_expiry_scope(&mut selection, &rows, TODAY);
    assert!(report.refused.is_empty(), "{report:?}");
    assert_eq!(report.checked, picked);
    assert_eq!(selection.instruments.len(), picked);
}

#[test]
fn test_enforce_subscribe_expiry_scope_counts_a_refusal_under_its_admissible_class() {
    // A next-month stock contract (refused: off expiry) whose id an expired
    // INDEX row also uses, listed after it. The refusal lowers the stock
    // count the selector raised, never the index count.
    let mut rows = havells_file();
    rows.push(opt(200, "OPTIDX", "NIFTY", 2026_10_27, 24_000, "CE"));
    let mut selection = ContractSelection {
        instruments: vec![inst(100), inst(200)],
        stock_options: 2,
        index_options: 7,
        ..ContractSelection::default()
    };
    let report = enforce_subscribe_expiry_scope(&mut selection, &rows, TODAY);
    assert_eq!(report.refused, vec![inst(200)]);
    assert_eq!(selection.stock_options, 1);
    assert_eq!(selection.index_options, 7);
}

#[test]
fn test_expiry_scope_run_worst_secs_ends_before_the_stop_window() {
    use tickvault_storage::expiry_scope_audit_persistence::{
        EXPIRY_SCOPE_AUDIT_ENSURE_WORST_SECS, EXPIRY_SCOPE_AUDIT_FLUSH_WORST_SECS,
    };
    // 17 DDL statements x 10 s, one 5 s request + 1 s.
    assert_eq!(EXPIRY_SCOPE_AUDIT_ENSURE_WORST_SECS, 170);
    assert_eq!(EXPIRY_SCOPE_AUDIT_FLUSH_WORST_SECS, 6);
    // 170 + 300 + 30 + 3x60 + 2x120 + 3x(170+6) + 2x60.
    assert_eq!(expiry_scope_run_worst_secs(), 1_568);
    let end = u64::from(EXPIRY_SCOPE_CHECK_SECS_OF_DAY_IST) + expiry_scope_run_worst_secs();
    assert_eq!(end, 16 * 3600 + 16 * 60 + 8, "16:16:08 IST");
    assert!(end < u64::from(crate::shutdown_class::SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST));
    // The DDL ensure and the flush are inside the bound (the old bound left
    // both out): removing either would lower it.
    assert!(
        expiry_scope_run_worst_secs()
            > 4 * EXPIRY_SCOPE_AUDIT_ENSURE_WORST_SECS
                + u64::from(EXPIRY_SCOPE_PERSIST_ATTEMPTS) * EXPIRY_SCOPE_AUDIT_FLUSH_WORST_SECS
    );
}

// ======================================================================
// Test-gap sweep (2026-10-28 night): order, permutations, dirty files.
// ======================================================================

/// The day before [`TODAY`] (expired).
const YESTERDAY: u32 = TODAY - 1;
/// A thin stub expiry two days after [`TODAY`] (30 Oct).
const STUB_EXPIRY: u32 = TODAY + 2;

/// The daily verdict for a fully read, final day with these counts.
fn verdict_of(counts: ExpiryScopeCounts) -> (ExpiryScopeVerdict, &'static str) {
    classify_expiry_scope(ExpiryScopeInputs {
        artifact_read: true,
        rows_read_ok: true,
        truncated: false,
        live_not_final: None,
        counts,
    })
}

/// X: a current (23 Nov) chain on ids 1000..=1003 and a next (29 Dec) chain
/// on ids 1010..=1013, two strikes each.
fn x_file() -> Vec<ContractRow> {
    let mut rows = chain(1_000, "OPTSTK", "X", CURRENT, &[100, 110]);
    rows.extend(chain(1_010, "OPTSTK", "X", NEXT, &[100, 110]));
    rows
}

/// Stored names for X: current, next, malformed, none.
const X_NAMES: [Option<&str>; 4] = [
    Some("X-23Nov2026-100-CE"),
    Some("X-29Dec2026-100-CE"),
    Some("X-23Nov2026-100"),
    None,
];

/// Candle ids for X: file-listed current (1000), file-listed next (1010),
/// name-only (999, 998), absent (997). Index 5 is id 1000 under a segment
/// that does not parse.
const X_IDS: [i64; 6] = [1_000, 1_010, 999, 998, 997, 1_000];

/// One candle row over the small X key space. With `one_name_per_name_only_id`
/// every contract the file does not list carries at most ONE parseable name
/// (999 only the current one, 998 only the next one, 997 none), the space in
/// which the classifier is order-independent today.
fn x_candle(id_at: usize, name_at: usize, one_name_per_name_only_id: bool) -> CandleContractRow {
    let id_at = id_at % X_IDS.len();
    let id = X_IDS[id_at];
    let mut name_at = name_at % X_NAMES.len();
    if one_name_per_name_only_id {
        name_at = match (id, name_at) {
            (999, 1) => 0,
            (998, 0) => 1,
            (997, _) => 3,
            _ => name_at,
        };
    }
    let segment = if id_at == 5 { "NSE-FNO" } else { "NSE_FNO" };
    candle(id, segment, X_NAMES[name_at])
}

fn x_rows_strategy(
    one_name_per_name_only_id: bool,
) -> impl Strategy<Value = (Vec<CandleContractRow>, Vec<CandleContractRow>)> {
    proptest::collection::vec((0_usize..6, 0_usize..4), 1..40)
        .prop_map(move |picks| {
            picks
                .into_iter()
                .map(|(i, n)| x_candle(i, n, one_name_per_name_only_id))
                .collect::<Vec<_>>()
        })
        .prop_flat_map(|rows| (Just(rows.clone()), Just(rows).prop_shuffle()))
}

proptest! {
    /// QuestDB's `select distinct` returns rows in no fixed order. Over
    /// every key shape (file-listed, name-only, absent, unparseable segment)
    /// and every name shape (current, next, malformed, NULL), with each
    /// name-only contract carrying at most one parseable name, the counts and
    /// the verdict do not depend on the row order.
    #[test]
    fn proptest_classify_candle_contracts_counts_are_row_order_independent(
        (rows, shuffled) in x_rows_strategy(true),
    ) {
        let file = x_file();
        let index = build_expiry_index(&file, TODAY, None);
        let (a, _) = classify_candle_contracts(&rows, &index, TODAY);
        let (b, _) = classify_candle_contracts(&shuffled, &index, TODAY);
        let mut reversed = rows.clone();
        reversed.reverse();
        let (c, _) = classify_candle_contracts(&reversed, &index, TODAY);
        prop_assert_eq!(a, b);
        prop_assert_eq!(a, c);
        prop_assert_eq!(verdict_of(a), verdict_of(b));
    }

    /// The full key space, including a name-only contract stored under two
    /// names that disagree on expiry: counts, verdict AND the sample name
    /// must not depend on the row order.
    #[test]
    #[ignore = "BUG: classify_candle_contracts keeps the FIRST parsed name of a contract the file does not list (KeyState::parsed) and the first name as the sample, so a name-only contract stored under two names that disagree on expiry reads partial/label_mismatch in one row order and violation/off_expiry in the other"]
    fn proptest_classify_candle_contracts_is_row_order_independent_over_the_full_key_space(
        (rows, shuffled) in x_rows_strategy(false),
    ) {
        let file = x_file();
        let index = build_expiry_index(&file, TODAY, None);
        let (a, sa) = classify_candle_contracts(&rows, &index, TODAY);
        let (b, sb) = classify_candle_contracts(&shuffled, &index, TODAY);
        prop_assert_eq!(a, b);
        prop_assert_eq!(verdict_of(a), verdict_of(b));
        prop_assert_eq!(sa, sb);
    }
}

#[test]
#[ignore = "BUG: a name-only contract under two names that disagree on expiry is judged by whichever row comes first (KeyState::parsed keeps the first): current-name-first gives off_expiry 0 (partial, label_mismatch), next-name-first gives off_expiry 1 (violation)"]
fn test_classify_candle_contracts_a_name_only_contract_with_two_expiries_is_order_independent() {
    let file = x_file();
    let index = build_expiry_index(&file, TODAY, None);
    let current = candle(999, "NSE_FNO", Some("X-23Nov2026-100-CE"));
    let next = candle(999, "NSE_FNO", Some("X-29Dec2026-100-CE"));
    let (a, sample_a) = classify_candle_contracts(&[current.clone(), next.clone()], &index, TODAY);
    let (b, sample_b) = classify_candle_contracts(&[next, current], &index, TODAY);
    assert_eq!(a, b);
    assert_eq!(verdict_of(a), verdict_of(b));
    assert_eq!(sample_a, sample_b);
    // Two names on one contract are never clean, whichever order.
    assert_ne!(verdict_of(a).0, ExpiryScopeVerdict::Clean);
}

// --------------------------------------------- ambiguity, every ordering

/// The seven row kinds one composite key (id 7, NSE) can carry.
/// 0 A, 1 A' (identical to A), 2 B (admissible, other expiry), 3 C
/// (admissible, other class), 4 S (admissible, A's contract on another
/// strike and leg), 5 U1 (expired), 6 U2 (no leg; differs from U1 by expiry).
fn ambiguity_kinds() -> [ContractRow; 7] {
    [
        opt(7, "OPTIDX", "NIFTY", CURRENT, 24_000, "CE"),
        opt(7, "OPTIDX", "NIFTY", CURRENT, 24_000, "CE"),
        opt(7, "OPTIDX", "NIFTY", NEXT, 24_000, "CE"),
        opt(7, "OPTSTK", "NIFTY", CURRENT, 24_000, "CE"),
        opt(7, "OPTIDX", "NIFTY", CURRENT, 24_100, "PE"),
        opt(7, "OPTIDX", "NIFTY", YESTERDAY, 24_000, "CE"),
        opt(7, "OPTIDX", "NIFTY", CURRENT, 24_000, ""),
    ]
}

/// Every ordered selection (no repeats) of `len` items out of `n`.
fn ordered_selections(n: usize, len: usize, prefix: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
    if prefix.len() == len {
        out.push(prefix.clone());
        return;
    }
    for k in 0..n {
        if !prefix.contains(&k) {
            prefix.push(k);
            ordered_selections(n, len, prefix, out);
            prefix.pop();
        }
    }
}

#[test]
fn test_build_expiry_index_ambiguity_matches_the_model_for_every_ordering() {
    // Exhaustive: every subset of 1..=5 of the seven kinds in every order
    // (3,619 files). Reference model: `by_key` keeps the first admissible
    // row, else the first row; the key is ambiguous when its admissible rows
    // name two contracts by (underlying, class, expiry), or, with no
    // admissible row, when its unusable rows do. `preferred_option_rows`
    // keeps the same row, and the `only` filter changes nothing.
    let kinds = ambiguity_kinds();
    let key: ContractKey = (7, ExchangeSegment::NseFno);
    let only: HashSet<ContractKey> = [key].into_iter().collect();
    let mut files = Vec::new();
    for len in 1..=5 {
        ordered_selections(kinds.len(), len, &mut Vec::new(), &mut files);
    }
    assert_eq!(files.len(), 7 + 42 + 210 + 840 + 2_520);
    let mut ambiguity_cleared_by_a_later_admissible_row = 0_usize;
    let mut ambiguous_files = 0_usize;
    for seq in &files {
        let rows: Vec<ContractRow> = seq.iter().map(|&k| kinds[k].clone()).collect();
        let admissible: Vec<usize> = (0..rows.len())
            .filter(|&at| counts_as_a_leg(&rows[at], TODAY))
            .collect();
        let expected_at = admissible.first().copied().unwrap_or(0);
        let distinct_contracts = |ats: &[usize]| {
            ats.iter()
                .map(|&at| (rows[at].u.as_str(), rows[at].c.as_str(), rows[at].e))
                .collect::<HashSet<_>>()
                .len()
        };
        let every: Vec<usize> = (0..rows.len()).collect();
        let expected_ambiguous = if admissible.is_empty() {
            distinct_contracts(&every) >= 2
        } else {
            distinct_contracts(&admissible) >= 2
        };
        let index = build_expiry_index(&rows, TODAY, None);
        let held = index.by_key.get(&key).copied().expect("id 7 is indexed");
        assert!(std::ptr::eq(held, &rows[expected_at]), "{seq:?}");
        assert_eq!(
            index.ambiguous.contains(&key),
            expected_ambiguous,
            "{seq:?}"
        );
        let preferred = preferred_option_rows(&rows, TODAY);
        assert_eq!(preferred.len(), 1, "{seq:?}");
        assert!(std::ptr::eq(preferred[0], held), "{seq:?}");
        let filtered = build_expiry_index(&rows, TODAY, Some(&only));
        let held_filtered = filtered.by_key.get(&key).copied().expect("kept");
        assert!(std::ptr::eq(held_filtered, held), "{seq:?}");
        assert_eq!(filtered.ambiguous, index.ambiguous, "{seq:?}");
        if expected_ambiguous {
            ambiguous_files += 1;
        }
        // The `ambiguous.remove` branch: two differing unusable rows first
        // (marked ambiguous), then an admissible row takes over.
        if rows.len() >= 3
            && !counts_as_a_leg(&rows[0], TODAY)
            && !counts_as_a_leg(&rows[1], TODAY)
            && differs_as_a_contract(&rows[0], &rows[1])
            && !admissible.is_empty()
            && !expected_ambiguous
        {
            ambiguity_cleared_by_a_later_admissible_row += 1;
        }
    }
    assert!(ambiguity_cleared_by_a_later_admissible_row > 0);
    assert!(ambiguous_files > 0 && ambiguous_files < files.len());
}

#[test]
fn test_build_expiry_index_a_second_strike_or_leg_under_one_id_is_not_ambiguous_and_its_own_name_mismatches()
 {
    // Deliberate: two admissible rows under one id that name the same
    // (underlying, class, expiry) on another strike or leg are NOT
    // ambiguous (`differs_as_a_contract` ignores strike and leg). The first
    // row is kept, so a candle carrying the SECOND row's own correct name
    // reads `label_mismatch`, never clean and never a violation.
    let kinds = ambiguity_kinds();
    let (a, s) = (kinds[0].clone(), kinds[4].clone());
    let mut rows = vec![a, s];
    rows.extend(chain(50, "OPTIDX", "NIFTY", CURRENT, &[24_000, 24_100]));
    let index = build_expiry_index(&rows, TODAY, None);
    let key: ContractKey = (7, ExchangeSegment::NseFno);
    assert!(!index.ambiguous.contains(&key));
    assert_eq!(index.by_key.get(&key).map(|r| r.s), Some(2_400_000));
    let (counts, sample) = classify_candle_contracts(
        &[candle(7, "NSE_FNO", Some("NIFTY-23Nov2026-24100-PE"))],
        &index,
        TODAY,
    );
    assert_eq!(counts.label_mismatch, 1);
    assert_eq!(counts.contracts_checked, 1);
    assert_eq!(counts.violations(), 0);
    assert_eq!(sample, None);
    assert_eq!(
        verdict_of(counts),
        (ExpiryScopeVerdict::Partial, "label_mismatch")
    );
    // The subscribe layer judges the id on its expiry and keeps it.
    assert!(
        check_selected_expiries(&[inst(7)], &rows, TODAY)
            .refused
            .is_empty()
    );
}

// ------------------------------------- dirty files: selector vs the check

/// One block of a dirty contract file: either one row of any shape, or a
/// small chain (both legs per strike) on consecutive ids from a SMALL pool,
/// so reused ids across underlyings, classes and expiries are common.
fn dirty_block() -> impl Strategy<Value = Vec<ContractRow>> {
    (
        prop_oneof![Just("OPTIDX"), Just("OPTSTK")],
        prop_oneof![4 => Just("NSE"), 1 => Just("BSE")],
        prop_oneof![
            3 => Just("NIFTY"),
            3 => Just("SBIN"),
            1 => Just("SBIN "),
            1 => Just(" "),
            1 => Just("BAJAJ-AUTO"),
        ],
        prop_oneof![
            1 => Just(YESTERDAY),
            1 => Just(TODAY),
            1 => Just(STUB_EXPIRY),
            1 => Just(NIFTY_WEEKLY),
            3 => Just(CURRENT),
            2 => Just(NEXT),
            1 => Just(0),
        ],
        0_u64..30,
        1_usize..=3,
        any::<bool>(),
        prop_oneof![1 => Just(0_i64), 4 => 1_i64..=5],
        prop_oneof![3 => Just("CE"), 3 => Just("PE"), 1 => Just(""), 1 => Just("ce")],
    )
        .prop_map(|(class, x, u, e, id0, strikes, single, k, leg)| {
            let strike = |k: i64| if k == 0 { 0 } else { 100 + 10 * k };
            let mut rows = if single {
                vec![opt(id0, class, u, e, strike(k), leg)]
            } else {
                let ks: Vec<i64> = (1..=strikes as i64).map(strike).collect();
                chain(id0, class, u, e, &ks)
            };
            for r in &mut rows {
                r.x = x.into();
            }
            rows
        })
}

fn dirty_file() -> impl Strategy<Value = Vec<ContractRow>> {
    proptest::collection::vec(dirty_block(), 1..20)
        .prop_map(|blocks| blocks.into_iter().flatten().collect::<Vec<_>>())
}

fn dirty_spot() -> HashMap<String, i64> {
    ["SBIN", "SBIN ", " ", "BAJAJ-AUTO"]
        .into_iter()
        .map(|u| (u.to_owned(), 12_000_i64))
        .collect()
}

fn keys_of(instruments: &[SubscribeInstrument]) -> Vec<ContractKey> {
    instruments
        .iter()
        .map(|i| (i.security_id, i.segment))
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// The selector and the check compute "current expiry" separately. On a
    /// dirty file (duplicate rows, ids reused across underlyings, classes and
    /// expiries, expired twins, leg-less and strike-0 rows, id 0, a
    /// whitespace or trailing-space underlying, BSE rows, chains of 2 to 6
    /// legs around `MIN_LEGS_FOR_A_REAL_EXPIRY`) the check refuses a picked
    /// contract ONLY when its id names two contracts in the file (ambiguous,
    /// by design) or is 0 (never a key); never off expiry, never expired; it
    /// judges every other pick, and the selection keeps exactly the rest.
    #[test]
    fn proptest_enforce_subscribe_expiry_scope_on_a_dirty_file_refuses_only_ambiguous_picks(
        rows in dirty_file(),
    ) {
        let master: Vec<_> = rows.iter().map(ContractRow::to_master_row).collect();
        let mut selection = select_contract_universe(&master, &dirty_spot(), TODAY, 25_000);
        let picked = keys_of(&selection.instruments);
        let backup_before = keys_of(&selection.backup_rank);
        let index = build_expiry_index(&rows, TODAY, None);
        let report = enforce_subscribe_expiry_scope(&mut selection, &rows, TODAY);
        let refused: HashSet<ContractKey> = keys_of(&report.refused).into_iter().collect();
        for key in &refused {
            prop_assert!(
                key.0 == 0 || index.ambiguous.contains(key),
                "refused a non-ambiguous pick {:?}: {:?}", key, report
            );
        }
        prop_assert_eq!(report.off_expiry, 0, "{:?}", report);
        prop_assert_eq!(report.expired, 0, "{:?}", report);
        prop_assert_eq!(report.unknown, report.refused.len());
        prop_assert_eq!(report.checked + report.refused.len(), picked.len());
        prop_assert_eq!(selection.refused_off_expiry, report.refused.len());
        let kept: Vec<ContractKey> =
            picked.iter().copied().filter(|k| !refused.contains(k)).collect();
        prop_assert_eq!(keys_of(&selection.instruments), kept);
        let backup_kept: Vec<ContractKey> =
            backup_before.into_iter().filter(|k| !refused.contains(k)).collect();
        prop_assert_eq!(keys_of(&selection.backup_rank), backup_kept);
    }

    /// After a refusal the per-class counts still describe what is left: the
    /// index count equals the remaining contracts the index pass picked (the
    /// selector run with no stock prices picks exactly those), and the stock
    /// count the rest.
    #[test]
    #[ignore = "BUG: enforce_subscribe_expiry_scope lowers the class of the FIRST admissible file row of a refused key, but the selector counts a reused id under the class it pushed first (index options before stock options), so refusing an ambiguous id whose stock row comes first in the file lowers stock_options for a contract counted as an index option"]
    fn proptest_enforce_subscribe_expiry_scope_keeps_the_class_counts_of_what_is_left(
        rows in dirty_file(),
    ) {
        let master: Vec<_> = rows.iter().map(ContractRow::to_master_row).collect();
        let mut selection = select_contract_universe(&master, &dirty_spot(), TODAY, 25_000);
        let index_picks: HashSet<ContractKey> = keys_of(
            &select_contract_universe(&master, &HashMap::new(), TODAY, 25_000).instruments,
        )
        .into_iter()
        .collect();
        let count_index = |s: &ContractSelection| {
            keys_of(&s.instruments).iter().filter(|k| index_picks.contains(k)).count()
        };
        prop_assert_eq!(selection.index_options, count_index(&selection));
        prop_assert_eq!(
            selection.stock_options,
            selection.instruments.len() - count_index(&selection)
        );
        let _ = enforce_subscribe_expiry_scope(&mut selection, &rows, TODAY);
        prop_assert_eq!(selection.index_options, count_index(&selection));
        prop_assert_eq!(
            selection.stock_options,
            selection.instruments.len() - count_index(&selection)
        );
    }

    /// The subscribe layer builds the index with `only` = the selection; the
    /// daily layer with `None`. For any dirty file and any key subset K the
    /// two agree on every key in K: the same kept row (by address), the same
    /// ambiguity, the same nearest tables; and `only` holds nothing outside K.
    #[test]
    fn proptest_build_expiry_index_only_filter_agrees_with_the_whole_file_on_its_keys(
        rows in dirty_file(),
        mask in any::<u64>(),
    ) {
        let every = build_expiry_index(&rows, TODAY, None);
        let k: HashSet<ContractKey> = every
            .by_key
            .keys()
            .copied()
            .filter(|&(id, seg)| (mask >> ((id + u64::from(seg.binary_code())) % 64)) & 1 == 1)
            .collect();
        let only = build_expiry_index(&rows, TODAY, Some(&k));
        prop_assert_eq!(&only.nearest, &every.nearest);
        prop_assert_eq!(&only.nearest_by_name, &every.nearest_by_name);
        prop_assert_eq!(only.thin_expiries_skipped, every.thin_expiries_skipped);
        prop_assert!(only.by_key.keys().all(|key| k.contains(key)));
        prop_assert!(only.ambiguous.iter().all(|key| k.contains(key)));
        for key in &k {
            let a = only.by_key.get(key).copied();
            let b = every.by_key.get(key).copied();
            prop_assert!(match (a, b) {
                (Some(a), Some(b)) => std::ptr::eq(a, b),
                _ => false,
            }, "{:?}", key);
            prop_assert_eq!(only.ambiguous.contains(key), every.ambiguous.contains(key));
        }
    }
}

// ----------------------------------------------- the leg-count boundary

/// `n` SBIN legs (alternating CE/PE, two per strike) on the 30 Oct stub,
/// ids 10.., next to a real current chain (ids 100..).
fn sbin_with_stub_legs(n: usize) -> Vec<ContractRow> {
    let mut rows: Vec<ContractRow> = (0..n)
        .map(|j| {
            let leg = if j % 2 == 0 { "CE" } else { "PE" };
            opt(
                10 + j as u64,
                "OPTSTK",
                "SBIN",
                STUB_EXPIRY,
                1_000 + 20 * (j / 2) as i64,
                leg,
            )
        })
        .collect();
    rows.extend(chain(100, "OPTSTK", "SBIN", CURRENT, &[1_000, 1_020]));
    rows
}

#[test]
fn test_build_expiry_index_leg_count_boundary_around_min_legs() {
    assert_eq!(MIN_LEGS_FOR_A_REAL_EXPIRY, 4);
    let sbin = (ExchangeSegment::NseFno, false, "SBIN");
    // (legs on the earliest expiry, its nearest, thin expiries skipped)
    for &(n, nearest, thin) in &[
        (0_usize, CURRENT, 0_usize),
        (1, CURRENT, 1),
        (3, CURRENT, 1),
        (4, STUB_EXPIRY, 0),
        (5, STUB_EXPIRY, 0),
    ] {
        let rows = sbin_with_stub_legs(n);
        let index = build_expiry_index(&rows, TODAY, None);
        assert_eq!(index.nearest.get(&sbin), Some(&nearest), "{n} legs");
        assert_eq!(index.thin_expiries_skipped, thin, "{n} legs");
        // The subscribe layer agrees: the stub legs are current only at 4+.
        let report = check_selected_expiries(&[inst(10), inst(100)], &rows, TODAY);
        if n == 0 {
            assert_eq!(report.unknown, 1, "id 10 is not in the file");
        } else if n >= MIN_LEGS_FOR_A_REAL_EXPIRY {
            assert_eq!(report.refused, vec![inst(100)], "{n} legs");
        } else {
            assert_eq!(report.refused, vec![inst(10)], "{n} legs");
        }
    }
}

#[test]
fn test_build_expiry_index_thin_expiries_skipped_counts_only_thin_dates_before_nearest() {
    let sbin = (ExchangeSegment::NseFno, false, "SBIN");
    // A thin expiry AFTER the nearest real one is not a skipped stub.
    let mut rows = chain(1, "OPTSTK", "SBIN", CURRENT, &[1_000, 1_020]);
    rows.extend(chain(20, "OPTSTK", "SBIN", NEXT, &[1_000]));
    let index = build_expiry_index(&rows, TODAY, None);
    assert_eq!(index.nearest.get(&sbin), Some(&CURRENT));
    assert_eq!(index.thin_expiries_skipped, 0);
    // Every expiry thin: no nearest, and nothing counted as skipped.
    let mut rows = chain(1, "OPTSTK", "SBIN", CURRENT, &[1_000]);
    rows.extend(chain(20, "OPTSTK", "SBIN", NEXT, &[1_000]));
    rows.push(opt(30, "OPTSTK", "SBIN", NEXT, 1_020, "CE"));
    let index = build_expiry_index(&rows, TODAY, None);
    assert!(index.nearest.is_empty());
    assert_eq!(index.thin_expiries_skipped, 0);
    // Two contracts each listed twice: four legs, a real chain, as the
    // selector counts them (it counts rows, duplicates included).
    let mut rows = vec![
        opt(1, "OPTSTK", "SBIN", STUB_EXPIRY, 1_000, "CE"),
        opt(1, "OPTSTK", "SBIN", STUB_EXPIRY, 1_000, "CE"),
        opt(2, "OPTSTK", "SBIN", STUB_EXPIRY, 1_000, "PE"),
        opt(2, "OPTSTK", "SBIN", STUB_EXPIRY, 1_000, "PE"),
    ];
    rows.extend(chain(20, "OPTSTK", "SBIN", CURRENT, &[1_000, 1_020]));
    let index = build_expiry_index(&rows, TODAY, None);
    assert_eq!(index.nearest.get(&sbin), Some(&STUB_EXPIRY));
    let master: Vec<_> = rows.iter().map(ContractRow::to_master_row).collect();
    let spot: HashMap<String, i64> = [("SBIN".to_owned(), 100_000_i64)].into_iter().collect();
    let mut selection = select_contract_universe(&master, &spot, TODAY, 25_000);
    assert!(!selection.instruments.is_empty());
    let report = enforce_subscribe_expiry_scope(&mut selection, &rows, TODAY);
    assert!(report.refused.is_empty(), "{report:?}");
    // Four legs on a whitespace underlying form their own bucket ' ' (the
    // selector admits any non-empty underlying, and so does the check).
    let rows = chain(1, "OPTSTK", " ", CURRENT, &[1_000, 1_020]);
    let index = build_expiry_index(&rows, TODAY, None);
    assert_eq!(
        index.nearest.get(&(ExchangeSegment::NseFno, false, " ")),
        Some(&CURRENT)
    );
    // Its trimmed name is empty, and a stored name never has an empty
    // underlying, so it is reachable only through the file row.
    assert_eq!(
        index.current_expiry_by_name(ExchangeSegment::NseFno, ""),
        Some(CURRENT)
    );
}

#[test]
fn test_build_expiry_index_nearest_by_name_agreeing_and_disagreeing_buckets() {
    let nse = ExchangeSegment::NseFno;
    // Index and stock buckets on one name and one expiry agree.
    let mut rows = chain(1, "OPTIDX", "SAME", CURRENT, &[100, 110]);
    rows.extend(chain(20, "OPTSTK", "SAME", CURRENT, &[100, 110]));
    let index = build_expiry_index(&rows, TODAY, None);
    assert_eq!(index.current_expiry_by_name(nse, "SAME"), Some(CURRENT));
    assert_eq!(index.current_expiry_by_name(nse, " SAME "), Some(CURRENT));
    // `SBIN` and `SBIN ` on one expiry agree.
    let mut rows = chain(1, "OPTSTK", "SBIN", CURRENT, &[1_000, 1_020]);
    rows.extend(chain(20, "OPTSTK", "SBIN ", CURRENT, &[1_000, 1_020]));
    let index = build_expiry_index(&rows, TODAY, None);
    assert_eq!(index.current_expiry_by_name(nse, "SBIN"), Some(CURRENT));
    // NSE and BSE are separate names.
    let mut rows = chain(1, "OPTSTK", "SBIN", CURRENT, &[1_000, 1_020]);
    let mut bse = chain(20, "OPTSTK", "SBIN", NEXT, &[1_000, 1_020]);
    for r in &mut bse {
        r.x = "BSE".into();
    }
    rows.extend(bse);
    let index = build_expiry_index(&rows, TODAY, None);
    assert_eq!(index.current_expiry_by_name(nse, "SBIN"), Some(CURRENT));
    assert_eq!(
        index.current_expiry_by_name(ExchangeSegment::BseFno, "SBIN"),
        Some(NEXT)
    );
    // Three buckets A (current), B (next), A (current) under one trimmed
    // name: one disagreement is sticky whatever order the hash map visits
    // them in. Each permutation of the file is built 16 times, so the
    // iteration order varies with each map's random state.
    let blocks = [
        chain(1, "OPTIDX", "N", CURRENT, &[100, 110]),
        chain(20, "OPTSTK", "N", NEXT, &[100, 110]),
        chain(40, "OPTSTK", "N ", CURRENT, &[100, 110]),
    ];
    for order in [
        [0, 1, 2],
        [1, 0, 2],
        [0, 2, 1],
        [2, 1, 0],
        [1, 2, 0],
        [2, 0, 1],
    ] {
        let rows: Vec<ContractRow> = order.iter().flat_map(|&b| blocks[b].clone()).collect();
        for _ in 0..16 {
            let index = build_expiry_index(&rows, TODAY, None);
            assert_eq!(index.current_expiry_by_name(nse, "N"), None, "{order:?}");
            assert_eq!(index.nearest_by_name.get(&(nse, "N")), Some(&None));
        }
    }
}

// ----------------------------------------- the daily loop with a calendar

/// Weekends plus two weekday holidays: Tue 10 Nov 2026 and Mon 16 Nov 2026
/// (a Monday holiday right after a weekend).
fn sweep_calendar() -> TradingCalendar {
    use tickvault_common::config::{NseHolidayEntry, TradingConfig};
    let holiday = |date: &str| NseHolidayEntry {
        date: date.to_owned(),
        name: "Synthetic Holiday".to_owned(),
    };
    TradingCalendar::from_config(&TradingConfig {
        market_open_time: "09:00:00".to_owned(),
        market_close_time: "15:30:00".to_owned(),
        order_cutoff_time: "15:29:00".to_owned(),
        data_collection_start: "09:00:00".to_owned(),
        data_collection_end: "15:30:00".to_owned(),
        timezone: "Asia/Kolkata".to_owned(),
        max_orders_per_second: 10,
        nse_holidays: vec![holiday("2026-11-10"), holiday("2026-11-16")],
        muhurat_trading_dates: vec![],
        nse_mock_trading_dates: vec![],
    })
    .expect("test calendar")
}

/// One daily check the simulated loop ran: its day, start and end (IST nanos).
type SimCheck = (NaiveDate, i64, i64);

/// A hand-written COPY of `spawn_expiry_scope_check`'s loop body on a
/// simulated IST clock: read the clock, on a trading day wait for 15:50 (or
/// none when it has passed) and run for `run_secs`, then sleep by
/// `secs_until_after_checked_day` measured from the instant read. Runs until
/// the clock reaches `until`. It reuses the production helpers but NOT the
/// production loop, so it proves only that the helpers compose correctly in
/// this order; that the real loop keeps this order is pinned separately, by
/// source, in `test_spawn_expiry_scope_check_loop_body_matches_the_simulated_steps`.
fn simulate_daily_loop(
    calendar: &TradingCalendar,
    start: i64,
    run_secs: i64,
    until: i64,
) -> Vec<SimCheck> {
    let mut clock = start;
    let mut checks = Vec::new();
    let mut iterations = 0;
    while clock < until {
        iterations += 1;
        assert!(iterations < 1_000, "the loop must advance");
        let now_ist = clock;
        let today = ist_date_of(now_ist);
        let trading = today.is_some_and(|d| calendar.is_trading_day(d));
        if let (true, Some(day)) = (trading, today) {
            let wait = secs_until_expiry_scope_check(secs_of_day(now_ist));
            clock += i64::try_from(wait).expect("secs") * NANOS_PER_SEC;
            let begin = clock;
            clock += run_secs * NANOS_PER_SEC;
            checks.push((day, begin, clock));
        }
        let secs = secs_until_after_checked_day(now_ist, clock);
        clock += i64::try_from(secs).expect("secs") * NANOS_PER_SEC;
    }
    checks
}

/// The production loop's statements, in the order `simulate_daily_loop`
/// copies them. A source scan, not an execution: it fails when the real loop
/// drops the trading-day gate, the 15:50 wait, or the run, reads the clock
/// for the after-run sleep before the run, or measures that sleep from the
/// instant after the run instead of the instant read before it.
#[test]
fn test_spawn_expiry_scope_check_loop_body_matches_the_simulated_steps() {
    let me = production(include_str!("expiry_scope_check.rs"));
    let spawn = me
        .find("pub fn spawn_expiry_scope_check(")
        .expect("spawn fn");
    let body_start = spawn + me[spawn..].find("loop {").expect("loop");
    let body_end = body_start
        + me[body_start..]
            .find("async fn read_candle_contracts(")
            .expect("next fn");
    let body = &me[body_start..body_end];
    let steps = [
        "let now_ist = now_utc_nanos().saturating_add(IST_UTC_OFFSET_NANOS);",
        "let today = ist_date_of(now_ist);",
        "let trading = today.is_some_and(|d| calendar.is_trading_day(d));",
        "if let (true, Some(day)) = (trading, today) {",
        "let wait = secs_until_expiry_scope_check(secs_of_day(now_ist));",
        "tokio::time::sleep(Duration::from_secs(wait)).await;",
        "run_one_check(&questdb, day, now_ist).await;",
        "let after = now_utc_nanos().saturating_add(IST_UTC_OFFSET_NANOS);",
        "let secs = secs_until_after_checked_day(now_ist, after);",
        "tokio::time::sleep(Duration::from_secs(secs)).await;",
    ];
    let mut at = 0;
    for step in steps {
        let found = body[at..]
            .find(step)
            .unwrap_or_else(|| panic!("loop step missing or out of order: {step}"));
        at += found + step.len();
    }
    // The run sits inside the trading-day block: the block closes after the
    // run and before the after-run clock read.
    let run = body.find("run_one_check(").expect("run");
    let after = body.find("let after =").expect("after");
    let gate = body.find("if let (true, Some(day))").expect("gate");
    assert!(gate < run && run < after);
    assert!(body[run..after].contains('}'), "the run is gated");
    // Exactly one clock read before the run, exactly one after it, and the
    // checked-day instant is never reassigned.
    assert_eq!(body.matches("now_utc_nanos()").count(), 2);
    assert_eq!(body.matches("let now_ist").count(), 1);
    assert_eq!(body.matches("now_ist =").count(), 1, "now_ist reassigned");
    assert_eq!(body.matches("secs_until_after_checked_day(").count(), 1);
}

/// Composes `secs_until_expiry_scope_check` and
/// `secs_until_after_checked_day` with a real calendar through
/// `simulate_daily_loop` (a copy of the loop, see its doc).
#[test]
fn test_secs_until_after_checked_day_composed_in_the_daily_loop_checks_every_trading_day_once() {
    use chrono::Weekday;
    let calendar = sweep_calendar();
    let thursday = NaiveDate::from_ymd_opt(2026, 10, 29).expect("date");
    let saturday = NaiveDate::from_ymd_opt(2026, 10, 31).expect("date");
    assert_eq!(thursday.weekday(), Weekday::Thu);
    assert_eq!(saturday.weekday(), Weekday::Sat);
    let check_at = i64::from(EXPIRY_SCOPE_CHECK_SECS_OF_DAY_IST);
    assert_eq!(check_at, 57_000);
    let mut skipped_inside_long_runs = 0_usize;
    for start_day in [thursday, saturday] {
        for start_secs in [0_u32, 57_000 - 1, 57_000, 86_399] {
            for run_secs in [0_i64, 1_568, 30_000, 90_000] {
                let window: Vec<NaiveDate> = (0..21)
                    .filter_map(|k| start_day.checked_add_days(chrono::Days::new(k)))
                    .collect();
                let until = ist_at(window[20], 0) + NANOS_PER_DAY;
                let checks =
                    simulate_daily_loop(&calendar, ist_at(start_day, start_secs), run_secs, until);
                let label = format!("start {start_day} {start_secs}s run {run_secs}s");
                let mut per_day: HashMap<NaiveDate, usize> = HashMap::new();
                for &(day, begin, _) in &checks {
                    *per_day.entry(day).or_insert(0) += 1;
                    assert!(
                        calendar.is_trading_day(day),
                        "{label}: {day} is not a trading day"
                    );
                    assert_eq!(ist_date_of(begin), Some(day), "{label}");
                    assert!(
                        secs_of_day(begin) >= EXPIRY_SCOPE_CHECK_SECS_OF_DAY_IST,
                        "{label}: {day} checked before 15:50"
                    );
                }
                assert!(per_day.values().all(|&n| n == 1), "{label}: {per_day:?}");
                for &day in &window {
                    if !calendar.is_trading_day(day) || per_day.contains_key(&day) {
                        continue;
                    }
                    // A missed trading day must lie wholly inside one run
                    // longer than a day; nothing else may skip a day.
                    let day_start = ist_at(day, 0);
                    let inside = checks.iter().any(|&(_, begin, end)| {
                        begin <= day_start && end >= day_start + NANOS_PER_DAY
                    });
                    assert!(inside && run_secs > 86_400, "{label}: {day} was skipped");
                    skipped_inside_long_runs += 1;
                }
                // The process started on a trading day after 15:50 checks
                // that day at once.
                if calendar.is_trading_day(start_day) && start_secs >= 57_000 {
                    assert_eq!(
                        checks.first().map(|c| c.1),
                        Some(ist_at(start_day, start_secs)),
                        "{label}"
                    );
                }
                // The holidays and the weekend after a Friday run that
                // crosses midnight are never checked.
                for holiday in ["2026-11-10", "2026-11-16", "2026-10-31", "2026-11-01"] {
                    let h = NaiveDate::parse_from_str(holiday, "%Y-%m-%d").expect("date");
                    assert!(!per_day.contains_key(&h), "{label}: {holiday}");
                }
            }
        }
    }
    // The only skip is the one asserted per case above: a trading day lying
    // wholly inside one run longer than a day (90,000 s here; the real run is
    // bounded far below a day, see the 1,568 s case). No count bound is
    // claimed: each skip is already proven to be that shape, and a run of a
    // day or less skips nothing. This only proves the long-run case was
    // reached, so the per-case check is not vacuous.
    assert!(skipped_inside_long_runs > 0);
}

proptest! {
    /// For any instant of any day within about 200 years of the epoch (sub-
    /// second offsets and days before 1970 included) and any run of up to
    /// three days: when the run ended before the checked day's end plus the
    /// margin, the loop wakes on the NEXT day, at or after that point and
    /// less than one second past it (never a nanosecond early, so the same
    /// day is never read twice); otherwise it does not sleep at all.
    #[test]
    fn proptest_secs_until_after_checked_day_wakes_on_the_next_day_within_one_second(
        day in -73_000_i64..73_000,
        within in 0_i64..NANOS_PER_DAY,
        run in 0_i64..3 * NANOS_PER_DAY,
    ) {
        let checked = day * NANOS_PER_DAY + within;
        let now = checked + run;
        let target = (day + 1) * NANOS_PER_DAY
            + i64::try_from(EXPIRY_SCOPE_NEXT_DAY_MARGIN_SECS).expect("secs") * NANOS_PER_SEC;
        let secs = secs_until_after_checked_day(checked, now);
        if now < target {
            let wake = now + i64::try_from(secs).expect("secs") * NANOS_PER_SEC;
            prop_assert!(wake >= target, "woke {} ns early", target - wake);
            prop_assert!(wake - target < NANOS_PER_SEC);
            prop_assert_eq!(ist_date_of(wake), ist_date_of(checked).and_then(|d| d.succ_opt()));
        } else {
            prop_assert_eq!(secs, 0);
        }
    }
}

// ------------------------------------------- subscribe-time count shapes

#[test]
fn test_enforce_subscribe_expiry_scope_count_shapes() {
    // HAVELLS (ids 10, 11, 100.., 200.., 300) plus a NIFTY weekly (400..)
    // and monthly (500..), and an id-0 stock leg.
    let mut rows = havells_file();
    rows.extend(chain(
        400,
        "OPTIDX",
        "NIFTY",
        NIFTY_WEEKLY,
        &[24_000, 24_100],
    ));
    rows.extend(chain(500, "OPTIDX", "NIFTY", CURRENT, &[24_000, 24_100]));
    rows.push(opt(0, "OPTSTK", "HAVELLS", CURRENT, 1_300, "CE"));
    struct Shape {
        name: &'static str,
        instruments: Vec<SubscribeInstrument>,
        backup: Vec<SubscribeInstrument>,
        index: usize,
        stock: usize,
        refused: Vec<SubscribeInstrument>,
        left: Vec<SubscribeInstrument>,
        backup_left: Vec<SubscribeInstrument>,
        index_after: usize,
        stock_after: usize,
        unknown: usize,
    }
    let shapes = [
        Shape {
            name: "a key the file does not list lowers no class count",
            instruments: vec![inst(100), inst(777)],
            backup: vec![inst(777)],
            index: 0,
            stock: 2,
            refused: vec![inst(777)],
            left: vec![inst(100)],
            backup_left: vec![],
            index_after: 0,
            stock_after: 2,
            unknown: 1,
        },
        Shape {
            name: "an index refusal with the index count already 0 saturates",
            instruments: vec![inst(500)],
            backup: vec![],
            index: 0,
            stock: 3,
            refused: vec![inst(500)],
            left: vec![],
            backup_left: vec![],
            index_after: 0,
            stock_after: 3,
            unknown: 0,
        },
        Shape {
            name: "a key only in the backup rank is never judged",
            instruments: vec![inst(100)],
            backup: vec![inst(200)],
            index: 0,
            stock: 1,
            refused: vec![],
            left: vec![inst(100)],
            backup_left: vec![inst(200)],
            index_after: 0,
            stock_after: 1,
            unknown: 0,
        },
        Shape {
            name: "id 0 is never a key: refused as unknown, counted under its file class",
            instruments: vec![inst(0), inst(100)],
            backup: vec![inst(0)],
            index: 0,
            stock: 2,
            refused: vec![inst(0)],
            left: vec![inst(100)],
            backup_left: vec![],
            index_after: 0,
            stock_after: 1,
            unknown: 1,
        },
    ];
    for s in shapes {
        let mut selection = ContractSelection {
            instruments: s.instruments.clone(),
            backup_rank: s.backup.clone(),
            index_options: s.index,
            stock_options: s.stock,
            refused_off_expiry: 4,
            ..ContractSelection::default()
        };
        let report = enforce_subscribe_expiry_scope(&mut selection, &rows, TODAY);
        assert_eq!(report.refused, s.refused, "{}", s.name);
        assert_eq!(report.unknown, s.unknown, "{}", s.name);
        assert_eq!(selection.instruments, s.left, "{}", s.name);
        assert_eq!(selection.backup_rank, s.backup_left, "{}", s.name);
        assert_eq!(selection.index_options, s.index_after, "{}", s.name);
        assert_eq!(selection.stock_options, s.stock_after, "{}", s.name);
        assert_eq!(
            selection.refused_off_expiry,
            4 + s.refused.len(),
            "{}",
            s.name
        );
    }
}

#[test]
#[ignore = "BUG: a selection that holds one contract twice is judged twice: check_selected_expiries pushes it into `refused` twice, so refused_off_expiry rises by 2 and `checked`/`off_expiry` double count while stock_options drops by 1"]
fn test_enforce_subscribe_expiry_scope_counts_a_contract_selected_twice_once() {
    let rows = havells_file();
    let mut selection = ContractSelection {
        instruments: vec![inst(200), inst(200)],
        stock_options: 1,
        ..ContractSelection::default()
    };
    let report = enforce_subscribe_expiry_scope(&mut selection, &rows, TODAY);
    assert!(selection.instruments.is_empty());
    assert_eq!(selection.stock_options, 0);
    assert_eq!(report.off_expiry, 1);
    assert_eq!(report.checked, 1);
    assert_eq!(selection.refused_off_expiry, 1);
}

#[test]
#[ignore = "BUG: enforce_subscribe_expiry_scope's class lookup does not check is_option_row on the first insert, so a refused key whose only same-id row is a FUTURE (FUTIDX) lowers stock_options although the selector never counted it as a stock option"]
fn test_enforce_subscribe_expiry_scope_a_future_only_key_lowers_no_option_count() {
    let mut rows = havells_file();
    rows.push(opt(888, "FUTIDX", "NIFTY", CURRENT, 0, ""));
    let mut selection = ContractSelection {
        instruments: vec![inst(100), inst(888)],
        index_options: 3,
        stock_options: 1,
        ..ContractSelection::default()
    };
    let report = enforce_subscribe_expiry_scope(&mut selection, &rows, TODAY);
    assert_eq!(report.refused, vec![inst(888)]);
    assert_eq!(report.unknown, 1);
    assert_eq!(selection.index_options, 3);
    assert_eq!(selection.stock_options, 1);
}

// ------------------------------------------------- the refusal sample name

#[test]
fn test_check_selected_expiries_sample_names_every_refusal_shape() {
    let bse = |id: u64| SubscribeInstrument {
        security_id: id,
        segment: ExchangeSegment::BseFno,
    };
    // HAVELLS plus id 9 listed twice on two SBIN expiries (ambiguous; the
    // first row, the current one, is kept) and NSE id 5 on the SBIN chain.
    let mut rows = havells_file();
    rows.extend(chain(1, "OPTSTK", "SBIN", CURRENT, &[1_000, 1_020]));
    rows.push(opt(9, "OPTSTK", "SBIN", CURRENT, 1_040, "CE"));
    rows.push(opt(9, "OPTSTK", "SBIN", NEXT, 1_040, "CE"));
    rows.push(opt(5, "OPTSTK", "SBIN", CURRENT, 1_060, "PE"));
    // (selection, sample, unknown, checked, off_expiry, expired)
    let cases: Vec<(
        Vec<SubscribeInstrument>,
        Option<&str>,
        usize,
        usize,
        usize,
        usize,
    )> = vec![
        (
            vec![inst(777)],
            Some("security_id 777 segment NSE_FNO"),
            1,
            0,
            0,
            0,
        ),
        (
            vec![bse(777)],
            Some("security_id 777 segment BSE_FNO"),
            1,
            0,
            0,
            0,
        ),
        // The kept row of an ambiguous id names the sample.
        (vec![inst(9)], Some("SBIN-23Nov2026-1040-CE"), 1, 0, 0, 0),
        // BSE 5 when the file lists only NSE 5: unknown, never judged
        // against the NSE row.
        (
            vec![bse(5)],
            Some("security_id 5 segment BSE_FNO"),
            1,
            0,
            0,
            0,
        ),
        (vec![inst(5)], None, 0, 1, 0, 0),
        // The first refusal names the sample; refusals keep selection order.
        (
            vec![inst(100), bse(5), inst(200), inst(300)],
            Some("security_id 5 segment BSE_FNO"),
            1,
            3,
            1,
            1,
        ),
    ];
    for (selected, sample, unknown, checked, off, expired) in cases {
        let report = check_selected_expiries(&selected, &rows, TODAY);
        assert_eq!(report.sample.as_deref(), sample, "{selected:?}");
        assert_eq!(report.unknown, unknown, "{selected:?}");
        assert_eq!(report.checked, checked, "{selected:?}");
        assert_eq!(report.off_expiry, off, "{selected:?}");
        assert_eq!(report.expired, expired, "{selected:?}");
        let expected_refused: Vec<SubscribeInstrument> = selected
            .iter()
            .copied()
            .filter(|i| {
                !(i.segment == ExchangeSegment::NseFno && [100, 5].contains(&i.security_id))
            })
            .collect();
        assert_eq!(report.refused, expected_refused, "{selected:?}");
    }
}

// ------------------------------------- the subscribe-time log de-duplication

/// Serialises the tests that drive `record_subscribe_expiry_scope`: it
/// writes process-wide day and fingerprint memory.
static RECORD_SCOPE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Counts ERROR events on the current thread.
struct ErrorEvents(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for ErrorEvents {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if *event.metadata().level() == tracing::Level::ERROR {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The number of `error!` lines one `record_subscribe_expiry_scope` call
/// writes.
fn errors_logged_by_record(report: &SubscribeScopeReport, day: u32) -> usize {
    use tracing_subscriber::layer::SubscriberExt;
    let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let subscriber = tracing_subscriber::registry().with(ErrorEvents(std::sync::Arc::clone(&n)));
    tracing::subscriber::with_default(subscriber, || record_subscribe_expiry_scope(report, day));
    n.load(Ordering::Relaxed)
}

fn refused_report(ids: &[u64]) -> SubscribeScopeReport {
    SubscribeScopeReport {
        off_expiry: ids.len(),
        refused: ids.iter().map(|&id| inst(id)).collect(),
        ..SubscribeScopeReport::default()
    }
}

#[test]
fn test_record_subscribe_expiry_scope_logs_each_refused_set_once_per_day() {
    let _guard = RECORD_SCOPE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let a = refused_report(&[300]);
    let clean = SubscribeScopeReport::default();
    // (step report, day, error lines expected), days no other test uses.
    let scenarios: [&[(&SubscribeScopeReport, u32, usize)]; 4] = [
        &[(&a, 19_970_201, 1), (&a, 19_970_201, 0)],
        &[
            (&a, 19_970_203, 1),
            (&clean, 19_970_203, 0),
            (&a, 19_970_203, 0),
        ],
        &[(&a, 19_970_205, 1), (&a, 19_970_206, 1)],
        &[(&a, 19_970_209, 1), (&a, 19_970_208, 1)],
    ];
    for steps in scenarios {
        for &(report, day, expected) in steps {
            assert_eq!(errors_logged_by_record(report, day), expected, "{steps:?}");
        }
    }
}

#[test]
#[ignore = "BUG: record_subscribe_expiry_scope remembers only the LAST logged fingerprint (SUBSCRIBE_SCOPE_LOGGED), so a selection flapping A, B, A on one day logs A's EXPIRY-SCOPE-01 error twice, against the documented 'the first time that exact set is refused that day'"]
fn test_record_subscribe_expiry_scope_a_flapping_selection_logs_each_set_once() {
    let _guard = RECORD_SCOPE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let a = refused_report(&[300]);
    let b = refused_report(&[200]);
    let day = 19_970_301;
    assert_eq!(errors_logged_by_record(&a, day), 1);
    assert_eq!(errors_logged_by_record(&b, day), 1);
    assert_eq!(errors_logged_by_record(&a, day), 0);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    /// Collision sanity (not proof): two different refused sets of up to 50
    /// composite keys on one day never share a fingerprint in 10,000 draws,
    /// and the fingerprint ignores the order of the set.
    #[test]
    fn proptest_refusal_fingerprint_two_distinct_sets_differ(
        a in proptest::collection::hash_set((0_u64..200, any::<bool>()), 1..50),
        b in proptest::collection::hash_set((0_u64..200, any::<bool>()), 1..50),
        day in 19_900_101_u32..21_001_231,
    ) {
        let to_report = |set: &HashSet<(u64, bool)>| {
            let mut refused: Vec<SubscribeInstrument> = set
                .iter()
                .map(|&(id, bse)| SubscribeInstrument {
                    security_id: id,
                    segment: if bse { ExchangeSegment::BseFno } else { ExchangeSegment::NseFno },
                })
                .collect();
            refused.sort_by_key(|i| (i.security_id, i.segment.binary_code()));
            SubscribeScopeReport { refused, ..SubscribeScopeReport::default() }
        };
        let fa = refusal_fingerprint(&to_report(&a), day);
        prop_assert_ne!(fa, 0);
        let mut reversed = to_report(&a);
        reversed.refused.reverse();
        prop_assert_eq!(fa, refusal_fingerprint(&reversed, day));
        if a != b {
            prop_assert_ne!(fa, refusal_fingerprint(&to_report(&b), day));
        }
    }
}

// -------------------------------------- candle-row key parsing and samples

#[test]
fn test_classify_candle_contracts_segment_and_id_edges() {
    // HAVELLS plus an id-0 HAVELLS leg and a whole-thin THIN bucket (ids 600,
    // 601, two legs).
    let mut file = havells_file();
    file.push(opt(0, "OPTSTK", "HAVELLS", CURRENT, 1_300, "CE"));
    file.push(opt(600, "OPTSTK", "THIN", CURRENT, 50, "CE"));
    file.push(opt(601, "OPTSTK", "THIN", CURRENT, 50, "PE"));
    let index = build_expiry_index(&file, TODAY, None);
    let good = Some("HAVELLS-23Nov2026-1240-CE");
    let base = ExpiryScopeCounts {
        rows_read: 1,
        ..ExpiryScopeCounts::default()
    };
    let checked = ExpiryScopeCounts {
        contracts_checked: 1,
        ..base
    };
    let cases = [
        // A lowercase segment does not parse: unresolved.
        (
            candle(100, "nse_fno", good),
            ExpiryScopeCounts {
                unresolved: 1,
                ..base
            },
        ),
        // Surrounding whitespace is trimmed: judged from the file.
        (candle(100, " NSE_FNO ", good), checked),
        // A valid non-F&O segment never matches a file key; its name is
        // looked up in that segment, which has no chain.
        (
            candle(100, "NSE_EQ", good),
            ExpiryScopeCounts {
                unknown_underlying: 1,
                ..base
            },
        ),
        (
            candle(100, "IDX_I", good),
            ExpiryScopeCounts {
                unknown_underlying: 1,
                ..base
            },
        ),
        // Id 0 is never a file key, even when the file lists it: judged by
        // its name.
        (
            candle(0, "NSE_FNO", Some("HAVELLS-23Nov2026-1300-CE")),
            checked,
        ),
        // The largest id: name-only, judged by name.
        (candle(i64::MAX, "NSE_FNO", good), checked),
        (
            candle(-1, "NSE_FNO", good),
            ExpiryScopeCounts {
                unresolved: 1,
                ..base
            },
        ),
        // A file-listed contract whose whole bucket is thin: unknown
        // underlying through the file row (and unlabelled: no name).
        (
            candle(600, "NSE_FNO", None),
            ExpiryScopeCounts {
                unknown_underlying: 1,
                unlabelled: 1,
                ..base
            },
        ),
    ];
    for (row, expected) in cases {
        let (counts, sample) = classify_candle_contracts(std::slice::from_ref(&row), &index, TODAY);
        assert_eq!(counts, expected, "{row:?}");
        assert_eq!(sample, None, "{row:?}");
    }
}

#[test]
fn test_classify_candle_contracts_sample_is_the_lowest_segment_code_then_id_in_any_order() {
    // BSE id 3 and NSE id 9 are both off expiry. NSE_FNO's wire code (2) is
    // below BSE_FNO's (8), so NSE 9 is the sample although 3 < 9.
    let mut file = chain(20, "OPTIDX", "SENSEX", CURRENT, &[80_000, 80_100]);
    file.push(opt(3, "OPTIDX", "SENSEX", NEXT, 80_000, "CE"));
    for r in &mut file {
        r.x = "BSE".into();
    }
    file.extend(chain(40, "OPTSTK", "SBIN", CURRENT, &[1_000, 1_020]));
    file.push(opt(9, "OPTSTK", "SBIN", NEXT, 1_000, "CE"));
    let index = build_expiry_index(&file, TODAY, None);
    let bse = candle(3, "BSE_FNO", None);
    let nse = candle(9, "NSE_FNO", None);
    for rows in [vec![bse.clone(), nse.clone()], vec![nse, bse]] {
        let (counts, sample) = classify_candle_contracts(&rows, &index, TODAY);
        assert_eq!(counts.off_expiry, 2);
        assert_eq!(sample.as_deref(), Some("SBIN-29Dec2026-1000-CE"));
    }
}

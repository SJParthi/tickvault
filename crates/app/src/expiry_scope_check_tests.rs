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
    // Only one usable leg: no real chain.
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
        // The daily check judges it from the admissible row: current.
        let (counts, _) = classify_candle_contracts(
            &[candle(1, "NSE_FNO", Some("SBIN-23Nov2026-1000-CE"))],
            &index,
            TODAY,
        );
        assert_eq!(counts.contracts_checked, 1);
        assert_eq!(counts.unresolved, 0);
        assert_eq!(counts.violations(), 0);
    }
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

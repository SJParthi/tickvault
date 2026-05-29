//! Real-code verification of the F&O extraction over the Dhan Detailed CSV.
//!
//! The operator's guarantee demand (2026-05-29): "did you really go through
//! the entire CSV row by row, column by column?" — answered here by running the
//! ACTUAL production pipeline (`parse_detailed_csv` → `extract_fno_underlyings`
//! → `collect_applicable_fno_contracts`) over CSV bytes and asserting the
//! per-instrument-type breakdown. NOT hand-built `CsvRow`s, NOT awk — the real
//! code over every row.
//!
//! Two tests:
//!  1. `fno_extraction_captures_nse_bse_index_and_resolved_stock_fno` — a
//!     CI-safe synthetic CSV that encodes the exact 2026-05-29 bug shape
//!     (index F&O underlying SID 26000/26009/1/12 ≠ spot 13/25/51) and proves
//!     the fix end-to-end through the real parser. Ratchet: fails the build if
//!     index F&O is ever dropped again.
//!  2. `verify_real_csv_breakdown` (`#[ignore]`) — operator-run over the real
//!     ~219K-row file. Prints the full captured breakdown vs the raw CSV so the
//!     operator can confirm completeness on the actual snapshot:
//!       TICKVAULT_VERIFY_CSV=/path/to/api-scrip-master-detailed.csv \
//!         cargo test -p tickvault-core --features daily_universe_fetcher \
//!         --test fno_extraction_verification -- --ignored --nocapture

#![cfg(feature = "daily_universe_fetcher")]

use std::collections::BTreeMap;

use tickvault_core::instrument::csv_parser::{CsvRow, parse_detailed_csv};
use tickvault_core::instrument::fno_underlying_extractor::{
    collect_applicable_fno_contracts, extract_fno_underlyings,
};

/// Count captured contracts grouped by raw `INSTRUMENT` type.
fn breakdown_by_instrument(contracts: &[CsvRow]) -> BTreeMap<String, usize> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for row in contracts {
        *counts.entry(row.instrument.clone()).or_default() += 1;
    }
    counts
}

const HEADER: &str =
    "SECURITY_ID,EXCH_ID,SEGMENT,INSTRUMENT,SYMBOL_NAME,UNDERLYING_SECURITY_ID,UNDERLYING_SYMBOL";

#[test]
fn fno_extraction_captures_nse_bse_index_and_resolved_stock_fno() {
    // A representative slice of the real Detailed-CSV SHAPE — crucially with
    // the index-underlying-SID quirk that broke extraction: NIFTY option's
    // UNDERLYING_SECURITY_ID is 26000 (NOT the IDX_I spot 13), SENSEX is 1,
    // BANKEX is 12. Run through the REAL parser + extractor.
    let mut csv = String::from(HEADER);
    csv.push('\n');
    // NSE_EQ spot for RELIANCE so its stock F&O underlying resolves.
    csv.push_str("2885,NSE,E,EQUITY,RELIANCE,,\n");
    // Index spots (IDX_I) — realistic context.
    csv.push_str("13,NSE,I,INDEX,NIFTY,13,NIFTY\n");
    csv.push_str("51,BSE,I,INDEX,SENSEX,51,SENSEX\n");
    // Stock F&O for RELIANCE (underlying 2885 resolves to the NSE_EQ spot).
    csv.push_str("40001,NSE,D,FUTSTK,RELIANCE26JUNFUT,2885,RELIANCE\n");
    csv.push_str("40002,NSE,D,OPTSTK,RELIANCE26JUN2800CE,2885,RELIANCE\n");
    // NSE index F&O — derivatives-domain underlying SID (NOT spot 13/25).
    csv.push_str("35000,NSE,D,OPTIDX,NIFTY26JUN24000CE,26000,NIFTY\n");
    csv.push_str("35001,NSE,D,FUTIDX,NIFTY26JUNFUT,26000,NIFTY\n");
    csv.push_str("35002,NSE,D,OPTIDX,BANKNIFTY26JUN50000CE,26009,BANKNIFTY\n");
    // BSE index F&O — SENSEX (underlying 1) + BANKEX (underlying 12).
    csv.push_str("12001,BSE,D,OPTIDX,SENSEX26JUN80000CE,1,SENSEX\n");
    csv.push_str("12002,BSE,D,OPTIDX,BANKEX26JUN60000CE,12,BANKEX\n");
    // MCX commodity-index F&O — MUST be excluded (no-commodity lock).
    csv.push_str("90001,MCX,D,OPTIDX,MCXBULLDEX26JUN100CE,568,MCXBULLDEX\n");
    // Currency F&O — excluded (not a stock/index derivative).
    csv.push_str("90002,NSE,D,OPTCUR,USDINR26JUN90CE,13,USDINR\n");

    let rows = parse_detailed_csv(csv.as_bytes()).expect("parse");
    let fno = extract_fno_underlyings(&rows).expect("extract underlyings");
    let contracts = collect_applicable_fno_contracts(&rows, &fno);
    let b = breakdown_by_instrument(&contracts);

    // Index F&O captured (THE fix): 4 OPTIDX (NIFTY, BANKNIFTY, SENSEX, BANKEX)
    // + 1 FUTIDX (NIFTY). Pre-fix these were ALL dropped (0).
    assert_eq!(
        *b.get("OPTIDX").unwrap_or(&0),
        4,
        "NSE+BSE index options: {b:?}"
    );
    assert_eq!(
        *b.get("FUTIDX").unwrap_or(&0),
        1,
        "NSE index futures: {b:?}"
    );
    // Stock F&O for the resolved underlying.
    assert_eq!(*b.get("OPTSTK").unwrap_or(&0), 1, "stock options: {b:?}");
    assert_eq!(*b.get("FUTSTK").unwrap_or(&0), 1, "stock futures: {b:?}");

    let syms: Vec<&str> = contracts.iter().map(|r| r.symbol_name.as_str()).collect();
    // MCX commodity-index + currency must NOT leak in.
    assert!(
        !syms.iter().any(|s| s.contains("MCXBULLDEX")),
        "MCX excluded: {syms:?}"
    );
    assert!(
        !syms.iter().any(|s| s.contains("USDINR")),
        "currency excluded: {syms:?}"
    );
}

#[test]
#[ignore = "operator-run: set TICKVAULT_VERIFY_CSV=/path/to/api-scrip-master-detailed.csv"]
fn verify_real_csv_breakdown() {
    let Ok(path) = std::env::var("TICKVAULT_VERIFY_CSV") else {
        eprintln!("set TICKVAULT_VERIFY_CSV to a Dhan Detailed CSV path to run this");
        return;
    };
    let bytes = std::fs::read(&path).expect("read CSV file");
    let rows = parse_detailed_csv(&bytes).expect("parse CSV");
    let fno = extract_fno_underlyings(&rows).expect("extract underlyings");
    let contracts = collect_applicable_fno_contracts(&rows, &fno);
    let b = breakdown_by_instrument(&contracts);

    eprintln!(
        "=== applicable F&O master (REAL code over {} CSV rows) ===",
        rows.len()
    );
    let mut total = 0usize;
    for (kind, count) in &b {
        eprintln!("  {kind:<8} = {count}");
        total += *count;
    }
    eprintln!("  -------------------------");
    eprintln!("  TOTAL contracts          = {total}");
    eprintln!(
        "  resolved NSE underlyings = {}",
        fno.unique_underlying_ids.len()
    );

    // Sanity: after the fix, index F&O MUST be present (it was 0 pre-fix).
    assert!(
        *b.get("OPTIDX").unwrap_or(&0) > 0,
        "index options MUST be captured after the 2026-05-29 fix"
    );
}

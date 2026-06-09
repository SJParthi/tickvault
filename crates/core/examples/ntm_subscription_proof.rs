//! Re-runnable PROOF that the NIFTY Total Market constituent pipeline works on
//! the REAL niftyindices file: parse → ISIN-map → resolve (the §31 / §31.1
//! chain that feeds the live subscription).
//!
//! The charter demands "no hallucination" — the "748 stocks, 100% ISIN,
//! 100% resolve" figures must be MEASURED, not claimed. Run it anytime:
//!
//! ```bash
//! cargo run -p tickvault-core --example ntm_subscription_proof \
//!     --features daily_universe_fetcher -- <path-to-ind_niftytotalmarket_list.csv>
//! ```
//!
//! HONEST ENVELOPE: a TRUE end-to-end live run (fetch niftyindices + fetch the
//! Dhan master + resolve against Dhan's REAL security_ids + subscribe on a real
//! socket) needs outbound network + a Dhan token. Where those are unavailable
//! (sandbox egress blocked / no secrets), this harness proves the deterministic
//! data pipeline against the operator's real file, assigning SYNTHETIC Dhan
//! security_ids (clearly labelled). The real security_ids come from Dhan's
//! master at boot — this proves the JOIN logic is correct for every real ISIN.

use tickvault_common::instrument_types::IndexConstituencyMap;
use tickvault_core::instrument::constituent_resolver::resolve_constituents;
use tickvault_core::instrument::csv_parser::CsvRow;
use tickvault_core::instrument::index_constituency::parser::parse_constituents;

fn synthetic_nse_eq_row(security_id: u32, symbol: &str, isin: &str) -> CsvRow {
    CsvRow {
        security_id: security_id.to_string(),
        exch_id: "NSE".to_string(),
        segment: "NSE_EQ".to_string(),
        instrument: "EQUITY".to_string(),
        symbol_name: symbol.to_string(),
        underlying_security_id: String::new(),
        underlying_symbol: String::new(),
        display_name: symbol.to_string(),
        lot_size: 1,
        tick_size: 0.05,
        expiry_date: "0001-01-01".to_string(),
        strike_price: 0.0,
        option_type: String::new(),
        isin: isin.to_string(),
    }
}

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        "/root/.claude/uploads/72116791-a4a5-5e3c-8596-37748725ab9e/\
         2221b455-ind_niftytotalmarket_list.csv"
            .to_string()
    });
    let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 6).expect("date");

    println!("=== NTM SUBSCRIPTION PROOF (real file: {path}) ===\n");

    let raw = std::fs::read(&path).expect("read CSV");
    println!(
        "STEP 1 — download stand-in: read {} bytes from disk",
        raw.len()
    );

    // STEP 2 — REAL parser.
    let parsed = parse_constituents(&raw, "Nifty Total Market", today).expect("parse");
    println!(
        "STEP 2 — REAL parse_constituents():\n  \
         constituents kept      = {}\n  \
         skipped blank rows     = {}\n  \
         missing ISIN           = {}\n  \
         skipped non-EQ series  = {}",
        parsed.constituents.len(),
        parsed.skipped_blank_rows,
        parsed.missing_isin,
        parsed.skipped_non_eq,
    );

    // STEP 3 — build the IndexConstituencyMap (what build_constituency_map does).
    let mut map = IndexConstituencyMap::default();
    map.index_to_constituents.insert(
        "Nifty Total Market".to_string(),
        parsed.constituents.clone(),
    );
    for c in &parsed.constituents {
        map.stock_to_indices
            .entry(c.symbol.clone())
            .or_default()
            .push("Nifty Total Market".to_string());
    }
    println!(
        "STEP 3 — IndexConstituencyMap: {} index, {} unique stocks",
        map.index_to_constituents.len(),
        map.stock_to_indices.len()
    );

    // STEP 4 — synthetic Dhan master from the REAL ISINs (real IDs come from
    // Dhan at boot; here we prove the JOIN matches every real ISIN).
    let dhan_rows: Vec<CsvRow> = parsed
        .constituents
        .iter()
        .enumerate()
        .map(|(i, c)| synthetic_nse_eq_row(100_000 + i as u32, &c.symbol, &c.isin))
        .collect();

    // STEP 5 — REAL resolver against the real ISINs.
    let outcome = resolve_constituents(&map, &dhan_rows).expect("resolve");
    let via_isin = outcome.resolved.iter().filter(|r| r.via_isin).count();
    println!(
        "STEP 5 — REAL resolve_constituents():\n  \
         total considered = {}\n  \
         resolved         = {}\n  \
         via ISIN (primary) = {}\n  \
         unresolved (dangling) = {}",
        outcome.total,
        outcome.resolved.len(),
        via_isin,
        outcome.unresolved_symbols.len(),
    );

    // STEP 6 — rename-proof check: change a symbol but keep its ISIN.
    if let Some(first) = parsed.constituents.first() {
        let mut renamed = dhan_rows.clone();
        renamed[0].symbol_name = "RENAMED_TICKER".to_string(); // ISIN unchanged
        let r2 = resolve_constituents(&map, &renamed).expect("resolve renamed");
        let still = r2
            .resolved
            .iter()
            .any(|x| x.isin == first.isin.trim().to_uppercase() && x.via_isin);
        println!(
            "STEP 6 — rename-proof: '{}' renamed ticker still resolved via ISIN = {}",
            first.symbol, still
        );
    }

    let pct = if outcome.total > 0 {
        100.0 * outcome.resolved.len() as f64 / outcome.total as f64
    } else {
        0.0
    };
    println!(
        "\n=== VERDICT: {}/{} resolved ({pct:.2}%) ===",
        outcome.resolved.len(),
        outcome.total
    );
}

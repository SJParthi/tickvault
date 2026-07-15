//! RETIRED (PR-C3, 2026-07-14 — Dhan instrument-chain deletion, operator
//! retirement directive 2026-07-13 per websocket-connection-scope-lock.md
//! "2026-07-13 Amendment" §B; Q3: "hereafter no Dhan instrument
//! download/parsing").
//!
//! This feature-gated integration suite verified the F&O underlying
//! extraction over the Dhan Detailed CSV (`fno_underlying_extractor` /
//! `csv_parser`), both DELETED with the chain and the
//! `daily_universe_fetcher` feature itself.

#[test]
fn fno_extraction_verification_suite_retired_with_the_dhan_instrument_chain() {
    // Tombstone: the module-level doc above records why every assertion in
    // this suite retired.
}

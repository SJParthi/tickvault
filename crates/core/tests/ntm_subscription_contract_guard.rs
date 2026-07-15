//! RETIRED (PR-C3, 2026-07-14 — Dhan instrument-chain deletion, operator
//! retirement directive 2026-07-13 per websocket-connection-scope-lock.md
//! "2026-07-13 Amendment" §B; Q3: "hereafter no Dhan instrument
//! download/parsing").
//!
//! This suite pinned the §31 NTM-union subscription contract (the
//! niftyindices constituent resolve → Dhan NSE-EQ union, the 2% dangling
//! tolerance, the role tagging) against the now-DELETED
//! `constituent_resolver` / `daily_universe` / `market_open_self_test`
//! integration surface. The §31/§31.1 NTM contract itself LIVES ON on the
//! GROWW side (the watch build consumes the niftyindices CSV via its own
//! hardened client — `build_isin_token_map` in `feed/groww/instruments.rs`,
//! with its own tests).

#[test]
fn ntm_subscription_contract_suite_retired_with_the_dhan_instrument_chain() {
    // Tombstone: the module-level doc above records why every assertion in
    // this suite retired.
}

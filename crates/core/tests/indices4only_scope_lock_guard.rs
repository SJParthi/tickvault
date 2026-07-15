//! RETIRED (PR-C3, 2026-07-14 — Dhan instrument-chain deletion, operator
//! retirement directive 2026-07-13 per websocket-connection-scope-lock.md
//! "2026-07-13 Amendment" §B item 2).
//!
//! This ratchet existed to pin the `SubscriptionScope` enum (blocking the
//! retired `FullUniverse` / `IndicesOnlyAllExpiries` /
//! `IndicesUnderlyingsOnly` variants and the 3 retired `subscribe_*` flags
//! from reappearing anywhere in `crates/`) plus the
//! `FnoUniverse::locked_4_idx_i()` hotfix regression. PR-C3 deleted the
//! ENUM itself, `subscription_planner.rs`, `LOCKED_UNIVERSE`, and
//! `locked_4_idx_i()` — a stronger state than any variant pin. The
//! surviving negative pin (the planner file must STAY deleted) lives in
//! `crates/storage/tests/daily_universe_scope_guard.rs::futidx_scope_legacy_gate_still_false`.

#[test]
fn indices4only_scope_lock_suite_retired_with_the_subscription_surface() {
    // Tombstone: the module-level doc above records why every assertion in
    // this suite retired. Re-introducing ANY Dhan market-data subscription
    // surface requires a fresh dated operator quote in
    // websocket-connection-scope-lock.md FIRST (§D of the amendment).
}

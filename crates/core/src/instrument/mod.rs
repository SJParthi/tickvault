//! Instrument-domain SURVIVORS of the Dhan instrument-download chain.
//!
//! PR-C3 (2026-07-14, operator retirement directive 2026-07-13 —
//! `websocket-connection-scope-lock.md` "2026-07-13 Amendment" §A/§B; Q3:
//! "hereafter no Dhan instrument download/parsing — just direct hardcoded
//! security IDs passed to spot 1m and option chain"): the entire Dhan
//! instrument-master download/parse/universe chain is DELETED —
//! `csv_downloader`, `csv_parser` (its [`csv_row::CsvRow`] type split out
//! per the C1 mandate), `fno_underlying_extractor`, `daily_universe` (+
//! orchestrator), `constituent_resolver`, `index_constituency` (module; the
//! `INDEX_CONSTITUENCY_*` constants in `tickvault_common::constants` are
//! KEPT — the Groww watch build consumes the niftyindices CSV via its own
//! client), the `instr_fetch_*` retry chain (policy/adapter/loop/runner),
//! the boot-day/boot-deadline guards (`boot_time_of_day_guard`,
//! `boot_day_classifier`(+`_extended`), `boot_complete_by_guard`,
//! `l3_anomaly_check`, `nse_holiday_cross_check`), the warm-boot
//! plan-snapshot machinery (`instrument_snapshot` trimmed to its surviving
//! path-traversal guard), the Dhan presence slot build
//! (`presence_registration` — DELETED 2026-07-18, stage-4: its last
//! surviving fn `ist_day_from_date` had zero callers after the presence
//! registry retired), and the
//! subscription planner/distribution (`subscription_planner`,
//! `subscription_distribution` — scope-lock §B item 2, with the
//! `SubscriptionScope` enum + `LOCKED_UNIVERSE`).
//!
//! What SURVIVES (the scope-lock §B KEEP/REWIRE seam — every item verified
//! to have live consumers):
//!
//! | Module | Why it lives |
//! |---|---|
//! | `index_extractor` | `NSE_INDEX_ALLOWLIST` + `canonicalize_index_symbol` — the Groww watch build + scoreboard consume the canonicalizer |
//! | `index_futures` | the §36.7 shared FUTIDX expiry selector — the GROWW futures leg stands (de-gated in C1; must never regain a feature gate) |
//! | `csv_row` | the shared instrument-row TYPE the two modules above consume (split out of the deleted parser) |
//! | `instrument_snapshot` | trimmed to `is_valid_trading_date` (the fail-closed date/path-traversal guard — Groww activation consumes) |
//! | `market_open_self_test` | dormant contract stub (pure evaluator; its spawner died with the lane in C2 — RETAINED by the C4 sweep 2026-07-15: its SELFTEST-01/02 codes were not in the operator-authorized C4 deletion set; a future sweep needs its own ruling) |
//!
//! `slo_score` was DELETED in the C4 sweep (2026-07-15) with its bench +
//! the SLO-01/02/03 ErrorCode variants (caller-less contract stub; the
//! wave-3-d PARK banner authorized "Phase C variant cleanup").

// Historical deletion trail (pre-C3): PR #4/#6a/#6b (2026-05-19) removed the
// bhavcopy/binary-cache/depth/diagnostic/loader/universe-builder generation;
// the 2026-05-27 daily-universe expansion rebuilt the fetch chain behind the
// `daily_universe_fetcher` feature; PR-C3 (2026-07-14) deleted that chain and
// the feature itself.
pub mod csv_row;
pub mod index_extractor;
pub mod index_futures;
pub mod instrument_snapshot;
pub mod market_open_self_test;

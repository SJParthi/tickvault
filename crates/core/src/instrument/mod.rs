//! Master instrument download, parsing, and F&O universe building.
//!
//! Downloads Dhan's instrument master CSV daily, parses it, runs a 5-pass
//! mapping algorithm, and produces a complete `FnoUniverse` with all lookup
//! maps ready for downstream consumption.
//!
//! # Boot Sequence Position
//! Config -> **Instrument Download -> Universe Build** -> Auth -> WebSocket

// PR #6a (2026-05-19): bhavcopy_cross_check + bhavcopy_fetcher + bhavcopy_scheduler
// DELETED — 16:30 IST NSE bhavcopy cross-check retired under 4-IDX_I LOCKED_UNIVERSE.
// PR #6b (2026-05-19): binary_cache module RETIRED — no rkyv cache under 4-IDX_I LOCKED_UNIVERSE.
// PR #6a (2026-05-19): daily_scheduler + delta_detector RETIRED
// (4-IDX_I LOCKED_UNIVERSE — no daily Dhan CSV refresh = no day-over-day delta).
// PR #4 (2026-05-19): 6 depth modules DELETED.
// PR #6a (2026-05-19): diagnostic module RETIRED (no CSV download/parse/validate cycle).
// PR #4 (2026-05-19): `dynamic_subscription_state` module DELETED
// (depth-only diff state machine; no consumers under 4-IDX_I scope).
// PR #6b (2026-05-19): instrument_loader module RETIRED (boot uses FnoUniverse::locked_4_idx_i).
// PR #6b (2026-05-19): universe_builder + csv_downloader + csv_parser + validation
// modules RETIRED — boot reads LOCKED_UNIVERSE constant; no CSV pipeline.
pub mod market_open_self_test;
pub mod slo_score;
pub mod subscription_distribution;
pub mod subscription_planner;

// Sub-PR #3 of 2026-05-27 daily-universe expansion: hardened
// instrument-master CSV downloader (re-introduced — predecessor was
// deleted in PR #6b under 4-IDX_I scope; reborn behind the
// daily_universe_fetcher feature flag per §21 of the rule file).
#[cfg(feature = "daily_universe_fetcher")]
pub mod csv_downloader;

// Sub-PR #4 of 2026-05-27 daily-universe expansion: robust parser for
// the Dhan Detailed instrument-master CSV. Same feature flag as the
// downloader — both compose into Sub-PR #10's orchestrator.
#[cfg(feature = "daily_universe_fetcher")]
pub mod csv_parser;

// Sub-PR #5 of 2026-05-27 daily-universe expansion: extract the set
// of unique F&O underlying SecurityIds + cross-validate the dangling-
// reference invariant from parsed CSV rows.
#[cfg(feature = "daily_universe_fetcher")]
pub mod fno_underlying_extractor;

// Sub-PR #6 of 2026-05-27 daily-universe expansion: extract every
// IDX_I INDEX row (NSE indices + 1 BSE SENSEX) per §2 universe scope.
#[cfg(feature = "daily_universe_fetcher")]
pub mod index_extractor;

// Sub-PR #7 of 2026-05-27 daily-universe expansion: combine the F&O
// underlyings (Sub-PR #5) + indices (Sub-PR #6) into a unified
// DailyUniverse with size-envelope enforcement.
#[cfg(feature = "daily_universe_fetcher")]
pub mod daily_universe;

// §36 (2026-07-08) / §36.7 (2026-07-10): all-monthly-expiries FUTIDX
// selection — the ONE shared pure selector both the Dhan orchestrator and
// the Groww watch builder call.
#[cfg(feature = "daily_universe_fetcher")]
pub mod index_futures;

// Scoreboard PR-D (2026-07-11): Dhan-side presence-registry registration —
// derives the cross-feed pairing keys (ISIN / canonical index / contract
// identity) from the built DailyUniverse at BOTH boot seams.
#[cfg(feature = "daily_universe_fetcher")]
pub mod presence_registration;

// Sub-PR #10 of 2026-05-27 daily-universe expansion: chain the
// CSV parser + extractors + universe builder into a single pure
// function. Maps each underlying error to the right INSTR-FETCH-*
// ErrorCode for Telegram routing + CloudWatch tagging.
#[cfg(feature = "daily_universe_fetcher")]
pub mod daily_universe_orchestrator;

// PR-2 (2026-05-29 disaster-recovery): date-keyed subscription-plan
// snapshot on host disk (separate from the QuestDB volume) for instant
// warm-resubscribe after a same-day crash / DB wipe. Same feature flag —
// it caches the output of the DailyUniverse build.
#[cfg(feature = "daily_universe_fetcher")]
pub mod instrument_snapshot;

// NTM Sub-PR #3 of 2026-05-27 daily-universe expansion: rebuild of the
// niftyindices.com index-constituency downloader + parser + cache (the
// predecessor was deleted under 4-IDX_I scope; constants + types survived).
// Builds the index→stocks map per §31.1; the Dhan-master ISIN join +
// boot wiring land in Sub-PR #4 / #10. Same feature flag.
#[cfg(feature = "daily_universe_fetcher")]
pub mod index_constituency;

// NTM Sub-PR #4 of 2026-05-27 daily-universe expansion: resolve
// niftyindices constituents → Dhan NSE-EQ security_id via the §31.1
// ISIN-primary (symbol-fallback) join, fail-closed on >0.5% dangling.
// Role tagging + union with F&O underlyings happen at subscription
// wiring (Sub-PR #5). Same feature flag.
#[cfg(feature = "daily_universe_fetcher")]
pub mod constituent_resolver;

// Sub-PR #10b-α of 2026-05-27 daily-universe expansion: pure-function
// retry policy primitives for the §4 infinite-retry escalation ladder.
// Boot stays BLOCKED until a fresh validated CSV is in hand — these
// helpers compute the backoff schedule + Telegram severity tier so the
// full boot orchestrator (Sub-PR #10b integration) can drive the loop
// deterministically.
#[cfg(feature = "daily_universe_fetcher")]
pub mod instr_fetch_retry_policy;

// Sub-PR #10b-β of 2026-05-27 daily-universe expansion: pure-function
// adapter that maps `(OrchestratorError, attempt)` into a
// `RetryDecision` carrying the §4 backoff `Duration` + an optional
// Telegram emit payload (severity, error_code, stage). Bridges
// Sub-PR #10 (orchestrator chain) with Sub-PR #10b-α (retry policy
// primitives). Boot orchestrator (Sub-PR #10b proper) owns the
// `tokio::sleep` + Telegram dispatch side-effects.
#[cfg(feature = "daily_universe_fetcher")]
pub mod instr_fetch_retry_adapter;

// Sub-PR #10b-γ of 2026-05-27 daily-universe expansion: pure-async
// retry-loop driver that composes the §4 infinite-retry contract. Takes
// 4 operator-supplied closures (fetch / build / sleep / emit) so unit
// tests can drive every branch without a tokio runtime. The integration
// PRs (#10b-δ HTTP wiring, #10b-ε Telegram wiring, #10b-ζ audit row)
// plug real side effects into this loop.
#[cfg(feature = "daily_universe_fetcher")]
pub mod instr_fetch_loop;

// Sub-PR #10b-δ of 2026-05-27 daily-universe expansion: production
// runner that drives the §4 infinite-retry loop with real
// `tokio::time::sleep` + structured tracing per ErrorCode severity.
// Wires Sub-PR #10b-γ (loop driver) + Sub-PR #10 (build) + Sub-PR #10b-β
// (retry decision) into one entry-point. Telegram dispatch + audit row
// land in #10b-ε / #10b-ζ.
#[cfg(feature = "daily_universe_fetcher")]
pub mod instr_fetch_runner;

// Sub-PR #11 of 2026-05-27 daily-universe expansion: boot-time-of-day
// guard per §10 — refuse boot if `now > 08:55 IST` without operator
// override flag (`--allow-late-boot`). Pure function.
#[cfg(feature = "daily_universe_fetcher")]
pub mod boot_time_of_day_guard;

// Sub-PR #11b of 2026-05-27 daily-universe expansion: boot day
// classifier — Weekend / DeclaredHoliday / Muhurat / RegularTradingDay.
// Pure function consuming operator-maintained `nse_holidays` +
// `muhurat_session_dates` lists. No URL fetch (deferred to #11c
// pending operator URL verification).
#[cfg(feature = "daily_universe_fetcher")]
pub mod boot_day_classifier;

// Sub-PR #9b of 2026-05-27 daily-universe expansion: L3 RECONCILE
// anomaly check — compare today's CSV row count vs yesterday's
// `instrument_fetch_audit` baseline. Pure function. Z+ defense layer 3.
// SHA-256 deferred to Sub-PR #9c (needs sha2 workspace dep approval).
#[cfg(feature = "daily_universe_fetcher")]
pub mod l3_anomaly_check;

// Sub-PR #11d of 2026-05-27 daily-universe expansion: extended boot
// day classifier covering NSE late-added holidays (additional_holidays)
// + Sunday Budget sessions (extra_trading_days) + Muhurat timing-
// pending status. Operator-verified per docs/operator/nse-trading-
// calendar-2026.md.
#[cfg(feature = "daily_universe_fetcher")]
pub mod boot_day_classifier_extended;

// Sub-PR #11c of 2026-05-27 daily-universe expansion: pure-function
// helpers to parse the NSE holiday-master JSON date format
// (`DD-MMM-YYYY`) and cross-check the operator-maintained nse_holidays
// config list against an externally-fetched set. Best-effort — the NSE
// endpoint is undocumented and may change; operator config remains
// authoritative.
#[cfg(feature = "daily_universe_fetcher")]
pub mod nse_holiday_cross_check;

// Sub-PR #11e of 2026-05-27 daily-universe expansion: boot complete-by
// 08:45 IST deadline per operator clarification 2026-05-27. Enforces
// the 15-minute buffer before 09:00 market open. Pure function.
#[cfg(feature = "daily_universe_fetcher")]
pub mod boot_complete_by_guard;

// PR #6b (2026-05-19): pub use binary_cache::MappedUniverse RETIRED.
// PR #6a (2026-05-19): pub use diagnostic::run_instrument_diagnostic RETIRED.
// PR #6b (2026-05-19): instrument_loader + universe_builder re-exports RETIRED —
// boot uses FnoUniverse::locked_4_idx_i() instead of the dynamic loader.
pub use subscription_planner::{build_subscription_plan, build_subscription_plan_from_archived};

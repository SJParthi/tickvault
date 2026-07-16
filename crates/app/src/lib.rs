//! Library target for the tickvault application.
//!
//! Exposes infrastructure, observability, and trading pipeline modules
//! so that `cargo llvm-cov` can instrument them through the lib target.
//! The binary entry point (`main.rs`) re-exports these via `use crate::`.

#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![deny(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]
// Phase 0.2: no dropped Result/JoinHandle/must-use values (silent error swallowing).
#![cfg_attr(not(test), deny(unused_must_use))]
#![cfg_attr(not(test), warn(clippy::let_underscore_must_use))]
#![cfg_attr(test, allow(clippy::assertions_on_constants))]
#![cfg_attr(test, allow(clippy::field_reassign_with_default))]
// APPROVED: clippy 1.95 tightened these doc-formatting lints; the codebase
// predates them. Allow rather than churn doc comments for a cosmetic
// markdown-rendering nicety with zero runtime/behavior impact.
#![allow(clippy::doc_lazy_continuation)]
#![allow(clippy::doc_overindented_list_items)]

pub mod bar_cache_loader;
// W2 PR#5 (2026-07-10, audit follow-up row 15): holiday-calendar
// coverage-horizon staleness watchdog - pages the operator BEFORE the
// calendar runs off its year-end cliff into un-listed holidays.
pub mod calendar_staleness;
// PR-C3 (2026-07-14): `cross_verify_1m_boot` module DELETED — the 15:31 IST
// Dhan live-vs-historical 1m cross-verify retired with the Dhan live WS
// (operator 2026-07-13; cross-verify-1m-error-codes.md retirement banner).
// Its parser lives on in `dhan_intraday_parse` (relocated in C1).
// BruteX↔TickVault daily cross-verify (BRUTEX-XVERIFY, 2026-07-12): PURE
// comparison core — CSV parse, symbol mapping, day compare. No I/O; the
// boot/runner shell lives in brutex_crossverify_boot (Unit 5).
pub mod brutex_crossverify_compare;
// BruteX↔TickVault daily cross-verify (BRUTEX-XVERIFY, 2026-07-12): the
// 15:50 IST I/O shell — S3 CSV fetch, QuestDB reads, compare orchestration,
// persistence, Telegram summary + supervised spawn (Unit 7).
pub mod brutex_crossverify_boot;
// Phase 0 Item 20 (wired 2026-06-13): supervised 15:25 IST orphan-position
// watchdog — daily open-position safety gate (alert-only in sandbox/dry-run).
pub mod orphan_position_watchdog_boot;
// Operator task DHAN-REST-400 (2026-06-10): scheduled REST-health canary —
// GET /v2/profile at 09:05 / 12:00 / 15:25 IST, pages HIGH with the captured
// (bounded, secret-redacted) body + final URL on any non-2xx.
// Per-minute spot 1m REST pipeline (operator grant 2026-07-12, PR-2 — the
// SPOT half): fetch each just-closed session minute's official 1m OHLCV
// for the 3 IDX_I spot indices via POST /v2/charts/intraday and persist to
// the `spot_1m_rest` table (SPOT1M-01/02).
pub mod spot_1m_rest_boot;
// Per-minute option-chain REST pipeline (operator grant 2026-07-12, PR-3 —
// the OPTION-CHAIN half; config-gated DEFAULT-OFF pending the live
// entitlement probe): day-start expirylist warmup, then each session
// minute — sequenced right after the spot leg — pull the current-expiry
// chain for the 3 underlyings via POST /v2/optionchain and persist to the
// `option_chain_1m` table (CHAIN-01..04).
pub mod option_chain_1m_boot;
// Groww per-minute spot 1m REST leg (operator grant 2026-07-13 — PR-2 of
// the Groww per-minute REST plan): the just-closed minute's official Groww
// 1m OHLCV for the 3 spot indices → `spot_1m_rest` feed='groww' + the
// `rest_fetch_audit` per-fetch forensics rows.
pub mod groww_spot_1m_boot;
// Groww per-minute option-chain REST leg (operator grant 2026-07-13 — PR-3
// of the Groww per-minute REST plan): the current-expiry chain for the 3
// underlyings, sequenced after the Groww spot leg → `option_chain_1m`
// feed='groww' + `rest_fetch_audit` leg='chain_1m' forensics rows.
pub mod groww_option_chain_1m_boot;
// Groww per-minute PER-CONTRACT 1m candle REST leg (operator grant
// 2026-07-13 — PR-4 of the Groww per-minute REST plan, the fill-model
// leg): the just-closed minute's 1m candle for a bounded ATM-window
// contract selection, sequenced after the Groww chain leg →
// `option_contract_1m_rest` feed='groww' + `rest_fetch_audit`
// leg='contract_1m' forensics rows.
pub mod groww_contract_1m_boot;
// Dual-feed scoreboard PR-A (operator 2026-07-10): boot-time process-death
// reconciler + the 15:45 IST daily Dhan-vs-Groww aggregation + the Telegram
// scorecard summary (SCOREBOARD-01 family).
pub mod feed_scoreboard_boot;
// Daily timeframe-consistency verifier (operator 2026-07-13): at 15:40 IST,
// recompute every higher-TF candle (2m..4h) from the stored 1m rows and
// compare against the persisted TF tables — Dhan verifies TODAY, Groww
// verifies the PREVIOUS trading day (TF-VERIFY-01/02).
pub mod tf_consistency_boot;
pub mod tick_conservation_boot;
// PR #8a (2026-05-19) — Slice 1: 09:15:00 IST `DayOhlcTracker::arm_sid()`
// boot wiring per `index-day-ohlc-tracker-error-codes.md`. Closes the
// operator-locked pre-open equilibrium open-price gap.
pub mod day_ohlc_orchestrator;
// PR #6a (2026-05-19): bhavcopy_pipeline DELETED — 16:30 IST NSE bhavcopy
// cross-check retired under 4-IDX_I LOCKED_UNIVERSE (operator lock 2026-05-15).
// Bhavcopy is NSE_FNO-only; no F&O subscriptions remain to cross-check.
pub mod boot_helpers;
/// Once-per-trading-day delivery markers for daily scheduled tasks
/// (Telegram cleanliness overhaul, coordinator-relayed directive
/// 2026-07-15). Fail-open advisory files under `data/state/daily/` —
/// the 15:40 timeframe check's catch-up arm consults them so a
/// post-15:40 restart never re-fires an already-delivered daily card.
pub mod daily_task_marker;
/// Dhan runtime activation watcher (PR-2) — dormant supervisor that keeps the
/// Dhan lane's running flag honest across runtime toggles and enforces the
/// Dhan-disable safety gate at the supervisor layer (operator 2026-06-21/24).
/// Shared self-tuning Dhan Data-API rate limiter (operator pacing
/// directive 2026-07-14): ONE process-wide token-bucket gate every
/// per-minute Dhan Data-API REST fire passes through — spot-1m fires +
/// ladder re-polls + the 15:33:30 sweep + the #1524 diagnostic probes +
/// the option-chain fires — 3 rps default, self-tuning down to the 2 rps
/// floor on observed 429 bursts. Dhan-ONLY; Groww untouched.
/// (`dhan_activation` — the lane cold-start watcher that preceded this
/// decl — was deleted in PR-C2 with the Dhan live-WS lane.)
pub mod dhan_data_api_limiter;
/// Shared Dhan `/v2/charts/intraday` 1m request/response primitives —
/// relocated from `cross_verify_1m_boot.rs` in Phase C1 of the 2026-07-13
/// Dhan live-WS retirement (the spot-1m legs must outlive the cross-verify
/// module the Phase C deletion PRs remove). Pure move, zero behavior change.
pub mod dhan_intraday_parse;
/// Dhan REST-only auth bootstrap (Phase A of the Dhan-live-feed removal,
/// operator directive 2026-07-13): with `feeds.dhan_enabled = false` this
/// brings up the RETAINED Dhan REST surface — dual-instance lock →
/// TokenManager → renewal + mid-session watchdog → REST canary +
/// spot_1m_rest + option_chain_1m — WITHOUT any WebSocket lane.
pub mod dhan_rest_stack;
/// `[groww_universe]` process-global daily Groww watch-set + shared-master
/// rider (2026-07-15 live-feed retirement re-home of the activation daily
/// build loop + the sole persist_groww_instruments caller).
pub mod groww_universe;
pub mod groww_watch_paths;
/// RAM residency stores boot (operator directive 2026-07-16, PR-2):
/// installs the month-deep spot bar rings + current-day chain minute ring,
/// runs the bounded chain-day rehydrate, and publishes the depth gauges.
/// RAMSTORE-01 runbook: `.claude/rules/project/ram-store-error-codes.md`.
pub mod market_ram_store_boot;
/// REST-era multi-TF candle derivation (operator directive 2026-07-16):
/// folds persist-confirmed `spot_1m_rest` 1m bars into all 21 `candles_*`
/// timeframes via the shared seal-writer channel + boot catch-up over the
/// stored month. FOLD-01 runbook:
/// `.claude/rules/project/rest-candle-fold-error-codes.md`.
pub mod rest_candle_fold;
/// Shared per-seal routing for BOTH feeds (Dhan + Groww) — the single
/// `route_seal` body the two `on_seal` call sites invoke (C2, behavior-preserving).
pub mod seal_routing;
/// Pure shutdown classifier (Telegram cleanliness overhaul, 2026-07-15):
/// signal kind × runtime source × IST clock × trading calendar →
/// `ShutdownClass`. Fails toward ExternalStop (loud) on any doubt.
pub mod shutdown_class;
// PR #4 (2026-05-19): depth-20 / depth-200 modules DELETED (operator-locked
// per websocket-connection-scope-lock.md — 4-IDX_I uses 1 main-feed conn
// + 1 order-update conn only).
// PR #3 (2026-05-19): `greeks_pipeline` module DELETED. Greeks
// pipeline retired alongside the indices-only universe. Option Chain
// REST overlay (PR #8) ships Dhan-computed greeks separately.
pub mod infra;
// 2026-05-09 PR 5c.5-final (Bug 3 — movers retirement): the
// `movers_pipeline` orchestrator is DELETED. Operator directive:
// "only ticks and our 9 needed candle timeframes are available".
// The 25 `movers_*` matviews + `movers_1s` base table are dropped at
// boot by `materialized_views::drop_bug3_retired_views`.
// PR-C3 (2026-07-14): `prev_day_ohlcv_boot` module DELETED — the boot-time
// previous-day OHLCV REST fetch retired with the Dhan live-WS lane (operator
// 2026-07-13; prev-day-ohlcv-error-codes.md retirement banner). The
// `prev_day_ohlcv` QuestDB table is retained (forensic).
// F2 (Wave-5 #504e follow-up) — boot-time loader for `PrevDayCache`
// so the cascade seal-time pct-stamping path (PR #520 / F1) sees
// non-zero `prev_day_close` values from QuestDB's `previous_close`
// table on cold boot.
pub mod metrics_catalog;
// PR-C3 (2026-07-14, operator retirement directive 2026-07-13 — scope-lock
// amendment §B): the Dhan lifecycle-reconcile + fetch-audit chain is
// DELETED with the instrument-download chain and the
// `daily_universe_fetcher` feature itself: `instr_fetch_audit_writer`,
// `lifecycle_cache_loader`, `lifecycle_reconcile_plan`, `today_instrument`,
// `lifecycle_apply`, `apply_reconcile_plan`,
// `lifecycle_reconcile_orchestrator`, `daily_universe_boot`. The SEBI
// tables they wrote (`instrument_lifecycle` / `instrument_lifecycle_audit`
// / `instrument_fetch_audit` / `index_constituency`) are RETAINED
// (never-delete) and keep their now-ungated storage persistence modules;
// the Groww shared-master writer remains their live producer.
// `index_constituency_boot` is KEPT (ungated) TRIMMED to its process-global
// ts-pin migration half — load-bearing for the Groww append MigrationGate.
/// W2#7 (2026-07-10): supervised SSM re-read loop so the operator can
/// rotate the API bearer token (/tickvault/<env>/api/bearer-token) without
/// an app restart — closes audit row 13.
pub mod api_token_rotation;
/// 🔷 DHAN exit-order execution dispatcher (Cluster B, 2026-07-14) — the
/// S6-G1 call-site hub for every engine exit method + LOCK #2's runtime
/// `!cfg.enabled` gate. Future entry-side (Cluster A) work constructs
/// `ExitCommand`s; only this module executes them (never the engine
/// methods directly).
pub mod exit_execution;
pub mod index_constituency_boot;
pub mod observability;
/// Cluster-C order-side observability (2026-07-14): OmsAlertSink /
/// RiskAlertSink bridges → Telegram + the rebuilt SEBI order_audit /
/// pnl_audit tables via one bounded mpsc(1024) consumer task; daily
/// OnEod heartbeat + counters-vs-rows reconcile (OMS-GAP-02 on mismatch).
pub mod order_observability;
pub mod subsystem_memory;
pub mod trading_pipeline;
/// C3 (2026-07-03): bounded, chunked, backpressured STAGE-C.2b WAL frame
/// re-injection — replaces the raw try_send loop that dropped 1,127,801
/// frames + kept the WAL unconfirmed (self-feeding re-replay storm).
pub mod wal_reinject;
/// Shared `ws_event_audit` channel + consumer helper — relocated from the
/// main.rs binary in Phase C1 (2026-07-13) so the lib-side `dhan_rest_stack`
/// (which owns the functional-dormant order-update WS per operator ruling
/// Q4-i) can create its own audit consumer. Pure move, zero behavior change.
pub mod ws_audit_consumer;

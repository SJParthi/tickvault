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
// Operator directive 2026-06-02: post-market (15:31 IST) 1-minute
// cross-verification of live candles_1m vs Dhan intraday — CSV + audit + count.
pub mod cross_verify_1m_boot;
// Phase 0 Item 20 (wired 2026-06-13): supervised 15:25 IST orphan-position
// watchdog — daily open-position safety gate (alert-only in sandbox/dry-run).
pub mod orphan_position_watchdog_boot;
// Operator task DHAN-REST-400 (2026-06-10): scheduled REST-health canary —
// GET /v2/profile at 09:05 / 12:00 / 15:25 IST, pages HIGH with the captured
// (bounded, secret-redacted) body + final URL on any non-2xx.
pub mod rest_canary_boot;
pub mod tick_conservation_boot;
// PR #8a (2026-05-19) — Slice 1: 09:15:00 IST `DayOhlcTracker::arm_sid()`
// boot wiring per `index-day-ohlc-tracker-error-codes.md`. Closes the
// operator-locked pre-open equilibrium open-price gap.
pub mod day_ohlc_orchestrator;
// PR #6a (2026-05-19): bhavcopy_pipeline DELETED — 16:30 IST NSE bhavcopy
// cross-check retired under 4-IDX_I LOCKED_UNIVERSE (operator lock 2026-05-15).
// Bhavcopy is NSE_FNO-only; no F&O subscriptions remain to cross-check.
pub mod boot_helpers;
/// Dhan runtime activation watcher (PR-2) — dormant supervisor that keeps the
/// Dhan lane's running flag honest across runtime toggles and enforces the
/// Dhan-disable safety gate at the supervisor layer (operator 2026-06-21/24).
pub mod dhan_activation;
/// Groww runtime activation watcher — feed toggle cold-starts/teardowns the
/// whole Groww lane live (operator 2026-06-24). Default-OFF; dormant until enabled.
pub mod groww_activation;
/// Groww second-feed bridge — consumer side (operator lock §32). Default-OFF.
pub mod groww_bridge;
/// Groww Python-sidecar auto-launcher + supervisor (operator lock §32 +
/// "no manual commands" 2026-06-19). Default-OFF.
pub mod groww_sidecar_supervisor;
/// Shared per-seal routing for BOTH feeds (Dhan + Groww) — the single
/// `route_seal` body the two `on_seal` call sites invoke (C2, behavior-preserving).
pub mod seal_routing;
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
// PR #454 (2026-05-03): boot-time prev_oi cache loader. Wires
// bhavcopy → cache extraction primitives into the boot path so the
// in-memory cache (consumed by the cascade seal-time pct-stamping
// path + tick enricher) is Dhan-precise from the first tick.
pub mod prev_day_ohlcv_boot;
// F2 (Wave-5 #504e follow-up) — boot-time loader for `PrevDayCache`
// so the cascade seal-time pct-stamping path (PR #520 / F1) sees
// non-zero `prev_day_close` values from QuestDB's `previous_close`
// table on cold boot.
pub mod metrics_catalog;
// Sub-PR #10b-ζ-2 (2026-05-27): maps INSTR-FETCH per-attempt + terminal
// fetch outcomes → `instrument_fetch_audit` rows and writes them via the
// storage helper. App crate owns this seam because `core` (the runner)
// cannot depend on `storage` (the writer). Feature-gated under
// `daily_universe_fetcher` per rule §21; boot callsite lands in the
// final Sub-PR #10 orchestrator.
#[cfg(feature = "daily_universe_fetcher")]
pub mod instr_fetch_audit_writer;
// §31 item 2 (NTM Map-B, 2026-06-06): boot population of the full
// index→constituents mapping (all ~46 tracked indices). Map-only, decoupled,
// degrade-safe — does NOT touch the live subscription. Feature-gated per §21.
#[cfg(feature = "daily_universe_fetcher")]
pub mod index_constituency_boot;
// Boot-time read-back of yesterday's instrument_lifecycle rows — the
// read-side primitive the daily reconciler I/O loop diffs against today's
// CSV. Feature-gated per rule §21.
#[cfg(feature = "daily_universe_fetcher")]
pub mod lifecycle_cache_loader;
// Daily reconcile-plan computation — pure diff of today's universe vs
// yesterday's read-back into a list of state transitions to persist.
// Feature-gated per rule §21.
#[cfg(feature = "daily_universe_fetcher")]
pub mod lifecycle_reconcile_plan;
// Extract today's full per-instrument detail from the built DailyUniverse
// (#845 parser detail) for the lifecycle-row UPSERT. Feature-gated §21.
pub mod observability;
#[cfg(feature = "daily_universe_fetcher")]
pub mod today_instrument;
// Pure value-resolution helpers for the reconciler apply step (expiry
// date→nanos, first_seen preservation, UPSERT/UPDATE routing).
// Feature-gated §21; the async apply loop is a follow-up.
#[cfg(feature = "daily_universe_fetcher")]
pub mod lifecycle_apply;
// The async apply step: walks the reconcile plan applying §24 audit-first
// ordering (append audit row → UPSERT present / UPDATE disappearance).
// Feature-gated §21; boot wiring is a follow-up.
#[cfg(feature = "daily_universe_fetcher")]
pub mod apply_reconcile_plan;
// App-side reconcile orchestrator: chains extract → load-prev → plan →
// apply into one async entry point. Fail-closed on read-back error.
// Feature-gated §21; the HTTP-fetch/retry/main.rs spawn is a follow-up.
#[cfg(feature = "daily_universe_fetcher")]
pub mod lifecycle_reconcile_orchestrator;
// Outer daily-universe boot orchestrator: §4 fetch-runner → reconcile →
// terminal fetch-audit, capturing CSV SHA-256 provenance. Feature-gated
// §21; the main.rs Step-6c task spawn is the final wiring PR.
#[cfg(feature = "daily_universe_fetcher")]
pub mod daily_universe_boot;
pub mod subsystem_memory;
pub mod trading_pipeline;
/// C3 (2026-07-03): bounded, chunked, backpressured STAGE-C.2b WAL frame
/// re-injection — replaces the raw try_send loop that dropped 1,127,801
/// frames + kept the WAL unconfirmed (self-feeding re-replay storm).
pub mod wal_reinject;

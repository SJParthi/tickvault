//! Historical candle data fetcher and cross-verification.
//!
//! Fetches 1-minute OHLCV candles from Dhan's intraday charts API
//! for all subscribed instruments. Used for:
//! 1. Populating the `candles_1m` QuestDB table
//! 2. Cross-verifying live tick data against historical records
//!
//! # API Details
//! - Endpoint: POST `{rest_api_base_url}/charts/intraday` and `/charts/historical`
//! - Response: parallel arrays of open, high, low, close, volume, timestamp
//! - Timestamps: standard UNIX epoch seconds (UTC from REST API; WebSocket uses IST)
//! - Limit: 90 days per request for intraday, unlimited for daily
//!
//! # Trading Day — Dual Window
//! - **Live data (WebSocket):** 09:00:00 IST to 15:29:59 IST (stored from 09:00)
//! - **Historical API (Dhan REST):** 09:15:00 IST to 15:29:00 IST (375 candles)
//! - **Cross-verification:** compares historical vs live for 09:15–15:29 only,
//!   because Dhan historical API has no pre-market data (09:00–09:14).

// `backfill` module DELETED 2026-04-17 — Parthiban directive:
// "live market feed websockets nowhere the backfill should happen, make it
// as hard enforcement ... live market feed should contain only live market
// feed data alone ... historical candle data fetch is a separate functionality".
//
// Rationale: the `ticks` QuestDB table is reserved for WebSocket-sourced
// live data. Historical REST-API data lives in `historical_candles` and
// `candles_*` materialized views, never in `ticks`. Synthesising ticks
// from historical candles and writing them to the `ticks` table would
// violate data-source provenance and SEBI audit trail expectations.
//
// Mechanical guards live in:
//   - `.claude/hooks/banned-pattern-scanner.sh` (live-feed-purity category)
//   - `crates/storage/tests/live_feed_purity_guard.rs`
//   - `.claude/rules/project/live-feed-purity.md`

pub mod candle_fetcher;
pub mod cross_verify;
// Phase 0 Item 10 — pure planner that computes which 1m bar starts a
// disconnect gap-fill scheduler should refill. Does NOT write to ticks
// or synthesize ticks (compliant with live-feed-purity rule). The
// scheduler task that consumes this plan lands in a follow-up PR.
pub mod gap_fill_planner;
// Phase 0 Item 8+9 (PR-C, 2026-05-17) — consumer side of the
// disconnect-event broadcast channel. Subscribes to events from
// `crate::websocket::connection`, calls `gap_fill_planner` to compute
// missing 1m bars, and emits Prometheus counters. PR-D adds REST fetch
// + `historical_candles` UPSERT.
pub mod gap_fill_scheduler;

// Phase 0 Item 15 (2026-05-18) — 09:16:05 IST post-open cross-check
// primitives. Pure-function comparator + clock helpers + outcome
// types. Scheduler wiring (REST fan-out + audit writes + strategy
// gate) lands in sub-deliverable 4. See plan §785.
pub mod post_open_cross_check;

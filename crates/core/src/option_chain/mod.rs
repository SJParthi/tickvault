//! Dhan Option Chain REST API client + types.
//!
//! # Endpoints
//! - `POST /v2/optionchain` — full option chain with Greeks
//! - `POST /v2/optionchain/expirylist` — active expiry dates
//!
//! # Rate Limit
//! 1 unique request every 3 seconds (stricter than standard Data APIs).
//!
//! # Headers
//! Requires both `access-token` AND `client-id` headers.

pub mod client;
// 2026-05-25 — Per-day cache of current (nearest) expiry per
// underlying. Halves Dhan REST traffic by skipping the
// `/optionchain/expirylist` call on every minute slot (per
// operator-confirmed `data[0]`-is-always-current insight). See
// `current_expiry_cache.rs` module docs.
pub mod current_expiry_cache;
// 2026-05-25 — Pre-market 09:00:30 IST one-shot task that populates
// `CurrentExpiryCache` with 3 expiry-list calls (one per
// NIFTY/BANKNIFTY/SENSEX). Triggered once per trading day.
pub mod expiry_warmup;
// PR #8d (2026-05-20) — L2 VERIFY structural validation of a fetched
// option-chain response before it is cached (heart-piece §3 row 2).
pub mod l2_verify;
// PR #450 commit 3 (2026-05-03): pure helpers to extract `previous_oi`
// from Option Chain REST responses for the unified /api/movers
// Dhan-parity OI Change calculations.
pub mod prev_oi;
// Option-chain minute-snapshot pipeline (PR #4a of 5, 2026-05-16) —
// RAM cache that decouples the strategy hot path from the network
// fetch cold path. See plan
// `.claude/plans/friday-may-15-mega/topic-OPTION-CHAIN-MINUTE-SNAPSHOT.md`.
pub mod snapshot_cache;
// Option-chain minute-snapshot pipeline (PR #4b of 5, 2026-05-16) —
// per-slot tokio scheduler that fires fetch + cache-insert + 4
// Prometheus counters + edge-triggered Telegram on failure.
pub mod snapshot_scheduler;
pub mod types;

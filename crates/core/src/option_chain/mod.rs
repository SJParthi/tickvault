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
// PR #450 commit 3 (2026-05-03): pure helpers to extract `previous_oi`
// from Option Chain REST responses for the unified /api/movers
// Dhan-parity OI Change calculations.
pub mod prev_oi;
pub mod types;

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
//! - Timestamps: standard UNIX epoch seconds (UTC)
//! - Limit: 90 days per request for intraday, unlimited for daily
//!
//! # Trading Day — Dual Window
//! - **Live data (WebSocket):** 09:00:00 IST to 15:29:59 IST (stored from 09:00)
//! - **Historical API (Dhan REST):** 09:15:00 IST to 15:29:00 IST (375 candles)
//! - **Cross-verification:** compares historical vs live for 09:15–15:29 only,
//!   because Dhan historical API has no pre-market data (09:00–09:14).

pub mod candle_fetcher;
pub mod cross_verify;

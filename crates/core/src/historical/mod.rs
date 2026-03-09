//! Historical candle data fetcher and cross-verification.
//!
//! Fetches 1-minute OHLCV candles from Dhan's intraday charts API
//! for all subscribed instruments. Used for:
//! 1. Populating the `candles_1m` QuestDB table
//! 2. Cross-verifying live tick data against historical records
//!
//! # API Details
//! - Endpoint: POST `{rest_api_base_url}/charts/intraday`
//! - Response: parallel arrays of open, high, low, close, volume, timestamp
//! - Timestamps: standard UNIX epoch seconds (UTC)
//! - Limit: 90 days per request, 1-minute candle resolution
//!
//! # Trading Day
//! NSE F&O: 09:15:00 IST to 15:29:00 IST (375 one-minute candles per day)
//! Data collection starts at 09:00:00 IST (pre-market) per config.

pub mod candle_fetcher;
pub mod cross_verify;

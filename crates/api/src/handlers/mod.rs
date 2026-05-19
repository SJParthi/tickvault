//! API request handlers.
//!
//! ## Movers retirement (PR #2, 2026-05-18)
//!
//! The `movers_v2` handler was deleted alongside the movers pipeline
//! in PR #2 of the AWS-lifecycle 14-PR sequence. Under the 4-IDX_I-only
//! universe (NIFTY/BANKNIFTY/SENSEX/INDIA VIX) a top-N gainers/losers/
//! most-active query is meaningless. The `/api/movers/v2` route is
//! removed.

pub mod debug;
pub mod health;
// PR #6a (2026-05-19): index_constituency handler retired (4-IDX_I LOCKED_UNIVERSE).
pub mod instruments;
pub mod market_data;
pub mod option_chain;
pub mod quote;
pub mod static_file;
pub mod stats;

//! API request handlers.

pub mod debug;
pub mod health;
pub mod index_constituency;
pub mod instruments;
pub mod market_data;
// PR #450 (2026-05-03): unified Dhan-parity /api/movers handler.
// Replaces top_movers + movers_v2.
pub mod movers;
pub mod movers_questdb;
// movers_v2 + top_movers handlers DELETED in PR #450 commit 6.
pub mod option_chain;
pub mod quote;
pub mod static_file;
pub mod stats;
// top_movers handler DELETED in PR #450 commit 6b — replaced by
// unified /api/movers handler in commit 4.

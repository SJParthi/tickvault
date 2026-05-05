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
// Phase 4a (2026-05-05): NEW dormant /api/movers?v=2 handler reading
// OHLCV from the in-RAM 29-TF CascadeFanout. Distinct from the old
// movers_v2 module deleted in PR #450 commit 6 (that used
// MoversTrackerV2; this one uses CascadeFanout). Route gated behind
// `config.api.movers_v2_enabled` (default false) per active-plan §6
// row 3.
pub mod movers_v2;
pub mod option_chain;
pub mod quote;
pub mod static_file;
pub mod stats;
// top_movers handler DELETED in PR #450 commit 6b — replaced by
// unified /api/movers handler in commit 4.

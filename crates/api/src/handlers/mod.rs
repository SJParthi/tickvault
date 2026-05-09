//! API request handlers.

pub mod debug;
pub mod health;
pub mod index_constituency;
pub mod instruments;
pub mod market_data;
// 2026-05-09 cleanup (PR 5b2): both `movers` (V1 handler) and
// `movers_questdb` (V1 SQL helper) modules deleted. The 3 static
// dashboards (`markets-stocks.html`, `markets-options.html`,
// `market-dashboard.html`) now consume `/api/movers/v2` (in-RAM
// CascadeFanout reads — no `movers_*` matview dependency).
// `market_data::get_stock_movers` + `get_option_movers` were the
// last consumers of `movers_questdb` and are also deleted in this
// commit. Plan reference:
// `.claude/plans/active-plan-movers-cleanup-5b-5c-5d.md`.
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

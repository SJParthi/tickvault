//! API request handlers.

pub mod debug;
pub mod health;
pub mod index_constituency;
pub mod instruments;
pub mod market_data;
// 2026-05-09 cleanup (PR 5a): the V1 `movers` handler module
// (`/api/movers` + `/api/movers/expiries`) was removed because no
// frontend consumes it (verified by grepping `crates/api/static/`).
// The unified `/api/movers/v2` endpoint in `movers_v2.rs` is the
// single source of truth.
//
// `movers_questdb` is RETAINED — `market_data::get_stock_movers`
// (routed at `/api/market/stock-movers`, consumed by static
// `markets-stocks.html` + `market-dashboard.html`) imports
// `query_movers` + helper enums from this module. Its deletion is
// scheduled for PR 5b/c after the static frontends migrate to
// `/api/movers/v2`.
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

//! API request handlers.
//!
//! ## AWS-lifecycle handler retirements
//!
//! - PR #2 (2026-05-18): `movers_v2` handler deleted with the movers pipeline.
//! - PR #6a (2026-05-19): `index_constituency` handler retired.
//! - PR #6b (2026-05-19): `instruments` handler retired.
//! - PR #7d (2026-05-19): `static_file`, `option_chain`, `market_data`
//!   handlers retired with the entire `/portal/*` HTML frontend +
//!   `/api/option-chain`, `/api/pcr`, `/api/market/indices` routes. The
//!   replacement surface is Grafana dashboards (live tick feed, candles,
//!   PCR), Telegram alerts (op events), MCP tools (`questdb_sql`,
//!   `run_doctor`; the `prometheus_query` tool was retired in #O5
//!   2026-05-30), and the QuestDB Console at
//!   `localhost:9000` for ad-hoc queries.

pub mod board;
pub mod board_page;
pub mod dashboard_page;
pub mod debug;
pub mod feeds;
pub mod feeds_page;
pub mod health;
pub mod quote;
pub mod stats;

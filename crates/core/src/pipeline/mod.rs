//! Pipeline support modules for the REST-only runtime.
//!
//! The live tick-capture pipeline (`WebSocket Pool → dispatch_frame →
//! ParsedTick → junk filter → TickPersistenceWriter → QuestDB`) is GONE —
//! stage-2 of the dead-WS sweep (2026-07-17) deleted it after both live
//! feeds retired (Dhan 2026-07-13, Groww 2026-07-15). What remains here is
//! the surviving cold/RAM surface: the chain snapshot/day stores (the RAM
//! decision surface), `feed_lag_monitor` (live consumers: the scoreboard
//! day-lag drain + the midnight histogram reset), and `feed_presence`.
//!
//! ## Movers retirement
//!
//! The `top_movers` / `option_movers` / `mover_classifier` / `movers_window`
//! modules were deleted in PR #2 of the AWS-lifecycle 14-PR sequence
//! (operator-locked 2026-05-18). Under the 4-IDX_I-only subscription
//! scope (NIFTY/BANKNIFTY/SENSEX/INDIA VIX) ranked gainers/losers/most-
//! active snapshots are meaningless — there are only 4 instruments.

// Dead live-WS sweep stage 1 (2026-07-17, operator directive via
// coordinator): `boot_ordering_gate` + `first_seen_set` DELETED — both
// had ZERO production callers (only comments referenced them since the
// PR-C2 Dhan live-WS lane deletion 2026-07-13).
pub mod chain_day_store;
pub mod chain_snapshot;
// Candle-engine re-architecture #T1b: `candle_aggregator` (Engine A)
// DELETED — Engine B (the multi-TF aggregator) is the only candle engine.
// PR #4 (2026-05-19): `depth_sequence_tracker` module DELETED.
// tick_gap_detector DELETED in PR-C3 (2026-07-14, operator Q4-ii 2026-07-13
// — websocket-connection-scope-lock.md "2026-07-13 Amendment" §B item 4):
// fed only by the retired Dhan WS pipeline; WS-GAP-06 retired with it.
// Stage-2 dead-WS sweep (2026-07-17): the dead Dhan tick chain DELETED —
// `tick_processor` (+ its `run_tick_processor` / `init_prev_close_cache_dir`
// re-exports), `feed_consumer`, `tick_enricher`, `prev_day_close_stamper`,
// `prev_oi_cache`, `volume_delta_tracker`, `volume_monotonicity_guard`,
// `prev_close_writer`. Zero production callers re-verified (the spawn sites
// died in PR-C2/C3; the Groww bridge died 2026-07-15). `feed_lag_monitor`
// is KEPT — main.rs's midnight `reset_day_lag_histogram` + the scoreboard's
// `day_lag_summary` drain are live consumers.
pub mod feed_lag_monitor;
pub mod feed_presence;

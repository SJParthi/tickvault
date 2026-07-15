//! Groww feed — REST-leg support modules (watch-set build + shared-master persist).
//!
//! The Groww LIVE WebSocket/NATS feed was RETIRED 2026-07-15 (operator
//! directive: "remove the whole Groww live feed; keep only spot 1m and
//! option chain for both brokers"). What remains here serves the per-minute
//! REST legs and the [groww_universe] daily watch-set rider:
//! - [`instruments`] — daily Groww master download + watch-set build
//! - [`watch_reader`] — watch-file parse (VIX resolution for the spot leg)
//! - [`shared_master_writer`] — feed='groww' master/constituency persist
//! Reference: `docs/groww-ref/`.

pub mod instruments;
pub mod shared_master_writer;
pub mod watch_reader;

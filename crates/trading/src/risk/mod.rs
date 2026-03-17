//! Risk engine — enforces trading limits and auto-halts on breach.
//!
//! # Components
//! - `RiskEngine` — stateful risk manager tracking daily P&L and positions
//! - `RiskCheck` — result of a pre-trade risk validation
//! - `RiskBreach` — enumeration of breach types that trigger auto-halt
//!
//! # Design
//! - O(1) per order check: HashMap lookup for position, simple arithmetic for P&L
//! - All limits from config (`RiskConfig`) — no hardcoded values
//! - Auto-halt: once breached, all subsequent order requests are rejected
//! - Reset: manual reset only (prevents accidental re-entry after breach)

pub mod engine;
pub mod tick_gap_tracker;
pub mod types;

pub use engine::RiskEngine;
pub use tick_gap_tracker::{TickGapResult, TickGapTracker};
pub use types::{PositionInfo, RiskBreach, RiskCheck};

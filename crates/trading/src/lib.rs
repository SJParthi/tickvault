// Compile-time lint enforcement — defense-in-depth with CLI clippy flags
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]
#![deny(clippy::dbg_macro)]
// Phase 0.2: no dropped Result/JoinHandle/must-use values (silent error swallowing).
#![cfg_attr(not(test), deny(unused_must_use))]
#![cfg_attr(not(test), warn(clippy::let_underscore_must_use))]
#![cfg_attr(test, allow(clippy::assertions_on_constants))]
#![cfg_attr(test, allow(clippy::field_reassign_with_default))]
#![allow(missing_docs)]
// TODO: enforce after adding docs to all public items
// APPROVED: clippy 1.95 tightened these doc-formatting lints; the codebase
// predates them. Allow rather than churn ~100 doc comments for a cosmetic
// markdown-rendering nicety with zero runtime/behavior impact.
#![allow(clippy::doc_lazy_continuation)]
#![allow(clippy::doc_overindented_list_items)]

//! Trading engine: O(1) indicators, FSM strategies, OMS, Greeks, and risk controls.
//!
//! # Modules
//! - `candles` — In-memory `CandleEngine<TF>` (29-timeframes plan, Phase 3)
//! - `indicator` — O(1) per-tick indicator engine (EMA, RSI, MACD, ATR, Bollinger, etc.)
//! - `strategy` — FSM-based strategy evaluator with declarative conditions
//! - `risk` — Risk engine: max daily loss, position limits, P&L tracking, auto-halt
//! - `oms` — Order Management System: lifecycle FSM, rate limiting, circuit breaker, reconciliation
//!
//! # Pipeline Position
//! ParsedTick → **IndicatorEngine → StrategyEvaluator** → OMS → Risk → Execute → Persist

pub mod candles;
// PR #3 (2026-05-19): `greeks` module DELETED. Under the 4-IDX_I-only
// LOCKED_UNIVERSE there are no live option contracts on the WebSocket
// to compute Greeks from. Option Chain REST overlay (PR #8) ships
// Dhan-computed greeks separately.
pub mod in_mem;
pub mod indicator;
pub mod oms;
pub mod orphan_position_watchdog;
pub mod risk;
pub mod strategy;

#[cfg(test)]
mod tests {
    //! Skeleton tests — establishes ratchet baseline for the trading crate.
    //! These verify crate-level invariants. Module-specific tests will be
    //! added alongside OMS, risk manager, and order executor implementations.

    /// Verifies that compile-time lint attributes don't conflict.
    /// If this test compiles, the deny/allow attributes in lib.rs are consistent.
    #[test]
    fn lint_attributes_compile() {
        // Intentionally empty — compilation IS the test.
        // The #![deny(...)] attributes at crate root are validated by rustc
        // when this test binary is built.
    }

    /// Verifies the crate links into the workspace without unresolved deps.
    /// If this test runs, it means cargo resolved all workspace deps for this crate.
    #[test]
    fn crate_links_in_workspace() {
        let crate_name = env!("CARGO_PKG_NAME");
        assert_eq!(crate_name, "tickvault-trading");
    }
}

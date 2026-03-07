// Compile-time lint enforcement — defense-in-depth with CLI clippy flags
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]
#![deny(clippy::dbg_macro)]
#![allow(missing_docs)] // TODO: enforce after adding docs to all public items

//! Trading engine: O(1) indicators, FSM strategies, OMS, and risk controls.
//!
//! # Modules
//! - `indicator` — O(1) per-tick indicator engine (EMA, RSI, MACD, ATR, Bollinger, etc.)
//! - `strategy` — FSM-based strategy evaluator with declarative conditions
//!
//! # Key Modules (to be built)
//! - `order_management` — statig state machine for order lifecycle
//! - `risk_manager` — Position sizing, drawdown limits, P&L tracking
//! - `order_executor` — Dhan REST API order submission with governor rate limiting
//!
//! # Pipeline Position
//! ParsedTick → **IndicatorEngine → StrategyEvaluator** → OMS → Risk → Execute → Persist

pub mod indicator;
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
        assert_eq!(crate_name, "dhan-live-trader-trading");
    }
}

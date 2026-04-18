// Compile-time lint enforcement — defense-in-depth with CLI clippy flags
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]
#![deny(clippy::dbg_macro)]
// Phase 0.2: no dropped Result/JoinHandle/must-use values (silent error swallowing).
#![cfg_attr(not(test), deny(unused_must_use))]
#![cfg_attr(not(test), warn(clippy::let_underscore_must_use))]
#![allow(missing_docs)] // TODO: enforce after adding docs to all public items

//! Trading engine: O(1) indicators, FSM strategies, OMS, Greeks, and risk controls.
//!
//! # Modules
//! - `greeks` — Options Greeks engine: Black-Scholes, IV solver, PCR, Buildup classification
//! - `indicator` — O(1) per-tick indicator engine (EMA, RSI, MACD, ATR, Bollinger, etc.)
//! - `strategy` — FSM-based strategy evaluator with declarative conditions
//! - `risk` — Risk engine: max daily loss, position limits, P&L tracking, auto-halt
//! - `oms` — Order Management System: lifecycle FSM, rate limiting, circuit breaker, reconciliation
//!
//! # Pipeline Position
//! ParsedTick → **IndicatorEngine → GreeksEngine → StrategyEvaluator** → OMS → Risk → Execute → Persist

pub mod greeks;
pub mod indicator;
pub mod oms;
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

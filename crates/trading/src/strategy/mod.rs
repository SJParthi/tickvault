//! Strategy evaluation — FSM-based O(1) trade signal generation.
//!
//! Each strategy is a finite state machine. `match` on enum variants
//! compiles to a jump table — O(1) dispatch regardless of state count.

pub mod evaluator;
#[cfg(test)]
mod tests;
pub mod types;

pub use evaluator::StrategyInstance;
pub use types::{
    ComparisonOp, Condition, ExitReason, IndicatorField, Signal, StrategyDefinition, StrategyState,
};

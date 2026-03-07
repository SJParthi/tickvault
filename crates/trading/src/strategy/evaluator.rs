//! Strategy evaluator — O(1) FSM state machine per strategy instance.
//!
//! Each strategy instance maintains a `StrategyState` enum. Evaluation
//! is a `match` on the enum — compiles to a jump table for O(1) dispatch.

use crate::indicator::IndicatorSnapshot;

use super::types::{ExitReason, Signal, StrategyDefinition, StrategyState};

// ---------------------------------------------------------------------------
// Strategy Instance — single FSM
// ---------------------------------------------------------------------------

/// A single strategy instance: definition + current FSM state.
///
/// Each instance tracks one strategy on one or more instruments.
/// The FSM state is `Copy` — zero allocation on state transitions.
pub struct StrategyInstance {
    /// The strategy rules (loaded from TOML).
    definition: StrategyDefinition,
    /// Current FSM state per security_id.
    /// Flat Vec indexed by security_id for O(1) lookup.
    states: Vec<StrategyState>,
    /// Tick counter (monotonically increasing).
    tick_counter: u32,
}

impl StrategyInstance {
    /// Creates a new strategy instance from a definition.
    ///
    /// Pre-allocates state for all tracked security IDs.
    pub fn new(definition: StrategyDefinition, max_security_id: usize) -> Self {
        let states = vec![StrategyState::default(); max_security_id];
        Self {
            definition,
            states,
            tick_counter: 0,
        }
    }

    /// Evaluates the strategy for a given indicator snapshot.
    ///
    /// # Performance
    /// O(C) where C = number of conditions (typically 2-5).
    /// FSM dispatch is O(1) via jump table. Total: ~5-50ns.
    ///
    /// # Returns
    /// The resulting `Signal` (Hold, EnterLong, EnterShort, or Exit).
    #[allow(clippy::arithmetic_side_effects)] // APPROVED: tick_counter saturating_add; ATR multiplications are bounded
    pub fn evaluate(&mut self, snapshot: &IndicatorSnapshot) -> Signal {
        let sid = snapshot.security_id as usize;
        if sid >= self.states.len() || !snapshot.is_warm {
            return Signal::Hold;
        }

        self.tick_counter = self.tick_counter.saturating_add(1);
        let state = self.states[sid];
        let (new_state, signal) = self.transition(state, snapshot);
        self.states[sid] = new_state;
        signal
    }

    /// FSM transition function — the core O(1) dispatch.
    ///
    /// `match` on `StrategyState` compiles to a jump table.
    #[allow(clippy::arithmetic_side_effects)] // APPROVED: ATR-based price calculations are bounded finite f64
    fn transition(
        &self,
        state: StrategyState,
        snapshot: &IndicatorSnapshot,
    ) -> (StrategyState, Signal) {
        match state {
            StrategyState::Idle => self.evaluate_idle(snapshot),

            StrategyState::WaitingForConfirmation {
                signal_tick,
                strength: _,
                is_long,
            } => self.evaluate_waiting(snapshot, signal_tick, is_long),

            StrategyState::InPosition {
                entry_price,
                stop_loss,
                target,
                is_long,
                highest_since_entry,
                lowest_since_entry,
            } => self.evaluate_in_position(
                snapshot,
                entry_price,
                stop_loss,
                target,
                is_long,
                highest_since_entry,
                lowest_since_entry,
            ),

            StrategyState::ExitPending { reason } => {
                // Stay in ExitPending until OMS confirms the exit.
                // The OMS will reset state to Idle after fill.
                (StrategyState::ExitPending { reason }, Signal::Hold)
            }
        }
    }

    /// Idle state: check entry conditions.
    fn evaluate_idle(&self, snapshot: &IndicatorSnapshot) -> (StrategyState, Signal) {
        // Check long entry conditions (AND logic)
        let long_entry = !self.definition.entry_long_conditions.is_empty()
            && self
                .definition
                .entry_long_conditions
                .iter()
                .all(|cond| cond.evaluate(snapshot));

        // Check short entry conditions (AND logic)
        let short_entry = !self.definition.entry_short_conditions.is_empty()
            && self
                .definition
                .entry_short_conditions
                .iter()
                .all(|cond| cond.evaluate(snapshot));

        if long_entry {
            if self.definition.confirmation_ticks == 0 {
                return self.generate_entry_signal(snapshot, true);
            }
            return (
                StrategyState::WaitingForConfirmation {
                    signal_tick: self.tick_counter,
                    strength: snapshot.rsi / 100.0,
                    is_long: true,
                },
                Signal::Hold,
            );
        }

        if short_entry {
            if self.definition.confirmation_ticks == 0 {
                return self.generate_entry_signal(snapshot, false);
            }
            return (
                StrategyState::WaitingForConfirmation {
                    signal_tick: self.tick_counter,
                    strength: 1.0 - snapshot.rsi / 100.0,
                    is_long: false,
                },
                Signal::Hold,
            );
        }

        (StrategyState::Idle, Signal::Hold)
    }

    /// Waiting state: re-check conditions after confirmation period.
    #[allow(clippy::arithmetic_side_effects)] // APPROVED: tick_counter subtraction checked by saturating
    fn evaluate_waiting(
        &self,
        snapshot: &IndicatorSnapshot,
        signal_tick: u32,
        is_long: bool,
    ) -> (StrategyState, Signal) {
        let ticks_elapsed = self.tick_counter.saturating_sub(signal_tick);

        if ticks_elapsed >= self.definition.confirmation_ticks {
            // Re-validate conditions
            let conditions = if is_long {
                &self.definition.entry_long_conditions
            } else {
                &self.definition.entry_short_conditions
            };

            if conditions.iter().all(|cond| cond.evaluate(snapshot)) {
                return self.generate_entry_signal(snapshot, is_long);
            }
            // Conditions no longer met — back to idle
            return (StrategyState::Idle, Signal::Hold);
        }

        // Still waiting
        (
            StrategyState::WaitingForConfirmation {
                signal_tick,
                strength: if is_long {
                    snapshot.rsi / 100.0
                } else {
                    1.0 - snapshot.rsi / 100.0
                },
                is_long,
            },
            Signal::Hold,
        )
    }

    /// In-position state: check exit conditions, stop-loss, target, trailing stop.
    #[allow(clippy::arithmetic_side_effects)] // APPROVED: price arithmetic is bounded finite f64
    #[allow(clippy::too_many_arguments)]
    fn evaluate_in_position(
        &self,
        snapshot: &IndicatorSnapshot,
        entry_price: f64,
        stop_loss: f64,
        target: f64,
        is_long: bool,
        highest_since_entry: f64,
        lowest_since_entry: f64,
    ) -> (StrategyState, Signal) {
        let price = snapshot.last_traded_price;

        // Update tracking extremes
        let new_highest = highest_since_entry.max(price);
        let new_lowest = lowest_since_entry.min(price);

        // Check stop-loss
        if is_long && price <= stop_loss {
            return (
                StrategyState::ExitPending {
                    reason: ExitReason::StopLossHit,
                },
                Signal::Exit {
                    reason: ExitReason::StopLossHit,
                },
            );
        }
        if !is_long && price >= stop_loss {
            return (
                StrategyState::ExitPending {
                    reason: ExitReason::StopLossHit,
                },
                Signal::Exit {
                    reason: ExitReason::StopLossHit,
                },
            );
        }

        // Check target
        if is_long && price >= target {
            return (
                StrategyState::ExitPending {
                    reason: ExitReason::TargetHit,
                },
                Signal::Exit {
                    reason: ExitReason::TargetHit,
                },
            );
        }
        if !is_long && price <= target {
            return (
                StrategyState::ExitPending {
                    reason: ExitReason::TargetHit,
                },
                Signal::Exit {
                    reason: ExitReason::TargetHit,
                },
            );
        }

        // Check trailing stop
        if self.definition.trailing_stop_enabled && snapshot.atr > 0.0 {
            let trail_offset = self.definition.trailing_stop_atr_multiplier * snapshot.atr;
            if is_long {
                let trailing_stop = new_highest - trail_offset;
                if price <= trailing_stop && trailing_stop > stop_loss {
                    return (
                        StrategyState::ExitPending {
                            reason: ExitReason::TrailingStop,
                        },
                        Signal::Exit {
                            reason: ExitReason::TrailingStop,
                        },
                    );
                }
            } else {
                let trailing_stop = new_lowest + trail_offset;
                if price >= trailing_stop && trailing_stop < stop_loss {
                    return (
                        StrategyState::ExitPending {
                            reason: ExitReason::TrailingStop,
                        },
                        Signal::Exit {
                            reason: ExitReason::TrailingStop,
                        },
                    );
                }
            }
        }

        // Check exit conditions (OR logic — any condition triggers exit)
        for cond in &self.definition.exit_conditions {
            if cond.evaluate(snapshot) {
                return (
                    StrategyState::ExitPending {
                        reason: ExitReason::SignalReversal,
                    },
                    Signal::Exit {
                        reason: ExitReason::SignalReversal,
                    },
                );
            }
        }

        // Hold position — update tracking extremes
        (
            StrategyState::InPosition {
                entry_price,
                stop_loss,
                target,
                is_long,
                highest_since_entry: new_highest,
                lowest_since_entry: new_lowest,
            },
            Signal::Hold,
        )
    }

    /// Generates an entry signal with ATR-based stop-loss and target.
    #[allow(clippy::arithmetic_side_effects)] // APPROVED: ATR multiplications are bounded finite f64
    fn generate_entry_signal(
        &self,
        snapshot: &IndicatorSnapshot,
        is_long: bool,
    ) -> (StrategyState, Signal) {
        let price = snapshot.last_traded_price;
        let atr = snapshot.atr;

        let (stop_loss, target) = if is_long {
            (
                price - self.definition.stop_loss_atr_multiplier * atr,
                price + self.definition.target_atr_multiplier * atr,
            )
        } else {
            (
                price + self.definition.stop_loss_atr_multiplier * atr,
                price - self.definition.target_atr_multiplier * atr,
            )
        };

        let signal = if is_long {
            Signal::EnterLong {
                size_fraction: self.definition.position_size_fraction,
                stop_loss,
                target,
            }
        } else {
            Signal::EnterShort {
                size_fraction: self.definition.position_size_fraction,
                stop_loss,
                target,
            }
        };

        let new_state = StrategyState::InPosition {
            entry_price: price,
            stop_loss,
            target,
            is_long,
            highest_since_entry: price,
            lowest_since_entry: price,
        };

        (new_state, signal)
    }

    /// Returns a reference to the strategy definition.
    pub fn definition(&self) -> &StrategyDefinition {
        &self.definition
    }

    /// Resets the FSM state for a given security_id to Idle.
    /// Called by the OMS after a position is fully closed.
    pub fn reset_state(&mut self, security_id: u32) {
        let sid = security_id as usize;
        if sid < self.states.len() {
            self.states[sid] = StrategyState::Idle;
        }
    }
}

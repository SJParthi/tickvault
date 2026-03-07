//! Strategy types: FSM states, signals, and configuration.
//!
//! Strategy evaluation is a state machine transition — Rust enum `match`
//! compiles to a jump table for O(1) dispatch.

use crate::indicator::IndicatorSnapshot;

// ---------------------------------------------------------------------------
// Trade Signal — output of strategy evaluation
// ---------------------------------------------------------------------------

/// A trade signal emitted by a strategy FSM.
///
/// `Copy` for zero-allocation passing to the OMS.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Signal {
    /// Enter a long position.
    EnterLong {
        /// Suggested position size as fraction of capital (0.0 - 1.0).
        size_fraction: f64,
        /// Stop-loss price.
        stop_loss: f64,
        /// Target price.
        target: f64,
    },
    /// Enter a short position.
    EnterShort {
        /// Suggested position size as fraction of capital (0.0 - 1.0).
        size_fraction: f64,
        /// Stop-loss price.
        stop_loss: f64,
        /// Target price.
        target: f64,
    },
    /// Exit current position.
    Exit {
        /// Reason for exit.
        reason: ExitReason,
    },
    /// No action — hold current position (or stay flat).
    Hold,
}

/// Reason for exiting a position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    /// Target price reached.
    TargetHit,
    /// Stop-loss triggered.
    StopLossHit,
    /// Strategy signal reversed.
    SignalReversal,
    /// End of trading session.
    SessionEnd,
    /// Trailing stop triggered.
    TrailingStop,
}

// ---------------------------------------------------------------------------
// Strategy FSM State — enum for O(1) jump-table dispatch
// ---------------------------------------------------------------------------

/// FSM state for a single strategy instance.
///
/// The Rust compiler generates a jump table for `match` on this enum,
/// giving O(1) state dispatch regardless of the number of states.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum StrategyState {
    /// Waiting for entry conditions.
    #[default]
    Idle,

    /// Entry conditions partially met — waiting for confirmation.
    WaitingForConfirmation {
        /// The tick count when the signal was first detected.
        signal_tick: u32,
        /// Signal strength (0.0 - 1.0).
        strength: f64,
        /// Whether the anticipated direction is long (true) or short (false).
        is_long: bool,
    },

    /// Currently in a position.
    InPosition {
        /// Entry price.
        entry_price: f64,
        /// Stop-loss price.
        stop_loss: f64,
        /// Target price.
        target: f64,
        /// Whether long (true) or short (false).
        is_long: bool,
        /// Highest price since entry (for trailing stop).
        highest_since_entry: f64,
        /// Lowest price since entry (for trailing stop).
        lowest_since_entry: f64,
    },

    /// Position exit pending — waiting for OMS confirmation.
    ExitPending {
        /// Reason for the exit.
        reason: ExitReason,
    },
}

// ---------------------------------------------------------------------------
// Strategy Condition — declarative entry/exit rules
// ---------------------------------------------------------------------------

/// A single condition that can be evaluated against an indicator snapshot.
///
/// Conditions are loaded from TOML configuration and evaluated in O(1)
/// per condition (single field access + comparison).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Condition {
    /// Which indicator field to read.
    pub field: IndicatorField,
    /// Comparison operator.
    pub operator: ComparisonOp,
    /// Threshold value.
    pub threshold: f64,
}

impl Condition {
    /// Evaluates this condition against an indicator snapshot.
    ///
    /// O(1) — one field read + one comparison.
    #[inline(always)]
    pub fn evaluate(&self, snapshot: &IndicatorSnapshot) -> bool {
        let value = self.field.read(snapshot);
        self.operator.compare(value, self.threshold)
    }
}

/// Which indicator field to read from a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndicatorField {
    Rsi,
    MacdLine,
    MacdSignal,
    MacdHistogram,
    EmaFast,
    EmaSlow,
    Sma,
    Vwap,
    BollingerUpper,
    BollingerMiddle,
    BollingerLower,
    Atr,
    Supertrend,
    Adx,
    Obv,
    LastTradedPrice,
    Volume,
    DayHigh,
    DayLow,
}

impl IndicatorField {
    /// Reads the corresponding value from a snapshot.
    ///
    /// O(1) — compiles to a jump table or direct offset load.
    #[inline(always)]
    pub fn read(self, snapshot: &IndicatorSnapshot) -> f64 {
        match self {
            Self::Rsi => snapshot.rsi,
            Self::MacdLine => snapshot.macd_line,
            Self::MacdSignal => snapshot.macd_signal,
            Self::MacdHistogram => snapshot.macd_histogram,
            Self::EmaFast => snapshot.ema_fast,
            Self::EmaSlow => snapshot.ema_slow,
            Self::Sma => snapshot.sma,
            Self::Vwap => snapshot.vwap,
            Self::BollingerUpper => snapshot.bollinger_upper,
            Self::BollingerMiddle => snapshot.bollinger_middle,
            Self::BollingerLower => snapshot.bollinger_lower,
            Self::Atr => snapshot.atr,
            Self::Supertrend => snapshot.supertrend,
            Self::Adx => snapshot.adx,
            Self::Obv => snapshot.obv,
            Self::LastTradedPrice => snapshot.last_traded_price,
            Self::Volume => snapshot.volume,
            Self::DayHigh => snapshot.day_high,
            Self::DayLow => snapshot.day_low,
        }
    }
}

/// Comparison operator for conditions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComparisonOp {
    /// Greater than.
    Gt,
    /// Greater than or equal.
    Gte,
    /// Less than.
    Lt,
    /// Less than or equal.
    Lte,
    /// Crosses above (requires previous state — approximated as Gt).
    CrossAbove,
    /// Crosses below (requires previous state — approximated as Lt).
    CrossBelow,
}

impl ComparisonOp {
    /// Compares two values.
    #[inline(always)]
    pub fn compare(self, value: f64, threshold: f64) -> bool {
        match self {
            Self::Gt | Self::CrossAbove => value > threshold,
            Self::Gte => value >= threshold,
            Self::Lt | Self::CrossBelow => value < threshold,
            Self::Lte => value <= threshold,
        }
    }
}

// ---------------------------------------------------------------------------
// Strategy Definition — loaded from TOML
// ---------------------------------------------------------------------------

/// A complete strategy definition with entry/exit rules.
///
/// Loaded from TOML at startup or via hot-reload.
/// Contains no heap allocations in the evaluation path.
#[derive(Debug, Clone)]
pub struct StrategyDefinition {
    /// Human-readable strategy name.
    pub name: String,
    /// Security IDs this strategy applies to.
    pub security_ids: Vec<u32>,
    /// All entry conditions must be true to trigger entry (AND logic).
    pub entry_long_conditions: Vec<Condition>,
    /// All short entry conditions must be true (AND logic).
    pub entry_short_conditions: Vec<Condition>,
    /// Any exit condition triggers an exit (OR logic).
    pub exit_conditions: Vec<Condition>,
    /// Position size as fraction of capital (0.0 - 1.0).
    pub position_size_fraction: f64,
    /// ATR multiplier for stop-loss calculation.
    pub stop_loss_atr_multiplier: f64,
    /// ATR multiplier for target calculation.
    pub target_atr_multiplier: f64,
    /// Number of ticks to wait for confirmation after initial signal.
    pub confirmation_ticks: u32,
    /// Enable trailing stop-loss.
    pub trailing_stop_enabled: bool,
    /// Trailing stop ATR multiplier (only used if trailing_stop_enabled).
    pub trailing_stop_atr_multiplier: f64,
}

//! TOML strategy configuration: parse → validate → convert to `StrategyDefinition`.
//!
//! Strategies are defined declaratively in TOML files. Each file can contain
//! multiple strategy definitions. The config module handles:
//! 1. Serde deserialization from TOML
//! 2. Validation (field names, operator strings, value ranges)
//! 3. Conversion to runtime `StrategyDefinition` structs
//!
//! # TOML Format
//! ```toml
//! [[strategy]]
//! name = "rsi_mean_reversion"
//! security_ids = [13, 25, 27]
//! position_size_fraction = 0.1
//! stop_loss_atr_multiplier = 2.0
//! target_atr_multiplier = 3.0
//! confirmation_ticks = 0
//! trailing_stop_enabled = false
//! trailing_stop_atr_multiplier = 1.5
//!
//! [[strategy.entry_long]]
//! field = "rsi"
//! operator = "lt"
//! threshold = 30.0
//!
//! [[strategy.entry_short]]
//! field = "rsi"
//! operator = "gt"
//! threshold = 70.0
//!
//! [[strategy.exit]]
//! field = "rsi"
//! operator = "gt"
//! threshold = 50.0
//! ```

use serde::Deserialize;

use super::types::{ComparisonOp, Condition, IndicatorField, StrategyDefinition};
use crate::indicator::IndicatorParams;

// ---------------------------------------------------------------------------
// TOML Serde Types — intermediate deserialization layer
// ---------------------------------------------------------------------------

/// Root TOML document containing strategy definitions and optional indicator params.
#[derive(Debug, Deserialize)]
pub struct StrategyConfigFile {
    /// Optional indicator parameters override (defaults used if absent).
    #[serde(default)]
    pub indicator_params: Option<TomlIndicatorParams>,
    /// List of strategy definitions.
    #[serde(default)]
    pub strategy: Vec<TomlStrategy>,
}

/// TOML representation of indicator parameters.
#[derive(Debug, Deserialize)]
pub struct TomlIndicatorParams {
    #[serde(default = "default_ema_fast_period")]
    pub ema_fast_period: u16,
    #[serde(default = "default_ema_slow_period")]
    pub ema_slow_period: u16,
    #[serde(default = "default_macd_signal_period")]
    pub macd_signal_period: u16,
    #[serde(default = "default_rsi_period")]
    pub rsi_period: u16,
    #[serde(default = "default_sma_period")]
    pub sma_period: u16,
    #[serde(default = "default_atr_period")]
    pub atr_period: u16,
    #[serde(default = "default_adx_period")]
    pub adx_period: u16,
    #[serde(default = "default_supertrend_multiplier")]
    pub supertrend_multiplier: f64,
    #[serde(default = "default_bollinger_multiplier")]
    pub bollinger_multiplier: f64,
}

// Default value functions for serde
const fn default_ema_fast_period() -> u16 {
    12
}
const fn default_ema_slow_period() -> u16 {
    26
}
const fn default_macd_signal_period() -> u16 {
    9
}
const fn default_rsi_period() -> u16 {
    14
}
const fn default_sma_period() -> u16 {
    20
}
const fn default_atr_period() -> u16 {
    14
}
const fn default_adx_period() -> u16 {
    14
}
const fn default_supertrend_multiplier() -> f64 {
    3.0
}
const fn default_bollinger_multiplier() -> f64 {
    2.0
}

/// TOML representation of a single strategy.
#[derive(Debug, Deserialize)]
pub struct TomlStrategy {
    /// Human-readable name.
    pub name: String,
    /// Security IDs this strategy applies to.
    #[serde(default)]
    pub security_ids: Vec<u32>,
    /// Position size fraction (0.0 - 1.0).
    #[serde(default = "default_position_size")]
    pub position_size_fraction: f64,
    /// Stop-loss in ATR multiples.
    #[serde(default = "default_stop_loss_atr")]
    pub stop_loss_atr_multiplier: f64,
    /// Target in ATR multiples.
    #[serde(default = "default_target_atr")]
    pub target_atr_multiplier: f64,
    /// Confirmation ticks before entry.
    #[serde(default)]
    pub confirmation_ticks: u32,
    /// Enable trailing stop.
    #[serde(default)]
    pub trailing_stop_enabled: bool,
    /// Trailing stop ATR multiplier.
    #[serde(default = "default_trailing_stop_atr")]
    pub trailing_stop_atr_multiplier: f64,
    /// Long entry conditions (AND logic).
    #[serde(default)]
    pub entry_long: Vec<TomlCondition>,
    /// Short entry conditions (AND logic).
    #[serde(default)]
    pub entry_short: Vec<TomlCondition>,
    /// Exit conditions (OR logic).
    #[serde(default)]
    pub exit: Vec<TomlCondition>,
}

const fn default_position_size() -> f64 {
    0.1
}
const fn default_stop_loss_atr() -> f64 {
    2.0
}
const fn default_target_atr() -> f64 {
    3.0
}
const fn default_trailing_stop_atr() -> f64 {
    1.5
}

/// TOML representation of a single condition.
#[derive(Debug, Deserialize)]
pub struct TomlCondition {
    /// Indicator field name (e.g., "rsi", "macd_line", "ema_fast").
    pub field: String,
    /// Comparison operator (e.g., "gt", "lt", "gte", "lte", "cross_above", "cross_below").
    pub operator: String,
    /// Threshold value.
    pub threshold: f64,
}

// ---------------------------------------------------------------------------
// Config Errors
// ---------------------------------------------------------------------------

/// Errors that can occur during strategy configuration parsing or validation.
#[derive(Debug, thiserror::Error)]
pub enum StrategyConfigError {
    /// TOML parsing failed.
    #[error("TOML parse error: {0}")]
    TomlParse(#[from] toml::de::Error),

    /// Unknown indicator field name in condition.
    #[error("unknown indicator field '{field}' in strategy '{strategy}'")]
    UnknownField { strategy: String, field: String },

    /// Unknown comparison operator in condition.
    #[error("unknown operator '{operator}' in strategy '{strategy}'")]
    UnknownOperator { strategy: String, operator: String },

    /// Validation error.
    #[error("validation error in strategy '{strategy}': {message}")]
    Validation { strategy: String, message: String },

    /// File I/O error.
    #[error("failed to read config file: {0}")]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Parsing & Conversion
// ---------------------------------------------------------------------------

/// Parses a TOML string into a list of `StrategyDefinition` and optional `IndicatorParams`.
///
/// # Cold path
/// Called at startup and on hot-reload. Heap allocation is acceptable.
pub fn parse_strategy_config(
    toml_content: &str,
) -> Result<(Vec<StrategyDefinition>, IndicatorParams), StrategyConfigError> {
    let config: StrategyConfigFile = toml::from_str(toml_content)?;

    let indicator_params = config
        .indicator_params
        .map(|p| IndicatorParams {
            ema_fast_period: p.ema_fast_period,
            ema_slow_period: p.ema_slow_period,
            macd_signal_period: p.macd_signal_period,
            rsi_period: p.rsi_period,
            sma_period: p.sma_period,
            atr_period: p.atr_period,
            adx_period: p.adx_period,
            supertrend_multiplier: p.supertrend_multiplier,
            bollinger_multiplier: p.bollinger_multiplier,
        })
        .unwrap_or_default();

    let mut definitions = Vec::with_capacity(config.strategy.len());
    for strategy in config.strategy {
        let def = convert_strategy(&strategy)?;
        definitions.push(def);
    }

    Ok((definitions, indicator_params))
}

/// Loads strategy configuration from a file path.
///
/// # Cold path
/// Called at startup and on hot-reload.
pub fn load_strategy_config_file(
    path: &std::path::Path,
) -> Result<(Vec<StrategyDefinition>, IndicatorParams), StrategyConfigError> {
    let content = std::fs::read_to_string(path)?;
    parse_strategy_config(&content)
}

/// Converts a TOML strategy to a runtime `StrategyDefinition`.
fn convert_strategy(toml: &TomlStrategy) -> Result<StrategyDefinition, StrategyConfigError> {
    // Validate position size
    if !(0.0..=1.0).contains(&toml.position_size_fraction) {
        return Err(StrategyConfigError::Validation {
            strategy: toml.name.clone(),
            message: format!(
                "position_size_fraction must be 0.0-1.0, got {}",
                toml.position_size_fraction
            ),
        });
    }

    // Validate ATR multipliers are positive
    if toml.stop_loss_atr_multiplier <= 0.0 {
        return Err(StrategyConfigError::Validation {
            strategy: toml.name.clone(),
            message: "stop_loss_atr_multiplier must be positive".to_owned(),
        });
    }
    if toml.target_atr_multiplier <= 0.0 {
        return Err(StrategyConfigError::Validation {
            strategy: toml.name.clone(),
            message: "target_atr_multiplier must be positive".to_owned(),
        });
    }

    // Validate at least one entry condition exists
    if toml.entry_long.is_empty() && toml.entry_short.is_empty() {
        return Err(StrategyConfigError::Validation {
            strategy: toml.name.clone(),
            message: "strategy must have at least one entry_long or entry_short condition"
                .to_owned(),
        });
    }

    let entry_long_conditions = toml
        .entry_long
        .iter()
        .map(|c| convert_condition(c, &toml.name))
        .collect::<Result<Vec<_>, _>>()?;

    let entry_short_conditions = toml
        .entry_short
        .iter()
        .map(|c| convert_condition(c, &toml.name))
        .collect::<Result<Vec<_>, _>>()?;

    let exit_conditions = toml
        .exit
        .iter()
        .map(|c| convert_condition(c, &toml.name))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(StrategyDefinition {
        name: toml.name.clone(),
        security_ids: toml.security_ids.clone(),
        entry_long_conditions,
        entry_short_conditions,
        exit_conditions,
        position_size_fraction: toml.position_size_fraction,
        stop_loss_atr_multiplier: toml.stop_loss_atr_multiplier,
        target_atr_multiplier: toml.target_atr_multiplier,
        confirmation_ticks: toml.confirmation_ticks,
        trailing_stop_enabled: toml.trailing_stop_enabled,
        trailing_stop_atr_multiplier: toml.trailing_stop_atr_multiplier,
    })
}

/// Converts a TOML condition to a runtime `Condition`.
fn convert_condition(
    toml: &TomlCondition,
    strategy_name: &str,
) -> Result<Condition, StrategyConfigError> {
    let field = parse_indicator_field(&toml.field, strategy_name)?;
    let operator = parse_comparison_op(&toml.operator, strategy_name)?;

    Ok(Condition {
        field,
        operator,
        threshold: toml.threshold,
    })
}

/// Parses a string indicator field name to `IndicatorField`.
fn parse_indicator_field(
    name: &str,
    strategy_name: &str,
) -> Result<IndicatorField, StrategyConfigError> {
    match name {
        "rsi" => Ok(IndicatorField::Rsi),
        "macd_line" => Ok(IndicatorField::MacdLine),
        "macd_signal" => Ok(IndicatorField::MacdSignal),
        "macd_histogram" => Ok(IndicatorField::MacdHistogram),
        "ema_fast" => Ok(IndicatorField::EmaFast),
        "ema_slow" => Ok(IndicatorField::EmaSlow),
        "sma" => Ok(IndicatorField::Sma),
        "vwap" => Ok(IndicatorField::Vwap),
        "bollinger_upper" => Ok(IndicatorField::BollingerUpper),
        "bollinger_middle" => Ok(IndicatorField::BollingerMiddle),
        "bollinger_lower" => Ok(IndicatorField::BollingerLower),
        "atr" => Ok(IndicatorField::Atr),
        "supertrend" => Ok(IndicatorField::Supertrend),
        "adx" => Ok(IndicatorField::Adx),
        "obv" => Ok(IndicatorField::Obv),
        "last_traded_price" | "ltp" => Ok(IndicatorField::LastTradedPrice),
        "volume" => Ok(IndicatorField::Volume),
        "day_high" => Ok(IndicatorField::DayHigh),
        "day_low" => Ok(IndicatorField::DayLow),
        _ => Err(StrategyConfigError::UnknownField {
            strategy: strategy_name.to_owned(),
            field: name.to_owned(),
        }),
    }
}

/// Parses a string comparison operator to `ComparisonOp`.
fn parse_comparison_op(op: &str, strategy_name: &str) -> Result<ComparisonOp, StrategyConfigError> {
    match op {
        "gt" | ">" => Ok(ComparisonOp::Gt),
        "gte" | ">=" => Ok(ComparisonOp::Gte),
        "lt" | "<" => Ok(ComparisonOp::Lt),
        "lte" | "<=" => Ok(ComparisonOp::Lte),
        "cross_above" => Ok(ComparisonOp::CrossAbove),
        "cross_below" => Ok(ComparisonOp::CrossBelow),
        _ => Err(StrategyConfigError::UnknownOperator {
            strategy: strategy_name.to_owned(),
            operator: op.to_owned(),
        }),
    }
}

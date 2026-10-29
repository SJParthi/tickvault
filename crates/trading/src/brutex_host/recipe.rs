//! `recipe.json`, `params.json`, `gates.json` and `universe.json` (contract §3).
//! Parsed strictly: an unknown schema, a missing field or a wrong status refuses.

// O(1) EXEMPT: begin — cold path: runs once before the market opens (or in the dry-run CLI), never per tick. The strategy crate behind `engine::StrategyEngine` carries the hot-path contract (R4–R6).
use super::{InstrumentKey, Refusal};

pub const RECIPE_SCHEMA: &str = "tickvault-strategy-recipe/1";
pub const RECIPE_FILE: &str = "recipe.json";
pub const PARAMS_FILE: &str = "params.json";
pub const GATES_FILE: &str = "gates.json";
pub const UNIVERSE_FILE: &str = "universe.json";
pub const CHECK_CANDLES: &str = "check_candles.csv";
pub const CHECK_VALUES: &str = "check_values.csv";
pub const CHECK_SIGNALS: &str = "check_signals.csv";

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct Recipe {
    pub schema: String,
    pub name: String,
    pub version: u32,
    pub status: String,
    pub approved_by: String,
    pub approved_at_ist: String,
    pub brutex_commit: String,
    #[serde(default)]
    pub dynamic: bool,
    #[serde(default)]
    pub instruments: Vec<InstrumentKey>,
    pub timeframes: Vec<String>,
    pub settings: serde_json::Value,
    pub backtest: serde_json::Value,
    #[serde(default)]
    pub sanity_limits: SanityLimits,
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Deserialize)]
pub struct SanityLimits {
    /// Largest allowed relative change of any numeric parameter between two
    /// consecutive snapshots (0.5 = 50%).
    pub max_param_change_fraction: f64,
}

impl Default for SanityLimits {
    fn default() -> Self {
        Self {
            max_param_change_fraction: 0.5,
        }
    }
}

/// A parameter snapshot (`v<N>/params/<date>/params.json`).
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct ParamSnapshot {
    pub date: String,
    /// Per-instrument or global numbers; compared against the previous snapshot
    /// by the sanity gate. Flat map of name -> number.
    pub params: std::collections::BTreeMap<String, f64>,
}

/// The Winner Gate result brutex computed on the newest walk-forward window.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct GatesFile {
    pub passed: bool,
    pub window_from: String,
    pub window_to: String,
    #[serde(default)]
    pub detail: serde_json::Value,
}

/// A universe snapshot (`universe/<date>/universe.json`).
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct UniverseSnapshot {
    pub date: String,
    pub instruments: Vec<InstrumentKey>,
}

/// Parses and checks a recipe against its folder version and tickvault's
/// built-against brutex commit.
pub fn parse_recipe(
    bytes: &[u8],
    folder_version: u32,
    built_commit: &str,
) -> Result<Recipe, Refusal> {
    let r: Recipe = serde_json::from_slice(bytes).map_err(|e| Refusal::BadRecipe(e.to_string()))?;
    if r.schema != RECIPE_SCHEMA {
        return Err(Refusal::BadRecipe(format!("schema {:?}", r.schema)));
    }
    if r.status != "approved" {
        return Err(Refusal::NotApproved(r.status));
    }
    if r.version != folder_version {
        return Err(Refusal::VersionMismatch {
            folder: folder_version,
            in_file: r.version,
        });
    }
    if !is_full_sha(&r.brutex_commit) {
        return Err(Refusal::BadRecipe(format!(
            "brutex_commit {:?} is not a 40-char sha",
            r.brutex_commit
        )));
    }
    if !r.brutex_commit.eq_ignore_ascii_case(built_commit) {
        return Err(Refusal::CommitMismatch {
            recipe: r.brutex_commit,
            built: built_commit.to_owned(),
        });
    }
    if r.name.is_empty() || r.timeframes.is_empty() {
        return Err(Refusal::BadRecipe("empty name or timeframes".to_owned()));
    }
    if !r.dynamic && r.instruments.is_empty() {
        return Err(Refusal::BadRecipe(
            "a fixed strategy lists no instruments".to_owned(),
        ));
    }
    let f = r.sanity_limits.max_param_change_fraction;
    if !f.is_finite() || f < 0.0 {
        return Err(Refusal::BadRecipe(
            "sanity_limits.max_param_change_fraction must be finite and >= 0".to_owned(),
        ));
    }
    Ok(r)
}

pub fn parse_json<T: serde::de::DeserializeOwned>(bytes: &[u8], what: &str) -> Result<T, Refusal> {
    serde_json::from_slice(bytes).map_err(|e| Refusal::BadRecipe(format!("{what}: {e}")))
}

#[must_use]
pub fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}
// O(1) EXEMPT: end

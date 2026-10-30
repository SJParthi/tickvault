//! Snapshot gates (decision 4): the Winner Gate result, sanity limits against
//! the previous snapshot, and the universe checks. Pure; O(params + instruments).

// O(1) EXEMPT: begin — cold path: runs once before the market opens (or in the dry-run CLI), never per tick. The strategy crate behind `engine::StrategyEngine` carries the hot-path contract (R4–R6).
use std::collections::BTreeSet;

use super::recipe::{GatesFile, ParamSnapshot, SanityLimits, UniverseSnapshot};
use super::{InstrumentKey, Refusal};

/// The Winner Gate brutex computed on the newest walk-forward window.
pub fn winner_gate(g: &GatesFile) -> Result<(), Refusal> {
    if g.passed {
        Ok(())
    } else {
        Err(Refusal::Gate(format!(
            "Winner Gate failed on the walk-forward window {} to {}",
            g.window_from, g.window_to
        )))
    }
}

/// Sanity limits: no parameter may be non-finite, the set of names must not
/// change, and no value may move more than `max_param_change_fraction` relative
/// to the previous snapshot. `previous = None` (first snapshot) checks finiteness only.
pub fn sanity(
    new: &ParamSnapshot,
    previous: Option<&ParamSnapshot>,
    limits: SanityLimits,
) -> Result<(), Refusal> {
    if new.params.is_empty() {
        return Err(Refusal::Gate("parameter snapshot is empty".to_owned()));
    }
    for (name, v) in &new.params {
        if !v.is_finite() {
            return Err(Refusal::Gate(format!("parameter {name} is not finite")));
        }
    }
    let Some(prev) = previous else { return Ok(()) };
    let a: BTreeSet<&String> = new.params.keys().collect();
    let b: BTreeSet<&String> = prev.params.keys().collect();
    if a != b {
        let added: Vec<&&String> = a.difference(&b).collect();
        let removed: Vec<&&String> = b.difference(&a).collect();
        return Err(Refusal::Gate(format!(
            "parameter names changed: added {added:?}, removed {removed:?}"
        )));
    }
    for (name, v) in &new.params {
        let p = prev.params[name];
        let base = p.abs().max(f64::MIN_POSITIVE);
        let change = (v - p).abs() / base;
        if change > limits.max_param_change_fraction {
            return Err(Refusal::Gate(format!(
                "parameter {name} moved {:.1}% ({p} to {v}), limit {:.1}%",
                change * 100.0,
                limits.max_param_change_fraction * 100.0
            )));
        }
    }
    Ok(())
}

/// The universe must be non-empty, have no duplicate instrument, and name only
/// instruments tickvault knows today.
pub fn universe(
    u: &UniverseSnapshot,
    is_known: &dyn Fn(InstrumentKey) -> bool,
) -> Result<(), Refusal> {
    if u.instruments.is_empty() {
        return Err(Refusal::Gate("universe is empty".to_owned()));
    }
    let mut seen = BTreeSet::new();
    for k in &u.instruments {
        if !seen.insert(*k) {
            return Err(Refusal::Gate(format!(
                "instrument {}/{} listed twice",
                k.security_id, k.exchange_segment
            )));
        }
        if !is_known(*k) {
            return Err(Refusal::Gate(format!(
                "instrument {}/{} is not in today's instrument list",
                k.security_id, k.exchange_segment
            )));
        }
    }
    Ok(())
}
// O(1) EXEMPT: end

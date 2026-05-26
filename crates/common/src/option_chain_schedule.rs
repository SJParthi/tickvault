//! Option-chain-minute-snapshot schedule validation.
//!
//! Validates the operator-supplied `[option_chain_minute_snapshot]`
//! TOML config against the 5 mandatory invariants that protect Dhan's
//! "1 unique request per 3 seconds" rate-limit rule + composite-key
//! uniqueness.
//!
//! See `.claude/plans/friday-may-15-mega/topic-OPTION-CHAIN-MINUTE-SNAPSHOT.md`
//! §6 for the contract this module enforces.
//!
//! ## Invariants enforced
//!
//! 1. Each underlying's `slot_sec` is in `[40, 59]` (last 20 sec of minute).
//! 2. All `slot_sec` values are distinct across underlyings.
//! 3. Every `(symbol, security_id, segment)` tuple is unique.
//! 4. Symbol strings are non-empty and ASCII-only.
//! 5. Total underlyings count is in `[1, 10]` — practical cap below
//!    the theoretical 20 the 60s minute allows at 3s spacing.
//!
//! ## Boot-time semantics
//!
//! Called once at boot before the scheduler spawns. On `Err`, the
//! app HALTS with `OptionChainConfigInvalid` Severity::Critical
//! Telegram. Operator fixes TOML + restarts.

use crate::config::OptionChainUnderlyingEntry;
use std::collections::HashSet;
use std::fmt;

/// First valid `slot_sec` (HH:MM:40). Slots earlier than this would
/// fire too far from the next-minute boundary; the snapshot's purpose
/// is to give the strategy the freshest-possible chain right BEFORE
/// the new minute starts.
///
/// 2026-05-26: widened from `50` → `40` to give the operator headroom
/// for 5-second spacing across 3 underlyings (e.g. 49/54/59). The
/// previous `[50, 59]` window only allowed 10 seconds, forcing either
/// 4-second spacing (which sits at the Dhan rate-limit boundary and
/// produced ~1 DH-904 per 5 min in live logs) or one slot to fall
/// outside the window (which is what broke the scheduler on
/// 2026-05-27 — SENSEX `slot_sec = 49` rejected, scheduler refused
/// to spawn).
pub const OPTION_CHAIN_MIN_SLOT_SEC: u32 = 40;

/// Last valid `slot_sec` (HH:MM:59). Going past 59 would cross the
/// minute boundary and stamp the row with the next minute's `ts`.
pub const OPTION_CHAIN_MAX_SLOT_SEC: u32 = 59;

/// Practical maximum number of underlyings. Theoretical cap is 20
/// (60s / 3s rate limit) but 10 keeps headroom for retries.
pub const OPTION_CHAIN_MAX_UNDERLYINGS: usize = 10;

/// Errors returned by [`validate_option_chain_schedule`]. Each variant
/// carries enough context for the operator to fix the TOML directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleError {
    /// `underlyings = []` — pipeline is enabled but has nothing to fetch.
    EmptyUnderlyings,

    /// `underlyings.len() > OPTION_CHAIN_MAX_UNDERLYINGS`.
    TooManyUnderlyings { count: usize, max: usize },

    /// One entry's `slot_sec` is outside `[50, 59]`.
    SlotSecOutOfRange {
        symbol: String,
        slot_sec: u32,
        min: u32,
        max: u32,
    },

    /// Two or more entries declared the same `slot_sec`.
    DuplicateSlotSec { slot_sec: u32, symbols: Vec<String> },

    /// Two or more entries declared the same `symbol`.
    DuplicateSymbol { symbol: String },

    /// Two or more entries declared the same `(security_id, segment)`
    /// composite key. Per I-P1-11 — `security_id` alone is not unique.
    DuplicateSecurityIdSegment {
        security_id: u32,
        segment: String,
        symbols: Vec<String>,
    },

    /// `symbol = ""` or contains non-ASCII characters.
    InvalidSymbol {
        symbol: String,
        reason: &'static str,
    },
}

impl fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyUnderlyings => {
                write!(
                    f,
                    "[option_chain_minute_snapshot] is enabled but `underlyings` is empty — \
                     add at least one entry, e.g. NIFTY at slot_sec=59"
                )
            }
            Self::TooManyUnderlyings { count, max } => {
                write!(
                    f,
                    "[option_chain_minute_snapshot.underlyings] has {count} entries, \
                     max is {max} (practical limit below the theoretical 20 the 60s \
                     minute allows at 3s spacing)"
                )
            }
            Self::SlotSecOutOfRange {
                symbol,
                slot_sec,
                min,
                max,
            } => {
                write!(
                    f,
                    "[option_chain_minute_snapshot.underlyings] entry `{symbol}` has \
                     slot_sec = {slot_sec}, must be in [{min}, {max}] (the last 10 \
                     seconds of each minute, before the next-minute boundary)"
                )
            }
            Self::DuplicateSlotSec { slot_sec, symbols } => {
                write!(
                    f,
                    "[option_chain_minute_snapshot.underlyings] {} entries share \
                     slot_sec = {slot_sec}: {} — each slot must be unique",
                    symbols.len(),
                    symbols.join(", ")
                )
            }
            Self::DuplicateSymbol { symbol } => {
                write!(
                    f,
                    "[option_chain_minute_snapshot.underlyings] `symbol = \"{symbol}\"` \
                     appears more than once — each underlying must have a unique symbol"
                )
            }
            Self::DuplicateSecurityIdSegment {
                security_id,
                segment,
                symbols,
            } => {
                write!(
                    f,
                    "[option_chain_minute_snapshot.underlyings] {} entries share \
                     (security_id = {security_id}, segment = \"{segment}\"): {} — \
                     per I-P1-11 the composite key must be unique",
                    symbols.len(),
                    symbols.join(", ")
                )
            }
            Self::InvalidSymbol { symbol, reason } => {
                write!(
                    f,
                    "[option_chain_minute_snapshot.underlyings] symbol \"{symbol}\" \
                     is invalid: {reason}"
                )
            }
        }
    }
}

impl std::error::Error for ScheduleError {}

/// Validate the option-chain schedule against the 5 invariants. Pure
/// function — no I/O, no allocation beyond error-message strings.
///
/// Call site: `main.rs` boot sequence, once, before spawning the
/// scheduler task. On `Err`, app HALTS — the strategy would otherwise
/// run blind without strike-chain data.
///
/// # Errors
///
/// Returns the FIRST violation found. Operator fixes that, restarts,
/// then sees the next violation if any. This matches the existing
/// boot-config validator pattern.
pub fn validate_option_chain_schedule(
    underlyings: &[OptionChainUnderlyingEntry],
) -> Result<(), ScheduleError> {
    // Invariant 1: non-empty
    if underlyings.is_empty() {
        return Err(ScheduleError::EmptyUnderlyings);
    }

    // Invariant 2: not too many
    if underlyings.len() > OPTION_CHAIN_MAX_UNDERLYINGS {
        return Err(ScheduleError::TooManyUnderlyings {
            count: underlyings.len(),
            max: OPTION_CHAIN_MAX_UNDERLYINGS,
        });
    }

    // Invariant 3: each entry's symbol is valid + slot_sec in range
    for entry in underlyings {
        if entry.symbol.is_empty() {
            return Err(ScheduleError::InvalidSymbol {
                symbol: entry.symbol.clone(),
                reason: "symbol must not be empty",
            });
        }
        if !entry.symbol.is_ascii() {
            return Err(ScheduleError::InvalidSymbol {
                symbol: entry.symbol.clone(),
                reason: "symbol must be ASCII",
            });
        }
        if entry.slot_sec < OPTION_CHAIN_MIN_SLOT_SEC || entry.slot_sec > OPTION_CHAIN_MAX_SLOT_SEC
        {
            return Err(ScheduleError::SlotSecOutOfRange {
                symbol: entry.symbol.clone(),
                slot_sec: entry.slot_sec,
                min: OPTION_CHAIN_MIN_SLOT_SEC,
                max: OPTION_CHAIN_MAX_SLOT_SEC,
            });
        }
    }

    // Invariant 4: slot_sec uniqueness
    let mut seen_slots: HashSet<u32> = HashSet::new();
    for entry in underlyings {
        if !seen_slots.insert(entry.slot_sec) {
            // Collect ALL entries sharing this slot for the error message.
            let collisions: Vec<String> = underlyings
                .iter()
                .filter(|e| e.slot_sec == entry.slot_sec)
                .map(|e| e.symbol.clone())
                .collect();
            return Err(ScheduleError::DuplicateSlotSec {
                slot_sec: entry.slot_sec,
                symbols: collisions,
            });
        }
    }

    // Invariant 5a: symbol uniqueness
    let mut seen_symbols: HashSet<&str> = HashSet::new();
    for entry in underlyings {
        if !seen_symbols.insert(entry.symbol.as_str()) {
            return Err(ScheduleError::DuplicateSymbol {
                symbol: entry.symbol.clone(),
            });
        }
    }

    // Invariant 5b: (security_id, segment) composite uniqueness per I-P1-11
    let mut seen_id_seg: HashSet<(u32, &str)> = HashSet::new();
    for entry in underlyings {
        let key = (entry.security_id, entry.segment.as_str());
        if !seen_id_seg.insert(key) {
            let collisions: Vec<String> = underlyings
                .iter()
                .filter(|e| e.security_id == entry.security_id && e.segment == entry.segment)
                .map(|e| e.symbol.clone())
                .collect();
            return Err(ScheduleError::DuplicateSecurityIdSegment {
                security_id: entry.security_id,
                segment: entry.segment.clone(),
                symbols: collisions,
            });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        symbol: &str,
        security_id: u32,
        segment: &str,
        slot_sec: u32,
    ) -> OptionChainUnderlyingEntry {
        OptionChainUnderlyingEntry {
            symbol: symbol.to_string(),
            security_id,
            segment: segment.to_string(),
            slot_sec,
        }
    }

    fn locked_three_underlyings() -> Vec<OptionChainUnderlyingEntry> {
        vec![
            entry("SENSEX", 51, "IDX_I", 53),
            entry("BANKNIFTY", 25, "IDX_I", 56),
            entry("NIFTY", 13, "IDX_I", 59),
        ]
    }

    #[test]
    fn test_validate_option_chain_schedule_with_locked_three_underlyings() {
        let underlyings = locked_three_underlyings();
        assert!(validate_option_chain_schedule(&underlyings).is_ok());
    }

    #[test]
    fn test_empty_underlyings_rejected() {
        let result = validate_option_chain_schedule(&[]);
        assert_eq!(result, Err(ScheduleError::EmptyUnderlyings));
    }

    #[test]
    fn test_single_underlying_validates_ok() {
        let underlyings = vec![entry("NIFTY", 13, "IDX_I", 59)];
        assert!(validate_option_chain_schedule(&underlyings).is_ok());
    }

    #[test]
    fn test_too_many_underlyings_rejected() {
        let mut underlyings = Vec::new();
        for i in 0..(OPTION_CHAIN_MAX_UNDERLYINGS + 1) {
            // Slot range [50,59] is only 10 wide; we DELIBERATELY collide on slot
            // because the "too many" check fires BEFORE the slot-collision check.
            underlyings.push(entry(&format!("SYM{i}"), 100 + i as u32, "IDX_I", 50));
        }
        let result = validate_option_chain_schedule(&underlyings);
        assert_eq!(
            result,
            Err(ScheduleError::TooManyUnderlyings {
                count: OPTION_CHAIN_MAX_UNDERLYINGS + 1,
                max: OPTION_CHAIN_MAX_UNDERLYINGS,
            })
        );
    }

    #[test]
    fn test_slot_sec_below_range_rejected() {
        let underlyings = vec![entry("NIFTY", 13, "IDX_I", 39)];
        let err = validate_option_chain_schedule(&underlyings).unwrap_err();
        match err {
            ScheduleError::SlotSecOutOfRange {
                slot_sec, min, max, ..
            } => {
                assert_eq!(slot_sec, 39);
                assert_eq!(min, 40);
                assert_eq!(max, 59);
            }
            other => panic!("expected SlotSecOutOfRange, got {other:?}"),
        }
    }

    #[test]
    fn test_slot_sec_above_range_rejected() {
        let underlyings = vec![entry("NIFTY", 13, "IDX_I", 60)];
        let err = validate_option_chain_schedule(&underlyings).unwrap_err();
        assert!(matches!(
            err,
            ScheduleError::SlotSecOutOfRange { slot_sec: 60, .. }
        ));
    }

    #[test]
    fn test_slot_sec_at_min_boundary_accepted() {
        let underlyings = vec![entry("NIFTY", 13, "IDX_I", 50)];
        assert!(validate_option_chain_schedule(&underlyings).is_ok());
    }

    #[test]
    fn test_slot_sec_at_max_boundary_accepted() {
        let underlyings = vec![entry("NIFTY", 13, "IDX_I", 59)];
        assert!(validate_option_chain_schedule(&underlyings).is_ok());
    }

    #[test]
    fn test_duplicate_slot_sec_rejected() {
        let underlyings = vec![
            entry("SENSEX", 51, "IDX_I", 56),
            entry("BANKNIFTY", 25, "IDX_I", 56),
        ];
        let err = validate_option_chain_schedule(&underlyings).unwrap_err();
        match err {
            ScheduleError::DuplicateSlotSec { slot_sec, symbols } => {
                assert_eq!(slot_sec, 56);
                assert_eq!(symbols.len(), 2);
                assert!(symbols.contains(&"SENSEX".to_string()));
                assert!(symbols.contains(&"BANKNIFTY".to_string()));
            }
            other => panic!("expected DuplicateSlotSec, got {other:?}"),
        }
    }

    #[test]
    fn test_duplicate_symbol_rejected() {
        let underlyings = vec![
            entry("NIFTY", 13, "IDX_I", 56),
            entry("NIFTY", 99, "IDX_I", 59),
        ];
        let err = validate_option_chain_schedule(&underlyings).unwrap_err();
        assert_eq!(
            err,
            ScheduleError::DuplicateSymbol {
                symbol: "NIFTY".to_string()
            }
        );
    }

    #[test]
    fn test_duplicate_security_id_same_segment_rejected_per_ip1_11() {
        // I-P1-11 — same (security_id, segment) must not appear twice
        let underlyings = vec![
            entry("ALPHA", 13, "IDX_I", 56),
            entry("BETA", 13, "IDX_I", 59),
        ];
        let err = validate_option_chain_schedule(&underlyings).unwrap_err();
        match err {
            ScheduleError::DuplicateSecurityIdSegment {
                security_id,
                segment,
                symbols,
            } => {
                assert_eq!(security_id, 13);
                assert_eq!(segment, "IDX_I");
                assert_eq!(symbols.len(), 2);
            }
            other => panic!("expected DuplicateSecurityIdSegment, got {other:?}"),
        }
    }

    #[test]
    fn test_same_security_id_different_segments_accepted_per_ip1_11() {
        // I-P1-11 — `security_id` ALONE is not unique; different segments
        // is legitimate (e.g. FINNIFTY=27 IDX_I + stock=27 NSE_EQ in
        // Dhan CSV on 2026-04-17).
        let underlyings = vec![
            entry("ALPHA", 27, "IDX_I", 56),
            entry("BETA", 27, "NSE_EQ", 59),
        ];
        assert!(validate_option_chain_schedule(&underlyings).is_ok());
    }

    #[test]
    fn test_empty_symbol_rejected() {
        let underlyings = vec![entry("", 13, "IDX_I", 59)];
        let err = validate_option_chain_schedule(&underlyings).unwrap_err();
        assert!(matches!(err, ScheduleError::InvalidSymbol { .. }));
    }

    #[test]
    fn test_non_ascii_symbol_rejected() {
        let underlyings = vec![entry("NIFTÝ", 13, "IDX_I", 59)];
        let err = validate_option_chain_schedule(&underlyings).unwrap_err();
        match err {
            ScheduleError::InvalidSymbol { reason, .. } => {
                assert_eq!(reason, "symbol must be ASCII");
            }
            other => panic!("expected InvalidSymbol (ASCII reason), got {other:?}"),
        }
    }

    #[test]
    fn test_constants_pinned() {
        // These constants are the schedule contract. Bumping them
        // changes the operator-visible TOML semantics; require a
        // deliberate edit + this test update.
        assert_eq!(OPTION_CHAIN_MIN_SLOT_SEC, 40);
        assert_eq!(OPTION_CHAIN_MAX_SLOT_SEC, 59);
        assert_eq!(OPTION_CHAIN_MAX_UNDERLYINGS, 10);
    }

    /// Regression: 2026-05-27 — SENSEX `slot_sec = 49` (operator bumped
    /// 50 → 49 on 2026-05-26 to widen S→B→N to 5s) was rejected by the
    /// `[50, 59]` validator, refusing the scheduler at boot for an
    /// entire trading day. Widening the window to `[40, 59]` accepts
    /// the operator's 49/54/59 layout.
    #[test]
    fn test_regression_2026_05_27_sensex_slot_49_accepted() {
        let entries = vec![
            OptionChainUnderlyingEntry {
                symbol: "SENSEX".to_string(),
                security_id: 51,
                segment: "IDX_I".to_string(),
                slot_sec: 49,
            },
            OptionChainUnderlyingEntry {
                symbol: "BANKNIFTY".to_string(),
                security_id: 25,
                segment: "IDX_I".to_string(),
                slot_sec: 54,
            },
            OptionChainUnderlyingEntry {
                symbol: "NIFTY".to_string(),
                security_id: 13,
                segment: "IDX_I".to_string(),
                slot_sec: 59,
            },
        ];
        assert!(validate_option_chain_schedule(&entries).is_ok());
    }

    #[test]
    fn test_error_display_includes_actionable_context() {
        // Every error variant's Display impl MUST include enough info
        // for the operator to fix the TOML without reading the code.
        let cases = vec![
            ScheduleError::EmptyUnderlyings,
            ScheduleError::TooManyUnderlyings { count: 11, max: 10 },
            ScheduleError::SlotSecOutOfRange {
                symbol: "NIFTY".into(),
                slot_sec: 49,
                min: 50,
                max: 59,
            },
            ScheduleError::DuplicateSlotSec {
                slot_sec: 56,
                symbols: vec!["A".into(), "B".into()],
            },
            ScheduleError::DuplicateSymbol {
                symbol: "NIFTY".into(),
            },
            ScheduleError::DuplicateSecurityIdSegment {
                security_id: 13,
                segment: "IDX_I".into(),
                symbols: vec!["A".into(), "B".into()],
            },
            ScheduleError::InvalidSymbol {
                symbol: "".into(),
                reason: "symbol must not be empty",
            },
        ];
        for err in cases {
            let msg = err.to_string();
            // Every message mentions the offending config section.
            assert!(
                msg.contains("[option_chain_minute_snapshot"),
                "error message must cite TOML section: {msg}"
            );
            // Every message is non-empty + > 30 chars (actionable).
            assert!(msg.len() > 30, "error message too short: {msg}");
        }
    }

    #[test]
    fn test_locked_schedule_uses_three_indices_via_idx_i() {
        // Pins the operator-locked 2026-05-16 schedule shape: 3 IDX_I
        // index underlyings staggered at :53, :56, :59. Changing any
        // of this requires updating both the test AND the rule file.
        let underlyings = locked_three_underlyings();
        assert_eq!(underlyings.len(), 3);
        for u in &underlyings {
            assert_eq!(u.segment, "IDX_I");
        }
        let slots: Vec<u32> = underlyings.iter().map(|u| u.slot_sec).collect();
        assert_eq!(slots, vec![53, 56, 59]);
    }
}

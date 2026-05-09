//! Shared exchange segment code → string mapping.
//!
//! I-P3-01: Deduplicated from tick_persistence.rs and candle_persistence.rs.
//! Single source of truth for the binary segment code → human-readable string mapping.

use crate::constants::{
    EXCHANGE_SEGMENT_BSE_CURRENCY, EXCHANGE_SEGMENT_BSE_EQ, EXCHANGE_SEGMENT_BSE_FNO,
    EXCHANGE_SEGMENT_IDX_I, EXCHANGE_SEGMENT_MCX_COMM, EXCHANGE_SEGMENT_NSE_CURRENCY,
    EXCHANGE_SEGMENT_NSE_EQ, EXCHANGE_SEGMENT_NSE_FNO,
};

/// Maps the binary exchange_segment_code to a human-readable symbol name.
///
/// Uses the same mapping as the Dhan API (Python SDK ref).
/// Note: code 6 is unused/skipped in Dhan's protocol.
pub fn segment_code_to_str(code: u8) -> &'static str {
    match code {
        EXCHANGE_SEGMENT_IDX_I => "IDX_I",
        EXCHANGE_SEGMENT_NSE_EQ => "NSE_EQ",
        EXCHANGE_SEGMENT_NSE_FNO => "NSE_FNO",
        EXCHANGE_SEGMENT_NSE_CURRENCY => "NSE_CURRENCY",
        EXCHANGE_SEGMENT_BSE_EQ => "BSE_EQ",
        EXCHANGE_SEGMENT_MCX_COMM => "MCX_COMM",
        EXCHANGE_SEGMENT_BSE_CURRENCY => "BSE_CURRENCY",
        EXCHANGE_SEGMENT_BSE_FNO => "BSE_FNO",
        _ => "UNKNOWN",
    }
}

/// Reverse of [`segment_code_to_str`] — maps a Dhan segment string to
/// its binary code. Returns `None` on unknown input. Used by F2 / cold-
/// path loaders that read the `previous_close` table (where segment is
/// stored as a string SYMBOL) and need to round-trip back to the
/// composite-key `(security_id, exchange_segment_code)` per I-P1-11.
///
/// Mirror of `segment_code_to_str` — every code that round-trips
/// `code → str → code` MUST be tested in `test_segment_str_to_code_round_trips_all_known_codes`.
#[must_use]
pub fn segment_str_to_code(s: &str) -> Option<u8> {
    match s {
        "IDX_I" => Some(EXCHANGE_SEGMENT_IDX_I),
        "NSE_EQ" => Some(EXCHANGE_SEGMENT_NSE_EQ),
        "NSE_FNO" => Some(EXCHANGE_SEGMENT_NSE_FNO),
        "NSE_CURRENCY" => Some(EXCHANGE_SEGMENT_NSE_CURRENCY),
        "BSE_EQ" => Some(EXCHANGE_SEGMENT_BSE_EQ),
        "MCX_COMM" => Some(EXCHANGE_SEGMENT_MCX_COMM),
        "BSE_CURRENCY" => Some(EXCHANGE_SEGMENT_BSE_CURRENCY),
        "BSE_FNO" => Some(EXCHANGE_SEGMENT_BSE_FNO),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_segment_code_to_str_known_codes() {
        assert_eq!(segment_code_to_str(EXCHANGE_SEGMENT_IDX_I), "IDX_I");
        assert_eq!(segment_code_to_str(EXCHANGE_SEGMENT_NSE_EQ), "NSE_EQ");
        assert_eq!(segment_code_to_str(EXCHANGE_SEGMENT_NSE_FNO), "NSE_FNO");
        assert_eq!(
            segment_code_to_str(EXCHANGE_SEGMENT_NSE_CURRENCY),
            "NSE_CURRENCY"
        );
        assert_eq!(segment_code_to_str(EXCHANGE_SEGMENT_BSE_EQ), "BSE_EQ");
        assert_eq!(segment_code_to_str(EXCHANGE_SEGMENT_MCX_COMM), "MCX_COMM");
        assert_eq!(
            segment_code_to_str(EXCHANGE_SEGMENT_BSE_CURRENCY),
            "BSE_CURRENCY"
        );
        assert_eq!(segment_code_to_str(EXCHANGE_SEGMENT_BSE_FNO), "BSE_FNO");
    }

    #[test]
    fn test_segment_code_to_str_unknown_code() {
        assert_eq!(segment_code_to_str(6), "UNKNOWN"); // Code 6 is unused in Dhan protocol
        assert_eq!(segment_code_to_str(9), "UNKNOWN");
        assert_eq!(segment_code_to_str(255), "UNKNOWN");
    }

    #[test]
    fn test_segment_str_to_code_round_trips_all_known_codes() {
        // F2 ratchet: every code that `segment_code_to_str` knows MUST
        // round-trip back through `segment_str_to_code`. Drift would
        // silently break the F2 PrevDayCache loader (which queries the
        // `previous_close` QuestDB table where segment is stored as
        // string and must round-trip to the binary code per I-P1-11).
        for code in 0..=255u8 {
            let label = segment_code_to_str(code);
            if label == "UNKNOWN" {
                assert!(
                    segment_str_to_code(label).is_none(),
                    "UNKNOWN must NOT round-trip"
                );
            } else {
                assert_eq!(
                    segment_str_to_code(label),
                    Some(code),
                    "round-trip failed for code {code} (label '{label}')"
                );
            }
        }
    }

    #[test]
    fn test_segment_str_to_code_unknown_returns_none() {
        assert!(segment_str_to_code("UNKNOWN").is_none());
        assert!(segment_str_to_code("").is_none());
        assert!(segment_str_to_code("nse_eq").is_none(), "case-sensitive");
        assert!(segment_str_to_code("ABCD").is_none());
    }

    #[test]
    fn test_segment_code_to_str_all_eight_known() {
        // Ensure exactly 8 known segments produce non-UNKNOWN results
        let known_count = (0..=255u8)
            .filter(|&code| segment_code_to_str(code) != "UNKNOWN")
            .count();
        assert_eq!(known_count, 8, "expected exactly 8 known segment codes");
    }
}

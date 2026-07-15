//! Trading-date string validation — the SURVIVOR of the deleted warm-boot
//! plan-snapshot module.
//!
//! PR-C3 (2026-07-14, operator retirement directive 2026-07-13 per
//! `websocket-connection-scope-lock.md` "2026-07-13 Amendment" §A/§B): the
//! §29 same-day warm-resubscribe plan-snapshot machinery
//! (`SnapshotTarget` / save / load / role parsing / the
//! `data/instrument-cache/plan-snapshot-YYYY-MM-DD.json` files) DIED with
//! the Dhan instrument-download chain — there is no subscription plan to
//! snapshot anymore. [`is_valid_trading_date`] is the scope-lock §B KEEP
//! item ("relocate to a surviving module if needed" — kept IN PLACE here so
//! the live consumers' import path
//! `instrument_snapshot::is_valid_trading_date` is unchanged): the strict
//! fail-closed `YYYY-MM-DD` path-traversal guard consumed by the Groww
//! activation watcher (`groww_activation.rs` ×2) and mirrored by the Groww
//! instruments reader + the scoreboard date validation.

/// Strictly validate a `YYYY-MM-DD` trading-date string.
///
/// This is the **path-traversal guard**: date strings are used to build
/// file paths, so they MUST contain nothing that could escape the target
/// directory (no `/`, no `.`, no `..`). We accept ONLY the exact shape
/// `dddd-dd-dd` of ASCII digits — anything else returns `false` and the
/// caller fails closed (no read, no write).
#[must_use]
pub fn is_valid_trading_date(date: &str) -> bool {
    let bytes = date.as_bytes();
    if bytes.len() != 10 {
        return false;
    }
    // Positions 4 and 7 must be '-'; all others must be ASCII digits.
    for (i, &b) in bytes.iter().enumerate() {
        let ok = if i == 4 || i == 7 {
            b == b'-'
        } else {
            b.is_ascii_digit()
        };
        if !ok {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_valid_trading_date_accepts_well_formed() {
        assert!(is_valid_trading_date("2026-05-29"));
        assert!(is_valid_trading_date("1999-12-31"));
    }

    #[test]
    fn test_is_valid_trading_date_rejects_traversal_and_malformed() {
        assert!(!is_valid_trading_date("../../etc/pass"));
        assert!(!is_valid_trading_date("2026/05/29"));
        assert!(!is_valid_trading_date("2026-05-29/x"));
        assert!(!is_valid_trading_date("2026-5-9"));
        assert!(!is_valid_trading_date(""));
        assert!(!is_valid_trading_date("2026-05-29 "));
        assert!(!is_valid_trading_date("20260529ab"));
    }
}

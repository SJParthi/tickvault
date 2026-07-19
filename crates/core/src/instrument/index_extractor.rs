//! Sub-PR #6 of 2026-05-27 daily-universe expansion — extract the
//! in-scope `IDX_I` index rows from the parsed Dhan Detailed CSV.
//!
//! **Operator lock 2026-06-01 (§30) + §31 item 1 (2026-06-06):** the NSE
//! side is now a FIXED 32-index allowlist (the Dhan Markets → Index → NSE
//! display set + NIFTY Total Market), NOT "all NSE index rows". The master
//! carries 119 NSE index rows; the rest (G-Sec, Shariah, leverage, ESG,
//! multicap variants) are dropped. BSE side stays "SENSEX only". GIFT Nifty
//! (`GIFTNIFTY`, sid 5024) is allowlisted AND flagged for the market-hours
//! exemption. NIFTY Total Market (§31) is the 32nd NSE entry — its exact Dhan
//! symbol self-verifies via `allowlist_misses` at boot.
//!
//! Dead-code batch 2 (2026-07-18): the Dhan-leg extraction half —
//! `extract_indices` + `IndexExtraction`/`IndexExtractError` and their
//! `CsvRow` fixtures — was DELETED (zero production callers since the
//! PR-C3 instrument-chain retirement; `csv_row` went with it). What
//! SURVIVES here is the allowlist + canonicalizer half the Groww watch
//! build and scoreboard consume.
//!
//! PR-C1 (2026-07-13): the module-level `daily_universe_fetcher` gate was
//! REMOVED — the Groww watch build consumes `NSE_INDEX_ALLOWLIST` +
//! `canonicalize_index_symbol` (scope-lock amendment §B KEEP items) and the
//! de-gated `index_futures` selector imports the canonicalizer, so this
//! module must build in every feature mode.

use tickvault_common::constants::INDEX_SYMBOL_ALIASES;

/// Fixed NSE index allowlist — operator lock 2026-06-01 (§30 of the rule
/// file). The Dhan master carries 119 NSE `IDX_I` index rows (G-Sec,
/// Shariah, leverage, ESG, multicap…); the operator wants ONLY the 31
/// indices shown on Dhan's Markets → Index → NSE page. Every string is
/// the EXACT `SYMBOL_NAME` verified 1-by-1 against the operator's real
/// master CSV (api-scrip-master-detailed.csv, 220,287 rows, 2026-06-01),
/// stored already-normalized (uppercase, single-spaced) so the match is
/// O(1)-ish over a 31-entry slice with no per-row allocation beyond the
/// normalized symbol. A non-allowlisted NSE index row is dropped.
///
/// `GIFTNIFTY` (sid 5024) is in this list AND additionally exempted from
/// the market-hours tick/candle window (it trades ~21h/day on NSE-IX) —
/// see the always-on exemption built from this extraction at boot.
pub const NSE_INDEX_ALLOWLIST: &[&str] = &[
    "NIFTY",
    "NIFTY NEXT 50",
    "BANKNIFTY",
    "FINNIFTY",
    "INDIA VIX",
    "NIFTYIT",
    "NIFTY AUTO",
    "NIFTY PHARMA",
    "NIFTY FMCG",
    "NIFTY METAL",
    "NIFTY MEDIA",
    "NIFTY 100",
    "NIFTY 200",
    "NIFTY 500",
    "GIFTNIFTY",
    "NIFTY PVT BANK",
    "NIFTY PSU BANK",
    "NIFTY REALTY",
    "NIFTY ENERGY",
    "NIFTYINFRA",
    "NIFTY MNC",
    "NIFTY CONSUMPTION",
    "NIFTY SERV SECTOR",
    "MIDCPNIFTY",
    "NIFTYMCAP50",
    "NIFTY MID100 FREE",
    "NIFTY MIDCAP 150",
    "NIFTY SMALLCAP 50",
    "NIFTY SMALLCAP 100",
    "NIFTY SMALLCAP 250",
    "NIFTY MICROCAP250",
    // §31 item 1 (operator 2026-06-06): NIFTY Total Market — the 33rd tracked
    // index value (32nd NSE entry). Dhan's exact IDX_I symbol is "NIFTY TOTAL MKT"
    // (abbreviated). The security_id is READ DYNAMICALLY from the matched live
    // master row — it is NEVER hardcoded here. The operator's live detailed
    // master shows the NSE IDX_I row secid = 443 (2026-06-06); an earlier "46"
    // came from the Dhan web-chart IDX-segment view and was never used as a
    // value. The `allowlist_misses` boot telemetry LOUD-warns if the live
    // master ever differs.
    "NIFTY TOTAL MKT",
];

/// The `SYMBOL_NAME` of GIFT Nifty in the Dhan master (sid 5024,
/// `EXCH=NSE SEGMENT=I INSTRUMENT=INDEX`). Used both as an allowlist
/// member above and to resolve the market-hours exemption SID at boot.
pub const GIFT_NIFTY_SYMBOL: &str = "GIFTNIFTY";

/// Normalize a symbol for allowlist comparison: collapse internal
/// whitespace to a single space, trim, uppercase. Cold-path (boot)
/// only — the one `String` allocation per index row is acceptable.
///
/// O(1) EXEMPT: runs once per CSV row at boot, not on the tick hot path.
#[must_use]
pub fn normalize_index_symbol(symbol: &str) -> String {
    symbol
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase()
}

/// Normalize a Dhan IDX_I row symbol, then resolve it through
/// [`INDEX_SYMBOL_ALIASES`] (alias → canonical) so symbols Dhan renamed map
/// back to their [`NSE_INDEX_ALLOWLIST`] form. Returns the canonical allowlist
/// string when an alias matches, otherwise the normalized symbol unchanged.
///
/// This is the SELF-HEAL for the 2026-06-26 live WARN
/// `index allowlist MISS ... NIFTY NEXT 50`: Dhan publishes that index as
/// `NIFTY NXT 50` / `NIFTYNXT50`, neither of which equals the allowlisted
/// `NIFTY NEXT 50` under an EXACT match. The alias map carries both forms →
/// canonical `NIFTY NEXT 50`, so the row is no longer silently dropped.
///
/// O(1) EXEMPT: cold-path boot only (once per CSV index row); the alias map is
/// a fixed small slice scanned linearly with no allocation beyond the one
/// `normalize_index_symbol` `String`.
#[must_use]
pub fn canonicalize_index_symbol(symbol: &str) -> String {
    let norm = normalize_index_symbol(symbol);
    for (alias, canonical) in INDEX_SYMBOL_ALIASES {
        if *alias == norm {
            return (*canonical).to_string();
        }
    }
    norm
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Operator lock 2026-06-01 §30 + §31 item 1 (NTM): 32-index allowlist ----

    #[test]
    fn allowlist_has_exactly_32_nse_indices() {
        // 31 (§30 lock) + NIFTY TOTAL MKT (§31 item 1, 2026-06-06) = 32.
        assert_eq!(NSE_INDEX_ALLOWLIST.len(), 32);
        // Dhan's exact IDX_I symbol is "NIFTY TOTAL MKT", NOT the long-form
        // "NIFTY TOTAL MARKET". The secid is read dynamically from the live
        // master (operator's live master = 443; "46" was the Dhan-chart IDX view).
        assert!(NSE_INDEX_ALLOWLIST.contains(&"NIFTY TOTAL MKT"));
        // Every entry must already be normalized (uppercase, single-spaced)
        // so the runtime match is a direct equality.
        for name in NSE_INDEX_ALLOWLIST {
            assert_eq!(*name, normalize_index_symbol(name), "{name} not normalized");
        }
        // GIFT Nifty must be in the allowlist (it's an NSE IDX_I index).
        assert!(NSE_INDEX_ALLOWLIST.contains(&GIFT_NIFTY_SYMBOL));
    }

    #[test]
    fn normalize_index_symbol_collapses_and_uppercases() {
        assert_eq!(normalize_index_symbol("nifty 50"), "NIFTY 50");
        assert_eq!(
            normalize_index_symbol("  NIFTY   MID100  FREE "),
            "NIFTY MID100 FREE"
        );
        assert_eq!(normalize_index_symbol("giftnifty"), "GIFTNIFTY");
    }

    #[test]
    fn canonicalize_index_symbol_resolves_aliases_and_passes_through() {
        // Alias hits → canonical allowlist form.
        assert_eq!(canonicalize_index_symbol("NIFTY NXT 50"), "NIFTY NEXT 50");
        assert_eq!(canonicalize_index_symbol("NIFTYNXT50"), "NIFTY NEXT 50");
        // Lowercase / extra-space variants normalize first, then alias-resolve.
        assert_eq!(canonicalize_index_symbol("nifty nxt 50"), "NIFTY NEXT 50");
        assert_eq!(canonicalize_index_symbol("  niftynxt50 "), "NIFTY NEXT 50");
        // No alias → normalized passthrough.
        assert_eq!(canonicalize_index_symbol("NIFTY"), "NIFTY");
        assert_eq!(canonicalize_index_symbol("nifty 50"), "NIFTY 50");
        // Already-canonical allowlist form is a no-op.
        assert_eq!(canonicalize_index_symbol("NIFTY NEXT 50"), "NIFTY NEXT 50");
    }
}

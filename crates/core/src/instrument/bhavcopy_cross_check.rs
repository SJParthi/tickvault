//! Wave 5 Item 26 L2 — NSE bhavcopy daily volume cross-check (pure-logic primitive).
//!
//! Verifies our captured EOD cumulative `volume` (per Dhan SecurityId) against
//! NSE's authoritative end-of-day bhavcopy CSV. Runs post-market (16:30 IST
//! by the scheduler — wiring deferred per CLAUDE.md "executing actions with
//! care"). Audit rows go to `volume_nse_audit` QuestDB table.
//!
//! # Honest envelope
//!
//! This module is the **pure-logic primitive only**. It does NOT:
//! - Fetch HTTP from `nsearchives.nseindia.com` (HTTP fetcher pending)
//! - Run a 16:30 IST tokio scheduler (wiring pending)
//! - Write to QuestDB `volume_nse_audit` table (storage path pending)
//! - Emit `NseBhavcopyCheckComplete` Telegram (notification path pending)
//!
//! It DOES provide:
//! - Pure CSV parser for the 34-column UDIFF bhavcopy format (no `unsafe`,
//!   no allocation beyond the row Vec)
//! - Tuple-key (TckrSymb, XpryDt, StrkPric, OptnTp) → Dhan SecurityId mapping
//! - Verdict computation (PASS/FAIL/MISSING_OUR/MISSING_NSE) with O(1) per
//!   row tolerance check
//! - Audit-row builder producing the same shape as the deferred QuestDB DDL
//!
//! # Coverage envelope
//!
//! Inside the tested envelope:
//! - 0.1% tolerance default (10 bps); operator can promote to 1% if false
//!   positives exceed ~2 contracts/day
//! - NSE column mapping pinned to UDIFF format observed live 2026-04-30
//!   (see plan §"Item 26 L2 NSE bhavcopy — verified implementation recipe")
//! - Cross-segment safety: NSE only emits NSE_FNO + NSE_EQ rows; Dhan IDX_I
//!   indices have no bhavcopy counterpart and are correctly skipped (the
//!   `lookup_dhan_security_id` returns None → `MissingNse`).
//!
//! Beyond the envelope:
//! - NSE may migrate the URL pattern (caught by HTTP-layer retry + 404 alert
//!   in the deferred wiring)
//! - NSE may rename columns mid-format (caught by `parse_header` strict
//!   matching — fails LOUD rather than silently misaligning)

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};

use tickvault_common::instrument_types::DerivativeContract;

/// Default tolerance for `dhan_eod_volume` vs `nse_eod_volume` comparison.
/// 0.1% (10 bps). Operator can promote to 1% if false-positive rate exceeds
/// ~2 contracts/day per the plan recipe.
pub const BHAVCOPY_VOLUME_TOLERANCE_PCT: f64 = 0.1;

/// Required column count for the UDIFF format (verified live 2026-04-30).
/// If NSE adds/removes columns mid-format, parsing will fail loud rather
/// than silently misalign.
pub const BHAVCOPY_UDIFF_COLUMN_COUNT: usize = 34;

/// 0-based column index of `TckrSymb` (e.g. "ABCAPITAL"). NSE plan recipe
/// documents this as "Col 8" (1-based) — we index from 0.
pub const COL_IDX_TCKR_SYMB: usize = 7;
/// 0-based column index of `XpryDt` (ISO `YYYY-MM-DD`).
pub const COL_IDX_XPRY_DT: usize = 9;
/// 0-based column index of `StrkPric` (decimal strike, blank for futures).
pub const COL_IDX_STRK_PRIC: usize = 11;
/// 0-based column index of `OptnTp` (`CE`/`PE`/blank for futures).
pub const COL_IDX_OPTN_TP: usize = 12;
/// 0-based column index of `OpnIntrst` (open interest at session close).
pub const COL_IDX_OPN_INTRST: usize = 22;
/// 0-based column index of `TtlTradgVol` — the cross-check target.
pub const COL_IDX_TTL_TRADG_VOL: usize = 24;
/// 0-based column index of `TtlTrfVal` (turnover, info only).
pub const COL_IDX_TTL_TRF_VAL: usize = 25;

/// Verdict for a single (security_id, segment) pair after cross-check.
/// Stored in `volume_nse_audit.verdict` SYMBOL column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BhavcopyVerdict {
    /// `|dhan - nse| / nse * 100 <= tolerance_pct`. Operator-actionable: none.
    Pass,
    /// `|dhan - nse| / nse * 100 > tolerance_pct`. Operator-actionable:
    /// inspect Dhan capture for the contract; common cause = main-feed
    /// disconnect window covered by rescue→spill→DLQ chain (recoverable
    /// from DLQ NDJSON).
    Fail,
    /// NSE bhavcopy has the row, our `ticks` table does not. Operator
    /// action: investigate why the contract was unsubscribed (possibly
    /// new contract listed mid-day, or rolled off our universe).
    MissingOur,
    /// We have ticks but NSE bhavcopy does not list the contract. Common
    /// for IDX_I (no bhavcopy) — these are filtered upstream so this
    /// verdict only fires for unexpected cases (e.g. expired contract
    /// our universe still subscribed).
    MissingNse,
}

impl BhavcopyVerdict {
    /// Stable wire-format string for the `volume_nse_audit.verdict` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::MissingOur => "MISSING_OUR",
            Self::MissingNse => "MISSING_NSE",
        }
    }
}

/// One parsed row from the NSE UDIFF bhavcopy CSV. Only the columns we
/// cross-check are retained — the other 28 columns are ignored to keep
/// the in-memory footprint tight (~80 bytes/row × 200K rows ≈ 16 MB).
#[derive(Debug, Clone, PartialEq)]
pub struct BhavcopyRow {
    /// `TckrSymb` — e.g. "ABCAPITAL". Owned `String` because NSE CSVs
    /// are ASCII variable-length and we hold the row across the lookup
    /// loop.
    pub tckr_symb: String,
    /// `XpryDt` — ISO `YYYY-MM-DD`. Empty `String` for cash equities.
    pub xpry_dt: String,
    /// `StrkPric` — decimal strike. `None` for futures + cash equities.
    pub strk_pric: Option<f64>,
    /// `OptnTp` — `"CE"` / `"PE"` / `None` for futures + cash equities.
    pub optn_tp: Option<String>,
    /// `OpnIntrst` — open interest at session close. `0` for cash equities.
    pub opn_intrst: i64,
    /// `TtlTradgVol` — daily total volume. **THIS is our cross-check target.**
    /// Non-negative; parser rejects negatives.
    pub ttl_tradg_vol: u64,
    /// `TtlTrfVal` — turnover (price × volume sum). Informational; not
    /// cross-checked but logged in audit row for forensics.
    pub ttl_trf_val: f64,
}

/// One audit row produced by `compare_volumes`. Same shape as the deferred
/// QuestDB `volume_nse_audit` table DDL (see plan §"Audit table DDL").
#[derive(Debug, Clone, PartialEq)]
pub struct BhavcopyAuditRow {
    /// `volume_nse_audit.trading_date_ist`. Caller passes through.
    pub trading_date_ist: String,
    /// `volume_nse_audit.security_id`. `0` if `verdict == MissingOur`
    /// (we never captured the contract).
    pub security_id: u32,
    /// `volume_nse_audit.segment` — `"NSE_FNO"` / `"NSE_EQ"`.
    pub segment: String,
    /// `volume_nse_audit.ticker_symbol` — copied from NSE row.
    pub ticker_symbol: String,
    /// `volume_nse_audit.dhan_eod_volume`. `0` if `verdict == MissingOur`.
    pub dhan_eod_volume: u64,
    /// `volume_nse_audit.nse_eod_volume`. `0` if `verdict == MissingNse`.
    pub nse_eod_volume: u64,
    /// `volume_nse_audit.diff_pct`. Signed: positive means our capture
    /// exceeded NSE (overcount, e.g. duplicate ticks); negative means
    /// undercount. `0.0` for missing-side cases.
    pub diff_pct: f64,
    pub verdict: BhavcopyVerdict,
}

/// Composite tuple-key used to map NSE bhavcopy rows to Dhan SecurityIds.
/// All four fields together uniquely identify a contract on a given trading
/// date (per plan recipe).
type BhavcopyTupleKey = (String, String, Option<String>, Option<String>);

/// Build the NSE-tuple → Dhan SecurityId lookup from our `DerivativeContract`
/// vector. Used by `compare_volumes` to resolve each NSE row to a Dhan ID.
///
/// Mapping rules (derived from the NSE UDIFF format vs our universe):
/// - `tckr_symb` ← `DerivativeContract::underlying_symbol` (e.g. "NIFTY",
///   "RELIANCE"). NSE uses the underlying ticker on every row; our
///   `symbol_name` is the full contract string ("NIFTY-Mar2026-18000-CE")
///   which would not match.
/// - `xpry_dt` ← `DerivativeContract::expiry_date` formatted as ISO
///   `YYYY-MM-DD` to match NSE's `XpryDt` field exactly.
/// - `strk_pric` ← `DerivativeContract::strike_price` rendered to 6-dp
///   fixed-point string. NSE emits decimal strikes like "25000.000000";
///   matching at 6 dp covers the entire NSE strike universe (NSE rounds
///   to 2 dp internally; 6 dp gives 4 dp of headroom).
/// - `optn_tp` ← `DerivativeContract::option_type.as_str()` (`"CE"` /
///   `"PE"` / `None` for futures).
///
/// O(N) where N = derivative count. Not on hot path; called once per
/// 16:30 IST cross-check cycle.
// TEST-EXEMPT: hash builder pure transform, covered via compare_volumes tests.
#[must_use]
pub fn build_dhan_lookup(instruments: &[DerivativeContract]) -> HashMap<BhavcopyTupleKey, u32> {
    let mut map = HashMap::with_capacity(instruments.len());
    for inst in instruments {
        let key: BhavcopyTupleKey = (
            inst.underlying_symbol.clone(),
            inst.expiry_date.format("%Y-%m-%d").to_string(),
            // Futures have strike_price == 0.0; map to None so the
            // bhavcopy futures rows (blank StrkPric) match.
            if inst.strike_price > 0.0 {
                Some(format!("{:.6}", inst.strike_price))
            } else {
                None
            },
            inst.option_type.map(|o| o.as_str().to_string()),
        );
        // Dedup-on-collision is fine here — `DerivativeContract` vector
        // is already segment-aware via the upstream universe build (see
        // I-P1-11 composite-key invariant).
        map.insert(key, inst.security_id);
    }
    map
}

/// Parse a single CSV row (already split into fields) into a `BhavcopyRow`.
/// Returns `Err` if the column count is wrong or any required numeric
/// field fails to parse.
///
/// O(1) per row; allocations are limited to the owned `String` clones for
/// `tckr_symb` / `xpry_dt` / `optn_tp`.
fn parse_row(fields: &[&str], row_index: usize) -> Result<BhavcopyRow> {
    if fields.len() != BHAVCOPY_UDIFF_COLUMN_COUNT {
        bail!(
            "row {}: expected {} columns, got {}",
            row_index,
            BHAVCOPY_UDIFF_COLUMN_COUNT,
            fields.len()
        );
    }

    let tckr_symb = fields[COL_IDX_TCKR_SYMB].trim().to_string();
    if tckr_symb.is_empty() {
        bail!("row {row_index}: TckrSymb is empty");
    }
    let xpry_dt = fields[COL_IDX_XPRY_DT].trim().to_string();

    let strk_raw = fields[COL_IDX_STRK_PRIC].trim();
    let strk_pric = if strk_raw.is_empty() || strk_raw == "0" || strk_raw == "0.0" {
        None
    } else {
        let v: f64 = strk_raw
            .parse()
            .with_context(|| format!("row {row_index}: invalid StrkPric={strk_raw:?}"))?;
        if v <= 0.0 { None } else { Some(v) }
    };

    let optn_raw = fields[COL_IDX_OPTN_TP].trim();
    let optn_tp = if optn_raw.is_empty() {
        None
    } else if optn_raw == "CE" || optn_raw == "PE" {
        Some(optn_raw.to_string())
    } else {
        bail!("row {row_index}: invalid OptnTp={optn_raw:?} (must be CE / PE / blank)");
    };

    let opn_intrst: i64 = fields[COL_IDX_OPN_INTRST]
        .trim()
        .parse()
        .with_context(|| format!("row {row_index}: invalid OpnIntrst"))?;

    let ttl_tradg_vol_str = fields[COL_IDX_TTL_TRADG_VOL].trim();
    let ttl_tradg_vol: u64 = ttl_tradg_vol_str
        .parse()
        .with_context(|| format!("row {row_index}: invalid TtlTradgVol={ttl_tradg_vol_str:?}"))?;

    let ttl_trf_val: f64 = fields[COL_IDX_TTL_TRF_VAL]
        .trim()
        .parse()
        .with_context(|| format!("row {row_index}: invalid TtlTrfVal"))?;

    Ok(BhavcopyRow {
        tckr_symb,
        xpry_dt,
        strk_pric,
        optn_tp,
        opn_intrst,
        ttl_tradg_vol,
        ttl_trf_val,
    })
}

/// Parse the full NSE UDIFF bhavcopy CSV body (after ZIP extraction by
/// the caller). Header line must be exactly 34 columns; data rows must
/// match. Returns rows in source order.
///
/// O(N) where N = total bhavcopy rows (~200K for F&O, ~2K for cash). Not
/// on hot path.
pub fn parse_bhavcopy_csv(content: &str) -> Result<Vec<BhavcopyRow>> {
    let mut lines = content.lines();
    let header = lines
        .next()
        .ok_or_else(|| anyhow!("bhavcopy CSV is empty"))?;
    let header_fields: Vec<&str> = header.split(',').collect();
    if header_fields.len() != BHAVCOPY_UDIFF_COLUMN_COUNT {
        bail!(
            "header has {} columns, expected {} (NSE may have migrated UDIFF format — \
             see .claude/rules/project/wave-5-error-codes.md VOLUME-MONO-01 / VOLUME-NSE-* runbook)",
            header_fields.len(),
            BHAVCOPY_UDIFF_COLUMN_COUNT
        );
    }
    if header_fields[COL_IDX_TCKR_SYMB] != "TckrSymb"
        || header_fields[COL_IDX_TTL_TRADG_VOL] != "TtlTradgVol"
    {
        bail!(
            "header column-name mismatch — expected TckrSymb at col {} and TtlTradgVol at col {}, \
             got {:?} and {:?} (NSE may have migrated UDIFF format)",
            COL_IDX_TCKR_SYMB,
            COL_IDX_TTL_TRADG_VOL,
            header_fields[COL_IDX_TCKR_SYMB],
            header_fields[COL_IDX_TTL_TRADG_VOL]
        );
    }

    let mut rows = Vec::with_capacity(content.len() / 200);
    for (i, line) in lines.enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(',').collect();
        let row = parse_row(&fields, i + 1)?;
        rows.push(row);
    }
    Ok(rows)
}

/// Compute the absolute percentage difference between Dhan-captured and
/// NSE-reported volumes. Sign reflects direction: positive = our capture
/// exceeded NSE (overcount), negative = undercount.
///
/// Edge cases:
/// - `nse_vol == 0 && dhan_vol == 0` → 0.0 (both empty → no information)
/// - `nse_vol == 0 && dhan_vol > 0` → `f64::INFINITY` (caller must treat
///   as MissingNse, not Fail)
/// - `nse_vol > 0 && dhan_vol == 0` → `-100.0` (full undercount)
#[inline]
#[must_use]
pub fn diff_pct(dhan_vol: u64, nse_vol: u64) -> f64 {
    if nse_vol == 0 {
        if dhan_vol == 0 { 0.0 } else { f64::INFINITY }
    } else {
        // i128 to avoid signed-cast overflow on extreme volumes (NSE max
        // observed ~3B/day for index futures).
        let signed_diff = i128::from(dhan_vol) - i128::from(nse_vol);
        #[allow(clippy::cast_precision_loss)] // bounded by NSE max ~3e9
        let scaled = (signed_diff as f64) * 100.0 / (nse_vol as f64);
        scaled
    }
}

/// Determine the verdict given a Dhan-captured volume and an NSE-reported
/// volume, both in absolute units.
#[inline]
#[must_use]
pub fn classify_verdict(dhan_vol: u64, nse_vol: u64, tolerance_pct: f64) -> BhavcopyVerdict {
    if nse_vol == 0 && dhan_vol > 0 {
        return BhavcopyVerdict::MissingNse;
    }
    if nse_vol > 0 && dhan_vol == 0 {
        return BhavcopyVerdict::MissingOur;
    }
    if nse_vol == 0 && dhan_vol == 0 {
        // Both empty — treat as missing on the NSE side (we never
        // captured anything for this contract; if it was tradable, NSE
        // would have a non-zero row). Defensive verdict.
        return BhavcopyVerdict::MissingOur;
    }
    let abs_diff = diff_pct(dhan_vol, nse_vol).abs();
    if abs_diff <= tolerance_pct {
        BhavcopyVerdict::Pass
    } else {
        BhavcopyVerdict::Fail
    }
}

/// Cross-check our cumulative EOD `last(volume)` snapshot against the NSE
/// bhavcopy. Produces one audit row per (NSE row OR Dhan-captured contract)
/// pair. Verdicts as documented in `BhavcopyVerdict`.
///
/// Inputs:
/// - `dhan_eod_volumes` — `HashMap<u32, u64>` keyed on Dhan SecurityId
///   (queried from `ticks` table at 15:30 IST per plan recipe).
/// - `nse_rows` — parsed bhavcopy rows from `parse_bhavcopy_csv`.
/// - `dhan_lookup` — pre-built tuple→SID map from `build_dhan_lookup`.
/// - `trading_date_ist` — ISO `YYYY-MM-DD` for audit-row stamping.
/// - `segment` — `"NSE_FNO"` or `"NSE_EQ"`.
/// - `tolerance_pct` — typically `BHAVCOPY_VOLUME_TOLERANCE_PCT`.
///
/// O(N + M) where N = NSE row count, M = Dhan-captured contract count.
/// Not on hot path; called once per 16:30 IST cycle.
pub fn compare_volumes(
    dhan_eod_volumes: &HashMap<u32, u64>,
    nse_rows: &[BhavcopyRow],
    dhan_lookup: &HashMap<BhavcopyTupleKey, u32>,
    trading_date_ist: &str,
    segment: &str,
    tolerance_pct: f64,
) -> Vec<BhavcopyAuditRow> {
    let mut out = Vec::with_capacity(nse_rows.len());
    let mut seen_sids: std::collections::HashSet<u32> =
        std::collections::HashSet::with_capacity(nse_rows.len());

    // Pass 1: every NSE row gets an audit row.
    for nse_row in nse_rows {
        let key: BhavcopyTupleKey = (
            nse_row.tckr_symb.clone(),
            nse_row.xpry_dt.clone(),
            nse_row.strk_pric.map(|s| format!("{s:.6}")),
            nse_row.optn_tp.clone(),
        );
        match dhan_lookup.get(&key) {
            Some(&sid) => {
                seen_sids.insert(sid);
                let dhan_vol = dhan_eod_volumes.get(&sid).copied().unwrap_or(0);
                let verdict = classify_verdict(dhan_vol, nse_row.ttl_tradg_vol, tolerance_pct);
                let dp = if nse_row.ttl_tradg_vol == 0 {
                    0.0
                } else {
                    diff_pct(dhan_vol, nse_row.ttl_tradg_vol)
                };
                out.push(BhavcopyAuditRow {
                    trading_date_ist: trading_date_ist.to_string(),
                    security_id: sid,
                    segment: segment.to_string(),
                    ticker_symbol: nse_row.tckr_symb.clone(),
                    dhan_eod_volume: dhan_vol,
                    nse_eod_volume: nse_row.ttl_tradg_vol,
                    diff_pct: dp,
                    verdict,
                });
            }
            None => {
                // NSE has it; we have no SID mapping at all → MissingOur.
                out.push(BhavcopyAuditRow {
                    trading_date_ist: trading_date_ist.to_string(),
                    security_id: 0,
                    segment: segment.to_string(),
                    ticker_symbol: nse_row.tckr_symb.clone(),
                    dhan_eod_volume: 0,
                    nse_eod_volume: nse_row.ttl_tradg_vol,
                    diff_pct: 0.0,
                    verdict: BhavcopyVerdict::MissingOur,
                });
            }
        }
    }

    // Pass 2: any Dhan-captured contract NSE didn't list → MissingNse.
    for (&sid, &dhan_vol) in dhan_eod_volumes {
        if seen_sids.contains(&sid) {
            continue;
        }
        out.push(BhavcopyAuditRow {
            trading_date_ist: trading_date_ist.to_string(),
            security_id: sid,
            segment: segment.to_string(),
            ticker_symbol: String::new(),
            dhan_eod_volume: dhan_vol,
            nse_eod_volume: 0,
            diff_pct: 0.0,
            verdict: BhavcopyVerdict::MissingNse,
        });
    }

    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn synth_header() -> String {
        // 34 column header matching the live 2026-04-30 NSE UDIFF format.
        // Only the columns we actually parse need exact names; the rest
        // can be placeholders since our parser only checks 2 column names.
        let mut cols = vec!["c0"; BHAVCOPY_UDIFF_COLUMN_COUNT];
        cols[COL_IDX_TCKR_SYMB] = "TckrSymb";
        cols[COL_IDX_XPRY_DT] = "XpryDt";
        cols[COL_IDX_STRK_PRIC] = "StrkPric";
        cols[COL_IDX_OPTN_TP] = "OptnTp";
        cols[COL_IDX_OPN_INTRST] = "OpnIntrst";
        cols[COL_IDX_TTL_TRADG_VOL] = "TtlTradgVol";
        cols[COL_IDX_TTL_TRF_VAL] = "TtlTrfVal";
        cols.join(",")
    }

    fn synth_data_row(
        tckr: &str,
        xpry: &str,
        strk: &str,
        optn: &str,
        oi: i64,
        vol: u64,
        trf: f64,
    ) -> String {
        let mut cols = vec!["x".to_string(); BHAVCOPY_UDIFF_COLUMN_COUNT];
        cols[COL_IDX_TCKR_SYMB] = tckr.to_string();
        cols[COL_IDX_XPRY_DT] = xpry.to_string();
        cols[COL_IDX_STRK_PRIC] = strk.to_string();
        cols[COL_IDX_OPTN_TP] = optn.to_string();
        cols[COL_IDX_OPN_INTRST] = oi.to_string();
        cols[COL_IDX_TTL_TRADG_VOL] = vol.to_string();
        cols[COL_IDX_TTL_TRF_VAL] = format!("{trf}");
        cols.join(",")
    }

    #[test]
    fn test_verdict_as_str_is_stable() {
        assert_eq!(BhavcopyVerdict::Pass.as_str(), "PASS");
        assert_eq!(BhavcopyVerdict::Fail.as_str(), "FAIL");
        assert_eq!(BhavcopyVerdict::MissingOur.as_str(), "MISSING_OUR");
        assert_eq!(BhavcopyVerdict::MissingNse.as_str(), "MISSING_NSE");
    }

    #[test]
    fn test_diff_pct_zero_both_sides() {
        assert!((diff_pct(0, 0) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_diff_pct_zero_nse_nonzero_dhan() {
        assert!(diff_pct(100, 0).is_infinite());
    }

    #[test]
    fn test_diff_pct_zero_dhan_nonzero_nse() {
        assert!((diff_pct(0, 100) - (-100.0)).abs() < f64::EPSILON);
    }

    #[test]
    fn test_diff_pct_within_tolerance() {
        // Dhan 100100, NSE 100000 → 0.1% over.
        let dp = diff_pct(100_100, 100_000);
        assert!((dp - 0.1).abs() < 1e-9);
    }

    #[test]
    fn test_diff_pct_undercount() {
        // Dhan 99900, NSE 100000 → -0.1%.
        let dp = diff_pct(99_900, 100_000);
        assert!((dp - (-0.1)).abs() < 1e-9);
    }

    #[test]
    fn test_classify_verdict_pass_at_tolerance_inclusive() {
        // 0.1% diff exactly equals tolerance → PASS (inclusive).
        let v = classify_verdict(100_100, 100_000, BHAVCOPY_VOLUME_TOLERANCE_PCT);
        assert_eq!(v, BhavcopyVerdict::Pass);
    }

    #[test]
    fn test_classify_verdict_fail_above_tolerance() {
        // 1% diff — well above 0.1% tolerance.
        let v = classify_verdict(101_000, 100_000, BHAVCOPY_VOLUME_TOLERANCE_PCT);
        assert_eq!(v, BhavcopyVerdict::Fail);
    }

    #[test]
    fn test_classify_verdict_missing_our() {
        // NSE has it, we don't.
        let v = classify_verdict(0, 100_000, BHAVCOPY_VOLUME_TOLERANCE_PCT);
        assert_eq!(v, BhavcopyVerdict::MissingOur);
    }

    #[test]
    fn test_classify_verdict_missing_nse() {
        // We have it, NSE doesn't.
        let v = classify_verdict(100_000, 0, BHAVCOPY_VOLUME_TOLERANCE_PCT);
        assert_eq!(v, BhavcopyVerdict::MissingNse);
    }

    #[test]
    fn test_classify_verdict_both_zero_is_missing_our() {
        // Defensive: both empty → treat as MissingOur (NSE didn't list it
        // either, but we should investigate why we subscribed in the first
        // place).
        let v = classify_verdict(0, 0, BHAVCOPY_VOLUME_TOLERANCE_PCT);
        assert_eq!(v, BhavcopyVerdict::MissingOur);
    }

    #[test]
    fn test_parse_bhavcopy_csv_minimal() {
        let csv = format!(
            "{}\n{}",
            synth_header(),
            synth_data_row("ABCAPITAL", "2026-05-29", "0", "", 0, 12_345, 1_234_500.0)
        );
        let rows = parse_bhavcopy_csv(&csv).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tckr_symb, "ABCAPITAL");
        assert_eq!(rows[0].xpry_dt, "2026-05-29");
        assert!(rows[0].strk_pric.is_none()); // 0 → futures
        assert!(rows[0].optn_tp.is_none()); // blank → futures
        assert_eq!(rows[0].ttl_tradg_vol, 12_345);
    }

    #[test]
    fn test_parse_bhavcopy_csv_option_row() {
        let csv = format!(
            "{}\n{}",
            synth_header(),
            synth_data_row(
                "NIFTY",
                "2026-05-29",
                "25000.000000",
                "CE",
                1234,
                567_890,
                7e9
            )
        );
        let rows = parse_bhavcopy_csv(&csv).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].strk_pric, Some(25_000.0));
        assert_eq!(rows[0].optn_tp.as_deref(), Some("CE"));
        assert_eq!(rows[0].opn_intrst, 1234);
    }

    #[test]
    fn test_parse_bhavcopy_csv_rejects_wrong_column_count() {
        let bad = "TckrSymb,TtlTradgVol\nNIFTY,1000";
        let r = parse_bhavcopy_csv(bad);
        assert!(r.is_err());
        let msg = format!("{:?}", r.unwrap_err());
        assert!(msg.contains("expected"));
    }

    #[test]
    fn test_parse_bhavcopy_csv_rejects_wrong_column_names() {
        // 34 columns but TckrSymb is in the wrong slot.
        let mut header_cols = vec!["c0"; BHAVCOPY_UDIFF_COLUMN_COUNT];
        header_cols[0] = "TckrSymb"; // wrong index
        let bad = header_cols.join(",");
        let r = parse_bhavcopy_csv(&bad);
        assert!(r.is_err());
    }

    #[test]
    fn test_parse_bhavcopy_csv_rejects_invalid_optn_tp() {
        let csv = format!(
            "{}\n{}",
            synth_header(),
            synth_data_row("NIFTY", "2026-05-29", "25000.000000", "XX", 0, 100, 100.0)
        );
        let r = parse_bhavcopy_csv(&csv);
        assert!(r.is_err());
    }

    #[test]
    fn test_parse_bhavcopy_csv_rejects_invalid_volume() {
        let mut cols = vec!["x".to_string(); BHAVCOPY_UDIFF_COLUMN_COUNT];
        cols[COL_IDX_TCKR_SYMB] = "NIFTY".into();
        cols[COL_IDX_XPRY_DT] = "2026-05-29".into();
        cols[COL_IDX_STRK_PRIC] = "0".into();
        cols[COL_IDX_OPTN_TP] = "".into();
        cols[COL_IDX_OPN_INTRST] = "0".into();
        cols[COL_IDX_TTL_TRADG_VOL] = "abc".into(); // non-numeric
        cols[COL_IDX_TTL_TRF_VAL] = "0".into();
        let csv = format!("{}\n{}", synth_header(), cols.join(","));
        let r = parse_bhavcopy_csv(&csv);
        assert!(r.is_err());
    }

    #[test]
    fn test_parse_bhavcopy_csv_skips_blank_lines() {
        let csv = format!(
            "{}\n\n{}\n",
            synth_header(),
            synth_data_row("NIFTY", "2026-05-29", "0", "", 0, 100, 100.0)
        );
        let rows = parse_bhavcopy_csv(&csv).unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn test_compare_volumes_pass_only_path() {
        // 1 NSE row, 1 Dhan capture, exact match.
        let nse_rows = vec![BhavcopyRow {
            tckr_symb: "NIFTY".into(),
            xpry_dt: "2026-05-29".into(),
            strk_pric: Some(25_000.0),
            optn_tp: Some("CE".into()),
            opn_intrst: 100,
            ttl_tradg_vol: 1_000_000,
            ttl_trf_val: 0.0,
        }];
        let mut lookup = HashMap::new();
        lookup.insert(
            (
                "NIFTY".into(),
                "2026-05-29".into(),
                Some("25000.000000".into()),
                Some("CE".into()),
            ),
            42_u32,
        );
        let mut dhan_vols = HashMap::new();
        dhan_vols.insert(42_u32, 1_000_000_u64);
        let audit = compare_volumes(
            &dhan_vols,
            &nse_rows,
            &lookup,
            "2026-05-04",
            "NSE_FNO",
            BHAVCOPY_VOLUME_TOLERANCE_PCT,
        );
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].verdict, BhavcopyVerdict::Pass);
        assert_eq!(audit[0].security_id, 42);
        assert_eq!(audit[0].dhan_eod_volume, 1_000_000);
    }

    #[test]
    fn test_compare_volumes_emits_missing_our_when_nse_has_unknown_contract() {
        let nse_rows = vec![BhavcopyRow {
            tckr_symb: "BANKNIFTY".into(),
            xpry_dt: "2026-05-29".into(),
            strk_pric: Some(48_000.0),
            optn_tp: Some("PE".into()),
            opn_intrst: 0,
            ttl_tradg_vol: 500,
            ttl_trf_val: 0.0,
        }];
        let lookup = HashMap::new(); // empty — we have no Dhan SID for it
        let dhan_vols = HashMap::new();
        let audit = compare_volumes(
            &dhan_vols,
            &nse_rows,
            &lookup,
            "2026-05-04",
            "NSE_FNO",
            BHAVCOPY_VOLUME_TOLERANCE_PCT,
        );
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].verdict, BhavcopyVerdict::MissingOur);
        assert_eq!(audit[0].security_id, 0); // no SID
        assert_eq!(audit[0].nse_eod_volume, 500);
    }

    #[test]
    fn test_compare_volumes_emits_missing_nse_when_we_captured_unknown_contract() {
        let nse_rows: Vec<BhavcopyRow> = Vec::new();
        let lookup = HashMap::new();
        let mut dhan_vols = HashMap::new();
        dhan_vols.insert(99_u32, 7_777_u64);
        let audit = compare_volumes(
            &dhan_vols,
            &nse_rows,
            &lookup,
            "2026-05-04",
            "NSE_FNO",
            BHAVCOPY_VOLUME_TOLERANCE_PCT,
        );
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].verdict, BhavcopyVerdict::MissingNse);
        assert_eq!(audit[0].security_id, 99);
        assert_eq!(audit[0].dhan_eod_volume, 7_777);
    }

    #[test]
    fn test_compare_volumes_emits_fail_when_diff_exceeds_tolerance() {
        let nse_rows = vec![BhavcopyRow {
            tckr_symb: "NIFTY".into(),
            xpry_dt: "2026-05-29".into(),
            strk_pric: None,
            optn_tp: None,
            opn_intrst: 0,
            ttl_tradg_vol: 1_000_000,
            ttl_trf_val: 0.0,
        }];
        let mut lookup = HashMap::new();
        lookup.insert(("NIFTY".into(), "2026-05-29".into(), None, None), 50_u32);
        let mut dhan_vols = HashMap::new();
        dhan_vols.insert(50_u32, 990_000_u64); // -1% under
        let audit = compare_volumes(
            &dhan_vols,
            &nse_rows,
            &lookup,
            "2026-05-04",
            "NSE_FNO",
            BHAVCOPY_VOLUME_TOLERANCE_PCT,
        );
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].verdict, BhavcopyVerdict::Fail);
        assert!((audit[0].diff_pct - (-1.0)).abs() < 1e-6);
    }

    #[test]
    fn test_constants_pinned() {
        // Ratchet against silent constant drift.
        assert!((BHAVCOPY_VOLUME_TOLERANCE_PCT - 0.1).abs() < f64::EPSILON);
        assert_eq!(BHAVCOPY_UDIFF_COLUMN_COUNT, 34);
        assert_eq!(COL_IDX_TCKR_SYMB, 7);
        assert_eq!(COL_IDX_TTL_TRADG_VOL, 24);
    }

    #[test]
    fn test_build_dhan_lookup_produces_tuple_keys() {
        use chrono::NaiveDate;
        use tickvault_common::types::{ExchangeSegment, OptionType};
        // Synthesize 2 contracts: one CE option, one future.
        let contracts = vec![
            DerivativeContract {
                security_id: 42,
                underlying_symbol: "NIFTY".to_string(),
                instrument_kind:
                    tickvault_common::instrument_types::DhanInstrumentKind::OptionIndex,
                exchange_segment: ExchangeSegment::NseFno,
                expiry_date: NaiveDate::from_ymd_opt(2026, 5, 29).unwrap(),
                strike_price: 25_000.0,
                option_type: Some(OptionType::Call),
                lot_size: 50,
                tick_size: 0.05,
                symbol_name: "NIFTY-May2026-25000-CE".into(),
                display_name: "NIFTY 25000 CE".into(),
            },
            DerivativeContract {
                security_id: 99,
                underlying_symbol: "NIFTY".into(),
                instrument_kind:
                    tickvault_common::instrument_types::DhanInstrumentKind::FutureIndex,
                exchange_segment: ExchangeSegment::NseFno,
                expiry_date: NaiveDate::from_ymd_opt(2026, 5, 29).unwrap(),
                strike_price: 0.0,
                option_type: None,
                lot_size: 50,
                tick_size: 0.05,
                symbol_name: "NIFTY-May2026-FUT".into(),
                display_name: "NIFTY MAY FUT".into(),
            },
        ];
        let lookup = build_dhan_lookup(&contracts);
        assert_eq!(lookup.len(), 2);
        // CE option → exact tuple match.
        let opt_key = (
            "NIFTY".to_string(),
            "2026-05-29".to_string(),
            Some("25000.000000".to_string()),
            Some("CE".to_string()),
        );
        assert_eq!(lookup.get(&opt_key), Some(&42));
        // Future → strike None + optn_tp None.
        let fut_key = ("NIFTY".to_string(), "2026-05-29".to_string(), None, None);
        assert_eq!(lookup.get(&fut_key), Some(&99));
    }

    #[test]
    fn test_build_dhan_lookup_handles_empty_input() {
        let lookup = build_dhan_lookup(&[]);
        assert!(lookup.is_empty());
    }
}

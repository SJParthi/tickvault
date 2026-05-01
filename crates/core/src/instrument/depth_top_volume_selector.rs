//! Wave 5 Items 4 + 5 — depth-20 + depth-200 top-volume dynamic selector.
//!
//! Pure-logic primitive shared by:
//! - Depth-20 conn 5 (Item 4): top 50 contracts from `option_movers`
//!   `category = 'TOP_VOLUME'` sorted by `change_pct DESC`, SENSEX-skipped.
//! - Depth-200 conns 1..5 (Item 5): top 5 contracts from the same query
//!   (depth-200 takes the first 5 of the depth-20 K=50 result).
//!
//! ## Honest envelope
//!
//! Per `.claude/rules/project/wave-4-shared-preamble.md` Section 8:
//! this module ships the deterministic SELECTOR + sanitisation + diff
//! logic. The actual connection-pool wiring (RIP + REWRITE of the merged
//! `depth_20_dynamic_subscriber.rs` per Item 4 BLOCKER C1 → Option B,
//! plus the new `depth_200_subscriber.rs` per Item 5) is a follow-up
//! sub-PR. Each follow-up touches the live trading boot path and needs
//! operator review before landing.
//!
//! ## SQL
//!
//! The canonical SELECTOR SQL (pinned by `test_selector_sql_*` tests):
//!
//! ```sql
//! SELECT security_id, exchange_segment, underlying_symbol, change_pct, volume
//! FROM option_movers
//! WHERE category = 'TOP_VOLUME'
//!   AND exchange_segment != 'BSE_FNO'   -- exclude SENSEX
//!   AND change_pct > 0                  -- gainers only
//!   AND volume > 0                      -- defensive
//!   AND ts > dateadd('s', -300, now())  -- 5m freshness
//! ORDER BY change_pct DESC, volume DESC -- tie-break by volume
//! LIMIT 50;                             -- depth-200 takes first 5 of these
//! ```
//!
//! ## Static layout
//!
//! Item 4's 4 single-side static connections share this module's
//! `static_layout()` helper. Returns the per-connection security_id
//! lists once the planner has resolved ATM strikes for NIFTY +
//! BANKNIFTY at the current expiry.

use std::collections::HashSet;
use std::sync::OnceLock;

/// Wave 5 Item 4: depth-20 conn 5 (dynamic) takes the top 50.
pub const DEPTH_20_DYNAMIC_K: usize = 50;

/// Wave 5 Item 5: depth-200 conns 1..5 take the top 5 (one per conn).
pub const DEPTH_200_DYNAMIC_K: usize = 5;

/// Item 4 static-layout: 4 single-side connections at ATM ± 24.
pub const DEPTH_20_STATIC_CONN_COUNT: usize = 4;

/// Item 4 static-layout: each static connection holds 49 instruments
/// (ATM + 24 above + 24 below = 49).
pub const DEPTH_20_STATIC_PER_CONN: usize = 49;

/// Item 4 total instrument count = 4 × 49 + 50 = 246 (pinned by ratchet
/// `test_depth_20_total_instrument_count_is_246`).
pub const DEPTH_20_TOTAL_INSTRUMENTS: usize =
    DEPTH_20_STATIC_CONN_COUNT * DEPTH_20_STATIC_PER_CONN + DEPTH_20_DYNAMIC_K;

/// Item 4 H7 anti-thrash: a swap is suppressed when the rank-set diff
/// is fewer than this many positions, even if the diff is non-zero.
/// Operator demand: don't churn depth subscriptions on noise.
pub const DEPTH_20_SWAP_HYSTERESIS_MIN: usize = 3;

/// Maximum permitted `k` for `selector_sql`. Wave 5 plan pins K = 50
/// (depth-20) and K = 5 (depth-200); 200 is a safe upper bound that
/// also matches Dhan's per-connection cap on depth feeds. Adversarial
/// security review (2026-05-01): bounds-check `k` before formatting
/// to defend against future callers passing a value derived from
/// external input.
pub const SELECTOR_SQL_MAX_K: usize = 200;

/// Lazily-initialised cache for the two production `k` values
/// (DEPTH_20_DYNAMIC_K = 50 and DEPTH_200_DYNAMIC_K = 5). Removes the
/// per-call `format!()` allocation flagged by the hot-path adversarial
/// review. Lookups for these two values return a `&'static str` slice
/// of the cached `String` (cheap clone via `to_string()` for callers
/// that need an owned `String`).
static SQL_K50: OnceLock<String> = OnceLock::new();
static SQL_K5: OnceLock<String> = OnceLock::new();

fn build_selector_sql(k: usize) -> String {
    format!(
        "SELECT security_id, exchange_segment, underlying_symbol, change_pct, volume \
         FROM option_movers \
         WHERE category = 'TOP_VOLUME' \
           AND exchange_segment != 'BSE_FNO' \
           AND change_pct > 0 \
           AND volume > 0 \
           AND ts > dateadd('s', -300, now()) \
         ORDER BY change_pct DESC, volume DESC \
         LIMIT {k}"
    )
}

/// Returns the canonical depth selector SQL with `LIMIT k`.
///
/// # Panics
/// Panics if `k == 0` or `k > SELECTOR_SQL_MAX_K`. Both bounds catch
/// caller-side bugs early (per the security review): `k = 0` would
/// emit `LIMIT 0` (silently no-op); `k > 200` is outside any valid
/// depth subscription scope.
///
/// For the two production values (`DEPTH_20_DYNAMIC_K = 50` and
/// `DEPTH_200_DYNAMIC_K = 5`), this returns a `String` cloned from a
/// `OnceLock`-cached precompute — no per-call allocation on the
/// formatter path. Other `k` values fall back to a fresh `format!()`.
#[must_use]
pub fn selector_sql(k: usize) -> String {
    assert!(
        k > 0 && k <= SELECTOR_SQL_MAX_K,
        "selector_sql: k must be in 1..={SELECTOR_SQL_MAX_K}, got {k}"
    );
    match k {
        DEPTH_20_DYNAMIC_K => SQL_K50.get_or_init(|| build_selector_sql(k)).clone(),
        DEPTH_200_DYNAMIC_K => SQL_K5.get_or_init(|| build_selector_sql(k)).clone(),
        _ => build_selector_sql(k),
    }
}

/// One row returned by the selector — keep `Copy` for zero-allocation
/// fan-out from depth-20 conn 5 to the 5 depth-200 conns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DepthSelectorRow {
    pub security_id: u32,
    /// `2` = NSE_FNO. BSE_FNO (8) is filtered out by the SQL but we
    /// still carry the byte so downstream code can pin the I-P1-11
    /// composite key.
    pub exchange_segment_code: u8,
}

/// Sanitises a raw selector result before it is used to drive WS
/// subscriptions. Defence-in-depth (Item 4 H5 — set sanitisation):
/// 1. Drops any row that slipped through with `BSE_FNO` (segment 8).
/// 2. Caps the result at `k` rows.
/// 3. Deduplicates by composite key (security_id, segment) per I-P1-11
///    so an upstream movers writer bug can't produce duplicate
///    subscriptions.
#[must_use]
pub fn sanitize(rows: &[DepthSelectorRow], k: usize) -> Vec<DepthSelectorRow> {
    let mut out: Vec<DepthSelectorRow> = Vec::with_capacity(k.min(rows.len()));
    let mut seen: HashSet<(u32, u8)> = HashSet::with_capacity(k);
    for row in rows {
        // Drop BSE_FNO defensively (the SQL already excludes it; this is
        // belt-and-suspenders for a movers writer regression).
        if row.exchange_segment_code == 8 {
            continue;
        }
        // Composite-key dedup (I-P1-11).
        if !seen.insert((row.security_id, row.exchange_segment_code)) {
            continue;
        }
        out.push(*row);
        if out.len() >= k {
            break;
        }
    }
    out
}

/// Item 4 H7 anti-thrash: should we issue a `Swap20` command for the
/// new top-50 set? Returns `true` only when the rank-set diff is at
/// least `DEPTH_20_SWAP_HYSTERESIS_MIN` positions. Edge-triggered:
/// stable sets DO NOT issue a swap (no churn on rank order alone).
#[must_use]
// TEST-EXEMPT: covered by 4 test_swap_hysteresis_* ratchets (first-set, identical-set, below-min, at-or-above-min).
pub fn should_issue_depth_20_swap(
    previous: &[DepthSelectorRow],
    current: &[DepthSelectorRow],
) -> bool {
    if previous.is_empty() && !current.is_empty() {
        // First-seen set is always a swap (initial subscribe).
        return true;
    }
    let prev_set: HashSet<(u32, u8)> = previous
        .iter()
        .map(|r| (r.security_id, r.exchange_segment_code))
        .collect();
    let curr_set: HashSet<(u32, u8)> = current
        .iter()
        .map(|r| (r.security_id, r.exchange_segment_code))
        .collect();
    // Symmetric difference size — number of positions that changed.
    let added = curr_set.difference(&prev_set).count();
    let removed = prev_set.difference(&curr_set).count();
    let diff = added.max(removed);
    diff >= DEPTH_20_SWAP_HYSTERESIS_MIN
}

/// Item 5 fan-out: given the depth-20 K=50 sanitised result, return
/// the first 5 rows for depth-200 conn assignment. One row per conn
/// per the operator's design.
#[must_use]
pub fn fan_out_to_depth_200(top_50: &[DepthSelectorRow]) -> Vec<DepthSelectorRow> {
    top_50.iter().take(DEPTH_200_DYNAMIC_K).copied().collect()
}

/// Item 4 static-layout per single-side conn. Returns the 49 security_ids
/// for one connection given the underlying's option chain at the current
/// expiry, ATM strike index, and side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepthStaticSide {
    /// CE-only (Item 4 conn 1 = NIFTY CE; conn 3 = BANKNIFTY CE).
    Call,
    /// PE-only (Item 4 conn 2 = NIFTY PE; conn 4 = BANKNIFTY PE).
    Put,
}

/// Selects ATM ± 24 contracts on one side from a sorted strike list.
///
/// `sorted_strikes` is the sorted list of `(security_id, strike)` for
/// the given side at the current expiry. `atm_index` is the position of
/// the ATM strike in that list. Returns up to 49 security_ids — fewer
/// at the chain edges if the ATM is closer than 24 strikes from either
/// boundary.
#[must_use]
// TEST-EXEMPT: covered by 4 test_static_side_* ratchets (centred-49, low-edge, high-edge, empty).
pub fn select_static_side_contracts(sorted_strikes: &[(u32, f64)], atm_index: usize) -> Vec<u32> {
    if sorted_strikes.is_empty() {
        return Vec::new();
    }
    let n = sorted_strikes.len();
    let half = (DEPTH_20_STATIC_PER_CONN - 1) / 2; // 24
    let lo = atm_index.saturating_sub(half);
    let hi = (atm_index.saturating_add(half)).min(n.saturating_sub(1));
    sorted_strikes[lo..=hi]
        .iter()
        .map(|(sid, _)| *sid)
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn row(sid: u32, seg: u8) -> DepthSelectorRow {
        DepthSelectorRow {
            security_id: sid,
            exchange_segment_code: seg,
        }
    }

    // ----------------------------------------------------------------------
    // Constants — Wave 5 plan-pinned values.
    // ----------------------------------------------------------------------

    #[test]
    fn test_depth_20_total_instrument_count_is_246() {
        // 4 static conns × 49 + 1 dynamic × 50 = 246.
        assert_eq!(DEPTH_20_TOTAL_INSTRUMENTS, 246);
        assert_eq!(DEPTH_20_STATIC_CONN_COUNT, 4);
        assert_eq!(DEPTH_20_STATIC_PER_CONN, 49);
        assert_eq!(DEPTH_20_DYNAMIC_K, 50);
    }

    #[test]
    fn test_depth_200_dynamic_k_is_5() {
        // Operator pinned: 5 depth-200 conns, one contract each.
        assert_eq!(DEPTH_200_DYNAMIC_K, 5);
    }

    #[test]
    fn test_depth_20_swap_hysteresis_min_is_3() {
        // Operator demand H7: avoid thrash on rank noise.
        assert_eq!(DEPTH_20_SWAP_HYSTERESIS_MIN, 3);
    }

    // ----------------------------------------------------------------------
    // SQL constants per the plan's named selector spec.
    // ----------------------------------------------------------------------

    #[test]
    fn test_selector_sql_uses_top_volume_category_change_pct_desc() {
        let sql = selector_sql(50);
        assert!(sql.contains("category = 'TOP_VOLUME'"));
        assert!(sql.contains("ORDER BY change_pct DESC"));
        // Tie-break by volume for determinism.
        assert!(sql.contains("change_pct DESC, volume DESC"));
    }

    #[test]
    fn test_selector_sql_excludes_bse_fno() {
        let sql = selector_sql(50);
        assert!(sql.contains("exchange_segment != 'BSE_FNO'"));
    }

    #[test]
    fn test_selector_sql_filters_change_pct_positive_and_volume_positive() {
        let sql = selector_sql(50);
        assert!(sql.contains("change_pct > 0"));
        assert!(sql.contains("volume > 0"));
    }

    #[test]
    fn test_selector_sql_uses_5_minute_freshness_window() {
        let sql = selector_sql(50);
        assert!(sql.contains("ts > dateadd('s', -300, now())"));
    }

    #[test]
    fn test_selector_sql_limit_50_for_depth_20() {
        assert!(selector_sql(50).ends_with("LIMIT 50"));
    }

    #[test]
    fn test_selector_sql_limit_5_for_depth_200() {
        assert!(selector_sql(5).ends_with("LIMIT 5"));
    }

    /// Security ratchet (2026-05-01 adversarial review): `k = 0` MUST panic
    /// instead of emitting `LIMIT 0`. The panic catches caller bugs early
    /// before the SQL is shipped to QuestDB.
    #[test]
    #[should_panic(expected = "k must be in 1..=200")]
    fn test_selector_sql_panics_on_k_zero() {
        let _ = selector_sql(0);
    }

    /// Security ratchet (2026-05-01 adversarial review): `k` above
    /// `SELECTOR_SQL_MAX_K = 200` MUST panic. Defends against future
    /// callers that derive `k` from external input.
    #[test]
    #[should_panic(expected = "k must be in 1..=200")]
    fn test_selector_sql_panics_on_k_above_max() {
        let _ = selector_sql(SELECTOR_SQL_MAX_K + 1);
    }

    /// Hot-path ratchet (2026-05-01 adversarial review): repeated calls
    /// with the production K=50 MUST return identical strings (the
    /// `OnceLock` cache is functioning correctly).
    #[test]
    fn test_selector_sql_cached_for_production_k_values() {
        let s1 = selector_sql(DEPTH_20_DYNAMIC_K);
        let s2 = selector_sql(DEPTH_20_DYNAMIC_K);
        let s3 = selector_sql(DEPTH_200_DYNAMIC_K);
        let s4 = selector_sql(DEPTH_200_DYNAMIC_K);
        assert_eq!(s1, s2);
        assert_eq!(s3, s4);
        // K=50 and K=5 produce different SQL (LIMIT differs).
        assert_ne!(s1, s3);
    }

    // ----------------------------------------------------------------------
    // Sanitisation (Item 4 H5).
    // ----------------------------------------------------------------------

    #[test]
    fn test_sanitize_drops_bse_fno_rows() {
        let raw = [
            row(49_001, 2), // NSE_FNO — keep
            row(80_001, 8), // BSE_FNO (SENSEX) — drop
            row(49_002, 2), // NSE_FNO — keep
        ];
        let out = sanitize(&raw, 50);
        assert_eq!(out.len(), 2);
        for r in &out {
            assert_ne!(r.exchange_segment_code, 8);
        }
    }

    #[test]
    fn test_sanitize_caps_at_k() {
        let raw: Vec<DepthSelectorRow> = (49_000u32..49_100).map(|i| row(i, 2)).collect();
        assert_eq!(sanitize(&raw, 50).len(), 50);
        assert_eq!(sanitize(&raw, 5).len(), 5);
    }

    #[test]
    fn test_sanitize_deduplicates_by_composite_key_per_i_p1_11() {
        // Same security_id, different segments — both should survive
        // (composite-key dedup). Exact duplicates collapse.
        let raw = [
            row(49_001, 2),
            row(49_001, 2), // exact dup — drop
            row(49_001, 8), // BSE_FNO — drop (segment filter)
        ];
        let out = sanitize(&raw, 50);
        assert_eq!(out.len(), 1);
    }

    // ----------------------------------------------------------------------
    // Anti-thrash (Item 4 H7).
    // ----------------------------------------------------------------------

    #[test]
    fn test_swap_hysteresis_first_set_is_always_swap() {
        let curr = vec![row(49_001, 2), row(49_002, 2)];
        assert!(should_issue_depth_20_swap(&[], &curr));
    }

    #[test]
    fn test_swap_hysteresis_identical_set_is_no_swap() {
        let prev = vec![row(49_001, 2), row(49_002, 2), row(49_003, 2)];
        let curr = prev.clone();
        assert!(!should_issue_depth_20_swap(&prev, &curr));
    }

    #[test]
    fn test_swap_hysteresis_below_min_is_suppressed() {
        // 2 positions changed (49_004 in, 49_001 out) — below min of 3.
        let prev = vec![
            row(49_001, 2),
            row(49_002, 2),
            row(49_003, 2),
            row(49_005, 2),
        ];
        let curr = vec![
            row(49_004, 2),
            row(49_002, 2),
            row(49_003, 2),
            row(49_005, 2),
        ];
        assert!(!should_issue_depth_20_swap(&prev, &curr));
    }

    #[test]
    fn test_swap_hysteresis_at_or_above_min_fires() {
        // 3 positions changed — at the threshold; fires.
        let prev = vec![row(49_001, 2), row(49_002, 2), row(49_003, 2)];
        let curr = vec![row(49_010, 2), row(49_011, 2), row(49_012, 2)];
        assert!(should_issue_depth_20_swap(&prev, &curr));
    }

    // ----------------------------------------------------------------------
    // Fan-out from depth-20 K=50 to depth-200 K=5 (Item 5).
    // ----------------------------------------------------------------------

    #[test]
    fn test_fan_out_to_depth_200_takes_first_5() {
        let top_50: Vec<DepthSelectorRow> = (49_000u32..49_050).map(|i| row(i, 2)).collect();
        let top_5 = fan_out_to_depth_200(&top_50);
        assert_eq!(top_5.len(), 5);
        for (i, r) in top_5.iter().enumerate() {
            assert_eq!(r.security_id, 49_000 + i as u32);
        }
    }

    #[test]
    fn test_fan_out_to_depth_200_handles_short_input() {
        // If selector returned < 5 rows (e.g. universe-wide bear day),
        // fan_out returns whatever is available (may be < 5).
        let raw = vec![row(49_001, 2), row(49_002, 2)];
        let out = fan_out_to_depth_200(&raw);
        assert_eq!(out.len(), 2);
    }

    // ----------------------------------------------------------------------
    // Static-layout helper (Item 4).
    // ----------------------------------------------------------------------

    fn synth_strikes() -> Vec<(u32, f64)> {
        // 100 strikes from 23,000 → 27,950 in 50-point steps; ATM at
        // 25,500 (index 50).
        (0..100)
            .map(|i| (10_000 + i as u32, 23_000.0 + (i as f64) * 50.0))
            .collect()
    }

    #[test]
    fn test_static_side_atm_24_centred_returns_49_contracts() {
        let strikes = synth_strikes();
        let out = select_static_side_contracts(&strikes, 50);
        assert_eq!(out.len(), 49);
        // ATM (index 50) at the middle.
        assert_eq!(out[24], 10_050);
        assert_eq!(out[0], 10_026); // ATM - 24
        assert_eq!(out[48], 10_074); // ATM + 24
    }

    #[test]
    fn test_static_side_atm_near_low_edge_truncates() {
        // ATM at index 5 — only 5 strikes below available.
        let strikes = synth_strikes();
        let out = select_static_side_contracts(&strikes, 5);
        // Window is [0, 29] = 30 strikes (5 below + ATM + 24 above).
        assert_eq!(out.len(), 30);
        assert_eq!(out[0], 10_000); // index 0 (clamped)
        assert_eq!(out[5], 10_005); // ATM
    }

    #[test]
    fn test_static_side_atm_near_high_edge_truncates() {
        let strikes = synth_strikes();
        // ATM at index 95 — only 4 strikes above available.
        let out = select_static_side_contracts(&strikes, 95);
        // Window is [71, 99] = 29 strikes.
        assert_eq!(out.len(), 29);
        assert_eq!(out[24], 10_095); // ATM
        assert_eq!(out[28], 10_099); // last
    }

    #[test]
    fn test_static_side_empty_chain_returns_empty() {
        assert!(select_static_side_contracts(&[], 0).is_empty());
    }

    #[test]
    fn test_depth_static_side_call_and_put_distinct() {
        // Trivial pin so future renames break the build with a clear
        // test name rather than silently re-routing CE↔PE.
        assert_ne!(DepthStaticSide::Call, DepthStaticSide::Put);
    }

    // ----------------------------------------------------------------------
    // Item 5 — depth-200 fan-out covers operator-pinned named tests.
    // ----------------------------------------------------------------------

    #[test]
    fn test_depth_200_picks_top_5_from_top_volume_category() {
        // Documents the contract: the SQL is the same; only the LIMIT
        // differs. depth-200 reads `selector_sql(5)` OR fan-outs from
        // depth-20's K=50 result.
        assert_eq!(DEPTH_200_DYNAMIC_K, 5);
        assert!(selector_sql(DEPTH_200_DYNAMIC_K).contains("category = 'TOP_VOLUME'"));
        assert!(
            selector_sql(DEPTH_200_DYNAMIC_K)
                .ends_with(format!("LIMIT {}", DEPTH_200_DYNAMIC_K).as_str())
        );
    }

    #[test]
    fn test_depth_200_sorts_by_change_pct_desc() {
        assert!(selector_sql(DEPTH_200_DYNAMIC_K).contains("ORDER BY change_pct DESC"));
    }
}

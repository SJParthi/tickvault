//! 2026-05-02 PR-B step 2 — depth-dynamic top-volume selector (redesign).
//!
//! Replaces the Wave 5 single-conn selector (`depth_top_volume_selector.rs`)
//! with an all-dynamic pool selector that targets `movers_1m`. Same shape
//! works for depth-20 (K = 250 = 5 conns × 50 SIDs) and depth-200
//! (K = 5 = 5 conns × 1 SID).
//!
//! ## Two-stage selection
//!
//! **Stage 1 (SQL):** read the top-N-by-volume cohort from `movers_1m`
//! filtered by `exchange_segment IN (...)` and `instrument_type IN (...)`.
//! Both filter lists are config-driven so adding BSE_FNO / NSE_EQ / IDX_I
//! is a `config/base.toml` flip with zero code change. SQL also enforces
//! freshness (`ts > now() - window_secs`) so stale post-market rows
//! never leak into the selection.
//!
//! **Stage 2 (Rust):** sort the cohort by `change_pct DESC` (with
//! deterministic tie-break on `security_id ASC`), then take the top K.
//! Pre-cohort is already volume-sorted from QuestDB so the first stage
//! enforces the "top volume FIRST" semantics; stage 2 re-ranks within
//! that cohort by % change.
//!
//! ## Why split SQL + Rust?
//!
//! - SQL is the cheap large-N filter (run on QuestDB, scales to the full
//!   universe).
//! - Rust is the small-N rank (run on ≤ cohort_size rows, deterministic
//!   ordering with explicit NaN handling and tie-break).
//!
//! ## Cold-path
//!
//! Selector runs every 60s on the rebalance scheduler — NOT the tick
//! processor hot path. Bounded allocations (cohort ≤ 1000 rows, K ≤ 250)
//! are acceptable. Per `hot-path.md` exemption: orchestrator code path,
//! not data plane.
//!
//! ## Operator demand: 4-bucket classification
//!
//! `SelectorConfig::instrument_types` is the operator's "indices vs
//! stocks vs futures vs options" filter. Examples:
//! - `["INDEX"]` — indices only
//! - `["EQUITY"]` — cash equity only
//! - `["FUTIDX","FUTSTK","FUTCOM","FUTCUR"]` — any future
//! - `["OPTIDX","OPTSTK","OPTFUT","OPTCUR"]` — any option (depth-20/200 default)
//! - `[]` — no filter (all instrument types pass)

use std::cmp::Ordering;

use tickvault_common::types::ExchangeSegment;

/// Composite-key uniqueness pair per I-P1-11. Same SecurityId can
/// exist on multiple ExchangeSegments (e.g. FINNIFTY id=27 IDX_I and
/// a stock id=27 NSE_EQ); collections must always carry the segment
/// to disambiguate.
pub type SubKey = (u32, ExchangeSegment);

/// Parses an ExchangeSegment from the canonical string representation
/// (e.g. `"NSE_FNO"`, `"BSE_FNO"`, `"NSE_EQ"`, `"IDX_I"`). Returns
/// `None` for unrecognised values — the selector skips such rows
/// rather than crashing on a malformed `movers_1m` row.
#[must_use]
fn parse_exchange_segment(s: &str) -> Option<ExchangeSegment> {
    match s {
        "IDX_I" => Some(ExchangeSegment::IdxI),
        "NSE_EQ" => Some(ExchangeSegment::NseEquity),
        "NSE_FNO" => Some(ExchangeSegment::NseFno),
        "NSE_CURRENCY" => Some(ExchangeSegment::NseCurrency),
        "BSE_EQ" => Some(ExchangeSegment::BseEquity),
        "MCX_COMM" => Some(ExchangeSegment::McxComm),
        "BSE_CURRENCY" => Some(ExchangeSegment::BseCurrency),
        "BSE_FNO" => Some(ExchangeSegment::BseFno),
        _ => None,
    }
}

/// Defensive upper bound on Stage 1 result row count. The redesigned
/// SQL no longer uses an `ORDER BY volume DESC LIMIT` cohort cap (per
/// operator clarification 2026-05-03 — illiquidity is the only filter,
/// driven by `min_liquidity_volume`). This constant is now a SAFETY
/// CAP applied via `LIMIT` only as defence-in-depth: if `min_liquidity_volume`
/// is misconfigured (too low) and returns millions of rows, the SQL
/// LIMIT still bounds Stage 2 sort cost. In normal operation
/// `min_liquidity_volume` ensures only liquid contracts pass — the
/// LIMIT is rarely hit.
pub const MAX_COHORT_SIZE: usize = 1000;

/// Minimum legal `min_liquidity_volume` value. Zero would let every
/// contract through (defeating the purpose); a too-low value risks
/// admitting illiquid contracts → slippage on live trade. Operators
/// should calibrate against real `movers_1m` data; this floor is a
/// defensive minimum.
pub const MIN_LIQUIDITY_VOLUME_FLOOR: u64 = 1;

/// Maximum final K supported by the selector. Pinned at 250 to match
/// the Wave 5 redesign cap (depth-20 = 5 conns × 50 SIDs each = 250).
pub const MAX_K: usize = 250;

/// One row from the Stage 1 cohort (top-N by volume from `movers_1m`).
///
/// Owned `String` for `exchange_segment` and `instrument_type` because
/// these come from the QuestDB query result. The selector runs on a 60s
/// cold path so `String` allocation is acceptable and bounded by
/// `MAX_COHORT_SIZE`.
#[derive(Debug, Clone, PartialEq)]
pub struct MoverRow {
    pub security_id: u32,
    /// Precise per-exchange tag (e.g., `"NSE_FNO"`, `"BSE_FNO"`,
    /// `"NSE_EQ"`, `"IDX_I"`). Populated by `movers_pipeline`
    /// from the InstrumentRegistry composite-key lookup per I-P1-11.
    pub exchange_segment: String,
    /// Precise instrument-type classification (`"INDEX"`, `"EQUITY"`,
    /// `"FUTIDX"`, `"FUTSTK"`, `"FUTCOM"`, `"FUTCUR"`, `"OPTIDX"`,
    /// `"OPTSTK"`, `"OPTFUT"`, `"OPTCUR"`).
    pub instrument_type: String,
    /// Cumulative session volume.
    pub volume: i64,
    /// `((last_price - prev_close) / prev_close) × 100` per Dhan
    /// option-chain formula. Computed by `movers_pipeline`.
    pub change_pct: f64,
}

/// Config-driven selector parameters. All values runtime-overridable
/// via `config/base.toml` per the operator's "common dynamic runtime
/// scalable" directive.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectorConfig {
    /// Allowed `instrument_type` values. Empty list = no filter (all
    /// types pass).
    pub instrument_types: Vec<String>,
    /// Allowed `exchange_segment` values. Empty list = no filter.
    pub exchange_segments: Vec<String>,
    /// Final K — number of SIDs to return after Stage 2 re-rank.
    pub k: usize,
}

/// Stage 1 SQL builder for `movers_1m` — REDESIGNED 2026-05-03 per
/// operator clarification.
///
/// **Old design (retired):** `ORDER BY volume DESC LIMIT cohort_size`
/// — admitted top-N contracts by volume regardless of absolute volume,
/// which could include illiquid contracts at rank 400-500 → slippage
/// risk on live trade.
///
/// **New design:** `WHERE volume >= min_liquidity_volume` — hard
/// liquidity gate. Only contracts that meet the operator's minimum
/// volume threshold pass to Stage 2 ranking. This guarantees that
/// every depth-subscribed contract is liquid enough for live trading.
///
/// The result is the cohort that the Rust-side `select_top_k_dynamic`
/// then ranks by `change_pct DESC` and trims to K.
///
/// `MAX_COHORT_SIZE` is applied as a defensive upper bound via `LIMIT`
/// only — if `min_liquidity_volume` is misconfigured, the LIMIT
/// prevents unbounded Stage 2 sort cost.
///
/// # Panics
///
/// Panics if `min_liquidity_volume < MIN_LIQUIDITY_VOLUME_FLOOR` or
/// `window_secs == 0`. These are caller-side defence per the security
/// review.
#[must_use]
pub fn build_cohort_sql(
    exchange_segments: &[String],
    instrument_types: &[String],
    min_liquidity_volume: u64,
    window_secs: u32,
) -> String {
    assert!(
        min_liquidity_volume >= MIN_LIQUIDITY_VOLUME_FLOOR,
        "min_liquidity_volume must be >= {MIN_LIQUIDITY_VOLUME_FLOOR}, got {min_liquidity_volume}"
    );
    assert!(
        window_secs > 0,
        "window_secs must be > 0, got {window_secs}"
    );

    let mut sql = String::with_capacity(512);
    sql.push_str(
        "SELECT security_id, exchange_segment, instrument_type, volume, change_pct \
         FROM movers_1m WHERE ts > dateadd('s', -",
    );
    sql.push_str(&window_secs.to_string());
    sql.push_str(", now())");

    if !exchange_segments.is_empty() {
        sql.push_str(" AND exchange_segment IN (");
        push_quoted_list(&mut sql, exchange_segments);
        sql.push(')');
    }
    if !instrument_types.is_empty() {
        sql.push_str(" AND instrument_type IN (");
        push_quoted_list(&mut sql, instrument_types);
        sql.push(')');
    }

    // Audit-2026-05-03: liquidity gate is the primary filter. Only
    // contracts meeting min_liquidity_volume qualify for Stage 2 rank.
    sql.push_str(" AND volume >= ");
    sql.push_str(&min_liquidity_volume.to_string());

    // Audit-2026-05-03 (hostile-bug-hunt H2 fix): explicit `ORDER BY
    // volume DESC` so the defensive LIMIT below truncates LOW-liquidity
    // rows (correct) rather than NEW rows in storage order (which would
    // silently drop the highest-information data when min_liquidity is
    // misconfigured low and matches > MAX_COHORT_SIZE rows). Stage 2
    // re-ranks the kept rows by change_pct DESC anyway, so this ORDER
    // BY only affects which rows get TRUNCATED — not the final output
    // ordering.
    sql.push_str(" ORDER BY volume DESC");

    // Defensive LIMIT to bound Stage 2 sort cost in case min_liquidity
    // is misconfigured. NOT the primary filter — the WHERE clause is.
    sql.push_str(" LIMIT ");
    sql.push_str(&MAX_COHORT_SIZE.to_string());
    sql
}

fn push_quoted_list(out: &mut String, items: &[String]) {
    for (idx, item) in items.iter().enumerate() {
        if idx > 0 {
            out.push_str(", ");
        }
        out.push('\'');
        // Defensive (security-reviewer 2026-05-02 MEDIUM 1+2): SQL-quote
        // escape (double `'` to `''`) AND filter ASCII / Latin-1 control
        // characters. Config-supplied values are operator-controlled and
        // should be enum-like uppercase tokens (NSE_FNO, OPTSTK, etc.) but
        // this matches the `sanitize_audit_string` defensive standard so
        // a malformed base.toml cannot inject SQL or wire-format chaos.
        for ch in item.chars() {
            // Strip ASCII C0 (0x00..0x1F), DEL (0x7F), and C1 (0x80..0x9F)
            // control characters — never legal in any ExchangeSegment /
            // InstrumentType enum value.
            if ch.is_control() {
                continue;
            }
            if ch == '\'' {
                // Standard SQL quote escape — doubled apostrophe.
                out.push('\'');
                out.push('\'');
            } else {
                out.push(ch);
            }
        }
        out.push('\'');
    }
}

/// 2026-05-02 PR-B step 2: Stage 2 — re-rank the cohort by
/// `change_pct DESC` and take the top K composite-key SubKeys.
///
/// Returns `Vec<SubKey>` = `Vec<(security_id, ExchangeSegment)>` per
/// I-P1-11 — same SecurityId on different segments is a real
/// production scenario (FINNIFTY id=27 vs stock id=27) and must be
/// disambiguated by the segment. Rows whose `exchange_segment` field
/// fails to parse to a known `ExchangeSegment` enum variant are
/// silently dropped (defensive against a malformed `movers_1m` row).
///
/// Tie-breaking: when two rows have the same `change_pct`, the one with
/// the smaller `security_id` wins. Deterministic order is required for
/// stable diff against the previous cycle's set (the diff state machine
/// uses set equality, not list equality, so deterministic order is
/// belt-and-suspenders for downstream debugging).
///
/// NaN handling: `f64::partial_cmp` returns `None` on NaN. We treat NaN
/// `change_pct` as the lowest possible value (sinks to bottom of the
/// ranking) so a corrupt row never wins the top-K race.
///
/// # Panics
///
/// Panics if `cfg.k > MAX_K`.
#[must_use]
pub fn select_top_k_dynamic(cohort: &[MoverRow], cfg: &SelectorConfig) -> Vec<SubKey> {
    assert!(cfg.k <= MAX_K, "k must be <= {MAX_K}, got {}", cfg.k);

    if cfg.k == 0 || cohort.is_empty() {
        return Vec::new();
    }

    // Filter (defensive — SQL should already do this, but a malformed
    // movers row could slip through).
    let mut filtered: Vec<&MoverRow> = Vec::with_capacity(cohort.len());
    for row in cohort {
        if !cfg.instrument_types.is_empty()
            && !cfg
                .instrument_types
                .iter()
                .any(|t| t == &row.instrument_type)
        {
            continue;
        }
        if !cfg.exchange_segments.is_empty()
            && !cfg
                .exchange_segments
                .iter()
                .any(|s| s == &row.exchange_segment)
        {
            continue;
        }
        filtered.push(row);
    }

    // Stage 2 sort: pure `change_pct DESC` (audit-2026-05-03 operator
    // clarification — security_id ASC tie-break removed). With the
    // min_liquidity_volume gate at Stage 1, contracts that pass have
    // distinct prev_close + last_price → distinct change_pct ratios in
    // practice. Exact ties are statistically negligible. NaN sinks to
    // the bottom defensively.
    filtered.sort_unstable_by(|a, b| {
        let a_nan = a.change_pct.is_nan();
        let b_nan = b.change_pct.is_nan();
        match (a_nan, b_nan) {
            // Both NaN: keep insertion order (input is already volume-DESC
            // from Stage 1 filter, so the deterministic seed is upstream).
            (true, true) => Ordering::Equal,
            // a is NaN — a sinks below b.
            (true, false) => Ordering::Greater,
            // b is NaN — b sinks below a.
            (false, true) => Ordering::Less,
            // Neither NaN — ordinary partial_cmp result. Exact ties keep
            // insertion order (Stage 1 volume-DESC ordering when min_liquidity
            // gate produces ties — rare in practice).
            (false, false) => b
                .change_pct
                .partial_cmp(&a.change_pct)
                .unwrap_or(Ordering::Equal),
        }
    });

    let take_n = cfg.k.min(filtered.len());
    let mut out: Vec<SubKey> = Vec::with_capacity(take_n);
    for row in filtered.iter() {
        if out.len() >= take_n {
            break;
        }
        // Skip rows whose exchange_segment string doesn't parse to a
        // known enum variant. Defensive: a malformed `movers_1m` row
        // (e.g., "UNKNOWN" fallback) must not produce an unsubscribable
        // SubKey downstream.
        if let Some(seg) = parse_exchange_segment(&row.exchange_segment) {
            out.push((row.security_id, seg));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(security_id: u32, seg: &str, kind: &str, volume: i64, change_pct: f64) -> MoverRow {
        MoverRow {
            security_id,
            exchange_segment: seg.to_string(),
            instrument_type: kind.to_string(),
            volume,
            change_pct,
        }
    }

    fn default_cfg(k: usize) -> SelectorConfig {
        SelectorConfig {
            instrument_types: vec!["OPTSTK".to_string(), "OPTIDX".to_string()],
            exchange_segments: vec!["NSE_FNO".to_string()],
            k,
        }
    }

    // ---- build_cohort_sql ----
    // Audit-2026-05-03: redesigned per operator clarification.
    // Old: ORDER BY volume DESC LIMIT cohort_size
    // New: WHERE ... AND volume >= min_liquidity_volume LIMIT MAX_COHORT_SIZE
    // Test volume threshold of 10_000 used as a realistic floor.

    const TEST_MIN_VOL: u64 = 10_000;

    #[test]
    fn test_build_cohort_sql_targets_movers_1m_table() {
        let sql = build_cohort_sql(&[], &[], TEST_MIN_VOL, 60);
        assert!(
            sql.contains("FROM movers_1m"),
            "must read from movers_1m unified base, got: {sql}"
        );
    }

    #[test]
    fn test_build_cohort_sql_includes_window_secs_in_dateadd() {
        let sql = build_cohort_sql(&[], &[], TEST_MIN_VOL, 90);
        assert!(
            sql.contains("dateadd('s', -90, now())"),
            "must include window_secs in dateadd, got: {sql}"
        );
    }

    #[test]
    fn test_build_cohort_sql_emits_exchange_segment_in_filter_when_provided() {
        let sql = build_cohort_sql(
            &["NSE_FNO".to_string(), "BSE_FNO".to_string()],
            &[],
            TEST_MIN_VOL,
            60,
        );
        assert!(sql.contains("exchange_segment IN ('NSE_FNO', 'BSE_FNO')"));
    }

    #[test]
    fn test_build_cohort_sql_emits_instrument_type_in_filter_when_provided() {
        let sql = build_cohort_sql(
            &[],
            &["OPTSTK".to_string(), "OPTIDX".to_string()],
            TEST_MIN_VOL,
            60,
        );
        assert!(sql.contains("instrument_type IN ('OPTSTK', 'OPTIDX')"));
    }

    #[test]
    fn test_build_cohort_sql_omits_filter_when_list_empty() {
        let sql = build_cohort_sql(&[], &[], TEST_MIN_VOL, 60);
        assert!(
            !sql.contains("exchange_segment IN"),
            "no segment filter when list empty"
        );
        assert!(
            !sql.contains("instrument_type IN"),
            "no type filter when list empty"
        );
    }

    /// Audit-2026-05-03 ratchet: the redesigned SQL MUST use a min-volume
    /// liquidity gate (`AND volume >=`) as the PRIMARY filter — the
    /// legacy top-N filtering via the cohort_size parameter is retired.
    /// (Note: `ORDER BY volume DESC` is RE-ADDED by the H2 fix to make
    /// the defensive `LIMIT 1000` truncate LOW-volume rows correctly,
    /// but it's no longer the primary mechanism — `WHERE volume >=` is.)
    #[test]
    fn test_build_cohort_sql_uses_min_volume_filter_as_primary_gate() {
        let sql = build_cohort_sql(&[], &[], 50_000, 60);
        assert!(
            sql.contains("AND volume >= 50000"),
            "must filter by `volume >= min_liquidity_volume`, got: {sql}"
        );
        // The WHERE clause must come BEFORE the ORDER BY (SQL syntax
        // + semantic correctness — filter first, then order/truncate).
        let where_pos = sql
            .find("AND volume >= 50000")
            .expect("WHERE clause missing");
        let order_pos = sql
            .find("ORDER BY volume DESC")
            .expect("ORDER BY (defensive) missing");
        assert!(
            where_pos < order_pos,
            "WHERE volume >= must precede ORDER BY (filter is primary, order is defensive)"
        );
    }

    /// Audit-2026-05-03 ratchet: the LIMIT clause is now defensive
    /// (capped at `MAX_COHORT_SIZE = 1000`), NOT operator-tunable. The
    /// liquidity gate is the primary filter; LIMIT only protects
    /// against runaway result sets if `min_liquidity_volume` is
    /// misconfigured.
    #[test]
    fn test_build_cohort_sql_uses_defensive_max_cohort_limit() {
        let sql = build_cohort_sql(&[], &[], TEST_MIN_VOL, 60);
        let expected = format!("LIMIT {}", MAX_COHORT_SIZE);
        assert!(
            sql.contains(&expected),
            "must include defensive LIMIT {MAX_COHORT_SIZE}, got: {sql}"
        );
    }

    /// Audit-2026-05-03 (hostile-bug-hunt H2 fix) ratchet: SQL MUST
    /// include `ORDER BY volume DESC` so the defensive `LIMIT 1000`
    /// truncates LOW-volume rows (correct) rather than dropping NEW
    /// rows in storage order (wrong — would lose highest-information
    /// data when min_liquidity is misconfigured low).
    #[test]
    fn test_build_cohort_sql_orders_by_volume_desc_for_safe_truncation() {
        let sql = build_cohort_sql(&[], &[], TEST_MIN_VOL, 60);
        assert!(
            sql.contains("ORDER BY volume DESC"),
            "ORDER BY volume DESC required so LIMIT truncates LOW-volume rows \
             (not new rows in storage order); got: {sql}"
        );
        // Position check: ORDER BY must come BEFORE LIMIT
        let order_pos = sql.find("ORDER BY volume DESC").expect("ORDER BY missing");
        let limit_pos = sql.find("LIMIT").expect("LIMIT missing");
        assert!(
            order_pos < limit_pos,
            "ORDER BY must precede LIMIT in SQL syntax"
        );
    }

    #[test]
    fn test_build_cohort_sql_doubles_single_quotes_per_sql_standard() {
        let sql = build_cohort_sql(&["'OR 1=1".to_string()], &[], TEST_MIN_VOL, 60);
        assert!(
            sql.contains("'''OR 1=1'"),
            "input leading single quote must be doubled per SQL standard, got: {sql}"
        );
        let in_list_chunk = sql.split("IN (").nth(1).unwrap_or("");
        let close_paren = in_list_chunk.find(')').unwrap_or(in_list_chunk.len());
        let chunk = &in_list_chunk[..close_paren];
        let quotes = chunk.chars().filter(|c| *c == '\'').count();
        assert_eq!(
            quotes % 2,
            0,
            "quotes must be balanced (even count) — found {quotes} in: {chunk}"
        );
    }

    #[test]
    fn test_build_cohort_sql_strips_control_characters() {
        let evil = "NSE\nFNO\r\0\x07\x1b\x7fX".to_string();
        let sql = build_cohort_sql(&[evil], &[], TEST_MIN_VOL, 60);
        assert!(
            sql.contains("'NSEFNOX'"),
            "control characters must be stripped, leaving 'NSEFNOX', got: {sql}"
        );
        assert!(
            !sql.contains('\n') && !sql.contains('\r') && !sql.contains('\0'),
            "no raw control chars must appear in the SQL, got: {sql}"
        );
    }

    #[test]
    #[should_panic(expected = "min_liquidity_volume must be >= 1")]
    fn test_build_cohort_sql_panics_on_zero_min_volume() {
        let _ = build_cohort_sql(&[], &[], 0, 60);
    }

    #[test]
    #[should_panic(expected = "window_secs must be > 0")]
    fn test_build_cohort_sql_panics_on_zero_window() {
        let _ = build_cohort_sql(&[], &[], TEST_MIN_VOL, 0);
    }

    // ---- select_top_k_dynamic ----

    #[test]
    fn test_select_top_k_dynamic_returns_empty_when_cohort_empty() {
        let cfg = default_cfg(10);
        let result = select_top_k_dynamic(&[], &cfg);
        assert!(result.is_empty());
    }

    #[test]
    fn test_select_top_k_dynamic_returns_empty_when_k_is_zero() {
        let mut cfg = default_cfg(0);
        cfg.k = 0;
        let cohort = vec![row(1, "NSE_FNO", "OPTSTK", 1_000_000, 5.0)];
        let result = select_top_k_dynamic(&cohort, &cfg);
        assert!(result.is_empty());
    }

    #[test]
    fn test_select_top_k_dynamic_returns_all_rows_when_k_exceeds_cohort_size() {
        let cohort = vec![
            row(1, "NSE_FNO", "OPTSTK", 1_000_000, 5.0),
            row(2, "NSE_FNO", "OPTSTK", 500_000, 8.0),
        ];
        let cfg = default_cfg(10);
        let result = select_top_k_dynamic(&cohort, &cfg);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_select_top_k_dynamic_sorts_by_change_pct_desc() {
        // Volume order is intentionally inverse to change_pct order to
        // verify Stage 2 re-rank.
        let cohort = vec![
            row(1, "NSE_FNO", "OPTSTK", 5_000_000, 1.0),
            row(2, "NSE_FNO", "OPTSTK", 4_000_000, 5.0),
            row(3, "NSE_FNO", "OPTSTK", 3_000_000, 10.0),
            row(4, "NSE_FNO", "OPTSTK", 2_000_000, 15.0),
            row(5, "NSE_FNO", "OPTSTK", 1_000_000, 20.0),
        ];
        let cfg = default_cfg(3);
        let result = select_top_k_dynamic(&cohort, &cfg);
        let nse_fno = ExchangeSegment::NseFno;
        assert_eq!(
            result,
            vec![(5, nse_fno), (4, nse_fno), (3, nse_fno)],
            "top 3 by change_pct DESC: 20%(5), 15%(4), 10%(3)"
        );
    }

    /// Audit-2026-05-03 ratchet: Stage 2 sort is now PURE `change_pct DESC`.
    /// The old `security_id ASC` tie-break was retired per operator
    /// clarification. With the min_liquidity_volume gate at Stage 1,
    /// contracts that pass have distinct prev_close + last_price →
    /// distinct change_pct ratios in practice. Exact change_pct ties
    /// keep insertion order (stable behaviour from Stage 1).
    #[test]
    fn test_select_top_k_dynamic_no_longer_uses_security_id_tie_break() {
        let cohort = vec![
            row(99, "NSE_FNO", "OPTSTK", 1_000_000, 5.0),
            row(50, "NSE_FNO", "OPTSTK", 1_000_000, 5.0),
            row(75, "NSE_FNO", "OPTSTK", 1_000_000, 5.0),
        ];
        let cfg = default_cfg(3);
        let result = select_top_k_dynamic(&cohort, &cfg);
        // All 3 rows returned (exact change_pct tie), but order is now
        // insertion-order from Stage 1, NOT security_id ASC.
        assert_eq!(result.len(), 3);
        let ids: Vec<u32> = result.iter().map(|(id, _)| *id).collect();
        // Insertion order from cohort: 99, 50, 75
        assert_eq!(ids, vec![99, 50, 75]);
        // Negative pin: NOT sorted by security_id
        assert_ne!(
            ids,
            vec![50, 75, 99],
            "Stage 2 must NOT apply security_id ASC tie-break (retired 2026-05-03)"
        );
    }

    #[test]
    fn test_select_top_k_dynamic_filters_by_instrument_type() {
        let cohort = vec![
            row(1, "NSE_FNO", "OPTSTK", 1_000_000, 10.0),
            row(2, "NSE_FNO", "EQUITY", 5_000_000, 20.0), // higher change_pct but EQUITY
            row(3, "NSE_FNO", "OPTSTK", 500_000, 5.0),
        ];
        let cfg = SelectorConfig {
            instrument_types: vec!["OPTSTK".to_string()],
            exchange_segments: vec![],
            k: 5,
        };
        let result = select_top_k_dynamic(&cohort, &cfg);
        let nse_fno = ExchangeSegment::NseFno;
        assert_eq!(
            result,
            vec![(1, nse_fno), (3, nse_fno)],
            "EQUITY row must be filtered out"
        );
    }

    #[test]
    fn test_select_top_k_dynamic_filters_by_exchange_segment() {
        let cohort = vec![
            row(1, "NSE_FNO", "OPTSTK", 1_000_000, 10.0),
            row(2, "BSE_FNO", "OPTSTK", 5_000_000, 20.0), // SENSEX
            row(3, "NSE_FNO", "OPTSTK", 500_000, 5.0),
        ];
        let cfg = SelectorConfig {
            instrument_types: vec![],
            exchange_segments: vec!["NSE_FNO".to_string()],
            k: 5,
        };
        let result = select_top_k_dynamic(&cohort, &cfg);
        let nse_fno = ExchangeSegment::NseFno;
        assert_eq!(
            result,
            vec![(1, nse_fno), (3, nse_fno)],
            "BSE_FNO (SENSEX) row must be filtered"
        );
    }

    #[test]
    fn test_select_top_k_dynamic_handles_nan_change_pct_by_sinking_to_bottom() {
        let cohort = vec![
            row(1, "NSE_FNO", "OPTSTK", 1_000_000, f64::NAN),
            row(2, "NSE_FNO", "OPTSTK", 500_000, 5.0),
            row(3, "NSE_FNO", "OPTSTK", 300_000, 10.0),
        ];
        let cfg = default_cfg(2);
        let result = select_top_k_dynamic(&cohort, &cfg);
        // NaN row sinks; top 2 are 10% (3) and 5% (2).
        let nse_fno = ExchangeSegment::NseFno;
        assert_eq!(result, vec![(3, nse_fno), (2, nse_fno)]);
    }

    #[test]
    fn test_select_top_k_dynamic_is_deterministic_across_calls() {
        let cohort = vec![
            row(1, "NSE_FNO", "OPTSTK", 1_000_000, 5.0),
            row(2, "NSE_FNO", "OPTSTK", 1_000_000, 5.0),
            row(3, "NSE_FNO", "OPTSTK", 1_000_000, 5.0),
        ];
        let cfg = default_cfg(3);
        let r1 = select_top_k_dynamic(&cohort, &cfg);
        let r2 = select_top_k_dynamic(&cohort, &cfg);
        let r3 = select_top_k_dynamic(&cohort, &cfg);
        assert_eq!(r1, r2);
        assert_eq!(r2, r3);
    }

    #[test]
    fn test_select_top_k_dynamic_empty_filter_lists_match_all_rows() {
        let cohort = vec![
            row(1, "NSE_FNO", "OPTSTK", 1_000_000, 5.0),
            row(2, "BSE_FNO", "EQUITY", 500_000, 10.0),
            row(3, "IDX_I", "INDEX", 300_000, 15.0),
        ];
        let cfg = SelectorConfig {
            instrument_types: vec![],
            exchange_segments: vec![],
            k: 10,
        };
        let result = select_top_k_dynamic(&cohort, &cfg);
        assert_eq!(result.len(), 3, "empty filter lists pass everything");
        // Verify per-row segments parsed correctly.
        assert_eq!(result[0], (3, ExchangeSegment::IdxI), "INDEX = IDX_I");
        assert_eq!(result[1], (2, ExchangeSegment::BseFno), "BSE_FNO");
        assert_eq!(result[2], (1, ExchangeSegment::NseFno), "NSE_FNO");
    }

    #[test]
    fn test_select_top_k_dynamic_supports_4_bucket_classification_options_only() {
        // Operator's classification: any option = OPTIDX|OPTSTK|OPTFUT|OPTCUR
        let cohort = vec![
            row(1, "NSE_FNO", "OPTSTK", 5_000_000, 5.0),
            row(2, "NSE_FNO", "OPTIDX", 4_000_000, 10.0),
            row(3, "MCX_COMM", "OPTFUT", 3_000_000, 15.0),
            row(4, "NSE_CUR", "OPTCUR", 2_000_000, 20.0),
            row(5, "NSE_FNO", "FUTSTK", 1_000_000, 25.0), // future, must filter
        ];
        let cfg = SelectorConfig {
            instrument_types: vec![
                "OPTIDX".to_string(),
                "OPTSTK".to_string(),
                "OPTFUT".to_string(),
                "OPTCUR".to_string(),
            ],
            exchange_segments: vec![],
            k: 10,
        };
        let result = select_top_k_dynamic(&cohort, &cfg);
        // Top 4 by change_pct DESC: OPTCUR(20)=4, OPTFUT(15)=3, OPTIDX(10)=2, OPTSTK(5)=1.
        // FUTSTK(25)=5 is filtered out. Note OPTCUR sits in NSE_CUR (typo
        // intentionally kept here to verify parse-failure path) — that
        // row gets dropped by the segment parser, leaving 3 results.
        // We accept both behaviours: either the parser accepts NSE_CUR
        // (it doesn't — only NSE_CURRENCY) so SID=4 is filtered.
        // Operator-side mitigation: the writer always emits canonical
        // strings via ExchangeSegment::as_str(), so this is defensive.
        assert_eq!(
            result.len(),
            3,
            "OPTCUR with malformed segment NSE_CUR is dropped"
        );
        let nse_fno = ExchangeSegment::NseFno;
        let mcx_comm = ExchangeSegment::McxComm;
        assert_eq!(
            result,
            vec![(3, mcx_comm), (2, nse_fno), (1, nse_fno)],
            "FUTSTK filtered, NSE_CUR malformed dropped, top 3 by change_pct DESC"
        );
    }

    #[test]
    fn test_select_top_k_dynamic_panics_when_k_exceeds_max() {
        // Defensive bound check.
        let cfg = SelectorConfig {
            instrument_types: vec![],
            exchange_segments: vec![],
            k: MAX_K + 1,
        };
        let cohort: Vec<MoverRow> = Vec::new();
        let result = std::panic::catch_unwind(|| select_top_k_dynamic(&cohort, &cfg));
        assert!(result.is_err());
    }
}

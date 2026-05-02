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

/// Maximum cohort size returned by Stage 1 SQL. Caller-side defence:
/// the SQL builder panics if requested cohort exceeds this. Pinned at
/// 1000 to keep Stage 2 sort cost bounded (a 60s cycle should never
/// process more than this many rows).
pub const MAX_COHORT_SIZE: usize = 1000;

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
    /// `"NSE_EQ"`, `"IDX_I"`). Populated by `movers_unified_pipeline`
    /// from the InstrumentRegistry composite-key lookup per I-P1-11.
    pub exchange_segment: String,
    /// Precise instrument-type classification (`"INDEX"`, `"EQUITY"`,
    /// `"FUTIDX"`, `"FUTSTK"`, `"FUTCOM"`, `"FUTCUR"`, `"OPTIDX"`,
    /// `"OPTSTK"`, `"OPTFUT"`, `"OPTCUR"`).
    pub instrument_type: String,
    /// Cumulative session volume.
    pub volume: i64,
    /// `((last_price - prev_close) / prev_close) × 100` per Dhan
    /// option-chain formula. Computed by `movers_unified_pipeline`.
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

/// 2026-05-02 PR-B step 2: Stage 1 SQL builder for `movers_1m`.
///
/// The query reads the top-`cohort_size`-by-volume from the most recent
/// `window_secs` of `movers_1m` data, filtered by `exchange_segment`
/// and `instrument_type` lists. Both filter lists are passed by the
/// caller (typically from `config/base.toml`).
///
/// The result is the cohort that the Rust-side `select_top_k_dynamic`
/// then re-ranks by `change_pct DESC` and trims to K.
///
/// # Panics
///
/// Panics if `cohort_size == 0`, `cohort_size > MAX_COHORT_SIZE`, or
/// `window_secs == 0`. These are caller-side defence per the security
/// review.
#[must_use]
pub fn build_cohort_sql(
    exchange_segments: &[String],
    instrument_types: &[String],
    cohort_size: usize,
    window_secs: u32,
) -> String {
    assert!(
        cohort_size > 0 && cohort_size <= MAX_COHORT_SIZE,
        "cohort_size must be in 1..={MAX_COHORT_SIZE}, got {cohort_size}"
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

    sql.push_str(" ORDER BY volume DESC LIMIT ");
    sql.push_str(&cohort_size.to_string());
    sql
}

fn push_quoted_list(out: &mut String, items: &[String]) {
    for (idx, item) in items.iter().enumerate() {
        if idx > 0 {
            out.push_str(", ");
        }
        out.push('\'');
        // Defensive: filter out single-quote chars (SQL injection guard).
        // Config-supplied values should be enum-like uppercase tokens
        // (NSE_FNO, OPTSTK, etc.) but this protects against a malformed
        // base.toml.
        for ch in item.chars() {
            if ch != '\'' {
                out.push(ch);
            }
        }
        out.push('\'');
    }
}

/// 2026-05-02 PR-B step 2: Stage 2 — re-rank the cohort by
/// `change_pct DESC` and take the top K.
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
pub fn select_top_k_dynamic(cohort: &[MoverRow], cfg: &SelectorConfig) -> Vec<u32> {
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

    // Stage 2 sort: change_pct DESC, security_id ASC for ties.
    // NaN guard: any NaN row sinks to the BOTTOM of the ranking,
    // regardless of the other operand. We check NaN BEFORE delegating
    // to partial_cmp because partial_cmp returns `None` on NaN — which
    // would otherwise fall into the "tie / unknown" branch and let the
    // NaN row win on a security_id tie-break.
    filtered.sort_unstable_by(|a, b| {
        let a_nan = a.change_pct.is_nan();
        let b_nan = b.change_pct.is_nan();
        match (a_nan, b_nan) {
            // Both NaN: stable tie-break by security_id ASC.
            (true, true) => a.security_id.cmp(&b.security_id),
            // a is NaN — a sinks below b.
            (true, false) => Ordering::Greater,
            // b is NaN — b sinks below a.
            (false, true) => Ordering::Less,
            // Neither NaN — ordinary partial_cmp result.
            (false, false) => match b.change_pct.partial_cmp(&a.change_pct) {
                Some(Ordering::Equal) | None => a.security_id.cmp(&b.security_id),
                Some(ord) => ord,
            },
        }
    });

    let take_n = cfg.k.min(filtered.len());
    let mut out = Vec::with_capacity(take_n);
    for row in filtered.iter().take(take_n) {
        out.push(row.security_id);
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

    #[test]
    fn test_build_cohort_sql_targets_movers_1m_table() {
        let sql = build_cohort_sql(&[], &[], 500, 60);
        assert!(
            sql.contains("FROM movers_1m"),
            "must read from movers_1m unified base, got: {sql}"
        );
    }

    #[test]
    fn test_build_cohort_sql_includes_window_secs_in_dateadd() {
        let sql = build_cohort_sql(&[], &[], 500, 90);
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
            500,
            60,
        );
        assert!(sql.contains("exchange_segment IN ('NSE_FNO', 'BSE_FNO')"));
    }

    #[test]
    fn test_build_cohort_sql_emits_instrument_type_in_filter_when_provided() {
        let sql = build_cohort_sql(&[], &["OPTSTK".to_string(), "OPTIDX".to_string()], 500, 60);
        assert!(sql.contains("instrument_type IN ('OPTSTK', 'OPTIDX')"));
    }

    #[test]
    fn test_build_cohort_sql_omits_filter_when_list_empty() {
        let sql = build_cohort_sql(&[], &[], 500, 60);
        assert!(
            !sql.contains("exchange_segment IN"),
            "no segment filter when list empty"
        );
        assert!(
            !sql.contains("instrument_type IN"),
            "no type filter when list empty"
        );
    }

    #[test]
    fn test_build_cohort_sql_orders_by_volume_desc() {
        let sql = build_cohort_sql(&[], &[], 500, 60);
        assert!(sql.contains("ORDER BY volume DESC"));
    }

    #[test]
    fn test_build_cohort_sql_includes_limit() {
        let sql = build_cohort_sql(&[], &[], 250, 60);
        assert!(sql.contains("LIMIT 250"));
    }

    #[test]
    fn test_build_cohort_sql_strips_single_quotes_from_input() {
        // Defensive SQL-injection guard: caller-supplied single quotes
        // must be stripped so the SQL stays well-formed even on a
        // malformed config file. A bare leading single quote in the
        // input would prematurely close our wrapping quote and inject.
        let sql = build_cohort_sql(&["'OR 1=1".to_string()], &[], 100, 60);
        // Expected: input `'OR 1=1` becomes `OR 1=1` after stripping;
        // wrapped in our own quotes the IN-list emits `'OR 1=1'`. The
        // user's leading single quote is gone (no `''` double-quote),
        // so SQL stays balanced.
        assert!(
            !sql.contains("''OR 1=1"),
            "leading single quote must be stripped (no double-quote pair), got: {sql}"
        );
        // Belt-and-suspenders: count of single quotes in the IN-list
        // chunk is exactly 2 (our wrapping pair) for a single value.
        let in_list_chunk = sql.split("IN (").nth(1).unwrap_or("");
        let close_paren = in_list_chunk.find(')').unwrap_or(in_list_chunk.len());
        let chunk = &in_list_chunk[..close_paren];
        let quotes = chunk.chars().filter(|c| *c == '\'').count();
        assert_eq!(
            quotes, 2,
            "exactly 2 wrapping quotes for one stripped value, got: {chunk}"
        );
    }

    #[test]
    #[should_panic(expected = "cohort_size must be in 1..=1000")]
    fn test_build_cohort_sql_panics_on_zero_cohort() {
        let _ = build_cohort_sql(&[], &[], 0, 60);
    }

    #[test]
    #[should_panic(expected = "cohort_size must be in 1..=1000")]
    fn test_build_cohort_sql_panics_on_oversized_cohort() {
        let _ = build_cohort_sql(&[], &[], MAX_COHORT_SIZE + 1, 60);
    }

    #[test]
    #[should_panic(expected = "window_secs must be > 0")]
    fn test_build_cohort_sql_panics_on_zero_window() {
        let _ = build_cohort_sql(&[], &[], 100, 0);
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
        assert_eq!(
            result,
            vec![5, 4, 3],
            "top 3 by change_pct DESC: 20%(5), 15%(4), 10%(3)"
        );
    }

    #[test]
    fn test_select_top_k_dynamic_breaks_ties_by_security_id_ascending() {
        let cohort = vec![
            row(99, "NSE_FNO", "OPTSTK", 1_000_000, 5.0),
            row(50, "NSE_FNO", "OPTSTK", 1_000_000, 5.0),
            row(75, "NSE_FNO", "OPTSTK", 1_000_000, 5.0),
        ];
        let cfg = default_cfg(3);
        let result = select_top_k_dynamic(&cohort, &cfg);
        assert_eq!(
            result,
            vec![50, 75, 99],
            "ties on change_pct must break by security_id ASC"
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
        assert_eq!(result, vec![1, 3], "EQUITY row must be filtered out");
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
        assert_eq!(result, vec![1, 3], "BSE_FNO (SENSEX) row must be filtered");
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
        assert_eq!(result, vec![3, 2]);
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
        // FUTSTK(25)=5 is filtered out.
        assert_eq!(result, vec![4, 3, 2, 1]);
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

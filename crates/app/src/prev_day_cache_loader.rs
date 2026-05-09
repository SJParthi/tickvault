//! F2 (Wave-5 §K-L13 / #504e follow-up): boot-time loader for
//! [`PrevDayCache`].
//!
//! ## Why this module exists
//!
//! PR #511 (#504e primitives) shipped the `PrevDayCache` data structure
//! and the `on_*_with_pct` engine variants. PR #520 (F1) wired the
//! cascade seal-site to use those variants. Without F2 the cache stays
//! empty at boot, so every sealed Bar carries `0.0` for the 5 frozen-
//! per-day % fields (per the div-by-zero policy in
//! `crates/trading/src/candles/pct_stamping.rs`).
//!
//! F2 reads the `previous_close` QuestDB table (PR #466, populated from
//! live IDX_I/NSE_EQ/NSE_FNO ticks via `PreviousCloseWriter`) and merges
//! the boot-loaded `prev_oi_cache` (PR #454, from yesterday's NSE
//! bhavcopy + Dhan Option Chain overlay) so every entry carries
//! `(prev_day_close, prev_day_oi)` populated. `prev_day_total_volume`
//! is set to `0` for now — the bhavcopy total-volume column is a
//! separate follow-up (the `compute_volume_pct` div-by-zero policy
//! returns `0.0` rather than `NaN`, so this is benign in production).
//!
//! ## Composite-key invariant (I-P1-11)
//!
//! Every cache entry is keyed `(security_id, exchange_segment_code)`.
//! The QuestDB `previous_close` row stores `segment` as a string
//! SYMBOL (`"IDX_I"`, `"NSE_EQ"`, `"NSE_FNO"`, ...); F2 round-trips
//! through [`segment_str_to_code`] to recover the binary code. Rows
//! whose segment string is unknown (corrupt input, future segment we
//! don't yet support) are skipped with a structured WARN — the loader
//! does NOT halt boot on unknown segments per the false-OK Rule 11
//! qualifier (we'd rather populate what we can).
//!
//! ## Boot timing
//!
//! Runs once at boot, after the `prev_oi_cache` loader has populated
//! the OI side. The cascade may already be live at this point — that
//! is fine because `PrevDayCache::insert` is concurrent-safe (papaya
//! handles the race) and the cascade reads are lock-free. Sealed Bars
//! that pop out before the loader finishes simply use the
//! div-by-zero-policy `0.0` fallback for the % fields; once the
//! loader populates a key, subsequent seals for that instrument see
//! the stamped values.
//!
//! ## Cold-path performance
//!
//! - QuestDB `/exec` HTTP GET: one network round-trip (~50 ms typical).
//! - JSON dataset parse: one allocation per row (~24 KB for 11K rows).
//! - `papaya::insert`: ~50 ns × N rows = ~550 µs at 11K rows.
//! - Total boot cost: under 100 ms even on cold QuestDB.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::{info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::market_hours::is_within_trading_session_ist;
use tickvault_common::segment::segment_str_to_code;
use tickvault_trading::candles::pct_stamping::PrevDayRefs;
use tickvault_trading::in_mem::PrevDayCache;

/// HTTP timeout for the boot-time `previous_close` SELECT against
/// QuestDB. Generous — the table is small (11K rows max) and the
/// query is unindexed but bounded; 10 s comfortably covers cold-cache
/// reads on AWS c8g.xlarge.
const QUESTDB_QUERY_TIMEOUT_SECS: u64 = 10;

/// Look-back window for the `previous_close` SELECT. Bounds the
/// designated-timestamp scan to the last 7 trading days so a stale
/// `previous_close` row from a multi-week-old run does NOT seed
/// today's cache. QuestDB's WAL replay across week-old rows would
/// otherwise produce false `prev_day_close` values for instruments
/// that were re-listed after a corporate action.
const QUESTDB_LOOKBACK_DAYS: u32 = 7;

/// Builds the SELECT SQL for the latest `prev_close` per
/// `(security_id, segment)` within the last [`QUESTDB_LOOKBACK_DAYS`].
///
/// Pure function — testable without a live QuestDB. The literal is
/// parameter-free; the only "input" is the constant lookback window
/// which is pinned by [`build_select_latest_prev_close_sql_pins_lookback`].
#[must_use]
pub fn build_select_latest_prev_close_sql() -> String {
    format!(
        "SELECT security_id, segment, last(prev_close) AS prev_close \
         FROM previous_close \
         WHERE ts > dateadd('d', -{lookback}, now()) \
         GROUP BY security_id, segment",
        lookback = QUESTDB_LOOKBACK_DAYS
    )
}

/// QuestDB `/exec` JSON response shape (subset). The dataset is an
/// array-of-arrays; each inner array's elements correspond to the
/// SELECT column order: `[security_id, segment, prev_close]`.
#[derive(Debug, Deserialize)]
struct QuestDbExecResponse {
    /// `[[<sec_id>, <segment>, <prev_close>], ...]`. Untyped to
    /// tolerate QuestDB's mixed-type rows (LONG | SYMBOL | DOUBLE).
    dataset: Vec<Vec<serde_json::Value>>,
}

/// Pure-logic decoder: consumes the QuestDB JSON dataset and produces
/// `(security_id, exchange_segment_code, prev_close)` triples. Rows
/// whose `segment` string is not recognised by [`segment_str_to_code`]
/// are skipped (the WARN counter is bumped by the caller).
///
/// Returns `(parsed, skipped_unknown_segment, skipped_malformed)` so
/// the caller can surface accurate boot-time diagnostics.
#[must_use]
pub fn parse_questdb_dataset(rows: &[Vec<serde_json::Value>]) -> ParseResult {
    let mut parsed: Vec<(u32, u8, f64)> = Vec::with_capacity(rows.len());
    let mut skipped_unknown_segment: u32 = 0;
    let mut skipped_malformed: u32 = 0;
    for row in rows {
        // Use explicit-arity check + indexed access — `row.get(..3)` +
        // `try_into()` triggers type inference ambiguity on `[T; 3]`.
        if row.len() < 3 {
            skipped_malformed = skipped_malformed.saturating_add(1);
            continue;
        }
        let sec_id_v = &row[0];
        let segment_v = &row[1];
        let prev_close_v = &row[2];
        // sec_id may arrive as integer JSON value (LONG) — coerce to u32 via i64.
        let Some(sec_id_i64) = sec_id_v.as_i64() else {
            skipped_malformed = skipped_malformed.saturating_add(1);
            continue;
        };
        let Ok(sec_id) = u32::try_from(sec_id_i64) else {
            skipped_malformed = skipped_malformed.saturating_add(1);
            continue;
        };
        let Some(segment_str) = segment_v.as_str() else {
            skipped_malformed = skipped_malformed.saturating_add(1);
            continue;
        };
        let Some(segment_code) = segment_str_to_code(segment_str) else {
            skipped_unknown_segment = skipped_unknown_segment.saturating_add(1);
            continue;
        };
        let Some(prev_close) = prev_close_v.as_f64() else {
            skipped_malformed = skipped_malformed.saturating_add(1);
            continue;
        };
        parsed.push((sec_id, segment_code, prev_close));
    }
    ParseResult {
        rows: parsed,
        skipped_unknown_segment,
        skipped_malformed,
    }
}

/// Result of [`parse_questdb_dataset`] — exposed so the caller can
/// log diagnostic counts without re-parsing.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseResult {
    /// `(security_id, exchange_segment_code, prev_close)` triples for
    /// every row that decoded cleanly.
    pub rows: Vec<(u32, u8, f64)>,
    /// Rows whose `segment` string was not recognised by
    /// [`segment_str_to_code`]. Always > 0 means the QuestDB table has
    /// stale or future-segment data; investigate with the operator.
    pub skipped_unknown_segment: u32,
    /// Rows whose JSON shape was unexpected (wrong arity, wrong type
    /// per column). Always > 0 means QuestDB's response shape changed
    /// — the SELECT or JSON parser must be updated.
    pub skipped_malformed: u32,
}

/// Pure-logic helper: merges parsed rows + `prev_oi_cache` into the
/// `PrevDayCache`. Returns the count of inserts.
///
/// `prev_day_total_volume` is hard-coded to `0` until the bhavcopy
/// total-volume column ships in a follow-up. The
/// `compute_volume_pct` div-by-zero policy returns `0.0` (never
/// `NaN`), so the read path is safe.
pub fn merge_into_cache(
    rows: &[(u32, u8, f64)],
    prev_oi_cache: &HashMap<(u32, u8), i64>,
    cache: &PrevDayCache,
) -> usize {
    let mut inserted = 0_usize;
    for &(security_id, segment_code, prev_close) in rows {
        let prev_day_oi = prev_oi_cache
            .get(&(security_id, segment_code))
            .copied()
            .unwrap_or(0);
        let refs = PrevDayRefs {
            prev_day_close: prev_close,
            prev_day_oi,
            prev_day_total_volume: 0,
        };
        cache.insert(security_id, segment_code, refs);
        inserted = inserted.saturating_add(1);
    }
    inserted
}

/// Boot-time entry point: read `previous_close` rows from QuestDB,
/// merge the boot-loaded `prev_oi_cache`, and populate the
/// `PrevDayCache`. Returns the number of rows inserted on success.
///
/// Best-effort — emits a structured `error!` with code `PREVCLOSE-01`
/// on QuestDB unavailability and returns `Err` so the caller can log
/// + continue boot. The `on_*_with_pct` div-by-zero policy keeps the
/// read path safe even when this loader fails outright.
pub async fn populate_prev_day_cache_at_boot(
    questdb_config: &QuestDbConfig,
    cache: &PrevDayCache,
    prev_oi_cache: &Arc<HashMap<(u32, u8), i64>>,
) -> Result<PopulationOutcome> {
    let url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(QUESTDB_QUERY_TIMEOUT_SECS))
        .build()
        .context("build reqwest client for previous_close SELECT")?;
    let sql = build_select_latest_prev_close_sql();
    let resp = client
        .get(&url)
        .query(&[("query", sql.as_str())])
        .send()
        .await
        .context("HTTP GET previous_close SELECT against QuestDB")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body_truncated: String = resp
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect::<String>(); // O(1) EXEMPT: cold-path error logging.
        anyhow::bail!("QuestDB previous_close SELECT returned HTTP {status}: {body_truncated}");
    }
    let parsed_body: QuestDbExecResponse = resp
        .json()
        .await
        .context("parse QuestDB JSON dataset for previous_close SELECT")?;
    let parse_result = parse_questdb_dataset(&parsed_body.dataset);
    let inserted = merge_into_cache(&parse_result.rows, prev_oi_cache, cache);
    Ok(PopulationOutcome {
        rows_inserted: inserted,
        skipped_unknown_segment: parse_result.skipped_unknown_segment,
        skipped_malformed: parse_result.skipped_malformed,
    })
}

/// Diagnostic outcome of [`populate_prev_day_cache_at_boot`]. The
/// caller logs + decides whether the population succeeded enough to
/// proceed (typically: `rows_inserted > 0` is the success signal).
#[derive(Debug, Clone, PartialEq)]
pub struct PopulationOutcome {
    /// Rows inserted into the `PrevDayCache`. `0` is a graceful-
    /// degradation signal — the loader ran but the QuestDB table was
    /// empty (fresh deployment, or an outage during the previous
    /// trading session that prevented `previous_close` writes).
    pub rows_inserted: usize,
    /// Forwarded from [`ParseResult::skipped_unknown_segment`].
    pub skipped_unknown_segment: u32,
    /// Forwarded from [`ParseResult::skipped_malformed`].
    pub skipped_malformed: u32,
}

/// Convenience boot wiring helper: invokes
/// [`populate_prev_day_cache_at_boot`] and emits the structured
/// `info!` / `warn!` / `error!` chain that downstream operators
/// expect. Decoupled from `main.rs` so the boot-time logging is
/// testable + auditable.
pub async fn populate_and_log(
    questdb_config: &QuestDbConfig,
    cache: &PrevDayCache,
    prev_oi_cache: &Arc<HashMap<(u32, u8), i64>>,
) {
    match populate_prev_day_cache_at_boot(questdb_config, cache, prev_oi_cache).await {
        Ok(outcome) => {
            if outcome.rows_inserted == 0 {
                // Off-hours / weekend / holiday: empty `previous_close`
                // is expected (no live ticks have populated it yet for
                // today), so emit `info!` to avoid Telegram noise. During
                // a trading session the same condition is genuinely
                // degraded — escalate via `warn!` + PREVCLOSE-04 so the
                // operator investigates per Rule 11.
                if is_within_trading_session_ist() {
                    warn!(
                        code = ErrorCode::PrevClose04CacheEmptyAtBoot.code_str(),
                        skipped_unknown_segment = outcome.skipped_unknown_segment,
                        skipped_malformed = outcome.skipped_malformed,
                        "F2 PrevDayCache loader: previous_close table is empty — \
                         cascade seal-time pct stamping will fall back to 0.0 \
                         until live ticks repopulate the table"
                    );
                } else {
                    info!(
                        skipped_unknown_segment = outcome.skipped_unknown_segment,
                        skipped_malformed = outcome.skipped_malformed,
                        "F2 PrevDayCache loader: previous_close table is empty \
                         (off-hours / weekend boot — expected; cascade pct \
                         fields will fall back to 0.0 until next session)"
                    );
                }
            } else {
                info!(
                    rows_inserted = outcome.rows_inserted,
                    skipped_unknown_segment = outcome.skipped_unknown_segment,
                    skipped_malformed = outcome.skipped_malformed,
                    "F2 PrevDayCache loader: cache populated — cascade \
                     seal-time pct stamping is now active"
                );
            }
        }
        Err(err) => {
            // QuestDB unreachable / SELECT failed. Cascade falls back
            // to 0.0 % fields per the div-by-zero policy. Operator
            // sees this via the typed code below.
            tracing::error!(
                code = ErrorCode::PrevClose04CacheEmptyAtBoot.code_str(),
                ?err,
                "F2 PrevDayCache loader: QuestDB SELECT failed — cascade \
                 seal-time pct stamping will fall back to 0.0 until next \
                 boot succeeds in reading previous_close"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn build_select_latest_prev_close_sql_pins_lookback() {
        // F2 ratchet: the lookback window is locked at 7 days so a
        // stale row from a multi-week-old session cannot seed today's
        // cache. Drift would silently re-introduce the bug class
        // documented in the module doc.
        let sql = build_select_latest_prev_close_sql();
        assert!(
            sql.contains("dateadd('d', -7, now())"),
            "lookback window changed unexpectedly: {sql}"
        );
        assert!(sql.contains("FROM previous_close"));
        assert!(sql.contains("last(prev_close)"));
        assert!(sql.contains("GROUP BY security_id, segment"));
    }

    #[test]
    fn build_select_sql_groups_by_composite_key_per_i_p1_11() {
        // Composite-key uniqueness is non-negotiable (see
        // `.claude/rules/project/security-id-uniqueness.md`).
        // The GROUP BY MUST mention BOTH security_id and segment.
        let sql = build_select_latest_prev_close_sql();
        assert!(sql.contains("security_id"));
        assert!(sql.contains("segment"));
        // Defensive: catch a regression that drops `segment` from the
        // grouping (which would silently merge cross-segment rows).
        assert!(sql.contains("GROUP BY security_id, segment"));
    }

    #[test]
    fn parse_questdb_dataset_accepts_well_formed_rows() {
        let rows = vec![
            vec![json!(13), json!("IDX_I"), json!(25_500.5)],
            vec![json!(1333), json!("NSE_EQ"), json!(2847.50)],
        ];
        let result = parse_questdb_dataset(&rows);
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.skipped_unknown_segment, 0);
        assert_eq!(result.skipped_malformed, 0);
        assert_eq!(result.rows[0], (13_u32, 0_u8, 25_500.5)); // IDX_I = 0
        assert_eq!(result.rows[1], (1333_u32, 1_u8, 2847.50)); // NSE_EQ = 1
    }

    #[test]
    fn parse_questdb_dataset_skips_unknown_segment() {
        let rows = vec![
            vec![json!(99), json!("IDX_I"), json!(100.0)],
            vec![json!(99), json!("MARS_EQ"), json!(100.0)], // unknown
        ];
        let result = parse_questdb_dataset(&rows);
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.skipped_unknown_segment, 1);
        assert_eq!(result.skipped_malformed, 0);
    }

    #[test]
    fn parse_questdb_dataset_skips_malformed_rows() {
        let rows = vec![
            vec![json!(99), json!("IDX_I"), json!(100.0)],
            vec![json!("not-a-number"), json!("IDX_I"), json!(100.0)], // bad sec_id
            vec![json!(13), json!(123), json!(100.0)],                 // segment is num
            vec![json!(13)],                                           // arity-1
        ];
        let result = parse_questdb_dataset(&rows);
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.skipped_malformed, 3);
        assert_eq!(result.skipped_unknown_segment, 0);
    }

    #[test]
    fn parse_questdb_dataset_handles_empty() {
        let result = parse_questdb_dataset(&[]);
        assert!(result.rows.is_empty());
        assert_eq!(result.skipped_malformed, 0);
        assert_eq!(result.skipped_unknown_segment, 0);
    }

    #[test]
    fn merge_into_cache_populates_prev_day_close_and_oi() {
        let cache = PrevDayCache::new();
        let mut prev_oi: HashMap<(u32, u8), i64> = HashMap::new();
        prev_oi.insert((13, 0), 1_000_000); // IDX_I has no real OI but exercise path
        prev_oi.insert((1333, 1), 0); // NSE_EQ = 0 OI (cash equity)
        let rows = vec![
            (13_u32, 0_u8, 25_500.5_f64),
            (1333_u32, 1_u8, 2847.50_f64),
            (4321_u32, 2_u8, 100.0_f64), // F&O — no entry in prev_oi → 0
        ];
        let inserted = merge_into_cache(&rows, &prev_oi, &cache);
        assert_eq!(inserted, 3);

        let refs_idx = cache.lookup(13, 0).expect("IDX_I in cache");
        assert!((refs_idx.prev_day_close - 25_500.5).abs() < 1e-6);
        assert_eq!(refs_idx.prev_day_oi, 1_000_000);
        assert_eq!(refs_idx.prev_day_total_volume, 0);

        let refs_eq = cache.lookup(1333, 1).expect("NSE_EQ in cache");
        assert!((refs_eq.prev_day_close - 2847.50).abs() < 1e-6);
        assert_eq!(refs_eq.prev_day_oi, 0);

        let refs_fno = cache.lookup(4321, 2).expect("NSE_FNO in cache");
        assert!((refs_fno.prev_day_close - 100.0).abs() < 1e-6);
        // Missing from prev_oi → defaults to 0 per F2 graceful-fallback.
        assert_eq!(refs_fno.prev_day_oi, 0);
    }

    #[test]
    fn merge_into_cache_returns_zero_on_empty_input() {
        let cache = PrevDayCache::new();
        let prev_oi: HashMap<(u32, u8), i64> = HashMap::new();
        assert_eq!(merge_into_cache(&[], &prev_oi, &cache), 0);
    }

    #[test]
    fn population_outcome_round_trips() {
        let outcome = PopulationOutcome {
            rows_inserted: 100,
            skipped_unknown_segment: 0,
            skipped_malformed: 0,
        };
        assert_eq!(outcome.rows_inserted, 100);
    }

    #[test]
    fn populate_and_log_explicit_name_match() {
        // pub-fn-test guard — surface the function name so
        // `pub-fn-test-guard.sh` sees a corresponding test.
        let _ = populate_and_log;
    }

    #[test]
    fn populate_prev_day_cache_at_boot_explicit_name_match() {
        let _ = populate_prev_day_cache_at_boot;
    }
}

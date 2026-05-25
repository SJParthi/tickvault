//! Crash-recovery REHYDRATE of [`CurrentExpiryCache`] from QuestDB's
//! `option_chain_minute_snapshot` table.
//!
//! ## Why this exists (Z+ L3 RECONCILE)
//!
//! If the app crashes mid-session, RAM is lost — including the
//! current-expiry cache populated by the 09:00:30 IST warmup. Without
//! rehydrate, the next per-minute slot would fall back to calling
//! `/optionchain/expirylist` inline (slow + costs a Dhan call).
//!
//! With rehydrate, the boot orchestrator reads the LAST `expiry` per
//! `(underlying_security_id, exchange_segment)` from the QuestDB
//! `option_chain_minute_snapshot` table (which the scheduler writes
//! to every minute), populating the cache in <1s from local disk.
//!
//! ## The 3-tier recovery contract
//!
//! | Order | Source | When | Latency |
//! |---|---|---|---|
//! | 1. REHYDRATE | QuestDB | Boot (always) | <1s |
//! | 2. Warmup | Dhan `/expirylist` | 09:00:30 IST daily | ~18s |
//! | 3. Inline fallback | Dhan `/expirylist` | Per slot if cache miss | ~6s |
//!
//! REHYDRATE is the L3 RECONCILE layer; warmup is L4 PREVENT; inline
//! is L6 RECOVER. Per Z+ doctrine.
//!
//! ## Empty-result semantics
//!
//! - Fresh deploy / first ever run → 0 rows, returns `Ok(0)`. Warmup
//!   at 09:00:30 IST cold-populates.
//! - Yesterday's session completed, no rows in last 24h → 0 rows.
//!   Warmup cold-populates.
//! - Mid-session crash today → returns N rows (1 per underlying).
//!   Cache restored in <1s.
//!
//! ## Authority
//!
//! - operator-charter-forever.md §C / §F — bounded crash-recovery
//!   guarantee; explicit "100% inside the tested envelope".
//! - disaster-recovery.md Scenario 14 (overnight wake) — this loader
//!   restores the cache without paying the Dhan REST cost on Monday
//!   morning boot.
//! - I-P1-11 composite key — query selects on `(underlying_security_id,
//!   exchange_segment)`.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_core::option_chain::current_expiry_cache::CurrentExpiryCache;

/// HTTP timeout for the QuestDB `/exec` SELECT. Mirrors
/// `bar_cache_loader::BAR_CACHE_QUERY_TIMEOUT_SECS`. Cold path.
const QUESTDB_QUERY_TIMEOUT_SECS: u64 = 10;

/// Lookback window for the rehydrate SELECT — only restore cache
/// entries written in the last N hours. 26h gives one full trading
/// day of headroom (markets close 15:30 IST; next boot might happen
/// just before next 09:00:30 IST warmup ~17.5h later).
const REHYDRATE_LOOKBACK_HOURS: u32 = 26;

/// Partial deserialization of QuestDB's `/exec` JSON response.
#[derive(Debug, Deserialize)]
struct QuestDbExecResponse {
    dataset: serde_json::Value,
}

/// Outcome of one rehydrate call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RehydrateOutcome {
    /// Number of distinct (underlying_security_id, exchange_segment)
    /// pairs restored into the cache.
    pub rows_inserted: usize,
    /// Number of rows skipped because the row's `expiry` field was
    /// missing, NULL, or non-string. Should be 0 in normal operation.
    pub skipped_malformed: usize,
}

/// Builds the SELECT that returns the LAST `expiry` per underlying
/// from the `option_chain_minute_snapshot` table.
///
/// Restricted to the last `REHYDRATE_LOOKBACK_HOURS` hours so a stale
/// week-old row doesn't repopulate the cache with an expired contract.
///
/// O(1) string construction. Public for test coverage.
#[must_use]
pub fn build_rehydrate_select_sql() -> String {
    format!(
        "SELECT underlying_security_id, exchange_segment, expiry \
         FROM option_chain_minute_snapshot \
         WHERE ts > dateadd('h', -{REHYDRATE_LOOKBACK_HOURS}, now()) \
         LATEST ON ts PARTITION BY underlying_security_id, exchange_segment;"
    )
}

/// Issues the rehydrate SELECT against QuestDB and populates the
/// in-memory cache. Returns a count + skipped-malformed summary.
///
/// # Errors
///
/// Network unreachable, non-2xx response, or malformed JSON dataset
/// all bubble up via `anyhow::Result` — the boot orchestrator
/// decides whether to log + continue (recommended) or halt.
///
/// # Performance
///
/// Cold path — called once at boot. ~50ms for the round-trip + parse,
/// plus 3 `cache.insert` calls (~150ns total). Negligible vs the
/// QuestDB boot probe (which can take seconds on a cold instance).
// TEST-EXEMPT: requires live QuestDB /exec endpoint; pure helpers `build_rehydrate_select_sql` + `parse_and_merge` + `segment_str_to_code` are covered by 9 unit tests below.
pub async fn rehydrate_current_expiry_cache_at_boot(
    questdb_config: &QuestDbConfig,
    cache: &CurrentExpiryCache,
) -> Result<RehydrateOutcome> {
    let url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(QUESTDB_QUERY_TIMEOUT_SECS))
        .build()
        .context("build reqwest client for option_chain rehydrate SELECT")?;
    let sql = build_rehydrate_select_sql();
    let resp = client
        .get(&url)
        .query(&[("query", sql.as_str())])
        .send()
        .await
        .context("HTTP GET option_chain rehydrate SELECT against QuestDB")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body_truncated: String = resp
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect::<String>(); // O(1) EXEMPT: cold-path error logging
        anyhow::bail!(
            "QuestDB option_chain rehydrate SELECT returned HTTP {status}: {body_truncated}"
        );
    }
    let parsed: QuestDbExecResponse = resp
        .json()
        .await
        .context("parse QuestDB JSON dataset for option_chain rehydrate")?;
    Ok(parse_and_merge(&parsed.dataset, cache))
}

/// Pure helper — parses a QuestDB `/exec` dataset (array of arrays)
/// and inserts each row into the cache. Public for test coverage.
///
/// Expected row shape:
///   `[underlying_security_id: i64, exchange_segment: String, expiry: String]`
///
/// Rows with the wrong shape are skipped and counted in
/// `skipped_malformed` — we never panic on partial data.
#[must_use]
pub fn parse_and_merge(
    dataset: &serde_json::Value,
    cache: &CurrentExpiryCache,
) -> RehydrateOutcome {
    let mut rows_inserted = 0usize;
    let mut skipped_malformed = 0usize;

    let Some(rows) = dataset.as_array() else {
        return RehydrateOutcome {
            rows_inserted: 0,
            skipped_malformed: 0,
        };
    };

    for row in rows {
        let Some(cols) = row.as_array() else {
            skipped_malformed = skipped_malformed.saturating_add(1);
            continue;
        };
        if cols.len() < 3 {
            skipped_malformed = skipped_malformed.saturating_add(1);
            continue;
        }
        let Some(security_id_i64) = cols[0].as_i64() else {
            skipped_malformed = skipped_malformed.saturating_add(1);
            continue;
        };
        let Some(segment_str) = cols[1].as_str() else {
            skipped_malformed = skipped_malformed.saturating_add(1);
            continue;
        };
        let Some(expiry_str) = cols[2].as_str() else {
            skipped_malformed = skipped_malformed.saturating_add(1);
            continue;
        };

        // Convert segment string to byte code. The minute-snapshot
        // table stores segment as a SYMBOL like "IDX_I"; the cache
        // key is the byte code.
        let Some(segment_code) = segment_str_to_code(segment_str) else {
            skipped_malformed = skipped_malformed.saturating_add(1);
            continue;
        };

        // Bounds-check the security_id cast. SecurityId is u32 per
        // I-P1-11; any QuestDB row with security_id > u32::MAX is
        // corruption.
        let Ok(security_id_u32) = u32::try_from(security_id_i64) else {
            skipped_malformed = skipped_malformed.saturating_add(1);
            continue;
        };

        cache.insert(security_id_u32, segment_code, expiry_str);
        rows_inserted = rows_inserted.saturating_add(1);
    }

    RehydrateOutcome {
        rows_inserted,
        skipped_malformed,
    }
}

/// Maps the QuestDB SYMBOL string back to its byte code, mirroring
/// `tickvault_common::segment::segment_code_to_str` in reverse.
///
/// Only the 4 codes that show up in `option_chain_minute_snapshot`
/// rows today are handled; unknown segments return `None` and the
/// row is skipped (counted as malformed).
#[must_use]
pub fn segment_str_to_code(s: &str) -> Option<u8> {
    match s {
        "IDX_I" => Some(0),
        "NSE_EQ" => Some(1),
        "NSE_FNO" => Some(2),
        "BSE_FNO" => Some(8),
        _ => None,
    }
}

/// Convenience boot wrapper that calls
/// [`rehydrate_current_expiry_cache_at_boot`] and emits the
/// structured `info!` / `warn!` / `error!` chain the operator
/// expects. Mirrors `bar_cache_loader::populate_and_log`.
// TEST-EXEMPT: thin async wrapper around `rehydrate_current_expiry_cache_at_boot`; logging branches are operator-readable and pinned by the source-scan ratchet in `option_chain_warmup_wiring.rs`.
pub async fn rehydrate_and_log(questdb_config: &QuestDbConfig, cache: &CurrentExpiryCache) {
    match rehydrate_current_expiry_cache_at_boot(questdb_config, cache).await {
        Ok(outcome) => {
            if outcome.rows_inserted == 0 {
                info!(
                    skipped_malformed = outcome.skipped_malformed,
                    "option-chain current-expiry cache REHYDRATE: 0 rows found \
                     (fresh deploy or pre-09:00:30 boot) — warmup task will cold-populate"
                );
            } else {
                info!(
                    rows_inserted = outcome.rows_inserted,
                    skipped_malformed = outcome.skipped_malformed,
                    "option-chain current-expiry cache REHYDRATE: restored from QuestDB \
                     (no Dhan REST cost; warmup task will refresh at next 09:00:30 IST)"
                );
            }
            if outcome.skipped_malformed > 0 {
                warn!(
                    skipped_malformed = outcome.skipped_malformed,
                    "option-chain rehydrate skipped malformed QuestDB rows — investigate \
                     table schema drift if this number is non-zero in a clean deploy"
                );
            }
        }
        Err(err) => {
            error!(
                error = %err,
                "option-chain current-expiry cache REHYDRATE failed — falling back to \
                 09:00:30 IST warmup. Strategy will see cache miss until then."
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NIFTY: u32 = 13;
    const BANKNIFTY: u32 = 25;
    const SENSEX: u32 = 51;
    const IDX_I: u8 = 0;

    #[test]
    fn test_build_rehydrate_select_sql_includes_lookback_filter() {
        let sql = build_rehydrate_select_sql();
        assert!(
            sql.contains("option_chain_minute_snapshot"),
            "must target the correct table"
        );
        assert!(
            sql.contains("dateadd('h', -26"),
            "must filter to last 26h — prevents stale week-old rows"
        );
        assert!(
            sql.contains("LATEST ON"),
            "must use LATEST ON to dedup to one row per underlying"
        );
        assert!(
            sql.contains("PARTITION BY underlying_security_id, exchange_segment"),
            "must partition by composite key per I-P1-11"
        );
    }

    #[test]
    fn test_segment_str_to_code_known_segments() {
        assert_eq!(segment_str_to_code("IDX_I"), Some(0));
        assert_eq!(segment_str_to_code("NSE_EQ"), Some(1));
        assert_eq!(segment_str_to_code("NSE_FNO"), Some(2));
        assert_eq!(segment_str_to_code("BSE_FNO"), Some(8));
    }

    #[test]
    fn test_segment_str_to_code_unknown_returns_none() {
        assert_eq!(segment_str_to_code(""), None);
        assert_eq!(segment_str_to_code("UNKNOWN"), None);
        assert_eq!(segment_str_to_code("idx_i"), None, "case-sensitive match");
    }

    #[test]
    fn test_parse_and_merge_happy_path_3_underlyings() {
        let dataset = json!([
            [13, "IDX_I", "2026-05-26"],
            [25, "IDX_I", "2026-05-26"],
            [51, "IDX_I", "2026-05-27"],
        ]);
        let cache = CurrentExpiryCache::new();
        let outcome = parse_and_merge(&dataset, &cache);
        assert_eq!(outcome.rows_inserted, 3);
        assert_eq!(outcome.skipped_malformed, 0);
        assert_eq!(&*cache.get(NIFTY, IDX_I).unwrap(), "2026-05-26");
        assert_eq!(&*cache.get(BANKNIFTY, IDX_I).unwrap(), "2026-05-26");
        assert_eq!(&*cache.get(SENSEX, IDX_I).unwrap(), "2026-05-27");
    }

    #[test]
    fn test_parse_and_merge_empty_dataset_returns_zero() {
        let dataset = json!([]);
        let cache = CurrentExpiryCache::new();
        let outcome = parse_and_merge(&dataset, &cache);
        assert_eq!(outcome.rows_inserted, 0);
        assert_eq!(outcome.skipped_malformed, 0);
        assert!(cache.is_empty());
    }

    #[test]
    fn test_parse_and_merge_non_array_dataset_returns_zero() {
        // QuestDB normally returns a JSON array, but a degraded
        // response could return an object or string. Must not panic.
        let dataset = json!({"error": "oops"});
        let cache = CurrentExpiryCache::new();
        let outcome = parse_and_merge(&dataset, &cache);
        assert_eq!(outcome.rows_inserted, 0);
        assert_eq!(outcome.skipped_malformed, 0);
    }

    #[test]
    fn test_parse_and_merge_skips_malformed_rows() {
        let dataset = json!([
            [13, "IDX_I", "2026-05-26"],           // good
            [25],                                  // too few cols
            [51, "UNKNOWN_SEG", "2026-05-27"],     // unknown segment
            ["not-an-int", "IDX_I", "2026-05-26"], // wrong type for sid
            [13, 123, "2026-05-26"],               // wrong type for segment
            [13, "IDX_I", null],                   // null expiry
        ]);
        let cache = CurrentExpiryCache::new();
        let outcome = parse_and_merge(&dataset, &cache);
        assert_eq!(outcome.rows_inserted, 1, "only the first row is valid");
        assert_eq!(outcome.skipped_malformed, 5);
        assert!(cache.get(NIFTY, IDX_I).is_some());
    }

    #[test]
    fn test_parse_and_merge_security_id_overflow_skipped() {
        let huge_sid = i64::MAX;
        let dataset = json!([[huge_sid, "IDX_I", "2026-05-26"]]);
        let cache = CurrentExpiryCache::new();
        let outcome = parse_and_merge(&dataset, &cache);
        assert_eq!(outcome.rows_inserted, 0);
        assert_eq!(outcome.skipped_malformed, 1, "u32 overflow is malformed");
    }

    #[test]
    fn test_parse_and_merge_thursday_rollover_overwrites_cache() {
        // Mid-session crash on Wed: cache has Thursday May 28 expiry.
        // App restarts on Fri morning AFTER warmup re-fetched (which
        // happened pre-this-rehydrate-call). The rehydrate sees a
        // newer row written by the new day's warmup (e.g. Thursday
        // June 4) and overwrites correctly.
        let cache = CurrentExpiryCache::new();
        let day1 = json!([[13, "IDX_I", "2026-05-28"]]);
        parse_and_merge(&day1, &cache);
        assert_eq!(&*cache.get(NIFTY, IDX_I).unwrap(), "2026-05-28");

        let day2 = json!([[13, "IDX_I", "2026-06-04"]]);
        let outcome = parse_and_merge(&day2, &cache);
        assert_eq!(outcome.rows_inserted, 1);
        assert_eq!(
            &*cache.get(NIFTY, IDX_I).unwrap(),
            "2026-06-04",
            "rehydrate must overwrite the previous cached expiry"
        );
    }
}

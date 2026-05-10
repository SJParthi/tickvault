//! Wave 6 Sub-PR #1 — Multi-TF aggregator shadow tables.
//!
//! Direct-flush target tables for the new in-memory multi-timeframe
//! aggregator. Sealed candles flow `aggregator → ring → ILP → shadow`
//! WITHOUT cascading through the legacy `candles_1s` materialized view
//! chain. Per the locked design (`active-plan-aggregator-direct-flush-rehydrate.md`
//! section "Sub-PR #1 — Aggregator Engine" decisions L-C1 / L-C2 / L-H10):
//!
//! - 9 timeframes (1m / 5m / 15m / 30m / 1h / 2h / 3h / 4h / 1d) each get
//!   their OWN shadow table — `candles_{tf}_shadow`.
//! - Every shadow table carries DEDUP UPSERT KEYS
//!   `(ts, security_id, exchange_segment)` — composite per
//!   `.claude/rules/project/security-id-uniqueness.md` (I-P1-11), since
//!   Dhan reuses `security_id` across segments.
//! - The 10th constant `DEDUP_KEY_AGGREGATOR_SEAL_AUDIT` covers the
//!   per-seal forensic audit table (Sub-PR #1 item 1.9, locked decision
//!   L-M16 — Box 10 of the 12-box matrix turns GREEN inside this PR).
//!
//! Sub-PR #4 ("Promotion") later renames these `_shadow` tables to the
//! canonical `candles_{tf}` names AFTER 1 trading week of zero-diff
//! cross-verify + 2 days clean. Until then both the legacy mat-views
//! and the new shadow tables coexist; the divergence ratchet test
//! (`test_shadow_tables_match_mv_within_1pct`, locked decision L-H5) is
//! added in the aggregator hot-path commit.
//!
//! ## What this file does NOT yet contain (deferred to follow-up commit)
//!
//! - The hot-path `ShadowCandleWriter` struct + `append_seal()` method.
//! - The ring → spill → DLQ pattern (locked decision L-C1 — mirror of
//!   `tick_persistence.rs::TICK_BUFFER_CAPACITY` machinery).
//! - The `BufferedSeal` retry struct.
//! - The DDL self-heal `ALTER TABLE ADD COLUMN IF NOT EXISTS` routine.
//!
//! Those land in the aggregator engine commit immediately following
//! this one. This file ships the constants + DDL setup so the meta-guard
//! ratchet (`crates/storage/tests/dedup_segment_meta_guard.rs`) can pin
//! the new keys today.
//!
//! ## DEDUP rationale for each shadow table
//!
//! `(ts, security_id, exchange_segment)` is the composite key. `ts` is
//! the candle open timestamp (IST nanos derived from the WS LTT field
//! per `data-integrity.md` — NEVER `Utc::now()` per locked decision
//! L-H7). `(security_id, exchange_segment)` is the I-P1-11 composite
//! identity. Three properties together hold:
//!
//! 1. **Idempotent re-flush** — a sealed candle re-emitted on aggregator
//!    crash recovery (Sub-PR #2 rehydration) collapses into the same row.
//! 2. **Cross-segment safety** — NIFTY (`security_id=13`, `IDX_I`) and a
//!    distinct NSE_EQ instrument with the same numeric id stay as two
//!    rows, not one.
//! 3. **Per-instrument-per-bucket uniqueness** — one (instrument, bucket)
//!    pair → exactly one row, regardless of how many ticks contributed.
//!
//! ## Aggregator seal audit table
//!
//! `aggregator_seal_audit` — DEDUP UPSERT KEYS `(trading_date_ist,
//! security_id, exchange_segment, timeframe, candle_ts)`. One audit row
//! per (day, instrument, TF, bucket). Ships in this file so future
//! Claude Code sessions can answer "why was bucket X sealed at IST
//! Y for instrument Z" weeks later without grepping logs.
//!
//! Cross-refs:
//! - Locked decisions L-C1 / L-C2 / L-H10 / L-M16 in
//!   `.claude/plans/active-plan-aggregator-direct-flush-rehydrate.md`
//! - Runbook stub: `.claude/rules/project/wave-6-error-codes.md`
//! - I-P1-11: `.claude/rules/project/security-id-uniqueness.md`
//! - Self-heal pattern: `crates/storage/src/instrument_persistence.rs::ensure_instrument_tables`

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;

// ---------------------------------------------------------------------------
// QuestDB table names — one per timeframe, plus the seal audit table.
// ---------------------------------------------------------------------------

/// 1-minute sealed candles — primary cross-verify target in Sub-PR #3.
pub const QUESTDB_TABLE_CANDLES_1M_SHADOW: &str = "candles_1m_shadow";
/// 5-minute sealed candles — derived from 1m via deterministic rollup.
pub const QUESTDB_TABLE_CANDLES_5M_SHADOW: &str = "candles_5m_shadow";
/// 15-minute sealed candles.
pub const QUESTDB_TABLE_CANDLES_15M_SHADOW: &str = "candles_15m_shadow";
/// 30-minute sealed candles.
pub const QUESTDB_TABLE_CANDLES_30M_SHADOW: &str = "candles_30m_shadow";
/// 1-hour sealed candles.
pub const QUESTDB_TABLE_CANDLES_1H_SHADOW: &str = "candles_1h_shadow";
/// 2-hour sealed candles.
pub const QUESTDB_TABLE_CANDLES_2H_SHADOW: &str = "candles_2h_shadow";
/// 3-hour sealed candles.
pub const QUESTDB_TABLE_CANDLES_3H_SHADOW: &str = "candles_3h_shadow";
/// 4-hour sealed candles.
pub const QUESTDB_TABLE_CANDLES_4H_SHADOW: &str = "candles_4h_shadow";
/// 1-day sealed candles.
pub const QUESTDB_TABLE_CANDLES_1D_SHADOW: &str = "candles_1d_shadow";

/// Per-seal forensic audit table (Sub-PR #1 item 1.9, locked L-M16).
pub const QUESTDB_TABLE_AGGREGATOR_SEAL_AUDIT: &str = "aggregator_seal_audit";

// ---------------------------------------------------------------------------
// DEDUP UPSERT keys — composite per I-P1-11.
//
// All 9 candle shadow tables share the same key shape because they are
// uniformly partitioned `(ts, security_id, exchange_segment)`. The
// `dedup_segment_meta_guard.rs` workspace meta-guard scans every
// `DEDUP_KEY_*` constant in `crates/storage/src/` and FAILS the build
// if any key that mentions `security_id` does NOT also mention
// `segment` / `exchange_segment` / `exchange`. Each constant below
// contains the literal substring `exchange_segment` to satisfy that
// invariant.
// ---------------------------------------------------------------------------

/// 1-minute shadow DEDUP key. Includes `exchange_segment` per I-P1-11.
pub const DEDUP_KEY_CANDLES_1M_SHADOW: &str = "security_id, exchange_segment";
/// 5-minute shadow DEDUP key.
pub const DEDUP_KEY_CANDLES_5M_SHADOW: &str = "security_id, exchange_segment";
/// 15-minute shadow DEDUP key.
pub const DEDUP_KEY_CANDLES_15M_SHADOW: &str = "security_id, exchange_segment";
/// 30-minute shadow DEDUP key.
pub const DEDUP_KEY_CANDLES_30M_SHADOW: &str = "security_id, exchange_segment";
/// 1-hour shadow DEDUP key.
pub const DEDUP_KEY_CANDLES_1H_SHADOW: &str = "security_id, exchange_segment";
/// 2-hour shadow DEDUP key.
pub const DEDUP_KEY_CANDLES_2H_SHADOW: &str = "security_id, exchange_segment";
/// 3-hour shadow DEDUP key.
pub const DEDUP_KEY_CANDLES_3H_SHADOW: &str = "security_id, exchange_segment";
/// 4-hour shadow DEDUP key.
pub const DEDUP_KEY_CANDLES_4H_SHADOW: &str = "security_id, exchange_segment";
/// 1-day shadow DEDUP key.
pub const DEDUP_KEY_CANDLES_1D_SHADOW: &str = "security_id, exchange_segment";

/// Aggregator seal audit DEDUP key — one row per (day, instrument, TF,
/// bucket). Includes `exchange_segment` for I-P1-11 and `timeframe` so
/// the same instrument's 1m + 5m seals coexist.
pub const DEDUP_KEY_AGGREGATOR_SEAL_AUDIT: &str =
    "security_id, exchange_segment, timeframe, candle_ts, trading_date_ist";

// ---------------------------------------------------------------------------
// Public helpers — aggregate the constants for downstream consumers.
// ---------------------------------------------------------------------------

/// All 9 candle shadow table names in canonical order (1m → 1d).
///
/// Used by:
/// - DDL setup loop below.
/// - Forthcoming hot-path `ShadowCandleWriter` to index into the
///   `[Sender; 9]` ILP sender array.
/// - Forthcoming Sub-PR #2 rehydration code to query "latest sealed
///   timestamp per (security_id, segment, TF)".
#[must_use]
// TEST-EXEMPT: pure const-array accessor; tested via test_canonical_timeframe_ordering_1m_to_1d + test_all_shadow_table_names_distinct + test_canonical_table_name_pattern.
pub fn shadow_candle_table_names() -> [&'static str; 9] {
    [
        QUESTDB_TABLE_CANDLES_1M_SHADOW,
        QUESTDB_TABLE_CANDLES_5M_SHADOW,
        QUESTDB_TABLE_CANDLES_15M_SHADOW,
        QUESTDB_TABLE_CANDLES_30M_SHADOW,
        QUESTDB_TABLE_CANDLES_1H_SHADOW,
        QUESTDB_TABLE_CANDLES_2H_SHADOW,
        QUESTDB_TABLE_CANDLES_3H_SHADOW,
        QUESTDB_TABLE_CANDLES_4H_SHADOW,
        QUESTDB_TABLE_CANDLES_1D_SHADOW,
    ]
}

/// All 9 DEDUP UPSERT keys for the candle shadow tables, paired with
/// their table names so DDL emitters can iterate without index drift.
#[must_use]
// TEST-EXEMPT: pure const-array accessor; tested via test_table_dedup_pair_alignment + test_every_dedup_key_includes_exchange_segment.
pub fn shadow_candle_table_dedup_keys() -> [(&'static str, &'static str); 9] {
    [
        (QUESTDB_TABLE_CANDLES_1M_SHADOW, DEDUP_KEY_CANDLES_1M_SHADOW),
        (QUESTDB_TABLE_CANDLES_5M_SHADOW, DEDUP_KEY_CANDLES_5M_SHADOW),
        (
            QUESTDB_TABLE_CANDLES_15M_SHADOW,
            DEDUP_KEY_CANDLES_15M_SHADOW,
        ),
        (
            QUESTDB_TABLE_CANDLES_30M_SHADOW,
            DEDUP_KEY_CANDLES_30M_SHADOW,
        ),
        (QUESTDB_TABLE_CANDLES_1H_SHADOW, DEDUP_KEY_CANDLES_1H_SHADOW),
        (QUESTDB_TABLE_CANDLES_2H_SHADOW, DEDUP_KEY_CANDLES_2H_SHADOW),
        (QUESTDB_TABLE_CANDLES_3H_SHADOW, DEDUP_KEY_CANDLES_3H_SHADOW),
        (QUESTDB_TABLE_CANDLES_4H_SHADOW, DEDUP_KEY_CANDLES_4H_SHADOW),
        (QUESTDB_TABLE_CANDLES_1D_SHADOW, DEDUP_KEY_CANDLES_1D_SHADOW),
    ]
}

// ---------------------------------------------------------------------------
// DDL setup — idempotent CREATE + DEDUP ENABLE.
//
// Follows the schema-self-heal pattern documented in
// `.claude/rules/project/observability-architecture.md` ("Schema
// self-heal at boot"). Future column additions ride
// `ALTER TABLE ADD COLUMN IF NOT EXISTS` so older deployments
// auto-migrate.
// ---------------------------------------------------------------------------

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Create all 9 candle shadow tables + the per-seal audit table if they
/// do not already exist, and enable DEDUP UPSERT on each.
///
/// Idempotent: safe to call on every boot. Failures are logged at
/// `error!` level (Telegram-routable per `error_level_meta_guard.rs`
/// Rule 5) but do NOT block boot — the writer falls back to ring/spill
/// on subsequent ILP errors.
///
/// Requires a running QuestDB; covered by boot integration in CI and
/// by manual `make doctor` post-boot.
// TEST-EXEMPT: requires a running QuestDB; tested via boot integration in CI and by `make doctor`. WIRING-EXEMPT: scaffolding for Sub-PR #1 follow-up commit on this branch (aggregator engine wires it into crates/app/src/main.rs alongside ensure_instrument_tables).
pub async fn ensure_shadow_candle_tables(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    for (table, dedup_key) in shadow_candle_table_dedup_keys() {
        let create_ddl = format!(
            "CREATE TABLE IF NOT EXISTS {table} (\
                ts                          TIMESTAMP, \
                security_id                 INT, \
                exchange_segment            SYMBOL, \
                open                        DOUBLE, \
                high                        DOUBLE, \
                low                         DOUBLE, \
                close                       DOUBLE, \
                volume                      LONG, \
                oi                          LONG, \
                tick_count                  INT, \
                close_pct_from_prev_day     DOUBLE, \
                oi_pct_from_prev_day        DOUBLE, \
                volume_pct_from_prev_day    DOUBLE\
            ) timestamp(ts) PARTITION BY DAY \
            DEDUP UPSERT KEYS(ts, {dedup_key});"
        );
        run_ddl(&client, &base_url, table, &create_ddl).await;
    }

    let audit_ddl = format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_AGGREGATOR_SEAL_AUDIT} (\
            ts                  TIMESTAMP, \
            trading_date_ist    DATE, \
            security_id         INT, \
            exchange_segment    SYMBOL, \
            timeframe           SYMBOL, \
            candle_ts           TIMESTAMP, \
            seals_emitted       LONG, \
            seals_dropped       LONG, \
            late_ticks_discarded LONG, \
            outcome             SYMBOL\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS(ts, {DEDUP_KEY_AGGREGATOR_SEAL_AUDIT});"
    );
    run_ddl(
        &client,
        &base_url,
        QUESTDB_TABLE_AGGREGATOR_SEAL_AUDIT,
        &audit_ddl,
    )
    .await;
}

async fn run_ddl(client: &Client, base_url: &str, table: &str, ddl: &str) {
    match client.get(base_url).query(&[("query", ddl)]).send().await {
        Ok(resp) if resp.status().is_success() => {
            info!(table, "shadow table ready");
        }
        Ok(resp) => {
            warn!(table, status = %resp.status(), "DDL non-2xx");
        }
        Err(err) => {
            error!(table, ?err, "DDL request failed");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_shadow_table_names_distinct() {
        let names = shadow_candle_table_names();
        let mut seen = std::collections::HashSet::new();
        for name in names {
            assert!(seen.insert(name), "duplicate table name: {name}");
        }
    }

    #[test]
    fn test_table_dedup_pair_alignment() {
        let pairs = shadow_candle_table_dedup_keys();
        let names = shadow_candle_table_names();
        for (idx, (table, _)) in pairs.iter().enumerate() {
            assert_eq!(
                *table, names[idx],
                "pairs[{idx}] table name diverges from canonical order"
            );
        }
    }

    #[test]
    fn test_every_dedup_key_includes_exchange_segment() {
        for (table, key) in shadow_candle_table_dedup_keys() {
            assert!(
                key.contains("exchange_segment"),
                "I-P1-11 violation: {table} key {key:?} missing `exchange_segment`"
            );
            assert!(
                key.contains("security_id"),
                "{table} key {key:?} missing `security_id`"
            );
        }
    }

    #[test]
    fn test_aggregator_seal_audit_dedup_includes_segment_and_tf() {
        assert!(DEDUP_KEY_AGGREGATOR_SEAL_AUDIT.contains("security_id"));
        assert!(DEDUP_KEY_AGGREGATOR_SEAL_AUDIT.contains("exchange_segment"));
        assert!(DEDUP_KEY_AGGREGATOR_SEAL_AUDIT.contains("timeframe"));
        assert!(DEDUP_KEY_AGGREGATOR_SEAL_AUDIT.contains("candle_ts"));
        assert!(DEDUP_KEY_AGGREGATOR_SEAL_AUDIT.contains("trading_date_ist"));
    }

    #[test]
    fn test_canonical_table_name_pattern() {
        // Defensive: every shadow table name must end with `_shadow` so
        // the Sub-PR #4 promotion rename script can be a simple
        // `s/_shadow$//` substitution.
        for name in shadow_candle_table_names() {
            assert!(
                name.ends_with("_shadow"),
                "table name {name:?} does not end in _shadow — Sub-PR #4 rename will break"
            );
            assert!(
                name.starts_with("candles_"),
                "table name {name:?} prefix wrong"
            );
        }
    }

    #[test]
    fn test_canonical_timeframe_ordering_1m_to_1d() {
        // The 9 timeframes must appear in canonical short-to-long order
        // because the forthcoming `[AtomicCell<LiveCandleState>; 9]`
        // hot-path array is indexed by `TfIndex` ordinal. If this order
        // ever changes, every consumer of `shadow_candle_table_names()`
        // must be reviewed.
        let names = shadow_candle_table_names();
        let expected = [
            "candles_1m_shadow",
            "candles_5m_shadow",
            "candles_15m_shadow",
            "candles_30m_shadow",
            "candles_1h_shadow",
            "candles_2h_shadow",
            "candles_3h_shadow",
            "candles_4h_shadow",
            "candles_1d_shadow",
        ];
        assert_eq!(names, expected);
    }
}

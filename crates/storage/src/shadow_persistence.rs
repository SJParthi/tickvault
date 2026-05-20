//! Candle-engine re-architecture #T1a — plain QuestDB candle tables.
//!
//! Direct-flush target tables for the live multi-timeframe aggregator.
//! Sealed candles flow `aggregator → ring → ILP → candles_<tf>` WITHOUT
//! cascading through the legacy `candles_1s` materialized-view chain and
//! WITHOUT the interim `_shadow` tables of the Wave 6 design.
//!
//! - 21 timeframes (1m / 2m / … / 15m / 30m / 1h / 2h / 3h / 4h / 1d)
//!   each get their OWN plain QuestDB table — `candles_<tf>`. Names are
//!   derived from `TfIndex::table_name()` (the single source of truth).
//! - Every candle table carries DEDUP UPSERT KEYS
//!   `(ts, security_id, segment)` — composite per
//!   `.claude/rules/project/security-id-uniqueness.md` (I-P1-11), since
//!   Dhan reuses `security_id` across segments. The designated
//!   timestamp `ts` is part of the key (QuestDB requires it).
//! - The 22nd constant `DEDUP_KEY_AGGREGATOR_SEAL_AUDIT` covers the
//!   per-seal forensic audit table.
//!
//! ## Schema (10 columns, NO pct columns)
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS candles_1m (
//!     segment       SYMBOL,
//!     security_id   INT,
//!     ts            TIMESTAMP,
//!     open          DOUBLE,
//!     high          DOUBLE,
//!     low           DOUBLE,
//!     close         DOUBLE,
//!     volume        LONG,
//!     oi            LONG,
//!     tick_count    INT
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, security_id, segment);
//! ```
//!
//! The 3 `*_pct_from_prev_day` DOUBLE columns of the legacy shadow
//! schema are intentionally dropped — the live aggregator no longer
//! stamps prev-day pct values at seal time.
//!
//! ## DEDUP rationale for each candle table
//!
//! `(ts, security_id, segment)` is the composite key. `ts` is the
//! candle open timestamp (IST nanos derived from the WS LTT field per
//! `data-integrity.md` — NEVER `Utc::now()`). `(security_id, segment)`
//! is the I-P1-11 composite identity. Three properties hold together:
//!
//! 1. **Idempotent re-flush** — a sealed candle re-emitted on aggregator
//!    crash recovery collapses into the same row.
//! 2. **Cross-segment safety** — NIFTY (`security_id=13`, `IDX_I`) and a
//!    distinct NSE_EQ instrument with the same numeric id stay as two
//!    rows, not one.
//! 3. **Per-instrument-per-bucket uniqueness** — one (instrument, bucket)
//!    pair → exactly one row, regardless of how many ticks contributed.
//!
//! ## Aggregator seal audit table
//!
//! `aggregator_seal_audit` — DEDUP UPSERT KEYS `(ts, security_id,
//! exchange_segment, timeframe, candle_ts, trading_date_ist)`. One audit
//! row per (day, instrument, TF, bucket).
//!
//! Cross-refs:
//! - I-P1-11: `.claude/rules/project/security-id-uniqueness.md`
//! - Self-heal pattern: `crates/storage/src/instrument_persistence.rs::ensure_instrument_tables`

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_trading::candles::{TF_COUNT, TfIndex};

// ---------------------------------------------------------------------------
// QuestDB table names — one per timeframe, plus the seal audit table.
//
// The 21 candle table names are derived from `TfIndex::table_name()`
// (the single source of truth in `crates/trading/src/candles/tf_index.rs`).
// Use `candle_table_names()` below to enumerate them.
// ---------------------------------------------------------------------------

/// Per-seal forensic audit table.
pub const QUESTDB_TABLE_AGGREGATOR_SEAL_AUDIT: &str = "aggregator_seal_audit";

// ---------------------------------------------------------------------------
// DEDUP UPSERT keys — composite per I-P1-11.
//
// All 21 candle tables share the same key shape because they are
// uniformly partitioned `(ts, security_id, segment)`. The
// `dedup_segment_meta_guard.rs` workspace meta-guard scans every
// `DEDUP_KEY_*` constant in `crates/storage/src/` and FAILS the build
// if any key that mentions `security_id` does NOT also mention
// `segment` / `exchange_segment` / `exchange`. The constant below
// contains the literal substring `segment` to satisfy that invariant.
// ---------------------------------------------------------------------------

/// DEDUP UPSERT key for every candle table. Includes the designated
/// timestamp `ts` (QuestDB requires it) and `segment` per I-P1-11. The
/// same value is returned by `TfIndex::dedup_key()`.
pub const DEDUP_KEY_CANDLES: &str = "ts, security_id, segment";

/// Aggregator seal audit DEDUP key — one row per (day, instrument, TF,
/// bucket). Includes `exchange_segment` for I-P1-11 and `timeframe` so
/// the same instrument's 1m + 5m seals coexist.
pub const DEDUP_KEY_AGGREGATOR_SEAL_AUDIT: &str =
    "security_id, exchange_segment, timeframe, candle_ts, trading_date_ist";

// ---------------------------------------------------------------------------
// Public helpers — aggregate the table names for downstream consumers.
// ---------------------------------------------------------------------------

/// All 21 candle table names in canonical order (1m → 1d), derived from
/// `TfIndex::table_name()`.
///
/// Used by:
/// - the DDL setup loop below.
/// - the hot-path seal-writer chain to index into the `[Sender; 21]`
///   ILP sender array.
#[must_use]
// TEST-EXEMPT: pure const-array accessor; tested via test_candle_table_names_are_plain_and_canonical + test_all_candle_table_names_distinct.
pub fn candle_table_names() -> [&'static str; TF_COUNT] {
    let mut names = [""; TF_COUNT];
    let mut idx = 0;
    while idx < TF_COUNT {
        // `TfIndex::from_ordinal` is const and total over `0..TF_COUNT`.
        if let Some(tf) = TfIndex::from_ordinal(idx) {
            names[idx] = tf.table_name();
        }
        idx += 1;
    }
    names
}

// ---------------------------------------------------------------------------
// DDL setup — idempotent CREATE + DEDUP UPSERT.
//
// Follows the schema-self-heal pattern documented in
// `.claude/rules/project/observability-architecture.md` ("Schema
// self-heal at boot"). Future column additions ride
// `ALTER TABLE ADD COLUMN IF NOT EXISTS` so older deployments
// auto-migrate.
// ---------------------------------------------------------------------------

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Create all 21 plain candle tables + the per-seal audit table if they
/// do not already exist, with DEDUP UPSERT enabled on each.
///
/// Idempotent: safe to call on every boot. Failures are logged at
/// `error!` level (Telegram-routable per `error_level_meta_guard.rs`
/// Rule 5) but do NOT block boot — the writer falls back to ring/spill
/// on subsequent ILP errors.
///
/// Requires a running QuestDB; covered by boot integration in CI and
/// by manual `make doctor` post-boot.
// TEST-EXEMPT: requires a running QuestDB; tested via boot integration in CI and by `make doctor`. WIRING-EXEMPT: boot wiring lives in crates/app/src/main.rs alongside ensure_instrument_tables.
pub async fn ensure_shadow_candle_tables(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    for table in candle_table_names() {
        let create_ddl = format!(
            "CREATE TABLE IF NOT EXISTS {table} (\
                segment                     SYMBOL, \
                security_id                 INT, \
                ts                          TIMESTAMP, \
                open                        DOUBLE, \
                high                        DOUBLE, \
                low                         DOUBLE, \
                close                       DOUBLE, \
                volume                      LONG, \
                oi                          LONG, \
                tick_count                  INT\
            ) timestamp(ts) PARTITION BY DAY \
            DEDUP UPSERT KEYS({DEDUP_KEY_CANDLES});"
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

/// Issue one DDL statement to QuestDB's `/exec` endpoint.
///
/// Finding S5: the DDL is sent via HTTP **POST** (was GET). QuestDB's
/// `/exec` endpoint accepts POST — using POST instead of GET keeps the
/// (potentially long) DDL statement out of the request line that
/// intermediary proxies / access-logs record verbatim. The `query`
/// parameter is form-urlencoded into the POST request body via an
/// explicit `Content-Type: application/x-www-form-urlencoded` header
/// (the `reqwest` `form` feature is not enabled in this workspace).
async fn run_ddl(client: &Client, base_url: &str, table: &str, ddl: &str) {
    // Manual form-urlencoding of the single `query=<ddl>` field.
    let body = format!("query={}", urlencode(ddl));
    let request = client
        .post(base_url)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(body);
    match request.send().await {
        Ok(resp) if resp.status().is_success() => {
            info!(table, "candle table ready");
        }
        Ok(resp) => {
            warn!(table, status = %resp.status(), "DDL non-2xx");
        }
        Err(err) => {
            error!(table, ?err, "DDL request failed");
        }
    }
}

/// Minimal `application/x-www-form-urlencoded` value encoder.
///
/// QuestDB DDL only ever contains ASCII identifiers, parentheses,
/// commas, semicolons and spaces — this percent-encodes every byte
/// that is not in the unreserved set so the POST body parses
/// correctly regardless of the statement content.
fn urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 3);
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push(hex_nibble(byte >> 4));
                out.push(hex_nibble(byte & 0x0F));
            }
        }
    }
    out
}

/// Maps a 0-15 nibble to its uppercase hex ASCII character.
const fn hex_nibble(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'A' + (nibble - 10)) as char,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_candle_table_names_has_twenty_one_entries() {
        assert_eq!(candle_table_names().len(), TF_COUNT);
        assert_eq!(TF_COUNT, 21);
    }

    #[test]
    fn test_all_candle_table_names_distinct() {
        let names = candle_table_names();
        let mut seen = std::collections::HashSet::new();
        for name in names {
            assert!(seen.insert(name), "duplicate table name: {name}");
        }
    }

    #[test]
    fn test_candle_table_names_are_plain_and_canonical() {
        // Plain first-class tables — no `_shadow` suffix anywhere.
        let names = candle_table_names();
        for name in names {
            assert!(
                name.starts_with("candles_"),
                "table name {name:?} prefix wrong"
            );
            assert!(
                !name.contains("shadow"),
                "table name {name:?} must be a plain table (no _shadow)"
            );
        }
    }

    #[test]
    fn test_candle_table_names_match_tf_index_table_name() {
        // The DDL loop derives names from `TfIndex::table_name()`; this
        // pins that alignment so the writer's `[Sender; 21]` indexing
        // stays consistent with the DDL it created.
        let names = candle_table_names();
        for (idx, tf) in TfIndex::ALL.iter().enumerate() {
            assert_eq!(
                names[idx],
                tf.table_name(),
                "candle_table_names()[{idx}] diverges from TfIndex::table_name()"
            );
        }
    }

    #[test]
    fn test_dedup_key_candles_includes_ts_security_id_segment() {
        // QuestDB rejects a DEDUP key omitting the designated timestamp;
        // I-P1-11 requires `segment` alongside `security_id`.
        assert!(DEDUP_KEY_CANDLES.contains("ts"));
        assert!(DEDUP_KEY_CANDLES.contains("security_id"));
        assert!(DEDUP_KEY_CANDLES.contains("segment"));
    }

    #[test]
    fn test_dedup_key_candles_matches_tf_index_dedup_key() {
        // The shared DEDUP key constant must equal `TfIndex::dedup_key()`
        // so the DDL and the writer agree.
        for tf in TfIndex::ALL {
            assert_eq!(DEDUP_KEY_CANDLES, tf.dedup_key());
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
    fn test_candle_table_names_canonical_ordering_1m_to_1d() {
        let names = candle_table_names();
        let expected = [
            "candles_1m",
            "candles_2m",
            "candles_3m",
            "candles_4m",
            "candles_5m",
            "candles_6m",
            "candles_7m",
            "candles_8m",
            "candles_9m",
            "candles_10m",
            "candles_11m",
            "candles_12m",
            "candles_13m",
            "candles_14m",
            "candles_15m",
            "candles_30m",
            "candles_1h",
            "candles_2h",
            "candles_3h",
            "candles_4h",
            "candles_1d",
        ];
        assert_eq!(names, expected);
    }
}

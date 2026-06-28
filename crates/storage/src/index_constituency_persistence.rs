//! `index_constituency` persistence — the §31-item-2 full index→constituents
//! mapping, queryable from QuestDB.
//!
//! NTM Sub-PR "Map-A" (operator 2026-06-06 — "full per-index mapping … I need
//! guarantee and assurance"). Stores one row per `(trading_date, index_name,
//! security_id, exchange_segment)` so BOTH directions are answerable in SQL:
//!
//! ```sql
//! -- which stocks are in NIFTY BANK today?
//! SELECT symbol_name FROM index_constituency
//!   WHERE ts = today() AND index_name = 'Nifty Bank';
//! -- which indices is RELIANCE (sid 2885) in today?
//! SELECT index_name FROM index_constituency
//!   WHERE ts = today() AND security_id = 2885;
//! ```
//!
//! This is the MAP-ONLY surface (§31 item 2). It does NOT change the live
//! WebSocket subscription, which stays the NTM-union-only set (the 2-WS lock +
//! `MAX_DAILY_UNIVERSE_SIZE`). Boot wiring (fetch all ~46 lists → resolve
//! per-index → write here) lands in the companion "Map-B" PR; this PR ships the
//! self-contained, fully-tested persistence layer first.
//!
//! ## Schema invariants (mirrors the canonical `instrument_lifecycle` template)
//! * **Composite DEDUP** `(ts, index_name, security_id, exchange_segment)` —
//!   I-P1-11 (Dhan reuses `security_id` across segments, so segment is
//!   mandatory) + `index_name` (a stock is in many indices) + `ts` (the IST
//!   trading-date midnight, which is the QuestDB designated timestamp, so DEDUP
//!   keeps one row per (index, stock) per day with daily rebalance history).
//! * Idempotent `CREATE TABLE IF NOT EXISTS` + `ALTER ADD COLUMN IF NOT EXISTS`
//!   (schema self-heal — `observability-architecture.md`).
//! * ILP bulk ingest (port 9009) — the per-index mapping is large (~46 indices ×
//!   constituents, heavily overlapping → thousands of rows), so the `/exec` URL
//!   door would overflow; ILP is the real pipe.
//!
//! **Feature-gated** under `daily_universe_fetcher` per rule §21.

#![cfg(feature = "daily_universe_fetcher")]

use std::time::Duration;

use anyhow::Context;
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use reqwest::Client;
use tracing::{info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::sanitize_ilp_symbol;

/// `/exec` HTTP timeout for DDL. Matches the other `*_persistence` modules.
const QUESTDB_EXEC_TIMEOUT_SECS: u64 = 10;

/// Broker-source label stamped on every constituency row today (operator
/// override 2026-06-28 — `feed` is in the DEDUP key on every persisted table).
/// The index→constituents map is built ONLY from Dhan-resolved SIDs today.
/// Replay-stable `&'static str` from the canonical `Feed` enum.
pub const INDEX_CONSTITUENCY_FEED_DHAN: &str = tickvault_common::feed::Feed::Dhan.as_str();

/// Wire-format table name. Stable across releases — operator SQL + any future
/// S3 archive job depend on the exact string.
pub const QUESTDB_TABLE_INDEX_CONSTITUENCY: &str = "index_constituency";

/// Composite DEDUP key — one row per (index, stock) per trading day.
///
/// `ts` (the IST trading-date midnight) is the designated timestamp, satisfying
/// QuestDB's designated-ts-in-DEDUP requirement AND giving daily rebalance
/// history. `index_name` distinguishes the same stock across its many indices.
/// `security_id` + `exchange_segment` is the I-P1-11 composite identity.
pub const DEDUP_KEY_INDEX_CONSTITUENCY: &str =
    "ts, index_name, security_id, exchange_segment, feed";

/// Column list shared by the DDL — one source so the writer + table cannot
/// drift. (The ILP writer names each column explicitly; this constant pins the
/// DDL ordering + the `test_ddl_contains_expected_columns` ratchet.)
///
/// Test-only: referenced exclusively by the DDL-column ratchet tests, so it is
/// `#[cfg(test)]`-gated to avoid a dead-code lint on the production lib build
/// (`cargo clippy --all-targets -- -D warnings`).
#[cfg(test)]
const INDEX_CONSTITUENCY_COLUMNS: &[&str] = &[
    "ts",
    "index_name",
    "security_id",
    "exchange_segment",
    "symbol_name",
    "isin",
    "via_isin",
    "source",
    "dry_run",
    "feed",
];

/// One `index_constituency` row — borrows so the bulk writer allocates nothing
/// per row beyond the ILP buffer.
#[derive(Debug, Clone, Copy)]
pub struct IndexConstituencyRow<'a> {
    /// IST trading-date midnight, nanoseconds (the designated `ts`).
    pub trading_date_ist_nanos: i64,
    /// Index display name (e.g. `"Nifty Bank"`).
    pub index_name: &'a str,
    /// Resolved Dhan `security_id` of the constituent.
    pub security_id: i64,
    /// Exchange segment (always `"NSE_EQ"` for cash constituents).
    pub exchange_segment: &'a str,
    /// NSE ticker (e.g. `"RELIANCE"`).
    pub symbol_name: &'a str,
    /// ISIN used for the match (empty if symbol-fallback).
    pub isin: &'a str,
    /// `true` if matched via the ISIN primary key, `false` via symbol fallback.
    pub via_isin: bool,
    /// Provenance — the constituency source (e.g. `"niftyindices"`).
    pub source: &'a str,
    /// `true` for a `--dry-run-universe` boot (isolation per §27).
    pub dry_run: bool,
    /// Broker-source label (`dhan`/`groww`). Part of the DEDUP key (operator
    /// override 2026-06-28). Always non-empty (stamped
    /// [`INDEX_CONSTITUENCY_FEED_DHAN`] today).
    pub feed: &'a str,
}

/// Idempotent `CREATE TABLE IF NOT EXISTS` for `index_constituency`.
///
/// Cold path — called once at boot before the bulk write. Logs (does not
/// return `Err`) so a transient DDL hiccup mirrors the other lifecycle DDLs;
/// the bulk write that follows surfaces a hard failure.
// TEST-EXEMPT: network DDL (live QuestDB) — mirrors the canonical ensure_*_table template; boot integration exercises it.
pub async fn ensure_index_constituency_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = match Client::builder()
        .timeout(Duration::from_secs(QUESTDB_EXEC_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            warn!(?err, "index_constituency DDL: HTTP client build failed");
            return;
        }
    };
    let create_ddl = format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_INDEX_CONSTITUENCY} (\
            ts TIMESTAMP, \
            index_name SYMBOL, \
            security_id LONG, \
            exchange_segment SYMBOL, \
            symbol_name SYMBOL, \
            isin SYMBOL, \
            via_isin BOOLEAN, \
            source SYMBOL, \
            dry_run BOOLEAN, \
            feed SYMBOL\
        ) timestamp(ts) PARTITION BY DAY WAL \
        DEDUP UPSERT KEYS({DEDUP_KEY_INDEX_CONSTITUENCY});"
    );
    match client
        .get(&base_url)
        .query(&[("query", create_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(
                table = QUESTDB_TABLE_INDEX_CONSTITUENCY,
                "index_constituency table ready"
            );
        }
        Ok(resp) => {
            warn!(
                table = QUESTDB_TABLE_INDEX_CONSTITUENCY,
                status = %resp.status(),
                "index_constituency DDL non-2xx"
            );
        }
        Err(err) => {
            warn!(?err, "index_constituency DDL request failed");
        }
    }

    // Feed-in-key self-heal (operator override 2026-06-28 — supersedes the
    // 2026-06-19 NON-key label). Additive + idempotent, IN THIS ORDER:
    // ADD COLUMN → backfill NULL→'dhan' → re-enable DEDUP with `feed` in the
    // key. The CREATE above already ships the new column+key on greenfield;
    // these run for tables created before this change. The backfill MUST precede
    // the DEDUP-ENABLE so a new `feed='dhan'` row upserts over a legacy NULL-feed
    // row instead of duplicating it. Never drops the table (SEBI retention).
    let self_heal = [
        format!(
            "ALTER TABLE {QUESTDB_TABLE_INDEX_CONSTITUENCY} ADD COLUMN IF NOT EXISTS feed SYMBOL;"
        ),
        format!(
            "UPDATE {QUESTDB_TABLE_INDEX_CONSTITUENCY} \
             SET feed = '{INDEX_CONSTITUENCY_FEED_DHAN}' WHERE feed IS NULL;"
        ),
        format!(
            "ALTER TABLE {QUESTDB_TABLE_INDEX_CONSTITUENCY} \
             DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_INDEX_CONSTITUENCY});"
        ),
    ];
    for ddl in &self_heal {
        match client
            .get(&base_url)
            .query(&[("query", ddl.as_str())])
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => {
                warn!(
                    table = QUESTDB_TABLE_INDEX_CONSTITUENCY,
                    status = %resp.status(),
                    ddl = ddl.as_str(),
                    "index_constituency feed self-heal non-2xx"
                );
            }
            Err(err) => {
                warn!(
                    ?err,
                    ddl = ddl.as_str(),
                    "index_constituency feed self-heal request failed"
                );
            }
        }
    }
}

/// Write one `index_constituency` row into an ILP [`Buffer`]. PURE builder
/// (no network) — deterministically unit-tested via `buffer.as_bytes()`.
///
/// ILP rules: all SYMBOLs before any field; empty optional SYMBOLs (`isin`)
/// are SKIPPED (→ NULL, not the empty symbol `''`). The designated `ts` is the
/// IST trading-date midnight so DEDUP fires on the (date, index, stock) key.
fn build_index_constituency_ilp_row(
    buffer: &mut Buffer,
    row: &IndexConstituencyRow<'_>,
) -> anyhow::Result<()> {
    buffer.table(QUESTDB_TABLE_INDEX_CONSTITUENCY)?;
    // ── SYMBOLs first (ILP requires all tags before any field). ──
    buffer.symbol("index_name", sanitize_ilp_symbol(row.index_name).as_ref())?;
    buffer.symbol(
        "exchange_segment",
        sanitize_ilp_symbol(row.exchange_segment).as_ref(),
    )?;
    buffer.symbol("symbol_name", sanitize_ilp_symbol(row.symbol_name).as_ref())?;
    buffer.symbol("source", sanitize_ilp_symbol(row.source).as_ref())?;
    // `feed` is a DEDUP-key SYMBOL — ALWAYS written (operator override 2026-06-28).
    buffer.symbol("feed", sanitize_ilp_symbol(row.feed).as_ref())?;
    // Optional SYMBOL — skip when empty (→ NULL, not empty ILP symbol).
    if !row.isin.is_empty() {
        buffer.symbol("isin", sanitize_ilp_symbol(row.isin).as_ref())?;
    }
    // ── Fields. ──
    buffer.column_i64("security_id", row.security_id)?;
    buffer.column_bool("via_isin", row.via_isin)?;
    buffer.column_bool("dry_run", row.dry_run)?;
    buffer.at(TimestampNanos::new(row.trading_date_ist_nanos))?;
    Ok(())
}

/// Bulk-ingest many `index_constituency` rows via QuestDB **ILP** (port 9009).
///
/// The whole [`Buffer`] is built on the async task (CPU-only, borrows `rows`),
/// then the blocking ILP connect + flush runs on `spawn_blocking`. Idempotent
/// via the table's DEDUP UPSERT KEYS — a re-run after a degraded boot is safe.
///
/// # Errors
/// Returns `Err` if the ILP `Sender` cannot connect or the flush fails; the
/// caller (Map-B boot wiring) logs + the next idempotent boot re-runs.
// TEST-EXEMPT: network I/O — pure builder `build_index_constituency_ilp_row` is unit-tested + boot integration exercises the flush.
pub async fn append_index_constituency_rows(
    questdb_config: &QuestDbConfig,
    rows: &[IndexConstituencyRow<'_>],
) -> anyhow::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let conf = questdb_config.build_ilp_conf_string();
    let mut buffer = Buffer::new(ProtocolVersion::V1);
    for row in rows {
        build_index_constituency_ilp_row(&mut buffer, row)
            .context("building index_constituency ILP row")?;
    }
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let mut sender =
            Sender::from_conf(&conf).context("connect QuestDB ILP (index_constituency)")?;
        sender
            .flush(&mut buffer)
            .context("flush index_constituency rows via ILP")?;
        Ok(())
    })
    .await
    .context("index_constituency ILP flush task join")??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_table_name_constant_is_stable() {
        assert_eq!(QUESTDB_TABLE_INDEX_CONSTITUENCY, "index_constituency");
    }

    #[test]
    fn test_dedup_key_includes_segment_and_index_and_designated_ts() {
        // I-P1-11: segment mandatory. + index_name (stock in many indices) + ts.
        assert!(DEDUP_KEY_INDEX_CONSTITUENCY.contains("exchange_segment"));
        assert!(DEDUP_KEY_INDEX_CONSTITUENCY.contains("index_name"));
        assert!(DEDUP_KEY_INDEX_CONSTITUENCY.contains("security_id"));
        assert!(
            DEDUP_KEY_INDEX_CONSTITUENCY.starts_with("ts"),
            "designated ts must be in the DEDUP key (QuestDB requirement)"
        );
        assert!(
            DEDUP_KEY_INDEX_CONSTITUENCY
                .split([',', ' '])
                .map(str::trim)
                .any(|t| t == "feed"),
            "operator override 2026-06-28: feed must be in the DEDUP key"
        );
        assert_eq!(
            DEDUP_KEY_INDEX_CONSTITUENCY.matches(',').count() + 1,
            5,
            "DEDUP key has exactly 5 columns (ts, index_name, security_id, exchange_segment, feed)"
        );
    }

    #[test]
    fn test_ddl_columns_constant_matches_schema() {
        // The 10 columns the DDL writes, pinned so the writer can't drift.
        assert_eq!(INDEX_CONSTITUENCY_COLUMNS.len(), 10);
        for col in [
            "ts",
            "index_name",
            "security_id",
            "exchange_segment",
            "symbol_name",
            "isin",
            "via_isin",
            "source",
            "dry_run",
            "feed",
        ] {
            assert!(
                INDEX_CONSTITUENCY_COLUMNS.contains(&col),
                "missing column {col}"
            );
        }
    }

    fn sample_row<'a>() -> IndexConstituencyRow<'a> {
        IndexConstituencyRow {
            trading_date_ist_nanos: 1_780_000_000_000_000_000,
            index_name: "Nifty Bank",
            security_id: 2885,
            exchange_segment: "NSE_EQ",
            symbol_name: "RELIANCE",
            isin: "INE002A01018",
            via_isin: true,
            source: "niftyindices",
            dry_run: false,
            feed: INDEX_CONSTITUENCY_FEED_DHAN,
        }
    }

    #[test]
    fn test_build_ilp_row_writes_feed_symbol() {
        // operator override 2026-06-28: every row carries feed in-key.
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        build_index_constituency_ilp_row(&mut buffer, &sample_row()).expect("build");
        let line = String::from_utf8(buffer.as_bytes().to_vec()).expect("utf8");
        assert!(
            line.contains(",feed=dhan"),
            "ILP must write feed tag: {line}"
        );
    }

    #[test]
    fn test_build_ilp_row_symbols_before_fields_and_contains_values() {
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        build_index_constituency_ilp_row(&mut buffer, &sample_row()).expect("build");
        let line = String::from_utf8(buffer.as_bytes().to_vec()).expect("utf8");
        // table name first
        assert!(line.starts_with("index_constituency"), "got: {line}");
        // Values present (ILP escapes the space in "Nifty Bank" as "Nifty\ Bank").
        assert!(line.contains("index_name=Nifty\\ Bank"), "got: {line}");
        assert!(line.contains("exchange_segment=NSE_EQ"), "got: {line}");
        assert!(line.contains(",isin=INE002A01018"), "got: {line}");
        assert!(line.contains("security_id=2885i"), "got: {line}");
        assert!(line.contains("via_isin=t"), "got: {line}");
        // Symbols (tags) precede fields: the first field key appears AFTER the
        // last tag key. (Don't split on ' ' — ILP escapes spaces inside tags.)
        let tag_pos = line.find("index_name=").expect("tag present");
        let field_pos = line.find("security_id=").expect("field present");
        assert!(tag_pos < field_pos, "symbols must precede fields: {line}");
    }

    #[test]
    fn test_build_ilp_row_skips_empty_isin() {
        let mut row = sample_row();
        row.isin = ""; // symbol-fallback resolved → NULL, not empty symbol
        let mut buffer = Buffer::new(ProtocolVersion::V1);
        build_index_constituency_ilp_row(&mut buffer, &row).expect("build");
        let line = String::from_utf8(buffer.as_bytes().to_vec()).expect("utf8");
        // The isin TAG appears as ",isin=" — must be absent when empty. (Note
        // the `via_isin=` FIELD contains the substring "isin=", so assert on the
        // comma-prefixed tag form, not bare "isin=".)
        assert!(
            !line.contains(",isin="),
            "empty isin must be skipped (NULL), got: {line}"
        );
        // The via_isin field still carries the resolution flag.
        assert!(line.contains("via_isin=t"), "got: {line}");
    }
}

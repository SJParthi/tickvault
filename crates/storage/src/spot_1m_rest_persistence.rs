//! `spot_1m_rest` table — per-minute spot 1m REST pipeline (operator grant
//! 2026-07-12, PR-2 the SPOT half; runbook
//! `.claude/rules/project/rest-1m-pipeline-error-codes.md`).
//!
//! ONE row per `(minute, index)` fetched by the per-minute Dhan
//! `POST /v2/charts/intraday` fetcher (`crates/app/src/spot_1m_rest_boot.rs`)
//! for the 3 IDX_I spot indices (NIFTY 13 / BANKNIFTY 25 / SENSEX 51). The
//! designated `ts` is the candle's MINUTE-OPEN IST stamp (REST UTC epoch
//! +19800 per `.claude/rules/dhan/historical-data.md` rule 8 — NEVER the
//! WebSocket no-offset convention), so a re-fetch / manual backfill of the
//! same minute UPSERTs in place (DEDUP idempotency by construction).
//!
//! ## Honesty columns
//! - `close_to_data_ms` — wall-clock ms from the minute CLOSE to the
//!   successful retrieval: the live probe of Dhan's (undocumented)
//!   just-closed-minute availability latency.
//! - `fetched_at` — the retrieval wall-clock instant (IST), forensic.
//! - `source` — `'rest_intraday'` label (NOT in the DEDUP key) so a future
//!   second writer to this table could be told apart.
//!
//! ## Schema
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS spot_1m_rest (
//!     ts TIMESTAMP, trading_date_ist TIMESTAMP, security_id LONG,
//!     exchange_segment SYMBOL, feed SYMBOL, source SYMBOL, symbol SYMBOL,
//!     open DOUBLE, high DOUBLE, low DOUBLE, close DOUBLE, volume LONG,
//!     close_to_data_ms LONG, fetched_at TIMESTAMP
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, security_id, exchange_segment, feed);
//! ```

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::config::QuestDbConfig;

/// QuestDB table name — one row per fetched `(minute, index)`.
pub const SPOT_1M_REST_TABLE: &str = "spot_1m_rest";

/// DEDUP key. Designated `ts` FIRST (2026-04-28 regression rule);
/// `exchange_segment` alongside `security_id` (I-P1-11); `feed` in-key
/// (operator override 2026-06-28 — feed-in-key EVERYWHERE). A re-fetch of
/// the same minute UPSERTs in place, never duplicates.
pub const DEDUP_KEY_SPOT_1M_REST: &str = "ts, security_id, exchange_segment, feed";

/// `feed` SYMBOL value — the source broker is Dhan (REST leg).
pub const SPOT_1M_REST_FEED_DHAN: &str = "dhan";

/// `source` SYMBOL label — provenance beyond `feed` (label, NOT in-key).
pub const SPOT_1M_REST_SOURCE: &str = "rest_intraday";

/// `exchange_segment` SYMBOL value — the 3 spot indices are all IDX_I.
pub const SPOT_1M_REST_SEGMENT_IDX_I: &str = "IDX_I";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// One fetched spot 1m candle row, ready for ILP write.
#[derive(Clone, Debug, PartialEq)]
pub struct Spot1mRestRow {
    /// Designated timestamp — the candle's MINUTE-OPEN, IST nanoseconds
    /// (REST UTC epoch secs +19800, floored to the minute, ×1e9).
    pub ts_ist_nanos: i64,
    /// The trading day — IST midnight nanoseconds.
    pub trading_date_ist_nanos: i64,
    /// Dhan IDX_I SecurityId (13 / 25 / 51).
    pub security_id: i64,
    /// Human symbol (`NIFTY` / `BANKNIFTY` / `SENSEX`).
    pub symbol: &'static str,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    /// Dhan-reported volume (indices legitimately read 0 — stored verbatim).
    pub volume: i64,
    /// Wall-clock ms from the minute CLOSE to the successful retrieval —
    /// the honest live probe of the just-closed-minute availability latency.
    pub close_to_data_ms: i64,
    /// Retrieval wall-clock instant, IST nanoseconds (`Utc::now()` source ⇒
    /// +IST offset per `data-integrity.md`).
    pub fetched_at_ist_nanos: i64,
}

/// The idempotent `CREATE TABLE` DDL for `spot_1m_rest`. Pure.
#[must_use]
pub fn spot_1m_rest_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {SPOT_1M_REST_TABLE} (\
            ts               TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            security_id      LONG, \
            exchange_segment SYMBOL, \
            feed             SYMBOL, \
            source           SYMBOL, \
            symbol           SYMBOL, \
            open             DOUBLE, \
            high             DOUBLE, \
            low              DOUBLE, \
            close            DOUBLE, \
            volume           LONG, \
            close_to_data_ms LONG, \
            fetched_at       TIMESTAMP\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_SPOT_1M_REST});"
    )
}

/// Create the `spot_1m_rest` table if absent (idempotent schema-self-heal
/// order: CREATE → per-column `ALTER ADD COLUMN IF NOT EXISTS` → DEDUP
/// ENABLE, so a table created by an earlier build auto-migrates; never a
/// table drop — SEBI retention). Greenfield table, writer always stamps
/// `feed` ⇒ no NULL-feed backfill UPDATE (the scoreboard precedent).
///
/// Failures log at `error!` (code SPOT1M-02) but never block — NOTE the
/// HTTP-CLIENT-01-class consequence: a failed ensure leaves the table to be
/// auto-created by the first ILP write WITHOUT DEDUP UPSERT KEYS — a
/// duplicate-row window until a later ensure succeeds.
// TEST-EXEMPT: live-QuestDB DDL runner (DDL strings unit-tested via the create_ddl tests; the 200/500/unreachable arms exercised via the mock-HTTP tokio tests below)
pub async fn ensure_spot_1m_rest_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            metrics::counter!("tv_spot1m_persist_errors_total", "stage" => "ensure_client_build")
                .increment(1);
            error!(
                code = "SPOT1M-02",
                stage = "ensure_client_build",
                ?err,
                "SPOT1M-02: HTTP client build failed — spot_1m_rest table not \
                 ensured (first ILP write may auto-create it WITHOUT dedup — \
                 duplicate-row window until the next successful boot)"
            );
            return;
        }
    };
    let mut statements = vec![spot_1m_rest_create_ddl()];
    // Per-column self-heal for tables created by earlier builds
    // (observability-architecture.md schema-self-heal pattern). QuestDB
    // ignores ADDs that already exist, so running every boot is free.
    for (col, ty) in [
        ("trading_date_ist", "TIMESTAMP"),
        ("security_id", "LONG"),
        ("exchange_segment", "SYMBOL"),
        ("feed", "SYMBOL"),
        ("source", "SYMBOL"),
        ("symbol", "SYMBOL"),
        ("open", "DOUBLE"),
        ("high", "DOUBLE"),
        ("low", "DOUBLE"),
        ("close", "DOUBLE"),
        ("volume", "LONG"),
        ("close_to_data_ms", "LONG"),
        ("fetched_at", "TIMESTAMP"),
    ] {
        statements.push(format!(
            "ALTER TABLE {SPOT_1M_REST_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty};"
        ));
    }
    statements.push(format!(
        "ALTER TABLE {SPOT_1M_REST_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_SPOT_1M_REST});"
    ));
    for ddl in &statements {
        match client
            .get(&base_url)
            .query(&[("query", ddl.as_str())])
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                metrics::counter!("tv_spot1m_persist_errors_total", "stage" => "ensure_ddl")
                    .increment(1);
                error!(code = "SPOT1M-02", stage = "ensure_ddl",
                    %status, ddl = ddl.as_str(),
                    body = %body.chars().take(200).collect::<String>(),
                    "SPOT1M-02: spot_1m_rest DDL returned non-2xx (dedup may be \
                     missing — duplicate-row window until a later ensure succeeds)");
            }
            Err(err) => {
                metrics::counter!("tv_spot1m_persist_errors_total", "stage" => "ensure_ddl")
                    .increment(1);
                error!(
                    code = "SPOT1M-02",
                    stage = "ensure_ddl",
                    ?err,
                    ddl = ddl.as_str(),
                    "SPOT1M-02: spot_1m_rest DDL request failed"
                );
            }
        }
    }
}

/// ILP-over-HTTP conf for the per-minute writer — per-flush server ACK (the
/// 2026-07-05 fire-and-forget lesson) with the shadow-candle-writer knobs:
/// `retry_timeout=0` (the questdb-rs internal 10s sleep-and-resend loop is
/// disabled — the fetch loop owns retry cadence) + `request_timeout=5000` ms
/// (bounds a hung flush).
fn spot_1m_rest_ilp_http_conf(config: &QuestDbConfig) -> String {
    format!(
        "http::addr={}:{};protocol_version=1;retry_timeout=0;request_timeout=5000;",
        config.host, config.http_port
    )
}

/// Lazy ILP-over-HTTP writer for `spot_1m_rest`. Mirrors
/// `FeedScoreboardWriter`: unreachable QuestDB at construction still builds
/// (rows buffer locally); `flush` returns `Err` — incl. server-side rejects
/// via the HTTP ACK — and rows stay buffered, never silently lost.
pub struct Spot1mRestWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl Spot1mRestWriter {
    /// Production constructor — ILP-over-HTTP sender, lazy on failure.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (lazy-build contract exercised via test_spot1m_writer_new_is_lazy...); append/flush paths covered via for_test()
    pub fn new(config: &QuestDbConfig) -> Self {
        let conf = spot_1m_rest_ilp_http_conf(config);
        match Sender::from_conf(&conf) {
            Ok(s) => {
                let b = s.new_buffer();
                Self {
                    sender: Some(s),
                    buffer: b,
                    pending: 0,
                }
            }
            Err(err) => {
                warn!(
                    ?err,
                    "spot_1m_rest writer: QuestDB unreachable — buffering locally"
                );
                Self {
                    sender: None,
                    buffer: Buffer::new(ProtocolVersion::V1),
                    pending: 0,
                }
            }
        }
    }

    /// Test constructor — disconnected writer, empty buffer.
    #[must_use]
    // TEST-EXEMPT: test-only helper used by the append/flush unit tests below.
    pub fn for_test() -> Self {
        Self {
            sender: None,
            buffer: Buffer::new(ProtocolVersion::V1),
            pending: 0,
        }
    }

    /// Rows appended but not yet flushed.
    #[must_use]
    // TEST-EXEMPT: observability accessor, exercised by the append tests below.
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// Test-only view of the ILP buffer bytes (shape assertions).
    #[cfg(test)]
    fn buffer_utf8(&self) -> String {
        String::from_utf8(self.buffer.as_bytes().to_vec()).unwrap_or_default()
    }

    /// Appends one spot 1m candle row (cold path, ≤3 rows/minute).
    ///
    /// # Errors
    /// Propagates ILP buffer errors (table/column append failure).
    pub fn append_row(&mut self, r: &Spot1mRestRow) -> Result<()> {
        self.buffer
            .table(SPOT_1M_REST_TABLE)
            .context("table")?
            // Symbols BEFORE columns (ILP tags-before-fields rule).
            .symbol("exchange_segment", SPOT_1M_REST_SEGMENT_IDX_I)
            .context("exchange_segment")?
            .symbol("feed", SPOT_1M_REST_FEED_DHAN)
            .context("feed")?
            .symbol("source", SPOT_1M_REST_SOURCE)
            .context("source")?
            .symbol("symbol", r.symbol)
            .context("symbol")?
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(r.trading_date_ist_nanos),
            )
            .context("trading_date_ist")?
            .column_i64("security_id", r.security_id)
            .context("security_id")?
            .column_f64("open", r.open)
            .context("open")?
            .column_f64("high", r.high)
            .context("high")?
            .column_f64("low", r.low)
            .context("low")?
            .column_f64("close", r.close)
            .context("close")?
            .column_i64("volume", r.volume)
            .context("volume")?
            .column_i64("close_to_data_ms", r.close_to_data_ms)
            .context("close_to_data_ms")?
            .column_ts("fetched_at", TimestampNanos::new(r.fetched_at_ist_nanos))
            .context("fetched_at")?
            .at(TimestampNanos::new(r.ts_ist_nanos))
            .context("designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Flushes buffered rows over ILP-HTTP (per-flush server ACK).
    ///
    /// On ANY failed flush the pending buffer is DISCARDED (the 2026-07-06
    /// shadow-writer `discard_pending` precedent — hostile-review M2): a
    /// server-REJECTED row retained across flushes would be re-sent every
    /// minute forever and block ALL later rows for the session. The rows
    /// are re-fetchable and DEDUP-idempotent, so the durable floor for a
    /// discarded minute is a re-fetch/backfill, and the miss is LOUD
    /// (SPOT1M-02 error + `tv_spot1m_rows_discarded_total` + the minute
    /// feeds the SPOT1M-01 failure edge).
    ///
    /// # Errors
    /// `Err` when disconnected or the HTTP flush fails (pending discarded).
    pub fn flush(&mut self) -> Result<()> {
        if self.pending == 0 {
            return Ok(());
        }
        if self.sender.is_none() {
            let dropped = self.discard_pending();
            anyhow::bail!(
                "spot_1m_rest: no ILP sender (QuestDB unreachable) — \
                 {dropped} pending row(s) discarded (re-fetchable, DEDUP-idempotent)"
            );
        }
        let flushed = self
            .sender
            .as_mut()
            .map(|sender| sender.flush(&mut self.buffer));
        match flushed {
            Some(Ok(())) => {
                self.pending = 0;
                Ok(())
            }
            Some(Err(err)) => {
                let dropped = self.discard_pending();
                Err(anyhow::Error::new(err).context(format!(
                    "spot_1m_rest ILP flush failed — {dropped} pending row(s) \
                     discarded (poisoned-buffer defense; rows are re-fetchable)"
                )))
            }
            // Unreachable (checked above) — treated as the no-sender arm.
            None => {
                let dropped = self.discard_pending();
                anyhow::bail!("spot_1m_rest: ILP sender vanished — {dropped} row(s) discarded");
            }
        }
    }

    /// Drop every buffered-but-unflushed row (poisoned-buffer defense).
    /// Returns the discarded row count; counted by
    /// `tv_spot1m_rows_discarded_total` so a discard is never silent.
    pub fn discard_pending(&mut self) -> usize {
        let dropped = self.pending;
        if dropped > 0 {
            metrics::counter!("tv_spot1m_rows_discarded_total").increment(dropped as u64);
        }
        self.buffer.clear();
        self.pending = 0;
        dropped
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row() -> Spot1mRestRow {
        Spot1mRestRow {
            // 2026-07-10 09:15:00 IST-as-epoch minute-open (illustrative).
            ts_ist_nanos: 1_770_000_900_000_000_000,
            trading_date_ist_nanos: 1_769_990_400_000_000_000,
            security_id: 13,
            symbol: "NIFTY",
            open: 25_461.3,
            high: 25_470.85,
            low: 25_455.0,
            close: 25_468.2,
            volume: 0,
            close_to_data_ms: 1_042,
            fetched_at_ist_nanos: 1_770_000_961_042_000_000,
        }
    }

    #[test]
    fn test_spot_1m_rest_create_ddl_contains_expected_columns() {
        let ddl = spot_1m_rest_create_ddl();
        for col in [
            "ts ",
            "trading_date_ist",
            "security_id",
            "exchange_segment",
            "feed",
            "source",
            "symbol",
            "open",
            "high",
            "low",
            "close",
            "volume",
            "close_to_data_ms",
            "fetched_at",
        ] {
            assert!(ddl.contains(col), "DDL missing column {col:?}: {ddl}");
        }
        assert!(ddl.contains("CREATE TABLE IF NOT EXISTS"));
        assert!(ddl.contains("PARTITION BY DAY"));
        assert!(ddl.contains(&format!("DEDUP UPSERT KEYS({DEDUP_KEY_SPOT_1M_REST})")));
    }

    /// DEDUP-key discipline: designated `ts` FIRST (2026-04-28 regression
    /// rule); `exchange_segment` with `security_id` (I-P1-11); `feed`
    /// in-key (operator override 2026-06-28) — whole-token matches.
    #[test]
    fn test_spot_1m_rest_dedup_key_ts_first_segment_and_feed_in_key() {
        assert!(DEDUP_KEY_SPOT_1M_REST.trim_start().starts_with("ts,"));
        let has_token = |t: &str| {
            DEDUP_KEY_SPOT_1M_REST
                .split([',', ' '])
                .map(str::trim)
                .any(|tok| tok == t)
        };
        assert!(has_token("security_id"));
        assert!(has_token("exchange_segment"));
        assert!(has_token("feed"));
        // Exactly (ts, security_id, exchange_segment, feed).
        assert_eq!(DEDUP_KEY_SPOT_1M_REST.matches(',').count() + 1, 4);
    }

    #[test]
    fn test_spot_1m_rest_symbol_labels_stable() {
        assert_eq!(SPOT_1M_REST_TABLE, "spot_1m_rest");
        assert_eq!(SPOT_1M_REST_FEED_DHAN, "dhan");
        assert_eq!(SPOT_1M_REST_SOURCE, "rest_intraday");
        assert_eq!(SPOT_1M_REST_SEGMENT_IDX_I, "IDX_I");
    }

    #[test]
    fn test_append_row_writes_symbols_and_columns() {
        let mut w = Spot1mRestWriter::for_test();
        w.append_row(&sample_row()).expect("append must succeed");
        assert_eq!(w.pending(), 1);
        let line = w.buffer_utf8();
        assert!(line.starts_with(SPOT_1M_REST_TABLE));
        assert!(
            line.contains(",exchange_segment=IDX_I"),
            "segment tag missing: {line}"
        );
        assert!(line.contains(",feed=dhan"), "feed tag missing: {line}");
        assert!(
            line.contains(",source=rest_intraday"),
            "source tag missing: {line}"
        );
        assert!(line.contains(",symbol=NIFTY"), "symbol tag missing: {line}");
        assert!(line.contains("security_id=13i"), "sid missing: {line}");
        assert!(
            line.contains("close_to_data_ms=1042i"),
            "latency column missing: {line}"
        );
        // Index volume of 0 is stored verbatim (never dropped).
        assert!(line.contains("volume=0i"), "volume missing: {line}");
    }

    #[test]
    fn test_spot1m_flush_when_disconnected_errors_and_discards_pending() {
        // 2026-07-12 M2 (poisoned-buffer defense): a failed flush DISCARDS
        // the pending buffer — one rejected row can never wedge the rest of
        // the session; the rows are re-fetchable + DEDUP-idempotent.
        let mut w = Spot1mRestWriter::for_test();
        w.append_row(&sample_row()).expect("append must succeed");
        let err = w.flush().expect_err("disconnected flush must error");
        assert!(err.to_string().contains("no ILP sender"));
        assert!(err.to_string().contains("discarded"));
        assert_eq!(w.pending(), 0, "failed flush discards pending (M2)");
        assert!(w.buffer_utf8().is_empty(), "ILP buffer cleared on discard");
        // Empty flush is a no-op Ok.
        let mut empty = Spot1mRestWriter::for_test();
        assert!(empty.flush().is_ok());
    }

    /// `discard_pending` clears BOTH the row count and the ILP buffer so
    /// the next fire starts from a clean slate (shadow-writer precedent).
    #[test]
    fn test_spot1m_discard_pending_clears_buffer_and_count() {
        let mut w = Spot1mRestWriter::for_test();
        w.append_row(&sample_row()).expect("append must succeed");
        w.append_row(&sample_row()).expect("append must succeed");
        assert_eq!(w.pending(), 2);
        assert!(!w.buffer_utf8().is_empty());
        assert_eq!(w.discard_pending(), 2, "returns the discarded count");
        assert_eq!(w.pending(), 0);
        assert!(w.buffer_utf8().is_empty());
        // Idempotent: a second discard drops nothing.
        assert_eq!(w.discard_pending(), 0);
    }

    /// Transport ratchet (2026-07-05 fire-and-forget lesson): ILP-over-HTTP
    /// with the shadow-candle-writer knobs — never ILP TCP 9009.
    #[test]
    fn test_spot1m_writer_uses_ilp_http_conf_with_bounded_knobs() {
        let cfg = QuestDbConfig {
            host: "tv-questdb".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let conf = spot_1m_rest_ilp_http_conf(&cfg);
        assert_eq!(
            conf,
            "http::addr=tv-questdb:9000;protocol_version=1;retry_timeout=0;request_timeout=5000;"
        );
        assert!(!conf.contains("9009"), "must not target ILP TCP: {conf}");
    }

    // ========================================================================
    // Persistence-helper tests — mock QuestDB /exec HTTP server + unreachable
    // host (the feed_scoreboard_persistence P2C pattern). These exercise the
    // real ensure/constructor code paths: success (200), non-2xx (500) and
    // transport-error arms.
    // ========================================================================

    const MOCK_HTTP_200: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
    const MOCK_HTTP_500: &str =
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 13\r\n\r\n{\"error\":\"x\"}";

    async fn spawn_mock_http(response: &'static str) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                if let Ok((mut stream, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = [0u8; 8192];
                        let _ = stream.read(&mut buf).await;
                        let _ = stream.write_all(response.as_bytes()).await;
                    });
                }
            }
        });
        port
    }

    fn mock_cfg(http_port: u16) -> QuestDbConfig {
        QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port,
            pg_port: 1,
            ilp_port: 1,
        }
    }

    fn unreachable_cfg() -> QuestDbConfig {
        // Port 1 is reserved and never listening; guarantees a real HTTP
        // transport failure without touching any live service.
        mock_cfg(1)
    }

    #[tokio::test]
    async fn test_ensure_spot_1m_rest_table_mock_200_completes() {
        // Success path: the CREATE + every ADD COLUMN self-heal + the DEDUP
        // ENABLE all take the Ok(2xx) arm.
        let port = spawn_mock_http(MOCK_HTTP_200).await;
        ensure_spot_1m_rest_table(&mock_cfg(port)).await;
    }

    #[tokio::test]
    async fn test_ensure_spot_1m_rest_table_mock_500_degrades_without_panic() {
        // Non-2xx path: every DDL statement takes the log-and-continue arm
        // (best-effort degrade — SPOT1M-02, never a panic, never blocks).
        let port = spawn_mock_http(MOCK_HTTP_500).await;
        ensure_spot_1m_rest_table(&mock_cfg(port)).await;
    }

    #[tokio::test]
    async fn test_ensure_spot_1m_rest_table_unreachable_degrades_without_panic() {
        // Transport-error path: every DDL send Err arm logs and continues.
        ensure_spot_1m_rest_table(&unreachable_cfg()).await;
    }

    #[tokio::test]
    async fn test_spot1m_writer_new_is_lazy_and_buffers_without_network() {
        // `Sender::from_conf` with `http::` does not dial at construction
        // (the ws_event_audit precedent), so new() against an unreachable
        // host still builds a sender-backed writer whose appends land in
        // the local buffer — the lazy-construction contract.
        let mut w = Spot1mRestWriter::new(&unreachable_cfg());
        assert_eq!(w.pending(), 0, "fresh writer has nothing pending");
        w.append_row(&sample_row())
            .expect("append must succeed without network");
        assert_eq!(w.pending(), 1, "row buffered locally");
    }
}

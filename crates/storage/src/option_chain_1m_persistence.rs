//! `option_chain_1m` table — per-minute option-chain REST pipeline
//! (operator grant 2026-07-12, PR-3 the OPTION-CHAIN half; runbook
//! `.claude/rules/project/rest-1m-pipeline-error-codes.md`).
//!
//! ONE row per `(minute, underlying, expiry, strike, leg)` fetched by the
//! per-minute Dhan `POST /v2/optionchain` fetcher
//! (`crates/app/src/option_chain_1m_boot.rs`) for the 3 IDX_I underlyings
//! (NIFTY 13 / BANKNIFTY 25 / SENSEX 51), current expiry only. The
//! designated `ts` is the MINUTE-OPEN IST stamp of the minute the snapshot
//! belongs to (the minute that just closed when the fetch fired — same
//! IST-as-epoch convention as `spot_1m_rest.ts` / `candles_1m.ts`), so a
//! re-fetch of the same minute UPSERTs in place (DEDUP idempotency by
//! construction).
//!
//! ## Honesty columns
//! - `underlying_spot` — the chain response's own `data.last_price`
//!   (Dhan's view of the underlying at snapshot time), forensic.
//! - `fetched_at` — the retrieval wall-clock instant (IST), forensic.
//! - `source` — `'rest_optionchain'` label (NOT in the DEDUP key).
//! - `contract_security_id` — Dhan's per-contract SecurityId from the
//!   response (0 when absent) — usable for future subscriptions without an
//!   instrument-master lookup (option-chain.md rule 8); NOT in the key.
//!
//! ## Disk envelope (flagged operator follow-up — retention)
//! ~150 strikes × 2 legs × 3 underlyings ≈ ~900 rows/minute worst case
//! (typically less — one-sided deep-OTM strikes are skipped), ~2–3K/min at
//! wide chains. At ~200 B/row that is roughly ~70 MB/trading-day and
//! ~6–8 GB per 90-day hot window — an order of magnitude above every other
//! DAY-partitioned table. The table IS registered with the partition
//! manager (DAY partitions age to S3 like the rest); whether the hot
//! window should be SHORTER for this table is a flagged operator
//! follow-up per the plan.
//!
//! ## Schema
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS option_chain_1m (
//!     ts TIMESTAMP, trading_date_ist TIMESTAMP,
//!     underlying_security_id LONG, exchange_segment SYMBOL, feed SYMBOL,
//!     source SYMBOL, underlying_symbol SYMBOL, expiry TIMESTAMP,
//!     strike DOUBLE, leg SYMBOL, contract_security_id LONG,
//!     last_price DOUBLE, iv DOUBLE, delta DOUBLE, theta DOUBLE,
//!     gamma DOUBLE, vega DOUBLE, oi LONG, volume LONG, previous_oi LONG,
//!     underlying_spot DOUBLE, fetched_at TIMESTAMP
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, underlying_security_id, exchange_segment,
//!                     expiry, strike, leg, feed);
//! ```

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::config::QuestDbConfig;

/// QuestDB table name — one row per fetched `(minute, underlying, expiry,
/// strike, leg)`.
pub const OPTION_CHAIN_1M_TABLE: &str = "option_chain_1m";

/// DEDUP key. Designated `ts` FIRST (2026-04-28 regression rule);
/// `exchange_segment` alongside the underlying id (I-P1-11); `feed` in-key
/// (operator override 2026-06-28 — feed-in-key EVERYWHERE); `expiry` +
/// `strike` + `leg` complete the per-contract identity. A re-fetch of the
/// same minute UPSERTs in place, never duplicates. `strike` is a DOUBLE
/// key column — safe here because every strike value comes from the SAME
/// deterministic decimal-string parse (`"25650.000000"` → f64), so a
/// re-fetch produces a bit-identical key.
pub const DEDUP_KEY_OPTION_CHAIN_1M: &str =
    "ts, underlying_security_id, exchange_segment, expiry, strike, leg, feed";

/// `feed` SYMBOL value — the source broker is Dhan (REST leg).
pub const OPTION_CHAIN_1M_FEED_DHAN: &str = "dhan";

/// `source` SYMBOL label — provenance beyond `feed` (label, NOT in-key).
pub const OPTION_CHAIN_1M_SOURCE: &str = "rest_optionchain";

/// `exchange_segment` SYMBOL value — the 3 underlyings are all IDX_I.
pub const OPTION_CHAIN_1M_SEGMENT_IDX_I: &str = "IDX_I";

/// `leg` SYMBOL values — call / put.
pub const OPTION_CHAIN_1M_LEG_CE: &str = "CE";
/// `leg` SYMBOL value for the put side.
pub const OPTION_CHAIN_1M_LEG_PE: &str = "PE";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// One fetched option-chain leg row, ready for ILP write.
#[derive(Clone, Debug, PartialEq)]
pub struct OptionChain1mRow {
    /// Designated timestamp — the snapshot's MINUTE-OPEN, IST nanoseconds
    /// (the minute that closed when the fetch fired).
    pub ts_ist_nanos: i64,
    /// The trading day — IST midnight nanoseconds.
    pub trading_date_ist_nanos: i64,
    /// Dhan IDX_I SecurityId of the UNDERLYING (13 / 25 / 51).
    pub underlying_security_id: i64,
    /// Human underlying symbol (`NIFTY` / `BANKNIFTY` / `SENSEX`).
    pub underlying_symbol: &'static str,
    /// The chain's expiry date — IST midnight nanoseconds of the expiry
    /// day (the house TIMESTAMP convention for expiry columns, per
    /// `instrument_lifecycle.expiry_date`).
    pub expiry_ist_nanos: i64,
    /// Strike price (parsed from Dhan's decimal-string key).
    pub strike: f64,
    /// `"CE"` or `"PE"` (one of the `OPTION_CHAIN_1M_LEG_*` constants).
    pub leg: &'static str,
    /// Dhan SecurityId of THIS option contract (0 when absent).
    pub contract_security_id: i64,
    pub last_price: f64,
    /// Implied volatility of this strike.
    pub iv: f64,
    pub delta: f64,
    pub theta: f64,
    pub gamma: f64,
    pub vega: f64,
    /// Current open interest.
    pub oi: i64,
    /// Today's traded volume.
    pub volume: i64,
    /// Previous day's open interest.
    pub previous_oi: i64,
    /// The response's own `data.last_price` for the underlying.
    pub underlying_spot: f64,
    /// Retrieval wall-clock instant, IST nanoseconds.
    pub fetched_at_ist_nanos: i64,
}

/// The idempotent `CREATE TABLE` DDL for `option_chain_1m`. Pure.
#[must_use]
pub fn option_chain_1m_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {OPTION_CHAIN_1M_TABLE} (\
            ts                     TIMESTAMP, \
            trading_date_ist       TIMESTAMP, \
            underlying_security_id LONG, \
            exchange_segment       SYMBOL, \
            feed                   SYMBOL, \
            source                 SYMBOL, \
            underlying_symbol      SYMBOL, \
            expiry                 TIMESTAMP, \
            strike                 DOUBLE, \
            leg                    SYMBOL, \
            contract_security_id   LONG, \
            last_price             DOUBLE, \
            iv                     DOUBLE, \
            delta                  DOUBLE, \
            theta                  DOUBLE, \
            gamma                  DOUBLE, \
            vega                   DOUBLE, \
            oi                     LONG, \
            volume                 LONG, \
            previous_oi            LONG, \
            underlying_spot        DOUBLE, \
            fetched_at             TIMESTAMP\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_OPTION_CHAIN_1M});"
    )
}

/// Every non-designated column as `(name, type)` — the single source for
/// both the CREATE DDL sanity tests and the per-column `ALTER ADD COLUMN
/// IF NOT EXISTS` self-heal below. Pure.
#[must_use]
pub fn option_chain_1m_columns() -> [(&'static str, &'static str); 21] {
    [
        ("trading_date_ist", "TIMESTAMP"),
        ("underlying_security_id", "LONG"),
        ("exchange_segment", "SYMBOL"),
        ("feed", "SYMBOL"),
        ("source", "SYMBOL"),
        ("underlying_symbol", "SYMBOL"),
        ("expiry", "TIMESTAMP"),
        ("strike", "DOUBLE"),
        ("leg", "SYMBOL"),
        ("contract_security_id", "LONG"),
        ("last_price", "DOUBLE"),
        ("iv", "DOUBLE"),
        ("delta", "DOUBLE"),
        ("theta", "DOUBLE"),
        ("gamma", "DOUBLE"),
        ("vega", "DOUBLE"),
        ("oi", "LONG"),
        ("volume", "LONG"),
        ("previous_oi", "LONG"),
        ("underlying_spot", "DOUBLE"),
        ("fetched_at", "TIMESTAMP"),
    ]
}

/// Create the `option_chain_1m` table if absent (idempotent schema-self-heal
/// order: CREATE → per-column `ALTER ADD COLUMN IF NOT EXISTS` → DEDUP
/// ENABLE, so a table created by an earlier build auto-migrates; never a
/// table drop — SEBI retention). Greenfield table, writer always stamps
/// `feed` ⇒ no NULL-feed backfill UPDATE.
///
/// Failures log at `error!` (code CHAIN-03) but never block — NOTE the
/// HTTP-CLIENT-01-class consequence: a failed ensure leaves the table to be
/// auto-created by the first ILP write WITHOUT DEDUP UPSERT KEYS — a
/// duplicate-row window until a later ensure succeeds.
// TEST-EXEMPT: live-QuestDB DDL runner (DDL strings unit-tested via the create_ddl tests; the 200/500/unreachable arms exercised via the mock-HTTP tokio tests below)
pub async fn ensure_option_chain_1m_table(questdb_config: &QuestDbConfig) {
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
            metrics::counter!("tv_chain1m_persist_errors_total", "stage" => "ensure_client_build")
                .increment(1);
            error!(
                code = "CHAIN-03",
                stage = "ensure_client_build",
                ?err,
                "CHAIN-03: HTTP client build failed — option_chain_1m table \
                 not ensured (first ILP write may auto-create it WITHOUT \
                 dedup — duplicate-row window until the next successful boot)"
            );
            return;
        }
    };
    let mut statements = vec![option_chain_1m_create_ddl()];
    // Per-column self-heal for tables created by earlier builds
    // (observability-architecture.md schema-self-heal pattern). QuestDB
    // ignores ADDs that already exist, so running every boot is free.
    for (col, ty) in option_chain_1m_columns() {
        statements.push(format!(
            "ALTER TABLE {OPTION_CHAIN_1M_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty};"
        ));
    }
    statements.push(format!(
        "ALTER TABLE {OPTION_CHAIN_1M_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_OPTION_CHAIN_1M});"
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
                metrics::counter!("tv_chain1m_persist_errors_total", "stage" => "ensure_ddl")
                    .increment(1);
                error!(code = "CHAIN-03", stage = "ensure_ddl",
                    %status, ddl = ddl.as_str(),
                    body = %body.chars().take(200).collect::<String>(),
                    "CHAIN-03: option_chain_1m DDL returned non-2xx (dedup may \
                     be missing — duplicate-row window until a later ensure \
                     succeeds)");
            }
            Err(err) => {
                metrics::counter!("tv_chain1m_persist_errors_total", "stage" => "ensure_ddl")
                    .increment(1);
                error!(
                    code = "CHAIN-03",
                    stage = "ensure_ddl",
                    ?err,
                    ddl = ddl.as_str(),
                    "CHAIN-03: option_chain_1m DDL request failed"
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
fn option_chain_1m_ilp_http_conf(config: &QuestDbConfig) -> String {
    format!(
        "http::addr={}:{};protocol_version=1;retry_timeout=0;request_timeout=5000;",
        config.host, config.http_port
    )
}

/// Lazy ILP-over-HTTP writer for `option_chain_1m`. Mirrors
/// `Spot1mRestWriter`: unreachable QuestDB at construction still builds
/// (rows buffer locally); `flush` returns `Err` — incl. server-side rejects
/// via the HTTP ACK — and DISCARDS pending on failure (poisoned-buffer
/// defense; rows are re-fetchable + DEDUP-idempotent).
pub struct OptionChain1mWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl OptionChain1mWriter {
    /// Production constructor — ILP-over-HTTP sender, lazy on failure.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (lazy-build contract exercised via test_chain1m_writer_new_is_lazy...); append/flush paths covered via for_test()
    pub fn new(config: &QuestDbConfig) -> Self {
        let conf = option_chain_1m_ilp_http_conf(config);
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
                    "option_chain_1m writer: QuestDB unreachable — buffering locally"
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

    /// Appends one option-chain leg row (cold path, ≤~3K rows/minute,
    /// batched: the fetcher appends a whole minute then flushes once).
    ///
    /// # Errors
    /// Propagates ILP buffer errors (table/column append failure).
    pub fn append_row(&mut self, r: &OptionChain1mRow) -> Result<()> {
        self.buffer
            .table(OPTION_CHAIN_1M_TABLE)
            .context("table")?
            // Symbols BEFORE columns (ILP tags-before-fields rule).
            .symbol("exchange_segment", OPTION_CHAIN_1M_SEGMENT_IDX_I)
            .context("exchange_segment")?
            .symbol("feed", OPTION_CHAIN_1M_FEED_DHAN)
            .context("feed")?
            .symbol("source", OPTION_CHAIN_1M_SOURCE)
            .context("source")?
            .symbol("underlying_symbol", r.underlying_symbol)
            .context("underlying_symbol")?
            .symbol("leg", r.leg)
            .context("leg")?
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(r.trading_date_ist_nanos),
            )
            .context("trading_date_ist")?
            .column_i64("underlying_security_id", r.underlying_security_id)
            .context("underlying_security_id")?
            .column_ts("expiry", TimestampNanos::new(r.expiry_ist_nanos))
            .context("expiry")?
            .column_f64("strike", r.strike)
            .context("strike")?
            .column_i64("contract_security_id", r.contract_security_id)
            .context("contract_security_id")?
            .column_f64("last_price", r.last_price)
            .context("last_price")?
            .column_f64("iv", r.iv)
            .context("iv")?
            .column_f64("delta", r.delta)
            .context("delta")?
            .column_f64("theta", r.theta)
            .context("theta")?
            .column_f64("gamma", r.gamma)
            .context("gamma")?
            .column_f64("vega", r.vega)
            .context("vega")?
            .column_i64("oi", r.oi)
            .context("oi")?
            .column_i64("volume", r.volume)
            .context("volume")?
            .column_i64("previous_oi", r.previous_oi)
            .context("previous_oi")?
            .column_f64("underlying_spot", r.underlying_spot)
            .context("underlying_spot")?
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
    /// shadow-writer `discard_pending` precedent): a server-REJECTED row
    /// retained across flushes would be re-sent every minute forever and
    /// block ALL later rows for the session. The rows are re-fetchable and
    /// DEDUP-idempotent, so the durable floor for a discarded minute is a
    /// re-fetch/backfill, and the miss is LOUD (CHAIN-03 error at the call
    /// site + `tv_chain1m_rows_discarded_total` + the minute feeds the
    /// CHAIN-02 failure edge).
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
                "option_chain_1m: no ILP sender (QuestDB unreachable) — \
                 {dropped} pending row(s) discarded (re-fetchable, DEDUP-idempotent)"
            );
        }
        let flushed = self
            .sender
            .as_mut()
            .map(|sender| sender.flush(&mut self.buffer));
        match flushed {
            Some(Ok(())) => {
                metrics::counter!("tv_chain1m_rows_written_total").increment(self.pending as u64);
                self.pending = 0;
                Ok(())
            }
            Some(Err(err)) => {
                let dropped = self.discard_pending();
                Err(anyhow::Error::new(err).context(format!(
                    "option_chain_1m ILP flush failed — {dropped} pending row(s) \
                     discarded (poisoned-buffer defense; rows are re-fetchable)"
                )))
            }
            // Unreachable (checked above) — treated as the no-sender arm.
            None => {
                let dropped = self.discard_pending();
                anyhow::bail!("option_chain_1m: ILP sender vanished — {dropped} row(s) discarded");
            }
        }
    }

    /// Drop every buffered-but-unflushed row (poisoned-buffer defense).
    /// Returns the discarded row count; counted by
    /// `tv_chain1m_rows_discarded_total` so a discard is never silent.
    pub fn discard_pending(&mut self) -> usize {
        let dropped = self.pending;
        if dropped > 0 {
            metrics::counter!("tv_chain1m_rows_discarded_total").increment(dropped as u64);
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

    fn sample_row() -> OptionChain1mRow {
        OptionChain1mRow {
            // 2026-07-10 09:15:00 IST-as-epoch minute-open (illustrative).
            ts_ist_nanos: 1_770_000_900_000_000_000,
            trading_date_ist_nanos: 1_769_990_400_000_000_000,
            underlying_security_id: 13,
            underlying_symbol: "NIFTY",
            // 2026-07-16 IST midnight (illustrative weekly expiry).
            expiry_ist_nanos: 1_770_508_800_000_000_000,
            strike: 25_650.0,
            leg: OPTION_CHAIN_1M_LEG_CE,
            contract_security_id: 42_528,
            last_price: 134.0,
            iv: 9.789,
            delta: 0.538_71,
            theta: -15.153_9,
            gamma: 0.001_32,
            vega: 12.185_93,
            oi: 3_786_445,
            volume: 117_567_970,
            previous_oi: 402_220,
            underlying_spot: 25_642.8,
            fetched_at_ist_nanos: 1_770_000_961_042_000_000,
        }
    }

    #[test]
    fn test_option_chain_1m_create_ddl_contains_expected_columns() {
        let ddl = option_chain_1m_create_ddl();
        assert!(ddl.contains("ts "), "designated ts missing: {ddl}");
        for (col, ty) in option_chain_1m_columns() {
            assert!(
                ddl.contains(&format!("{col} ")),
                "DDL missing column {col:?}: {ddl}"
            );
            assert!(!ty.is_empty());
        }
        assert!(ddl.contains("CREATE TABLE IF NOT EXISTS"));
        assert!(ddl.contains("PARTITION BY DAY"));
        assert!(ddl.contains(&format!("DEDUP UPSERT KEYS({DEDUP_KEY_OPTION_CHAIN_1M})")));
    }

    /// DEDUP-key discipline: designated `ts` FIRST (2026-04-28 regression
    /// rule); `exchange_segment` with the underlying id (I-P1-11); `feed`
    /// in-key (operator override 2026-06-28); per-contract identity via
    /// `expiry` + `strike` + `leg` — whole-token matches, exactly 7 keys.
    #[test]
    fn test_option_chain_1m_dedup_key_shape() {
        assert!(DEDUP_KEY_OPTION_CHAIN_1M.trim_start().starts_with("ts,"));
        let has_token = |t: &str| {
            DEDUP_KEY_OPTION_CHAIN_1M
                .split([',', ' '])
                .map(str::trim)
                .any(|tok| tok == t)
        };
        for key in [
            "underlying_security_id",
            "exchange_segment",
            "expiry",
            "strike",
            "leg",
            "feed",
        ] {
            assert!(has_token(key), "DEDUP key missing {key:?}");
        }
        // Exactly (ts, underlying_security_id, exchange_segment, expiry,
        // strike, leg, feed).
        assert_eq!(DEDUP_KEY_OPTION_CHAIN_1M.matches(',').count() + 1, 7);
        // The per-contract SecurityId is a LABEL, never a key (derivative
        // ids are unstable across relists — instrument-master.md rule 3).
        assert!(!has_token("contract_security_id"));
    }

    #[test]
    fn test_option_chain_1m_symbol_labels_stable() {
        assert_eq!(OPTION_CHAIN_1M_TABLE, "option_chain_1m");
        assert_eq!(OPTION_CHAIN_1M_FEED_DHAN, "dhan");
        assert_eq!(OPTION_CHAIN_1M_SOURCE, "rest_optionchain");
        assert_eq!(OPTION_CHAIN_1M_SEGMENT_IDX_I, "IDX_I");
        assert_eq!(OPTION_CHAIN_1M_LEG_CE, "CE");
        assert_eq!(OPTION_CHAIN_1M_LEG_PE, "PE");
    }

    #[test]
    fn test_chain1m_append_row_writes_symbols_and_columns() {
        let mut w = OptionChain1mWriter::for_test();
        w.append_row(&sample_row()).expect("append must succeed");
        assert_eq!(w.pending(), 1);
        let line = w.buffer_utf8();
        assert!(line.starts_with(OPTION_CHAIN_1M_TABLE));
        assert!(
            line.contains(",exchange_segment=IDX_I"),
            "segment tag missing: {line}"
        );
        assert!(line.contains(",feed=dhan"), "feed tag missing: {line}");
        assert!(
            line.contains(",source=rest_optionchain"),
            "source tag missing: {line}"
        );
        assert!(
            line.contains(",underlying_symbol=NIFTY"),
            "underlying tag missing: {line}"
        );
        assert!(line.contains(",leg=CE"), "leg tag missing: {line}");
        assert!(
            line.contains("underlying_security_id=13i"),
            "underlying sid missing: {line}"
        );
        assert!(
            line.contains("contract_security_id=42528i"),
            "contract sid missing: {line}"
        );
        assert!(line.contains("strike=25650"), "strike missing: {line}");
        assert!(line.contains("oi=3786445i"), "oi missing: {line}");
        assert!(
            line.contains("previous_oi=402220i"),
            "previous_oi missing: {line}"
        );
        assert!(line.contains("volume=117567970i"), "volume missing: {line}");
    }

    #[test]
    fn test_chain1m_flush_when_disconnected_errors_and_discards_pending() {
        // Poisoned-buffer defense (the spot-writer M2 precedent): a failed
        // flush DISCARDS the pending buffer — one rejected row can never
        // wedge the rest of the session; rows are re-fetchable +
        // DEDUP-idempotent.
        let mut w = OptionChain1mWriter::for_test();
        w.append_row(&sample_row()).expect("append must succeed");
        let err = w.flush().expect_err("disconnected flush must error");
        assert!(err.to_string().contains("no ILP sender"));
        assert!(err.to_string().contains("discarded"));
        assert_eq!(w.pending(), 0, "failed flush discards pending");
        assert!(w.buffer_utf8().is_empty(), "ILP buffer cleared on discard");
        // Empty flush is a no-op Ok.
        let mut empty = OptionChain1mWriter::for_test();
        assert!(empty.flush().is_ok());
    }

    /// `discard_pending` clears BOTH the row count and the ILP buffer so
    /// the next fire starts from a clean slate (shadow-writer precedent).
    #[test]
    fn test_chain1m_discard_pending_clears_buffer_and_count() {
        let mut w = OptionChain1mWriter::for_test();
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
    fn test_chain1m_writer_uses_ilp_http_conf_with_bounded_knobs() {
        let cfg = QuestDbConfig {
            host: "tv-questdb".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let conf = option_chain_1m_ilp_http_conf(&cfg);
        assert_eq!(
            conf,
            "http::addr=tv-questdb:9000;protocol_version=1;retry_timeout=0;request_timeout=5000;"
        );
        assert!(!conf.contains("9009"), "must not target ILP TCP: {conf}");
    }

    // ========================================================================
    // Persistence-helper tests — mock QuestDB /exec HTTP server + unreachable
    // host (the spot_1m_rest_persistence pattern). These exercise the real
    // ensure/constructor code paths: success (200), non-2xx (500) and
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
    async fn test_ensure_option_chain_1m_table_mock_200_completes() {
        // Success path: the CREATE + every ADD COLUMN self-heal + the DEDUP
        // ENABLE all take the Ok(2xx) arm.
        let port = spawn_mock_http(MOCK_HTTP_200).await;
        ensure_option_chain_1m_table(&mock_cfg(port)).await;
    }

    #[tokio::test]
    async fn test_ensure_option_chain_1m_table_mock_500_degrades_without_panic() {
        // Non-2xx path: every DDL statement takes the log-and-continue arm
        // (best-effort degrade — CHAIN-03, never a panic, never blocks).
        let port = spawn_mock_http(MOCK_HTTP_500).await;
        ensure_option_chain_1m_table(&mock_cfg(port)).await;
    }

    #[tokio::test]
    async fn test_ensure_option_chain_1m_table_unreachable_degrades_without_panic() {
        // Transport-error path: every DDL send Err arm logs and continues.
        ensure_option_chain_1m_table(&unreachable_cfg()).await;
    }

    #[tokio::test]
    async fn test_chain1m_writer_new_is_lazy_and_buffers_without_network() {
        // `Sender::from_conf` with `http::` does not dial at construction
        // (the ws_event_audit precedent), so new() against an unreachable
        // host still builds a sender-backed writer whose appends land in
        // the local buffer — the lazy-construction contract.
        let mut w = OptionChain1mWriter::new(&unreachable_cfg());
        assert_eq!(w.pending(), 0, "fresh writer has nothing pending");
        w.append_row(&sample_row())
            .expect("append must succeed without network");
        assert_eq!(w.pending(), 1, "row buffered locally");
    }
}

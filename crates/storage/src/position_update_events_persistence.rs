//! `position_update_events` table — full-fidelity position push-event
//! capture (design 2026-07-18; runbook
//! `.claude/rules/project/order-update-events-error-codes.md` ORDER-EVT-01).
//!
//! ONE row PER RECEIVED position push event — every decoded
//! `PositionDetailProto` field (SymbolInfo + PositionInfo NSE/BSE
//! credit/debit legs), replacing the previous decode-and-drop. Absent legs
//! stay NULL (the `Option<f64>` columns are simply omitted), NEVER a
//! fabricated 0.0 — proto3 absent-vs-zero stays distinguishable forever.
//!
//! BEST-EFFORT ONLY: a capture write failure must NEVER affect the push
//! transport or any trading decision — the app-crate consumer logs (coded
//! ORDER-EVT-01, staged) + counts and continues.
//!
//! ## DEDUP discipline
//! Designated `ts` FIRST (2026-04-28 rule); `trading_date_ist` in-key
//! (phase-0 composite-DEDUP rule 1); `feed` in-key (feed-in-key EVERYWHERE,
//! 2026-06-28); per-instrument identity `symbol_isin` (the Groww position
//! wire's own identity key — its numeric ids live in a different id space,
//! so ISIN is the honest key); `event_seq` in-key — burst safety + ILP-retry
//! idempotency. `security_id` is deliberately NOT in the key (best-effort
//! `-1` sentinel when unresolved would poison a key), so the I-P1-11
//! security_id⇒segment DEDUP rule does not bind.

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::broker_order_events::PositionUpdateEventRecord;
use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::sanitize::{MAX_AUDIT_STR_LEN, sanitize_ilp_string, sanitize_ilp_symbol};

use crate::order_update_events_persistence::log_safe_id;

/// QuestDB table name — one row per received position push event.
pub const POSITION_UPDATE_EVENTS_TABLE: &str = "position_update_events";

/// DEDUP key. Designated `ts` FIRST (2026-04-28 regression rule);
/// `trading_date_ist` in-key (phase-0 composite-DEDUP rule 1); `feed`
/// in-key (2026-06-28 override); per-instrument identity `symbol_isin`;
/// `event_seq` in-key (burst-safety + ILP-retry idempotency).
pub const DEDUP_KEY_POSITION_UPDATE_EVENTS: &str =
    "ts, trading_date_ist, feed, symbol_isin, event_seq";

/// `detail_raw` column hard cap — the bounded full-fidelity remainder.
///
/// Equal to [`MAX_AUDIT_STR_LEN`] (1024) BY CONSTRUCTION: the ILP STRING
/// sanitiser (`sanitize_ilp_string`) caps internally at that length, so a
/// larger declared cap here would be dead (review round 1 LOW-1 — the
/// originally-declared 2000 was unreachable). One honest bound, one source.
pub const POSITION_UPDATE_EVENTS_DETAIL_MAX_CHARS: usize = MAX_AUDIT_STR_LEN;

/// Sentinel for a SYMBOL value that is empty/not-applicable at row time
/// (empty ILP tag values are invalid in classic line protocol; `symbol_isin`
/// is IN the DEDUP key, so the constant sentinel gives a stable identity).
pub const POSITION_UPDATE_EVENTS_SYMBOL_NA: &str = "n/a";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// IST nanoseconds per day — for deriving `trading_date_ist` from the
/// record's OWN `ts` at persist time (never a cached boot date).
const NANOS_PER_DAY: i64 = 86_400 * 1_000_000_000;

/// IST midnight (nanos) of the IST instant `ts_ist_nanos`.
#[must_use]
fn ist_midnight_nanos(ts_ist_nanos: i64) -> i64 {
    ts_ist_nanos - ts_ist_nanos.rem_euclid(NANOS_PER_DAY)
}

/// The idempotent `CREATE TABLE` DDL for `position_update_events`. Pure.
// FINANCIAL-TEST-EXEMPT: pure DDL string builder — no financial arithmetic
#[must_use]
pub fn position_update_events_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {POSITION_UPDATE_EVENTS_TABLE} (\
            ts                    TIMESTAMP, \
            trading_date_ist      TIMESTAMP, \
            feed                  SYMBOL, \
            event_seq             LONG, \
            symbol_isin           SYMBOL, \
            security_id           LONG, \
            exchange_segment      SYMBOL, \
            symbol                SYMBOL, \
            exchange              SYMBOL, \
            tr_time_stamp         LONG, \
            search_id             SYMBOL, \
            product               SYMBOL, \
            contract_id           SYMBOL, \
            equity_type           SYMBOL, \
            display_name          SYMBOL, \
            underlying_id         SYMBOL, \
            nse_market_lot        LONG, \
            bse_market_lot        LONG, \
            underlying_asset_type SYMBOL, \
            freeze_qty            LONG, \
            nse_credit_qty        DOUBLE, \
            nse_credit_price      DOUBLE, \
            nse_debit_qty         DOUBLE, \
            nse_debit_price       DOUBLE, \
            bse_credit_qty        DOUBLE, \
            bse_credit_price      DOUBLE, \
            bse_debit_qty         DOUBLE, \
            bse_debit_price       DOUBLE, \
            detail_raw            STRING\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_POSITION_UPDATE_EVENTS});"
    )
}

/// Every non-designated column `(name, type)` — shared by the CREATE DDL
/// self-heal loop so the two can never drift.
const POSITION_UPDATE_EVENTS_COLUMNS: &[(&str, &str)] = &[
    ("trading_date_ist", "TIMESTAMP"),
    ("feed", "SYMBOL"),
    ("event_seq", "LONG"),
    ("symbol_isin", "SYMBOL"),
    ("security_id", "LONG"),
    ("exchange_segment", "SYMBOL"),
    ("symbol", "SYMBOL"),
    ("exchange", "SYMBOL"),
    ("tr_time_stamp", "LONG"),
    ("search_id", "SYMBOL"),
    ("product", "SYMBOL"),
    ("contract_id", "SYMBOL"),
    ("equity_type", "SYMBOL"),
    ("display_name", "SYMBOL"),
    ("underlying_id", "SYMBOL"),
    ("nse_market_lot", "LONG"),
    ("bse_market_lot", "LONG"),
    ("underlying_asset_type", "SYMBOL"),
    ("freeze_qty", "LONG"),
    ("nse_credit_qty", "DOUBLE"),
    ("nse_credit_price", "DOUBLE"),
    ("nse_debit_qty", "DOUBLE"),
    ("nse_debit_price", "DOUBLE"),
    ("bse_credit_qty", "DOUBLE"),
    ("bse_credit_price", "DOUBLE"),
    ("bse_debit_qty", "DOUBLE"),
    ("bse_debit_price", "DOUBLE"),
    ("detail_raw", "STRING"),
];

/// Create the `position_update_events` table if absent (idempotent
/// self-heal order: CREATE → per-column `ALTER ADD COLUMN IF NOT EXISTS`
/// → DEDUP ENABLE). Failures log at `error!` (code ORDER-EVT-01, stage
/// `ensure_*`) but never block boot and never panic; NOTE the
/// HTTP-CLIENT-01-class consequence: a failed ensure leaves the table to be
/// auto-created by the first ILP write WITHOUT dedup (a duplicate-row
/// window until a later ensure succeeds).
// TEST-EXEMPT: live-QuestDB DDL runner (DDL string unit-tested; the 200/500/unreachable arms exercised via the mock-HTTP tokio tests below)
// FINANCIAL-TEST-EXEMPT: idempotent DDL runner — no financial arithmetic
pub async fn ensure_position_update_events_table(questdb_config: &QuestDbConfig) {
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
            metrics::counter!("tv_order_update_events_persist_errors_total", "stage" => "ensure_client_build")
                .increment(1);
            error!(
                code = ErrorCode::OrderEvt01PersistFailed.code_str(),
                stage = "ensure_client_build",
                ?err,
                "ORDER-EVT-01: HTTP client build failed — position_update_events \
                 table not ensured (first ILP write may auto-create it WITHOUT \
                 dedup — duplicate-row window until the next successful boot)"
            );
            return;
        }
    };
    let mut statements = vec![position_update_events_create_ddl()];
    // Per-column self-heal for tables created by earlier builds
    // (observability-architecture.md schema-self-heal pattern).
    for (col, ty) in POSITION_UPDATE_EVENTS_COLUMNS {
        statements.push(format!(
            "ALTER TABLE {POSITION_UPDATE_EVENTS_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty};"
        ));
    }
    statements.push(format!(
        "ALTER TABLE {POSITION_UPDATE_EVENTS_TABLE} DEDUP ENABLE UPSERT \
         KEYS({DEDUP_KEY_POSITION_UPDATE_EVENTS});"
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
                metrics::counter!("tv_order_update_events_persist_errors_total", "stage" => "ensure_ddl")
                    .increment(1);
                error!(code = ErrorCode::OrderEvt01PersistFailed.code_str(),
                    stage = "ensure_ddl",
                    %status, ddl = %ddl.chars().take(80).collect::<String>(),
                    body = %body.chars().take(200).collect::<String>(),
                    "ORDER-EVT-01: position_update_events DDL returned non-2xx \
                     (dedup may be missing — duplicate-row window until a later \
                     ensure succeeds)");
            }
            Err(err) => {
                metrics::counter!("tv_order_update_events_persist_errors_total", "stage" => "ensure_ddl")
                    .increment(1);
                error!(
                    code = ErrorCode::OrderEvt01PersistFailed.code_str(),
                    stage = "ensure_ddl",
                    ?err,
                    ddl = %ddl.chars().take(80).collect::<String>(),
                    "ORDER-EVT-01: position_update_events DDL request failed"
                );
            }
        }
    }
}

/// ILP-over-HTTP conf — per-flush server ACK (the 2026-07-05
/// fire-and-forget lesson) with the shadow-candle-writer knobs.
fn position_update_events_ilp_http_conf(config: &QuestDbConfig) -> String {
    format!(
        "http::addr={}:{};protocol_version=1;retry_timeout=0;request_timeout=5000;",
        config.host, config.http_port
    )
}

/// Lazy ILP-over-HTTP writer for `position_update_events`. Unreachable
/// QuestDB at construction still builds (rows buffer locally); a failed
/// flush DISCARDS the pending buffer (poisoned-buffer defense) — the rows
/// are best-effort forensics; the discard is counted + coded, never silent.
pub struct PositionUpdateEventsWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl PositionUpdateEventsWriter {
    /// Production constructor — ILP-over-HTTP sender, lazy on failure.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (lazy-build contract exercised via test_position_update_events_writer_new_is_lazy...); append/flush paths covered via for_test()
    pub fn new(config: &QuestDbConfig) -> Self {
        let conf = position_update_events_ilp_http_conf(config);
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
                    "position_update_events writer: QuestDB unreachable — buffering locally"
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

    /// Appends one full-fidelity position push-event row (cold path). The 8
    /// NSE/BSE credit/debit leg columns are written ONLY when `Some` —
    /// an absent leg stays NULL in QuestDB (proto3 absent-vs-zero stays
    /// distinguishable; never a fabricated 0.0). Non-finite leg values are
    /// clamped to 0.0 + counted; every string is sanitized through the
    /// house choke point; `detail_raw` is bounded at
    /// [`POSITION_UPDATE_EVENTS_DETAIL_MAX_CHARS`]. `trading_date_ist` is
    /// derived from the record's OWN `ts` (never a cached boot date).
    ///
    /// # Errors
    /// Propagates ILP buffer errors (table/column append failure).
    pub fn append_position_update_event(&mut self, r: &PositionUpdateEventRecord) -> Result<()> {
        let clamp = |value: f64, field: &'static str| -> f64 {
            if value.is_finite() {
                value
            } else {
                metrics::counter!("tv_order_update_events_nonfinite_clamped_total").increment(1);
                warn!(
                    symbol_isin = %log_safe_id(&r.symbol_isin),
                    field,
                    raw = value,
                    "position_update_events: non-finite value clamped to 0.0 to avoid QuestDB reject"
                );
                0.0
            }
        };
        // Never write an empty ILP SYMBOL value — map to the "n/a" house
        // sentinel (checked AFTER sanitize, so a hostile all-control-char
        // input cannot sneak an empty tag through either). SYMBOL values go
        // through the ILP TAG sanitiser (`sanitize_ilp_symbol` — strips the
        // tag delimiters `,`/`=` + control chars, 256-byte cap); the SQL
        // sanitiser (`sanitize_audit_string`) is the WRONG choke point for
        // ILP (quote-doubling would corrupt stored text — review round 1
        // SECURITY MEDIUM-1).
        let symbol_or_na = |value: &str| -> String {
            let sanitized = sanitize_ilp_symbol(value);
            if sanitized.is_empty() {
                POSITION_UPDATE_EVENTS_SYMBOL_NA.to_string()
            } else {
                sanitized.into_owned()
            }
        };
        // STRING (field) value goes through the ILP STRING sanitiser —
        // preserves `,`/`=`/`'`/`;`/`"` verbatim (questdb-rs escapes the
        // genuinely ILP-special chars itself), strips control/BiDi, and
        // caps internally at MAX_AUDIT_STR_LEN.
        let detail_raw: String = sanitize_ilp_string(&r.detail_raw)
            .chars()
            .take(POSITION_UPDATE_EVENTS_DETAIL_MAX_CHARS)
            .collect();
        let buf = self
            .buffer
            .table(POSITION_UPDATE_EVENTS_TABLE)
            .context("table")?
            // Symbols BEFORE columns (ILP tags-before-fields rule).
            .symbol("feed", r.feed.as_str())
            .context("feed")?
            .symbol("symbol_isin", symbol_or_na(&r.symbol_isin))
            .context("symbol_isin")?
            .symbol("exchange_segment", symbol_or_na(&r.exchange_segment))
            .context("exchange_segment")?
            .symbol("symbol", symbol_or_na(&r.symbol))
            .context("symbol")?
            .symbol("exchange", symbol_or_na(&r.exchange))
            .context("exchange")?
            .symbol("search_id", symbol_or_na(&r.search_id))
            .context("search_id")?
            .symbol("product", symbol_or_na(&r.product))
            .context("product")?
            .symbol("contract_id", symbol_or_na(&r.contract_id))
            .context("contract_id")?
            .symbol("equity_type", symbol_or_na(&r.equity_type))
            .context("equity_type")?
            .symbol("display_name", symbol_or_na(&r.display_name))
            .context("display_name")?
            .symbol("underlying_id", symbol_or_na(&r.underlying_id))
            .context("underlying_id")?
            .symbol(
                "underlying_asset_type",
                symbol_or_na(&r.underlying_asset_type),
            )
            .context("underlying_asset_type")?
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(ist_midnight_nanos(r.ts_ist_nanos)),
            )
            .context("trading_date_ist")?
            .column_i64("event_seq", r.event_seq)
            .context("event_seq")?
            .column_i64("security_id", r.security_id)
            .context("security_id")?
            .column_i64("tr_time_stamp", r.tr_time_stamp)
            .context("tr_time_stamp")?
            .column_i64("nse_market_lot", r.nse_market_lot)
            .context("nse_market_lot")?
            .column_i64("bse_market_lot", r.bse_market_lot)
            .context("bse_market_lot")?
            .column_i64("freeze_qty", r.freeze_qty)
            .context("freeze_qty")?;
        // The 8 Option<f64> legs — column omitted when None (NULL in
        // QuestDB), so an absent proto3 leg never becomes a fabricated 0.0.
        let legs: [(&'static str, Option<f64>); 8] = [
            ("nse_credit_qty", r.nse_credit_qty),
            ("nse_credit_price", r.nse_credit_price),
            ("nse_debit_qty", r.nse_debit_qty),
            ("nse_debit_price", r.nse_debit_price),
            ("bse_credit_qty", r.bse_credit_qty),
            ("bse_credit_price", r.bse_credit_price),
            ("bse_debit_qty", r.bse_debit_qty),
            ("bse_debit_price", r.bse_debit_price),
        ];
        let mut buf = buf;
        for (name, value) in legs {
            if let Some(v) = value {
                buf = buf.column_f64(name, clamp(v, name)).context(name)?;
            }
        }
        buf.column_str("detail_raw", detail_raw.as_str())
            .context("detail_raw")?
            .at(TimestampNanos::new(r.ts_ist_nanos))
            .context("designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        metrics::counter!("tv_position_update_events_rows_total", "feed" => r.feed.as_str())
            .increment(1);
        Ok(())
    }

    /// Flushes buffered rows over ILP-HTTP (per-flush server ACK). On ANY
    /// failed flush the pending buffer is DISCARDED (poisoned-buffer
    /// defense) — the rows are best-effort forensics; the discard is
    /// counted (`tv_order_update_events_rows_discarded_total`), never silent.
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
                "position_update_events: no ILP sender (QuestDB unreachable) — \
                 {dropped} pending capture row(s) discarded (best-effort)"
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
                    "position_update_events ILP flush failed — {dropped} pending \
                     capture row(s) discarded (poisoned-buffer defense)"
                )))
            }
            // Unreachable (checked above) — treated as the no-sender arm.
            None => {
                let dropped = self.discard_pending();
                anyhow::bail!(
                    "position_update_events: ILP sender vanished — {dropped} row(s) discarded"
                );
            }
        }
    }

    /// Drop every buffered-but-unflushed row (poisoned-buffer defense).
    /// Returns the discarded row count; counted so a discard is never silent.
    pub fn discard_pending(&mut self) -> usize {
        let dropped = self.pending;
        if dropped > 0 {
            metrics::counter!("tv_order_update_events_rows_discarded_total")
                .increment(dropped as u64);
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
    use tickvault_common::feed::Feed;

    fn sample_record() -> PositionUpdateEventRecord {
        PositionUpdateEventRecord {
            ts_ist_nanos: 1_770_000_900_000_000_000,
            feed: Feed::Groww,
            event_seq: 7,
            symbol_isin: "INE002A01018".to_string(),
            security_id: -1,
            exchange_segment: "NSE_EQ".to_string(),
            symbol: "RELIANCE".to_string(),
            exchange: "NSE".to_string(),
            tr_time_stamp: 1_770_000_899,
            search_id: "reliance-industries-ltd".to_string(),
            product: "MIS".to_string(),
            contract_id: String::new(),
            equity_type: "EQ".to_string(),
            display_name: "Reliance Industries".to_string(),
            underlying_id: String::new(),
            nse_market_lot: 1,
            bse_market_lot: 0,
            underlying_asset_type: String::new(),
            freeze_qty: 0,
            nse_credit_qty: Some(10.0),
            nse_credit_price: Some(2_847.50),
            nse_debit_qty: None,
            nse_debit_price: None,
            bse_credit_qty: None,
            bse_credit_price: None,
            bse_debit_qty: None,
            bse_debit_price: None,
            detail_raw: "position_valid=true".to_string(),
        }
    }

    #[test]
    fn test_position_update_events_ddl_contains_expected_columns() {
        let ddl = position_update_events_create_ddl();
        // Every self-heal column must appear in the CREATE DDL (the two
        // lists can never drift).
        for (col, _) in POSITION_UPDATE_EVENTS_COLUMNS {
            assert!(ddl.contains(col), "DDL missing column {col:?}: {ddl}");
        }
        assert!(ddl.contains("CREATE TABLE IF NOT EXISTS"));
        assert!(ddl.contains("PARTITION BY DAY"));
        assert!(ddl.contains(&format!(
            "DEDUP UPSERT KEYS({DEDUP_KEY_POSITION_UPDATE_EVENTS})"
        )));
    }

    /// DEDUP-key discipline: designated `ts` FIRST (2026-04-28 regression
    /// rule); `trading_date_ist` in-key (phase-0 composite-DEDUP rule 1);
    /// `feed` in-key (2026-06-28 override); `symbol_isin` identity;
    /// `event_seq` in-key (burst safety) — EXACT arity 5.
    #[test]
    fn test_dedup_key_position_update_events_exact() {
        assert_eq!(
            DEDUP_KEY_POSITION_UPDATE_EVENTS,
            "ts, trading_date_ist, feed, symbol_isin, event_seq"
        );
        assert!(
            DEDUP_KEY_POSITION_UPDATE_EVENTS
                .trim_start()
                .starts_with("ts,")
        );
        // Per-instrument identity is the ISIN — deliberately NO security_id
        // in-key (best-effort -1 sentinel would poison a key), so the
        // I-P1-11 security_id⇒segment rule does not bind.
        assert!(!DEDUP_KEY_POSITION_UPDATE_EVENTS.contains("security_id"));
    }

    #[test]
    fn test_append_position_update_event_writes_symbols_and_columns() {
        let mut w = PositionUpdateEventsWriter::for_test();
        w.append_position_update_event(&sample_record())
            .expect("append must succeed");
        assert_eq!(w.pending(), 1);
        let line = w.buffer_utf8();
        assert!(line.starts_with(POSITION_UPDATE_EVENTS_TABLE));
        assert!(line.contains(",feed=groww"), "feed tag: {line}");
        assert!(
            line.contains(",symbol_isin=INE002A01018"),
            "symbol_isin tag: {line}"
        );
        assert!(line.contains("event_seq=7i"), "event_seq: {line}");
        assert!(line.contains("security_id=-1i"), "security_id: {line}");
        assert!(line.contains("nse_market_lot=1i"), "nse_market_lot: {line}");
        assert!(line.contains("detail_raw="), "detail_raw column: {line}");
        // Empty SYMBOL fields degrade to the n/a sentinel, never an empty
        // ILP tag.
        assert!(
            line.contains(",contract_id=n/a") && line.contains(",underlying_id=n/a"),
            "empty symbols must map to n/a: {line}"
        );
    }

    /// Absent Option legs are OMITTED (NULL in QuestDB) — never fabricated
    /// 0.0; present legs land as DOUBLE columns.
    #[test]
    fn test_append_position_update_event_absent_legs_stay_null() {
        let mut w = PositionUpdateEventsWriter::for_test();
        w.append_position_update_event(&sample_record())
            .expect("append must succeed");
        let line = w.buffer_utf8();
        // Present NSE legs are written…
        assert!(
            line.contains("nse_credit_qty=10"),
            "present leg written: {line}"
        );
        assert!(
            line.contains("nse_credit_price=2847.5"),
            "present leg written: {line}"
        );
        // …absent legs are OMITTED entirely (NULL, not 0.0).
        for absent in [
            "nse_debit_qty",
            "nse_debit_price",
            "bse_credit_qty",
            "bse_credit_price",
            "bse_debit_qty",
            "bse_debit_price",
        ] {
            assert!(
                !line.contains(absent),
                "absent leg {absent} must be omitted (NULL): {line}"
            );
        }
    }

    /// Non-finite leg values clamp to 0.0 instead of poisoning the buffer.
    #[test]
    fn test_append_position_update_event_nonfinite_clamped() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut w = PositionUpdateEventsWriter::for_test();
            let rec = PositionUpdateEventRecord {
                nse_credit_price: Some(bad),
                ..sample_record()
            };
            w.append_position_update_event(&rec)
                .expect("append must succeed");
            let line = w.buffer_utf8();
            assert!(!line.contains("NaN"), "NaN leaked into ILP: {line}");
            assert!(
                line.contains("nse_credit_price=0"),
                "non-finite must clamp to 0.0: {line}"
            );
        }
    }

    /// `detail_raw` bounded ≤`MAX_AUDIT_STR_LEN` (1024) — a hostile broker
    /// string can never bloat a row.
    #[test]
    fn test_append_position_update_event_detail_cap() {
        let mut w = PositionUpdateEventsWriter::for_test();
        let rec = PositionUpdateEventRecord {
            detail_raw: "d".repeat(5_000),
            ..sample_record()
        };
        w.append_position_update_event(&rec)
            .expect("append must succeed");
        let line = w.buffer_utf8();
        let field = line
            .split("detail_raw=")
            .nth(1)
            .expect("detail_raw present");
        assert!(
            field.matches('d').count() <= POSITION_UPDATE_EVENTS_DETAIL_MAX_CHARS,
            "detail_raw must be capped at MAX_AUDIT_STR_LEN"
        );
    }

    #[test]
    fn test_position_update_events_flush_failure_discards_pending() {
        let mut w = PositionUpdateEventsWriter::for_test();
        w.append_position_update_event(&sample_record())
            .expect("append must succeed");
        let err = w.flush().expect_err("disconnected flush must error");
        assert!(err.to_string().contains("no ILP sender"));
        assert!(err.to_string().contains("discarded"));
        assert_eq!(w.pending(), 0, "failed flush discards pending");
        assert!(w.buffer_utf8().is_empty(), "ILP buffer cleared on discard");
        // Empty flush is a no-op Ok; second discard drops nothing.
        let mut empty = PositionUpdateEventsWriter::for_test();
        assert!(empty.flush().is_ok());
        assert_eq!(w.discard_pending(), 0);
    }

    /// Transport ratchet (2026-07-05 fire-and-forget lesson): ILP-over-HTTP
    /// with the shadow-candle-writer knobs — never ILP TCP 9009.
    #[test]
    fn test_position_update_events_ilp_conf_targets_http_port() {
        let cfg = QuestDbConfig {
            host: "tv-questdb".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let conf = position_update_events_ilp_http_conf(&cfg);
        assert_eq!(
            conf,
            "http::addr=tv-questdb:9000;protocol_version=1;retry_timeout=0;request_timeout=5000;"
        );
        assert!(!conf.contains("9009"), "must not target ILP TCP: {conf}");
    }

    // ========================================================================
    // Mock QuestDB /exec HTTP server + unreachable host (the order_audit
    // pattern) — exercises the real ensure / constructor code paths.
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
    async fn test_ensure_position_update_events_table_mock_200_completes() {
        let port = spawn_mock_http(MOCK_HTTP_200).await;
        ensure_position_update_events_table(&mock_cfg(port)).await;
    }

    #[tokio::test]
    async fn test_ensure_position_update_events_table_mock_500_degrades_without_panic() {
        let port = spawn_mock_http(MOCK_HTTP_500).await;
        ensure_position_update_events_table(&mock_cfg(port)).await;
    }

    #[tokio::test]
    async fn test_ensure_position_update_events_table_unreachable_degrades_without_panic() {
        ensure_position_update_events_table(&unreachable_cfg()).await;
    }

    #[tokio::test]
    async fn test_position_update_events_writer_new_is_lazy_and_buffers_without_network() {
        let mut w = PositionUpdateEventsWriter::new(&unreachable_cfg());
        assert_eq!(w.pending(), 0, "fresh writer has nothing pending");
        w.append_position_update_event(&sample_record())
            .expect("append must succeed without network");
        assert_eq!(w.pending(), 1, "row buffered locally");
    }

    /// The `Some(Err(..))` flush arm — a REAL server reject through a live
    /// (lazily-built) HTTP sender — exercises the poisoned-buffer discard,
    /// not just the no-sender bail.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_position_update_events_flush_server_reject_discards() {
        let port = spawn_mock_http(MOCK_HTTP_500).await;
        let mut w = PositionUpdateEventsWriter::new(&mock_cfg(port));
        w.append_position_update_event(&sample_record())
            .expect("append must succeed");
        assert_eq!(w.pending(), 1);
        let err = w.flush().expect_err("server 500 must surface as Err");
        assert!(
            err.to_string().contains("discarded"),
            "discard context expected: {err:#}"
        );
        assert_eq!(w.pending(), 0, "failed flush discards pending");
        assert!(w.buffer_utf8().is_empty(), "ILP buffer cleared on discard");
    }
}

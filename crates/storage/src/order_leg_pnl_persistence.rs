//! Cold-path QuestDB persistence for dry-run per-leg option P&L snapshots.
//!
//! Mirrors the `order_update_events_persistence` house pattern: idempotent DDL
//! over `/exec`, a lazy ILP-over-HTTP writer with non-finite clamping and the
//! poisoned-buffer discard defense. Rows are mark/fill snapshots emitted by the
//! paper order runtime (`dry_run` hard-true) — log-sink-only observability.
//!
//! DEDUP carries `feed` per the I-P1-11 extension plus the composite
//! `(security_id, segment)` identity, so a Dhan row and a Groww row for the
//! same contract second stay distinct and re-appends are idempotent.

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::sanitize::sanitize_ilp_symbol;
use tracing::{error, warn};

/// QuestDB table holding per-leg P&L snapshots (paper mode only).
pub const ORDER_LEG_PNL_TABLE: &str = "order_leg_pnl";

/// DEDUP UPSERT KEYS — `feed` per the I-P1-11 extension; `segment` beside
/// `security_id` per the composite-uniqueness rule; `event_seq` keeps
/// same-second bursts distinct while an ILP retry collapses idempotently.
pub const DEDUP_KEY_ORDER_LEG_PNL: &str =
    "ts, trading_date_ist, feed, security_id, segment, event_seq";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;
const NANOS_PER_DAY: i64 = 86_400 * 1_000_000_000;

/// Floor an IST wall-clock nanosecond timestamp to IST midnight.
fn ist_midnight_nanos(ts_ist_nanos: i64) -> i64 {
    ts_ist_nanos - ts_ist_nanos.rem_euclid(NANOS_PER_DAY)
}

/// Column set (name, QuestDB type) — the designated `ts` column is separate.
const ORDER_LEG_PNL_COLUMNS: &[(&str, &str)] = &[
    ("trading_date_ist", "TIMESTAMP"),
    ("feed", "SYMBOL"),
    ("security_id", "LONG"),
    ("segment", "SYMBOL"),
    ("event_seq", "LONG"),
    ("event_kind", "SYMBOL"),
    ("underlying", "SYMBOL"),
    ("expiry", "SYMBOL"),
    ("strike_paise", "LONG"),
    ("option_type", "SYMBOL"),
    ("net_lots", "INT"),
    ("lot_size", "INT"),
    ("avg_entry_price", "DOUBLE"),
    ("mark_price", "DOUBLE"),
    ("realized_pnl", "DOUBLE"),
    ("unrealized_pnl", "DOUBLE"),
    ("mode", "SYMBOL"),
];

// FINANCIAL-TEST-EXEMPT: DDL string assembly — no price/order arithmetic.
/// Build the idempotent CREATE TABLE statement.
#[must_use]
pub fn order_leg_pnl_create_ddl() -> String {
    let mut cols = String::new();
    for (name, ty) in ORDER_LEG_PNL_COLUMNS {
        cols.push_str(", ");
        cols.push_str(name);
        cols.push(' ');
        cols.push_str(ty);
    }
    format!(
        "CREATE TABLE IF NOT EXISTS {ORDER_LEG_PNL_TABLE} ( ts TIMESTAMP{cols} ) \
         timestamp(ts) PARTITION BY DAY DEDUP UPSERT KEYS({DEDUP_KEY_ORDER_LEG_PNL});"
    )
}

/// Per-column self-heal ALTERs + the final DEDUP re-enable.
fn order_leg_pnl_alter_ddls() -> Vec<String> {
    let mut ddls = Vec::with_capacity(ORDER_LEG_PNL_COLUMNS.len() + 1);
    for (name, ty) in ORDER_LEG_PNL_COLUMNS {
        ddls.push(format!(
            "ALTER TABLE {ORDER_LEG_PNL_TABLE} ADD COLUMN IF NOT EXISTS {name} {ty};"
        ));
    }
    ddls.push(format!(
        "ALTER TABLE {ORDER_LEG_PNL_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_ORDER_LEG_PNL});"
    ));
    ddls
}

// TEST-EXEMPT: thin HTTP wrapper over the DDL builders, which are unit-tested;
// exercising it needs a live QuestDB (covered by the boot integration path).
/// Ensure the table + columns + DEDUP keys exist (best-effort, never fatal).
pub async fn ensure_order_leg_pnl_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            metrics::counter!("tv_order_leg_pnl_persist_errors_total", "stage" => "ensure_client_build")
                .increment(1);
            error!(
                code = ErrorCode::OrderPnl01PersistFailed.code_str(),
                stage = "ensure_client_build",
                error = %err,
                "order_leg_pnl: DDL client build failed — table ensure skipped this boot"
            );
            return;
        }
    };
    let mut ddls = vec![order_leg_pnl_create_ddl()];
    ddls.extend(order_leg_pnl_alter_ddls());
    for ddl in &ddls {
        match client
            .get(&base_url)
            .query(&[("query", ddl.as_str())])
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => {
                metrics::counter!("tv_order_leg_pnl_persist_errors_total", "stage" => "ensure_ddl")
                    .increment(1);
                error!(
                    code = ErrorCode::OrderPnl01PersistFailed.code_str(),
                    stage = "ensure_ddl",
                    status = %response.status(),
                    "order_leg_pnl: DDL refused — duplicate-row window until a later boot's ensure succeeds"
                );
            }
            Err(err) => {
                metrics::counter!("tv_order_leg_pnl_persist_errors_total", "stage" => "ensure_ddl")
                    .increment(1);
                error!(
                    code = ErrorCode::OrderPnl01PersistFailed.code_str(),
                    stage = "ensure_ddl",
                    error = %err,
                    "order_leg_pnl: DDL unreachable — duplicate-row window until a later boot's ensure succeeds"
                );
            }
        }
    }
}

/// ILP-over-HTTP sender conf — HTTP port (9000), never the TCP ILP port.
fn order_leg_pnl_ilp_http_conf(host: &str, http_port: u16) -> String {
    format!(
        "http::addr={}:{};protocol_version=1;retry_timeout=0;request_timeout=5000;",
        host, http_port
    )
}

/// One per-leg P&L snapshot row (mark or fill event).
#[derive(Debug)]
pub struct OrderLegPnlRecord {
    /// IST wall-clock nanoseconds — the designated timestamp.
    pub ts_ist_nanos: i64,
    /// Feed tag (`Feed::as_str()` — e.g. "groww" / "dhan").
    pub feed: &'static str,
    /// Contract security id (exchange token domain).
    pub security_id: i64,
    /// Exchange segment slug ("NSE_FNO" / "BSE_FNO").
    pub segment: &'static str,
    /// Process-global receipt sequence (consumer-stamped).
    pub event_seq: i64,
    /// "mark" or "fill".
    pub event_kind: &'static str,
    /// Underlying symbol ("NIFTY" / "BANKNIFTY" / "SENSEX" / "n/a").
    pub underlying: &'static str,
    /// ISO expiry date, or "n/a" when identity is unresolved.
    pub expiry: String,
    /// Strike in paise (0 when unresolved).
    pub strike_paise: i64,
    /// "CE" / "PE" / "n/a".
    pub option_type: &'static str,
    /// Signed net position in lots.
    pub net_lots: i32,
    /// Contract lot size.
    pub lot_size: u32,
    /// Average entry price (rupees).
    pub avg_entry_price: f64,
    /// Latest mark price (rupees).
    pub mark_price: f64,
    /// Realized P&L (rupees).
    pub realized_pnl: f64,
    /// Unrealized P&L at the mark (rupees).
    pub unrealized_pnl: f64,
}

/// Lazy ILP writer with pending-count tracking + poisoned-buffer discard.
pub struct OrderLegPnlWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl OrderLegPnlWriter {
    // TEST-EXEMPT: needs a reachable ILP endpoint to construct a real sender;
    // the sender-less path is covered via `for_test`.
    /// Build the writer; a failed sender build degrades to buffer-only.
    #[must_use]
    pub fn new(config: &QuestDbConfig) -> Self {
        let conf = order_leg_pnl_ilp_http_conf(&config.host, config.http_port);
        match Sender::from_conf(&conf) {
            Ok(sender) => {
                let buffer = sender.new_buffer();
                Self {
                    sender: Some(sender),
                    buffer,
                    pending: 0,
                }
            }
            Err(err) => {
                warn!(error = %err, "order_leg_pnl: ILP sender build failed — rows will be discarded at flush");
                Self {
                    sender: None,
                    buffer: Buffer::new(ProtocolVersion::V1),
                    pending: 0,
                }
            }
        }
    }

    /// Sender-less writer for tests.
    #[must_use]
    pub fn for_test() -> Self {
        Self {
            sender: None,
            buffer: Buffer::new(ProtocolVersion::V1),
            pending: 0,
        }
    }

    /// Rows appended since the last successful flush/discard.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending
    }

    #[cfg(test)]
    fn buffer_utf8(&self) -> String {
        String::from_utf8_lossy(self.buffer.as_bytes()).into_owned()
    }

    /// Append one snapshot row to the ILP buffer.
    pub fn append(&mut self, record: &OrderLegPnlRecord) -> Result<()> {
        let clamp = |value: f64, field: &'static str| -> f64 {
            if value.is_finite() {
                value
            } else {
                metrics::counter!("tv_order_leg_pnl_nonfinite_clamped_total").increment(1);
                warn!(field, "order_leg_pnl: non-finite value clamped to 0.0");
                0.0
            }
        };
        let symbol_or_na = |raw: &str| -> String {
            let cleaned = sanitize_ilp_symbol(raw);
            if cleaned.is_empty() {
                "n/a".to_string()
            } else {
                cleaned.into_owned()
            }
        };
        self.buffer
            .table(ORDER_LEG_PNL_TABLE)
            .context("order_leg_pnl: table")?;
        self.buffer
            .symbol("feed", record.feed)
            .context("order_leg_pnl: feed")?;
        self.buffer
            .symbol("segment", record.segment)
            .context("order_leg_pnl: segment")?;
        self.buffer
            .symbol("event_kind", record.event_kind)
            .context("order_leg_pnl: event_kind")?;
        self.buffer
            .symbol("underlying", record.underlying)
            .context("order_leg_pnl: underlying")?;
        self.buffer
            .symbol("expiry", symbol_or_na(&record.expiry))
            .context("order_leg_pnl: expiry")?;
        self.buffer
            .symbol("option_type", record.option_type)
            .context("order_leg_pnl: option_type")?;
        self.buffer
            .symbol("mode", "paper")
            .context("order_leg_pnl: mode")?;
        self.buffer
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(ist_midnight_nanos(record.ts_ist_nanos)),
            )
            .context("order_leg_pnl: trading_date_ist")?;
        self.buffer
            .column_i64("security_id", record.security_id)
            .context("order_leg_pnl: security_id")?;
        self.buffer
            .column_i64("event_seq", record.event_seq)
            .context("order_leg_pnl: event_seq")?;
        self.buffer
            .column_i64("strike_paise", record.strike_paise)
            .context("order_leg_pnl: strike_paise")?;
        self.buffer
            .column_i64("net_lots", i64::from(record.net_lots))
            .context("order_leg_pnl: net_lots")?;
        self.buffer
            .column_i64("lot_size", i64::from(record.lot_size))
            .context("order_leg_pnl: lot_size")?;
        self.buffer
            .column_f64(
                "avg_entry_price",
                clamp(record.avg_entry_price, "avg_entry_price"),
            )
            .context("order_leg_pnl: avg_entry_price")?;
        self.buffer
            .column_f64("mark_price", clamp(record.mark_price, "mark_price"))
            .context("order_leg_pnl: mark_price")?;
        self.buffer
            .column_f64("realized_pnl", clamp(record.realized_pnl, "realized_pnl"))
            .context("order_leg_pnl: realized_pnl")?;
        self.buffer
            .column_f64(
                "unrealized_pnl",
                clamp(record.unrealized_pnl, "unrealized_pnl"),
            )
            .context("order_leg_pnl: unrealized_pnl")?;
        self.buffer
            .at(TimestampNanos::new(record.ts_ist_nanos))
            .context("order_leg_pnl: designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        metrics::counter!("tv_order_leg_pnl_rows_total", "feed" => record.feed).increment(1);
        Ok(())
    }

    /// Flush pending rows; a refused flush discards them (poisoned-buffer defense).
    pub fn flush(&mut self) -> Result<()> {
        if self.pending == 0 {
            return Ok(());
        }
        match self
            .sender
            .as_mut()
            .map(|sender| sender.flush(&mut self.buffer))
        {
            Some(Ok(())) => {
                self.pending = 0;
                Ok(())
            }
            Some(Err(err)) => {
                let dropped = self.discard_pending();
                Err(anyhow::Error::new(err).context(format!(
                    "order_leg_pnl: ILP flush rejected — {dropped} row(s) discarded (poisoned-buffer defense)"
                )))
            }
            None => {
                let dropped = self.discard_pending();
                anyhow::bail!("order_leg_pnl: no ILP sender — {dropped} row(s) discarded")
            }
        }
    }

    /// Drop buffered rows (counted); returns how many were discarded.
    pub fn discard_pending(&mut self) -> usize {
        let dropped = self.pending;
        if dropped > 0 {
            metrics::counter!("tv_order_leg_pnl_rows_discarded_total").increment(dropped as u64);
        }
        self.buffer.clear();
        self.pending = 0;
        dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_record() -> OrderLegPnlRecord {
        OrderLegPnlRecord {
            ts_ist_nanos: 1_770_000_900_000_000_000,
            feed: "groww",
            security_id: 12_345,
            segment: "NSE_FNO",
            event_seq: 7,
            event_kind: "mark",
            underlying: "NIFTY",
            expiry: "2026-07-30".to_string(),
            strike_paise: 2_450_000,
            option_type: "CE",
            net_lots: -2,
            lot_size: 75,
            avg_entry_price: 123.45,
            mark_price: 120.10,
            realized_pnl: 0.0,
            unrealized_pnl: 502.5,
        }
    }

    #[test]
    fn test_order_leg_pnl_ddl_idempotent() {
        let create = order_leg_pnl_create_ddl();
        assert!(create.starts_with("CREATE TABLE IF NOT EXISTS order_leg_pnl"));
        assert!(create.contains("timestamp(ts) PARTITION BY DAY"));
        assert!(create.contains(&format!("DEDUP UPSERT KEYS({DEDUP_KEY_ORDER_LEG_PNL})")));
        for (name, _ty) in ORDER_LEG_PNL_COLUMNS {
            assert!(create.contains(name), "create DDL missing column {name}");
        }
        let alters = order_leg_pnl_alter_ddls();
        assert_eq!(alters.len(), ORDER_LEG_PNL_COLUMNS.len() + 1);
        for (ddl, (name, ty)) in alters.iter().zip(ORDER_LEG_PNL_COLUMNS) {
            assert!(ddl.contains("ADD COLUMN IF NOT EXISTS"));
            assert!(ddl.contains(name));
            assert!(ddl.contains(ty));
        }
        assert!(
            alters
                .last()
                .expect("alter list non-empty")
                .contains("DEDUP ENABLE UPSERT KEYS")
        );
    }

    #[test]
    fn test_order_leg_pnl_dedup_key_has_feed_and_segment() {
        assert_eq!(
            DEDUP_KEY_ORDER_LEG_PNL,
            "ts, trading_date_ist, feed, security_id, segment, event_seq"
        );
        assert!(DEDUP_KEY_ORDER_LEG_PNL.starts_with("ts,"));
        assert!(DEDUP_KEY_ORDER_LEG_PNL.contains("feed"));
        assert!(DEDUP_KEY_ORDER_LEG_PNL.contains("security_id"));
        assert!(DEDUP_KEY_ORDER_LEG_PNL.contains("segment"));
    }

    #[test]
    fn test_order_leg_pnl_flush_failure_discards() {
        let mut writer = OrderLegPnlWriter::for_test();
        writer.append(&sample_record()).expect("append succeeds");
        assert_eq!(writer.pending(), 1);
        let err = writer.flush().expect_err("sender-less flush fails");
        assert!(format!("{err:#}").contains("discarded"));
        assert_eq!(writer.pending(), 0);
        writer.flush().expect("empty flush is Ok");
    }

    #[test]
    fn test_order_leg_pnl_nonfinite_clamp() {
        let mut writer = OrderLegPnlWriter::for_test();
        let mut record = sample_record();
        record.mark_price = f64::NAN;
        record.unrealized_pnl = f64::INFINITY;
        record.avg_entry_price = f64::NEG_INFINITY;
        writer.append(&record).expect("append clamps, never fails");
        let buf = writer.buffer_utf8();
        assert!(!buf.contains("NaN"));
        assert!(!buf.contains("Infinity"));
        assert!(buf.contains("mark_price"));
        assert!(buf.contains("realized_pnl"));
    }

    #[test]
    fn test_order_leg_pnl_append_symbols_and_columns() {
        let mut writer = OrderLegPnlWriter::for_test();
        writer.append(&sample_record()).expect("append succeeds");
        let buf = writer.buffer_utf8();
        assert!(buf.starts_with(ORDER_LEG_PNL_TABLE));
        for token in [
            "feed=groww",
            "segment=NSE_FNO",
            "event_kind=mark",
            "underlying=NIFTY",
            "option_type=CE",
            "mode=paper",
            "security_id=",
            "event_seq=",
            "strike_paise=",
            "net_lots=",
            "lot_size=",
            "trading_date_ist=",
        ] {
            assert!(buf.contains(token), "ILP buffer missing token {token}");
        }
    }

    #[test]
    fn test_order_leg_pnl_ilp_conf_targets_http_port() {
        let conf = order_leg_pnl_ilp_http_conf("tv-questdb", 9000);
        assert_eq!(
            conf,
            "http::addr=tv-questdb:9000;protocol_version=1;retry_timeout=0;request_timeout=5000;"
        );
        assert!(!conf.contains("9009"));
    }

    #[test]
    fn test_order_leg_pnl_ist_midnight_floors() {
        assert_eq!(ist_midnight_nanos(0), 0);
        assert_eq!(ist_midnight_nanos(NANOS_PER_DAY - 1), 0);
        assert_eq!(ist_midnight_nanos(NANOS_PER_DAY), NANOS_PER_DAY);
        let ts = sample_record().ts_ist_nanos;
        let floored = ist_midnight_nanos(ts);
        assert_eq!(floored % NANOS_PER_DAY, 0);
        assert!(ts - floored < NANOS_PER_DAY);
    }
}

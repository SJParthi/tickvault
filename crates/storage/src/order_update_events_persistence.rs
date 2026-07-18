//! `order_update_events` table — full-fidelity order push-event capture,
//! BOTH feeds (design 2026-07-18; runbook
//! `.claude/rules/project/order-update-events-error-codes.md` ORDER-EVT-01).
//!
//! ONE row PER RECEIVED order push event — every field the broker's push
//! channel delivered, typed where the schema names it and bounded in
//! `detail_raw` otherwise — so "what exactly did the broker tell us about
//! that order, and when?" is answerable from QuestDB later. This is the
//! capture companion of the lossy 11-field `BrokerOrderEvent` decision seam
//! (which stays UNTOUCHED) and of the `order_audit` lifecycle table (which
//! records OUR actions; this table records the BROKER's words).
//!
//! BEST-EFFORT ONLY: a capture write failure must NEVER affect the order
//! path — the app-crate consumer logs (coded ORDER-EVT-01, staged) + counts
//! and continues; the push read loops are never blocked by this table.
//!
//! ## DEDUP discipline
//! Designated `ts` FIRST (2026-04-28 rule); `trading_date_ist` in-key
//! (phase-0 composite-DEDUP rule 1); `feed` in-key (feed-in-key EVERYWHERE,
//! 2026-06-28); per-order identity `order_id`; `event_seq` in-key — the
//! process-global receipt sequence makes a same-second burst of updates for
//! ONE order survive as distinct rows while an ILP retry of the identical
//! row (same seq) collapses idempotently. `security_id` is deliberately NOT
//! in the key (per-order identity, the order_audit precedent), so the
//! I-P1-11 security_id⇒segment DEDUP rule does not bind.

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::broker_order_events::OrderUpdateEventRecord;
use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::sanitize::{MAX_AUDIT_STR_LEN, sanitize_ilp_string, sanitize_ilp_symbol};

/// QuestDB table name — one row per received order push event.
pub const ORDER_UPDATE_EVENTS_TABLE: &str = "order_update_events";

/// DEDUP key. Designated `ts` FIRST (2026-04-28 regression rule);
/// `trading_date_ist` in-key (phase-0 composite-DEDUP rule 1); `feed`
/// in-key (2026-06-28 override); per-order identity `order_id`;
/// `event_seq` in-key (burst-safety + ILP-retry idempotency).
pub const DEDUP_KEY_ORDER_UPDATE_EVENTS: &str = "ts, trading_date_ist, feed, order_id, event_seq";

/// `reject_reason` column hard cap — bounded broker text, never raw bodies.
pub const ORDER_UPDATE_EVENTS_REJECT_REASON_MAX_CHARS: usize = 300;

/// `remarks` column hard cap — DELIBERATELY the same 300-char bound as
/// `reject_reason` (both are short free-form broker text; a named alias so
/// the reuse is a documented decision, not a silent constant grab —
/// review round 1 Fix 8).
pub const ORDER_UPDATE_EVENTS_REMARKS_MAX_CHARS: usize =
    ORDER_UPDATE_EVENTS_REJECT_REASON_MAX_CHARS;

/// `stage_trail` / `detail_raw` column hard cap — the bounded full-fidelity
/// remainder (a hostile broker string can never bloat a row).
///
/// Equal to [`MAX_AUDIT_STR_LEN`] (1024) BY CONSTRUCTION: the ILP STRING
/// sanitiser (`sanitize_ilp_string`) caps internally at that length, so a
/// larger declared cap here would be dead (review round 1 LOW-1 — the
/// originally-declared 2000 was unreachable). One honest bound, one source.
pub const ORDER_UPDATE_EVENTS_DETAIL_MAX_CHARS: usize = MAX_AUDIT_STR_LEN;

/// Sentinel for a SYMBOL value that is empty/not-applicable at row time
/// (empty ILP tag values are invalid in classic line protocol — the
/// order_audit C3 convention; `order_id` is IN the DEDUP key, so the
/// constant sentinel gives a stable dedup identity).
pub const ORDER_UPDATE_EVENTS_SYMBOL_NA: &str = "n/a";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// IST nanoseconds per day — for deriving `trading_date_ist` from the
/// record's OWN `ts` at persist time (never a cached boot date).
const NANOS_PER_DAY: i64 = 86_400 * 1_000_000_000;

/// IST midnight (nanos) of the IST instant `ts_ist_nanos`.
#[must_use]
fn ist_midnight_nanos(ts_ist_nanos: i64) -> i64 {
    ts_ist_nanos - ts_ist_nanos.rem_euclid(NANOS_PER_DAY)
}

/// The idempotent `CREATE TABLE` DDL for `order_update_events`. Pure.
// FINANCIAL-TEST-EXEMPT: pure DDL string builder — no financial arithmetic ("order" is the table name)
#[must_use]
pub fn order_update_events_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {ORDER_UPDATE_EVENTS_TABLE} (\
            ts                  TIMESTAMP, \
            trading_date_ist    TIMESTAMP, \
            feed                SYMBOL, \
            event_seq           LONG, \
            order_id            SYMBOL, \
            exch_order_id       SYMBOL, \
            correlation_id      SYMBOL, \
            status              SYMBOL, \
            raw_status          SYMBOL, \
            leg_no              INT, \
            product             SYMBOL, \
            order_type          SYMBOL, \
            transaction_type    SYMBOL, \
            validity            SYMBOL, \
            exchange            SYMBOL, \
            exchange_segment    SYMBOL, \
            security_id         LONG, \
            symbol              SYMBOL, \
            quantity            LONG, \
            disclosed_qty       LONG, \
            remaining_qty       LONG, \
            traded_qty          LONG, \
            price               DOUBLE, \
            trigger_price       DOUBLE, \
            avg_traded_price    DOUBLE, \
            last_traded_price   DOUBLE, \
            reject_reason       STRING, \
            remarks             STRING, \
            source              SYMBOL, \
            off_mkt_flag        SYMBOL, \
            opt_type            SYMBOL, \
            algo_ord_no         SYMBOL, \
            algo_id             SYMBOL, \
            mkt_type            SYMBOL, \
            series              SYMBOL, \
            good_till_days_date SYMBOL, \
            ref_ltp             DOUBLE, \
            tick_size           DOUBLE, \
            multiplier          INT, \
            instrument          SYMBOL, \
            broker_create_time  STRING, \
            broker_update_time  STRING, \
            exchange_time       STRING, \
            exchange_ts_ms      LONG, \
            contract_id         SYMBOL, \
            gui_order_id        SYMBOL, \
            stage_trail         STRING, \
            detail_raw          STRING\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_ORDER_UPDATE_EVENTS});"
    )
}

/// Every non-designated column `(name, type)` — shared by the CREATE DDL
/// self-heal loop so the two can never drift.
const ORDER_UPDATE_EVENTS_COLUMNS: &[(&str, &str)] = &[
    ("trading_date_ist", "TIMESTAMP"),
    ("feed", "SYMBOL"),
    ("event_seq", "LONG"),
    ("order_id", "SYMBOL"),
    ("exch_order_id", "SYMBOL"),
    ("correlation_id", "SYMBOL"),
    ("status", "SYMBOL"),
    ("raw_status", "SYMBOL"),
    ("leg_no", "INT"),
    ("product", "SYMBOL"),
    ("order_type", "SYMBOL"),
    ("transaction_type", "SYMBOL"),
    ("validity", "SYMBOL"),
    ("exchange", "SYMBOL"),
    ("exchange_segment", "SYMBOL"),
    ("security_id", "LONG"),
    ("symbol", "SYMBOL"),
    ("quantity", "LONG"),
    ("disclosed_qty", "LONG"),
    ("remaining_qty", "LONG"),
    ("traded_qty", "LONG"),
    ("price", "DOUBLE"),
    ("trigger_price", "DOUBLE"),
    ("avg_traded_price", "DOUBLE"),
    ("last_traded_price", "DOUBLE"),
    ("reject_reason", "STRING"),
    ("remarks", "STRING"),
    ("source", "SYMBOL"),
    ("off_mkt_flag", "SYMBOL"),
    ("opt_type", "SYMBOL"),
    ("algo_ord_no", "SYMBOL"),
    ("algo_id", "SYMBOL"),
    ("mkt_type", "SYMBOL"),
    ("series", "SYMBOL"),
    ("good_till_days_date", "SYMBOL"),
    ("ref_ltp", "DOUBLE"),
    ("tick_size", "DOUBLE"),
    ("multiplier", "INT"),
    ("instrument", "SYMBOL"),
    ("broker_create_time", "STRING"),
    ("broker_update_time", "STRING"),
    ("exchange_time", "STRING"),
    ("exchange_ts_ms", "LONG"),
    ("contract_id", "SYMBOL"),
    ("gui_order_id", "SYMBOL"),
    ("stage_trail", "STRING"),
    ("detail_raw", "STRING"),
];

/// Create the `order_update_events` table if absent (idempotent self-heal
/// order: CREATE → per-column `ALTER ADD COLUMN IF NOT EXISTS` → DEDUP
/// ENABLE — the house template). Failures log at `error!` (code
/// ORDER-EVT-01, stage `ensure_*`) but never block boot and never panic;
/// NOTE the HTTP-CLIENT-01-class consequence: a failed ensure leaves the
/// table to be auto-created by the first ILP write WITHOUT dedup (a
/// duplicate-row window until a later ensure succeeds).
// TEST-EXEMPT: live-QuestDB DDL runner (DDL string unit-tested; the 200/500/unreachable arms exercised via the mock-HTTP tokio tests below)
// FINANCIAL-TEST-EXEMPT: idempotent DDL runner — no financial arithmetic ("order" is the table name)
pub async fn ensure_order_update_events_table(questdb_config: &QuestDbConfig) {
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
                "ORDER-EVT-01: HTTP client build failed — order_update_events \
                 table not ensured (first ILP write may auto-create it WITHOUT \
                 dedup — duplicate-row window until the next successful boot)"
            );
            return;
        }
    };
    let mut statements = vec![order_update_events_create_ddl()];
    // Per-column self-heal for tables created by earlier builds
    // (observability-architecture.md schema-self-heal pattern).
    for (col, ty) in ORDER_UPDATE_EVENTS_COLUMNS {
        statements.push(format!(
            "ALTER TABLE {ORDER_UPDATE_EVENTS_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty};"
        ));
    }
    statements.push(format!(
        "ALTER TABLE {ORDER_UPDATE_EVENTS_TABLE} DEDUP ENABLE UPSERT \
         KEYS({DEDUP_KEY_ORDER_UPDATE_EVENTS});"
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
                // Bounded DDL prefix (our own static statement — the prefix
                // identifies it; the full CREATE would exceed the ≤300-char
                // error-payload house bound).
                error!(code = ErrorCode::OrderEvt01PersistFailed.code_str(),
                    stage = "ensure_ddl",
                    %status, ddl = %ddl.chars().take(80).collect::<String>(),
                    body = %body.chars().take(200).collect::<String>(),
                    "ORDER-EVT-01: order_update_events DDL returned non-2xx \
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
                    "ORDER-EVT-01: order_update_events DDL request failed"
                );
            }
        }
    }
}

/// ILP-over-HTTP conf — per-flush server ACK (the 2026-07-05
/// fire-and-forget lesson) with the shadow-candle-writer knobs.
fn order_update_events_ilp_http_conf(config: &QuestDbConfig) -> String {
    format!(
        "http::addr={}:{};protocol_version=1;retry_timeout=0;request_timeout=5000;",
        config.host, config.http_port
    )
}

/// Lazy ILP-over-HTTP writer for `order_update_events`. Unreachable
/// QuestDB at construction still builds (rows buffer locally); a failed
/// flush DISCARDS the pending buffer (poisoned-buffer defense) — the rows
/// are best-effort forensics; the discard is counted + coded, never silent.
pub struct OrderUpdateEventsWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl OrderUpdateEventsWriter {
    /// Production constructor — ILP-over-HTTP sender, lazy on failure.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (lazy-build contract exercised via test_order_update_events_writer_new_is_lazy...); append/flush paths covered via for_test()
    pub fn new(config: &QuestDbConfig) -> Self {
        let conf = order_update_events_ilp_http_conf(config);
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
                    "order_update_events writer: QuestDB unreachable — buffering locally"
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

    /// Appends one full-fidelity order push-event row (cold path). Every
    /// non-finite DOUBLE is clamped to 0.0 + counted
    /// (`tv_order_update_events_nonfinite_clamped_total`); every string is
    /// sanitized through the house choke point; `reject_reason` is bounded
    /// at [`ORDER_UPDATE_EVENTS_REJECT_REASON_MAX_CHARS`] and
    /// `stage_trail`/`detail_raw` at
    /// [`ORDER_UPDATE_EVENTS_DETAIL_MAX_CHARS`]. `trading_date_ist` is
    /// derived from the record's OWN `ts` (never a cached boot date).
    ///
    /// # Errors
    /// Propagates ILP buffer errors (table/column append failure).
    pub fn append_order_update_event(&mut self, r: &OrderUpdateEventRecord) -> Result<()> {
        let clamp = |value: f64, field: &'static str| -> f64 {
            if value.is_finite() {
                value
            } else {
                metrics::counter!("tv_order_update_events_nonfinite_clamped_total").increment(1);
                warn!(
                    order_id = %r.order_id,
                    field,
                    raw = value,
                    "order_update_events: non-finite value clamped to 0.0 to avoid QuestDB reject"
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
                ORDER_UPDATE_EVENTS_SYMBOL_NA.to_string()
            } else {
                sanitized.into_owned()
            }
        };
        // STRING (field) values go through the ILP STRING sanitiser —
        // preserves `,`/`=`/`'`/`;`/`"` verbatim (questdb-rs escapes the
        // genuinely ILP-special chars itself), strips control/BiDi, and
        // caps internally at MAX_AUDIT_STR_LEN.
        let bounded = |value: &str, cap: usize| -> String {
            sanitize_ilp_string(value).chars().take(cap).collect()
        };
        let reject_reason = bounded(
            &r.reject_reason,
            ORDER_UPDATE_EVENTS_REJECT_REASON_MAX_CHARS,
        );
        let remarks = bounded(&r.remarks, ORDER_UPDATE_EVENTS_REMARKS_MAX_CHARS);
        let stage_trail = bounded(&r.stage_trail, ORDER_UPDATE_EVENTS_DETAIL_MAX_CHARS);
        let detail_raw = bounded(&r.detail_raw, ORDER_UPDATE_EVENTS_DETAIL_MAX_CHARS);
        self.buffer
            .table(ORDER_UPDATE_EVENTS_TABLE)
            .context("table")?
            // Symbols BEFORE columns (ILP tags-before-fields rule).
            .symbol("feed", r.feed.as_str())
            .context("feed")?
            .symbol("order_id", symbol_or_na(&r.order_id))
            .context("order_id")?
            .symbol("exch_order_id", symbol_or_na(&r.exch_order_id))
            .context("exch_order_id")?
            .symbol("correlation_id", symbol_or_na(&r.correlation_id))
            .context("correlation_id")?
            .symbol("status", symbol_or_na(&r.status))
            .context("status")?
            .symbol("raw_status", symbol_or_na(&r.raw_status))
            .context("raw_status")?
            .symbol("product", symbol_or_na(&r.product))
            .context("product")?
            .symbol("order_type", symbol_or_na(&r.order_type))
            .context("order_type")?
            .symbol("transaction_type", symbol_or_na(&r.transaction_type))
            .context("transaction_type")?
            .symbol("validity", symbol_or_na(&r.validity))
            .context("validity")?
            .symbol("exchange", symbol_or_na(&r.exchange))
            .context("exchange")?
            .symbol("exchange_segment", symbol_or_na(&r.exchange_segment))
            .context("exchange_segment")?
            .symbol("symbol", symbol_or_na(&r.symbol))
            .context("symbol")?
            .symbol("source", symbol_or_na(&r.source))
            .context("source")?
            .symbol("off_mkt_flag", symbol_or_na(&r.off_mkt_flag))
            .context("off_mkt_flag")?
            .symbol("opt_type", symbol_or_na(&r.opt_type))
            .context("opt_type")?
            .symbol("algo_ord_no", symbol_or_na(&r.algo_ord_no))
            .context("algo_ord_no")?
            .symbol("algo_id", symbol_or_na(&r.algo_id))
            .context("algo_id")?
            .symbol("mkt_type", symbol_or_na(&r.mkt_type))
            .context("mkt_type")?
            .symbol("series", symbol_or_na(&r.series))
            .context("series")?
            .symbol("good_till_days_date", symbol_or_na(&r.good_till_days_date))
            .context("good_till_days_date")?
            .symbol("instrument", symbol_or_na(&r.instrument))
            .context("instrument")?
            .symbol("contract_id", symbol_or_na(&r.contract_id))
            .context("contract_id")?
            .symbol("gui_order_id", symbol_or_na(&r.gui_order_id))
            .context("gui_order_id")?
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(ist_midnight_nanos(r.ts_ist_nanos)),
            )
            .context("trading_date_ist")?
            .column_i64("event_seq", r.event_seq)
            .context("event_seq")?
            .column_i64("leg_no", i64::from(r.leg_no))
            .context("leg_no")?
            .column_i64("security_id", r.security_id)
            .context("security_id")?
            .column_i64("quantity", r.quantity)
            .context("quantity")?
            .column_i64("disclosed_qty", r.disclosed_qty)
            .context("disclosed_qty")?
            .column_i64("remaining_qty", r.remaining_qty)
            .context("remaining_qty")?
            .column_i64("traded_qty", r.traded_qty)
            .context("traded_qty")?
            .column_f64("price", clamp(r.price, "price"))
            .context("price")?
            .column_f64("trigger_price", clamp(r.trigger_price, "trigger_price"))
            .context("trigger_price")?
            .column_f64(
                "avg_traded_price",
                clamp(r.avg_traded_price, "avg_traded_price"),
            )
            .context("avg_traded_price")?
            .column_f64(
                "last_traded_price",
                clamp(r.last_traded_price, "last_traded_price"),
            )
            .context("last_traded_price")?
            .column_str("reject_reason", reject_reason.as_str())
            .context("reject_reason")?
            .column_str("remarks", remarks.as_str())
            .context("remarks")?
            .column_f64("ref_ltp", clamp(r.ref_ltp, "ref_ltp"))
            .context("ref_ltp")?
            .column_f64("tick_size", clamp(r.tick_size, "tick_size"))
            .context("tick_size")?
            .column_i64("multiplier", i64::from(r.multiplier))
            .context("multiplier")?
            .column_str("broker_create_time", bounded(&r.broker_create_time, 64))
            .context("broker_create_time")?
            .column_str("broker_update_time", bounded(&r.broker_update_time, 64))
            .context("broker_update_time")?
            .column_str("exchange_time", bounded(&r.exchange_time, 64))
            .context("exchange_time")?
            .column_i64("exchange_ts_ms", r.exchange_ts_ms)
            .context("exchange_ts_ms")?
            .column_str("stage_trail", stage_trail.as_str())
            .context("stage_trail")?
            .column_str("detail_raw", detail_raw.as_str())
            .context("detail_raw")?
            .at(TimestampNanos::new(r.ts_ist_nanos))
            .context("designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        metrics::counter!("tv_order_update_events_rows_total", "feed" => r.feed.as_str())
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
                "order_update_events: no ILP sender (QuestDB unreachable) — \
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
                    "order_update_events ILP flush failed — {dropped} pending \
                     capture row(s) discarded (poisoned-buffer defense)"
                )))
            }
            // Unreachable (checked above) — treated as the no-sender arm.
            None => {
                let dropped = self.discard_pending();
                anyhow::bail!(
                    "order_update_events: ILP sender vanished — {dropped} row(s) discarded"
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

    fn sample_record() -> OrderUpdateEventRecord {
        OrderUpdateEventRecord {
            ts_ist_nanos: 1_770_000_900_000_000_000,
            feed: Feed::Dhan,
            event_seq: 42,
            order_id: "5225022912912".to_string(),
            exch_order_id: "1300000012345".to_string(),
            correlation_id: "TV-abc123".to_string(),
            status: "FILLED".to_string(),
            raw_status: "TRADED".to_string(),
            leg_no: 1,
            product: "INTRADAY".to_string(),
            order_type: "LMT".to_string(),
            transaction_type: "BUY".to_string(),
            validity: "DAY".to_string(),
            exchange: "NSE".to_string(),
            exchange_segment: "NSE_FNO".to_string(),
            security_id: 49_081,
            symbol: "NIFTY-CE".to_string(),
            quantity: 75,
            disclosed_qty: 0,
            remaining_qty: 0,
            traded_qty: 75,
            price: 122.50,
            trigger_price: 0.0,
            avg_traded_price: 122.45,
            last_traded_price: 122.45,
            reject_reason: String::new(),
            remarks: String::new(),
            source: "P".to_string(),
            off_mkt_flag: "0".to_string(),
            opt_type: "CE".to_string(),
            algo_ord_no: String::new(),
            algo_id: String::new(),
            mkt_type: "NL".to_string(),
            series: "EQ".to_string(),
            good_till_days_date: String::new(),
            ref_ltp: 122.40,
            tick_size: 0.05,
            multiplier: 1,
            instrument: "OPTIDX".to_string(),
            broker_create_time: "2026-07-18 09:20:01".to_string(),
            broker_update_time: "2026-07-18 09:20:02".to_string(),
            exchange_time: "2026-07-18 09:20:02".to_string(),
            exchange_ts_ms: -1,
            contract_id: String::new(),
            gui_order_id: String::new(),
            stage_trail: String::new(),
            detail_raw: "product_code=V lot_size=75".to_string(),
        }
    }

    #[test]
    fn test_order_update_events_ddl_contains_expected_columns() {
        let ddl = order_update_events_create_ddl();
        // Every self-heal column must appear in the CREATE DDL (the two
        // lists can never drift).
        for (col, _) in ORDER_UPDATE_EVENTS_COLUMNS {
            assert!(ddl.contains(col), "DDL missing column {col:?}: {ddl}");
        }
        assert!(ddl.contains("CREATE TABLE IF NOT EXISTS"));
        assert!(ddl.contains("PARTITION BY DAY"));
        assert!(ddl.contains(&format!(
            "DEDUP UPSERT KEYS({DEDUP_KEY_ORDER_UPDATE_EVENTS})"
        )));
    }

    /// DEDUP-key discipline: designated `ts` FIRST (2026-04-28 regression
    /// rule); `trading_date_ist` in-key (phase-0 composite-DEDUP rule 1);
    /// `feed` in-key (2026-06-28 override); `event_seq` in-key (burst
    /// safety) — whole-token matches + EXACT arity 5.
    #[test]
    fn test_dedup_key_order_update_events_exact() {
        assert_eq!(
            DEDUP_KEY_ORDER_UPDATE_EVENTS,
            "ts, trading_date_ist, feed, order_id, event_seq"
        );
        assert!(
            DEDUP_KEY_ORDER_UPDATE_EVENTS
                .trim_start()
                .starts_with("ts,")
        );
        // Per-order identity — deliberately NO security_id in-key (the
        // order_audit precedent; the I-P1-11 rule therefore does not bind).
        assert!(!DEDUP_KEY_ORDER_UPDATE_EVENTS.contains("security_id"));
    }

    #[test]
    fn test_ist_midnight_nanos_floors_to_day() {
        // 1 day + 1 hour of nanos floors to exactly 1 day.
        let one_day = NANOS_PER_DAY;
        let one_hour = 3_600 * 1_000_000_000_i64;
        assert_eq!(ist_midnight_nanos(one_day + one_hour), one_day);
        assert_eq!(ist_midnight_nanos(one_day), one_day);
        assert_eq!(ist_midnight_nanos(0), 0);
    }

    #[test]
    fn test_append_order_update_event_writes_symbols_and_columns() {
        let mut w = OrderUpdateEventsWriter::for_test();
        w.append_order_update_event(&sample_record())
            .expect("append must succeed");
        assert_eq!(w.pending(), 1);
        let line = w.buffer_utf8();
        assert!(line.starts_with(ORDER_UPDATE_EVENTS_TABLE));
        assert!(line.contains(",feed=dhan"), "feed tag: {line}");
        assert!(
            line.contains(",order_id=5225022912912"),
            "order_id tag: {line}"
        );
        assert!(line.contains(",status=FILLED"), "status tag: {line}");
        assert!(
            line.contains(",raw_status=TRADED"),
            "raw_status tag: {line}"
        );
        assert!(line.contains("event_seq=42i"), "event_seq: {line}");
        assert!(line.contains("quantity=75i"), "quantity: {line}");
        assert!(line.contains("traded_qty=75i"), "traded_qty: {line}");
        assert!(line.contains("security_id=49081i"), "security_id: {line}");
        assert!(line.contains("detail_raw="), "detail_raw column: {line}");
        // Empty Groww-only symbols degrade to the n/a sentinel, never an
        // empty ILP tag.
        assert!(
            line.contains(",contract_id=n/a") && line.contains(",gui_order_id=n/a"),
            "empty symbols must map to n/a: {line}"
        );
        assert!(
            !line.contains("=,") && !line.contains("= "),
            "no bare-equals empty tag value: {line}"
        );
    }

    /// Non-finite DOUBLEs (any of the 6 price-class fields) clamp to 0.0
    /// instead of poisoning the ILP buffer.
    #[test]
    fn test_append_order_update_event_nonfinite_clamped() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut w = OrderUpdateEventsWriter::for_test();
            let rec = OrderUpdateEventRecord {
                price: bad,
                ref_ltp: bad,
                ..sample_record()
            };
            w.append_order_update_event(&rec)
                .expect("append must succeed");
            let line = w.buffer_utf8();
            assert!(!line.contains("NaN"), "NaN leaked into ILP: {line}");
            assert!(
                line.contains("price=0") && line.contains("ref_ltp=0"),
                "non-finite must clamp to 0.0: {line}"
            );
        }
    }

    /// `reject_reason` bounded ≤300; `detail_raw`/`stage_trail` bounded
    /// ≤`MAX_AUDIT_STR_LEN` (1024) — a hostile broker string can never
    /// bloat a row.
    #[test]
    fn test_append_order_update_event_string_caps() {
        let mut w = OrderUpdateEventsWriter::for_test();
        let rec = OrderUpdateEventRecord {
            reject_reason: "r".repeat(1_000),
            detail_raw: "d".repeat(5_000),
            stage_trail: "s".repeat(5_000),
            ..sample_record()
        };
        w.append_order_update_event(&rec)
            .expect("append must succeed");
        let line = w.buffer_utf8();
        // ILP STRING column values are double-quoted — extract ONLY the
        // quoted value (counting the raw remainder would also count the
        // letters of later field NAMES).
        let field = |name: &str| -> String {
            line.split(&format!("{name}="))
                .nth(1)
                .expect("field present")
                .split('"')
                .nth(1)
                .unwrap_or_default()
                .to_string()
        };
        assert!(
            field("reject_reason").matches('r').count()
                <= ORDER_UPDATE_EVENTS_REJECT_REASON_MAX_CHARS,
            "reject_reason must be capped at 300"
        );
        assert!(
            field("detail_raw").matches('d').count() <= ORDER_UPDATE_EVENTS_DETAIL_MAX_CHARS,
            "detail_raw must be capped at MAX_AUDIT_STR_LEN"
        );
        assert!(
            field("stage_trail").matches('s').count() <= ORDER_UPDATE_EVENTS_DETAIL_MAX_CHARS,
            "stage_trail must be capped at MAX_AUDIT_STR_LEN"
        );
    }

    #[test]
    fn test_order_update_events_flush_failure_discards_pending() {
        let mut w = OrderUpdateEventsWriter::for_test();
        w.append_order_update_event(&sample_record())
            .expect("append must succeed");
        let err = w.flush().expect_err("disconnected flush must error");
        assert!(err.to_string().contains("no ILP sender"));
        assert!(err.to_string().contains("discarded"));
        assert_eq!(w.pending(), 0, "failed flush discards pending");
        assert!(w.buffer_utf8().is_empty(), "ILP buffer cleared on discard");
        // Empty flush is a no-op Ok; second discard drops nothing.
        let mut empty = OrderUpdateEventsWriter::for_test();
        assert!(empty.flush().is_ok());
        assert_eq!(w.discard_pending(), 0);
    }

    /// Transport ratchet (2026-07-05 fire-and-forget lesson): ILP-over-HTTP
    /// with the shadow-candle-writer knobs — never ILP TCP 9009.
    #[test]
    fn test_order_update_events_ilp_conf_targets_http_port() {
        let cfg = QuestDbConfig {
            host: "tv-questdb".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let conf = order_update_events_ilp_http_conf(&cfg);
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
    async fn test_ensure_order_update_events_table_mock_200_completes() {
        let port = spawn_mock_http(MOCK_HTTP_200).await;
        ensure_order_update_events_table(&mock_cfg(port)).await;
    }

    #[tokio::test]
    async fn test_ensure_order_update_events_table_mock_500_degrades_without_panic() {
        let port = spawn_mock_http(MOCK_HTTP_500).await;
        ensure_order_update_events_table(&mock_cfg(port)).await;
    }

    #[tokio::test]
    async fn test_ensure_order_update_events_table_unreachable_degrades_without_panic() {
        ensure_order_update_events_table(&unreachable_cfg()).await;
    }

    #[tokio::test]
    async fn test_order_update_events_writer_new_is_lazy_and_buffers_without_network() {
        let mut w = OrderUpdateEventsWriter::new(&unreachable_cfg());
        assert_eq!(w.pending(), 0, "fresh writer has nothing pending");
        w.append_order_update_event(&sample_record())
            .expect("append must succeed without network");
        assert_eq!(w.pending(), 1, "row buffered locally");
    }

    /// The `Some(Err(..))` flush arm — a REAL server reject through a live
    /// (lazily-built) HTTP sender — exercises the poisoned-buffer discard,
    /// not just the no-sender bail.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_order_update_events_flush_server_reject_discards() {
        let port = spawn_mock_http(MOCK_HTTP_500).await;
        let mut w = OrderUpdateEventsWriter::new(&mock_cfg(port));
        w.append_order_update_event(&sample_record())
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

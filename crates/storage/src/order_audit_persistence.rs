//! `order_audit` table — SEBI 5-year order-lifecycle forensics (cluster-C
//! rebuild 2026-07-14 of the table deleted in #T4 2026-05-20; runbook
//! `.claude/rules/project/wave-2-error-codes.md` AUDIT-06).
//!
//! ONE row PER order-lifecycle EVENT — placement (paper AND live), placement
//! failure, cancel, cancel failure, broker rejection, circuit-breaker
//! open/close, rate-limit exhaustion — so every "what happened to that
//! order?" question is answerable from QuestDB five years later. A RiskHalt
//! does NOT write an `order_audit` row (it is not an order lifecycle event —
//! the coded `error!` + the Critical Telegram are its record).
//!
//! BEST-EFFORT ONLY: a forensics write failure must NEVER affect the order
//! path — the app-crate consumer logs (coded AUDIT-06, staged) + counts and
//! continues; orders (paper today) are never blocked by this table.
//!
//! **SEBI 5-year retention applies — rows are NEVER deleted.**
//!
//! ## Schema
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS order_audit (
//!     ts TIMESTAMP, trading_date_ist TIMESTAMP,
//!     order_id SYMBOL, correlation_id SYMBOL, leg SYMBOL,
//!     event SYMBOL, feed SYMBOL, mode SYMBOL,
//!     security_id LONG, exchange_segment SYMBOL,
//!     transaction_type SYMBOL, quantity LONG, price DOUBLE,
//!     order_status SYMBOL, outcome SYMBOL, detail STRING
//! ) timestamp(ts) PARTITION BY DAY
//! DEDUP UPSERT KEYS(ts, trading_date_ist, order_id, leg, event, feed);
//! ```
//!
//! DEDUP discipline: designated `ts` FIRST (2026-04-28 rule);
//! `trading_date_ist` in-key (phase-0-architecture composite-DEDUP rule 1);
//! per-ORDER identity `order_id, leg` (NOT `security_id` — the I-P1-11
//! segment rule is therefore out of scope, kept as an explanatory test);
//! `event` in-key (phase-0 DEDUP rule 3 — transition rows BOTH survive: a
//! same-nanosecond `placed` + `cancelled` pair for one order never
//! collapses); `feed` in-key (feed-in-key EVERYWHERE, 2026-06-28 — every
//! writer stamps `'dhan'` today).

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::sanitize::sanitize_audit_string;

/// QuestDB table name — one row per order-lifecycle event (SEBI 5y).
pub const ORDER_AUDIT_TABLE: &str = "order_audit";

/// DEDUP key. Designated `ts` FIRST (2026-04-28 regression rule);
/// `trading_date_ist` in-key (phase-0 composite-DEDUP rule 1); per-order
/// identity `order_id, leg`; `event` in-key (phase-0 rule 3 — transition
/// rows BOTH survive); `feed` in-key (2026-06-28 override).
pub const DEDUP_KEY_ORDER_AUDIT: &str = "ts, trading_date_ist, order_id, leg, event, feed";

/// `detail` column hard cap — bounded forensic text, never raw bodies.
pub const ORDER_AUDIT_DETAIL_MAX_CHARS: usize = 200;

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Typed order-lifecycle event — the SYMBOL wire strings are stable
/// (forensic queries group on them) and part of the DEDUP key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderAuditEvent {
    /// An order was accepted by the OMS (paper `PAPER-*` id or live id).
    Placed,
    /// `place_order` returned Err at the call site.
    PlaceFailed,
    /// An order was cancelled successfully.
    Cancelled,
    /// `cancel_order` returned Err at the call site.
    CancelFailed,
    /// The broker/validation rejected an order (`OmsAlert::OrderRejected`).
    Rejected,
    /// The OMS circuit breaker OPENED (orders blocked).
    CircuitOpen,
    /// The OMS circuit breaker CLOSED (orders allowed again).
    CircuitClosed,
    /// The SEBI rate limiter denied an order.
    RateLimited,
}

impl OrderAuditEvent {
    /// Stable SYMBOL wire string (in the DEDUP key — never reworded).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Placed => "placed",
            Self::PlaceFailed => "place_failed",
            Self::Cancelled => "cancelled",
            Self::CancelFailed => "cancel_failed",
            Self::Rejected => "rejected",
            Self::CircuitOpen => "circuit_open",
            Self::CircuitClosed => "circuit_closed",
            Self::RateLimited => "rate_limited",
        }
    }
}

/// One order-lifecycle forensics row, ready for ILP write. `detail` is
/// sanitized (`sanitize_audit_string`) + truncated to
/// [`ORDER_AUDIT_DETAIL_MAX_CHARS`] at append time — never raw broker text.
#[derive(Clone, Debug, PartialEq)]
pub struct OrderAuditRow {
    /// Designated timestamp — event instant, IST wall-clock nanoseconds.
    pub ts_ist_nanos: i64,
    /// The trading day — IST midnight nanoseconds.
    pub trading_date_ist_nanos: i64,
    /// Broker/paper order id (`PAPER-1` today; empty for pre-id failures).
    pub order_id: String,
    /// Caller correlation id (may be empty when unknown at the call site).
    pub correlation_id: String,
    /// Order leg slug (`single` for plain intraday market orders).
    pub leg: &'static str,
    /// Typed lifecycle event.
    pub event: OrderAuditEvent,
    /// Source broker (`dhan` always today).
    pub feed: &'static str,
    /// `paper` (dry-run) or `live`.
    pub mode: &'static str,
    /// Instrument id (-1 sentinel when not applicable to the event).
    pub security_id: i64,
    /// Segment classification (`IDX_I` / `NSE_FNO` / … / `n/a`).
    pub exchange_segment: &'static str,
    /// `BUY` / `SELL` / `n/a`.
    pub transaction_type: &'static str,
    /// Order quantity (0 when not applicable).
    pub quantity: i64,
    /// Order price (0.0 for market orders; non-finite clamped at append).
    pub price: f64,
    /// Order status slug at event time (`pending` / `n/a`).
    pub order_status: &'static str,
    /// `ok` / `failed` — the event's own outcome.
    pub outcome: &'static str,
    /// Bounded free-text detail (sanitized + truncated at append time).
    pub detail: String,
}

/// The idempotent `CREATE TABLE` DDL for `order_audit`. Pure.
// FINANCIAL-TEST-EXEMPT: pure DDL string builder — no financial arithmetic ("order" is the table name)
#[must_use]
pub fn order_audit_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {ORDER_AUDIT_TABLE} (\
            ts               TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            order_id         SYMBOL, \
            correlation_id   SYMBOL, \
            leg              SYMBOL, \
            event            SYMBOL, \
            feed             SYMBOL, \
            mode             SYMBOL, \
            security_id      LONG, \
            exchange_segment SYMBOL, \
            transaction_type SYMBOL, \
            quantity         LONG, \
            price            DOUBLE, \
            order_status     SYMBOL, \
            outcome          SYMBOL, \
            detail           STRING\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_ORDER_AUDIT});"
    )
}

/// Create the `order_audit` table if absent (idempotent self-heal order:
/// CREATE → per-column `ALTER ADD COLUMN IF NOT EXISTS` → DEDUP ENABLE —
/// the house template). Failures log at `error!` (code AUDIT-06, stage
/// `ensure_*`) but never block boot and never panic; NOTE the
/// HTTP-CLIENT-01-class consequence: a failed ensure leaves the table to be
/// auto-created by the first ILP write WITHOUT dedup (a duplicate-row
/// window until a later ensure succeeds).
// TEST-EXEMPT: live-QuestDB DDL runner (DDL string unit-tested; the 200/500/unreachable arms exercised via the mock-HTTP tokio tests below)
// FINANCIAL-TEST-EXEMPT: idempotent DDL runner — no financial arithmetic ("order" is the table name)
pub async fn ensure_order_audit_table(questdb_config: &QuestDbConfig) {
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
            metrics::counter!("tv_order_audit_persist_errors_total", "stage" => "ensure_client_build")
                .increment(1);
            error!(
                code = ErrorCode::Audit06OrderWriteFailed.code_str(),
                stage = "ensure_client_build",
                ?err,
                "AUDIT-06: HTTP client build failed — order_audit table not \
                 ensured (first ILP write may auto-create it WITHOUT dedup — \
                 duplicate-row window until the next successful boot)"
            );
            return;
        }
    };
    let mut statements = vec![order_audit_create_ddl()];
    // Per-column self-heal for tables created by earlier builds
    // (observability-architecture.md schema-self-heal pattern).
    for (col, ty) in [
        ("trading_date_ist", "TIMESTAMP"),
        ("order_id", "SYMBOL"),
        ("correlation_id", "SYMBOL"),
        ("leg", "SYMBOL"),
        ("event", "SYMBOL"),
        ("feed", "SYMBOL"),
        ("mode", "SYMBOL"),
        ("security_id", "LONG"),
        ("exchange_segment", "SYMBOL"),
        ("transaction_type", "SYMBOL"),
        ("quantity", "LONG"),
        ("price", "DOUBLE"),
        ("order_status", "SYMBOL"),
        ("outcome", "SYMBOL"),
        ("detail", "STRING"),
    ] {
        statements.push(format!(
            "ALTER TABLE {ORDER_AUDIT_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty};"
        ));
    }
    statements.push(format!(
        "ALTER TABLE {ORDER_AUDIT_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_ORDER_AUDIT});"
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
                metrics::counter!("tv_order_audit_persist_errors_total", "stage" => "ensure_ddl")
                    .increment(1);
                error!(code = ErrorCode::Audit06OrderWriteFailed.code_str(),
                    stage = "ensure_ddl",
                    %status, ddl = ddl.as_str(),
                    body = %body.chars().take(200).collect::<String>(),
                    "AUDIT-06: order_audit DDL returned non-2xx (dedup may be \
                     missing — duplicate-row window until a later ensure succeeds)");
            }
            Err(err) => {
                metrics::counter!("tv_order_audit_persist_errors_total", "stage" => "ensure_ddl")
                    .increment(1);
                error!(
                    code = ErrorCode::Audit06OrderWriteFailed.code_str(),
                    stage = "ensure_ddl",
                    ?err,
                    ddl = ddl.as_str(),
                    "AUDIT-06: order_audit DDL request failed"
                );
            }
        }
    }
}

/// ILP-over-HTTP conf — per-flush server ACK (the 2026-07-05
/// fire-and-forget lesson) with the shadow-candle-writer knobs.
fn order_audit_ilp_http_conf(config: &QuestDbConfig) -> String {
    format!(
        "http::addr={}:{};protocol_version=1;retry_timeout=0;request_timeout=5000;",
        config.host, config.http_port
    )
}

/// Lazy ILP-over-HTTP writer for `order_audit`. Unreachable QuestDB at
/// construction still builds (rows buffer locally); a failed flush
/// DISCARDS the pending buffer (poisoned-buffer defense) — the rows are
/// best-effort forensics; the discard is counted + coded, never silent.
pub struct OrderAuditWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl OrderAuditWriter {
    /// Production constructor — ILP-over-HTTP sender, lazy on failure.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (lazy-build contract exercised via test_order_audit_writer_new_is_lazy...); append/flush paths covered via for_test()
    pub fn new(config: &QuestDbConfig) -> Self {
        let conf = order_audit_ilp_http_conf(config);
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
                    "order_audit writer: QuestDB unreachable — buffering locally"
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

    /// Appends one order-lifecycle forensics row (cold path — orders are
    /// bounded by the 10/sec SEBI cap). Non-finite `price` is clamped to
    /// 0.0 + counted (Wave-2-D HIGH finding preserved: a NaN/Infinity from
    /// a malformed response must never poison the ILP buffer); `detail` is
    /// sanitized + truncated to [`ORDER_AUDIT_DETAIL_MAX_CHARS`].
    ///
    /// # Errors
    /// Propagates ILP buffer errors (table/column append failure).
    pub fn append_order_audit_row(&mut self, r: &OrderAuditRow) -> Result<()> {
        // Wave-2-D adversarial review (HIGH), preserved from the recovered
        // module: QuestDB rejects NaN/Infinity DOUBLE values — clamp + count.
        let safe_price: f64 = if r.price.is_finite() {
            r.price
        } else {
            metrics::counter!("tv_order_audit_nonfinite_clamped_total").increment(1);
            warn!(
                order_id = %r.order_id,
                raw_price = r.price,
                "order_audit: non-finite price clamped to 0.0 to avoid QuestDB reject"
            );
            0.0
        };
        let detail: String = sanitize_audit_string(&r.detail)
            .chars()
            .take(ORDER_AUDIT_DETAIL_MAX_CHARS)
            .collect();
        self.buffer
            .table(ORDER_AUDIT_TABLE)
            .context("table")?
            // Symbols BEFORE columns (ILP tags-before-fields rule).
            .symbol("order_id", sanitize_audit_string(&r.order_id))
            .context("order_id")?
            .symbol("correlation_id", sanitize_audit_string(&r.correlation_id))
            .context("correlation_id")?
            .symbol("leg", r.leg)
            .context("leg")?
            .symbol("event", r.event.as_str())
            .context("event")?
            .symbol("feed", r.feed)
            .context("feed")?
            .symbol("mode", r.mode)
            .context("mode")?
            .symbol("exchange_segment", r.exchange_segment)
            .context("exchange_segment")?
            .symbol("transaction_type", r.transaction_type)
            .context("transaction_type")?
            .symbol("order_status", r.order_status)
            .context("order_status")?
            .symbol("outcome", r.outcome)
            .context("outcome")?
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(r.trading_date_ist_nanos),
            )
            .context("trading_date_ist")?
            .column_i64("security_id", r.security_id)
            .context("security_id")?
            .column_i64("quantity", r.quantity)
            .context("quantity")?
            .column_f64("price", safe_price)
            .context("price")?
            .column_str("detail", detail.as_str())
            .context("detail")?
            .at(TimestampNanos::new(r.ts_ist_nanos))
            .context("designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        metrics::counter!("tv_order_audit_rows_total", "event" => r.event.as_str()).increment(1);
        Ok(())
    }

    /// Flushes buffered rows over ILP-HTTP (per-flush server ACK). On ANY
    /// failed flush the pending buffer is DISCARDED (poisoned-buffer
    /// defense) — the rows are best-effort forensics; the discard is
    /// counted (`tv_order_audit_rows_discarded_total`), never silent.
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
                "order_audit: no ILP sender (QuestDB unreachable) — \
                 {dropped} pending forensics row(s) discarded (best-effort)"
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
                    "order_audit ILP flush failed — {dropped} pending \
                     forensics row(s) discarded (poisoned-buffer defense)"
                )))
            }
            // Unreachable (checked above) — treated as the no-sender arm.
            None => {
                let dropped = self.discard_pending();
                anyhow::bail!("order_audit: ILP sender vanished — {dropped} row(s) discarded");
            }
        }
    }

    /// Drop every buffered-but-unflushed row (poisoned-buffer defense).
    /// Returns the discarded row count; counted so a discard is never silent.
    pub fn discard_pending(&mut self) -> usize {
        let dropped = self.pending;
        if dropped > 0 {
            metrics::counter!("tv_order_audit_rows_discarded_total").increment(dropped as u64);
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

    fn sample_row() -> OrderAuditRow {
        OrderAuditRow {
            ts_ist_nanos: 1_770_000_900_000_000_000,
            trading_date_ist_nanos: 1_769_990_400_000_000_000,
            order_id: "PAPER-1".to_string(),
            correlation_id: "corr-001".to_string(),
            leg: "single",
            event: OrderAuditEvent::Placed,
            feed: "dhan",
            mode: "paper",
            security_id: 13,
            exchange_segment: "IDX_I",
            transaction_type: "BUY",
            quantity: 1,
            price: 0.0,
            order_status: "pending",
            outcome: "ok",
            detail: "market order placed".to_string(),
        }
    }

    #[test]
    fn test_order_audit_create_ddl_contains_expected_columns() {
        let ddl = order_audit_create_ddl();
        for col in [
            "ts ",
            "trading_date_ist",
            "order_id",
            "correlation_id",
            "leg",
            "event",
            "feed",
            "mode",
            "security_id",
            "exchange_segment",
            "transaction_type",
            "quantity",
            "price",
            "order_status",
            "outcome",
            "detail",
        ] {
            assert!(ddl.contains(col), "DDL missing column {col:?}: {ddl}");
        }
        assert!(ddl.contains("CREATE TABLE IF NOT EXISTS"));
        assert!(ddl.contains("PARTITION BY DAY"));
        assert!(ddl.contains(&format!("DEDUP UPSERT KEYS({DEDUP_KEY_ORDER_AUDIT})")));
    }

    /// DEDUP-key discipline: designated `ts` FIRST (2026-04-28 regression
    /// rule); `trading_date_ist` in-key (phase-0 composite-DEDUP rule 1);
    /// `event` in-key (phase-0 rule 3 — transition rows BOTH survive);
    /// `feed` in-key (2026-06-28 override) — whole-token matches + arity 6.
    #[test]
    fn test_order_audit_dedup_key_ts_first_event_and_feed_in_key() {
        assert!(DEDUP_KEY_ORDER_AUDIT.trim_start().starts_with("ts,"));
        let has_token = |t: &str| {
            DEDUP_KEY_ORDER_AUDIT
                .split([',', ' '])
                .map(str::trim)
                .any(|tok| tok == t)
        };
        assert!(has_token("trading_date_ist"));
        assert!(has_token("order_id"));
        assert!(has_token("leg"));
        assert!(has_token("event"));
        assert!(has_token("feed"));
        // Exactly (ts, trading_date_ist, order_id, leg, event, feed).
        assert_eq!(DEDUP_KEY_ORDER_AUDIT.matches(',').count() + 1, 6);
    }

    /// Kept from the recovered pre-#T4 module: order rows are per-ORDER,
    /// not per-instrument — the natural identity is `(order_id, leg,
    /// event)`. `security_id` is in the schema for join-friendliness but
    /// NOT in the DEDUP key, so the meta-guard's "security_id ⇒ segment"
    /// rule does not apply to this constant.
    #[test]
    fn test_dedup_key_uses_order_id_not_security_id() {
        assert!(DEDUP_KEY_ORDER_AUDIT.contains("order_id"));
        assert!(DEDUP_KEY_ORDER_AUDIT.contains("leg"));
        assert!(!DEDUP_KEY_ORDER_AUDIT.contains("security_id"));
    }

    /// Event SYMBOL wire strings are stable (in the DEDUP key + grouped on
    /// by forensic queries) and cover every enum variant distinctly.
    #[test]
    fn test_order_audit_event_wire_strings_stable_and_distinct() {
        let all = [
            (OrderAuditEvent::Placed, "placed"),
            (OrderAuditEvent::PlaceFailed, "place_failed"),
            (OrderAuditEvent::Cancelled, "cancelled"),
            (OrderAuditEvent::CancelFailed, "cancel_failed"),
            (OrderAuditEvent::Rejected, "rejected"),
            (OrderAuditEvent::CircuitOpen, "circuit_open"),
            (OrderAuditEvent::CircuitClosed, "circuit_closed"),
            (OrderAuditEvent::RateLimited, "rate_limited"),
        ];
        for (variant, wire) in all {
            assert_eq!(variant.as_str(), wire);
        }
        let mut wires: Vec<&str> = all.iter().map(|(v, _)| v.as_str()).collect();
        wires.sort_unstable();
        wires.dedup();
        assert_eq!(wires.len(), all.len(), "wire strings must be distinct");
    }

    #[test]
    fn test_append_order_audit_row_writes_symbols_and_columns() {
        let mut w = OrderAuditWriter::for_test();
        w.append_order_audit_row(&sample_row())
            .expect("append must succeed");
        assert_eq!(w.pending(), 1);
        let line = w.buffer_utf8();
        assert!(line.starts_with(ORDER_AUDIT_TABLE));
        assert!(line.contains(",order_id=PAPER-1"), "order_id tag: {line}");
        assert!(line.contains(",event=placed"), "event tag: {line}");
        assert!(line.contains(",feed=dhan"), "feed tag: {line}");
        assert!(line.contains(",mode=paper"), "mode tag: {line}");
        assert!(line.contains(",leg=single"), "leg tag: {line}");
        assert!(
            line.contains(",exchange_segment=IDX_I"),
            "segment tag: {line}"
        );
        assert!(line.contains(",outcome=ok"), "outcome tag: {line}");
        assert!(line.contains("security_id=13i"), "security_id: {line}");
        assert!(line.contains("quantity=1i"), "quantity: {line}");
        assert!(line.contains("detail="), "detail column: {line}");
    }

    /// Financial boundary: zero-quantity events (circuit transitions,
    /// rate-limit denials carry no quantity) append cleanly with 0.
    #[test]
    fn test_append_order_audit_row_zero_quantity_boundary() {
        let mut w = OrderAuditWriter::for_test();
        let row = OrderAuditRow {
            event: OrderAuditEvent::CircuitOpen,
            quantity: 0,
            price: 0.0,
            security_id: -1,
            exchange_segment: "n/a",
            transaction_type: "n/a",
            order_status: "n/a",
            ..sample_row()
        };
        w.append_order_audit_row(&row).expect("append must succeed");
        let line = w.buffer_utf8();
        assert!(line.contains("quantity=0i"), "got: {line}");
        assert!(line.contains(",event=circuit_open"), "got: {line}");
        assert!(line.contains("security_id=-1i"), "got: {line}");
    }

    /// Financial boundary: i64::MAX quantity survives the append without
    /// overflow (saturating pending arithmetic; ILP i64 column).
    #[test]
    fn test_append_order_audit_row_i64_max_quantity_boundary() {
        let mut w = OrderAuditWriter::for_test();
        let row = OrderAuditRow {
            quantity: i64::MAX,
            ..sample_row()
        };
        w.append_order_audit_row(&row).expect("append must succeed");
        let line = w.buffer_utf8();
        assert!(
            line.contains(&format!("quantity={}i", i64::MAX)),
            "got: {line}"
        );
    }

    /// Financial boundary (Wave-2-D HIGH finding preserved): NaN/Infinity
    /// prices clamp to 0.0 instead of poisoning the ILP buffer.
    #[test]
    fn test_append_order_audit_row_nonfinite_price_clamped_to_zero_boundary() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut w = OrderAuditWriter::for_test();
            let row = OrderAuditRow {
                price: bad,
                ..sample_row()
            };
            w.append_order_audit_row(&row).expect("append must succeed");
            let line = w.buffer_utf8();
            assert!(
                line.contains("price=0") || line.contains("price=0.0"),
                "non-finite price must clamp to 0.0, got: {line}"
            );
            assert!(!line.contains("NaN"), "NaN leaked into ILP: {line}");
        }
    }

    /// `detail` is sanitized and hard-capped at 200 chars — bounded
    /// forensic text, never an unbounded broker body.
    #[test]
    fn test_append_order_audit_row_detail_truncated_to_200_chars() {
        let mut w = OrderAuditWriter::for_test();
        let row = OrderAuditRow {
            detail: "x".repeat(500),
            ..sample_row()
        };
        w.append_order_audit_row(&row).expect("append must succeed");
        let line = w.buffer_utf8();
        let detail_field = line
            .split("detail=")
            .nth(1)
            .expect("detail field present in ILP line");
        assert!(
            detail_field.matches('x').count() <= ORDER_AUDIT_DETAIL_MAX_CHARS,
            "detail must be truncated to {ORDER_AUDIT_DETAIL_MAX_CHARS} chars"
        );
    }

    #[test]
    fn test_order_audit_flush_when_disconnected_errors_and_discards_pending() {
        let mut w = OrderAuditWriter::for_test();
        w.append_order_audit_row(&sample_row())
            .expect("append must succeed");
        let err = w.flush().expect_err("disconnected flush must error");
        assert!(err.to_string().contains("no ILP sender"));
        assert!(err.to_string().contains("discarded"));
        assert_eq!(w.pending(), 0, "failed flush discards pending");
        assert!(w.buffer_utf8().is_empty(), "ILP buffer cleared on discard");
        // Empty flush is a no-op Ok.
        let mut empty = OrderAuditWriter::for_test();
        assert!(empty.flush().is_ok());
        // Idempotent: a second discard drops nothing.
        assert_eq!(w.discard_pending(), 0);
    }

    /// Transport ratchet (2026-07-05 fire-and-forget lesson): ILP-over-HTTP
    /// with the shadow-candle-writer knobs — never ILP TCP 9009.
    #[test]
    fn test_order_audit_writer_uses_ilp_http_conf_with_bounded_knobs() {
        let cfg = QuestDbConfig {
            host: "tv-questdb".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let conf = order_audit_ilp_http_conf(&cfg);
        assert_eq!(
            conf,
            "http::addr=tv-questdb:9000;protocol_version=1;retry_timeout=0;request_timeout=5000;"
        );
        assert!(!conf.contains("9009"), "must not target ILP TCP: {conf}");
    }

    // ========================================================================
    // Mock QuestDB /exec HTTP server + unreachable host (the
    // rest_fetch_audit_persistence pattern) — exercises the real ensure /
    // constructor code paths: success (200), non-2xx (500), transport error.
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
    async fn test_ensure_order_audit_table_mock_200_completes() {
        let port = spawn_mock_http(MOCK_HTTP_200).await;
        ensure_order_audit_table(&mock_cfg(port)).await;
    }

    #[tokio::test]
    async fn test_ensure_order_audit_table_mock_500_degrades_without_panic() {
        let port = spawn_mock_http(MOCK_HTTP_500).await;
        ensure_order_audit_table(&mock_cfg(port)).await;
    }

    #[tokio::test]
    async fn test_ensure_order_audit_table_unreachable_degrades_without_panic() {
        ensure_order_audit_table(&unreachable_cfg()).await;
    }

    #[tokio::test]
    async fn test_order_audit_writer_new_is_lazy_and_buffers_without_network() {
        let mut w = OrderAuditWriter::new(&unreachable_cfg());
        assert_eq!(w.pending(), 0, "fresh writer has nothing pending");
        w.append_order_audit_row(&sample_row())
            .expect("append must succeed without network");
        assert_eq!(w.pending(), 1, "row buffered locally");
    }
}

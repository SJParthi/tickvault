//! `live_order_events` table — one row per PAPER order event of the live
//! signal-timing work (plan item LF-5, `.claude/plans/active-plan-audit-2026-10-20.md`
//! "LF — 2026-11-12"; owner Quote 57 in the daily-universe rule file).
//!
//! Each would-be order goes through the events decision, built, sent, ack,
//! exchange_order, fill, reject, modify, cancel, exit, stop and square_off.
//! Every event is one row, so the daily calibration report (LF-7) can read
//! the decision time against its candle boundary, every latency step, and
//! every would-be fill against the Brutex worst-case model price.
//!
//! PAPER ONLY today (`mode = paper`). Nothing in this module places, modifies
//! or cancels an order; it only records what the paper chain did.
//!
//! Money is integer paise everywhere. A negative price, quantity or charge is
//! refused before the row reaches the ILP buffer (counted and coded), and a
//! float can never reach this table, so NaN and infinity are unrepresentable.
//!
//! Treated like `order_audit`: an audit table that is never deleted. It is on
//! the DAY retention list (90 days hot in QuestDB, then copied to S3 and
//! verified before the partition leaves the box), on the fresh-start
//! never-reset list, and its failed flushes go to the audit disk tier.
//!
//! ## Schema
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS live_order_events (
//!     ts TIMESTAMP,
//!     event SYMBOL, signal_id LONG, seq INT, feed SYMBOL, mode SYMBOL,
//!     recipe SYMBOL, recipe_version INT,
//!     security_id LONG, segment SYMBOL, symbol SYMBOL, side SYMBOL,
//!     qty LONG, slot_paise LONG,
//!     candle_start TIMESTAMP, candle_end TIMESTAMP,
//!     last_tick_exchange_ts TIMESTAMP, last_tick_receipt_ts TIMESTAMP,
//!     close_by SYMBOL, decision_ts TIMESTAMP,
//!     best_bid_paise LONG, best_bid_qty LONG,
//!     best_ask_paise LONG, best_ask_qty LONG,
//!     ltp_paise LONG, second_volume LONG,
//!     limit_paise LONG, limit_bound SYMBOL,
//!     queue_wait_us LONG, feed_lag_us LONG, api_rtt_us LONG,
//!     order_id STRING, exchange_order_ts TIMESTAMP, exchange_trade_ts TIMESTAMP,
//!     fill_qty LONG, fill_price_paise LONG, avg_price_paise LONG, remainder_qty LONG,
//!     model_price_paise LONG, worse_than_model SYMBOL,
//!     reason STRING, charges_paise LONG,
//!     tick_paise LONG, lot_size LONG, participation_bps INT, max_fill_seconds INT
//! ) timestamp(ts) PARTITION BY DAY
//! DEDUP UPSERT KEYS(ts, signal_id, event, seq, feed);
//! ```
//!
//! DEDUP discipline: designated `ts` FIRST (2026-04-28 rule); per-SIGNAL
//! identity `signal_id, event, seq` (two events of one signal at the same
//! instant, or two fills of one order, both survive because `seq` differs);
//! `feed` in-key (feed-in-key EVERYWHERE, 2026-06-28). `security_id` is a
//! column, NOT a key part: one signal has one instrument, so the I-P1-11
//! segment rule does not apply to the key (same reasoning as `order_audit`).
//!
//! Every timestamp is IST wall-clock written as Z, like every other table.
//! The designated `ts` is a plain `TIMESTAMP`, which QuestDB stores in
//! MICROSECONDS: the caller's nanoseconds go on the wire but the sub-micro
//! part is dropped on store, so the DEDUP key works at 1 µs. Two events of one
//! signal inside the same microsecond are kept apart only by `seq`, which is
//! why `seq` must be unique per event within a signal. The other TIMESTAMP
//! columns are microseconds on the wire too (ILP protocol version 1 writes
//! `column_ts` in micros), finer than the millisecond the report needs.
//!
//! `worse_than_model` is a SYMBOL (`worse` / `not_worse`), not a BOOLEAN:
//! QuestDB cannot store a NULL BOOLEAN, so an unknown verdict would read back
//! as `false` ("not worse"), a false OK in the calibration report. An omitted
//! SYMBOL reads as NULL. The table has never been created anywhere (no
//! production writer exists yet, 2026-11-12), so the type is changed in place
//! rather than by a migration.
//!
//! QuestDB stores `i64::MIN` in a LONG and `i32::MIN` in an INT as NULL, so
//! both are refused before the buffer (a value would otherwise read back as
//! "missing").
//!
//! Text values are cleaned for ILP, not SQL: SYMBOL values go through
//! `sanitize_ilp_symbol` and STRING values through `sanitize_ilp_string`, which
//! strip control, BiDi and zero-width characters and cap the length but keep
//! `'` and `--` (the questdb buffer escapes the ILP-special characters
//! itself). `order_audit` still runs its ILP values through the SQL sanitizer
//! `sanitize_audit_string` (doubles `'`, removes `;` and `--`); this table does
//! not copy that, because on an ILP path it changes the stored value.

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::sanitize::{sanitize_ilp_string, sanitize_ilp_symbol};
use tickvault_common::types::ExchangeSegment;

use crate::audit_spill::{AuditSpill, AuditSpillTable, spill_failed_batch};

/// This writer's disk-tier table.
const SPILL_TABLE: AuditSpillTable = AuditSpillTable::LiveOrderEvents;

/// QuestDB table name — one row per paper order event.
pub const LIVE_ORDER_EVENTS_TABLE: &str = "live_order_events";

/// DEDUP key. Designated `ts` FIRST; per-signal identity `signal_id, event,
/// seq`; `feed` in-key (2026-06-28 override).
pub const DEDUP_KEY_LIVE_ORDER_EVENTS: &str = "ts, signal_id, event, seq, feed";

/// `reason` column hard cap — bounded text, never a raw broker body.
pub const LIVE_ORDER_EVENTS_REASON_MAX_CHARS: usize = 200;

/// `order_id` column hard cap.
pub const LIVE_ORDER_EVENTS_ORDER_ID_MAX_CHARS: usize = 64;

/// Sentinel for a SYMBOL value that is empty at row time (an empty ILP tag
/// value is invalid in line protocol; same convention as `order_audit`).
pub const LIVE_ORDER_EVENTS_SYMBOL_NA: &str = "n/a";

/// The feed every writer stamps today.
pub const LIVE_ORDER_EVENTS_FEED_DHAN: &str = "dhan";

/// `worse_than_model` SYMBOL value when the fill was worse than the model.
pub const LIVE_ORDER_EVENTS_WORSE: &str = "worse";

/// `worse_than_model` SYMBOL value when the fill was not worse than the model.
pub const LIVE_ORDER_EVENTS_NOT_WORSE: &str = "not_worse";

/// Wire value of `worse_than_model`; `None` omits the column, which QuestDB
/// stores as NULL (a BOOLEAN cannot be NULL and would read as `false`). Pure.
#[must_use]
pub const fn worse_than_model_wire(worse: Option<bool>) -> Option<&'static str> {
    match worse {
        Some(true) => Some(LIVE_ORDER_EVENTS_WORSE),
        Some(false) => Some(LIVE_ORDER_EVENTS_NOT_WORSE),
        None => None,
    }
}

/// Every column after `ts`, with its QuestDB type, in DDL order. The CREATE
/// below and the `ADD COLUMN IF NOT EXISTS` self-heal both follow this list,
/// and a unit test pins all three (manifest, CREATE, ILP row) to each other.
pub const LIVE_ORDER_EVENTS_COLUMNS: &[(&str, &str)] = &[
    ("event", "SYMBOL"),
    ("signal_id", "LONG"),
    ("seq", "INT"),
    ("feed", "SYMBOL"),
    ("mode", "SYMBOL"),
    ("recipe", "SYMBOL"),
    ("recipe_version", "INT"),
    ("security_id", "LONG"),
    ("segment", "SYMBOL"),
    ("symbol", "SYMBOL"),
    ("side", "SYMBOL"),
    ("qty", "LONG"),
    ("slot_paise", "LONG"),
    ("candle_start", "TIMESTAMP"),
    ("candle_end", "TIMESTAMP"),
    ("last_tick_exchange_ts", "TIMESTAMP"),
    ("last_tick_receipt_ts", "TIMESTAMP"),
    ("close_by", "SYMBOL"),
    ("decision_ts", "TIMESTAMP"),
    ("best_bid_paise", "LONG"),
    ("best_bid_qty", "LONG"),
    ("best_ask_paise", "LONG"),
    ("best_ask_qty", "LONG"),
    ("ltp_paise", "LONG"),
    ("second_volume", "LONG"),
    ("limit_paise", "LONG"),
    ("limit_bound", "SYMBOL"),
    ("queue_wait_us", "LONG"),
    ("feed_lag_us", "LONG"),
    ("api_rtt_us", "LONG"),
    ("order_id", "STRING"),
    ("exchange_order_ts", "TIMESTAMP"),
    ("exchange_trade_ts", "TIMESTAMP"),
    ("fill_qty", "LONG"),
    ("fill_price_paise", "LONG"),
    ("avg_price_paise", "LONG"),
    ("remainder_qty", "LONG"),
    ("model_price_paise", "LONG"),
    ("worse_than_model", "SYMBOL"),
    ("reason", "STRING"),
    ("charges_paise", "LONG"),
    ("tick_paise", "LONG"),
    ("lot_size", "LONG"),
    ("participation_bps", "INT"),
    ("max_fill_seconds", "INT"),
];

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Typed order event. The SYMBOL wire strings are stable (the report groups
/// on them) and part of the DEDUP key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveOrderEventKind {
    /// The strategy decided to trade (signal raised).
    Decision,
    /// The order request was built (limit price chosen).
    Built,
    /// The order request was sent (paper: handed to the paper OMS).
    Sent,
    /// The order was acknowledged.
    Ack,
    /// The exchange order time is known.
    ExchangeOrder,
    /// A (partial or full) fill.
    Fill,
    /// The order was rejected.
    Reject,
    /// The order was modified.
    Modify,
    /// The order was cancelled.
    Cancel,
    /// An exit order for the position.
    Exit,
    /// A stop was triggered.
    Stop,
    /// The position was squared off (end of day or forced).
    SquareOff,
}

impl LiveOrderEventKind {
    /// Every variant, in lifecycle order.
    pub const ALL: [Self; 12] = [
        Self::Decision,
        Self::Built,
        Self::Sent,
        Self::Ack,
        Self::ExchangeOrder,
        Self::Fill,
        Self::Reject,
        Self::Modify,
        Self::Cancel,
        Self::Exit,
        Self::Stop,
        Self::SquareOff,
    ];

    /// Stable SYMBOL wire string (in the DEDUP key — never reworded).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Decision => "decision",
            Self::Built => "built",
            Self::Sent => "sent",
            Self::Ack => "ack",
            Self::ExchangeOrder => "exchange_order",
            Self::Fill => "fill",
            Self::Reject => "reject",
            Self::Modify => "modify",
            Self::Cancel => "cancel",
            Self::Exit => "exit",
            Self::Stop => "stop",
            Self::SquareOff => "square_off",
        }
    }
}

/// `paper` or `live`. Only `Paper` is written today (Quote 57).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveOrderMode {
    /// Paper order (no real order exists).
    Paper,
    /// Real order (not used today).
    Live,
}

impl LiveOrderMode {
    /// Stable SYMBOL wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Paper => "paper",
            Self::Live => "live",
        }
    }
}

/// Order side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveOrderSide {
    /// Buy.
    Buy,
    /// Sell.
    Sell,
}

impl LiveOrderSide {
    /// Stable SYMBOL wire string (same spelling as `order_audit`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Buy => "BUY",
            Self::Sell => "SELL",
        }
    }
}

/// What closed the strategy candle the decision was made on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveCandleCloseBy {
    /// The wall clock passed the candle boundary.
    Clock,
    /// A tick stamped at or after the boundary arrived first.
    Tick,
}

impl LiveCandleCloseBy {
    /// Stable SYMBOL wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Clock => "clock",
            Self::Tick => "tick",
        }
    }
}

/// Which bound set the limit price.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveLimitBound {
    /// The configured band around the last traded price.
    Band,
    /// The slot cap (slot × 5 / qty).
    SlotCap,
    /// The day's circuit (price band) limit.
    Circuit,
}

impl LiveLimitBound {
    /// Stable SYMBOL wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Band => "band",
            Self::SlotCap => "slot_cap",
            Self::Circuit => "circuit",
        }
    }
}

/// One paper order event, ready for an ILP row. Optional fields that do not
/// apply to an event are `None` and written as NULL (the column is omitted).
/// Every `*_ist_nanos` field is IST wall-clock nanoseconds written as Z.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveOrderEvent {
    /// Designated timestamp — the event instant. Stored in microseconds (the
    /// sub-micro part is dropped by QuestDB).
    pub ts_ist_nanos: i64,
    /// Typed event.
    pub event: LiveOrderEventKind,
    /// The signal this event belongs to (non-negative).
    pub signal_id: i64,
    /// Order of this event within its signal (non-negative). MUST be unique
    /// per event within a signal: `ts` is stored in microseconds, so two
    /// events of one signal in the same microsecond (two fills, or a fill and
    /// a reject) differ in the DEDUP key only by `seq`, and a repeated `seq`
    /// would make one silently replace the other.
    pub seq: i32,
    /// Source broker (`dhan` today).
    pub feed: &'static str,
    /// `paper` today.
    pub mode: LiveOrderMode,
    /// Strategy recipe name (sanitized at append).
    pub recipe: String,
    /// Strategy recipe version (non-negative).
    pub recipe_version: i32,
    /// Instrument id.
    pub security_id: i64,
    /// Instrument segment.
    pub segment: ExchangeSegment,
    /// Contract label (sanitized at append).
    pub symbol: String,
    /// Order side.
    pub side: LiveOrderSide,
    /// Order quantity (non-negative).
    pub qty: i64,
    /// Capital slot in paise (non-negative).
    pub slot_paise: i64,
    /// Strategy candle start.
    pub candle_start_ist_nanos: Option<i64>,
    /// Strategy candle end.
    pub candle_end_ist_nanos: Option<i64>,
    /// Exchange time of the last tick in the candle.
    pub last_tick_exchange_ist_nanos: Option<i64>,
    /// Receipt time of the last tick in the candle.
    pub last_tick_receipt_ist_nanos: Option<i64>,
    /// What closed the candle.
    pub close_by: Option<LiveCandleCloseBy>,
    /// When the strategy decided.
    pub decision_ist_nanos: Option<i64>,
    /// Best bid at the decision, paise.
    pub best_bid_paise: Option<i64>,
    /// Best bid quantity at the decision.
    pub best_bid_qty: Option<i64>,
    /// Best ask at the decision, paise.
    pub best_ask_paise: Option<i64>,
    /// Best ask quantity at the decision.
    pub best_ask_qty: Option<i64>,
    /// Last traded price at the decision, paise.
    pub ltp_paise: Option<i64>,
    /// Traded volume in the decision second.
    pub second_volume: Option<i64>,
    /// Limit price sent, paise.
    pub limit_paise: Option<i64>,
    /// Which bound set the limit price.
    pub limit_bound: Option<LiveLimitBound>,
    /// Time the intent waited in the send queue, microseconds.
    pub queue_wait_us: Option<i64>,
    /// Feed lag (receipt − exchange time), microseconds; may be negative.
    pub feed_lag_us: Option<i64>,
    /// Order API round trip, microseconds.
    pub api_rtt_us: Option<i64>,
    /// Broker or paper order id (sanitized at append).
    pub order_id: Option<String>,
    /// Exchange order time.
    pub exchange_order_ist_nanos: Option<i64>,
    /// Exchange trade time.
    pub exchange_trade_ist_nanos: Option<i64>,
    /// Quantity filled by this event.
    pub fill_qty: Option<i64>,
    /// Price of this fill, paise.
    pub fill_price_paise: Option<i64>,
    /// Average fill price so far, paise.
    pub avg_price_paise: Option<i64>,
    /// Quantity still unfilled.
    pub remainder_qty: Option<i64>,
    /// The worst-case model price for this fill, paise.
    pub model_price_paise: Option<i64>,
    /// Whether the fill was worse than the worst-case model. Written as the
    /// SYMBOL `worse` / `not_worse`; `None` omits it, so it reads as NULL
    /// (unknown), never as "not worse".
    pub worse_than_model: Option<bool>,
    /// Bounded free-text reason (sanitized and truncated at append).
    pub reason: Option<String>,
    /// Charges, paise.
    pub charges_paise: Option<i64>,
    /// The contract's tick size, paise. Decision row only (LF-7: the
    /// daily report re-prices each order from the stored seconds).
    pub tick_paise: Option<i64>,
    /// The contract's lot size, shares. Decision row only (LF-7).
    pub lot_size: Option<i64>,
    /// Our share of a second's volume, basis points. Decision row only (LF-7).
    pub participation_bps: Option<i32>,
    /// The fill window, seconds. Decision row only (LF-7).
    pub max_fill_seconds: Option<i32>,
}

/// Why a row was refused before it reached the ILP buffer. Names the column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiveOrderEventRefusal {
    /// The offending column.
    pub column: &'static str,
}

impl std::fmt::Display for LiveOrderEventRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "live_order_events row refused: `{}` is negative or out of range",
            self.column
        )
    }
}

impl std::error::Error for LiveOrderEventRefusal {}

/// Checks a row before any byte reaches the buffer. Pure, O(columns).
///
/// Refuses: a designated `ts` at or before the epoch; a negative identity
/// (`signal_id`, `seq`, `recipe_version`); a negative quantity, volume or
/// money column; a negative optional timestamp. Latencies (`queue_wait_us`,
/// `feed_lag_us`, `api_rtt_us`) are NOT checked for sign: feed lag is
/// legitimately negative when the exchange time is ahead of receipt.
///
/// Also refuses `i64::MIN` in every LONG column (`security_id` and the three
/// latencies are the only LONGs the sign checks leave open) and `i32::MIN` in
/// every INT column (covered by the sign checks on `seq` and
/// `recipe_version`): QuestDB stores those values as NULL, so the row would
/// read back as "missing" rather than the value written.
///
/// # Errors
/// The first offending column, in DDL order.
// FINANCIAL-TEST-EXEMPT: sign checks only, no arithmetic; boundary tests below cover every column
pub fn validate_live_order_event(e: &LiveOrderEvent) -> Result<(), LiveOrderEventRefusal> {
    let refuse = |column: &'static str| Err(LiveOrderEventRefusal { column });
    if e.ts_ist_nanos <= 0 {
        return refuse("ts");
    }
    if e.signal_id < 0 {
        return refuse("signal_id");
    }
    if e.seq < 0 {
        return refuse("seq");
    }
    if e.recipe_version < 0 {
        return refuse("recipe_version");
    }
    // QuestDB's LONG NULL. Every other LONG is refused below zero, except the
    // three latencies checked just below.
    if e.security_id == i64::MIN {
        return refuse("security_id");
    }
    if e.qty < 0 {
        return refuse("qty");
    }
    if e.slot_paise < 0 {
        return refuse("slot_paise");
    }
    let not_long_null: [(&'static str, Option<i64>); 3] = [
        ("queue_wait_us", e.queue_wait_us),
        ("feed_lag_us", e.feed_lag_us),
        ("api_rtt_us", e.api_rtt_us),
    ];
    for (column, value) in not_long_null {
        if value == Some(i64::MIN) {
            return refuse(column);
        }
    }
    let non_negative: [(&'static str, Option<i64>); 24] = [
        ("candle_start", e.candle_start_ist_nanos),
        ("candle_end", e.candle_end_ist_nanos),
        ("last_tick_exchange_ts", e.last_tick_exchange_ist_nanos),
        ("last_tick_receipt_ts", e.last_tick_receipt_ist_nanos),
        ("decision_ts", e.decision_ist_nanos),
        ("best_bid_paise", e.best_bid_paise),
        ("best_bid_qty", e.best_bid_qty),
        ("best_ask_paise", e.best_ask_paise),
        ("best_ask_qty", e.best_ask_qty),
        ("ltp_paise", e.ltp_paise),
        ("second_volume", e.second_volume),
        ("limit_paise", e.limit_paise),
        ("exchange_order_ts", e.exchange_order_ist_nanos),
        ("exchange_trade_ts", e.exchange_trade_ist_nanos),
        ("fill_qty", e.fill_qty),
        ("fill_price_paise", e.fill_price_paise),
        ("avg_price_paise", e.avg_price_paise),
        ("remainder_qty", e.remainder_qty),
        ("model_price_paise", e.model_price_paise),
        ("charges_paise", e.charges_paise),
        ("tick_paise", e.tick_paise),
        ("lot_size", e.lot_size),
        ("participation_bps", e.participation_bps.map(i64::from)),
        ("max_fill_seconds", e.max_fill_seconds.map(i64::from)),
    ];
    for (column, value) in non_negative {
        if value.is_some_and(|v| v < 0) {
            return refuse(column);
        }
    }
    Ok(())
}

/// The idempotent `CREATE TABLE` DDL for `live_order_events`. Pure.
// FINANCIAL-TEST-EXEMPT: pure DDL string builder — no financial arithmetic ("order" is the table name)
#[must_use]
pub fn live_order_events_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {LIVE_ORDER_EVENTS_TABLE} (\
            ts                    TIMESTAMP, \
            event                 SYMBOL, \
            signal_id             LONG, \
            seq                   INT, \
            feed                  SYMBOL, \
            mode                  SYMBOL, \
            recipe                SYMBOL, \
            recipe_version        INT, \
            security_id           LONG, \
            segment               SYMBOL, \
            symbol                SYMBOL, \
            side                  SYMBOL, \
            qty                   LONG, \
            slot_paise            LONG, \
            candle_start          TIMESTAMP, \
            candle_end            TIMESTAMP, \
            last_tick_exchange_ts TIMESTAMP, \
            last_tick_receipt_ts  TIMESTAMP, \
            close_by              SYMBOL, \
            decision_ts           TIMESTAMP, \
            best_bid_paise        LONG, \
            best_bid_qty          LONG, \
            best_ask_paise        LONG, \
            best_ask_qty          LONG, \
            ltp_paise             LONG, \
            second_volume         LONG, \
            limit_paise           LONG, \
            limit_bound           SYMBOL, \
            queue_wait_us         LONG, \
            feed_lag_us           LONG, \
            api_rtt_us            LONG, \
            order_id              STRING, \
            exchange_order_ts     TIMESTAMP, \
            exchange_trade_ts     TIMESTAMP, \
            fill_qty              LONG, \
            fill_price_paise      LONG, \
            avg_price_paise       LONG, \
            remainder_qty         LONG, \
            model_price_paise     LONG, \
            worse_than_model      SYMBOL, \
            reason                STRING, \
            charges_paise         LONG, \
            tick_paise            LONG, \
            lot_size              LONG, \
            participation_bps     INT, \
            max_fill_seconds      INT\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_LIVE_ORDER_EVENTS});"
    )
}

/// Every DDL statement the ensure runs, in order: CREATE, one
/// `ADD COLUMN IF NOT EXISTS` per manifest column, then DEDUP ENABLE. Pure.
// FINANCIAL-TEST-EXEMPT: pure DDL string builder — no financial arithmetic ("order" is the table name)
#[must_use]
pub fn live_order_events_ensure_statements() -> Vec<String> {
    let mut statements = vec![live_order_events_create_ddl()];
    // Per-column self-heal for a table created by an earlier build
    // (observability-architecture.md schema-self-heal pattern).
    for (col, ty) in LIVE_ORDER_EVENTS_COLUMNS {
        statements.push(format!(
            "ALTER TABLE {LIVE_ORDER_EVENTS_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty};"
        ));
    }
    statements.push(format!(
        "ALTER TABLE {LIVE_ORDER_EVENTS_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_LIVE_ORDER_EVENTS});"
    ));
    statements
}

/// Creates the `live_order_events` table if absent, adds any missing column,
/// and enables its DEDUP key. Idempotent; runs at boot and again before the
/// disk tier replays a backlog. Failures log at `error!` (AUDIT-06, stage
/// `ensure_*`) and never block boot or panic.
///
/// Returns `true` only when the closing `DEDUP ENABLE UPSERT KEYS` answered
/// 2xx, so the table carries its key whatever created it.
// TEST-EXEMPT: live-QuestDB DDL runner (statements unit-tested; the 200/500/unreachable arms exercised via the mock-HTTP tokio tests below)
// FINANCIAL-TEST-EXEMPT: idempotent DDL runner — no financial arithmetic ("order" is the table name)
pub async fn ensure_live_order_events_table(questdb_config: &QuestDbConfig) -> bool {
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
            metrics::counter!("tv_live_order_events_persist_errors_total", "stage" => "ensure_client_build")
                .increment(1);
            error!(
                code = ErrorCode::Audit06OrderWriteFailed.code_str(),
                stage = "ensure_client_build",
                table = LIVE_ORDER_EVENTS_TABLE,
                ?err,
                "AUDIT-06: HTTP client build failed — live_order_events table not \
                 ensured (the first ILP write may create it WITHOUT dedup)"
            );
            return false;
        }
    };
    let statements = live_order_events_ensure_statements();
    let dedup_index = statements.len().saturating_sub(1);
    let mut keyed = false;
    for (index, ddl) in statements.iter().enumerate() {
        match client
            .get(&base_url)
            .query(&[("query", ddl.as_str())])
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => keyed |= index == dedup_index,
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                metrics::counter!("tv_live_order_events_persist_errors_total", "stage" => "ensure_ddl")
                    .increment(1);
                error!(
                    code = ErrorCode::Audit06OrderWriteFailed.code_str(),
                    stage = "ensure_ddl",
                    table = LIVE_ORDER_EVENTS_TABLE,
                    %status,
                    ddl = %ddl.chars().take(80).collect::<String>(),
                    body = %body.chars().take(200).collect::<String>(),
                    "AUDIT-06: live_order_events DDL returned non-2xx (dedup may be missing)"
                );
            }
            Err(err) => {
                metrics::counter!("tv_live_order_events_persist_errors_total", "stage" => "ensure_ddl")
                    .increment(1);
                error!(
                    code = ErrorCode::Audit06OrderWriteFailed.code_str(),
                    stage = "ensure_ddl",
                    table = LIVE_ORDER_EVENTS_TABLE,
                    ?err,
                    ddl = %ddl.chars().take(80).collect::<String>(),
                    "AUDIT-06: live_order_events DDL request failed"
                );
            }
        }
    }
    keyed
}

/// ILP-over-HTTP conf — per-flush server ACK, same knobs as `order_audit`.
fn live_order_events_ilp_http_conf(config: &QuestDbConfig) -> String {
    format!(
        "http::addr={}:{};protocol_version=1;retry_timeout=0;request_timeout=5000;",
        config.host, config.http_port
    )
}

/// SYMBOL value cleaned for ILP (`sanitize_ilp_symbol`: control, BiDi,
/// zero-width and the tag delimiters `,` `=` removed, length capped; `'` and
/// `--` kept), or the `n/a` sentinel when nothing is left.
fn symbol_or_na(value: &str) -> String {
    let sanitized = sanitize_ilp_symbol(value);
    if sanitized.is_empty() {
        LIVE_ORDER_EVENTS_SYMBOL_NA.to_string()
    } else {
        sanitized.into_owned()
    }
}

/// STRING value cleaned for ILP (`sanitize_ilp_string`: control, BiDi and
/// zero-width removed, `'` `--` `,` `=` kept) and capped at `max_chars`.
fn ilp_string_capped(value: &str, max_chars: usize) -> String {
    sanitize_ilp_string(value).chars().take(max_chars).collect()
}

/// Lazy ILP-over-HTTP writer for `live_order_events`.
///
/// `append` only writes into the in-memory ILP buffer (no network, no lock,
/// no wait). `flush` sends the batch; a failed flush hands the whole rows to
/// the audit disk tier (`audit_spill`), which replays them once QuestDB
/// answers. Only when the disk tier refuses the batch are the rows discarded,
/// counted on `tv_live_order_events_rows_discarded_total` and logged AUDIT-06.
///
/// Off the hot path: the paper chain (LF-6) owns one writer on its own task.
pub struct LiveOrderEventsWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
    /// Buffer length after the last whole row.
    committed_len: usize,
    /// Disk tier for a failed flush; `None` in `for_test`.
    spill: Option<AuditSpill>,
}

impl LiveOrderEventsWriter {
    /// Production constructor — ILP-over-HTTP sender, lazy on failure.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (lazy-build contract exercised via test_live_order_events_writer_new_is_lazy_and_buffers_without_network)
    pub fn new(config: &QuestDbConfig) -> Self {
        // Seeded at zero so the series exists before the first discard (an
        // absent series reads as healthy; ilp_overflow_bound_guard).
        metrics::counter!("tv_live_order_events_rows_discarded_total").increment(0);
        let conf = live_order_events_ilp_http_conf(config);
        match Sender::from_conf(&conf) {
            Ok(s) => {
                let b = s.new_buffer();
                Self {
                    sender: Some(s),
                    buffer: b,
                    pending: 0,
                    committed_len: 0,
                    spill: Some(AuditSpill::for_table(SPILL_TABLE)),
                }
            }
            Err(err) => {
                warn!(
                    ?err,
                    "live_order_events writer: QuestDB unreachable — buffering locally"
                );
                Self {
                    sender: None,
                    buffer: Buffer::new(ProtocolVersion::V1),
                    pending: 0,
                    committed_len: 0,
                    spill: Some(AuditSpill::for_table(SPILL_TABLE)),
                }
            }
        }
    }

    /// Test constructor — disconnected writer, empty buffer, no disk tier.
    #[must_use]
    // TEST-EXEMPT: test-only helper used by the append/flush unit tests below.
    pub fn for_test() -> Self {
        Self {
            sender: None,
            buffer: Buffer::new(ProtocolVersion::V1),
            pending: 0,
            committed_len: 0,
            spill: None,
        }
    }

    /// Rows appended but not yet flushed.
    #[must_use]
    // TEST-EXEMPT: observability accessor, exercised by the append tests below.
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// Test-only view of the ILP buffer bytes.
    #[cfg(test)]
    fn buffer_utf8(&self) -> String {
        String::from_utf8(self.buffer.as_bytes().to_vec()).unwrap_or_default()
    }

    /// Appends one event row to the ILP buffer. In-memory only: never sends,
    /// never waits. O(columns + string lengths).
    ///
    /// The row is validated first ([`validate_live_order_event`]); a refused
    /// row leaves the buffer untouched, is counted on
    /// `tv_live_order_events_rows_refused_total{column}` and logged AUDIT-06.
    /// A row whose append fails half-way is rewound, so the buffer only ever
    /// holds whole rows.
    ///
    /// # Errors
    /// The refusal, or the ILP buffer error.
    // FINANCIAL-TEST-EXEMPT: no arithmetic; integer paise passed through; negative and boundary values tested below
    pub fn append(&mut self, e: &LiveOrderEvent) -> Result<()> {
        if let Err(refusal) = validate_live_order_event(e) {
            metrics::counter!("tv_live_order_events_rows_refused_total", "column" => refusal.column)
                .increment(1);
            error!(
                code = ErrorCode::Audit06OrderWriteFailed.code_str(),
                stage = "append",
                table = LIVE_ORDER_EVENTS_TABLE,
                column = refusal.column,
                signal_id = e.signal_id,
                event = e.event.as_str(),
                "AUDIT-06: live_order_events row refused before the buffer — a column \
                 holds a negative value"
            );
            return Err(anyhow::Error::new(refusal));
        }
        self.buffer.set_marker().context("set_marker")?;
        let appended = self.write_row(e);
        if let Err(err) = appended {
            // Drop the half-written row; the buffer keeps only whole rows.
            // A failed rewind can only mean no marker was set, which the
            // `set_marker` above rules out; if it ever happens, the half row
            // stays, and `committed_len` still keeps it out of a spill.
            if let Err(rewind) = self.buffer.rewind_to_marker() {
                error!(
                    code = ErrorCode::Audit06OrderWriteFailed.code_str(),
                    stage = "append",
                    table = LIVE_ORDER_EVENTS_TABLE,
                    ?rewind,
                    "AUDIT-06: live_order_events could not rewind a half-written row"
                );
            }
            self.buffer.clear_marker();
            metrics::counter!("tv_live_order_events_persist_errors_total", "stage" => "append")
                .increment(1);
            error!(
                code = ErrorCode::Audit06OrderWriteFailed.code_str(),
                stage = "append",
                table = LIVE_ORDER_EVENTS_TABLE,
                signal_id = e.signal_id,
                event = e.event.as_str(),
                error = %err,
                "AUDIT-06: live_order_events row could not be appended to the ILP buffer"
            );
            return Err(err);
        }
        self.buffer.clear_marker();
        self.pending = self.pending.saturating_add(1);
        self.committed_len = self.buffer.len();
        // Counted at APPEND, like `tv_order_audit_rows_total`:
        // acked = this − `tv_live_order_events_rows_discarded_total`.
        metrics::counter!("tv_live_order_events_rows_total", "event" => e.event.as_str())
            .increment(1);
        Ok(())
    }

    /// Writes the row's symbols then columns then the designated timestamp.
    fn write_row(&mut self, e: &LiveOrderEvent) -> Result<()> {
        let b = &mut self.buffer;
        b.table(LIVE_ORDER_EVENTS_TABLE).context("table")?;
        // Symbols BEFORE columns (ILP tags-before-fields rule).
        b.symbol("event", e.event.as_str()).context("event")?;
        b.symbol("feed", symbol_or_na(e.feed)).context("feed")?;
        b.symbol("mode", e.mode.as_str()).context("mode")?;
        b.symbol("recipe", symbol_or_na(&e.recipe))
            .context("recipe")?;
        b.symbol("segment", e.segment.as_str()).context("segment")?;
        b.symbol("symbol", symbol_or_na(&e.symbol))
            .context("symbol")?;
        b.symbol("side", e.side.as_str()).context("side")?;
        if let Some(close_by) = e.close_by {
            b.symbol("close_by", close_by.as_str())
                .context("close_by")?;
        }
        if let Some(bound) = e.limit_bound {
            b.symbol("limit_bound", bound.as_str())
                .context("limit_bound")?;
        }
        if let Some(worse) = worse_than_model_wire(e.worse_than_model) {
            b.symbol("worse_than_model", worse)
                .context("worse_than_model")?;
        }
        b.column_i64("signal_id", e.signal_id)
            .context("signal_id")?;
        b.column_i64("seq", i64::from(e.seq)).context("seq")?;
        b.column_i64("recipe_version", i64::from(e.recipe_version))
            .context("recipe_version")?;
        b.column_i64("security_id", e.security_id)
            .context("security_id")?;
        b.column_i64("qty", e.qty).context("qty")?;
        b.column_i64("slot_paise", e.slot_paise)
            .context("slot_paise")?;
        let timestamps: [(&'static str, Option<i64>); 7] = [
            ("candle_start", e.candle_start_ist_nanos),
            ("candle_end", e.candle_end_ist_nanos),
            ("last_tick_exchange_ts", e.last_tick_exchange_ist_nanos),
            ("last_tick_receipt_ts", e.last_tick_receipt_ist_nanos),
            ("decision_ts", e.decision_ist_nanos),
            ("exchange_order_ts", e.exchange_order_ist_nanos),
            ("exchange_trade_ts", e.exchange_trade_ist_nanos),
        ];
        for (name, value) in timestamps {
            if let Some(nanos) = value {
                b.column_ts(name, TimestampNanos::new(nanos))
                    .context(name)?;
            }
        }
        let longs: [(&'static str, Option<i64>); 20] = [
            ("best_bid_paise", e.best_bid_paise),
            ("best_bid_qty", e.best_bid_qty),
            ("best_ask_paise", e.best_ask_paise),
            ("best_ask_qty", e.best_ask_qty),
            ("ltp_paise", e.ltp_paise),
            ("second_volume", e.second_volume),
            ("limit_paise", e.limit_paise),
            ("queue_wait_us", e.queue_wait_us),
            ("feed_lag_us", e.feed_lag_us),
            ("api_rtt_us", e.api_rtt_us),
            ("fill_qty", e.fill_qty),
            ("fill_price_paise", e.fill_price_paise),
            ("avg_price_paise", e.avg_price_paise),
            ("remainder_qty", e.remainder_qty),
            ("model_price_paise", e.model_price_paise),
            ("charges_paise", e.charges_paise),
            ("tick_paise", e.tick_paise),
            ("lot_size", e.lot_size),
            ("participation_bps", e.participation_bps.map(i64::from)),
            ("max_fill_seconds", e.max_fill_seconds.map(i64::from)),
        ];
        for (name, value) in longs {
            if let Some(v) = value {
                b.column_i64(name, v).context(name)?;
            }
        }
        if let Some(order_id) = e.order_id.as_deref() {
            let order_id = ilp_string_capped(order_id, LIVE_ORDER_EVENTS_ORDER_ID_MAX_CHARS);
            if !order_id.is_empty() {
                b.column_str("order_id", order_id.as_str())
                    .context("order_id")?;
            }
        }
        if let Some(reason) = e.reason.as_deref() {
            let reason = ilp_string_capped(reason, LIVE_ORDER_EVENTS_REASON_MAX_CHARS);
            if !reason.is_empty() {
                b.column_str("reason", reason.as_str()).context("reason")?;
            }
        }
        b.at(TimestampNanos::new(e.ts_ist_nanos))
            .context("designated timestamp")?;
        Ok(())
    }

    /// Flushes buffered rows over ILP-HTTP (per-flush server ACK). On ANY
    /// failed flush the pending rows go to the disk tier and the flush
    /// reports `Ok`; if the disk tier refuses them they are DISCARDED and
    /// counted, never silent.
    ///
    /// # Errors
    /// `Err` when the flush failed AND the disk tier refused the batch.
    pub fn flush(&mut self) -> Result<()> {
        if self.pending == 0 {
            return Ok(());
        }
        let flushed = self
            .sender
            .as_mut()
            .map(|sender| sender.flush(&mut self.buffer));
        match flushed {
            Some(Ok(())) => {
                self.pending = 0;
                self.committed_len = 0;
                Ok(())
            }
            Some(Err(err)) => match self.spill_or_discard() {
                Ok(_spilled) => Ok(()),
                Err(dropped) => Err(anyhow::Error::new(err).context(format!(
                    "live_order_events ILP flush failed and the disk tier refused the batch — \
                     {dropped} pending row(s) discarded"
                ))),
            },
            None => match self.spill_or_discard() {
                Ok(_spilled) => Ok(()),
                Err(dropped) => anyhow::bail!(
                    "live_order_events: no ILP sender (QuestDB unreachable) and the disk tier \
                     refused the batch — {dropped} pending row(s) discarded"
                ),
            },
        }
    }

    /// Drops every buffered-but-unflushed row. Returns the discarded row
    /// count; counted and logged so a discard is never silent.
    pub fn discard_pending(&mut self) -> usize {
        let dropped = self.pending;
        if dropped > 0 {
            metrics::counter!("tv_live_order_events_rows_discarded_total")
                .increment(dropped as u64);
            error!(
                code = ErrorCode::Audit06OrderWriteFailed.code_str(),
                stage = "flush",
                table = LIVE_ORDER_EVENTS_TABLE,
                rows = dropped,
                "AUDIT-06: live_order_events rows discarded — the flush failed and the \
                 disk tier did not take them"
            );
        }
        self.buffer.clear();
        self.pending = 0;
        self.committed_len = 0;
        dropped
    }

    /// Writes the pending whole rows to the disk tier and returns how many
    /// were spilled, or discards them (counted) and returns how many were
    /// dropped.
    fn spill_or_discard(&mut self) -> std::result::Result<usize, usize> {
        let rows = self.pending;
        let len = self.committed_len.min(self.buffer.len());
        let bytes = self.buffer.as_bytes().get(..len).unwrap_or_default();
        if spill_failed_batch(self.spill.as_ref(), bytes, rows) {
            self.buffer.clear();
            self.pending = 0;
            self.committed_len = 0;
            return Ok(rows);
        }
        Err(self.discard_pending())
    }

    /// Points this writer's disk tier at `dir` (tests).
    #[cfg(test)]
    // TEST-EXEMPT: test-only builder (cfg(test)); exercised by the four spill tests below (failed flush, refused spill, live writer)
    pub(crate) fn with_spill_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.spill = Some(AuditSpill::in_dir(SPILL_TABLE, dir));
        self
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// A row with every optional column set, so the ILP line carries all of them.
    fn full_event() -> LiveOrderEvent {
        LiveOrderEvent {
            ts_ist_nanos: 1_794_000_005_123_000_000,
            event: LiveOrderEventKind::Fill,
            signal_id: 42,
            seq: 6,
            feed: LIVE_ORDER_EVENTS_FEED_DHAN,
            mode: LiveOrderMode::Paper,
            recipe: "orb_5s".to_string(),
            recipe_version: 3,
            security_id: 2885,
            segment: ExchangeSegment::NseEquity,
            symbol: "RELIANCE".to_string(),
            side: LiveOrderSide::Buy,
            qty: 50,
            slot_paise: 10_000_000,
            candle_start_ist_nanos: Some(1_794_000_000_000_000_000),
            candle_end_ist_nanos: Some(1_794_000_005_000_000_000),
            last_tick_exchange_ist_nanos: Some(1_794_000_004_000_000_000),
            last_tick_receipt_ist_nanos: Some(1_794_000_004_750_000_000),
            close_by: Some(LiveCandleCloseBy::Clock),
            decision_ist_nanos: Some(1_794_000_005_001_000_000),
            best_bid_paise: Some(140_000),
            best_bid_qty: Some(120),
            best_ask_paise: Some(140_010),
            best_ask_qty: Some(80),
            ltp_paise: Some(140_005),
            second_volume: Some(3_400),
            limit_paise: Some(154_005),
            limit_bound: Some(LiveLimitBound::Band),
            queue_wait_us: Some(15),
            feed_lag_us: Some(-250),
            api_rtt_us: Some(3_500),
            order_id: Some("PAPER-7".to_string()),
            exchange_order_ist_nanos: Some(1_794_000_005_000_000_000),
            exchange_trade_ist_nanos: Some(1_794_000_006_000_000_000),
            fill_qty: Some(50),
            fill_price_paise: Some(140_020),
            avg_price_paise: Some(140_020),
            remainder_qty: Some(0),
            model_price_paise: Some(140_030),
            worse_than_model: Some(false),
            reason: Some("filled at second +1".to_string()),
            charges_paise: Some(2_150),
            tick_paise: Some(5),
            lot_size: Some(250),
            participation_bps: Some(2_500),
            max_fill_seconds: Some(60),
        }
    }

    /// A decision row: every optional column `None`.
    fn bare_event() -> LiveOrderEvent {
        LiveOrderEvent {
            event: LiveOrderEventKind::Decision,
            seq: 0,
            candle_start_ist_nanos: None,
            candle_end_ist_nanos: None,
            last_tick_exchange_ist_nanos: None,
            last_tick_receipt_ist_nanos: None,
            close_by: None,
            decision_ist_nanos: None,
            best_bid_paise: None,
            best_bid_qty: None,
            best_ask_paise: None,
            best_ask_qty: None,
            ltp_paise: None,
            second_volume: None,
            limit_paise: None,
            limit_bound: None,
            queue_wait_us: None,
            feed_lag_us: None,
            api_rtt_us: None,
            order_id: None,
            exchange_order_ist_nanos: None,
            exchange_trade_ist_nanos: None,
            fill_qty: None,
            fill_price_paise: None,
            avg_price_paise: None,
            remainder_qty: None,
            model_price_paise: None,
            worse_than_model: None,
            reason: None,
            charges_paise: None,
            tick_paise: None,
            lot_size: None,
            participation_bps: None,
            max_fill_seconds: None,
            ..full_event()
        }
    }

    /// Column (or tag) names in one ILP line: `table,tag=v,... f=v,... ts`.
    fn line_column_names(line: &str) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        let mut in_quotes = false;
        let mut escaped = false;
        let mut token = String::new();
        let mut first = true;
        // Split on unquoted, unescaped ',' and ' '.
        let mut tokens = Vec::new();
        for ch in line.trim_end().chars() {
            if escaped {
                token.push(ch);
                escaped = false;
                continue;
            }
            match ch {
                '\\' => {
                    token.push(ch);
                    escaped = true;
                }
                '"' => {
                    token.push(ch);
                    in_quotes = !in_quotes;
                }
                ',' | ' ' if !in_quotes => tokens.push(std::mem::take(&mut token)),
                _ => token.push(ch),
            }
        }
        tokens.push(token);
        for t in tokens {
            if first {
                first = false;
                continue;
            }
            if let Some((name, _)) = t.split_once('=') {
                names.insert(name.to_string());
            }
        }
        names
    }

    #[test]
    fn test_event_wire_strings_are_stable_and_distinct() {
        let expected = [
            "decision",
            "built",
            "sent",
            "ack",
            "exchange_order",
            "fill",
            "reject",
            "modify",
            "cancel",
            "exit",
            "stop",
            "square_off",
        ];
        let got: Vec<&str> = LiveOrderEventKind::ALL.iter().map(|k| k.as_str()).collect();
        assert_eq!(got, expected);
        let distinct: BTreeSet<&str> = got.iter().copied().collect();
        assert_eq!(distinct.len(), LiveOrderEventKind::ALL.len());
    }

    #[test]
    fn test_small_enum_wire_strings_are_stable() {
        assert_eq!(LiveOrderMode::Paper.as_str(), "paper");
        assert_eq!(LiveOrderMode::Live.as_str(), "live");
        assert_eq!(LiveOrderSide::Buy.as_str(), "BUY");
        assert_eq!(LiveOrderSide::Sell.as_str(), "SELL");
        assert_eq!(LiveCandleCloseBy::Clock.as_str(), "clock");
        assert_eq!(LiveCandleCloseBy::Tick.as_str(), "tick");
        assert_eq!(LiveLimitBound::Band.as_str(), "band");
        assert_eq!(LiveLimitBound::SlotCap.as_str(), "slot_cap");
        assert_eq!(LiveLimitBound::Circuit.as_str(), "circuit");
    }

    #[test]
    fn test_live_order_events_create_ddl_contains_every_column_and_the_dedup_key() {
        let ddl = live_order_events_create_ddl();
        assert!(ddl.contains("CREATE TABLE IF NOT EXISTS live_order_events"));
        assert!(ddl.contains("ts                    TIMESTAMP"));
        for (col, ty) in LIVE_ORDER_EVENTS_COLUMNS {
            let needle = format!(" {col} ");
            let at = ddl
                .find(&needle)
                .unwrap_or_else(|| panic!("DDL missing column {col}: {ddl}"));
            let rest = ddl[at + needle.len()..].trim_start();
            assert!(rest.starts_with(ty), "{col} must be {ty}: {rest:.40}");
        }
        assert!(ddl.contains("timestamp(ts) PARTITION BY DAY"));
        assert!(ddl.contains("DEDUP UPSERT KEYS(ts, signal_id, event, seq, feed)"));
    }

    #[test]
    fn test_dedup_key_ts_first_with_signal_event_seq_feed() {
        let tokens: Vec<&str> = DEDUP_KEY_LIVE_ORDER_EVENTS
            .split(',')
            .map(str::trim)
            .collect();
        assert_eq!(tokens, ["ts", "signal_id", "event", "seq", "feed"]);
        // Per-signal identity, not per-instrument (I-P1-11 not applicable).
        assert!(!DEDUP_KEY_LIVE_ORDER_EVENTS.contains("security_id"));
    }

    /// The CREATE body, the self-heal manifest and the ILP row must name the
    /// same columns: adding a column to one and not the others fails here.
    #[test]
    fn test_live_order_events_ensure_statements_manifest_ddl_and_ilp_row_match() {
        let manifest: BTreeSet<String> = LIVE_ORDER_EVENTS_COLUMNS
            .iter()
            .map(|(c, _)| (*c).to_string())
            .collect();
        assert_eq!(
            manifest.len(),
            LIVE_ORDER_EVENTS_COLUMNS.len(),
            "duplicates"
        );

        // CREATE body: every `name TYPE` pair after `ts`.
        let ddl = live_order_events_create_ddl();
        let body = &ddl[ddl.find('(').expect("open")..ddl.find(") timestamp(").expect("close")];
        let created: BTreeSet<String> = body
            .trim_start_matches('(')
            .split(',')
            .filter_map(|pair| pair.split_whitespace().next())
            .filter(|name| *name != "ts")
            .map(str::to_string)
            .collect();
        assert_eq!(created, manifest, "CREATE vs manifest");

        // ILP row with every optional column set.
        let mut w = LiveOrderEventsWriter::for_test();
        w.append(&full_event()).expect("append");
        let written = line_column_names(&w.buffer_utf8());
        assert_eq!(written, manifest, "ILP row vs manifest");

        // Self-heal statements: one ADD COLUMN per manifest column, same type.
        let statements = live_order_events_ensure_statements();
        assert_eq!(statements.len(), LIVE_ORDER_EVENTS_COLUMNS.len() + 2);
        for (col, ty) in LIVE_ORDER_EVENTS_COLUMNS {
            let alter =
                format!("ALTER TABLE live_order_events ADD COLUMN IF NOT EXISTS {col} {ty};");
            assert!(statements.contains(&alter), "missing self-heal for {col}");
        }
        assert!(statements[0].starts_with("CREATE TABLE IF NOT EXISTS"));
        assert_eq!(
            statements.last().map(String::as_str),
            Some(
                "ALTER TABLE live_order_events DEDUP ENABLE UPSERT KEYS(ts, signal_id, event, seq, feed);"
            )
        );
    }

    #[test]
    fn test_append_full_row_ilp_line_carries_every_value() {
        let mut w = LiveOrderEventsWriter::for_test();
        w.append(&full_event()).expect("append");
        assert_eq!(w.pending(), 1);
        let line = w.buffer_utf8();
        assert!(line.starts_with("live_order_events,event=fill,feed=dhan,mode=paper,"));
        for part in [
            ",recipe=orb_5s",
            ",segment=NSE_EQ",
            ",symbol=RELIANCE",
            ",side=BUY",
            ",close_by=clock",
            ",limit_bound=band",
            "signal_id=42i",
            "seq=6i",
            "recipe_version=3i",
            "security_id=2885i",
            "qty=50i",
            "slot_paise=10000000i",
            "candle_start=1794000000000000t",
            "candle_end=1794000005000000t",
            "last_tick_exchange_ts=1794000004000000t",
            "last_tick_receipt_ts=1794000004750000t",
            "decision_ts=1794000005001000t",
            "best_bid_paise=140000i",
            "best_bid_qty=120i",
            "best_ask_paise=140010i",
            "best_ask_qty=80i",
            "ltp_paise=140005i",
            "second_volume=3400i",
            "limit_paise=154005i",
            "queue_wait_us=15i",
            "feed_lag_us=-250i",
            "api_rtt_us=3500i",
            "order_id=\"PAPER-7\"",
            "exchange_order_ts=1794000005000000t",
            "exchange_trade_ts=1794000006000000t",
            "fill_qty=50i",
            "fill_price_paise=140020i",
            "avg_price_paise=140020i",
            "remainder_qty=0i",
            "model_price_paise=140030i",
            ",worse_than_model=not_worse",
            "reason=\"filled at second +1\"",
            "charges_paise=2150i",
            "tick_paise=5i",
            "lot_size=250i",
            "participation_bps=2500i",
            "max_fill_seconds=60i",
        ] {
            assert!(line.contains(part), "missing {part:?} in {line}");
        }
        assert!(line.trim_end().ends_with(" 1794000005123000000"), "{line}");
    }

    #[test]
    fn test_bare_row_omits_every_optional_column() {
        let mut w = LiveOrderEventsWriter::for_test();
        w.append(&bare_event()).expect("append");
        let written = line_column_names(&w.buffer_utf8());
        let required: BTreeSet<String> = [
            "event",
            "feed",
            "mode",
            "recipe",
            "segment",
            "symbol",
            "side",
            "signal_id",
            "seq",
            "recipe_version",
            "security_id",
            "qty",
            "slot_paise",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        assert_eq!(written, required);
    }

    /// Money is integer paise, so NaN cannot be represented; every negative
    /// money, quantity, identity or timestamp column is refused before the
    /// buffer, which stays empty.
    #[test]
    fn test_validate_live_order_event_refuses_negative_money_quantity_and_timestamps() {
        type Setter = fn(&mut LiveOrderEvent);
        let cases: [(&str, Setter); 26] = [
            ("ts", |e| e.ts_ist_nanos = 0),
            ("signal_id", |e| e.signal_id = -1),
            ("seq", |e| e.seq = -1),
            ("recipe_version", |e| e.recipe_version = -1),
            ("qty", |e| e.qty = -1),
            ("slot_paise", |e| e.slot_paise = -1),
            ("candle_start", |e| e.candle_start_ist_nanos = Some(-1)),
            ("candle_end", |e| e.candle_end_ist_nanos = Some(-1)),
            ("last_tick_exchange_ts", |e| {
                e.last_tick_exchange_ist_nanos = Some(-1)
            }),
            ("last_tick_receipt_ts", |e| {
                e.last_tick_receipt_ist_nanos = Some(-1)
            }),
            ("decision_ts", |e| e.decision_ist_nanos = Some(-1)),
            ("best_bid_paise", |e| e.best_bid_paise = Some(-1)),
            ("best_bid_qty", |e| e.best_bid_qty = Some(-1)),
            ("best_ask_paise", |e| e.best_ask_paise = Some(i64::MIN)),
            ("best_ask_qty", |e| e.best_ask_qty = Some(-1)),
            ("ltp_paise", |e| e.ltp_paise = Some(-1)),
            ("second_volume", |e| e.second_volume = Some(-1)),
            ("limit_paise", |e| e.limit_paise = Some(-1)),
            ("exchange_order_ts", |e| {
                e.exchange_order_ist_nanos = Some(-1)
            }),
            ("exchange_trade_ts", |e| {
                e.exchange_trade_ist_nanos = Some(-1)
            }),
            ("fill_qty", |e| e.fill_qty = Some(-1)),
            ("fill_price_paise", |e| e.fill_price_paise = Some(-1)),
            ("avg_price_paise", |e| e.avg_price_paise = Some(-1)),
            ("remainder_qty", |e| e.remainder_qty = Some(-1)),
            ("model_price_paise", |e| e.model_price_paise = Some(-1)),
            ("charges_paise", |e| e.charges_paise = Some(-1)),
        ];
        for (column, set) in cases {
            let mut e = full_event();
            set(&mut e);
            assert_eq!(
                validate_live_order_event(&e),
                Err(LiveOrderEventRefusal { column }),
                "{column}"
            );
            let mut w = LiveOrderEventsWriter::for_test();
            let err = w.append(&e).expect_err("refused");
            assert!(err.to_string().contains(column), "{err}");
            assert_eq!(w.pending(), 0);
            assert!(
                w.buffer_utf8().is_empty(),
                "{column}: buffer must stay empty"
            );
        }
    }

    /// Boundaries: zero and i64::MAX money and quantities pass; latencies may
    /// be negative (exchange time ahead of receipt) down to `i64::MIN + 1`;
    /// `i64::MIN` itself is QuestDB's LONG NULL and is refused.
    #[test]
    fn test_zero_max_and_negative_latency_boundaries_are_accepted() {
        let mut e = full_event();
        e.qty = 0;
        e.slot_paise = i64::MAX;
        e.fill_price_paise = Some(0);
        e.charges_paise = Some(i64::MAX);
        e.feed_lag_us = Some(i64::MIN + 1);
        e.queue_wait_us = Some(i64::MIN + 1);
        e.api_rtt_us = Some(-1);
        e.security_id = i64::MIN + 1;
        e.signal_id = i64::MAX;
        e.seq = i32::MAX;
        assert_eq!(validate_live_order_event(&e), Ok(()));
        let mut w = LiveOrderEventsWriter::for_test();
        w.append(&e).expect("append");
        let line = w.buffer_utf8();
        assert!(
            line.contains(&format!("slot_paise={}i", i64::MAX)),
            "{line}"
        );
        assert!(
            line.contains(&format!("feed_lag_us={}i", i64::MIN + 1)),
            "{line}"
        );
        assert!(line.contains(&format!("seq={}i", i32::MAX)), "{line}");
        assert!(line.contains("qty=0i"), "{line}");
    }

    /// `i64::MIN` in a LONG and `i32::MIN` in an INT are QuestDB NULL: they
    /// are refused before the buffer, named by column, never written.
    #[test]
    fn test_long_and_int_null_sentinels_are_refused() {
        type Setter = fn(&mut LiveOrderEvent);
        let cases: [(&str, Setter); 6] = [
            ("security_id", |e| e.security_id = i64::MIN),
            ("queue_wait_us", |e| e.queue_wait_us = Some(i64::MIN)),
            ("feed_lag_us", |e| e.feed_lag_us = Some(i64::MIN)),
            ("api_rtt_us", |e| e.api_rtt_us = Some(i64::MIN)),
            ("seq", |e| e.seq = i32::MIN),
            ("recipe_version", |e| e.recipe_version = i32::MIN),
        ];
        for (column, set) in cases {
            let mut e = full_event();
            set(&mut e);
            assert_eq!(
                validate_live_order_event(&e),
                Err(LiveOrderEventRefusal { column }),
                "{column}"
            );
            let mut w = LiveOrderEventsWriter::for_test();
            let err = w.append(&e).expect_err("refused");
            assert!(err.to_string().contains(column), "{err}");
            assert_eq!(w.pending(), 0);
            assert!(
                w.buffer_utf8().is_empty(),
                "{column}: buffer must stay empty"
            );
        }
        // Every LONG/INT column in the manifest is covered by a refusal of
        // its NULL sentinel: the sign checks for the others (validated by the
        // negative-value test), this test for the rest.
        let covered: BTreeSet<&str> = cases.iter().map(|(c, _)| *c).collect();
        let sign_checked: BTreeSet<&str> = [
            "signal_id",
            "qty",
            "slot_paise",
            "best_bid_paise",
            "best_bid_qty",
            "best_ask_paise",
            "best_ask_qty",
            "ltp_paise",
            "second_volume",
            "limit_paise",
            "fill_qty",
            "fill_price_paise",
            "avg_price_paise",
            "remainder_qty",
            "model_price_paise",
            "charges_paise",
            "tick_paise",
            "lot_size",
            "participation_bps",
            "max_fill_seconds",
        ]
        .into_iter()
        .collect();
        for (col, ty) in LIVE_ORDER_EVENTS_COLUMNS {
            if *ty == "LONG" || *ty == "INT" {
                assert!(
                    covered.contains(col) || sign_checked.contains(col),
                    "{col} ({ty}) has no NULL-sentinel refusal"
                );
            }
        }
    }

    /// `worse_than_model` is a SYMBOL so an unknown verdict reads as NULL:
    /// Some(true) → `worse`, Some(false) → `not_worse`, None → no column.
    #[test]
    fn test_worse_than_model_wire_form_keeps_unknown_as_null() {
        assert_eq!(worse_than_model_wire(Some(true)), Some("worse"));
        assert_eq!(worse_than_model_wire(Some(false)), Some("not_worse"));
        assert_eq!(worse_than_model_wire(None), None);
        assert!(
            LIVE_ORDER_EVENTS_COLUMNS.contains(&("worse_than_model", "SYMBOL")),
            "worse_than_model must be a SYMBOL (a BOOLEAN cannot be NULL)"
        );
        assert!(
            live_order_events_create_ddl().contains("worse_than_model      SYMBOL"),
            "CREATE must declare worse_than_model SYMBOL"
        );
        let wire = |worse: Option<bool>| {
            let mut e = full_event();
            e.worse_than_model = worse;
            let mut w = LiveOrderEventsWriter::for_test();
            w.append(&e).expect("append");
            w.buffer_utf8()
        };
        let worse = wire(Some(true));
        assert!(worse.contains(",worse_than_model=worse"), "{worse}");
        let not_worse = wire(Some(false));
        assert!(
            not_worse.contains(",worse_than_model=not_worse"),
            "{not_worse}"
        );
        let unknown = wire(None);
        assert!(!unknown.contains("worse_than_model"), "{unknown}");
    }

    /// ILP text is not SQL: `'` and `--` arrive unchanged (only the questdb
    /// buffer escapes ILP-special characters), and `\n` / `\r` are stripped
    /// so they cannot start a second line.
    #[test]
    fn test_ilp_text_keeps_apostrophe_and_double_dash_and_strips_line_breaks() {
        let mut e = full_event();
        e.recipe = "can't".to_string();
        e.symbol = "a--b".to_string();
        e.order_id = Some("can't--1".to_string());
        e.reason = Some("can't a--b\nlive_order_events,event=fill qty=9i 1\r".to_string());
        let mut w = LiveOrderEventsWriter::for_test();
        w.append(&e).expect("append");
        let line = w.buffer_utf8();
        assert!(line.contains(",recipe=can't"), "{line}");
        assert!(line.contains(",symbol=a--b"), "{line}");
        assert!(line.contains("order_id=\"can't--1\""), "{line}");
        assert!(
            line.contains("reason=\"can't a--blive_order_events,event=fill qty=9i 1\""),
            "{line}"
        );
        assert_eq!(line.matches('\n').count(), 1, "one ILP line: {line}");
        assert!(!line.contains('\r'), "{line}");
    }

    /// Hostile symbol and string values cannot inject a tag, a field or a
    /// second line: control characters are stripped, the SYMBOL delimiters
    /// `,` `=` removed, and ILP separators escaped by the buffer. SQL text in
    /// a STRING is just text on the ILP path (no SQL is ever built from it).
    #[test]
    fn test_symbol_and_string_values_are_escaped_and_injection_safe() {
        let mut e = full_event();
        e.recipe = "orb,evil=1 x=2i\nlive_order_events,event=fill qty=999i 1".to_string();
        e.symbol = "A B=C,D\\".to_string();
        e.order_id = Some("ID\"; DROP TABLE live_order_events; --".to_string());
        e.reason = Some("line1\nline2\r\"quoted\" \\ back".to_string());
        let mut w = LiveOrderEventsWriter::for_test();
        w.append(&e).expect("append");
        let text = w.buffer_utf8();
        assert_eq!(
            text.matches('\n').count(),
            1,
            "exactly one ILP line: {text}"
        );
        let names = line_column_names(&text);
        let manifest: BTreeSet<String> = LIVE_ORDER_EVENTS_COLUMNS
            .iter()
            .map(|(c, _)| (*c).to_string())
            .collect();
        assert_eq!(names, manifest, "no injected tag or field: {text}");
        assert!(
            text.contains(r#"order_id="ID\"; DROP TABLE live_order_events; --""#),
            "the quote is escaped and the text stays inside the STRING: {text}"
        );
        assert!(text.contains(r"symbol=A\ BCD\\"), "{text}");
    }

    #[test]
    fn test_empty_symbols_map_to_na_and_empty_strings_are_omitted() {
        let mut e = full_event();
        e.recipe = String::new();
        e.symbol = "\n\t".to_string();
        e.feed = "";
        e.order_id = Some(String::new());
        e.reason = Some("\n".to_string());
        let mut w = LiveOrderEventsWriter::for_test();
        w.append(&e).expect("append");
        let line = w.buffer_utf8();
        assert!(line.contains(",recipe=n/a"), "{line}");
        assert!(line.contains(",symbol=n/a"), "{line}");
        assert!(line.contains(",feed=n/a"), "{line}");
        assert!(!line.contains("order_id="), "{line}");
        assert!(!line.contains("reason="), "{line}");
    }

    #[test]
    fn test_reason_and_order_id_are_truncated() {
        let mut e = full_event();
        e.reason = Some("r".repeat(1_000));
        e.order_id = Some("o".repeat(1_000));
        let mut w = LiveOrderEventsWriter::for_test();
        w.append(&e).expect("append");
        let line = w.buffer_utf8();
        let reason = line.split("reason=\"").nth(1).expect("reason");
        assert_eq!(
            reason.chars().take_while(|c| *c == 'r').count(),
            LIVE_ORDER_EVENTS_REASON_MAX_CHARS
        );
        let order_id = line.split("order_id=\"").nth(1).expect("order_id");
        assert_eq!(
            order_id.chars().take_while(|c| *c == 'o').count(),
            LIVE_ORDER_EVENTS_ORDER_ID_MAX_CHARS
        );
    }

    #[test]
    fn test_several_rows_append_in_order() {
        let mut w = LiveOrderEventsWriter::for_test();
        for (seq, kind) in LiveOrderEventKind::ALL.iter().enumerate() {
            let mut e = full_event();
            e.event = *kind;
            e.seq = i32::try_from(seq).expect("seq");
            w.append(&e).expect("append");
        }
        assert_eq!(w.pending(), LiveOrderEventKind::ALL.len());
        let text = w.buffer_utf8();
        let events: Vec<&str> = text
            .lines()
            .map(|l| {
                l.split(',')
                    .find_map(|p| p.strip_prefix("event="))
                    .expect("event tag")
            })
            .collect();
        let expected: Vec<&str> = LiveOrderEventKind::ALL.iter().map(|k| k.as_str()).collect();
        assert_eq!(events, expected);
    }

    #[test]
    fn test_flush_without_sender_or_spill_errors_via_discard_pending() {
        let mut w = LiveOrderEventsWriter::for_test();
        assert!(w.flush().is_ok(), "an empty flush is a no-op");
        w.append(&full_event()).expect("append");
        let err = w.flush().expect_err("no sender and no disk tier");
        assert!(err.to_string().contains("discarded"), "{err}");
        assert_eq!(w.pending(), 0);
        assert!(w.buffer_utf8().is_empty());
        assert_eq!(w.discard_pending(), 0, "a second discard drops nothing");
    }

    #[test]
    fn test_failed_flush_spills_the_exact_bytes_and_reports_ok() {
        let dir = crate::audit_spill::test_support::TestDir::new();
        let mut w = LiveOrderEventsWriter::for_test().with_spill_dir(dir.path().to_path_buf());
        w.append(&full_event()).expect("append");
        w.append(&bare_event()).expect("append");
        let expected = w.buffer.as_bytes().to_vec();
        w.flush().expect("a spilled batch is not an error");
        assert_eq!(w.pending(), 0);
        assert!(w.buffer_utf8().is_empty());
        let files = crate::tick_spill_replay::list_spill_files(dir.path());
        assert_eq!(files.len(), 1);
        assert_eq!(std::fs::read(&files[0]).expect("read"), expected);
        assert_eq!(
            crate::audit_spill::rows_in_spill_file_name(&files[0]),
            Some(2)
        );
    }

    #[test]
    fn test_spill_leaves_out_a_half_appended_row() {
        let dir = crate::audit_spill::test_support::TestDir::new();
        let mut w = LiveOrderEventsWriter::for_test().with_spill_dir(dir.path().to_path_buf());
        w.append(&full_event()).expect("append");
        let whole = w.buffer.as_bytes().to_vec();
        let _ = w.buffer.table(LIVE_ORDER_EVENTS_TABLE);
        assert!(w.buffer.as_bytes().len() > whole.len());
        w.flush().expect("spilled");
        let files = crate::tick_spill_replay::list_spill_files(dir.path());
        assert_eq!(std::fs::read(&files[0]).expect("read"), whole);
    }

    #[test]
    fn test_refused_spill_discards_and_errors() {
        let dir = crate::audit_spill::test_support::TestDir::new();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, b"x").expect("write");
        let mut w = LiveOrderEventsWriter::for_test().with_spill_dir(blocker);
        w.append(&full_event()).expect("append");
        let err = w.flush().expect_err("a refused spill is a loss");
        assert!(err.to_string().contains("discarded"), "{err}");
        assert_eq!(w.pending(), 0);
    }

    #[test]
    fn test_writer_uses_ilp_http_conf_with_bounded_knobs() {
        let cfg = QuestDbConfig {
            host: "tv-questdb".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        assert_eq!(
            live_order_events_ilp_http_conf(&cfg),
            "http::addr=tv-questdb:9000;protocol_version=1;retry_timeout=0;request_timeout=5000;"
        );
    }

    #[test]
    fn test_writer_new_spills_to_the_production_dir() {
        let cfg = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        let w = LiveOrderEventsWriter::new(&cfg);
        assert_eq!(
            w.spill.as_ref().map(|s| s.dir().to_path_buf()),
            Some(std::path::PathBuf::from(
                "data/spill/audit/live_order_events"
            ))
        );
        assert!(LiveOrderEventsWriter::for_test().spill.is_none());
    }

    // Mock QuestDB /exec HTTP server (the order_audit pattern).

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

    #[tokio::test]
    async fn test_ensure_live_order_events_table_mock_200_reads_keyed() {
        let port = spawn_mock_http(MOCK_HTTP_200).await;
        assert!(ensure_live_order_events_table(&mock_cfg(port)).await);
    }

    #[tokio::test]
    async fn test_ensure_live_order_events_table_mock_500_degrades_without_panic() {
        let port = spawn_mock_http(MOCK_HTTP_500).await;
        assert!(!ensure_live_order_events_table(&mock_cfg(port)).await);
    }

    #[tokio::test]
    async fn test_ensure_live_order_events_table_unreachable_degrades_without_panic() {
        assert!(!ensure_live_order_events_table(&mock_cfg(1)).await);
    }

    #[tokio::test]
    async fn test_live_order_events_writer_new_is_lazy_and_buffers_without_network() {
        let mut w = LiveOrderEventsWriter::new(&mock_cfg(1));
        assert_eq!(w.pending(), 0);
        w.append(&full_event()).expect("append without network");
        assert_eq!(w.pending(), 1);
    }

    /// A real server reject through a live HTTP sender goes to the disk tier.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_server_reject_spills_instead_of_discarding() {
        let dir = crate::audit_spill::test_support::TestDir::new();
        let port = spawn_mock_http(MOCK_HTTP_500).await;
        let mut w =
            LiveOrderEventsWriter::new(&mock_cfg(port)).with_spill_dir(dir.path().to_path_buf());
        w.append(&full_event()).expect("append");
        w.flush().expect("a spilled batch is not an error");
        assert_eq!(w.pending(), 0);
        assert_eq!(
            crate::tick_spill_replay::list_spill_files(dir.path()).len(),
            1
        );
    }

    /// The same reject with no disk tier hits the `Some(Err)` discard arm.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_server_reject_without_disk_tier_discards_and_errors() {
        let port = spawn_mock_http(MOCK_HTTP_500).await;
        let mut w = LiveOrderEventsWriter::new(&mock_cfg(port));
        w.spill = None;
        w.append(&full_event()).expect("append");
        let err = w.flush().expect_err("server 500 with no disk tier");
        assert!(format!("{err:#}").contains("discarded"), "{err:#}");
        assert_eq!(w.pending(), 0);
    }
}

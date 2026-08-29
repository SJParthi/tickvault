//! Per-CONNECTION daily WebSocket rollup (`ws_connection_daily`).
//!
//! ## Why this table exists
//!
//! Operator directive 2026-08-29, verbatim (typos preserved): *"see how will
//! you always montiro in a day si there a wesbcoekt dsiconnect or websocket
//! reocnenct happened for all the conecntons dude ... based on evry day we
//! ened to cpature this rpeicsley rigth dude"*, with the standing constraint
//! *"always ensure to achieve O(1) always"*.
//!
//! Two tables already carry per-connection truth and neither answers that
//! question in one lookup:
//!
//! | Table | Grain | Answers "did connection 7 drop today?" |
//! |---|---|---|
//! | `ws_event_audit` | one row per lifecycle EVENT, per connection | only by scanning a day of rows and grouping |
//! | `feed_episode_audit` | one row per classified EPISODE, per connection | same — and only for connections that had an incident |
//! | `feed_scoreboard_daily` | one row per FEED per day | **no** — it sums all 16 connections into one number |
//!
//! So the day's verdict existed per feed, and the per-connection detail
//! existed only as raw events. This table is the missing join: **one row per
//! `(trading_date_ist, feed, ws_type, connection_index)` per day**, so the
//! question is a single keyed row read — O(1) — instead of a day-scan.
//!
//! ## The zero-row rule (audit Rule 11 — no false OK)
//!
//! A connection that ran all day without incident MUST still get a row, with
//! explicit zeros and `clean_day = true`. Without it, "no row" would be
//! ambiguous between *this connection was healthy*, *this connection never
//! started*, and *the rollup did not run* — the absent-series failure this
//! repository has already been bitten by (`tv_depth_rows_spilled_total`,
//! 2026-08-28: an absent reading looked exactly like a healthy zero).
//! `saw_any_event` separates "healthy" from "never appeared": a connection
//! that never even connected has `saw_any_event = false`, and that is a
//! finding, not a clean day.
//!
//! ## Table
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS ws_connection_daily (
//!     ts                    TIMESTAMP,  -- deterministic IST-midnight daily ts
//!     trading_date_ist      TIMESTAMP,  -- the trading day (IST midnight)
//!     feed                  SYMBOL,     -- dhan / truedata / ...
//!     ws_type               SYMBOL,     -- main_feed / depth_20 / depth_200 / order_update
//!     connection_index      LONG,       -- 0..pool_size-1
//!     pool_size             LONG,       -- configured conns of this ws_type
//!     connects              LONG,       -- 'connected' events
//!     reconnects            LONG,       -- 'reconnected' events
//!     disconnect_events     LONG,       -- raw 'disconnected' events
//!     disconnects_market    LONG,       -- classified episodes, inside session
//!     disconnects_off_hours LONG,       -- classified episodes, outside session
//!     stalls                LONG,       -- stall / never-streamed restarts
//!     restarts              LONG,       -- in-session process deaths
//!     blame_broker          LONG,
//!     blame_ours            LONG,
//!     blame_indeterminate   LONG,
//!     total_down_secs       LONG,       -- summed reconnect downtime
//!     max_down_secs         LONG,       -- worst single outage
//!     total_attempts        LONG,       -- summed reconnect attempts
//!     max_attempts          LONG,       -- worst single reconnect ladder
//!     saw_any_event         BOOLEAN,    -- false = never appeared at all
//!     clean_day             BOOLEAN     -- true = appeared AND zero incidents
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, trading_date_ist, feed, ws_type, connection_index);
//! ```
//!
//! The DEDUP key carries the designated timestamp first (2026-04-28
//! regression rule), `feed` (operator override 2026-06-28: feed in every
//! persisted key), and the `(ws_type, connection_index)` composite that is
//! the I-P1-11 uniqueness discipline extended to WebSocket streams. `ts` is
//! DETERMINISTIC per day, so a re-run of the daily job UPSERTs its rows in
//! place instead of duplicating them.
//!
//! Per-connection-per-day, not per-instrument, so I-P1-11's
//! `(security_id, exchange_segment)` pair is N/A here.

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::config::QuestDbConfig;

/// QuestDB table name. One row per connection per day.
pub const WS_CONNECTION_DAILY_TABLE: &str = "ws_connection_daily";

/// DEDUP UPSERT key — designated `ts` first, then the day, the feed, and the
/// `(ws_type, connection_index)` composite connection identity.
pub const DEDUP_KEY_WS_CONNECTION_DAILY: &str =
    "ts, trading_date_ist, feed, ws_type, connection_index";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// One connection's whole day, ready for ILP write.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WsConnectionDailyRow {
    /// Deterministic daily timestamp — IST midnight nanoseconds. Equal to
    /// `trading_date_ist_nanos`; both are carried because the designated
    /// timestamp must be in the DEDUP key and the day column is what every
    /// operator query filters on.
    pub ts_ist_nanos: i64,
    /// The trading day — IST midnight nanoseconds.
    pub trading_date_ist_nanos: i64,
    /// Feed wire label (`"dhan"` / ...).
    pub feed: String,
    /// WS-type wire label (`"main_feed"` / `"depth_20"` / ...).
    pub ws_type: String,
    /// 0-based connection index within its ws_type pool.
    pub connection_index: i64,
    /// Configured pool size for this ws_type on the day.
    pub pool_size: i64,
    /// `connected` events observed for this connection.
    pub connects: i64,
    /// `reconnected` events observed.
    pub reconnects: i64,
    /// Raw `disconnected` events observed (unclassified count).
    pub disconnect_events: i64,
    /// Classified disconnect episodes inside the session.
    pub disconnects_market: i64,
    /// Classified disconnect episodes outside the session.
    pub disconnects_off_hours: i64,
    /// Stall / never-streamed restarts.
    pub stalls: i64,
    /// In-session process deaths attributed to this connection.
    pub restarts: i64,
    /// Headline episodes blamed on the broker.
    pub blame_broker: i64,
    /// Headline episodes blamed on us.
    pub blame_ours: i64,
    /// Headline episodes with no determinable blame.
    pub blame_indeterminate: i64,
    /// Summed reconnect downtime in seconds.
    pub total_down_secs: i64,
    /// Worst single outage in seconds.
    pub max_down_secs: i64,
    /// Summed reconnect attempts.
    pub total_attempts: i64,
    /// Worst single reconnect ladder length.
    pub max_attempts: i64,
    /// `false` = this connection produced NO lifecycle event all day. That is
    /// a finding (it never appeared), never a clean day.
    pub saw_any_event: bool,
}

impl WsConnectionDailyRow {
    /// `true` when the connection appeared AND recorded zero incidents of any
    /// kind. Derived, never stored twice: a caller cannot set a `clean_day`
    /// that disagrees with the counts beside it.
    ///
    /// A connection that never appeared is deliberately NOT clean — absence
    /// is the one reading that must never render as health.
    #[must_use]
    pub fn clean_day(&self) -> bool {
        self.saw_any_event
            && self.disconnect_events == 0
            && self.disconnects_market == 0
            && self.disconnects_off_hours == 0
            && self.stalls == 0
            && self.restarts == 0
            && self.reconnects == 0
    }

    /// Total incidents of every kind — the one number an operator scans a
    /// column of. Saturating: a corrupt read can never wrap into a small
    /// reassuring value.
    #[must_use]
    pub fn incident_total(&self) -> i64 {
        self.disconnects_market
            .saturating_add(self.disconnects_off_hours)
            .saturating_add(self.stalls)
            .saturating_add(self.restarts)
    }
}

/// Every column, in DDL order, as `(name, questdb_type)`.
///
/// SINGLE SOURCE OF TRUTH: both the `CREATE TABLE` statement and the
/// per-column `ADD COLUMN IF NOT EXISTS` self-heal are generated from this
/// one list, so a column added here reaches BOTH a fresh table and one that
/// already exists. Hand-writing the two lists separately is how a column
/// silently never arrives on an existing table.
pub const WS_CONNECTION_DAILY_COLUMNS: &[(&str, &str)] = &[
    ("ts", "TIMESTAMP"),
    ("trading_date_ist", "TIMESTAMP"),
    ("feed", "SYMBOL"),
    ("ws_type", "SYMBOL"),
    ("connection_index", "LONG"),
    ("pool_size", "LONG"),
    ("connects", "LONG"),
    ("reconnects", "LONG"),
    ("disconnect_events", "LONG"),
    ("disconnects_market", "LONG"),
    ("disconnects_off_hours", "LONG"),
    ("stalls", "LONG"),
    ("restarts", "LONG"),
    ("blame_broker", "LONG"),
    ("blame_ours", "LONG"),
    ("blame_indeterminate", "LONG"),
    ("total_down_secs", "LONG"),
    ("max_down_secs", "LONG"),
    ("total_attempts", "LONG"),
    ("max_attempts", "LONG"),
    ("saw_any_event", "BOOLEAN"),
    ("clean_day", "BOOLEAN"),
];

/// The idempotent `CREATE TABLE` DDL, generated from
/// [`WS_CONNECTION_DAILY_COLUMNS`]. Pure (testable without QuestDB).
#[must_use]
pub fn ws_connection_daily_create_ddl() -> String {
    let cols = WS_CONNECTION_DAILY_COLUMNS
        .iter()
        .map(|(name, ty)| format!("{name} {ty}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "CREATE TABLE IF NOT EXISTS {WS_CONNECTION_DAILY_TABLE} ({cols}) \
         timestamp(ts) PARTITION BY DAY \
         DEDUP UPSERT KEYS({DEDUP_KEY_WS_CONNECTION_DAILY});"
    )
}

/// The full ordered DDL statement list: `CREATE` → per-column
/// `ALTER ADD COLUMN IF NOT EXISTS` → `DEDUP ENABLE`. Never a drop. Pure, so
/// the shape is unit-testable without a live QuestDB.
///
/// The per-column ALTERs are what make the schema self-healing: a table
/// created by an OLDER build (or auto-created by a first ILP write without
/// DEDUP) gains every missing column and the dedup key on the next boot.
#[must_use]
pub fn ws_connection_daily_ddl_statements() -> Vec<String> {
    let mut statements = vec![ws_connection_daily_create_ddl()];
    for (col, ty) in WS_CONNECTION_DAILY_COLUMNS {
        // The designated timestamp cannot be added after the fact; it exists
        // by construction on any table this code created.
        if *col == "ts" {
            continue;
        }
        statements.push(format!(
            "ALTER TABLE {WS_CONNECTION_DAILY_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty};"
        ));
    }
    statements.push(format!(
        "ALTER TABLE {WS_CONNECTION_DAILY_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_WS_CONNECTION_DAILY});"
    ));
    statements
}

/// Create the rollup table if absent (idempotent, schema-self-heal pattern).
///
/// Failures log at `error!` but never block the caller: the per-connection
/// truth still exists in `ws_event_audit` and `feed_episode_audit`, which is
/// what this table summarises. NOTE the HTTP-CLIENT-01-class consequence — a
/// failed ensure leaves the table to be auto-created by the first ILP write
/// WITHOUT DEDUP UPSERT KEYS, i.e. a duplicate-row window until a later
/// boot's ensure succeeds.
// TEST-EXEMPT: live-QuestDB DDL runner (DDL string unit-tested via ws_connection_daily_create_ddl tests)
pub async fn ensure_ws_connection_daily_table(questdb_config: &QuestDbConfig) {
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
            error!(
                code = "SCOREBOARD-01",
                stage = "ensure_client_build",
                ?err,
                "SCOREBOARD-01: HTTP client build failed — ws_connection_daily \
                 not ensured (first ILP write may auto-create it WITHOUT dedup \
                 — duplicate-row window until the next successful boot)"
            );
            return;
        }
    };
    let statements = ws_connection_daily_ddl_statements();
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
                error!(
                    code = "SCOREBOARD-01",
                    stage = "ensure_ddl",
                    %status,
                    ddl = ddl.as_str(),
                    body = %body.chars().take(200).collect::<String>(),
                    "SCOREBOARD-01: ws_connection_daily DDL returned non-2xx \
                     (dedup may be missing — duplicate-row window until a later \
                     ensure succeeds)"
                );
            }
            Err(err) => error!(
                code = "SCOREBOARD-01",
                stage = "ensure_ddl",
                ?err,
                ddl = ddl.as_str(),
                "SCOREBOARD-01: ws_connection_daily DDL request failed"
            ),
        }
    }
}

/// Builds the ILP-over-HTTP conf string. HTTP (NOT ILP TCP) so every flush
/// gets a per-request server ACK — a server-side reject (schema drift, DEDUP
/// violation) surfaces as `Err` instead of a silently empty table (the
/// 2026-07-05 `ws_event_audit` lesson).
fn ws_connection_daily_ilp_http_conf(config: &QuestDbConfig) -> String {
    format!("http::addr={}:{};", config.host, config.http_port)
}

/// Lazy ILP-over-HTTP writer for `ws_connection_daily`. Mirrors
/// `FeedEpisodeAuditWriter`: unreachable QuestDB at construction still builds
/// (`sender = None`); `append_row` fills the local buffer and `flush` returns
/// `Err` until QuestDB is reachable.
pub struct WsConnectionDailyWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl WsConnectionDailyWriter {
    /// Production constructor — ILP-over-HTTP sender, lazy on failure
    /// (`http::` does not dial at construction; failures surface at flush).
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (needs live QuestDB); append/flush paths covered via for_test()
    pub fn new(config: &QuestDbConfig) -> Self {
        // Seeded here so an absent discard series can never read as a healthy
        // zero — see `ilp_overflow::register_overflow_baseline`.
        crate::ilp_overflow::register_overflow_baseline("ws_connection_daily");
        let conf = ws_connection_daily_ilp_http_conf(config);
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
                    "ws_connection_daily writer: QuestDB unreachable — buffering locally"
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
    // TEST-EXEMPT: test-only helper used by append/flush unit tests below.
    pub fn for_test() -> Self {
        Self {
            sender: None,
            buffer: Buffer::new(ProtocolVersion::V1),
            pending: 0,
        }
    }

    /// Rows appended but not yet flushed.
    #[must_use]
    // TEST-EXEMPT: observability accessor, exercised by append tests below.
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// Test-only view of the ILP buffer bytes (shape assertions).
    #[cfg(test)]
    fn buffer_utf8(&self) -> String {
        String::from_utf8(self.buffer.as_bytes().to_vec()).unwrap_or_default()
    }

    /// Appends one connection-day row (cold path — at most one row per
    /// connection per day, so 16 rows at the authorized socket budget).
    ///
    /// `clean_day` is DERIVED from the counts at the write boundary rather
    /// than taken from the caller, so a persisted row can never claim a clean
    /// day while carrying a non-zero incident count beside it.
    ///
    /// # Errors
    /// Propagates ILP buffer errors (table/column append failure).
    pub fn append_row(&mut self, r: &WsConnectionDailyRow) -> Result<()> {
        self.buffer
            .table(WS_CONNECTION_DAILY_TABLE)
            .context("table")?
            // Symbols BEFORE columns (ILP tags-before-fields rule).
            .symbol("feed", r.feed.as_str())
            .context("feed")?
            .symbol("ws_type", r.ws_type.as_str())
            .context("ws_type")?
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(r.trading_date_ist_nanos),
            )
            .context("trading_date_ist")?
            .column_i64("connection_index", r.connection_index)
            .context("connection_index")?
            .column_i64("pool_size", r.pool_size)
            .context("pool_size")?
            .column_i64("connects", r.connects)
            .context("connects")?
            .column_i64("reconnects", r.reconnects)
            .context("reconnects")?
            .column_i64("disconnect_events", r.disconnect_events)
            .context("disconnect_events")?
            .column_i64("disconnects_market", r.disconnects_market)
            .context("disconnects_market")?
            .column_i64("disconnects_off_hours", r.disconnects_off_hours)
            .context("disconnects_off_hours")?
            .column_i64("stalls", r.stalls)
            .context("stalls")?
            .column_i64("restarts", r.restarts)
            .context("restarts")?
            .column_i64("blame_broker", r.blame_broker)
            .context("blame_broker")?
            .column_i64("blame_ours", r.blame_ours)
            .context("blame_ours")?
            .column_i64("blame_indeterminate", r.blame_indeterminate)
            .context("blame_indeterminate")?
            .column_i64("total_down_secs", r.total_down_secs)
            .context("total_down_secs")?
            .column_i64("max_down_secs", r.max_down_secs)
            .context("max_down_secs")?
            .column_i64("total_attempts", r.total_attempts)
            .context("total_attempts")?
            .column_i64("max_attempts", r.max_attempts)
            .context("max_attempts")?
            .column_bool("saw_any_event", r.saw_any_event)
            .context("saw_any_event")?
            .column_bool("clean_day", r.clean_day())
            .context("clean_day")?
            .at(TimestampNanos::new(r.ts_ist_nanos))
            .context("designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Flushes buffered rows over ILP-HTTP (per-flush server ACK — a
    /// server-side reject surfaces as `Err`, never a silently empty table).
    ///
    /// # Errors
    /// `Err` when disconnected or the HTTP flush fails (rows stay buffered).
    pub fn flush(&mut self) -> Result<()> {
        if self.pending == 0 {
            return Ok(());
        }
        let Some(sender) = self.sender.as_mut() else {
            anyhow::bail!("ws_connection_daily: no ILP sender (QuestDB unreachable)");
        };
        if let Err(err) = sender.flush(&mut self.buffer) {
            let dropped = crate::ilp_overflow::discard_if_overflowing(
                &mut self.buffer,
                &mut self.pending,
                "ws_connection_daily",
            );
            return Err(anyhow::Error::new(err).context(
                crate::ilp_overflow::flush_failure_context(
                    "ws_connection_daily ILP flush",
                    dropped,
                ),
            ));
        }
        self.pending = 0;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row() -> WsConnectionDailyRow {
        WsConnectionDailyRow {
            ts_ist_nanos: 1_769_990_400_000_000_000,
            trading_date_ist_nanos: 1_769_990_400_000_000_000,
            feed: "dhan".to_string(),
            ws_type: "main_feed".to_string(),
            connection_index: 3,
            pool_size: 5,
            connects: 1,
            reconnects: 2,
            disconnect_events: 2,
            disconnects_market: 1,
            disconnects_off_hours: 1,
            stalls: 0,
            restarts: 0,
            blame_broker: 1,
            blame_ours: 0,
            blame_indeterminate: 0,
            total_down_secs: 30,
            max_down_secs: 22,
            total_attempts: 4,
            max_attempts: 3,
            saw_any_event: true,
        }
    }

    #[test]
    fn test_ddl_carries_every_column_and_the_dedup_key() {
        let ddl = ws_connection_daily_create_ddl();
        for col in [
            "ts",
            "trading_date_ist",
            "feed",
            "ws_type",
            "connection_index",
            "pool_size",
            "connects",
            "reconnects",
            "disconnect_events",
            "disconnects_market",
            "disconnects_off_hours",
            "stalls",
            "restarts",
            "blame_broker",
            "blame_ours",
            "blame_indeterminate",
            "total_down_secs",
            "max_down_secs",
            "total_attempts",
            "max_attempts",
            "saw_any_event",
            "clean_day",
        ] {
            assert!(ddl.contains(col), "DDL is missing column `{col}`");
        }
        assert!(ddl.contains("CREATE TABLE IF NOT EXISTS"));
        assert!(ddl.contains("timestamp(ts) PARTITION BY DAY"));
        assert!(ddl.contains(&format!(
            "DEDUP UPSERT KEYS({DEDUP_KEY_WS_CONNECTION_DAILY})"
        )));
    }

    #[test]
    fn test_dedup_key_carries_the_full_connection_identity() {
        // The whole point of the table: one row per CONNECTION, not per feed.
        // Dropping `connection_index` would collapse all 16 sockets into one
        // row and silently overwrite fifteen of them.
        for part in [
            "ts",
            "trading_date_ist",
            "feed",
            "ws_type",
            "connection_index",
        ] {
            assert!(
                DEDUP_KEY_WS_CONNECTION_DAILY.contains(part),
                "DEDUP key is missing `{part}`"
            );
        }
        assert!(
            DEDUP_KEY_WS_CONNECTION_DAILY
                .trim_start()
                .starts_with("ts,"),
            "designated timestamp must lead the DEDUP key (2026-04-28 rule)"
        );
    }

    #[test]
    fn test_a_connection_that_never_appeared_is_not_a_clean_day() {
        // Rule 11: absence must never render as health. This is the exact
        // shape that made the 2026-08-28 depth-spill counter unreadable.
        let never = WsConnectionDailyRow {
            saw_any_event: false,
            ..WsConnectionDailyRow::default()
        };
        assert!(
            !never.clean_day(),
            "a connection that never appeared is a finding, not a clean day"
        );
        assert_eq!(
            never.incident_total(),
            0,
            "and it genuinely had zero incidents — which is why the flag, not the counts, is what separates the two"
        );
    }

    #[test]
    fn test_a_connection_that_appeared_with_no_incidents_is_clean() {
        let healthy = WsConnectionDailyRow {
            saw_any_event: true,
            connects: 1,
            ..WsConnectionDailyRow::default()
        };
        assert!(healthy.clean_day());
        assert_eq!(healthy.incident_total(), 0);
    }

    #[test]
    fn test_any_single_incident_kind_breaks_the_clean_day() {
        // Each field checked independently: a clean_day that ignored even one
        // incident kind would report a bad day as good.
        let base = WsConnectionDailyRow {
            saw_any_event: true,
            connects: 1,
            ..WsConnectionDailyRow::default()
        };
        assert!(base.clean_day(), "control must be clean");

        let mut cases: Vec<(&str, WsConnectionDailyRow)> = Vec::new();
        cases.push((
            "disconnect_events",
            WsConnectionDailyRow {
                disconnect_events: 1,
                ..base.clone()
            },
        ));
        cases.push((
            "disconnects_market",
            WsConnectionDailyRow {
                disconnects_market: 1,
                ..base.clone()
            },
        ));
        cases.push((
            "disconnects_off_hours",
            WsConnectionDailyRow {
                disconnects_off_hours: 1,
                ..base.clone()
            },
        ));
        cases.push((
            "stalls",
            WsConnectionDailyRow {
                stalls: 1,
                ..base.clone()
            },
        ));
        cases.push((
            "restarts",
            WsConnectionDailyRow {
                restarts: 1,
                ..base.clone()
            },
        ));
        cases.push((
            "reconnects",
            WsConnectionDailyRow {
                reconnects: 1,
                ..base.clone()
            },
        ));

        for (field, row) in cases {
            assert!(
                !row.clean_day(),
                "a non-zero `{field}` must break clean_day — otherwise a real \
                 incident renders as a clean day"
            );
        }
    }

    #[test]
    fn test_incident_total_sums_the_classified_kinds() {
        let r = sample_row();
        assert_eq!(r.incident_total(), 2, "1 market + 1 off-hours + 0 + 0");
    }

    #[test]
    fn test_incident_total_saturates_instead_of_wrapping() {
        // A corrupt read must never wrap into a small reassuring number.
        let r = WsConnectionDailyRow {
            disconnects_market: i64::MAX,
            stalls: i64::MAX,
            saw_any_event: true,
            ..WsConnectionDailyRow::default()
        };
        assert_eq!(r.incident_total(), i64::MAX);
    }

    #[test]
    fn test_append_row_writes_the_connection_identity_and_counts() {
        let mut w = WsConnectionDailyWriter::for_test();
        w.append_row(&sample_row()).expect("append");
        assert_eq!(w.pending(), 1);
        let line = w.buffer_utf8();
        assert!(line.contains(WS_CONNECTION_DAILY_TABLE));
        assert!(line.contains("feed=dhan"));
        assert!(line.contains("ws_type=main_feed"));
        assert!(line.contains("connection_index=3"));
        assert!(line.contains("pool_size=5"));
        assert!(line.contains("disconnects_market=1"));
        assert!(line.contains("max_down_secs=22"));
        assert!(line.contains("saw_any_event=t"));
    }

    #[test]
    fn test_append_row_derives_clean_day_and_ignores_a_caller_that_disagrees() {
        // `clean_day` is not a caller-supplied field at all — it is computed
        // at the write boundary, so a row carrying incidents can never be
        // persisted as clean.
        let mut w = WsConnectionDailyWriter::for_test();
        w.append_row(&sample_row()).expect("append");
        assert!(
            w.buffer_utf8().contains("clean_day=f"),
            "a row with 1 market disconnect must persist clean_day=false"
        );

        let mut w2 = WsConnectionDailyWriter::for_test();
        w2.append_row(&WsConnectionDailyRow {
            saw_any_event: true,
            connects: 1,
            ..WsConnectionDailyRow::default()
        })
        .expect("append");
        assert!(w2.buffer_utf8().contains("clean_day=t"));
    }

    #[test]
    fn test_flush_without_sender_errors_and_retains_rows() {
        let mut w = WsConnectionDailyWriter::for_test();
        w.append_row(&sample_row()).expect("append");
        let err = w
            .flush()
            .expect_err("disconnected writer must not report success");
        assert!(err.to_string().contains("no ILP sender"));
        assert_eq!(
            w.pending(),
            1,
            "rows must be retained, never silently dropped"
        );
    }

    #[test]
    fn test_flush_with_nothing_pending_is_a_noop_success() {
        let mut w = WsConnectionDailyWriter::for_test();
        assert!(w.flush().is_ok());
    }

    #[test]
    fn test_every_column_gains_a_self_heal_alter_so_none_can_silently_never_arrive() {
        // A table created by an older build must gain every column added
        // since. The CREATE and the ALTERs come from ONE list precisely so
        // that a new column cannot reach a fresh table while never reaching
        // an existing one.
        let statements = ws_connection_daily_ddl_statements();
        let joined = statements.join("\n");
        for (col, ty) in WS_CONNECTION_DAILY_COLUMNS {
            if *col == "ts" {
                // The designated timestamp cannot be added after the fact.
                continue;
            }
            assert!(
                joined.contains(&format!("ADD COLUMN IF NOT EXISTS {col} {ty}")),
                "column `{col}` has no self-heal ALTER — it would never reach \
                 a table that already exists"
            );
        }
    }

    #[test]
    fn test_ws_connection_daily_ddl_statements_create_then_alter_then_enable_dedup() {
        let statements = ws_connection_daily_ddl_statements();
        assert!(
            statements
                .first()
                .is_some_and(|s| s.contains("CREATE TABLE IF NOT EXISTS")),
            "CREATE must come first — an ALTER against a missing table fails"
        );
        assert!(
            statements
                .last()
                .is_some_and(|s| s.contains("DEDUP ENABLE UPSERT KEYS")),
            "DEDUP ENABLE must come last, after every column it names exists"
        );
        assert!(
            !joined_contains_drop(&statements),
            "no DDL statement may DROP anything — this table is retention-swept, never dropped by code"
        );
    }

    fn joined_contains_drop(statements: &[String]) -> bool {
        statements
            .iter()
            .any(|s| s.to_ascii_uppercase().contains("DROP "))
    }

    #[test]
    fn test_the_dedup_key_columns_all_exist_in_the_column_list() {
        // A DEDUP key naming a column the CREATE does not define makes the
        // whole DDL fail at boot, leaving an un-deduped auto-created table.
        for part in DEDUP_KEY_WS_CONNECTION_DAILY.split(',') {
            let name = part.trim();
            assert!(
                WS_CONNECTION_DAILY_COLUMNS.iter().any(|(c, _)| *c == name),
                "DEDUP key names `{name}`, which is not a declared column"
            );
        }
    }
}

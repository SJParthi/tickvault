//! Speed and health snapshot — `GET /api/debug/speed-health` (owner,
//! 2026-10-30: *"always ensure saving into db, auditing, logging, tracking,
//! capturing, debugging, finding, searching, monitoring, dashboard
//! visualising"*).
//!
//! The `/dashboard` page polls this endpoint and paints whatever it returns.
//! Every label, every number and every green / amber / red verdict is decided
//! HERE, in Rust, where it is tested; the page only paints tiles (its script is
//! size-capped by the browser-surface guard). Nothing on the response names a
//! library, a file path or a metric name: the labels are plain English.
//!
//! # Sources, all already stored or already measured
//!
//! | Section | Source |
//! |---|---|
//! | How fast each step runs | the process's own step-latency counters (`hot_path_telemetry::stage_snapshot`), read in memory: the API runs inside the same process as the feed |
//! | Database catch-up queue, feed sockets connected now | read in process: the WAL watcher's last apply-lag reading (`ingest_shed::wal_apply_lag_max`) and the feed's live socket count (the `/health` source) |
//! | Worst step in the last 75 s, bytes kept on disk until confirmed, reconnects, candle overflow | the app's own metrics page (`[observability]` address), read once per refresh |
//! | Ticks per minute | QuestDB `ticks` and `tick_quotes`, the last 15 full minutes |
//! | Every tick accounted for | QuestDB `tick_conservation_audit`, the last 14 days |
//! | Connection gaps closed today | QuestDB `feed_gap_audit`, since IST midnight (a gap is written when it closes) |
//! | Per-minute speed record | QuestDB `hot_path_latency_minute` when that table exists; "not yet available" when it does not |
//!
//! # Honesty rules
//!
//! * A source that cannot be read shows a GREY tile that says so. A number is
//!   never invented and a missing source never reads green.
//! * Step latencies come from a power-of-two histogram, so a percentile is the
//!   upper edge of its bucket: the true value is at most that, and above half
//!   of it. The verdict uses that: RED only when even half the bound is over
//!   the step's budget (certainly over), AMBER when the bound is over but half
//!   is not (possibly over).
//! * The in-memory step figures cover the time since the app started; the
//!   "worst in the last 75 s" figure is the only recent window.
//! * Ticks are judged only between 09:15 and 15:30, and "no trade in the last
//!   minute" is red only while the database is caught up: rows QuestDB has not
//!   applied yet are not readable, so a backlog reads amber, not "lost".
//! * Figures that cannot say whether something is wrong right now carry no
//!   verdict (grey): closed connection gaps (an open gap has no row yet), the
//!   candle overflow bytes (they include files already written back), and the
//!   per-minute speed record (shown as stored, column names included).
//!
//! # Cost
//!
//! Cold path only. One refresh is at most six QuestDB queries (each bounded
//! by [`SPEED_HEALTH_QUERY_TIMEOUT_SECS`] and by a time window on the
//! designated timestamp) and one read of the metrics page (bounded by
//! [`METRICS_SCRAPE_TIMEOUT_SECS`] and [`METRICS_SCRAPE_MAX_BYTES`]), run
//! concurrently; the body is cached for 5 s in a single slot
//! ([`crate::state::SharedAppState::speed_health_cache`]). A miss takes a
//! single-flight lock and re-checks the cache, so concurrent misses (several
//! tabs, or a pass slower than the 5 s TTL while the database is slow) run one
//! pass, not one each; the worst pass is about 9 s (the per-minute table's
//! columns probe and rows read run one after the other). Reading the metrics page is
//! **O(lines)** in one pass; reading the step counters is O(5 steps × 28
//! buckets). Neither runs on the feed path. The route is bearer-gated with the
//! other `/api/debug/*` reads.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::Value;

use tickvault_storage::hot_path_telemetry::{
    ALL_STAGES, QuantileBound, STAGE_BUCKETS, Stage, stage_quantiles, stage_snapshot,
};

use crate::state::SharedAppState;

/// Per-request timeout for each QuestDB query this endpoint runs.
const SPEED_HEALTH_QUERY_TIMEOUT_SECS: u64 = 3;

/// Per-request timeout for reading the app's own metrics page.
const METRICS_SCRAPE_TIMEOUT_SECS: u64 = 2;

/// Largest metrics page this endpoint reads. A bigger page is not parsed and
/// every figure from it shows "not available" (never a partial read).
const METRICS_SCRAPE_MAX_BYTES: usize = 16 * 1024 * 1024;

/// Full minutes of tick counts shown.
const TICK_WINDOW_MINUTES: i64 = 15;

/// Days of daily tick-accounting checks read back.
const CONSERVATION_LOOKBACK_DAYS: i64 = 14;

/// Daily tick-accounting checks shown.
const CONSERVATION_ROWS: usize = 5;

/// The per-minute step-latency table another change adds. Read when it exists.
const MINUTE_TABLE: &str = "hot_path_latency_minute";

/// Rows of the per-minute table shown (newest first).
const MINUTE_TABLE_ROWS: usize = 20;

/// How far back the per-minute table is read. 24 h covers both an IST-stored
/// and a UTC-stored designated timestamp, since the window starts from the
/// IST wall clock, which is ahead of UTC.
const MINUTE_TABLE_LOOKBACK_HOURS: i64 = 24;

/// Longest text shown from one stored cell.
const CELL_MAX_CHARS: usize = 64;

/// QuestDB apply backlog at or under this many transactions is idle (the
/// same-day drain's "idle level", operator Quote 31).
const APPLY_LAG_GREEN_MAX: u64 = 10;

/// Above this many transactions the evening write-back pauses (operator
/// Quote 31), so the dashboard reads it as red.
const APPLY_LAG_AMBER_MAX: u64 = 500;

/// Bytes the frame log keeps on disk because the database has not confirmed
/// them; from this much the tile is red. A dashboard colour choice, about 7%
/// of the 150 GB root disk; nothing pages on it.
const PINNED_BYTES_RED_MIN: f64 = 10.0 * 1024.0 * 1024.0 * 1024.0;

/// Reconnects since the app started up to this many read amber; more read red.
const RECONNECTS_AMBER_MAX: f64 = 5.0;

/// One minute in seconds.
const SECS_PER_MINUTE: i64 = 60;

/// One day in seconds.
const SECS_PER_DAY: i64 = 86_400;

/// One hour in seconds.
const SECS_PER_HOUR: i64 = 3_600;

/// Nanoseconds per second.
const NANOS_PER_SEC: i64 = 1_000_000_000;

/// End of continuous trading, 15:30 IST, as seconds of the IST day. The
/// closing session that follows (to 15:40) trades too rarely to judge each
/// minute. Pinned to `MARKET_CLOSE_TIME_IST_EXCLUSIVE` by a test.
const CONTINUOUS_TRADING_CLOSE_SECS_OF_DAY_IST: i64 = 55_800;

/// A newest daily tick check older than this many days reads amber: a
/// weekend plus a holiday is three days, so four means a check went missing.
const CONSERVATION_STALE_DAYS: i64 = 4;

/// Metric names read from the metrics page. Where the producing module exports
/// the name as a constant, that constant is used so a rename cannot drift.
const STAGE_MAX_75S_METRIC: &str = "tv_hot_path_stage_max_ns_75s";
const SEAL_SPILL_BYTES_METRIC: &str = "tv_seal_spill_bytes";
const SEAL_SPILL_WRITE_ERRORS_METRIC: &str = "tv_seal_spill_write_errors_total";

// ---------------------------------------------------------------------------
// Response shape
// ---------------------------------------------------------------------------

/// A tile's colour. `Grey` means "no verdict": the source could not be read,
/// has nothing yet, or the market is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Light {
    Green,
    Amber,
    Red,
    Grey,
}

/// One labelled number on the page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Tile {
    /// What the number is, in plain English.
    pub label: String,
    /// The number, already formatted with its unit.
    pub value: String,
    /// The verdict colour.
    pub status: Light,
    /// One short line of context (may be empty).
    pub detail: String,
}

impl Tile {
    fn new(label: impl Into<String>, value: impl Into<String>, status: Light) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            status,
            detail: String::new(),
        }
    }

    fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }
}

/// A titled group of tiles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Section {
    pub title: &'static str,
    /// One plain-English line under the title (may be empty).
    pub note: String,
    pub tiles: Vec<Tile>,
}

/// The full `GET /api/debug/speed-health` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SpeedHealthResponse {
    /// Trading session open right now (weekend and NSE-holiday aware).
    pub market_open: bool,
    pub sections: Vec<Section>,
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// `GET /api/debug/speed-health` — the speed and health snapshot. Always 200;
/// every unreadable source degrades to a grey tile. The body is cached for
/// 5 s in one slot, so a cache hit runs no query and no metrics read.
pub async fn speed_health(State(state): State<SharedAppState>) -> Response {
    if let Some(body) = state.speed_health_cache().get() {
        metrics::counter!("tv_api_cache_hits_total", "endpoint" => "speed_health").increment(1);
        return crate::response_cache::cached_json_response(body, "hit");
    }
    // Single flight: one pass at a time; a request that waited here finds
    // the body the pass before it stored.
    let _flight = state.speed_health_refresh().lock().await;
    if let Some(body) = state.speed_health_cache().get() {
        metrics::counter!("tv_api_cache_hits_total", "endpoint" => "speed_health").increment(1);
        return crate::response_cache::cached_json_response(body, "hit");
    }
    let snapshot = compute_speed_health(&state).await;
    match serde_json::to_string(&snapshot) {
        Ok(body) => {
            state.speed_health_cache().put(body.clone());
            crate::response_cache::cached_json_response(body, "miss")
        }
        // Practically unreachable (plain strings and enums); never cache a
        // body that did not serialize.
        Err(_) => Json(snapshot).into_response(),
    }
}

/// Runs every read concurrently and assembles the sections.
pub(crate) async fn compute_speed_health(state: &SharedAppState) -> SpeedHealthResponse {
    let market_open = tickvault_common::market_hours::is_trading_session_now();
    let ist_now = ist_now_secs();
    let cfg = state.questdb_config();
    let base_url = format!("http://{}:{}", cfg.host, cfg.http_port);
    // The shared pooled client; every request below sets its own timeout.
    let client = state.questdb_http_client();

    let ticks_sql = ticks_per_minute_sql(TickTable::Trades, ist_now);
    let quotes_sql = ticks_per_minute_sql(TickTable::RepeatQuotes, ist_now);
    let conservation = conservation_sql(ist_now);
    let gaps = feed_gaps_sql(ist_now);
    let (ticks, quotes, conservation, gaps, minute_table, exporter_text) = tokio::join!(
        query_rows(client, &base_url, &ticks_sql),
        query_rows(client, &base_url, &quotes_sql),
        query_rows(client, &base_url, &conservation),
        query_rows(client, &base_url, &gaps),
        read_minute_table(client, &base_url, ist_now),
        fetch_metrics_text(client, state.metrics_scrape_url()),
    );
    let exporter = exporter_text.as_deref().map(parse_metrics_text);
    let stages = ALL_STAGES.map(|stage| {
        let snap = stage_snapshot(stage);
        StageFigures {
            stage,
            count: snap.count,
            sum_nanos: snap.sum_nanos,
            over_budget: snap.over_budget,
            buckets: snap.buckets,
        }
    });

    build_sections(&Inputs {
        market_open,
        ist_now,
        apply_lag: tickvault_common::ingest_shed::wal_apply_lag_max(),
        sockets_now: state
            .health_status()
            .websocket_reported()
            .then(|| state.health_status().websocket_connections()),
        stages,
        exporter,
        ticks,
        quotes,
        conservation,
        gaps,
        minute_table,
    })
}

// ---------------------------------------------------------------------------
// Pure assembly (everything below is testable without a database)
// ---------------------------------------------------------------------------

/// One step's in-memory counters, copied out so assembly is a pure function.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StageFigures {
    pub stage: Stage,
    pub count: u64,
    pub sum_nanos: u64,
    pub over_budget: u64,
    pub buckets: [u64; STAGE_BUCKETS],
}

/// Everything the sections are built from.
pub(crate) struct Inputs {
    pub market_open: bool,
    pub ist_now: i64,
    /// The database apply backlog the WAL watcher last read, in process;
    /// `None` when unknown (watcher blind, database not answering).
    pub apply_lag: Option<u64>,
    /// Feed sockets connected right now; `None` until the feed reports.
    pub sockets_now: Option<u64>,
    pub stages: [StageFigures; 5],
    pub exporter: Option<ExporterReadings>,
    pub ticks: QueryOutcome,
    pub quotes: QueryOutcome,
    pub conservation: QueryOutcome,
    pub gaps: QueryOutcome,
    pub minute_table: QueryOutcome,
}

/// Builds every section, in page order.
pub(crate) fn build_sections(inputs: &Inputs) -> SpeedHealthResponse {
    let sections = vec![
        speed_section(&inputs.stages, inputs.exporter.as_ref()),
        ticks_section(inputs),
        conservation_section(&inputs.conservation, inputs.ist_now),
        backlog_section(inputs),
        connections_section(inputs),
        overflow_section(inputs.exporter.as_ref()),
        minute_table_section(&inputs.minute_table),
    ];
    SpeedHealthResponse {
        market_open: inputs.market_open,
        sections,
    }
}

/// Plain-English name of each step.
pub(crate) const fn stage_label(stage: Stage) -> &'static str {
    match stage {
        Stage::SocketToWal => "Saving each message as it arrives",
        Stage::RingDwell => "Waiting in line before processing",
        Stage::MainRuntimeLag => "Main engine response delay",
        Stage::ReaderRuntimeLag => "Socket reader response delay",
        Stage::DrainMainFrame => "Processing one market message",
    }
}

/// The verdict for one step from its 99th percentile bound and its budget.
pub(crate) fn stage_light(p99: Option<QuantileBound>, budget_nanos: u64) -> Light {
    match p99 {
        None => Light::Grey,
        Some(QuantileBound::Overflow) => Light::Red,
        // The true value is in (bound/2, bound]: certainly over when even
        // half the bound is over the budget, possibly over when only the
        // bound is.
        Some(QuantileBound::AtMost(bound)) if bound / 2 >= budget_nanos => Light::Red,
        Some(QuantileBound::AtMost(bound)) if bound > budget_nanos => Light::Amber,
        Some(QuantileBound::AtMost(_)) => Light::Green,
    }
}

fn bound_text(bound: QuantileBound) -> String {
    match bound {
        QuantileBound::AtMost(n) => format!("at most {}", human_nanos(n)),
        QuantileBound::Overflow => "over 68 s".to_string(),
    }
}

fn speed_section(stages: &[StageFigures; 5], exporter: Option<&ExporterReadings>) -> Section {
    let tiles = stages
        .iter()
        .map(|fig| {
            let label = stage_label(fig.stage);
            let budget = fig.stage.stall_budget_nanos();
            let Some(q) = stage_quantiles(&fig.buckets) else {
                return Tile::new(label, "no samples yet", Light::Grey)
                    .with_detail(format!("limit {}", human_nanos(budget)));
            };
            let mean = fig.sum_nanos.checked_div(fig.count).unwrap_or(0);
            let worst_75s = exporter
                .and_then(|e| e.stage_max_75s[fig.stage as usize])
                .map_or_else(
                    || "worst in the last 75 s not available".to_string(),
                    |n| format!("worst in the last 75 s {}", human_nanos(n)),
                );
            Tile::new(
                label,
                format!("99% {}", bound_text(q.p99)),
                stage_light(Some(q.p99), budget),
            )
            .with_detail(format!(
                "half {} · average {} · limit {} · {} of {} over the limit · {}",
                bound_text(q.p50),
                human_nanos(mean),
                human_nanos(budget),
                group_digits(fig.over_budget),
                group_digits(fig.count),
                worst_75s,
            ))
        })
        .collect();
    Section {
        title: "How fast each step runs",
        note: "Since the app started. Green: 99 of every 100 runs are inside the step's \
               limit. Amber: they may not be. Red: they are not."
            .to_string(),
        tiles,
    }
}

/// Which tick table a per-minute count reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TickTable {
    /// `ticks`: packets that carried a new trade.
    Trades,
    /// `tick_quotes`: packets whose trade time was older than their arrival.
    RepeatQuotes,
}

impl TickTable {
    const fn table_name(self) -> &'static str {
        match self {
            Self::Trades => "ticks",
            Self::RepeatQuotes => "tick_quotes",
        }
    }
}

/// `SELECT ts, count()` per minute over the last [`TICK_WINDOW_MINUTES`] FULL
/// minutes. Both tables store IST wall time in `ts`, so the window is built
/// from the IST clock. The minute still filling is excluded.
pub(crate) fn ticks_per_minute_sql(table: TickTable, ist_now: i64) -> String {
    let (start, end) = tick_window(ist_now);
    format!(
        "SELECT ts, count() FROM {} WHERE ts >= '{}' AND ts < '{}' SAMPLE BY 1m ALIGN TO CALENDAR",
        table.table_name(),
        ist_literal(start),
        ist_literal(end),
    )
}

/// `[start, end)` of the tick window, in IST epoch seconds; `end` is the start
/// of the current minute.
pub(crate) fn tick_window(ist_now: i64) -> (i64, i64) {
    let end = ist_now.div_euclid(SECS_PER_MINUTE) * SECS_PER_MINUTE;
    (end - TICK_WINDOW_MINUTES * SECS_PER_MINUTE, end)
}

/// The newest daily tick-accounting rows of the last
/// [`CONSERVATION_LOOKBACK_DAYS`] days.
pub(crate) fn conservation_sql(ist_now: i64) -> String {
    format!(
        "SELECT trading_date_ist, feed, verdict, reason, accepted_rows, db_tick_rows, \
         not_landed_rows, shortfall FROM tick_conservation_audit WHERE ts >= '{}' \
         ORDER BY ts DESC LIMIT {CONSERVATION_ROWS}",
        ist_literal(ist_now - CONSERVATION_LOOKBACK_DAYS * SECS_PER_DAY),
    )
}

/// Connection gaps since IST midnight: how many and how long in total.
pub(crate) fn feed_gaps_sql(ist_now: i64) -> String {
    let midnight = ist_now.div_euclid(SECS_PER_DAY) * SECS_PER_DAY;
    format!(
        "SELECT count(), sum(down_secs) FROM feed_gap_audit WHERE ts >= '{}'",
        ist_literal(midnight)
    )
}

/// Lists the per-minute table's columns; errors with "does not exist" when the
/// table has not been created yet.
pub(crate) fn minute_table_columns_sql() -> String {
    format!("SELECT \"column\", designated FROM table_columns('{MINUTE_TABLE}')")
}

/// The newest rows of the per-minute table inside its lookback window,
/// filtered on its designated timestamp column.
pub(crate) fn minute_table_rows_sql(designated: &str, ist_now: i64) -> String {
    format!(
        "SELECT * FROM {MINUTE_TABLE} WHERE {designated} >= '{}' ORDER BY {designated} DESC \
         LIMIT {MINUTE_TABLE_ROWS}",
        ist_literal(ist_now - MINUTE_TABLE_LOOKBACK_HOURS * SECS_PER_HOUR),
    )
}

/// True for a plain column name (letters, digits, underscore), so a name
/// read back from the database can be put into a query safely.
pub(crate) fn is_plain_identifier(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= CELL_MAX_CHARS
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Per-minute counts for the tick window, oldest first; a minute with no row
/// is 0. `None` when the rows cannot be read.
pub(crate) fn minute_counts(outcome: &QueryOutcome, ist_now: i64) -> Option<Vec<u64>> {
    let QueryOutcome::Rows { rows, .. } = outcome else {
        return None;
    };
    let (start, _) = tick_window(ist_now);
    let mut counts = vec![0u64; usize::try_from(TICK_WINDOW_MINUTES).unwrap_or(0)];
    for row in rows {
        let ts = row.first().and_then(Value::as_str).and_then(parse_ts_secs);
        let count = row.get(1).and_then(Value::as_u64);
        if let (Some(ts), Some(count)) = (ts, count) {
            let minute = (ts - start).div_euclid(SECS_PER_MINUTE);
            if let Some(slot) = usize::try_from(minute).ok().and_then(|i| counts.get_mut(i)) {
                *slot = slot.saturating_add(count);
            }
        }
    }
    Some(counts)
}

/// Start of IST day `ist_now` falls in, plus `secs_of_day`.
fn ist_day_at(ist_now: i64, secs_of_day: i64) -> i64 {
    ist_now.div_euclid(SECS_PER_DAY) * SECS_PER_DAY + secs_of_day
}

/// True for a minute that starts inside continuous trading, 09:15 to 15:30.
/// Outside it (the pre-open, the closing session to 15:40) trades are not
/// expected every minute, so no verdict is given.
pub(crate) fn in_continuous_trading(minute_start: i64) -> bool {
    let open = ist_day_at(
        minute_start,
        tickvault_common::constants::MARKET_OPEN_IST_NANOS / NANOS_PER_SEC,
    );
    let close = ist_day_at(minute_start, continuous_trading_close_secs_of_day());
    (open..close).contains(&minute_start)
}

/// End of continuous trading as seconds of the IST day, read from the shared
/// `MARKET_CLOSE_TIME_IST_EXCLUSIVE` ("15:30:00") so the two cannot drift;
/// [`CONTINUOUS_TRADING_CLOSE_SECS_OF_DAY_IST`] only if that text ever stops
/// parsing (a test pins that it parses to the same value).
pub(crate) fn continuous_trading_close_secs_of_day() -> i64 {
    chrono::NaiveTime::parse_from_str(
        tickvault_common::constants::MARKET_CLOSE_TIME_IST_EXCLUSIVE,
        "%H:%M:%S",
    )
    .map_or(CONTINUOUS_TRADING_CLOSE_SECS_OF_DAY_IST, |t| {
        i64::from(chrono::Timelike::num_seconds_from_midnight(&t))
    })
}

/// The verdict on the trade counts. Grey when the counts are unreadable, the
/// market is closed, or the last full minute is outside continuous trading.
/// Red when the last full minute holds no trade while the database is caught
/// up (so the rows would be readable); amber when it holds none while the
/// database is behind or its backlog is unknown, or when an earlier minute of
/// continuous trading holds none; else green.
pub(crate) fn ticks_light(
    market_open: bool,
    counts: Option<&[u64]>,
    ist_now: i64,
    database_caught_up: bool,
) -> Light {
    let Some(counts) = counts else {
        return Light::Grey;
    };
    let (start, end) = tick_window(ist_now);
    if !market_open || !in_continuous_trading(end - SECS_PER_MINUTE) {
        return Light::Grey;
    }
    if counts.last().copied().unwrap_or(0) == 0 {
        return if database_caught_up {
            Light::Red
        } else {
            Light::Amber
        };
    }
    let quiet_in_session = counts.iter().enumerate().any(|(i, n)| {
        let minute_start = start + i64::try_from(i).unwrap_or(0) * SECS_PER_MINUTE;
        *n == 0 && in_continuous_trading(minute_start)
    });
    if quiet_in_session {
        Light::Amber
    } else {
        Light::Green
    }
}

fn ticks_section(inputs: &Inputs) -> Section {
    let trades = minute_counts(&inputs.ticks, inputs.ist_now);
    let quotes = minute_counts(&inputs.quotes, inputs.ist_now);
    let light = ticks_light(
        inputs.market_open,
        trades.as_deref(),
        inputs.ist_now,
        database_caught_up(inputs),
    );
    let mut tiles = Vec::with_capacity(3);
    match trades.as_deref() {
        Some(counts) => {
            let last = counts.last().copied().unwrap_or(0);
            let total: u64 = counts.iter().sum();
            let quiet = counts.iter().filter(|n| **n == 0).count();
            let average = total / u64::try_from(counts.len().max(1)).unwrap_or(1);
            tiles.push(Tile::new(
                "Trades readable in the database, last full minute",
                group_digits(last),
                light,
            ));
            tiles.push(
                Tile::new(
                    "Trades readable in the database, 15-minute average per minute",
                    group_digits(average),
                    light,
                )
                .with_detail(format!(
                    "{} in 15 minutes · {quiet} minute(s) with none",
                    group_digits(total)
                )),
            );
        }
        None => tiles.push(
            Tile::new(
                "Trades readable in the database",
                "could not read",
                Light::Grey,
            )
            .with_detail(outcome_reason(&inputs.ticks)),
        ),
    }
    match quotes.as_deref() {
        Some(counts) => tiles.push(
            Tile::new(
                "Late quotes readable in the database, last full minute",
                group_digits(counts.last().copied().unwrap_or(0)),
                Light::Grey,
            )
            .with_detail("packets whose trade was older than their arrival; no verdict"),
        ),
        None => tiles.push(
            Tile::new(
                "Late quotes readable in the database",
                "could not read",
                Light::Grey,
            )
            .with_detail(outcome_reason(&inputs.quotes)),
        ),
    }
    let note = "The last 15 full minutes, judged only between 09:15 and 15:30. Red: no trade \
                in the last minute while the database is caught up. Amber: none while the \
                database is behind, or some earlier minute had none. Grey: no verdict.";
    Section {
        title: "Ticks saved per minute",
        note: note.to_string(),
        tiles,
    }
}

/// The verdict for one daily tick-accounting row.
pub(crate) fn conservation_light(verdict: &str) -> Light {
    match verdict {
        "match" => Light::Green,
        "partial" | "unverified" => Light::Amber,
        "mismatch" => Light::Red,
        _ => Light::Grey,
    }
}

/// Plain-English word for a stored verdict.
pub(crate) fn conservation_word(verdict: &str) -> &'static str {
    match verdict {
        "match" => "every tick counted",
        "partial" => "partly checked (app restarted mid-day)",
        "unverified" => "could not be checked",
        "mismatch" => "ticks missing",
        _ => "unknown result",
    }
}

/// How old the newest daily tick check is. Green within
/// [`CONSERVATION_STALE_DAYS`] days, amber older (a check went missing, so the
/// older green rows below it say nothing about the last trading days), grey
/// when its day cannot be read. `rows` come newest first.
pub(crate) fn conservation_freshness_tile(
    columns: &[String],
    rows: &[Vec<Value>],
    ist_now: i64,
) -> Tile {
    const LABEL: &str = "Newest daily tick check";
    let newest = rows
        .first()
        .and_then(|row| column_value(columns, row, "trading_date_ist"))
        .and_then(Value::as_str)
        .and_then(parse_ts_secs);
    match newest {
        None => Tile::new(LABEL, "unknown day", Light::Grey),
        Some(day) => {
            let age_days = (ist_day_at(ist_now, 0) - ist_day_at(day, 0)).div_euclid(SECS_PER_DAY);
            let value = match age_days {
                d if d <= 0 => "today".to_string(),
                1 => "1 day ago".to_string(),
                d => format!("{d} days ago"),
            };
            if age_days <= CONSERVATION_STALE_DAYS {
                Tile::new(LABEL, value, Light::Green)
            } else {
                Tile::new(LABEL, value, Light::Amber)
                    .with_detail("a daily check is missing since then")
            }
        }
    }
}

fn conservation_section(outcome: &QueryOutcome, ist_now: i64) -> Section {
    let tiles = match outcome {
        QueryOutcome::Rows { columns, rows } if !rows.is_empty() => {
            std::iter::once(conservation_freshness_tile(columns, rows, ist_now))
                .chain(rows.iter().map(|row| {
                    let cell = |name: &str| column_value(columns, row, name);
                    let verdict = cell("verdict").and_then(Value::as_str).unwrap_or("");
                    let day = cell("trading_date_ist")
                        .and_then(Value::as_str)
                        .map_or("unknown day", |d| d.get(..10).unwrap_or(d));
                    let feed = cell("feed").and_then(Value::as_str).unwrap_or("unknown");
                    let number = |name: &str| {
                        cell(name)
                            .and_then(Value::as_i64)
                            .map_or_else(|| "unknown".to_string(), format_signed)
                    };
                    let db = match cell("db_tick_rows").and_then(Value::as_i64) {
                        Some(n) if n >= 0 => format_signed(n),
                        _ => "unknown".to_string(),
                    };
                    Tile::new(
                format!("{day} · {}", short_text(feed)),
                conservation_word(verdict),
                conservation_light(verdict),
            )
            .with_detail(format!(
                "received {} · in the database {db} · not landed {} · short by {} · reason {}",
                number("accepted_rows"),
                number("not_landed_rows"),
                number("shortfall"),
                short_text(
                    &cell("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("none")
                        .replace('_', " ")
                ),
            ))
                }))
                .collect()
        }
        QueryOutcome::Rows { .. } => vec![
            Tile::new("Daily tick check", "no check in 14 days", Light::Grey)
                .with_detail("the check runs at 15:45 on trading days"),
        ],
        other => vec![
            Tile::new("Daily tick check", "could not read", Light::Grey)
                .with_detail(outcome_reason(other)),
        ],
    };
    Section {
        title: "Every tick accounted for",
        note: "The 15:45 daily check: ticks the app received against ticks in the database."
            .to_string(),
        tiles,
    }
}

/// The verdict on the database's catch-up queue.
pub(crate) fn apply_lag_light(lag: u64) -> Light {
    if lag <= APPLY_LAG_GREEN_MAX {
        Light::Green
    } else if lag <= APPLY_LAG_AMBER_MAX {
        Light::Amber
    } else {
        Light::Red
    }
}

/// The verdict on bytes the frame log keeps until the database confirms them.
pub(crate) fn pinned_bytes_light(bytes: f64) -> Light {
    if bytes <= 0.0 {
        Light::Green
    } else if bytes < PINNED_BYTES_RED_MIN {
        Light::Amber
    } else {
        Light::Red
    }
}

const NOT_REPORTED: &str = "not reported yet";
const COUNTERS_UNREADABLE: &str = "the app's own counters could not be read";

fn unavailable_tile(label: &str) -> Tile {
    Tile::new(label, "not available", Light::Grey).with_detail(COUNTERS_UNREADABLE)
}

/// True when the database's apply backlog is known and at its idle level, so
/// rows written a minute ago are readable now.
pub(crate) fn database_caught_up(inputs: &Inputs) -> bool {
    inputs
        .apply_lag
        .is_some_and(|lag| lag <= APPLY_LAG_GREEN_MAX)
        && !database_not_answering(inputs)
}

/// True when every database read in this pass got no answer.
pub(crate) fn database_not_answering(inputs: &Inputs) -> bool {
    [
        &inputs.ticks,
        &inputs.quotes,
        &inputs.conservation,
        &inputs.gaps,
    ]
    .iter()
    .all(|o| **o == QueryOutcome::Unreachable)
}

/// The catch-up queue tile: read in process from the WAL watcher's last poll.
/// Grey when that reading is unknown or the database is not answering now,
/// so a reading taken before an outage is never shown green during it.
pub(crate) fn apply_lag_tile(inputs: &Inputs) -> Tile {
    const LABEL: &str = "Database catch-up queue";
    if database_not_answering(inputs) {
        return Tile::new(LABEL, "not known", Light::Grey)
            .with_detail("the database is not answering");
    }
    match inputs.apply_lag {
        Some(lag) => Tile::new(
            LABEL,
            format!("{} transactions", group_digits(lag)),
            apply_lag_light(lag),
        )
        .with_detail("green up to 10, red above 500"),
        None => Tile::new(LABEL, "not known", Light::Grey)
            .with_detail("the last check could not read it"),
    }
}

fn backlog_section(inputs: &Inputs) -> Section {
    const PINNED: &str = "Kept on disk until the database confirms";
    let pinned = match inputs.exporter.as_ref() {
        None => unavailable_tile(PINNED),
        Some(e) => match e.pinned_bytes {
            Some(bytes) => Tile::new(PINNED, human_bytes(bytes), pinned_bytes_light(bytes))
                .with_detail(match e.pinned_segments {
                    Some(n) => format!(
                        "{} file(s); measured at start-up and after each catch-up",
                        group_digits(as_count(n))
                    ),
                    None => "measured at start-up and after each catch-up".to_string(),
                }),
            None => Tile::new(PINNED, NOT_REPORTED, Light::Grey),
        },
    };
    Section {
        title: "Backlog waiting to be saved",
        note: "Data received but not yet confirmed by the database. Zero is best.".to_string(),
        tiles: vec![apply_lag_tile(inputs), pinned],
    }
}

/// The verdict on reconnects since the app started.
pub(crate) fn reconnects_light(reconnects: f64) -> Light {
    if reconnects <= 0.0 {
        Light::Green
    } else if reconnects <= RECONNECTS_AMBER_MAX {
        Light::Amber
    } else {
        Light::Red
    }
}

/// The live socket tile: sockets connected right now, from the feed's own
/// count. Grey when the feed has not reported or the market is closed; red
/// when none is connected while the market is open; else green. A green tile
/// means at least one socket is up, not that every socket is.
pub(crate) fn sockets_now_tile(market_open: bool, sockets_now: Option<u64>) -> Tile {
    const LABEL: &str = "Feed sockets connected now";
    match sockets_now {
        None => Tile::new(LABEL, NOT_REPORTED, Light::Grey),
        Some(n) if !market_open => {
            Tile::new(LABEL, group_digits(n), Light::Grey).with_detail("market closed; no verdict")
        }
        Some(0) => Tile::new(LABEL, "0", Light::Red).with_detail("no socket is connected"),
        Some(n) => Tile::new(LABEL, group_digits(n), Light::Green)
            .with_detail("green means at least one is up"),
    }
}

fn connections_section(inputs: &Inputs) -> Section {
    const RECONNECTS: &str = "Socket reconnects since the app started";
    const GAPS: &str = "Connection gaps closed today";
    let mut tiles = Vec::with_capacity(3);
    tiles.push(sockets_now_tile(inputs.market_open, inputs.sockets_now));
    tiles.push(match inputs.exporter.as_ref() {
        None => unavailable_tile(RECONNECTS),
        Some(e) => match e.reconnects {
            Some(n) => Tile::new(RECONNECTS, group_digits(as_count(n)), reconnects_light(n)),
            // Absent also when the feed never dialled, so no verdict.
            None => Tile::new(RECONNECTS, NOT_REPORTED, Light::Grey),
        },
    });
    tiles.push(match &inputs.gaps {
        QueryOutcome::Rows { rows, .. } => {
            let first = rows.first();
            let count = first
                .and_then(|r| r.first())
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let down = first
                .and_then(|r| r.get(1))
                .and_then(Value::as_i64)
                .unwrap_or(0);
            // A gap is written when it closes, so 0 here says nothing about a
            // socket that is down right now: no verdict on 0.
            Tile::new(
                GAPS,
                group_digits(count),
                if count == 0 {
                    Light::Grey
                } else {
                    Light::Amber
                },
            )
            .with_detail(format!(
                "{} down in total; a gap still open is not counted",
                human_secs(down)
            ))
        }
        other => Tile::new(GAPS, "could not read", Light::Grey).with_detail(outcome_reason(other)),
    });
    Section {
        title: "Connections",
        note: "Each gap is a socket that dropped and came back.".to_string(),
        tiles,
    }
}

fn overflow_section(exporter: Option<&ExporterReadings>) -> Section {
    let tiles = match exporter {
        None => vec![
            unavailable_tile("Candle overflow files on disk"),
            unavailable_tile("Candle files set aside"),
            unavailable_tile("Overflow write errors"),
        ],
        Some(e) => vec![
            match e.seal_spill_bytes {
                // The figure includes files already written back (kept for a
                // while) and set-aside files, so it carries no verdict.
                Some(bytes) => Tile::new(
                    "Candle overflow files on disk",
                    human_bytes(bytes),
                    Light::Grey,
                )
                .with_detail("includes files already written back; no verdict"),
                None => Tile::new("Candle overflow files on disk", NOT_REPORTED, Light::Grey),
            },
            match e.seal_refused_files {
                Some(n) => Tile::new(
                    "Candle files set aside",
                    group_digits(as_count(n)),
                    if n <= 0.0 { Light::Green } else { Light::Red },
                )
                .with_detail("kept for review; the database refused them"),
                None => Tile::new("Candle files set aside", NOT_REPORTED, Light::Grey),
            },
            match e.seal_write_errors {
                Some(n) => Tile::new(
                    "Overflow write errors",
                    group_digits(as_count(n)),
                    if n <= 0.0 { Light::Green } else { Light::Red },
                ),
                None => Tile::new("Overflow write errors", NOT_REPORTED, Light::Grey),
            },
        ],
    };
    Section {
        title: "Candle overflow files",
        note: "Candles the database could not take at once are written to disk and saved later."
            .to_string(),
        tiles,
    }
}

fn minute_table_section(outcome: &QueryOutcome) -> Section {
    let tiles = match outcome {
        QueryOutcome::Missing => vec![
            Tile::new("Per-minute speed record", "not yet available", Light::Grey)
                .with_detail("the table that keeps it has not been created yet"),
        ],
        QueryOutcome::Rows { rows, .. } if rows.is_empty() => vec![Tile::new(
            "Per-minute speed record",
            "no rows in 24 hours",
            Light::Grey,
        )],
        QueryOutcome::Rows { columns, rows } => rows
            .iter()
            .map(|row| {
                let (label, value) = describe_row(columns, row);
                Tile::new(label, value, Light::Grey)
            })
            .collect(),
        other => vec![
            Tile::new("Per-minute speed record", "could not read", Light::Grey)
                .with_detail(outcome_reason(other)),
        ],
    };
    Section {
        title: "Per-minute speed record",
        note: "Saved once a minute, newest first, shown as stored (no verdict).".to_string(),
        tiles,
    }
}

/// One stored row as a tile: text and time cells form the label, numbers the
/// value (`name number`). Every cell is shortened to [`CELL_MAX_CHARS`].
pub(crate) fn describe_row(columns: &[String], row: &[Value]) -> (String, String) {
    let mut label: Vec<String> = Vec::new();
    let mut value: Vec<String> = Vec::new();
    for (name, cell) in columns.iter().zip(row.iter()) {
        match cell {
            Value::String(s) => label.push(short_text(s)),
            Value::Number(n) => value.push(format!(
                "{} {}",
                short_text(name),
                short_text(&n.to_string())
            )),
            Value::Bool(b) => value.push(format!("{} {b}", short_text(name))),
            _ => {}
        }
    }
    let label = if label.is_empty() {
        "row".to_string()
    } else {
        label.join(" · ")
    };
    let value = if value.is_empty() {
        "no numbers".to_string()
    } else {
        value.join(", ")
    };
    (label, value)
}

// ---------------------------------------------------------------------------
// QuestDB reads
// ---------------------------------------------------------------------------

/// What a query returned.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum QueryOutcome {
    /// The column names and rows.
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
    },
    /// The table does not exist (yet).
    Missing,
    /// The database answered with an error other than a missing table.
    Refused,
    /// No answer (down, timed out, or not JSON).
    Unreachable,
}

/// A short plain-English reason for a non-row outcome.
pub(crate) fn outcome_reason(outcome: &QueryOutcome) -> &'static str {
    match outcome {
        QueryOutcome::Rows { .. } => "",
        QueryOutcome::Missing => "the table does not exist yet",
        QueryOutcome::Refused => "the database refused the query",
        QueryOutcome::Unreachable => "the database did not answer",
    }
}

/// Classifies a QuestDB `/exec` JSON body.
pub(crate) fn classify_exec_body(body: &Value) -> QueryOutcome {
    if let Some(error) = body.get("error").and_then(Value::as_str) {
        return if error.contains("does not exist") {
            QueryOutcome::Missing
        } else {
            QueryOutcome::Refused
        };
    }
    let columns = body.get("columns").and_then(Value::as_array).map(|cols| {
        cols.iter()
            .map(|c| {
                c.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            })
            .collect::<Vec<_>>()
    });
    let rows = body.get("dataset").and_then(Value::as_array).map(|rows| {
        rows.iter()
            .map(|r| r.as_array().cloned().unwrap_or_default())
            .collect::<Vec<_>>()
    });
    match (columns, rows) {
        (Some(columns), Some(rows)) => QueryOutcome::Rows { columns, rows },
        _ => QueryOutcome::Unreachable,
    }
}

/// Runs one read-only query with its own timeout.
async fn query_rows(client: &reqwest::Client, base_url: &str, sql: &str) -> QueryOutcome {
    let Ok(resp) = client
        .get(format!("{base_url}/exec"))
        .query(&[("query", sql)])
        .timeout(std::time::Duration::from_secs(
            SPEED_HEALTH_QUERY_TIMEOUT_SECS,
        ))
        .send()
        .await
    else {
        return QueryOutcome::Unreachable;
    };
    match resp.json::<Value>().await {
        Ok(body) => classify_exec_body(&body),
        Err(_) => QueryOutcome::Unreachable,
    }
}

/// The designated timestamp column of the per-minute table, from its
/// `table_columns` rows; `None` when there is none or its name is not plain.
pub(crate) fn designated_column(outcome: &QueryOutcome) -> Option<String> {
    let QueryOutcome::Rows { columns, rows } = outcome else {
        return None;
    };
    rows.iter().find_map(|row| {
        let name = column_value(columns, row, "column")?.as_str()?;
        let designated = column_value(columns, row, "designated")?.as_bool()?;
        (designated && is_plain_identifier(name)).then(|| name.to_string())
    })
}

/// Reads the per-minute table: its columns first (which also says whether it
/// exists), then its newest rows inside the lookback window.
async fn read_minute_table(client: &reqwest::Client, base_url: &str, ist_now: i64) -> QueryOutcome {
    let columns = query_rows(client, base_url, &minute_table_columns_sql()).await;
    match &columns {
        QueryOutcome::Rows { .. } => match designated_column(&columns) {
            Some(ts) => query_rows(client, base_url, &minute_table_rows_sql(&ts, ist_now)).await,
            // A table with no designated timestamp cannot be read in a
            // bounded window, so it is not read at all.
            None => QueryOutcome::Refused,
        },
        _ => columns,
    }
}

// ---------------------------------------------------------------------------
// The app's own metrics page
// ---------------------------------------------------------------------------

/// The metrics-page address for `[observability] metrics_bind_addr` and
/// `metrics_port`. A wildcard bind address is read on the matching loopback
/// address; an IPv6 address is bracketed.
#[must_use]
pub fn metrics_scrape_url_for(bind: IpAddr, port: u16) -> String {
    let host = match bind {
        IpAddr::V4(v4) if v4.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(v6) if v6.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        other => other,
    };
    match host {
        IpAddr::V4(v4) => format!("http://{v4}:{port}/metrics"),
        IpAddr::V6(v6) => format!("http://[{v6}]:{port}/metrics"),
    }
}

/// Reads the metrics page, or `None` when no address is set, it does not
/// answer, or it is larger than [`METRICS_SCRAPE_MAX_BYTES`].
async fn fetch_metrics_text(client: &reqwest::Client, url: Option<&str>) -> Option<String> {
    let url = url?;
    let mut resp = client
        .get(url)
        .timeout(std::time::Duration::from_secs(METRICS_SCRAPE_TIMEOUT_SECS))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.ok()? {
        if body.len().saturating_add(chunk.len()) > METRICS_SCRAPE_MAX_BYTES {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).ok()
}

/// The figures read from the metrics page. `None` = the series is not on the
/// page (never reported in this process).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ExporterReadings {
    /// Worst sample per step in the last 75 s, nanoseconds, by step index.
    pub stage_max_75s: [Option<u64>; 5],
    pub pinned_bytes: Option<f64>,
    pub pinned_segments: Option<f64>,
    /// Sum over every label set.
    pub reconnects: Option<f64>,
    pub seal_spill_bytes: Option<f64>,
    pub seal_refused_files: Option<f64>,
    /// Sum over every label set.
    pub seal_write_errors: Option<f64>,
}

fn add(slot: &mut Option<f64>, value: f64) {
    *slot = Some(slot.unwrap_or(0.0) + value);
}

fn max_into(slot: &mut Option<f64>, value: f64) {
    *slot = Some(slot.map_or(value, |old| old.max(value)));
}

/// Reads the wanted series from the metrics text in ONE pass, O(lines).
/// Comment lines, unknown series and non-finite values are skipped.
pub(crate) fn parse_metrics_text(text: &str) -> ExporterReadings {
    use tickvault_core::websocket::pool_supervisor::RECONNECT_METRIC;
    use tickvault_storage::seal_spill::SEAL_SPILL_REFUSED_FILES_GAUGE;
    use tickvault_storage::wal_unapplied_ranges::{
        WAL_UNAPPLIED_PINNED_BYTES_GAUGE, WAL_UNAPPLIED_PINNED_SEGMENTS_GAUGE,
    };

    let mut out = ExporterReadings::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((series, raw_value)) = line.rsplit_once(char::is_whitespace) else {
            continue;
        };
        let Ok(value) = raw_value.trim().parse::<f64>() else {
            continue;
        };
        if !value.is_finite() {
            continue;
        }
        let series = series.trim();
        let (name, labels) = match series.split_once('{') {
            Some((name, rest)) => (name, rest.trim_end_matches('}')),
            None => (series, ""),
        };
        match name {
            STAGE_MAX_75S_METRIC => {
                let stage = label_value(labels, "stage");
                if let Some(i) = ALL_STAGES.iter().position(|s| Some(s.as_str()) == stage) {
                    // A finite, non-negative gauge in nanoseconds; the cast
                    // saturates and a negative reads 0.
                    out.stage_max_75s[i] = Some(as_count(value));
                }
            }
            n if n == WAL_UNAPPLIED_PINNED_BYTES_GAUGE => max_into(&mut out.pinned_bytes, value),
            n if n == WAL_UNAPPLIED_PINNED_SEGMENTS_GAUGE => {
                max_into(&mut out.pinned_segments, value);
            }
            n if n == RECONNECT_METRIC => add(&mut out.reconnects, value),
            SEAL_SPILL_BYTES_METRIC => add(&mut out.seal_spill_bytes, value),
            n if n == SEAL_SPILL_REFUSED_FILES_GAUGE => add(&mut out.seal_refused_files, value),
            SEAL_SPILL_WRITE_ERRORS_METRIC => add(&mut out.seal_write_errors, value),
            _ => {}
        }
    }
    out
}

/// The value of one `key="value"` label, or `None`.
pub(crate) fn label_value<'a>(labels: &'a str, key: &str) -> Option<&'a str> {
    labels.split(',').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k.trim() == key).then(|| v.trim().trim_matches('"'))
    })
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

fn column_value<'a>(columns: &[String], row: &'a [Value], name: &str) -> Option<&'a Value> {
    let i = columns.iter().position(|c| c == name)?;
    row.get(i)
}

/// IST wall clock now, as epoch seconds (the convention the tables use).
fn ist_now_secs() -> i64 {
    chrono::Utc::now().timestamp() + tickvault_common::constants::IST_UTC_OFFSET_SECONDS_I64
}

/// An IST epoch second as a QuestDB timestamp literal.
pub(crate) fn ist_literal(ist_secs: i64) -> String {
    chrono::DateTime::from_timestamp(ist_secs, 0).map_or_else(
        || "1970-01-01T00:00:00.000000Z".to_string(),
        |t| t.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string(),
    )
}

/// Parses a QuestDB timestamp string (`2026-10-30T09:15:00.000000Z`) to
/// epoch seconds.
pub(crate) fn parse_ts_secs(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|t| t.timestamp())
}

/// A finite non-negative float as a whole count (saturating; negative → 0).
fn as_count(value: f64) -> u64 {
    if value.is_finite() && value > 0.0 {
        // Float-to-int `as` saturates: a finite, positive gauge reading; precision past 2^53 is irrelevant here.
        // APPROVED: saturating display-only cast of a finite positive gauge.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let n = value.round() as u64;
        n
    } else {
        0
    }
}

/// Nanoseconds in the largest unit that keeps the number readable.
pub(crate) fn human_nanos(nanos: u64) -> String {
    // APPROVED: display-only float conversion of a duration.
    #[allow(clippy::cast_precision_loss)]
    let n = nanos as f64;
    if nanos < 1_000 {
        format!("{nanos} ns")
    } else if nanos < 1_000_000 {
        format!("{:.1} µs", n / 1e3)
    } else if nanos < 1_000_000_000 {
        format!("{:.1} ms", n / 1e6)
    } else {
        format!("{:.1} s", n / 1e9)
    }
}

/// Bytes in KB / MB / GB (binary units, labelled plainly).
pub(crate) fn human_bytes(bytes: f64) -> String {
    const KIB: f64 = 1024.0;
    if !bytes.is_finite() || bytes <= 0.0 {
        "0 bytes".to_string()
    } else if bytes < KIB {
        format!("{} bytes", as_count(bytes))
    } else if bytes < KIB * KIB {
        format!("{:.1} KB", bytes / KIB)
    } else if bytes < KIB * KIB * KIB {
        format!("{:.1} MB", bytes / (KIB * KIB))
    } else {
        format!("{:.1} GB", bytes / (KIB * KIB * KIB))
    }
}

/// Whole seconds as `Xh Ym Zs`, dropping leading zero units.
pub(crate) fn human_secs(secs: i64) -> String {
    let secs = secs.max(0);
    let (h, m, s) = (
        secs / SECS_PER_HOUR,
        (secs % SECS_PER_HOUR) / SECS_PER_MINUTE,
        secs % SECS_PER_MINUTE,
    );
    if h > 0 {
        format!("{h}h {m}m {s}s")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

/// `1234567` → `1,234,567`.
pub(crate) fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn format_signed(n: i64) -> String {
    if n < 0 {
        format!("-{}", group_digits(n.unsigned_abs()))
    } else {
        group_digits(n.unsigned_abs())
    }
}

/// Shortens stored text to [`CELL_MAX_CHARS`] characters (on a character
/// boundary) so one odd cell cannot stretch the page.
pub(crate) fn short_text(text: &str) -> String {
    match text.char_indices().nth(CELL_MAX_CHARS) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed_state::FeedRuntimeState;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tickvault_common::config::{DhanConfig, FeedsConfig, InstrumentConfig, QuestDbConfig};

    /// 2026-10-30 09:30:25 IST as IST epoch seconds (IST wall time stored as
    /// if UTC, the tables' convention).
    const IST_0930_25: i64 = 1_793_352_625;

    fn rows(columns: &[&str], data: Vec<Vec<Value>>) -> QueryOutcome {
        QueryOutcome::Rows {
            columns: columns.iter().map(|c| (*c).to_string()).collect(),
            rows: data,
        }
    }

    fn empty_stage(stage: Stage) -> StageFigures {
        StageFigures {
            stage,
            count: 0,
            sum_nanos: 0,
            over_budget: 0,
            buckets: [0; STAGE_BUCKETS],
        }
    }

    fn all_down_inputs(market_open: bool) -> Inputs {
        Inputs {
            market_open,
            ist_now: IST_0930_25,
            apply_lag: None,
            sockets_now: None,
            stages: ALL_STAGES.map(empty_stage),
            exporter: None,
            ticks: QueryOutcome::Unreachable,
            quotes: QueryOutcome::Unreachable,
            conservation: QueryOutcome::Unreachable,
            gaps: QueryOutcome::Unreachable,
            minute_table: QueryOutcome::Unreachable,
        }
    }

    fn all_tiles(resp: &SpeedHealthResponse) -> Vec<&Tile> {
        resp.sections.iter().flat_map(|s| s.tiles.iter()).collect()
    }

    #[test]
    fn test_ist_literal_and_parse_ts_secs_agree_on_the_constant() {
        assert_eq!(ist_literal(IST_0930_25), "2026-10-30T09:30:25.000000Z");
        assert_eq!(
            parse_ts_secs("2026-10-30T09:30:25.000000Z"),
            Some(IST_0930_25)
        );
        assert_eq!(parse_ts_secs("not a time"), None);
    }

    #[test]
    fn test_ticks_per_minute_sql_and_tick_window_cover_the_last_15_full_minutes() {
        assert_eq!(
            ticks_per_minute_sql(TickTable::Trades, IST_0930_25),
            "SELECT ts, count() FROM ticks WHERE ts >= '2026-10-30T09:15:00.000000Z' \
             AND ts < '2026-10-30T09:30:00.000000Z' SAMPLE BY 1m ALIGN TO CALENDAR"
        );
        assert!(
            ticks_per_minute_sql(TickTable::RepeatQuotes, IST_0930_25)
                .starts_with("SELECT ts, count() FROM tick_quotes WHERE ts >= ")
        );
        // On an exact minute boundary the minute just starting is excluded.
        let (start, end) = tick_window(IST_0930_25 - 25);
        assert_eq!(end, IST_0930_25 - 25);
        assert_eq!(end - start, 15 * 60);
    }

    #[test]
    fn test_conservation_sql_and_feed_gaps_sql_are_time_bounded_and_read_only() {
        let c = conservation_sql(IST_0930_25);
        assert!(c.starts_with("SELECT trading_date_ist, feed, verdict, reason,"));
        assert!(
            c.contains("FROM tick_conservation_audit WHERE ts >= '2026-10-16T09:30:25.000000Z'")
        );
        assert!(c.ends_with("ORDER BY ts DESC LIMIT 5"));
        assert_eq!(
            feed_gaps_sql(IST_0930_25),
            "SELECT count(), sum(down_secs) FROM feed_gap_audit WHERE ts >= \
             '2026-10-30T00:00:00.000000Z'"
        );
        for sql in [
            c,
            feed_gaps_sql(IST_0930_25),
            ticks_per_minute_sql(TickTable::Trades, IST_0930_25),
            minute_table_columns_sql(),
            minute_table_rows_sql("ts", IST_0930_25),
        ] {
            let upper = sql.to_ascii_uppercase();
            assert!(upper.starts_with("SELECT "), "read-only: {sql}");
            for verb in ["INSERT", "UPDATE", "DELETE", "DROP", "ALTER", "TRUNCATE"] {
                assert!(!upper.contains(verb), "{verb} in {sql}");
            }
        }
    }

    #[test]
    fn test_minute_table_columns_sql_and_minute_table_rows_sql_use_a_24h_window() {
        assert_eq!(
            minute_table_columns_sql(),
            "SELECT \"column\", designated FROM table_columns('hot_path_latency_minute')"
        );
        assert_eq!(
            minute_table_rows_sql("ts", IST_0930_25),
            "SELECT * FROM hot_path_latency_minute WHERE ts >= '2026-10-29T09:30:25.000000Z' \
             ORDER BY ts DESC LIMIT 20"
        );
    }

    #[test]
    fn test_is_plain_identifier_refuses_anything_that_could_change_a_query() {
        assert!(is_plain_identifier("ts"));
        assert!(is_plain_identifier("minute_ts_2"));
        for bad in [
            "",
            "ts; DROP TABLE x",
            "ts--",
            "a b",
            "t\"s",
            "ts'",
            &"x".repeat(65),
        ] {
            assert!(!is_plain_identifier(bad), "accepted {bad:?}");
        }
    }

    #[test]
    fn test_designated_column_reads_table_columns_and_refuses_odd_names() {
        let cols = ["column", "type", "designated"];
        let ok = rows(
            &cols,
            vec![
                vec![json_str("stage"), json_str("SYMBOL"), Value::Bool(false)],
                vec![json_str("ts"), json_str("TIMESTAMP"), Value::Bool(true)],
            ],
        );
        assert_eq!(designated_column(&ok).as_deref(), Some("ts"));
        let odd = rows(
            &cols,
            vec![vec![
                json_str("t s"),
                json_str("TIMESTAMP"),
                Value::Bool(true),
            ]],
        );
        assert_eq!(designated_column(&odd), None);
        let none = rows(
            &cols,
            vec![vec![json_str("x"), json_str("LONG"), Value::Bool(false)]],
        );
        assert_eq!(designated_column(&none), None);
        assert_eq!(designated_column(&QueryOutcome::Missing), None);
    }

    fn json_str(s: &str) -> Value {
        Value::String(s.to_string())
    }

    #[test]
    fn test_classify_exec_body_and_outcome_reason_separate_missing_refused_rows_garbage() {
        let missing = serde_json::json!({"query":"x","error":"table does not exist [table=hot_path_latency_minute]","position":14});
        assert_eq!(classify_exec_body(&missing), QueryOutcome::Missing);
        let refused = serde_json::json!({"query":"x","error":"Invalid column: foo","position":7});
        assert_eq!(classify_exec_body(&refused), QueryOutcome::Refused);
        let ok = serde_json::json!({"columns":[{"name":"ts","type":"TIMESTAMP"},{"name":"count","type":"LONG"}],"dataset":[["2026-10-30T09:15:00.000000Z",7]]});
        assert_eq!(
            classify_exec_body(&ok),
            rows(
                &["ts", "count"],
                vec![vec![
                    json_str("2026-10-30T09:15:00.000000Z"),
                    Value::from(7)
                ]]
            )
        );
        assert_eq!(
            classify_exec_body(&serde_json::json!({"hello":1})),
            QueryOutcome::Unreachable
        );
        assert_eq!(
            outcome_reason(&QueryOutcome::Missing),
            "the table does not exist yet"
        );
        assert_eq!(
            outcome_reason(&QueryOutcome::Refused),
            "the database refused the query"
        );
        assert_eq!(
            outcome_reason(&QueryOutcome::Unreachable),
            "the database did not answer"
        );
    }

    #[test]
    fn test_minute_counts_fills_quiet_minutes_with_zero_and_ignores_outside_rows() {
        let outcome = rows(
            &["ts", "count"],
            vec![
                vec![json_str("2026-10-30T09:15:00.000000Z"), Value::from(10)],
                vec![json_str("2026-10-30T09:29:00.000000Z"), Value::from(40)],
                // Outside the window (the minute still filling) and before it.
                vec![json_str("2026-10-30T09:30:00.000000Z"), Value::from(99)],
                vec![json_str("2026-10-30T09:14:00.000000Z"), Value::from(99)],
                vec![json_str("garbage"), Value::from(99)],
            ],
        );
        let counts = minute_counts(&outcome, IST_0930_25).expect("rows");
        assert_eq!(counts.len(), 15);
        assert_eq!(counts[0], 10);
        assert_eq!(counts[14], 40);
        assert_eq!(counts.iter().sum::<u64>(), 50);
        assert_eq!(minute_counts(&QueryOutcome::Unreachable, IST_0930_25), None);
    }

    #[test]
    fn test_ticks_light_red_amber_green_grey() {
        let mut full = vec![5u64; 15];
        assert_eq!(
            ticks_light(true, Some(&full), IST_0930_25, true),
            Light::Green
        );
        assert_eq!(
            ticks_light(false, Some(&full), IST_0930_25, true),
            Light::Grey
        );
        assert_eq!(ticks_light(true, None, IST_0930_25, true), Light::Grey);
        full[3] = 0;
        assert_eq!(
            ticks_light(true, Some(&full), IST_0930_25, true),
            Light::Amber
        );
        full[14] = 0;
        assert_eq!(
            ticks_light(true, Some(&full), IST_0930_25, true),
            Light::Red
        );
        // The same empty minute while the database is behind: not "lost".
        assert_eq!(
            ticks_light(true, Some(&full), IST_0930_25, false),
            Light::Amber
        );
        // At 09:20 the window starts at 09:05: quiet pre-open minutes are
        // not a fault.
        let at_0920 = IST_0930_25 - 10 * 60;
        let mut early = vec![0u64; 15];
        for n in early.iter_mut().skip(10) {
            *n = 3;
        }
        assert_eq!(ticks_light(true, Some(&early), at_0920, true), Light::Green);
    }

    #[test]
    fn test_ticks_light_gives_no_verdict_outside_continuous_trading() {
        let empty = vec![0u64; 15];
        // 09:00:30 (last full minute 08:59), 09:05 (pre-open), 15:35 and
        // 15:39 (closing session): the market is "open" but no trade is due.
        for ist in [
            IST_0930_25 - 29 * 60 - 55,
            IST_0930_25 - 25 * 60,
            IST_0930_25 + 6 * 3_600 + 5 * 60,
            IST_0930_25 + 6 * 3_600 + 9 * 60,
        ] {
            assert_eq!(
                ticks_light(true, Some(&empty), ist, true),
                Light::Grey,
                "at {}",
                ist_literal(ist)
            );
        }
        // 09:16:05: the last full minute 09:15 is judged.
        assert_eq!(
            ticks_light(true, Some(&empty), IST_0930_25 - 14 * 60 - 20, true),
            Light::Red
        );
        // 15:30:10: the last full minute 15:29 is still judged.
        assert_eq!(
            ticks_light(true, Some(&empty), IST_0930_25 + 6 * 3_600 - 15, true),
            Light::Red
        );
    }

    #[test]
    fn test_in_continuous_trading_edges_and_continuous_trading_close_secs_of_day() {
        assert_eq!(
            continuous_trading_close_secs_of_day(),
            CONTINUOUS_TRADING_CLOSE_SECS_OF_DAY_IST
        );
        let day = ist_day_at(IST_0930_25, 0);
        assert!(!in_continuous_trading(day + 9 * 3_600 + 14 * 60));
        assert!(in_continuous_trading(day + 9 * 3_600 + 15 * 60));
        assert!(in_continuous_trading(day + 15 * 3_600 + 29 * 60));
        assert!(!in_continuous_trading(day + 15 * 3_600 + 30 * 60));
        let close = tickvault_common::constants::MARKET_CLOSE_TIME_IST_EXCLUSIVE;
        let parsed = chrono::NaiveTime::parse_from_str(close, "%H:%M:%S").expect("time");
        assert_eq!(
            i64::from(chrono::Timelike::num_seconds_from_midnight(&parsed)),
            CONTINUOUS_TRADING_CLOSE_SECS_OF_DAY_IST
        );
    }

    #[test]
    fn test_stage_light_honours_bucket_resolution() {
        let budget = 100_000;
        assert_eq!(stage_light(None, budget), Light::Grey);
        assert_eq!(
            stage_light(Some(QuantileBound::Overflow), budget),
            Light::Red
        );
        assert_eq!(
            stage_light(Some(QuantileBound::AtMost(65_536)), budget),
            Light::Green
        );
        assert_eq!(
            stage_light(Some(QuantileBound::AtMost(budget)), budget),
            Light::Green
        );
        // (65 µs, 131 µs]: possibly over a 100 µs budget.
        assert_eq!(
            stage_light(Some(QuantileBound::AtMost(131_072)), budget),
            Light::Amber
        );
        // (131 µs, 262 µs]: certainly over.
        assert_eq!(
            stage_light(Some(QuantileBound::AtMost(262_144)), budget),
            Light::Red
        );
    }

    #[test]
    fn test_speed_section_reads_quantiles_and_the_75s_worst() {
        let mut fig = empty_stage(Stage::SocketToWal);
        // 100 samples in bucket 3 (at most 8,192 ns), one in bucket 8.
        fig.buckets[3] = 100;
        fig.buckets[8] = 1;
        fig.count = 101;
        fig.sum_nanos = 101 * 5_000;
        fig.over_budget = 1;
        let mut stages = ALL_STAGES.map(empty_stage);
        stages[0] = fig;
        let mut exporter = ExporterReadings::default();
        exporter.stage_max_75s[0] = Some(250_000);
        let section = speed_section(&stages, Some(&exporter));
        let tile = &section.tiles[0];
        assert_eq!(tile.label, "Saving each message as it arrives");
        assert_eq!(tile.value, "99% at most 8.2 µs");
        assert_eq!(tile.status, Light::Green);
        assert!(tile.detail.contains("average 5.0 µs"), "{}", tile.detail);
        assert!(tile.detail.contains("limit 100.0 µs"), "{}", tile.detail);
        assert!(
            tile.detail.contains("1 of 101 over the limit"),
            "{}",
            tile.detail
        );
        assert!(
            tile.detail.contains("worst in the last 75 s 250.0 µs"),
            "{}",
            tile.detail
        );
        // A step with no samples is grey and says so.
        assert_eq!(section.tiles[1].status, Light::Grey);
        assert_eq!(section.tiles[1].value, "no samples yet");
        assert_eq!(section.tiles.len(), ALL_STAGES.len());
    }

    const METRICS_SAMPLE: &str = "\
# HELP tv_hot_path_stage_max_ns_75s worst sample
# TYPE tv_hot_path_stage_max_ns_75s gauge
tv_hot_path_stage_max_ns_75s{stage=\"socket_to_wal\"} 32768
tv_hot_path_stage_max_ns_75s{stage=\"drain_main_frame\"} 1048576
tv_hot_path_stage_max_ns_75s{stage=\"no_such_stage\"} 7
tv_questdb_wal_apply_lag_max 42
tv_wal_unapplied_pinned_bytes 1073741824
tv_wal_unapplied_pinned_segments 8
tv_dhan_ws_reconnect_total{reason=\"idle\",slot=\"0\"} 2
tv_dhan_ws_reconnect_total{reason=\"auth_rejected\",slot=\"3\"} 1
tv_seal_spill_bytes 0
tv_seal_spill_refused_files NaN
tv_seal_spill_write_errors_total{stage=\"write\"} 0
tv_seal_spill_write_errors_total{stage=\"sync\"} 0
tv_unrelated_total 999
garbage line without a number x
";

    #[test]
    fn test_parse_metrics_text_reads_wanted_series_in_one_pass() {
        let r = parse_metrics_text(METRICS_SAMPLE);
        assert_eq!(r.stage_max_75s[Stage::SocketToWal as usize], Some(32_768));
        assert_eq!(
            r.stage_max_75s[Stage::DrainMainFrame as usize],
            Some(1_048_576)
        );
        assert_eq!(r.stage_max_75s[Stage::RingDwell as usize], None);
        assert_eq!(r.pinned_bytes, Some(1_073_741_824.0));
        assert_eq!(r.pinned_segments, Some(8.0));
        assert_eq!(r.reconnects, Some(3.0), "summed over label sets");
        assert_eq!(r.seal_spill_bytes, Some(0.0));
        assert_eq!(r.seal_refused_files, None, "NaN is skipped, never shown");
        assert_eq!(r.seal_write_errors, Some(0.0));
        assert_eq!(parse_metrics_text(""), ExporterReadings::default());
    }

    #[test]
    fn test_metric_names_match_the_producers() {
        // The literals here must equal what the producers publish; the
        // exported constants are used directly where they exist.
        assert_eq!(STAGE_MAX_75S_METRIC, "tv_hot_path_stage_max_ns_75s");
        assert_eq!(SEAL_SPILL_BYTES_METRIC, "tv_seal_spill_bytes");
        assert_eq!(
            SEAL_SPILL_WRITE_ERRORS_METRIC,
            "tv_seal_spill_write_errors_total"
        );
    }

    #[test]
    fn test_label_value_finds_one_label() {
        assert_eq!(
            label_value("stage=\"ring_dwell\"", "stage"),
            Some("ring_dwell")
        );
        assert_eq!(label_value("a=\"1\",stage=\"x\"", "stage"), Some("x"));
        assert_eq!(label_value("a=\"1\"", "stage"), None);
        assert_eq!(label_value("", "stage"), None);
    }

    #[test]
    fn test_metrics_scrape_url_for_every_address_shape() {
        use std::net::{Ipv4Addr, Ipv6Addr};
        assert_eq!(
            metrics_scrape_url_for(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)), 9091),
            "http://10.0.0.5:9091/metrics"
        );
        assert_eq!(
            metrics_scrape_url_for(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 9091),
            format!("http://{}:9091/metrics", Ipv4Addr::LOCALHOST)
        );
        assert_eq!(
            metrics_scrape_url_for(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 9091),
            "http://[::1]:9091/metrics"
        );
        assert_eq!(
            metrics_scrape_url_for(IpAddr::V6(Ipv6Addr::LOCALHOST), 1),
            "http://[::1]:1/metrics"
        );
    }

    #[test]
    fn test_apply_lag_light_pinned_bytes_light_reconnects_light_conservation_light_conservation_word()
     {
        assert_eq!(apply_lag_light(0), Light::Green);
        assert_eq!(apply_lag_light(10), Light::Green);
        assert_eq!(apply_lag_light(11), Light::Amber);
        assert_eq!(apply_lag_light(500), Light::Amber);
        assert_eq!(apply_lag_light(501), Light::Red);
        assert_eq!(pinned_bytes_light(0.0), Light::Green);
        assert_eq!(pinned_bytes_light(1.0), Light::Amber);
        assert_eq!(pinned_bytes_light(PINNED_BYTES_RED_MIN), Light::Red);
        assert_eq!(reconnects_light(0.0), Light::Green);
        assert_eq!(reconnects_light(5.0), Light::Amber);
        assert_eq!(reconnects_light(6.0), Light::Red);
        assert_eq!(conservation_light("match"), Light::Green);
        assert_eq!(conservation_light("partial"), Light::Amber);
        assert_eq!(conservation_light("unverified"), Light::Amber);
        assert_eq!(conservation_light("mismatch"), Light::Red);
        assert_eq!(conservation_light(""), Light::Grey);
        assert_eq!(conservation_word("mismatch"), "ticks missing");
        assert_eq!(conservation_word("weird"), "unknown result");
    }

    #[test]
    fn test_conservation_section_maps_each_row_and_never_shows_minus_one() {
        let outcome = rows(
            &[
                "trading_date_ist",
                "feed",
                "verdict",
                "reason",
                "accepted_rows",
                "db_tick_rows",
                "not_landed_rows",
                "shortfall",
            ],
            vec![
                vec![
                    json_str("2026-10-29T00:00:00.000000Z"),
                    json_str("dhan"),
                    json_str("match"),
                    json_str("counted"),
                    Value::from(64_349_753),
                    Value::from(64_349_753),
                    Value::from(0),
                    Value::from(0),
                ],
                vec![
                    json_str("2026-10-28T00:00:00.000000Z"),
                    json_str("dhan"),
                    json_str("unverified"),
                    json_str("db_unreadable"),
                    Value::from(10),
                    Value::from(-1),
                    Value::from(0),
                    Value::from(-3),
                ],
            ],
        );
        let section = conservation_section(&outcome, IST_0930_25);
        assert_eq!(section.tiles.len(), 3);
        assert_eq!(section.tiles[0].label, "Newest daily tick check");
        assert_eq!(section.tiles[0].value, "1 day ago");
        assert_eq!(section.tiles[0].status, Light::Green);
        assert_eq!(section.tiles[1].label, "2026-10-29 · dhan");
        assert_eq!(section.tiles[1].value, "every tick counted");
        assert_eq!(section.tiles[1].status, Light::Green);
        assert!(section.tiles[1].detail.contains("received 64,349,753"));
        assert_eq!(section.tiles[2].status, Light::Amber);
        assert!(section.tiles[2].detail.contains("in the database unknown"));
        assert!(section.tiles[2].detail.contains("short by -3"));
        assert!(section.tiles[2].detail.contains("reason db unreadable"));

        // The same rows read eight days later: old greens, so the freshness
        // tile goes amber.
        let late = conservation_section(&outcome, IST_0930_25 + 7 * 86_400);
        assert_eq!(late.tiles[0].value, "8 days ago");
        assert_eq!(late.tiles[0].status, Light::Amber);

        let none = conservation_section(&rows(&["verdict"], vec![]), IST_0930_25);
        assert_eq!(none.tiles[0].value, "no check in 14 days");
        assert_eq!(none.tiles[0].status, Light::Grey);
        let down = conservation_section(&QueryOutcome::Unreachable, IST_0930_25);
        assert_eq!(down.tiles[0].status, Light::Grey);
    }

    #[test]
    fn test_apply_lag_tile_database_caught_up_and_database_not_answering() {
        let mut inputs = all_down_inputs(true);
        // A last reading of 0 taken before the database stopped answering is
        // not shown green.
        inputs.apply_lag = Some(0);
        assert!(database_not_answering(&inputs));
        assert!(!database_caught_up(&inputs));
        let stale = apply_lag_tile(&inputs);
        assert_eq!(
            (stale.value.as_str(), stale.status),
            ("not known", Light::Grey)
        );
        // The database answers: the reading counts.
        inputs.gaps = rows(&["count", "sum"], vec![vec![Value::from(0), Value::Null]]);
        assert!(!database_not_answering(&inputs));
        assert!(database_caught_up(&inputs));
        let live = apply_lag_tile(&inputs);
        assert_eq!(
            (live.value.as_str(), live.status),
            ("0 transactions", Light::Green)
        );
        inputs.apply_lag = Some(11);
        assert!(!database_caught_up(&inputs));
        assert_eq!(apply_lag_tile(&inputs).status, Light::Amber);
        // Unknown (watcher blind) is grey, not 0.
        inputs.apply_lag = None;
        assert!(!database_caught_up(&inputs));
        let unknown = apply_lag_tile(&inputs);
        assert_eq!(
            (unknown.value.as_str(), unknown.status),
            ("not known", Light::Grey)
        );
    }

    #[test]
    fn test_sockets_now_tile_every_case() {
        assert_eq!(sockets_now_tile(true, None).status, Light::Grey);
        assert_eq!(sockets_now_tile(false, Some(0)).status, Light::Grey);
        assert_eq!(sockets_now_tile(true, Some(0)).status, Light::Red);
        let up = sockets_now_tile(true, Some(16));
        assert_eq!((up.value.as_str(), up.status), ("16", Light::Green));
    }

    #[test]
    fn test_connections_section_absent_counter_and_zero_closed_gaps_are_grey() {
        let mut inputs = all_down_inputs(true);
        inputs.exporter = Some(ExporterReadings::default());
        inputs.gaps = rows(&["count", "sum"], vec![vec![Value::from(0), Value::Null]]);
        let section = connections_section(&inputs);
        assert_eq!(section.tiles.len(), 3);
        for tile in &section.tiles {
            assert_eq!(
                tile.status,
                Light::Grey,
                "{} must carry no verdict",
                tile.label
            );
        }
        assert_eq!(section.tiles[1].value, NOT_REPORTED);
        assert_eq!(section.tiles[2].value, "0");
    }

    #[test]
    fn test_conservation_freshness_tile_unknown_day_and_today() {
        let cols = vec!["trading_date_ist".to_string()];
        let unknown = conservation_freshness_tile(&cols, &[vec![Value::Null]], IST_0930_25);
        assert_eq!(
            (unknown.value.as_str(), unknown.status),
            ("unknown day", Light::Grey)
        );
        let today = conservation_freshness_tile(
            &cols,
            &[vec![json_str("2026-10-30T00:00:00.000000Z")]],
            IST_0930_25,
        );
        assert_eq!(
            (today.value.as_str(), today.status),
            ("today", Light::Green)
        );
        let four = conservation_freshness_tile(
            &cols,
            &[vec![json_str("2026-10-26T00:00:00.000000Z")]],
            IST_0930_25,
        );
        assert_eq!(
            (four.value.as_str(), four.status),
            ("4 days ago", Light::Green)
        );
    }

    #[test]
    fn test_every_unreadable_source_is_grey_never_green() {
        // The false-OK guard: with every source down nothing may read green.
        for market_open in [true, false] {
            let resp = build_sections(&all_down_inputs(market_open));
            assert_eq!(resp.sections.len(), 7);
            for tile in all_tiles(&resp) {
                assert_eq!(
                    tile.status,
                    Light::Grey,
                    "{} read {:?} with every source down",
                    tile.label,
                    tile.status
                );
            }
        }
    }

    #[test]
    fn test_build_sections_full_inputs_give_every_section_a_verdict() {
        let mut inputs = all_down_inputs(true);
        inputs.exporter = Some(parse_metrics_text(METRICS_SAMPLE));
        inputs.ticks = rows(
            &["ts", "count"],
            (0..15)
                .map(|m| {
                    vec![
                        json_str(&ist_literal(IST_0930_25 - 25 - (15 - m) * 60)),
                        Value::from(1_000),
                    ]
                })
                .collect(),
        );
        inputs.quotes = rows(&["ts", "count"], vec![]);
        inputs.gaps = rows(
            &["count", "sum"],
            vec![vec![Value::from(2), Value::from(75)]],
        );
        inputs.minute_table = QueryOutcome::Missing;
        inputs.apply_lag = Some(42);
        inputs.sockets_now = Some(3);
        let resp = build_sections(&inputs);
        let by_label = |label: &str| {
            all_tiles(&resp)
                .into_iter()
                .find(|t| t.label == label)
                .unwrap_or_else(|| panic!("no tile {label}"))
                .clone()
        };
        let last = by_label("Trades readable in the database, last full minute");
        assert_eq!((last.value.as_str(), last.status), ("1,000", Light::Green));
        let lag = by_label("Database catch-up queue");
        assert_eq!(
            (lag.value.as_str(), lag.status),
            ("42 transactions", Light::Amber)
        );
        let pinned = by_label("Kept on disk until the database confirms");
        assert_eq!(
            (pinned.value.as_str(), pinned.status),
            ("1.0 GB", Light::Amber)
        );
        assert!(pinned.detail.starts_with("8 file(s)"));
        let reconnects = by_label("Socket reconnects since the app started");
        assert_eq!(
            (reconnects.value.as_str(), reconnects.status),
            ("3", Light::Amber)
        );
        let gaps = by_label("Connection gaps closed today");
        assert_eq!((gaps.value.as_str(), gaps.status), ("2", Light::Amber));
        assert_eq!(
            gaps.detail,
            "1m 15s down in total; a gap still open is not counted"
        );
        let sockets = by_label("Feed sockets connected now");
        assert_eq!(
            (sockets.value.as_str(), sockets.status),
            ("3", Light::Green)
        );
        let refused = by_label("Candle files set aside");
        assert_eq!(refused.status, Light::Grey, "NaN reads as not reported");
        let overflow = by_label("Candle overflow files on disk");
        assert_eq!(
            (overflow.value.as_str(), overflow.status),
            ("0 bytes", Light::Grey)
        );
        let record = by_label("Per-minute speed record");
        assert_eq!(
            (record.value.as_str(), record.status),
            ("not yet available", Light::Grey)
        );
    }

    #[test]
    fn test_no_tile_shows_a_metric_name_library_or_path() {
        let mut inputs = all_down_inputs(true);
        inputs.exporter = Some(parse_metrics_text(METRICS_SAMPLE));
        let resp = build_sections(&inputs);
        let body = serde_json::to_string(&resp)
            .expect("serializes")
            .to_ascii_lowercase();
        for banned in [
            "tv_",
            "questdb",
            "prometheus",
            "tokio",
            "crates/",
            ".rs",
            "wal",
        ] {
            assert!(!body.contains(banned), "page text contains {banned:?}");
        }
    }

    #[test]
    fn test_minute_table_section_paints_stored_rows_as_grey_tiles() {
        let outcome = rows(
            &["ts", "stage", "p99_ns", "samples"],
            vec![vec![
                json_str("2026-10-30T09:29:00.000000Z"),
                json_str("socket_to_wal"),
                Value::from(32_768),
                Value::from(60_000),
            ]],
        );
        let section = minute_table_section(&outcome);
        assert_eq!(
            section.tiles[0].label,
            "2026-10-30T09:29:00.000000Z · socket_to_wal"
        );
        assert_eq!(section.tiles[0].value, "p99_ns 32768, samples 60000");
        assert_eq!(section.tiles[0].status, Light::Grey);
        let empty = minute_table_section(&rows(&["ts"], vec![]));
        assert_eq!(empty.tiles[0].value, "no rows in 24 hours");
        let refused = minute_table_section(&QueryOutcome::Refused);
        assert_eq!(refused.tiles[0].value, "could not read");
    }

    #[test]
    fn test_describe_row_handles_bools_nulls_and_no_numbers() {
        let cols = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let (label, value) = describe_row(&cols, &[Value::Null, Value::Bool(true), json_str("x")]);
        assert_eq!(label, "x");
        assert_eq!(value, "b true");
        let (label, value) = describe_row(&cols, &[Value::Null]);
        assert_eq!((label.as_str(), value.as_str()), ("row", "no numbers"));
    }

    #[test]
    fn test_human_nanos_human_bytes_human_secs_group_digits_format_signed() {
        assert_eq!(human_nanos(999), "999 ns");
        assert_eq!(human_nanos(1_500), "1.5 µs");
        assert_eq!(human_nanos(2_500_000), "2.5 ms");
        assert_eq!(human_nanos(3_000_000_000), "3.0 s");
        assert_eq!(human_bytes(0.0), "0 bytes");
        assert_eq!(human_bytes(f64::NAN), "0 bytes");
        assert_eq!(human_bytes(512.0), "512 bytes");
        assert_eq!(human_bytes(2048.0), "2.0 KB");
        assert_eq!(human_bytes(3.0 * 1024.0 * 1024.0), "3.0 MB");
        assert_eq!(human_secs(-5), "0s");
        assert_eq!(human_secs(59), "59s");
        assert_eq!(human_secs(3_725), "1h 2m 5s");
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(1_000), "1,000");
        assert_eq!(group_digits(u64::MAX), "18,446,744,073,709,551,615");
        assert_eq!(format_signed(-1_234), "-1,234");
        assert_eq!(format_signed(i64::MIN), "-9,223,372,036,854,775,808");
        assert_eq!(as_count(-3.0), 0);
        assert_eq!(as_count(f64::INFINITY), 0);
        assert_eq!(as_count(2.6), 3);
        assert_eq!(as_count(1e30), u64::MAX);
        assert_eq!(bound_text(QuantileBound::Overflow), "over 68 s");
    }

    #[test]
    fn test_short_text_cuts_on_a_character_boundary() {
        assert_eq!(short_text("abc"), "abc");
        let long = "é".repeat(70);
        let cut = short_text(&long);
        assert_eq!(cut.chars().count(), CELL_MAX_CHARS + 1);
        assert!(cut.ends_with('…'));
    }

    // -----------------------------------------------------------------------
    // Handler, against a mock that answers by what each request asks for
    // -----------------------------------------------------------------------

    fn test_state_with_port(http_port: u16) -> SharedAppState {
        SharedAppState::new_with_feed_runtime(
            QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port,
                pg_port: 8812,
                ilp_port: 9009,
            },
            DhanConfig {
                websocket_url: "wss://api-feed.dhan.co".to_string(),
                order_update_websocket_url: "wss://api-order-update.dhan.co".to_string(),
                rest_api_base_url: "https://api.dhan.co/v2".to_string(),
                auth_base_url: "https://auth.dhan.co".to_string(),
                instrument_csv_url: "https://images.dhan.co/api-data/api-scrip-master-detailed.csv"
                    .to_string(),
                instrument_csv_fallback_url: "https://images.dhan.co/api-data/api-scrip-master.csv"
                    .to_string(),
                max_instruments_per_connection: 5000,
                max_websocket_connections: 5,
                sandbox_base_url: String::new(),
            },
            InstrumentConfig {
                daily_download_time: "08:55:00".to_string(),
                csv_cache_directory: "/tmp/tv-cache".to_string(),
                csv_cache_filename: "instruments.csv".to_string(),
                csv_download_timeout_secs: 120,
                build_window_start: "08:25:00".to_string(),
                build_window_end: "08:55:00".to_string(),
            },
            Arc::new(crate::state::SystemHealthStatus::new()),
            Arc::new(FeedRuntimeState::from_config(&FeedsConfig::default())),
        )
    }

    /// Picks the canned body for one request line. Order matters:
    /// `tick_quotes` and `tick_conservation_audit` both contain `tick`.
    fn mock_body(request_line: &str) -> (&'static str, &'static str) {
        if request_line.contains("/metrics") {
            return ("text/plain", METRICS_SAMPLE);
        }
        if request_line.contains("table_columns") {
            return (
                "application/json",
                r#"{"query":"x","error":"table does not exist [table=hot_path_latency_minute]","position":0}"#,
            );
        }
        if request_line.contains("tick_conservation_audit") {
            return (
                "application/json",
                r#"{"columns":[{"name":"trading_date_ist"},{"name":"feed"},{"name":"verdict"},{"name":"reason"},{"name":"accepted_rows"},{"name":"db_tick_rows"},{"name":"not_landed_rows"},{"name":"shortfall"}],"dataset":[["2026-10-29T00:00:00.000000Z","dhan","mismatch","short",100,90,0,10]]}"#,
            );
        }
        if request_line.contains("feed_gap_audit") {
            return (
                "application/json",
                r#"{"columns":[{"name":"count"},{"name":"sum"}],"dataset":[[0,null]]}"#,
            );
        }
        (
            "application/json",
            r#"{"columns":[{"name":"ts"},{"name":"count"}],"dataset":[]}"#,
        )
    }

    /// A mock that answers every connection by its request line and counts
    /// the connections it served.
    async fn start_routing_mock() -> (u16, Arc<AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let served = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&served);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let counter = Arc::clone(&counter);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]);
                    let line = request.lines().next().unwrap_or("").to_string();
                    let (content_type, body) = mock_body(&line);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                    counter.fetch_add(1, Ordering::SeqCst);
                });
            }
        });
        (port, served)
    }

    #[tokio::test]
    async fn test_handler_reads_every_source_and_a_second_call_is_a_cache_hit() {
        let (port, served) = start_routing_mock().await;
        let state = test_state_with_port(port);
        assert!(state.set_metrics_scrape_url(format!("http://127.0.0.1:{port}/metrics")));
        assert!(
            !state.set_metrics_scrape_url("http://example.invalid/metrics".to_string()),
            "the first address wins"
        );

        let first = speed_health(State(state.clone())).await;
        assert_eq!(first.status(), axum::http::StatusCode::OK);
        assert_eq!(
            first
                .headers()
                .get(crate::response_cache::CACHE_MARKER_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("miss")
        );
        let body = axum::body::to_bytes(first.into_body(), 1024 * 1024)
            .await
            .expect("body");
        let parsed: Value = serde_json::from_slice(&body).expect("json");
        let text = parsed.to_string();
        assert!(
            text.contains("ticks missing"),
            "conservation row read: {text}"
        );
        assert!(text.contains("1.0 GB"), "metrics page read: {text}");
        assert!(
            text.contains("not yet available"),
            "minute table missing: {text}"
        );
        assert!(text.contains("Connection gaps closed today"), "{text}");
        // 4 queries + table_columns + the metrics page; the missing minute
        // table is never queried for rows.
        let after_first = served.load(Ordering::SeqCst);
        assert_eq!(after_first, 6);

        let second = speed_health(State(state)).await;
        assert_eq!(
            second
                .headers()
                .get(crate::response_cache::CACHE_MARKER_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("hit")
        );
        let second_body = axum::body::to_bytes(second.into_body(), 1024 * 1024)
            .await
            .expect("body");
        assert_eq!(body, second_body);
        assert_eq!(
            served.load(Ordering::SeqCst),
            after_first,
            "a hit makes no request"
        );
    }

    #[test]
    fn test_set_metrics_scrape_url_first_wins_and_speed_health_cache_and_speed_health_refresh() {
        // The single-flight lock is free when nothing computes.
        assert!(
            test_state_with_port(1)
                .speed_health_refresh()
                .try_lock()
                .is_ok()
        );
        let state = test_state_with_port(1);
        assert_eq!(state.metrics_scrape_url(), None);
        assert!(state.speed_health_cache().get().is_none());
        assert!(state.set_metrics_scrape_url("http://10.0.0.1:9091/metrics".to_string()));
        assert!(!state.set_metrics_scrape_url("http://10.0.0.2:9091/metrics".to_string()));
        assert_eq!(
            state.metrics_scrape_url(),
            Some("http://10.0.0.1:9091/metrics")
        );
        state.speed_health_cache().put("{}".to_string());
        assert_eq!(state.speed_health_cache().get().as_deref(), Some("{}"));
    }

    #[tokio::test]
    async fn test_compute_speed_health_everything_down_answers_grey_without_panicking() {
        // Port 1 refuses at once; no metrics address is set.
        let state = test_state_with_port(1);
        assert_eq!(state.metrics_scrape_url(), None);
        let resp = compute_speed_health(&state).await;
        assert_eq!(resp.sections.len(), 7);
        for tile in all_tiles(&resp) {
            // The in-memory step counters may hold samples recorded by other
            // tests in this process; every database and metrics tile is grey.
            if !resp.sections[0].tiles.iter().any(|t| t == tile) {
                assert_eq!(tile.status, Light::Grey, "{}", tile.label);
            }
        }
    }
}

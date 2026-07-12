//! BruteX↔TickVault daily cross-verification forensic tables
//! (`brutex_crossverify_cell_audit` + `brutex_crossverify_daily`,
//! BRUTEX-XVERIFY family — see
//! `.claude/rules/project/brutex-crossverify-error-codes.md`).
//!
//! The 15:50 IST daily run compares BruteX-produced Groww 1m OHLCV CSVs
//! (read-only S3 GETs) cell-by-cell against the live `candles_1m` rows tagged
//! `feed='groww'`. Every divergent CELL lands as one forensic row in the cell
//! table; every day lands as per-symbol summary rows + one `__RUN__` row in
//! the daily table (the trend store). COLD PATH, best-effort — never on the
//! tick hot path, never on any order/recovery path.
//!
//! ## Cell audit table
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS brutex_crossverify_cell_audit (
//!     ts                TIMESTAMP,  -- minute ts (IST nanos, designated)
//!     trading_date_ist  TIMESTAMP,  -- the trading day (IST midnight)
//!     feed              SYMBOL,     -- 'groww' (feed-in-key, 2026-06-28 override)
//!     symbol            SYMBOL,     -- BruteX CSV symbol (sanitized)
//!     security_id       LONG,       -- mapped live id; -1 sentinel = unmapped
//!     exchange_segment  SYMBOL,     -- I-P1-11 composite pair with security_id
//!     minute_ts_ist     TIMESTAMP,  -- the compared minute (IST)
//!     kind              SYMBOL,     -- diverged / missing_live / missing_brutex /
//!                                   -- unmapped_symbol / out_of_session / tail_unsealed
//!     field             SYMBOL,     -- open / high / low / close / volume / none
//!     brutex_paise      LONG,       -- BruteX side value in paise; -1 = none
//!     live_paise        LONG,       -- live side value in paise; -1 = none
//!     brutex_volume     LONG,       -- stored (never classified for groww); -1 = none
//!     live_volume       LONG,       -- stored; -1 = none
//!     observed_at       TIMESTAMP,  -- NON-key wall clock of the classifying run
//!     note              STRING      -- ≤200 chars, secret-redacted
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, trading_date_ist, feed, symbol, security_id,
//!                     exchange_segment, minute_ts_ist, kind, field);
//! ```
//!
//! ## Daily summary table
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS brutex_crossverify_daily (
//!     ts                TIMESTAMP,  -- deterministic run ts (15:50 IST, designated)
//!     trading_date_ist  TIMESTAMP,
//!     feed              SYMBOL,
//!     symbol            SYMBOL,     -- per-symbol rows + one '__RUN__' row
//!     security_id       LONG,       -- -1 sentinel on the run row
//!     exchange_segment  SYMBOL,     -- 'none' on the run row
//!     outcome           SYMBOL,     -- clean / diverged / partial / no_data /
//!                                   -- blind / degraded
//!     minutes_compared  LONG,       -- -1 sentinel = not measured
//!     cells_compared    LONG,
//!     diverged_cells    LONG,
//!     missing_live      LONG,
//!     missing_brutex    LONG,
//!     tail_unsealed     LONG,
//!     out_of_session    LONG,
//!     unmapped_symbols  LONG,       -- run-row aggregate; -1 on symbol rows
//!     objects_fetched   LONG,       -- run-row aggregate; -1 on symbol rows
//!     run_partial       BOOLEAN,    -- the run had partial evidence
//!     observed_at       TIMESTAMP,  -- NON-key wall clock
//!     note              STRING      -- ≤200 chars, secret-redacted
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, trading_date_ist, feed, symbol, security_id,
//!                     exchange_segment);
//! ```
//!
//! Both DEDUP keys lead with the designated timestamp (2026-04-28 regression
//! rule), carry `feed` in-key (operator override 2026-06-28), and pair
//! `security_id` with `exchange_segment` (I-P1-11) — so a re-run of the same
//! trading day UPSERTs in place instead of duplicating. The keep-better guard
//! ([`daily_outcome_rank`] / [`keep_better_blocks_downgrade`]) additionally
//! stops a later evidence-less re-run (`no_data`/`blind`/`degraded`) from
//! overwriting a MEASURED day (`clean`/`diverged`/`partial`) —
//! `stage="outcome_regression"` logs the suppression at the app call site.
//!
//! Transport is ILP-over-HTTP (per-flush server ACK — the 2026-07-05
//! ws_event_audit fire-and-forget lesson: an ILP TCP reject NEVER returned
//! `Err`, leaving a silently empty table). Persist failures emit
//! `error!(code = "BRUTEX-XVERIFY-02", stage = ...)`.

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{error, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::{capture_rest_error_body, sanitize_audit_string};

/// Cell-level forensic table — one row per divergent/missing/informational cell.
pub const BRUTEX_CROSSVERIFY_CELL_AUDIT_TABLE: &str = "brutex_crossverify_cell_audit";

/// Daily summary table — per-symbol rows + one `__RUN__` row per trading day.
pub const BRUTEX_CROSSVERIFY_DAILY_TABLE: &str = "brutex_crossverify_daily";

/// Cell-audit DEDUP UPSERT key. Designated timestamp first (2026-04-28 rule);
/// `feed` in-key (2026-06-28 override); `(security_id, exchange_segment)`
/// composite per I-P1-11; `(minute_ts_ist, kind, field)` make each classified
/// cell its own idempotent row so re-runs UPSERT in place.
pub const DEDUP_KEY_BRUTEX_CROSSVERIFY_CELL_AUDIT: &str =
    "ts, trading_date_ist, feed, symbol, security_id, exchange_segment, minute_ts_ist, kind, field";

/// Daily-summary DEDUP UPSERT key. Same discipline; one row per
/// `(day, feed, symbol, security_id, exchange_segment)` — the deterministic
/// run `ts` (15:50 IST of the trading day) keeps re-runs idempotent.
pub const DEDUP_KEY_BRUTEX_CROSSVERIFY_DAILY: &str =
    "ts, trading_date_ist, feed, symbol, security_id, exchange_segment";

/// The reserved daily-table symbol for the whole-run aggregate row.
pub const BRUTEX_CROSSVERIFY_RUN_ROW_SYMBOL: &str = "__RUN__";

/// Sentinel for "value not present / not measured" on LONG columns
/// (paise, volumes, counts). Never a fabricated zero (audit Rule 11).
pub const BRUTEX_CROSSVERIFY_MISSING_SENTINEL: i64 = -1;

/// Bound on the persisted `note` string (post-redaction truncation).
pub const BRUTEX_CROSSVERIFY_NOTE_MAX_CHARS: usize = 200;

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

// ---------------------------------------------------------------------------
// Wire-format enums
// ---------------------------------------------------------------------------

/// Classification of one compared cell (the `kind` SYMBOL wire labels).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrutexCrossverifyCellKind {
    /// Both sides have the minute; a field differs beyond tolerance.
    Diverged,
    /// BruteX has the minute; live `candles_1m` (feed='groww') does not.
    MissingLive,
    /// Live has the minute; the BruteX CSV does not.
    MissingBrutex,
    /// A BruteX CSV symbol that resolved to no live instrument.
    UnmappedSymbol,
    /// A BruteX row outside [09:15, 15:30) IST — recorded, not classified.
    OutOfSession,
    /// 15:28/15:29 minute absent live (close-seal timing) — informational.
    TailUnsealed,
}

impl BrutexCrossverifyCellKind {
    /// Stable wire-format label persisted in the `kind` SYMBOL column.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Diverged => "diverged",
            Self::MissingLive => "missing_live",
            Self::MissingBrutex => "missing_brutex",
            Self::UnmappedSymbol => "unmapped_symbol",
            Self::OutOfSession => "out_of_session",
            Self::TailUnsealed => "tail_unsealed",
        }
    }
}

/// Which OHLCV field a cell row refers to (the `field` SYMBOL wire labels).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrutexCrossverifyField {
    /// Session-open value of the minute.
    Open,
    /// Session-high value of the minute.
    High,
    /// Session-low value of the minute.
    Low,
    /// Session-close value of the minute.
    Close,
    /// Volume (STORED for forensics only — never classified for groww,
    /// whose live volume is always 0 on the LTP-only feed).
    Volume,
    /// Whole-minute rows (missing/out-of-session/unmapped) carry no field.
    None,
}

impl BrutexCrossverifyField {
    /// Stable wire-format label persisted in the `field` SYMBOL column.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::High => "high",
            Self::Low => "low",
            Self::Close => "close",
            Self::Volume => "volume",
            Self::None => "none",
        }
    }
}

/// Daily-row outcome (the `outcome` SYMBOL wire labels).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrutexCrossverifyDailyOutcome {
    /// Measured; zero divergent cells.
    Clean,
    /// Measured; ≥1 divergent cell.
    Diverged,
    /// Measured with partial coverage (some legs degraded mid-run).
    Partial,
    /// The S3 prefix stayed empty through the 16:05 IST deadline.
    NoData,
    /// The live side had no `feed='groww'` rows to compare against.
    Blind,
    /// A run leg failed before a measurement could be recorded.
    Degraded,
}

impl BrutexCrossverifyDailyOutcome {
    /// Stable wire-format label persisted in the `outcome` SYMBOL column.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Diverged => "diverged",
            Self::Partial => "partial",
            Self::NoData => "no_data",
            Self::Blind => "blind",
            Self::Degraded => "degraded",
        }
    }
}

// ---------------------------------------------------------------------------
// Keep-better guard (pure)
// ---------------------------------------------------------------------------

/// Rank of a daily-outcome wire label for the keep-better guard: MEASURED
/// outcomes (`clean`/`diverged`/`partial`) rank 1; evidence-less outcomes
/// (`no_data`/`blind`/`degraded`) rank 0; unknown labels rank 0 (fail-open
/// for the guard — an unknown existing label never blocks a measured write).
#[must_use]
pub fn daily_outcome_rank(outcome: &str) -> u8 {
    match outcome {
        "clean" | "diverged" | "partial" => 1,
        _ => 0,
    }
}

/// The keep-better decision (the dual-feed scoreboard precedent): returns
/// `true` when writing `new_outcome` over `existing_outcome` would DOWNGRADE
/// a measured day to an evidence-less one — the caller suppresses the write
/// and logs `stage="outcome_regression"`. `None` existing (no prior row)
/// never blocks.
#[must_use]
pub fn keep_better_blocks_downgrade(existing_outcome: Option<&str>, new_outcome: &str) -> bool {
    match existing_outcome {
        Some(existing) => daily_outcome_rank(existing) > daily_outcome_rank(new_outcome),
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Row structs
// ---------------------------------------------------------------------------

/// One classified cell, ready for ILP write.
#[derive(Clone, Debug, PartialEq)]
pub struct BrutexCrossverifyCellRow {
    /// Designated timestamp — the compared minute (IST nanos). Deterministic
    /// per cell so re-runs UPSERT in place.
    pub ts_ist_nanos: i64,
    /// The trading day — IST midnight nanoseconds.
    pub trading_date_ist_nanos: i64,
    /// Feed wire label (`"groww"` today).
    pub feed: &'static str,
    /// BruteX CSV symbol — UNTRUSTED external text; sanitized at the write
    /// boundary (BiDi/control strip via `sanitize_audit_string`).
    pub symbol: String,
    /// Mapped live security id; [`BRUTEX_CROSSVERIFY_MISSING_SENTINEL`] when
    /// the symbol did not resolve.
    pub security_id: i64,
    /// Exchange segment wire label (I-P1-11 pair with `security_id`);
    /// `"none"` when unmapped.
    pub exchange_segment: String,
    /// The compared minute (IST nanos) — duplicated as a queryable column.
    pub minute_ts_ist_nanos: i64,
    /// The cell classification.
    pub kind: BrutexCrossverifyCellKind,
    /// Which field diverged (`None` for whole-minute rows).
    pub field: BrutexCrossverifyField,
    /// BruteX-side value in paise; -1 sentinel when absent.
    pub brutex_paise: i64,
    /// Live-side value in paise; -1 sentinel when absent.
    pub live_paise: i64,
    /// BruteX-side volume (stored, never classified for groww); -1 = absent.
    pub brutex_volume: i64,
    /// Live-side volume (stored); -1 = absent.
    pub live_volume: i64,
    /// NON-key wall clock of the classifying run (IST nanos).
    pub observed_at_ist_nanos: i64,
    /// Free-form note. Redacted + truncated at the write boundary.
    pub note: String,
}

/// One daily summary row (per-symbol, or the `__RUN__` aggregate row).
#[derive(Clone, Debug, PartialEq)]
pub struct BrutexCrossverifyDailyRow {
    /// Designated timestamp — the deterministic run instant (15:50 IST of
    /// the trading day) so re-runs UPSERT in place.
    pub ts_ist_nanos: i64,
    /// The trading day — IST midnight nanoseconds.
    pub trading_date_ist_nanos: i64,
    /// Feed wire label (`"groww"` today).
    pub feed: &'static str,
    /// Per-symbol rows carry the sanitized CSV symbol;
    /// the aggregate row carries [`BRUTEX_CROSSVERIFY_RUN_ROW_SYMBOL`].
    pub symbol: String,
    /// Mapped live security id; -1 sentinel on the run row / unmapped.
    pub security_id: i64,
    /// Exchange segment (`"none"` on the run row).
    pub exchange_segment: String,
    /// The day outcome.
    pub outcome: BrutexCrossverifyDailyOutcome,
    /// Distinct minutes compared; -1 sentinel = not measured.
    pub minutes_compared: i64,
    /// Total cells compared; -1 sentinel = not measured.
    pub cells_compared: i64,
    /// Cells classified `diverged`.
    pub diverged_cells: i64,
    /// Minutes classified `missing_live`.
    pub missing_live: i64,
    /// Minutes classified `missing_brutex`.
    pub missing_brutex: i64,
    /// Minutes classified `tail_unsealed` (informational).
    pub tail_unsealed: i64,
    /// Rows classified `out_of_session` (recorded, not classified).
    pub out_of_session: i64,
    /// Run-row aggregate: symbols that resolved to no live instrument;
    /// -1 sentinel on per-symbol rows.
    pub unmapped_symbols: i64,
    /// Run-row aggregate: S3 objects fetched; -1 sentinel on symbol rows.
    pub objects_fetched: i64,
    /// `true` when the run had PARTIAL evidence (a leg degraded).
    pub run_partial: bool,
    /// NON-key wall clock of the classifying run (IST nanos).
    pub observed_at_ist_nanos: i64,
    /// Free-form note. Redacted + truncated at the write boundary.
    pub note: String,
}

// ---------------------------------------------------------------------------
// DDL (pure builders)
// ---------------------------------------------------------------------------

/// The idempotent cell-audit `CREATE TABLE` DDL. Pure (testable without
/// QuestDB).
#[must_use]
pub fn brutex_crossverify_cell_audit_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {BRUTEX_CROSSVERIFY_CELL_AUDIT_TABLE} (\
            ts                TIMESTAMP, \
            trading_date_ist  TIMESTAMP, \
            feed              SYMBOL, \
            symbol            SYMBOL, \
            security_id       LONG, \
            exchange_segment  SYMBOL, \
            minute_ts_ist     TIMESTAMP, \
            kind              SYMBOL, \
            field             SYMBOL, \
            brutex_paise      LONG, \
            live_paise        LONG, \
            brutex_volume     LONG, \
            live_volume       LONG, \
            observed_at       TIMESTAMP, \
            note              STRING\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_BRUTEX_CROSSVERIFY_CELL_AUDIT});"
    )
}

/// The idempotent daily-summary `CREATE TABLE` DDL. Pure (testable without
/// QuestDB).
#[must_use]
pub fn brutex_crossverify_daily_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {BRUTEX_CROSSVERIFY_DAILY_TABLE} (\
            ts                TIMESTAMP, \
            trading_date_ist  TIMESTAMP, \
            feed              SYMBOL, \
            symbol            SYMBOL, \
            security_id       LONG, \
            exchange_segment  SYMBOL, \
            outcome           SYMBOL, \
            minutes_compared  LONG, \
            cells_compared    LONG, \
            diverged_cells    LONG, \
            missing_live      LONG, \
            missing_brutex    LONG, \
            tail_unsealed     LONG, \
            out_of_session    LONG, \
            unmapped_symbols  LONG, \
            objects_fetched   LONG, \
            run_partial       BOOLEAN, \
            observed_at       TIMESTAMP, \
            note              STRING\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_BRUTEX_CROSSVERIFY_DAILY});"
    )
}

/// Creates both cross-verify tables if absent (idempotent, schema-self-heal
/// pattern). Failures log at `error!` (code BRUTEX-XVERIFY-02) but never
/// block the caller. NOTE the HTTP-CLIENT-01-class consequence: a failed
/// ensure leaves a table to be auto-created by the first ILP write WITHOUT
/// DEDUP UPSERT KEYS — a duplicate-row window until a later boot's ensure
/// succeeds. Never drops a populated table (SEBI retention).
// TEST-EXEMPT: live-QuestDB DDL runner exercised via mock /exec HTTP tests below; DDL strings unit-tested via the pure builders.
pub async fn ensure_brutex_crossverify_tables(questdb_config: &QuestDbConfig) {
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
                code = "BRUTEX-XVERIFY-02",
                stage = "questdb_write",
                ?err,
                "BRUTEX-XVERIFY-02: HTTP client build failed — cross-verify \
                 tables not ensured (first ILP write may auto-create them \
                 WITHOUT dedup — duplicate-row window until the next \
                 successful boot)"
            );
            return;
        }
    };
    // CREATE first, then the feed ADD-COLUMN self-heal + DEDUP re-enable
    // (operator override 2026-06-28: feed in-key on every persisted table).
    // Greenfield tables: the column + key come from the CREATE; the ALTERs
    // are no-ops there. Deliberately NO NULL-feed backfill: both tables are
    // greenfield in this PR and legitimately per-feed forever.
    let statements = [
        brutex_crossverify_cell_audit_create_ddl(),
        format!(
            "ALTER TABLE {BRUTEX_CROSSVERIFY_CELL_AUDIT_TABLE} ADD COLUMN IF NOT EXISTS feed SYMBOL;"
        ),
        format!(
            "ALTER TABLE {BRUTEX_CROSSVERIFY_CELL_AUDIT_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_BRUTEX_CROSSVERIFY_CELL_AUDIT});"
        ),
        brutex_crossverify_daily_create_ddl(),
        format!(
            "ALTER TABLE {BRUTEX_CROSSVERIFY_DAILY_TABLE} ADD COLUMN IF NOT EXISTS feed SYMBOL;"
        ),
        format!(
            "ALTER TABLE {BRUTEX_CROSSVERIFY_DAILY_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_BRUTEX_CROSSVERIFY_DAILY});"
        ),
    ];
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
                error!(code = "BRUTEX-XVERIFY-02", stage = "questdb_write",
                    %status, ddl = ddl.as_str(),
                    body = %body.chars().take(200).collect::<String>(),
                    "BRUTEX-XVERIFY-02: cross-verify DDL returned non-2xx \
                     (dedup may be missing — duplicate-row window until a \
                     later ensure succeeds)");
            }
            Err(err) => error!(
                code = "BRUTEX-XVERIFY-02",
                stage = "questdb_write",
                ?err,
                ddl = ddl.as_str(),
                "BRUTEX-XVERIFY-02: cross-verify DDL request failed"
            ),
        }
    }
}

/// Builds the ILP-over-HTTP conf string. HTTP (NOT ILP TCP) so every flush
/// gets a per-request server ACK — a server-side reject (schema drift, DEDUP
/// violation) surfaces as `Err` instead of a silently empty table (the
/// 2026-07-05 ws_event_audit lesson).
fn crossverify_ilp_http_conf(config: &QuestDbConfig) -> String {
    format!("http::addr={}:{};", config.host, config.http_port)
}

/// Lazy ILP-over-HTTP writer for both cross-verify tables. Mirrors
/// `FeedEpisodeAuditWriter`: unreachable QuestDB at construction still
/// builds (`sender = None`); appends fill the local buffer and `flush`
/// returns `Err` until QuestDB is reachable.
pub struct BrutexCrossverifyWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
}

impl BrutexCrossverifyWriter {
    /// Production constructor — ILP-over-HTTP sender, lazy on failure
    /// (`http::` does not dial at construction; failures surface at flush).
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor (needs live QuestDB); append/flush paths covered via for_test() + the lazy-construction test.
    pub fn new(config: &QuestDbConfig) -> Self {
        let conf = crossverify_ilp_http_conf(config);
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
                    "brutex_crossverify writer: QuestDB unreachable — buffering locally"
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

    /// Appends one classified cell row (cold path — divergences are bounded
    /// per day).
    ///
    /// SECURITY: `symbol` comes from an UNTRUSTED external CSV — it is
    /// sanitized (BiDi/control strip) at this write boundary; `note` is
    /// routed through the `capture_rest_error_body` redaction choke point
    /// then truncated to [`BRUTEX_CROSSVERIFY_NOTE_MAX_CHARS`].
    ///
    /// # Errors
    /// Propagates ILP buffer errors (table/column append failure).
    pub fn append_cell_row(&mut self, r: &BrutexCrossverifyCellRow) -> Result<()> {
        let safe_symbol = sanitize_audit_string(&r.symbol);
        let safe_note: String = capture_rest_error_body(&r.note)
            .chars()
            .take(BRUTEX_CROSSVERIFY_NOTE_MAX_CHARS)
            .collect();
        self.buffer
            .table(BRUTEX_CROSSVERIFY_CELL_AUDIT_TABLE)
            .context("table")?
            // Symbols BEFORE columns (ILP tags-before-fields rule).
            .symbol("feed", r.feed)
            .context("feed")?
            .symbol("symbol", safe_symbol.as_str())
            .context("symbol")?
            .symbol("exchange_segment", r.exchange_segment.as_str())
            .context("exchange_segment")?
            .symbol("kind", r.kind.as_str())
            .context("kind")?
            .symbol("field", r.field.as_str())
            .context("field")?
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(r.trading_date_ist_nanos),
            )
            .context("trading_date_ist")?
            .column_i64("security_id", r.security_id)
            .context("security_id")?
            .column_ts("minute_ts_ist", TimestampNanos::new(r.minute_ts_ist_nanos))
            .context("minute_ts_ist")?
            .column_i64("brutex_paise", r.brutex_paise)
            .context("brutex_paise")?
            .column_i64("live_paise", r.live_paise)
            .context("live_paise")?
            .column_i64("brutex_volume", r.brutex_volume)
            .context("brutex_volume")?
            .column_i64("live_volume", r.live_volume)
            .context("live_volume")?
            .column_ts("observed_at", TimestampNanos::new(r.observed_at_ist_nanos))
            .context("observed_at")?
            .column_str("note", safe_note.as_str())
            .context("note")?
            .at(TimestampNanos::new(r.ts_ist_nanos))
            .context("designated timestamp")?;
        self.pending = self.pending.saturating_add(1);
        Ok(())
    }

    /// Appends one daily summary row (per-symbol or the `__RUN__` row).
    ///
    /// # Errors
    /// Propagates ILP buffer errors (table/column append failure).
    pub fn append_daily_row(&mut self, r: &BrutexCrossverifyDailyRow) -> Result<()> {
        let safe_symbol = sanitize_audit_string(&r.symbol);
        let safe_note: String = capture_rest_error_body(&r.note)
            .chars()
            .take(BRUTEX_CROSSVERIFY_NOTE_MAX_CHARS)
            .collect();
        self.buffer
            .table(BRUTEX_CROSSVERIFY_DAILY_TABLE)
            .context("table")?
            .symbol("feed", r.feed)
            .context("feed")?
            .symbol("symbol", safe_symbol.as_str())
            .context("symbol")?
            .symbol("exchange_segment", r.exchange_segment.as_str())
            .context("exchange_segment")?
            .symbol("outcome", r.outcome.as_str())
            .context("outcome")?
            .column_ts(
                "trading_date_ist",
                TimestampNanos::new(r.trading_date_ist_nanos),
            )
            .context("trading_date_ist")?
            .column_i64("security_id", r.security_id)
            .context("security_id")?
            .column_i64("minutes_compared", r.minutes_compared)
            .context("minutes_compared")?
            .column_i64("cells_compared", r.cells_compared)
            .context("cells_compared")?
            .column_i64("diverged_cells", r.diverged_cells)
            .context("diverged_cells")?
            .column_i64("missing_live", r.missing_live)
            .context("missing_live")?
            .column_i64("missing_brutex", r.missing_brutex)
            .context("missing_brutex")?
            .column_i64("tail_unsealed", r.tail_unsealed)
            .context("tail_unsealed")?
            .column_i64("out_of_session", r.out_of_session)
            .context("out_of_session")?
            .column_i64("unmapped_symbols", r.unmapped_symbols)
            .context("unmapped_symbols")?
            .column_i64("objects_fetched", r.objects_fetched)
            .context("objects_fetched")?
            .column_bool("run_partial", r.run_partial)
            .context("run_partial")?
            .column_ts("observed_at", TimestampNanos::new(r.observed_at_ist_nanos))
            .context("observed_at")?
            .column_str("note", safe_note.as_str())
            .context("note")?
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
            anyhow::bail!("brutex_crossverify: no ILP sender (QuestDB unreachable)");
        };
        sender
            .flush(&mut self.buffer)
            .context("brutex_crossverify ILP flush")?;
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

    fn sample_cell_row() -> BrutexCrossverifyCellRow {
        BrutexCrossverifyCellRow {
            ts_ist_nanos: 1_770_000_000_000_000_000,
            trading_date_ist_nanos: 1_769_990_400_000_000_000,
            feed: "groww",
            symbol: "RELIANCE".to_string(),
            security_id: 2885,
            exchange_segment: "NSE_EQ".to_string(),
            minute_ts_ist_nanos: 1_770_000_000_000_000_000,
            kind: BrutexCrossverifyCellKind::Diverged,
            field: BrutexCrossverifyField::High,
            brutex_paise: 284_755,
            live_paise: 284_750,
            brutex_volume: BRUTEX_CROSSVERIFY_MISSING_SENTINEL,
            live_volume: 0,
            observed_at_ist_nanos: 1_770_020_000_000_000_000,
            note: "delta=5 paise".to_string(),
        }
    }

    fn sample_daily_row() -> BrutexCrossverifyDailyRow {
        BrutexCrossverifyDailyRow {
            ts_ist_nanos: 1_770_018_600_000_000_000,
            trading_date_ist_nanos: 1_769_990_400_000_000_000,
            feed: "groww",
            symbol: "RELIANCE".to_string(),
            security_id: 2885,
            exchange_segment: "NSE_EQ".to_string(),
            outcome: BrutexCrossverifyDailyOutcome::Diverged,
            minutes_compared: 375,
            cells_compared: 1_500,
            diverged_cells: 3,
            missing_live: 1,
            missing_brutex: 0,
            tail_unsealed: 2,
            out_of_session: 0,
            unmapped_symbols: BRUTEX_CROSSVERIFY_MISSING_SENTINEL,
            objects_fetched: BRUTEX_CROSSVERIFY_MISSING_SENTINEL,
            run_partial: false,
            observed_at_ist_nanos: 1_770_020_000_000_000_000,
            note: String::new(),
        }
    }

    #[test]
    fn test_cell_audit_ddl_contains_expected_columns() {
        let ddl = brutex_crossverify_cell_audit_create_ddl();
        for col in [
            "ts ",
            "trading_date_ist",
            "feed",
            "symbol",
            "security_id",
            "exchange_segment",
            "minute_ts_ist",
            "kind",
            "field",
            "brutex_paise",
            "live_paise",
            "brutex_volume",
            "live_volume",
            "observed_at",
            "note",
        ] {
            assert!(ddl.contains(col), "cell DDL missing column {col:?}: {ddl}");
        }
        assert!(ddl.contains("CREATE TABLE IF NOT EXISTS"));
        assert!(ddl.contains("PARTITION BY DAY"));
        assert!(ddl.contains(&format!(
            "DEDUP UPSERT KEYS({DEDUP_KEY_BRUTEX_CROSSVERIFY_CELL_AUDIT})"
        )));
    }

    #[test]
    fn test_daily_ddl_contains_expected_columns() {
        let ddl = brutex_crossverify_daily_create_ddl();
        for col in [
            "ts ",
            "trading_date_ist",
            "feed",
            "symbol",
            "security_id",
            "exchange_segment",
            "outcome",
            "minutes_compared",
            "cells_compared",
            "diverged_cells",
            "missing_live",
            "missing_brutex",
            "tail_unsealed",
            "out_of_session",
            "unmapped_symbols",
            "objects_fetched",
            "run_partial",
            "observed_at",
            "note",
        ] {
            assert!(ddl.contains(col), "daily DDL missing column {col:?}: {ddl}");
        }
        assert!(ddl.contains("CREATE TABLE IF NOT EXISTS"));
        assert!(ddl.contains("PARTITION BY DAY"));
        assert!(ddl.contains(&format!(
            "DEDUP UPSERT KEYS({DEDUP_KEY_BRUTEX_CROSSVERIFY_DAILY})"
        )));
    }

    #[test]
    fn test_cell_dedup_key_ts_first_feed_and_composite_pair() {
        // ts first (2026-04-28 regression rule).
        assert!(
            DEDUP_KEY_BRUTEX_CROSSVERIFY_CELL_AUDIT
                .trim_start()
                .starts_with("ts,"),
            "designated timestamp must lead the DEDUP key"
        );
        let has_token = |t: &str| {
            DEDUP_KEY_BRUTEX_CROSSVERIFY_CELL_AUDIT
                .split([',', ' '])
                .map(str::trim)
                .any(|tok| tok == t)
        };
        // feed in-key (operator override 2026-06-28).
        assert!(has_token("feed"), "feed must be in the DEDUP key");
        // I-P1-11 composite pair.
        assert!(has_token("security_id"));
        assert!(has_token("exchange_segment"));
        // Per-cell identity.
        assert!(has_token("minute_ts_ist"));
        assert!(has_token("kind"));
        assert!(has_token("field"));
        assert!(has_token("symbol"));
        // observed_at MUST NOT be in the key (re-stamped per run — would
        // duplicate rows on every re-run).
        assert!(!has_token("observed_at"));
        assert_eq!(
            DEDUP_KEY_BRUTEX_CROSSVERIFY_CELL_AUDIT.matches(',').count() + 1,
            9
        );
    }

    #[test]
    fn test_daily_dedup_key_ts_first_feed_and_composite_pair() {
        assert!(
            DEDUP_KEY_BRUTEX_CROSSVERIFY_DAILY
                .trim_start()
                .starts_with("ts,"),
            "designated timestamp must lead the DEDUP key"
        );
        let has_token = |t: &str| {
            DEDUP_KEY_BRUTEX_CROSSVERIFY_DAILY
                .split([',', ' '])
                .map(str::trim)
                .any(|tok| tok == t)
        };
        assert!(has_token("feed"), "feed must be in the DEDUP key");
        assert!(has_token("security_id"));
        assert!(has_token("exchange_segment"));
        assert!(has_token("symbol"));
        assert!(has_token("trading_date_ist"));
        // outcome MUST NOT be in the key — the day row UPSERTs in place so
        // the keep-better guard governs overwrites, never a duplicate row.
        assert!(!has_token("outcome"));
        assert!(!has_token("observed_at"));
        assert_eq!(
            DEDUP_KEY_BRUTEX_CROSSVERIFY_DAILY.matches(',').count() + 1,
            6
        );
    }

    #[test]
    fn test_cell_kind_and_field_wire_labels_are_stable() {
        assert_eq!(BrutexCrossverifyCellKind::Diverged.as_str(), "diverged");
        assert_eq!(
            BrutexCrossverifyCellKind::MissingLive.as_str(),
            "missing_live"
        );
        assert_eq!(
            BrutexCrossverifyCellKind::MissingBrutex.as_str(),
            "missing_brutex"
        );
        assert_eq!(
            BrutexCrossverifyCellKind::UnmappedSymbol.as_str(),
            "unmapped_symbol"
        );
        assert_eq!(
            BrutexCrossverifyCellKind::OutOfSession.as_str(),
            "out_of_session"
        );
        assert_eq!(
            BrutexCrossverifyCellKind::TailUnsealed.as_str(),
            "tail_unsealed"
        );
        assert_eq!(BrutexCrossverifyField::Open.as_str(), "open");
        assert_eq!(BrutexCrossverifyField::High.as_str(), "high");
        assert_eq!(BrutexCrossverifyField::Low.as_str(), "low");
        assert_eq!(BrutexCrossverifyField::Close.as_str(), "close");
        assert_eq!(BrutexCrossverifyField::Volume.as_str(), "volume");
        assert_eq!(BrutexCrossverifyField::None.as_str(), "none");
    }

    #[test]
    fn test_daily_outcome_wire_labels_are_stable() {
        assert_eq!(BrutexCrossverifyDailyOutcome::Clean.as_str(), "clean");
        assert_eq!(BrutexCrossverifyDailyOutcome::Diverged.as_str(), "diverged");
        assert_eq!(BrutexCrossverifyDailyOutcome::Partial.as_str(), "partial");
        assert_eq!(BrutexCrossverifyDailyOutcome::NoData.as_str(), "no_data");
        assert_eq!(BrutexCrossverifyDailyOutcome::Blind.as_str(), "blind");
        assert_eq!(BrutexCrossverifyDailyOutcome::Degraded.as_str(), "degraded");
    }

    #[test]
    fn test_keep_better_blocks_measured_to_unmeasured_downgrade() {
        // A measured day is never overwritten by an evidence-less re-run.
        for measured in ["clean", "diverged", "partial"] {
            for weak in ["no_data", "blind", "degraded"] {
                assert!(
                    keep_better_blocks_downgrade(Some(measured), weak),
                    "{measured} -> {weak} must be blocked"
                );
            }
        }
    }

    #[test]
    fn test_keep_better_allows_upgrades_laterals_and_fresh_writes() {
        // Fresh day (no prior row): never blocked.
        assert!(!keep_better_blocks_downgrade(None, "no_data"));
        assert!(!keep_better_blocks_downgrade(None, "clean"));
        // Unmeasured -> measured upgrade: allowed.
        for weak in ["no_data", "blind", "degraded"] {
            for measured in ["clean", "diverged", "partial"] {
                assert!(
                    !keep_better_blocks_downgrade(Some(weak), measured),
                    "{weak} -> {measured} must be allowed"
                );
            }
        }
        // Lateral moves (same rank) allowed — a re-measure may legitimately
        // change clean <-> diverged.
        assert!(!keep_better_blocks_downgrade(Some("clean"), "diverged"));
        assert!(!keep_better_blocks_downgrade(Some("no_data"), "degraded"));
        // Unknown existing label ranks 0 (fail-open) — a measured write wins.
        assert!(!keep_better_blocks_downgrade(Some("garbage"), "clean"));
        // Unknown NEW label ranks 0 — blocked over a measured day.
        assert!(keep_better_blocks_downgrade(Some("clean"), "garbage"));
    }

    #[test]
    fn test_daily_outcome_rank_covers_all_wire_labels() {
        for o in [
            BrutexCrossverifyDailyOutcome::Clean,
            BrutexCrossverifyDailyOutcome::Diverged,
            BrutexCrossverifyDailyOutcome::Partial,
        ] {
            assert_eq!(daily_outcome_rank(o.as_str()), 1, "{o:?} is measured");
        }
        for o in [
            BrutexCrossverifyDailyOutcome::NoData,
            BrutexCrossverifyDailyOutcome::Blind,
            BrutexCrossverifyDailyOutcome::Degraded,
        ] {
            assert_eq!(daily_outcome_rank(o.as_str()), 0, "{o:?} is unmeasured");
        }
    }

    #[test]
    fn test_append_cell_row_writes_kind_field_and_feed_symbols() {
        let mut w = BrutexCrossverifyWriter::for_test();
        w.append_cell_row(&sample_cell_row())
            .expect("append must succeed");
        assert_eq!(w.pending(), 1);
        let line = w.buffer_utf8();
        assert!(
            line.starts_with(BRUTEX_CROSSVERIFY_CELL_AUDIT_TABLE),
            "wrong table: {line}"
        );
        assert!(line.contains(",feed=groww"), "feed tag missing: {line}");
        assert!(line.contains(",kind=diverged"), "kind tag missing: {line}");
        assert!(line.contains(",field=high"), "field tag missing: {line}");
        assert!(
            line.contains(",exchange_segment=NSE_EQ"),
            "segment tag missing: {line}"
        );
        assert!(
            line.contains("brutex_paise=284755i"),
            "brutex paise column missing: {line}"
        );
        assert!(
            line.contains("live_paise=284750i"),
            "live paise column missing: {line}"
        );
    }

    #[test]
    fn test_append_cell_row_sanitizes_hostile_symbol() {
        // BruteX CSV symbols are UNTRUSTED external text — BiDi overrides and
        // control chars must never reach the table verbatim.
        let mut r = sample_cell_row();
        r.symbol = "REL\u{202e}IANCE\u{0007}".to_string();
        let mut w = BrutexCrossverifyWriter::for_test();
        w.append_cell_row(&r).expect("append must succeed");
        let line = w.buffer_utf8();
        assert!(
            !line.contains('\u{202e}'),
            "BiDi override must be stripped: {line:?}"
        );
        assert!(
            !line.contains('\u{0007}'),
            "control char must be stripped: {line:?}"
        );
    }

    #[test]
    fn test_append_daily_row_writes_outcome_and_run_row_shape() {
        let mut run_row = sample_daily_row();
        run_row.symbol = BRUTEX_CROSSVERIFY_RUN_ROW_SYMBOL.to_string();
        run_row.security_id = BRUTEX_CROSSVERIFY_MISSING_SENTINEL;
        run_row.exchange_segment = "none".to_string();
        run_row.unmapped_symbols = 2;
        run_row.objects_fetched = 5;
        let mut w = BrutexCrossverifyWriter::for_test();
        w.append_daily_row(&run_row).expect("append must succeed");
        assert_eq!(w.pending(), 1);
        let line = w.buffer_utf8();
        assert!(
            line.starts_with(BRUTEX_CROSSVERIFY_DAILY_TABLE),
            "wrong table: {line}"
        );
        assert!(
            line.contains(",symbol=__RUN__"),
            "run-row symbol missing: {line}"
        );
        assert!(
            line.contains(",outcome=diverged"),
            "outcome tag missing: {line}"
        );
        assert!(
            line.contains("security_id=-1i"),
            "run-row sentinel missing: {line}"
        );
        assert!(
            line.contains("objects_fetched=5i"),
            "objects_fetched missing: {line}"
        );
    }

    #[test]
    fn test_note_is_redacted_and_bounded() {
        // A JWT-shaped credential in the note must never reach the table,
        // and the note is truncated to the 200-char bound.
        let mut r = sample_cell_row();
        let jwt = format!(
            "eyJ{}.eyJ{}.sig{}",
            "a".repeat(40),
            "b".repeat(40),
            "c".repeat(200)
        );
        r.note = format!("fetch body token={jwt}");
        let mut w = BrutexCrossverifyWriter::for_test();
        w.append_cell_row(&r).expect("append must succeed");
        let line = w.buffer_utf8();
        assert!(
            !line.contains("eyJaaaa"),
            "JWT-shaped credential must be redacted: {line}"
        );
        let note_field = line.split("note=").nth(1).expect("note field present");
        assert!(
            note_field.len() <= BRUTEX_CROSSVERIFY_NOTE_MAX_CHARS + 210,
            "note must be bounded (field tail: {} bytes)",
            note_field.len()
        );
    }

    #[test]
    fn test_flush_when_disconnected_errors_and_keeps_rows_pending() {
        let mut w = BrutexCrossverifyWriter::for_test();
        w.append_cell_row(&sample_cell_row())
            .expect("append must succeed");
        w.append_daily_row(&sample_daily_row())
            .expect("append must succeed");
        let err = w.flush().expect_err("disconnected flush must error");
        assert!(err.to_string().contains("no ILP sender"));
        // Rows stay pending (not lost) for a later retry.
        assert_eq!(w.pending(), 2);
    }

    #[test]
    fn test_flush_empty_is_ok_even_disconnected() {
        let mut w = BrutexCrossverifyWriter::for_test();
        assert!(w.flush().is_ok(), "empty flush must be a no-op Ok");
    }

    #[test]
    fn test_writer_uses_ilp_http_conf() {
        // Transport ratchet (the 2026-07-05 fire-and-forget lesson): the
        // writer targets the HTTP port for a per-flush server ACK.
        let cfg = QuestDbConfig {
            host: "tv-questdb".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let conf = crossverify_ilp_http_conf(&cfg);
        assert_eq!(conf, "http::addr=tv-questdb:9000;");
        assert!(
            !conf.contains("9009"),
            "must target the HTTP port, not ILP TCP: {conf}"
        );
    }

    // ========================================================================
    // Persistence-helper tests — mock QuestDB /exec HTTP server + unreachable
    // host (the feed_episode_audit_persistence.rs P2C pattern). These
    // exercise the real ensure/constructor code paths: success (200),
    // non-2xx (500) and transport-error arms.
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
    async fn test_ensure_brutex_crossverify_tables_mock_200_completes() {
        // Success path: both CREATEs + feed self-heal ALTERs + DEDUP ENABLEs
        // all take the Ok(2xx) arm.
        let port = spawn_mock_http(MOCK_HTTP_200).await;
        ensure_brutex_crossverify_tables(&mock_cfg(port)).await;
    }

    #[tokio::test]
    async fn test_ensure_brutex_crossverify_tables_mock_500_degrades_without_panic() {
        // Non-2xx path: every DDL statement takes the log-and-continue arm
        // (best-effort degrade — BRUTEX-XVERIFY-02, never a panic).
        let port = spawn_mock_http(MOCK_HTTP_500).await;
        ensure_brutex_crossverify_tables(&mock_cfg(port)).await;
    }

    #[tokio::test]
    async fn test_ensure_brutex_crossverify_tables_unreachable_degrades_without_panic() {
        // Transport-error path: every DDL send Err arm logs and continues.
        ensure_brutex_crossverify_tables(&unreachable_cfg()).await;
    }

    #[tokio::test]
    async fn test_writer_new_is_lazy_and_buffers_without_network() {
        // `Sender::from_conf` with `http::` does not dial at construction
        // (the ws_event_audit precedent), so new() against an unreachable
        // host still builds a sender-backed writer whose appends land in the
        // local buffer — the lazy-construction contract.
        let mut w = BrutexCrossverifyWriter::new(&unreachable_cfg());
        assert_eq!(w.pending(), 0, "fresh writer has nothing pending");
        w.append_cell_row(&sample_cell_row())
            .expect("append must succeed without network");
        assert_eq!(w.pending(), 1, "row buffered locally");
    }
}

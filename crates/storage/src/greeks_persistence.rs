//! QuestDB persistence for computed Options Greeks, PCR snapshots, and
//! cross-verification results.
//!
//! # Tables
//! - `option_greeks` — per-contract Greeks (IV, Delta, Gamma, Theta, Vega) with security_id
//! - `pcr_snapshots` — per-underlying PCR ratio time series
//! - `greeks_verification` — cross-verification results (our Greeks vs Dhan API)
//!
//! # Dedup Strategy
//! All tables use `(security_id, segment)` as DEDUP UPSERT KEYS to prevent
//! duplicate rows on reconnect/restart. Same pattern as tick_persistence.

use std::time::Duration;

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, Sender, TimestampNanos};
use reqwest::Client;
use tracing::{debug, error, info, warn};

use dhan_live_trader_common::config::QuestDbConfig;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// DDL timeout for QuestDB HTTP queries.
const DDL_TIMEOUT_SECS: u64 = 10;

/// QuestDB table name for per-contract Greeks.
const TABLE_OPTION_GREEKS: &str = "option_greeks";

/// QuestDB table name for PCR snapshots.
const TABLE_PCR_SNAPSHOTS: &str = "pcr_snapshots";

/// QuestDB table name for cross-verification results.
const TABLE_GREEKS_VERIFICATION: &str = "greeks_verification";

// ---------------------------------------------------------------------------
// DDL — Table Creation
// ---------------------------------------------------------------------------

/// SQL to create the `option_greeks` table.
///
/// Stores per-contract Greeks computed from live tick data using Black-Scholes.
/// `security_id` is the primary identifier for dedup and cross-referencing
/// with ticks, instrument master, and Dhan API.
const OPTION_GREEKS_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS option_greeks (\
        segment SYMBOL,\
        security_id LONG,\
        symbol_name SYMBOL,\
        underlying_security_id LONG,\
        underlying_symbol SYMBOL,\
        strike_price DOUBLE,\
        option_type SYMBOL,\
        expiry_date SYMBOL,\
        iv DOUBLE,\
        delta DOUBLE,\
        gamma DOUBLE,\
        theta DOUBLE,\
        vega DOUBLE,\
        rho DOUBLE,\
        charm DOUBLE,\
        vanna DOUBLE,\
        volga DOUBLE,\
        veta DOUBLE,\
        speed DOUBLE,\
        color DOUBLE,\
        zomma DOUBLE,\
        ultima DOUBLE,\
        bs_price DOUBLE,\
        intrinsic_value DOUBLE,\
        extrinsic_value DOUBLE,\
        spot_price DOUBLE,\
        option_ltp DOUBLE,\
        oi LONG,\
        volume LONG,\
        buildup_type SYMBOL,\
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY HOUR WAL\
";

/// SQL to create the `pcr_snapshots` table.
///
/// Stores PCR ratio time series per underlying per expiry.
const PCR_SNAPSHOTS_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS pcr_snapshots (\
        underlying_symbol SYMBOL,\
        expiry_date SYMBOL,\
        pcr_oi DOUBLE,\
        pcr_volume DOUBLE,\
        total_put_oi LONG,\
        total_call_oi LONG,\
        total_put_volume LONG,\
        total_call_volume LONG,\
        sentiment SYMBOL,\
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY HOUR WAL\
";

/// QuestDB table name for tick-driven (candle-aligned) Greeks.
const TABLE_OPTION_GREEKS_LIVE: &str = "option_greeks_live";

/// QuestDB table name for tick-driven (candle-aligned) PCR snapshots.
const TABLE_PCR_SNAPSHOTS_LIVE: &str = "pcr_snapshots_live";

/// SQL to create the `option_greeks_live` table.
///
/// Tick-driven Greeks aligned to candle close boundaries.
/// `ts` = candle boundary timestamp (exchange clock), ensuring exact JOIN
/// with `candles_1s`/`candles_1m` on timestamp.
const OPTION_GREEKS_LIVE_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS option_greeks_live (\
        segment SYMBOL,\
        security_id LONG,\
        symbol_name SYMBOL,\
        underlying_security_id LONG,\
        underlying_symbol SYMBOL,\
        strike_price DOUBLE,\
        option_type SYMBOL,\
        expiry_date SYMBOL,\
        candle_interval SYMBOL,\
        iv DOUBLE,\
        delta DOUBLE,\
        gamma DOUBLE,\
        theta DOUBLE,\
        vega DOUBLE,\
        rho DOUBLE,\
        charm DOUBLE,\
        vanna DOUBLE,\
        volga DOUBLE,\
        veta DOUBLE,\
        speed DOUBLE,\
        color DOUBLE,\
        zomma DOUBLE,\
        ultima DOUBLE,\
        bs_price DOUBLE,\
        intrinsic_value DOUBLE,\
        extrinsic_value DOUBLE,\
        spot_price DOUBLE,\
        option_ltp DOUBLE,\
        oi LONG,\
        volume LONG,\
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY HOUR WAL\
";

/// SQL to create the `pcr_snapshots_live` table.
///
/// Tick-driven PCR aligned to candle close boundaries.
const PCR_SNAPSHOTS_LIVE_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS pcr_snapshots_live (\
        underlying_symbol SYMBOL,\
        expiry_date SYMBOL,\
        candle_interval SYMBOL,\
        pcr_oi DOUBLE,\
        pcr_volume DOUBLE,\
        total_put_oi LONG,\
        total_call_oi LONG,\
        total_put_volume LONG,\
        total_call_volume LONG,\
        sentiment SYMBOL,\
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY HOUR WAL\
";

/// DEDUP key for `option_greeks_live`.
const DEDUP_KEY_OPTION_GREEKS_LIVE: &str = "security_id, segment, candle_interval";

/// DEDUP key for `pcr_snapshots_live`.
const DEDUP_KEY_PCR_SNAPSHOTS_LIVE: &str = "underlying_symbol, expiry_date, candle_interval";

/// SQL to create the `dhan_option_chain_raw` table.
///
/// Stores raw Dhan Option Chain API response data **exactly as received**.
/// NEVER modify, transform, or recalculate any values — store as-is for audit,
/// cross-verification, and backtesting. One row per contract per snapshot.
const DHAN_OPTION_CHAIN_RAW_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS dhan_option_chain_raw (\
        security_id LONG,\
        segment SYMBOL,\
        symbol_name SYMBOL,\
        underlying_symbol SYMBOL,\
        underlying_security_id LONG,\
        underlying_segment SYMBOL,\
        strike_price DOUBLE,\
        option_type SYMBOL,\
        expiry_date SYMBOL,\
        spot_price DOUBLE,\
        last_price DOUBLE,\
        average_price DOUBLE,\
        oi LONG,\
        previous_close_price DOUBLE,\
        previous_oi LONG,\
        previous_volume LONG,\
        volume LONG,\
        top_bid_price DOUBLE,\
        top_bid_quantity LONG,\
        top_ask_price DOUBLE,\
        top_ask_quantity LONG,\
        implied_volatility DOUBLE,\
        delta DOUBLE,\
        theta DOUBLE,\
        gamma DOUBLE,\
        vega DOUBLE,\
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY HOUR WAL\
";

/// QuestDB table name for raw Dhan option chain snapshots.
const TABLE_DHAN_OPTION_CHAIN_RAW: &str = "dhan_option_chain_raw";

/// SQL to create the `greeks_verification` table.
///
/// Stores cross-verification results comparing our computed Greeks
/// against Dhan Option Chain API values. Used for quality assurance.
const GREEKS_VERIFICATION_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS greeks_verification (\
        security_id LONG,\
        segment SYMBOL,\
        symbol_name SYMBOL,\
        underlying_symbol SYMBOL,\
        strike_price DOUBLE,\
        option_type SYMBOL,\
        our_iv DOUBLE,\
        dhan_iv DOUBLE,\
        iv_diff DOUBLE,\
        our_delta DOUBLE,\
        dhan_delta DOUBLE,\
        delta_diff DOUBLE,\
        our_gamma DOUBLE,\
        dhan_gamma DOUBLE,\
        gamma_diff DOUBLE,\
        our_theta DOUBLE,\
        dhan_theta DOUBLE,\
        theta_diff DOUBLE,\
        our_vega DOUBLE,\
        dhan_vega DOUBLE,\
        vega_diff DOUBLE,\
        match_status SYMBOL,\
        ts TIMESTAMP\
    ) TIMESTAMP(ts) PARTITION BY DAY WAL\
";

// ---------------------------------------------------------------------------
// DEDUP UPSERT KEYS — must be separate ALTER TABLE (QuestDB rejects inline)
// ---------------------------------------------------------------------------

/// DEDUP key for `option_greeks`.
const DEDUP_KEY_OPTION_GREEKS: &str = "security_id, segment";

/// DEDUP key for `pcr_snapshots`.
const DEDUP_KEY_PCR_SNAPSHOTS: &str = "underlying_symbol, expiry_date";

/// DEDUP key for `dhan_option_chain_raw`.
const DEDUP_KEY_DHAN_OPTION_CHAIN_RAW: &str = "security_id, segment";

/// DEDUP key for `greeks_verification`.
const DEDUP_KEY_GREEKS_VERIFICATION: &str = "security_id, segment";

// ---------------------------------------------------------------------------
// Public API — Table Setup
// ---------------------------------------------------------------------------

/// Creates all Greeks-related QuestDB tables (idempotent).
///
/// Best-effort: if QuestDB is unreachable, logs a warning and continues.
// TEST-EXEMPT: requires live QuestDB HTTP endpoint (integration test)
pub async fn ensure_greeks_tables(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );

    let client = match Client::builder()
        .timeout(Duration::from_secs(DDL_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            warn!(?err, "failed to build HTTP client for Greeks DDL");
            return;
        }
    };

    // Step 1: CREATE TABLE IF NOT EXISTS (no inline DEDUP — QuestDB rejects it).
    execute_ddl(
        &client,
        &base_url,
        OPTION_GREEKS_DDL,
        "option_greeks CREATE",
    )
    .await;
    execute_ddl(
        &client,
        &base_url,
        PCR_SNAPSHOTS_DDL,
        "pcr_snapshots CREATE",
    )
    .await;
    execute_ddl(
        &client,
        &base_url,
        DHAN_OPTION_CHAIN_RAW_DDL,
        "dhan_option_chain_raw CREATE",
    )
    .await;
    execute_ddl(
        &client,
        &base_url,
        GREEKS_VERIFICATION_DDL,
        "greeks_verification CREATE",
    )
    .await;
    execute_ddl(
        &client,
        &base_url,
        OPTION_GREEKS_LIVE_DDL,
        "option_greeks_live CREATE",
    )
    .await;
    execute_ddl(
        &client,
        &base_url,
        PCR_SNAPSHOTS_LIVE_DDL,
        "pcr_snapshots_live CREATE",
    )
    .await;

    // Step 2: DEDUP UPSERT KEYS via ALTER TABLE (idempotent — re-enabling is a no-op).
    let dedup_statements: &[(&str, &str, &str)] = &[
        (
            TABLE_OPTION_GREEKS,
            DEDUP_KEY_OPTION_GREEKS,
            "option_greeks DEDUP",
        ),
        (
            TABLE_PCR_SNAPSHOTS,
            DEDUP_KEY_PCR_SNAPSHOTS,
            "pcr_snapshots DEDUP",
        ),
        (
            TABLE_DHAN_OPTION_CHAIN_RAW,
            DEDUP_KEY_DHAN_OPTION_CHAIN_RAW,
            "dhan_option_chain_raw DEDUP",
        ),
        (
            TABLE_GREEKS_VERIFICATION,
            DEDUP_KEY_GREEKS_VERIFICATION,
            "greeks_verification DEDUP",
        ),
        (
            TABLE_OPTION_GREEKS_LIVE,
            DEDUP_KEY_OPTION_GREEKS_LIVE,
            "option_greeks_live DEDUP",
        ),
        (
            TABLE_PCR_SNAPSHOTS_LIVE,
            DEDUP_KEY_PCR_SNAPSHOTS_LIVE,
            "pcr_snapshots_live DEDUP",
        ),
    ];
    for (table, dedup_key, label) in dedup_statements {
        let dedup_sql = format!("ALTER TABLE {table} DEDUP ENABLE UPSERT KEYS(ts, {dedup_key})");
        execute_ddl(&client, &base_url, &dedup_sql, label).await;
    }

    info!(
        "Greeks tables setup complete (option_greeks, pcr_snapshots, dhan_option_chain_raw, greeks_verification, option_greeks_live, pcr_snapshots_live)"
    );
}

/// Executes a DDL statement against QuestDB HTTP, logging warnings on failure.
async fn execute_ddl(client: &Client, base_url: &str, sql: &str, label: &str) {
    match client.get(base_url).query(&[("query", sql)]).send().await {
        Ok(response) => {
            if response.status().is_success() {
                debug!(label, "DDL executed successfully");
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                warn!(
                    %status,
                    label,
                    body = body.chars().take(200).collect::<String>(),
                    "DDL returned non-success"
                );
            }
        }
        Err(err) => {
            warn!(?err, label, "DDL request failed");
        }
    }
}

// ---------------------------------------------------------------------------
// ILP Writer — Batched writes to QuestDB
// ---------------------------------------------------------------------------

/// Max reconnection attempts for greeks writer.
const GREEKS_MAX_RECONNECT_ATTEMPTS: u32 = 3;
/// Initial backoff delay (ms) for greeks reconnection.
const GREEKS_RECONNECT_INITIAL_DELAY_MS: u64 = 1000;
/// Minimum interval between reconnection attempt cycles.
const GREEKS_RECONNECT_THROTTLE_SECS: u64 = 30;

/// Batched writer for all greeks-related QuestDB tables via ILP.
///
/// **Resilience:** On QuestDB disconnect, writes are silently skipped
/// (no ring buffer — Greeks are recomputed every second from live ticks).
/// Reconnection is attempted with exponential backoff, throttled to at
/// most once per `GREEKS_RECONNECT_THROTTLE_SECS`.
pub struct GreeksPersistenceWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending_count: usize,
    /// ILP connection config string, retained for reconnection.
    ilp_conf_string: String,
    /// Throttle: earliest time a reconnect may be attempted.
    next_reconnect_allowed: std::time::Instant,
}

impl GreeksPersistenceWriter {
    /// Creates a new writer connected to QuestDB via ILP TCP.
    ///
    /// If QuestDB is unreachable, initializes with `sender = None` and
    /// skips writes until reconnected.
    // TEST-EXEMPT: requires live QuestDB ILP connection (integration test)
    pub fn new(config: &QuestDbConfig) -> Result<Self> {
        let conf_string = format!("tcp::addr={}:{};", config.host, config.ilp_port);
        let (sender, buffer) = match Sender::from_conf(&conf_string) {
            Ok(s) => {
                let b = s.new_buffer();
                (Some(s), b)
            }
            Err(err) => {
                warn!(
                    ?err,
                    "greeks writer: QuestDB unreachable at startup — skipping writes"
                );
                (None, Buffer::new(questdb::ingress::ProtocolVersion::V1))
            }
        };
        Ok(Self {
            sender,
            buffer,
            pending_count: 0,
            ilp_conf_string: conf_string,
            next_reconnect_allowed: std::time::Instant::now(),
        })
    }

    /// Writes a raw Dhan option chain row to `dhan_option_chain_raw`.
    ///
    /// Stores the API response exactly as received — no transformation.
    // TEST-EXEMPT: delegates to build_dhan_raw_row which is tested via DDL schema tests
    pub fn write_dhan_raw_row(&mut self, row: &DhanRawRow) -> Result<()> {
        build_dhan_raw_row(&mut self.buffer, row)?;
        self.pending_count = self.pending_count.saturating_add(1);
        Ok(())
    }

    /// Writes a computed Greeks row to `option_greeks`.
    // TEST-EXEMPT: delegates to build_option_greeks_row which is tested via DDL schema tests
    pub fn write_option_greeks_row(&mut self, row: &OptionGreeksRow) -> Result<()> {
        build_option_greeks_row(&mut self.buffer, row)?;
        self.pending_count = self.pending_count.saturating_add(1);
        Ok(())
    }

    /// Writes a PCR snapshot row to `pcr_snapshots`.
    // TEST-EXEMPT: delegates to build_pcr_snapshot_row which is tested via DDL schema tests
    pub fn write_pcr_snapshot_row(&mut self, row: &PcrSnapshotRow) -> Result<()> {
        build_pcr_snapshot_row(&mut self.buffer, row)?;
        self.pending_count = self.pending_count.saturating_add(1);
        Ok(())
    }

    /// Writes a cross-verification row to `greeks_verification`.
    // TEST-EXEMPT: delegates to build_verification_row which is tested via DDL schema tests
    pub fn write_verification_row(&mut self, row: &VerificationRow) -> Result<()> {
        build_verification_row(&mut self.buffer, row)?;
        self.pending_count = self.pending_count.saturating_add(1);
        Ok(())
    }

    /// Writes a tick-driven (candle-aligned) Greeks row to `option_greeks_live`.
    // TEST-EXEMPT: delegates to build_option_greeks_live_row which is tested via ILP builder tests
    pub fn write_option_greeks_live_row(&mut self, row: &OptionGreeksLiveRow) -> Result<()> {
        build_option_greeks_live_row(&mut self.buffer, row)?;
        self.pending_count = self.pending_count.saturating_add(1);
        Ok(())
    }

    /// Writes a tick-driven (candle-aligned) PCR row to `pcr_snapshots_live`.
    // TEST-EXEMPT: delegates to build_pcr_snapshot_live_row which is tested via ILP builder tests
    pub fn write_pcr_snapshot_live_row(&mut self, row: &PcrSnapshotLiveRow) -> Result<()> {
        build_pcr_snapshot_live_row(&mut self.buffer, row)?;
        self.pending_count = self.pending_count.saturating_add(1);
        Ok(())
    }

    /// Flushes all pending rows to QuestDB.
    ///
    /// On flush error, sets sender to `None` (triggers reconnect on next write).
    /// Returns `Ok(())` even on failure — resilience guarantee.
    // TEST-EXEMPT: requires live QuestDB ILP connection (integration test)
    pub fn flush(&mut self) -> Result<()> {
        if self.pending_count == 0 {
            return Ok(());
        }

        // If disconnected, try reconnect first.
        if self.sender.is_none() {
            self.try_reconnect_on_error();
        }

        let count = self.pending_count;
        if let Some(ref mut sender) = self.sender {
            if let Err(err) = sender.flush(&mut self.buffer) {
                warn!(
                    ?err,
                    pending = count,
                    "greeks flush failed — disconnecting sender"
                );
                self.sender = None;
                self.buffer.clear();
                self.pending_count = 0;
                return Ok(()); // Swallow — Greeks are recomputed next second
            }
        } else {
            // No sender — discard buffered ILP rows (will be recomputed).
            self.buffer.clear();
            self.pending_count = 0;
            return Ok(());
        }

        self.pending_count = 0;
        debug!(rows = count, "greeks data flushed to QuestDB");
        Ok(())
    }

    /// Returns the number of pending (unflushed) rows.
    // TEST-EXEMPT: trivial accessor, tested indirectly via pipeline cycle logs
    pub fn pending_count(&self) -> usize {
        self.pending_count
    }

    /// Attempts to reconnect to QuestDB with exponential backoff.
    fn reconnect(&mut self) -> Result<()> {
        let mut delay_ms = GREEKS_RECONNECT_INITIAL_DELAY_MS;

        for attempt in 1..=GREEKS_MAX_RECONNECT_ATTEMPTS {
            warn!(
                attempt,
                max_attempts = GREEKS_MAX_RECONNECT_ATTEMPTS,
                delay_ms,
                "attempting QuestDB ILP reconnection for greeks writer"
            );

            std::thread::sleep(Duration::from_millis(delay_ms));

            match Sender::from_conf(&self.ilp_conf_string) {
                Ok(new_sender) => {
                    self.buffer = new_sender.new_buffer();
                    self.sender = Some(new_sender);
                    self.pending_count = 0;
                    info!(
                        attempt,
                        "QuestDB ILP reconnection succeeded for greeks writer"
                    );
                    return Ok(());
                }
                Err(err) => {
                    error!(
                        attempt,
                        max_attempts = GREEKS_MAX_RECONNECT_ATTEMPTS,
                        ?err,
                        "QuestDB ILP reconnection failed for greeks writer"
                    );
                    delay_ms = delay_ms.saturating_mul(2);
                }
            }
        }

        anyhow::bail!(
            "QuestDB ILP reconnection failed after {} attempts for greeks writer",
            GREEKS_MAX_RECONNECT_ATTEMPTS,
        )
    }

    /// Throttled reconnect — at most once per `GREEKS_RECONNECT_THROTTLE_SECS`.
    fn try_reconnect_on_error(&mut self) {
        if self.sender.is_some() {
            return;
        }
        let now = std::time::Instant::now();
        if now < self.next_reconnect_allowed {
            return;
        }
        self.next_reconnect_allowed = now + Duration::from_secs(GREEKS_RECONNECT_THROTTLE_SECS);
        if let Err(err) = self.reconnect() {
            warn!(?err, "greeks writer reconnect failed — will retry later");
        }
    }
}

// ---------------------------------------------------------------------------
// Row Types — Input structs for ILP row builders
// ---------------------------------------------------------------------------

/// Raw Dhan Option Chain API response row (stored as-is).
pub struct DhanRawRow<'a> {
    pub security_id: i64,
    pub segment: &'a str,
    pub symbol_name: &'a str,
    pub underlying_symbol: &'a str,
    pub underlying_security_id: i64,
    pub underlying_segment: &'a str,
    pub strike_price: f64,
    pub option_type: &'a str,
    pub expiry_date: &'a str,
    pub spot_price: f64,
    pub last_price: f64,
    pub average_price: f64,
    pub oi: i64,
    pub previous_close_price: f64,
    pub previous_oi: i64,
    pub previous_volume: i64,
    pub volume: i64,
    pub top_bid_price: f64,
    pub top_bid_quantity: i64,
    pub top_ask_price: f64,
    pub top_ask_quantity: i64,
    pub implied_volatility: f64,
    pub delta: f64,
    pub theta: f64,
    pub gamma: f64,
    pub vega: f64,
    pub ts_nanos: i64,
}

/// Computed Greeks row for `option_greeks` table.
pub struct OptionGreeksRow<'a> {
    pub segment: &'a str,
    pub security_id: i64,
    pub symbol_name: &'a str,
    pub underlying_security_id: i64,
    pub underlying_symbol: &'a str,
    pub strike_price: f64,
    pub option_type: &'a str,
    pub expiry_date: &'a str,
    pub iv: f64,
    pub delta: f64,
    pub gamma: f64,
    pub theta: f64,
    pub vega: f64,
    pub rho: f64,
    pub charm: f64,
    pub vanna: f64,
    pub volga: f64,
    pub veta: f64,
    pub speed: f64,
    pub color: f64,
    pub zomma: f64,
    pub ultima: f64,
    pub bs_price: f64,
    pub intrinsic_value: f64,
    pub extrinsic_value: f64,
    pub spot_price: f64,
    pub option_ltp: f64,
    pub oi: i64,
    pub volume: i64,
    pub buildup_type: &'a str,
    pub ts_nanos: i64,
}

/// PCR snapshot row for `pcr_snapshots` table.
pub struct PcrSnapshotRow<'a> {
    pub underlying_symbol: &'a str,
    pub expiry_date: &'a str,
    pub pcr_oi: f64,
    pub pcr_volume: f64,
    pub total_put_oi: i64,
    pub total_call_oi: i64,
    pub total_put_volume: i64,
    pub total_call_volume: i64,
    pub sentiment: &'a str,
    pub ts_nanos: i64,
}

/// Cross-verification row for `greeks_verification` table.
pub struct VerificationRow<'a> {
    pub security_id: i64,
    pub segment: &'a str,
    pub symbol_name: &'a str,
    pub underlying_symbol: &'a str,
    pub strike_price: f64,
    pub option_type: &'a str,
    pub our_iv: f64,
    pub dhan_iv: f64,
    pub iv_diff: f64,
    pub our_delta: f64,
    pub dhan_delta: f64,
    pub delta_diff: f64,
    pub our_gamma: f64,
    pub dhan_gamma: f64,
    pub gamma_diff: f64,
    pub our_theta: f64,
    pub dhan_theta: f64,
    pub theta_diff: f64,
    pub our_vega: f64,
    pub dhan_vega: f64,
    pub vega_diff: f64,
    pub match_status: &'a str,
    pub ts_nanos: i64,
}

/// Tick-driven (candle-aligned) Greeks row for `option_greeks_live` table.
pub struct OptionGreeksLiveRow<'a> {
    pub segment: &'a str,
    pub security_id: i64,
    pub symbol_name: &'a str,
    pub underlying_security_id: i64,
    pub underlying_symbol: &'a str,
    pub strike_price: f64,
    pub option_type: &'a str,
    pub expiry_date: &'a str,
    pub candle_interval: &'a str,
    pub iv: f64,
    pub delta: f64,
    pub gamma: f64,
    pub theta: f64,
    pub vega: f64,
    pub rho: f64,
    pub charm: f64,
    pub vanna: f64,
    pub volga: f64,
    pub veta: f64,
    pub speed: f64,
    pub color: f64,
    pub zomma: f64,
    pub ultima: f64,
    pub bs_price: f64,
    pub intrinsic_value: f64,
    pub extrinsic_value: f64,
    pub spot_price: f64,
    pub option_ltp: f64,
    pub oi: i64,
    pub volume: i64,
    pub ts_nanos: i64,
}

/// Tick-driven (candle-aligned) PCR row for `pcr_snapshots_live` table.
pub struct PcrSnapshotLiveRow<'a> {
    pub underlying_symbol: &'a str,
    pub expiry_date: &'a str,
    pub candle_interval: &'a str,
    pub pcr_oi: f64,
    pub pcr_volume: f64,
    pub total_put_oi: i64,
    pub total_call_oi: i64,
    pub total_put_volume: i64,
    pub total_call_volume: i64,
    pub sentiment: &'a str,
    pub ts_nanos: i64,
}

// ---------------------------------------------------------------------------
// ILP Row Builders (extracted for testability)
// ---------------------------------------------------------------------------

/// Writes a single `dhan_option_chain_raw` row into the ILP buffer.
fn build_dhan_raw_row(buffer: &mut Buffer, row: &DhanRawRow<'_>) -> Result<()> {
    let ts = TimestampNanos::new(row.ts_nanos);
    buffer
        .table(TABLE_DHAN_OPTION_CHAIN_RAW)
        .context("table")?
        .symbol("segment", row.segment)
        .context("segment")?
        .symbol("symbol_name", row.symbol_name)
        .context("symbol_name")?
        .symbol("underlying_symbol", row.underlying_symbol)
        .context("underlying_symbol")?
        .symbol("underlying_segment", row.underlying_segment)
        .context("underlying_segment")?
        .symbol("option_type", row.option_type)
        .context("option_type")?
        .symbol("expiry_date", row.expiry_date)
        .context("expiry_date")?
        .column_i64("security_id", row.security_id)
        .context("security_id")?
        .column_i64("underlying_security_id", row.underlying_security_id)
        .context("underlying_security_id")?
        .column_f64("strike_price", row.strike_price)
        .context("strike_price")?
        .column_f64("spot_price", row.spot_price)
        .context("spot_price")?
        .column_f64("last_price", row.last_price)
        .context("last_price")?
        .column_f64("average_price", row.average_price)
        .context("average_price")?
        .column_i64("oi", row.oi)
        .context("oi")?
        .column_f64("previous_close_price", row.previous_close_price)
        .context("previous_close_price")?
        .column_i64("previous_oi", row.previous_oi)
        .context("previous_oi")?
        .column_i64("previous_volume", row.previous_volume)
        .context("previous_volume")?
        .column_i64("volume", row.volume)
        .context("volume")?
        .column_f64("top_bid_price", row.top_bid_price)
        .context("top_bid_price")?
        .column_i64("top_bid_quantity", row.top_bid_quantity)
        .context("top_bid_quantity")?
        .column_f64("top_ask_price", row.top_ask_price)
        .context("top_ask_price")?
        .column_i64("top_ask_quantity", row.top_ask_quantity)
        .context("top_ask_quantity")?
        .column_f64("implied_volatility", row.implied_volatility)
        .context("implied_volatility")?
        .column_f64("delta", row.delta)
        .context("delta")?
        .column_f64("theta", row.theta)
        .context("theta")?
        .column_f64("gamma", row.gamma)
        .context("gamma")?
        .column_f64("vega", row.vega)
        .context("vega")?
        .at(ts)
        .context("ts")?;
    Ok(())
}

/// Writes a single `option_greeks` row into the ILP buffer.
fn build_option_greeks_row(buffer: &mut Buffer, row: &OptionGreeksRow<'_>) -> Result<()> {
    let ts = TimestampNanos::new(row.ts_nanos);
    buffer
        .table(TABLE_OPTION_GREEKS)
        .context("table")?
        .symbol("segment", row.segment)
        .context("segment")?
        .symbol("symbol_name", row.symbol_name)
        .context("symbol_name")?
        .symbol("underlying_symbol", row.underlying_symbol)
        .context("underlying_symbol")?
        .symbol("option_type", row.option_type)
        .context("option_type")?
        .symbol("expiry_date", row.expiry_date)
        .context("expiry_date")?
        .symbol("buildup_type", row.buildup_type)
        .context("buildup_type")?
        .column_i64("security_id", row.security_id)
        .context("security_id")?
        .column_i64("underlying_security_id", row.underlying_security_id)
        .context("underlying_security_id")?
        .column_f64("strike_price", row.strike_price)
        .context("strike_price")?
        .column_f64("iv", row.iv)
        .context("iv")?
        .column_f64("delta", row.delta)
        .context("delta")?
        .column_f64("gamma", row.gamma)
        .context("gamma")?
        .column_f64("theta", row.theta)
        .context("theta")?
        .column_f64("vega", row.vega)
        .context("vega")?
        .column_f64("rho", row.rho)
        .context("rho")?
        .column_f64("charm", row.charm)
        .context("charm")?
        .column_f64("vanna", row.vanna)
        .context("vanna")?
        .column_f64("volga", row.volga)
        .context("volga")?
        .column_f64("veta", row.veta)
        .context("veta")?
        .column_f64("speed", row.speed)
        .context("speed")?
        .column_f64("color", row.color)
        .context("color")?
        .column_f64("zomma", row.zomma)
        .context("zomma")?
        .column_f64("ultima", row.ultima)
        .context("ultima")?
        .column_f64("bs_price", row.bs_price)
        .context("bs_price")?
        .column_f64("intrinsic_value", row.intrinsic_value)
        .context("intrinsic_value")?
        .column_f64("extrinsic_value", row.extrinsic_value)
        .context("extrinsic_value")?
        .column_f64("spot_price", row.spot_price)
        .context("spot_price")?
        .column_f64("option_ltp", row.option_ltp)
        .context("option_ltp")?
        .column_i64("oi", row.oi)
        .context("oi")?
        .column_i64("volume", row.volume)
        .context("volume")?
        .at(ts)
        .context("ts")?;
    Ok(())
}

/// Writes a single `pcr_snapshots` row into the ILP buffer.
fn build_pcr_snapshot_row(buffer: &mut Buffer, row: &PcrSnapshotRow<'_>) -> Result<()> {
    let ts = TimestampNanos::new(row.ts_nanos);
    buffer
        .table(TABLE_PCR_SNAPSHOTS)
        .context("table")?
        .symbol("underlying_symbol", row.underlying_symbol)
        .context("underlying_symbol")?
        .symbol("expiry_date", row.expiry_date)
        .context("expiry_date")?
        .symbol("sentiment", row.sentiment)
        .context("sentiment")?
        .column_f64("pcr_oi", row.pcr_oi)
        .context("pcr_oi")?
        .column_f64("pcr_volume", row.pcr_volume)
        .context("pcr_volume")?
        .column_i64("total_put_oi", row.total_put_oi)
        .context("total_put_oi")?
        .column_i64("total_call_oi", row.total_call_oi)
        .context("total_call_oi")?
        .column_i64("total_put_volume", row.total_put_volume)
        .context("total_put_volume")?
        .column_i64("total_call_volume", row.total_call_volume)
        .context("total_call_volume")?
        .at(ts)
        .context("ts")?;
    Ok(())
}

/// Writes a single `greeks_verification` row into the ILP buffer.
fn build_verification_row(buffer: &mut Buffer, row: &VerificationRow<'_>) -> Result<()> {
    let ts = TimestampNanos::new(row.ts_nanos);
    buffer
        .table(TABLE_GREEKS_VERIFICATION)
        .context("table")?
        .symbol("segment", row.segment)
        .context("segment")?
        .symbol("symbol_name", row.symbol_name)
        .context("symbol_name")?
        .symbol("underlying_symbol", row.underlying_symbol)
        .context("underlying_symbol")?
        .symbol("option_type", row.option_type)
        .context("option_type")?
        .symbol("match_status", row.match_status)
        .context("match_status")?
        .column_i64("security_id", row.security_id)
        .context("security_id")?
        .column_f64("strike_price", row.strike_price)
        .context("strike_price")?
        .column_f64("our_iv", row.our_iv)
        .context("our_iv")?
        .column_f64("dhan_iv", row.dhan_iv)
        .context("dhan_iv")?
        .column_f64("iv_diff", row.iv_diff)
        .context("iv_diff")?
        .column_f64("our_delta", row.our_delta)
        .context("our_delta")?
        .column_f64("dhan_delta", row.dhan_delta)
        .context("dhan_delta")?
        .column_f64("delta_diff", row.delta_diff)
        .context("delta_diff")?
        .column_f64("our_gamma", row.our_gamma)
        .context("our_gamma")?
        .column_f64("dhan_gamma", row.dhan_gamma)
        .context("dhan_gamma")?
        .column_f64("gamma_diff", row.gamma_diff)
        .context("gamma_diff")?
        .column_f64("our_theta", row.our_theta)
        .context("our_theta")?
        .column_f64("dhan_theta", row.dhan_theta)
        .context("dhan_theta")?
        .column_f64("theta_diff", row.theta_diff)
        .context("theta_diff")?
        .column_f64("our_vega", row.our_vega)
        .context("our_vega")?
        .column_f64("dhan_vega", row.dhan_vega)
        .context("dhan_vega")?
        .column_f64("vega_diff", row.vega_diff)
        .context("vega_diff")?
        .at(ts)
        .context("ts")?;
    Ok(())
}

/// Writes a single `option_greeks_live` row into the ILP buffer.
fn build_option_greeks_live_row(buffer: &mut Buffer, row: &OptionGreeksLiveRow<'_>) -> Result<()> {
    let ts = TimestampNanos::new(row.ts_nanos);
    buffer
        .table(TABLE_OPTION_GREEKS_LIVE)
        .context("table")?
        .symbol("segment", row.segment)
        .context("segment")?
        .symbol("symbol_name", row.symbol_name)
        .context("symbol_name")?
        .symbol("underlying_symbol", row.underlying_symbol)
        .context("underlying_symbol")?
        .symbol("option_type", row.option_type)
        .context("option_type")?
        .symbol("expiry_date", row.expiry_date)
        .context("expiry_date")?
        .symbol("candle_interval", row.candle_interval)
        .context("candle_interval")?
        .column_i64("security_id", row.security_id)
        .context("security_id")?
        .column_i64("underlying_security_id", row.underlying_security_id)
        .context("underlying_security_id")?
        .column_f64("strike_price", row.strike_price)
        .context("strike_price")?
        .column_f64("iv", row.iv)
        .context("iv")?
        .column_f64("delta", row.delta)
        .context("delta")?
        .column_f64("gamma", row.gamma)
        .context("gamma")?
        .column_f64("theta", row.theta)
        .context("theta")?
        .column_f64("vega", row.vega)
        .context("vega")?
        .column_f64("rho", row.rho)
        .context("rho")?
        .column_f64("charm", row.charm)
        .context("charm")?
        .column_f64("vanna", row.vanna)
        .context("vanna")?
        .column_f64("volga", row.volga)
        .context("volga")?
        .column_f64("veta", row.veta)
        .context("veta")?
        .column_f64("speed", row.speed)
        .context("speed")?
        .column_f64("color", row.color)
        .context("color")?
        .column_f64("zomma", row.zomma)
        .context("zomma")?
        .column_f64("ultima", row.ultima)
        .context("ultima")?
        .column_f64("bs_price", row.bs_price)
        .context("bs_price")?
        .column_f64("intrinsic_value", row.intrinsic_value)
        .context("intrinsic_value")?
        .column_f64("extrinsic_value", row.extrinsic_value)
        .context("extrinsic_value")?
        .column_f64("spot_price", row.spot_price)
        .context("spot_price")?
        .column_f64("option_ltp", row.option_ltp)
        .context("option_ltp")?
        .column_i64("oi", row.oi)
        .context("oi")?
        .column_i64("volume", row.volume)
        .context("volume")?
        .at(ts)
        .context("ts")?;
    Ok(())
}

/// Writes a single `pcr_snapshots_live` row into the ILP buffer.
fn build_pcr_snapshot_live_row(buffer: &mut Buffer, row: &PcrSnapshotLiveRow<'_>) -> Result<()> {
    let ts = TimestampNanos::new(row.ts_nanos);
    buffer
        .table(TABLE_PCR_SNAPSHOTS_LIVE)
        .context("table")?
        .symbol("underlying_symbol", row.underlying_symbol)
        .context("underlying_symbol")?
        .symbol("expiry_date", row.expiry_date)
        .context("expiry_date")?
        .symbol("candle_interval", row.candle_interval)
        .context("candle_interval")?
        .symbol("sentiment", row.sentiment)
        .context("sentiment")?
        .column_f64("pcr_oi", row.pcr_oi)
        .context("pcr_oi")?
        .column_f64("pcr_volume", row.pcr_volume)
        .context("pcr_volume")?
        .column_i64("total_put_oi", row.total_put_oi)
        .context("total_put_oi")?
        .column_i64("total_call_oi", row.total_call_oi)
        .context("total_call_oi")?
        .column_i64("total_put_volume", row.total_put_volume)
        .context("total_put_volume")?
        .column_i64("total_call_volume", row.total_call_volume)
        .context("total_call_volume")?
        .at(ts)
        .context("ts")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- DDL SQL validation ---

    #[test]
    fn test_option_greeks_ddl_contains_security_id() {
        assert!(
            OPTION_GREEKS_DDL.contains("security_id LONG"),
            "option_greeks must have security_id for dedup"
        );
    }

    #[test]
    fn test_option_greeks_dedup_key_constant() {
        assert!(
            DEDUP_KEY_OPTION_GREEKS.contains("security_id"),
            "option_greeks dedup must include security_id"
        );
        assert!(
            DEDUP_KEY_OPTION_GREEKS.contains("segment"),
            "option_greeks dedup must include segment"
        );
    }

    #[test]
    fn test_option_greeks_ddl_has_all_greeks_columns() {
        for col in [
            "iv DOUBLE",
            "delta DOUBLE",
            "gamma DOUBLE",
            "theta DOUBLE",
            "vega DOUBLE",
        ] {
            assert!(
                OPTION_GREEKS_DDL.contains(col),
                "option_greeks missing column: {col}"
            );
        }
    }

    #[test]
    fn test_option_greeks_ddl_has_underlying_fields() {
        assert!(OPTION_GREEKS_DDL.contains("underlying_security_id LONG"));
        assert!(OPTION_GREEKS_DDL.contains("underlying_symbol SYMBOL"));
        assert!(OPTION_GREEKS_DDL.contains("strike_price DOUBLE"));
        assert!(OPTION_GREEKS_DDL.contains("option_type SYMBOL"));
        assert!(OPTION_GREEKS_DDL.contains("expiry_date SYMBOL"));
    }

    #[test]
    fn test_option_greeks_ddl_has_market_data() {
        assert!(OPTION_GREEKS_DDL.contains("spot_price DOUBLE"));
        assert!(OPTION_GREEKS_DDL.contains("option_ltp DOUBLE"));
        assert!(OPTION_GREEKS_DDL.contains("oi LONG"));
        assert!(OPTION_GREEKS_DDL.contains("volume LONG"));
        assert!(OPTION_GREEKS_DDL.contains("buildup_type SYMBOL"));
    }

    #[test]
    fn test_option_greeks_ddl_has_bs_values() {
        assert!(OPTION_GREEKS_DDL.contains("bs_price DOUBLE"));
        assert!(OPTION_GREEKS_DDL.contains("intrinsic_value DOUBLE"));
        assert!(OPTION_GREEKS_DDL.contains("extrinsic_value DOUBLE"));
    }

    #[test]
    fn test_pcr_snapshots_dedup_key_constant() {
        assert!(
            DEDUP_KEY_PCR_SNAPSHOTS.contains("underlying_symbol"),
            "pcr_snapshots dedup must include underlying_symbol"
        );
        assert!(
            DEDUP_KEY_PCR_SNAPSHOTS.contains("expiry_date"),
            "pcr_snapshots dedup must include expiry_date"
        );
    }

    #[test]
    fn test_pcr_snapshots_ddl_has_all_fields() {
        for col in [
            "pcr_oi DOUBLE",
            "pcr_volume DOUBLE",
            "total_put_oi LONG",
            "total_call_oi LONG",
            "total_put_volume LONG",
            "total_call_volume LONG",
            "sentiment SYMBOL",
        ] {
            assert!(
                PCR_SNAPSHOTS_DDL.contains(col),
                "pcr_snapshots missing: {col}"
            );
        }
    }

    #[test]
    fn test_verification_ddl_has_both_sources() {
        // Must have both our values and Dhan values for comparison.
        for prefix in ["our_", "dhan_"] {
            for greek in ["iv", "delta", "gamma", "theta", "vega"] {
                let col = format!("{prefix}{greek} DOUBLE");
                assert!(
                    GREEKS_VERIFICATION_DDL.contains(&col),
                    "greeks_verification missing: {col}"
                );
            }
        }
    }

    #[test]
    fn test_verification_ddl_has_diff_columns() {
        for greek in [
            "iv_diff",
            "delta_diff",
            "gamma_diff",
            "theta_diff",
            "vega_diff",
        ] {
            let col = format!("{greek} DOUBLE");
            assert!(
                GREEKS_VERIFICATION_DDL.contains(&col),
                "greeks_verification missing diff: {col}"
            );
        }
    }

    #[test]
    fn test_verification_ddl_has_match_status() {
        assert!(GREEKS_VERIFICATION_DDL.contains("match_status SYMBOL"));
    }

    #[test]
    fn test_verification_ddl_has_security_id() {
        assert!(GREEKS_VERIFICATION_DDL.contains("security_id LONG"));
    }

    #[test]
    fn test_verification_dedup_key_constant() {
        assert!(DEDUP_KEY_GREEKS_VERIFICATION.contains("security_id"));
        assert!(DEDUP_KEY_GREEKS_VERIFICATION.contains("segment"));
    }

    #[test]
    fn test_all_tables_have_timestamp() {
        assert!(OPTION_GREEKS_DDL.contains("TIMESTAMP(ts)"));
        assert!(PCR_SNAPSHOTS_DDL.contains("TIMESTAMP(ts)"));
        assert!(GREEKS_VERIFICATION_DDL.contains("TIMESTAMP(ts)"));
    }

    #[test]
    fn test_all_tables_have_wal() {
        assert!(OPTION_GREEKS_DDL.contains("WAL"));
        assert!(PCR_SNAPSHOTS_DDL.contains("WAL"));
        assert!(GREEKS_VERIFICATION_DDL.contains("WAL"));
    }

    #[test]
    fn test_all_tables_idempotent() {
        assert!(OPTION_GREEKS_DDL.contains("IF NOT EXISTS"));
        assert!(PCR_SNAPSHOTS_DDL.contains("IF NOT EXISTS"));
        assert!(GREEKS_VERIFICATION_DDL.contains("IF NOT EXISTS"));
    }

    #[test]
    fn test_option_greeks_ddl_has_symbol_name() {
        assert!(
            OPTION_GREEKS_DDL.contains("symbol_name SYMBOL"),
            "option_greeks must have symbol_name for cross-referencing"
        );
    }

    #[test]
    fn test_verification_ddl_has_symbol_name() {
        assert!(
            GREEKS_VERIFICATION_DDL.contains("symbol_name SYMBOL"),
            "greeks_verification must have symbol_name for cross-referencing"
        );
    }

    // --- Raw Dhan option chain table tests ---

    #[test]
    fn test_dhan_raw_ddl_has_all_dhan_api_fields() {
        // Every field from Dhan Option Chain API response must be stored as-is.
        for col in [
            "security_id LONG",
            "last_price DOUBLE",
            "average_price DOUBLE",
            "oi LONG",
            "previous_close_price DOUBLE",
            "previous_oi LONG",
            "previous_volume LONG",
            "volume LONG",
            "top_bid_price DOUBLE",
            "top_bid_quantity LONG",
            "top_ask_price DOUBLE",
            "top_ask_quantity LONG",
            "implied_volatility DOUBLE",
            "delta DOUBLE",
            "theta DOUBLE",
            "gamma DOUBLE",
            "vega DOUBLE",
        ] {
            assert!(
                DHAN_OPTION_CHAIN_RAW_DDL.contains(col),
                "dhan_option_chain_raw missing Dhan API field: {col}"
            );
        }
    }

    #[test]
    fn test_dhan_raw_ddl_has_identity_fields() {
        for col in [
            "underlying_symbol SYMBOL",
            "underlying_security_id LONG",
            "strike_price DOUBLE",
            "option_type SYMBOL",
            "expiry_date SYMBOL",
            "spot_price DOUBLE",
            "symbol_name SYMBOL",
            "segment SYMBOL",
        ] {
            assert!(
                DHAN_OPTION_CHAIN_RAW_DDL.contains(col),
                "dhan_option_chain_raw missing identity field: {col}"
            );
        }
    }

    #[test]
    fn test_dhan_raw_ddl_partition_and_wal() {
        assert!(DHAN_OPTION_CHAIN_RAW_DDL.contains("PARTITION BY HOUR WAL"));
        assert!(DHAN_OPTION_CHAIN_RAW_DDL.contains("TIMESTAMP(ts)"));
        assert!(DHAN_OPTION_CHAIN_RAW_DDL.contains("IF NOT EXISTS"));
    }

    #[test]
    fn test_dhan_raw_dedup_key_constant() {
        assert!(DEDUP_KEY_DHAN_OPTION_CHAIN_RAW.contains("security_id"));
        assert!(DEDUP_KEY_DHAN_OPTION_CHAIN_RAW.contains("segment"));
    }

    #[test]
    fn test_dhan_raw_table_name() {
        assert_eq!(TABLE_DHAN_OPTION_CHAIN_RAW, "dhan_option_chain_raw");
    }

    #[test]
    fn test_table_names() {
        assert_eq!(TABLE_OPTION_GREEKS, "option_greeks");
        assert_eq!(TABLE_PCR_SNAPSHOTS, "pcr_snapshots");
        assert_eq!(TABLE_GREEKS_VERIFICATION, "greeks_verification");
        assert_eq!(TABLE_OPTION_GREEKS_LIVE, "option_greeks_live");
        assert_eq!(TABLE_PCR_SNAPSHOTS_LIVE, "pcr_snapshots_live");
    }

    // --- option_greeks_live DDL tests ---

    #[test]
    fn test_option_greeks_live_ddl() {
        assert!(OPTION_GREEKS_LIVE_DDL.contains("IF NOT EXISTS"));
        assert!(OPTION_GREEKS_LIVE_DDL.contains("TIMESTAMP(ts)"));
        assert!(OPTION_GREEKS_LIVE_DDL.contains("PARTITION BY HOUR WAL"));
        assert!(OPTION_GREEKS_LIVE_DDL.contains("candle_interval SYMBOL"));
        assert!(OPTION_GREEKS_LIVE_DDL.contains("security_id LONG"));
        assert!(OPTION_GREEKS_LIVE_DDL.contains("delta DOUBLE"));
        assert!(OPTION_GREEKS_LIVE_DDL.contains("iv DOUBLE"));
        assert!(OPTION_GREEKS_LIVE_DDL.contains("spot_price DOUBLE"));
    }

    #[test]
    fn test_dedup_key_includes_candle_interval() {
        assert!(
            DEDUP_KEY_OPTION_GREEKS_LIVE.contains("candle_interval"),
            "live greeks dedup must include candle_interval"
        );
        assert!(
            DEDUP_KEY_OPTION_GREEKS_LIVE.contains("security_id"),
            "live greeks dedup must include security_id"
        );
        assert!(
            DEDUP_KEY_OPTION_GREEKS_LIVE.contains("segment"),
            "live greeks dedup must include segment"
        );
    }

    // --- pcr_snapshots_live DDL tests ---

    #[test]
    fn test_pcr_snapshots_live_ddl() {
        assert!(PCR_SNAPSHOTS_LIVE_DDL.contains("IF NOT EXISTS"));
        assert!(PCR_SNAPSHOTS_LIVE_DDL.contains("TIMESTAMP(ts)"));
        assert!(PCR_SNAPSHOTS_LIVE_DDL.contains("PARTITION BY HOUR WAL"));
        assert!(PCR_SNAPSHOTS_LIVE_DDL.contains("candle_interval SYMBOL"));
        assert!(PCR_SNAPSHOTS_LIVE_DDL.contains("underlying_symbol SYMBOL"));
        assert!(PCR_SNAPSHOTS_LIVE_DDL.contains("pcr_oi DOUBLE"));
        assert!(PCR_SNAPSHOTS_LIVE_DDL.contains("sentiment SYMBOL"));
    }

    #[test]
    fn test_pcr_live_dedup_key_includes_candle_interval() {
        assert!(DEDUP_KEY_PCR_SNAPSHOTS_LIVE.contains("candle_interval"));
        assert!(DEDUP_KEY_PCR_SNAPSHOTS_LIVE.contains("underlying_symbol"));
        assert!(DEDUP_KEY_PCR_SNAPSHOTS_LIVE.contains("expiry_date"));
    }

    // --- ILP builder tests for live tables ---

    #[test]
    fn test_build_option_greeks_live_row() {
        let mut buffer = make_test_buffer();
        let row = OptionGreeksLiveRow {
            segment: "NSE_FNO",
            security_id: 42528,
            symbol_name: "NIFTY25MAR25650CE",
            underlying_security_id: 13,
            underlying_symbol: "NIFTY",
            strike_price: 25650.0,
            option_type: "CE",
            expiry_date: "2025-03-27",
            candle_interval: "1s",
            iv: 0.098,
            delta: 0.54,
            gamma: 0.0013,
            theta: -15.2,
            vega: 12.1,
            rho: 0.05,
            charm: -0.002,
            vanna: 0.15,
            volga: 3.2,
            veta: -1.8,
            speed: -0.00001,
            color: -0.0003,
            zomma: 0.001,
            ultima: -0.5,
            bs_price: 135.5,
            intrinsic_value: 0.0,
            extrinsic_value: 134.0,
            spot_price: 25642.8,
            option_ltp: 134.0,
            oi: 3786445,
            volume: 117567970,
            ts_nanos: sample_ts_nanos(),
        };
        assert!(build_option_greeks_live_row(&mut buffer, &row).is_ok());
        assert!(buffer.len() > 0);
    }

    #[test]
    fn test_build_pcr_snapshot_live_row() {
        let mut buffer = make_test_buffer();
        let row = PcrSnapshotLiveRow {
            underlying_symbol: "NIFTY",
            expiry_date: "2025-03-27",
            candle_interval: "1m",
            pcr_oi: 0.95,
            pcr_volume: 0.82,
            total_put_oi: 5000000,
            total_call_oi: 5263158,
            total_put_volume: 90000000,
            total_call_volume: 109756098,
            sentiment: "Neutral",
            ts_nanos: sample_ts_nanos(),
        };
        assert!(build_pcr_snapshot_live_row(&mut buffer, &row).is_ok());
        assert!(buffer.len() > 0);
    }

    // --- Async HTTP tests for ensure_greeks_tables + execute_ddl ---

    const MOCK_HTTP_200: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
    const MOCK_HTTP_400: &str = "HTTP/1.1 400 Bad Request\r\nContent-Length: 31\r\n\r\n{\"error\":\"table does not exist\"}";

    async fn spawn_mock_http_server(response: &'static str) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                if let Ok((mut stream, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = [0u8; 4096];
                        let _ = stream.read(&mut buf).await;
                        let _ = stream.write_all(response.as_bytes()).await;
                    });
                }
            }
        });
        port
    }

    #[tokio::test]
    async fn test_ensure_greeks_tables_unreachable_no_panic() {
        let config = QuestDbConfig {
            host: "unreachable-host-99999".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        // Must not panic — best-effort DDL.
        ensure_greeks_tables(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_greeks_tables_http_200() {
        let port = spawn_mock_http_server(MOCK_HTTP_200).await;
        tokio::task::yield_now().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Exercises success path through all 4 DDL calls.
        ensure_greeks_tables(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_greeks_tables_http_400() {
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;
        tokio::task::yield_now().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Exercises non-success warn path through all 4 DDL calls.
        ensure_greeks_tables(&config).await;
    }

    #[tokio::test]
    async fn test_ensure_greeks_tables_send_error() {
        // Server that immediately drops connection → Err branch in execute_ddl.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream); // Force connection reset
            }
        });
        tokio::task::yield_now().await;
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: port,
            pg_port: port,
            ilp_port: port,
        };
        // Exercises Err(err) branch in execute_ddl.
        ensure_greeks_tables(&config).await;
    }

    #[tokio::test]
    async fn test_execute_ddl_success() {
        let port = spawn_mock_http_server(MOCK_HTTP_200).await;
        tokio::task::yield_now().await;
        let base_url = format!("http://127.0.0.1:{}/exec", port);
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        execute_ddl(&client, &base_url, "SELECT 1", "test_success").await;
    }

    #[tokio::test]
    async fn test_execute_ddl_non_success() {
        let port = spawn_mock_http_server(MOCK_HTTP_400).await;
        tokio::task::yield_now().await;
        let base_url = format!("http://127.0.0.1:{}/exec", port);
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        execute_ddl(&client, &base_url, "INVALID SQL", "test_non_success").await;
    }

    #[tokio::test]
    async fn test_execute_ddl_send_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        tokio::task::yield_now().await;
        let base_url = format!("http://127.0.0.1:{}/exec", port);
        let client = Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        execute_ddl(&client, &base_url, "SELECT 1", "test_send_error").await;
    }

    // --- ILP row builder tests ---

    fn make_test_buffer() -> Buffer {
        // TCP transport uses V1 protocol by default.
        Buffer::new(questdb::ingress::ProtocolVersion::V1)
    }

    fn sample_ts_nanos() -> i64 {
        1_711_180_800_000_000_000 // 2024-03-23T00:00:00Z in nanos
    }

    #[test]
    fn test_build_dhan_raw_row() {
        let mut buffer = make_test_buffer();
        let row = DhanRawRow {
            security_id: 42528,
            segment: "NSE_FNO",
            symbol_name: "NIFTY25MAR25650CE",
            underlying_symbol: "NIFTY",
            underlying_security_id: 13,
            underlying_segment: "IDX_I",
            strike_price: 25650.0,
            option_type: "CE",
            expiry_date: "2025-03-27",
            spot_price: 25642.8,
            last_price: 134.0,
            average_price: 146.99,
            oi: 3786445,
            previous_close_price: 244.85,
            previous_oi: 402220,
            previous_volume: 31931705,
            volume: 117567970,
            top_bid_price: 133.55,
            top_bid_quantity: 1625,
            top_ask_price: 134.0,
            top_ask_quantity: 1365,
            implied_volatility: 9.789,
            delta: 0.53871,
            theta: -15.1539,
            gamma: 0.00132,
            vega: 12.18593,
            ts_nanos: sample_ts_nanos(),
        };
        assert!(build_dhan_raw_row(&mut buffer, &row).is_ok());
        assert!(buffer.len() > 0);
    }

    #[test]
    fn test_build_option_greeks_row() {
        let mut buffer = make_test_buffer();
        let row = OptionGreeksRow {
            segment: "NSE_FNO",
            security_id: 42528,
            symbol_name: "NIFTY25MAR25650CE",
            underlying_security_id: 13,
            underlying_symbol: "NIFTY",
            strike_price: 25650.0,
            option_type: "CE",
            expiry_date: "2025-03-27",
            iv: 0.098,
            delta: 0.54,
            gamma: 0.0013,
            theta: -15.2,
            vega: 12.1,
            rho: 0.05,
            charm: -0.002,
            vanna: 0.15,
            volga: 3.2,
            veta: -1.8,
            speed: -0.00001,
            color: -0.0003,
            zomma: 0.001,
            ultima: -0.5,
            bs_price: 135.5,
            intrinsic_value: 0.0,
            extrinsic_value: 134.0,
            spot_price: 25642.8,
            option_ltp: 134.0,
            oi: 3786445,
            volume: 117567970,
            buildup_type: "LongBuildup",
            ts_nanos: sample_ts_nanos(),
        };
        assert!(build_option_greeks_row(&mut buffer, &row).is_ok());
        assert!(buffer.len() > 0);
    }

    #[test]
    fn test_build_pcr_snapshot_row() {
        let mut buffer = make_test_buffer();
        let row = PcrSnapshotRow {
            underlying_symbol: "NIFTY",
            expiry_date: "2025-03-27",
            pcr_oi: 0.78,
            pcr_volume: 0.65,
            total_put_oi: 4000000,
            total_call_oi: 5128205,
            total_put_volume: 80000000,
            total_call_volume: 123000000,
            sentiment: "Bullish",
            ts_nanos: sample_ts_nanos(),
        };
        assert!(build_pcr_snapshot_row(&mut buffer, &row).is_ok());
        assert!(buffer.len() > 0);
    }

    #[test]
    fn test_build_verification_row() {
        let mut buffer = make_test_buffer();
        let row = VerificationRow {
            security_id: 42528,
            segment: "NSE_FNO",
            symbol_name: "NIFTY25MAR25650CE",
            underlying_symbol: "NIFTY",
            strike_price: 25650.0,
            option_type: "CE",
            our_iv: 0.098,
            dhan_iv: 0.09789,
            iv_diff: 0.00011,
            our_delta: 0.54,
            dhan_delta: 0.53871,
            delta_diff: 0.00129,
            our_gamma: 0.0013,
            dhan_gamma: 0.00132,
            gamma_diff: -0.00002,
            our_theta: -15.2,
            dhan_theta: -15.1539,
            theta_diff: -0.0461,
            our_vega: 12.1,
            dhan_vega: 12.18593,
            vega_diff: -0.08593,
            match_status: "MATCH",
            ts_nanos: sample_ts_nanos(),
        };
        assert!(build_verification_row(&mut buffer, &row).is_ok());
        assert!(buffer.len() > 0);
    }

    #[test]
    fn test_multiple_rows_accumulate_in_buffer() {
        let mut buffer = make_test_buffer();
        let row = PcrSnapshotRow {
            underlying_symbol: "NIFTY",
            expiry_date: "2025-03-27",
            pcr_oi: 0.78,
            pcr_volume: 0.65,
            total_put_oi: 4000000,
            total_call_oi: 5128205,
            total_put_volume: 80000000,
            total_call_volume: 123000000,
            sentiment: "Bullish",
            ts_nanos: sample_ts_nanos(),
        };
        build_pcr_snapshot_row(&mut buffer, &row).unwrap();
        let len_after_one = buffer.len();

        let row2 = PcrSnapshotRow {
            underlying_symbol: "BANKNIFTY",
            ts_nanos: sample_ts_nanos() + 1_000_000_000,
            ..row
        };
        build_pcr_snapshot_row(&mut buffer, &row2).unwrap();
        assert!(
            buffer.len() > len_after_one,
            "buffer should grow with each row"
        );
    }

    // -----------------------------------------------------------------------
    // GreeksPersistenceWriter resilience tests (Phase C)
    // -----------------------------------------------------------------------

    fn spawn_tcp_drain_server() -> u16 {
        use std::io::Read as _;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 65536];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            }
        });
        port
    }

    #[test]
    fn test_greeks_writer_starts_connected() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let writer = GreeksPersistenceWriter::new(&config).unwrap();
        assert!(
            writer.sender.is_some(),
            "greeks writer must start connected"
        );
        assert!(
            !writer.ilp_conf_string.is_empty(),
            "must store conf string for reconnect"
        );
    }

    #[test]
    fn test_greeks_writer_starts_disconnected_when_unreachable() {
        let config = QuestDbConfig {
            host: "192.0.2.1".to_string(),
            ilp_port: 1,
            http_port: 1,
            pg_port: 1,
        };
        let writer = GreeksPersistenceWriter::new(&config).unwrap();
        assert!(
            writer.sender.is_none(),
            "greeks writer must start disconnected when host unreachable"
        );
    }

    #[test]
    fn test_greeks_writer_flush_ok_when_disconnected() {
        let config = QuestDbConfig {
            host: "192.0.2.1".to_string(),
            ilp_port: 1,
            http_port: 1,
            pg_port: 1,
        };
        let mut writer = GreeksPersistenceWriter::new(&config).unwrap();

        // Prevent reconnect attempts.
        writer.next_reconnect_allowed = std::time::Instant::now() + Duration::from_secs(3600);

        // Flush with nothing pending must return Ok.
        let result = writer.flush();
        assert!(
            result.is_ok(),
            "flush must never fail — resilience guarantee"
        );
    }

    #[test]
    fn test_greeks_writer_reconnect_throttle_constant() {
        assert_eq!(
            GREEKS_RECONNECT_THROTTLE_SECS, 30,
            "greeks writer throttle must be 30s"
        );
    }

    #[test]
    fn test_greeks_writer_reconnect_succeeds_with_valid_host() {
        // Spawn a drain server that accepts multiple connections.
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = GreeksPersistenceWriter::new(&config).unwrap();

        // Simulate disconnect — point to a NEW drain server for reconnect.
        writer.sender = None;
        let reconnect_port = spawn_tcp_drain_server();
        writer.ilp_conf_string = format!("tcp::addr=127.0.0.1:{reconnect_port};");
        writer.next_reconnect_allowed = std::time::Instant::now();

        writer.try_reconnect_on_error();
        assert!(
            writer.sender.is_some(),
            "sender must be Some after successful reconnect"
        );
    }

    #[test]
    fn test_greeks_writer_throttle_skips_when_too_soon() {
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = GreeksPersistenceWriter::new(&config).unwrap();

        // Simulate disconnect + set throttle far in future.
        writer.sender = None;
        writer.next_reconnect_allowed = std::time::Instant::now() + Duration::from_secs(3600);

        writer.try_reconnect_on_error();
        assert!(
            writer.sender.is_none(),
            "reconnect must be skipped when throttled"
        );
    }

    // -----------------------------------------------------------------------
    // GreeksPersistenceWriter resilience: reconnect, flush failure, buffering
    // -----------------------------------------------------------------------

    /// Spawns a TCP server that accepts multiple connections and counts ILP
    /// newlines (each newline = 1 row written).
    fn spawn_counting_tcp_server() -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::Read as _;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let count = Arc::new(AtomicUsize::new(0));
        let count_clone = Arc::clone(&count);

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(mut stream) => {
                        let cnt = Arc::clone(&count_clone);
                        std::thread::spawn(move || {
                            let mut buf = [0u8; 65536];
                            loop {
                                match stream.read(&mut buf) {
                                    Ok(0) | Err(_) => break,
                                    Ok(n) => {
                                        let newlines =
                                            buf[..n].iter().filter(|&&b| b == b'\n').count();
                                        cnt.fetch_add(newlines, Ordering::Relaxed);
                                    }
                                }
                            }
                        });
                    }
                    Err(_) => break,
                }
            }
        });

        (port, count)
    }

    #[test]
    fn test_greeks_reconnect_after_disconnect() {
        // Disconnect, reconnect to a new server, verify flush works.
        use std::sync::atomic::Ordering;

        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = GreeksPersistenceWriter::new(&config).unwrap();
        assert!(writer.sender.is_some());

        // Simulate disconnect.
        writer.sender = None;

        // Point to a new counting server for reconnect.
        let (port2, row_count) = spawn_counting_tcp_server();
        writer.ilp_conf_string = format!("tcp::addr=127.0.0.1:{port2};");
        writer.next_reconnect_allowed = std::time::Instant::now();

        // Trigger reconnect.
        writer.try_reconnect_on_error();
        assert!(
            writer.sender.is_some(),
            "greeks writer must reconnect to new server"
        );

        // Write and flush a row to verify the new connection works.
        let row = PcrSnapshotRow {
            underlying_symbol: "NIFTY",
            expiry_date: "2025-03-27",
            pcr_oi: 0.95,
            pcr_volume: 0.82,
            total_put_oi: 5000000,
            total_call_oi: 5263158,
            total_put_volume: 90000000,
            total_call_volume: 109756098,
            sentiment: "Neutral",
            ts_nanos: sample_ts_nanos(),
        };
        writer.write_pcr_snapshot_row(&row).unwrap();
        assert_eq!(writer.pending_count(), 1);

        let result = writer.flush();
        assert!(result.is_ok(), "flush must succeed after reconnect");
        assert_eq!(writer.pending_count(), 0);

        std::thread::sleep(Duration::from_millis(100));
        let received = row_count.load(Ordering::Relaxed);
        assert!(
            received >= 1,
            "counting server must receive at least 1 ILP row after reconnect flush"
        );
    }

    #[test]
    fn test_greeks_reconnect_throttled() {
        // Attempt reconnect within 30s window, verify throttled (stays None).
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = GreeksPersistenceWriter::new(&config).unwrap();

        // Simulate disconnect.
        writer.sender = None;

        // Set throttle window far in the future.
        writer.next_reconnect_allowed = std::time::Instant::now() + Duration::from_secs(3600);

        // Point to a valid server — but throttle should prevent reconnect.
        let port2 = spawn_tcp_drain_server();
        writer.ilp_conf_string = format!("tcp::addr=127.0.0.1:{port2};");

        writer.try_reconnect_on_error();
        assert!(
            writer.sender.is_none(),
            "reconnect must be throttled — sender must remain None"
        );

        // Verify it stays throttled on a second attempt.
        writer.try_reconnect_on_error();
        assert!(
            writer.sender.is_none(),
            "reconnect must still be throttled on second call"
        );
    }

    #[test]
    fn test_greeks_flush_failure_nulls_sender() {
        // Flush with a broken sender, verify sender becomes None and
        // pending_count is reset.
        let port = spawn_tcp_drain_server();
        let config = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            ilp_port: port,
            http_port: port,
            pg_port: port,
        };
        let mut writer = GreeksPersistenceWriter::new(&config).unwrap();

        // Write a row to have pending data.
        let row = OptionGreeksRow {
            segment: "NSE_FNO",
            security_id: 42528,
            symbol_name: "NIFTY25MAR25650CE",
            underlying_security_id: 13,
            underlying_symbol: "NIFTY",
            strike_price: 25650.0,
            option_type: "CE",
            expiry_date: "2025-03-27",
            iv: 0.098,
            delta: 0.54,
            gamma: 0.0013,
            theta: -15.2,
            vega: 12.1,
            rho: 0.05,
            charm: -0.002,
            vanna: 0.15,
            volga: 3.2,
            veta: -1.8,
            speed: -0.00001,
            color: -0.0003,
            zomma: 0.001,
            ultima: -0.5,
            bs_price: 135.5,
            intrinsic_value: 0.0,
            extrinsic_value: 134.0,
            spot_price: 25642.8,
            option_ltp: 134.0,
            oi: 3786445,
            volume: 117567970,
            buildup_type: "LongBuildup",
            ts_nanos: sample_ts_nanos(),
        };
        writer.write_option_greeks_row(&row).unwrap();
        assert_eq!(writer.pending_count(), 1);

        // Kill sender to simulate broken connection, then replace with
        // unreachable endpoint to prevent reconnect.
        writer.sender = None;
        writer.ilp_conf_string = "tcp::addr=192.0.2.1:1;".to_string();
        writer.next_reconnect_allowed = std::time::Instant::now() + Duration::from_secs(3600);

        // Flush — should gracefully handle missing sender.
        let result = writer.flush();
        assert!(
            result.is_ok(),
            "flush must return Ok even when sender is None — resilience guarantee"
        );
        assert_eq!(
            writer.pending_count(),
            0,
            "pending_count must be reset after discard"
        );
        assert!(
            writer.sender.is_none(),
            "sender must remain None after failed flush"
        );
    }

    #[test]
    fn test_greeks_append_buffers_when_disconnected() {
        // When disconnected, writes should still succeed (buffered in ILP
        // buffer). On flush, they are discarded (Greeks are recomputed).
        let config = QuestDbConfig {
            host: "192.0.2.1".to_string(),
            ilp_port: 1,
            http_port: 1,
            pg_port: 1,
        };
        let mut writer = GreeksPersistenceWriter::new(&config).unwrap();
        assert!(writer.sender.is_none(), "must start disconnected");

        // Block reconnect.
        writer.next_reconnect_allowed = std::time::Instant::now() + Duration::from_secs(3600);

        // Write multiple rows — they go into the ILP buffer.
        let row = DhanRawRow {
            security_id: 42528,
            segment: "NSE_FNO",
            symbol_name: "NIFTY25MAR25650CE",
            underlying_symbol: "NIFTY",
            underlying_security_id: 13,
            underlying_segment: "IDX_I",
            strike_price: 25650.0,
            option_type: "CE",
            expiry_date: "2025-03-27",
            spot_price: 25642.8,
            last_price: 134.0,
            average_price: 146.99,
            oi: 3786445,
            previous_close_price: 244.85,
            previous_oi: 402220,
            previous_volume: 31931705,
            volume: 117567970,
            top_bid_price: 133.55,
            top_bid_quantity: 1625,
            top_ask_price: 134.0,
            top_ask_quantity: 1365,
            implied_volatility: 9.789,
            delta: 0.53871,
            theta: -15.1539,
            gamma: 0.00132,
            vega: 12.18593,
            ts_nanos: sample_ts_nanos(),
        };
        let result = writer.write_dhan_raw_row(&row);
        assert!(
            result.is_ok(),
            "write must succeed even when disconnected — data goes to ILP buffer"
        );
        assert_eq!(writer.pending_count(), 1);

        // Write a second row.
        let row2 = VerificationRow {
            security_id: 42528,
            segment: "NSE_FNO",
            symbol_name: "NIFTY25MAR25650CE",
            underlying_symbol: "NIFTY",
            strike_price: 25650.0,
            option_type: "CE",
            our_iv: 0.098,
            dhan_iv: 0.09789,
            iv_diff: 0.00011,
            our_delta: 0.54,
            dhan_delta: 0.53871,
            delta_diff: 0.00129,
            our_gamma: 0.0013,
            dhan_gamma: 0.00132,
            gamma_diff: -0.00002,
            our_theta: -15.2,
            dhan_theta: -15.1539,
            theta_diff: -0.0461,
            our_vega: 12.1,
            dhan_vega: 12.18593,
            vega_diff: -0.08593,
            match_status: "MATCH",
            ts_nanos: sample_ts_nanos(),
        };
        let result = writer.write_verification_row(&row2);
        assert!(result.is_ok(), "second write must also succeed");
        assert_eq!(writer.pending_count(), 2);

        // Flush — discards data (resilience: Greeks are recomputed).
        let result = writer.flush();
        assert!(
            result.is_ok(),
            "flush must return Ok when disconnected — data discarded"
        );
        assert_eq!(
            writer.pending_count(),
            0,
            "pending_count must be 0 after flush discards data"
        );
    }
}

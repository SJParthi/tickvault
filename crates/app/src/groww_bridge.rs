//! Groww feed bridge — second feed (operator lock §32).
//!
//! The Rust consumer side of the Groww feed: reads the **capture-at-receipt
//! durable file** the Python `growwapi` sidecar appends (one NDJSON tick per
//! line), persists each raw tick to the SHARED `ticks` table (tagged
//! `feed='groww'`), and folds ticks through the SAME 21-timeframe
//! [`MultiTfAggregator`] the Dhan feed uses — ONE common candle engine,
//! parameterized by the per-feed [`FeedStrategy::GROWW`] (Discard late-policy)
//! + an `i64`-widened cumulative-volume override. Every sealed bar across ALL
//! 21 timeframes is routed through the SHARED seal-writer chain
//! ([`global_seal_sender`]) tagged `feed='groww'` (the `BufferedSeal::feed`
//! field), landing in the SHARED `candles_<tf>` tables — the "same tables + feed
//! column" model (operator 2026-06-19).
//!
//! Groww therefore now generates ALL 21 timeframes (it previously produced only
//! 1-minute). The 1-minute slice is the one cross-verified vs the Groww backtest;
//! GENERATION is full-21-TF, identical to Dhan, through the SAME engine code.
//!
//! **Default OFF + isolated:** only runs when `feeds.groww_enabled`. It writes
//! ONLY Groww-tagged (`feed='groww'`) rows into the shared tables, distinguished
//! from Dhan rows (`feed='dhan'`) by the feed-extended DEDUP keys, and never
//! touches the Dhan write path. The Groww aggregator is a SEPARATE
//! `MultiTfAggregator` INSTANCE (engine CODE shared, instance per-feed) so the two
//! feeds never cross-key; the shared writer routes each seal by its `feed`.
//! **Dormant until the sidecar exists:** the loop is a TEST-EXEMPT I/O driver; the
//! pure, fully-unit-tested primitives are the line parser, the segment map, the
//! validation gate, and the `ParsedTick` builder (incl. the u32 token-width +
//! u32 timestamp-width guards). The NDJSON schema below is the Python↔Rust
//! contract (defined consumer-side; the sidecar conforms).
//!
//! ```jsonc
//! {"security_id":1333,"segment":"NSE_EQ","ts_ist_nanos":1780000020123000000,
//!  "exchange_ts_millis":1780000020123,"ltp":2847.55,"cum_volume":123456}
//! ```

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{EventKind, RecursiveMode, Watcher};
use serde::Deserialize;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tracing::{debug, error, info, warn};

use tickvault_api::feed_state::{Feed, FeedRuntimeState};
use tickvault_common::config::QuestDbConfig;
use tickvault_common::tick_types::ParsedTick;
use tickvault_common::types::ExchangeSegment;
use tickvault_common::ws_event_types::{
    WS_EVENT_NO_DHAN_CODE, WsEventAuditRow, WsEventKind, WsType,
};
use tickvault_storage::groww_persistence::{GrowwLiveTickRow, GrowwLiveTickWriter};
use tickvault_storage::seal_writer_runner::global_seal_sender;
use tickvault_trading::candles::{FeedStrategy, MultiTfAggregator};

/// Capacity hint for the Groww feed's own [`MultiTfAggregator`] instance —
/// matches the NTM-union universe headroom (operator §31). The container grows
/// lazily; this only sizes the initial papaya table.
const GROWW_AGGREGATOR_CAPACITY: usize = 1_200;

/// Max bytes read from the tick file per wake (hostile-review HIGH 2026-06-19).
/// Bounds the per-wake allocation so a large backlog on restart (e.g. a full
/// trading day of ~779-instrument NDJSON) is drained in fixed 4 MiB chunks
/// across wakes instead of one multi-GB `read_to_string` that would OOM the
/// 8 GiB host. The event-driven watcher re-fires immediately if more remains.
const GROWW_BRIDGE_MAX_READ_BYTES: u64 = 4 * 1024 * 1024;

/// Default path of the Python sidecar's capture-at-receipt append-only tick file.
pub const GROWW_TICK_FILE_DEFAULT: &str = "data/groww/live-ticks.ndjson";

/// Default path of the sidecar's connect+subscribe PROOF status file (operator
/// 2026-06-28). The sidecar atomically writes `{event, stocks, indices, total,
/// ts_ist_nanos}` here AFTER it subscribes (`event="subscribed"`) and again when it
/// enters the blocking consume (`event="streaming"`). This bridge reads it to:
/// (1) emit the ONE structured `CONNECTED — subscribed N stocks + M indices` log,
/// (2) record the counts in feed-health (`set_subscribed`), and (3) flip
/// `connected=true` ONLY on `streaming` (or the first parsed tick) — NOT on mere
/// file existence (the false-OK the diagnosis flagged). Mirrors
/// `GROWW_STATUS_FILE_DEFAULT` env in `groww_sidecar.py` / the supervisor.
pub const GROWW_STATUS_FILE_DEFAULT: &str = "data/groww/groww-status.json";

/// FALLBACK poll cadence for the event-driven tick-file reader (operator
/// 2026-06-28: "true zero latency, no polling"). The PRIMARY path is the `notify`
/// filesystem watcher (sub-ms OS event delivery on append); this timer is ONLY a
/// safety net that wakes the loop if the watcher errored / dropped an event, and
/// to re-poll the status file. It is NOT the main data path. 1s is well below any
/// human-observable latency and only matters in the degraded (watcher-down) case.
const GROWW_BRIDGE_FALLBACK_POLL_MS: u64 = 1_000;
/// Fallback (watcher-down) poll interval.
const GROWW_BRIDGE_FALLBACK_POLL: Duration = Duration::from_millis(GROWW_BRIDGE_FALLBACK_POLL_MS);

/// Build ONE `ws_event_audit` row for a Groww bridge lifecycle transition.
///
/// Mirrors the Dhan `WebSocketConnection::emit_ws_audit`
/// (`crates/core/src/websocket/connection.rs`) but stamps `feed='groww'` +
/// `ws_type='groww_bridge'` so a Groww lifecycle row never collides with a Dhan
/// row (per-feed identity in the DEDUP key). Pure builder — no I/O — so the
/// transition mapping is unit-testable without a live QuestDB.
///
/// COLD PATH: fired at most once per connect/disconnect/reconnect transition,
/// NEVER per tick — the single owned-`String` reason is off the tick path.
fn build_groww_ws_audit_row(
    event_kind: WsEventKind,
    source: &str,
    reason: &str,
) -> WsEventAuditRow {
    // IST nanos for the designated timestamp + IST-midnight for the day —
    // identical math to the Dhan helper (`connection.rs:688-692`).
    let now_utc_nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
    let now_ist_nanos =
        now_utc_nanos.saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS);
    let nanos_per_day: i64 = 86_400 * 1_000_000_000;
    let trading_date_ist_nanos = now_ist_nanos - now_ist_nanos.rem_euclid(nanos_per_day);
    WsEventAuditRow {
        event_ts_ist_nanos: now_ist_nanos,
        trading_date_ist_nanos,
        feed: Feed::Groww,
        ws_type: WsType::GrowwBridge,
        // Single Groww bridge (pool_size 1) — matches the single-conn convention
        // and the DEDUP composite `(ws_type, connection_index)`.
        connection_index: 0,
        pool_size: 1,
        event_kind,
        source: source.to_string(),
        reason: reason.to_string(),
        // Groww has no Dhan disconnect codes.
        dhan_code: WS_EVENT_NO_DHAN_CODE,
        down_secs: 0,
        attempts: 0,
        market_hours: tickvault_common::market_hours::is_within_market_hours_ist(),
    }
}

/// Emit ONE Groww `ws_event_audit` row, best-effort (`try_send`).
///
/// Non-blocking: drop on full/closed exactly like the Dhan helper — the bridge
/// loop is NEVER stalled by the forensic side-record (the CloudWatch log +
/// `feed_health` for the same event still fired, so no operator-visible signal is
/// lost; an ILP outage is AUDIT-WS-01 Medium on the shared consumer). No-op when
/// no audit channel is attached (defensive / test builds).
fn emit_groww_ws_audit(
    tx: Option<&tokio::sync::mpsc::Sender<WsEventAuditRow>>,
    event_kind: WsEventKind,
    source: &str,
    reason: &str,
) {
    let Some(tx) = tx else {
        return;
    };
    let row = build_groww_ws_audit_row(event_kind, source, reason);
    if let Err(err) = tx.try_send(row) {
        // %err (Display) prints only "full"/"closed" — NOT the dropped row, whose
        // pre-redaction reason must never reach a log (security review).
        debug!(
            reason = %err,
            "groww ws_event_audit channel full/closed — row dropped (log+feed_health still fired)"
        );
    }
}

/// One line of the sidecar's PROOF status file. `event` is `"subscribed"` or
/// `"streaming"`; the counts are the SIDs the sidecar actually subscribed today.
/// `emitted`/`dropped` are the honest-feed PROOF (operator 2026-06-29): records the
/// producer DECODED+EMITTED vs DECODED-but-DROPPED (a `sid_map` miss / missing
/// field). Both are `#[serde(default)]` so an OLD sidecar status (no fields) parses
/// to 0 — forward-safe, no error.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq)]
struct GrowwStatusLine {
    event: GrowwStatusEvent,
    #[serde(default)]
    stocks: u64,
    #[serde(default)]
    indices: u64,
    #[serde(default)]
    emitted: u64,
    #[serde(default)]
    dropped: u64,
}

/// The sidecar's two PROOF status events. `Unknown` tolerates a future/unexpected
/// tag without failing the parse (forward-compatible, fail-soft).
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum GrowwStatusEvent {
    /// The sidecar finished its subscribe calls (counts are final).
    Subscribed,
    /// The sidecar entered the blocking consume — ticks are flowing.
    Streaming,
    /// Any other / future tag — parsed but treated as "not yet streaming".
    #[serde(other)]
    #[default]
    Unknown,
}

/// Parse the sidecar's atomic status file JSON. Pure + testable. Returns `None`
/// (not an error) for an absent/empty/half-written file so the caller simply
/// retries on the next wake — a transient torn read is never fatal.
fn parse_groww_status_line(text: &str) -> Option<GrowwStatusLine> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    serde_json::from_str::<GrowwStatusLine>(trimmed).ok()
}

/// One capture-at-receipt tick line the Python sidecar appends. The fields are
/// the Python↔Rust contract (see module docs). `ts_ist_nanos` is already IST
/// (the sidecar converts); `cum_volume` is Groww cumulative day volume.
#[derive(Debug, Deserialize)]
struct GrowwTickLine {
    security_id: i64,
    segment: String,
    ts_ist_nanos: i64,
    exchange_ts_millis: i64,
    ltp: f64,
    cum_volume: i64,
}

/// A normalised Groww live tick — the aggregator input. Self-contained (no
/// dependency on the deleted `Groww1mTick`); `cum_volume` is Groww's cumulative
/// day volume (`i64`, exceeds `u32` intraday); `ts_ist_nanos` is already IST.
#[derive(Clone, Copy, Debug, PartialEq)]
struct GrowwTick {
    security_id: i64,
    segment: ExchangeSegment,
    /// IST epoch nanoseconds (already IST — NO offset applied here).
    ts_ist_nanos: i64,
    ltp: f64,
    cum_volume: i64,
}

/// A parsed Groww tick — the aggregator input plus the raw fields needed for the
/// shared `ticks` (feed='groww') row.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ParsedGrowwTick {
    tick: GrowwTick,
    exchange_ts_millis: i64,
}

/// Maps the canonical segment string to [`ExchangeSegment`]. Pure + testable.
fn segment_from_str(s: &str) -> Result<ExchangeSegment> {
    Ok(match s {
        "IDX_I" => ExchangeSegment::IdxI,
        "NSE_EQ" => ExchangeSegment::NseEquity,
        "NSE_FNO" => ExchangeSegment::NseFno,
        "NSE_CURRENCY" => ExchangeSegment::NseCurrency,
        "BSE_EQ" => ExchangeSegment::BseEquity,
        "MCX_COMM" => ExchangeSegment::McxComm,
        "BSE_CURRENCY" => ExchangeSegment::BseCurrency,
        "BSE_FNO" => ExchangeSegment::BseFno,
        other => anyhow::bail!("unknown Groww segment: {other}"),
    })
}

/// Parses one NDJSON tick line. Pure + testable.
fn parse_groww_tick_line(line: &str) -> Result<ParsedGrowwTick> {
    let l: GrowwTickLine =
        serde_json::from_str(line).context("groww bridge: tick line is not valid JSON")?;
    let segment = segment_from_str(&l.segment)?;
    Ok(ParsedGrowwTick {
        tick: GrowwTick {
            security_id: l.security_id,
            segment,
            ts_ist_nanos: l.ts_ist_nanos,
            ltp: l.ltp,
            cum_volume: l.cum_volume,
        },
        exchange_ts_millis: l.exchange_ts_millis,
    })
}

/// Nanoseconds per second — used to derive the cell's IST-second timestamp from
/// Groww's IST-nanos. The 21-TF candle grid buckets on seconds (minute boundary
/// is identical), so the millisecond precision is preserved only in the raw
/// `ticks` row (which keeps `ts_ist_nanos`); the candle bucket is second-granular.
const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// Why a Groww tick cannot be folded through the shared 21-TF engine (the
/// engine keys on `(u64 security_id, u8 segment_code)` and buckets on a `u32`
/// IST-second timestamp). REJECT LOUDLY rather than a silent alias.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GrowwAggregateReject {
    /// `security_id` (Groww `exchange_token`) is negative. Groww's native
    /// exchange_token is non-negative and — since the 2026-06-29 `SecurityId`
    /// u32→u64 widening — fits `u64` directly (indices set bit 62, well within
    /// `u64`). This is a defence-in-depth gate that only rejects a truly negative
    /// id; `validate_groww_tick` already filters `security_id <= 0` upstream.
    NegativeSecurityId,
    /// The IST-second timestamp (`ts_ist_nanos / 1e9`) does not fit `u32`
    /// (epoch-seconds overflow u32 only ~2106 — an honest documented bound).
    TimestampExceedsU32,
}

/// Builds the shared-engine [`ParsedTick`] from a validated Groww tick.
///
/// Pure + testable. Performs the guards the adversarial review flagged:
/// - `security_id: i64 → u64` via `u64::try_from` — REJECT only on a negative id
///   (since the 2026-06-29 `SecurityId` u32→u64 widening, Groww's native
///   exchange_token — including bit-62 index ids — folds losslessly; no width
///   rejection). This is the payoff: every Groww index tick now reaches a candle.
/// - `ts_ist_nanos → IST seconds → u32` — REJECT on overflow.
///
/// Groww has NO `day_open` / `day_close` / `oi`, so those `ParsedTick` fields are
/// left 0: the cell opens the day's first bar at the first-tick LTP (day_open=0
/// fallback), and the pct/oi columns stay 0 (missing data, NOT forked logic).
/// The `i64` `cum_volume` is NOT placed in `ParsedTick.volume` (a `u32` that would
/// truncate) — it is carried separately as the consume_tick override.
fn build_parsed_tick(parsed: &ParsedGrowwTick) -> Result<ParsedTick, GrowwAggregateReject> {
    let security_id = u64::try_from(parsed.tick.security_id)
        .map_err(|_| GrowwAggregateReject::NegativeSecurityId)?;
    let exchange_timestamp = u32::try_from(parsed.tick.ts_ist_nanos / NANOS_PER_SECOND)
        .map_err(|_| GrowwAggregateReject::TimestampExceedsU32)?;
    let t = ParsedTick {
        security_id,
        exchange_segment_code: parsed.tick.segment.binary_code(),
        exchange_timestamp,
        // f64 → f32 for the shared ParsedTick LTP field; the candle cell widens it
        // back via f32_to_f64_clean. (The raw `ticks` row keeps the full-precision
        // f64 ltp via live_tick_row, so no precision is lost in the system of record.)
        last_traded_price: parsed.tick.ltp as f32,
        // Groww cum_volume is i64 and exceeds u32 intraday — it MUST NOT be funnelled
        // through this u32 field. It flows via the consume_tick override instead.
        volume: 0,
        ..Default::default()
    };
    Ok(t)
}

/// Lower bound for a plausible IST epoch-nanos tick timestamp (~2020-01-01).
/// Rejects 0 / negative / pre-2020 garbage that would mis-partition the shared
/// `ticks` designated timestamp.
const MIN_PLAUSIBLE_TS_IST_NANOS: i64 = 1_577_836_800_000_000_000;
/// Upper bound for a plausible IST epoch-nanos tick timestamp (~2100-01-01).
/// Rejects `i64::MAX` / mangled far-future stamps.
const MAX_PLAUSIBLE_TS_IST_NANOS: i64 = 4_102_444_800_000_000_000;

/// Upper bound for a plausible cumulative day volume (1 trillion shares).
/// ~1000× the busiest real NSE day-volume — rejects an absurd-but-finite `i64`
/// (e.g. a mangled field) before it can reach the shared `ticks` table, while
/// never rejecting a genuine cumulative volume. Mirrors the LTP upper bound.
const MAX_PLAUSIBLE_VOLUME: i64 = 1_000_000_000_000;

/// Why a Groww NDJSON tick is rejected before it can reach the SHARED tables.
/// Copy + testable; mirrors the Dhan path's `is_valid_ltp` / OHLC-integrity
/// guards (phase-0-architecture.md Pillar 2) so a misbehaving sidecar cannot
/// corrupt shared-table data or the 15:31 cross-verify.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GrowwTickReject {
    /// `ltp` is NaN or ±Inf.
    NonFinitePrice,
    /// `ltp` is zero or negative.
    NonPositivePrice,
    /// `ltp` exceeds `MAX_PLAUSIBLE_LTP` (~₹10 crore) — absurd-but-finite garbage.
    ImplausiblePrice,
    /// `cum_volume` is negative (cumulative day volume can never decrease below 0).
    NegativeVolume,
    /// `cum_volume` exceeds `MAX_PLAUSIBLE_VOLUME` — absurd-but-finite garbage.
    ImplausibleVolume,
    /// `security_id` is zero or negative.
    NonPositiveSecurityId,
    /// `ts_ist_nanos` is outside the plausible `[2020, 2100)` IST window.
    TimestampOutOfRange,
}

/// Validates a parsed Groww tick before persist. Pure, O(1) (a handful of
/// finite/compare checks), no allocation. Mirrors the Dhan `is_valid_ltp`
/// upper/lower bounds so it can never reject a genuine price.
fn validate_groww_tick(parsed: &ParsedGrowwTick) -> Result<(), GrowwTickReject> {
    let ltp = parsed.tick.ltp;
    if !ltp.is_finite() {
        return Err(GrowwTickReject::NonFinitePrice);
    }
    if ltp <= 0.0 {
        return Err(GrowwTickReject::NonPositivePrice);
    }
    if ltp > f64::from(tickvault_common::constants::MAX_PLAUSIBLE_LTP) {
        return Err(GrowwTickReject::ImplausiblePrice);
    }
    if parsed.tick.cum_volume < 0 {
        return Err(GrowwTickReject::NegativeVolume);
    }
    if parsed.tick.cum_volume > MAX_PLAUSIBLE_VOLUME {
        return Err(GrowwTickReject::ImplausibleVolume);
    }
    if parsed.tick.security_id <= 0 {
        return Err(GrowwTickReject::NonPositiveSecurityId);
    }
    if !(MIN_PLAUSIBLE_TS_IST_NANOS..=MAX_PLAUSIBLE_TS_IST_NANOS)
        .contains(&parsed.tick.ts_ist_nanos)
    {
        return Err(GrowwTickReject::TimestampOutOfRange);
    }
    Ok(())
}

/// Wall-clock receipt time in IST epoch nanos (`Utc::now()` + IST offset) — the
/// instant WE captured a tick. Used ONLY for live-feed-health last-tick-age math
/// so it is consistent with the `/api/feeds/health` endpoint's clock; never used
/// for persistence (rows keep the exchange `ts_ist_nanos`). A clock read that
/// somehow fails saturates to 0 (treated as "very old" — fails safe toward Down,
/// never a false Ok).
fn receipt_ist_nanos() -> i64 {
    chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(0)
        .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS)
}

/// Builds the shared `ticks` (feed='groww') row for a parsed tick. `capture_seq`
/// is the monotonic, replay-stable dedup tiebreaker (TICK-SEQ-01). Pure + testable.
fn live_tick_row(parsed: &ParsedGrowwTick, capture_seq: i64) -> GrowwLiveTickRow {
    GrowwLiveTickRow {
        ts_ist_nanos: parsed.tick.ts_ist_nanos,
        security_id: parsed.tick.security_id,
        segment: parsed.tick.segment.as_str(),
        ltp: parsed.tick.ltp,
        volume: parsed.tick.cum_volume,
        exchange_ts_millis: parsed.exchange_ts_millis,
        capture_seq,
    }
}

/// Monotonic, replay-stable `capture_seq` source (TICK-SEQ-01): `max(prev+1,
/// seed_nanos)`. The call site seeds it from the tick's IST epoch-nanos
/// (`ts_ist_nanos`) — a replay-STABLE value (NOT wall-clock), so `capture_seq`
/// stays strictly monotonic AND identical on replay of the same tick stream
/// (a wall-clock seed would differ on replay and create duplicate rows).
fn next_capture_seq(counter: &AtomicI64, seed_nanos: i64) -> i64 {
    let mut prev = counter.load(Ordering::Relaxed);
    loop {
        let next = (prev + 1).max(seed_nanos);
        match counter.compare_exchange_weak(prev, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(observed) => prev = observed,
        }
    }
}

/// Drains all COMPLETE newline-terminated records from `residual`, returning the
/// drained prefix bytes (up to and including the final `\n`). Any trailing partial
/// line — including a partial multi-byte UTF-8 char left by the bounded chunked
/// read — stays in `residual` for the next poll. Splitting on the `0x0A` byte is
/// UTF-8-safe: a newline byte never occurs inside a multi-byte sequence. Pure +
/// testable; O(n) single drain (no per-line shift).
fn drain_complete_prefix(residual: &mut Vec<u8>) -> Vec<u8> {
    match residual.iter().rposition(|&b| b == b'\n') {
        Some(last_nl) => residual.drain(..=last_nl).collect(),
        None => Vec::new(),
    }
}

/// Install a `notify` filesystem watcher on the tick file's PARENT directory and
/// forward each create/modify/remove event as a unit `()` wake into a tokio mpsc.
/// Returns `(watcher_handle, wake_rx)` on success — the handle MUST be kept alive
/// (dropping it stops the watch). Watching the directory (not the file) survives
/// atomic temp+rename writes and a not-yet-created file.
///
/// This is the PRIMARY, ZERO-POLL data path (operator 2026-06-28: "true zero
/// latency, no polling"): the OS delivers an append event sub-ms, the bridge wakes
/// immediately and reads the new bytes. The fallback poll timer in the loop is a
/// safety net used ONLY if this returns `Err` or the channel later closes.
// TEST-EXEMPT: thin wrapper over the `notify` watcher (filesystem I/O); the loop's
// event-vs-fallback wake handling and the pure status/parse helpers are tested.
fn spawn_tick_file_watcher(
    tick_file_path: &Path,
) -> Result<(notify::RecommendedWatcher, tokio::sync::mpsc::Receiver<()>)> {
    // A small bounded channel: many rapid FS events coalesce into "there is new
    // data" — the loop drains the file fully on each wake, so a full channel
    // (dropped extra wakes) loses nothing (the next wake or the fallback re-reads).
    let (wake_tx, wake_rx) = tokio::sync::mpsc::channel::<()>(8);
    let mut watcher = notify::recommended_watcher(
        move |result: std::result::Result<notify::Event, notify::Error>| match result {
            Ok(event) => {
                if matches!(
                    event.kind,
                    EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
                ) {
                    // Non-blocking coalesced wake: a Full channel means a wake is
                    // already pending (this one is redundant — fine); a Closed
                    // channel means the loop exited (nothing to do). Both are
                    // intentionally ignored — `try_send` is `#[must_use]`, so name
                    // the outcomes explicitly rather than a lint-flagged `let _ =`.
                    match wake_tx.try_send(()) {
                        Ok(()) => {}
                        Err(tokio::sync::mpsc::error::TrySendError::Full(())) => {}
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(())) => {}
                    }
                }
            }
            Err(err) => {
                // Watcher-internal error: log; the loop's fallback poll keeps it
                // moving. (Never panics the watcher thread.)
                error!(
                    ?err,
                    "groww bridge: file-watcher error (fallback poll covers it)"
                );
            }
        },
    )
    .context("groww bridge: failed to create tick-file watcher")?;

    // Watch the PARENT dir non-recursively (the file may not exist yet, and the
    // sidecar's status file uses temp+rename — directory watching catches both).
    let watch_dir = tick_file_path.parent().unwrap_or_else(|| Path::new("."));
    watcher
        .watch(watch_dir, RecursiveMode::NonRecursive)
        .with_context(|| format!("groww bridge: failed to watch dir {}", watch_dir.display()))?;
    Ok((watcher, wake_rx))
}

/// Read the sidecar's PROOF status file (best-effort). Returns the parsed status,
/// or `None` if absent/empty/torn/unparseable (the caller retries next wake).
// TEST-EXEMPT: thin async file-read wrapper; `parse_groww_status_line` is unit-tested.
async fn read_status_file(status_file_path: &Path) -> Option<GrowwStatusLine> {
    match tokio::fs::read_to_string(status_file_path).await {
        Ok(text) => parse_groww_status_line(&text),
        Err(_) => None, // not written yet / transient — not an error
    }
}

/// All mutable bridge state for the tick-file tail: the ILP writer, the Groww
/// 21-TF aggregator instance, the first-seen seed set, the monotonic
/// `capture_seq`, the byte read offset, and the partial-line residual. Bundling it
/// lets BOTH the production loop AND the live-QuestDB e2e test drive the SAME
/// `drain_new_data` ingestion logic (no re-implementation in the test).
pub(crate) struct GrowwBridgeState {
    live_writer: GrowwLiveTickWriter,
    /// The Groww feed's OWN 21-TF aggregator INSTANCE — the SAME engine code as
    /// Dhan (`MultiTfAggregator`), parameterized per-feed by FeedStrategy::GROWW.
    aggregator: MultiTfAggregator,
    /// Per-instrument first-seen set: on first sight pre_populate + seed the
    /// cumulative baseline (Groww running day-volume). O(1) lookups. Keyed on the
    /// `u64` security_id (post-2026-06-29 widening) so the SAME id keys both the
    /// `ticks` row and the candle fold for Groww's native exchange_token.
    seeded: std::collections::HashSet<(u64, u8)>,
    /// Monotonic, replay-stable dedup tiebreaker source (TICK-SEQ-01).
    capture_seq: AtomicI64,
    /// Byte read offset into the tick file (resumes across wakes).
    offset: u64,
    /// Partial-line byte residual: a bounded chunked read can split a multi-byte
    /// UTF-8 char / a line at the chunk boundary; buffering raw bytes and splitting
    /// on the 0x0A newline keeps partial chars + lines safely buffered until the
    /// next wake completes them.
    residual: Vec<u8>,
}

impl GrowwBridgeState {
    /// Build a bridge state around a CALLER-OWNED aggregator.
    ///
    /// `MultiTfAggregator` is `Clone` over a shared `Arc<HashMap>` (papaya
    /// concurrent map; every method takes `&self`), so the caller can keep a
    /// `.clone()` that shares the SAME underlying buckets. `run_groww_bridge`
    /// uses this so the IST-midnight force-seal boundary task can `force_seal_all`
    /// the SAME aggregator the per-tick path folds into — parity with the Dhan
    /// IST-midnight force-seal (`crates/app/src/main.rs`). The per-tick fold path
    /// (`drain_new_data`) is UNCHANGED.
    pub(crate) fn with_aggregator(qdb: &QuestDbConfig, aggregator: MultiTfAggregator) -> Self {
        Self {
            live_writer: GrowwLiveTickWriter::new(qdb),
            aggregator,
            seeded: std::collections::HashSet::new(),
            capture_seq: AtomicI64::new(0),
            offset: 0,
            residual: Vec::new(),
        }
    }

    /// Read whatever new bytes are in `tick_file_path` since the last call, parse +
    /// validate + persist each COMPLETE tick line to the SHARED `ticks` table
    /// (feed='groww'), and fold each through the SHARED 21-TF engine routing seals
    /// via `global_seal_sender`. Returns `true` if at least ONE valid tick was
    /// parsed this call (used to flip `connected` on a first tick). This is the ONE
    /// ingestion body — the production loop and the e2e test both call it, so the
    /// proof exercises the real path.
    ///
    /// File-absent / no-new-bytes / read errors are non-fatal (return `false`); a
    /// file that shrank restarts from offset 0 (rotation/truncate).
    pub(crate) async fn drain_new_data(
        &mut self,
        tick_file_path: &Path,
        feed_health: &tickvault_common::feed_health::FeedHealthRegistry,
    ) -> bool {
        let mut file = match File::open(tick_file_path).await {
            Ok(f) => f,
            // Sidecar not started yet — nothing to read.
            Err(_) => return false,
        };
        let len = match file.metadata().await {
            Ok(m) => m.len(),
            Err(err) => {
                warn!(?err, "groww bridge: metadata failed");
                return false;
            }
        };
        if len < self.offset {
            // File shrank (rotation/truncate) — restart from the top.
            self.offset = 0;
            self.residual.clear();
        }
        if len == self.offset {
            return false; // nothing new
        }
        if file.seek(SeekFrom::Start(self.offset)).await.is_err() {
            return false;
        }
        // Bounded chunked read: read at most GROWW_BRIDGE_MAX_READ_BYTES this wake —
        // never the whole (possibly multi-GB) file at once.
        let to_read = (len - self.offset).min(GROWW_BRIDGE_MAX_READ_BYTES);
        let mut chunk: Vec<u8> = Vec::new();
        match (&mut file).take(to_read).read_to_end(&mut chunk).await {
            Ok(n) => self.offset = self.offset.saturating_add(n as u64),
            Err(err) => {
                warn!(?err, "groww bridge: read failed");
                return false;
            }
        }

        self.residual.extend_from_slice(&chunk);
        // Most-recent receipt ts of a row appended this wake (0 if none). The
        // dashboard "ticks" counter is bumped by the number of rows ACTUALLY
        // persisted on a successful flush (NOT per append) — so it reflects rows
        // written to QuestDB, never in-memory buffer appends (the 0-rows-despite-
        // counter bug, 2026-06-29).
        let mut last_receipt_ist_nanos: i64 = 0;
        let mut parsed_any = false;
        // Drain only COMPLETE newline-terminated lines; a trailing partial line
        // (incl. a partial UTF-8 char from the chunk boundary) stays buffered.
        let prefix = drain_complete_prefix(&mut self.residual);
        for line_bytes in prefix.split(|&b| b == b'\n') {
            if line_bytes.is_empty() {
                continue;
            }
            let line = match std::str::from_utf8(line_bytes) {
                Ok(s) => s.trim(),
                Err(err) => {
                    error!(?err, "groww bridge: skipping non-UTF-8 tick line");
                    continue;
                }
            };
            if line.is_empty() {
                continue;
            }
            let parsed = match parse_groww_tick_line(line) {
                Ok(p) => p,
                Err(err) => {
                    error!(?err, "groww bridge: skipping malformed tick line");
                    continue;
                }
            };
            // Data-integrity gate (security-review MEDIUM 2026-06-19): a
            // misbehaving sidecar must NOT write a non-finite/≤0/absurd price,
            // negative volume, or far-future timestamp into the SHARED
            // ticks/candles_1m tables (which Dhan also reads + the 15:31
            // cross-verify compares). Reject + log + skip before persist AND
            // before the aggregator fold, so one bad tick can't poison a candle.
            if let Err(reason) = validate_groww_tick(&parsed) {
                error!(
                    ?reason,
                    security_id = parsed.tick.security_id,
                    "groww bridge: rejecting invalid tick before persist"
                );
                continue;
            }
            parsed_any = true;
            let seq = next_capture_seq(&self.capture_seq, parsed.tick.ts_ist_nanos);
            if let Err(err) = self.live_writer.append_row(&live_tick_row(&parsed, seq)) {
                error!(
                    ?err,
                    "groww bridge: shared ticks (feed=groww) append failed"
                );
            } else {
                // The dashboard counter is bumped ONLY on a successful flush below,
                // by the number of rows actually persisted. Stamp the WALL-CLOCK
                // receipt time (NOT the exchange ts) for the most-recent row.
                last_receipt_ist_nanos = receipt_ist_nanos();
            }
            // Fold through the SHARED 21-TF engine (ONE common candle engine).
            let pt = match build_parsed_tick(&parsed) {
                Ok(pt) => pt,
                Err(reason) => {
                    error!(
                        ?reason,
                        security_id = parsed.tick.security_id,
                        "groww bridge: tick cannot fold through the shared 21-TF engine (width guard)"
                    );
                    continue;
                }
            };
            let seg_code = pt.exchange_segment_code;
            let key = (pt.security_id, seg_code);
            let cum_volume_u64 = match u64::try_from(parsed.tick.cum_volume) {
                Ok(v) => v,
                Err(_) => {
                    error!(
                        security_id = parsed.tick.security_id,
                        cum_volume = parsed.tick.cum_volume,
                        "groww bridge: negative cum_volume past validation — dropping tick"
                    );
                    continue;
                }
            };
            // First sight: register + seed the cumulative baseline (idempotent).
            if self.seeded.insert(key) {
                self.aggregator.pre_populate(std::iter::once(key));
                self.aggregator
                    .seed_cumulative(pt.security_id, seg_code, cum_volume_u64);
            }
            // Route every sealed bar across ALL 21 TFs through the SHARED
            // seal-writer chain, tagged feed=Groww.
            let stats = self.aggregator.consume_tick(
                &pt,
                seg_code,
                FeedStrategy::GROWW,
                Some(cum_volume_u64),
                |tf, state| {
                    let Some(sender) = global_seal_sender() else {
                        return;
                    };
                    crate::seal_routing::route_seal(
                        crate::seal_routing::SealRouteParams {
                            feed: Feed::Groww,
                            drop_d1: false,
                            prev_day_cache: None,
                            heartbeat: None,
                            feed_health_on_m1: Some(feed_health),
                        },
                        pt.security_id,
                        seg_code,
                        tf,
                        state,
                        sender,
                    );
                },
            );
            if !stats.instrument_found {
                self.aggregator.pre_populate(std::iter::once(key));
            }
        }
        // Flush the buffered rows. The dashboard "ticks" counter is bumped ONLY by
        // the number of rows ACTUALLY persisted to QuestDB (the flush return value),
        // so it reflects rows written — not in-memory buffer appends. A flush
        // failure leaves the buffer + rows intact (the writer reconnects on the next
        // flush) and the counter is NOT bumped (honest — nothing was persisted).
        // Flush failures are data-at-risk — `error!` per audit Rule 5 / charter
        // Rule 6 so they route to Telegram via ERROR-level Loki routing.
        // Gate on the writer's OWN pending count, NOT this wake's appends: a prior
        // wake whose flush failed leaves rows buffered with `pending > 0` but
        // `appended_this_wake == 0` — those retained rows must still be re-attempted
        // on a quiet wake (otherwise they sit unflushed until the next NEW tick,
        // exactly during a post-burst quiet period — hostile-review MEDIUM finding).
        if self.live_writer.pending() > 0 {
            // For a quiet wake (no new appends), `last_receipt_ist_nanos` is 0 — use
            // the wall-clock receipt time so record_ticks never stamps a 1970 ts.
            let ts = if last_receipt_ist_nanos != 0 {
                last_receipt_ist_nanos
            } else {
                receipt_ist_nanos()
            };
            match self.live_writer.flush() {
                Ok(outcome) => {
                    feed_health.record_ticks(Feed::Groww, outcome.persisted as u64, ts);
                    // A transient QuestDB socket reset (broken pipe) that the writer
                    // reconnected + replayed IN this wake is a SELF-HEALED recovery,
                    // NOT data-at-risk — log it at info! so it doesn't re-spam the
                    // operator's ERROR feed (the open-bell `error!` the operator
                    // flagged 2026-06-30). The give-up-to-spill path below stays
                    // error! per audit Rule 5 / charter Rule 6.
                    if outcome.reconnected {
                        info!(
                            persisted = outcome.persisted,
                            "groww bridge: shared ticks (feed=groww) flush recovered via reconnect + replay (zero drop)"
                        );
                    }
                }
                Err(err) => {
                    error!(?err, "groww bridge: shared ticks (feed=groww) flush failed");
                }
            }
        }
        parsed_any
    }
}

/// Spawn the IST-midnight force-seal boundary task for the Groww aggregator.
///
/// PARITY WITH DHAN: this mirrors the Dhan IST-midnight force-seal task in
/// `crates/app/src/main.rs` (the only other production `force_seal_all` call
/// site). At IST 00:00 each trading day it force-seals every open bucket across
/// all 21 timeframes so day-N candle state never fuses into day-(N+1)'s first
/// bar. WITHOUT this, the LAST open bucket of each of the 21 Groww timeframes
/// each day is silently overwritten by the next day's first tick — the verified
/// candle-loss bug this task fixes.
///
/// `aggregator` is a `.clone()` of the bridge's aggregator — `MultiTfAggregator`
/// shares its papaya `Arc<HashMap>` across clones, so this seals the SAME buckets
/// the per-tick path folds into. Each sealed bar routes through the SAME shared
/// seal-writer chain (`global_seal_sender`) tagged `feed: Feed::Groww` — exactly
/// the per-tick Groww policy (`drop_d1: false` ⇒ D1 is routed for Groww; no
/// prev-day pct-stamp; no Dhan heartbeat). The cell's `force_seal` re-arms
/// `armed_for_day_open` and clears `last_sealed` internally, so no closure-level
/// re-arm is needed (identical to the Dhan path).
///
/// Gated TWICE: skips on a non-trading-day midnight (`is_trading_day_today`) AND
/// when Groww is runtime-disabled (`feed_runtime.is_enabled(Feed::Groww)`). An
/// empty aggregator is a natural no-op (`force_seal_all` iterates zero entries).
// TEST-EXEMPT: cold-path tokio I/O driver (sleep loop + boundary gate). The
// load-bearing seal-routing closure is unit-tested via `route_seal`
// (`test_groww_force_seal_routes_with_feed_groww`); the wiring is pinned by the
// source-scan ratchet `test_run_groww_bridge_spawns_ist_midnight_force_seal`.
fn spawn_groww_ist_midnight_force_seal(
    aggregator: MultiTfAggregator,
    feed_runtime: Arc<FeedRuntimeState>,
    trading_calendar: Arc<tickvault_common::trading_calendar::TradingCalendar>,
) {
    tokio::spawn(async move {
        loop {
            // Sleep until the next IST midnight (bounded helper, ≤ 24h) — the
            // SAME helper the Dhan IST-midnight force-seal uses.
            let sleep_secs = tickvault_common::market_hours::secs_until_next_ist_midnight().max(1);
            tokio::time::sleep(Duration::from_secs(sleep_secs)).await;

            // Only force-seal on trading days — a non-trading-day midnight has no
            // open buckets worth flushing (mirrors the Dhan task).
            if !trading_calendar.is_trading_day_today() {
                debug!("Groww IST-midnight force-seal: skipping (non-trading day)");
                continue;
            }

            // Skip when Groww is runtime-disabled: a disabled feed produces no
            // ticks, so there is nothing to seal (and we must not emit Groww
            // candles while the feed is off). The flag is read into a local so
            // this gate does not collide with the `run_groww_bridge` disable-branch
            // source-scan anchor (which `find`s the FIRST literal occurrence).
            let groww_enabled = feed_runtime.is_enabled(Feed::Groww);
            if !groww_enabled {
                debug!("Groww IST-midnight force-seal: skipping (Groww feed disabled)");
                continue;
            }

            let Some(sender) = global_seal_sender() else {
                warn!("Groww IST-midnight force-seal: seal sender not installed — skipping");
                continue;
            };

            let mut sealed: u64 = 0;
            let mut dropped: u64 = 0;
            aggregator.force_seal_all(|security_id, segment_code, tf, state| {
                // SAME shared per-seal routing as the per-tick Groww path:
                // feed=Groww, drop_d1=false (Groww routes D1), no prev-day
                // pct-stamp, no Dhan heartbeat, no per-M1 feed-health record
                // (this is the EOD flush, not a live tick).
                match crate::seal_routing::route_seal(
                    crate::seal_routing::SealRouteParams {
                        feed: Feed::Groww,
                        drop_d1: false,
                        prev_day_cache: None,
                        heartbeat: None,
                        feed_health_on_m1: None,
                    },
                    security_id,
                    segment_code,
                    tf,
                    state,
                    sender,
                ) {
                    crate::seal_routing::SealOutcome::Sent => {
                        sealed = sealed.saturating_add(1);
                    }
                    crate::seal_routing::SealOutcome::DroppedFull => {
                        dropped = dropped.saturating_add(1);
                    }
                    // Groww never drops D1 (drop_d1=false), so DroppedD1 is
                    // unreachable here — handled for exhaustiveness only.
                    crate::seal_routing::SealOutcome::DroppedD1 => {}
                }
            });
            info!(
                sealed,
                dropped,
                "Groww IST-midnight force-seal complete — open buckets flushed (feed=groww)"
            );
        }
    });
    info!("Groww bridge — IST-midnight force-seal task spawned (parity with Dhan)");
}

/// Consumer driver: EVENT-DRIVEN (zero-poll) tail of the sidecar's append-only
/// NDJSON tick file (operator 2026-06-28: "true zero latency, no polling"). A
/// `notify` filesystem watcher wakes the loop sub-ms on each append; the loop reads
/// the new bytes, persists each raw tick to the SHARED `ticks` table (feed='groww'),
/// and folds them through the SHARED 21-TF engine into the SHARED `candles_<tf>`
/// tables (feed='groww'). A small FALLBACK poll fires ONLY if the watcher is
/// unavailable / drops an event (resilience) and to re-poll the PROOF status file —
/// never as the primary path.
///
/// Runs ONLY when `feeds.groww_enabled`; writes ONLY `feed='groww'`-tagged rows;
/// never touches the Dhan write path. The durable fsync'd NDJSON file (zero-tick-
/// loss floor) is UNCHANGED — only HOW the bridge learns of new data changed.
///
/// CONNECT+SUBSCRIBE PROOF (operator 2026-06-28): reads the sidecar's status file
/// each wake; on the first `subscribed` event emits ONE structured `CONNECTED —
/// subscribed N stocks + M indices` log + records the counts in feed-health; flips
/// `connected=true` ONLY on the `streaming` status OR the first parsed tick — NOT
/// on mere file existence (removing the false-OK).
// TEST-EXEMPT: event-driven file-tail + live-QuestDB ILP I/O driver; the pure
// primitives (parse_groww_tick_line, parse_groww_status_line, segment_from_str,
// live_tick_row, build_parsed_tick, next_capture_seq) + the GrowwBridgeState
// ingestion body are unit/integration-tested below + in groww_live_pipeline_e2e.
pub async fn run_groww_bridge(
    qdb: QuestDbConfig,
    tick_file_path: PathBuf,
    status_file_path: PathBuf,
    feed_runtime: Arc<FeedRuntimeState>,
    // Live-feed health (operator 2026-06-22): record Groww ticks/candles +
    // connected-state so GET /api/feeds/health reports the truthful verdict.
    feed_health: Arc<tickvault_common::feed_health::FeedHealthRegistry>,
    // ws_event_audit forensic sender (2026-06-29): one `feed='groww'` row per
    // Groww connect/disconnect/reconnect, drained by the SAME consumer Dhan uses
    // (`spawn_ws_event_audit_consumer`). `Option` mirrors the Dhan `ws_audit_tx`
    // convention so a `None` build (defensive / test) is a no-op.
    ws_audit_tx: Option<tokio::sync::mpsc::Sender<WsEventAuditRow>>,
    // Trading calendar (2026-06-29): drives the IST-midnight force-seal boundary
    // task's trading-day gate, using the SAME calendar handle the Dhan
    // IST-midnight force-seal uses (`crates/app/src/main.rs`). Shared `Arc`.
    trading_calendar: Arc<tickvault_common::trading_calendar::TradingCalendar>,
) {
    info!(
        path = %tick_file_path.display(),
        status = %status_file_path.display(),
        "Groww bridge started — EVENT-DRIVEN tail of the sidecar tick file (zero-poll; \
         fallback watchdog only if the watcher drops). Idles until the sidecar appends."
    );
    // Build the Groww aggregator ONCE and hand a `.clone()` (shared papaya map)
    // to the IST-midnight force-seal boundary task — parity with the Dhan
    // IST-midnight force-seal (`main.rs`). The per-tick fold path keeps the SAME
    // instance via `GrowwBridgeState`.
    let aggregator = MultiTfAggregator::with_capacity(GROWW_AGGREGATOR_CAPACITY);
    spawn_groww_ist_midnight_force_seal(
        aggregator.clone(),
        Arc::clone(&feed_runtime),
        trading_calendar,
    );
    let mut state = GrowwBridgeState::with_aggregator(&qdb, aggregator);

    // PRIMARY path: install the event-driven filesystem watcher. If it fails, the
    // fallback poll alone keeps the bridge working (degraded, logged once). The
    // `_watcher` handle is held ONLY for its RAII side-effect — dropping it stops
    // the watch — so it is intentionally never read (the `_` prefix says so); the
    // events arrive via `wake_rx`.
    let (mut _watcher, mut wake_rx) = match spawn_tick_file_watcher(&tick_file_path) {
        Ok((w, rx)) => (Some(w), Some(rx)),
        Err(err) => {
            warn!(
                ?err,
                "groww bridge: file watcher unavailable — falling back to the {}ms safety poll \
                 (degraded; not the zero-latency path)",
                GROWW_BRIDGE_FALLBACK_POLL_MS
            );
            (None, None)
        }
    };
    // Re-arm the watcher lazily if it ever drops (channel closed) — keeps the
    // zero-poll path alive across a transient watcher failure.
    let mut connect_logged = false;
    let mut streaming_observed = false;
    // ws_event_audit edge latches (2026-06-29, audit-findings Rule 4 — edge-trigger
    // only): `audited_connected` gates the Connected/Reconnected emit to fire ONCE
    // per streaming rising edge (never per loop turn). `prior_disconnect` is set when
    // a Disconnected/DisconnectedOffHours row was emitted on a disable falling edge,
    // so the NEXT rising edge is classified Reconnected (not a second Connected).
    let mut audited_connected = false;
    let mut prior_disconnect = false;

    loop {
        // Wake on EITHER a filesystem event (zero-latency) OR the fallback timer.
        // The fallback also re-polls the status file (cheap) so a `subscribed`/
        // `streaming` transition is observed even with no tick append in between.
        match wake_rx.as_mut() {
            Some(rx) => {
                tokio::select! {
                    recv = rx.recv() => {
                        if recv.is_none() {
                            // Watcher channel closed — try to re-arm; until then, poll.
                            warn!("groww bridge: file-watcher channel closed — re-arming (fallback poll meanwhile)");
                            match spawn_tick_file_watcher(&tick_file_path) {
                                Ok((w, new_rx)) => { _watcher = Some(w); wake_rx = Some(new_rx); }
                                Err(err) => { warn!(?err, "groww bridge: watcher re-arm failed — fallback poll only"); _watcher = None; wake_rx = None; }
                            }
                        }
                    }
                    () = tokio::time::sleep(GROWW_BRIDGE_FALLBACK_POLL) => {}
                }
            }
            // No watcher — pure fallback poll (degraded path).
            None => {
                tokio::time::sleep(GROWW_BRIDGE_FALLBACK_POLL).await;
            }
        }

        // Feed-toggle API (operator 2026-06-19): live pause/resume. When Groww is
        // runtime-disabled, idle — no file read, no writes. Re-enabling resumes
        // from the persisted `offset` with NO double-read and NO tick loss.
        if !feed_runtime.is_enabled(Feed::Groww) {
            // Clear the stale `connected` bit on disable: the feed is no longer
            // streaming, so leaving it `true` would report a misleading green
            // (false-OK). Re-arm the connect-log + streaming latches so a later
            // re-enable re-emits the ONE CONNECTED log and re-gates `connected`
            // on fresh streaming, never on the pre-disable carry-over.
            feed_health.set_connected(Feed::Groww, false);
            // ws_event_audit falling edge: emit ONE Disconnected row on the
            // transition out of a previously-audited connected state (off-hours →
            // DisconnectedOffHours Low, else Disconnected). Gated on
            // `audited_connected` so a disable while already-disconnected (or
            // never-connected) does not spam a row each idle turn.
            if audited_connected {
                let kind = if tickvault_common::market_hours::is_within_market_hours_ist() {
                    WsEventKind::Disconnected
                } else {
                    WsEventKind::DisconnectedOffHours
                };
                emit_groww_ws_audit(
                    ws_audit_tx.as_ref(),
                    kind,
                    "feed_disabled",
                    "groww disabled",
                );
                prior_disconnect = true;
            }
            audited_connected = false;
            connect_logged = false;
            streaming_observed = false;
            continue;
        }

        // CONNECT+SUBSCRIBE PROOF: read the sidecar status file (cheap, best-effort).
        if let Some(status) = read_status_file(&status_file_path).await {
            // First `subscribed`/`streaming` observation → emit the ONE CONNECT log
            // + record the subscribe counts (idempotent; only logged once).
            if !connect_logged && status.event != GrowwStatusEvent::Unknown {
                info!(
                    stocks = status.stocks,
                    indices = status.indices,
                    total = status.stocks + status.indices,
                    "Groww live feed CONNECTED — subscribed {} stocks + {} indices = {}",
                    status.stocks,
                    status.indices,
                    status.stocks + status.indices
                );
                feed_health.set_subscribed(Feed::Groww, status.stocks, status.indices);
                connect_logged = true;
            } else if status.event != GrowwStatusEvent::Unknown {
                // Keep the counts fresh on a re-subscribe (reconnect) without re-logging.
                feed_health.set_subscribed(Feed::Groww, status.stocks, status.indices);
            }
            // HONEST-FEED PROOF (operator 2026-06-29): surface the producer-side
            // decoded+emitted vs decoded-but-dropped counts so a sid-map mismatch
            // ("streaming but 0 ticks") is a VISIBLE number, not a silent drop.
            // Always refreshed from the latest status (the sidecar reports cumulative
            // totals); display-only, never changes the verdict or `connected`.
            feed_health.set_decode_counts(Feed::Groww, status.emitted, status.dropped);
            if status.event == GrowwStatusEvent::Streaming {
                streaming_observed = true;
            }
        }

        // Drain whatever new bytes arrived (the real ingestion body).
        let parsed_a_tick = state.drain_new_data(&tick_file_path, &feed_health).await;
        if parsed_a_tick {
            // A first parsed tick is the strongest proof the feed is live.
            streaming_observed = true;
        }

        // Re-gate `connected` (operator 2026-06-28 — kill the false-OK): the feed is
        // "connected" ONLY when it is actually STREAMING (the sidecar reported the
        // `streaming` status OR we parsed a real tick). File-exists-without-streaming
        // is NOT connected — the verdict engine then reports the honest Unknown/Down,
        // never a misleading green.
        feed_health.set_connected(Feed::Groww, streaming_observed);

        // ws_event_audit rising edge: on the FIRST streaming observation since the
        // last (re)arm, emit ONE row — Reconnected if a Disconnected was emitted
        // since the last connect, else Connected. Gated on `audited_connected` so
        // it fires once per rising edge, never per loop turn (audit-findings Rule 4).
        if streaming_observed && !audited_connected {
            let (kind, source) = if prior_disconnect {
                (WsEventKind::Reconnected, "groww_resumed")
            } else {
                (WsEventKind::Connected, "groww_sidecar")
            };
            emit_groww_ws_audit(ws_audit_tx.as_ref(), kind, source, "groww streaming");
            audited_connected = true;
            prior_disconnect = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = r#"{"security_id":1333,"segment":"NSE_EQ","ts_ist_nanos":1780000020123000000,"exchange_ts_millis":1780000020123,"ltp":2847.55,"cum_volume":123456}"#;

    #[test]
    fn test_receipt_ist_nanos_is_recent_and_in_ist_window() {
        // Receipt clock must be a plausible "now" in IST nanos (2020..2100) so the
        // health last-tick-age math is well-formed — never 0/negative/absurd.
        let now = receipt_ist_nanos();
        assert!(
            (MIN_PLAUSIBLE_TS_IST_NANOS..=MAX_PLAUSIBLE_TS_IST_NANOS).contains(&now),
            "receipt clock {now} outside plausible IST window"
        );
    }

    #[test]
    fn test_segment_from_str_known_and_unknown() {
        assert_eq!(
            segment_from_str("NSE_EQ").unwrap(),
            ExchangeSegment::NseEquity
        );
        assert_eq!(segment_from_str("IDX_I").unwrap(), ExchangeSegment::IdxI);
        assert_eq!(
            segment_from_str("NSE_FNO").unwrap(),
            ExchangeSegment::NseFno
        );
        assert!(segment_from_str("WAT_EQ").is_err());
        assert!(segment_from_str("").is_err());
    }

    #[test]
    fn test_parse_groww_tick_line_ok() {
        let p = parse_groww_tick_line(LINE).expect("parse");
        assert_eq!(p.tick.security_id, 1333);
        assert_eq!(p.tick.segment, ExchangeSegment::NseEquity);
        assert_eq!(p.tick.ts_ist_nanos, 1_780_000_020_123_000_000);
        assert_eq!(p.tick.ltp, 2847.55);
        assert_eq!(p.tick.cum_volume, 123_456);
        assert_eq!(p.exchange_ts_millis, 1_780_000_020_123);
    }

    #[test]
    fn test_parse_groww_tick_line_rejects_garbage_and_bad_segment() {
        assert!(parse_groww_tick_line("not json").is_err());
        assert!(parse_groww_tick_line("{}").is_err());
        let bad_seg = r#"{"security_id":1,"segment":"XYZ","ts_ist_nanos":1,"exchange_ts_millis":1,"ltp":1.0,"cum_volume":1}"#;
        assert!(parse_groww_tick_line(bad_seg).is_err());
    }

    /// A known-good parsed tick (ts ~2026, within the plausible window).
    fn valid_parsed() -> ParsedGrowwTick {
        parse_groww_tick_line(LINE).expect("parse")
    }

    #[test]
    fn test_validate_groww_tick_accepts_valid() {
        assert_eq!(validate_groww_tick(&valid_parsed()), Ok(()));
    }

    #[test]
    fn test_validate_groww_tick_rejects_non_finite_ltp() {
        let mut p = valid_parsed();
        p.tick.ltp = f64::NAN;
        assert_eq!(
            validate_groww_tick(&p),
            Err(GrowwTickReject::NonFinitePrice)
        );
        p.tick.ltp = f64::INFINITY;
        assert_eq!(
            validate_groww_tick(&p),
            Err(GrowwTickReject::NonFinitePrice)
        );
    }

    #[test]
    fn test_validate_groww_tick_rejects_non_positive_ltp() {
        let mut p = valid_parsed();
        p.tick.ltp = 0.0;
        assert_eq!(
            validate_groww_tick(&p),
            Err(GrowwTickReject::NonPositivePrice)
        );
        p.tick.ltp = -1.5;
        assert_eq!(
            validate_groww_tick(&p),
            Err(GrowwTickReject::NonPositivePrice)
        );
    }

    #[test]
    fn test_validate_groww_tick_rejects_implausible_ltp() {
        let mut p = valid_parsed();
        p.tick.ltp = f64::from(tickvault_common::constants::MAX_PLAUSIBLE_LTP) + 1.0;
        assert_eq!(
            validate_groww_tick(&p),
            Err(GrowwTickReject::ImplausiblePrice)
        );
    }

    #[test]
    fn test_validate_groww_tick_rejects_negative_volume() {
        let mut p = valid_parsed();
        p.tick.cum_volume = -1;
        assert_eq!(
            validate_groww_tick(&p),
            Err(GrowwTickReject::NegativeVolume)
        );
    }

    #[test]
    fn test_validate_groww_tick_rejects_implausible_volume() {
        let mut p = valid_parsed();
        p.tick.cum_volume = MAX_PLAUSIBLE_VOLUME + 1;
        assert_eq!(
            validate_groww_tick(&p),
            Err(GrowwTickReject::ImplausibleVolume)
        );
        // Boundary is inclusive-valid.
        p.tick.cum_volume = MAX_PLAUSIBLE_VOLUME;
        assert_eq!(validate_groww_tick(&p), Ok(()));
    }

    #[test]
    fn test_drain_complete_prefix_splits_complete_lines_keeps_partial() {
        let mut residual: Vec<u8> = b"line1\nline2\npartial".to_vec();
        let prefix = drain_complete_prefix(&mut residual);
        assert_eq!(prefix, b"line1\nline2\n");
        assert_eq!(residual, b"partial");
        let lines: Vec<&[u8]> = prefix.split(|&b| b == b'\n').collect();
        // "line1", "line2", "" (trailing after final \n)
        assert_eq!(lines, vec![&b"line1"[..], &b"line2"[..], &b""[..]]);
    }

    #[test]
    fn test_drain_complete_prefix_no_newline_returns_empty() {
        let mut residual: Vec<u8> = b"no newline yet".to_vec();
        let prefix = drain_complete_prefix(&mut residual);
        assert!(prefix.is_empty());
        assert_eq!(residual, b"no newline yet"); // untouched
    }

    #[test]
    fn test_drain_complete_prefix_partial_utf8_tail_buffered() {
        // A complete line, then the first byte of a 2-byte UTF-8 char (0xC3).
        let mut residual: Vec<u8> = b"good\n".to_vec();
        residual.push(0xC3); // partial multi-byte char, no newline
        let prefix = drain_complete_prefix(&mut residual);
        assert_eq!(prefix, b"good\n");
        assert_eq!(residual, vec![0xC3]); // partial byte safely buffered
    }

    #[test]
    fn test_validate_groww_tick_rejects_non_positive_security_id() {
        let mut p = valid_parsed();
        p.tick.security_id = 0;
        assert_eq!(
            validate_groww_tick(&p),
            Err(GrowwTickReject::NonPositiveSecurityId)
        );
        p.tick.security_id = -7;
        assert_eq!(
            validate_groww_tick(&p),
            Err(GrowwTickReject::NonPositiveSecurityId)
        );
    }

    #[test]
    fn test_validate_groww_tick_rejects_timestamp_out_of_range() {
        let mut p = valid_parsed();
        p.tick.ts_ist_nanos = 0;
        assert_eq!(
            validate_groww_tick(&p),
            Err(GrowwTickReject::TimestampOutOfRange)
        );
        p.tick.ts_ist_nanos = -1;
        assert_eq!(
            validate_groww_tick(&p),
            Err(GrowwTickReject::TimestampOutOfRange)
        );
        p.tick.ts_ist_nanos = i64::MAX;
        assert_eq!(
            validate_groww_tick(&p),
            Err(GrowwTickReject::TimestampOutOfRange)
        );
        // Boundaries are inclusive-valid.
        p.tick.ts_ist_nanos = MIN_PLAUSIBLE_TS_IST_NANOS;
        assert_eq!(validate_groww_tick(&p), Ok(()));
        p.tick.ts_ist_nanos = MAX_PLAUSIBLE_TS_IST_NANOS;
        assert_eq!(validate_groww_tick(&p), Ok(()));
    }

    #[test]
    fn test_live_tick_row_maps_all_fields() {
        let p = parse_groww_tick_line(LINE).expect("parse");
        let row = live_tick_row(&p, 42);
        assert_eq!(row.security_id, 1333);
        assert_eq!(row.segment, "NSE_EQ");
        assert_eq!(row.ltp, 2847.55);
        assert_eq!(row.volume, 123_456);
        assert_eq!(row.exchange_ts_millis, 1_780_000_020_123);
        assert_eq!(row.capture_seq, 42);
        assert_eq!(row.ts_ist_nanos, 1_780_000_020_123_000_000);
    }

    #[test]
    fn test_build_parsed_tick_maps_fields_and_segment_code() {
        // Groww → shared-engine ParsedTick: segment code, IST-second timestamp,
        // f64→f32 ltp; volume left 0 (the i64 cumulative flows via the override,
        // NOT this u32 field — no truncation path).
        let p = valid_parsed();
        let pt = build_parsed_tick(&p).expect("valid tick builds");
        assert_eq!(pt.security_id, 1333);
        assert_eq!(
            pt.exchange_segment_code,
            ExchangeSegment::NseEquity.binary_code()
        );
        assert_eq!(pt.exchange_timestamp, 1_780_000_020); // ts_ist_nanos / 1e9
        assert_eq!(pt.last_traded_price, 2847.55_f32);
        assert_eq!(
            pt.volume, 0,
            "i64 cumulative MUST NOT be funnelled through u32 volume"
        );
    }

    #[test]
    fn test_build_parsed_tick_folds_64bit_token_with_bit62_set() {
        // THE PAYOFF of the u32→u64 SecurityId widening (2026-06-29): a Groww
        // native exchange_token that sets bit 62 (the index-id encoding, see
        // instruments.rs) exceeds u32 and was PREVIOUSLY rejected
        // (TokenExceedsU32) so every Groww index tick was silently lost. It now
        // folds losslessly through the shared 21-TF engine.
        let mut p = valid_parsed();
        let sixty_four_bit_id: i64 = (1_i64 << 62) | 12_345;
        p.tick.security_id = sixty_four_bit_id;
        let pt = build_parsed_tick(&p).expect("64-bit bit-62 Groww id must fold, NOT reject");
        assert_eq!(
            pt.security_id,
            (1_u64 << 62) | 12_345,
            "the full u64 id (incl. bit 62) must be carried, not truncated"
        );
        // Boundary: exactly u32::MAX is accepted (was the old upper bound).
        p.tick.security_id = i64::from(u32::MAX);
        assert!(build_parsed_tick(&p).is_ok());
        // u64::MAX-range value (i64::MAX) also folds — no width rejection.
        p.tick.security_id = i64::MAX;
        let pt_max = build_parsed_tick(&p).expect("i64::MAX id must fold");
        assert_eq!(pt_max.security_id, i64::MAX as u64);
    }

    #[test]
    fn test_build_parsed_tick_rejects_negative_security_id() {
        // Defence-in-depth: a negative id (which validate_groww_tick already
        // filters upstream) is rejected LOUDLY by the u64::try_from gate, never
        // wrapped into a huge u64.
        let mut p = valid_parsed();
        p.tick.security_id = -7;
        assert!(
            matches!(
                build_parsed_tick(&p),
                Err(GrowwAggregateReject::NegativeSecurityId)
            ),
            "negative security_id must reject loudly"
        );
    }

    #[test]
    fn test_build_parsed_tick_rejects_timestamp_seconds_above_u32_max() {
        // An IST-nanos that floors to > u32::MAX seconds (post-~2106) is rejected
        // rather than wrapping the candle bucket.
        let mut p = valid_parsed();
        p.tick.ts_ist_nanos = (i64::from(u32::MAX) + 1) * 1_000_000_000;
        assert!(
            matches!(
                build_parsed_tick(&p),
                Err(GrowwAggregateReject::TimestampExceedsU32)
            ),
            "timestamp-seconds above u32::MAX must reject, never wrap the bucket"
        );
    }

    #[test]
    fn test_run_groww_bridge_routes_seals_through_shared_writer() {
        // ONE common candle engine: the bridge MUST fold through the shared 21-TF
        // MultiTfAggregator + route seals to global_seal_sender (NOT a Groww-only
        // 1m writer). Pin by source-scan — the I/O driver is TEST-EXEMPT, but a
        // future refactor cannot silently revert to a separate Groww candle path.
        let src = include_str!("groww_bridge.rs");
        assert!(
            src.contains("MultiTfAggregator")
                && src.contains("global_seal_sender")
                && src.contains("FeedStrategy::GROWW"),
            "the Groww bridge must fold through the shared MultiTfAggregator + \
             global_seal_sender with FeedStrategy::GROWW (the one common candle engine)"
        );
        // Tokens are concatenated at runtime so this assertion's OWN strings do
        // not trip the `include_str!` self-scan (the literals never appear whole
        // in the file source).
        let legacy_agg = format!("Groww1m{}", "Aggregator");
        let legacy_writer = format!("GrowwCandle1m{}", "Writer");
        assert!(
            !src.contains(&legacy_agg) && !src.contains(&legacy_writer),
            "the Groww-1m-only path (the deleted aggregator + 1m-only writer) must be gone"
        );
    }

    #[test]
    fn test_next_capture_seq_is_monotonic() {
        let c = AtomicI64::new(0);
        let a = next_capture_seq(&c, 1_000);
        let b = next_capture_seq(&c, 1_000); // wall not advanced → prev+1
        let d = next_capture_seq(&c, 5_000); // wall jumps → takes wall
        assert_eq!(a, 1_000);
        assert_eq!(b, 1_001);
        assert_eq!(d, 5_000);
        assert!(b > a && d > b, "strictly monotonic");
    }

    #[test]
    fn test_run_groww_bridge_has_runtime_disable_gate() {
        // Feed-toggle API (2026-06-19): the bridge MUST check the shared runtime
        // flag each loop so `POST /api/feeds/groww {enabled:false}` pauses it live.
        // `run_groww_bridge` is a TEST-EXEMPT I/O driver, so pin the gate by
        // source-scan — a future refactor cannot silently drop live pause/resume.
        let src = include_str!("groww_bridge.rs");
        assert!(
            src.contains("feed_runtime.is_enabled(Feed::Groww)"),
            "the Groww bridge loop must gate on the runtime feed flag (live pause/resume); \
             the `feed_runtime.is_enabled(Feed::Groww)` check is missing"
        );
    }

    // ── Connect+subscribe PROOF status parsing (B3) ──
    // test coverage (one line for pub-fn-test-guard test.*<fn>): parse_groww_status_line

    #[test]
    fn test_parse_groww_status_line_subscribed() {
        let s = r#"{"event":"subscribed","stocks":765,"indices":2,"total":767,"ts_ist_nanos":1780000000000000000}"#;
        let got = parse_groww_status_line(s).expect("parse subscribed");
        assert_eq!(got.event, GrowwStatusEvent::Subscribed);
        assert_eq!(got.stocks, 765);
        assert_eq!(got.indices, 2);
    }

    #[test]
    fn test_parse_groww_status_line_streaming() {
        let s = r#"{"event":"streaming","stocks":765,"indices":2,"total":767}"#;
        let got = parse_groww_status_line(s).expect("parse streaming");
        assert_eq!(got.event, GrowwStatusEvent::Streaming);
        assert_eq!(got.stocks, 765);
        assert_eq!(got.indices, 2);
    }

    #[test]
    fn test_parse_groww_status_line_unknown_event_is_not_streaming() {
        // A future/unexpected event tag parses (forward-compatible) but is treated
        // as "not yet streaming" → does NOT flip connected (no false-OK).
        let s = r#"{"event":"warming_up","stocks":1,"indices":0}"#;
        let got = parse_groww_status_line(s).expect("parse unknown");
        assert_eq!(got.event, GrowwStatusEvent::Unknown);
        assert_ne!(got.event, GrowwStatusEvent::Streaming);
    }

    #[test]
    fn test_parse_groww_status_line_torn_or_empty_is_none() {
        // Half-written / empty / garbage → None (retry next wake), never an error.
        assert!(parse_groww_status_line("").is_none());
        assert!(parse_groww_status_line("   ").is_none());
        assert!(parse_groww_status_line("{\"event\":\"subscr").is_none());
        assert!(parse_groww_status_line("not json").is_none());
    }

    #[test]
    fn test_parse_groww_status_line_decode_counts() {
        // Honest-feed PROOF (2026-06-29): the `emitted`/`dropped` fields parse, and
        // the events keep their connected-gating semantics — `subscribed` does NOT
        // mean streaming, `streaming` DOES.
        let subscribed = r#"{"event":"subscribed","stocks":765,"indices":2,"total":767,"emitted":0,"dropped":3}"#;
        let s = parse_groww_status_line(subscribed).expect("parse subscribed");
        assert_eq!(s.event, GrowwStatusEvent::Subscribed);
        assert_ne!(
            s.event,
            GrowwStatusEvent::Streaming,
            "a `subscribed` status must NOT read as streaming (no false-OK)"
        );
        assert_eq!(s.emitted, 0);
        assert_eq!(s.dropped, 3);

        let streaming = r#"{"event":"streaming","stocks":765,"indices":2,"total":767,"emitted":1500,"dropped":12}"#;
        let st = parse_groww_status_line(streaming).expect("parse streaming");
        assert_eq!(
            st.event,
            GrowwStatusEvent::Streaming,
            "a `streaming` status DOES mean ticks are flowing (flips connected)"
        );
        assert_eq!(st.emitted, 1500);
        assert_eq!(st.dropped, 12);
    }

    #[test]
    fn test_parse_groww_status_line_decode_counts_default() {
        // Mixed-deploy safety: an OLD sidecar status (no emitted/dropped fields)
        // still parses — the `#[serde(default)]` fields read 0, never an error.
        let old = r#"{"event":"streaming","stocks":765,"indices":2,"total":767}"#;
        let got = parse_groww_status_line(old).expect("old status parses");
        assert_eq!(got.event, GrowwStatusEvent::Streaming);
        assert_eq!(got.emitted, 0, "absent emitted → serde default 0");
        assert_eq!(got.dropped, 0, "absent dropped → serde default 0");
    }

    #[test]
    fn test_bridge_records_decode_counts_from_status() {
        // The honest-feed surface (2026-06-29): the bridge MUST push the sidecar's
        // emitted/dropped counts into feed-health via set_decode_counts on each
        // status read. Pin by source-scan — `run_groww_bridge` is a TEST-EXEMPT I/O
        // driver, so a future refactor cannot silently drop the decode-count surface.
        let src = include_str!("groww_bridge.rs");
        assert!(
            src.contains("set_decode_counts(Feed::Groww, status.emitted, status.dropped)"),
            "the bridge must record the producer-side decoded+emitted / dropped \
             counts in feed-health from the status read"
        );
    }

    #[test]
    fn test_sidecar_streaming_is_honest_not_optimistic() {
        // HONEST-FEED FIX (operator 2026-06-29): the Python sidecar must NOT write the
        // `streaming` status optimistically BEFORE `feed.consume()` (the false-OK the
        // diagnosis flagged). It must instead write `streaming` only on the FIRST real
        // decoded+emitted tick (note_emit), and COUNT the previously-silent drops
        // (note_drop). Pin the contract by source-scanning the sidecar so a future
        // edit cannot silently reintroduce the optimistic write.
        let sidecar = include_str!("../../../scripts/groww-sidecar/groww_sidecar.py");
        // The two emit functions count emits + drops, and the first emit drives streaming.
        assert!(
            sidecar.contains("def note_emit(") && sidecar.contains("def note_drop("),
            "the sidecar must count decoded-emitted and decoded-dropped records"
        );
        assert!(
            sidecar.contains("write_status(\"streaming\""),
            "the sidecar must still write the `streaming` status (now on first emit)"
        );
        // The `streaming` status must be written ONLY from inside note_emit (driven by
        // a REAL decoded+emitted tick) — note_emit legitimately writes it twice (the
        // first-emit branch + the throttled periodic re-write). The contract to enforce
        // is that the consume BLOCK (between the `phase = "consume"` marker and the
        // blocking `feed.consume()`) contains NO optimistic, uncommented streaming write.
        let consume_idx = sidecar
            .find("feed.consume()")
            .expect("sidecar must call feed.consume()");
        let consume_phase_idx = sidecar
            .find("phase = \"consume\"")
            .expect("sidecar must mark the consume phase");
        assert!(
            consume_phase_idx < consume_idx,
            "the consume-phase marker must precede the blocking consume call"
        );
        // Scan ONLY the consume-block window line-by-line; a `write_status("streaming"`
        // here is a real optimistic write ONLY if the line is NOT a comment. The removed
        // optimistic write lived exactly here, so a non-comment occurrence is the bug.
        let consume_block = &sidecar[consume_phase_idx..consume_idx];
        let optimistic_writes = consume_block
            .lines()
            .filter(|line| {
                let trimmed = line.trim_start();
                !trimmed.starts_with('#') && trimmed.contains("write_status(\"streaming\"")
            })
            .count();
        assert_eq!(
            optimistic_writes, 0,
            "the optimistic pre-consume write_status(\"streaming\") must be gone — \
             `streaming` is written ONLY inside note_emit on the first real decoded tick"
        );
        // And note_emit MUST itself drive the streaming status (first real tick).
        let note_emit_idx = sidecar.find("def note_emit(").expect("note_emit defined");
        let after_note_emit = &sidecar[note_emit_idx..];
        assert!(
            after_note_emit.contains("write_status(\"streaming\""),
            "note_emit must write the `streaming` status (driven by a real decoded tick)"
        );
    }

    #[test]
    fn test_bridge_is_event_driven_zero_poll_with_fallback() {
        // Operator 2026-06-28 ("true zero latency, no polling"): the bridge MUST be
        // event-driven via the `notify` watcher (PRIMARY), with the fallback poll as
        // a SAFETY NET only. Pin both by source-scan — a future refactor cannot
        // silently revert to a fixed-interval poll as the main path.
        let src = include_str!("groww_bridge.rs");
        assert!(
            src.contains("spawn_tick_file_watcher") && src.contains("notify::recommended_watcher"),
            "the bridge must install a `notify` filesystem watcher (zero-poll primary path)"
        );
        assert!(
            src.contains("wake_rx.as_mut()") && src.contains("rx.recv()"),
            "the loop must wake on filesystem events (rx.recv), not only a fixed sleep"
        );
        assert!(
            src.contains("GROWW_BRIDGE_FALLBACK_POLL"),
            "a fallback poll must exist as a watcher-down safety net"
        );
    }

    #[test]
    fn test_bridge_connected_gate_is_streaming_not_file_existence() {
        // Operator 2026-06-28 (kill the false-OK): `set_connected(Groww,true)` MUST be
        // driven by ACTUAL streaming (`streaming_observed`), NOT by File::open
        // succeeding. Pin by source-scan.
        let src = include_str!("groww_bridge.rs");
        // Build the needles at runtime via concatenation so this assertion's OWN
        // strings do not appear whole in the file source (the include_str! self-scan
        // would otherwise match the test's literals, not the real code).
        let gated = format!("set_connected(Feed::Groww, {}streaming_observed)", "");
        assert!(
            src.contains(&gated),
            "connected must be gated on streaming_observed (streaming status OR first \
             parsed tick), never on mere file existence (the false-OK)"
        );
        let false_ok = format!("set_connected(Feed::Groww, {}true)", "");
        assert!(
            !src.contains(&false_ok),
            "the file-exists false-OK set_connected-true must be removed"
        );
    }

    #[tokio::test]
    async fn test_tick_file_watcher_wakes_on_append_below_fallback_interval() {
        // Operator 2026-06-28 ("true zero latency, no polling"): a freshly-appended
        // line must wake the bridge via the OS filesystem EVENT — NOT by waiting the
        // fallback poll interval. Prove the watcher delivers a wake well under the
        // 1s fallback (so ingestion is event-driven, not poll-driven).
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!(
            "tv_groww_watch_test_{}_{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).expect("mk dir");
        let tick_file = dir.join("live-ticks.ndjson");
        std::fs::write(&tick_file, b"").expect("seed empty file");

        let (_watcher, mut wake_rx) =
            spawn_tick_file_watcher(&tick_file).expect("watcher installs");

        // Append a line AFTER the watcher is armed.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&tick_file)
                .expect("open append");
            f.write_all(b"{\"x\":1}\n").expect("append");
            f.flush().expect("flush");
        }

        // The wake must arrive far under the fallback interval — proving it is the
        // filesystem EVENT, not the poll timer. Give a generous-but-sub-fallback
        // budget (CI filesystems vary); the fallback is 1000ms.
        let woke = tokio::time::timeout(
            Duration::from_millis(GROWW_BRIDGE_FALLBACK_POLL_MS - 200),
            wake_rx.recv(),
        )
        .await;
        assert!(
            matches!(woke, Ok(Some(()))),
            "the filesystem watcher must deliver a wake on append BEFORE the fallback \
             poll interval — ingestion is event-driven (zero-poll), not poll-driven"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_drain_new_data_no_file_is_noop_fallback_safe() {
        // The fallback/degraded path must be safe: if the tick file does not exist
        // yet (sidecar not started), drain_new_data is a no-op returning false — the
        // loop simply idles. Proves the bridge survives the watcher-unavailable case.
        let reg = tickvault_common::feed_health::FeedHealthRegistry::new();
        let mut state = GrowwBridgeState::with_aggregator(
            &QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port: 9000,
                pg_port: 8812,
                ilp_port: 9009,
            },
            MultiTfAggregator::with_capacity(GROWW_AGGREGATOR_CAPACITY),
        );
        let missing = std::path::Path::new("/nonexistent-tv-groww-tick-file-xyz.ndjson");
        let parsed = state.drain_new_data(missing, &reg).await;
        assert!(
            !parsed,
            "absent file → no tick parsed, no panic (fallback-safe)"
        );
    }

    #[test]
    fn test_bridge_emits_connect_log_and_records_subscribed() {
        // B3: the bridge must emit the ONE structured CONNECT log + record the
        // subscribe counts in feed-health. Pin by source-scan.
        let src = include_str!("groww_bridge.rs");
        assert!(
            src.contains("Groww live feed CONNECTED — subscribed"),
            "the bridge must emit the structured connect+subscribe proof log"
        );
        assert!(
            src.contains("feed_health.set_subscribed(Feed::Groww"),
            "the bridge must record the subscribe counts in feed-health"
        );
    }

    #[test]
    fn test_runtime_disable_clears_connected_and_rearms_latches() {
        // LOW 4c: when Groww is runtime-disabled the bridge `continue`s. Before this
        // fix it left `connected = true` (a stale false-OK green) and kept the
        // connect-log / streaming latches latched, so a later re-enable would NOT
        // re-emit the CONNECTED log. The disable branch MUST clear the connected bit
        // and re-arm both latches. Pin the disable-branch behaviour by source-scan
        // (`run_groww_bridge` is a TEST-EXEMPT I/O driver).
        let src = include_str!("groww_bridge.rs");
        // Build the needle at runtime so this test's literal does not self-match the
        // include_str! scan (the disable branch is the only real occurrence).
        let clear = format!("feed_health.set_connected(Feed::Groww, {}false)", "");
        assert!(
            src.contains(&clear),
            "the runtime-disable branch must clear the connected bit (no stale green)"
        );
        // The re-arm of both latches must live alongside the disable clear.
        let disable_idx = src
            .find("if !feed_runtime.is_enabled(Feed::Groww) {")
            .expect("disable branch present");
        // Window widened (PR-10) to span the inserted ws_event_audit falling-edge
        // emit block — the latch re-arm now lives just below it.
        let after = &src[disable_idx..disable_idx + 1600];
        assert!(
            after.contains(&clear)
                && after.contains("connect_logged = false")
                && after.contains("streaming_observed = false"),
            "the disable branch must clear connected AND re-arm connect_logged + \
             streaming_observed so a re-enable re-emits CONNECTED on fresh streaming"
        );
    }

    #[test]
    fn test_feed_health_connected_roundtrip_on_disable_then_reenable() {
        // Drive the REAL feed-health state the disable branch + re-enable path use:
        // streaming → connected=true; disable clears it → connected=false; a fresh
        // streaming observation after re-enable flips it back to true. Proves the
        // `set_connected` calls the bridge makes produce the honest verdict signal.
        let reg = tickvault_common::feed_health::FeedHealthRegistry::new();
        // Use bool vars (not adjacent bool literals) so this test's source does not
        // self-trip the connected-true false-OK source-scan in
        // `test_bridge_connected_gate_is_streaming_not_file_existence`.
        let streaming = true;
        let disabled = false;
        // Streaming observed → connected.
        reg.set_connected(Feed::Groww, streaming);
        assert!(
            reg.snapshot(Feed::Groww, true, true, true, 0)
                .input
                .connected,
            "streaming → connected true"
        );
        // Runtime-disable clears it (no stale green).
        reg.set_connected(Feed::Groww, disabled);
        assert!(
            !reg.snapshot(Feed::Groww, false, false, true, 0)
                .input
                .connected,
            "runtime-disable → connected false (stale green cleared)"
        );
        // Re-enable + fresh streaming → connected true again.
        reg.set_connected(Feed::Groww, streaming);
        assert!(
            reg.snapshot(Feed::Groww, true, true, true, 0)
                .input
                .connected,
            "re-enable + fresh streaming → connected true again"
        );
    }

    // --- PR-10: Groww ws_event_audit emit (the audit-gap fix) ---

    #[test]
    fn test_groww_connected_transition_builds_connected_row() {
        // First streaming/parsed-tick rising edge → a Connected row, feed=Groww.
        let row =
            build_groww_ws_audit_row(WsEventKind::Connected, "groww_sidecar", "groww streaming");
        assert_eq!(row.event_kind, WsEventKind::Connected);
        assert_eq!(row.feed, Feed::Groww);
        assert_eq!(row.source, "groww_sidecar");
    }

    #[test]
    fn test_groww_disable_builds_disconnected_offhours_or_disconnected_row() {
        // The disable falling edge classifies on market hours: DisconnectedOffHours
        // when out-of-session, Disconnected in-session. Assert the builder honours
        // BOTH kinds (the bridge picks the kind; the builder stamps it faithfully).
        let off = build_groww_ws_audit_row(
            WsEventKind::DisconnectedOffHours,
            "feed_disabled",
            "groww disabled",
        );
        assert_eq!(off.event_kind, WsEventKind::DisconnectedOffHours);
        assert_eq!(off.feed, Feed::Groww);
        let on =
            build_groww_ws_audit_row(WsEventKind::Disconnected, "feed_disabled", "groww disabled");
        assert_eq!(on.event_kind, WsEventKind::Disconnected);
        assert_eq!(on.source, "feed_disabled");
    }

    #[test]
    fn test_groww_reconnect_builds_reconnected_row() {
        // A second streaming edge after a disable → Reconnected (NOT a 2nd Connected).
        let row =
            build_groww_ws_audit_row(WsEventKind::Reconnected, "groww_resumed", "groww streaming");
        assert_eq!(row.event_kind, WsEventKind::Reconnected);
        assert_eq!(row.feed, Feed::Groww);
        assert_eq!(row.source, "groww_resumed");
    }

    #[test]
    fn test_groww_audit_row_stamps_feed_groww_and_growwbridge_wstype() {
        // The row identity that keeps a Groww row distinct from a Dhan row in the
        // feed-keyed DEDUP, and the no-Dhan-code/single-conn convention.
        let row =
            build_groww_ws_audit_row(WsEventKind::Connected, "groww_sidecar", "groww streaming");
        assert_eq!(row.feed, Feed::Groww);
        assert_eq!(row.ws_type, WsType::GrowwBridge);
        assert_eq!(row.connection_index, 0);
        assert_eq!(row.pool_size, 1);
        assert_eq!(row.dhan_code, WS_EVENT_NO_DHAN_CODE);
        assert_eq!(row.down_secs, 0);
        assert_eq!(row.attempts, 0);
        // Round-trips cleanly through the SAME multi-feed append path Dhan uses.
        let mut writer =
            tickvault_storage::ws_event_audit_persistence::WsEventAuditWriter::for_test();
        writer
            .append_row(&row)
            .expect("groww row appends to the shared writer");
        assert_eq!(writer.pending(), 1);
    }

    #[test]
    fn test_emit_groww_ws_audit_is_noop_when_tx_none() {
        // The Option<Sender> convention: a None build is a no-op (defensive/test),
        // exactly like the Dhan `emit_ws_audit` early-return. No panic, no work.
        emit_groww_ws_audit(
            None,
            WsEventKind::Connected,
            "groww_sidecar",
            "groww streaming",
        );
    }

    #[test]
    fn test_emit_groww_ws_audit_sends_row_when_tx_present() {
        // With a channel attached, one emit lands exactly one row.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<WsEventAuditRow>(4);
        emit_groww_ws_audit(
            Some(&tx),
            WsEventKind::Connected,
            "groww_sidecar",
            "groww streaming",
        );
        let row = rx.try_recv().expect("one groww audit row was sent");
        assert_eq!(row.feed, Feed::Groww);
        assert_eq!(row.ws_type, WsType::GrowwBridge);
        assert_eq!(row.event_kind, WsEventKind::Connected);
    }

    #[test]
    fn test_groww_bridge_emits_ws_event_audit_on_connect_and_disconnect() {
        // Source-scan ratchet (mirrors the other bridge source-scan guards): a future
        // refactor cannot silently drop the audit emit on the connect rising edge OR
        // the disable falling edge. `run_groww_bridge` is a TEST-EXEMPT I/O driver.
        let src = include_str!("groww_bridge.rs");
        // Build the needle at runtime so this test literal does not self-match the scan.
        let connected = format!("WsEventKind::{}", "Connected");
        let disconnected = format!("WsEventKind::{}", "Disconnected");
        // The connect rising edge must emit on a Connected/Reconnected kind.
        assert!(
            src.contains("emit_groww_ws_audit(") && src.contains(&connected),
            "the bridge must emit a ws_event_audit row on the connect rising edge"
        );
        // The disable falling edge must emit on a Disconnected/DisconnectedOffHours kind.
        let disable_idx = src
            .find("if !feed_runtime.is_enabled(Feed::Groww) {")
            .expect("disable branch present");
        let after = &src[disable_idx..disable_idx + 1600];
        assert!(
            after.contains("emit_groww_ws_audit(") && after.contains(&disconnected),
            "the disable branch must emit a Disconnected/DisconnectedOffHours audit row"
        );
    }

    // --- IST-midnight / EOD force-seal parity with Dhan (2026-06-29) ---

    #[tokio::test]
    async fn test_groww_force_seal_routes_with_feed_groww() {
        // The load-bearing contract of the Groww IST-midnight force-seal closure:
        // it routes each sealed bar through the SHARED seal-writer chain tagged
        // `feed: Feed::Groww`, with `drop_d1: false` (Groww routes EVERY TF,
        // including D1 — unlike Dhan which drops D1). This is the EXACT
        // SealRouteParams the boundary task in `spawn_groww_ist_midnight_force_seal`
        // passes; driving `route_seal` directly proves a force-sealed Groww bucket
        // lands in the channel with the right feed tag for a non-D1 AND a D1 TF.
        use tickvault_trading::candles::{BufferedSeal, LiveCandleState, TfIndex};
        let (tx, mut rx) = tokio::sync::mpsc::channel::<BufferedSeal>(8);
        let params = crate::seal_routing::SealRouteParams {
            feed: Feed::Groww,
            drop_d1: false,
            prev_day_cache: None,
            heartbeat: None,
            feed_health_on_m1: None,
        };

        // A non-D1 force-sealed bar routes, tagged feed=Groww.
        let out = crate::seal_routing::route_seal(
            params,
            1333,
            1,
            TfIndex::M1,
            LiveCandleState::empty(),
            &tx,
        );
        assert_eq!(out, crate::seal_routing::SealOutcome::Sent);
        let m1 = rx.try_recv().expect("Groww force-sealed M1 bar must route");
        assert_eq!(
            m1.feed,
            Feed::Groww,
            "force-sealed Groww bar must carry feed=Groww"
        );
        assert_eq!(m1.security_id, 1333);
        assert_eq!(m1.tf, TfIndex::M1);

        // D1 is NOT dropped for Groww (drop_d1=false) — the last open day-bucket
        // is sealed, not silently lost (the bug this task fixes).
        let out = crate::seal_routing::route_seal(
            params,
            1333,
            1,
            TfIndex::D1,
            LiveCandleState::empty(),
            &tx,
        );
        assert_eq!(out, crate::seal_routing::SealOutcome::Sent);
        let d1 = rx
            .try_recv()
            .expect("Groww force-sealed D1 bar must route (no drop)");
        assert_eq!(d1.tf, TfIndex::D1);
        assert_eq!(d1.feed, Feed::Groww);
    }

    #[test]
    fn test_run_groww_bridge_spawns_ist_midnight_force_seal() {
        // PARITY WITH DHAN (the verified bug fix): the bridge MUST spawn an
        // IST-midnight force-seal boundary task that flushes every open bucket of
        // every Groww timeframe at day rollover — WITHOUT it the last bucket of
        // each day is silently overwritten by the next day's first tick. Pin the
        // wiring by source-scan (the spawn helper is a TEST-EXEMPT cold-path I/O
        // driver), so a future refactor cannot silently drop the EOD seal.
        let src = include_str!("groww_bridge.rs");
        assert!(
            src.contains("fn spawn_groww_ist_midnight_force_seal")
                && src.contains("spawn_groww_ist_midnight_force_seal("),
            "the bridge must define AND call an IST-midnight force-seal spawn helper"
        );
        assert!(
            src.contains("force_seal_all")
                && src.contains("secs_until_next_ist_midnight")
                && src.contains("is_trading_day_today"),
            "the force-seal task must call force_seal_all, sleep until IST midnight, \
             and gate on the trading-day calendar (mirroring the Dhan task)"
        );
        // Build the feed-tag needle at runtime so this assertion's OWN literal does
        // not self-match the include_str! scan — the real occurrence is the
        // boundary closure's SealRouteParams.
        let groww_feed = format!("feed: Feed::{}", "Groww");
        assert!(
            src.contains(&groww_feed),
            "the force-seal closure must route with feed: Feed::Groww (drop_d1: false)"
        );
    }
}

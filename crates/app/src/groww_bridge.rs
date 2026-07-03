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
pub const GROWW_AGGREGATOR_CAPACITY: usize = 1_200;

/// Max bytes read from the tick file per wake (hostile-review HIGH 2026-06-19).
/// Bounds the per-wake allocation so a large backlog on restart (e.g. a full
/// trading day of ~779-instrument NDJSON) is drained in fixed 4 MiB chunks
/// across wakes instead of one multi-GB `read_to_string` that would OOM the
/// 8 GiB host. The event-driven watcher re-fires immediately if more remains.
const GROWW_BRIDGE_MAX_READ_BYTES: u64 = 4 * 1024 * 1024;

/// F3 hostile finding (2026-07-02): cap on RETAINED ILP-buffer bytes during a
/// sustained QuestDB outage. Beyond this, the bridge PAUSES NDJSON consumption
/// (reads 0 bytes; offset NOT advanced) — the capture file itself is the
/// durable spill tier, so nothing is lost while RAM stays bounded and the
/// flush retries a fixed-size buffer instead of an ever-growing one
/// (previously: unbounded RAM ramp + quadratic re-transmit across a
/// multi-hour outage).
const GROWW_ILP_BUFFER_MAX_BYTES: usize = 64 * 1024 * 1024;

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

/// PR-4 (2026-07-02): max tolerated FUTURE skew between a tick's exchange
/// timestamp and our receipt clock. Generous for legitimate exchange-vs-box
/// skew (~15x observed), fatal for day-level poison: a tick stamped
/// "tomorrow" (sidecar bug / SDK clock glitch) would land in a future candle
/// bucket and confuse the IST-midnight force-seal.
const GROWW_FUTURE_TS_TOLERANCE_NANOS: i64 = 60 * 1_000_000_000;

/// Minimum plausible wall-clock receipt (2020-01-01 UTC expressed in the same
/// IST-nanos convention). F6 hostile finding (2026-07-02): a degenerate clock
/// read yields `IST_UTC_OFFSET_NANOS` (a ~1970 instant), NOT 0 — so the
/// future-ts clamp must fail OPEN for any implausibly OLD receipt, otherwise
/// a dead clock would mass-reject every valid tick as FutureTimestamp
/// (fail-closed by accident).
const GROWW_MIN_PLAUSIBLE_RECEIPT_IST_NANOS: i64 = 1_577_836_800 * 1_000_000_000;

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
    /// `ts_ist_nanos` is ahead of OUR receipt clock beyond
    /// `GROWW_FUTURE_TS_TOLERANCE_NANOS` (PR-4 2026-07-02) — a "tomorrow"
    /// stamp would poison a future candle bucket.
    FutureTimestamp,
    /// `ts_ist_nanos` is outside the plausible `[2020, 2100)` IST window.
    TimestampOutOfRange,
}

/// Validates a parsed Groww tick before persist. Pure, O(1) (a handful of
/// finite/compare checks), no allocation. Mirrors the Dhan `is_valid_ltp`
/// upper/lower bounds so it can never reject a genuine price.
fn validate_groww_tick(
    parsed: &ParsedGrowwTick,
    receipt_ist_nanos: i64,
) -> Result<(), GrowwTickReject> {
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
    // PR-4 relative clamp: reject a tick stamped ahead of OUR receipt clock
    // beyond the skew tolerance. Applies only when the clock read succeeded
    // (receipt > 0) — a broken clock fails OPEN to the static bounds above,
    // never mass-rejecting a healthy feed.
    if receipt_ist_nanos > GROWW_MIN_PLAUSIBLE_RECEIPT_IST_NANOS
        && parsed.tick.ts_ist_nanos
            > receipt_ist_nanos.saturating_add(GROWW_FUTURE_TS_TOLERANCE_NANOS)
    {
        return Err(GrowwTickReject::FutureTimestamp);
    }
    Ok(())
}

/// Wall-clock receipt time in IST epoch nanos (`Utc::now()` + IST offset) — the
/// instant WE captured a tick. Used ONLY for live-feed-health last-tick-age math
/// so it is consistent with the `/api/feeds/health` endpoint's clock; never used
/// for persistence (rows keep the exchange `ts_ist_nanos`). A clock read that
/// somehow fails saturates to `IST_UTC_OFFSET_NANOS` (a ~1970 instant, NOT 0)
/// — treated as "very old": fails safe toward Down for health math, and fails
/// OPEN for the future-ts clamp (guarded by
/// [`GROWW_MIN_PLAUSIBLE_RECEIPT_IST_NANOS`], F6 hostile finding 2026-07-02).
fn receipt_ist_nanos() -> i64 {
    chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(0)
        .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS)
}

/// Pure liveness-advance decision (2026-07-03 frozen-snapshot masking fix):
/// returns `true` — and advances `prev_max` — ONLY when the max exchange
/// timestamp seen this wake is STRICTLY greater than the max ever seen. Feed
/// liveness (the FEED-STALL-01 watchdog's `last_tick_age_secs` input) is
/// stamped only on `true`, so:
/// - a frozen snapshot re-dumped forever (same exchange ts, any price) stops
///   counting as alive → the stall watchdog fires instead of being masked;
/// - a byte-0 re-tail REPLAY of already-seen lines (ts ≤ max) never stamps;
/// - a healthy in-market feed (tsInMillis advances every second across the
///   universe) stamps every wake, exactly as before.
/// O(1), no allocation. `wake_max = 0` (no parsed lines) never advances.
pub(crate) fn liveness_ts_advanced(prev_max: &mut i64, wake_max_exchange_ts_nanos: i64) -> bool {
    if wake_max_exchange_ts_nanos > *prev_max {
        *prev_max = wake_max_exchange_ts_nanos;
        true
    } else {
        false
    }
}

/// Pure per-wake read-budget decision (F3 hostile finding 2026-07-02): 0 when
/// the retained ILP buffer is over [`GROWW_ILP_BUFFER_MAX_BYTES`] (pause
/// consumption — the capture file is the durable spill tier, offset not
/// advanced, zero loss), else the bounded chunk size.
#[must_use]
pub(crate) fn wake_read_budget(buffered_bytes: usize, len: u64, offset: u64) -> u64 {
    if buffered_bytes >= GROWW_ILP_BUFFER_MAX_BYTES {
        return 0;
    }
    len.saturating_sub(offset).min(GROWW_BRIDGE_MAX_READ_BYTES)
}

/// Builds the shared `ticks` (feed='groww') row for a parsed tick. `capture_seq`
/// is the monotonic, replay-stable dedup tiebreaker (TICK-SEQ-01).
/// `received_at_ist_nanos` is the per-WAKE receipt stamp from [`row_received_at`]
/// (fix #3, 2026-07-03 lag forensics) — a stored column only, NOT part of the
/// DEDUP key, so replay idempotency is untouched. Pure + testable.
fn live_tick_row(
    parsed: &ParsedGrowwTick,
    capture_seq: i64,
    received_at_ist_nanos: Option<i64>,
) -> GrowwLiveTickRow {
    GrowwLiveTickRow {
        ts_ist_nanos: parsed.tick.ts_ist_nanos,
        security_id: parsed.tick.security_id,
        segment: parsed.tick.segment.as_str(),
        ltp: parsed.tick.ltp,
        volume: parsed.tick.cum_volume,
        exchange_ts_millis: parsed.exchange_ts_millis,
        capture_seq,
        received_at_ist_nanos,
    }
}

/// Pure receipt-stamp gate (fix #3, 2026-07-03 lag forensics): the `received_at`
/// value persisted on every `feed='groww'` row — `Some(receipt)` only when the
/// per-wake clock read is plausible (mirrors `validate_groww_tick`'s fail-open
/// guard), so a broken clock writes NULL, never a ~1970 stamp. One clock read
/// per wake (O(1) preserved — no per-line syscall added).
fn row_received_at(receipt_ist_nanos: i64) -> Option<i64> {
    (receipt_ist_nanos > GROWW_MIN_PLAUSIBLE_RECEIPT_IST_NANOS).then_some(receipt_ist_nanos)
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
    /// Last offset-snapshot write instant (PR-3 throttle — one write per
    /// [`GROWW_OFFSET_PERSIST_MIN_SECS`], cold path).
    last_offset_persist: Option<std::time::Instant>,
    /// F3 rising-edge latch: true while NDJSON consumption is paused because
    /// the retained ILP buffer is over cap (one error! per episode, not per wake).
    ilp_backpressure_active: bool,
    /// PR-5 H-3: while draining a PREVIOUS day's archive tail at boot, the
    /// offset snapshot must be stamped with the ARCHIVE's day (not today's) so
    /// a crash mid-drain resumes the SAME archive from the flushed offset
    /// instead of orphaning its tail.
    active_drain_ist_day: Option<i64>,
    /// Max exchange timestamp (ts_ist_nanos) ever seen across parsed lines this
    /// bridge lifetime (2026-07-03 frozen-snapshot masking fix). Feed LIVENESS
    /// is stamped ONLY when this ADVANCES: a feed re-dumping a frozen snapshot
    /// (same exchange ts, same or changing price — the 2026-07-03 09:00:00.183
    /// flood) must NOT count as alive, so the FEED-STALL-01 stall watchdog
    /// (`should_restart_on_stall` via `last_tick_age_secs`) can fire instead of
    /// being masked by duplicate re-dumps.
    liveness_max_exchange_ts_nanos: i64,
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
            last_offset_persist: None,
            ilp_backpressure_active: false,
            active_drain_ist_day: None,
            liveness_max_exchange_ts_nanos: 0,
        }
    }

    /// Read the first [`GROWW_OFFSET_HEAD_LEN`] bytes of the capture file (the
    /// identity guard), lossy-decoded. Empty on any failure (fail-safe: an
    /// empty head never matches a snapshot, forcing the byte-0 re-tail).
    async fn read_file_head(tick_file_path: &Path) -> String {
        let Ok(mut f) = File::open(tick_file_path).await else {
            return String::new();
        };
        let mut buf = [0u8; GROWW_OFFSET_HEAD_LEN];
        let Ok(n) = f.read(&mut buf).await else {
            return String::new();
        };
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }

    /// PR-3 (2026-07-02): resume the read position from the persisted
    /// flushed-offset snapshot when it provably belongs to the current file
    /// (pure decision: [`resume_from_snapshot`]). Called ONCE at bridge start;
    /// any mismatch falls back to the byte-0 re-tail (DEDUP-idempotent —
    /// exactly the pre-PR-3 behavior).
    // TEST-EXEMPT: async file I/O wrapper; the pure resume decision (resume_from_snapshot) + snapshot round-trip are unit-tested.
    pub(crate) async fn try_resume_from_snapshot(
        &mut self,
        tick_file_path: &Path,
        feed_health: &tickvault_common::feed_health::FeedHealthRegistry,
    ) {
        let snap_path = offset_snapshot_path(tick_file_path);
        let Ok(raw) = tokio::fs::read_to_string(&snap_path).await else {
            info!("groww bridge: no offset snapshot — starting from byte 0");
            metrics::counter!("tv_groww_bridge_offset_resume_total", "outcome" => "no_snapshot")
                .increment(1);
            return;
        };
        let Ok(snap) = serde_json::from_str::<GrowwOffsetSnapshot>(&raw) else {
            warn!("groww bridge: offset snapshot unparseable — byte-0 re-tail (fail-safe)");
            metrics::counter!("tv_groww_bridge_offset_resume_total", "outcome" => "corrupt")
                .increment(1);
            return;
        };
        let today_ist_day = ist_day_from_ist_nanos(receipt_ist_nanos());
        let current_len = match tokio::fs::metadata(tick_file_path).await {
            Ok(m) => m.len(),
            Err(_) => {
                // B2 fix 2 (2026-07-03, hostile-review H-3 completion): the
                // live capture file being ABSENT at bridge start (IST-midnight
                // rotation happened; the sidecar first-writes today's file
                // later — e.g. bridge boots 08:31, ticks start 09:00) must NOT
                // skip a provable previous-day archive tail. Before this fix,
                // the early return here silently orphaned those bytes until
                // the archive's 2-day retention wiped them. Drain the archive
                // through the SAME parse/validate/persist path, then proceed
                // exactly as before: byte-0 tail once today's file appears.
                self.drain_archive_tail_if_needed(
                    tick_file_path,
                    &snap,
                    today_ist_day,
                    feed_health,
                )
                .await;
                self.offset = 0;
                info!("groww bridge: capture file absent — snapshot ignored");
                metrics::counter!("tv_groww_bridge_offset_resume_total", "outcome" => "no_file")
                    .increment(1);
                return;
            }
        };
        let current_head = Self::read_file_head(tick_file_path).await;
        match resume_from_snapshot(&snap, current_len, &current_head, today_ist_day) {
            Some((offset, capture_seq)) => {
                self.offset = offset;
                self.capture_seq.store(capture_seq, Ordering::Relaxed);
                info!(
                    offset,
                    capture_seq,
                    file_len = current_len,
                    "groww bridge: resumed from persisted flushed-offset — no full re-tail"
                );
                metrics::counter!("tv_groww_bridge_offset_resume_total", "outcome" => "resumed")
                    .increment(1);
            }
            None => {
                self.drain_archive_tail_if_needed(
                    tick_file_path,
                    &snap,
                    today_ist_day,
                    feed_health,
                )
                .await;
                self.offset = 0;
                info!(
                    snapshot_offset = snap.offset,
                    file_len = current_len,
                    "groww bridge: snapshot does not match the current capture file \
                     (rotated/replaced) — byte-0 re-tail (DEDUP-idempotent)"
                );
                metrics::counter!("tv_groww_bridge_offset_resume_total", "outcome" => "identity_mismatch")
                    .increment(1);
            }
        }
    }

    /// PR-5 H-3 (hostile F4) + B2 fix 2 (2026-07-03): if the snapshot belongs
    /// to a PREVIOUS IST day, the sidecar rotated the live file at midnight and
    /// any bytes appended after our last flush were ARCHIVED unread (deleted
    /// after 2 days). Drain that archive's tail ONCE through the SAME
    /// parse/validate/persist path before the byte-0 re-tail of the fresh live
    /// file. Bounded + non-fatal: an absent/short archive is a no-op (the pure
    /// gate is [`should_drain_archive`]). Extracted so BOTH resume arms reach
    /// it — the identity-mismatch arm (live file exists but rotated) AND the
    /// absent-live-file arm (rotation happened, today's file not yet created).
    /// Leaves the state ready for a byte-0 live-file tail: the CALLER resets
    /// `self.offset = 0` (this fn leaves it at the drained archive length);
    /// `active_drain_ist_day` + `residual` are cleared here after the drain.
    // TEST-EXEMPT: async file I/O wrapper around the pure should_drain_archive
    // gate; executed end-to-end by test_retail_cross_day_snapshot_drains_archive_
    // then_byte0_retail + test_retail_cross_day_archive_drains_even_without_
    // today_live_file (+ the multi-wake chunking test).
    async fn drain_archive_tail_if_needed(
        &mut self,
        tick_file_path: &Path,
        snap: &GrowwOffsetSnapshot,
        today_ist_day: i64,
        feed_health: &tickvault_common::feed_health::FeedHealthRegistry,
    ) {
        let archive = archive_path_for_ist_day(tick_file_path, snap.ist_day);
        let archive_len = tokio::fs::metadata(&archive).await.ok().map(|m| m.len());
        if !should_drain_archive(snap.ist_day, today_ist_day, archive_len, snap.offset) {
            return;
        }
        let len = archive_len.unwrap_or(0);
        info!(
            archive = %archive.display(),
            from_offset = snap.offset,
            archive_len = len,
            "groww bridge: draining rotated archive tail (bytes flushed before \
             the downtime resume from the flushed offset; DEDUP-idempotent)"
        );
        metrics::counter!("tv_groww_bridge_archive_tail_drains_total").increment(1);
        self.offset = snap.offset;
        self.capture_seq.store(snap.capture_seq, Ordering::Relaxed);
        self.active_drain_ist_day = Some(snap.ist_day);
        let mut wakes: u32 = 0;
        while self.offset < len && wakes < GROWW_ARCHIVE_DRAIN_MAX_WAKES {
            let before = self.offset;
            self.drain_new_data(&archive, feed_health).await;
            wakes = wakes.saturating_add(1);
            if self.offset <= before {
                break; // no forward progress — read error / EOF
            }
        }
        if wakes >= GROWW_ARCHIVE_DRAIN_MAX_WAKES {
            warn!(
                archive = %archive.display(),
                drained_to = self.offset,
                archive_len = len,
                "groww bridge: archive tail drain hit the wake cap — proceeding \
                 to the live file (remaining archive rows stay on disk)"
            );
        }
        self.active_drain_ist_day = None;
        self.residual.clear();
    }

    /// PR-3 (2026-07-02): persist the FLUSHED-THROUGH offset snapshot —
    /// called ONLY from the flush-`Ok` arm (source-scan-ratcheted ordering:
    /// the offset on disk is never ahead of rows persisted to QuestDB, so a
    /// crash replays only the unflushed tail). Throttled to one write per
    /// [`GROWW_OFFSET_PERSIST_MIN_SECS`]; atomic tmp+rename; best-effort (a
    /// write failure only means a longer re-tail after the next restart).
    async fn maybe_persist_offset(&mut self, tick_file_path: &Path) {
        if let Some(last) = self.last_offset_persist
            && last.elapsed().as_secs() < GROWW_OFFSET_PERSIST_MIN_SECS
        {
            return;
        }
        self.last_offset_persist = Some(std::time::Instant::now());
        let head = Self::read_file_head(tick_file_path).await;
        if head.is_empty() {
            return; // nothing durable to key the identity on
        }
        let file_len = tokio::fs::metadata(tick_file_path)
            .await
            .map(|m| m.len())
            .unwrap_or(self.offset);
        if file_len < self.offset {
            // F2 (2026-07-02): the file at this PATH is shorter than the
            // offset we drained — the sidecar rotated it between our read and
            // this persist. Re-reading head+len by path would pair the NEW
            // file's head with the OLD file's offset (a poisoned snapshot that
            // could silently skip the new file's prefix). Skip this persist;
            // the next flush writes a consistent snapshot for the new file.
            return;
        }
        // B2 fix 1 (2026-07-03, hostile-review VERIFIED): persist the offset of
        // the last COMPLETE line, not the raw read cursor. `self.offset` counts
        // every byte READ — including a torn trailing partial line whose bytes
        // live only in the RAM `residual`. Persisting the raw offset meant a
        // crash right after this write resumed PAST the torn line's start
        // (restart begins with an empty residual), its tail then failed the
        // parse ("skipping malformed tick line") and that line was permanently
        // lost — a bounded 1-line loss window on crash-with-partial-residual.
        // The adjusted offset re-reads exactly the torn bytes on resume and
        // never re-reads a flushed line: `capture_seq` advances only on parsed
        // COMPLETE lines, so the persisted capture_seq already corresponds to
        // this boundary. `head` is file-START bytes (tail-independent) and
        // `file_len` is diagnostic-only (unused by `resume_from_snapshot`) —
        // neither needs adjusting. The F2 rotation guard above deliberately
        // keeps comparing the RAW `self.offset` (the strictest shrink
        // detection). saturating_sub is a defensive belt: residual bytes are
        // always a suffix of the bytes counted in `offset`.
        let snap = GrowwOffsetSnapshot {
            offset: self.offset.saturating_sub(self.residual.len() as u64),
            capture_seq: self.capture_seq.load(Ordering::Relaxed),
            file_len,
            head,
            ist_day: self
                .active_drain_ist_day
                .unwrap_or_else(|| ist_day_from_ist_nanos(receipt_ist_nanos())),
        };
        let Ok(json) = serde_json::to_string(&snap) else {
            return;
        };
        let path = offset_snapshot_path(tick_file_path);
        let tmp = path.with_extension("json.tmp");
        if tokio::fs::write(&tmp, &json).await.is_ok()
            && let Err(err) = tokio::fs::rename(&tmp, &path).await
        {
            warn!(
                ?err,
                "groww bridge: offset snapshot rename failed (best-effort)"
            );
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
        // Bounded chunked read: read at most GROWW_BRIDGE_MAX_READ_BYTES this wake
        // — never the whole (possibly multi-GB) file at once. F3 (2026-07-02):
        // budget is 0 while the retained ILP buffer is over cap (sustained
        // QuestDB outage) — consumption pauses (offset NOT advanced; the
        // capture file is the durable spill tier) while the flush arm below
        // keeps retrying the bounded buffer. Rising-edge error! only.
        let to_read = wake_read_budget(self.live_writer.buffer_byte_count(), len, self.offset);
        if to_read == 0 && !self.ilp_backpressure_active {
            self.ilp_backpressure_active = true;
            metrics::counter!("tv_groww_ilp_buffer_backpressure_total").increment(1);
            error!(
                buffered_bytes = self.live_writer.buffer_byte_count(),
                "groww bridge: ILP buffer over cap — pausing NDJSON consumption until QuestDB recovers (capture file holds the tail; zero loss)"
            );
        }
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
        // Max exchange ts across the lines parsed THIS wake — feeds the
        // liveness-advance gate below (2026-07-03 frozen-snapshot fix).
        let mut wake_max_exchange_ts_nanos: i64 = 0;
        let mut parsed_any = false;
        // Drain only COMPLETE newline-terminated lines; a trailing partial line
        // (incl. a partial UTF-8 char from the chunk boundary) stays buffered.
        let prefix = drain_complete_prefix(&mut self.residual);
        // PR-4: one clock read per WAKE (not per line — O(1) preserved) for
        // the relative future-timestamp clamp in validate_groww_tick.
        let wake_receipt_ist_nanos = receipt_ist_nanos();
        // Fix #3 (2026-07-03 lag forensics): the receipt stamp persisted on
        // every row this wake — plausibility-gated so a broken clock writes
        // NULL. Makes per-feed lag (received_at − ts) measurable in SQL.
        let wake_received_at = row_received_at(wake_receipt_ist_nanos);
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
            if let Err(reason) = validate_groww_tick(&parsed, wake_receipt_ist_nanos) {
                error!(
                    ?reason,
                    security_id = parsed.tick.security_id,
                    "groww bridge: rejecting invalid tick before persist"
                );
                continue;
            }
            parsed_any = true;
            wake_max_exchange_ts_nanos = wake_max_exchange_ts_nanos.max(parsed.tick.ts_ist_nanos);
            let seq = next_capture_seq(&self.capture_seq, parsed.tick.ts_ist_nanos);
            if let Err(err) =
                self.live_writer
                    .append_row(&live_tick_row(&parsed, seq, wake_received_at))
            {
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
        // PARSE-TIME LIVENESS (2026-07-02 adversarial-sweep fix): lines were
        // parsed from the sidecar NDJSON this wake — the FEED delivered,
        // regardless of what QuestDB does next. Stamp liveness BEFORE the flush
        // so the sidecar stall-watchdog (`should_restart_on_stall`, which reads
        // `last_tick_age_secs`) never mistakes a QuestDB outage for a dead
        // socket and kills the healthy Python sidecar in a restart storm. Tick
        // COUNTS stay persist-honest via record_ticks in the flush arm below.
        // TIMESTAMP-ADVANCING GATE (2026-07-03 live incident): stamp liveness
        // ONLY when the max exchange ts across this wake's parsed lines ADVANCES
        // past the max ever seen. The 09:00–09:17 IST flood (530K duplicate rows
        // all frozen at 09:00:00.183) kept the old unconditional stamp fresh, so
        // the FEED-STALL-01 watchdog saw a "live" feed while Groww delivered
        // nothing new — a frozen snapshot re-dump must NOT count as alive. The
        // stamped VALUE stays the wall-clock receipt time (consistent with the
        // /api/feeds/health age math); a healthy feed advances tsInMillis every
        // second across the universe, so this gate never starves on real data.
        if last_receipt_ist_nanos != 0
            && liveness_ts_advanced(
                &mut self.liveness_max_exchange_ts_nanos,
                wake_max_exchange_ts_nanos,
            )
        {
            feed_health.record_feed_liveness(Feed::Groww, last_receipt_ist_nanos);
        }
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
                    if self.ilp_backpressure_active {
                        self.ilp_backpressure_active = false;
                        info!(
                            persisted = outcome.persisted,
                            "groww bridge: ILP backpressure released — resuming NDJSON consumption"
                        );
                    }
                    feed_health.record_ticks(Feed::Groww, outcome.persisted as u64, ts);
                    // PR-3: persist the FLUSHED-THROUGH offset (ordering
                    // ratcheted — never ahead of rows persisted to QuestDB).
                    self.maybe_persist_offset(tick_file_path).await;
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

/// Persisted bridge read-position (PR-3, 2026-07-02): written AFTER each
/// successful flush (never ahead of persisted rows), read once at bridge
/// start so a process restart resumes the tail instead of re-reading the
/// whole capture file from byte 0. `head` is the file's first bytes — the
/// identity guard: a rotated/replaced file has a different first line, so a
/// stale snapshot can never resume into the middle of a DIFFERENT file
/// (fail-safe = byte-0 re-tail, which is DEDUP-idempotent).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GrowwOffsetSnapshot {
    /// Flushed-through byte offset into the capture file.
    pub offset: u64,
    /// The capture_seq counter value at the flush (restored on resume so the
    /// dedup tiebreaker stays monotonic across the restart).
    pub capture_seq: i64,
    /// Capture-file length at the flush (diagnostic only).
    pub file_len: u64,
    /// First bytes of the capture file at the flush (identity guard).
    pub head: String,
    /// IST day index (IST nanos / 86_400e9) at the flush (F1 hostile finding
    /// 2026-07-02): the sidecar rotates the capture file at IST midnight, so a
    /// snapshot from a PREVIOUS IST day can never belong to today's live file —
    /// rejected outright even when the head coincidentally matches.
    /// `#[serde(default)]` (0) makes pre-F1 snapshots fail the day check →
    /// safe byte-0 re-tail.
    #[serde(default)]
    pub ist_day: i64,
}

/// How many leading bytes of the capture file form the identity guard.
/// 256 (was 64 — F1 hostile finding 2026-07-02): with a 19-digit bit-62 Groww
/// index token, 64 bytes ended BEFORE the first timestamp digit, so two
/// different days' files could share an identical head when the same
/// instrument ticked first each day. 256 always reaches past `ts_ist_nanos`
/// (day-varying content) for any line shape.
pub const GROWW_OFFSET_HEAD_LEN: usize = 256;

/// Minimum seconds between offset-snapshot writes (cold-path throttle).
pub const GROWW_OFFSET_PERSIST_MIN_SECS: u64 = 5;

/// Sibling path of the capture file where the offset snapshot lives.
#[must_use]
pub fn offset_snapshot_path(tick_file_path: &Path) -> PathBuf {
    tick_file_path
        .parent()
        .map(|d| d.join("bridge-offset.json"))
        .unwrap_or_else(|| PathBuf::from("bridge-offset.json"))
}

/// Pure resume decision: `Some((offset, capture_seq))` ONLY when the snapshot
/// provably belongs to the CURRENT file — its head matches and its offset is
/// within the current length (the file only grew since the flush). Any
/// mismatch (rotation, truncation, corruption, empty head) → `None` →
/// fail-safe byte-0 re-tail.
#[must_use]
pub fn resume_from_snapshot(
    snap: &GrowwOffsetSnapshot,
    current_len: u64,
    current_head: &str,
    today_ist_day: i64,
) -> Option<(u64, i64)> {
    if snap.ist_day != today_ist_day {
        // F1 (2026-07-02): rotation happens at IST midnight — a snapshot from
        // a previous IST day can NEVER belong to today's live file, even when
        // the head coincidentally matches (same first instrument every day).
        return None;
    }
    if snap.head.is_empty() || snap.head != current_head {
        return None; // rotated / replaced / unreadable — identity broken
    }
    if snap.offset > current_len {
        return None; // shrank below the flushed point — rotated/truncated
    }
    Some((snap.offset, snap.capture_seq))
}

/// IST day index (days since epoch in IST wall-clock) for an IST-nanos
/// instant — the same day boundary the sidecar's `_ist_day` rotation uses.
#[must_use]
pub fn ist_day_from_ist_nanos(ist_nanos: i64) -> i64 {
    ist_nanos.div_euclid(86_400 * 1_000_000_000)
}

/// Max archive-drain iterations (× 4 MiB chunks ⇒ ≤16 GiB) — a hard bound so
/// a pathological archive can never wedge boot (PR-5 H-3, hostile F4).
pub const GROWW_ARCHIVE_DRAIN_MAX_WAKES: u32 = 4096;

/// The dated archive the sidecar leaves at IST-midnight rotation for a
/// COMPLETED IST day index: `live-ticks-YYYYMMDD.ndjson`, sibling of the live
/// capture file (mirrors the sidecar's `_archive_path`).
#[must_use]
pub fn archive_path_for_ist_day(tick_file_path: &Path, ist_day: i64) -> PathBuf {
    let date = chrono::DateTime::<chrono::Utc>::from_timestamp(ist_day.saturating_mul(86_400), 0)
        .map(|d| d.format("%Y%m%d").to_string())
        .unwrap_or_default();
    let name = format!("live-ticks-{date}.ndjson");
    tick_file_path
        .parent()
        .map(|d| d.join(&name))
        .unwrap_or_else(|| PathBuf::from(name))
}

/// Pure drain decision (PR-5 H-3): drain a PREVIOUS day's archive tail ONLY
/// when the snapshot provably belongs to an earlier IST day (a legacy
/// `ist_day == 0` snapshot is NOT trusted to name an archive), the archive
/// exists, and it holds bytes beyond the flushed offset. Everything else →
/// exact prior behavior (byte-0 re-tail of the live file).
#[must_use]
pub fn should_drain_archive(
    snap_ist_day: i64,
    today_ist_day: i64,
    archive_len: Option<u64>,
    snap_offset: u64,
) -> bool {
    snap_ist_day > 0
        && snap_ist_day < today_ist_day
        && archive_len.is_some_and(|len| len > snap_offset)
}

/// ws_event_audit edge latches shared between the supervisor wrapper and the
/// (respawnable) bridge task, so a bridge panic never resets the forensic
/// Connected/Disconnected/Reconnected chain (2026-07-02 adversarial finding M3).
#[derive(Debug, Default)]
pub struct GrowwAuditLatches {
    /// A Connected/Reconnected row has been emitted for the current streaming
    /// episode (rising-edge gate).
    pub audited_connected: std::sync::atomic::AtomicBool,
    /// A Disconnected row was emitted since the last connect — the next rising
    /// edge classifies as Reconnected, not a second Connected.
    pub prior_disconnect: std::sync::atomic::AtomicBool,
}

/// Pure: the next consecutive-respawn count given how long the previous bridge
/// incarnation ran. A HEALTHY long run (>= `HEALTHY_RUN_RESET_SECS`) resets the
/// curve to 1 so one panic per day never converges to the permanent 60s
/// ceiling (2026-07-02 adversarial finding M2); rapid deaths keep climbing.
#[must_use]
pub fn next_respawn_count(prev_count: u32, ran_secs: u64) -> u32 {
    if ran_secs >= HEALTHY_RUN_RESET_SECS {
        1
    } else {
        prev_count.saturating_add(1)
    }
}

/// A bridge incarnation that survived this long ran healthy — reset the
/// respawn-backoff curve (5 minutes ≫ the 60s backoff ceiling).
pub const HEALTHY_RUN_RESET_SECS: u64 = 300;

/// Backoff (seconds) before the Nth consecutive bridge respawn: 5s doubling to
/// a 60s cap. Pure — mirrors the sidecar supervisor's restart curve so a
/// persistently-panicking bridge never tight-loops, yet a one-off panic
/// self-heals within seconds.
#[must_use]
pub fn bridge_respawn_backoff(consecutive_respawns: u32) -> std::time::Duration {
    const BASE_SECS: u64 = 5;
    const CAP_SECS: u64 = 60;
    let exp = consecutive_respawns.saturating_sub(1).min(6); // 5 * 2^6 > cap
    std::time::Duration::from_secs((BASE_SECS << exp).min(CAP_SECS))
}

/// Spawn the Groww bridge UNDER A SUPERVISOR (2026-07-02 adversarial-sweep fix
/// — WS-GAP-05 / DISK-WATCHER-01 / FEED-SUPERVISOR-01 pattern). Before this,
/// `run_groww_bridge` was a bare `tokio::spawn`: a panic anywhere in the bridge
/// silently killed the ONLY consumer of the sidecar NDJSON — ticks kept
/// appending to disk while nothing persisted and no candle sealed, and the only
/// eventual signal was the stall watchdog killing the WRONG process (the
/// healthy Python sidecar, in a loop).
///
/// The wrapper creates the Groww aggregator ONCE and spawns the IST-midnight
/// force-seal ONCE (both hoisted out of the respawned body, so respawns never
/// leak duplicate force-seal tasks and in-memory bars survive a panic), then
/// loops: spawn the bridge → await its JoinHandle → on ANY exit (panic or
/// unexpected clean return) log `error!(code = FEED-SUPERVISOR-01)`, increment
/// `tv_feed_supervisor_respawn_total{feed="groww", component="bridge"}`, back
/// off ([`bridge_respawn_backoff`]) and respawn. A respawned bridge re-tails
/// the NDJSON from byte 0 — residual-neutral: the deterministic `capture_seq`
/// regenerates identical DEDUP keys, so QuestDB collapses the replay.
// TEST-EXEMPT: supervision loop (spawn/await/sleep); the pure backoff curve is unit-tested (test_bridge_respawn_backoff_curve) and the wiring is pinned by per_feed_boot_isolation_guard + test_spawn_supervised_groww_bridge_owns_aggregator_and_force_seal_once.
pub fn spawn_supervised_groww_bridge(
    qdb: QuestDbConfig,
    tick_file_path: PathBuf,
    status_file_path: PathBuf,
    feed_runtime: Arc<FeedRuntimeState>,
    feed_health: Arc<tickvault_common::feed_health::FeedHealthRegistry>,
    ws_audit_tx: Option<tokio::sync::mpsc::Sender<WsEventAuditRow>>,
    trading_calendar: Arc<tickvault_common::trading_calendar::TradingCalendar>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Process-lifetime state: ONE aggregator (shared papaya map) + ONE
        // IST-midnight force-seal task, regardless of how often the inner
        // bridge task respawns.
        let aggregator = MultiTfAggregator::with_capacity(GROWW_AGGREGATOR_CAPACITY);
        spawn_groww_ist_midnight_force_seal(
            aggregator.clone(),
            Arc::clone(&feed_runtime),
            trading_calendar,
        );
        // Process-lifetime ws_event_audit edge latches (finding M3): survive a
        // bridge respawn so the forensic chain stays Connected → Disconnected →
        // Reconnected, never Connected → Connected.
        let audit_latches = Arc::new(GrowwAuditLatches::default());
        let mut consecutive_respawns: u32 = 0;
        loop {
            let started = std::time::Instant::now();
            let handle = tokio::spawn(run_groww_bridge(
                qdb.clone(),
                tick_file_path.clone(),
                status_file_path.clone(),
                Arc::clone(&feed_runtime),
                Arc::clone(&feed_health),
                ws_audit_tx.clone(),
                aggregator.clone(),
                Arc::clone(&audit_latches),
            ));
            let reason = match handle.await {
                Err(err) if err.is_panic() => "panic",
                // A non-panic JoinError = the inner task was CANCELLED — the
                // runtime is shutting down (finding M2). Do NOT respawn onto a
                // dying runtime and do NOT page a spurious FEED-SUPERVISOR-01;
                // exit the supervisor cleanly (mirrors the sidecar wrapper).
                Err(_) => {
                    info!(
                        "groww bridge supervisor: inner task cancelled (runtime \
                         shutdown) — supervisor exiting cleanly"
                    );
                    return;
                }
                Ok(()) => "clean_exit",
            };
            // If the previous incarnation ran healthy for a while, reset the
            // backoff curve — one panic per day must never converge to the
            // permanent 60s ceiling (finding M2).
            consecutive_respawns =
                next_respawn_count(consecutive_respawns, started.elapsed().as_secs());
            let backoff = bridge_respawn_backoff(consecutive_respawns);
            // Forensic falling edge (finding M3): a panic unwound the bridge
            // before it could emit its Disconnected row — emit it HERE so the
            // audit chain never reads Connected → Connected. The respawned
            // bridge's first streaming rising edge then classifies Reconnected.
            if audit_latches
                .audited_connected
                .swap(false, std::sync::atomic::Ordering::Relaxed)
            {
                emit_groww_ws_audit(
                    ws_audit_tx.as_ref(),
                    WsEventKind::Disconnected,
                    "bridge_died",
                    "groww bridge task died — respawning",
                );
                audit_latches
                    .prior_disconnect
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            error!(
                code =
                    tickvault_common::error_code::ErrorCode::FeedSupervisor01Respawned.code_str(),
                reason,
                consecutive_respawns,
                backoff_secs = backoff.as_secs(),
                "FEED-SUPERVISOR-01: groww bridge task died — respawning (the \
                 NDJSON re-tail is DEDUP-idempotent; in-memory bars survive on \
                 the shared aggregator)"
            );
            metrics::counter!(
                "tv_feed_supervisor_respawn_total",
                "feed" => "groww",
                "component" => "bridge"
            )
            .increment(1);
            tokio::time::sleep(backoff).await;
        }
    })
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
    // The process-lifetime Groww aggregator, created ONCE by the supervisor
    // wrapper — a respawned bridge reuses it (bars survive a panic) and the
    // IST-midnight force-seal (spawned once by the wrapper) keeps sealing it.
    aggregator: MultiTfAggregator,
    // Process-lifetime ws_event_audit edge latches (created once by the
    // supervisor wrapper) — survive a bridge respawn so the forensic
    // Connected/Disconnected/Reconnected chain stays consistent (finding M3).
    audit_latches: Arc<GrowwAuditLatches>,
) {
    info!(
        path = %tick_file_path.display(),
        status = %status_file_path.display(),
        "Groww bridge started — EVENT-DRIVEN tail of the sidecar tick file (zero-poll; \
         fallback watchdog only if the watcher drops). Idles until the sidecar appends."
    );
    // The aggregator is created ONCE by `spawn_supervised_groww_bridge` (which
    // also spawns the IST-midnight force-seal exactly once) and passed in —
    // so a supervised RESPAWN of this task reuses the same shared papaya map
    // (in-memory bars survive a bridge panic) and never leaks a duplicate
    // force-seal task. Parity with the Dhan IST-midnight force-seal (`main.rs`).
    let mut state = GrowwBridgeState::with_aggregator(&qdb, aggregator);
    // PR-3 (2026-07-02): resume from the persisted flushed-offset when it
    // provably belongs to the current capture file — otherwise byte-0 re-tail
    // (DEDUP-idempotent, the pre-PR-3 behavior).
    state
        .try_resume_from_snapshot(&tick_file_path, &feed_health)
        .await;

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
    // SHARED with the supervisor wrapper (2026-07-02 adversarial finding M3): the
    // latches live in `audit_latches` (created once per process) so a bridge PANIC
    // does not reset them — the wrapper emits the missing Disconnected row on death
    // and the respawned bridge's first rising edge classifies as RECONNECTED, so the
    // forensic chain never reads Connected→Connected with no disconnect between.

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
            if audit_latches
                .audited_connected
                .load(std::sync::atomic::Ordering::Relaxed)
            {
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
                audit_latches
                    .prior_disconnect
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            audit_latches
                .audited_connected
                .store(false, std::sync::atomic::Ordering::Relaxed);
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
        if streaming_observed
            && !audit_latches
                .audited_connected
                .load(std::sync::atomic::Ordering::Relaxed)
        {
            let (kind, source) = if audit_latches
                .prior_disconnect
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                (WsEventKind::Reconnected, "groww_resumed")
            } else {
                (WsEventKind::Connected, "groww_sidecar")
            };
            emit_groww_ws_audit(ws_audit_tx.as_ref(), kind, source, "groww streaming");
            audit_latches
                .audited_connected
                .store(true, std::sync::atomic::Ordering::Relaxed);
            audit_latches
                .prior_disconnect
                .store(false, std::sync::atomic::Ordering::Relaxed);
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
        assert_eq!(validate_groww_tick(&valid_parsed(), 0), Ok(()));
    }

    #[test]
    fn test_validate_groww_tick_rejects_non_finite_ltp() {
        let mut p = valid_parsed();
        p.tick.ltp = f64::NAN;
        assert_eq!(
            validate_groww_tick(&p, 0),
            Err(GrowwTickReject::NonFinitePrice)
        );
        p.tick.ltp = f64::INFINITY;
        assert_eq!(
            validate_groww_tick(&p, 0),
            Err(GrowwTickReject::NonFinitePrice)
        );
    }

    #[test]
    fn test_validate_groww_tick_rejects_non_positive_ltp() {
        let mut p = valid_parsed();
        p.tick.ltp = 0.0;
        assert_eq!(
            validate_groww_tick(&p, 0),
            Err(GrowwTickReject::NonPositivePrice)
        );
        p.tick.ltp = -1.5;
        assert_eq!(
            validate_groww_tick(&p, 0),
            Err(GrowwTickReject::NonPositivePrice)
        );
    }

    #[test]
    fn test_validate_groww_tick_rejects_implausible_ltp() {
        let mut p = valid_parsed();
        p.tick.ltp = f64::from(tickvault_common::constants::MAX_PLAUSIBLE_LTP) + 1.0;
        assert_eq!(
            validate_groww_tick(&p, 0),
            Err(GrowwTickReject::ImplausiblePrice)
        );
    }

    #[test]
    fn test_validate_groww_tick_rejects_negative_volume() {
        let mut p = valid_parsed();
        p.tick.cum_volume = -1;
        assert_eq!(
            validate_groww_tick(&p, 0),
            Err(GrowwTickReject::NegativeVolume)
        );
    }

    #[test]
    fn test_validate_groww_tick_rejects_implausible_volume() {
        let mut p = valid_parsed();
        p.tick.cum_volume = MAX_PLAUSIBLE_VOLUME + 1;
        assert_eq!(
            validate_groww_tick(&p, 0),
            Err(GrowwTickReject::ImplausibleVolume)
        );
        // Boundary is inclusive-valid.
        p.tick.cum_volume = MAX_PLAUSIBLE_VOLUME;
        assert_eq!(validate_groww_tick(&p, 0), Ok(()));
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
            validate_groww_tick(&p, 0),
            Err(GrowwTickReject::NonPositiveSecurityId)
        );
        p.tick.security_id = -7;
        assert_eq!(
            validate_groww_tick(&p, 0),
            Err(GrowwTickReject::NonPositiveSecurityId)
        );
    }

    #[test]
    fn test_validate_groww_tick_rejects_timestamp_out_of_range() {
        let mut p = valid_parsed();
        p.tick.ts_ist_nanos = 0;
        assert_eq!(
            validate_groww_tick(&p, 0),
            Err(GrowwTickReject::TimestampOutOfRange)
        );
        p.tick.ts_ist_nanos = -1;
        assert_eq!(
            validate_groww_tick(&p, 0),
            Err(GrowwTickReject::TimestampOutOfRange)
        );
        p.tick.ts_ist_nanos = i64::MAX;
        assert_eq!(
            validate_groww_tick(&p, 0),
            Err(GrowwTickReject::TimestampOutOfRange)
        );
        // Boundaries are inclusive-valid.
        p.tick.ts_ist_nanos = MIN_PLAUSIBLE_TS_IST_NANOS;
        assert_eq!(validate_groww_tick(&p, 0), Ok(()));
        p.tick.ts_ist_nanos = MAX_PLAUSIBLE_TS_IST_NANOS;
        assert_eq!(validate_groww_tick(&p, 0), Ok(()));
    }

    #[test]
    fn test_live_tick_row_maps_all_fields() {
        let p = parse_groww_tick_line(LINE).expect("parse");
        let receipt = Some(1_780_000_020_500_000_000);
        let row = live_tick_row(&p, 42, receipt);
        assert_eq!(row.security_id, 1333);
        assert_eq!(row.segment, "NSE_EQ");
        assert_eq!(row.ltp, 2847.55);
        assert_eq!(row.volume, 123_456);
        assert_eq!(row.exchange_ts_millis, 1_780_000_020_123);
        assert_eq!(row.capture_seq, 42);
        assert_eq!(row.ts_ist_nanos, 1_780_000_020_123_000_000);
        // Fix #3 (2026-07-03 lag forensics): the per-wake receipt stamp is
        // carried onto the row so received_at is no longer NULL for groww.
        assert_eq!(row.received_at_ist_nanos, receipt);
    }

    #[test]
    fn test_live_tick_row_received_at_gated_by_plausibility() {
        // A broken clock (receipt saturates to ~1970 = IST offset) must yield
        // None → the shared builder OMITS the column → NULL, never a garbage
        // 1970 received_at stamp.
        assert_eq!(row_received_at(0), None);
        assert_eq!(
            row_received_at(tickvault_common::constants::IST_UTC_OFFSET_NANOS),
            None
        );
        assert_eq!(
            row_received_at(GROWW_MIN_PLAUSIBLE_RECEIPT_IST_NANOS),
            None,
            "boundary is exclusive — exactly the floor is still implausible"
        );
        let plausible = GROWW_MIN_PLAUSIBLE_RECEIPT_IST_NANOS + 1;
        assert_eq!(row_received_at(plausible), Some(plausible));
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
        // Window widened (PR-10, then 2026-07-02 M3 atomic-latch expansion) to
        // span the ws_event_audit falling-edge emit block — the latch re-arm
        // lives just below it.
        let after = &src[disable_idx..disable_idx + 2600];
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

    // ── 2026-07-02 adversarial-sweep fixes: bridge supervision + parse-time
    // liveness (the two HIGHs) ──

    #[test]
    fn test_bridge_respawn_backoff_curve() {
        // 5s doubling to a 60s cap — a one-off panic self-heals in seconds, a
        // persistent panic never tight-loops.
        assert_eq!(bridge_respawn_backoff(1).as_secs(), 5);
        assert_eq!(bridge_respawn_backoff(2).as_secs(), 10);
        assert_eq!(bridge_respawn_backoff(3).as_secs(), 20);
        assert_eq!(bridge_respawn_backoff(4).as_secs(), 40);
        assert_eq!(bridge_respawn_backoff(5).as_secs(), 60, "cap");
        assert_eq!(bridge_respawn_backoff(50).as_secs(), 60, "stays capped");
        assert_eq!(
            bridge_respawn_backoff(u32::MAX).as_secs(),
            60,
            "no overflow"
        );
        // Defensive: a 0th call (before any respawn) still yields the base.
        assert_eq!(bridge_respawn_backoff(0).as_secs(), 5);
    }

    #[test]
    fn test_validate_rejects_future_timestamp() {
        // PR-4: a tick stamped ahead of the receipt clock beyond the 60s skew
        // tolerance is rejected; at/below tolerance passes; a broken clock
        // (receipt=0) fails OPEN to the static bounds (never mass-rejects).
        let mut p = valid_parsed();
        let now = p.tick.ts_ist_nanos; // pretend receipt == the tick's stamp
        assert_eq!(validate_groww_tick(&p, now), Ok(()), "same-instant passes");
        p.tick.ts_ist_nanos = now + 59 * 1_000_000_000;
        assert_eq!(
            validate_groww_tick(&p, now),
            Ok(()),
            "within tolerance passes"
        );
        p.tick.ts_ist_nanos = now + 61 * 1_000_000_000;
        assert_eq!(
            validate_groww_tick(&p, now),
            Err(GrowwTickReject::FutureTimestamp),
            "beyond tolerance rejected"
        );
        // 24h future (the candle-poison case) definitely rejected.
        p.tick.ts_ist_nanos = now + 24 * 3600 * 1_000_000_000;
        assert_eq!(
            validate_groww_tick(&p, now),
            Err(GrowwTickReject::FutureTimestamp)
        );
        // Broken clock: fail-open (static bounds only).
        assert_eq!(validate_groww_tick(&p, 0), Ok(()), "receipt=0 fails open");
        // F6: a degenerate clock read yields IST_UTC_OFFSET_NANOS (~1970),
        // NOT 0 — it must ALSO fail open (below the min-plausible receipt),
        // never mass-reject a valid tick as FutureTimestamp.
        p.tick.ts_ist_nanos = now;
        assert_eq!(
            validate_groww_tick(&p, tickvault_common::constants::IST_UTC_OFFSET_NANOS),
            Ok(()),
            "degenerate ~1970 clock fails open"
        );
    }

    #[test]
    fn test_ist_day_from_ist_nanos_day_boundaries() {
        // F1 helper: day index flips exactly at the IST-midnight nanos boundary
        // (same boundary as the sidecar rotation).
        let day_nanos: i64 = 86_400 * 1_000_000_000;
        assert_eq!(ist_day_from_ist_nanos(0), 0);
        assert_eq!(ist_day_from_ist_nanos(day_nanos - 1), 0);
        assert_eq!(ist_day_from_ist_nanos(day_nanos), 1);
        assert_eq!(ist_day_from_ist_nanos(20_636 * day_nanos + 5), 20_636);
        // Negative (pre-epoch, defensive): div_euclid floors, never panics.
        assert_eq!(ist_day_from_ist_nanos(-1), -1);
    }

    #[test]
    fn test_archive_path_for_ist_day_matches_sidecar_rotation() {
        // PR-5 H-3: the archive name must match the sidecar's rotation output
        // (live-ticks-YYYYMMDD.ndjson, sibling of the live capture file).
        // 2026-07-01 in IST-day-index terms: days since epoch for that date.
        let day_20026_07_01 = 20_635; // 20635 * 86400 = 2026-07-01T00:00:00
        let p =
            archive_path_for_ist_day(Path::new("/data/groww/live-ticks.ndjson"), day_20026_07_01);
        assert_eq!(p, PathBuf::from("/data/groww/live-ticks-20260701.ndjson"));
        // Parent-less path degrades to a bare file name (never panics).
        let bare = archive_path_for_ist_day(Path::new("live-ticks.ndjson"), day_20026_07_01);
        assert!(
            bare.to_string_lossy()
                .ends_with("live-ticks-20260701.ndjson")
        );
    }

    #[test]
    fn test_should_drain_archive_decision() {
        // PR-5 H-3 decision matrix — drain ONLY a provable previous-day
        // archive holding bytes beyond the flushed offset.
        let today = 20_636;
        // Previous day, archive longer than offset → drain.
        assert!(should_drain_archive(today - 1, today, Some(500), 100));
        // Archive absent → no drain (retention passed / wiped).
        assert!(!should_drain_archive(today - 1, today, None, 100));
        // Archive shorter/equal (already fully flushed) → no drain.
        assert!(!should_drain_archive(today - 1, today, Some(100), 100));
        assert!(!should_drain_archive(today - 1, today, Some(50), 100));
        // Same-day snapshot → never an archive case.
        assert!(!should_drain_archive(today, today, Some(500), 100));
        // Legacy pre-F1 snapshot (ist_day=0) names no trustworthy archive.
        assert!(!should_drain_archive(0, today, Some(500), 100));
        // Future-day snapshot (clock weirdness) → fail safe, no drain.
        assert!(!should_drain_archive(today + 1, today, Some(500), 100));
    }

    #[test]
    fn test_wake_read_budget_pauses_consumption_over_ilp_cap() {
        // F3: over-cap buffer → 0 budget (pause; capture file is the spill);
        // under cap → bounded chunk as before.
        assert_eq!(wake_read_budget(GROWW_ILP_BUFFER_MAX_BYTES, 1_000, 0), 0);
        assert_eq!(
            wake_read_budget(GROWW_ILP_BUFFER_MAX_BYTES + 1, u64::MAX, 0),
            0
        );
        assert_eq!(wake_read_budget(0, 1_000, 400), 600);
        assert_eq!(
            wake_read_budget(0, u64::MAX, 0),
            GROWW_BRIDGE_MAX_READ_BYTES,
            "chunk stays bounded"
        );
        assert_eq!(wake_read_budget(0, 100, 100), 0, "nothing new");
    }

    #[test]
    fn test_offset_snapshot_roundtrip() {
        // PR-3: the persisted snapshot must serialize/parse losslessly.
        let snap = GrowwOffsetSnapshot {
            offset: 123_456,
            capture_seq: 1_780_000_020_123_000_000,
            file_len: 200_000,
            head: "{\"security_id\":1333".to_string(),
            ist_day: 20_636,
        };
        let json = serde_json::to_string(&snap).expect("serialize");
        let back: GrowwOffsetSnapshot = serde_json::from_str(&json).expect("parse");
        assert_eq!(back, snap);
    }

    #[test]
    fn test_resume_from_snapshot_decision_matrix() {
        // PR-3: resume ONLY when the snapshot provably belongs to the current
        // file — every mismatch is a fail-safe byte-0 re-tail.
        let today = 20_636;
        let snap = GrowwOffsetSnapshot {
            offset: 100,
            capture_seq: 42,
            file_len: 100,
            head: "HEAD".to_string(),
            ist_day: today,
        };
        // Match: same head, same IST day, file grew → resume.
        assert_eq!(
            resume_from_snapshot(&snap, 150, "HEAD", today),
            Some((100, 42))
        );
        // Match: same head, file unchanged → resume (nothing new).
        assert_eq!(
            resume_from_snapshot(&snap, 100, "HEAD", today),
            Some((100, 42))
        );
        // Rotated: fresh file has a different first line → byte-0.
        assert_eq!(resume_from_snapshot(&snap, 150, "OTHER", today), None);
        // Truncated below the flushed point → byte-0.
        assert_eq!(resume_from_snapshot(&snap, 50, "HEAD", today), None);
        // F1: PREVIOUS-day snapshot never resumes — even with a matching head
        // and plausible length (identical first instrument across rotations).
        assert_eq!(
            resume_from_snapshot(&snap, 150, "HEAD", today + 1),
            None,
            "cross-day snapshot must byte-0 re-tail"
        );
        // Pre-F1 snapshot (serde default ist_day=0) never resumes.
        let legacy = GrowwOffsetSnapshot {
            ist_day: 0,
            ..snap.clone()
        };
        assert_eq!(resume_from_snapshot(&legacy, 150, "HEAD", today), None);
        // Empty-head snapshot (unreadable at persist) never resumes.
        let empty = GrowwOffsetSnapshot {
            head: String::new(),
            ..snap
        };
        assert_eq!(resume_from_snapshot(&empty, 150, "", today), None);
    }

    #[test]
    fn test_offset_snapshot_path_is_sibling() {
        assert_eq!(
            offset_snapshot_path(Path::new("data/groww/live-ticks.ndjson")),
            PathBuf::from("data/groww/bridge-offset.json")
        );
    }

    #[test]
    fn test_offset_persist_is_after_flush_ok() {
        // PR-3 ordering ratchet: the offset on disk must NEVER be ahead of
        // rows persisted to QuestDB — the persist call must live INSIDE the
        // flush-Ok arm (after record_ticks), never before the flush.
        let src = include_str!("groww_bridge.rs");
        let flush_idx = src
            .find("self.live_writer.flush()")
            .expect("flush arm exists");
        let persist_idx = src
            .find("self.maybe_persist_offset(")
            .expect("persist call exists");
        assert!(
            persist_idx > flush_idx,
            "maybe_persist_offset must be called AFTER the flush — persisting \
             an offset ahead of the QuestDB rows is a data-loss ordering bug"
        );
        // And it must sit before the flush-Err arm (i.e. inside Ok).
        let err_arm = src[flush_idx..]
            .find("Err(err) => {")
            .map(|i| flush_idx + i)
            .expect("flush Err arm exists");
        assert!(
            persist_idx < err_arm,
            "maybe_persist_offset must be inside the flush-Ok arm"
        );
    }

    #[test]
    fn test_sidecar_rotates_at_ist_midnight() {
        // PR-3 ratchet on the PYTHON writer: rotation must be gated on the IST
        // day boundary (never size-based — a mid-session size rotation could
        // reset capture_seq and collide same-second dedup keys) and must never
        // stop capture on failure.
        let sidecar = include_str!("../../../scripts/groww-sidecar/groww_sidecar.py");
        assert!(
            sidecar.contains("NDJSON_ARCHIVE_KEEP_DAYS = 2"),
            "archive retention constant must be pinned"
        );
        assert!(
            sidecar.contains("def _ist_day(") && sidecar.contains("class _RotatingOut"),
            "the sidecar must rotate via the IST-day-boundary rotating writer"
        );
        assert!(
            sidecar.contains("capture continues on the current file"),
            "a rotation failure must never stop capture"
        );
        assert!(
            !sidecar.contains("MAX_NDJSON_BYTES"),
            "size-based rotation is deliberately NOT allowed (capture_seq \
             collision risk mid-session)"
        );
    }

    #[test]
    fn test_next_respawn_count_resets_after_healthy_run() {
        // Finding M2: a bridge that ran healthy >= HEALTHY_RUN_RESET_SECS resets
        // the curve (one panic/day never converges to the 60s ceiling); rapid
        // deaths keep climbing.
        assert_eq!(next_respawn_count(0, 0), 1);
        assert_eq!(next_respawn_count(3, 10), 4, "rapid death keeps climbing");
        assert_eq!(
            next_respawn_count(7, HEALTHY_RUN_RESET_SECS),
            1,
            "healthy run resets"
        );
        assert_eq!(next_respawn_count(u32::MAX, 0), u32::MAX, "no overflow");
        assert_eq!(HEALTHY_RUN_RESET_SECS, 300);
    }

    #[test]
    fn test_spawn_supervised_groww_bridge_owns_aggregator_and_force_seal_once() {
        // The supervisor wrapper must create the aggregator + spawn the
        // IST-midnight force-seal ONCE (outside the respawn loop) so respawns
        // never leak duplicate force-seal tasks and bars survive a panic. Pin
        // by source-scan: inside spawn_supervised_groww_bridge, the force-seal
        // spawn appears BEFORE the respawn `loop {`.
        let src = include_str!("groww_bridge.rs");
        let sup_idx = src
            .find("pub fn spawn_supervised_groww_bridge(")
            .expect("spawn_supervised_groww_bridge must exist");
        let tail = &src[sup_idx..];
        let seal_idx = tail
            .find("spawn_groww_ist_midnight_force_seal(")
            .expect("the supervisor must spawn the force-seal");
        let loop_idx = tail
            .find("loop {")
            .expect("the supervisor must have a respawn loop");
        assert!(
            seal_idx < loop_idx,
            "the force-seal spawn must be OUTSIDE (before) the respawn loop — \
             inside it, every respawn would leak a duplicate force-seal task"
        );
        // And the respawn must be observable: FEED-SUPERVISOR-01 + the counter.
        assert!(
            tail.contains("FeedSupervisor01Respawned"),
            "a bridge respawn must log error!(code = FEED-SUPERVISOR-01)"
        );
        assert!(
            tail.contains("tv_feed_supervisor_respawn_total"),
            "a bridge respawn must increment tv_feed_supervisor_respawn_total"
        );
    }

    #[test]
    fn test_parse_time_liveness_is_recorded_before_flush() {
        // The stall watchdog reads last_tick_age_secs; it MUST see parse-time
        // delivery (the sidecar is healthy) even while QuestDB is down —
        // otherwise a DB outage mimics a dead socket and the watchdog kills the
        // healthy Python sidecar in a restart storm (2026-07-02 HIGH). Pin: the
        // record_feed_liveness call appears BEFORE the flush arm in this file.
        let src = include_str!("groww_bridge.rs");
        let liveness_idx = src
            .find("feed_health.record_feed_liveness(Feed::Groww")
            .expect("the bridge must record parse-time liveness");
        let flush_idx = src
            .find("self.live_writer.flush()")
            .expect("the flush arm must exist");
        assert!(
            liveness_idx < flush_idx,
            "record_feed_liveness must be stamped BEFORE (independent of) the \
             QuestDB flush — persist-coupled liveness is the bug this fixes"
        );
    }

    #[test]
    fn test_liveness_ts_advanced_first_tick() {
        // First real tick from a fresh bridge (prev_max = 0) stamps liveness.
        let mut prev = 0i64;
        assert!(liveness_ts_advanced(&mut prev, 1_000));
        assert_eq!(prev, 1_000);
    }

    #[test]
    fn test_liveness_ts_advanced_frozen_ts_does_not_stamp() {
        // The 2026-07-03 incident shape: the same exchange ts re-dumped forever
        // (with any price) must NOT keep the feed looking alive — the stall
        // watchdog needs the age to grow so FEED-STALL-01 can fire.
        let mut prev = 0i64;
        assert!(liveness_ts_advanced(&mut prev, 1_783_069_200_183_000_000));
        for _ in 0..25 {
            assert!(
                !liveness_ts_advanced(&mut prev, 1_783_069_200_183_000_000),
                "a frozen exchange ts must never re-stamp liveness"
            );
        }
        assert_eq!(prev, 1_783_069_200_183_000_000);
    }

    #[test]
    fn test_liveness_ts_advanced_replay_lower_ts_does_not_stamp() {
        // A byte-0 re-tail replay of already-seen (older) lines is not fresh
        // delivery; neither is a no-parsed-lines wake (wake_max = 0).
        let mut prev = 5_000i64;
        assert!(!liveness_ts_advanced(&mut prev, 4_999));
        assert!(!liveness_ts_advanced(&mut prev, 0));
        assert_eq!(prev, 5_000, "replay must not move the max backwards");
    }

    #[test]
    fn test_liveness_ts_advanced_monotonic_updates() {
        // A healthy in-market feed advances every wake — stamps every time.
        let mut prev = 0i64;
        for ts in [10i64, 20, 30, 40] {
            assert!(liveness_ts_advanced(&mut prev, ts));
            assert_eq!(prev, ts);
        }
    }

    // ── B2 (2026-07-03): re-tail/resume crash-recovery suite ──────────────
    //
    // The Groww lane has NO WAL — the NDJSON re-tail/resume IS its entire
    // crash-recovery mechanism, so the mechanics below are EXECUTED (not just
    // source-pinned): offset tracking + partial-line residual completion,
    // shrink/rotation reset, mid-file snapshot resume, the rotated-archive
    // tail drain, and every fail-closed byte-0 branch.
    //
    // ZERO QuestDB dependency (CI-runnable): the writer conf points at the
    // reserved/refused loopback port 1 (the `groww_persistence::for_test()`
    // fail-fast idiom), so `GrowwLiveTickWriter::new` degrades to local
    // buffering and every in-drain flush fails fast after the bounded
    // 50+100+200ms reconnect ladder. `pending()` therefore RETAINS the exact
    // count of rows appended — the loss/duplicate-proof observable every
    // assertion below counts (a lost line under-counts, a double-read
    // over-counts).
    //
    // HONEST ENVELOPE (per operator-charter §F): the snapshot-PERSIST arm
    // (`maybe_persist_offset` inside the flush-Ok arm) is NOT executed by
    // these tests — CI has no QuestDB, so the flush never returns Ok. The
    // resume tests exercise the READ side by hand-writing the snapshot JSON
    // (the same `GrowwOffsetSnapshot` serde shape the persist arm writes;
    // round-trip pinned by `test_offset_snapshot_roundtrip`, write-ordering
    // pinned by `test_offset_persist_is_after_flush_ok`). The live persist
    // path remains covered only by the QuestDB-gated e2e.

    /// Fail-fast QuestDB conf: port 1 is reserved/refused on every platform,
    /// so writer construction + every flush reconnect fails immediately
    /// (never stalls CI) and appended rows stay retained in the local buffer.
    fn retail_unreachable_qdb() -> QuestDbConfig {
        QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        }
    }

    /// A fresh bridge state around the fail-fast conf + its own aggregator.
    fn retail_fresh_state() -> GrowwBridgeState {
        GrowwBridgeState::with_aggregator(
            &retail_unreachable_qdb(),
            MultiTfAggregator::with_capacity(GROWW_AGGREGATOR_CAPACITY),
        )
    }

    /// pid+nanos-unique temp dir (the chaos_tmp idiom) — removed by the caller.
    fn retail_tmp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "tv_groww_retail_{tag}_{}_{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("mk retail tmp dir");
        dir
    }

    /// Base exchange ts for generated lines: ~2026-05-29 IST — inside the
    /// `[2020, 2100)` plausibility window and in the PAST relative to the
    /// receipt clock, so `validate_groww_tick` accepts every generated line.
    const RETAIL_BASE_TS_NANOS: i64 = 1_780_000_000_000_000_000;

    /// One VALID newline-terminated NDJSON tick line per the sidecar contract.
    fn retail_line(n: i64) -> String {
        let ts = RETAIL_BASE_TS_NANOS + n * NANOS_PER_SECOND;
        format!(
            "{{\"security_id\":{},\"segment\":\"NSE_EQ\",\"ts_ist_nanos\":{ts},\"exchange_ts_millis\":{},\"ltp\":{}.5,\"cum_volume\":{}}}\n",
            1_000 + n,
            ts / 1_000_000,
            100 + n,
            1_000 + n
        )
    }

    /// The identity head EXACTLY as `maybe_persist_offset` records it: the
    /// first [`GROWW_OFFSET_HEAD_LEN`] bytes, lossy-decoded.
    fn retail_head(bytes: &[u8]) -> String {
        let n = bytes.len().min(GROWW_OFFSET_HEAD_LEN);
        String::from_utf8_lossy(&bytes[..n]).into_owned()
    }

    /// Today's IST day index via the SAME clock production uses.
    fn retail_current_ist_day() -> i64 {
        ist_day_from_ist_nanos(receipt_ist_nanos())
    }

    /// Hand-write the offset snapshot at its production sibling path.
    fn retail_write_snapshot(tick_file: &Path, snap: &GrowwOffsetSnapshot) {
        let json = serde_json::to_string(snap).expect("serialize snapshot");
        std::fs::write(offset_snapshot_path(tick_file), json).expect("write snapshot");
    }

    #[tokio::test]
    async fn test_retail_drain_offset_tracks_complete_lines_and_carries_partial() {
        // (a) N complete lines drain → offset == file length; a trailing
        // PARTIAL line is carried in the residual and completed on the next
        // drain — no line lost, no line duplicated (exact pending() counts).
        let dir = retail_tmp_dir("offset_partial");
        let tick_file = dir.join("live-ticks.ndjson");
        let full: String = (0..3).map(retail_line).collect();
        let line4 = retail_line(3);
        let (part_a, part_b) = line4.split_at(line4.len() / 2);
        std::fs::write(&tick_file, format!("{full}{part_a}")).expect("seed file");
        let reg = tickvault_common::feed_health::FeedHealthRegistry::new();
        let mut state = retail_fresh_state();

        assert!(
            state.drain_new_data(&tick_file, &reg).await,
            "3 complete lines must parse"
        );
        let len1 = std::fs::metadata(&tick_file).expect("meta").len();
        assert_eq!(
            state.offset, len1,
            "offset must equal the file length (the partial bytes were READ, just not parsed)"
        );
        assert_eq!(
            state.live_writer.pending(),
            3,
            "exactly the 3 COMPLETE lines appended — the partial 4th must NOT be parsed early"
        );
        assert_eq!(
            state.residual,
            part_a.as_bytes(),
            "the partial line must be carried in the residual, byte-exact"
        );

        // Next wake: the sidecar completes the split line.
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&tick_file)
                .expect("open append");
            f.write_all(part_b.as_bytes()).expect("append rest");
        }
        assert!(
            state.drain_new_data(&tick_file, &reg).await,
            "the completed line must parse on the next drain"
        );
        assert_eq!(
            state.offset,
            std::fs::metadata(&tick_file).expect("meta").len(),
            "offset advanced over the completed line"
        );
        assert_eq!(
            state.live_writer.pending(),
            4,
            "the completed line appended exactly ONCE — no loss, no duplicate"
        );
        assert!(state.residual.is_empty(), "residual fully consumed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_retail_drain_file_shrink_resets_offset_no_stall() {
        // (b) rotation/truncate to a SHORTER file → offset resets to byte 0 and
        // the fresh file drains; subsequent appends keep flowing (no permanent
        // stall, which is what a stuck `offset > len` would produce).
        let dir = retail_tmp_dir("shrink");
        let tick_file = dir.join("live-ticks.ndjson");
        let five: String = (0..5).map(retail_line).collect();
        std::fs::write(&tick_file, &five).expect("seed 5 lines");
        let reg = tickvault_common::feed_health::FeedHealthRegistry::new();
        let mut state = retail_fresh_state();
        assert!(state.drain_new_data(&tick_file, &reg).await);
        assert_eq!(state.live_writer.pending(), 5);

        // The sidecar rotates: the path now holds a SHORTER fresh file.
        let two: String = (10..12).map(retail_line).collect();
        assert!(
            two.len() < five.len(),
            "test setup: the rotated file must actually be shorter"
        );
        std::fs::write(&tick_file, &two).expect("rotate to shorter file");
        assert!(
            state.drain_new_data(&tick_file, &reg).await,
            "the shrunk file must re-drain from byte 0 — never a stall"
        );
        assert_eq!(
            state.offset,
            two.len() as u64,
            "offset reset to 0 then advanced over the whole fresh file"
        );
        assert_eq!(
            state.live_writer.pending(),
            7,
            "5 pre-rotation + 2 fresh — the fresh file read exactly once"
        );
        assert!(
            state.residual.is_empty(),
            "stale residual cleared on shrink"
        );

        // The tail keeps flowing after the reset.
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&tick_file)
                .expect("open append");
            f.write_all(retail_line(12).as_bytes()).expect("append");
        }
        assert!(state.drain_new_data(&tick_file, &reg).await);
        assert_eq!(
            state.live_writer.pending(),
            8,
            "post-shrink appends must keep flowing (no permanent stall)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_retail_resume_mid_file_no_double_read() {
        // (c) a hand-written same-day snapshot at a flushed line boundary →
        // outcome "resumed": offset + capture_seq restored, and the next drain
        // parses ONLY the unflushed tail — exact counts prove zero double-read.
        let reg = tickvault_common::feed_health::FeedHealthRegistry::new();
        for attempt in 0..2 {
            let dir = retail_tmp_dir("resume_midfile");
            let tick_file = dir.join("live-ticks.ndjson");
            let lines: Vec<String> = (0..5).map(retail_line).collect();
            let all: String = lines.concat();
            std::fs::write(&tick_file, &all).expect("seed 5 lines");
            // The first 3 lines were flushed-through before the "crash".
            let flushed: usize = lines[..3].iter().map(String::len).sum();
            let today = retail_current_ist_day();
            let snap = GrowwOffsetSnapshot {
                offset: flushed as u64,
                capture_seq: 777_000,
                file_len: all.len() as u64,
                head: retail_head(all.as_bytes()),
                ist_day: today,
            };
            retail_write_snapshot(&tick_file, &snap);

            let mut state = retail_fresh_state();
            state.try_resume_from_snapshot(&tick_file, &reg).await;

            if retail_current_ist_day() != today {
                // IST midnight flipped mid-test (sub-second window): the
                // resume legitimately rejected the now-previous-day snapshot.
                // Re-run once with the new day — bounded, deterministic.
                let _ = std::fs::remove_dir_all(&dir);
                assert_eq!(attempt, 0, "IST day cannot flip twice in one test");
                continue;
            }

            assert_eq!(
                state.offset, flushed as u64,
                "outcome=resumed: read position restored MID-FILE (no full re-tail)"
            );
            assert_eq!(
                state.capture_seq.load(Ordering::Relaxed),
                777_000,
                "capture_seq (the replay-stable dedup tiebreaker) restored from the snapshot"
            );

            assert!(state.drain_new_data(&tick_file, &reg).await);
            assert_eq!(
                state.live_writer.pending(),
                2,
                "ONLY the 2 unflushed tail lines parsed — zero duplicates (a byte-0 re-read would show 5)"
            );
            assert_eq!(state.offset, all.len() as u64);
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
    }

    #[tokio::test]
    async fn test_retail_cross_day_snapshot_drains_archive_then_byte0_retail() {
        // (d) a PREVIOUS-day snapshot + the rotated archive on disk → the
        // `should_drain_archive` branch EXECUTES: the archive tail beyond the
        // flushed offset drains through the real parse/validate/append path,
        // then today's live file re-tails from byte 0 — exact counts prove the
        // flushed archive prefix is never re-read and no live line is lost.
        let reg = tickvault_common::feed_health::FeedHealthRegistry::new();
        for attempt in 0..2 {
            let dir = retail_tmp_dir("archive_drain");
            let tick_file = dir.join("live-ticks.ndjson");
            let today = retail_current_ist_day();
            let yesterday = today - 1;

            // Yesterday's rotated archive: 5 lines, the first 2 flushed
            // before the downtime — only the 3-line tail is un-persisted.
            let archive_lines: Vec<String> = (0..5).map(retail_line).collect();
            let archive_all: String = archive_lines.concat();
            let archive = archive_path_for_ist_day(&tick_file, yesterday);
            std::fs::write(&archive, &archive_all).expect("seed archive");
            let flushed: usize = archive_lines[..2].iter().map(String::len).sum();

            // Today's fresh live file MUST exist — an absent live file
            // short-circuits the resume before the archive branch.
            let live: String = (20..22).map(retail_line).collect();
            std::fs::write(&tick_file, &live).expect("seed live file");

            let snap = GrowwOffsetSnapshot {
                offset: flushed as u64,
                capture_seq: 555,
                file_len: archive_all.len() as u64,
                head: retail_head(archive_all.as_bytes()),
                ist_day: yesterday,
            };
            retail_write_snapshot(&tick_file, &snap);

            let mut state = retail_fresh_state();
            state.try_resume_from_snapshot(&tick_file, &reg).await;

            if retail_current_ist_day() != today {
                // IST midnight flipped mid-test: `yesterday` is now 2 days
                // back and the archive naming no longer lines up — re-run
                // once with the new day. Bounded, deterministic.
                let _ = std::fs::remove_dir_all(&dir);
                assert_eq!(attempt, 0, "IST day cannot flip twice in one test");
                continue;
            }

            assert_eq!(
                state.live_writer.pending(),
                3,
                "archive tail drained FROM the flushed offset — 3 lines, never a re-read of the flushed prefix"
            );
            assert_eq!(
                state.offset, 0,
                "after the archive drain the live file re-tails from byte 0"
            );
            assert!(
                state.residual.is_empty(),
                "cross-file residual cleared before the live re-tail"
            );
            assert!(
                state.active_drain_ist_day.is_none(),
                "the archive-day snapshot stamp is cleared after the drain"
            );

            assert!(state.drain_new_data(&tick_file, &reg).await);
            assert_eq!(
                state.live_writer.pending(),
                5,
                "3 archive-tail + 2 live lines — every line read exactly once"
            );
            assert_eq!(state.offset, live.len() as u64);
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
    }

    #[tokio::test]
    async fn test_retail_corrupt_snapshot_fails_closed_to_byte0() {
        // (e) unparseable snapshot JSON → fail-closed byte-0 full re-tail —
        // never a partial/undefined resume, no garbage state adopted.
        let dir = retail_tmp_dir("corrupt_snap");
        let tick_file = dir.join("live-ticks.ndjson");
        let all: String = (0..4).map(retail_line).collect();
        std::fs::write(&tick_file, &all).expect("seed 4 lines");
        std::fs::write(offset_snapshot_path(&tick_file), b"{ not a snapshot }")
            .expect("write corrupt snapshot");
        let reg = tickvault_common::feed_health::FeedHealthRegistry::new();
        let mut state = retail_fresh_state();
        state.try_resume_from_snapshot(&tick_file, &reg).await;
        assert_eq!(state.offset, 0, "corrupt snapshot → byte-0 re-tail");
        assert_eq!(
            state.capture_seq.load(Ordering::Relaxed),
            0,
            "no partial state adopted from garbage"
        );
        assert!(state.drain_new_data(&tick_file, &reg).await);
        assert_eq!(
            state.live_writer.pending(),
            4,
            "full re-tail — every line recovered exactly once"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_retail_wrong_day_snapshot_without_archive_fails_closed_to_byte0() {
        // (e) a previous-day snapshot whose archive is GONE (retention wiped /
        // never rotated) → no archive drain, fail-closed byte-0 full re-tail.
        // Flip-safe without a retry: however IST midnight moves, the snapshot
        // day stays in the past and no archive exists at its derived path.
        let dir = retail_tmp_dir("wrong_day_snap");
        let tick_file = dir.join("live-ticks.ndjson");
        let all: String = (0..3).map(retail_line).collect();
        std::fs::write(&tick_file, &all).expect("seed 3 lines");
        let snap = GrowwOffsetSnapshot {
            offset: 10,
            capture_seq: 99,
            file_len: all.len() as u64,
            head: retail_head(all.as_bytes()),
            ist_day: retail_current_ist_day() - 1,
        };
        retail_write_snapshot(&tick_file, &snap);
        let reg = tickvault_common::feed_health::FeedHealthRegistry::new();
        let mut state = retail_fresh_state();
        state.try_resume_from_snapshot(&tick_file, &reg).await;
        assert_eq!(
            state.offset, 0,
            "cross-day snapshot with no archive → byte-0 re-tail (never a mid-file resume)"
        );
        assert!(state.drain_new_data(&tick_file, &reg).await);
        assert_eq!(
            state.live_writer.pending(),
            3,
            "full re-tail of today's file — nothing skipped, nothing doubled"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_retail_snapshot_offset_beyond_truncated_file_fails_closed_to_byte0() {
        // Boundary: the snapshot's offset lies BEYOND the current file length
        // (rotation/truncate shrank it below the flushed point) with a
        // matching head → the resume must reject (offset > len) and byte-0
        // re-tail. Flip-safe without a retry: a mid-test day flip only changes
        // WHICH check rejects (day instead of length) — the byte-0 outcome and
        // the exact-count assertions are identical.
        let dir = retail_tmp_dir("over_offset");
        let tick_file = dir.join("live-ticks.ndjson");
        let all: String = (0..2).map(retail_line).collect();
        std::fs::write(&tick_file, &all).expect("seed 2 lines");
        let snap = GrowwOffsetSnapshot {
            offset: all.len() as u64 + 10_000,
            capture_seq: 42,
            file_len: all.len() as u64 + 10_000,
            head: retail_head(all.as_bytes()),
            ist_day: retail_current_ist_day(),
        };
        retail_write_snapshot(&tick_file, &snap);
        let reg = tickvault_common::feed_health::FeedHealthRegistry::new();
        let mut state = retail_fresh_state();
        state.try_resume_from_snapshot(&tick_file, &reg).await;
        assert_eq!(
            state.offset, 0,
            "offset beyond the truncated length → fail-closed byte-0 (never a past-EOF seek)"
        );
        assert!(state.drain_new_data(&tick_file, &reg).await);
        assert_eq!(
            state.live_writer.pending(),
            2,
            "full re-tail — both surviving lines recovered exactly once"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_retail_snapshot_for_missing_file_is_ignored() {
        // Boundary: a snapshot whose capture file no longer exists → the
        // snapshot is ignored (offset stays 0) and draining the missing file
        // is a safe no-op — no panic, no stall.
        let dir = retail_tmp_dir("missing_file");
        let tick_file = dir.join("live-ticks.ndjson"); // never created
        let snap = GrowwOffsetSnapshot {
            offset: 500,
            capture_seq: 7,
            file_len: 1_000,
            head: "{\"security_id\":1000".to_string(),
            ist_day: retail_current_ist_day(),
        };
        retail_write_snapshot(&tick_file, &snap);
        let reg = tickvault_common::feed_health::FeedHealthRegistry::new();
        let mut state = retail_fresh_state();
        state.try_resume_from_snapshot(&tick_file, &reg).await;
        assert_eq!(state.offset, 0, "absent capture file → snapshot ignored");
        assert!(
            !state.drain_new_data(&tick_file, &reg).await,
            "draining the missing file is a safe no-op"
        );
        assert_eq!(state.live_writer.pending(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_retail_drain_empty_file_noop_then_first_line_flows() {
        // Boundary: an EMPTY capture file (sidecar started, nothing appended)
        // is a no-op drain; the very first appended line then flows normally.
        let dir = retail_tmp_dir("empty_file");
        let tick_file = dir.join("live-ticks.ndjson");
        std::fs::write(&tick_file, b"").expect("seed empty file");
        let reg = tickvault_common::feed_health::FeedHealthRegistry::new();
        let mut state = retail_fresh_state();
        assert!(
            !state.drain_new_data(&tick_file, &reg).await,
            "empty file → nothing parsed, no panic"
        );
        assert_eq!(state.offset, 0);
        assert_eq!(state.live_writer.pending(), 0);

        std::fs::write(&tick_file, retail_line(0)).expect("first line");
        assert!(state.drain_new_data(&tick_file, &reg).await);
        assert_eq!(state.live_writer.pending(), 1, "first line flows");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_retail_persisted_offset_excludes_torn_partial_line() {
        // B2 fix 1 pinning test (hostile-review VERIFIED bug): the snapshot the
        // REAL `maybe_persist_offset` writes must point at the last COMPLETE
        // line boundary, NOT the raw read cursor — the raw cursor includes a
        // torn trailing partial line whose bytes exist only in the RAM
        // residual, so a crash right after the persist would resume PAST the
        // torn line's start and its tail would be dropped as malformed
        // (1 line permanently lost). Drives `maybe_persist_offset` DIRECTLY
        // (the production snapshot writer), then simulates crash + restart +
        // sidecar-completes-the-line, asserting ZERO lost and ZERO duplicated.
        let reg = tickvault_common::feed_health::FeedHealthRegistry::new();
        for attempt in 0..2 {
            let dir = retail_tmp_dir("torn_persist");
            let tick_file = dir.join("live-ticks.ndjson");
            let complete: String = (0..3).map(retail_line).collect();
            let line4 = retail_line(3);
            let (part_a, part_b) = line4.split_at(line4.len() / 2);
            std::fs::write(&tick_file, format!("{complete}{part_a}")).expect("seed torn file");
            let today = retail_current_ist_day();

            let mut state = retail_fresh_state();
            assert!(state.drain_new_data(&tick_file, &reg).await);
            assert_eq!(state.live_writer.pending(), 3, "3 complete lines parsed");
            assert_eq!(
                state.residual,
                part_a.as_bytes(),
                "torn bytes buffered in RAM"
            );

            // The instant production persists the snapshot (normally inside the
            // flush-Ok arm — unreachable in CI without QuestDB, so drive the
            // REAL writer directly at the same state).
            state.maybe_persist_offset(&tick_file).await;
            let raw = std::fs::read_to_string(offset_snapshot_path(&tick_file))
                .expect("snapshot written");
            let snap: GrowwOffsetSnapshot = serde_json::from_str(&raw).expect("snapshot parses");
            assert_eq!(
                snap.offset,
                complete.len() as u64,
                "persisted offset must be the last COMPLETE-line boundary — the \
                 torn residual bytes must be EXCLUDED (the fix)"
            );
            assert_eq!(
                snap.file_len,
                (complete.len() + part_a.len()) as u64,
                "file_len stays the diagnostic metadata length (unused by resume)"
            );

            // CRASH: state dropped (residual lost). The sidecar then completes
            // the torn line. RESTART: fresh state resumes from the snapshot.
            drop(state);
            {
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&tick_file)
                    .expect("open append");
                f.write_all(part_b.as_bytes()).expect("complete the line");
            }
            let mut restarted = retail_fresh_state();
            restarted.try_resume_from_snapshot(&tick_file, &reg).await;

            if retail_current_ist_day() != today {
                // IST midnight flipped mid-test (sub-second window) — the
                // resume legitimately byte-0'd. Re-run once with the new day.
                let _ = std::fs::remove_dir_all(&dir);
                assert_eq!(attempt, 0, "IST day cannot flip twice in one test");
                continue;
            }

            assert_eq!(
                restarted.offset,
                complete.len() as u64,
                "resume adopts the complete-line boundary, never a mid-line offset"
            );
            assert!(restarted.drain_new_data(&tick_file, &reg).await);
            assert_eq!(
                restarted.live_writer.pending(),
                1,
                "the torn 4th line is recovered exactly ONCE — zero lost \
                 (pre-fix: 0, dropped as malformed), zero duplicated (a flushed \
                 line re-read would show >1)"
            );
            assert_eq!(
                restarted.offset,
                std::fs::metadata(&tick_file).expect("meta").len()
            );
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
    }

    #[tokio::test]
    async fn test_retail_cross_day_archive_drains_even_without_today_live_file() {
        // B2 fix 2 pinning test (H-3 completion): midnight rotation + the
        // sidecar has NOT created today's live file yet when the bridge boots
        // (rotation at 00:00, bridge boots 08:31, first tick 09:00). Before the
        // fix, the `no_file` early-return skipped the archive branch and
        // yesterday's unflushed tail was silently orphaned until the 2-day
        // retention wiped it. Now: the archive tail drains EVEN WITHOUT the
        // live file, then today's file flows from byte 0 when it appears.
        // Flip-safe without a retry: however IST midnight moves, the snapshot
        // day stays strictly in the past and the archive path is derived from
        // the SNAPSHOT's day, so `should_drain_archive` stays true.
        let reg = tickvault_common::feed_health::FeedHealthRegistry::new();
        let dir = retail_tmp_dir("archive_no_live");
        let tick_file = dir.join("live-ticks.ndjson"); // NOT created yet
        let yesterday = retail_current_ist_day() - 1;

        // Yesterday's rotated archive: 4 lines, the first flushed pre-crash.
        let archive_lines: Vec<String> = (0..4).map(retail_line).collect();
        let archive_all: String = archive_lines.concat();
        let archive = archive_path_for_ist_day(&tick_file, yesterday);
        std::fs::write(&archive, &archive_all).expect("seed archive");
        let flushed = archive_lines[0].len();

        let snap = GrowwOffsetSnapshot {
            offset: flushed as u64,
            capture_seq: 321,
            file_len: archive_all.len() as u64,
            head: retail_head(archive_all.as_bytes()),
            ist_day: yesterday,
        };
        retail_write_snapshot(&tick_file, &snap);

        let mut state = retail_fresh_state();
        state.try_resume_from_snapshot(&tick_file, &reg).await;

        assert_eq!(
            state.live_writer.pending(),
            3,
            "archive tail drained EVEN THOUGH today's live file is absent — \
             3 unflushed lines recovered (pre-fix: 0, silently orphaned)"
        );
        assert_eq!(
            state.offset, 0,
            "state left ready for the byte-0 tail of today's file"
        );
        assert!(state.residual.is_empty());
        assert!(state.active_drain_ist_day.is_none());
        assert!(
            !state.drain_new_data(&tick_file, &reg).await,
            "today's file still absent → safe no-op"
        );

        // The sidecar creates today's file later — lines flow from byte 0.
        let live: String = (30..32).map(retail_line).collect();
        std::fs::write(&tick_file, &live).expect("today's file appears");
        assert!(state.drain_new_data(&tick_file, &reg).await);
        assert_eq!(
            state.live_writer.pending(),
            5,
            "3 archive-tail + 2 live lines — every line read exactly once"
        );
        assert_eq!(state.offset, live.len() as u64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_retail_archive_drain_spans_multiple_bounded_wakes() {
        // MEDIUM follow-up: the archive drain loop must make forward progress
        // ACROSS bounded 4 MiB wakes (GROWW_BRIDGE_MAX_READ_BYTES) — an
        // archive tail larger than one wake budget drains fully, with the
        // chunk-boundary line split carried in the residual between wakes.
        // Exact counts prove no line is lost or duplicated at the chunk seam.
        // HONEST ENVELOPE: the wake CAP arm (4096 wakes = 16 GiB) and the
        // no-forward-progress break (a mid-loop read fault) remain unexecuted
        // — exercising them needs a 16 GiB fixture / mid-loop fault injection,
        // documented as an open envelope note in the plan.
        let reg = tickvault_common::feed_health::FeedHealthRegistry::new();
        let dir = retail_tmp_dir("archive_multiwake");
        let tick_file = dir.join("live-ticks.ndjson");
        let yesterday = retail_current_ist_day() - 1;

        // Build an archive whose UNFLUSHED tail exceeds one 4 MiB wake budget.
        let mut archive_all = String::new();
        let mut n_lines: u64 = 0;
        while (archive_all.len() as u64) <= GROWW_BRIDGE_MAX_READ_BYTES + 64 * 1024 {
            // i64 cast: n_lines stays ~33K, far inside i64.
            archive_all.push_str(&retail_line(n_lines as i64));
            n_lines += 1;
        }
        let archive = archive_path_for_ist_day(&tick_file, yesterday);
        std::fs::write(&archive, &archive_all).expect("seed >4MiB archive");
        // Today's live file exists (the classic H-3 shape) — empty.
        std::fs::write(&tick_file, b"").expect("seed empty live file");

        let snap = GrowwOffsetSnapshot {
            offset: 0,
            capture_seq: 1,
            file_len: archive_all.len() as u64,
            head: retail_head(archive_all.as_bytes()),
            ist_day: yesterday,
        };
        retail_write_snapshot(&tick_file, &snap);

        let mut state = retail_fresh_state();
        state.try_resume_from_snapshot(&tick_file, &reg).await;

        assert_eq!(
            state.live_writer.pending() as u64,
            n_lines,
            "every archive line drained exactly once across ≥2 bounded wakes \
             (the 4 MiB chunk seam must not lose or duplicate the split line)"
        );
        assert_eq!(state.offset, 0, "ready for the live byte-0 tail");
        assert!(state.residual.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_liveness_gate_is_wired_into_drain() {
        // Ratchet (2026-07-03): the parse-time record_feed_liveness call must
        // be gated by the timestamp-advance decision — an unconditional stamp
        // silently restores the frozen-snapshot masking bug.
        let src = include_str!("groww_bridge.rs");
        let gate_idx = src
            .find("liveness_ts_advanced(\n                &mut self.liveness_max_exchange_ts_nanos")
            .or_else(|| src.find("liveness_ts_advanced(&mut self.liveness_max_exchange_ts_nanos"))
            .expect("the drain path must gate liveness on liveness_ts_advanced");
        let liveness_idx = src
            .find("feed_health.record_feed_liveness(Feed::Groww")
            .expect("the bridge must record parse-time liveness");
        assert!(
            gate_idx < liveness_idx,
            "liveness_ts_advanced must guard the record_feed_liveness stamp"
        );
    }
}

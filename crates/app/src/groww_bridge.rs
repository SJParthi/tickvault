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
use tickvault_storage::feed_gap_audit_persistence::{
    FeedGapAuditRow, FeedGapAuditWriter, ensure_feed_gap_audit_table,
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
        // O(1) EXEMPT: begin — WS lifecycle audit row: at most one per
        // connect/disconnect transition (COLD PATH per the fn docs), never per tick.
        source: source.to_string(),
        reason: reason.to_string(),
        // O(1) EXEMPT: end
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
    // 2026-07-05: upgraded from debug! (silent-loss window — the operator found
    // ws_event_audit feed='groww' EMPTY with zero signal). Static reason label
    // only ("full"/"closed") — never the dropped row, whose pre-redaction
    // reason must never reach a log (security review).
    if let Err(err) = tx.try_send(row) {
        let drop_reason = groww_ws_audit_drop_reason(&err);
        error!(
            code = tickvault_common::error_code::ErrorCode::AuditWs01EventWriteFailed.code_str(),
            reason = drop_reason,
            "groww ws_event_audit channel full/closed — row dropped (log+feed_health still fired)"
        );
        metrics::counter!("tv_ws_event_audit_dropped_total", "reason" => drop_reason).increment(1);
    }
}

/// 2026-07-05: static drop-reason label for a Groww `ws_event_audit`
/// `try_send` failure — `"full"` / `"closed"`. Mirrors the core helper
/// (`tickvault_core::websocket::connection::ws_audit_drop_reason`, crate-private
/// there) so the `tv_ws_event_audit_dropped_total{reason}` counter never
/// allocates a label and the dropped row's content is never formatted.
#[inline]
#[must_use]
fn groww_ws_audit_drop_reason(
    err: &tokio::sync::mpsc::error::TrySendError<WsEventAuditRow>,
) -> &'static str {
    match err {
        tokio::sync::mpsc::error::TrySendError::Full(_) => "full",
        tokio::sync::mpsc::error::TrySendError::Closed(_) => "closed",
    }
}

/// One line of the sidecar's PROOF status file. `event` is `"subscribed"` or
/// `"streaming"`; the counts are the SIDs the sidecar actually subscribed today.
/// `emitted`/`dropped` are the honest-feed PROOF (operator 2026-06-29): records the
/// producer DECODED+EMITTED vs DECODED-but-DROPPED (a `sid_map` miss / missing
/// field). All are `#[serde(default)]` so an OLD sidecar status (no fields) parses
/// to 0 — forward-safe, no error. `ts_ist_nanos` (the sidecar's own write stamp,
/// present in every `write_status` record) is the FRESHNESS signal: a status
/// record with `ts_ist_nanos == 0` (old format) can never prove liveness — see
/// [`groww_status_is_live`] (stale-status false-OK fix, 2026-07-06).
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
    /// IST epoch nanos (UTC epoch + 19,800s — the SAME convention as
    /// [`receipt_ist_nanos`] and the tick lines' `ts_ist_nanos`) the sidecar
    /// stamped when it WROTE this record (`write_status` → `now_ist_nanos()`
    /// in `groww_sidecar.py`). The cross-language epoch contract is ratcheted
    /// by `test_sidecar_status_stamp_shares_the_ist_epoch_convention`
    /// (2026-07-06 fix: the sidecar previously stamped the PLAIN UTC epoch,
    /// so every real record read 19,800s old and the freshness gate rejected
    /// it forever). 0 = the stamp is absent (old-format line) — treated as
    /// NEVER-live, fail-closed. A pre-fix sidecar's UTC-epoch stamp reads
    /// ~5.5h old under this gate and is likewise rejected — correct, since
    /// any such file is by definition a pre-restart fossil (the supervisor
    /// relaunches the sidecar from the same deployed tree as this binary).
    #[serde(default)]
    ts_ist_nanos: i64,
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

/// Max age for a sidecar status record to count as LIVE evidence at FIRST
/// observation per connected episode (the sidecar rewrites the status ≤1s
/// apart while streaming and once at subscribe, so a healthy record is always
/// well inside this bound; a record older than it is a fossil from a killed /
/// stalled / prior-day sidecar — mirrors the ladder's
/// `PLATEAU_PROOF_FRESHNESS_SECS` fossil-aging pattern). The one-shot
/// `subscribed` write is NOT rewritten on a tickless window, so this window
/// deliberately does NOT need to cover the boot-ordering notifier-slot
/// deferral: once a record has been observed fresh, the bridge latches
/// `fresh_status_seen_this_episode` and the deferral / gauge no longer
/// require the record to STILL be fresh at announce time (2026-07-06 fix —
/// the floor check alone kills fossils; without the latch, a >120s
/// docker/notifier boot chain silently LOST the boot-connect ping and left
/// the gauge 0 until the first tick, contradicting the in-code
/// "delayed, never lost" contract).
const GROWW_STATUS_LIVE_MAX_AGE_NANOS: i64 = 120 * NANOS_PER_SECOND;

/// PURE freshness classification of one sidecar status record (stale-status
/// false-OK fix, 2026-07-06). A status is LIVE evidence ONLY when ALL hold:
///
/// 1. it carries a write stamp (`ts_ist_nanos > 0` — an old-format record
///    without one can never prove liveness, fail-closed),
/// 2. the stamp is at/after the caller's live FLOOR (the bridge incarnation
///    start, advanced on every runtime-disabled wake) — so yesterday's
///    EBS-persisted file at boot AND the pre-disable frozen file on a
///    disable→re-enable cycle are both fossils, never proof,
/// 3. the stamp is within [`GROWW_STATUS_LIVE_MAX_AGE_NANOS`] of now (a
///    killed sidecar's last write ages out), and
/// 4. the stamp is not absurdly in the future (garbage / clock-skew guard —
///    a future stamp beyond the same window is rejected, fail-closed).
///
/// The incident class this closes: the sidecar status file is NEVER deleted
/// on a runtime disable (the child is `start_kill`ed mid-write-cadence) nor
/// at boot (EBS persists it across the daily stop/start), so a bare
/// `event == streaming` read latched `streaming_observed`, fired a false
/// "✅ Prices are flowing" `FeedRecovered` on every disable→re-enable, and
/// pinned `tv_groww_ws_active = 1` all session while a dead-at-boot sidecar
/// streamed nothing (blinding the groww-ws-inactive alarm).
///
/// EPOCH CONTRACT (2026-07-06 fix): both sides of the comparison are "IST
/// epoch" nanos (UTC epoch + 19,800s) — `status_ts_ist_nanos` is stamped by
/// the sidecar's `now_ist_nanos()` (the `replace(tzinfo=timezone.utc)`
/// trick, same as `ms_to_ist_nanos`), and `now`/`floor` come from
/// [`receipt_ist_nanos`]. The sidecar previously stamped the PLAIN UTC epoch
/// (`.timestamp()` on an aware datetime ignores the zone), making condition
/// 3 unsatisfiable (every real record read ≈19,800s old) — every fresh
/// status was rejected forever. Ratchet:
/// `test_sidecar_status_stamp_shares_the_ist_epoch_convention`.
#[must_use]
fn groww_status_is_live(
    status_ts_ist_nanos: i64,
    now_ist_nanos: i64,
    live_floor_ist_nanos: i64,
) -> bool {
    status_ts_ist_nanos > 0
        && status_ts_ist_nanos >= live_floor_ist_nanos
        && now_ist_nanos.saturating_sub(status_ts_ist_nanos) <= GROWW_STATUS_LIVE_MAX_AGE_NANOS
        && status_ts_ist_nanos.saturating_sub(now_ist_nanos) <= GROWW_STATUS_LIVE_MAX_AGE_NANOS
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
    /// Optional per-message capture stamp (2026-07-03 per-callback instant
    /// capture): UTC epoch NANOS from `time.time_ns()` stamped INSIDE the
    /// sidecar's NATS-callback hook — the true capture-at-receipt instant.
    /// Absent (`None`) on old-format lines and on the walker's reconcile-sweep
    /// rows (snapshot drains have no per-message stamp) — those fall back to
    /// the per-wake receipt stamp, exactly the pre-PR behaviour.
    #[serde(default)]
    capture_ns: Option<i64>,
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
    /// Sidecar capture-at-receipt stamp (UTC epoch nanos), when the line
    /// carried one — see [`GrowwTickLine::capture_ns`].
    capture_ns: Option<i64>,
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
        capture_ns: l.capture_ns,
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
/// IST-nanos convention). A degenerate clock read yields
/// `IST_UTC_OFFSET_NANOS` (a ~1970 instant), NOT 0 — both fall below this
/// floor. F1 security finding (2026-07-03, supersedes the 2026-07-02
/// fail-open F6 behavior): a receipt at/below this floor now REJECTS the
/// tick (fail-closed, [`GrowwTickReject::ImplausibleReceiptClock`]) instead
/// of skipping the ±60 s future-skew clamp — a skipped clamp let a tick
/// dated years in the future (inside the static `[2020, 2100)` bounds)
/// poison the per-feed seal watermark (`fetch_max` never regresses) and fold
/// garbage into candle cells. A broken host clock is a BOOT-03-class
/// condition; dropping Groww ticks then is strictly safer.
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
    /// OUR receipt clock read is implausible (≤ 2020-01-01 — a 0 /
    /// negative / degenerate ~1970 read), so the ±60 s future-skew clamp
    /// cannot anchor. F1 (2026-07-03): fail CLOSED — reject the tick rather
    /// than admit a possibly-future-dated stamp that would poison the seal
    /// watermark. BOOT-03-class host condition.
    ImplausibleReceiptClock,
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
        // O(1) EXEMPT: RangeInclusive::contains — two-comparison O(1) bounds check (scanner heuristic misreads it as Vec::contains).
        .contains(&parsed.tick.ts_ist_nanos)
    {
        return Err(GrowwTickReject::TimestampOutOfRange);
    }
    // F1 (2026-07-03 security review, supersedes the 2026-07-02 fail-open
    // F6 behavior): the receipt clock anchors the ±60 s future-skew clamp.
    // An implausible read (0 / negative / degenerate ~1970) means the clamp
    // CANNOT run — and skipping it would let a tick dated years in the
    // future (still inside the static [2020, 2100) bounds above) poison the
    // per-feed seal watermark (fetch_max never regresses) and fold garbage
    // into candle cells. A broken host clock is a BOOT-03-class condition:
    // dropping Groww ticks then is strictly safer, so fail CLOSED.
    if receipt_ist_nanos <= GROWW_MIN_PLAUSIBLE_RECEIPT_IST_NANOS {
        return Err(GrowwTickReject::ImplausibleReceiptClock);
    }
    // PR-4 relative clamp: reject a tick stamped ahead of OUR receipt clock
    // beyond the skew tolerance.
    if parsed.tick.ts_ist_nanos > receipt_ist_nanos.saturating_add(GROWW_FUTURE_TS_TOLERANCE_NANOS)
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
/// CLOSED for the validator (every tick rejected as
/// [`GrowwTickReject::ImplausibleReceiptClock`] while below
/// [`GROWW_MIN_PLAUSIBLE_RECEIPT_IST_NANOS`] — F1 security finding
/// 2026-07-03, supersedes the 2026-07-02 fail-open F6 behavior).
fn receipt_ist_nanos() -> i64 {
    chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(0)
        .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS)
}

/// IST epoch SECONDS "now" — the down-episode clock for the feed-down
/// alerting latches (operator 2026-07-06). Same clock on the falling and
/// rising edges, so `down_secs` is a well-formed duration regardless of the
/// IST offset.
fn ist_epoch_secs_now() -> u64 {
    (receipt_ist_nanos() / 1_000_000_000).max(0) as u64
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

// ---------------------------------------------------------------------------
// Feed gap-episode tracker (FEED-GAP-01 — Groww hardening PR-3, 2026-07-14)
// ---------------------------------------------------------------------------

/// In-session feed-level last-tick age (whole seconds) at/above which a gap
/// EPISODE opens (the 2026-07-14 34.999s incident class). Pinned by
/// `test_feed_gap_episode_threshold_is_10s`. Deliberately BELOW the
/// FEED-STALL-01 restart threshold — the episode names the gap before the
/// watchdog's kill+relaunch resolves it.
pub(crate) const FEED_GAP_EPISODE_THRESHOLD_SECS: u64 = 10;

/// Floor (whole seconds) below which a measured inter-tick spacing is the
/// NORMAL ≤1s universe cadence, not a gap — the unconditional
/// `tv_feed_gap_seconds_total` counter accumulates every measured gap at or
/// above this floor, INDEPENDENT of the 10s episode threshold (sub-episode
/// micro-gaps stay visible as a trend without episode noise).
pub(crate) const FEED_GAP_COUNTER_FLOOR_SECS: u64 = 2;

/// Bound on the named partial 1-minute bucket labels carried by a CLOSE row
/// (defense against a pathological multi-hour gap producing an unbounded
/// string — entries beyond the bound collapse to a single ellipsis marker).
pub(crate) const FEED_GAP_PARTIAL_MINUTES_MAX: usize = 10;

/// One gap-episode EDGE decision from a tracker poll. Timestamps are IST
/// epoch nanos (the bridge's receipt clock).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FeedGapEdge {
    /// No edge this wake.
    None,
    /// The last-tick age crossed the episode threshold in-session — the
    /// episode OPENS (one OPEN row + one High Telegram, edge-latched by the
    /// tracker's own `open` state).
    Open {
        /// Detection instant (this wake's clock).
        detect_nanos: i64,
        /// The last tick BEFORE the silence (now − age).
        last_tick_nanos: i64,
    },
    /// Liveness advanced after an open episode — the episode CLOSES with the
    /// measured last-tick-to-recovery gap.
    Close {
        /// Detection instant of the matching OPEN (the episode identity).
        detect_nanos: i64,
        /// The last tick BEFORE the gap.
        last_tick_before_nanos: i64,
        /// The recovery tick instant.
        end_nanos: i64,
        /// Measured gap in whole seconds (last-tick-before → recovery).
        gap_secs: u64,
    },
}

/// Pure gap-episode tracker: ONE `Open` per episode (edge-latched via the
/// `open` state), `Close` on the liveness-advance recovery edge, and the
/// unconditional counter contribution for EVERY measured gap ≥ the floor
/// (threshold-independent). Driven from the bridge's existing ≤1s wake —
/// NEVER the per-tick hot path. Never opens before the feed's first tick of
/// the session (`age == None` — the never-streamed class belongs to
/// FEED-STALL-01, not a gap).
#[derive(Debug, Default)]
pub(crate) struct FeedGapTracker {
    /// `Some((last_tick_before_nanos, detect_nanos))` while an episode is open.
    open: Option<(i64, i64)>,
    /// Highest last-tick instant ever observed (0 = none yet) — the
    /// liveness-advance reference for the unconditional gap counter.
    prev_last_tick_nanos: i64,
}

impl FeedGapTracker {
    /// One poll: `last_tick_nanos` = the RAW feed-level last-tick IST stamp
    /// ([`tickvault_common::feed_health::FeedHealthRegistry::last_tick_ist_nanos`]
    /// — NEVER a value reconstructed from the FLOORED age, whose discarded
    /// sub-second remainder creeps the reconstruction forward every >1s poll
    /// and fakes a liveness advance during a real gap; review CRITICAL
    /// 2026-07-14), `None` until the first tick of the session; `now_nanos` =
    /// this wake's IST receipt clock; `in_session` = the episode-OPEN gate
    /// (an episode only OPENS in-session; a recovery CLOSE fires regardless
    /// so a session-tail gap that recovers just after close is still
    /// measured).
    ///
    /// Returns the edge (if any) + the whole-seconds contribution to the
    /// unconditional `tv_feed_gap_seconds_total` counter.
    pub(crate) fn poll(
        &mut self,
        last_tick_nanos: Option<i64>,
        now_nanos: i64,
        in_session: bool,
    ) -> (FeedGapEdge, u64) {
        let Some(last_tick_nanos) = last_tick_nanos else {
            // Never streamed this session — no last tick, no gap to name.
            return (FeedGapEdge::None, 0);
        };
        // Belt AND suspenders (review CRITICAL, 2026-07-14): the raw stamp
        // only moves on a real tick, and the advanced branch ADDITIONALLY
        // requires a full ≥1s advance (hysteresis) — so even a creeping /
        // jittering input (sub-second forward drift per poll, the floored-age
        // reconstruction shape) can never fake a recovery: it falls through
        // to the silence branch, the age keeps growing, and OPEN latches.
        if last_tick_nanos >= self.prev_last_tick_nanos.saturating_add(1_000_000_000) {
            // Liveness ADVANCED by at least a full second (a genuinely newer
            // tick; `prev == 0` = first observation, trivially satisfied).
            let mut counter_add = 0u64;
            if self.prev_last_tick_nanos != 0 {
                let delta_secs = (last_tick_nanos.saturating_sub(self.prev_last_tick_nanos)
                    / 1_000_000_000)
                    .max(0) as u64;
                if delta_secs >= FEED_GAP_COUNTER_FLOOR_SECS {
                    counter_add = delta_secs;
                }
            }
            self.prev_last_tick_nanos = last_tick_nanos;
            if let Some((last_tick_before, detect_nanos)) = self.open.take() {
                let gap_secs = (last_tick_nanos.saturating_sub(last_tick_before) / 1_000_000_000)
                    .max(0) as u64;
                return (
                    FeedGapEdge::Close {
                        detect_nanos,
                        last_tick_before_nanos: last_tick_before,
                        end_nanos: last_tick_nanos,
                        gap_secs,
                    },
                    counter_add,
                );
            }
            return (FeedGapEdge::None, counter_add);
        }
        // Liveness did NOT advance by a full second — silence continues (or
        // a sub-second creep/jitter holds steady; treated as silence).
        let age = (now_nanos.saturating_sub(last_tick_nanos).max(0) / 1_000_000_000) as u64;
        if self.open.is_none() && in_session && age >= FEED_GAP_EPISODE_THRESHOLD_SECS {
            self.open = Some((last_tick_nanos, now_nanos));
            return (
                FeedGapEdge::Open {
                    detect_nanos: now_nanos,
                    last_tick_nanos,
                },
                0,
            );
        }
        (FeedGapEdge::None, 0)
    }
}

/// IST "HH:MM" label for an IST-epoch-nanos instant (pure).
pub(crate) fn ist_hhmm_label(ist_nanos: i64) -> String {
    let secs_of_day = (ist_nanos / 1_000_000_000).rem_euclid(86_400);
    format!(
        "{:02}:{:02}",
        secs_of_day / 3_600,
        (secs_of_day % 3_600) / 60
    )
}

/// Comma list of the 1-minute bucket labels overlapped by `[start, end]`
/// (IST epoch nanos) — the NAMED partial bars a gap makes thin. Bounded to
/// [`FEED_GAP_PARTIAL_MINUTES_MAX`] entries + a trailing `…` marker (a
/// multi-hour gap can never produce an unbounded string). Pure + testable.
/// Annotation only — the bars themselves are never touched (live-feed
/// purity).
pub(crate) fn partial_minute_labels(start_ist_nanos: i64, end_ist_nanos: i64) -> String {
    if end_ist_nanos < start_ist_nanos {
        return String::new();
    }
    const MINUTE_NANOS: i64 = 60 * 1_000_000_000;
    let first = start_ist_nanos.div_euclid(MINUTE_NANOS);
    let last = end_ist_nanos.div_euclid(MINUTE_NANOS);
    let mut out = String::new();
    let mut emitted = 0usize;
    let mut bucket = first;
    while bucket <= last {
        if emitted >= FEED_GAP_PARTIAL_MINUTES_MAX {
            out.push_str(",…");
            break;
        }
        if emitted > 0 {
            out.push(',');
        }
        out.push_str(&ist_hhmm_label(bucket * MINUTE_NANOS));
        emitted += 1;
        bucket += 1;
    }
    out
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
/// per-wake clock read is plausible (same plausibility floor as
/// `validate_groww_tick`, which since F1 2026-07-03 REJECTS ticks outright
/// below it), so a broken clock writes NULL, never a ~1970 stamp. One clock
/// read per wake (O(1) preserved — no per-line syscall added).
fn row_received_at(receipt_ist_nanos: i64) -> Option<i64> {
    (receipt_ist_nanos > GROWW_MIN_PLAUSIBLE_RECEIPT_IST_NANOS).then_some(receipt_ist_nanos)
}

/// Per-ROW received_at (2026-07-03 per-callback instant capture): prefer the
/// sidecar's per-message `capture_ns` stamp (UTC epoch nanos, stamped inside
/// the NATS-callback hook — the true capture-at-receipt instant) converted to
/// the IST-nanos convention `received_at` already uses; fall back to the
/// per-wake receipt stamp for old-format / reconcile-sweep lines. The capture
/// stamp is accepted ONLY when plausible: above the same ~2020 floor as
/// [`row_received_at`] AND not ahead of the wake receipt clock beyond
/// [`GROWW_FUTURE_TS_TOLERANCE_NANOS`] (the sidecar and the bridge share the
/// host clock — a capture stamp "after" the wake that read it means a broken
/// clock, so fail toward the pre-PR wake stamp, never a garbage column).
/// Pure arithmetic — O(1), no clock read, no allocation per line.
fn row_received_at_with_capture(
    capture_ns_utc: Option<i64>,
    wake_receipt_ist_nanos: i64,
) -> Option<i64> {
    capture_stamp_ist_nanos(capture_ns_utc, wake_receipt_ist_nanos)
        .or_else(|| row_received_at(wake_receipt_ist_nanos))
}

/// The plausibility-gated per-message capture stamp ONLY (no wake-stamp
/// fallback) in IST nanos — the TRUSTED evidence of WHEN the sidecar actually
/// captured the line at its NATS callback. Extracted (2026-07-06 replayed-
/// backlog false-recovery fix) so the liveness/recovery signal can
/// distinguish a line captured FRESH this episode from a replayed
/// pre-disable / pre-respawn backlog line: only a stamp at/after the live
/// floor may latch `streaming_observed`. A line WITHOUT a capture stamp
/// (old-format / reconcile-sweep row) returns `None` — it still persists and
/// folds, but it can never prove liveness (the sidecar's ≤1s status rewrite
/// on real emits covers those flows via the freshness-gated status channel).
/// Pure arithmetic — O(1), no clock read, no allocation per line.
fn capture_stamp_ist_nanos(
    capture_ns_utc: Option<i64>,
    wake_receipt_ist_nanos: i64,
) -> Option<i64> {
    let ns = capture_ns_utc?;
    let capture_ist_nanos = ns.saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS);
    (capture_ist_nanos > GROWW_MIN_PLAUSIBLE_RECEIPT_IST_NANOS
        && capture_ist_nanos
            <= wake_receipt_ist_nanos.saturating_add(GROWW_FUTURE_TS_TOLERANCE_NANOS))
    .then_some(capture_ist_nanos)
}

/// Same-IST-day gate for the presence fold (scoreboard PR-D fix round 1,
/// review MEDIUM): `true` when the tick's own exchange stamp and the wake
/// receipt wall-clock fall on the SAME IST day number. The presence
/// bitsets discard the day component (`ts % 86400`), so a cross-day
/// NDJSON re-tail replay (rotation-failure degraded mode / byte-0
/// re-tail) would otherwise fold yesterday's session minutes into
/// today's slots as `in_memory` truth. Same-day replay stays idempotent
/// (fetch_or). Pure integer math — one divide + compare, zero-alloc.
fn tick_is_same_ist_day(tick_ts_ist_nanos: i64, wake_receipt_ist_nanos: i64) -> bool {
    const DAY_NANOS: i64 = 86_400 * 1_000_000_000;
    tick_ts_ist_nanos.div_euclid(DAY_NANOS) == wake_receipt_ist_nanos.div_euclid(DAY_NANOS)
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
        // O(1) EXEMPT: begin — per-WAKE bounded residual drain (capped by the
        // 4 MiB read budget), amortized across the lines it yields; documented
        // "O(n) single drain" in the fn docs. Not a per-tick allocation site.
        Some(last_nl) => residual.drain(..=last_nl).collect(),
        None => Vec::new(),
        // O(1) EXEMPT: end
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
        // O(1) EXEMPT: watcher-setup error path — once per bridge start.
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
    /// `Arc` so the auto-scale shard bridges (§34 PR-2) can draw from ONE
    /// process-wide counter — globally unique + monotonic across shards.
    /// The single-conn path owns a private instance (behavior-identical).
    capture_seq: Arc<AtomicI64>,
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
    /// Max plausibility-gated per-message capture stamp
    /// ([`capture_stamp_ist_nanos`]) across the lines parsed by the MOST
    /// RECENT `drain_new_data` call (reset each call; 0 = no trusted capture
    /// stamp this wake). 2026-07-06 replayed-backlog false-recovery fix: the
    /// run loop latches `streaming_observed` from the tick channel ONLY when
    /// this is at/after the live floor — a drained pre-disable / pre-respawn
    /// backlog (the sidecar keeps appending for ≤2s after a runtime disable;
    /// a respawned bridge byte-0 re-tails already-captured bytes) carries
    /// OLD capture stamps and must persist/fold WITHOUT firing FeedRecovered
    /// ("Prices are flowing") or pinning `tv_groww_ws_active = 1` before the
    /// relaunched sidecar has actually authenticated + subscribed + streamed.
    wake_max_capture_ist_nanos: i64,
    /// Byte-0 re-tail REPLAY-WINDOW flag (scoreboard PR-C review round 1,
    /// 2026-07-11) — the SECOND condition of the Groww lag stale-capture
    /// exclusion (`feed_lag_monitor::record_groww_tick`; the ≥60 s dwell
    /// alone also matches LIVE backlog drains — the ILP-backpressure pause
    /// + the respawn-backoff wake — whose lines were never recorded and
    /// must be ADMITTED to the day lag histogram). TRUE while drained
    /// bytes may REPLAY lines a previous bridge/process already recorded:
    /// set at construction (a fresh bridge without a proven same-day
    /// offset resume byte-0 re-tails), on a `len < offset` shrink reset,
    /// and re-armed after an archive-tail drain (the caller's byte-0
    /// live-file re-tail follows); cleared FALSE on a proven same-day
    /// snapshot resume and whenever a drain wake finishes fully caught up
    /// to the file end (everything after that is live). Affects ONLY the
    /// lag-sample classification — persist/fold/dedup are untouched.
    replay_window: bool,
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
    /// (Test-only convenience since §34 PR-2: production callers —
    /// `run_groww_bridge` + the shard fleet — pass an explicit shared
    /// counter via [`Self::with_aggregator_and_seq`].)
    #[cfg(test)]
    pub(crate) fn with_aggregator(qdb: &QuestDbConfig, aggregator: MultiTfAggregator) -> Self {
        Self::with_aggregator_and_seq(qdb, aggregator, Arc::new(AtomicI64::new(0)))
    }

    /// Build a bridge state around a CALLER-OWNED aggregator AND a
    /// CALLER-OWNED capture-seq counter (§34 auto-scale PR-2): the shard
    /// bridges all draw from ONE `Arc<AtomicI64>` so capture_seq stays
    /// globally unique + monotonic across N connections' NDJSON files.
    pub(crate) fn with_aggregator_and_seq(
        qdb: &QuestDbConfig,
        aggregator: MultiTfAggregator,
        capture_seq: Arc<AtomicI64>,
    ) -> Self {
        Self {
            live_writer: GrowwLiveTickWriter::new(qdb),
            aggregator,
            seeded: std::collections::HashSet::new(),
            capture_seq,
            offset: 0,
            // O(1) EXEMPT: bridge-state construction — once per bridge start, not per tick.
            residual: Vec::new(),
            last_offset_persist: None,
            ilp_backpressure_active: false,
            active_drain_ist_day: None,
            liveness_max_exchange_ts_nanos: 0,
            wake_max_capture_ist_nanos: 0,
            // A fresh bridge byte-0 re-tails until a resume proves a
            // preserved offset or a drain fully catches up — replayed
            // lines must not double-count in the day lag histogram.
            replay_window: true,
        }
    }

    /// TRUE when the most recent [`Self::drain_new_data`] call parsed at
    /// least one line whose per-message capture stamp is at/after
    /// `live_floor_ist_nanos` — i.e. the sidecar captured it FRESH this
    /// episode, not a replayed pre-disable / pre-respawn backlog line. The
    /// run loop gates the tick-channel `streaming_observed` latch (and only
    /// that — persist/fold stay unconditional, zero-loss) on this, mirroring
    /// the status channel's `groww_status_is_live` floor (2026-07-06
    /// replayed-backlog false-recovery fix). Pure — O(1) compare.
    #[must_use]
    pub(crate) fn wake_had_fresh_capture(&self, live_floor_ist_nanos: i64) -> bool {
        self.wake_max_capture_ist_nanos > 0
            && self.wake_max_capture_ist_nanos >= live_floor_ist_nanos
    }

    /// Read the first [`GROWW_OFFSET_HEAD_LEN`] bytes of the capture file (the
    /// identity guard), lossy-decoded. Empty on any failure (fail-safe: an
    /// empty head never matches a snapshot, forcing the byte-0 re-tail).
    async fn read_file_head(tick_file_path: &Path) -> String {
        let Ok(mut f) = File::open(tick_file_path).await else {
            // O(1) EXEMPT: resume/persist identity-guard fail-safe arm — cold path (boot resume + throttled offset persist).
            return String::new();
        };
        let mut buf = [0u8; GROWW_OFFSET_HEAD_LEN];
        let Ok(n) = f.read(&mut buf).await else {
            // O(1) EXEMPT: resume/persist identity-guard fail-safe arm — cold path.
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
                // The archive drain may have cleared the flag on catch-up;
                // the byte-0 live-file tail that follows is a re-tail.
                self.replay_window = true;
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
                // Proven same-day continuation from the flushed offset:
                // every byte past it was never read — no replay possible,
                // so post-downtime backlog lines keep their true lag in
                // the day histogram (review round 1, 2026-07-11).
                self.replay_window = false;
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
                // Same re-arm as the absent-file arm: the byte-0 re-tail
                // of the rotated/replaced live file follows.
                self.replay_window = true;
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
        // Reset the per-wake fresh-capture evidence BEFORE any early return
        // so a quiet/failed wake can never re-report the PREVIOUS wake's
        // freshness (2026-07-06 replayed-backlog false-recovery fix).
        self.wake_max_capture_ist_nanos = 0;
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
            // File shrank (rotation/truncate) — restart from the top. The
            // byte-0 re-tail may replay already-recorded lines, so the lag
            // classifier's replay window re-arms (review round 1,
            // 2026-07-11).
            self.offset = 0;
            self.residual.clear();
            self.replay_window = true;
        }
        if len == self.offset {
            return false; // nothing new
        }
        // The lag classification for THIS wake's lines uses the flag as it
        // stood when the bytes were read; a catch-up below clears it for
        // the NEXT wake only.
        let wake_replay_window = self.replay_window;
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
        // O(1) EXEMPT: per-WAKE read buffer, bounded by wake_read_budget (≤ 4 MiB take()); not per-tick.
        let mut chunk: Vec<u8> = Vec::new();
        match (&mut file).take(to_read).read_to_end(&mut chunk).await {
            Ok(n) => {
                self.offset = self.offset.saturating_add(n as u64);
                if self.offset >= len {
                    // Fully caught up to the file end sampled this wake —
                    // any replayable prefix is behind us; every later byte
                    // is live (the flag re-arms only on a fresh
                    // state/shrink/failed resume).
                    self.replay_window = false;
                }
            }
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
        // Fix #3 (2026-07-03 lag forensics) + per-callback capture: each row's
        // received_at prefers the sidecar's per-message capture_ns stamp (true
        // capture-at-receipt) and falls back to this per-wake receipt stamp —
        // plausibility-gated so a broken clock writes NULL. Makes per-feed lag
        // (received_at − ts) measurable in SQL, now at per-message fidelity.
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
            let mut parsed = match parse_groww_tick_line(line) {
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
            // Trusted per-message capture stamp (None for old-format /
            // reconcile-sweep rows): tracked as the wake max so the run loop
            // can gate the streaming latch on capture FRESHNESS — a replayed
            // pre-disable backlog line carries an OLD stamp and must never
            // prove liveness (2026-07-06 false-recovery fix). The persisted
            // `received_at` keeps the exact pre-fix semantics (stamp, else
            // per-wake fallback).
            let capture_ist = capture_stamp_ist_nanos(parsed.capture_ns, wake_receipt_ist_nanos);
            if let Some(c) = capture_ist {
                self.wake_max_capture_ist_nanos = self.wake_max_capture_ist_nanos.max(c);
            }
            // Scoreboard PR-C: Groww exchange→capture lag fold — operands
            // the drain ALREADY computed (zero new clock reads, zero
            // allocation). Lines without a capture stamp (old-format /
            // reconcile-sweep) and ≥60s-stale captures INSIDE a byte-0
            // re-tail replay window are EXCLUDED + counted inside
            // record_groww_tick; ≥60s-stale captures on a PRESERVED offset
            // (ILP-backpressure pause / respawn-backoff backlog — review
            // round 1, 2026-07-11) are ADMITTED to the day histogram only;
            // fresh admitted samples feed the trailing-60s ring (the
            // tv_groww_exchange_lag_p99_seconds publisher) + the day
            // histogram (the 15:45 scorecard lag columns) at Groww's
            // native MILLISECOND precision.
            tickvault_core::pipeline::feed_lag_monitor::record_groww_tick(
                capture_ist,
                wake_receipt_ist_nanos,
                parsed.tick.ts_ist_nanos,
                wake_replay_window,
            );
            // Scoreboard PR-D: per-instrument session-minute presence fold
            // — co-located with the lag fold above (post-validate). The
            // minute derives from the tick's own millisecond exchange
            // stamp (one integer division — zero new clock reads); the
            // (id, segment) key is the SAME composite the shared 21-TF
            // engine folds on. validate_groww_tick already rejected
            // security_id <= 0, so the u64 conversion cannot fail.
            // SAME-IST-DAY gate (PR-D review round 1, MEDIUM): the presence
            // bitsets carry no day component (`ts % 86400`), and
            // validate_groww_tick rejects FUTURE skew only — a cross-day
            // NDJSON re-tail replay (the documented rotation-failure
            // degraded mode / a byte-0 re-tail) would otherwise fold
            // YESTERDAY's session minutes into today's registered slots as
            // 'in_memory' truth. One divide + compare, zero-alloc — the
            // Dhan fold sites are already behind the tick processor's
            // is_today_ist stale-day gate.
            if tick_is_same_ist_day(parsed.tick.ts_ist_nanos, wake_receipt_ist_nanos) {
                tickvault_core::pipeline::feed_presence::record_presence(
                    Feed::Groww,
                    u64::try_from(parsed.tick.security_id).unwrap_or(0),
                    parsed.tick.segment.binary_code(),
                    u32::try_from(parsed.tick.ts_ist_nanos / 1_000_000_000).unwrap_or(0),
                );
            }
            let row_received =
                row_received_at_with_capture(parsed.capture_ns, wake_receipt_ist_nanos);
            // C2 (feed convergence): the ordered enrich → persist → aggregate
            // per-tick consumer sequence is the ONE shared `consume_feed_tick`
            // core (`tickvault_core::pipeline::feed_consumer`) — the same core
            // the Dhan tick processor delegates to. The core NEVER invokes the
            // enrich stage for Feed::Groww (no greeks/OI — plan edge E5), and
            // pins the persist-before-fold order. Monomorphized closures over
            // the private ParsedGrowwTick — zero-alloc, no dyn.
            let live_writer = &mut self.live_writer;
            let seeded = &mut self.seeded;
            let aggregator = &self.aggregator;
            tickvault_core::pipeline::feed_consumer::consume_feed_tick(
                Feed::Groww,
                &mut parsed,
                |_| {}, // Groww has no enrichment stage; the core gates it anyway.
                |p| {
                    if let Err(err) = live_writer.append_row(&live_tick_row(p, seq, row_received)) {
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
                },
                |p| {
                    // Fold through the SHARED 21-TF engine (ONE common candle engine).
                    let pt = match build_parsed_tick(p) {
                        Ok(pt) => pt,
                        Err(reason) => {
                            error!(
                                ?reason,
                                security_id = p.tick.security_id,
                                "groww bridge: tick cannot fold through the shared 21-TF engine (width guard)"
                            );
                            return;
                        }
                    };
                    let seg_code = pt.exchange_segment_code;
                    let key = (pt.security_id, seg_code);
                    let Ok(cum_volume_u64) = u64::try_from(p.tick.cum_volume) else {
                        error!(
                            security_id = p.tick.security_id,
                            cum_volume = p.tick.cum_volume,
                            "groww bridge: negative cum_volume past validation — dropping tick"
                        );
                        return;
                    };
                    // First sight: register + seed the cumulative baseline (idempotent).
                    if seeded.insert(key) {
                        aggregator.pre_populate(std::iter::once(key));
                        aggregator.seed_cumulative(pt.security_id, seg_code, cum_volume_u64);
                    }
                    // Route every sealed bar across ALL 21 TFs through the SHARED
                    // seal-writer chain, tagged feed=Groww.
                    let stats = aggregator.consume_tick(
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
                    if stats.late_count > 0 {
                        // AGGREGATOR-LATE-01 visibility (2026-07-03): Groww late
                        // discards were previously COMPLETELY silent — this path
                        // captured `stats` but never read `late_count` (no counter,
                        // no log), so a Discard-policy drop wave was invisible. Count
                        // them with the `feed=groww` label; the Dhan site keeps its
                        // label-less series (mirroring the `tv_seal_mpsc_dropped_total`
                        // per-feed labelling precedent in `seal_routing.rs`) so the
                        // existing tv-aggregator-late-tick-sustained alert series is
                        // not renamed or broken.
                        metrics::counter!("tv_aggregator_late_ticks_discarded_total", "feed" => "groww")
                            .increment(u64::from(stats.late_count));
                    }
                    if !stats.instrument_found {
                        aggregator.pre_populate(std::iter::once(key));
                    }
                },
            );
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

            // Scoreboard PR-C: reset the Groww DAY lag histogram at every
            // IST midnight — BEFORE the trading-day / feed-enabled gates
            // below (a Saturday-midnight `continue` must still clear
            // Friday's distribution before the next trading day fills it).
            // Cold, O(96); an empty/disabled feed's reset is a no-op.
            tickvault_core::pipeline::feed_lag_monitor::reset_day_lag_histogram(Feed::Groww);
            // Scoreboard PR-D: reset the Groww presence bitsets at the same
            // boundary (belt-and-braces — registration's day-change clear
            // is the backstop). Cold, O(slots × 6).
            tickvault_core::pipeline::feed_presence::reset_daily(Feed::Groww);

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
            // F2 self-heal (2026-07-03): restart the day's event-time
            // watermark from 0 so a POISONED watermark (garbage future-dated
            // tick advanced the never-regressing fetch_max past the
            // future-skew guard, disabling catch-up) self-heals within one
            // day and the next day's watermark rebuilds from the first real
            // tick (mirrors the Dhan Task 3 reset).
            aggregator.reset_watermark();
            info!(
                sealed,
                dropped,
                "Groww IST-midnight force-seal complete — open buckets flushed (feed=groww, watermark reset)"
            );
        }
    });
    info!("Groww bridge — IST-midnight force-seal task spawned (parity with Dhan)");
}

/// Spawn the watermark-aware per-minute catch-up seal driver for the Groww
/// aggregator (BOUNDARY-01, 2026-07-03).
///
/// Bounds candle seal lag WITHOUT mass-discarding backlogged ticks: every
/// [`tickvault_trading::candles::CATCHUP_SEAL_POLL_INTERVAL_SECS`] it reads
/// the Groww instance's event-time watermark (the max `exchange_timestamp`
/// ever consumed — advanced only by real, newer ticks, so a frozen-snapshot
/// re-dump never moves it) and gates via the shared pure
/// [`tickvault_trading::candles::compute_catchup_cutoff`]: scan ONLY when the
/// watermark ADVANCED since the last scan AND is not POISONED (further ahead
/// of the IST wall clock than
/// [`tickvault_trading::candles::CATCHUP_WATERMARK_FUTURE_SKEW_GUARD_SECS`]
/// allows); the cutoff is `min(watermark −`
/// [`tickvault_trading::candles::CATCHUP_SEAL_LATENESS_MARGIN_SECS_GROWW`]`,
/// now_ist)`. The Groww margin is a FULL MINUTE (vs Dhan's 5 s) because the
/// NDJSON path has measured per-subject delivery skew (snapshot freezes,
/// byte-0 re-tail) — a wider margin means only >60 s skew becomes a counted
/// discard, while seal lag stays bounded at ~65 s instead of unbounded. A
/// backlogged bridge (ILP-backpressure pause, post-restart re-tail) has a
/// trailing watermark, so its still-filling buckets are NEVER sealed ahead of
/// the backlog — the exact mass-discard failure a naive wall-clock force-seal
/// would cause under `LatePolicy::Discard`. A STALLED watermark (dead feed)
/// gets NO catch-up seals — FEED-STALL-01 owns the dead-feed page; there is
/// deliberately no "assume dead then force-seal anyway" escape hatch. A
/// POISONED watermark disables catch-up (coalesced BOUNDARY-01 error,
/// reason=watermark_future_skew) until the IST-midnight watermark reset
/// self-heals it.
///
/// Routing is the SAME Groww policy as the IST-midnight force-seal closure
/// above (feed=Groww, drop_d1=false, no prev-day pct-stamp, no Dhan
/// heartbeat) through the SAME shared seal-writer chain, so a later amend or
/// re-seal of the same bucket UPSERTs in place (DEDUP `ts, security_id,
/// segment, feed`; `ts` = bucket start, never wall-clock). The midnight
/// force-seal task keeps its cross-day duties (clear `last_sealed`, re-arm
/// day-open) — post-catch-up it is idempotent (`force_seal` on an emptied
/// slot returns `None`).
// TEST-EXEMPT: cold-path tokio interval driver (same supervision level as the
// sibling IST-midnight force-seal task). The load-bearing logic is unit-tested
// in the trading crate (catch_up_seal / catch_up_seal_all / watermark tests);
// the routing params are pinned by test_groww_force_seal_routes_with_feed_groww
// (identical SealRouteParams).
fn spawn_groww_catchup_seal(
    aggregator: MultiTfAggregator,
    feed_runtime: Arc<FeedRuntimeState>,
    trading_calendar: Arc<tickvault_common::trading_calendar::TradingCalendar>,
) {
    use tickvault_trading::candles::{
        CATCHUP_SEAL_LATENESS_MARGIN_SECS_GROWW, CATCHUP_SEAL_POLL_INTERVAL_SECS,
        CATCHUP_WATERMARK_FUTURE_SKEW_GUARD_SECS, compute_catchup_cutoff,
    };
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(Duration::from_secs(CATCHUP_SEAL_POLL_INTERVAL_SECS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_scanned_watermark: u32 = 0;
        // Edge latch for the poisoned-watermark error — ONE coalesced line
        // per poisoning episode (audit Rule 4), not one per 5 s wave.
        let mut poison_logged = false;
        loop {
            interval.tick().await;
            let watermark = aggregator.watermark_secs();
            // IST wall-clock now (epoch seconds) via the bridge's EXISTING
            // clock helper (`receipt_ist_nanos` — the same path the
            // feed-health math uses). A broken clock returns a ~1970 value →
            // every watermark looks poisoned → catch-up stays disabled
            // (fail-closed, BOOT-03-class posture, matching the F1
            // fail-closed validator).
            let now_ist_secs = u32::try_from(receipt_ist_nanos() / 1_000_000_000).unwrap_or(0);
            // Shared pure gate: self-gate (watermark unchanged), no-tick-yet,
            // and the poisoned-watermark defense in one testable place.
            let Some(cutoff) = compute_catchup_cutoff(
                watermark,
                last_scanned_watermark,
                now_ist_secs,
                CATCHUP_SEAL_LATENESS_MARGIN_SECS_GROWW,
                CATCHUP_WATERMARK_FUTURE_SKEW_GUARD_SECS,
            ) else {
                // Only the poisoned arm is observable: watermark non-zero and
                // advanced, yet the gate refused — it sits past now + guard.
                // last_scanned is NOT updated, so scanning resumes the moment
                // the watermark self-heals (IST-midnight reset_watermark).
                if watermark != 0 && watermark != last_scanned_watermark {
                    metrics::counter!(
                        "tv_boundary_catchup_skipped_total",
                        "feed" => "groww", "reason" => "future_skew"
                    )
                    .increment(1);
                    if !poison_logged {
                        poison_logged = true;
                        error!(
                            code = tickvault_common::error_code::ErrorCode::Boundary01CatchupSeal
                                .code_str(),
                            reason = "watermark_future_skew",
                            feed = "groww",
                            watermark_secs = watermark,
                            now_ist_secs,
                            "BOUNDARY-01: poisoned event-time watermark (further ahead of the \
                             IST wall clock than host skew allows) — catch-up sealing disabled \
                             until the IST-midnight watermark reset self-heals it"
                        );
                    }
                }
                continue;
            };
            poison_logged = false;
            // Trading-day + runtime-enable gates, mirroring the IST-midnight
            // task. The enable flag is read into a local so this gate does
            // not collide with the `run_groww_bridge` disable-branch
            // source-scan anchor (which `find`s the FIRST literal occurrence).
            if !trading_calendar.is_trading_day_today() {
                continue;
            }
            let groww_enabled_for_catchup = feed_runtime.is_enabled(Feed::Groww);
            if !groww_enabled_for_catchup {
                continue;
            }
            let Some(sender) = global_seal_sender() else {
                // Seal-writer not installed yet — retry next wave WITHOUT
                // consuming the watermark advance (no seal is lost).
                continue;
            };
            last_scanned_watermark = watermark;
            let sealed =
                aggregator.catch_up_seal_all(cutoff, |security_id, segment_code, tf, state| {
                    metrics::counter!("tv_boundary_catchup_total", "feed" => "groww").increment(1);
                    // SAME Groww routing policy as the IST-midnight force-seal
                    // closure: feed=Groww, drop_d1=false (Groww routes D1),
                    // no prev-day pct-stamp, no Dhan heartbeat. A full-mpsc
                    // drop is already counted by route_seal
                    // (`tv_seal_mpsc_dropped_total{feed=groww}`).
                    crate::seal_routing::route_seal(
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
                    );
                });
            if sealed > 0 {
                // ONE coalesced line per scan wave — never per-seal spam.
                warn!(
                    code =
                        tickvault_common::error_code::ErrorCode::Boundary01CatchupSeal.code_str(),
                    feed = "groww",
                    seals = sealed,
                    cutoff_secs = cutoff,
                    watermark_secs = watermark,
                    "BOUNDARY-01: watermark catch-up sealed lagging candle bucket(s) — \
                     late but correct; buckets past the watermark stay open for the backlog"
                );
            }
        }
    });
    info!("Groww bridge — watermark catch-up seal task spawned (BOUNDARY-01)");
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
        // O(1) EXEMPT: IST-midnight archive-path naming — once per day/rotation, cold path.
        .map(|d| d.format("%Y%m%d").to_string())
        .unwrap_or_default();
    // O(1) EXEMPT: archive-path naming — once per rotation, cold path.
    let name = format!("live-ticks-{date}.ndjson");
    tick_file_path
        .parent()
        .map(|d| d.join(&name))
        .unwrap_or_else(|| PathBuf::from(name))
}

/// Rotation-at-open action for a stale (previous-IST-day) capture file —
/// decided by [`decide_capture_rotate_at_open`] (pure). 2026-07-13
/// disk-retention hardening, REDESIGNED after review round 1 (F1+F2): the
/// rotation lives in RUST and runs synchronously BEFORE the Groww bridge +
/// sidecar supervisor spawns, so (F1) the bridge's one-shot archive-drain
/// decision always sees the archive (no sidecar-vs-bridge boot race — only
/// Rust renames at open, and the rename completes before either process/task
/// starts), and (F2) the archive is NAMED BY THE BRIDGE'S SNAPSHOT DAY
/// (`archive_path_for_ist_day(snap.ist_day)`) — exactly the name
/// `drain_archive_tail_if_needed` probes — so a snapshot stuck on an older
/// day (QuestDB degraded before the last stop) still finds its archive and
/// drains from the persisted offset to EOF.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureRotateAction {
    /// Live file absent, empty, or last written TODAY (in-day restart) —
    /// no rotation, append continues.
    Keep,
    /// Rename the live file to the dated archive for `archive_day`.
    /// `named_by_snapshot` = the day came from the bridge's offset snapshot
    /// (the drain lookup key — the F2-correct name); `false` = no usable
    /// snapshot (first-boot edge) → fall back to the file-mtime day and log
    /// loudly.
    Rotate {
        /// IST day index the archive is named for.
        archive_day: i64,
        /// Whether the day came from the bridge snapshot (vs mtime fallback).
        named_by_snapshot: bool,
    },
    /// The target archive name already exists (only reachable after a
    /// dev-Mac in-process midnight rotation the same day — prod never
    /// crosses midnight). Do NOT overwrite: leave the live file in place
    /// (= pre-PR behavior for this boot: zero loss, no reclaim — honest
    /// degrade, logged loudly).
    SkipCollision {
        /// IST day index whose archive name collided.
        archive_day: i64,
    },
}

/// Pure decision core for the boot-time capture rotation-at-open.
///
/// Name precedence: a snapshot from a provable PREVIOUS day (`0 < snap_day <
/// today`) names the archive — that is the exact name the bridge's drain
/// probes (F2 by construction; the legacy `ist_day == 0` snapshot is not
/// trusted, mirroring [`should_drain_archive`]). Otherwise the file-mtime
/// day names it (first-boot fallback; the drain has no snapshot to resume
/// from anyway). NOTE (F5): a file rotated at open may span MULTIPLE IST
/// days (e.g. snapshot stuck on Thursday, file written through Friday) —
/// the archive carries the SNAPSHOT day in its name as the drain/resume
/// key, and the drain reads from the persisted offset to EOF, so every
/// spanned day's rows still drain; the dated S3 object name is a forensic
/// note of the resume day, not a content bound.
#[must_use]
pub fn decide_capture_rotate_at_open(
    mtime_ist_day: i64,
    today_ist_day: i64,
    snapshot_ist_day: Option<i64>,
    target_exists: bool,
) -> CaptureRotateAction {
    if mtime_ist_day >= today_ist_day {
        // Same-day restart (append must continue) or future mtime (clock
        // skew — never rotate today's capture out on uncertainty).
        return CaptureRotateAction::Keep;
    }
    let (archive_day, named_by_snapshot) = match snapshot_ist_day {
        Some(d) if d > 0 && d < today_ist_day => (d, true),
        _ => (mtime_ist_day, false),
    };
    if target_exists {
        CaptureRotateAction::SkipCollision { archive_day }
    } else {
        CaptureRotateAction::Rotate {
            archive_day,
            named_by_snapshot,
        }
    }
}

/// IST day index for a unix-epoch-seconds instant (the sidecar's `_ist_day`
/// boundary: `(secs + 19800) / 86400`).
#[must_use]
pub fn ist_day_from_unix_secs(unix_secs: i64) -> i64 {
    unix_secs
        .saturating_add(i64::from(
            tickvault_common::constants::IST_UTC_OFFSET_SECONDS,
        ))
        .div_euclid(i64::from(tickvault_common::constants::SECONDS_PER_DAY))
}

/// Boot-time capture rotation-at-open with an injected `today` — the
/// testable core of [`rotate_stale_groww_capture_at_open`]. Sync std::fs on
/// the COLD boot path, called strictly BEFORE the bridge + sidecar spawns
/// (ordering pinned by `disk_retention_wiring_guard.rs`). Returns the
/// archive path when a rotation happened. Failure never blocks boot — the
/// live file is simply left in place (pre-PR behavior).
pub fn rotate_stale_groww_capture_at_open_at(
    tick_file_path: &Path,
    today_ist_day: i64,
) -> Option<PathBuf> {
    let meta = std::fs::metadata(tick_file_path).ok()?;
    if meta.len() == 0 {
        return None; // empty file — nothing worth archiving
    }
    let mtime_secs = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    // APPROVED: mtime seconds fit i64 for any realistic wall clock; cold boot path.
    let mtime_ist_day = ist_day_from_unix_secs(mtime_secs.as_secs() as i64);
    // Best-effort snapshot read (sync — cold boot path, before any spawn).
    let snapshot_ist_day = std::fs::read_to_string(offset_snapshot_path(tick_file_path))
        .ok()
        .and_then(|raw| serde_json::from_str::<GrowwOffsetSnapshot>(&raw).ok())
        .map(|snap| snap.ist_day);
    let candidate_day = match snapshot_ist_day {
        Some(d) if d > 0 && d < today_ist_day => d,
        _ => mtime_ist_day,
    };
    let target = archive_path_for_ist_day(tick_file_path, candidate_day);
    match decide_capture_rotate_at_open(
        mtime_ist_day,
        today_ist_day,
        snapshot_ist_day,
        target.exists(),
    ) {
        CaptureRotateAction::Keep => None,
        CaptureRotateAction::SkipCollision { archive_day } => {
            warn!(
                live = %tick_file_path.display(),
                target = %target.display(),
                archive_day,
                "groww capture rotate-at-open: target archive already exists \
                 (in-process midnight rotation same day) — NOT overwriting; \
                 live file left in place (no reclaim this boot, zero loss)"
            );
            None
        }
        CaptureRotateAction::Rotate {
            archive_day,
            named_by_snapshot,
        } => {
            if !named_by_snapshot {
                warn!(
                    live = %tick_file_path.display(),
                    mtime_ist_day,
                    "groww capture rotate-at-open: no usable bridge offset \
                     snapshot — archive named by the file's last-write (mtime) \
                     day; the bridge will byte-0 re-tail today's fresh file \
                     (first-boot edge)"
                );
            }
            match std::fs::rename(tick_file_path, &target) {
                Ok(()) => {
                    info!(
                        archive = %target.display(),
                        bytes = meta.len(),
                        archive_day,
                        named_by_snapshot,
                        "groww capture rotated at open (stale previous-IST-day \
                         file archived BEFORE the bridge + sidecar spawn; the \
                         file may span multiple IST days — the dated name is \
                         the bridge's drain/resume key, and the drain reads to \
                         EOF)"
                    );
                    Some(target)
                }
                Err(err) => {
                    warn!(
                        live = %tick_file_path.display(),
                        error = %err,
                        "groww capture rotate-at-open: rename failed — live \
                         file left in place (capture unaffected; no reclaim \
                         this boot)"
                    );
                    None
                }
            }
        }
    }
}

/// Wall-clock wrapper over [`rotate_stale_groww_capture_at_open_at`].
pub fn rotate_stale_groww_capture_at_open(tick_file_path: &Path) -> Option<PathBuf> {
    rotate_stale_groww_capture_at_open_at(
        tick_file_path,
        ist_day_from_ist_nanos(receipt_ist_nanos()),
    )
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
    /// Boot-stage socket-connected announcement (operator 2026-07-04 — Groww
    /// boot-visibility parity): set once the "feed connected — subscribed N —
    /// awaiting first tick" Telegram + the boot-time `ws_event_audit`
    /// Connected row have fired for the CURRENT connected episode. Re-armed
    /// ONLY on a genuine disconnect falling edge (feed disable / bridge
    /// death) — never per poll turn, never per reconnect attempt — so a
    /// closed-market boot announces exactly once and a genuine reconnect
    /// announces exactly once more.
    pub boot_connect_announced: std::sync::atomic::AtomicBool,
    /// Feed-down alerting (operator 2026-07-06): a `FeedDown` Telegram was
    /// consumed for the CURRENT down episode — set once on the FIRST falling
    /// edge (feed disable / bridge death), cleared only when the streaming
    /// rising edge fires the honest `FeedRecovered`. Edge-triggered per
    /// audit-findings Rule 4: one DOWN page per episode, never per idle turn.
    pub down_announced: std::sync::atomic::AtomicBool,
    /// IST epoch SECONDS of the FIRST falling edge of the current down
    /// episode (0 = not down). First-down-wins CAS (mirror of the Dhan
    /// `record_disconnect` total-downtime semantics) so the recovery's
    /// `down_secs` spans intermediate retry failures / repeated bridge
    /// deaths, not just the latest one.
    pub down_since_epoch_secs: std::sync::atomic::AtomicU64,
}

impl GrowwAuditLatches {
    /// Consume the boot-connect edge: returns `true` exactly once per
    /// connected episode (first caller wins; subsequent polls read `false`
    /// until [`Self::rearm_boot_connect`]).
    pub fn try_announce_boot_connect(&self) -> bool {
        !self
            .boot_connect_announced
            .swap(true, std::sync::atomic::Ordering::Relaxed)
    }

    /// Re-arm the boot-connect edge on a GENUINE disconnect falling edge
    /// (feed disable / bridge death) so the next observed subscribe state
    /// announces one fresh "connected — awaiting first tick".
    pub fn rearm_boot_connect(&self) {
        self.boot_connect_announced
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    /// Consume the feed-DOWN falling edge (operator 2026-07-06): stamps the
    /// episode's first-down instant (first-down-wins CAS — a second falling
    /// edge inside the same episode keeps the original stamp so `down_secs`
    /// spans the whole episode) and returns `true` exactly once per DOWN
    /// episode. The caller fires ONE `FeedDown` Telegram on `true`; repeated
    /// falling edges (bridge respawn storms, idle disable turns) read `false`
    /// until [`Self::take_feed_recovery`] closes the episode.
    pub fn try_announce_feed_down(&self, now_epoch_secs: u64) -> bool {
        // First-down-wins: only stamp when no episode is in progress (0).
        // A lost CAS means an earlier falling edge already stamped this
        // episode — exactly the desired outcome, so the Err is discarded
        // into a named binding (must_use satisfied, no behavior on Err).
        let _first_down_stamp = self.down_since_epoch_secs.compare_exchange(
            0,
            now_epoch_secs.max(1),
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
        );
        !self
            .down_announced
            .swap(true, std::sync::atomic::Ordering::Relaxed)
    }

    /// Close the feed-DOWN episode on the STREAMING rising edge: returns
    /// `Some(down_secs)` exactly once per episode (and re-arms the latch for
    /// the next episode), `None` when no episode is in progress. The caller
    /// fires ONE honest `FeedRecovered` Telegram on `Some` — recovery is
    /// claimed on ticks/streaming, never on mere socket-connect. Call ONLY
    /// when the Telegram notifier is deliverable: an unfilled boot-ordering
    /// slot must DEFER (skip the call) so the episode is delayed, never lost.
    pub fn take_feed_recovery(&self, now_epoch_secs: u64) -> Option<u64> {
        if !self
            .down_announced
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            return None;
        }
        let since = self
            .down_since_epoch_secs
            .swap(0, std::sync::atomic::Ordering::Relaxed);
        Some(now_epoch_secs.saturating_sub(since))
    }
}

/// Pure decision for the boot-stage socket-connected announcement
/// (operator 2026-07-04 parity). Announce ONLY when:
/// - the sidecar status file reported a KNOWN subscribe state (`subscribed`
///   or `streaming` — never the forward-compat `Unknown` tag), AND
/// - this connected episode has not been announced yet (edge latch), AND
/// - the Telegram notifier is deliverable (`notifier_ready`) — a slot that
///   exists but is not yet filled (boot ordering) defers to the NEXT wake
///   instead of consuming the edge, so the announcement is delayed, never
///   silently lost.
#[must_use]
pub fn should_announce_boot_connect(
    status_known: bool,
    already_announced: bool,
    notifier_ready: bool,
) -> bool {
    status_known && !already_announced && notifier_ready
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
// APPROVED: supervision wiring — every arg is a distinct owned resource
#[allow(clippy::too_many_arguments)]
// TEST-EXEMPT: supervision loop (spawn/await/sleep); the pure backoff curve is unit-tested (test_bridge_respawn_backoff_curve) and the wiring is pinned by per_feed_boot_isolation_guard + test_spawn_supervised_groww_bridge_owns_aggregator_and_force_seal_once.
pub fn spawn_supervised_groww_bridge(
    qdb: QuestDbConfig,
    tick_file_path: PathBuf,
    status_file_path: PathBuf,
    feed_runtime: Arc<FeedRuntimeState>,
    feed_health: Arc<tickvault_common::feed_health::FeedHealthRegistry>,
    ws_audit_tx: Option<tokio::sync::mpsc::Sender<WsEventAuditRow>>,
    // 2026-07-04 boot-visibility parity: the same lazily-filled Telegram slot
    // the sidecar supervisor + activation watcher share — carries the ONE
    // "feed connected — subscribed N — awaiting first tick" boot ping.
    // `None` (tests) = audit row only, no Telegram.
    notifier_slot: Option<
        Arc<arc_swap::ArcSwapOption<tickvault_core::notification::NotificationService>>,
    >,
    trading_calendar: Arc<tickvault_common::trading_calendar::TradingCalendar>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Process-lifetime state: ONE aggregator (shared papaya map) + ONE
        // IST-midnight force-seal task, regardless of how often the inner
        // bridge task respawns.
        let aggregator = MultiTfAggregator::with_capacity(GROWW_AGGREGATOR_CAPACITY);
        spawn_groww_ist_midnight_force_seal(
            // O(1) EXEMPT: spawn-time wiring (once per process) — MultiTfAggregator::clone shares the same papaya Arc.
            aggregator.clone(),
            Arc::clone(&feed_runtime),
            Arc::clone(&trading_calendar),
        );
        // BOUNDARY-01 (2026-07-03): watermark-aware per-minute catch-up seal —
        // spawned ONCE alongside the midnight force-seal (outside the respawn
        // loop, same process-lifetime ownership) so bridge respawns never leak
        // duplicate driver tasks. The clone shares the SAME papaya map + the
        // SAME event-time watermark the bridge's consume path advances.
        spawn_groww_catchup_seal(
            // O(1) EXEMPT: spawn-time wiring (once per process) — shares the same papaya Arc.
            aggregator.clone(),
            Arc::clone(&feed_runtime),
            trading_calendar,
        );
        // Process-lifetime ws_event_audit edge latches (finding M3): survive a
        // bridge respawn so the forensic chain stays Connected → Disconnected →
        // Reconnected, never Connected → Connected.
        let audit_latches = Arc::new(GrowwAuditLatches::default());
        // Single-conn path: a fresh private counter — behavior-identical to
        // the pre-scale bridge (the shard path shares one across conns).
        let capture_seq = Arc::new(AtomicI64::new(0));
        supervise_groww_bridge_loop(
            qdb,
            tick_file_path,
            status_file_path,
            feed_runtime,
            feed_health,
            ws_audit_tx,
            notifier_slot,
            aggregator,
            audit_latches,
            capture_seq,
            None,
        )
        .await;
    })
}

/// One connection's supervised respawn loop (extracted from
/// `spawn_supervised_groww_bridge` for §34 PR-2 shard reuse — the
/// FEED-SUPERVISOR-01 / WS-GAP-05 pattern, byte-identical semantics).
/// `conn_id = None` = the single-conn path; `Some(n)` = shard bridge `n`
/// under the auto-scale fleet (log context only — same respawn policy).
// TEST-EXEMPT: supervision loop (spawn/await/sleep); the pure backoff curve is unit-tested (test_bridge_respawn_backoff_curve) and the shared-seq + shard-path contracts have their own unit tests.
// APPROVED: supervision wiring — every arg is a distinct owned resource
#[allow(clippy::too_many_arguments)]
async fn supervise_groww_bridge_loop(
    qdb: QuestDbConfig,
    tick_file_path: PathBuf,
    status_file_path: PathBuf,
    feed_runtime: Arc<FeedRuntimeState>,
    feed_health: Arc<tickvault_common::feed_health::FeedHealthRegistry>,
    ws_audit_tx: Option<tokio::sync::mpsc::Sender<WsEventAuditRow>>,
    notifier_slot: Option<
        Arc<arc_swap::ArcSwapOption<tickvault_core::notification::NotificationService>>,
    >,
    aggregator: MultiTfAggregator,
    audit_latches: Arc<GrowwAuditLatches>,
    capture_seq: Arc<AtomicI64>,
    conn_id: Option<usize>,
) {
    {
        let mut consecutive_respawns: u32 = 0;
        loop {
            let started = std::time::Instant::now();
            let handle = tokio::spawn(run_groww_bridge(
                // O(1) EXEMPT: begin — supervisor respawn wiring (at most one per
                // bridge death + backoff): Arc/PathBuf/config handle clones, cold path.
                qdb.clone(),
                tick_file_path.clone(),
                status_file_path.clone(),
                Arc::clone(&feed_runtime),
                Arc::clone(&feed_health),
                ws_audit_tx.clone(),
                notifier_slot.clone(),
                aggregator.clone(),
                // O(1) EXEMPT: end
                Arc::clone(&audit_latches),
                Arc::clone(&capture_seq),
                conn_id,
            ));
            let reason = match handle.await {
                // PANIC HONESTY (house standard: tick-flush-worker-error-codes.md
                // §1): this arm engages in UNWIND (dev/test) builds only — the
                // workspace release profile sets `panic = "abort"`, so in the
                // production binary a panic inside run_groww_bridge ABORTS the
                // whole process before a JoinError can exist. Release-build
                // recovery for that class is the EXTERNAL process restart +
                // the boot Telegram chain, never this supervisor.
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
                // 2026-07-04 parity: a bridge death is a genuine disconnect —
                // re-arm the boot-connect edge so the respawned bridge
                // announces the (re)connected socket exactly once.
                audit_latches.rearm_boot_connect();
                // Feed-down alerting (operator 2026-07-06): the connected
                // feed just went DOWN — the previous DB-row-only silence was
                // the incident class. ONE FeedDown Telegram per DOWN episode
                // (latched; a respawn storm never re-pages), fail-open when
                // the lazily-filled slot is still empty at boot ordering
                // (audit row above is already written; the page is skipped,
                // never blocks the respawn). Cold path — event-driven, never
                // per tick. The connected-level gauge drops to 0 so the
                // CloudWatch inactivity alarm sees the outage too.
                //
                // PANIC HONESTY (tick-flush-worker-error-codes.md §1 house
                // standard): this whole bridge-death falling-edge block
                // (audit row + gauge→0 + FeedDown page) is reachable for
                // non-panic exits / UNWIND (dev/test) builds ONLY. In the
                // release (panic = "abort") binary a bridge panic — e.g. an
                // overflow-checks trip — aborts the PROCESS: no FeedDown
                // page fires, and the gauge goes MISSING (the exporter is
                // gone; notBreaching keeps the groww-ws-inactive alarm
                // SILENT), not 0. Visibility + recovery for that class are
                // the external process restart + the boot/StartupComplete
                // Telegram chain. No in-process panic self-healing is
                // claimed for release builds.
                metrics::gauge!("tv_groww_ws_active").set(0.0);
                if audit_latches.try_announce_feed_down(ist_epoch_secs_now())
                    && let Some(notifier) = notifier_slot.as_ref().and_then(|slot| slot.load_full())
                {
                    notifier.notify(tickvault_core::notification::NotificationEvent::FeedDown {
                        feed: Feed::Groww.display_name().to_string(), // O(1) EXEMPT: cold path — once per DOWN episode
                        reason: "internal restart — recovering automatically".to_string(), // O(1) EXEMPT: cold path — once per DOWN episode
                        // Calendar-aware (2026-07-06 fix): never page High +
                        // SNS on a Saturday manual run / NSE weekday holiday
                        // (the 2026-05-09 false-page class).
                        market_open: tickvault_common::market_hours::is_trading_session_now(),
                        // A bridge death that REACHES this arm is
                        // auto-recovering (the supervisor respawns it), so
                        // the auto-retry trailer is honest for every exit
                        // that can fire this page. A release-build panic
                        // never reaches here (panic = "abort" — process
                        // abort; see the PANIC HONESTY note above): recovery
                        // there is the external process restart.
                        operator_initiated: false,
                    });
                }
            }
            error!(
                code =
                    tickvault_common::error_code::ErrorCode::FeedSupervisor01Respawned.code_str(),
                reason,
                consecutive_respawns,
                conn_id = conn_id.map_or(-1i64, |c| c as i64),
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
    }
}

/// One shard's derived file paths under the auto-scale layout (§34 PR-2):
/// `data/groww/shards/c<NN>/` holds that connection's watch file, NDJSON
/// tick capture, and PROOF status file. Pure — path math only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrowwShardFiles {
    /// Zero-based connection id (matches the cutter's `GrowwShard::conn_id`).
    pub conn_id: usize,
    /// The per-conn watch DIRECTORY the Rust side writes the shard watch
    /// file into and the sidecar polls (`GROWW_WATCH_DIR`).
    pub watch_dir: PathBuf,
    /// Per-conn capture-at-receipt NDJSON (`GROWW_TICK_FILE`).
    pub tick_file: PathBuf,
    /// Per-conn connect+subscribe PROOF status file (`GROWW_STATUS_FILE`).
    pub status_file: PathBuf,
}

/// Root under which per-conn shard directories live.
pub const GROWW_SHARDS_DIR_DEFAULT: &str = "data/groww/shards";

/// Pure path derivation for shard `conn_id` (zero-padded 2-digit directory:
/// `c00` … `c99` — the config hard max is 100 conns, so 2 digits always
/// sort correctly in `ls`).
#[must_use]
pub fn shard_files(shards_root: &Path, conn_id: usize) -> GrowwShardFiles {
    // O(1) EXEMPT: fleet path derivation — once per connection spawn/scale event, cold path.
    let dir = shards_root.join(format!("c{conn_id:02}"));
    GrowwShardFiles {
        conn_id,
        // O(1) EXEMPT: PathBuf clone at shard-path derivation — cold fleet path.
        watch_dir: dir.clone(),
        tick_file: dir.join("live-ticks.ndjson"),
        status_file: dir.join("groww-status.json"),
    }
}

/// Best-effort removal of one connection's subscribe-proof status file
/// (hardening 2026-07-06): the status path is undated and the sidecar never
/// deletes it, so a killed connection would otherwise keep "proving" its
/// subscription forever — pinning the ladder's plateau gate at the fleet's
/// historical maximum. Called on fleet scale-down (after the child abort)
/// and by the ladder's start-of-run sweep. Returns `true` when a file was
/// actually removed; a missing file is a clean no-op.
pub(crate) fn remove_subscribe_proof(shards_root: &Path, conn_id: usize) -> bool {
    let status_file = shard_files(shards_root, conn_id).status_file;
    // O(1) EXEMPT: best-effort proof-file removal — fleet scale-down / boot sweep, cold path.
    match std::fs::remove_file(&status_file) {
        Ok(()) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => {
            // Best-effort: the ladder's freshness gate ages the leftover
            // out within its bound, so this is degraded, never silent.
            tracing::warn!(
                conn_id,
                error = %err,
                path = %status_file.display(),
                "groww shard scale-down: subscribe-proof status file removal failed"
            );
            false
        }
    }
}

/// §34 PR-2 Item 7: ONE process-lifetime aggregator + midnight force-seal +
/// catch-up seal + ONE shared capture-seq counter, then one supervised
/// bridge tail-loop PER SHARD FILE — all folding into the SAME papaya map
/// and the SAME `ticks`/`candles_*` write chain the single-conn path uses.
///
/// Bridges are spawned for EVERY conn id up to `max_conns` upfront: a bridge
/// tailing a not-yet-created shard file idles at zero cost (the tail loop
/// already handles a missing file), so bridge lifecycle never has to track
/// the ladder — only the sidecar PROCESSES scale up/down.
// APPROVED: supervision wiring — every arg is a distinct owned resource
#[allow(clippy::too_many_arguments)]
// TEST-EXEMPT: composition-only spawn wiring; the pieces it composes (supervise_groww_bridge_loop via the single-conn wrapper, shard_files, next_capture_seq sharing) are unit-tested, and per_feed_boot_isolation_guard pins the single-conn path.
pub fn spawn_supervised_groww_shard_bridges(
    qdb: QuestDbConfig,
    shards_root: PathBuf,
    max_conns: usize,
    feed_runtime: Arc<FeedRuntimeState>,
    feed_health: Arc<tickvault_common::feed_health::FeedHealthRegistry>,
    ws_audit_tx: Option<tokio::sync::mpsc::Sender<WsEventAuditRow>>,
    // 2026-07-04 boot-visibility parity — see `spawn_supervised_groww_bridge`.
    notifier_slot: Option<
        Arc<arc_swap::ArcSwapOption<tickvault_core::notification::NotificationService>>,
    >,
    trading_calendar: Arc<tickvault_common::trading_calendar::TradingCalendar>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Process-lifetime shared state — created ONCE for the whole fleet
        // (mirrors the single-conn wrapper): one aggregator, one midnight
        // force-seal, one catch-up seal, one capture-seq counter.
        let aggregator = MultiTfAggregator::with_capacity(GROWW_AGGREGATOR_CAPACITY);
        spawn_groww_ist_midnight_force_seal(
            // O(1) EXEMPT: fleet spawn-time wiring (once per process) — shares the same papaya Arc.
            aggregator.clone(),
            Arc::clone(&feed_runtime),
            Arc::clone(&trading_calendar),
        );
        spawn_groww_catchup_seal(
            // O(1) EXEMPT: fleet spawn-time wiring (once per process) — shares the same papaya Arc.
            aggregator.clone(),
            Arc::clone(&feed_runtime),
            trading_calendar,
        );
        let capture_seq = Arc::new(AtomicI64::new(0));
        let mut handles = Vec::with_capacity(max_conns);
        for conn_id in 0..max_conns {
            let files = shard_files(&shards_root, conn_id);
            // Per-conn audit latches: each connection carries its own
            // Connected/Disconnected/Reconnected forensic chain (the
            // ws_event_audit rows stay per-connection honest).
            let audit_latches = Arc::new(GrowwAuditLatches::default());
            handles.push(tokio::spawn(supervise_groww_bridge_loop(
                // O(1) EXEMPT: begin — per-conn fleet spawn wiring (once per connection
                // at scale-up): Arc/config handle clones, cold path.
                qdb.clone(),
                files.tick_file,
                files.status_file,
                Arc::clone(&feed_runtime),
                Arc::clone(&feed_health),
                ws_audit_tx.clone(),
                notifier_slot.clone(),
                aggregator.clone(),
                // O(1) EXEMPT: end
                audit_latches,
                Arc::clone(&capture_seq),
                Some(conn_id),
            )));
        }
        info!(
            max_conns,
            root = %shards_root.display(),
            "[feeds] groww shard bridges spawned (idle until their shard files appear)"
        );
        for h in handles {
            let _join = h.await;
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
// APPROVED: I/O driver wiring — every arg is a distinct owned resource
#[allow(clippy::too_many_arguments)]
// The pure primitives (parse_groww_tick_line, parse_groww_status_line,
// segment_from_str, live_tick_row, build_parsed_tick, next_capture_seq) + the
// GrowwBridgeState ingestion body are unit/integration-tested below + in
// groww_live_pipeline_e2e.
// TEST-EXEMPT: event-driven file-tail + live-QuestDB ILP I/O driver.
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
    // Boot-visibility parity (operator 2026-07-04): the lazily-filled
    // Telegram slot for the ONE "feed connected — subscribed N — awaiting
    // first tick" boot ping, emitted at SOCKET-CONNECT time off the SAME
    // sidecar status-file transition that drives the /feeds panel
    // (`set_subscribed`). `None` (tests) = audit row only, no Telegram.
    notifier_slot: Option<
        Arc<arc_swap::ArcSwapOption<tickvault_core::notification::NotificationService>>,
    >,
    // The process-lifetime Groww aggregator, created ONCE by the supervisor
    // wrapper — a respawned bridge reuses it (bars survive a panic) and the
    // IST-midnight force-seal (spawned once by the wrapper) keeps sealing it.
    aggregator: MultiTfAggregator,
    // Process-lifetime ws_event_audit edge latches (created once by the
    // supervisor wrapper) — survive a bridge respawn so the forensic
    // Connected/Disconnected/Reconnected chain stays consistent (finding M3).
    audit_latches: Arc<GrowwAuditLatches>,
    // Process-lifetime capture-seq counter (§34 auto-scale PR-2): the shard
    // bridges all share ONE counter so capture_seq stays globally unique +
    // monotonic across N connections. The single-conn wrapper passes a
    // fresh private counter — behavior-identical to the pre-scale path.
    capture_seq: Arc<AtomicI64>,
    // §34 fleet identity (exam-fix 2026-07-06): `Some(n)` routes the
    // boot-connect Telegram ping through the FLEET-scoped alert coalescer
    // (at most ONE connected ping per 60s window across ALL shard bridges,
    // instead of one per connection per episode); `None` (single-conn path)
    // keeps today's semantics byte-identical. ws_event_audit rows, logs and
    // feed-health stay per-connection either way — only Telegram coalesces.
    conn_id: Option<usize>,
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
    let mut state = GrowwBridgeState::with_aggregator_and_seq(&qdb, aggregator, capture_seq);
    // Feed gap-episode forensics (FEED-GAP-01, 2026-07-14): ensure the audit
    // table once per bridge incarnation (idempotent DDL, best-effort — a
    // failure logs FEED-GAP-01 and never blocks the bridge), and hold the
    // tracker + a LAZY writer (constructed at the first edge; episodes are
    // rare cold-path events).
    {
        let qdb_for_gap_ddl = qdb.clone();
        tokio::spawn(async move {
            ensure_feed_gap_audit_table(&qdb_for_gap_ddl).await;
        });
    }
    let mut gap_tracker = FeedGapTracker::default();
    // Shared slot for the lazy writer: the edge append+flush runs OFF this
    // loop in `spawn_blocking` (review HIGH 2026-07-14), so the slot is an
    // Arc<Mutex<Option<...>>> the blocking task locks per edge (cold path —
    // once per episode edge; the Mutex is uncontended in practice).
    let gap_writer: Arc<std::sync::Mutex<Option<FeedGapAuditWriter>>> =
        Arc::new(std::sync::Mutex::new(None));
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

    // Groww liveness gauges (operator 2026-07-06 feed-down alerting).
    // Handles registered LAZILY on first publish (2026-07-06 stale-status
    // hardening): a session where Groww is never enabled must leave BOTH
    // metrics MISSING (the prometheus exporter renders a registered gauge at
    // its last value on every scrape), so the groww-ws-inactive alarm's
    // notBreaching keeps a deliberate config/runtime disable silent —
    // mirroring the order-update precedent (tv_order_update_ws_active is
    // only registered after a successful connect). Once registered, the
    // handle is cached — O(1) per wake, static &'static str labels, zero
    // per-wake allocation. Published from THIS loop (≤1s wake cadence)
    // because the Dhan pool watchdog is Dhan-lane-spawned and never runs on
    // a groww-only boot.
    // - `tv_groww_ws_active`: CONNECTED-level 0/1. Registered ONLY at the
    //   FIRST connected episode of the session (2026-07-06 boot-grace fix —
    //   mirrors the order-update precedent exactly): the Groww activation
    //   chain (CSV pull-until-success → sidecar launch incl. possible venv
    //   re-provision → SSM token read → NATS connect → subscribe) routinely
    //   exceeds the alarm's 60s×2 shape on cold/slow boots, so publishing an
    //   honest 0 from the first enabled wake produced a false pre-open
    //   ALARM/OK page pair on ordinary boot mornings. MISSING + notBreaching
    //   keeps the alarm silent for a boot chain of ANY length; once
    //   registered, 0/1 publishes every wake so a mid-session outage, a
    //   failed disable→re-enable, and a runtime disable all show. Honest
    //   envelope: a sidecar DEAD AT BOOT (never connects) leaves the metric
    //   missing — that class is paged by the sidecar supervisor's reject
    //   alerts + the FEED-STALL-01 watchdog, not by this alarm.
    // - `tv_feed_last_tick_age_seconds{feed="groww"}`: seconds since the
    //   feed's most-recent tick; SKIPPED (never a fake 0, not even the
    //   registration-default 0) until the first tick of the session so
    //   missing-data + notBreaching stays silent.
    let mut m_groww_active: Option<metrics::Gauge> = None;
    let mut m_groww_tick_age: Option<metrics::Gauge> = None;
    // Live-evidence floor (2026-07-06 false-recovery fix): sidecar evidence
    // — a status record's write stamp (`groww_status_is_live`) OR a tick
    // line's per-message capture stamp (`wake_had_fresh_capture`) — only
    // counts when it is at/after this floor. Initialized to THIS bridge
    // incarnation's start (yesterday's EBS-persisted file / a pre-panic
    // byte-0 re-tail can never latch) and advanced on every runtime-disabled
    // wake (the pre-disable frozen status file AND the ≤2s pre-disable
    // NDJSON backlog can never close a DOWN episode after a re-enable).
    let mut live_floor_ist_nanos: i64 = receipt_ist_nanos();
    // Episode latch for the one-shot `subscribed` status record (2026-07-06
    // boot-deferral fix): the sidecar writes `subscribed` exactly ONCE and
    // only rewrites on real emits, so on a tickless window the record is
    // fresh for just GROWW_STATUS_LIVE_MAX_AGE_NANOS. Once observed FRESH
    // (past the floor), this latch keeps the status-evidence path open so
    // the boot-connect notifier deferral stays "delayed, never lost" and the
    // connected gauge does not require the record to STILL be fresh at
    // announce time. Reset on every disabled wake (with the floor advance).
    let mut fresh_status_seen_this_episode = false;
    // One-per-incarnation triage breadcrumb for a skipped stale status file.
    let mut stale_status_logged = false;

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
                let in_market = tickvault_common::market_hours::is_within_market_hours_ist();
                let kind = if in_market {
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
                // Feed-down alerting (operator 2026-07-06): the connected
                // feed just went DOWN — the previous DB-row-only silence was
                // the incident class. ONE FeedDown Telegram per DOWN episode
                // (latched; repeat idle disable turns never re-page).
                // Fail-open on an unfilled boot-ordering notifier slot: the
                // audit row above is already written, the latch is still
                // set, only the page is skipped — the disable path is never
                // blocked. Cold path — event-driven, never per tick.
                if audit_latches.try_announce_feed_down(ist_epoch_secs_now())
                    && let Some(notifier) = notifier_slot.as_ref().and_then(|slot| slot.load_full())
                {
                    notifier.notify(tickvault_core::notification::NotificationEvent::FeedDown {
                        feed: Feed::Groww.display_name().to_string(), // O(1) EXEMPT: cold path — once per DOWN episode
                        reason: "feed switched off by the operator".to_string(), // O(1) EXEMPT: cold path — once per DOWN episode
                        // Calendar-aware (2026-07-06 fix): severity must not
                        // page High + SNS inside the 09:00–15:30 clock window
                        // on a Saturday manual run / NSE weekday holiday —
                        // the 2026-05-09 false-page class. The audit-row kind
                        // above keeps the time-of-day split (Dhan parity).
                        market_open: tickvault_common::market_hours::is_trading_session_now(),
                        // A runtime disable is a DELIBERATE toggle (the
                        // bearer-gated feeds page) — the body must say "stays
                        // off until you re-enable it", never a false
                        // "retrying automatically" (2026-07-06 fix).
                        operator_initiated: true,
                    });
                }
            }
            audit_latches
                .audited_connected
                .store(false, std::sync::atomic::Ordering::Relaxed);
            // 2026-07-04 parity: a feed disable is a genuine disconnect —
            // re-arm the boot-connect edge so a later re-enable announces
            // the fresh subscribe exactly once (never per reconnect attempt).
            audit_latches.rearm_boot_connect();
            connect_logged = false;
            streaming_observed = false;
            fresh_status_seen_this_episode = false;
            // Advance the live-evidence floor every disabled wake: after a
            // re-enable, ONLY evidence the sidecar produces AFTER this
            // instant — a status write stamp OR a tick line's per-message
            // capture stamp — can latch streaming / close the DOWN episode.
            // The pre-disable frozen "streaming" file AND the ≤2s of
            // pre-disable NDJSON backlog the sidecar appends before its
            // kill are fossils (2026-07-06 false-recovery fix; the backlog
            // still persists + folds on re-enable — zero loss — it just
            // proves nothing about liveness).
            live_floor_ist_nanos = receipt_ist_nanos();
            // Connected-level gauge drops to 0 while disabled ONLY when it
            // was already registered this session (a genuine enabled→off
            // transition shows the outage). A session where Groww was never
            // enabled leaves the metric MISSING so notBreaching keeps the
            // deliberate disable silent (no daily ALARM/OK churn).
            if let Some(gauge) = m_groww_active.as_ref() {
                gauge.set(0.0);
            }
            continue;
        }

        // CONNECT+SUBSCRIBE PROOF: read the sidecar status file (cheap, best-effort).
        // FRESHNESS-GATED (2026-07-06 stale-status false-OK fix): the sidecar
        // never deletes this file (a killed child freezes it at its last
        // write, EBS persists it across the daily stop/start), so a record is
        // evidence ONLY once it has been observed with a write stamp at/after
        // the live floor AND recent — a stale record is treated exactly like
        // an absent file (no connect log, no boot-connect ping, no streaming
        // latch, no count refresh). This is what keeps FeedRecovered ("Prices
        // are flowing") and tv_groww_ws_active honest across disable→
        // re-enable cycles and dead-at-boot sidecars. EPISODE LATCH
        // (2026-07-06 boot-deferral fix): the `subscribed` record is written
        // ONCE and only rewritten on real emits, so freshness is required at
        // the FIRST observation per episode, then latched — otherwise a
        // >120s docker/notifier boot chain silently LOST the deferred
        // boot-connect ping ("delayed, never lost" broken) and the connected
        // gauge stayed 0 on tickless windows while the sidecar was healthily
        // subscribed. The floor (advanced on every disabled wake) alone
        // kills fossils; the max-age window ages out a KILLED sidecar's last
        // write before it can ever latch.
        if let Some(status) = read_status_file(&status_file_path).await {
            if groww_status_is_live(
                status.ts_ist_nanos,
                receipt_ist_nanos(),
                live_floor_ist_nanos,
            ) {
                fresh_status_seen_this_episode = true;
            }
            if !fresh_status_seen_this_episode {
                if !stale_status_logged {
                    stale_status_logged = true;
                    info!(
                        status_ts_ist_nanos = status.ts_ist_nanos,
                        live_floor_ist_nanos,
                        "groww bridge: ignoring STALE sidecar status file (leftover from a \
                         prior sidecar incarnation) — waiting for fresh subscribe/streaming \
                         proof before claiming the feed is live"
                    );
                }
            } else {
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
                // Boot-stage socket-connected announcement (operator 2026-07-04 —
                // "i need the same view display everything even for groww"): on
                // the FIRST observed subscribe state per connected episode, emit
                // ONE ws_event_audit Connected row AT SOCKET-CONNECT time + ONE
                // Telegram ping — WITHOUT waiting for the first streaming tick
                // (a closed market previously meant total boot silence). The
                // existing first-streaming-tick rising edge below is KEPT
                // unchanged as the streaming confirmation; this is ADDITIVE.
                // Edge-latched via `boot_connect_announced` (once per episode,
                // never per poll); a notifier slot that exists but is not yet
                // filled (boot ordering) defers to the next wake so the ping is
                // delayed, never lost.
                let notifier = notifier_slot.as_ref().and_then(|slot| slot.load_full());
                let notifier_ready = match notifier_slot.as_ref() {
                    None => true, // no Telegram wiring (tests) — audit row only
                    Some(_) => notifier.is_some(),
                };
                if should_announce_boot_connect(
                    status.event != GrowwStatusEvent::Unknown,
                    audit_latches
                        .boot_connect_announced
                        .load(std::sync::atomic::Ordering::Relaxed),
                    notifier_ready,
                ) && audit_latches.try_announce_boot_connect()
                {
                    emit_groww_ws_audit(
                        ws_audit_tx.as_ref(),
                        WsEventKind::Connected,
                        "groww_subscribed",
                        "groww socket connected + subscribed — awaiting first tick",
                    );
                    // FLEET-scoped Telegram coalescing (exam-fix 2026-07-06): in
                    // fleet mode every shard bridge used to fire its own LOW
                    // "connected — Subscribed N" ping per connected episode —
                    // alternating with the per-connection reject alerts into
                    // dozens of messages. Fleet connections now emit at most ONE
                    // connected ping per 60s window (the FIRST bridge in the
                    // window carries its own subscribe counts); suppressed pings
                    // keep their ws_event_audit row + CONNECT log + feed-health
                    // update above. Single-conn (`conn_id == None`) = today's
                    // behavior byte-identical.
                    let connected_decision = crate::groww_fleet_alerts::global_fleet_alert_decision(
                        conn_id,
                        crate::groww_fleet_alerts::FleetAlertKind::Connected,
                    );
                    if matches!(
                        connected_decision,
                        crate::groww_fleet_alerts::FleetAlertDecision::Suppress
                    ) {
                        metrics::counter!(
                            "tv_groww_fleet_alerts_suppressed_total",
                            "direction" => "connected",
                        )
                        .increment(1);
                    } else if let Some(notifier) = notifier {
                        notifier.notify(
                        tickvault_core::notification::NotificationEvent::FeedConnectedAwaitingTicks {
                            // O(1) EXEMPT: one Telegram event per fleet connect edge — cold notification path.
                            feed: Feed::Groww.display_name().to_string(),
                            subscribed: status.stocks.saturating_add(status.indices),
                            market_open:
                                tickvault_common::market_hours::is_within_market_hours_ist(),
                        },
                    );
                    }
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
        }

        // Drain whatever new bytes arrived (the real ingestion body — ALWAYS
        // unconditional: every drained line persists + folds, zero loss).
        let parsed_a_tick = state.drain_new_data(&tick_file_path, &feed_health).await;
        // FRESHNESS-GATED streaming latch (2026-07-06 replayed-backlog
        // false-recovery fix): a parsed tick proves the feed is live ONLY
        // when its per-message capture stamp is at/after the live floor —
        // the SAME floor the status channel uses. Without the gate, the ≤2s
        // pre-disable NDJSON backlog (the sidecar keeps appending until its
        // 2s disable poll kills it, and the disable arm idles WITHOUT
        // draining) replayed on the first re-enable wake: it latched
        // streaming, fired a false "✅ Prices are flowing" FeedRecovered
        // BEFORE the relaunched sidecar had even authenticated, consumed the
        // DOWN episode, and pinned tv_groww_ws_active = 1 all session while
        // the relaunch failed — the exact blind-alarm/false-OK class the
        // status-file floor closed, through the second evidence channel. The
        // same gate covers a respawned bridge's byte-0 re-tail of
        // already-captured bytes. Lines WITHOUT a capture stamp (old-format /
        // reconcile-sweep rows) never latch here — the sidecar's ≤1s status
        // rewrite on real emits latches those flows via the status channel.
        if parsed_a_tick && state.wake_had_fresh_capture(live_floor_ist_nanos) {
            // A freshly-captured parsed tick is the strongest proof the feed
            // is live.
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

        // FeedRecovered (operator 2026-07-06): close the DOWN episode ONCE,
        // on the STREAMING rising edge only — recovery is claimed on ticks /
        // the sidecar's own streaming status, never on mere socket-connect
        // (the boot-connect ping above keeps saying "awaiting first tick").
        // Deliberately OUTSIDE the `audited_connected` gate: if the lazily-
        // filled Telegram slot is not deliverable yet (boot ordering), the
        // episode latch is NOT consumed and the next ≤1s wake retries —
        // delayed, never lost.
        if streaming_observed
            && audit_latches
                .down_announced
                .load(std::sync::atomic::Ordering::Relaxed)
        {
            let notifier = notifier_slot.as_ref().and_then(|slot| slot.load_full());
            let notifier_ready = match notifier_slot.as_ref() {
                None => true, // no Telegram wiring (tests) — consume silently
                Some(_) => notifier.is_some(),
            };
            if notifier_ready
                && let Some(down_secs) = audit_latches.take_feed_recovery(ist_epoch_secs_now())
                && let Some(notifier) = notifier
            {
                notifier.notify(
                    tickvault_core::notification::NotificationEvent::FeedRecovered {
                        feed: Feed::Groww.display_name().to_string(), // O(1) EXEMPT: cold path — once per DOWN-episode recovery
                        down_secs,
                    },
                );
            }
        }

        // Groww liveness gauges (operator 2026-07-06): cold ≤1s cadence,
        // handles cached after the first publish. Connected-level active bit
        // — 1 while the connected episode holds (socket connected +
        // subscribed OR streaming, FRESH evidence only — a stale status file
        // or a replayed tick backlog can never latch it). BOOT GRACE
        // (2026-07-06 fix — order-update precedent): the gauge is REGISTERED
        // only at the first connected episode of the session, so the metric
        // is MISSING (notBreaching → alarm silent) for the whole activation
        // chain (CSV pull-until-success → sidecar launch/venv → token →
        // connect → subscribe) — a chain of ANY length never produces the
        // pre-open false ALARM/OK page pair the unverified "~2min grace"
        // assumed away. Once registered, 0/1 publishes every enabled wake
        // (plus 0 on disabled wakes), so a mid-session outage, a failed
        // disable→re-enable, and a runtime disable all show honestly. A
        // sidecar DEAD AT BOOT leaves the metric missing — that class pages
        // via the sidecar supervisor's reject alerts + FEED-STALL-01, not
        // this alarm (documented in app-alarms.tf).
        let connected_episode = streaming_observed
            || audit_latches
                .boot_connect_announced
                .load(std::sync::atomic::Ordering::Relaxed);
        if connected_episode || m_groww_active.is_some() {
            m_groww_active
                .get_or_insert_with(|| metrics::gauge!("tv_groww_ws_active"))
                .set(if connected_episode { 1.0 } else { 0.0 });
        }
        // Tick age: SKIP (never a fake 0 — the gauge is not even REGISTERED,
        // so the exporter renders nothing) until the first tick of the
        // session — missing data + the alarm's notBreaching stays silent.
        if let Some(age) = feed_health.last_tick_age_secs(Feed::Groww, receipt_ist_nanos()) {
            m_groww_tick_age
                .get_or_insert_with(
                    || metrics::gauge!("tv_feed_last_tick_age_seconds", "feed" => "groww"),
                )
                .set(age as f64);
        }

        // Feed gap-episode tracker (FEED-GAP-01, 2026-07-14): rides THIS ≤1s
        // wake — NEVER the per-tick hot path. Every gap becomes a NAMED
        // durable episode: OPEN row + ONE High Telegram at the ≥10s
        // in-session threshold crossing, CLOSE row + Info Telegram (measured
        // gap + named partial 1-minute bars) at the liveness-advance
        // recovery edge, and the unconditional gap-seconds counter for
        // sub-threshold micro-gaps. Annotation, never repair — candles are
        // untouched; a failed write logs FEED-GAP-01 and never touches the
        // recovery machinery.
        {
            let gap_now_nanos = receipt_ist_nanos();
            // RAW stamp — never the floored age (review CRITICAL 2026-07-14:
            // the sub-second remainder creeps a reconstruction forward every
            // poll, so OPEN could never latch during a real gap).
            let gap_last_tick = feed_health.last_tick_ist_nanos(Feed::Groww);
            let in_session = tickvault_common::market_hours::is_trading_session_now();
            let (edge, counter_add_secs) =
                gap_tracker.poll(gap_last_tick, gap_now_nanos, in_session);
            if counter_add_secs > 0 {
                metrics::counter!("tv_feed_gap_seconds_total", "feed" => "groww")
                    .increment(counter_add_secs);
            }
            if !matches!(edge, FeedGapEdge::None) {
                let nanos_per_day: i64 = 86_400 * 1_000_000_000;
                let trading_date_ist_nanos =
                    gap_now_nanos - gap_now_nanos.rem_euclid(nanos_per_day);
                let feed_label = Feed::Groww.as_str();
                let (row, event) = match &edge {
                    FeedGapEdge::Open {
                        detect_nanos,
                        last_tick_nanos,
                    } => (
                        FeedGapAuditRow::open(
                            gap_now_nanos,
                            trading_date_ist_nanos,
                            feed_label,
                            *detect_nanos,
                        ),
                        tickvault_core::notification::NotificationEvent::FeedGapEpisodeOpened {
                            feed: Feed::Groww.display_name().to_string(), // O(1) EXEMPT: cold path — once per gap episode
                            start_hhmm_ist: ist_hhmm_label(*last_tick_nanos), // O(1) EXEMPT: cold path — once per gap episode
                            // "No live orders exist" — the Dhan-disable
                            // safety gate is wired to dry_run at boot and
                            // flips false exactly when live orders can
                            // exist, so it doubles as the paper-mode truth.
                            dry_run: feed_runtime.can_disable_dhan(),
                        },
                    ),
                    FeedGapEdge::Close {
                        detect_nanos,
                        last_tick_before_nanos,
                        end_nanos,
                        gap_secs,
                    } => {
                        let partial = partial_minute_labels(*last_tick_before_nanos, *end_nanos);
                        (
                            FeedGapAuditRow::closed(
                                gap_now_nanos,
                                trading_date_ist_nanos,
                                feed_label,
                                *detect_nanos,
                                *end_nanos,
                                (*gap_secs).min(i64::MAX as u64) as i64,
                                // Kill count: the stall-watchdog restart
                                // count lives only in a write-only metrics
                                // counter — not cheaply readable in-process,
                                // so the row carries the honest -1 sentinel
                                // (never a fabricated 0).
                                tickvault_storage::feed_gap_audit_persistence::GAP_SENTINEL_UNKNOWN,
                                partial.clone(),
                            ),
                            tickvault_core::notification::NotificationEvent::FeedGapEpisodeClosed {
                                feed: Feed::Groww.display_name().to_string(), // O(1) EXEMPT: cold path — once per gap episode
                                gap_secs: *gap_secs,
                                kill_count: tickvault_storage::feed_gap_audit_persistence::GAP_SENTINEL_UNKNOWN,
                                partial_minutes: partial,
                            },
                        )
                    }
                    FeedGapEdge::None => unreachable!("gated by the matches! above"),
                };
                // Durable row FIRST (audit-first §24), best-effort — a
                // failure is loud (FEED-GAP-01 + counter) and never blocks
                // the Telegram bubble or the recovery machinery. The
                // append+flush runs OFF this loop (`spawn_blocking`,
                // fire-and-forget; review HIGH 2026-07-14): even with the
                // bounded no-retry/5s-timeout ILP conf, a down QuestDB must
                // never stall the NDJSON drain or the parse-time liveness
                // stamp (a stalled stamp risks a false FEED-STALL-01 kill of
                // a healthy sidecar). Episode edges are cold (once per
                // episode), so the spawn cost is irrelevant.
                let writer_slot = Arc::clone(&gap_writer);
                let qdb_for_gap = qdb.clone();
                tokio::task::spawn_blocking(move || {
                    let mut slot = writer_slot
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let writer = slot.get_or_insert_with(|| FeedGapAuditWriter::new(&qdb_for_gap));
                    let write_result = writer.append_row(&row).and_then(|()| writer.flush());
                    if let Err(err) = write_result {
                        metrics::counter!(
                            "tv_feed_gap_audit_write_errors_total",
                            "stage" => "append_flush"
                        )
                        .increment(1);
                        error!(
                            code = "FEED-GAP-01",
                            stage = "append_flush",
                            ?err,
                            outcome = row.outcome,
                            "FEED-GAP-01: gap-episode row not persisted — the \
                             Telegram bubble still fires; the next edge re-appends \
                             (DEDUP-idempotent)"
                        );
                        // Drop the writer so the next edge rebuilds a fresh
                        // sender instead of replaying a poisoned buffer.
                        *slot = None;
                    }
                });
                if let Some(notifier) = notifier_slot.as_ref().and_then(|slot| slot.load_full()) {
                    notifier.notify(event);
                }
            }
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
        // Old-format line (no capture_ns): the optional field defaults None —
        // backward compatible with every pre-2026-07-03 sidecar row.
        assert_eq!(p.capture_ns, None);
    }

    #[test]
    fn test_parse_groww_tick_line_with_capture_ns() {
        // New-format line from the per-callback capture path: capture_ns is
        // UTC epoch nanos stamped inside the sidecar's NATS-callback hook.
        let line = r#"{"security_id":1333,"segment":"NSE_EQ","ts_ist_nanos":1780000020123000000,"exchange_ts_millis":1780000020123,"ltp":2847.55,"cum_volume":0,"capture_ns":1780000020125000000}"#;
        let p = parse_groww_tick_line(line).expect("parse");
        assert_eq!(p.capture_ns, Some(1_780_000_020_125_000_000));
        assert_eq!(p.tick.security_id, 1333);
        assert_eq!(p.tick.ltp, 2847.55);
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

    /// Plausible receipt clock for validator tests, anchored at the valid
    /// tick's own stamp. F1 (2026-07-03): `receipt = 0` now fails CLOSED
    /// (`ImplausibleReceiptClock`), so every non-clock test must supply a
    /// plausible clock to reach the field-specific reject arms.
    fn ok_receipt() -> i64 {
        valid_parsed().tick.ts_ist_nanos
    }

    #[test]
    fn test_validate_groww_tick_accepts_valid() {
        assert_eq!(validate_groww_tick(&valid_parsed(), ok_receipt()), Ok(()));
    }

    #[test]
    fn test_validate_groww_tick_rejects_non_finite_ltp() {
        let mut p = valid_parsed();
        p.tick.ltp = f64::NAN;
        assert_eq!(
            validate_groww_tick(&p, ok_receipt()),
            Err(GrowwTickReject::NonFinitePrice)
        );
        p.tick.ltp = f64::INFINITY;
        assert_eq!(
            validate_groww_tick(&p, ok_receipt()),
            Err(GrowwTickReject::NonFinitePrice)
        );
    }

    #[test]
    fn test_validate_groww_tick_rejects_non_positive_ltp() {
        let mut p = valid_parsed();
        p.tick.ltp = 0.0;
        assert_eq!(
            validate_groww_tick(&p, ok_receipt()),
            Err(GrowwTickReject::NonPositivePrice)
        );
        p.tick.ltp = -1.5;
        assert_eq!(
            validate_groww_tick(&p, ok_receipt()),
            Err(GrowwTickReject::NonPositivePrice)
        );
    }

    #[test]
    fn test_validate_groww_tick_rejects_implausible_ltp() {
        let mut p = valid_parsed();
        p.tick.ltp = f64::from(tickvault_common::constants::MAX_PLAUSIBLE_LTP) + 1.0;
        assert_eq!(
            validate_groww_tick(&p, ok_receipt()),
            Err(GrowwTickReject::ImplausiblePrice)
        );
    }

    #[test]
    fn test_validate_groww_tick_rejects_negative_volume() {
        let mut p = valid_parsed();
        p.tick.cum_volume = -1;
        assert_eq!(
            validate_groww_tick(&p, ok_receipt()),
            Err(GrowwTickReject::NegativeVolume)
        );
    }

    #[test]
    fn test_validate_groww_tick_rejects_implausible_volume() {
        let mut p = valid_parsed();
        p.tick.cum_volume = MAX_PLAUSIBLE_VOLUME + 1;
        assert_eq!(
            validate_groww_tick(&p, ok_receipt()),
            Err(GrowwTickReject::ImplausibleVolume)
        );
        // Boundary is inclusive-valid.
        p.tick.cum_volume = MAX_PLAUSIBLE_VOLUME;
        assert_eq!(validate_groww_tick(&p, ok_receipt()), Ok(()));
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
            validate_groww_tick(&p, ok_receipt()),
            Err(GrowwTickReject::NonPositiveSecurityId)
        );
        p.tick.security_id = -7;
        assert_eq!(
            validate_groww_tick(&p, ok_receipt()),
            Err(GrowwTickReject::NonPositiveSecurityId)
        );
    }

    #[test]
    fn test_validate_groww_tick_rejects_timestamp_out_of_range() {
        let mut p = valid_parsed();
        p.tick.ts_ist_nanos = 0;
        assert_eq!(
            validate_groww_tick(&p, ok_receipt()),
            Err(GrowwTickReject::TimestampOutOfRange)
        );
        p.tick.ts_ist_nanos = -1;
        assert_eq!(
            validate_groww_tick(&p, ok_receipt()),
            Err(GrowwTickReject::TimestampOutOfRange)
        );
        p.tick.ts_ist_nanos = i64::MAX;
        assert_eq!(
            validate_groww_tick(&p, ok_receipt()),
            Err(GrowwTickReject::TimestampOutOfRange)
        );
        // Boundaries are inclusive-valid.
        p.tick.ts_ist_nanos = MIN_PLAUSIBLE_TS_IST_NANOS;
        assert_eq!(validate_groww_tick(&p, ok_receipt()), Ok(()));
        // The 2100 upper bound needs a same-era receipt clock — with a 2026
        // receipt the (correct) ±60 s future-skew clamp fires first.
        p.tick.ts_ist_nanos = MAX_PLAUSIBLE_TS_IST_NANOS;
        assert_eq!(validate_groww_tick(&p, MAX_PLAUSIBLE_TS_IST_NANOS), Ok(()));
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
    fn test_row_received_at_prefers_plausible_capture_ns() {
        // 2026-07-03 per-callback capture: a plausible per-message capture_ns
        // (UTC nanos) wins over the per-wake stamp, converted UTC→IST nanos —
        // received_at now means TRUE capture-at-receipt time per message.
        let wake = GROWW_MIN_PLAUSIBLE_RECEIPT_IST_NANOS + 10_000_000_000;
        let capture_utc = wake - tickvault_common::constants::IST_UTC_OFFSET_NANOS - 5;
        assert_eq!(
            row_received_at_with_capture(Some(capture_utc), wake),
            Some(capture_utc.saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS)),
            "plausible capture stamp (IST-converted) must be used per-row"
        );
    }

    #[test]
    fn test_row_received_at_falls_back_without_capture_ns() {
        // Old-format / reconcile-sweep rows: exact pre-PR per-wake behaviour.
        let wake = GROWW_MIN_PLAUSIBLE_RECEIPT_IST_NANOS + 1;
        assert_eq!(row_received_at_with_capture(None, wake), Some(wake));
        assert_eq!(row_received_at_with_capture(None, 0), None);
    }

    #[test]
    fn test_row_received_at_falls_back_on_implausible_capture_ns() {
        let wake = GROWW_MIN_PLAUSIBLE_RECEIPT_IST_NANOS + 10_000_000_000;
        // Pre-2020 garbage stamp (0 / negative) → wake fallback.
        assert_eq!(row_received_at_with_capture(Some(0), wake), Some(wake));
        assert_eq!(row_received_at_with_capture(Some(-1), wake), Some(wake));
        // Capture ahead of the wake clock beyond tolerance (impossible on a
        // sane shared host clock — the wake read happens AFTER the sidecar's
        // write) → broken clock → wake fallback, never a garbage column.
        let far_future = wake + GROWW_FUTURE_TS_TOLERANCE_NANOS + 1;
        assert_eq!(
            row_received_at_with_capture(Some(far_future), wake),
            Some(wake)
        );
        // Both implausible (broken clock everywhere) → NULL, matching the
        // pre-PR broken-clock behaviour.
        assert_eq!(row_received_at_with_capture(Some(0), 0), None);
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

    /// §34 PR-2 Item 7 ratchet: two shard bridges drawing from the SAME
    /// shared counter never emit the same capture_seq and stay globally
    /// monotonic — the cross-shard uniqueness half of the `ticks` DEDUP key
    /// (shard disjointness is the other half).
    #[test]
    fn test_with_aggregator_and_seq_shares_the_caller_counter() {
        // §34 PR-2: every shard bridge draws from ONE caller-owned counter —
        // the constructor must store the SAME Arc, never a private clone of
        // the value.
        let shared = Arc::new(AtomicI64::new(41));
        let state = GrowwBridgeState::with_aggregator_and_seq(
            &QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port: 9000,
                pg_port: 8812,
                ilp_port: 9009,
            },
            MultiTfAggregator::with_capacity(GROWW_AGGREGATOR_CAPACITY),
            Arc::clone(&shared),
        );
        assert!(Arc::ptr_eq(&state.capture_seq, &shared));
        // A draw through the state's counter is visible to the shared handle.
        let _ = next_capture_seq(&state.capture_seq, 1_000);
        assert!(shared.load(std::sync::atomic::Ordering::Relaxed) > 41);
    }

    #[test]
    fn test_shared_capture_seq_monotonic_across_shards() {
        let shared = Arc::new(AtomicI64::new(0));
        // Shard 0 and shard 1 interleave draws at the same wall instant.
        let s0 = Arc::clone(&shared);
        let s1 = Arc::clone(&shared);
        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.push(next_capture_seq(&s0, 1_000));
            seen.push(next_capture_seq(&s1, 1_000));
        }
        let mut sorted = seen.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), seen.len(), "no duplicate seq across shards");
        assert!(
            seen.windows(2).all(|w| w[1] > w[0]),
            "globally monotonic across interleaved shard draws: {seen:?}"
        );
    }

    /// §34 PR-2 shard layout: pure path derivation, zero-padded 2-digit
    /// per-conn directories under the shards root.
    #[test]
    fn test_shard_files_layout_and_padding() {
        let root = Path::new("data/groww/shards");
        let f0 = shard_files(root, 0);
        assert_eq!(f0.conn_id, 0);
        assert_eq!(f0.watch_dir, root.join("c00"));
        assert_eq!(f0.tick_file, root.join("c00").join("live-ticks.ndjson"));
        assert_eq!(f0.status_file, root.join("c00").join("groww-status.json"));
        let f7 = shard_files(root, 7);
        assert_eq!(f7.watch_dir, root.join("c07"));
        let f42 = shard_files(root, 42);
        assert_eq!(f42.watch_dir, root.join("c42"));
        // Distinct conns never share a file (disjoint capture chains).
        assert_ne!(f0.tick_file, f7.tick_file);
        assert_eq!(GROWW_SHARDS_DIR_DEFAULT, "data/groww/shards");
    }

    /// Hardening 2026-07-06: scale-down must be able to drop the killed
    /// conn's subscribe-proof file (undated path, never sidecar-deleted) so
    /// the ladder's plateau gate cannot count dead connections.
    /// test coverage (pub-fn-test-guard line): remove_subscribe_proof
    #[test]
    fn test_remove_subscribe_proof_removes_only_the_target_conn() {
        let root = std::env::temp_dir().join(format!(
            "tv_bridge_proof_rm_{}_{}",
            std::process::id(),
            line!()
        ));
        for conn_id in 0..2 {
            let files = shard_files(&root, conn_id);
            std::fs::create_dir_all(&files.watch_dir).unwrap_or_else(|e| panic!("mkdir: {e}"));
            std::fs::write(&files.status_file, b"{}").unwrap_or_else(|e| panic!("write: {e}"));
        }
        // Removes the target, leaves the sibling.
        assert!(remove_subscribe_proof(&root, 1));
        assert!(!shard_files(&root, 1).status_file.exists());
        assert!(shard_files(&root, 0).status_file.exists());
        // Missing file → clean no-op (idempotent).
        assert!(!remove_subscribe_proof(&root, 1));
        let _ = std::fs::remove_dir_all(&root);
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
        assert_eq!(
            got.ts_ist_nanos, 0,
            "absent ts_ist_nanos → serde default 0 (never-live, fail-closed)"
        );
    }

    #[test]
    fn test_parse_groww_status_line_carries_write_stamp() {
        // Stale-status false-OK fix (2026-07-06): the sidecar's own write
        // stamp (`ts_ist_nanos` from write_status) MUST survive the parse —
        // it is the freshness signal that keeps a killed / prior-day
        // sidecar's frozen "streaming" record from latching liveness.
        let s = r#"{"event":"streaming","stocks":765,"indices":2,"total":767,"ts_ist_nanos":1780000020123000000}"#;
        let got = parse_groww_status_line(s).expect("parse streaming");
        assert_eq!(got.ts_ist_nanos, 1_780_000_020_123_000_000);
    }

    // test coverage (one line for pub-fn-test-guard test.*<fn>): groww_status_is_live

    #[test]
    fn test_groww_status_is_live_fresh_after_floor_is_live() {
        // A record written after the floor and within the max-age window is
        // live evidence (the healthy ≤1s streaming-rewrite case).
        let now = 2_000_000_000 * NANOS_PER_SECOND;
        let floor = now - 3_600 * NANOS_PER_SECOND;
        assert!(groww_status_is_live(now - NANOS_PER_SECOND, now, floor));
        assert!(groww_status_is_live(now, now, floor));
    }

    #[test]
    fn test_groww_status_is_live_zero_stamp_is_never_live() {
        // Old-format record without a write stamp → fail-closed, never live.
        let now = 2_000_000_000 * NANOS_PER_SECOND;
        assert!(!groww_status_is_live(0, now, 0));
        assert!(!groww_status_is_live(-1, now, i64::MIN));
    }

    #[test]
    fn test_groww_status_is_live_pre_floor_stamp_is_stale() {
        // The disable→re-enable / boot-with-yesterdays-file class: a record
        // written BEFORE the live floor (bridge incarnation start / last
        // disabled wake) is a fossil from a killed sidecar — never evidence,
        // even if it is recent by wall clock.
        let now = 2_000_000_000 * NANOS_PER_SECOND;
        let floor = now - 10 * NANOS_PER_SECOND;
        assert!(!groww_status_is_live(floor - 1, now, floor));
        assert!(
            groww_status_is_live(floor, now, floor),
            "at-floor stamp counts (>= floor)"
        );
    }

    #[test]
    fn test_groww_status_is_live_aged_out_stamp_is_stale() {
        // A stamp older than GROWW_STATUS_LIVE_MAX_AGE_NANOS ages out (the
        // sidecar rewrites ≤1s while streaming, so a healthy record is
        // always far inside the bound).
        let now = 2_000_000_000 * NANOS_PER_SECOND;
        let floor = 0;
        let stale = now - GROWW_STATUS_LIVE_MAX_AGE_NANOS - 1;
        assert!(!groww_status_is_live(stale, now, floor));
        let boundary = now - GROWW_STATUS_LIVE_MAX_AGE_NANOS;
        assert!(
            groww_status_is_live(boundary, now, floor),
            "exactly-at-window stamp still counts"
        );
    }

    #[test]
    fn test_groww_status_is_live_absurd_future_stamp_is_rejected() {
        // Garbage / clock-skew guard: a stamp absurdly in the FUTURE of the
        // receipt clock is rejected fail-closed (it would otherwise pass the
        // floor + age checks forever).
        let now = 2_000_000_000 * NANOS_PER_SECOND;
        let future = now + GROWW_STATUS_LIVE_MAX_AGE_NANOS + 1;
        assert!(!groww_status_is_live(future, now, 0));
        assert!(
            groww_status_is_live(now + GROWW_STATUS_LIVE_MAX_AGE_NANOS, now, 0),
            "small forward skew inside the window is tolerated"
        );
    }

    #[test]
    fn test_sidecar_status_stamp_shares_the_ist_epoch_convention() {
        // CROSS-LANGUAGE EPOCH RATCHET (2026-07-06 CRITICAL fix): the Rust
        // freshness gate (`groww_status_is_live`) compares the sidecar's
        // status write stamp against `receipt_ist_nanos()` = UTC epoch +
        // 19,800s ("IST epoch"). The sidecar's `now_ist_nanos()` previously
        // returned `datetime.now(tz=IST).timestamp()` — the PLAIN UTC epoch
        // (`.timestamp()` on an aware datetime ignores the zone) — so every
        // genuinely fresh record read ≈19,800s old, 165× the 120s window:
        // the gate rejected ALL real status evidence forever, killing the
        // boot-connect announcement and false-firing the groww-ws-inactive
        // alarm every morning. Pin that `now_ist_nanos` uses the SAME
        // `replace(tzinfo=timezone.utc)` trick as `ms_to_ist_nanos` (the
        // verified IST-epoch producer for the tick lines), so both sides of
        // the gate share one epoch.
        let sidecar = include_str!("../../../scripts/groww-sidecar/groww_sidecar.py");
        let def_at = sidecar
            .find("def now_ist_nanos(")
            .expect("sidecar must define now_ist_nanos");
        let body_region = &sidecar[def_at..];
        let body_end = body_region[1..]
            .find("\ndef ")
            .map_or(body_region.len(), |rel| rel + 1);
        let body = &body_region[..body_end];
        // 2026-07-06 anti-vacuity hardening: every assertion below runs
        // against the body's EXECUTABLE code lines only (docstring + `#`
        // comment lines stripped). The function's own docstring quotes the
        // `replace(tzinfo=timezone.utc)` literal in prose, so a positive
        // `body.contains(...)` was self-satisfied by documentation — a
        // rewrite to `return time.time_ns()` (plain UTC epoch, the exact
        // stale-forever bug) passed the guard. Prose can no longer satisfy
        // the pin.
        let mut in_docstring = false;
        let code_lines: Vec<&str> = body
            .lines()
            .filter(|line| {
                let t = line.trim_start();
                let quotes = t.matches("\"\"\"").count();
                if quotes == 1 {
                    // A lone triple-quote toggles docstring state; the line
                    // itself is docstring text either way.
                    in_docstring = !in_docstring;
                    return false;
                }
                if quotes >= 2 {
                    return false; // one-line docstring
                }
                !in_docstring && !t.starts_with('#') && !t.is_empty()
            })
            .collect();
        assert!(
            code_lines
                .iter()
                .any(|l| l.contains("replace(tzinfo=timezone.utc)") && l.contains(".timestamp()")),
            "groww_sidecar.py: now_ist_nanos (the status-file write stamp) must use the \
             replace(tzinfo=timezone.utc)...timestamp() IST-epoch trick on an EXECUTABLE \
             code line (not just the docstring) — the same convention as ms_to_ist_nanos \
             and the Rust receipt_ist_nanos — or the bridge's groww_status_is_live gate \
             classifies every real status record as 19,800s stale forever:\n{body}"
        );
        // A bare aware-datetime `.timestamp()` in the same body's CODE
        // would reintroduce the UTC-epoch bug; the replace-trick line is
        // the only allowed `.timestamp()` call site.
        let bare_utc_calls = code_lines
            .iter()
            .filter(|t| t.contains(".timestamp()") && !t.contains("replace(tzinfo=timezone.utc)"))
            .count();
        assert_eq!(
            bare_utc_calls, 0,
            "groww_sidecar.py: now_ist_nanos must not call .timestamp() on a bare aware \
             datetime in code (that returns the plain UTC epoch, not IST epoch)"
        );
        // `time.time()` / `time.time_ns()` are the other plain-UTC-epoch
        // producers a future "pythonic simplification" would reach for —
        // ban them from the body's code outright.
        let plain_epoch_calls = code_lines
            .iter()
            .filter(|t| t.contains("time.time(") || t.contains("time.time_ns("))
            .count();
        assert_eq!(
            plain_epoch_calls, 0,
            "groww_sidecar.py: now_ist_nanos must not call time.time()/time.time_ns() — \
             both return the plain UTC epoch, reintroducing the 19,800s-stale-forever \
             freshness-gate bug"
        );
        // 2026-07-07 hardening (review finding): the wall-clock SOURCE line is
        // the other half of the two-line function and was previously unpinned.
        // A one-token revert to `datetime.now(timezone.utc)` — or dropping
        // `tz=IST` on a UTC-configured box — makes the `replace(tzinfo=
        // timezone.utc)` trick a no-op (the datetime is already UTC wall
        // clock), so `now_ist_nanos` returns the plain UTC epoch again: the
        // exact 19,800s stale-forever regression, with the replace-trick line
        // still present to satisfy the positive pin above. Pin the IST-zoned
        // now positively on an EXECUTABLE code line.
        assert!(
            code_lines
                .iter()
                .any(|l| l.contains("datetime.now(tz=IST)")),
            "groww_sidecar.py: now_ist_nanos must take its wall clock from \
             datetime.now(tz=IST) on an EXECUTABLE code line — any other now-source \
             (datetime.now(timezone.utc), a naked datetime.now() on a UTC box) turns \
             the replace(tzinfo=timezone.utc) trick into a no-op and the stamp back \
             into the plain UTC epoch, so groww_status_is_live classifies every real \
             status record as 19,800s stale forever:\n{body}"
        );
        // ...and ban every OTHER datetime.now(...) / utcnow() source in the
        // body's code so the IST-keyword form is the ONLY wall-clock read
        // (a positional `datetime.now(IST)` must be rewritten to the pinned
        // `tz=IST` keyword form — the ratchet is deliberately strict).
        let non_ist_now_calls = code_lines
            .iter()
            .filter(|l| {
                (l.contains("datetime.now(") && !l.contains("datetime.now(tz=IST)"))
                    || l.contains("utcnow(")
            })
            .count();
        assert_eq!(
            non_ist_now_calls, 0,
            "groww_sidecar.py: now_ist_nanos must not read the wall clock from any \
             source other than datetime.now(tz=IST) — datetime.now(timezone.utc) / \
             naked datetime.now() / utcnow() all defeat the IST-epoch replace-trick \
             and reintroduce the 19,800s-stale-forever freshness-gate bug"
        );
    }

    #[test]
    fn test_capture_stamp_ist_nanos_trusted_stamp_only_no_fallback() {
        // 2026-07-06 replayed-backlog fix: the liveness/recovery signal needs
        // the TRUSTED per-message capture stamp ONLY — a missing stamp
        // (old-format / reconcile-sweep row) must return None, never the
        // always-fresh wake fallback (which would defeat the floor gate).
        let wake = 2_000_000_000 * NANOS_PER_SECOND;
        assert_eq!(capture_stamp_ist_nanos(None, wake), None);
        // A plausible capture converts UTC → IST-nanos convention.
        let capture_utc = wake - tickvault_common::constants::IST_UTC_OFFSET_NANOS - 5;
        assert_eq!(
            capture_stamp_ist_nanos(Some(capture_utc), wake),
            Some(capture_utc.saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS)),
        );
        // Implausible stamps (degenerate ~1970 / far ahead of the wake clock)
        // are rejected exactly like row_received_at_with_capture's gate.
        assert_eq!(capture_stamp_ist_nanos(Some(0), wake), None);
        let far_future_utc = wake; // + offset lands ~5.5h ahead of the wake
        assert_eq!(capture_stamp_ist_nanos(Some(far_future_utc), wake), None);
    }

    #[test]
    fn test_wake_had_fresh_capture_gates_on_floor_and_zero() {
        let mut state = retail_fresh_state();
        // No trusted capture this wake → never fresh, for ANY floor
        // (including i64::MIN-ish floors — the > 0 guard).
        state.wake_max_capture_ist_nanos = 0;
        assert!(!state.wake_had_fresh_capture(0));
        assert!(!state.wake_had_fresh_capture(i64::MIN));
        // A capture at/after the floor is fresh; before the floor is not
        // (the replayed pre-disable backlog class).
        let floor = 2_000_000_000 * NANOS_PER_SECOND;
        state.wake_max_capture_ist_nanos = floor;
        assert!(state.wake_had_fresh_capture(floor), "at-floor counts");
        state.wake_max_capture_ist_nanos = floor - 1;
        assert!(
            !state.wake_had_fresh_capture(floor),
            "a pre-floor capture stamp is a replayed fossil — never liveness"
        );
    }

    #[tokio::test]
    async fn test_replay_window_lifecycle_clears_on_catchup_and_rearms_on_shrink() {
        // Scoreboard PR-C review round 1 (2026-07-11): the byte-0 re-tail
        // replay-window flag — the SECOND condition of the Groww lag
        // stale-capture exclusion — must arm on a fresh bridge, clear on a
        // fully-caught-up drain (later ≥60s-dwelt lines are LIVE backlog:
        // ILP-backpressure pause / respawn-backoff wake — admitted to the
        // day histogram), and re-arm on a shrink/rotation reset.
        let dir = retail_tmp_dir("replay_window_lifecycle");
        let tick_file = dir.join("live-ticks.ndjson");
        let reg = tickvault_common::feed_health::FeedHealthRegistry::new();
        let mut state = retail_fresh_state();
        assert!(
            state.replay_window,
            "a fresh bridge byte-0 re-tails — the replay window must arm at construction"
        );
        std::fs::write(&tick_file, retail_line(1)).expect("seed one live line");
        assert!(state.drain_new_data(&tick_file, &reg).await);
        assert!(
            !state.replay_window,
            "a fully-caught-up drain must clear the replay window — every later \
             byte is live, so a post-pause backlog drain keeps its true lag"
        );
        // Rotation/truncate (len < offset) re-arms: the byte-0 re-tail may
        // replay already-recorded lines.
        std::fs::write(&tick_file, b"").expect("truncate");
        let _ = state.drain_new_data(&tick_file, &reg).await;
        assert!(
            state.replay_window,
            "a shrink/rotation reset must re-arm the replay window"
        );
        // ... and the re-tail clears it again once caught up.
        std::fs::write(&tick_file, retail_line(2)).expect("rewrite");
        assert!(state.drain_new_data(&tick_file, &reg).await);
        assert!(
            !state.replay_window,
            "catching up after the re-tail must clear the replay window again"
        );
    }

    #[tokio::test]
    async fn test_drain_replayed_backlog_parses_but_never_reads_fresh() {
        // THE 2026-07-06 false-recovery regression class, end-to-end through
        // the REAL drain body: a disable→re-enable replay of pre-disable
        // bytes (old capture_ns) must PERSIST (parsed_a_tick == true, zero
        // loss) yet never count as fresh liveness evidence — while a line
        // the sidecar captures AFTER the floor must latch.
        let dir = retail_tmp_dir("fresh_capture_gate");
        let tick_file = dir.join("live-ticks.ndjson");
        let reg = tickvault_common::feed_health::FeedHealthRegistry::new();
        let mut state = retail_fresh_state();

        // The live floor, as the run loop would advance it on a disabled wake.
        let floor = receipt_ist_nanos();
        let old_capture_utc =
            floor - tickvault_common::constants::IST_UTC_OFFSET_NANOS - 3_600 * NANOS_PER_SECOND;
        let line_old = format!(
            "{{\"security_id\":1001,\"segment\":\"NSE_EQ\",\"ts_ist_nanos\":{RETAIL_BASE_TS_NANOS},\"exchange_ts_millis\":{},\"ltp\":100.5,\"cum_volume\":10,\"capture_ns\":{old_capture_utc}}}\n",
            RETAIL_BASE_TS_NANOS / 1_000_000
        );
        std::fs::write(&tick_file, &line_old).expect("seed pre-disable backlog line");
        assert!(
            state.drain_new_data(&tick_file, &reg).await,
            "the replayed backlog line must still parse + persist (zero loss)"
        );
        assert!(
            !state.wake_had_fresh_capture(floor),
            "a pre-floor capture stamp must NOT read as fresh liveness — this is the \
             false-FeedRecovered / pinned-gauge regression class"
        );

        // A line WITHOUT capture_ns (reconcile-sweep / old format) also never
        // latches — only the status channel can prove those flows live.
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&tick_file)
                .expect("open append");
            f.write_all(retail_line(1).as_bytes()).expect("append");
        }
        assert!(state.drain_new_data(&tick_file, &reg).await);
        assert!(
            !state.wake_had_fresh_capture(floor),
            "a stamp-less line must not satisfy the freshness gate"
        );

        // A genuinely fresh capture (stamped after the floor) latches.
        let fresh_capture_utc =
            receipt_ist_nanos() - tickvault_common::constants::IST_UTC_OFFSET_NANOS;
        let ts2 = RETAIL_BASE_TS_NANOS + 2 * NANOS_PER_SECOND;
        let line_fresh = format!(
            "{{\"security_id\":1002,\"segment\":\"NSE_EQ\",\"ts_ist_nanos\":{ts2},\"exchange_ts_millis\":{},\"ltp\":101.5,\"cum_volume\":11,\"capture_ns\":{fresh_capture_utc}}}\n",
            ts2 / 1_000_000
        );
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&tick_file)
                .expect("open append");
            f.write_all(line_fresh.as_bytes()).expect("append fresh");
        }
        assert!(state.drain_new_data(&tick_file, &reg).await);
        assert!(
            state.wake_had_fresh_capture(floor),
            "a capture stamped after the floor is the strongest liveness proof"
        );

        // The per-wake evidence resets: a quiet wake (nothing new) must not
        // re-report the previous wake's freshness.
        assert!(!state.drain_new_data(&tick_file, &reg).await);
        assert!(
            !state.wake_had_fresh_capture(floor),
            "freshness is per-wake evidence — a quiet wake proves nothing"
        );
        let _ = std::fs::remove_dir_all(&dir);
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
        // Window widened (PR-10, then 2026-07-02 M3 atomic-latch expansion,
        // then 2026-07-06 feed-down alerting: the latched FeedDown Telegram
        // emit now lives inside the falling-edge block) to span the
        // ws_event_audit + FeedDown emit blocks — the latch re-arm lives
        // just below them.
        let after = &src[disable_idx..disable_idx + 4200];
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

    // --- 2026-07-04 boot-visibility parity: boot-time socket-connected edge ---

    #[test]
    fn test_boot_connect_audit_row_is_connected_kind_groww_feed() {
        // The NEW boot-time announcement row (socket connected + subscribed,
        // BEFORE any tick) — Connected kind, feed=Groww, ws_type=GrowwBridge,
        // source `groww_subscribed`, honest awaiting-first-tick reason.
        let row = build_groww_ws_audit_row(
            WsEventKind::Connected,
            "groww_subscribed",
            "groww socket connected + subscribed — awaiting first tick",
        );
        assert_eq!(row.event_kind, WsEventKind::Connected);
        assert_eq!(row.feed, Feed::Groww);
        assert_eq!(row.ws_type, WsType::GrowwBridge);
        assert_eq!(row.source, "groww_subscribed");
        assert!(row.reason.contains("awaiting first tick"), "{}", row.reason);
    }

    #[test]
    fn test_try_announce_boot_connect_latch_fires_once_not_per_poll() {
        // The bridge re-reads the status file every wake (≤1s) — the latch
        // must consume the edge exactly once per connected episode.
        let latches = GrowwAuditLatches::default();
        assert!(latches.try_announce_boot_connect(), "first poll announces");
        for _ in 0..10 {
            assert!(
                !latches.try_announce_boot_connect(),
                "subsequent polls must NEVER re-announce (spam trap)"
            );
        }
    }

    #[test]
    fn test_rearm_boot_connect_latch_rearms_after_genuine_disconnect() {
        // A genuine disconnect falling edge (feed disable / bridge death)
        // re-arms the edge so the NEXT observed subscribe state announces
        // exactly once more — a genuine reconnect is visible, attempts are not.
        let latches = GrowwAuditLatches::default();
        assert!(latches.try_announce_boot_connect());
        assert!(!latches.try_announce_boot_connect());
        latches.rearm_boot_connect();
        assert!(
            latches.try_announce_boot_connect(),
            "post-disconnect subscribe must announce once more"
        );
        assert!(!latches.try_announce_boot_connect(), "and only once");
    }

    #[test]
    fn test_try_announce_feed_down_fires_once_per_episode() {
        // Feed-down alerting (operator 2026-07-06): ONE FeedDown page per
        // DOWN episode — repeated falling edges (idle disable turns, bridge
        // respawn storms) must never re-page (audit-findings Rule 4).
        let latches = GrowwAuditLatches::default();
        assert!(
            latches.try_announce_feed_down(1_000),
            "first falling edge announces"
        );
        for _ in 0..10 {
            assert!(
                !latches.try_announce_feed_down(1_050),
                "subsequent falling edges must NEVER re-announce (spam trap)"
            );
        }
        // First-down-wins: the episode stamp stays at the FIRST edge so
        // down_secs spans the whole episode, not just the latest death.
        assert_eq!(
            latches
                .down_since_epoch_secs
                .load(std::sync::atomic::Ordering::Relaxed),
            1_000
        );
    }

    #[test]
    fn test_take_feed_recovery_returns_down_secs_and_rearms() {
        // Rising edge closes the episode ONCE (down_secs from the FIRST
        // falling edge), then the next falling edge opens a fresh episode.
        let latches = GrowwAuditLatches::default();
        assert!(latches.try_announce_feed_down(1_000));
        assert_eq!(latches.take_feed_recovery(1_154), Some(154));
        assert_eq!(
            latches.take_feed_recovery(1_155),
            None,
            "episode already closed — recovery fires once"
        );
        assert!(
            latches.try_announce_feed_down(2_000),
            "next falling edge opens a fresh episode"
        );
        assert_eq!(latches.take_feed_recovery(2_010), Some(10));
    }

    #[test]
    fn test_take_feed_recovery_none_when_not_down() {
        // No episode in progress → no recovery Telegram (no false-positive
        // "streaming again" on a normal first connect).
        let latches = GrowwAuditLatches::default();
        assert_eq!(latches.take_feed_recovery(1_000), None);
        // A clock stamped 0 at the falling edge is clamped to 1, so the
        // recovery duration never reads as the "not down" sentinel.
        assert!(latches.try_announce_feed_down(0));
        assert_eq!(latches.take_feed_recovery(5), Some(4));
    }

    #[test]
    fn test_should_announce_boot_connect_decision_table() {
        // Announce: known subscribe state + not yet announced + notifier ready.
        assert!(should_announce_boot_connect(true, false, true));
        // Unknown/absent status (forward-compat tag, no file yet) → never.
        assert!(!should_announce_boot_connect(false, false, true));
        // Already announced this episode → never (per-poll spam trap).
        assert!(!should_announce_boot_connect(true, true, true));
        // Notifier slot exists but not yet filled (boot ordering) → defer,
        // do NOT consume the edge — delayed, never lost.
        assert!(!should_announce_boot_connect(true, false, false));
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
    fn test_groww_ws_audit_drop_reason_labels() {
        // 2026-07-05 loud-drop fix: Full → "full", Closed → "closed". Static
        // labels only — the counter `tv_ws_event_audit_dropped_total{reason}`
        // must never see a third value or a formatted row.
        let row = build_groww_ws_audit_row(WsEventKind::Connected, "n/a", "test");
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.try_send(row.clone()).expect("first send fills capacity");
        let full_err = tx.try_send(row.clone()).expect_err("second send is Full");
        assert_eq!(groww_ws_audit_drop_reason(&full_err), "full");
        drop(rx);
        let closed_err = tx.try_send(row).expect_err("send on closed channel");
        assert_eq!(groww_ws_audit_drop_reason(&closed_err), "closed");
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

    #[test]
    fn test_record_groww_tick_producer_site_wired_into_drain() {
        // Scoreboard PR-C producer-half pin (mirror of the Dhan
        // `test_record_dhan_tick_producer_sites_wired_into_tick_processor`
        // ratchet): the Groww lag pipeline has two wiring halves — the
        // PUBLISHER (main.rs supervisor, pinned in secret_manager.rs) and
        // the PRODUCER (the ONE `record_groww_tick` call at the validated
        // drain site). Dropping the producer silently starves the Groww
        // ring below the 50-sample floor: the publisher publishes NOTHING,
        // the groww lag alarm reads notBreaching forever, and the 15:45
        // scorecard lag columns regress to −1 — the exact dark-gauge class
        // of the 2026-07-06 incident, on the second feed.
        let src = include_str!("groww_bridge.rs");
        // Needles are ASSEMBLED at runtime so this test's own literals can
        // never self-match the include_str! scan (the `groww_feed` pattern
        // above).
        let producer_needle = format!("feed_lag_monitor::record_groww_tick{}", "(");
        let producer_call_sites = src
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && !t.starts_with("///") && t.contains(&producer_needle)
            })
            .count();
        assert_eq!(
            producer_call_sites, 1,
            "groww_bridge.rs must invoke the Groww lag producer at EXACTLY 1 \
             non-comment site (the validated drain_new_data hook, AFTER \
             validate_groww_tick); found {producer_call_sites}."
        );
        // The IST-midnight task must reset the Groww DAY histogram (before
        // its trading-day/enabled gates).
        let reset_needle = format!(
            "feed_lag_monitor::reset_day_lag_histogram{}",
            "(Feed::Groww)"
        );
        let reset_sites = src
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && !t.starts_with("///") && t.contains(&reset_needle)
            })
            .count();
        assert_eq!(
            reset_sites, 1,
            "the Groww IST-midnight force-seal task must reset the Groww day \
             lag histogram exactly once (found {reset_sites} sites) — without \
             it Friday's distribution bleeds into Monday's scorecard row."
        );
    }

    #[test]
    fn test_record_presence_producer_site_wired_into_drain() {
        // Scoreboard PR-D producer-half pin (sibling of the lag ratchet
        // above): dropping the presence fold silently empties the Groww
        // side of every coverage slot — the 15:45 drain then reports 375
        // "Dhan unique win" minutes for every paired instrument (a false
        // verdict, not a missing one).
        let src = include_str!("groww_bridge.rs");
        let producer_needle = format!("feed_presence::record_presence{}", "(");
        let producer_call_sites = src
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && !t.starts_with("///") && t.contains(&producer_needle)
            })
            .count();
        assert_eq!(
            producer_call_sites, 1,
            "groww_bridge.rs must fold Groww presence at EXACTLY 1 \
             non-comment site (the validated drain hook, next to \
             record_groww_tick); found {producer_call_sites}."
        );
        // The IST-midnight task must reset the Groww presence bitsets too.
        let reset_needle = format!("feed_presence::reset_daily{}", "(Feed::Groww)");
        let reset_sites = src
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && !t.starts_with("///") && t.contains(&reset_needle)
            })
            .count();
        assert_eq!(
            reset_sites, 1,
            "the Groww IST-midnight force-seal task must reset the Groww \
             presence bitsets exactly once (found {reset_sites} sites)."
        );
        // PR-D fix round 1 (review MEDIUM): the fold must sit behind the
        // same-IST-day gate — a cross-day NDJSON re-tail replay must never
        // fold yesterday's session minutes into today's bitsets.
        let gate_needle = format!("tick_is_same_ist_day{}", "(parsed.tick.ts_ist_nanos");
        assert!(
            src.lines().any(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && t.contains(&gate_needle)
            }),
            "the Groww presence fold must be gated on tick_is_same_ist_day \
             (cross-day replay protection)"
        );
    }

    #[test]
    fn test_tick_is_same_ist_day_gates_cross_day_replay() {
        const DAY_NANOS: i64 = 86_400 * 1_000_000_000;
        let today_open = 20_644 * DAY_NANOS + (9 * 3600 + 15 * 60) * 1_000_000_000;
        let now = 20_644 * DAY_NANOS + 10 * 3600 * 1_000_000_000;
        // Same-day tick folds; yesterday's replayed line does not.
        assert!(tick_is_same_ist_day(today_open, now));
        assert!(!tick_is_same_ist_day(today_open - DAY_NANOS, now));
        // Day boundaries: 23:59:59.999… of yesterday vs 00:00:00 of today.
        assert!(!tick_is_same_ist_day(20_644 * DAY_NANOS - 1, now));
        assert!(tick_is_same_ist_day(20_644 * DAY_NANOS, now));
        // A (validator-bounded) small future skew inside the same day folds.
        assert!(tick_is_same_ist_day(now + 30 * 1_000_000_000, now));
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
        // tolerance is rejected; at/below tolerance passes. (The broken-clock
        // arm is F1 fail-CLOSED since 2026-07-03 — see the dedicated
        // test_validate_groww_tick_fail_closed_on_implausible_receipt_clock.)
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
    }

    #[test]
    fn test_validate_groww_tick_fail_closed_on_implausible_receipt_clock() {
        // F1 (2026-07-03 security review, supersedes the 2026-07-02 fail-open
        // F6 behavior): an implausible receipt clock (0 / negative /
        // degenerate ~1970 / at the plausibility floor) REJECTS every tick —
        // fail CLOSED. A skipped future-skew clamp previously let a tick
        // dated years in the future (inside the static [2020, 2100) bounds)
        // poison the per-feed seal watermark (fetch_max never regresses) and
        // fold garbage into candle cells. A broken host clock is a
        // BOOT-03-class condition; dropping Groww ticks then is strictly
        // safer than corrupting the seal path.
        let p = valid_parsed();
        for broken in [
            0,
            -1,
            tickvault_common::constants::IST_UTC_OFFSET_NANOS, // degenerate ~1970 read
            GROWW_MIN_PLAUSIBLE_RECEIPT_IST_NANOS,             // boundary: floor is exclusive
        ] {
            assert_eq!(
                validate_groww_tick(&p, broken),
                Err(GrowwTickReject::ImplausibleReceiptClock),
                "receipt {broken} must fail closed"
            );
        }
        // Just above the floor the clamp anchors normally: a same-era tick
        // passes, a future-dated one is caught by the clamp (not by the
        // clock guard).
        assert_eq!(validate_groww_tick(&p, p.tick.ts_ist_nanos), Ok(()));
        let mut future = valid_parsed();
        future.tick.ts_ist_nanos = p.tick.ts_ist_nanos + 3600 * 1_000_000_000;
        assert_eq!(
            validate_groww_tick(&future, p.tick.ts_ist_nanos),
            Err(GrowwTickReject::FutureTimestamp)
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
        // 2026-07-13 (disk-retention hardening): the blind 2-day age-delete
        // (`NDJSON_ARCHIVE_KEEP_DAYS`) is RETIRED — retention is now the
        // verified S3 offload sweep (see groww_capture_archive_guard.rs in
        // crates/common for the full contract). Pin the delete-grace constant
        // that replaced it.
        assert!(
            sidecar.contains("ARCHIVE_DELETE_GRACE_SECS = 45 * 60"),
            "archive delete-grace constant must be pinned"
        );
        assert!(
            !sidecar.contains("NDJSON_ARCHIVE_KEEP_DAYS"),
            "the blind age-delete constant must stay retired — deletion goes \
             through the verified S3 offload path only"
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

    // ── boot capture rotation-at-open (2026-07-13, review round 1 F1+F2) ────

    #[test]
    fn test_decide_capture_rotate_keeps_same_day_and_future_mtime() {
        // In-day restart (append continues) + clock-skew future mtime.
        assert_eq!(
            decide_capture_rotate_at_open(20650, 20650, Some(20649), false),
            CaptureRotateAction::Keep
        );
        assert_eq!(
            decide_capture_rotate_at_open(20651, 20650, Some(20649), false),
            CaptureRotateAction::Keep
        );
    }

    #[test]
    fn test_decide_capture_rotate_names_by_snapshot_day() {
        // F2: a snapshot stuck on an OLDER day (QuestDB degraded before the
        // last stop) must name the archive — that is the exact name
        // drain_archive_tail_if_needed probes, so the tail drains from the
        // persisted offset even when the file spans multiple days.
        assert_eq!(
            decide_capture_rotate_at_open(20650, 20651, Some(20647), false),
            CaptureRotateAction::Rotate {
                archive_day: 20647,
                named_by_snapshot: true
            }
        );
    }

    #[test]
    fn test_decide_capture_rotate_falls_back_to_mtime_day() {
        // No snapshot (first boot), legacy ist_day==0, or a same/future-day
        // snapshot → mtime-day fallback name (logged loudly by the wrapper).
        for snap in [None, Some(0), Some(20651), Some(20655)] {
            assert_eq!(
                decide_capture_rotate_at_open(20650, 20651, snap, false),
                CaptureRotateAction::Rotate {
                    archive_day: 20650,
                    named_by_snapshot: false
                },
                "snapshot {snap:?} must fall back to the mtime day"
            );
        }
    }

    #[test]
    fn test_decide_capture_rotate_skips_on_collision() {
        // Target archive exists (dev-Mac in-process midnight rotation the
        // same day) → NEVER overwrite; leave the live file (zero loss,
        // no reclaim — pre-PR behavior for this boot).
        assert_eq!(
            decide_capture_rotate_at_open(20650, 20651, Some(20650), true),
            CaptureRotateAction::SkipCollision { archive_day: 20650 }
        );
    }

    fn backdate_mtime(path: &Path, secs_ago: u64) {
        let mtime = std::time::SystemTime::now() - std::time::Duration::from_secs(secs_ago);
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(mtime))
            .unwrap();
    }

    fn today_ist_day_for_tests() -> i64 {
        ist_day_from_ist_nanos(receipt_ist_nanos())
    }

    #[test]
    fn test_rotate_at_open_uses_snapshot_day_name() {
        let dir = retail_tmp_dir("rotate_open_snap");
        let tick_file = dir.join("live-ticks.ndjson");
        std::fs::write(&tick_file, "{\"ltp\":1}\n").unwrap();
        backdate_mtime(&tick_file, 86_400); // yesterday
        let today = today_ist_day_for_tests();
        let snap_day = today - 3; // stuck snapshot, older than the mtime day
        retail_write_snapshot(
            &tick_file,
            &GrowwOffsetSnapshot {
                offset: 4,
                capture_seq: 9,
                file_len: 10,
                head: "{\"ltp\":1}\n".to_string(),
                ist_day: snap_day,
            },
        );
        let rotated = rotate_stale_groww_capture_at_open_at(&tick_file, today)
            .expect("stale file must rotate");
        assert_eq!(
            rotated,
            archive_path_for_ist_day(&tick_file, snap_day),
            "the archive must carry the SNAPSHOT day name — the exact name \
             the bridge's drain probes (F2)"
        );
        assert!(!tick_file.exists());
        assert!(rotated.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_rotate_at_open_mtime_fallback_without_snapshot() {
        let dir = retail_tmp_dir("rotate_open_mtime");
        let tick_file = dir.join("live-ticks.ndjson");
        std::fs::write(&tick_file, "{\"ltp\":2}\n").unwrap();
        backdate_mtime(&tick_file, 2 * 86_400);
        let today = today_ist_day_for_tests();
        let rotated = rotate_stale_groww_capture_at_open_at(&tick_file, today)
            .expect("stale file must rotate on the first-boot edge too");
        // mtime two days back → the mtime IST day is today-2 (exact value
        // depends on the IST wall clock; assert via the same helper).
        let meta_day = ist_day_from_unix_secs(
            // APPROVED: test-only clock math.
            (std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 86_400))
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64,
        );
        assert_eq!(rotated, archive_path_for_ist_day(&tick_file, meta_day));
        assert!(!tick_file.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_rotate_at_open_collision_leaves_live_file() {
        let dir = retail_tmp_dir("rotate_open_collision");
        let tick_file = dir.join("live-ticks.ndjson");
        std::fs::write(&tick_file, "{\"ltp\":3}\n").unwrap();
        backdate_mtime(&tick_file, 86_400);
        let today = today_ist_day_for_tests();
        let snap_day = today - 1;
        retail_write_snapshot(
            &tick_file,
            &GrowwOffsetSnapshot {
                offset: 0,
                capture_seq: 1,
                file_len: 10,
                head: "{\"ltp\":3}\n".to_string(),
                ist_day: snap_day,
            },
        );
        let target = archive_path_for_ist_day(&tick_file, snap_day);
        std::fs::write(&target, "prior archive — never overwritten\n").unwrap();
        assert_eq!(
            rotate_stale_groww_capture_at_open_at(&tick_file, today),
            None,
            "collision must skip the rotation"
        );
        assert!(
            tick_file.exists(),
            "live file left in place (honest degrade)"
        );
        let prior = std::fs::read_to_string(&target).unwrap();
        assert!(prior.contains("never overwritten"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_rotate_at_open_keeps_fresh_missing_and_empty_files() {
        let dir = retail_tmp_dir("rotate_open_keep");
        let tick_file = dir.join("live-ticks.ndjson");
        let today = today_ist_day_for_tests();
        // Missing file.
        assert_eq!(
            rotate_stale_groww_capture_at_open_at(&tick_file, today),
            None
        );
        // Fresh (today) file — in-day restart must keep appending.
        std::fs::write(&tick_file, "{\"ltp\":4}\n").unwrap();
        assert_eq!(
            rotate_stale_groww_capture_at_open_at(&tick_file, today),
            None
        );
        assert!(tick_file.exists());
        // Empty stale file — nothing worth archiving.
        std::fs::write(&tick_file, "").unwrap();
        backdate_mtime(&tick_file, 86_400);
        assert_eq!(
            rotate_stale_groww_capture_at_open_at(&tick_file, today),
            None
        );
        assert!(tick_file.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Feed gap-episode tracker (FEED-GAP-01, 2026-07-14)
    // -----------------------------------------------------------------------

    const GAP_NOW: i64 = 1_780_000_000_000_000_000;
    const SEC: i64 = 1_000_000_000;

    #[test]
    fn test_feed_gap_episode_threshold_is_10s() {
        // Operator-approved S4 design pin (2026-07-14): below the
        // FEED-STALL-01 restart threshold, above the ≤1s wake cadence.
        assert_eq!(FEED_GAP_EPISODE_THRESHOLD_SECS, 10);
    }

    #[test]
    fn test_gap_tracker_never_streamed_never_opens() {
        let mut t = FeedGapTracker::default();
        let (edge, add) = t.poll(None, GAP_NOW, true);
        assert_eq!(edge, FeedGapEdge::None);
        assert_eq!(add, 0);
    }

    #[test]
    fn test_gap_tracker_no_open_below_threshold() {
        let mut t = FeedGapTracker::default();
        // Seed liveness (raw last-tick stamp = now).
        let _ = t.poll(Some(GAP_NOW), GAP_NOW, true);
        let (edge, _) = t.poll(Some(GAP_NOW), GAP_NOW + 9 * SEC, true);
        assert_eq!(edge, FeedGapEdge::None);
    }

    #[test]
    fn test_gap_tracker_opens_at_threshold_once_per_episode() {
        let mut t = FeedGapTracker::default();
        let _ = t.poll(Some(GAP_NOW), GAP_NOW, true);
        let (edge, _) = t.poll(Some(GAP_NOW), GAP_NOW + 10 * SEC, true);
        assert!(
            matches!(edge, FeedGapEdge::Open { last_tick_nanos, .. } if last_tick_nanos == GAP_NOW),
            "expected Open at ≥10s, got {edge:?}"
        );
        // Silence continues — NO second Open (one bubble per episode).
        let (edge2, _) = t.poll(Some(GAP_NOW), GAP_NOW + 20 * SEC, true);
        assert_eq!(edge2, FeedGapEdge::None);
    }

    #[test]
    fn test_gap_tracker_no_open_out_of_session() {
        let mut t = FeedGapTracker::default();
        let _ = t.poll(Some(GAP_NOW), GAP_NOW, false);
        let (edge, _) = t.poll(Some(GAP_NOW), GAP_NOW + 60 * SEC, false);
        assert_eq!(
            edge,
            FeedGapEdge::None,
            "out-of-session silence is not a gap"
        );
    }

    #[test]
    fn test_gap_tracker_closes_on_liveness_advance_with_measured_gap() {
        let mut t = FeedGapTracker::default();
        let _ = t.poll(Some(GAP_NOW), GAP_NOW, true);
        let _ = t.poll(Some(GAP_NOW), GAP_NOW + 12 * SEC, true); // Open
        // Recovery: a fresh tick lands (raw stamp jumps to now+35s).
        let (edge, add) = t.poll(Some(GAP_NOW + 35 * SEC), GAP_NOW + 35 * SEC, true);
        match edge {
            FeedGapEdge::Close {
                last_tick_before_nanos,
                end_nanos,
                gap_secs,
                ..
            } => {
                assert_eq!(last_tick_before_nanos, GAP_NOW);
                assert_eq!(end_nanos, GAP_NOW + 35 * SEC);
                assert_eq!(gap_secs, 35, "gap measured last-tick → recovery");
            }
            other => panic!("expected Close, got {other:?}"),
        }
        // The unconditional counter also accumulates the measured gap.
        assert_eq!(add, 35);
        // Fully re-armed: a later gap opens a NEW episode.
        let (edge2, _) = t.poll(Some(GAP_NOW + 35 * SEC), GAP_NOW + 46 * SEC, true);
        assert!(matches!(edge2, FeedGapEdge::Open { .. }));
    }

    #[test]
    fn test_gap_tracker_counter_accumulates_sub_threshold_gaps() {
        let mut t = FeedGapTracker::default();
        let _ = t.poll(Some(GAP_NOW), GAP_NOW, true);
        // 5s micro-gap: below the 10s episode threshold but ≥ the 2s floor —
        // no episode, counter still accumulates (threshold-independent).
        let (edge, add) = t.poll(Some(GAP_NOW + 5 * SEC), GAP_NOW + 5 * SEC, true);
        assert_eq!(edge, FeedGapEdge::None);
        assert_eq!(add, 5);
        // 1s normal cadence: below the floor — never counted.
        let (_, add2) = t.poll(Some(GAP_NOW + 6 * SEC), GAP_NOW + 6 * SEC, true);
        assert_eq!(add2, 0);
    }

    #[test]
    fn test_gap_tracker_floored_age_jitter_is_not_an_advance() {
        // Review CRITICAL (2026-07-14): reconstructing the last-tick stamp
        // from the FLOORED age (`now - age*1e9`) yields `T + (now-T) mod 1s`
        // — a value that jitters inside [T, T+1s) on every poll. Under the
        // old strict-`>` advance check that sub-second creep counted as a
        // fresh tick each wake: OPEN never latched and jitter could flap
        // open/close. The 1s hysteresis + raw-stamp contract must hold: the
        // episode OPENS exactly once, never flap-closes, and the counter
        // never accumulates from creep.
        let mut t = FeedGapTracker::default();
        let t0 = GAP_NOW;
        let _ = t.poll(Some(t0), t0, true);
        let mut opened = 0usize;
        for i in 1..=20i64 {
            // Uneven poll cadence so the modeled remainder actually moves.
            let now = t0 + i * SEC + 300_000_000 * (i % 3);
            let jittered = t0 + (now - t0).rem_euclid(SEC); // < t0 + 1s always
            let (edge, add) = t.poll(Some(jittered), now, true);
            assert_eq!(add, 0, "sub-second creep must never feed the counter");
            match edge {
                FeedGapEdge::Open { .. } => opened += 1,
                FeedGapEdge::Close { .. } => {
                    panic!("sub-second jitter flap-closed the episode at poll {i}")
                }
                FeedGapEdge::None => {}
            }
        }
        assert_eq!(opened, 1, "episode must OPEN exactly once despite jitter");
        // A REAL new tick (full ≥1s raw-stamp jump) closes it with the
        // measured gap (~40s; the stored pre-gap reference carries the
        // caller-supplied jittered stamp, so the floor lands on 39-40 —
        // production callers pass RAW stamps, making it exact).
        let (edge, _) = t.poll(Some(t0 + 40 * SEC), t0 + 40 * SEC, true);
        assert!(
            matches!(edge, FeedGapEdge::Close { gap_secs, .. } if (39..=40).contains(&gap_secs)),
            "real recovery tick must Close with the measured gap, got {edge:?}"
        );
    }

    #[test]
    fn test_ist_hhmm_label() {
        // 10:15:30 IST on some day: seconds-of-day 36930.
        let nanos = (86_400 * 20_000 + 36_930) * SEC;
        assert_eq!(ist_hhmm_label(nanos), "10:15");
    }

    #[test]
    fn test_partial_minute_labels_overlap_and_bound() {
        // Gap 10:15:50 → 10:17:05 overlaps buckets 10:15, 10:16, 10:17.
        let day = 86_400 * 20_000 * SEC;
        let start = day + (10 * 3_600 + 15 * 60 + 50) * SEC;
        let end = day + (10 * 3_600 + 17 * 60 + 5) * SEC;
        assert_eq!(partial_minute_labels(start, end), "10:15,10:16,10:17");
        // Bound: a multi-hour gap collapses past the cap with an ellipsis.
        let long_end = day + (12 * 3_600) * SEC;
        let labels = partial_minute_labels(start, long_end);
        assert!(
            labels.ends_with(",…"),
            "bounded list must end with ellipsis: {labels}"
        );
        assert_eq!(
            labels.trim_end_matches(",…").split(',').count(),
            FEED_GAP_PARTIAL_MINUTES_MAX
        );
        // Degenerate: end before start → empty.
        assert_eq!(partial_minute_labels(end, start), "");
    }
}

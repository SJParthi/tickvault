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
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tracing::{error, info, warn};

use tickvault_api::feed_state::{Feed, FeedRuntimeState};
use tickvault_common::config::QuestDbConfig;
use tickvault_common::tick_types::ParsedTick;
use tickvault_common::types::ExchangeSegment;
use tickvault_storage::groww_persistence::{GrowwLiveTickRow, GrowwLiveTickWriter};
use tickvault_storage::seal_writer_runner::global_seal_sender;
use tickvault_trading::candles::{BufferedSeal, FeedStrategy, MultiTfAggregator, TfIndex};

/// Capacity hint for the Groww feed's own [`MultiTfAggregator`] instance —
/// matches the NTM-union universe headroom (operator §31). The container grows
/// lazily; this only sizes the initial papaya table.
const GROWW_AGGREGATOR_CAPACITY: usize = 1_200;

/// Poll interval (milliseconds) for tailing the sidecar's append-only tick file.
const GROWW_BRIDGE_POLL_MS: u64 = 500;
/// Poll interval for tailing the sidecar's append-only tick file.
const GROWW_BRIDGE_POLL: Duration = Duration::from_millis(GROWW_BRIDGE_POLL_MS);

/// Max bytes read from the tick file per poll (hostile-review HIGH 2026-06-19).
/// Bounds the per-poll allocation so a large backlog on restart (e.g. a full
/// trading day of ~779-instrument NDJSON) is drained in fixed 4 MiB chunks
/// across polls instead of one multi-GB `read_to_string` that would OOM the
/// 8 GiB host. At 500 ms/poll this drains ~8 MiB/s — far above the live rate.
const GROWW_BRIDGE_MAX_READ_BYTES: u64 = 4 * 1024 * 1024;

/// Default path of the Python sidecar's capture-at-receipt append-only tick file.
pub const GROWW_TICK_FILE_DEFAULT: &str = "data/groww/live-ticks.ndjson";

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
/// engine keys on `(u32 security_id, u8 segment_code)` and buckets on a `u32`
/// IST-second timestamp). REJECT LOUDLY rather than a silent `as u32` alias.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GrowwAggregateReject {
    /// `security_id` (Groww `exchange_token`) does not fit `u32` (max observed
    /// 1,175,236 ≪ u32::MAX, so this is a defense-in-depth guard against a future
    /// Groww schema change — NEVER a silent truncation per the adversarial review).
    TokenExceedsU32,
    /// The IST-second timestamp (`ts_ist_nanos / 1e9`) does not fit `u32`
    /// (epoch-seconds overflow u32 only ~2106 — an honest documented bound).
    TimestampExceedsU32,
}

/// Builds the shared-engine [`ParsedTick`] from a validated Groww tick.
///
/// Pure + testable. Performs the two WIDTH guards the adversarial review flagged:
/// - `security_id: i64 → u32` via `u32::try_from` — REJECT on overflow, never `as u32`.
/// - `ts_ist_nanos → IST seconds → u32` — REJECT on overflow.
///
/// Groww has NO `day_open` / `day_close` / `oi`, so those `ParsedTick` fields are
/// left 0: the cell opens the day's first bar at the first-tick LTP (day_open=0
/// fallback), and the pct/oi columns stay 0 (missing data, NOT forked logic).
/// The `i64` `cum_volume` is NOT placed in `ParsedTick.volume` (a `u32` that would
/// truncate) — it is carried separately as the consume_tick override.
fn build_parsed_tick(parsed: &ParsedGrowwTick) -> Result<ParsedTick, GrowwAggregateReject> {
    let security_id = u32::try_from(parsed.tick.security_id)
        .map_err(|_| GrowwAggregateReject::TokenExceedsU32)?;
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

/// Dormant consumer driver: tails the sidecar's append-only NDJSON tick file,
/// persists each raw tick to the SHARED `ticks` table (feed='groww'), folds them
/// through the 1m aggregator, and persists sealed candles to the SHARED
/// `candles_1m` table (feed='groww'). Runs ONLY when `feeds.groww_enabled`;
/// writes ONLY `feed='groww'`-tagged rows; never touches the Dhan write path.
/// Idles (no writes) until the Python sidecar starts appending to
/// `tick_file_path`. Loops forever; per-line/flush errors are logged, never
/// propagated (a bad line or a transient QuestDB blip must not kill the feed).
// TEST-EXEMPT: file-tail + live-QuestDB ILP I/O driver; the pure primitives it
// uses (parse_groww_tick_line, segment_from_str, live_tick_row,
// build_parsed_tick, next_capture_seq) are unit-tested below.
pub async fn run_groww_bridge(
    qdb: QuestDbConfig,
    tick_file_path: PathBuf,
    feed_runtime: Arc<FeedRuntimeState>,
    // Live-feed health (operator 2026-06-22): record Groww ticks/candles +
    // connected-state so GET /api/feeds/health reports the truthful verdict.
    feed_health: Arc<tickvault_common::feed_health::FeedHealthRegistry>,
) {
    info!(
        path = %tick_file_path.display(),
        "Groww bridge started — tailing the sidecar tick file (idles until the sidecar appends)"
    );
    let mut live_writer = GrowwLiveTickWriter::new(&qdb);
    // The Groww feed's OWN 21-TF aggregator INSTANCE — the SAME engine code as
    // Dhan (`MultiTfAggregator`), parameterized per-feed by FeedStrategy::GROWW.
    // A separate instance keeps Dhan/Groww from cross-keying; the SHARED
    // seal-writer (`global_seal_sender`) routes each seal by its `feed`.
    let aggregator = MultiTfAggregator::with_capacity(GROWW_AGGREGATOR_CAPACITY);
    // Per-instrument first-seen set: on first sight we pre_populate + seed the
    // cumulative baseline (Groww's running day-volume) so the first 1m bucket's
    // volume matches the legacy Groww 1m aggregator behaviour (golden-tested). O(1) lookups.
    let mut seeded: std::collections::HashSet<(u32, u8)> = std::collections::HashSet::new();
    let _ = &qdb; // candle persistence is now the shared seal-writer chain, not a Groww writer.
    let capture_seq = AtomicI64::new(0);
    let mut offset: u64 = 0;
    // Byte residual (not String): a bounded chunked read can split a multi-byte
    // UTF-8 char at the chunk boundary; buffering raw bytes and splitting on the
    // 0x0A newline (never part of a multi-byte sequence) keeps partial chars +
    // partial lines safely buffered until the next poll completes them.
    let mut residual: Vec<u8> = Vec::new();

    loop {
        tokio::time::sleep(GROWW_BRIDGE_POLL).await;

        // Feed-toggle API (operator 2026-06-19): live pause/resume. When Groww is
        // runtime-disabled via `POST /api/feeds/groww {enabled:false}`, idle — no
        // file read, no writes. Any in-flight batch already past this check
        // completed its flush; re-enabling resumes from the persisted `offset`
        // with NO double-read and NO tick loss. Honest caveat: a pause that spans
        // a minute boundary leaves the open candle bucket unsealed until a later
        // tick arrives on resume — the same accuracy gap as any feed outage, not
        // new loss (every raw tick is still persisted exactly once).
        if !feed_runtime.is_enabled(Feed::Groww) {
            continue;
        }

        let mut file = match File::open(&tick_file_path).await {
            Ok(f) => {
                // The sidecar's tick file is present + readable → Groww source up.
                feed_health.set_connected(Feed::Groww, true);
                f
            }
            Err(_) => {
                // sidecar not started yet — idle, no writes; report disconnected.
                feed_health.set_connected(Feed::Groww, false);
                continue;
            }
        };
        let len = match file.metadata().await {
            Ok(m) => m.len(),
            Err(err) => {
                warn!(?err, "groww bridge: metadata failed");
                continue;
            }
        };
        if len < offset {
            // File shrank (rotation/truncate) — restart from the top.
            offset = 0;
            residual.clear();
        }
        if len == offset {
            continue; // nothing new
        }
        if file.seek(SeekFrom::Start(offset)).await.is_err() {
            continue;
        }
        // Bounded chunked read (HIGH fix): read at most GROWW_BRIDGE_MAX_READ_BYTES
        // this poll — never the whole (possibly multi-GB) file at once.
        let to_read = (len - offset).min(GROWW_BRIDGE_MAX_READ_BYTES);
        let mut chunk: Vec<u8> = Vec::new();
        match (&mut file).take(to_read).read_to_end(&mut chunk).await {
            Ok(n) => offset = offset.saturating_add(n as u64),
            Err(err) => {
                warn!(?err, "groww bridge: read failed");
                continue;
            }
        }

        residual.extend_from_slice(&chunk);
        let mut wrote_live = false;
        // Drain only COMPLETE newline-terminated lines; a trailing partial line
        // (incl. a partial UTF-8 char from the chunk boundary) stays buffered.
        let prefix = drain_complete_prefix(&mut residual);
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
            let seq = next_capture_seq(&capture_seq, parsed.tick.ts_ist_nanos);
            if let Err(err) = live_writer.append_row(&live_tick_row(&parsed, seq)) {
                error!(
                    ?err,
                    "groww bridge: shared ticks (feed=groww) append failed"
                );
            } else {
                wrote_live = true;
                // Live-feed health: a Groww tick was captured (O(1) atomic).
                // Record WALL-CLOCK receipt time, NOT the exchange ts — the
                // health verdict's last-tick-age math compares against the
                // endpoint's `Utc::now()+IST` clock, so a stale/replayed
                // exchange ts (or any exchange-vs-host clock skew) must not be
                // read as "feed silent". Receipt time = when WE saw the tick.
                feed_health.record_tick(Feed::Groww, receipt_ist_nanos());
            }
            // Fold through the SHARED 21-TF engine (ONE common candle engine).
            // Build the ParsedTick with the u32 token + u32 timestamp WIDTH
            // guards; reject loudly on overflow (never a silent `as u32` alias).
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
            // Groww cum_volume is i64; `validate_groww_tick` already guarantees
            // it is >= 0. Convert with `try_from` + reject loudly (security-review
            // LOW 2026-06-23) — consistent with the token/timestamp width guards,
            // never a silent `as u64`.
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
            // First sight of this instrument: register it + seed the cumulative
            // baseline so the first 1m bucket's volume matches the legacy Groww
            // (golden-tested). Idempotent — only on the very first tick per key.
            if seeded.insert(key) {
                aggregator.pre_populate(std::iter::once(key));
                aggregator.seed_cumulative(pt.security_id, seg_code, cum_volume_u64);
            }
            // Route every sealed bar across ALL 21 TFs through the SHARED
            // seal-writer chain, tagged feed=Groww. `Some(cum as u64)` carries
            // Groww's i64 cumulative without u32 truncation.
            let stats = aggregator.consume_tick(
                &pt,
                seg_code,
                FeedStrategy::GROWW,
                Some(cum_volume_u64),
                |tf, state| {
                    let Some(sender) = global_seal_sender() else {
                        return;
                    };
                    let seal = BufferedSeal::new(pt.security_id, seg_code, tf, state, Feed::Groww);
                    if sender.try_send(seal).is_err() {
                        // Mirror the Dhan path: the seal mpsc is full (writer
                        // behind). Counter only — the ring→spill→DLQ chain
                        // downstream is the durable absorber; never panic.
                        metrics::counter!("tv_seal_mpsc_dropped_total", "feed" => "groww")
                            .increment(1);
                    } else if tf == TfIndex::M1 {
                        // Live-feed health: count one Groww candle per sealed
                        // 1-minute bar (the cross-verified slice).
                        feed_health.record_candle(Feed::Groww);
                    }
                },
            );
            // A lazily-missing instrument cannot happen here (we pre_populate on
            // first sight), but mirror the Dhan diagnostic defensively.
            if !stats.instrument_found {
                aggregator.pre_populate(std::iter::once(key));
            }
        }
        // Flush failures are data-at-risk (ILP-buffered Groww rows may not have
        // reached QuestDB) — `error!` per audit Rule 5 / charter Rule 6 so they
        // route to Telegram via ERROR-level Loki routing, never silent `warn!`.
        // Candle persistence is now the SHARED seal-writer chain (its own task
        // owns the flush + ring→spill→DLQ), so only the raw-tick writer flushes here.
        if wrote_live && let Err(err) = live_writer.flush() {
            error!(?err, "groww bridge: shared ticks (feed=groww) flush failed");
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
    fn test_build_parsed_tick_rejects_token_above_u32_max() {
        // Defense-in-depth (adversarial review): a Groww exchange_token that does
        // not fit u32 is rejected LOUDLY, never silently aliased via `as u32`.
        let mut p = valid_parsed();
        p.tick.security_id = i64::from(u32::MAX) + 1;
        assert!(
            matches!(
                build_parsed_tick(&p),
                Err(GrowwAggregateReject::TokenExceedsU32)
            ),
            "token above u32::MAX must reject loudly, never `as u32` alias"
        );
        // Boundary: exactly u32::MAX is accepted.
        p.tick.security_id = i64::from(u32::MAX);
        assert!(build_parsed_tick(&p).is_ok());
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
}

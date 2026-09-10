//! Candle-engine re-architecture #T1a — `ShadowCandleWriter` ILP append struct.
//!
//! Owns the questdb-rs `Sender` + `Buffer` and exposes
//! `append_seal(&BufferedSeal)` which fills the ILP buffer for the
//! correct plain `candles_<tf>` table by dispatching through
//! [`ShadowSealRow::from_buffered_seal`].
//!
//! ## Mirrors `LiveCandleWriter` connection-management pattern
//!
//! Like `crates/storage/src/candle_persistence.rs::LiveCandleWriter`,
//! the constructor is **lazy** — if QuestDB is unreachable at startup
//! the writer initialises with `sender = None` and an empty `Buffer`,
//! and a future reconnect attempt rebuilds both. This is the standard
//! tickvault resilience pattern for storage writers.
//!
//! ## What this slice ships
//!
//! - [`ShadowCandleWriter`] struct (lazy-connect ILP writer for the
//!   21 plain `candles_<tf>` tables).
//! - [`ShadowCandleWriter::new`] — production constructor.
//! - [`ShadowCandleWriter::append_seal`] — fills the ILP buffer for the
//!   right candle table; uses [`ShadowSealRow`] for column dispatch.
//! - Observability accessors: `is_connected`, `pending_count`,
//!   `buffer_byte_count`, `buffer_row_count`.
//! - Unit tests covering: lazy disconnect-OK construction; appends
//!   fill the buffer; row count grows; bytes contain the expected
//!   table name + segment string + IST timestamp; multi-TF dispatch;
//!   I-P1-11 segment isolation in the wire bytes; pending_count
//!   tracking; unconnected-flush noop; capacity reset semantics.
//!
//! ## What this slice does NOT ship
//!
//! - The async writer task that drains [`SealAbsorptionPipeline`]
//!   (item 1.2f.3).
//! - In-flight tracking for spill-rescue on flush failure (item 1.2f.3).
//! - Reconnect throttling (item 1.2f.3).
//! - `flush()` real implementation calling `Sender::flush(&buffer)`
//!   (it is in this slice but UNTESTED without a live QuestDB; tests
//!   exercise only the `append_seal` + buffer-state path).
//! - Boot wiring + Prometheus counters.

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use secrecy::{ExposeSecret, SecretString};
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_trading::candles::BufferedSeal;

use crate::shadow_seal_columns::ShadowSealRow;

/// Build the questdb-rs **ILP-over-HTTP** connection conf string for a given
/// host + HTTP port.
///
/// **2026-07-06 exam-fix (the silent candle-persist bug):** this writer
/// previously used ILP **TCP** (`tcp::addr=host:ilp_port`). ILP TCP flush is
/// fire-and-forget — a server-side reject (schema drift, type mismatch, table
/// error) NEVER surfaced as `Err`, and QuestDB then silently closes the
/// socket. On the 2026-07-06 groww-only exam session that produced the
/// worst-case failure signature: every drain cycle "flushed Ok" (or quietly
/// recovered via reconnect + replay at `debug!`), the AGGREGATOR-SEAL-01
/// error path never fired, and ZERO of ~71K sealed candles reached the
/// `candles_*` tables — total silence. HTTP flush returns a real per-request
/// server ACK, so a reject surfaces as `Err` → `drain_once` fires
/// `AGGREGATOR-SEAL-01` and the ring→spill→DLQ rescue engages. This is the
/// SAME transport fix already proven on `ws_event_audit_persistence.rs`
/// (2026-07-05 empty-table incident). The seal writer is a cold path (100 ms
/// drain cycles, ≤1024 rows/batch) — HTTP round-trip latency is irrelevant.
/// `protocol_version=1` is pinned so the retained-buffer reconnect path is
/// version-consistent: the disconnected fallback buffer, `new()`'s buffer, and
/// every re-built sender all speak the V1 text line protocol — a reconnect can
/// therefore always replay the SAME buffer without a client-side
/// protocol-version mismatch (and no auto-detect settings round-trip).
///
/// **2026-07-06 hardening (hostile-review HIGH — blocking-flush bound):** the
/// questdb-rs 6.1.0 defaults are `retry_timeout=10s` (an INTERNAL
/// sleep-and-resend loop inside every `sender.flush`) and
/// `request_timeout=10s`. Left at defaults, one outer flush() during a hung
/// QuestDB outage blocked synchronously up to ~80-120s (4 outer attempts ×
/// (10s internal retry + ~10s in-flight request)) — on the shared tokio
/// runtime. The conf now pins:
/// - `retry_timeout=[`SEAL_FLUSH_ILP_RETRY_TIMEOUT_MS`]` (= 0) — the internal
///   questdb-rs retry loop is DISABLED; the writer's own bounded
///   reconnect+replay loop in [`ShadowCandleWriter::flush`] is the single
///   owner of retry policy (no doubled retries).
/// - `request_timeout=[`SEAL_FLUSH_REQUEST_TIMEOUT_MS`]` (= 5000 ms) — one
///   HTTP POST is bounded to ~5s (+ the questdb-rs min-throughput extension,
///   ≤ ~2.5s at the 1024-seal max batch), so a failed drain cycle blocks at
///   most ~4 × ~7.5s ≈ 30s worst-case against a black-holed server (and
///   fails in milliseconds on connection-refused). The loop additionally
///   wraps the cycle in `block_in_place` on the production multi-thread
///   runtime (see `seal_writer_loop.rs`) so even that bounded block never
///   pins a tokio worker's other tasks.
fn build_ilp_conf_string(host: &str, http_port: u16) -> String {
    format!(
        "http::addr={host}:{http_port};protocol_version=1;retry_timeout={SEAL_FLUSH_ILP_RETRY_TIMEOUT_MS};request_timeout={SEAL_FLUSH_REQUEST_TIMEOUT_MS};"
    )
}

/// Per-request HTTP timeout for one seal-flush POST, in milliseconds
/// (questdb-rs conf-string durations are milliseconds). Bounds a single
/// flush attempt against a hung/black-holed QuestDB; connection-refused
/// still fails in milliseconds. 5s is generous for a ≤1024-row (~256 KB)
/// LAN batch.
const SEAL_FLUSH_REQUEST_TIMEOUT_MS: u64 = 5_000;

/// questdb-rs INTERNAL retry budget, in milliseconds. Pinned to 0 —
/// disabled — because [`ShadowCandleWriter::flush`]'s bounded
/// reconnect+replay loop is the single owner of retry policy; the library's
/// hidden sleep-and-resend loop would multiply every outer attempt by up to
/// 10s of extra synchronous blocking.
const SEAL_FLUSH_ILP_RETRY_TIMEOUT_MS: u64 = 0;

/// Broker-source label this writer stamps on every candle row.
///
/// Operator lock 2026-06-19 ("same tables + feed column"): the Dhan candle
/// writer stamps a constant `feed='dhan'` so a Dhan candle and a Groww
/// candle (`feed='groww'`) for the SAME `(ts, security_id, segment)` minute
/// are BOTH kept by the `DEDUP_KEY_CANDLES` 4-column key — distinct broker
/// feeds are distinct observations, never a duplicate. A `&'static str`
/// constant: zero-alloc, O(1), replay-stable (never per-row computed).
// SP1: sourced from the single canonical feed identity (`common::feed::Feed`).
// Value unchanged ("dhan") → DEDUP key + replay-stability byte-identical.
pub const CANDLE_FEED_DHAN: &str = tickvault_common::feed::Feed::Dhan.as_str();

/// Counter for candle rows refused by the session-window gate.
///
/// A REFUSAL, not a loss. `tv_dhan_feed_seals_dropped_total` means "a sealed
/// candle was discarded and is gone", and it feeds AGGREGATOR-DROP-01; this
/// means "a bar whose BUCKET falls outside 09:00-15:39:59 IST, which we
/// declined to write".
pub const CANDLE_OUT_OF_WINDOW_COUNTER: &str = "tv_candle_rows_out_of_window_refused_total";

/// The two reason labels the candle gate reports.
///
/// # Why `bucket_` and not `ts_` or `arrival_`
///
/// A candle's `ts` is its BUCKET-OPEN instant (`shadow_seal_columns.rs` sets
/// `timestamp_ist_nanos = bucket_start_ist_secs * 1e9`), not an event time and
/// not an arrival time. Saying `ts_out_of_window` would invite the reader to
/// look for a tick clock that does not exist here.
///
/// # ⚠ Why the gate reads the BUCKET and NEVER a seal or receipt clock
///
/// This is the trap an adversarial review was commissioned to find, and it is
/// real. A bucket cannot be sealed before its last second has elapsed: the
/// 15:39 one-minute bar has exclusive end 15:40:00, and `catch_up_seal` only
/// releases it once the watermark passes `bucket_end + CATCHUP_LATENESS_MARGIN`
/// = **15:40:02** — two seconds PAST the window's exclusive end. The close-time
/// `force_seal_all` runs later still, after every socket sender is dropped, and
/// the boot recovery drain replays spilled seals the NEXT MORNING.
///
/// So a gate on any seal or receipt clock would have silently discarded the
/// FINAL BAR of every emitted timeframe, every session, plus every rescued bar
/// — while reporting success. Gating the bucket is safe by construction:
/// `TfIndex::bucket_start` anchors every bucket at
/// `CANDLE_SESSION_OPEN_SECS_OF_DAY_IST` (09:00) and is monotone within the
/// day, so the first emitted bucket is 09:00:00 and the last one-minute bucket
/// opens at 15:39:00 — both inside the window at its inclusive and exclusive
/// edges respectively.
///
/// That anchoring also disposes of a second worry: a DAILY bar does NOT stamp
/// midnight. `bucket_start` returns the session open for D1, so its `ts` is
/// 09:00:00 IST and it would pass this gate — which matters because D1 could
/// be re-enabled by a future dated quote.
pub const CANDLE_OUT_OF_WINDOW_REASONS: [&str; 2] =
    ["bucket_out_of_window", "bucket_out_of_plausible_band"];

/// Pre-resolved candle refusal counters.
///
/// # Why no `feed` label, unlike every sibling loss counter
///
/// `ShadowCandleWriter` is the FEED-AGNOSTIC write boundary by design — its
/// own doc says so, and `row.feed` is stamped verbatim from whichever seal
/// arrived, with no match on it anywhere. The writer therefore has no feed of
/// its own to bind a handle to, and resolving one PER ROW is exactly the
/// allocating-arm mistake that cost 10,010 blocks over 10,000 ticks on the
/// tick path on 2026-09-05.
///
/// The trade is deliberate and cheap here: this counter is a TRIPWIRE that
/// should read zero forever (a bucket outside the session is not a thing the
/// aggregator can currently produce), so per-feed attribution buys nothing,
/// while a per-row allocation on the seal path would be a real regression.
#[derive(Debug)]
struct CandleOutOfWindowCounters {
    bucket_out: metrics::Counter,
    band: metrics::Counter,
}

impl CandleOutOfWindowCounters {
    fn new() -> Self {
        let make = |reason: &'static str| {
            let c = metrics::counter!(CANDLE_OUT_OF_WINDOW_COUNTER, "reason" => reason);
            // Seed at 0: the CloudWatch agent drops the first sample of a
            // series it has never seen, so an unseeded counter publishes
            // nothing on the one sample that matters.
            c.increment(0);
            c
        };
        Self {
            bucket_out: make(CANDLE_OUT_OF_WINDOW_REASONS[0]),
            band: make(CANDLE_OUT_OF_WINDOW_REASONS[1]),
        }
    }

    /// Allocation-free: pick a pre-resolved handle, increment, log on a power
    /// of two. The `warn!` sits beside the increment for the same readability
    /// reason recorded on the tick path.
    fn note(&self, reason: &'static str) {
        let counter = if reason == CANDLE_OUT_OF_WINDOW_REASONS[0] {
            &self.bucket_out
        } else {
            &self.band
        };
        counter.increment(1);

        static SEEN: AtomicU64 = AtomicU64::new(0);
        let total = SEEN.fetch_add(1, Ordering::Relaxed).saturating_add(1);
        if total.is_power_of_two() {
            warn!(
                reason,
                refused_total = total,
                "a candle row was REFUSED by the session-window gate and not \
                 written. This should read ZERO in normal operation: every \
                 bucket the aggregator opens is anchored at 09:00 IST and \
                 monotone within the day, so a refusal here means a bucket \
                 was built from a clock nobody expected."
            );
        }
    }
}

/// ILP writer for the 21 plain `candles_<tf>` tables.
///
/// Single producer (the async writer task is the sole owner) —
/// `&mut self` methods are sufficient.
pub struct ShadowCandleWriter {
    /// Production sender. `None` when QuestDB is unreachable; the
    /// writer task retries connection in a throttled loop. This
    /// slice only flips it from `Some → None` on flush failure.
    sender: Option<Sender>,
    /// In-memory ILP wire-format buffer. Filled by `append_seal`,
    /// drained by `flush`. Reused across batches to avoid alloc.
    buffer: Buffer,
    /// Number of seals appended since the last successful flush. Used
    /// by the writer task to drive batch-size-based flush triggers
    /// and by tests to verify append behaviour.
    pending_count: usize,
    /// Retained for the reconnect logic. Finding S2 (HIGH): the ILP
    /// conf string is wrapped in `SecretString` because it carries
    /// the QuestDB endpoint and (in conf-string-auth deployments)
    /// could carry credentials — never logged via `Debug`. Read it
    /// back with `.expose_secret()` only at `Sender` construction.
    /// Read by `reconnect()` (the broken-pipe recovery path) — no
    /// longer dead since the candle-writer reconnect landed 2026-06-30.
    /// Pre-resolved session-window refusal counters. See
    /// [`CANDLE_OUT_OF_WINDOW_REASONS`] for why this gate reads the BUCKET
    /// and never a seal clock.
    out_of_window: CandleOutOfWindowCounters,
    ilp_conf_string: SecretString,
}

impl ShadowCandleWriter {
    /// Production constructor. Connects to QuestDB via **ILP-over-HTTP**
    /// (per-flush server ACK — see [`build_ilp_conf_string`] for the
    /// 2026-07-06 silent-candle-persist rationale).
    ///
    /// `Sender::from_conf` with `http::` does not dial at construction, so
    /// this normally succeeds with `sender = Some`; an unreachable QuestDB
    /// surfaces at `flush()` as `Err` and routes through the reconnect +
    /// rescue chain. If construction itself fails the writer still
    /// constructs with `sender = None` and reconnects lazily on flush.
    pub fn new(config: &QuestDbConfig) -> Result<Self> {
        let conf_string = build_ilp_conf_string(&config.host, config.http_port);
        let (sender, buffer) = match Sender::from_conf(&conf_string) {
            Ok(s) => {
                let b = s.new_buffer();
                (Some(s), b)
            }
            Err(err) => {
                warn!(
                    ?err,
                    "shadow candle writer: QuestDB unreachable at startup — will buffer locally until reconnect"
                );
                (None, Buffer::new(ProtocolVersion::V1))
            }
        };
        Ok(Self {
            sender,
            buffer,
            pending_count: 0,
            ilp_conf_string: SecretString::from(conf_string),
            out_of_window: CandleOutOfWindowCounters::new(),
        })
    }

    /// Test constructor — disconnected writer with an empty buffer.
    /// Used by every unit test in this module so they do NOT require a
    /// live QuestDB. The conf string is left empty; production must
    /// always go via [`Self::new`].
    #[must_use]
    // TEST-EXEMPT: test-only construction helper used by every test in this module (lazy-connect, append, row-count, byte-content, multi-TF dispatch, I-P1-11 isolation, flush-disconnected). Separate name-matched test would be redundant.
    pub fn for_test() -> Self {
        Self {
            sender: None,
            buffer: Buffer::new(ProtocolVersion::V1),
            pending_count: 0,
            ilp_conf_string: SecretString::from(String::new()),
            out_of_window: CandleOutOfWindowCounters::new(),
        }
    }

    /// Returns `true` when the writer holds a live ILP `Sender`.
    /// `false` when constructed in disconnected mode (test or boot
    /// before QuestDB is up).
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.sender.is_some()
    }

    /// Number of seals appended since the last successful flush.
    /// Used by the future writer task to drive batch-size-based flush.
    #[must_use]
    pub const fn pending_count(&self) -> usize {
        self.pending_count
    }

    /// Number of bytes currently in the ILP wire buffer. `0` after a
    /// successful flush. Used by observability + tests to verify the
    /// append actually wrote bytes.
    #[must_use]
    pub fn buffer_byte_count(&self) -> usize {
        self.buffer.len()
    }

    /// Number of rows currently in the ILP wire buffer. Equals
    /// `pending_count` while no flushes have happened in-between.
    #[must_use]
    pub fn buffer_row_count(&self) -> usize {
        self.buffer.row_count()
    }

    /// Direct raw bytes currently in the ILP wire buffer. Used in
    /// unit tests to assert the table name + symbol + numeric column
    /// contents got serialised correctly.
    #[must_use]
    pub fn buffer_bytes(&self) -> &[u8] {
        self.buffer.as_bytes()
    }

    /// Append one sealed candle to the ILP buffer for its target
    /// plain `candles_<tf>` table.
    ///
    /// Dispatch:
    /// - `Buffer::table(row.table_name)` — one of the 21 plain candle
    ///   tables per `seal.tf.table_name()`.
    /// - `symbol("segment", row.segment)` — IDX_I / NSE_EQ / NSE_FNO /
    ///   etc. per `segment_code_to_str`.
    /// - `column_i64("security_id", row.security_id)` — composite-key
    ///   part 1 (I-P1-11).
    /// - `column_f64("open" / "high" / "low" / "close")` — OHLC.
    /// - `column_i64("volume" / "oi" / "tick_count")` — non-OHLC ints.
    /// - `at(TimestampNanos::new(row.timestamp_ist_nanos))` —
    ///   designated timestamp from `bucket_start_ist_secs * 1e9`
    ///   (CRITICAL data-integrity rule: NEVER add IST offset).
    ///
    /// The 3 `*_pct_from_prev_day` columns of the legacy shadow schema
    /// are intentionally NOT written — the plain candle tables have a
    /// 10-column schema with no pct columns.
    ///
    /// The `Buffer` is reused across calls. After N appends the
    /// caller (the writer task) calls [`Self::flush`] to send.
    pub fn append_seal(&mut self, seal: &BufferedSeal) -> Result<()> {
        // Feed-agnostic by construction: the ONLY feed-dependent input is
        // `seal.feed.as_str()` inside `ShadowSealRow::from_buffered_seal`. The
        // write path below stamps `row.feed` verbatim with NO per-feed branch, so
        // ANY feed (Dhan, Groww, or a feed #3 added later by one `Feed` enum edit)
        // flows through the SAME candle path automatically. `append_row` is the
        // feed-agnostic write boundary the generality test exercises directly.
        let row = ShadowSealRow::from_buffered_seal(seal);
        self.append_row(&row)
    }

    /// Serialise one already-built [`ShadowSealRow`] into the ILP buffer for its
    /// target plain `candles_<tf>` table. This is the FEED-AGNOSTIC write boundary:
    /// `row.feed` is a `&'static str` stamped verbatim — there is NO match on the
    /// feed anywhere in this method, so a novel feed string flows through
    /// unchanged. `append_seal` is the thin `BufferedSeal` adapter over this.
    pub fn append_row(&mut self, row: &ShadowSealRow) -> Result<()> {
        // ---- SESSION-WINDOW GATE (BUCKET clock only) ---------------------
        //
        // Returns Ok(()) on refusal: the caller escalates an Err to the seal
        // spill/DLQ tier and AGGREGATOR-DROP-01, so an Err here would report
        // a deliberate refusal as lost data and page on it.
        //
        // Reads `row.timestamp_ist_nanos`, which is the BUCKET-OPEN instant --
        // never the seal instant. See CANDLE_OUT_OF_WINDOW_REASONS for the
        // proof that gating any seal clock would discard the final bar of
        // every session.
        //
        // ## ⚠ HONEST STATUS, established 2026-09-05 by an adversarial sweep:
        // ## this gate is DEFENCE IN DEPTH, not enforcement. It cannot fire today.
        //
        // Two upstream facts make every bucket that reaches this writer
        // in-window by construction:
        //   * `tf_index::bucket_start` CLAMPS the bucket-open to
        //     `CANDLE_SESSION_OPEN_SECS_OF_DAY_IST` (32_400 = 09:00), so no
        //     bucket can open earlier -- including D1, which stamps 09:00
        //     rather than midnight; and
        //   * `MultiTfAggregator::consume` refuses the tick outright
        //     (`out_of_session`) unless its fold clock is inside
        //     `[CANDLE_SESSION_OPEN.., MARKET_CLOSE_SECS_OF_DAY_IST)` =
        //     `[09:00, 15:40)` -- byte-identical to the persist window.
        //
        // So a folded bar's `timestamp_ist_nanos` is always >= 09:00 and always
        // < 15:40, and this check cannot refuse one. It is kept anyway, at two
        // integer compares per seal, because the redundancy is the point: the
        // fold window is a `trading`-crate constant and this is a `storage`
        // writer, and the day someone widens one the other still holds the
        // operator's rule. What must NOT happen is this comment reading as
        // though candles were previously unguarded and are now guarded -- they
        // were guarded, one layer up, by a different crate.
        {
            let secs = row.timestamp_ist_nanos.div_euclid(1_000_000_000);
            let in_band = u32::try_from(secs).is_ok_and(|s| {
                (tickvault_trading::candles::multi_tf_aggregator::MIN_PLAUSIBLE_EXCHANGE_TS_SECS
                    ..=tickvault_trading::candles::multi_tf_aggregator::MAX_PLAUSIBLE_EXCHANGE_TS_SECS)
                    // O(1) EXEMPT: RangeInclusive::contains on a const range is two integer compares, not a scan
                    .contains(&s)
            });
            if !in_band {
                self.out_of_window.note(CANDLE_OUT_OF_WINDOW_REASONS[1]);
                return Ok(());
            }
            if !tickvault_common::session_window::row_is_in_an_open_window(row.timestamp_ist_nanos)
            {
                self.out_of_window.note(CANDLE_OUT_OF_WINDOW_REASONS[0]);
                return Ok(());
            }
        }
        let buf = self
            .buffer
            .table(row.table_name)
            .with_context(|| format!("candle append: invalid table name {}", row.table_name))?
            // Feed-provenance label (operator 2026-06-19, "same tables + feed
            // column"). Sourced from the SEAL's feed (`Feed::Dhan` / `Feed::Truedata`
            // via `row.feed = seal.feed.as_str()`) — the ONE feed-parameterized
            // writer stamps whichever feed produced the seal, never a hardcoded
            // constant. Part of the DEDUP key so a Dhan candle and a Groww candle
            // for the same minute/instrument never collide. Zero-alloc `&'static str`.
            .symbol("feed", row.feed)
            .with_context(|| "candle append: symbol(feed) failed")?
            .symbol("segment", row.segment)
            .with_context(|| "candle append: symbol(segment) failed")?
            .column_i64("security_id", row.security_id)
            .with_context(|| "candle append: column_i64(security_id) failed")?
            .column_f64("open", row.open)
            .with_context(|| "candle append: column_f64(open) failed")?
            .column_f64("high", row.high)
            .with_context(|| "candle append: column_f64(high) failed")?
            .column_f64("low", row.low)
            .with_context(|| "candle append: column_f64(low) failed")?
            .column_f64("close", row.close)
            .with_context(|| "candle append: column_f64(close) failed")?
            .column_i64("volume", row.volume)
            .with_context(|| "candle append: column_i64(volume) failed")?
            .column_i64("oi", row.oi)
            .with_context(|| "candle append: column_i64(oi) failed")?
            .column_i64("tick_count", row.tick_count)
            .with_context(|| "candle append: column_i64(tick_count) failed")?
            .column_f64("close_pct_from_prev_day", row.close_pct_from_prev_day)
            .with_context(|| "candle append: column_f64(close_pct_from_prev_day) failed")?
            // §31 Option 2: % change vs the official 09:15 session open.
            .column_f64("open_pct", row.open_pct)
            .with_context(|| "candle append: column_f64(open_pct) failed")?
            // Operator request 2026-06-02: headline day change % + opening gap %.
            .column_f64("change_pct", row.change_pct)
            .with_context(|| "candle append: column_f64(change_pct) failed")?
            .column_f64("open_gap_pct", row.open_gap_pct)
            .with_context(|| "candle append: column_f64(open_gap_pct) failed")?
            // The vendor's PENDING order-book totals at this bar's last
            // observed packet — resting orders, NOT executed volume. Written
            // unconditionally, `0` included: the fold has already applied the
            // last-non-zero rule, so a `0` here means the whole bar saw no
            // book, which is a fact worth storing rather than a gap.
            .column_i64("total_buy_qty", row.total_buy_qty)
            .with_context(|| "candle append: column_i64(total_buy_qty) failed")?
            .column_i64("total_sell_qty", row.total_sell_qty)
            .with_context(|| "candle append: column_i64(total_sell_qty) failed")?;
        // Net volume is the ONE column deliberately omitted rather than
        // zero-filled when absent. Omitting an ILP column persists NULL, and
        // NULL is the honest value for "THIS PROCESS DID NOT CLASSIFY THIS
        // BAR'S FLOW" — a disk-spill replay (the 128-byte record cannot carry
        // the accumulator), a REST-folded bar, or a bar with no ticks or no
        // volume. Writing `0` there would claim perfectly balanced buy and sell
        // flow about a bar nobody measured, and on a chart it draws a FLAT bar
        // where NULL draws nothing. Two distinct facts, two distinct storages.
        //
        // ⚠ CORRECTED 2026-09-10: this said NULL meant "there was no previous
        // bar to compare against — the day's first bar of this timeframe". That
        // was true of the retired close-vs-close definition and is false now: a
        // first bar WITH ticks is classified and reports real flow. The NULL is
        // a provenance signal, not a calendar one, and reading it as "start of
        // day" would mislabel every spill-replayed bar in the table.
        if let Some(net_volume) = row.net_volume {
            buf.column_i64("net_volume", net_volume)
                .with_context(|| "candle append: column_i64(net_volume) failed")?;
        }
        buf.at(TimestampNanos::new(row.timestamp_ist_nanos))
            .with_context(|| "candle append: at(TimestampNanos) failed")?;
        self.pending_count += 1;
        Ok(())
    }

    /// Flush the buffered candle rows to QuestDB with IN-CYCLE reconnect +
    /// idempotent replay on a transient connection error.
    ///
    /// **The bug this fixes (2026-06-30, operator's #1 blocker):** the candle
    /// writer previously had NO reconnect — `flush()` returned `Err` whenever the
    /// sender was `None` and never rebuilt it (the `ilp_conf_string` field was
    /// stored but `#[allow(dead_code)]`). At the 09:00 market-open burst a QuestDB
    /// ILP socket reset (`Broken pipe`) broke the candle sender; once a
    /// `questdb::Sender` is `must_close()` every later `flush()` returns
    /// `SocketError`, so `drain_once` rescued EVERY sealed candle to spill/DLQ and
    /// ZERO rows reached `candles_1m` for the rest of the session — 3.6M ticks but
    /// 0 candles in Groww-only mode. This now reconnects via the retained
    /// `ilp_conf_string` + replays the SAME buffer BEFORE the caller escalates to
    /// spill.
    ///
    /// Resilience contract (mirrors `GrowwLiveTickWriter::flush`):
    /// - `sender == None` (cold boot, or a prior connection-error arm dropped it):
    ///   reconnect via `ilp_conf_string` BEFORE flushing.
    /// - flush returns a CONNECTION error (broken pipe / reset / not-connected ⇒
    ///   `ErrorCode::SocketError`): drop the sender, reconnect, re-flush the SAME
    ///   buffer (RETAINED — questdb `flush` only `buf.clear()`s on `Ok`), bounded
    ///   to [`crate::ilp_flush_reconnect::ILP_FLUSH_RECONNECT_MAX_RETRIES`] with
    ///   [`crate::ilp_flush_reconnect::ILP_FLUSH_RECONNECT_BACKOFF_MS`] backoff.
    ///   Replay is idempotent — `candles_1m` DEDUP `(ts, security_id, segment,
    ///   feed)` collapses any partially-written row, never a double candle.
    /// - flush returns a NON-connection error (`InvalidName`, server reject, etc.):
    ///   NOT retried — return `Err` immediately (re-sending a bad row fails again).
    /// - retries exhausted: return `Err` with the buffer + `pending_count`
    ///   RETAINED at THIS level (so the caller can still inspect/rescue) —
    ///   `drain_once` then rescues the popped seals to the spill/DLQ net (the
    ///   durable floor) and calls [`Self::discard_pending`] so the NEXT cycle
    ///   starts with a CLEAN buffer. 2026-07-06 hostile-review fix: the old
    ///   cross-cycle retain-and-replay contract meant (a) a single
    ///   server-REJECTED row was replayed forever — every later flush failed on
    ///   the same poisoned bytes, killing candle persistence for the session —
    ///   and (b) during a sustained connection outage the buffer grew without
    ///   bound (each 100 ms cycle appended more already-spilled rows) until the
    ///   questdb-rs `max_buf_size` (100 MiB) turned every flush into a permanent
    ///   pre-transport error. Rescue + discard is the bounded, poison-proof
    ///   contract; DEDUP keys absorb any partially-committed rows.
    pub fn flush(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            self.pending_count = 0;
            return Ok(());
        }
        let mut reconnected = false;
        for attempt in 0..=crate::ilp_flush_reconnect::ILP_FLUSH_RECONNECT_MAX_RETRIES {
            if self.sender.is_none()
                && let Err(err) = self.reconnect()
            {
                if attempt == crate::ilp_flush_reconnect::ILP_FLUSH_RECONNECT_MAX_RETRIES {
                    return Err(err).context("shadow flush: reconnect to QuestDB failed");
                }
                metrics::counter!("tv_shadow_candle_reconnect_attempts_total").increment(1);
                reconnected = true;
                Self::backoff_sleep(attempt);
                continue;
            }

            // SAFETY: `sender` is `Some` here — either it was already connected, or
            // the reconnect above set it. The flush borrows `&mut self.buffer`
            // alongside `self.sender`, so split the borrows via take/restore.
            let mut sender = match self.sender.take() {
                Some(s) => s,
                // Unreachable (reconnect set it), but never panic.
                None => continue,
            };
            match sender.flush(&mut self.buffer) {
                Ok(()) => {
                    self.sender = Some(sender);
                    if reconnected {
                        metrics::counter!("tv_shadow_candle_reconnect_recoveries_total")
                            .increment(1);
                        // 2026-07-06 exam-fix: `info!`, not `debug!` — the tick
                        // writer's equivalent recovery line was visible in the
                        // live-exam logs while this one was invisible, which is
                        // exactly how the candle leg went silent. A reconnect
                        // recovery is an operator-relevant self-heal signal.
                        info!(
                            flushed_rows = self.pending_count,
                            "shadow candle writer flush recovered via reconnect + replay (zero drop)"
                        );
                    } else {
                        tracing::debug!(
                            flushed_rows = self.pending_count,
                            "shadow candle writer flushed"
                        );
                    }
                    self.pending_count = 0;
                    return Ok(());
                }
                Err(err) => {
                    // Drop the broken sender so the next iteration reconnects. The
                    // buffer is RETAINED (questdb `flush` does not clear it on err).
                    self.sender = None;
                    if !crate::ilp_flush_reconnect::is_connection_error(&err) {
                        return Err(err).context("shadow flush: ILP send failed (non-connection)");
                    }
                    if attempt == crate::ilp_flush_reconnect::ILP_FLUSH_RECONNECT_MAX_RETRIES {
                        return Err(err)
                            .context("shadow flush: ILP send failed after reconnect retries");
                    }
                    metrics::counter!("tv_shadow_candle_reconnect_attempts_total").increment(1);
                    reconnected = true;
                    Self::backoff_sleep(attempt);
                }
            }
        }
        Err(anyhow::anyhow!("shadow flush: exhausted reconnect retries"))
    }

    /// (Re)establish the ILP-over-HTTP sender from the retained conf. The EXISTING
    /// buffer is KEPT so its already-appended candles are flushed by the new sender
    /// (the conf pins `protocol_version=1`, so the disconnected `Buffer::new(V1)`,
    /// `new()`'s buffer, and every rebuilt sender all speak the same V1 line
    /// protocol). Cold path: only runs when `sender` is `None`.
    fn reconnect(&mut self) -> Result<()> {
        let conf = self.ilp_conf_string.expose_secret();
        if conf.is_empty() {
            // `for_test()` writers carry an empty conf — there is nothing to
            // reconnect to. Surface a connection error so the retry loop treats it
            // as a (futile) connection failure rather than panicking.
            return Err(anyhow::anyhow!(
                "shadow reconnect: no ILP conf (disconnected test writer)"
            ));
        }
        let new_sender =
            Sender::from_conf(conf).context("shadow reconnect: ILP sender rebuild failed")?;
        self.sender = Some(new_sender);
        Ok(())
    }

    /// Discard every buffered-but-unflushed candle row (poison-buffer recovery,
    /// 2026-07-06 hostile-review fix). Called by `seal_writer_task::drain_once`
    /// AFTER the popped seals of a failed flush have been rescued to the
    /// spill/DLQ durable floor (or counted as Dropped with AGGREGATOR-DROP-01).
    ///
    /// Why discarding is the SAFE move at that point:
    /// - the same rows are durably parked in spill/DLQ as recoverable records
    ///   (replayable via `SealSpillWriter::read_all` / the DLQ NDJSON), so the
    ///   ILP buffer copy is redundant;
    /// - retaining it replays a server-REJECTED row on every later flush — one
    ///   poisoned row would kill candle persistence for the whole session;
    /// - retaining it during a sustained connection outage grows the buffer
    ///   without bound (each cycle appends more already-rescued rows) until the
    ///   questdb-rs 100 MiB `max_buf_size` wedges every flush permanently.
    ///
    /// Cold path; O(1) — `Buffer::clear` keeps the allocation for reuse.
    pub fn discard_pending(&mut self) {
        self.buffer.clear();
        self.pending_count = 0;
    }

    /// Sleep the bounded backoff for a 0-based attempt index — shares the Groww
    /// tick-writer schedule (`GROWW_FLUSH_RECONNECT_BACKOFF_MS`, ≤350ms total).
    /// Synchronous: the seal-writer drain runs on its own task and the total
    /// across retries is tiny.
    fn backoff_sleep(attempt: usize) {
        if let Some(&ms) = crate::ilp_flush_reconnect::ILP_FLUSH_RECONNECT_BACKOFF_MS.get(attempt) {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::constants::{EXCHANGE_SEGMENT_IDX_I, EXCHANGE_SEGMENT_NSE_EQ};
    use tickvault_common::feed::Feed;
    use tickvault_trading::candles::{LiveCandleState, TfIndex};

    fn mk_seal_feed(
        sid: u64,
        seg: u8,
        tf: TfIndex,
        bucket: u32,
        close: f64,
        feed: Feed,
    ) -> BufferedSeal {
        let mut state = LiveCandleState::empty();
        state.bucket_start_ist_secs = bucket;
        state.open = 100.0;
        state.high = 105.0;
        state.low = 99.0;
        state.close = close;
        state.volume = 1234;
        state.bucket_start_cumulative = 1000;
        state.oi = 50_000;
        state.tick_count = 5;
        state.close_pct_from_prev_day = 1.5;
        state.bucket_open_prev_close = 24_200.10;
        state.total_buy_qty = 89_600;
        state.total_sell_qty = 4_800;
        BufferedSeal::new(sid, seg, tf, state, feed)
    }

    fn mk_seal(sid: u64, seg: u8, tf: TfIndex, bucket: u32, close: f64) -> BufferedSeal {
        mk_seal_feed(sid, seg, tf, bucket, close, Feed::Dhan)
    }

    #[test]
    fn test_for_test_constructor_creates_disconnected_writer() {
        let w = ShadowCandleWriter::for_test();
        assert!(!w.is_connected());
        assert_eq!(w.pending_count(), 0);
        assert_eq!(w.buffer_row_count(), 0);
        assert_eq!(w.buffer_byte_count(), 0);
    }

    #[test]
    fn test_is_connected_returns_false_in_test_mode() {
        let w = ShadowCandleWriter::for_test();
        assert!(!w.is_connected());
    }

    #[test]
    fn test_buffer_bytes_returns_empty_slice_initially() {
        let w = ShadowCandleWriter::for_test();
        assert!(w.buffer_bytes().is_empty());
    }

    #[test]
    fn test_buffer_byte_count_starts_at_zero() {
        let w = ShadowCandleWriter::for_test();
        assert_eq!(w.buffer_byte_count(), 0);
    }

    #[test]
    fn test_new_constructs_even_when_questdb_unreachable() {
        // With ILP-over-HTTP the sender does NOT dial at construction, so
        // `new()` succeeds even against a closed port. The failure surfaces
        // at flush() as Err (per-request server ACK) instead of a silent
        // fire-and-forget TCP write — the 2026-07-06 exam-fix contract.
        let cfg = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1, // closed port — flush will fail loudly
            pg_port: 8812,
            ilp_port: 9009,
        };
        let w = ShadowCandleWriter::new(&cfg)
            .expect("constructor must not return Err even when QuestDB is unreachable");
        assert_eq!(w.pending_count(), 0);
    }

    #[test]
    fn test_ilp_conf_targets_http_port_not_tcp() {
        // 2026-07-06 exam-fix transport ratchet (mirrors
        // `ws_event_audit_persistence::test_ilp_http_conf_targets_http_port`):
        // the candle writer MUST use ILP-over-HTTP so a server-side reject
        // returns Err instead of the fire-and-forget TCP silence that left
        // the candle tables empty on the 2026-07-06 groww-only exam session.
        let conf = build_ilp_conf_string("tv-questdb", 9000);
        assert_eq!(
            conf,
            "http::addr=tv-questdb:9000;protocol_version=1;retry_timeout=0;request_timeout=5000;"
        );
        assert!(
            conf.starts_with("http::addr="),
            "candle writer transport must be ILP-over-HTTP (per-flush ACK), got {conf}"
        );
        assert!(
            !conf.contains("tcp::"),
            "ILP TCP is fire-and-forget — a server reject never Errs; banned here"
        );
        // 2026-07-06 hostile-review ratchet: the questdb-rs defaults
        // (retry_timeout=10s + request_timeout=10s) turned one failed drain
        // cycle into up to ~80-120s of synchronous blocking on the shared
        // tokio runtime. The internal library retry MUST stay disabled (the
        // outer flush() loop owns retry) and the per-request timeout bounded.
        assert!(
            conf.contains("retry_timeout=0;"),
            "questdb-rs internal retry must be DISABLED (outer loop owns retry), got {conf}"
        );
        assert_eq!(SEAL_FLUSH_ILP_RETRY_TIMEOUT_MS, 0);
        assert!(
            conf.contains("request_timeout=5000;"),
            "per-request HTTP timeout must be pinned (5s), got {conf}"
        );
        assert_eq!(SEAL_FLUSH_REQUEST_TIMEOUT_MS, 5_000);
    }

    #[test]
    fn test_discard_pending_clears_buffer_and_pending_for_clean_next_cycle() {
        // Poison-buffer recovery contract (2026-07-06 hostile-review fix):
        // after drain_once has rescued a failed batch to spill/DLQ it discards
        // the writer's retained ILP buffer, so a server-rejected row can never
        // replay forever and the buffer can never grow across cycles toward
        // the questdb-rs 100 MiB max_buf_size cliff.
        let mut w = ShadowCandleWriter::for_test();
        w.append_seal(&mk_seal(13, 0, TfIndex::M1, 1_716_023_700, 100.0))
            .expect("append");
        w.append_seal(&mk_seal(25, 0, TfIndex::M1, 1_716_024_300, 200.0))
            .expect("append");
        assert!(w.flush().is_err(), "disconnected flush fails");
        assert_eq!(
            w.pending_count(),
            2,
            "flush itself retains (in-cycle replay)"
        );
        w.discard_pending();
        assert_eq!(w.pending_count(), 0, "discard must reset pending");
        assert_eq!(w.buffer_byte_count(), 0, "discard must empty the buffer");
        assert_eq!(w.buffer_row_count(), 0);
        // The next cycle starts clean: a fresh append contains ONLY the new
        // row's bytes — no poisoned replay of the failed batch.
        w.append_seal(&mk_seal(51, 0, TfIndex::M1, 1_716_024_900, 300.0))
            .expect("append after discard");
        assert_eq!(w.pending_count(), 1);
        let s = std::str::from_utf8(w.buffer_bytes()).expect("utf8");
        assert!(
            s.contains("security_id=51i") && !s.contains("security_id=13i"),
            "post-discard buffer must contain only the new cycle's rows, got {s}"
        );
    }

    #[test]
    fn test_append_seal_increments_pending_count() {
        let mut w = ShadowCandleWriter::for_test();
        w.append_seal(&mk_seal(13, 0, TfIndex::M1, 1_716_023_700, 100.0))
            .expect("append");
        assert_eq!(w.pending_count(), 1);
        w.append_seal(&mk_seal(25, 0, TfIndex::M1, 1_716_024_300, 200.0))
            .expect("append");
        assert_eq!(w.pending_count(), 2);
    }

    #[test]
    fn test_append_seal_increments_buffer_row_count() {
        let mut w = ShadowCandleWriter::for_test();
        for i in 0..5 {
            let s = mk_seal(
                u64::from(13 + i),
                0,
                TfIndex::M1,
                1_716_023_700 + i,
                100.0 + i as f64,
            );
            w.append_seal(&s).expect("append");
        }
        assert_eq!(w.buffer_row_count(), 5);
        assert_eq!(w.pending_count(), 5);
    }

    #[test]
    fn test_append_seal_writes_bytes_to_the_buffer() {
        let mut w = ShadowCandleWriter::for_test();
        assert_eq!(w.buffer_byte_count(), 0);
        w.append_seal(&mk_seal(13, 0, TfIndex::M1, 1_716_023_700, 100.0))
            .expect("append");
        assert!(
            w.buffer_byte_count() > 0,
            "ILP buffer must contain bytes after append"
        );
    }

    #[test]
    fn test_append_seal_buffer_contains_target_table_name_for_m1() {
        let mut w = ShadowCandleWriter::for_test();
        w.append_seal(&mk_seal(13, 0, TfIndex::M1, 1_716_023_700, 100.0))
            .expect("append");
        let bytes = w.buffer_bytes();
        let s = std::str::from_utf8(bytes).expect("ILP wire format is utf-8");
        assert!(
            s.contains("candles_1m"),
            "expected candles_1m in buffer, got {s}"
        );
        assert!(
            !s.contains("shadow"),
            "plain candle table must NOT contain _shadow, got {s}"
        );
    }

    #[test]
    fn test_append_seal_dispatches_to_correct_table_for_each_tf() {
        // For each TfIndex, append one seal and verify the buffer
        // bytes contain the right plain candle table name. This pins
        // the dispatch contract end-to-end.
        for tf in TfIndex::ALL {
            let mut w = ShadowCandleWriter::for_test();
            w.append_seal(&mk_seal(13, 0, tf, 1_716_023_700, 100.0))
                .expect("append");
            let bytes = w.buffer_bytes();
            let s = std::str::from_utf8(bytes).expect("ILP wire format is utf-8");
            let expected = tf.table_name();
            assert!(
                s.contains(expected),
                "TF {tf:?}: expected `{expected}` in ILP bytes, got: {s}"
            );
        }
    }

    #[test]
    fn test_append_seal_buffer_contains_segment_string() {
        let mut w = ShadowCandleWriter::for_test();
        // IDX_I segment
        w.append_seal(&mk_seal(
            13,
            EXCHANGE_SEGMENT_IDX_I,
            TfIndex::M1,
            1_716_023_700,
            100.0,
        ))
        .expect("append");
        let s = std::str::from_utf8(w.buffer_bytes()).expect("utf8");
        assert!(s.contains("IDX_I"), "expected IDX_I in bytes, got {s}");
    }

    #[test]
    fn test_append_seal_stamps_feed_dhan_in_wire_bytes() {
        // Operator 2026-06-19 "same tables + feed column": every Dhan candle
        // row MUST carry the constant `feed=dhan` SYMBOL so it never collides
        // with a Groww candle (`feed=groww`) under the 4-column DEDUP key.
        let mut w = ShadowCandleWriter::for_test();
        w.append_seal(&mk_seal(
            13,
            EXCHANGE_SEGMENT_IDX_I,
            TfIndex::M1,
            1_716_023_700,
            100.0,
        ))
        .expect("append");
        let s = std::str::from_utf8(w.buffer_bytes()).expect("utf8");
        assert!(
            s.contains("feed=dhan"),
            "expected feed=dhan SYMBOL in ILP bytes, got {s}"
        );
        assert_eq!(CANDLE_FEED_DHAN, "dhan", "feed label constant must be dhan");
    }

    #[test]
    fn test_append_seal_stamps_feed_from_seal_not_hardcoded() {
        // ONE feed-parameterized writer: a non-Dhan seal MUST stamp ITS OWN
        // feed label, never the old hardcoded `feed=dhan`, so the same
        // append_seal path serves every feed and their candles can never
        // collide under the (ts, security_id, segment, feed) DEDUP key.
        // Probed with Groww until 2026-08-21; TrueData is the non-Dhan feed now.
        let mut w = ShadowCandleWriter::for_test();
        w.append_seal(&mk_seal_feed(
            13,
            EXCHANGE_SEGMENT_IDX_I,
            TfIndex::M1,
            1_716_023_700,
            100.0,
            Feed::Truedata,
        ))
        .expect("append");
        let s = std::str::from_utf8(w.buffer_bytes()).expect("utf8");
        assert!(
            s.contains("feed=truedata"),
            "a TrueData seal must stamp feed=truedata (the writer is feed-parameterized), got {s}"
        );
        assert!(
            !s.contains("feed=dhan"),
            "a non-Dhan seal must NOT stamp feed=dhan, got {s}"
        );
    }

    #[test]
    fn test_append_seal_isolates_segments_in_wire_bytes_for_i_p1_11() {
        // I-P1-11: same security_id, different segments → both segment
        // strings present in the ILP bytes after two appends. Verifies
        // the dispatch preserves composite-key-distinct identities all
        // the way through to the wire.
        let mut w = ShadowCandleWriter::for_test();
        w.append_seal(&mk_seal(
            13,
            EXCHANGE_SEGMENT_IDX_I,
            TfIndex::M1,
            1_716_023_700,
            100.0,
        ))
        .expect("append idx_i");
        w.append_seal(&mk_seal(
            13,
            EXCHANGE_SEGMENT_NSE_EQ,
            TfIndex::M1,
            1_716_024_300,
            200.0,
        ))
        .expect("append nse_eq");
        let s = std::str::from_utf8(w.buffer_bytes()).expect("utf8");
        assert!(s.contains("IDX_I"), "IDX_I missing");
        assert!(s.contains("NSE_EQ"), "NSE_EQ missing");
        assert_eq!(w.buffer_row_count(), 2);
    }

    #[test]
    fn test_append_seal_buffer_contains_security_id() {
        let mut w = ShadowCandleWriter::for_test();
        w.append_seal(&mk_seal(13, 0, TfIndex::M1, 1_716_023_700, 100.0))
            .expect("append");
        let s = std::str::from_utf8(w.buffer_bytes()).expect("utf8");
        // ILP int columns format: `security_id=13i`
        assert!(
            s.contains("security_id=13i"),
            "expected security_id=13i in {s}"
        );
    }

    #[test]
    fn test_append_seal_buffer_contains_ohlc_values() {
        let mut w = ShadowCandleWriter::for_test();
        w.append_seal(&mk_seal(13, 0, TfIndex::M1, 1_716_023_700, 100.0))
            .expect("append");
        let s = std::str::from_utf8(w.buffer_bytes()).expect("utf8");
        // OHLC are f64 fields written as ILP doubles; questdb-rs emits
        // a binary protocol marker so we can't simply grep "100.0".
        // Instead verify the column names are present.
        assert!(s.contains("open="), "open column missing in {s}");
        assert!(s.contains("high="), "high column missing in {s}");
        assert!(s.contains("low="), "low column missing in {s}");
        assert!(s.contains("close="), "close column missing in {s}");
    }

    #[test]
    fn test_append_seal_buffer_writes_close_pct_only() {
        // PR-4b (2026-05-28): the 11-column schema writes
        // `close_pct_from_prev_day` but NOT the oi/volume pct columns —
        // spot has no OI and indices no volume, so those stay dropped.
        let mut w = ShadowCandleWriter::for_test();
        w.append_seal(&mk_seal(13, 0, TfIndex::M1, 1_716_023_700, 100.0))
            .expect("append");
        let s = std::str::from_utf8(w.buffer_bytes()).expect("utf8");
        assert!(
            s.contains("close_pct_from_prev_day="),
            "close_pct MUST be written in {s}"
        );
        assert!(
            !s.contains("oi_pct_from_prev_day"),
            "oi_pct must NOT be written in {s}"
        );
        assert!(
            !s.contains("volume_pct_from_prev_day"),
            "volume_pct must NOT be written in {s}"
        );
    }

    #[test]
    fn test_append_seal_buffer_contains_volume_oi_tickcount_columns() {
        let mut w = ShadowCandleWriter::for_test();
        w.append_seal(&mk_seal(13, 0, TfIndex::M1, 1_716_023_700, 100.0))
            .expect("append");
        let s = std::str::from_utf8(w.buffer_bytes()).expect("utf8");
        assert!(s.contains("volume="), "volume column missing in {s}");
        assert!(s.contains("oi="), "oi column missing in {s}");
        assert!(
            s.contains("tick_count="),
            "tick_count column missing in {s}"
        );
    }

    #[test]
    fn test_flush_empty_buffer_is_noop_ok_even_when_disconnected() {
        // Behaviour change (2026-06-30 candle-reconnect): an EMPTY-buffer flush is
        // a no-op `Ok` — there is nothing to persist, so no reconnect is attempted
        // and no error is raised (mirrors the tick writer + avoids a pointless TCP
        // round-trip). `drain_once` only calls flush after popping ≥1 seal, so this
        // empty-Ok never masks a real persist failure. A flush WITH pending rows on
        // a disconnected writer still Errs — see
        // `test_flush_returns_err_when_disconnected_with_pending_rows`.
        let mut w = ShadowCandleWriter::for_test();
        let result = w.flush();
        assert!(
            result.is_ok(),
            "empty-buffer flush must be a no-op Ok, even disconnected"
        );
    }

    #[test]
    fn test_flush_returns_err_when_disconnected_with_pending_rows() {
        let mut w = ShadowCandleWriter::for_test();
        w.append_seal(&mk_seal(13, 0, TfIndex::M1, 1_716_023_700, 100.0))
            .expect("append");
        assert_eq!(w.pending_count(), 1);
        let result = w.flush();
        assert!(
            result.is_err(),
            "flush with pending rows MUST return Err when disconnected"
        );
        // pending_count MUST remain unchanged so the future writer
        // task can re-route the buffered seals to the spill tier.
        assert_eq!(w.pending_count(), 1);
    }

    #[test]
    fn test_repeated_appends_accumulate_buffer_bytes() {
        let mut w = ShadowCandleWriter::for_test();
        let n = 10;
        let mut prev_bytes = 0;
        for i in 0..n {
            let s = mk_seal(
                u64::from(13 + i),
                0,
                TfIndex::M1,
                1_716_023_700 + i,
                100.0 + i as f64,
            );
            w.append_seal(&s).expect("append");
            let now = w.buffer_byte_count();
            assert!(now > prev_bytes, "buffer must grow on each append");
            prev_bytes = now;
        }
        assert_eq!(w.buffer_row_count(), n as usize);
        assert_eq!(w.pending_count(), n as usize);
    }

    // ---- candle ILP reconnect (Part B — zero-candles-at-open fix 2026-06-30) ----

    #[test]
    fn test_shadow_writer_retains_buffer_and_pending_after_failed_flush() {
        // The candle-writer reconnect's core invariant: a flush that exhausts all
        // reconnect attempts (for_test writer has an empty conf → reconnect fails)
        // must RETAIN the buffered candles + pending so drain_once can rescue them
        // to spill/DLQ AND a later cycle re-attempts — no silent candle loss.
        let mut w = ShadowCandleWriter::for_test();
        w.append_seal(&mk_seal(
            13,
            EXCHANGE_SEGMENT_IDX_I,
            TfIndex::M1,
            1_716_023_700,
            100.0,
        ))
        .expect("append");
        let bytes_before = w.buffer_byte_count();
        assert_eq!(w.pending_count(), 1);
        assert!(
            w.flush().is_err(),
            "exhausted reconnect retries must Err (empty conf)"
        );
        assert_eq!(
            w.pending_count(),
            1,
            "pending must be retained after exhausted retries (candles re-attempted)"
        );
        assert_eq!(
            w.buffer_byte_count(),
            bytes_before,
            "buffer bytes must be retained after exhausted retries"
        );
    }

    #[test]
    fn test_shadow_writer_reattempts_flush_after_prior_failure() {
        // Recovery contract (2026-07-06 exam-fix): a failed flush must NOT
        // poison the writer — the SAME buffered seals are replayed on every
        // subsequent flush attempt (reconnect + replay each cycle), so the
        // moment QuestDB comes back the backlog commits. Proven here by two
        // consecutive failed flushes both retaining the identical buffer.
        let mut w = ShadowCandleWriter::for_test();
        w.append_seal(&mk_seal(13, 0, TfIndex::M1, 1_716_023_700, 100.0))
            .expect("append");
        let bytes = w.buffer_byte_count();
        assert!(w.flush().is_err(), "first flush fails (no conf)");
        assert!(
            w.flush().is_err(),
            "second flush must RE-ATTEMPT (reconnect+replay), not silently no-op"
        );
        assert_eq!(w.pending_count(), 1, "seal still pending for replay");
        assert_eq!(w.buffer_byte_count(), bytes, "identical buffer replayed");
    }

    #[test]
    fn test_shadow_writer_empty_buffer_flush_is_noop_ok() {
        // An empty buffer flush is a no-op Ok — no reconnect attempt, no error.
        let mut w = ShadowCandleWriter::for_test();
        assert!(w.flush().is_ok(), "empty flush must be Ok (no-op)");
    }

    #[test]
    fn test_shadow_writer_reconnect_uses_shared_connection_classifier() {
        // The candle writer reuses the SAME `is_connection_error` classifier as the
        // Groww tick writer, so broken-pipe / reset → retry, structural → no retry.
        // (Proving the classifier wiring without a live QuestDB.)
        use crate::ilp_flush_reconnect::is_connection_error;
        let socket = questdb::Error::new(
            questdb::ErrorCode::SocketError,
            "Could not flush buffer: Broken pipe (os error 32)",
        );
        let structural = questdb::Error::new(questdb::ErrorCode::InvalidName, "bad name");
        assert!(
            is_connection_error(&socket),
            "broken pipe must be retry-worthy"
        );
        assert!(
            !is_connection_error(&structural),
            "structural error must NOT be retried"
        );
    }

    // ---- FEED-AGNOSTIC generality (operator forever-requirement 2026-06-30) ----

    #[test]
    fn test_candle_writer_is_feed_agnostic_arbitrary_novel_feed() {
        // OPERATOR FOREVER-REQUIREMENT: the candle write path must be GENERIC —
        // ANY future feed (#3, #4, …) produces candles tagged with its own `feed`
        // through the EXACT SAME path, with ZERO feed-specific code. Proven here by
        // stamping an ARBITRARY, NOVEL feed string that is NOT Dhan or Groww
        // straight through the feed-agnostic `append_row` write boundary and
        // asserting it appears verbatim on the `candles_1m` wire. Because the write
        // path never matches on the feed (it stamps `row.feed` verbatim), a feed
        // added later by ONE `Feed` enum edit flows through automatically.
        let mut w = ShadowCandleWriter::for_test();
        let novel_row = ShadowSealRow {
            table_name: TfIndex::M1.table_name(),
            timestamp_ist_nanos: 1_716_023_700_i64 * 1_000_000_000,
            security_id: 4242,
            segment: "NSE_EQ",
            feed: "future_test_feed",
            open: 100.0,
            high: 105.0,
            low: 99.0,
            close: 101.0,
            volume: 1234,
            oi: 50_000,
            tick_count: 5,
            close_pct_from_prev_day: 1.5,
            open_pct: 0.4,
            change_pct: 1.5,
            open_gap_pct: 0.2,
            net_volume: Some(1234),
            total_buy_qty: 89_600,
            total_sell_qty: 4_800,
        };
        w.append_row(&novel_row).expect("append novel-feed row");
        let s = std::str::from_utf8(w.buffer_bytes()).expect("utf8");
        assert!(
            s.starts_with("candles_1m,"),
            "novel feed must seal to the SHARED candles_1m table, got {s}"
        );
        assert!(
            s.contains("feed=future_test_feed"),
            "the candle path must stamp the ARBITRARY feed verbatim (feed-agnostic), got {s}"
        );
        assert!(
            !s.contains("feed=dhan") && !s.contains("feed=truedata"),
            "a novel-feed candle must NOT be relabelled to a known feed, got {s}"
        );
    }

    #[test]
    fn test_candle_writer_seals_every_feed_in_canonical_list() {
        // Generality across the canonical feed registry: EVERY `Feed::ALL` variant
        // seals to the SAME candles_1m table tagged with its own `feed.as_str()` —
        // no per-feed branch. Adding a feed #3 to the `Feed` enum extends `ALL`, so
        // this loop automatically covers it (the registry IS the single edit point).
        for feed in Feed::ALL {
            let mut w = ShadowCandleWriter::for_test();
            w.append_seal(&mk_seal_feed(
                13,
                EXCHANGE_SEGMENT_NSE_EQ,
                TfIndex::M1,
                1_716_023_700,
                100.0,
                *feed,
            ))
            .expect("append");
            let s = std::str::from_utf8(w.buffer_bytes()).expect("utf8");
            assert!(
                s.starts_with("candles_1m,"),
                "feed {} must seal to the shared candles_1m table, got {s}",
                feed.as_str()
            );
            let expected = format!("feed={}", feed.as_str());
            assert!(
                s.contains(&expected),
                "feed {} must be stamped verbatim ({expected}), got {s}",
                feed.as_str()
            );
        }
    }

    #[test]
    fn test_candle_writer_covers_all_21_tf_tables_for_arbitrary_feed() {
        // OPERATOR SCOPE CLARIFICATION 2026-06-30: the candle path must cover
        // EVERY timeframe table, not just candles_1m — and for ANY feed. Drive one
        // seal for EVERY TfIndex::ALL (all 21 TFs) tagged an ARBITRARY novel feed
        // and assert each lands in its OWN candles_<tf> table tagged with that feed.
        // Proves: one common writer → all 21 TF tables, feed stamped verbatim, no
        // per-TF and no per-feed branch.
        let novel_feed = "future_test_feed";
        let mut seen_tables = std::collections::HashSet::new();
        for tf in TfIndex::ALL {
            let mut w = ShadowCandleWriter::for_test();
            let row = ShadowSealRow {
                table_name: tf.table_name(),
                timestamp_ist_nanos: 1_716_023_700_i64 * 1_000_000_000,
                security_id: 4242,
                segment: "NSE_EQ",
                feed: novel_feed,
                open: 100.0,
                high: 105.0,
                low: 99.0,
                close: 101.0,
                volume: 1234,
                oi: 50_000,
                tick_count: 5,
                close_pct_from_prev_day: 1.5,
                open_pct: 0.4,
                change_pct: 1.5,
                open_gap_pct: 0.2,
                net_volume: Some(1234),
                total_buy_qty: 89_600,
                total_sell_qty: 4_800,
            };
            w.append_row(&row).expect("append per-TF row");
            let s = std::str::from_utf8(w.buffer_bytes()).expect("utf8");
            let table = tf.table_name();
            assert!(
                s.starts_with(&format!("{table},")),
                "TF {table} must serialise to its OWN candle table, got {s}"
            );
            assert!(
                s.contains("feed=future_test_feed"),
                "TF {table} must stamp the arbitrary feed verbatim, got {s}"
            );
            seen_tables.insert(table);
        }
        // All 21 distinct candle tables were exercised.
        assert_eq!(
            seen_tables.len(),
            TfIndex::ALL.len(),
            "every one of the 21 TF candle tables must be covered"
        );
        assert!(seen_tables.contains("candles_1m"));
        assert!(seen_tables.contains("candles_1d"));
    }

    #[test]
    fn test_append_seal_and_append_row_produce_identical_wire_bytes() {
        // `append_seal` is a thin adapter over the feed-agnostic `append_row`; both
        // must serialise identically for the same logical candle (proves the
        // refactor introduced no behavioural drift on the Dhan/Groww path).
        let seal = mk_seal_feed(
            13,
            EXCHANGE_SEGMENT_NSE_EQ,
            TfIndex::M1,
            1_716_023_700,
            100.0,
            Feed::Truedata,
        );
        let row = ShadowSealRow::from_buffered_seal(&seal);

        let mut a = ShadowCandleWriter::for_test();
        a.append_seal(&seal).expect("append_seal");
        let mut b = ShadowCandleWriter::for_test();
        b.append_row(&row).expect("append_row");

        assert_eq!(
            a.buffer_bytes(),
            b.buffer_bytes(),
            "append_seal and append_row must produce identical ILP bytes"
        );
    }

    /// THE TRAP, pinned: the final bar of the session must be WRITTEN.
    ///
    /// A one-minute bucket opening at 15:39:00 has exclusive end 15:40:00 and
    /// is only released by `catch_up_seal` once the watermark passes
    /// 15:40:02 -- two seconds PAST the window's exclusive end. Any gate that
    /// consulted the seal instant would discard this bar every single
    /// session, silently, while reporting success. This test is what stops a
    /// future edit from "tightening" the gate into that.
    #[test]
    fn the_final_bucket_of_the_session_is_written_even_though_it_seals_after_1540() {
        let midnight = 1_715_990_400_u32;
        let mut w = ShadowCandleWriter::for_test();
        // 15:39:00 IST -- the last one-minute bucket the session can open.
        w.append_seal(&mk_seal(13, 0, TfIndex::M1, midnight + 56_340, 100.0))
            .expect("append");
        assert_eq!(
            w.pending_count(),
            1,
            "the 15:39 bar seals at ~15:40:02, AFTER the window closes. It is \
             gated on its BUCKET, which is 15:39:00 and in window, so it must \
             be written. Gating a seal clock here loses the close every day."
        );
    }

    /// The first bucket of the session, at the inclusive edge.
    #[test]
    fn the_first_bucket_at_0900_is_written() {
        let midnight = 1_715_990_400_u32;
        let mut w = ShadowCandleWriter::for_test();
        w.append_seal(&mk_seal(13, 0, TfIndex::M1, midnight + 32_400, 100.0))
            .expect("append");
        assert_eq!(w.pending_count(), 1, "09:00:00 is IN, inclusively");
    }

    /// A bucket genuinely outside the session is refused -- as Ok, never Err.
    ///
    /// An `Err` would route through the seal spill/DLQ escalation and
    /// AGGREGATOR-DROP-01, reporting a deliberate refusal as lost data.
    #[test]
    fn an_out_of_session_bucket_is_refused_as_ok_never_as_a_loss() {
        let midnight = 1_715_990_400_u32;
        let mut w = ShadowCandleWriter::for_test();
        // 16:00:00 IST -- past the close.
        assert!(
            w.append_seal(&mk_seal(13, 0, TfIndex::M1, midnight + 57_600, 100.0))
                .is_ok(),
            "a refusal is Ok(()): an Err escalates to the DLQ and pages"
        );
        assert_eq!(w.pending_count(), 0, "and nothing reaches the buffer");
    }

    /// A corrupt far-future bucket whose TIME OF DAY looks in-session.
    #[test]
    fn a_far_future_bucket_that_looks_in_window_is_refused() {
        let mut w = ShadowCandleWriter::for_test();
        // 2_600_000_000 s is year 2052; its seconds-of-day is 51_200 = 14:13:20.
        assert_eq!(2_600_000_000_u32 % 86_400, 51_200, "fixture precondition");
        assert!(
            w.append_seal(&mk_seal(13, 0, TfIndex::M1, 2_600_000_000, 100.0))
                .is_ok()
        );
        assert_eq!(w.pending_count(), 0, "the band check must refuse it");
    }

    /// Candle reasons say `bucket_`, because that is the clock being read.
    #[test]
    fn every_candle_reason_names_the_bucket_clock_and_is_distinct() {
        for reason in CANDLE_OUT_OF_WINDOW_REASONS {
            assert!(
                reason.starts_with("bucket_"),
                "{reason} must say bucket: a candle's ts is its bucket-open \
                 instant, not an event time and not an arrival time"
            );
        }
        assert_ne!(
            CANDLE_OUT_OF_WINDOW_REASONS[0], CANDLE_OUT_OF_WINDOW_REASONS[1],
            "two reasons must never share a metric label"
        );
    }
}

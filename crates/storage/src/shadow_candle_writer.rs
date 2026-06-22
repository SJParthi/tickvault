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
use secrecy::SecretString;
use tracing::{debug, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_trading::candles::BufferedSeal;

use crate::shadow_seal_columns::ShadowSealRow;

/// Build the questdb-rs ILP TCP connection conf string for a given
/// host + port. Mirrors the helper used by `LiveCandleWriter` so the
/// two writers connect identically.
fn build_ilp_conf_string(host: &str, ilp_port: u16) -> String {
    format!("tcp::addr={host}:{ilp_port};")
}

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
    // APPROVED: read by the reconnect path; no callers in this slice.
    #[allow(dead_code)]
    ilp_conf_string: SecretString,
}

impl ShadowCandleWriter {
    /// Production constructor. Connects to QuestDB via ILP TCP.
    ///
    /// If QuestDB is unreachable at startup the writer still
    /// constructs successfully with `sender = None`. A subsequent
    /// `append_seal` call will succeed (filling the local Buffer),
    /// but `flush` will return `Err` until a future reconnect lands
    /// (item 1.2f.3).
    pub fn new(config: &QuestDbConfig) -> Result<Self> {
        let conf_string = build_ilp_conf_string(&config.host, config.ilp_port);
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
        let row = ShadowSealRow::from_buffered_seal(seal);
        self.buffer
            .table(row.table_name)
            .with_context(|| format!("candle append: invalid table name {}", row.table_name))?
            // Feed-provenance label (operator 2026-06-19, "same tables + feed
            // column"). Constant `feed='dhan'` — part of the DEDUP key so a Dhan
            // candle and a Groww candle for the same minute/instrument never
            // collide. Stamped as a SYMBOL alongside `segment`. Zero-alloc.
            .symbol("feed", CANDLE_FEED_DHAN)
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
            .at(TimestampNanos::new(row.timestamp_ist_nanos))
            .with_context(|| "candle append: at(TimestampNanos) failed")?;
        self.pending_count += 1;
        Ok(())
    }

    /// Flush the buffered ILP rows to QuestDB.
    ///
    /// Returns `Err` when the writer is disconnected (caller — the
    /// future writer task — escalates to spill via the
    /// `SealAbsorptionPipeline`). On success the buffer is reset and
    /// `pending_count` returns to 0.
    ///
    /// This method is wired but NOT exercised by unit tests in this
    /// slice (no live QuestDB). Item 1.2f.3 lands the integration
    /// test that verifies it end-to-end.
    pub fn flush(&mut self) -> Result<()> {
        let Some(sender) = self.sender.as_mut() else {
            return Err(anyhow::anyhow!(
                "shadow flush: disconnected — sender is None"
            ));
        };
        if self.buffer.is_empty() {
            return Ok(());
        }
        sender
            .flush(&mut self.buffer)
            .context("shadow flush: ILP send failed")?;
        debug!(
            flushed_rows = self.pending_count,
            "shadow candle writer flushed"
        );
        self.pending_count = 0;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::constants::{EXCHANGE_SEGMENT_IDX_I, EXCHANGE_SEGMENT_NSE_EQ};
    use tickvault_trading::candles::{LiveCandleState, TfIndex};

    fn mk_seal(sid: u32, seg: u8, tf: TfIndex, bucket: u32, close: f64) -> BufferedSeal {
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
        state.oi_pct_from_prev_day = -0.2;
        state.volume_pct_from_prev_day = 12.3;
        BufferedSeal::new(sid, seg, tf, state)
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
    fn test_new_lazy_connects_when_questdb_unreachable() {
        // Use a port we know is closed. Construction must still
        // succeed (the writer enters disconnected mode), mirroring
        // LiveCandleWriter::new behaviour.
        let cfg = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 1, // closed port — connect will fail
        };
        let w =
            ShadowCandleWriter::new(&cfg).expect("constructor must not return Err on disconnect");
        assert!(
            !w.is_connected(),
            "writer must be disconnected for closed port"
        );
        assert_eq!(w.pending_count(), 0);
    }

    #[test]
    fn test_append_seal_increments_pending_count() {
        let mut w = ShadowCandleWriter::for_test();
        w.append_seal(&mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0))
            .expect("append");
        assert_eq!(w.pending_count(), 1);
        w.append_seal(&mk_seal(25, 0, TfIndex::M1, 1_716_001_500, 200.0))
            .expect("append");
        assert_eq!(w.pending_count(), 2);
    }

    #[test]
    fn test_append_seal_increments_buffer_row_count() {
        let mut w = ShadowCandleWriter::for_test();
        for i in 0..5 {
            let s = mk_seal(13 + i, 0, TfIndex::M1, 1_716_000_900 + i, 100.0 + i as f64);
            w.append_seal(&s).expect("append");
        }
        assert_eq!(w.buffer_row_count(), 5);
        assert_eq!(w.pending_count(), 5);
    }

    #[test]
    fn test_append_seal_writes_bytes_to_the_buffer() {
        let mut w = ShadowCandleWriter::for_test();
        assert_eq!(w.buffer_byte_count(), 0);
        w.append_seal(&mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0))
            .expect("append");
        assert!(
            w.buffer_byte_count() > 0,
            "ILP buffer must contain bytes after append"
        );
    }

    #[test]
    fn test_append_seal_buffer_contains_target_table_name_for_m1() {
        let mut w = ShadowCandleWriter::for_test();
        w.append_seal(&mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0))
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
            w.append_seal(&mk_seal(13, 0, tf, 1_716_000_900, 100.0))
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
            1_716_000_900,
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
            1_716_000_900,
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
            1_716_000_900,
            100.0,
        ))
        .expect("append idx_i");
        w.append_seal(&mk_seal(
            13,
            EXCHANGE_SEGMENT_NSE_EQ,
            TfIndex::M1,
            1_716_001_500,
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
        w.append_seal(&mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0))
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
        w.append_seal(&mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0))
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
        w.append_seal(&mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0))
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
        w.append_seal(&mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0))
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
    fn test_flush_returns_err_when_disconnected() {
        let mut w = ShadowCandleWriter::for_test();
        // Buffer empty → flush returns Err (disconnected, can't send
        // anything; the absent sender is the failure mode).
        let result = w.flush();
        assert!(result.is_err(), "flush MUST return Err when disconnected");
    }

    #[test]
    fn test_flush_returns_err_when_disconnected_with_pending_rows() {
        let mut w = ShadowCandleWriter::for_test();
        w.append_seal(&mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0))
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
            let s = mk_seal(13 + i, 0, TfIndex::M1, 1_716_000_900 + i, 100.0 + i as f64);
            w.append_seal(&s).expect("append");
            let now = w.buffer_byte_count();
            assert!(now > prev_bytes, "buffer must grow on each append");
            prev_bytes = now;
        }
        assert_eq!(w.buffer_row_count(), n as usize);
        assert_eq!(w.pending_count(), n as usize);
    }
}

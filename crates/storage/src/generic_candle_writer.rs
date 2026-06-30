//! SP4 — `GenericCandle1mWriter(feed)`: the ONE feed-parameterized candle
//! writer, named as an explicit public contract.
//!
//! ## What SP4 does (and the honest fact about what it does NOT)
//!
//! The "merge the two candle writers" work the SP4 plan-item text describes
//! was **already done structurally** by SP3b (#1180): the Groww-only candle
//! writer (`GrowwCandle1mWriter`) was deleted, and BOTH feeds now fold through
//! the SAME shared `MultiTfAggregator` → [`BufferedSeal`] (carrying `feed`) →
//! `SealWriterRunner` → [`ShadowCandleWriter::append_seal`], which stamps the
//! `feed` SYMBOL **from the seal** (never a hardcoded constant). So
//! [`ShadowCandleWriter`] is the already-converged, feed-parameterized writer
//! — parameterized PER SEAL (the seal carries its feed), which is strictly
//! more general than "constructed with one feed", because the shared seal
//! chain interleaves Dhan + Groww seals on one ILP connection.
//!
//! SP4 delivers the named `GenericCandle1mWriter(feed)` contract the plan
//! asks for as a thin, **feed-pinned facade** over [`ShadowCandleWriter`]:
//! constructed with a [`Feed`], it only ever writes seals of that feed (a
//! mismatched-feed seal is REJECTED, never silently mis-labelled). This is
//! additive + behavior-preserving:
//!
//! - identical wire bytes / `candles_<tf>` tables / OHLCV + pct columns,
//! - identical `(ts, security_id, segment, feed)` DEDUP key,
//! - identical lazy-connect + flush + ring/spill semantics (all delegated),
//! - NO live call-site rewired — the production shared seal chain keeps using
//!   the per-seal [`ShadowCandleWriter`] (it MUST interleave both feeds on one
//!   connection; pinning it to one feed would break that shared design).
//!
//! `GenericCandle1mWriter` is the single-feed handle for any future
//! single-feed candle wiring, and the type SP4's tests use to prove a Dhan
//! candle and a Groww candle each persist — with the correct `feed` label and
//! the existing DEDUP key — through the SAME generic writer type.

use anyhow::{Result, bail};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::feed::Feed;
use tickvault_trading::candles::BufferedSeal;

use crate::shadow_candle_writer::ShadowCandleWriter;

/// The ONE feed-parameterized candle writer, pinned to a single [`Feed`] at
/// construction.
///
/// Wraps the per-seal [`ShadowCandleWriter`] and enforces that every seal it
/// writes belongs to its pinned `feed`. The `feed` SYMBOL written to the
/// shared `candles_<tf>` tables comes from the seal (so it is byte-identical
/// to the shared seal chain), and the feed-pin is a defensive type-checked
/// invariant — a `GenericCandle1mWriter(Feed::Dhan)` can never write a
/// `feed='groww'` row, and vice-versa.
pub struct GenericCandle1mWriter {
    /// The feed this writer instance is pinned to. Every appended seal MUST
    /// carry this feed (else the append is rejected).
    feed: Feed,
    /// The existing per-seal feed-parameterized ILP writer. SP4 does not fork
    /// or duplicate it — it delegates verbatim, preserving every byte.
    inner: ShadowCandleWriter,
}

impl GenericCandle1mWriter {
    /// Production constructor. Pins the writer to `feed` and lazy-connects the
    /// inner [`ShadowCandleWriter`] to QuestDB ILP (same disconnect-OK
    /// behaviour as [`ShadowCandleWriter::new`]).
    pub fn new(config: &QuestDbConfig, feed: Feed) -> Result<Self> {
        let inner = ShadowCandleWriter::new(config)?;
        Ok(Self { feed, inner })
    }

    /// Test constructor — a disconnected writer pinned to `feed`.
    #[must_use]
    // TEST-EXEMPT: test-only construction helper used by every test in this module (feed-pin, per-feed stamping, mismatch rejection, flush-disconnected delegation). A separate name-matched test would be redundant.
    pub fn for_test(feed: Feed) -> Self {
        Self {
            feed,
            inner: ShadowCandleWriter::for_test(),
        }
    }

    /// The feed this writer is pinned to.
    #[must_use]
    pub const fn feed(&self) -> Feed {
        self.feed
    }

    /// Append one sealed candle. The seal's `feed` MUST match this writer's
    /// pinned feed; otherwise the append is REJECTED (returns `Err`) so a
    /// feed-pinned writer can never silently mis-label a row's `feed` SYMBOL.
    ///
    /// On a matching feed, delegates verbatim to
    /// [`ShadowCandleWriter::append_seal`] — same tables, same columns, same
    /// `feed` SYMBOL (sourced from the seal), same DEDUP key.
    pub fn append_candle(&mut self, seal: &BufferedSeal) -> Result<()> {
        if seal.feed != self.feed {
            bail!(
                "generic candle writer pinned to feed={} cannot write a feed={} seal \
                 (a feed-pinned writer never mis-labels the feed SYMBOL)",
                self.feed.as_str(),
                seal.feed.as_str(),
            );
        }
        self.inner.append_seal(seal)
    }

    /// Flush buffered ILP rows. Delegates to [`ShadowCandleWriter::flush`]
    /// (returns `Err` when disconnected — behaviour preserved).
    pub fn flush(&mut self) -> Result<()> {
        self.inner.flush()
    }

    /// `true` when the inner writer holds a live ILP `Sender`.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.inner.is_connected()
    }

    /// Seals appended since the last successful flush.
    #[must_use]
    pub const fn pending_count(&self) -> usize {
        self.inner.pending_count()
    }

    /// Bytes currently in the inner ILP buffer.
    #[must_use]
    pub fn buffer_byte_count(&self) -> usize {
        self.inner.buffer_byte_count()
    }

    /// Rows currently in the inner ILP buffer.
    #[must_use]
    pub fn buffer_row_count(&self) -> usize {
        self.inner.buffer_row_count()
    }

    /// Raw bytes currently in the inner ILP buffer (test/observability).
    #[must_use]
    pub fn buffer_bytes(&self) -> &[u8] {
        self.inner.buffer_bytes()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::constants::EXCHANGE_SEGMENT_IDX_I;
    use tickvault_trading::candles::{LiveCandleState, TfIndex};

    fn mk_seal_feed(sid: u64, seg: u8, bucket: u32, close: f64, feed: Feed) -> BufferedSeal {
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
        BufferedSeal::new(sid, seg, TfIndex::M1, state, feed)
    }

    #[test]
    fn test_generic_writer_pins_feed() {
        // The writer remembers the feed it was constructed with.
        let dhan = GenericCandle1mWriter::for_test(Feed::Dhan);
        assert_eq!(dhan.feed(), Feed::Dhan);
        let groww = GenericCandle1mWriter::for_test(Feed::Groww);
        assert_eq!(groww.feed(), Feed::Groww);
    }

    #[test]
    fn test_generic_writer_starts_disconnected_and_empty() {
        let w = GenericCandle1mWriter::for_test(Feed::Dhan);
        assert!(!w.is_connected());
        assert_eq!(w.pending_count(), 0);
        assert_eq!(w.buffer_row_count(), 0);
        assert_eq!(w.buffer_byte_count(), 0);
        assert!(w.buffer_bytes().is_empty());
    }

    #[test]
    fn test_generic_writer_dhan_seal_stamps_feed_dhan() {
        // A Dhan-pinned writer writing a Dhan seal stamps feed=dhan into the
        // shared candles_1m table — byte-identical to the shared seal chain.
        let mut w = GenericCandle1mWriter::for_test(Feed::Dhan);
        w.append_candle(&mk_seal_feed(
            13,
            EXCHANGE_SEGMENT_IDX_I,
            1_716_000_900,
            100.0,
            Feed::Dhan,
        ))
        .expect("append dhan");
        assert_eq!(w.pending_count(), 1);
        let s = std::str::from_utf8(w.buffer_bytes()).expect("utf8");
        assert!(
            s.contains("candles_1m"),
            "expected candles_1m table, got {s}"
        );
        assert!(
            s.contains("feed=dhan"),
            "expected feed=dhan SYMBOL, got {s}"
        );
    }

    #[test]
    fn test_generic_writer_groww_seal_stamps_feed_groww() {
        // The SAME generic writer TYPE, pinned to Groww, stamps feed=groww —
        // proving ONE feed-parameterized writer serves both feeds.
        let mut w = GenericCandle1mWriter::for_test(Feed::Groww);
        w.append_candle(&mk_seal_feed(
            13,
            EXCHANGE_SEGMENT_IDX_I,
            1_716_000_900,
            100.0,
            Feed::Groww,
        ))
        .expect("append groww");
        let s = std::str::from_utf8(w.buffer_bytes()).expect("utf8");
        assert!(
            s.contains("candles_1m"),
            "expected candles_1m table, got {s}"
        );
        assert!(
            s.contains("feed=groww"),
            "expected feed=groww SYMBOL, got {s}"
        );
        assert!(
            !s.contains("feed=dhan"),
            "a Groww writer must NOT stamp feed=dhan, got {s}"
        );
    }

    #[test]
    fn test_generic_writer_both_feeds_one_writer_type() {
        // The deliverable: a Dhan candle row and a Groww candle row both
        // persist to candles_1m with the correct feed label, via the SAME
        // GenericCandle1mWriter type (two instances, one per feed).
        let mut dhan = GenericCandle1mWriter::for_test(Feed::Dhan);
        dhan.append_candle(&mk_seal_feed(
            13,
            EXCHANGE_SEGMENT_IDX_I,
            1_716_000_900,
            100.0,
            Feed::Dhan,
        ))
        .expect("append dhan");
        let mut groww = GenericCandle1mWriter::for_test(Feed::Groww);
        groww
            .append_candle(&mk_seal_feed(
                13,
                EXCHANGE_SEGMENT_IDX_I,
                1_716_000_900,
                100.0,
                Feed::Groww,
            ))
            .expect("append groww");

        let ds = std::str::from_utf8(dhan.buffer_bytes()).expect("utf8");
        let gs = std::str::from_utf8(groww.buffer_bytes()).expect("utf8");
        assert!(ds.contains("candles_1m") && ds.contains("feed=dhan"));
        assert!(gs.contains("candles_1m") && gs.contains("feed=groww"));
        // Both land in the SAME shared table; the feed column distinguishes
        // them under the (ts, security_id, segment, feed) DEDUP key.
        assert!(ds.contains("security_id=13i") && gs.contains("security_id=13i"));
    }

    #[test]
    fn test_generic_writer_rejects_mismatched_feed() {
        // A Dhan-pinned writer REFUSES a Groww seal — never silently writes
        // the wrong feed label. Defensive type-checked feed-pin invariant.
        let mut w = GenericCandle1mWriter::for_test(Feed::Dhan);
        let result = w.append_candle(&mk_seal_feed(
            13,
            EXCHANGE_SEGMENT_IDX_I,
            1_716_000_900,
            100.0,
            Feed::Groww,
        ));
        assert!(result.is_err(), "mismatched-feed seal must be rejected");
        // Nothing was buffered.
        assert_eq!(w.pending_count(), 0);
        assert_eq!(w.buffer_row_count(), 0);
    }

    #[test]
    fn test_generic_writer_rejects_mismatched_feed_groww_writer() {
        // Symmetric: a Groww-pinned writer refuses a Dhan seal.
        let mut w = GenericCandle1mWriter::for_test(Feed::Groww);
        let result = w.append_candle(&mk_seal_feed(
            13,
            EXCHANGE_SEGMENT_IDX_I,
            1_716_000_900,
            100.0,
            Feed::Dhan,
        ));
        assert!(result.is_err(), "mismatched-feed seal must be rejected");
        assert_eq!(w.pending_count(), 0);
    }

    #[test]
    fn test_append_candle_increments_pending_and_buffers_row() {
        // Name-matched coverage for the `append_candle` pub fn: a matching-feed
        // seal increments pending_count and writes a row into the ILP buffer.
        let mut w = GenericCandle1mWriter::for_test(Feed::Dhan);
        assert_eq!(w.pending_count(), 0);
        w.append_candle(&mk_seal_feed(
            13,
            EXCHANGE_SEGMENT_IDX_I,
            1_716_000_900,
            100.0,
            Feed::Dhan,
        ))
        .expect("append");
        assert_eq!(w.pending_count(), 1);
        assert_eq!(w.buffer_row_count(), 1);
        assert!(w.buffer_byte_count() > 0);
    }

    #[test]
    fn test_generic_writer_delegates_flush_disconnected() {
        // Behaviour change (2026-06-30 candle-reconnect): an EMPTY-buffer flush is
        // a no-op Ok (nothing to persist → no reconnect, no error). This delegates
        // to ShadowCandleWriter::flush, so it inherits the same semantics.
        let mut w = GenericCandle1mWriter::for_test(Feed::Dhan);
        assert!(
            w.flush().is_ok(),
            "empty-buffer flush is a no-op Ok, even disconnected"
        );
        // With a pending row, a disconnected flush still Errs (the for_test writer
        // has an empty conf → reconnect fails) AND the row is RETAINED for the
        // spill re-route — the zero-loss invariant the candle reconnect preserves.
        w.append_candle(&mk_seal_feed(
            13,
            EXCHANGE_SEGMENT_IDX_I,
            1_716_000_900,
            100.0,
            Feed::Dhan,
        ))
        .expect("append");
        assert!(
            w.flush().is_err(),
            "flush with a pending row on a disconnected writer must Err"
        );
        assert_eq!(
            w.pending_count(),
            1,
            "pending row retained for spill re-route on disconnected flush"
        );
    }
}

//! Depth sequence-hole detector (Phase 10.2, PR #288).
//!
//! Dhan's 20-level depth feed carries a 32-bit message-sequence number in
//! bytes 8-11 of the 12-byte header. Sequences are per-connection and
//! informational — the feed doc calls out the server may reset or skip
//! on reconnect — but persistent gaps while a session is alive indicate
//! real packet loss (network drop, buffer overflow, server-side issue)
//! that belongs in an operator dashboard.
//!
//! This tracker is a standalone pipeline module that the depth consumer
//! calls once per parsed frame. It is cold-configuration (one map
//! insert on first-seen id) and hot-read (one lookup + one compare) per
//! packet. Segment-aware per I-P1-11 — the key is `(security_id, segment,
//! side)`.
//!
//! # O(1) guarantees
//! - `observe()` does: 1 `papaya::HashMap` lookup, 1 scalar compare,
//!   0-1 atomic counter increment, 0 allocation. No Vec, no clone, no
//!   String. Bounded by `with_capacity(INITIAL_CAPACITY)`.
//! - Memory is `O(instruments_ever_seen * 2)` (bid + ask), capped by
//!   Dhan's 50-instruments-per-20-level-connection × 4 connections = 200
//!   ids max per process.
//!
//! # Metrics emitted
//! - `tv_depth_sequence_holes_total{segment,side}` — counter, rising edge
//!   only. Dashboard + alert decisions live in Grafana.
//! - `tv_depth_sequence_duplicates_total{segment,side}` — counter, for
//!   operators who want to know replay behaviour.
//! - `tv_depth_sequence_rollbacks_total{segment,side}` — counter, for
//!   reconnect observability.

use std::sync::Arc;

use metrics::counter;
use papaya::HashMap as PapayaMap;
use tickvault_common::types::ExchangeSegment;

use crate::parser::deep_depth::DepthSide;

/// Initial capacity: 4 depth connections × 50 instruments × 2 sides = 400.
/// Papaya re-allocates beyond this but we aim to never pay that cost in
/// production.
const INITIAL_CAPACITY: usize = 512;

/// The key we track sequence under. Segment-aware per I-P1-11: FINNIFTY
/// IDX_I id=27 and NSE_EQ id=27 are two distinct streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct TrackerKey {
    security_id: u32,
    segment: ExchangeSegment,
    side: DepthSide,
}

/// Outcome of observing a new sequence — returned for tests and for
/// callers that want to gate downstream work (persist / alert / skip).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceOutcome {
    /// First packet seen for this key — sequence recorded, nothing compared.
    FirstSeen,
    /// Sequence advanced by exactly 1 (the healthy case).
    Monotonic,
    /// Sequence advanced by more than 1. The number of missed packets is
    /// recorded (saturates at `u32::MAX`) for dashboards.
    HoleDetected { missed: u32 },
    /// Sequence went backwards and by more than a small amount — most
    /// likely a server-side reset on reconnect. Tracker state is reset.
    Rollback,
    /// Same sequence as previously observed — usually a benign replay
    /// after reconnect. Not a hole.
    Duplicate,
}

/// O(1) segment-aware sequence tracker for 20-level depth frames.
///
/// Cheap to share: the internal map is an `Arc<PapayaMap>`, so cloning
/// the tracker is a single atomic refcount bump. Callers may store one
/// `Arc<DepthSequenceTracker>` in the depth processor and reuse it for
/// the whole connection lifetime.
#[derive(Clone)]
pub struct DepthSequenceTracker {
    last_sequence: Arc<PapayaMap<TrackerKey, u32>>,
}

impl DepthSequenceTracker {
    /// Create a new tracker sized for the production workload.
    #[must_use]
    // TEST-EXEMPT: covered by test_first_packet_initializes_sequence, test_default_equals_new
    pub fn new() -> Self {
        Self {
            last_sequence: Arc::new(PapayaMap::with_capacity(INITIAL_CAPACITY)),
        }
    }

    /// Observe a sequence number for `(security_id, segment, side)`.
    /// Emits Prometheus counters as appropriate and returns the outcome
    /// so callers can log at their preferred level.
    ///
    /// # Performance
    /// Constant-time: one papaya lookup, one compare, optional insert.
    /// Never allocates. Never blocks.
    // TEST-EXEMPT: covered by test_first_packet_initializes_sequence, test_monotonic_increment_no_gap, test_gap_increments_hole_counter, test_rollback_resets_tracking, test_duplicate_counted_separately, test_bid_ask_tracked_independently, test_tracker_is_segment_aware_i_p1_11, test_u32_max_sequence_does_not_panic, test_big_gap_reports_missed_count
    pub fn observe(
        &self,
        security_id: u32,
        segment: ExchangeSegment,
        side: DepthSide,
        new_sequence: u32,
    ) -> SequenceOutcome {
        let key = TrackerKey {
            security_id,
            segment,
            side,
        };
        let pin = self.last_sequence.pin();
        let outcome = match pin.get(&key) {
            None => {
                pin.insert(key, new_sequence);
                SequenceOutcome::FirstSeen
            }
            Some(&prev) if new_sequence == prev.saturating_add(1) => {
                pin.insert(key, new_sequence);
                SequenceOutcome::Monotonic
            }
            Some(&prev) if new_sequence == prev => SequenceOutcome::Duplicate,
            Some(&prev) if new_sequence > prev => {
                // Gap of N means we missed (new - prev - 1) packets. Clamp
                // to u32::MAX in the impossible case of a saturated u32
                // sequence; the counter stays meaningful either way.
                let missed = new_sequence.saturating_sub(prev).saturating_sub(1);
                pin.insert(key, new_sequence);
                SequenceOutcome::HoleDetected { missed }
            }
            Some(_) => {
                // new_sequence < prev by more than 1 → rollback.
                pin.insert(key, new_sequence);
                SequenceOutcome::Rollback
            }
        };

        // Emit metrics on the edge events; keep labels static strings to
        // avoid allocating label buffers in the hot path.
        let segment_label = segment.as_str();
        let side_label = match side {
            DepthSide::Bid => "bid",
            DepthSide::Ask => "ask",
        };
        match outcome {
            SequenceOutcome::HoleDetected { .. } => {
                counter!(
                    "tv_depth_sequence_holes_total",
                    "segment" => segment_label,
                    "side" => side_label,
                )
                .increment(1);
            }
            SequenceOutcome::Duplicate => {
                counter!(
                    "tv_depth_sequence_duplicates_total",
                    "segment" => segment_label,
                    "side" => side_label,
                )
                .increment(1);
            }
            SequenceOutcome::Rollback => {
                counter!(
                    "tv_depth_sequence_rollbacks_total",
                    "segment" => segment_label,
                    "side" => side_label,
                )
                .increment(1);
            }
            SequenceOutcome::FirstSeen | SequenceOutcome::Monotonic => {}
        }
        outcome
    }

    /// Number of distinct `(security_id, segment, side)` keys currently
    /// tracked. Used by tests + operator diagnostics (cold path).
    #[must_use]
    // TEST-EXEMPT: covered by test_first_packet_initializes_sequence, test_bid_ask_tracked_independently, test_tracker_is_segment_aware_i_p1_11, test_default_equals_new
    pub fn tracked_key_count(&self) -> usize {
        self.last_sequence.pin().len()
    }
}

impl Default for DepthSequenceTracker {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn seg() -> ExchangeSegment {
        ExchangeSegment::NseFno
    }

    #[test]
    fn test_first_packet_initializes_sequence() {
        let t = DepthSequenceTracker::new();
        let out = t.observe(1333, seg(), DepthSide::Bid, 1000);
        assert_eq!(out, SequenceOutcome::FirstSeen);
        assert_eq!(t.tracked_key_count(), 1);
    }

    #[test]
    fn test_monotonic_increment_no_gap() {
        let t = DepthSequenceTracker::new();
        t.observe(1333, seg(), DepthSide::Bid, 10);
        assert_eq!(
            t.observe(1333, seg(), DepthSide::Bid, 11),
            SequenceOutcome::Monotonic
        );
    }

    #[test]
    fn test_gap_increments_hole_counter() {
        let t = DepthSequenceTracker::new();
        t.observe(1333, seg(), DepthSide::Bid, 10);
        assert_eq!(
            t.observe(1333, seg(), DepthSide::Bid, 12),
            SequenceOutcome::HoleDetected { missed: 1 }
        );
    }

    #[test]
    fn test_big_gap_reports_missed_count() {
        let t = DepthSequenceTracker::new();
        t.observe(1333, seg(), DepthSide::Bid, 100);
        assert_eq!(
            t.observe(1333, seg(), DepthSide::Bid, 1000),
            SequenceOutcome::HoleDetected { missed: 899 }
        );
    }

    #[test]
    fn test_rollback_resets_tracking() {
        let t = DepthSequenceTracker::new();
        t.observe(1333, seg(), DepthSide::Bid, 500);
        assert_eq!(
            t.observe(1333, seg(), DepthSide::Bid, 10),
            SequenceOutcome::Rollback
        );
        // Next monotonic increment should now be clean.
        assert_eq!(
            t.observe(1333, seg(), DepthSide::Bid, 11),
            SequenceOutcome::Monotonic
        );
    }

    #[test]
    fn test_duplicate_counted_separately() {
        let t = DepthSequenceTracker::new();
        t.observe(1333, seg(), DepthSide::Bid, 10);
        assert_eq!(
            t.observe(1333, seg(), DepthSide::Bid, 10),
            SequenceOutcome::Duplicate
        );
    }

    #[test]
    fn test_bid_ask_tracked_independently() {
        let t = DepthSequenceTracker::new();
        t.observe(1333, seg(), DepthSide::Bid, 100);
        // Ask is a different key — first-seen, no hole.
        assert_eq!(
            t.observe(1333, seg(), DepthSide::Ask, 50),
            SequenceOutcome::FirstSeen
        );
        // Bid advances cleanly.
        assert_eq!(
            t.observe(1333, seg(), DepthSide::Bid, 101),
            SequenceOutcome::Monotonic
        );
        assert_eq!(t.tracked_key_count(), 2);
    }

    #[test]
    fn test_tracker_is_segment_aware_i_p1_11() {
        // Same security_id on different segments must be independent.
        // (Depth feed in practice only covers NSE_EQ and NSE_FNO, but the
        // tracker is ready for the full enum.)
        let t = DepthSequenceTracker::new();
        t.observe(27, ExchangeSegment::NseFno, DepthSide::Bid, 100);
        assert_eq!(
            t.observe(27, ExchangeSegment::NseEquity, DepthSide::Bid, 50),
            SequenceOutcome::FirstSeen
        );
        assert_eq!(t.tracked_key_count(), 2);
    }

    #[test]
    fn test_u32_max_sequence_does_not_panic() {
        let t = DepthSequenceTracker::new();
        t.observe(1333, seg(), DepthSide::Bid, u32::MAX - 1);
        // saturating_add prevents overflow; u32::MAX is treated as
        // monotonic successor of u32::MAX - 1.
        assert_eq!(
            t.observe(1333, seg(), DepthSide::Bid, u32::MAX),
            SequenceOutcome::Monotonic
        );
        // Arrival of 0 after u32::MAX is NOT a rollback in practice
        // (server-side wraparound is observationally identical to a
        // reconnect reset). The tracker records it as a rollback — this
        // is the safer outcome because it does NOT report a spurious
        // u32::MAX-sized hole on wraparound.
        assert_eq!(
            t.observe(1333, seg(), DepthSide::Bid, 0),
            SequenceOutcome::Rollback
        );
    }

    #[test]
    fn test_default_equals_new() {
        let a = DepthSequenceTracker::new();
        let b = DepthSequenceTracker::default();
        assert_eq!(a.tracked_key_count(), b.tracked_key_count());
    }

    #[test]
    fn test_clone_shares_state() {
        // Tracker clones must share the inner map — otherwise two handles
        // to the same logical tracker would diverge and the hole count
        // would be under-reported.
        let t1 = DepthSequenceTracker::new();
        let t2 = t1.clone();
        t1.observe(1333, seg(), DepthSide::Bid, 100);
        // t2 should see the same prior state.
        assert_eq!(
            t2.observe(1333, seg(), DepthSide::Bid, 101),
            SequenceOutcome::Monotonic
        );
    }
}

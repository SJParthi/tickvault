//! The WAL receipt anchor's cell (plan ITEM DT-SOCK, 2026-10-30), split out of
//! `ws_frame_spill.rs` so that file does not grow. See [`AnchorCell`].

use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use super::{AnchorRefresh, ReceiptAnchor, anchor_refresh_decision};

/// One published anchor: its instant as nanos past [`AnchorCell::base`], its
/// wall nanos, and a version that is odd while the slot is being written.
#[derive(Debug)]
struct AnchorSlot {
    version: AtomicU64,
    offset_nanos: AtomicU64,
    nanos: AtomicI64,
}

impl AnchorSlot {
    const fn new(offset_nanos: u64, nanos: i64) -> Self {
        Self {
            version: AtomicU64::new(0),
            offset_nanos: AtomicU64::new(offset_nanos),
            nanos: AtomicI64::new(nanos),
        }
    }
}

/// The receipt anchor's cell (ITEM DT, 2026-10-30): two slots and an index,
/// read with plain loads.
///
/// It was an `ArcSwap<ReceiptAnchor>`, whose `load` takes a debt slot and pays
/// it back on drop: three SeqCst read-modify-writes per frame on the socket read
/// task, for a value that changes once every 30 s. Measured at production
/// spacing on the x86 dev VM: 121 ns p50, 1,119 ns p99 for `Instant::now` plus
/// that load (`socket_read_in_memory_harness.rs`, breakdown).
///
/// Now a reader loads the active index, then that slot's version, both halves
/// and the version again, and retries only if the version moved or is odd. A
/// refresh writes the INACTIVE slot and then flips the index, so a reader never
/// waits on a writer: the slot it reads is only rewritten after a second
/// refresh, and a reader that slow simply re-reads the index. The two halves
/// still travel together, which is the property the old single swapped value
/// had (a torn pair would be off by a whole refresh interval).
///
/// `base` is the first anchor's instant and never changes. Every receipt is
/// `nanos + saturating(captured − base − offset)`, which is exactly
/// `nanos + saturating(captured − instant)` (integer nanos, both terms clamp at
/// zero for an instant before the anchor), so the output is unchanged.
///
/// Aligned to a cache line so it never shares one with a static written per
/// frame (`WAL_FRAME_SEQ` is CAS'd on every frame).
#[derive(Debug)]
#[repr(align(64))]
pub(super) struct AnchorCell {
    base: Instant,
    active: AtomicUsize,
    slots: [AnchorSlot; 2],
    /// Serialises refreshes (cold, one every 30 s). Never taken by a reader.
    writer: std::sync::Mutex<()>,
}

/// `Duration` to `u64` nanos, saturating (584 years).
#[inline]
fn duration_nanos_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

impl AnchorCell {
    pub(super) fn new(first: ReceiptAnchor) -> Self {
        Self {
            base: first.instant,
            active: AtomicUsize::new(0),
            slots: [
                AnchorSlot::new(0, first.nanos),
                AnchorSlot::new(0, first.nanos),
            ],
            writer: std::sync::Mutex::new(()),
        }
    }

    /// The published anchor as `(offset past base, wall nanos)`. Loads only.
    #[inline]
    pub(super) fn load(&self) -> (u64, i64) {
        loop {
            let slot = &self.slots[self.active.load(Ordering::Acquire) & 1];
            let before = slot.version.load(Ordering::Acquire);
            if before & 1 == 0 {
                let offset = slot.offset_nanos.load(Ordering::Relaxed);
                let nanos = slot.nanos.load(Ordering::Relaxed);
                std::sync::atomic::fence(Ordering::Acquire);
                if slot.version.load(Ordering::Relaxed) == before {
                    return (offset, nanos);
                }
            }
            std::hint::spin_loop();
        }
    }

    /// The published anchor as a [`ReceiptAnchor`]. Cold (refresh, tests).
    pub(super) fn current(&self) -> ReceiptAnchor {
        let (offset, nanos) = self.load();
        ReceiptAnchor {
            instant: self
                .base
                .checked_add(Duration::from_nanos(offset))
                .unwrap_or(self.base),
            nanos,
        }
    }

    /// Publishes `anchor`: writes the inactive slot, then flips the index.
    /// The caller holds `writer`.
    pub(super) fn publish(&self, anchor: ReceiptAnchor) {
        debug_assert!(
            anchor.instant >= self.base,
            "every anchor is taken after the first one"
        );
        let target = (self.active.load(Ordering::Relaxed) & 1) ^ 1;
        let slot = &self.slots[target];
        // Odd = being written. Derived with `| 1`, not `+ 1`, so a slot whose
        // version were ever left odd could not flip the parity and leave the
        // active slot looking "in progress" to every reader for good.
        let writing = slot.version.load(Ordering::Relaxed) | 1;
        slot.version.store(writing, Ordering::Relaxed);
        std::sync::atomic::fence(Ordering::Release);
        slot.offset_nanos.store(
            duration_nanos_u64(anchor.instant.saturating_duration_since(self.base)),
            Ordering::Relaxed,
        );
        slot.nanos.store(anchor.nanos, Ordering::Relaxed);
        slot.version
            .store(writing.wrapping_add(1), Ordering::Release);
        self.active.store(target, Ordering::Release);
    }

    /// Takes `new_anchor()` and publishes it when [`anchor_refresh_decision`]
    /// adopts it. Refreshes are serialised by `writer` (never taken by a
    /// reader), so two callers cannot both publish from the same old anchor.
    /// A poisoned lock still guards: a slot is published only after both its
    /// halves are stored.
    pub(super) fn refresh_with(&self, new_anchor: impl FnOnce() -> ReceiptAnchor) -> AnchorRefresh {
        let _writer = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let new = new_anchor();
        let decision = anchor_refresh_decision(self.current(), new);
        if decision != AnchorRefresh::RefuseBackward {
            self.publish(new);
        }
        decision
    }

    /// The receipt for `captured_at` against this cell: the anchor's nanos
    /// plus `captured − anchor instant`, computed as `(captured − base) −
    /// offset`. Both forms clamp at zero for an instant before the anchor
    /// (`saturating_duration_since`), so the result is the same integer.
    #[inline]
    pub(super) fn receipt_nanos(&self, captured_at: Instant) -> i64 {
        let (offset, nanos) = self.load();
        let delta = duration_nanos_u64(captured_at.saturating_duration_since(self.base))
            .saturating_sub(offset);
        nanos.saturating_add(i64::try_from(delta).unwrap_or(i64::MAX))
    }
}

/// ITEM DT (2026-10-30): the receipt anchor moved from an `ArcSwap` to a
/// two-slot cell read with plain loads. These pin that every receipt is the
/// same integer the old code produced, and that a reader never sees one slot's
/// offset against another slot's wall nanos.
#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::sync::atomic::AtomicBool;

    /// The pre-2026-10-30 arithmetic, verbatim, as the reference.
    fn old_receipt(anchor: ReceiptAnchor, captured_at: Instant) -> i64 {
        let delta = captured_at.saturating_duration_since(anchor.instant);
        let delta_nanos = i64::try_from(delta.as_nanos()).unwrap_or(i64::MAX);
        anchor.nanos.saturating_add(delta_nanos)
    }

    /// The production path: `receipt_nanos_from` is exactly this call on the
    /// process cell.
    fn new_receipt(cell: &AnchorCell, captured_at: Instant) -> i64 {
        cell.receipt_nanos(captured_at)
    }

    /// Wall nanos that exercise saturation, plus ordinary ones.
    fn edge_nanos() -> impl Strategy<Value = i64> {
        prop_oneof![
            any::<i64>(),
            (0_i64..100_000_000_000).prop_map(|d| i64::MAX - d),
            (0_i64..100_000_000_000).prop_map(|d| i64::MIN + d),
            1_700_000_000_000_000_000_i64..1_900_000_000_000_000_000,
        ]
    }

    proptest! {
        /// Old and new give the same receipt for any anchor history and any
        /// capture instant, including instants before the base, between the
        /// base and the anchor, and wall nanos near `i64` limits.
        #[test]
        fn proptest_receipt_identical_to_the_arcswap_arithmetic(
            first_nanos in edge_nanos(),
            later in proptest::collection::vec((0_u64..10_000_000_000, edge_nanos()), 0..4),
            captured_before in 0_u64..5_000_000_000,
            captured_after in 0_u64..50_000_000_000,
        ) {
            let base = Instant::now() + Duration::from_secs(10);
            let first = ReceiptAnchor { instant: base, nanos: first_nanos };
            let cell = AnchorCell::new(first);
            let mut current = first;
            for (step, nanos) in later {
                current = ReceiptAnchor {
                    instant: current.instant + Duration::from_nanos(step),
                    nanos,
                };
                cell.publish(current);
            }
            prop_assert_eq!(cell.current().instant, current.instant);
            prop_assert_eq!(cell.current().nanos, current.nanos);
            for captured in [
                base - Duration::from_nanos(captured_before),
                base + Duration::from_nanos(captured_after),
                current.instant + Duration::from_nanos(captured_after),
                current.instant,
            ] {
                prop_assert_eq!(new_receipt(&cell, captured), old_receipt(current, captured));
            }
        }
    }

    /// A reader racing a writer that republishes as fast as it can always
    /// gets a pair that some single publish wrote. Each publish writes
    /// `nanos = 7 × offset`, so a torn pair breaks the relation.
    ///
    /// Honest limit: on x86 (strongly ordered) this passes even without the
    /// fences, so it catches a broken version protocol, not a missing fence.
    /// The fence placement is Boehm's seqlock pattern and was reviewed for
    /// the C++20 model (the ARM64 production target); it is not tested on
    /// aarch64 here.
    #[test]
    fn test_anchor_cell_reader_never_sees_a_torn_pair() {
        let base = Instant::now();
        let cell = std::sync::Arc::new(AnchorCell::new(ReceiptAnchor {
            instant: base,
            nanos: 0,
        }));
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let writer = {
            let cell = std::sync::Arc::clone(&cell);
            let stop = std::sync::Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut offset = 0_u64;
                while !stop.load(Ordering::Relaxed) {
                    offset += 1;
                    cell.publish(ReceiptAnchor {
                        instant: base + Duration::from_nanos(offset),
                        nanos: (offset * 7) as i64,
                    });
                }
                offset
            })
        };
        let mut reads = 0_u64;
        let deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < deadline {
            let (offset, nanos) = cell.load();
            assert_eq!(nanos, (offset * 7) as i64, "torn anchor pair");
            reads += 1;
        }
        stop.store(true, Ordering::Relaxed);
        let published = writer.join().expect("writer");
        assert!(
            published > 1_000,
            "the writer raced the reader ({published} publishes)"
        );
        assert!(reads > 1_000, "the reader ran ({reads} reads)");
    }

    /// The refresh decision drives the publish: a forward anchor and a
    /// wall-clock step back are published (the next receipt follows them), a
    /// slew-sized rewind is refused and leaves the cell exactly as it was.
    #[test]
    fn test_refresh_with_publishes_adopted_anchors_and_keeps_a_refused_one_out() {
        let t0 = Instant::now();
        let base_nanos = 1_780_000_000_000_000_000_i64;
        let secs = |n: i64| n * 1_000_000_000;
        let cell = AnchorCell::new(ReceiptAnchor {
            instant: t0,
            nanos: base_nanos,
        });
        let at = |after: u64, nanos: i64| ReceiptAnchor {
            instant: t0 + Duration::from_secs(after),
            nanos,
        };
        // Forward: adopted and published.
        let forward = at(30, base_nanos + secs(30) + 5);
        assert_eq!(cell.refresh_with(|| forward), AnchorRefresh::Adopt);
        assert_eq!(cell.current().instant, forward.instant);
        assert_eq!(cell.current().nanos, forward.nanos);
        assert_eq!(cell.receipt_nanos(forward.instant), forward.nanos);
        // Slew-sized rewind: refused, the cell keeps the forward anchor.
        let slew = at(60, base_nanos + secs(60) - 10_000_000);
        assert_eq!(cell.refresh_with(|| slew), AnchorRefresh::RefuseBackward);
        assert_eq!(cell.current().instant, forward.instant);
        assert_eq!(cell.current().nanos, forward.nanos);
        // A 10 s step back: adopted and published.
        let stepped = at(90, base_nanos + secs(80));
        assert!(matches!(
            cell.refresh_with(|| stepped),
            AnchorRefresh::AdoptBackwardStep { .. }
        ));
        assert_eq!(cell.current().instant, stepped.instant);
        assert_eq!(cell.receipt_nanos(stepped.instant), stepped.nanos);
    }

    /// A slot left odd (a write that never finished) cannot flip parity: the
    /// next publish still ends even, so readers are never stranded.
    #[test]
    fn test_publish_ends_even_even_after_a_slot_was_left_odd() {
        let t0 = Instant::now();
        let cell = AnchorCell::new(ReceiptAnchor {
            instant: t0,
            nanos: 0,
        });
        cell.slots[1].version.store(3, Ordering::Relaxed);
        cell.publish(ReceiptAnchor {
            instant: t0 + Duration::from_secs(1),
            nanos: 9,
        });
        assert_eq!(cell.slots[1].version.load(Ordering::Relaxed) & 1, 0);
        assert_eq!(cell.load(), (1_000_000_000, 9));
    }
}

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

/// How far `new` moves the time `old` projects for `new.instant`, either way,
/// in nanos (saturating). Pure, O(1).
fn anchor_jump_nanos(old: ReceiptAnchor, new: ReceiptAnchor) -> u64 {
    let elapsed = new.instant.saturating_duration_since(old.instant);
    let projected = old
        .nanos
        .saturating_add(i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX));
    new.nanos.abs_diff(projected)
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
    #[cfg(test)]
    pub(super) fn refresh_with(&self, new_anchor: impl FnOnce() -> ReceiptAnchor) -> AnchorRefresh {
        self.refresh_noting_step(|| Some(new_anchor()), |_, _, _| {})
            .unwrap_or(AnchorRefresh::RefuseBackward)
    }

    /// [`Self::refresh_with`], and when the adopted anchor moves the projected
    /// time by more than one second either way (a wall-clock step, PR AA
    /// review finding 2), calls `note_step(previous anchor's wall nanos, new wall nanos, size of
    /// the move)`. The previous anchor bounds where the step can have begun:
    /// the refresh can run late on a busy drain (recheck of finding 2).
    /// The production refresh passes `wal_clock_step::note_clock_step`; the
    /// cell tests pass nothing, so synthetic anchors never reach the process
    /// window. O(1).
    ///
    /// 2026-11-17: `new_anchor` returns `None` when it could not read the two
    /// clocks close enough together ([`best_anchor_reading`]); the cell is
    /// then left as it is, nothing is noted, and `None` is returned.
    pub(super) fn refresh_noting_step(
        &self,
        new_anchor: impl FnOnce() -> Option<ReceiptAnchor>,
        note_step: impl FnOnce(u64, u64, u64),
    ) -> Option<AnchorRefresh> {
        let _writer = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let new = new_anchor()?;
        let old = self.current();
        let decision = anchor_refresh_decision(old, new);
        if decision != AnchorRefresh::RefuseBackward {
            self.publish(new);
            let jump = anchor_jump_nanos(old, new);
            if jump > crate::wal_clock_step::CLOCK_STEP_MIN_NANOS {
                note_step(
                    u64::try_from(old.nanos).unwrap_or(0),
                    u64::try_from(new.nanos).unwrap_or(0),
                    jump,
                );
            }
        }
        Some(decision)
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

/// How far apart the two monotonic reads around the wall-clock read may be
/// for the reading to anchor receipts (2026-11-17). Live, the box recorded
/// six "clock steps" of 1.0 to 1.1 s at restarts and load bursts while
/// chrony's offset was 0.35 µs and it never stepped: the thread was
/// descheduled for about a second between its wall-clock and monotonic
/// reads, so the anchor paired a wall time with an instant a second later
/// and the next refresh read the difference as a step. 1 ms is far above
/// the two reads' own cost (well under a microsecond, Assumed) and far
/// below the 1 s step threshold.
pub(super) const ANCHOR_READ_TIGHT_NANOS: u64 = 1_000_000;

/// How many readings [`best_anchor_reading`] takes at most.
pub(super) const ANCHOR_READ_ATTEMPTS: u32 = 3;

/// One paired reading of the two clocks (2026-11-17): the monotonic clock,
/// then the wall clock, then the monotonic clock again.
#[derive(Debug, Clone, Copy)]
pub(super) struct AnchorReading {
    pub(super) before: Instant,
    pub(super) nanos: i64,
    pub(super) after: Instant,
}

impl AnchorReading {
    /// How far apart the two monotonic reads are. O(1).
    fn gap_nanos(self) -> u64 {
        duration_nanos_u64(self.after.saturating_duration_since(self.before))
    }

    /// The wall nanos paired with the SECOND monotonic read: any delay
    /// between the two makes a derived receipt slightly early, never late
    /// (an early receipt files a frame in the bucket it arrived in), the
    /// bias the single wall-then-monotonic read always had.
    fn anchor(self) -> ReceiptAnchor {
        ReceiptAnchor {
            instant: self.after,
            nanos: self.nanos,
        }
    }
}

/// Takes readings until one has its two monotonic reads within
/// [`ANCHOR_READ_TIGHT_NANOS`], at most [`ANCHOR_READ_ATTEMPTS`]. Returns the
/// tightest reading's anchor and whether it is tight. A real wall-clock step
/// shows on a tight reading exactly as on any other, so this never hides a
/// step; it only refuses a pair of reads a deschedule pulled apart.
/// O(attempts) = O(3), no allocation, no lock.
pub(super) fn best_anchor_reading(
    mut read: impl FnMut() -> AnchorReading,
) -> (ReceiptAnchor, bool) {
    let mut best = read();
    let mut taken = 1;
    // O(1) EXEMPT: bounded by ANCHOR_READ_ATTEMPTS (3)
    loop {
        if best.gap_nanos() <= ANCHOR_READ_TIGHT_NANOS {
            return (best.anchor(), true);
        }
        if taken >= ANCHOR_READ_ATTEMPTS {
            return (best.anchor(), false);
        }
        let next = read();
        taken += 1;
        if next.gap_nanos() < best.gap_nanos() {
            best = next;
        }
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

    /// PR AA review finding 2: an adopted anchor that moves the projected time
    /// by more than 1 s either way is reported as a clock step with its size;
    /// a normal forward refresh and a refused slew report nothing.
    #[test]
    fn test_refresh_noting_step_reports_steps_both_ways_and_ignores_slew() {
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
        let mut seen: Vec<(u64, u64, u64)> = Vec::new();
        // Normal refresh, 5 ms of forward slew: adopted, no step.
        let normal = at(30, base_nanos + secs(30) + 5_000_000);
        cell.refresh_noting_step(|| Some(normal), |o, w, m| seen.push((o, w, m)));
        // Slew-sized rewind: refused, no step.
        let slew = at(60, base_nanos + secs(60) - 10_000_000);
        cell.refresh_noting_step(|| Some(slew), |o, w, m| seen.push((o, w, m)));
        assert!(seen.is_empty(), "no step expected, got {seen:?}");
        // A 600 s step back.
        let back = at(90, base_nanos + secs(90) + 5_000_000 - secs(600));
        cell.refresh_noting_step(|| Some(back), |o, w, m| seen.push((o, w, m)));
        assert_eq!(
            seen,
            vec![(normal.nanos as u64, back.nanos as u64, secs(600) as u64)]
        );
        // A 600 s step forward.
        let fwd = at(120, back.nanos + secs(30) + secs(600));
        cell.refresh_noting_step(|| Some(fwd), |o, w, m| seen.push((o, w, m)));
        assert_eq!(
            seen[1],
            (back.nanos as u64, fwd.nanos as u64, secs(600) as u64)
        );
        assert_eq!(seen.len(), 2);
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

    /// A scripted clock: each reading is `(monotonic before, wall, monotonic
    /// after)` as offsets in nanos from `t0` / `wall0`.
    fn scripted(
        t0: Instant,
        wall0: i64,
        script: &[(u64, i64, u64)],
    ) -> impl FnMut() -> AnchorReading + '_ {
        let mut next = 0;
        move || {
            let (before, wall, after) = script[next.min(script.len() - 1)];
            next += 1;
            AnchorReading {
                before: t0 + Duration::from_nanos(before),
                nanos: wall0 + wall,
                after: t0 + Duration::from_nanos(after),
            }
        }
    }

    const SEC: u64 = 1_000_000_000;

    /// 17 Nov 2026 live: the thread descheduled for 1.05 s between the
    /// wall-clock read and the monotonic read made the anchor pair a wall
    /// time with an instant 1.05 s later, and the refresh noted a 1.05 s
    /// "clock step" although the clock never stepped. A reading whose two
    /// monotonic reads are that far apart is retried, and when every retry
    /// is as noisy, the refresh keeps the previous anchor and notes nothing.
    /// Bites: the old single wall-then-monotonic read notes the step.
    #[test]
    fn test_regression_a_descheduled_read_records_no_clock_step() {
        let t0 = Instant::now();
        let wall0 = 1_794_911_430_000_000_000_i64;
        let cell = AnchorCell::new(ReceiptAnchor {
            instant: t0,
            nanos: wall0,
        });
        // 30 s later, the clock has not stepped: wall = monotonic. Each try
        // is descheduled 1.05 s after the wall read.
        let at = 30 * SEC;
        let late = at + 1_050_000_000;
        let noisy = [(at, at as i64, late); 3];
        let mut steps = Vec::new();
        let mut reads = 0;
        let mut clock = scripted(t0, wall0, &noisy);
        let outcome = cell.refresh_noting_step(
            || {
                let (anchor, tight) = best_anchor_reading(|| {
                    reads += 1;
                    clock()
                });
                tight.then_some(anchor)
            },
            |o, w, m| steps.push((o, w, m)),
        );
        assert_eq!(outcome, None, "the noisy reading is not adopted");
        assert_eq!(reads, ANCHOR_READ_ATTEMPTS, "bounded retries");
        assert!(steps.is_empty(), "no step noted: {steps:?}");
        assert_eq!(cell.current().instant, t0, "the previous anchor stays");
        // The old read (wall first, the instant after the deschedule) notes
        // a 1.05 s step.
        let old_read = ReceiptAnchor {
            instant: t0 + Duration::from_nanos(late),
            nanos: wall0 + at as i64,
        };
        let mut old_steps = Vec::new();
        cell.refresh_noting_step(|| Some(old_read), |o, w, m| old_steps.push((o, w, m)));
        assert_eq!(old_steps.len(), 1);
        assert_eq!(old_steps[0].2, 1_050_000_000);
    }

    /// One noisy reading then a tight one: the tight one is adopted after
    /// two reads, and its instant is the second monotonic read.
    #[test]
    fn test_best_anchor_reading_retries_until_tight_and_pairs_the_second_read() {
        let t0 = Instant::now();
        let wall0 = 1_794_911_430_000_000_000_i64;
        let script = [
            (30 * SEC, (30 * SEC) as i64, 31 * SEC),
            (32 * SEC, (32 * SEC) as i64, 32 * SEC + 500),
        ];
        let mut reads = 0;
        let mut clock = scripted(t0, wall0, &script);
        let (anchor, tight) = best_anchor_reading(|| {
            reads += 1;
            clock()
        });
        assert!(tight);
        assert_eq!(reads, 2);
        assert_eq!(anchor.instant, t0 + Duration::from_nanos(32 * SEC + 500));
        assert_eq!(anchor.nanos, wall0 + (32 * SEC) as i64);
        // A tight first reading takes one read.
        let mut one = 0;
        let mut clock = scripted(t0, wall0, &script[1..]);
        let (_, tight) = best_anchor_reading(|| {
            one += 1;
            clock()
        });
        assert!(tight);
        assert_eq!(one, 1);
        // Every reading noisy: the tightest is returned, flagged not tight.
        let noisy = [
            (0, 0, 3 * SEC),
            (10 * SEC, (10 * SEC) as i64, 10 * SEC + 2_000_000),
            (20 * SEC, (20 * SEC) as i64, 25 * SEC),
        ];
        let mut clock = scripted(t0, wall0, &noisy);
        let (anchor, tight) = best_anchor_reading(&mut clock);
        assert!(!tight);
        assert_eq!(anchor.nanos, wall0 + (10 * SEC) as i64);
        // A gap of exactly the bound is tight.
        let edge = [(0, 0, ANCHOR_READ_TIGHT_NANOS)];
        assert!(best_anchor_reading(scripted(t0, wall0, &edge)).1);
    }

    /// A real 2 s wall-clock step shows on a tight reading and is noted,
    /// both ways.
    #[test]
    fn test_a_real_step_on_tight_reads_is_still_recorded() {
        let t0 = Instant::now();
        let wall0 = 1_794_911_430_000_000_000_i64;
        let cell = AnchorCell::new(ReceiptAnchor {
            instant: t0,
            nanos: wall0,
        });
        let mut steps = Vec::new();
        // Forward 2 s at 30 s, read tight (300 ns apart).
        let fwd = [(30 * SEC, (32 * SEC) as i64, 30 * SEC + 300)];
        let tight_from = |script: &[(u64, i64, u64)]| {
            let (anchor, tight) = best_anchor_reading(scripted(t0, wall0, script));
            tight.then_some(anchor)
        };
        let outcome =
            cell.refresh_noting_step(|| tight_from(&fwd), |o, w, m| steps.push((o, w, m)));
        assert_eq!(outcome, Some(AnchorRefresh::Adopt));
        assert_eq!(steps.len(), 1);
        assert!(steps[0].2.abs_diff(2 * SEC) < 1_000, "{steps:?}");
        // Back 2 s at 60 s, read tight.
        let back = [(60 * SEC, (60 * SEC) as i64, 60 * SEC + 300)];
        let outcome =
            cell.refresh_noting_step(|| tight_from(&back), |o, w, m| steps.push((o, w, m)));
        assert!(matches!(
            outcome,
            Some(AnchorRefresh::AdoptBackwardStep { .. })
        ));
        assert_eq!(steps.len(), 2);
        assert!(steps[1].2.abs_diff(2 * SEC) < 1_000, "{steps:?}");
    }
}

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
    /// Serialises refreshes (cold, one every 30 s) and holds their state.
    /// Never taken by a reader.
    writer: std::sync::Mutex<WriterState>,
}

/// What the refresh keeps between calls (2026-11-17 review). Under
/// `AnchorCell::writer`.
#[derive(Debug, Default)]
struct WriterState {
    /// Refreshes in a row that kept the previous anchor on a noisy reading.
    noisy_streak: u32,
    /// How far apart the two monotonic reads of the PUBLISHED anchor were
    /// (`0` for a synthetic anchor). Its wall time can be early by up to this
    /// much, so a later forward jump of up to this much is that error, not a
    /// step.
    read_gap_nanos: u64,
}

/// How far `new` moves the time `old` projects for `new.instant`, in nanos
/// (saturating), and whether it moves it forward. Pure, O(1).
fn anchor_jump(old: ReceiptAnchor, new: ReceiptAnchor) -> (u64, bool) {
    let elapsed = new.instant.saturating_duration_since(old.instant);
    let projected = old
        .nanos
        .saturating_add(i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX));
    (new.nanos.abs_diff(projected), new.nanos >= projected)
}

/// How many refreshes in a row may keep the previous anchor because no
/// reading was tight (2026-11-17 review). The next one adopts its tightest
/// reading anyway, judging a step only past what the two readings' gaps can
/// explain, so a real step under sustained starvation is recorded at most
/// `ANCHOR_NOISY_REFRESHES_KEPT + 1` refreshes (2.5 minutes at 30 s) late
/// instead of never, and a slew correction is not held off for longer.
pub(super) const ANCHOR_NOISY_REFRESHES_KEPT: u32 = 4;

/// `Duration` to `u64` nanos, saturating (584 years).
#[inline]
fn duration_nanos_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

impl AnchorCell {
    /// A cell from a synthetic anchor (no read gap). Tests only.
    #[cfg(test)]
    pub(super) fn new(first: ReceiptAnchor) -> Self {
        Self::with_read_gap(first, 0)
    }

    /// A cell from the first reading and how far apart its two monotonic
    /// reads were (2026-11-17 review: a noisy first anchor is remembered, so
    /// the first refresh does not read its error as a clock step).
    pub(super) fn with_read_gap(first: ReceiptAnchor, read_gap_nanos: u64) -> Self {
        Self {
            base: first.instant,
            active: AtomicUsize::new(0),
            slots: [
                AnchorSlot::new(0, first.nanos),
                AnchorSlot::new(0, first.nanos),
            ],
            writer: std::sync::Mutex::new(WriterState {
                noisy_streak: 0,
                read_gap_nanos,
            }),
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
        self.refresh_noting_step(|| (new_anchor(), 0), |_, _, _| {})
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
    /// 2026-11-17: `new_anchor` returns a reading and how far apart its two
    /// monotonic reads were ([`best_anchor_reading`]). A reading wider than
    /// [`ANCHOR_READ_TIGHT_NANOS`] leaves the cell as it is, notes nothing
    /// and returns `None`, for at most [`ANCHOR_NOISY_REFRESHES_KEPT`]
    /// refreshes in a row; the next is adopted as any other.
    ///
    /// ⚠ CHANGED 2026-11-17 (review): a step is judged only past what the
    /// read gaps can explain. An anchor's wall time can be early by up to its
    /// gap, so a FORWARD jump is excused by the old anchor's gap (a noisy
    /// first anchor, or one adopted after the streak) and a BACKWARD jump by
    /// the new one's. The full jump is what is noted. Honest limit: a real
    /// step no larger than that gap, met on such a refresh, is not recorded;
    /// receipts still follow the adopted anchor.
    pub(super) fn refresh_noting_step(
        &self,
        new_anchor: impl FnOnce() -> (ReceiptAnchor, u64),
        note_step: impl FnOnce(u64, u64, u64),
    ) -> Option<AnchorRefresh> {
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (new, gap) = new_anchor();
        if gap > ANCHOR_READ_TIGHT_NANOS && writer.noisy_streak < ANCHOR_NOISY_REFRESHES_KEPT {
            writer.noisy_streak += 1;
            return None;
        }
        writer.noisy_streak = 0;
        let old = self.current();
        // A rewind the new reading's own gap can explain is not taken as a
        // backward step (it would move receipts back for nothing); it is
        // refused like slew. Matters only for a reading adopted after the
        // noisy streak: a tight gap is at most 1 ms.
        let decision = match anchor_refresh_decision(old, new) {
            AnchorRefresh::AdoptBackwardStep { rewind_nanos }
                if u64::try_from(rewind_nanos).unwrap_or(0).saturating_sub(gap)
                    <= super::RECEIPT_ANCHOR_BACKWARD_STEP_NANOS.unsigned_abs() =>
            {
                AnchorRefresh::RefuseBackward
            }
            decision => decision,
        };
        if decision != AnchorRefresh::RefuseBackward {
            self.publish(new);
            let (jump, forward) = anchor_jump(old, new);
            let explained = if forward { writer.read_gap_nanos } else { gap };
            writer.read_gap_nanos = gap;
            if jump.saturating_sub(explained) > crate::wal_clock_step::CLOCK_STEP_MIN_NANOS {
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
/// tightest reading's anchor and its gap (tight when the gap is at most
/// [`ANCHOR_READ_TIGHT_NANOS`]). A real wall-clock step shows on a tight
/// reading exactly as on any other, so this never hides a step; it only
/// refuses a pair of reads a deschedule pulled apart.
/// O(attempts) = O(3), no allocation, no lock.
pub(super) fn best_anchor_reading(mut read: impl FnMut() -> AnchorReading) -> (ReceiptAnchor, u64) {
    let mut best = read();
    let mut taken = 1;
    // O(1) EXEMPT: bounded by ANCHOR_READ_ATTEMPTS (3)
    loop {
        if best.gap_nanos() <= ANCHOR_READ_TIGHT_NANOS || taken >= ANCHOR_READ_ATTEMPTS {
            return (best.anchor(), best.gap_nanos());
        }
        let next = read();
        taken += 1;
        if next.gap_nanos() < best.gap_nanos() {
            best = next;
        }
    }
}

/// The best of up to three readings from `read`, counting one that is not
/// tight on `RECEIPT_ANCHOR_NOISY_READ_COUNTER` (2026-11-17 review: the first
/// anchor and every refresh go through here, so a noisy first anchor is
/// visible too). Production passes `read_anchor_clocks`. O(3).
pub(super) fn take_anchor_from(read: impl FnMut() -> AnchorReading) -> (ReceiptAnchor, u64) {
    let (anchor, gap) = best_anchor_reading(read);
    if gap > ANCHOR_READ_TIGHT_NANOS {
        metrics::counter!(super::RECEIPT_ANCHOR_NOISY_READ_COUNTER).increment(1);
    }
    (anchor, gap)
}

/// One refresh of `cell` from `read` (2026-11-17 review: the production
/// refresh is this call on the process cell with `read_anchor_clocks` and
/// `note_clock_step`, so the tests drive the same path with a scripted
/// clock). O(3) clock reads plus [`AnchorCell::refresh_noting_step`].
pub(super) fn refresh_cell(
    cell: &AnchorCell,
    read: impl FnMut() -> AnchorReading,
    note_step: impl FnOnce(u64, u64, u64),
) -> Option<AnchorRefresh> {
    cell.refresh_noting_step(|| take_anchor_from(read), note_step)
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
        cell.refresh_noting_step(|| (normal, 0), |o, w, m| seen.push((o, w, m)));
        // Slew-sized rewind: refused, no step.
        let slew = at(60, base_nanos + secs(60) - 10_000_000);
        cell.refresh_noting_step(|| (slew, 0), |o, w, m| seen.push((o, w, m)));
        assert!(seen.is_empty(), "no step expected, got {seen:?}");
        // A 600 s step back.
        let back = at(90, base_nanos + secs(90) + 5_000_000 - secs(600));
        cell.refresh_noting_step(|| (back, 0), |o, w, m| seen.push((o, w, m)));
        assert_eq!(
            seen,
            vec![(normal.nanos as u64, back.nanos as u64, secs(600) as u64)]
        );
        // A 600 s step forward.
        let fwd = at(120, back.nanos + secs(30) + secs(600));
        cell.refresh_noting_step(|| (fwd, 0), |o, w, m| seen.push((o, w, m)));
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
    /// Drives `refresh_cell`, the production refresh path, with a scripted
    /// clock. Bites: the old single wall-then-monotonic read notes the step.
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
        let outcome = refresh_cell(
            &cell,
            || {
                reads += 1;
                clock()
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
        cell.refresh_noting_step(|| (old_read, 0), |o, w, m| old_steps.push((o, w, m)));
        assert_eq!(old_steps.len(), 1);
        assert_eq!(old_steps[0].2, 1_050_000_000);
    }

    /// 2026-11-17 review: the FIRST anchor was taken from a noisy reading
    /// (descheduled 1.05 s after the wall read, the restart case). The first
    /// tight refresh sees a 1.05 s forward jump, which the first reading's
    /// gap explains, so no step is noted; a real 2 s forward step on top of
    /// it still is. Bites: the same first anchor with no recorded gap notes
    /// the false step.
    #[test]
    fn test_regression_a_noisy_first_anchor_records_no_step_at_the_first_refresh() {
        let t0 = Instant::now();
        let wall0 = 1_794_911_430_000_000_000_i64;
        let noisy_first = [(0, 0, 1_050_000_000)];
        let first_cell = || {
            let (first, gap) = take_anchor_from(scripted(t0, wall0, &noisy_first));
            assert_eq!(gap, 1_050_000_000);
            AnchorCell::with_read_gap(first, gap)
        };
        let tight = [(30 * SEC, (30 * SEC) as i64, 30 * SEC + 300)];
        let cell = first_cell();
        let mut steps = Vec::new();
        let outcome = refresh_cell(&cell, scripted(t0, wall0, &tight), |o, w, m| {
            steps.push((o, w, m));
        });
        assert_eq!(outcome, Some(AnchorRefresh::Adopt));
        assert!(steps.is_empty(), "no false step: {steps:?}");
        assert_eq!(cell.current().nanos, wall0 + (30 * SEC) as i64);
        // A real 2 s forward step at the same refresh is still noted.
        let stepped = [(30 * SEC, (32 * SEC) as i64, 30 * SEC + 300)];
        let cell = first_cell();
        refresh_cell(&cell, scripted(t0, wall0, &stepped), |o, w, m| {
            steps.push((o, w, m));
        });
        assert_eq!(steps.len(), 1, "{steps:?}");
        // Bite: without the remembered gap the same refresh notes 1.05 s.
        let (first, _) = take_anchor_from(scripted(t0, wall0, &noisy_first));
        let unaware = AnchorCell::new(first);
        let mut bite = Vec::new();
        refresh_cell(&unaware, scripted(t0, wall0, &tight), |o, w, m| {
            bite.push((o, w, m));
        });
        assert_eq!(bite.len(), 1);
    }

    /// 2026-11-17 review: under sustained starvation a refresh cannot keep
    /// the old anchor for ever. Four noisy refreshes in a row are kept; the
    /// fifth is adopted, a real step it shows past its gap is noted, and a
    /// rewind its gap explains is refused like slew (receipts never moved
    /// back for noise).
    #[test]
    fn test_a_noisy_streak_ends_after_the_kept_refreshes() {
        let t0 = Instant::now();
        let wall0 = 1_794_911_430_000_000_000_i64;
        let desched = 1_050_000_000;
        let noisy_at = |k: u64, step: u64| {
            [(
                k * 30 * SEC,
                (k * 30 * SEC + step) as i64,
                k * 30 * SEC + desched,
            ); 3]
        };
        // A real 3 s forward step at the fifth refresh.
        let cell = AnchorCell::new(ReceiptAnchor {
            instant: t0,
            nanos: wall0,
        });
        let mut steps = Vec::new();
        for k in 1..=u64::from(ANCHOR_NOISY_REFRESHES_KEPT) {
            let script = noisy_at(k, 0);
            let outcome = refresh_cell(&cell, scripted(t0, wall0, &script), |o, w, m| {
                steps.push((o, w, m));
            });
            assert_eq!(outcome, None, "refresh {k} kept");
        }
        assert_eq!(cell.current().instant, t0);
        let k = u64::from(ANCHOR_NOISY_REFRESHES_KEPT) + 1;
        let script = noisy_at(k, 3 * SEC);
        let outcome = refresh_cell(&cell, scripted(t0, wall0, &script), |o, w, m| {
            steps.push((o, w, m));
        });
        assert_eq!(outcome, Some(AnchorRefresh::Adopt));
        assert_eq!(steps.len(), 1, "the real step is noted: {steps:?}");
        // No step at the fifth: the apparent 1.05 s rewind is the gap.
        let cell = AnchorCell::new(ReceiptAnchor {
            instant: t0,
            nanos: wall0,
        });
        let mut quiet = Vec::new();
        for k in 1..=u64::from(ANCHOR_NOISY_REFRESHES_KEPT) {
            let script = noisy_at(k, 0);
            refresh_cell(&cell, scripted(t0, wall0, &script), |o, w, m| {
                quiet.push((o, w, m));
            });
        }
        let script = noisy_at(k, 0);
        let outcome = refresh_cell(&cell, scripted(t0, wall0, &script), |o, w, m| {
            quiet.push((o, w, m));
        });
        assert_eq!(outcome, Some(AnchorRefresh::RefuseBackward));
        assert!(quiet.is_empty(), "{quiet:?}");
        assert_eq!(cell.current().instant, t0, "receipts not moved back");
        // The streak started over: the next noisy refresh is kept again.
        let script = noisy_at(k + 1, 0);
        assert_eq!(
            refresh_cell(&cell, scripted(t0, wall0, &script), |_, _, _| {}),
            None
        );
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
        let (anchor, gap) = best_anchor_reading(|| {
            reads += 1;
            clock()
        });
        assert_eq!(gap, 500);
        assert_eq!(reads, 2);
        assert_eq!(anchor.instant, t0 + Duration::from_nanos(32 * SEC + 500));
        assert_eq!(anchor.nanos, wall0 + (32 * SEC) as i64);
        // A tight first reading takes one read.
        let mut one = 0;
        let mut clock = scripted(t0, wall0, &script[1..]);
        let (_, gap) = best_anchor_reading(|| {
            one += 1;
            clock()
        });
        assert!(gap <= ANCHOR_READ_TIGHT_NANOS);
        assert_eq!(one, 1);
        // Every reading noisy: the tightest is returned with its gap.
        let noisy = [
            (0, 0, 3 * SEC),
            (10 * SEC, (10 * SEC) as i64, 10 * SEC + 2_000_000),
            (20 * SEC, (20 * SEC) as i64, 25 * SEC),
        ];
        let mut clock = scripted(t0, wall0, &noisy);
        let (anchor, gap) = best_anchor_reading(&mut clock);
        assert_eq!(gap, 2_000_000);
        assert_eq!(anchor.nanos, wall0 + (10 * SEC) as i64);
        // A gap of exactly the bound is tight.
        let edge = [(0, 0, ANCHOR_READ_TIGHT_NANOS)];
        assert!(best_anchor_reading(scripted(t0, wall0, &edge)).1 <= ANCHOR_READ_TIGHT_NANOS);
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
        let outcome = refresh_cell(&cell, scripted(t0, wall0, &fwd), |o, w, m| {
            steps.push((o, w, m));
        });
        assert_eq!(outcome, Some(AnchorRefresh::Adopt));
        assert_eq!(steps.len(), 1);
        assert!(steps[0].2.abs_diff(2 * SEC) < 1_000, "{steps:?}");
        // Back 2 s at 60 s, read tight.
        let back = [(60 * SEC, (60 * SEC) as i64, 60 * SEC + 300)];
        let outcome = refresh_cell(&cell, scripted(t0, wall0, &back), |o, w, m| {
            steps.push((o, w, m));
        });
        assert!(matches!(
            outcome,
            Some(AnchorRefresh::AdoptBackwardStep { .. })
        ));
        assert_eq!(steps.len(), 2);
        assert!(steps[1].2.abs_diff(2 * SEC) < 1_000, "{steps:?}");
    }

    /// The body of `fn <name>` in `src`, up to the first line that is a
    /// lone closing brace at column 0.
    fn fn_body<'a>(src: &'a str, signature: &str) -> &'a str {
        let at = src
            .find(signature)
            .unwrap_or_else(|| panic!("{signature} not found"));
        let rest = &src[at..];
        let end = rest.find("\n}\n").expect("function end");
        &rest[..end]
    }

    /// 2026-11-17 review: the production wiring the scripted tests cannot
    /// reach. The clocks are read monotonic, wall, monotonic; the first
    /// anchor and every refresh go through `take_anchor_from` /
    /// `refresh_cell` with that reader, and the refresh notes steps on the
    /// process log. Reordering the reads, or bypassing the tight-read path,
    /// fails here.
    #[test]
    fn test_production_anchor_wiring_reads_mono_wall_mono_through_the_tight_path() {
        let src = include_str!("../ws_frame_spill.rs");
        let squeeze = |s: &str| s.split_whitespace().collect::<String>();
        let read = fn_body(src, "fn read_anchor_clocks()");
        let first_mono = read.find("Instant::now()").expect("first monotonic read");
        let wall = read.find("SystemTime::now()").expect("wall read");
        let second_mono = read.rfind("Instant::now()").expect("second monotonic read");
        assert!(
            first_mono < wall && wall < second_mono,
            "read_anchor_clocks must read monotonic, wall, monotonic"
        );
        assert_eq!(read.matches("Instant::now()").count(), 2);
        let cell = squeeze(fn_body(src, "fn anchor_cell()"));
        assert!(
            cell.contains("receipt_anchor::take_anchor_from(read_anchor_clocks)")
                && cell.contains("AnchorCell::with_read_gap(first,read_gap)"),
            "the first anchor keeps its read gap: {cell}"
        );
        let refresh = squeeze(fn_body(src, "pub fn refresh_receipt_anchor()"));
        assert!(
            refresh.contains(
                "receipt_anchor::refresh_cell(cell,read_anchor_clocks,crate::wal_clock_step::note_clock_step"
            ),
            "the refresh goes through refresh_cell with the real reader: {refresh}"
        );
        assert!(!refresh.contains("refresh_noting_step("));
    }
}

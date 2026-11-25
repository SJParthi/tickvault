//! 2026-11-16: the `ahash` map (plan ITEM DT shape) decides exactly as the
//! SipHash map it replaced. `SipHashFilter` below is a frozen copy of the
//! pre-2026-11-16 decision code (`judge`, `note_written`, `mark_gap`,
//! `clear`) over the standard `HashMap`, kept only as the oracle. Both filters
//! are fed the same random sequence and must return the same verdict, with
//! the same noted values, at every step. The value compare is the exact
//! 13-column compare in both (Quote 61: never a fingerprint).

use std::collections::HashMap;

use proptest::prelude::*;

use super::*;
use crate::feed_aux_persistence::AuxPacketKind;

/// 2026-11-11 10:00:00 IST as IST nanoseconds.
const DAY_10AM: i64 = (20_768 * 86_400 + 10 * 3_600) * 1_000_000_000;

/// The pre-2026-11-16 filter, verbatim apart from the counters (plain
/// integers here, so the oracle needs no metrics recorder).
struct SipHashFilter {
    last: HashMap<(u64, u8), AuxRowValues>,
    day: i64,
    gap_epoch: fn() -> u64,
    local_gaps: u64,
    skipped: u64,
    untracked: u64,
}

impl SipHashFilter {
    fn with_gap_epoch(gap_epoch: fn() -> u64) -> Self {
        Self {
            last: HashMap::with_capacity(FEED_AUX_REPEAT_MAX_TRACKED),
            day: i64::MIN,
            gap_epoch,
            local_gaps: 0,
            skipped: 0,
            untracked: 0,
        }
    }

    fn judge(
        &mut self,
        key: (u64, u8),
        row: &AuxPacketRow,
        write_always: bool,
        pin: bool,
    ) -> AuxRepeatVerdict {
        let day = row.ts_ist_nanos.div_euclid(NANOS_PER_DAY);
        if day != self.day {
            self.last.clear();
            self.day = day;
        }
        let epoch = (self.gap_epoch)().wrapping_add(self.local_gaps);
        let mut values = AuxRowValues::of(row, epoch);
        match self.last.get(&key) {
            Some(prev) if prev.never_skip => values.never_skip = true,
            Some(prev) if !write_always && *prev == values => {
                self.skipped += 1;
                return AuxRepeatVerdict::Skip;
            }
            _ => values.never_skip = pin,
        }
        AuxRepeatVerdict::Write(values)
    }

    fn note_written(&mut self, key: (u64, u8), values: AuxRowValues) {
        if let Some(slot) = self.last.get_mut(&key) {
            *slot = values;
        } else if self.last.len() < FEED_AUX_REPEAT_MAX_TRACKED {
            self.last.insert(key, values);
        } else {
            self.untracked += 1;
        }
    }

    fn mark_gap(&mut self) {
        self.local_gaps = self.local_gaps.wrapping_add(1);
    }

    fn clear(&mut self) {
        self.last.clear();
        self.day = i64::MIN;
    }
}

std::thread_local! {
    /// A per-test-thread gap epoch both filters read.
    static EPOCH: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn thread_epoch() -> u64 {
    EPOCH.with(std::cell::Cell::get)
}

/// One step of the random sequence.
#[derive(Debug, Clone)]
enum Op {
    /// Judge a row: `mode` 0 = `check`, 1 = `check_never_skip`,
    /// 2 = `check_always_write`; `note` = stage it (call `note_written` on a
    /// `Write`), as the drain does unless staging fails.
    Judge {
        key: (u64, u8),
        value: u8,
        kind: u8,
        mode: u8,
        note: bool,
        day: i64,
        at: i64,
    },
    MarkGap,
    Clear,
    MoveEpoch,
}

/// A row from a small value space, so exact repeats are common. Bit 7 of
/// `value` makes `oi` NULL (never equal to a zero `oi`).
fn row_of(key: (u64, u8), value: u8, kind: u8, ts: i64) -> AuxPacketRow {
    let kind = match kind % 3 {
        0 => AuxPacketKind::NoTradeToday,
        1 => AuxPacketKind::ConnectSnapshot,
        _ => AuxPacketKind::OpenInterest,
    };
    let mut r = AuxPacketRow::from_header(kind, key.0, key.1, 0, 1).expect("fits");
    r.ts_ist_nanos = ts;
    let v = value & 0x3;
    r.ltp = Some([140.0, 141.0, 142.0, 143.0][usize::from(v)]);
    r.exchange_ts_ist_nanos = Some(1_000 + i64::from(value >> 2 & 0x1));
    r.oi = if value & 0x80 == 0 {
        Some(i64::from(value >> 3 & 0x1))
    } else {
        None
    };
    r.day_close = Some(if value & 0x40 == 0 { 0.0 } else { -0.0 });
    r
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        8 => (
            (0u64..6, 1u8..3),
            any::<u8>(),
            0u8..3,
            0u8..3,
            prop::bool::weighted(0.9),
            0i64..2,
            0i64..1_000,
        )
            .prop_map(|(key, value, kind, mode, note, day, at)| Op::Judge {
                key,
                value,
                kind,
                mode,
                note,
                day,
                at,
            }),
        1 => Just(Op::MarkGap),
        1 => Just(Op::Clear),
        1 => Just(Op::MoveEpoch),
    ]
}

/// Runs one op on both filters and asserts the same verdict.
fn step(new: &mut AuxRepeatFilter, old: &mut SipHashFilter, op: &Op) {
    match *op {
        Op::Judge {
            key,
            value,
            kind,
            mode,
            note,
            day,
            at,
        } => {
            let row = row_of(key, value, kind, DAY_10AM + day * NANOS_PER_DAY + at);
            let (got, want) = match mode {
                0 => (new.check(key, &row), old.judge(key, &row, false, false)),
                1 => (
                    new.check_never_skip(key, &row),
                    old.judge(key, &row, true, true),
                ),
                _ => (
                    new.check_always_write(key, &row),
                    old.judge(key, &row, true, false),
                ),
            };
            assert_eq!(got, want, "{op:?}");
            if let (true, AuxRepeatVerdict::Write(v)) = (note, got) {
                new.note_written(key, v);
                old.note_written(key, v);
            }
        }
        Op::MarkGap => {
            new.mark_gap();
            old.mark_gap();
        }
        Op::Clear => {
            new.clear();
            old.clear();
        }
        Op::MoveEpoch => EPOCH.with(|e| e.set(e.get().wrapping_add(1))),
    }
    assert_eq!(new.last.len(), old.last.len());
    assert_eq!(new.day, old.day);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Random packet sequences: every verdict and every noted value of the
    /// `ahash` filter equals the frozen SipHash filter's.
    #[test]
    fn proptest_ahash_filter_decides_exactly_as_the_siphash_filter(
        ops in prop::collection::vec(op_strategy(), 1..200),
    ) {
        EPOCH.with(|e| e.set(0));
        let mut new = AuxRepeatFilter::with_gap_epoch(thread_epoch);
        let mut old = SipHashFilter::with_gap_epoch(thread_epoch);
        for op in &ops {
            step(&mut new, &mut old, op);
        }
        // Every remembered key holds the same values in both maps.
        for (key, values) in &old.last {
            prop_assert_eq!(new.last.get(key), Some(values));
        }
    }
}

/// The cap path in both: 25,000 keys tracked, the next new key written and
/// not remembered, a tracked key still updated in place.
#[test]
fn test_ahash_filter_matches_the_siphash_filter_at_the_cap() {
    let mut new = AuxRepeatFilter::with_gap_epoch(thread_epoch);
    let mut old = SipHashFilter::with_gap_epoch(thread_epoch);
    EPOCH.with(|e| e.set(0));
    for id in 0..=FEED_AUX_REPEAT_MAX_TRACKED as u64 + 2 {
        for at in 0..2 {
            step(
                &mut new,
                &mut old,
                &Op::Judge {
                    key: (id, 2),
                    value: 0,
                    kind: 0,
                    mode: 0,
                    note: true,
                    day: 0,
                    at,
                },
            );
        }
    }
    assert_eq!(new.last.len(), FEED_AUX_REPEAT_MAX_TRACKED);
    assert_eq!(old.untracked, 6, "3 untracked keys, 2 rows each");
    assert_eq!(old.skipped, FEED_AUX_REPEAT_MAX_TRACKED as u64);
}

/// The map's hasher is `ahash` (2026-11-16). A revert to the std hasher no
/// longer compiles here.
#[test]
fn test_repeat_filter_map_hashes_with_ahash() {
    let f = AuxRepeatFilter::with_gap_epoch(thread_epoch);
    let _hasher: &ahash::RandomState = f.last.hasher();
    assert!(f.last.capacity() >= FEED_AUX_REPEAT_MAX_TRACKED);
}

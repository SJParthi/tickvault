//! Complete integer-key ranking: lots descending, id ascending, segment ascending.
//!
//! Above a fixed small-input cutoff, in-place MSD radix partitioning skips
//! common key prefixes. Fixed 17-byte keys bound recursion depth to 17; work is
//! linear in rows for fixed byte width and cutoff, not O(1). Each radix level
//! uses three 256-entry usize arrays; at most 17 levels can be live. No heap
//! allocation or payload truncation. Small groups use comparison sorting.
//! The initial LSD experiment regressed on AWS. Keep the small-board cutoff
//! separate from recursive groups so tuning tiny inputs does not change the
//! measured large-board partition path. Validate changes on target hardware.

use std::cmp::Ordering;

use super::RankedContract;

const COMPARISON_CUTOFF: usize = 64;
const SMALL_BOARD_CUTOFF: usize = 128;

pub(super) fn compare(a: &RankedContract, b: &RankedContract) -> Ordering {
    b.window_lots_milli
        .cmp(&a.window_lots_milli)
        .then_with(|| a.security_id.cmp(&b.security_id))
        .then_with(|| (a.segment as u8).cmp(&(b.segment as u8)))
}

// Most significant varying byte: lots first, then id, then segment. Higher
// bytes inside a recursive bucket are already identical. XOR detects variation
// independently of the complemented representation used for descending lots.
fn first_varying_pass(rows: &[RankedContract]) -> Option<usize> {
    let first = rows[0];
    let mut lots = 0u64;
    let mut ids = 0u64;
    let mut segments = 0u8;
    for row in rows {
        lots |= row.window_lots_milli ^ first.window_lots_milli;
        ids |= row.security_id ^ first.security_id;
        segments |= (row.segment as u8) ^ (first.segment as u8);
    }
    if lots != 0 {
        Some((lots.leading_zeros() / 8) as usize)
    } else if ids != 0 {
        Some(8 + (ids.leading_zeros() / 8) as usize)
    } else if segments != 0 {
        Some(16)
    } else {
        None
    }
}

// American-flag partitioning. Every iteration advances a bucket cursor, so
// there are at most rows.len() iterations; complete payloads move via swap.
// Each invocation's three 256-entry arrays are bounded stack storage.
fn partition<F>(rows: &mut [RankedContract], pass: usize, digit: F)
where
    F: Fn(&RankedContract) -> usize,
{
    let mut ends = [0usize; 256];
    for row in rows.iter() {
        ends[digit(row)] += 1;
    }
    let mut starts = [0usize; 256];
    let mut offset = 0usize;
    for (bucket, end) in ends.iter_mut().enumerate() {
        starts[bucket] = offset;
        offset += *end;
        *end = offset;
    }
    let mut next = starts;
    for bucket in 0..256 {
        while next[bucket] < ends[bucket] {
            let target = digit(&rows[next[bucket]]);
            if target == bucket {
                next[bucket] += 1;
            } else {
                rows.swap(next[bucket], next[target]);
                next[target] += 1;
            }
        }
    }
    if pass < 16 {
        for bucket in 0..256 {
            let group = &mut rows[starts[bucket]..ends[bucket]];
            if group.len() > 1 {
                msd(group, pass + 1);
            }
        }
    }
}

fn msd(rows: &mut [RankedContract], first_allowed_pass: usize) {
    if rows.len() <= COMPARISON_CUTOFF {
        rows.sort_unstable_by(compare);
        return;
    }
    let Some(pass) = first_varying_pass(rows) else {
        return;
    };
    debug_assert!(pass >= first_allowed_pass);
    match pass {
        0..=7 => {
            let shift = (7 - pass) * 8;
            partition(rows, pass, |row| {
                ((!row.window_lots_milli >> shift) & 255) as usize
            });
        }
        8..=15 => {
            let shift = (15 - pass) * 8;
            partition(rows, pass, |row| {
                ((row.security_id >> shift) & 255) as usize
            });
        }
        _ => partition(rows, pass, |row| usize::from(row.segment as u8)),
    }
}

pub(super) fn sort(rows: &mut [RankedContract]) {
    if rows.len() <= SMALL_BOARD_CUTOFF {
        rows.sort_unstable_by(compare);
    } else {
        msd(rows, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::types::ExchangeSegment;

    const SEGMENTS: [ExchangeSegment; 8] = [
        ExchangeSegment::IdxI,
        ExchangeSegment::NseEquity,
        ExchangeSegment::NseFno,
        ExchangeSegment::NseCurrency,
        ExchangeSegment::BseEquity,
        ExchangeSegment::McxComm,
        ExchangeSegment::BseCurrency,
        ExchangeSegment::BseFno,
    ];

    fn row(id: u64, lots: u64, segment: usize) -> RankedContract {
        RankedContract {
            security_id: id,
            segment: SEGMENTS[segment % SEGMENTS.len()],
            underlying_id: id.rotate_left(19),
            volume: 41,
            window_lots_milli: lots,
            delta_units: 29,
            lot_size: 7,
        }
    }

    fn oracle(rows: &mut [RankedContract]) {
        // Independent tuple formulation rather than the candidate comparator.
        rows.sort_unstable_by_key(|r| {
            (
                std::cmp::Reverse(r.window_lots_milli),
                r.security_id,
                r.segment as u8,
            )
        });
    }

    fn assert_matches_oracle(input: Vec<RankedContract>) {
        let mut expected = input.clone();
        oracle(&mut expected);
        let mut actual = input;
        sort(&mut actual);
        assert_eq!(actual, expected);
    }

    #[test]
    fn full_width_random_keys_match_independent_comparison_oracle() {
        let mut state = 0x672a_932b_f18c_d409u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for size in [0, 1, 2, 63, 64, 65, 127, 128, 129, 257, 2_000, 25_000] {
            assert_matches_oracle((0..size).map(|i| row(next(), next(), i)).collect());
        }
    }

    #[test]
    fn byte_boundaries_high_bits_and_all_segment_ties_are_preserved() {
        let mut values = vec![0, 1, u64::MAX - 1, u64::MAX];
        for bit in (8..64).step_by(8) {
            values.extend([(1u64 << bit) - 1, 1u64 << bit, (1u64 << bit) + 1]);
        }
        let mut rows = Vec::new();
        for &id in &values {
            for &lots in &values {
                for segment in 0..SEGMENTS.len() {
                    rows.push(row(id, lots, segment));
                }
            }
        }
        assert_matches_oracle(rows);
        assert_matches_oracle((0..1024).map(|i| row(42, 42, i)).collect());
    }

    #[test]
    fn sorted_reverse_and_uniform_keys_keep_complete_rows() {
        let mut rows: Vec<_> = (0..2048).map(|i| row(i, i % 17, i as usize)).collect();
        oracle(&mut rows);
        assert_matches_oracle(rows.clone());
        rows.reverse();
        assert_matches_oracle(rows);
        assert_matches_oracle(vec![row(u64::MAX, 0, 7); 2048]);
    }

    #[test]
    fn equal_sort_keys_do_not_drop_or_overwrite_distinct_payloads() {
        let mut rows: Vec<_> = (0..1024)
            .map(|i| {
                let mut value = row(42, (i % 2) as u64, 2);
                value.underlying_id = i as u64;
                value.delta_units = i;
                value
            })
            .collect();
        sort(&mut rows);
        assert!(
            rows.windows(2)
                .all(|pair| { pair[0].window_lots_milli >= pair[1].window_lots_milli })
        );
        rows.sort_unstable_by_key(|r| r.underlying_id);
        for (i, value) in rows.iter().enumerate() {
            assert_eq!(value.underlying_id, i as u64);
            assert_eq!(value.delta_units as usize, i);
            assert_eq!(value.window_lots_milli, (i % 2) as u64);
        }
    }

    #[test]
    fn hundred_thousand_low_entropy_keys_and_long_common_prefixes_match_oracle() {
        for shape in 0..3 {
            assert_matches_oracle(
                (0..100_000usize)
                    .map(|i| {
                        let reversed = (100_000 - i) as u64;
                        match shape {
                            // Only the final lots byte varies; ids have long prefixes.
                            0 => row(u64::MAX - reversed, u64::MAX - (reversed % 4), i),
                            // Identical lots; id variation is confined to low bits.
                            1 => row(0xffff_ffff_ffff_ff00 | (reversed % 256), u64::MAX, i),
                            // All lots/id bytes identical; only segment distinguishes.
                            _ => row(u64::MAX, u64::MAX, i),
                        }
                    })
                    .collect(),
            );
        }
    }

    #[test]
    fn repeated_population_changes_reuse_the_original_row_allocation() {
        let capacity = 2048;
        let mut rows = Vec::with_capacity(capacity);
        let original = rows.as_ptr();
        for size in [2048, 65, 0, 1, 64, 1024, 128, 2048] {
            rows.clear();
            rows.extend((0..size).map(|i| row((size - i) as u64, (i % 3) as u64, i)));
            let mut expected = rows.clone();
            oracle(&mut expected);
            sort(&mut rows);
            assert_eq!(rows, expected);
            assert_eq!(original, rows.as_ptr());
            assert_eq!(rows.capacity(), capacity);
        }
    }
}

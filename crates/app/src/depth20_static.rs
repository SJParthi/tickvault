//! The depth-20 static day set — fixed at the open and held until the close.
//!
//! # The operator's design (2026-09-24)
//!
//! > "pick entire fno undelryign stocks as the fixed once at 9 am itslef tis
//! > finailised til leod right dude meanwhiel for nifty and abnknfity aloen
//! > once rpe market is finalsied alone only then we ened to makr up either
//! > atm plus or minus 3 or 4 right dude til leod roght to fill up the entrie
//! > 250 slots right so that our depth 20 entire day shdou leb completely
//! > fixed static"
//!
//! | Part | What | When | Slots |
//! |---|---|---|---|
//! | Spots | every F&O underlying's NSE cash equity | the 09:00 attach | ~208 |
//! | Index legs | NIFTY and BANKNIFTY ATM ±k, call and put | once the pre-open price is final (09:12) | 4(2k+1) |
//!
//! `k` is the largest value up to [`DEPTH20_INDEX_MAX_EACH_SIDE`] that keeps
//! the whole set inside the 250-slot budget. With 208 spots that is ±4 and
//! 244 instruments.
//!
//! # Why the set is static
//!
//! Dhan has not confirmed that its depth unsubscribe works, and two live
//! sessions measured it being ignored. A set that never changes sends no
//! unsubscribe at all, so it cannot leave a ghost stream behind.
//!
//! # This module is pure except for [`read_fno_spot_instruments`]
//!
//! That one reads the day's F&O underlying artifact from disk. Everything else
//! takes slices and returns instruments. Each function is O(n) in its input and
//! runs at most a few times a session, on the attach task. It is never on the
//! tick path.

use std::collections::HashSet;

use tickvault_common::types::ExchangeSegment;
use tickvault_core::websocket::pool_supervisor::SubscribeInstrument;

use crate::depth20_layout::{
    DEPTH_20_INDEX_UNDERLYINGS, DEPTH_20_PER_SOCKET, DEPTH_20_SOCKETS, index_window,
};
use crate::dhan_depth_universe::DepthCandidate;
use crate::dhan_live_universe::MasterEntry;

/// The widest index window the operator asked for: "atm plus or minus 3 or 4".
pub const DEPTH20_INDEX_MAX_EACH_SIDE: usize = 4;

/// Every depth-20 slot across the five sockets: 5 × 50 = 250.
pub const DEPTH20_STATIC_BUDGET: usize = DEPTH_20_SOCKETS * DEPTH_20_PER_SOCKET;

/// Legs one strike contributes: a call and a put.
const LEGS_PER_STRIKE: usize = 2;

const _: () = assert!(DEPTH20_STATIC_BUDGET == 250);

/// Instruments the two index windows cost at `each_side`.
#[must_use]
pub const fn index_legs_cost(each_side: usize) -> usize {
    DEPTH_20_INDEX_UNDERLYINGS.len() * LEGS_PER_STRIKE * (2 * each_side + 1)
}

/// The F&O underlying spots, taken from the day's F&O artifact.
///
/// Keeps NSE cash equities only. The artifact also lists the indices, which
/// have no order book and so no depth. Duplicates are dropped on the
/// `(security_id, segment)` key (I-P1-11). The order is the artifact's own, so
/// the same file always gives the same set.
#[must_use]
pub fn fno_spot_instruments(entries: &[MasterEntry]) -> Vec<SubscribeInstrument> {
    let mut seen: HashSet<(u64, ExchangeSegment)> = HashSet::with_capacity(entries.len());
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        if ExchangeSegment::from_byte(entry.exchange_segment_code)
            != Some(ExchangeSegment::NseEquity)
        {
            continue;
        }
        if entry.security_id == 0 {
            continue;
        }
        if seen.insert((entry.security_id, ExchangeSegment::NseEquity)) {
            out.push(SubscribeInstrument {
                security_id: entry.security_id,
                segment: ExchangeSegment::NseEquity,
            });
        }
    }
    out
}

/// Reads today's F&O underlying artifact and returns its NSE cash spots.
///
/// # Errors
/// Returns `Err` naming the path when the file is missing, unreadable or not a
/// mapping artifact. The caller logs it and dials the index legs alone.
pub fn read_fno_spot_instruments(date_ist: &str) -> Result<Vec<SubscribeInstrument>, String> {
    let path = crate::dhan_universe::fno_underlying_artifact_path(date_ist);
    // O(1) EXEMPT: cold path, one file read per attach attempt, never per tick.
    let body = std::fs::read_to_string(&path)
        .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    let entries = crate::dhan_live_universe::parse_mapping_artifact(&body)
        .map_err(|err| format!("{}: {err}", path.display()))?;
    Ok(fno_spot_instruments(&entries))
}

/// The widest index window that still fits the budget beside `spot_count`.
///
/// Returns `None` when not even ATM alone (k = 0, four instruments) fits.
#[must_use]
pub fn index_strikes_each_side(
    spot_count: usize,
    budget: usize,
    max_each_side: usize,
) -> Option<usize> {
    let room = budget.checked_sub(spot_count)?;
    (0..=max_each_side)
        .rev()
        .find(|&k| index_legs_cost(k) <= room)
}

/// NIFTY then BANKNIFTY, ATM ±`each_side`, call and put, ascending by strike.
///
/// An index whose chain has no priced ATM yet contributes nothing. It is never
/// padded with a guessed strike.
#[must_use]
pub fn index_legs(candidates: &[DepthCandidate], each_side: usize) -> Vec<SubscribeInstrument> {
    let mut out = Vec::with_capacity(index_legs_cost(each_side));
    for underlying in DEPTH_20_INDEX_UNDERLYINGS {
        out.extend(index_window(candidates, underlying, each_side));
    }
    out
}

/// Spots first, then index legs, deduplicated and capped at `budget`.
///
/// Spots come first because they are known at 09:00. The index legs arrive at
/// 09:12 and fill the remaining room.
#[must_use]
pub fn build_static_depth20(
    spots: &[SubscribeInstrument],
    index: &[SubscribeInstrument],
    budget: usize,
) -> Vec<SubscribeInstrument> {
    let mut seen: HashSet<(u64, ExchangeSegment)> =
        HashSet::with_capacity(spots.len() + index.len());
    let mut out = Vec::with_capacity(budget.min(spots.len() + index.len()));
    for inst in spots.iter().chain(index.iter()) {
        if out.len() >= budget {
            break;
        }
        if seen.insert((inst.security_id, inst.segment)) {
            out.push(*inst);
        }
    }
    out
}

/// The instruments of `wanted` that no depth-20 connection holds yet.
///
/// Keyed on `(security_id, segment)` (I-P1-11). Order is `wanted`'s own. O(n)
/// in `wanted`, with one hash probe each.
#[must_use]
pub fn not_yet_held(
    wanted: &[SubscribeInstrument],
    held: &HashSet<(u64, ExchangeSegment)>,
) -> Vec<SubscribeInstrument> {
    wanted
        .iter()
        .filter(|inst| !held.contains(&(inst.security_id, inst.segment)))
        .copied()
        .collect()
}

/// Splits `items` across connections in order, never past a connection's room.
///
/// Returns each connection's chunk (by its index in `rooms`) and how many
/// items found no room at all. A connection with zero room gets no chunk. A
/// socket is never over-filled, because Dhan answers an over-limit subscribe
/// with 804 and closes it.
#[must_use]
pub fn assign_by_room(
    rooms: &[usize],
    items: &[SubscribeInstrument],
) -> (Vec<(usize, Vec<SubscribeInstrument>)>, usize) {
    let mut chunks = Vec::with_capacity(rooms.len());
    let mut cursor = 0usize;
    for (index, &room) in rooms.iter().enumerate() {
        if cursor >= items.len() {
            break;
        }
        if room == 0 {
            continue;
        }
        let take = room.min(items.len() - cursor);
        chunks.push((index, items[cursor..cursor + take].to_vec()));
        cursor += take;
    }
    (chunks, items.len() - cursor)
}

/// After this IST second the attach stops waiting for a FULL index window and
/// accepts whatever legs the chain priced (09:20, five minutes into the
/// continuous session).
///
/// NIFTY and BANKNIFTY price from 09:00 and always list more than four strikes
/// each side, so the full window is the normal case. The deadline exists so a
/// short ladder or an unpriced chain cannot hold the whole depth attach — and
/// the depth-200 rotation that waits on it — until the 10:00 give-up.
pub const DEPTH20_INDEX_SETTLE_DEADLINE_IST_SECS: u32 = 9 * 3_600 + 20 * 60;

/// Whether the depth-20 index legs are finished for the day.
///
/// True once the 09:12 window is due, nothing offered is still unconfirmed,
/// and either the full window was offered or the settle deadline has passed.
/// A budget with no room for index legs (`full_cost == 0`) settles as soon as
/// the window is due. O(1).
#[must_use]
pub const fn index_legs_settled(
    index_due: bool,
    offered: usize,
    full_cost: usize,
    outstanding: usize,
    past_settle_deadline: bool,
) -> bool {
    index_due && outstanding == 0 && (offered >= full_cost || past_settle_deadline)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: u64, seg: ExchangeSegment) -> MasterEntry {
        MasterEntry {
            security_id: id,
            exchange_segment_code: seg.binary_code(),
        }
    }

    fn eq(id: u64) -> SubscribeInstrument {
        SubscribeInstrument {
            security_id: id,
            segment: ExchangeSegment::NseEquity,
        }
    }

    fn fno(id: u64) -> SubscribeInstrument {
        SubscribeInstrument {
            security_id: id,
            segment: ExchangeSegment::NseFno,
        }
    }

    #[test]
    fn test_index_legs_cost_matches_operator_arithmetic() {
        assert_eq!(index_legs_cost(0), 4);
        assert_eq!(index_legs_cost(3), 28);
        assert_eq!(index_legs_cost(4), 36);
    }

    #[test]
    fn test_index_strikes_each_side_208_spots_give_plus_minus_four_and_244_slots() {
        let k = index_strikes_each_side(208, DEPTH20_STATIC_BUDGET, DEPTH20_INDEX_MAX_EACH_SIDE);
        assert_eq!(k, Some(4));
        assert_eq!(208 + index_legs_cost(4), 244);
    }

    #[test]
    fn test_index_strikes_each_side_more_spots_shrink_rather_than_breach() {
        // 250 - 216 = 34 < 36, so ±3 (28) is the widest that fits.
        assert_eq!(index_strikes_each_side(216, 250, 4), Some(3));
        assert_eq!(index_strikes_each_side(246, 250, 4), Some(0));
    }

    #[test]
    fn test_no_window_when_even_atm_does_not_fit() {
        assert_eq!(index_strikes_each_side(247, 250, 4), None);
        assert_eq!(index_strikes_each_side(300, 250, 4), None);
    }

    #[test]
    fn test_fno_spot_instruments_keep_nse_equity_only_and_dedup() {
        let entries = [
            entry(13, ExchangeSegment::IdxI),
            entry(2885, ExchangeSegment::NseEquity),
            entry(2885, ExchangeSegment::NseEquity),
            entry(500325, ExchangeSegment::BseEquity),
            entry(1333, ExchangeSegment::NseEquity),
            entry(0, ExchangeSegment::NseEquity),
        ];
        assert_eq!(fno_spot_instruments(&entries), vec![eq(2885), eq(1333)]);
    }

    #[test]
    fn test_same_id_in_two_segments_is_two_instruments() {
        // I-P1-11: the id alone is not the key.
        let out = build_static_depth20(&[eq(27)], &[fno(27)], 250);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn test_build_static_depth20_puts_spots_first_dedups_and_caps() {
        let spots = [eq(1), eq(2), eq(3)];
        let index = [fno(10), eq(2), fno(11), fno(12)];
        assert_eq!(
            build_static_depth20(&spots, &index, 250),
            vec![eq(1), eq(2), eq(3), fno(10), fno(11), fno(12)]
        );
        assert_eq!(
            build_static_depth20(&spots, &index, 4),
            vec![eq(1), eq(2), eq(3), fno(10)]
        );
    }

    #[test]
    fn test_index_legs_empty_without_candidates() {
        assert!(index_legs(&[], 4).is_empty());
    }

    #[test]
    fn test_not_yet_held_keys_on_id_and_segment() {
        let held: HashSet<(u64, ExchangeSegment)> = [
            (27, ExchangeSegment::NseEquity),
            (10, ExchangeSegment::NseFno),
        ]
        .into_iter()
        .collect();
        // fno(27) shares an id with a held equity but is a different instrument.
        let wanted = [eq(27), fno(27), fno(10), fno(11)];
        assert_eq!(not_yet_held(&wanted, &held), vec![fno(27), fno(11)]);
    }

    #[test]
    fn test_not_yet_held_empty_when_all_held() {
        let held: HashSet<(u64, ExchangeSegment)> =
            [(1, ExchangeSegment::NseFno)].into_iter().collect();
        assert!(not_yet_held(&[fno(1)], &held).is_empty());
    }

    #[test]
    fn test_assign_by_room_fills_in_order_and_never_overfills() {
        let items: Vec<SubscribeInstrument> = (1..=7).map(fno).collect();
        let (chunks, unplaced) = assign_by_room(&[3, 0, 2, 5], &items);
        assert_eq!(unplaced, 0);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0], (0, vec![fno(1), fno(2), fno(3)]));
        // Connection 1 has no room and gets no chunk.
        assert_eq!(chunks[1], (2, vec![fno(4), fno(5)]));
        assert_eq!(chunks[2], (3, vec![fno(6), fno(7)]));
        for (index, chunk) in &chunks {
            assert!(chunk.len() <= [3, 0, 2, 5][*index]);
        }
    }

    #[test]
    fn test_assign_by_room_reports_unplaced() {
        let items: Vec<SubscribeInstrument> = (1..=5).map(fno).collect();
        let (chunks, unplaced) = assign_by_room(&[1, 2], &items);
        assert_eq!(unplaced, 2);
        assert_eq!(chunks.iter().map(|(_, c)| c.len()).sum::<usize>(), 3);
        let (none, all) = assign_by_room(&[], &items);
        assert!(none.is_empty());
        assert_eq!(all, 5);
    }

    #[test]
    fn test_assign_by_room_nothing_to_place() {
        let (chunks, unplaced) = assign_by_room(&[50, 50], &[]);
        assert!(chunks.is_empty());
        assert_eq!(unplaced, 0);
    }

    #[test]
    fn test_index_legs_settled_waits_for_the_full_window() {
        // Before 09:12 nothing is settled, whatever else is true.
        assert!(!index_legs_settled(false, 36, 36, 0, true));
        // Due, full window, all confirmed: settled.
        assert!(index_legs_settled(true, 36, 36, 0, false));
        // Due and full, but a leg is still unconfirmed: keep trying.
        assert!(!index_legs_settled(true, 36, 36, 1, true));
        // Due, short window, before the deadline: keep waiting.
        assert!(!index_legs_settled(true, 18, 36, 0, false));
        // Short window after the deadline: accept what exists.
        assert!(index_legs_settled(true, 18, 36, 0, true));
        // No room for index legs at all: settled as soon as it is due.
        assert!(index_legs_settled(true, 0, 0, 0, false));
    }

    #[test]
    fn test_settle_deadline_is_after_the_preopen_window() {
        let preopen = 9 * 3_600 + 12 * 60;
        let continuous = 9 * 3_600 + 15 * 60;
        assert!(DEPTH20_INDEX_SETTLE_DEADLINE_IST_SECS > preopen);
        assert!(DEPTH20_INDEX_SETTLE_DEADLINE_IST_SECS > continuous);
    }

    #[test]
    fn test_read_fno_spot_instruments_reports_missing_file() {
        let err = read_fno_spot_instruments("1900-01-01").unwrap_err();
        assert!(
            err.contains("dhan-fno-underlyings-1900-01-01.json"),
            "{err}"
        );
    }
}

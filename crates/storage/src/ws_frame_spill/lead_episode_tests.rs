//! Frame-counter lead-episode tests (2026-11-23, PR24a), kept out of
//! `ws_frame_spill.rs` so the parent stays under its size pin
//! (`source_size_ratchet_guard.rs`).

use super::*;

/// 2026-11-23 (PR24a): the mint arithmetic, pulled out of
/// `next_frame_seq` unchanged, and the lead test after it.
#[test]
fn test_mint_base_and_lead_exceeds_cover_the_wall_the_counter_and_the_ceiling() {
    let ceiling = u64::MAX >> PACKET_INDEX_BITS;
    // The wall ahead of the counter: the wall's base.
    assert_eq!(mint_base(10, 50, ceiling), 50);
    // The counter at or ahead of the wall: one past it.
    assert_eq!(mint_base(50, 50, ceiling), 51);
    assert_eq!(mint_base(90, 50, ceiling), 91);
    // Saturation at the ceiling, from below and from u64::MAX.
    assert_eq!(mint_base(ceiling - 1, 0, ceiling), ceiling);
    assert_eq!(mint_base(ceiling, 0, ceiling), ceiling);
    assert_eq!(mint_base(u64::MAX, u64::MAX, ceiling), ceiling);
    // The lead threshold, in bases: exactly LEAD_RECORD_BASES is not a
    // lead, one more is.
    assert_eq!(
        LEAD_RECORD_BASES,
        crate::wal_clock_step::COUNTER_LEAD_RECORD_NANOS >> PACKET_INDEX_BITS
    );
    assert_eq!(LEAD_RECORD_BASES, 3_814);
    let now_base = 1_000_000;
    assert!(!lead_exceeds(now_base, now_base));
    assert!(!lead_exceeds(now_base + LEAD_RECORD_BASES, now_base));
    assert!(lead_exceeds(now_base + LEAD_RECORD_BASES + 1, now_base));
    assert!(!lead_exceeds(now_base - 1, now_base));
    // A wall near the top never wraps into a false lead.
    assert!(!lead_exceeds(u64::MAX, u64::MAX - 1));
    // A burst: 4,000 mints at one wall base lead it by 4,000 bases.
    let mut base = now_base;
    for _ in 0..4_000 {
        base = mint_base(base, now_base, ceiling);
    }
    assert!(lead_exceeds(base, now_base));
}

/// One compare-and-swap per lead episode: the first leading mint sets the
/// cell, later ones keep it; once the step log ends the episode the next
/// leading mint opens a new one.
#[test]
fn test_note_lead_began_in_sets_the_cell_once_per_episode_and_end_counter_lead_episode_in_clears_it()
 {
    let cell = AtomicU64::new(0);
    note_lead_began_in(&cell, 1_000);
    assert_eq!(cell.load(Ordering::Acquire), 1_000);
    note_lead_began_in(&cell, 2_000);
    assert_eq!(
        cell.load(Ordering::Acquire),
        1_000,
        "kept within the episode"
    );
    // A stale end (another episode's start) clears nothing.
    end_counter_lead_episode_in(&cell, 999);
    assert_eq!(cell.load(Ordering::Acquire), 1_000);
    end_counter_lead_episode_in(&cell, 0);
    assert_eq!(cell.load(Ordering::Acquire), 1_000);
    end_counter_lead_episode_in(&cell, 1_000);
    assert_eq!(cell.load(Ordering::Acquire), 0);
    note_lead_began_in(&cell, 3_000);
    assert_eq!(cell.load(Ordering::Acquire), 3_000, "a new episode");
    // A wall clock reading 0 still opens an episode.
    let zero = AtomicU64::new(0);
    note_lead_began_in(&zero, 0);
    assert_eq!(zero.load(Ordering::Acquire), 1);
}

/// The minter's hook and the step log's reader name the one process cell
/// (source scan; the arithmetic is tested above). Bites: a mint that
/// tests the lead before its compare-and-swap succeeds, or marks another
/// cell, fails it.
#[test]
fn test_next_frame_seq_marks_the_lead_and_counter_lead_since_nanos_end_counter_lead_episode_read_the_cell()
 {
    let src = include_str!("../ws_frame_spill.rs");
    let body = |name: &str| {
        let start = src.find(name).expect("fn present");
        let rest = &src[start..];
        &rest[..rest.find("\n}\n").expect("fn end")]
    };
    let mint = body("pub fn next_frame_seq() -> u64 {");
    let cas = mint.find(".is_ok()").expect("compare-and-swap");
    let hook = mint
        .find("if lead_exceeds(next_base, now_base) {\n                note_lead_began_in(&LEAD_SINCE_NANOS, now);")
        .expect("the mint hook");
    assert!(cas < hook, "the lead is marked only after the CAS succeeds");
    assert!(mint.contains("mint_base(prev >> PACKET_INDEX_BITS, now_base, base_ceiling)"));
    assert!(
        body("pub fn counter_lead_since_nanos() -> u64 {")
            .contains("LEAD_SINCE_NANOS.load(Ordering::Acquire)")
    );
    let end = body("pub fn end_counter_lead_episode(since: u64) {");
    assert!(end.contains("&LEAD_SINCE_NANOS,"));
    assert!(end.contains("&WAL_FRAME_SEQ,"));
    assert!(end.contains("end_counter_lead_episode_on("));
    // Review finding (2026-11-23): the store-buffering pair is SeqCst on
    // both sides (the minter's CAS on the counter, then its load of the
    // cell; the ender's clear of the cell, then its load of the counter).
    assert!(
        mint.contains(".compare_exchange_weak(prev, next, Ordering::SeqCst, Ordering::Relaxed)")
    );
    let note = body("pub(crate) fn note_lead_began_in(cell: &AtomicU64, now_nanos: u64) {");
    assert!(note.contains("cell.load(Ordering::SeqCst)"));
    assert!(note.contains("Ordering::SeqCst, Ordering::SeqCst)"));
    let on = body("pub(crate) fn end_counter_lead_episode_on(");
    assert!(on.contains("counter.load(Ordering::SeqCst)"));
    let clear = on
        .find("end_counter_lead_episode_in(cell, since);")
        .expect("clear");
    let reread = on.find("counter.load(Ordering::SeqCst)").expect("re-read");
    assert!(clear < reread, "the counter is read again AFTER the clear");
    assert!(
        body("pub(crate) fn end_counter_lead_episode_in(cell: &AtomicU64, since: u64) {")
            .contains("compare_exchange(since, 0, Ordering::SeqCst, Ordering::SeqCst)")
    );
}

/// Review finding (2026-11-23): ending an episode re-reads the counter
/// after the clear and reopens the episode at once when the counter
/// still leads the wall, so a lead minted while the old episode was open
/// (which set nothing) is never left unmarked. Bites: removing the
/// re-check leaves case (a) at 0.
#[test]
fn test_end_counter_lead_episode_on_reopens_a_lead_still_standing() {
    const SEC: u64 = 1_000_000_000;
    let wall = 1_795_404_600 * SEC;
    let leading = AtomicU64::new(wall + 2 * SEC);
    let at_wall = AtomicU64::new(wall);
    // (a) The counter still leads: a new episode, begun now.
    let cell = AtomicU64::new(7);
    end_counter_lead_episode_on(&cell, 7, &leading, || wall);
    assert_eq!(
        cell.load(Ordering::SeqCst),
        wall,
        "reopened at the wall now"
    );
    // (b) The counter met the wall: the episode ends.
    let cell = AtomicU64::new(7);
    end_counter_lead_episode_on(&cell, 7, &at_wall, || wall);
    assert_eq!(cell.load(Ordering::SeqCst), 0);
    // (c) No episode was read: nothing, even with the counter leading.
    let cell = AtomicU64::new(0);
    end_counter_lead_episode_on(&cell, 0, &leading, || wall);
    assert_eq!(cell.load(Ordering::SeqCst), 0);
    // (d) The cell holds another episode's start: left as it is.
    let cell = AtomicU64::new(9);
    end_counter_lead_episode_on(&cell, 7, &at_wall, || wall);
    assert_eq!(cell.load(Ordering::SeqCst), 9);
    let cell = AtomicU64::new(9);
    end_counter_lead_episode_on(&cell, 7, &leading, || wall);
    assert_eq!(cell.load(Ordering::SeqCst), 9, "an open episode is kept");
}

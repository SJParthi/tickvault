//! Drain-level tests of operator Quote 61 (2026-11-14): a live packet for a
//! contract with no trade yet today is written to `feed_aux_packets` only when
//! it differs from the contract's previous written row.

use super::super::*;
use tickvault_storage::feed_aux_repeat::AuxRepeatFilter;

/// 2026-05-22 10:00:00 IST, in IST epoch seconds (Dhan's LTT convention).
const LTT_1000_IST: u32 = 1_779_321_600 + 10 * 3_600;
/// The same contract's last trade, one day earlier.
const YESTERDAY: u32 = LTT_1000_IST - 86_400;

fn receipt_utc_nanos(ist_secs: u32) -> i64 {
    (i64::from(ist_secs) - i64::from(tickvault_common::constants::IST_UTC_OFFSET_SECONDS))
        * 1_000_000_000
}

fn ticker(segment: u8, security_id: u32, ltp: f32, ltt: u32) -> Vec<u8> {
    let len = tickvault_common::constants::TICKER_PACKET_SIZE;
    let mut p = vec![0u8; len];
    p[0] = tickvault_common::constants::RESPONSE_CODE_TICKER;
    p[1..3].copy_from_slice(&u16::try_from(len).unwrap_or(0).to_le_bytes());
    p[3] = segment;
    p[4..8].copy_from_slice(&security_id.to_le_bytes());
    p[8..12].copy_from_slice(&ltp.to_le_bytes());
    p[12..16].copy_from_slice(&ltt.to_le_bytes());
    p
}

/// A Full packet (NSE_EQ, five non-zero depth levels).
fn full(security_id: u32, ltp: f32, ltt: u32) -> Vec<u8> {
    use tickvault_common::constants as k;
    let mut p = vec![0u8; k::FULL_QUOTE_PACKET_SIZE];
    p[0] = k::RESPONSE_CODE_FULL;
    p[1..3].copy_from_slice(
        &u16::try_from(k::FULL_QUOTE_PACKET_SIZE)
            .unwrap_or(0)
            .to_le_bytes(),
    );
    p[3] = 1;
    p[4..8].copy_from_slice(&security_id.to_le_bytes());
    p[k::QUOTE_OFFSET_LTP..k::QUOTE_OFFSET_LTP + 4].copy_from_slice(&ltp.to_le_bytes());
    p[k::QUOTE_OFFSET_LTT..k::QUOTE_OFFSET_LTT + 4].copy_from_slice(&ltt.to_le_bytes());
    for level in 0..k::MARKET_DEPTH_LEVELS {
        let base = k::FULL_OFFSET_DEPTH_START + level * k::MARKET_DEPTH_LEVEL_SIZE;
        let step = u32::try_from(level).unwrap_or(0) + 1;
        p[base..base + 4].copy_from_slice(&(100u32 * step).to_le_bytes());
        p[base + 4..base + 8].copy_from_slice(&(200u32 * step).to_le_bytes());
    }
    p
}

fn drain(ingest: &mut LiveIngest, seq: u64, bytes: Vec<u8>, receipt: i64) -> FrameOutcome {
    drain_main_feed_frame(
        ingest,
        &CapturedFrame {
            seq,
            endpoint: DhanEndpointType::MainFeed,
            connection_index: 0,
            received_at: std::time::Instant::now(),
            received_at_nanos: receipt,
            wal_backed: true,
            bytes: bytes.into(),
        },
        receipt,
        1_000,
        counters(),
    )
}

/// A gap epoch that never moves, so these tests judge values only and no
/// other test bumping the process-wide counters can change their counts.
fn still() -> u64 {
    0
}

fn ingest() -> LiveIngest {
    let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8);
    ingest.aux_repeat = AuxRepeatFilter::with_gap_epoch(still);
    ingest
}

/// Receipt `n` seconds after 10:00 IST.
fn at(n: u32) -> i64 {
    receipt_utc_nanos(LTT_1000_IST + n)
}

#[test]
fn test_append_no_trade_today_skips_exact_repeats_across_frames() {
    let mut ingest = ingest();
    for n in 0..5u32 {
        let out = drain(
            &mut ingest,
            u64::from(n) + 1,
            ticker(2, 66_422, 142.5, YESTERDAY),
            at(n),
        );
        assert_eq!(out.folded, 0, "never a `ticks` row");
    }
    assert_eq!(
        ingest.writer.aux_rows_appended(),
        1,
        "five identical packets: one row (Quote 61)"
    );
    assert_eq!(ingest.writer.pending(), 1);
}

#[test]
fn test_append_no_trade_today_writes_a_change_and_keys_on_the_segment() {
    let mut ingest = ingest();
    let _ = drain(&mut ingest, 1, ticker(2, 66_422, 142.5, YESTERDAY), at(0));
    // Price changed: written.
    let _ = drain(&mut ingest, 2, ticker(2, 66_422, 142.55, YESTERDAY), at(1));
    assert_eq!(ingest.writer.aux_rows_appended(), 2);
    // Trade time changed (still an earlier day): written.
    let _ = drain(
        &mut ingest,
        3,
        ticker(2, 66_422, 142.55, YESTERDAY + 1),
        at(2),
    );
    assert_eq!(ingest.writer.aux_rows_appended(), 3);
    // Same id, other segment: its own first row.
    let _ = drain(
        &mut ingest,
        4,
        ticker(1, 66_422, 142.55, YESTERDAY + 1),
        at(3),
    );
    assert_eq!(ingest.writer.aux_rows_appended(), 4);
    // Back to the first segment, unchanged: skipped.
    let _ = drain(
        &mut ingest,
        5,
        ticker(2, 66_422, 142.55, YESTERDAY + 1),
        at(4),
    );
    assert_eq!(ingest.writer.aux_rows_appended(), 4);
    // A packet stacking the same repeat twice in one frame: still skipped.
    let mut two = ticker(1, 66_422, 142.55, YESTERDAY + 1);
    two.extend_from_slice(&ticker(1, 66_422, 142.55, YESTERDAY + 1));
    let _ = drain(&mut ingest, 6, two, at(5));
    assert_eq!(ingest.writer.aux_rows_appended(), 4);
}

/// The next IST receipt day starts every contract over.
#[test]
fn test_append_no_trade_today_writes_again_on_the_next_receipt_day() {
    let mut ingest = ingest();
    let _ = drain(&mut ingest, 1, ticker(2, 66_422, 142.5, YESTERDAY), at(0));
    let _ = drain(&mut ingest, 2, ticker(2, 66_422, 142.5, YESTERDAY), at(1));
    assert_eq!(ingest.writer.aux_rows_appended(), 1);
    // The next day the same last trade is two days old: still wrong-day.
    let _ = drain(
        &mut ingest,
        3,
        ticker(2, 66_422, 142.5, YESTERDAY),
        at(86_400),
    );
    assert_eq!(ingest.writer.aux_rows_appended(), 2);
}

/// Every WAL replay pass starts the filter over, so a pass that re-reads a
/// frame whose row never landed writes it again instead of skipping it against
/// the lost row. Inside one pass the same rule as live applies.
#[test]
fn test_append_no_trade_today_a_replay_pass_starts_over_and_skips_inside_the_pass() {
    let mut ingest = ingest();
    let _ = drain(&mut ingest, 1, ticker(2, 66_422, 142.5, YESTERDAY), at(0));
    assert_eq!(ingest.writer.aux_rows_appended(), 1);
    let frame = |seq: u64, n: u32| {
        (
            seq,
            at(n),
            WalEndpoint::MainFeed,
            bytes::Bytes::from(ticker(2, 66_422, 142.5, YESTERDAY)),
        )
    };
    let out = refold_wal_frames(&mut ingest, &[frame(1, 0), frame(2, 1)], &[], None);
    assert_eq!(
        out.refused_wrong_day, 2,
        "both replayed packets reached the rule"
    );
    assert_eq!(
        ingest.writer.aux_rows_appended(),
        2,
        "the pass writes its first row again and skips the identical second"
    );
}

/// The replay stores the same rows as the live drain for the same frames.
#[test]
fn test_append_no_trade_today_replay_matches_live_for_the_same_frames() {
    let packets = [
        (2u8, 142.5f32, YESTERDAY),
        (2, 142.5, YESTERDAY),
        (2, 143.0, YESTERDAY),
        (2, 143.0, YESTERDAY),
        (1, 143.0, YESTERDAY),
        (2, 143.0, YESTERDAY),
    ];
    let mut live = ingest();
    let mut frames = Vec::new();
    for (n, (seg, ltp, ltt)) in packets.iter().enumerate() {
        let n32 = u32::try_from(n).unwrap_or(0);
        let seq = u64::from(n32) + 1;
        let _ = drain(&mut live, seq, ticker(*seg, 66_422, *ltp, *ltt), at(n32));
        frames.push((
            seq,
            at(n32),
            WalEndpoint::MainFeed,
            bytes::Bytes::from(ticker(*seg, 66_422, *ltp, *ltt)),
        ));
    }
    let mut replay = ingest();
    let _ = refold_wal_frames(&mut replay, &frames, &[], None);
    assert_eq!(live.writer.aux_rows_appended(), 3);
    assert_eq!(
        replay.writer.aux_rows_appended(),
        live.writer.aux_rows_appended()
    );
}

fn sealed(
    ingest: &mut LiveIngest,
) -> Vec<(
    u64,
    u8,
    tickvault_trading::candles::tf_index::TfIndex,
    tickvault_trading::candles::LiveCandleState,
)> {
    let mut out = Vec::new();
    let _ = ingest
        .aggregator
        .force_seal_all(|_feed, sid, seg, tf, state| out.push((sid, seg, tf, state)));
    out
}

/// What one run produced: tick rows, aux rows, inline-depth rows, candles.
type RunOutput = (
    usize,
    u64,
    u64,
    Vec<(
        u64,
        u8,
        tickvault_trading::candles::tf_index::TfIndex,
        tickvault_trading::candles::LiveCandleState,
    )>,
);

/// Ticks, candles and inline depth are the same with and without the skip:
/// the "without" run clears the filter before every frame, so every repeat is
/// written, exactly as before Quote 61.
#[test]
fn test_append_no_trade_today_leaves_ticks_candles_and_depth_unchanged() {
    let run = |skip: bool| -> RunOutput {
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8)
            .with_inline_depth(DepthIngest::for_test());
        ingest.aux_repeat = AuxRepeatFilter::with_gap_epoch(still);
        let mut depth_rows = 0u64;
        for n in 0..40u32 {
            let mut bytes = full(
                2_885,
                2_500.0 + f32::from(u16::try_from(n % 7).unwrap_or(0)),
                LTT_1000_IST + n,
            );
            // Two contracts with no trade today, repeating every frame, one of
            // them changing every tenth frame.
            bytes.extend_from_slice(&full(66_422, 142.5, YESTERDAY));
            let moved = 99.0 + f32::from(u16::try_from(n / 10).unwrap_or(0));
            bytes.extend_from_slice(&full(66_423, moved, YESTERDAY));
            if !skip {
                ingest.aux_repeat.clear();
            }
            let out = drain(&mut ingest, u64::from(n) + 1, bytes, at(n));
            depth_rows += out.inline_depth_rows;
        }
        let aux = ingest.writer.aux_rows_appended();
        let ticks = ingest.writer.pending() - usize::try_from(aux).unwrap_or(0);
        (ticks, aux, depth_rows, sealed(&mut ingest))
    };
    let with_skip = run(true);
    let without = run(false);
    assert_eq!(with_skip.0, 40, "every today packet is a `ticks` row");
    assert_eq!(with_skip.0, without.0, "ticks unchanged");
    assert_eq!(with_skip.2, without.2, "inline depth unchanged");
    assert!(
        with_skip.2 >= 40 * 3,
        "depth ran for every packet, wrong-day included"
    );
    assert_eq!(with_skip.3, without.3, "candles unchanged");
    assert!(!with_skip.3.is_empty());
    assert_eq!(without.1, 80, "without the skip every repeat is written");
    assert_eq!(with_skip.1, 1 + 4, "with it: one row, plus one per change");
}

/// Review finding (2026-11-14): the live drain must not start from the last
/// REPLAYED row of a contract. A replay re-reads only unapplied frames, so
/// that row can be older than the newest row already stored; skipping a live
/// packet against it would drop a real change. `refold_wal_frames` clears the
/// filter at the end of every pass, so the first live row is always written.
#[test]
fn test_append_no_trade_today_live_after_a_replay_writes_its_first_row() {
    let mut ingest = ingest();
    let frame = (
        1,
        at(0),
        WalEndpoint::MainFeed,
        bytes::Bytes::from(ticker(2, 66_422, 142.5, YESTERDAY)),
    );
    let _ = refold_wal_frames(&mut ingest, &[frame], &[], None);
    ingest.finish_wal_replay(false);
    assert_eq!(ingest.writer.aux_rows_appended(), 1, "the replayed row");
    let _ = drain(&mut ingest, 2, ticker(2, 66_422, 142.5, YESTERDAY), at(60));
    assert_eq!(
        ingest.writer.aux_rows_appended(),
        2,
        "the first live row after a replay is written, never skipped against \
         the replayed (possibly older) value"
    );
    let _ = drain(&mut ingest, 3, ticker(2, 66_422, 142.5, YESTERDAY), at(61));
    assert_eq!(ingest.writer.aux_rows_appended(), 2, "then repeats skip");
}

/// The production gap epoch moves when a staged row does not land (the
/// conservation ledger's not-landed count), so a row the filter remembered
/// but that never reached QuestDB stops arming a skip. Both inputs only grow,
/// so a concurrent test bumping them can only move it further.
#[test]
fn test_gap_epoch_moves_when_rows_do_not_land() {
    let before = super::gap_epoch();
    tickvault_storage::tick_conservation_ledger::ledger().note_rows_not_landed(1);
    assert_ne!(super::gap_epoch(), before);
}

/// Review finding (2026-11-14): `kind` is in the table's DEDUP key, so the
/// label follows the frame's receipt: a frame received before the cutover
/// keeps `connect_snapshot` (what the old binary wrote for it, so a replay of
/// it overwrites its own row), a later one is `no_trade_today`.
#[test]
fn test_no_trade_today_row_labels_by_receipt_cutover() {
    use tickvault_storage::feed_aux_persistence::{
        AuxPacketKind, NO_TRADE_TODAY_FIRST_RECEIPT_IST_SECS,
    };
    let cutover = u32::try_from(NO_TRADE_TODAY_FIRST_RECEIPT_IST_SECS).unwrap_or(0);
    let tick_at = |receipt_ist: u32| ParsedTick {
        security_id: 66_422,
        exchange_segment_code: 2,
        last_traded_price: 142.5,
        exchange_timestamp: receipt_ist - 86_400,
        received_at_nanos: receipt_utc_nanos(receipt_ist),
        ..Default::default()
    };
    let before = super::no_trade_today_row(&tick_at(LTT_1000_IST), 1).expect("fits");
    assert_eq!(before.kind, AuxPacketKind::ConnectSnapshot);
    let after = super::no_trade_today_row(&tick_at(cutover + 10 * 3_600), 1).expect("fits");
    assert_eq!(after.kind, AuxPacketKind::NoTradeToday);
    // And the drain reaches the rule for a post-cutover frame too.
    let mut new = ingest();
    let ten_am = cutover + 10 * 3_600;
    let _ = drain(
        &mut new,
        1,
        ticker(2, 66_422, 142.5, ten_am - 86_400),
        receipt_utc_nanos(ten_am),
    );
    assert_eq!(new.writer.aux_rows_appended(), 1);
}

/// The three process-wide inputs of the production gap epoch, read apart.
fn epoch_inputs() -> [u64; 3] {
    [
        tickvault_core::websocket::pool_supervisor::live_shed::main_feed_live_shed_total(),
        tickvault_storage::tick_conservation_ledger::ledger().rows_not_landed(),
        tickvault_storage::candle_contract_labels::candle_contract_labels_generation(),
    ]
}

/// Runs `scenario` (which moves input `own` of [`epoch_inputs`] between its
/// two drains and returns the aux rows it staged) until a run in which no
/// OTHER input moved. Other tests in this process bump the same statics, and
/// a run in which one of them moved could pass with `own` removed from
/// `gap_epoch`; a quiet run cannot, so reverting the term fails the test.
/// `None` when no quiet run happened in 50 tries.
fn quiet_run(own: usize, mut scenario: impl FnMut() -> (u64, [u64; 3], [u64; 3])) -> Option<u64> {
    for _ in 0..50 {
        let before = epoch_inputs();
        let (rows, own_before, own_after) = scenario();
        let after = epoch_inputs();
        assert!(
            own_after[own] > own_before[own],
            "the scenario must move its own input across its two drains"
        );
        let others_still = (0..3).filter(|&i| i != own).all(|i| before[i] == after[i]);
        if others_still {
            return Some(rows);
        }
    }
    None
}

/// Review findings (2026-11-14): with the production filter, a staged row
/// that later fails to land must not keep skipping that contract's repeats.
/// The ledger bump stands in for the failed batch; the next repeat is written.
#[test]
fn test_production_filter_writes_the_repeat_after_a_row_did_not_land() {
    let rows = quiet_run(1, || {
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8);
        let _ = drain(&mut ingest, 1, ticker(2, 66_422, 142.5, YESTERDAY), at(0));
        let own_before = epoch_inputs();
        tickvault_storage::tick_conservation_ledger::ledger().note_rows_not_landed(1);
        let own_after = epoch_inputs();
        let _ = drain(&mut ingest, 2, ticker(2, 66_422, 142.5, YESTERDAY), at(1));
        (ingest.writer.aux_rows_appended(), own_before, own_after)
    });
    assert_eq!(
        rows,
        Some(2),
        "the first repeat after a row did not land is written"
    );
}

/// Review findings (2026-11-14): values A, then B in a frame the ring SHED
/// (kept in the WAL, never seen by the live drain, replayed later), then A
/// again. With the production filter the shed moves the gap epoch, so the
/// second A is written and the replayed B is never the last stored value.
/// Drives a real `WalRingSink` with a zero-slot budget, so the shed is the
/// production one.
#[test]
fn test_production_filter_writes_the_repeat_after_a_ring_shed() {
    use tickvault_core::websocket::pool_supervisor::FrameSink;
    let dir = std::env::temp_dir().join(format!(
        "tv-aux-repeat-shed-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    let spill = Arc::new(WsFrameSpill::new(&dir).expect("wal"));
    let (tx, _rx) = frame_ring::<CapturedFrame>(8);
    let sink = WalRingSink::new(
        Arc::clone(&spill),
        tx,
        Arc::new(RingByteBudget::with_slot_cap(1 << 20, 0)),
        WsType::LiveFeed,
        DhanEndpointType::MainFeed,
        0,
    );
    let rows = quiet_run(0, || {
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8);
        let _ = drain(&mut ingest, 1, ticker(2, 66_422, 142.5, YESTERDAY), at(0));
        let own_before = epoch_inputs();
        let _ = sink.accept(bytes::Bytes::from(ticker(2, 66_422, 150.0, YESTERDAY)));
        let own_after = epoch_inputs();
        let _ = drain(&mut ingest, 3, ticker(2, 66_422, 142.5, YESTERDAY), at(2));
        (ingest.writer.aux_rows_appended(), own_before, own_after)
    });
    assert_eq!(
        rows,
        Some(2),
        "the first repeat after a ring shed is written"
    );
    drop(sink);
    drop(spill);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Review finding (2026-11-14): a row's `contract` name comes from the name
/// table when it is staged, so a row staged before the table named the
/// contract stores NULL there. After a publish the next identical packet is
/// written (with the name), never skipped against the unnamed row. The
/// publish re-stores the current table's own names, so no concurrent test
/// loses a name.
#[test]
fn test_production_filter_writes_the_repeat_after_a_name_table_publish() {
    use tickvault_storage::candle_contract_labels as names;
    let _guard = crate::main_feed_backup::TEST_PUBLISH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let rows = quiet_run(2, || {
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8);
        let _ = drain(&mut ingest, 1, ticker(2, 66_422, 142.5, YESTERDAY), at(0));
        let own_before = epoch_inputs();
        let same = (**names::candle_contract_labels()).clone();
        let _ = names::publish_candle_contract_labels(same);
        let own_after = epoch_inputs();
        let _ = drain(&mut ingest, 2, ticker(2, 66_422, 142.5, YESTERDAY), at(1));
        (ingest.writer.aux_rows_appended(), own_before, own_after)
    });
    assert_eq!(
        rows,
        Some(2),
        "the first repeat after a name publish is written"
    );
}

/// Review finding (2026-11-14, HIGH): a replay gap INSIDE one pass. Frames
/// between two replayed frames were not re-read (applied, or below the
/// floor), and live may have stored a different row for the contract in
/// them, so the first row after the gap is written even when it repeats the
/// row before the gap.
#[test]
fn test_mark_replay_gap_writes_the_first_row_after_a_gap_inside_a_pass() {
    let frame = |seq: u64, n: u32| {
        (
            seq,
            at(n),
            WalEndpoint::MainFeed,
            bytes::Bytes::from(ticker(2, 66_422, 142.5, YESTERDAY)),
        )
    };
    let frames = [frame(1, 0), frame(5, 9), frame(6, 10)];
    let mut gapped = ingest();
    let out = refold_wal_frames(&mut gapped, &frames, &[1], None);
    assert_eq!(out.gaps_marked, 1);
    assert_eq!(
        gapped.writer.aux_rows_appended(),
        2,
        "the row before the gap and the first row after it; the third repeats \
         the second with no gap between them"
    );
    let mut contiguous = ingest();
    let _ = refold_wal_frames(&mut contiguous, &frames, &[], None);
    assert_eq!(contiguous.writer.aux_rows_appended(), 1, "no gap: one row");
}

/// The same for the gap a below-floor stretch leaves: the frame after it is
/// folded after a replay gap, and its row is written.
#[test]
fn test_mark_replay_gap_writes_the_first_row_after_a_below_floor_stretch() {
    let frame = |seq: u64, n: u32| {
        (
            seq,
            at(n),
            WalEndpoint::MainFeed,
            bytes::Bytes::from(ticker(2, 66_422, 142.5, YESTERDAY)),
        )
    };
    // The floor sits between receipts 5 and 6: the middle frame is below it.
    let mut floored = ingest();
    floored.market_data_floor_unix_nanos = Some(at(5) + 500_000_000);
    let out = refold_wal_frames(
        &mut floored,
        &[frame(1, 6), frame(2, 4), frame(3, 7)],
        &[],
        None,
    );
    assert_eq!((out.frames_below_floor, out.gaps_marked), (1, 1));
    assert_eq!(
        floored.writer.aux_rows_appended(),
        2,
        "the row after the below-floor stretch is written"
    );
}

/// A Quote packet (code 4) whose total buy quantity is not stored in the aux
/// row, so two packets can differ in bytes (not a backup "second copy") and
/// still give identical rows.
fn quote(segment: u8, security_id: u32, ltp: f32, ltt: u32, total_buy: u32) -> Vec<u8> {
    use tickvault_common::constants as k;
    let mut p = vec![0u8; k::QUOTE_PACKET_SIZE];
    p[0] = k::RESPONSE_CODE_QUOTE;
    p[1..3].copy_from_slice(
        &u16::try_from(k::QUOTE_PACKET_SIZE)
            .unwrap_or(0)
            .to_le_bytes(),
    );
    p[3] = segment;
    p[4..8].copy_from_slice(&security_id.to_le_bytes());
    p[k::QUOTE_OFFSET_LTP..k::QUOTE_OFFSET_LTP + 4].copy_from_slice(&ltp.to_le_bytes());
    p[k::QUOTE_OFFSET_LTT..k::QUOTE_OFFSET_LTT + 4].copy_from_slice(&ltt.to_le_bytes());
    p[k::QUOTE_OFFSET_TOTAL_BUY_QTY..k::QUOTE_OFFSET_TOTAL_BUY_QTY + 4]
        .copy_from_slice(&total_buy.to_le_bytes());
    p
}

/// Review finding (2026-11-14): a contract the main feed carries on two
/// sockets (the backup copy) is never skipped. A ring shed on one socket is
/// counted on that socket's own task, so a repeat from the other socket can
/// reach the drain before the shed moves the gap epoch; skipping it would
/// leave the stored rows on the shed packet's value with nothing counting it.
/// The key stays unskippable for the day after it leaves the backup set, so a
/// copy still in flight is written too.
#[test]
fn test_append_no_trade_today_never_skips_a_contract_carried_on_two_sockets() {
    let mut ingest = ingest();
    ingest.backup.install(&[(66_422, 2)]);
    for n in 0..4u32 {
        let _ = drain(
            &mut ingest,
            u64::from(n) + 1,
            ticker(2, 66_422, 142.5, YESTERDAY),
            at(n),
        );
    }
    assert_eq!(
        ingest.writer.aux_rows_appended(),
        4,
        "every row of a backed-up contract is written"
    );
    // The contract leaves the backup set: still written for the rest of the day.
    ingest.backup.install(&[]);
    let _ = drain(&mut ingest, 5, ticker(2, 66_422, 142.5, YESTERDAY), at(4));
    assert_eq!(ingest.writer.aux_rows_appended(), 5);
    // A contract that was never backed up is still filtered.
    let _ = drain(&mut ingest, 6, ticker(2, 66_423, 142.5, YESTERDAY), at(5));
    let _ = drain(&mut ingest, 7, ticker(2, 66_423, 142.5, YESTERDAY), at(6));
    assert_eq!(
        ingest.writer.aux_rows_appended(),
        6,
        "a contract on one socket: the repeat is skipped"
    );
}

/// The replay judges a backed-up contract by its own publication, so a
/// replayed day writes every row the live drain wrote for it.
#[test]
fn test_append_no_trade_today_replay_never_skips_a_backed_up_contract_like_live() {
    const N: u32 = 4;
    let mut live = ingest();
    live.backup.install(&[(66_422, 2)]);
    let mut frames = Vec::new();
    for n in 0..N {
        let seq = u64::from(n) + 1;
        let bytes = quote(2, 66_422, 142.5, YESTERDAY, 100 + n);
        let _ = drain(&mut live, seq, bytes.clone(), at(n));
        frames.push((seq, at(n), WalEndpoint::MainFeed, bytes::Bytes::from(bytes)));
    }
    assert_eq!(live.writer.aux_rows_appended(), u64::from(N));
    let set = crate::main_feed_backup::PersistedBackupSet {
        published_at_nanos: at(0) - 1_000_000_000,
        process_started_at_nanos: 0,
        contracts: vec![(66_422, 2)],
    };
    let history = crate::main_feed_backup::PersistedBackupHistory {
        publications: vec![set],
        trimmed_through_day: None,
    };
    let mut replay = ingest();
    replay.install_replay_backup(Some(&history));
    let out = refold_wal_frames(&mut replay, &frames, &[], None);
    assert_eq!(out.backup_dropped, 0, "the packets differ in bytes");
    assert_eq!(
        replay.writer.aux_rows_appended(),
        live.writer.aux_rows_appended(),
        "the replay writes every row the live drain wrote"
    );
}

/// 2026-11-14 audit fix: the skip rests on the WAL keeping every packet. A
/// frame the WAL refused (it reached the drain through the ring, unbacked)
/// has no copy anywhere else, so its exact repeat is written, never skipped;
/// a later WAL-backed repeat of the same values is skipped against it.
#[test]
fn test_append_no_trade_today_writes_an_exact_repeat_from_an_unbacked_frame() {
    let mut ingest = ingest();
    let unbacked = |ingest: &mut LiveIngest, seq: u64, n: u32| {
        drain_main_feed_frame(
            ingest,
            &CapturedFrame {
                seq,
                endpoint: DhanEndpointType::MainFeed,
                connection_index: 0,
                received_at: std::time::Instant::now(),
                received_at_nanos: at(n),
                wal_backed: false,
                bytes: ticker(2, 66_422, 142.5, YESTERDAY).into(),
            },
            at(n),
            1_000,
            counters(),
        )
    };
    let _ = drain(&mut ingest, 1, ticker(2, 66_422, 142.5, YESTERDAY), at(0));
    assert_eq!(ingest.writer.aux_rows_appended(), 1);
    let _ = unbacked(&mut ingest, 2, 1);
    let _ = unbacked(&mut ingest, 3, 2);
    assert_eq!(
        ingest.writer.aux_rows_appended(),
        3,
        "exact repeats with no WAL copy are written (nothing skipped)"
    );
    let _ = drain(&mut ingest, 4, ticker(2, 66_422, 142.5, YESTERDAY), at(3));
    assert_eq!(
        ingest.writer.aux_rows_appended(),
        3,
        "a later WAL-backed repeat is skipped: the flag is per frame"
    );
}

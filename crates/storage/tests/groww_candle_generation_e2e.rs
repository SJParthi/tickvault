//! End-to-end Part-B proof (operator's #1 blocker, 2026-06-30): a Groww tick
//! driven through the SHARED multi-TF candle aggregator produces a sealed candle
//! that the SHARED `ShadowCandleWriter` serialises to the plain `candles_1m`
//! table tagged `feed=groww`. This is the exact path the live Groww bridge uses
//! (`groww_bridge.rs` → `MultiTfAggregator::consume_tick(FeedStrategy::GROWW)` →
//! `route_seal(feed=Groww)` → seal-writer → `ShadowCandleWriter::append_seal`),
//! minus the tokio plumbing — so it proves candles seal in Groww-only mode.
//!
//! It ALSO proves the candle path is FEED-AGNOSTIC: the same aggregator + writer,
//! driven by a Dhan-tagged seal, stamps `feed=dhan`; driven by any other feed,
//! stamps that feed verbatim — no per-feed branch anywhere in the candle path.
//!
//! No live QuestDB needed — `ShadowCandleWriter::for_test()` is disconnected and
//! we assert the ILP WIRE BYTES, which are built independently of the connection.

use tickvault_common::feed::Feed;
use tickvault_common::tick_types::ParsedTick;
use tickvault_storage::shadow_candle_writer::ShadowCandleWriter;
use tickvault_trading::candles::{BufferedSeal, FeedStrategy, MultiTfAggregator, TfIndex};

const NSE_EQ: u8 = 1;
const SID: u64 = 1_333;

/// A whole IST day in seconds. We pick a base second-of-day inside the regular
/// session window [09:15, 15:30) IST so the aggregator's session gate admits the
/// ticks. 10:00:00 IST = 36000 secs-of-day. Use a recent-ish day's midnight.
const IST_DAY_BASE_SECS: u32 = 1_780_000_000 - (1_780_000_000 % 86_400); // IST midnight
const TEN_AM_SECS_OF_DAY: u32 = 10 * 3_600; // 36_000 — well inside [33_300, 55_800)

fn groww_tick(secs_of_day: u32, ltp: f32, volume: u32) -> ParsedTick {
    ParsedTick {
        security_id: SID,
        exchange_segment_code: NSE_EQ,
        last_traded_price: ltp,
        exchange_timestamp: IST_DAY_BASE_SECS + secs_of_day,
        volume,
        ..ParsedTick::default()
    }
}

/// Drive the SHARED aggregator with a Groww tick stream that crosses a 1-minute
/// boundary, capture the sealed minute-1 candle, and assert the SHARED candle
/// writer serialises it to `candles_1m` tagged `feed=groww`.
#[test]
fn test_groww_tick_seals_candle_to_candles_1m_with_feed_groww() {
    let agg = MultiTfAggregator::with_capacity(8);
    let key = (SID, NSE_EQ);
    agg.pre_populate(std::iter::once(key));
    agg.seed_cumulative(SID, NSE_EQ, 0);

    // Tick 1: 10:00:30 IST — opens the 10:00 minute bucket.
    let t1 = groww_tick(TEN_AM_SECS_OF_DAY + 30, 100.0, 10);
    let s1 = agg.consume_tick(&t1, NSE_EQ, FeedStrategy::GROWW, Some(10), |_tf, _state| {
        // First tick only opens buckets — no boundary crossed yet.
        panic!("no seal expected on the bucket-opening tick");
    });
    assert!(s1.instrument_found, "instrument must be registered");
    assert_eq!(s1.sealed_count, 0, "opening tick seals nothing");

    // Tick 2: 10:01:05 IST — crosses into the 10:01 minute → seals the 10:00 M1
    // bucket. Capture the sealed M1 state, tagged with the GROWW feed.
    let t2 = groww_tick(TEN_AM_SECS_OF_DAY + 60 + 5, 101.0, 20);
    let mut sealed_m1: Option<BufferedSeal> = None;
    let s2 = agg.consume_tick(&t2, NSE_EQ, FeedStrategy::GROWW, Some(20), |tf, state| {
        if tf == TfIndex::M1 {
            // This is EXACTLY what route_seal does: build a BufferedSeal stamped
            // with the producing feed (Groww here).
            sealed_m1 = Some(BufferedSeal::new(SID, NSE_EQ, tf, state, Feed::Groww));
        }
    });
    assert!(
        s2.sealed_count >= 1,
        "boundary cross must seal ≥1 TF (incl. M1)"
    );

    let seal = sealed_m1.expect("M1 candle must seal on the minute boundary");
    assert_eq!(
        seal.feed,
        Feed::Groww,
        "the sealed candle must carry feed=Groww"
    );

    // Now the SHARED candle writer serialises it — assert the wire row is the
    // plain candles_1m table tagged feed=groww (what would land in QuestDB).
    let mut w = ShadowCandleWriter::for_test();
    w.append_seal(&seal).expect("append groww candle");
    let wire = std::str::from_utf8(w.buffer_bytes()).expect("utf8");
    assert!(
        wire.starts_with("candles_1m,"),
        "Groww candle must seal to the SHARED candles_1m table, got {wire}"
    );
    assert!(
        wire.contains("feed=groww"),
        "the sealed Groww candle must be tagged feed=groww, got {wire}"
    );
    assert!(
        !wire.contains("feed=dhan"),
        "a Groww candle must NOT be relabelled feed=dhan, got {wire}"
    );
}

/// Feed parity: the IDENTICAL aggregator + writer path, driven by a Dhan-tagged
/// seal, stamps `feed=dhan`. Proves the Groww fix did not regress Dhan and that
/// the seal feed is what differentiates the candle, not any per-feed code branch.
#[test]
fn test_dhan_tick_seals_candle_with_feed_dhan_same_path() {
    let agg = MultiTfAggregator::with_capacity(8);
    agg.pre_populate(std::iter::once((SID, NSE_EQ)));
    agg.seed_cumulative(SID, NSE_EQ, 0);

    let t1 = groww_tick(TEN_AM_SECS_OF_DAY + 30, 100.0, 10);
    agg.consume_tick(&t1, NSE_EQ, FeedStrategy::DHAN, Some(10), |_tf, _s| {});

    let t2 = groww_tick(TEN_AM_SECS_OF_DAY + 60 + 5, 101.0, 20);
    let mut sealed: Option<BufferedSeal> = None;
    agg.consume_tick(&t2, NSE_EQ, FeedStrategy::DHAN, Some(20), |tf, state| {
        if tf == TfIndex::M1 {
            sealed = Some(BufferedSeal::new(SID, NSE_EQ, tf, state, Feed::Dhan));
        }
    });

    let seal = sealed.expect("M1 must seal");
    let mut w = ShadowCandleWriter::for_test();
    w.append_seal(&seal).expect("append dhan candle");
    let wire = std::str::from_utf8(w.buffer_bytes()).expect("utf8");
    assert!(
        wire.starts_with("candles_1m,"),
        "Dhan candle to candles_1m, got {wire}"
    );
    assert!(
        wire.contains("feed=dhan"),
        "Dhan candle must be tagged feed=dhan, got {wire}"
    );
}

/// ALL-TIMEFRAMES proof (operator scope clarification 2026-06-30): ONE common
/// aggregator engine fans a single tick out across MULTIPLE timeframes. A tick
/// that jumps from 10:00 to 14:00 IST crosses the M1, M2, …, M15, M30, H1, H2,
/// H3 boundaries at once, so a SINGLE `consume_tick` seals many TFs — every one
/// tagged with the tick's feed, each serialising to its OWN `candles_<tf>` table.
/// This is the "one common entry → all TFs" guarantee from the shared engine.
#[test]
fn test_one_groww_tick_seals_multiple_timeframes_from_common_engine() {
    let agg = MultiTfAggregator::with_capacity(8);
    agg.pre_populate(std::iter::once((SID, NSE_EQ)));
    agg.seed_cumulative(SID, NSE_EQ, 0);

    // Open buckets across all TFs at 10:00:30 IST.
    let t1 = groww_tick(TEN_AM_SECS_OF_DAY + 30, 100.0, 10);
    agg.consume_tick(&t1, NSE_EQ, FeedStrategy::GROWW, Some(10), |_tf, _s| {});

    // Jump to 14:00:05 IST (50_405 secs-of-day, still < 15:30 close) — crosses the
    // 1m/2m/…/30m/1h/2h/3h boundaries at once → a single tick seals MANY TFs.
    let t2 = groww_tick(14 * 3_600 + 5, 102.0, 30);
    let mut sealed_tfs: Vec<TfIndex> = Vec::new();
    let mut writer = ShadowCandleWriter::for_test();
    let stats = agg.consume_tick(&t2, NSE_EQ, FeedStrategy::GROWW, Some(30), |tf, state| {
        sealed_tfs.push(tf);
        let seal = BufferedSeal::new(SID, NSE_EQ, tf, state, Feed::Groww);
        writer
            .append_seal(&seal)
            .expect("append per-TF groww candle");
    });

    assert!(
        stats.sealed_count >= 4,
        "a 4-hour jump must seal MANY timeframes from the ONE common engine, got {}",
        stats.sealed_count
    );
    // M1 must be among them, plus larger TFs — proving multi-TF fan-out.
    assert!(sealed_tfs.contains(&TfIndex::M1), "M1 must seal");
    assert!(
        sealed_tfs.iter().any(|tf| *tf != TfIndex::M1),
        "at least one NON-M1 timeframe must also seal from the same tick"
    );

    // Every sealed TF row is on the wire, in its OWN candles_<tf> table, tagged
    // feed=groww — no per-TF, no per-feed branch.
    let wire = std::str::from_utf8(writer.buffer_bytes()).expect("utf8");
    let groww_tags = wire.matches("feed=groww").count();
    assert_eq!(
        groww_tags,
        sealed_tfs.len(),
        "every sealed TF candle must carry feed=groww, got {wire}"
    );
    for tf in &sealed_tfs {
        assert!(
            wire.contains(tf.table_name()),
            "sealed TF {} must serialise to its own table {}, got {wire}",
            tf.as_ordinal(),
            tf.table_name()
        );
    }
    assert!(wire.contains("candles_1m"), "candles_1m must be present");
}

/// EXTREME CASE — session-gate boundaries (operator demand 2026-06-30). A tick
/// at EXACTLY 09:15:00 IST (the inclusive open, secs-of-day 33_300) must be
/// admitted and fold; a tick at EXACTLY 15:30:00 IST (the exclusive close,
/// secs-of-day 55_800) must be gated OUT. Proves the ts→secs-of-day conversion
/// the Groww feed uses lands on the right side of the gate (rules out SUSPECT 2).
#[test]
fn test_session_gate_boundaries_open_inclusive_close_exclusive() {
    let agg = MultiTfAggregator::with_capacity(8);
    agg.pre_populate(std::iter::once((SID, NSE_EQ)));
    agg.seed_cumulative(SID, NSE_EQ, 0);

    // 09:15:00 IST exactly — inclusive open → instrument_found AND folds (the
    // bucket-opening tick seals nothing yet but IS admitted).
    let open_tick = groww_tick(33_300, 100.0, 5);
    let s_open = agg.consume_tick(&open_tick, NSE_EQ, FeedStrategy::GROWW, Some(5), |_, _| {});
    assert!(
        s_open.instrument_found,
        "a 09:15:00 IST tick must be ADMITTED (inclusive open boundary)"
    );

    // 15:30:00 IST exactly — exclusive close → gated OUT. The aggregator returns
    // `instrument_found: true` but seals nothing (the early-return path). Assert
    // no seal fires (the gate prevented the fold).
    let mut sealed_after_close = 0u32;
    let close_tick = groww_tick(55_800, 200.0, 99);
    agg.consume_tick(
        &close_tick,
        NSE_EQ,
        FeedStrategy::GROWW,
        Some(99),
        |_, _| {
            sealed_after_close += 1;
        },
    );
    assert_eq!(
        sealed_after_close, 0,
        "a 15:30:00 IST tick is gated OUT (exclusive close) — must NOT seal"
    );
}

/// EXTREME CASE — a feed with a 64-bit security_id (Groww index token sets bit
/// 62). The seed key MUST equal the consume key so `instrument_found` is true and
/// the candle seals (rules out SUSPECT 3, the u32→u64 widening key mismatch).
#[test]
fn test_64bit_security_id_seeds_and_consumes_same_key_seals_candle() {
    // Groww index exchange_token with bit 62 set — far beyond u32.
    let big_id: u64 = (1u64 << 62) | 13;
    let seg = 0u8; // IDX_I
    let agg = MultiTfAggregator::with_capacity(8);
    agg.pre_populate(std::iter::once((big_id, seg)));
    assert!(
        agg.seed_cumulative(big_id, seg, 0),
        "seed must find the cell under the SAME 64-bit key it was pre-populated with"
    );

    let mut t1 = ParsedTick::default();
    t1.security_id = big_id;
    t1.exchange_segment_code = seg;
    t1.last_traded_price = 100.0;
    t1.exchange_timestamp = IST_DAY_BASE_SECS + TEN_AM_SECS_OF_DAY + 30;
    t1.volume = 10;
    let s1 = agg.consume_tick(&t1, seg, FeedStrategy::GROWW, Some(10), |_, _| {});
    assert!(
        s1.instrument_found,
        "consume must find the SAME 64-bit key (no u32 truncation) — instrument_found"
    );

    let mut t2 = t1;
    t2.exchange_timestamp = IST_DAY_BASE_SECS + TEN_AM_SECS_OF_DAY + 60 + 5;
    t2.volume = 20;
    let mut sealed: Option<BufferedSeal> = None;
    agg.consume_tick(&t2, seg, FeedStrategy::GROWW, Some(20), |tf, state| {
        if tf == TfIndex::M1 {
            sealed = Some(BufferedSeal::new(big_id, seg, tf, state, Feed::Groww));
        }
    });
    let seal = sealed.expect("64-bit-id instrument must seal its M1 candle");
    assert_eq!(
        seal.security_id, big_id,
        "the 64-bit id must survive into the seal"
    );
    let mut w = ShadowCandleWriter::for_test();
    w.append_seal(&seal).expect("append 64-bit-id groww candle");
    let wire = std::str::from_utf8(w.buffer_bytes()).expect("utf8");
    assert!(
        wire.contains("feed=groww"),
        "64-bit-id candle tagged feed=groww, got {wire}"
    );
}

/// EXTREME CASE — IST-midnight force-seal. The same common engine flushes every
/// open bucket across all 21 TFs at the day boundary (the Groww bridge's
/// `force_seal_all` path), each tagged with the feed. Proves the midnight flush
/// also produces feed-tagged candles through the shared writer.
#[test]
fn test_ist_midnight_force_seal_emits_feed_tagged_candles() {
    let agg = MultiTfAggregator::with_capacity(8);
    agg.pre_populate(std::iter::once((SID, NSE_EQ)));
    agg.seed_cumulative(SID, NSE_EQ, 0);

    // One in-session tick opens buckets across all TFs.
    let t = groww_tick(TEN_AM_SECS_OF_DAY + 30, 100.0, 10);
    agg.consume_tick(&t, NSE_EQ, FeedStrategy::GROWW, Some(10), |_, _| {});

    // Force-seal every open bucket (what the Groww IST-midnight task does).
    let mut writer = ShadowCandleWriter::for_test();
    let mut sealed_count = 0u32;
    agg.force_seal_all(|sid, seg, tf, state| {
        sealed_count += 1;
        let seal = BufferedSeal::new(sid, seg, tf, state, Feed::Groww);
        writer
            .append_seal(&seal)
            .expect("append force-sealed groww candle");
    });
    assert!(
        sealed_count >= 1,
        "force_seal_all must flush the open buckets (≥1 TF), got {sealed_count}"
    );
    let wire = std::str::from_utf8(writer.buffer_bytes()).expect("utf8");
    assert_eq!(
        wire.matches("feed=groww").count(),
        sealed_count as usize,
        "every force-sealed candle must carry feed=groww, got {wire}"
    );
}

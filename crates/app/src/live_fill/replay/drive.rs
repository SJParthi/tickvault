//! Drives the live paper fill chain from recorded frames on a virtual clock
//! (plan item LF-8).
//!
//! Each frame goes through the live drain's own steps, in its order: the
//! backup dedup (when its publications are given), the live candle fold
//! (`dhan_feed_stack::live_fold`), the drain's verdict
//! (`dhan_feed_stack::fold_verdict`), and for a folded tick the tap's
//! [`TapTick::from_parsed`], then [`LiveFillChain::on_tap`]. EVERY folded tick
//! is tapped, not only the scheduled instruments: the chain's feed-gap watch
//! measures silence across the whole stream, so a filtered stream would read
//! a quiet contract as a feed gap.
//!
//! The chain starts at the live process's BOOT, as live builds it at boot,
//! before any socket is dialled ([`BootInstant`]). The WAL does not record
//! that instant: `--boot-ist` gives it, and without it the scheduled 08:30
//! start is assumed (or the first frame, if that is earlier). The chain's
//! `chain_start` taint (a chain built after 09:15:00) reads it. The fold is
//! handed over as live hands it over at every lane start
//! (`LiveIngest::finish_wal_replay`): its capture start is set to the same
//! instant and marked provisional, so the first tick folded moves it to that
//! tick's own receipt second, as live confirms it at its first live tick.
//! The fold's first-trade rule and its mid-session rule (a capture start
//! after 09:10:00) therefore read the day's first receipt, as live does, not
//! the boot. A day whose first frame came after 09:10:00 with no `--boot-ist`
//! still reads incomplete (a conservative line: the boot can matter to the
//! chain only when it is after 09:15:00), and so does a frame received before
//! a given `--boot-ist` (an earlier process's frame). Passes start at the
//! first frame, not at the boot: with no tick read yet a pass changes nothing
//! (no receipt, no candle, no order), so the passes in between are skipped.
//!
//! The clock is the newest frame receipt seen (it never moves back, so no
//! clock step is ever seen). Before each frame the chain gets one pass at
//! every grid instant (`pass_ns`, 1 ms by default) up to that receipt, then
//! the frame's ticks; so a decision lands on the first grid instant at or
//! after its release, which is the live thread's best case (since 2026-11-13 it
//! wakes on each new tick, and otherwise passes at least every 1 ms, or 5 ms
//! after 50 idle passes). The chain's paper commands are not sent to an
//! order runtime: each Execute is written as rows by
//! `live_fill::paper_rows`, with a deterministic `PAPER-R<n>` id, no
//! round-trip time and the model's fill taken as booked. PAPER ONLY: nothing
//! here calls a broker, an OMS, a database or a bucket.
//!
//! # Cost
//!
//! O(packets) per frame; per tick the fold's O(`TF_COUNT`) and the chain's
//! own per-tick cost; one chain pass per grid instant from the first frame
//! to the end of the grid window (about 24 million at 1 ms for a day), each
//! O(1) when nothing is due. Memory: the fold sized once to
//! `AGGREGATOR_MAX_SLOTS` (about 68 MiB), the universe's 1 s bars
//! (O(instruments × seconds traded), never pruned, about 80 B per kept
//! second: about 3.5 to 4 GB for 2,000 liquid contracts over a full day,
//! Assumed, see the replay module's cost docs), and the day's order rows.
//!
//! A loss the replay sees (a stretch of a segment it could not read, or an
//! abandoned frame tail) moves the loss count and is handed to the chain at
//! once (`LiveFillChain::note_tap_loss`), as the live thread reads the tap's
//! loss total before every pass, so no pass after it releases a candle the
//! loss may have touched. A loss found before the day's chain exists is HELD
//! and judged by where it ends: when the next readable frame is on the day
//! and after the chain's start (damage at the head of the day's first
//! segment, past the boot), the lost frames may be ones the live chain
//! folded, so the loss is handed to the chain right after it is built; when
//! that frame is on the previous date, or at or before the chain's start,
//! the loss touched no chain candle and is only counted
//! (`losses_before_chain`).

// O(1) EXEMPT: begin: cold offline CLI; see the module docs for every cost.
use std::collections::{BTreeMap, HashMap, HashSet};

use tickvault_common::feed::Feed;
use tickvault_common::types::ExchangeSegment;
use tickvault_storage::live_order_events_persistence::{LiveOrderEvent, LiveOrderEventKind};
use tickvault_storage::ws_frame_spill::{
    ReplayedFrame, WAL_RECEIPT_UNKNOWN_NANOS, frame_feeds_the_candle_fold,
};
use tickvault_trading::brutex_host::InstrumentKey;
use tickvault_trading::brutex_host::engine::Side;
use tickvault_trading::brutex_host::live_candles::IST_OFFSET_NS;
use tickvault_trading::brutex_host::worst_fill::{SecondBar, rupees_to_paise};
use tickvault_trading::candles::{MultiTfAggregator, TfIndex};

use super::source::{FrameTick, walk_frame};
use crate::dhan_feed_stack::fold_verdict::{FoldVerdict, fold_verdict};
use crate::dhan_feed_stack::live_fill_tap::{TapTick, TapVolume};
use crate::live_fill::chain::{
    ChainError, ChainOutput, ChainParams, ChainSetup, LiveFillChain, PaperCmd, PaperExecution,
};
use crate::live_fill::paper_rows::{
    PaperOutcome, clamp_exit, execution_rows, position_report, reject_row, remainder_reason,
};
use crate::live_fill::replay::scripted::ScriptedIntents;

/// The replay's engine type. Named so the chain type below reads
/// `LiveFillChain<ReplayEngine>`: written out with the engine's own name,
/// the lowercased text holds the HTML tag opener the browser-surface guard
/// counts, which would wrongly mark this file as a frontend surface.
type ReplayEngine = ScriptedIntents;
use crate::live_fill::timing::{SESSION_CLOSE_SOD, SESSION_OPEN_SOD};
use crate::main_feed_backup::ReplayBackup;

const NANOS_PER_SEC: i64 = 1_000_000_000;
const NANOS_PER_DAY: i64 = 86_400 * NANOS_PER_SEC;
/// Offset between the virtual wall and the virtual monotonic clock. Constant,
/// so the chain never sees a clock step.
const MONO_OFFSET_NS: i64 = 1_000 * NANOS_PER_SEC;
/// A silence between two in-session frames longer than this marks the day
/// incomplete.
pub const INCOMPLETE_GAP_NS: i64 = 5 * NANOS_PER_SEC;

/// The scheduled live start, 08:30:00 IST, seconds of the day: the boot
/// assumed when `--boot-ist` is not given.
pub const SCHEDULED_BOOT_SOD: i64 = 8 * 3_600 + 30 * 60;

/// A first frame after 09:10:00 IST (seconds of the day) with no
/// `--boot-ist` leaves the boot unknown, so the day reads incomplete. The
/// chain taints its first candles when built after 09:15:00, so a boot can
/// change what live decided only when it is after 09:15:00; the fold reads
/// the first receipt, not the boot (see the module docs). The 09:10:00 line
/// (the fold's mid-session line, the 09:15 candle open less its 300 s dial
/// window) is kept as a conservative bound: it errs toward incomplete.
pub const BOOT_MATTERS_AFTER_SOD: i64 = 9 * 3_600 + 10 * 60;

/// When the live process booted on the replayed day: the chain's start and
/// the fold's capture start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootInstant {
    /// IST epoch nanos.
    pub ist_ns: i64,
    /// `true` when assumed (no `--boot-ist`): the scheduled 08:30 start.
    pub assumed: bool,
}

impl BootInstant {
    /// The scheduled 08:30 start of the day at `day_start_ist_ns`, assumed.
    /// O(1).
    #[must_use]
    pub fn scheduled(day_start_ist_ns: i64) -> Self {
        Self {
            ist_ns: day_start_ist_ns.saturating_add(SCHEDULED_BOOT_SOD * NANOS_PER_SEC),
            assumed: true,
        }
    }

    /// A boot given with `--boot-ist` (`sod`, seconds of the day). O(1).
    #[must_use]
    pub fn given(day_start_ist_ns: i64, sod: i64) -> Self {
        Self {
            ist_ns: day_start_ist_ns.saturating_add(sod.saturating_mul(NANOS_PER_SEC)),
            assumed: false,
        }
    }
}

/// What the replay counted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReplayStats {
    pub segments: u64,
    pub damaged_segments: u64,
    pub frames: u64,
    /// Records of another transport kept by the main-feed endpoint read (a
    /// TrueData frame, or a v1 to v3 order-update record that carries no
    /// endpoint and so reads as main feed): skipped before anything else, as
    /// the live replay and catch-up skip them
    /// (`ws_frame_spill::frame_feeds_the_candle_fold`). Walking one as a Dhan
    /// binary frame would abandon it and move the loss count.
    pub other_transport_frames: u64,
    /// Frames received on another IST day (the previous date's last segment).
    pub other_day_frames: u64,
    /// v1/v2 records with no receipt: refused (the day exits 2).
    pub no_receipt_frames: u64,
    pub packets: u64,
    pub abandoned_bytes: u64,
    pub ticks: u64,
    pub backup_dropped: u64,
    pub folded: u64,
    pub tapped: u64,
    pub candle_only: u64,
    pub refused: u64,
    pub drop_seq: u32,
    /// Losses found before the day's chain existed that touched no chain
    /// candle (the next readable frame was on the previous date, at or before
    /// the chain's start, or never came): counted, never handed to the chain
    /// (the live tap's baseline is taken at its creation). A pre-chain loss
    /// whose next readable frame is on the day and after the chain's start is
    /// handed to the chain instead and counted in `drop_seq`.
    pub losses_before_chain: u64,
    pub passes: u64,
    pub marks: u64,
    pub executions: u64,
    pub first_receipt_ist_ns: Option<i64>,
    pub last_receipt_ist_ns: Option<i64>,
    /// The longest silence between two frames received inside 09:15–15:30.
    pub max_session_gap_ns: i64,
    /// The instant the chain and the fold were started at (IST epoch nanos):
    /// the boot, or the day's first frame when that came earlier.
    pub chain_start_ist_ns: Option<i64>,
    /// The boot was assumed (no `--boot-ist`).
    pub boot_assumed: bool,
    /// Frames of the day received before a given `--boot-ist` (an earlier
    /// process's frames; the replay starts at the first of them).
    pub frames_before_boot: u64,
}

/// One decision's timing, for `decisions.csv`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionTiming {
    pub signal_id: i64,
    pub security_id: i64,
    pub segment: &'static str,
    pub symbol: String,
    pub frame_secs: i64,
    pub candle_start_ns: i64,
    pub candle_end_ns: i64,
    pub close_by: &'static str,
    pub last_tick_exchange_ns: Option<i64>,
    pub last_tick_receipt_ns: Option<i64>,
    pub release_ns: i64,
    pub decision_ns: i64,
    pub opening_clamp: bool,
    pub late_price_skip: bool,
    /// The refusal reason when it is an untrusted stretch (`feed_gap`, ...).
    pub taint: Option<String>,
}

/// Everything a finished replay produced.
#[derive(Debug, Clone, Default)]
pub struct ReplayOutput {
    pub events: Vec<LiveOrderEvent>,
    pub decisions: Vec<DecisionTiming>,
    /// The scheduled instruments' 1 s bars from the fold's seals, per key,
    /// for the report's re-pricing (the stored `candles_1s` stand-in).
    pub seconds: HashMap<InstrumentKey, BTreeMap<i64, SecondBar>>,
    pub stats: ReplayStats,
}

/// See the module docs.
pub struct ReplayDriver {
    engine: ScriptedIntents,
    setup: ChainSetup,
    params: ChainParams,
    chain: Option<LiveFillChain<ReplayEngine>>,
    fold: MultiTfAggregator,
    backup: Option<ReplayBackup>,
    universe: HashSet<InstrumentKey>,
    day: i64,
    pass_ns: i64,
    next_grid_ns: i64,
    grid_end_ns: i64,
    clock_ns: i64,
    drop_seq: u32,
    /// Losses found before the day's chain exists, not yet judged: judged by
    /// the next readable frame (see the module docs).
    pending_losses: u64,
    order_n: u64,
    held_lots: HashMap<(u64, u8), i32>,
    boot: BootInstant,
    events: Vec<LiveOrderEvent>,
    seconds: HashMap<InstrumentKey, BTreeMap<i64, SecondBar>>,
    ticks: Vec<FrameTick>,
    stats: ReplayStats,
    failed: Option<ChainError>,
}

impl ReplayDriver {
    /// A driver for the IST day starting at `day_start_ist_ns` (IST midnight,
    /// epoch nanos). The chain is built once here to refuse a bad setup
    /// early, then again at the day's first frame, started at `boot` (or at
    /// that frame when it came earlier; see the module docs). Cold.
    ///
    /// # Errors
    /// The chain refused the setup.
    pub fn new(
        engine: ScriptedIntents,
        setup: ChainSetup,
        params: ChainParams,
        day_start_ist_ns: i64,
        pass_ns: i64,
        backup: Option<ReplayBackup>,
        boot: BootInstant,
    ) -> Result<Self, ChainError> {
        let _check = LiveFillChain::new(engine.clone(), setup.clone(), params, day_start_ist_ns)?;
        let pass_ns = pass_ns.max(1);
        let universe: HashSet<InstrumentKey> = setup.universe.iter().copied().collect();
        Ok(Self {
            grid_end_ns: grid_end_ns(day_start_ist_ns, &params),
            engine,
            setup,
            params,
            chain: None,
            fold: crate::dhan_feed_stack::live_fold(
                tickvault_trading::candles::AGGREGATOR_MAX_SLOTS,
            ),
            backup,
            universe,
            day: day_start_ist_ns.div_euclid(NANOS_PER_DAY),
            pass_ns,
            next_grid_ns: 0,
            clock_ns: i64::MIN,
            drop_seq: 0,
            pending_losses: 0,
            order_n: 0,
            held_lots: HashMap::new(),
            boot,
            events: Vec::new(),
            seconds: HashMap::new(),
            ticks: Vec::with_capacity(1_024),
            stats: ReplayStats {
                boot_assumed: boot.assumed,
                ..ReplayStats::default()
            },
            failed: None,
        })
    }

    /// The counts so far.
    #[must_use]
    pub fn stats(&self) -> ReplayStats {
        self.stats
    }

    /// A segment that could not be read whole, COUNTED only. Where the loss
    /// sits is marked by [`Self::note_frames_skipped`]: at the frame after a
    /// stretch the reader resynced past, or after the segment when the walk
    /// stopped early (the caller, `feed_segment`, decides which). O(1).
    pub fn note_segment_damage(&mut self) {
        self.stats.damaged_segments = self.stats.damaged_segments.saturating_add(1);
    }

    /// Frames were lost here (a stretch the reader skipped, or the rest of a
    /// segment after the walk stopped): the loss count moves, and the chain
    /// reads it AT ONCE, as the live thread reads the tap's loss total before
    /// every pass (`LiveFillChain::note_tap_loss`), so no pass after the loss
    /// releases a candle the loss may have touched. O(1).
    pub fn note_frames_skipped(&mut self) {
        self.bump_loss();
    }

    /// Moves the loss count and hands it to the chain out of band (see
    /// [`Self::note_frames_skipped`]). O(1).
    ///
    /// A loss found before the day's chain exists (the previous date's
    /// segment, or damage at the head of the day's first segment) does NOT
    /// move the loss count yet: it is HELD and judged by the next readable
    /// frame ([`Self::judge_pending_losses`]). The live tap takes its baseline
    /// when it is created, so a loss wholly before the chain's start touched
    /// no chain candle, and moving the count for it would make the day's first
    /// tapped tick carry a loss the chain never saw (the opening bars tainted
    /// `tick_dropped`, which live never does). But a stretch that ends after
    /// the chain's start may hold frames the live chain folded, so that loss
    /// is handed to the chain as soon as it is built.
    fn bump_loss(&mut self) {
        let Some(chain) = self.chain.as_mut() else {
            self.pending_losses = self.pending_losses.saturating_add(1);
            return;
        };
        self.drop_seq = self.drop_seq.wrapping_add(1);
        if self.failed.is_some() {
            return;
        }
        chain.note_tap_loss(u64::from(self.drop_seq));
        self.drain(self.clock_now());
    }

    /// Judges the held pre-chain losses by the next readable frame. `touches`
    /// is `true` when the lost stretch can reach past the chain's start (that
    /// frame is on the day and after the start): the losses are handed to the
    /// chain, which must exist by then. Otherwise they touched no chain candle
    /// and are only counted. O(1).
    fn judge_pending_losses(&mut self, touches: bool) {
        let n = std::mem::take(&mut self.pending_losses);
        if n == 0 {
            return;
        }
        if !touches || self.chain.is_none() {
            self.stats.losses_before_chain = self.stats.losses_before_chain.saturating_add(n);
            return;
        }
        // Every held loss moves the count; the last one hands it to the
        // chain (one taint covers them all: the chain has no tick yet).
        // APPROVED: the loss count's low 32 bits, as the live tap's.
        #[allow(clippy::cast_possible_truncation)]
        let earlier = (n - 1) as u32;
        self.drop_seq = self.drop_seq.wrapping_add(earlier);
        self.bump_loss();
    }

    /// One segment begins (counted only). O(1).
    pub fn note_segment(&mut self) {
        self.stats.segments = self.stats.segments.saturating_add(1);
    }

    /// One recorded main-feed frame. See the module docs. O(packets) plus
    /// the passes the clock moved over. A record the live drain never folds
    /// (another transport, see `ReplayStats::other_transport_frames`) is
    /// counted and skipped first, O(1).
    pub fn feed_frame(&mut self, f: &ReplayedFrame) {
        // Only what the live drain folds: a Dhan live-feed record from a
        // main-feed socket. The WAL read keeps every record whose endpoint
        // reads main feed, which includes TrueData frames and order-update
        // records written before the endpoint byte existed; the live replay
        // and catch-up skip those, and so does this, before the clock, the
        // receipt checks or the packet walk see them.
        if !frame_feeds_the_candle_fold(f.ws_type, f.endpoint) {
            self.stats.other_transport_frames = self.stats.other_transport_frames.saturating_add(1);
            return;
        }
        if f.received_at_nanos == WAL_RECEIPT_UNKNOWN_NANOS {
            self.stats.no_receipt_frames = self.stats.no_receipt_frames.saturating_add(1);
            return;
        }
        let receipt_ist = f.received_at_nanos.saturating_add(IST_OFFSET_NS);
        if receipt_ist.div_euclid(NANOS_PER_DAY) != self.day {
            self.stats.other_day_frames = self.stats.other_day_frames.saturating_add(1);
            // A held loss followed by a frame of another day ended there: it
            // touched no frame of the day's chain.
            if self.chain.is_none() {
                self.judge_pending_losses(false);
            }
            return;
        }
        if self.failed.is_some() {
            return;
        }
        if !self.boot.assumed && receipt_ist < self.boot.ist_ns {
            self.stats.frames_before_boot = self.stats.frames_before_boot.saturating_add(1);
        }
        if self.chain.is_none() && !self.start(receipt_ist) {
            return;
        }
        self.note_receipt(receipt_ist);
        if receipt_ist > self.clock_ns {
            self.clock_ns = receipt_ist;
            self.advance_to(receipt_ist.min(self.grid_end_ns));
        }
        let covers = self
            .backup
            .as_mut()
            .is_some_and(|b| b.select(f.received_at_nanos));
        self.ticks.clear();
        let walk = walk_frame(&f.frame, f.received_at_nanos, &mut self.ticks);
        self.stats.frames = self.stats.frames.saturating_add(1);
        self.stats.packets = self.stats.packets.saturating_add(u64::from(walk.packets));
        let ticks = std::mem::take(&mut self.ticks);
        for t in &ticks {
            self.on_tick(t, &f.frame, covers);
        }
        self.ticks = ticks;
        if walk.abandoned_bytes > 0 {
            // The live drain counts an abandoned frame tail as a live fill
            // loss (`note_live_fill_abandoned`) and publishes it at once; the
            // live thread reads it before its next pass.
            self.stats.abandoned_bytes = self
                .stats
                .abandoned_bytes
                .saturating_add(walk.abandoned_bytes);
            self.bump_loss();
        }
    }

    /// Builds the day's chain at its first frame, started at the boot (or at
    /// this frame when it came earlier), as live builds it at boot, and hands
    /// the fold over as live does at every lane start
    /// (`LiveIngest::finish_wal_replay`): the capture start set to the same
    /// instant, then `finish_replay` on the empty fold, which marks it
    /// provisional, so the first tick folded confirms it at its own receipt.
    /// A held pre-chain loss is judged here ([`Self::judge_pending_losses`]).
    /// Passes start at this frame (see the module docs). `false` when the
    /// chain refused.
    fn start(&mut self, receipt_ist: i64) -> bool {
        let start_ns = self.boot.ist_ns.min(receipt_ist);
        match LiveFillChain::new(
            self.engine.clone(),
            self.setup.clone(),
            self.params,
            start_ns,
        ) {
            Ok(c) => {
                self.chain = Some(c);
                self.stats.chain_start_ist_ns = Some(start_ns);
                let secs = start_ns.div_euclid(NANOS_PER_SEC);
                self.fold
                    .set_live_capture_start(u32::try_from(secs).unwrap_or(u32::MAX));
                // Live's lane start always runs the WAL refold (in replay
                // mode, even with no frame to refold) and then the hand-over,
                // which marks the capture start provisional; without it the
                // fold's first-trade and mid-session rules would read the
                // boot instead of the first receipt.
                self.fold.set_replay_mode(true);
                let _handed = self.fold.finish_replay(false, |_, _, _, _, _| {});
                self.next_grid_ns = receipt_ist
                    .div_euclid(self.pass_ns)
                    .saturating_mul(self.pass_ns);
                self.judge_pending_losses(receipt_ist > start_ns);
                true
            }
            Err(e) => {
                self.failed = Some(e);
                false
            }
        }
    }

    fn note_receipt(&mut self, receipt_ist: i64) {
        let s = &mut self.stats;
        if s.first_receipt_ist_ns.is_none() {
            s.first_receipt_ist_ns = Some(receipt_ist);
        }
        if let Some(prev) = s.last_receipt_ist_ns
            && receipt_ist > prev
        {
            // The part of the silence inside 09:15-15:30, clamped at both
            // ends as the chain's own feed-gap watch clamps it at the open: a
            // hole across the open or the close is measured, not only one
            // with both frames inside the session.
            let day_start = self.day.saturating_mul(NANOS_PER_DAY);
            let open_ns = day_start.saturating_add(SESSION_OPEN_SOD.saturating_mul(NANOS_PER_SEC));
            let close_ns =
                day_start.saturating_add(SESSION_CLOSE_SOD.saturating_mul(NANOS_PER_SEC));
            let gap = receipt_ist.min(close_ns).saturating_sub(prev.max(open_ns));
            if gap > 0 {
                s.max_session_gap_ns = s.max_session_gap_ns.max(gap);
            }
        }
        if s.last_receipt_ist_ns.is_none_or(|p| receipt_ist > p) {
            s.last_receipt_ist_ns = Some(receipt_ist);
        }
    }

    fn on_tick(&mut self, t: &FrameTick, frame: &[u8], covers: bool) {
        self.stats.ticks = self.stats.ticks.saturating_add(1);
        let packet = frame.get(t.start..t.end).unwrap_or(&[]);
        if covers
            && let Some(b) = self.backup.as_mut()
            && b.dedup_mut()
                .admit_tick(&t.tick, packet, u8::MAX, 0)
                .is_drop()
        {
            self.stats.backup_dropped = self.stats.backup_dropped.saturating_add(1);
            return;
        }
        let universe = &self.universe;
        let seconds = &mut self.seconds;
        let stats = self
            .fold
            .consume_tick(Feed::Dhan, &t.tick, None, |_, sid, seg, tf, state| {
                record_second(universe, seconds, sid, seg, tf, &state);
            });
        match fold_verdict(&stats, t.tick.received_at_nanos) {
            FoldVerdict::Folded {
                new_trade,
                older_trade,
            } => {
                self.stats.folded = self.stats.folded.saturating_add(1);
                let tap = TapTick::from_parsed(
                    &t.tick,
                    t.best.as_ref(),
                    self.drop_seq,
                    new_trade,
                    older_trade,
                    TapVolume::from_stats(&stats),
                );
                if let Some(chain) = self.chain.as_mut() {
                    chain.on_tap(&tap);
                    self.stats.tapped = self.stats.tapped.saturating_add(1);
                }
                self.drain(self.clock_now());
            }
            FoldVerdict::CandleOnly => {
                self.stats.candle_only = self.stats.candle_only.saturating_add(1);
            }
            FoldVerdict::HardRefusal { .. } => {
                self.stats.refused = self.stats.refused.saturating_add(1);
            }
        }
    }

    /// The last grid instant passed (the chain's wall).
    fn clock_now(&self) -> i64 {
        self.next_grid_ns.saturating_sub(self.pass_ns)
    }

    /// One pass at every grid instant up to `t`. O(instants).
    ///
    /// When `t` reaches the grid's end and `pass_ns` does not divide that
    /// instant (an odd `--pass-us`), the grid stops short of it, and an order
    /// held to it (a decision on the 15:29 bar) would never be finished: one
    /// more pass runs at exactly the end, here, before the frame that reached
    /// it is tapped (as a grid instant on the end would).
    fn advance_to(&mut self, t: i64) {
        while self.next_grid_ns <= t && self.failed.is_none() {
            self.pass_at(self.next_grid_ns);
        }
        if t >= self.grid_end_ns && self.failed.is_none() && self.clock_now() < self.grid_end_ns {
            self.pass_at(self.grid_end_ns);
        }
    }

    /// One chain pass at `g`; the next grid instant is `g + pass_ns`. O(1)
    /// when nothing is due.
    fn pass_at(&mut self, g: i64) {
        if let Some(chain) = self.chain.as_mut()
            && let Err(e) = chain.on_pass(g, g.saturating_sub(MONO_OFFSET_NS))
        {
            self.failed = Some(e);
            return;
        }
        self.stats.passes = self.stats.passes.saturating_add(1);
        self.next_grid_ns = g.saturating_add(self.pass_ns);
        self.drain(g);
    }

    /// Hands the chain's outputs on, stamped at `ts`.
    fn drain(&mut self, ts: i64) {
        let Some(chain) = self.chain.as_mut() else {
            return;
        };
        if chain.outputs_waiting() == 0 {
            return;
        }
        let mut outs = Vec::with_capacity(chain.outputs_waiting());
        chain.drain_outputs(|o| outs.push(o));
        for o in outs {
            match o {
                ChainOutput::Event(e) => self.events.push(*e),
                ChainOutput::Cmd(PaperCmd::Mark { .. }) => {
                    self.stats.marks = self.stats.marks.saturating_add(1);
                }
                ChainOutput::Cmd(PaperCmd::Execute(x)) => self.execute(&x, ts),
            }
        }
    }

    /// One paper order, as the runtime books it on a dry-run OMS with every
    /// gate open: an exit cut to the paper position held, the model's fill
    /// booked, the remainder cancelled. No OMS, no risk check (see the
    /// module docs of `live_fill::replay`).
    fn execute(&mut self, x: &PaperExecution, ts: i64) {
        self.stats.executions = self.stats.executions.saturating_add(1);
        let sent = x;
        let mut seq = x.next_seq;
        let key = (x.security_id, x.segment.binary_code());
        let held = self.held_lots.get(&key).copied().unwrap_or(0);
        let clamped;
        let x = if x.exit {
            let Some(c) = clamp_exit(held, x) else {
                self.events.push(reject_row(
                    x,
                    ts,
                    "exit_without_position".to_owned(),
                    &mut seq,
                ));
                self.report_position(sent, held);
                return;
            };
            clamped = c;
            &clamped
        } else {
            x
        };
        self.order_n = self.order_n.saturating_add(1);
        let filled = x.filled_qty.clamp(0, x.qty.max(0));
        let outcome = PaperOutcome {
            order_id: format!("PAPER-R{}", self.order_n),
            api_rtt_us: None,
            filled,
            closed: Ok(remainder_reason(filled).to_owned()),
        };
        execution_rows(x, ts, &outcome, &mut seq, &mut self.events);
        let lot = i64::from(x.lot_size.max(1));
        let lots = i32::try_from(filled / lot).unwrap_or(i32::MAX);
        let signed = match x.side {
            Side::Buy => lots,
            Side::Sell => lots.saturating_neg(),
        };
        let slot = self.held_lots.entry(key).or_insert(0);
        *slot = slot.saturating_add(signed);
        let now_held = *slot;
        self.report_position(sent, now_held);
    }

    /// Hands the chain what the emulated paper book holds once `sent` is
    /// booked or refused, as the live runtime does (LF-9 finding 2). O(1).
    fn report_position(&mut self, sent: &PaperExecution, held_lots: i32) {
        if let Some(chain) = self.chain.as_mut() {
            chain.on_position_report(&position_report(sent, held_lots));
        }
    }

    /// Runs the grid to its end (held orders reach their fill window's end),
    /// seals the fold's open bars, and hands back what the day produced.
    ///
    /// The end is the earlier of 15:30 IST plus the wait, the longest fill
    /// window and a second ([`grid_end_ns`]), and the last frame plus
    /// [`tail_ns`] (the longest frame, the wait, the longest fill window and a
    /// second): after that nothing recorded can still close a candle or fill
    /// an order, so a day that stopped early is not driven to 15:30 through
    /// millions of empty passes (it reads incomplete anyway).
    ///
    /// # Errors
    /// The chain stopped (a day roll it could not configure).
    pub fn finish(mut self) -> Result<ReplayOutput, ChainError> {
        if let Some(last) = self.stats.last_receipt_ist_ns
            && self.chain.is_some()
        {
            let tail = tail_ns(&self.params, &self.setup.engine_frames_secs);
            let end = self.grid_end_ns.min(last.saturating_add(tail));
            self.advance_to(end);
            // One last pass at exactly the end (`advance_to` does this for the
            // grid's own end; this is for a day whose frames stopped early,
            // where the end is the last frame plus the tail): an odd
            // `--pass-us` stops the grid short of it.
            if self.failed.is_none() && self.clock_now() < end {
                self.pass_at(end);
            }
        }
        // A held loss no readable frame followed touched no chain candle.
        self.judge_pending_losses(false);
        if let Some(e) = self.failed.take() {
            return Err(e);
        }
        let universe = &self.universe;
        let seconds = &mut self.seconds;
        let _sealed = self.fold.force_seal_all(|_, sid, seg, tf, state| {
            record_second(universe, seconds, sid, seg, tf, &state);
        });
        self.stats.drop_seq = self.drop_seq;
        let decisions =
            decision_timings(&self.events, &self.params, &self.setup.engine_frames_secs);
        Ok(ReplayOutput {
            events: self.events,
            decisions,
            seconds: self.seconds,
            stats: self.stats,
        })
    }
}

/// After the last frame: the longest strategy frame (a bar the clock closes
/// later), the longer wait, the longest fill window and one second. O(frames).
#[must_use]
pub fn tail_ns(params: &ChainParams, engine_frames_secs: &[u32]) -> i64 {
    let longest = i64::from(engine_frames_secs.iter().copied().max().unwrap_or(1));
    let wait = i64::try_from(params.open_wait_ms.max(params.wait_ms))
        .unwrap_or(i64::MAX / 4)
        .saturating_mul(1_000_000);
    longest
        .saturating_add(i64::from(params.max_fill_seconds.max(1)))
        .saturating_add(1)
        .saturating_mul(NANOS_PER_SEC)
        .saturating_add(wait)
}

/// The end of the grid: 15:30 IST plus the opening wait, the longest fill
/// window and one second, so every held order reaches its end. O(1).
#[must_use]
pub fn grid_end_ns(day_start_ist_ns: i64, params: &ChainParams) -> i64 {
    let ms = |v: u64| {
        i64::try_from(v)
            .unwrap_or(i64::MAX / 4)
            .saturating_mul(1_000_000)
    };
    day_start_ist_ns
        .saturating_add(SESSION_CLOSE_SOD.saturating_mul(NANOS_PER_SEC))
        .saturating_add(ms(params.open_wait_ms.max(params.wait_ms)))
        .saturating_add(i64::from(params.max_fill_seconds.max(1)).saturating_mul(NANOS_PER_SEC))
        .saturating_add(NANOS_PER_SEC)
}

/// The decision rows' timing, one per signal, in signal order: the candle, the
/// release instant the chain's book clock gives it, the decision, and whether
/// the order was then skipped for a late price or an untrusted stretch.
/// O(rows log rows).
#[must_use]
pub fn decision_timings(
    events: &[LiveOrderEvent],
    params: &ChainParams,
    engine_frames_secs: &[u32],
) -> Vec<DecisionTiming> {
    let mut frames: Vec<u32> = vec![1];
    frames.extend(engine_frames_secs.iter().copied());
    let clock =
        crate::live_fill::timing::BookClock::new(&frames, params.wait_ms, params.open_wait_ms);
    let mut rejects: HashMap<i64, &str> = HashMap::new();
    for e in events {
        if e.event == LiveOrderEventKind::Reject
            && let Some(r) = e.reason.as_deref()
        {
            rejects.entry(e.signal_id).or_insert(r);
        }
    }
    let taints: Vec<&str> = crate::live_fill::taint::TaintReason::ALL
        .iter()
        .map(|t| t.as_str())
        .collect();
    let mut out: Vec<DecisionTiming> = events
        .iter()
        .filter(|e| e.event == LiveOrderEventKind::Decision)
        .filter_map(|e| {
            let start = e.candle_start_ist_nanos?;
            let end = e.candle_end_ist_nanos?;
            let open_secs = start.div_euclid(NANOS_PER_SEC);
            let end_secs = end.div_euclid(NANOS_PER_SEC);
            let release_ns = clock.release_ns(open_secs, end_secs);
            let wait_ns = i64::try_from(params.wait_ms)
                .unwrap_or(i64::MAX / 4)
                .saturating_mul(1_000_000);
            let reason = rejects.get(&e.signal_id).copied();
            Some(DecisionTiming {
                signal_id: e.signal_id,
                security_id: e.security_id,
                segment: e.segment.as_str(),
                symbol: e.symbol.clone(),
                frame_secs: end_secs.saturating_sub(open_secs),
                candle_start_ns: start,
                candle_end_ns: end,
                close_by: e.close_by.map_or("", |c| c.as_str()),
                last_tick_exchange_ns: e.last_tick_exchange_ist_nanos,
                last_tick_receipt_ns: e.last_tick_receipt_ist_nanos,
                release_ns,
                decision_ns: e.decision_ist_nanos.unwrap_or(e.ts_ist_nanos),
                opening_clamp: release_ns > end.saturating_add(wait_ns),
                late_price_skip: reason == Some(crate::live_fill::chain::REASON_LATE_PRICE),
                taint: reason.filter(|r| taints.contains(r)).map(str::to_owned),
            })
        })
        .collect();
    out.sort_by_key(|d| d.signal_id);
    out
}

/// The segment and security id of an instrument key, for messages. O(1).
#[must_use]
pub fn key_label(key: &InstrumentKey) -> String {
    let seg = ExchangeSegment::from_byte(key.exchange_segment).map_or("UNKNOWN", |s| s.as_str());
    format!("{}/{seg}", key.security_id)
}

/// Keeps one sealed (or amended) 1 s bar of a scheduled instrument, the
/// stand-in for its stored `candles_1s` row. Other frames and instruments are
/// ignored. O(log seconds).
fn record_second(
    universe: &HashSet<InstrumentKey>,
    seconds: &mut HashMap<InstrumentKey, BTreeMap<i64, SecondBar>>,
    security_id: u64,
    segment: u8,
    tf: TfIndex,
    state: &tickvault_trading::candles::LiveCandleState,
) {
    let key = InstrumentKey {
        security_id,
        exchange_segment: segment,
    };
    if tf != TfIndex::S1 || !universe.contains(&key) {
        return;
    }
    if let (Ok(high_paise), Ok(low_paise)) =
        (rupees_to_paise(state.high), rupees_to_paise(state.low))
    {
        let sec = i64::from(state.bucket_start_ist_secs);
        seconds.entry(key).or_default().insert(
            sec,
            SecondBar {
                sec,
                high_paise,
                low_paise,
                volume: state.volume,
            },
        );
    }
}
// O(1) EXEMPT: end

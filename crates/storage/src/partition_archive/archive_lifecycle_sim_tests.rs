//! Randomized 30-day simulation of the archive protocol (plan item PR AA
//! step 6, AA-7; owner Quote 51, 2026-11-07).
//!
//! # What runs for real, and what is modelled
//!
//! REAL production code in every simulated partition:
//! [`archive_with_snapshot_gate`] (the whole per-partition protocol: pre-wait,
//! quiesce hold, in-flight check, snapshot under the lock, copy, fresh
//! snapshot, recount, drop), [`recheck_and_drop`], [`hour_in_flight_verdict`]
//! over a real [`DeferredDepth`] mark table and the real
//! [`hour_frame_seq_span`], the real table selection
//! ([`hour_window_for`], [`hot_hour_floor`]) and the hour offset read out of
//! the real SQL builder ([`build_closed_hour_list_sql`]).
//!
//! MODELLED (a simplified world behind [`ArchiveIo`]): QuestDB (one WAL per
//! table, `sequencerTxn` / `writerTxn`, rows visible once applied), S3 (a key
//! to row-batch map, a different copy goes to a sidecar key, never over an
//! existing one), the old-hour writers (spill replay, the after-close depth
//! pass, the post-stall rescue), the small writers that keep committing into
//! the current hour after the close, the tick sink's acknowledgements, and
//! QuestDB dropping a partition with committed but unapplied rows of it
//! (assumed the worst: those rows are lost, owner decision 2 is still open).
//! The pass lock (`ARCHIVE_PASS_LOCK`) is not exercised: passes run one after
//! another here, which is what that lock enforces.
//!
//! # The invariants checked in every case
//!
//! 1. `UNCOPIED_DROP`: a drop never removes a row its S3 copy lacks.
//! 2. `LOSS`: every received row ends up in S3 or on the box (WAL, spill,
//!    QuestDB), never discarded.
//! 3. `UNAPPLIED_DROP`: no committed row of the hour is still unapplied at
//!    its drop.
//! 4. `INFLIGHT_DROP`: no row of the hour is still on its way (received, not
//!    acknowledged) at its drop.
//! 5. `MARKED_DROP`: no after-close depth row of the hour is still waiting.
//! 6. `FLOOR`: a dropped hour ended at least 4 h before the drop.
//! 7. `EXEMPT_TABLE`: candles and audit tables are never dropped (the model
//!    reports every table's WAL, so a selection that let them onto the hour
//!    path would reach the drop; bite-tested).
//! 8. `DISK`: on a clean night every hour that started at least
//!    `HOT_HOURS + 1 + DISK_CHECK_MARGIN_HOURS` (12) hours before the last
//!    pass (about 06:xx) is gone by 07:00, i.e. every session hour up to
//!    15:00 and a Muhurat 18:00 hour, except the stated cases (a row still in
//!    flight or waiting for the depth pass, next to such an hour for a depth
//!    table, an hour whose last frame the sink has not acknowledged past
//!    (usually the session's last hour), the mark table overflowed, an upload
//!    over the single-object limit). The horizon is fixed from the floor,
//!    never from the hour offset under test.
//!
//! # Bite tests
//!
//! Each known-bad variant ([`Mutation`]) must break its invariant on at
//! least one seed: the old exact-equality gate (DISK), no quiesce lock
//! (LOSS), reading S before the lock (LOSS), no acknowledged-watermark check
//! (INFLIGHT_DROP), the all-night depth hold (DISK), the start-based hour
//! SQL (FLOOR), every table on the hour path (EXEMPT_TABLE), an hour SQL
//! that lists hours 8 h late (DISK).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use chrono::{Datelike, NaiveDate, NaiveDateTime};
use proptest::prelude::*;
use tickvault_common::config::PartitionRetentionConfig;
use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;

use super::hour_path::{hot_hour_floor, hour_window_for};
use super::snapshot_gate::{
    ArchiveIo, CopiedPartition, HourFrameSpan, InFlightInputs, InFlightVerdict, SnapshotGate,
    WalSnapshotVerdict, archive_with_snapshot_gate, hour_frame_seq_span, hour_in_flight_verdict,
    recheck_and_drop, snapshot_target, wal_snapshot_verdict,
};
use super::{ArchiveOutcome, ArchiveRunSummary, PartitionStep, RangeTally, verify_archive};
use crate::partition_manager::build_closed_hour_list_sql;
use crate::wal_applied_watermark::{AppliedSnapshot, REPLAY_REORDER_SLACK_SEQ};
use crate::wal_deferred_depth::{
    DEFERRED_BUCKET_SHIFT, DEFERRED_BUCKETS, DeferredDepth, DeferredDepthSnapshot, ShedDepth,
};
use crate::wal_suspension_watcher::WalTableRow;

const NANOS: u64 = 1_000_000_000;
const HOUR_SECS: i64 = 3_600;
/// Transactions QuestDB applies per snapshot poll in the model.
const APPLY_PER_PAUSE: u64 = 1_000;
/// The configured hour window (`depth_hot_hours`).
const HOT_HOURS: u32 = 4;
/// Rows of one hour above which its upload is refused (the single PutObject
/// limit, stated case).
const TOO_BIG_ROWS: u64 = 60_000;
/// A clean night's DISK check covers hours eligible at least this long
/// before the last pass (several passes to retry a random failure). PR AA
/// review: 7, not 11, so the session's last hour (15:00, eligible about
/// 20:00) and a Muhurat 18:00 hour are checked the morning after; the limit
/// is anchored to the floor (`HOT_HOURS + 1`), never to the hour offset the
/// code under test produced.
const DISK_CHECK_MARGIN_HOURS: i64 = 7;
/// How many hours late [`Mutation::LateHourSql`] lists an hour.
const LATE_SQL_EXTRA_HOURS: i64 = 8;

/// The tables of the simulation: the two hour-path tables the plan moves
/// first, and two tables the hour path must never touch.
const TABLES: [(&str, bool); 4] = [
    ("market_depth", true),
    ("tick_quotes", false),
    ("candles_1m", false),
    ("order_audit", false),
];

/// A known-bad variant of the protocol for the bite tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mutation {
    None,
    ExactGate,
    NoLock,
    SBeforeLock,
    NoAckCheck,
    AllNightDepthHold,
    StartBasedSql,
    /// The table selection lets every table onto the hour path (PR AA
    /// review): candles and audit hours are dropped.
    HourPathOnEveryTable,
    /// The hour SQL lists an hour 8 h later than it should (PR AA review):
    /// the day's afternoon hours stay on the box past the morning.
    LateHourSql,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchState {
    /// Received, not yet acknowledged by its sink (a stall).
    InFlight,
    /// In a spill file nothing tracks (an untracked old-hour writer).
    Spilled,
    /// Shed under load; the after-close depth pass writes it back.
    MarkPending,
    /// In QuestDB's WAL as this transaction.
    Committed(u64),
    /// Dropped from the box after its copy.
    Removed,
    /// Lost: dropped while unapplied.
    Discarded,
}

#[derive(Debug, Clone)]
struct Batch {
    table: usize,
    hour: i64,
    seq: u64,
    rows: u64,
    state: BatchState,
}

#[derive(Debug, Default)]
struct Wal {
    sequencer: u64,
    writer: u64,
    /// `sequencer` at the previous hour step: QuestDB has applied at least
    /// that much an hour later (a lag is transient, never permanent).
    previous_step: u64,
}

/// Inputs of one simulated case.
#[derive(Debug, Clone, Copy)]
struct Params {
    seed: u64,
    start_offset_days: i64,
    lag_txns: u64,
    background_pct: u64,
}

/// The night's conditions; S3 down, a clock step or a flaky probe make it
/// unclean (no DISK check).
#[derive(Debug, Default, Clone, Copy)]
struct Night {
    s3_down: bool,
    clock_step_secs: i64,
    writers_busy_until_hour: i64,
    marks_can_finish: bool,
    overflow: bool,
    crash_pending: bool,
    probe_flaky: bool,
}

#[derive(Debug, Default, Clone, Copy)]
struct Day {
    session: bool,
    muhurat: bool,
    volume_pct: u64,
    stall: Option<(i64, i64)>,
    shed_pct: u64,
    spill_pct: u64,
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
    fn pct(&mut self, p: u64) -> bool {
        self.below(100) < p
    }
}

struct World {
    rng: Rng,
    params: Params,
    mutation: Mutation,
    /// IST midnight of day 0, as Unix seconds of the IST wall clock.
    base_ist: i64,
    walls: Vec<Wal>,
    batches: Vec<Batch>,
    by_hour: HashMap<(usize, i64), Vec<usize>>,
    /// `(hour, table)` with a committed row on the box, oldest first.
    live: BTreeSet<(i64, usize)>,
    pending: Vec<usize>,
    s3: HashMap<String, BTreeSet<usize>>,
    sidecars: HashMap<String, u32>,
    holds: Arc<AtomicBool>,
    /// The true time now, IST seconds.
    now_ist: i64,
    night: Night,
    day: Day,
    writers_open: bool,
    stall_active: bool,
    /// Highest frame acknowledged per sink: depth, ticks.
    acked_max: [u64; 2],
    max_seq: u64,
    too_big: HashSet<(usize, i64)>,
    violations: Vec<String>,
    cfg: PartitionRetentionConfig,
    /// Hours back from now that the hour SQL selects by its START.
    hour_offset: i64,
    /// Bumped whenever the set of waiting depth marks changes.
    marks_version: u64,
    /// The real mark table's snapshot for `(marks_version, overflow)`.
    marks_cache: std::cell::RefCell<Option<(u64, bool, DeferredDepthSnapshot)>>,
}

impl World {
    fn new(params: Params, mutation: Mutation) -> Self {
        let base = NaiveDate::from_ymd_opt(2026, 10, 20)
            .and_then(|d| d.checked_add_signed(chrono::TimeDelta::days(params.start_offset_days)))
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .expect("base date");
        let cfg = PartitionRetentionConfig {
            depth_hot_hours: HOT_HOURS,
            under_disk_pressure: true,
            pressure_hour_tables_enabled: true,
            ..PartitionRetentionConfig::default()
        };
        let floor = hot_hour_floor(HOT_HOURS).expect("hour floor on");
        let real_offset = sql_hour_offset(&build_closed_hour_list_sql("market_depth", floor));
        let hour_offset = match mutation {
            Mutation::StartBasedSql => i64::from(floor),
            Mutation::LateHourSql => real_offset + LATE_SQL_EXTRA_HOURS,
            _ => real_offset,
        };
        Self {
            rng: Rng(params.seed),
            params,
            mutation,
            base_ist: base.and_utc().timestamp(),
            walls: (0..TABLES.len()).map(|_| Wal::default()).collect(),
            batches: Vec::new(),
            by_hour: HashMap::new(),
            live: BTreeSet::new(),
            pending: Vec::new(),
            s3: HashMap::new(),
            sidecars: HashMap::new(),
            holds: Arc::new(AtomicBool::new(false)),
            now_ist: base.and_utc().timestamp(),
            night: Night::default(),
            day: Day::default(),
            writers_open: false,
            stall_active: false,
            acked_max: [0, 0],
            max_seq: 0,
            too_big: HashSet::new(),
            violations: Vec::new(),
            cfg,
            hour_offset,
            marks_version: 0,
            marks_cache: std::cell::RefCell::new(None),
        }
    }

    fn hour_index(&self, ist_secs: i64) -> i64 {
        (ist_secs - self.base_ist).div_euclid(HOUR_SECS)
    }

    fn hour_start_ist(&self, hour: i64) -> i64 {
        self.base_ist + hour * HOUR_SECS
    }

    fn partition_name(&self, hour: i64) -> String {
        let at = NaiveDateTime::from_timestamp_opt_ist(self.hour_start_ist(hour));
        at.format("%Y-%m-%dT%H").to_string()
    }

    fn seq_at(ist_secs: i64) -> u64 {
        let utc = ist_secs - i64::from(IST_UTC_OFFSET_SECONDS);
        u64::try_from(utc).unwrap_or(0) * NANOS
    }

    fn wall_ist(&self) -> i64 {
        self.now_ist - self.night.clock_step_secs
    }

    fn sink(table: usize) -> usize {
        usize::from(!TABLES[table].1)
    }

    fn push_batch(&mut self, table: usize, ist_secs: i64, rows: u64, state: BatchState) {
        let hour = self.hour_index(ist_secs);
        let seq = Self::seq_at(ist_secs);
        let id = self.batches.len();
        self.batches.push(Batch {
            table,
            hour,
            seq,
            rows,
            state,
        });
        self.by_hour.entry((table, hour)).or_default().push(id);
        self.max_seq = self.max_seq.max(seq);
        match state {
            BatchState::InFlight | BatchState::Spilled | BatchState::MarkPending => {
                if state == BatchState::MarkPending {
                    self.marks_version += 1;
                }
                self.pending.push(id);
            }
            _ => {}
        }
    }

    fn commit(&mut self, id: usize) {
        let (table, hour) = (self.batches[id].table, self.batches[id].hour);
        self.walls[table].sequencer += 1;
        self.batches[id].state = BatchState::Committed(self.walls[table].sequencer);
        self.live.insert((hour, table));
    }

    fn applied(&self, id: usize) -> bool {
        match self.batches[id].state {
            BatchState::Committed(txn) => txn <= self.walls[self.batches[id].table].writer,
            _ => false,
        }
    }

    fn apply(&mut self, txns: u64) {
        for wal in &mut self.walls {
            wal.writer = (wal.writer + txns).min(wal.sequencer);
        }
    }

    /// The small writers that keep committing into the current hour.
    fn background(&mut self) {
        if !self.rng.pct(self.params.background_pct) {
            return;
        }
        let now = self.now_ist;
        for table in 0..2 {
            self.push_batch(table, now, 1, BatchState::Committed(0));
            let id = self.batches.len() - 1;
            self.commit(id);
        }
    }

    /// One action of an old-hour writer, when the lock lets it act.
    fn old_hour_writer(&mut self) {
        if !self.writers_open {
            return;
        }
        if self.holds.load(Ordering::SeqCst) && self.mutation != Mutation::NoLock {
            return;
        }
        if !self.rng.pct(40) {
            return;
        }
        let now_hour = self.hour_index(self.now_ist);
        let pick = self
            .pending
            .iter()
            .position(|&id| match self.batches[id].state {
                BatchState::InFlight => !self.stall_active,
                // The spill replay reaches a file on a later evening, often
                // after its hour was archived and dropped.
                BatchState::Spilled => now_hour - self.batches[id].hour >= 24,
                BatchState::MarkPending => self.night.marks_can_finish,
                _ => false,
            });
        if let Some(pos) = pick {
            let id = self.pending.remove(pos);
            if self.batches[id].state == BatchState::MarkPending {
                self.marks_version += 1;
            }
            if self.batches[id].state == BatchState::InFlight {
                let sink = Self::sink(self.batches[id].table);
                self.acked_max[sink] = self.acked_max[sink].max(self.batches[id].seq);
            }
            self.commit(id);
        }
    }

    fn on_box_applied(&self, table: usize, hour: i64) -> Vec<usize> {
        self.by_hour
            .get(&(table, hour))
            .map(|ids| ids.iter().copied().filter(|&id| self.applied(id)).collect())
            .unwrap_or_default()
    }

    fn any_applied(&self, table: usize, hour: i64) -> bool {
        self.by_hour
            .get(&(table, hour))
            .is_some_and(|ids| ids.iter().any(|&id| self.applied(id)))
    }

    fn tally(&self, ids: &[usize]) -> RangeTally {
        let rows = ids.iter().map(|&id| self.batches[id].rows).sum();
        let fingerprint = ids.iter().fold(0_u64, |acc, &id| {
            acc ^ (id as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        });
        RangeTally {
            rows,
            max_ts: (!ids.is_empty()).then(|| format!("{fingerprint:x}")),
        }
    }

    fn has_state(&self, table: usize, hour: i64, wanted: BatchState) -> bool {
        self.by_hour
            .get(&(table, hour))
            .is_some_and(|ids| ids.iter().any(|&id| self.batches[id].state == wanted))
    }

    fn acked_high(&self, sink: usize) -> u64 {
        let lowest_in_flight = self
            .pending
            .iter()
            .filter(|&&id| {
                self.batches[id].state == BatchState::InFlight
                    && Self::sink(self.batches[id].table) == sink
            })
            .map(|&id| self.batches[id].seq)
            .min();
        match lowest_in_flight {
            Some(seq) => seq.saturating_sub(1).min(self.acked_max[sink]),
            None => self.acked_max[sink],
        }
    }

    /// The real in-flight verdict over this world's state.
    fn in_flight(&self, table: usize, span: HourFrameSpan) -> InFlightVerdict {
        let depth = TABLES[table].1;
        let marks: Vec<u64> = self
            .pending
            .iter()
            .filter(|&&id| {
                self.batches[id].state == BatchState::MarkPending && self.batches[id].table == table
            })
            .map(|&id| self.batches[id].seq)
            .collect();
        if self.mutation == Mutation::AllNightDepthHold && depth && !marks.is_empty() {
            return InFlightVerdict::DeferredDepth;
        }
        if depth && !marks.is_empty() {
            let fresh = self
                .marks_cache
                .borrow()
                .as_ref()
                .is_some_and(|(v, o, _)| *v == self.marks_version && *o == self.night.overflow);
            if !fresh {
                let table = DeferredDepth::new_for_tests();
                for &seq in &marks {
                    table.note_shed(seq, ShedDepth::Inline);
                }
                if self.night.overflow {
                    let collide = (DEFERRED_BUCKETS as u64) << DEFERRED_BUCKET_SHIFT;
                    table.note_shed(marks[0] + collide, ShedDepth::Inline);
                }
                *self.marks_cache.borrow_mut() =
                    Some((self.marks_version, self.night.overflow, table.snapshot()));
            }
        }
        let cache = self.marks_cache.borrow();
        let snapshot = (depth && !marks.is_empty())
            .then(|| cache.as_ref().map(|(_, _, snapshot)| snapshot))
            .flatten();
        let applied = AppliedSnapshot::default();
        let acked_high = if self.mutation == Mutation::NoAckCheck {
            u64::MAX
        } else {
            self.acked_high(Self::sink(table))
        };
        let inputs = InFlightInputs {
            wal_bound: true,
            applied: &applied,
            deferred: snapshot,
            rescue_floor_lowest: u64::MAX,
            acked_high,
            last_frame_seq: self.max_seq,
            wall_nanos: Self::seq_at(self.wall_ist()),
            clock_step: None,
        };
        hour_in_flight_verdict(span, &inputs)
    }

    fn wal_rows(&self) -> Vec<WalTableRow> {
        TABLES
            .iter()
            .zip(&self.walls)
            .map(|((name, _), wal)| WalTableRow {
                name: (*name).to_string(),
                suspended: false,
                writer_txn: i64::try_from(wal.writer).ok(),
                sequencer_txn: i64::try_from(wal.sequencer).ok(),
                error_tag: None,
                error_message: None,
            })
            .collect()
    }

    /// The drop, with invariants 1, 3, 4, 5, 6 and 7.
    fn drop_hour(&mut self, table: usize, hour: i64, key: &str) {
        let name = TABLES[table].0;
        if !super::hour_path::HOUR_PATH_TABLES.contains(&name) {
            self.violations.push(format!("EXEMPT_TABLE {name}"));
        }
        let end = self.hour_start_ist(hour) + HOUR_SECS;
        if end > self.now_ist - i64::from(HOT_HOURS) * HOUR_SECS {
            self.violations.push(format!(
                "FLOOR {name} hour {hour} end {end} now {}",
                self.now_ist
            ));
        }
        if self.has_state(table, hour, BatchState::InFlight) {
            self.violations
                .push(format!("INFLIGHT_DROP {name} hour {hour}"));
        }
        if self.has_state(table, hour, BatchState::MarkPending) {
            self.violations
                .push(format!("MARKED_DROP {name} hour {hour}"));
        }
        let copy = self.s3.get(key).cloned().unwrap_or_default();
        let ids = self
            .by_hour
            .get(&(table, hour))
            .cloned()
            .unwrap_or_default();
        for id in ids {
            if !matches!(self.batches[id].state, BatchState::Committed(_)) {
                continue;
            }
            if self.applied(id) {
                if !copy.contains(&id) {
                    self.violations
                        .push(format!("UNCOPIED_DROP {name} hour {hour}"));
                    self.batches[id].state = BatchState::Discarded;
                } else {
                    self.batches[id].state = BatchState::Removed;
                }
            } else {
                self.violations
                    .push(format!("UNAPPLIED_DROP {name} hour {hour}"));
                self.batches[id].state = BatchState::Discarded;
            }
        }
        self.live.remove(&(hour, table));
    }
}

/// The hours back from now in the hour SQL's `dateadd('h', -X, …)`.
fn sql_hour_offset(sql: &str) -> i64 {
    let marker = "dateadd('h', -";
    let at = sql.find(marker).expect("hour SQL names its offset") + marker.len();
    sql[at..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .expect("hour offset is a number")
}

trait IstName {
    fn from_timestamp_opt_ist(secs: i64) -> NaiveDateTime;
}

impl IstName for NaiveDateTime {
    fn from_timestamp_opt_ist(secs: i64) -> NaiveDateTime {
        chrono::DateTime::from_timestamp(secs, 0)
            .expect("in range")
            .naive_utc()
    }
}

struct SimHold(Arc<AtomicBool>);

impl Drop for SimHold {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// The world behind [`ArchiveIo`] for one partition.
struct SimIo<'w> {
    world: &'w mut World,
    table: usize,
    hour: i64,
    crash_at: Option<u32>,
    calls: u32,
    crashed: bool,
}

impl SimIo<'_> {
    /// One I/O call: the clock of the crash, then an old-hour writer may act.
    fn tick(&mut self) -> bool {
        self.calls += 1;
        if self.crash_at.is_some_and(|at| self.calls >= at) {
            self.crashed = true;
        }
        if self.crashed {
            return false;
        }
        self.world.old_hour_writer();
        true
    }
}

impl ArchiveIo for SimIo<'_> {
    type Hold = SimHold;

    async fn read_wal_rows(&mut self) -> Result<Vec<WalTableRow>> {
        if !self.tick() {
            anyhow::bail!("process gone");
        }
        if self.world.night.probe_flaky && self.world.rng.pct(2) {
            anyhow::bail!("QuestDB restarting");
        }
        Ok(self.world.wal_rows())
    }

    async fn hold_old_hour_writers(&mut self, _wait: Duration) -> Option<SimHold> {
        if !self.tick() {
            return None;
        }
        if self.world.hour_index(self.world.now_ist).rem_euclid(24)
            < self.world.night.writers_busy_until_hour
            && self.world.writers_open
        {
            return None;
        }
        self.world.holds.store(true, Ordering::SeqCst);
        Some(SimHold(Arc::clone(&self.world.holds)))
    }

    fn in_flight_now(&mut self, _table: &str, span: HourFrameSpan) -> InFlightVerdict {
        if !self.tick() {
            return InFlightVerdict::WalNotBound;
        }
        self.world.in_flight(self.table, span)
    }

    async fn pause_for(&mut self, _duration: Duration) {
        if self.tick() {
            self.world.apply(APPLY_PER_PAUSE);
            // The live writers keep committing while QuestDB applies.
            if self.world.rng.pct(10) {
                self.world.background();
            }
        }
    }

    async fn copy_and_verify(
        &mut self,
        table: &str,
        partition: &str,
        _start: &str,
        _end: &str,
        summary: &mut ArchiveRunSummary,
    ) -> Option<CopiedPartition> {
        if !self.tick() {
            return None;
        }
        let ids = self.world.on_box_applied(self.table, self.hour);
        let tally = self.world.tally(&ids);
        let upload_refused = self.world.night.s3_down
            || self.world.too_big.contains(&(self.table, self.hour))
            || self.world.rng.pct(2);
        let fail = if upload_refused {
            Some(ArchiveOutcome::UploadFailed)
        } else if self.world.rng.pct(2) {
            Some(ArchiveOutcome::SizeMismatch)
        } else {
            None
        };
        if let Some(outcome) = fail {
            self.note_failure(table, partition, outcome, "simulated", summary);
            return None;
        }
        let content: BTreeSet<usize> = ids.iter().copied().collect();
        let canonical = format!("{table}/{partition}");
        let (key, uploaded) = match self.world.s3.get(&canonical) {
            None => (canonical, true),
            Some(existing) if *existing == content => (canonical, false),
            Some(_) => {
                // A different copy never replaces the canonical object.
                let n = self.world.sidecars.entry(canonical.clone()).or_insert(0);
                *n += 1;
                (format!("{canonical}#{n}"), true)
            }
        };
        if uploaded {
            self.world.s3.insert(key.clone(), content);
        }
        // A crash right after the upload: the object stays, nothing drops.
        if !self.tick() {
            return None;
        }
        let proof = verify_archive(
            table,
            partition,
            tally.rows,
            tally.rows,
            tally.rows + 1,
            "simulated",
            Some(tally.rows),
            Some(tally.rows + 1),
        )
        .expect("simulated proof");
        Some(CopiedPartition {
            proof,
            s3_key: key,
            uploaded_this_run: uploaded,
            verified_tally: Some(tally),
        })
    }

    async fn recount_range(&mut self, _t: &str, _s: &str, _e: &str) -> Option<RangeTally> {
        if !self.tick() {
            return None;
        }
        let ids = self.world.on_box_applied(self.table, self.hour);
        Some(self.world.tally(&ids))
    }

    async fn delete_own_upload(&mut self, key: &str) -> Result<()> {
        self.world.s3.remove(key);
        Ok(())
    }

    fn note_failure(
        &mut self,
        _table: &str,
        _partition: &str,
        _outcome: ArchiveOutcome,
        _detail: &str,
        summary: &mut ArchiveRunSummary,
    ) {
        summary.failed += 1;
    }

    async fn drop_verified(&mut self, copied: CopiedPartition, summary: &mut ArchiveRunSummary) {
        if !self.tick() {
            return;
        }
        let (table, hour) = (self.table, self.hour);
        self.world.drop_hour(table, hour, &copied.s3_key);
        summary.dropped += 1;
    }
}

/// The old per-partition gate (bite): `writerTxn == sequencerTxn` at one
/// read before the export and one before the drop, no quiesce lock.
async fn exact_gate_protocol(
    io: &mut SimIo<'_>,
    table: &'static str,
    partition: &str,
    summary: &mut ArchiveRunSummary,
) -> PartitionStep {
    let exact = |rows: &[WalTableRow]| {
        rows.iter()
            .find(|r| r.name == table)
            .is_some_and(|r| r.writer_txn == r.sequencer_txn)
    };
    match io.read_wal_rows().await {
        Ok(rows) if exact(&rows) => {}
        _ => return PartitionStep::WalNotAppliedBeforeExport,
    }
    let Some(copied) = io.copy_and_verify(table, partition, "", "", summary).await else {
        return PartitionStep::Done;
    };
    match io.read_wal_rows().await {
        Ok(rows) if exact(&rows) => {}
        _ => return PartitionStep::WalNotAppliedBeforeDrop,
    }
    recheck_and_drop(io, table, partition, "", "", copied, summary).await
}

/// The snapshot protocol with S read BEFORE the lock and never re-read
/// (bite).
async fn s_before_lock_protocol(
    io: &mut SimIo<'_>,
    table: &'static str,
    partition: &str,
    span: HourFrameSpan,
    summary: &mut ArchiveRunSummary,
) -> PartitionStep {
    let Ok(rows) = io.read_wal_rows().await else {
        return PartitionStep::WalNotAppliedBeforeExport;
    };
    let Ok(target) = snapshot_target(&rows, table) else {
        return PartitionStep::WalNotAppliedBeforeExport;
    };
    let mut reached = false;
    for _ in 0..200 {
        let Ok(rows) = io.read_wal_rows().await else {
            break;
        };
        if matches!(
            wal_snapshot_verdict(&rows, table, target),
            WalSnapshotVerdict::Reached { .. }
        ) {
            reached = true;
            break;
        }
        io.pause_for(Duration::ZERO).await;
    }
    if !reached {
        return PartitionStep::WalNotAppliedBeforeExport;
    }
    let Some(hold) = io.hold_old_hour_writers(Duration::ZERO).await else {
        return PartitionStep::QuiesceBusy;
    };
    if io.in_flight_now(table, span) != InFlightVerdict::Clear {
        return PartitionStep::Held;
    }
    let Some(copied) = io.copy_and_verify(table, partition, "", "", summary).await else {
        return PartitionStep::Done;
    };
    let step = recheck_and_drop(io, table, partition, "", "", copied, summary).await;
    drop(hold);
    step
}

/// One archive pass at the world's current time: the real selection, oldest
/// hour first, each through the protocol. `cap` limits partitions exported.
fn run_pass(world: &mut World, runtime: &tokio::runtime::Runtime, cap: Option<u32>) {
    let cutoff = world.wall_ist() - world.hour_offset * HOUR_SECS;
    // Oldest hour first, as the real worklist is.
    let work: Vec<(usize, i64)> = world
        .live
        .iter()
        .take_while(|(h, _)| world.hour_start_ist(*h) < cutoff)
        .filter(|(_, t)| {
            world.mutation == Mutation::HourPathOnEveryTable
                || hour_window_for(TABLES[*t].0, &world.cfg).is_some()
        })
        .filter(|(h, t)| world.any_applied(*t, *h))
        .map(|&(h, t)| (t, h))
        .collect();
    let gate = SnapshotGate {
        quiesce_wait: Duration::ZERO,
        snapshot_polls: 150,
    };
    let mut blocked: HashSet<usize> = HashSet::new();
    let mut exported = 0_u32;
    for (table, hour) in work {
        if cap.is_some_and(|cap| exported >= cap) || blocked.contains(&table) {
            continue;
        }
        let crash_at = if world.night.crash_pending && world.rng.pct(30) {
            world.night.crash_pending = false;
            Some(1 + u32::try_from(world.rng.below(30)).unwrap_or(0))
        } else {
            None
        };
        let name = TABLES[table].0;
        let partition = world.partition_name(hour);
        let Some(span) = hour_frame_seq_span(&partition) else {
            world
                .violations
                .push(format!("SIM bad partition {partition}"));
            continue;
        };
        let mutation = world.mutation;
        let mut io = SimIo {
            world,
            table,
            hour,
            crash_at,
            calls: 0,
            crashed: false,
        };
        let mut summary = ArchiveRunSummary::default();
        let step = runtime.block_on(async {
            match mutation {
                Mutation::ExactGate => {
                    exact_gate_protocol(&mut io, name, &partition, &mut summary).await
                }
                Mutation::SBeforeLock => {
                    s_before_lock_protocol(&mut io, name, &partition, span, &mut summary).await
                }
                _ => {
                    archive_with_snapshot_gate(&mut io, name, &partition, gate, &mut summary).await
                }
            }
        });
        let crashed = io.crashed;
        let world = io.world;
        world.holds.store(false, Ordering::SeqCst);
        if crashed {
            // The process restarts; the next pass carries on.
            return;
        }
        if step.exported() {
            exported += 1;
        }
        if step.wal_not_applied() {
            blocked.insert(table);
        }
        if step == PartitionStep::QuiesceBusy {
            return;
        }
    }
}

/// The DISK invariant for the night that ends now (07:00), after its last
/// pass at `last_wall`.
fn check_disk(world: &mut World, last_wall: i64, clean: bool) {
    if !clean {
        return;
    }
    // Anchored to the configured floor, not to `world.hour_offset`: an hour
    // SQL that lists hours late must not move its own check back with it.
    let limit = last_wall - (i64::from(HOT_HOURS) + 1 + DISK_CHECK_MARGIN_HOURS) * HOUR_SECS;
    let mut found = Vec::new();
    for &(hour, table) in &world.live {
        let name = TABLES[table].0;
        if world.hour_start_ist(hour) >= limit {
            break;
        }
        if hour_window_for(name, &world.cfg).is_none() {
            continue;
        }
        if world.on_box_applied(table, hour).is_empty() {
            continue;
        }
        let waiting = |h: i64| {
            world.has_state(table, h, BatchState::InFlight)
                || world.has_state(table, h, BatchState::MarkPending)
                || world.has_state(table, h, BatchState::Spilled)
        };
        let marks_near = TABLES[table].1 && (waiting(hour - 1) || waiting(hour + 1));
        let Some(span) = hour_frame_seq_span(&world.partition_name(hour)) else {
            continue;
        };
        let unacked = world.acked_high(World::sink(table)) <= span.hi + REPLAY_REORDER_SLACK_SEQ;
        let overflow = TABLES[table].1 && world.night.overflow;
        if waiting(hour)
            || marks_near
            || unacked
            || overflow
            || world.too_big.contains(&(table, hour))
        {
            continue;
        }
        found.push(format!(
            "DISK {name} hour {hour} left after a clean night (in flight {:?})",
            world.in_flight(table, span)
        ));
    }
    found.sort();
    world.violations.extend(found);
}

/// Runs one case for `days` days; returns the violations.
fn simulate(params: Params, mutation: Mutation, days: i64) -> Vec<String> {
    simulate_world(params, mutation, days).0
}

/// [`simulate`], also handing back the final world.
fn simulate_world(params: Params, mutation: Mutation, days: i64) -> (Vec<String>, World) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let mut world = World::new(params, mutation);
    let mut last_wall = world.now_ist;
    let mut night_clean = false;
    for day in 0..days {
        let date = NaiveDateTime::from_timestamp_opt_ist(world.base_ist + day * 86_400).date();
        let weekend = date.weekday().number_from_monday() >= 6;
        let holiday = !weekend && world.rng.pct(7);
        let muhurat = weekend && world.rng.pct(10);
        let stall = world.rng.pct(20).then(|| {
            let start = 10 + i64::try_from(world.rng.below(5)).unwrap_or(0);
            (
                start,
                start + 1 + i64::try_from(world.rng.below(8)).unwrap_or(0),
            )
        });
        world.day = Day {
            session: !weekend && !holiday,
            muhurat,
            volume_pct: 50 + world.rng.below(951),
            stall,
            shed_pct: if world.rng.pct(30) { 20 } else { 0 },
            spill_pct: if stall.is_some() { 30 } else { 0 },
        };
        for hour_of_day in 0..24_i64 {
            world.now_ist = world.base_ist + (day * 24 + hour_of_day) * HOUR_SECS;
            if hour_of_day == 7 && day > 0 {
                check_disk(&mut world, last_wall, night_clean);
            }
            if hour_of_day == 16 {
                let marks_can_finish = world.rng.pct(70);
                world.night = Night {
                    s3_down: world.rng.pct(5),
                    clock_step_secs: if world.rng.pct(5) { 2 * HOUR_SECS } else { 0 },
                    writers_busy_until_hour: if world.rng.pct(5) { 22 } else { 0 },
                    marks_can_finish,
                    overflow: world.rng.pct(10),
                    crash_pending: world.rng.pct(15),
                    probe_flaky: world.rng.pct(10),
                };
                // A night with S3 down, the clock stepped back, or QuestDB
                // restarting all night (a failed probe skips the table for
                // the rest of a pass) may legitimately leave hours behind.
                night_clean = !world.night.s3_down
                    && world.night.clock_step_secs == 0
                    && !world.night.probe_flaky;
            }
            world.writers_open = (16..23).contains(&hour_of_day);
            world.stall_active = world
                .day
                .stall
                .is_some_and(|(s, e)| (s..e).contains(&hour_of_day));
            ingest_hour(&mut world, hour_of_day);
            world.background();
            for _ in 0..4 {
                world.old_hour_writer();
            }
            let lag = world.params.lag_txns;
            for wal in &mut world.walls {
                let behind = wal.sequencer.saturating_sub(lag).max(wal.previous_step);
                wal.writer = wal.writer.max(behind.min(wal.sequencer));
                wal.previous_step = wal.sequencer;
            }
            let in_session_pass = world.day.session && (13..=15).contains(&hour_of_day);
            let night_pass = !(7..16).contains(&hour_of_day);
            if in_session_pass || night_pass {
                let minute = 1 + i64::try_from(world.rng.below(58)).unwrap_or(0);
                world.now_ist += minute * 60;
                run_pass(&mut world, &runtime, in_session_pass.then_some(1));
                last_wall = world.wall_ist();
            }
        }
    }
    // Invariant 2: every received row is in S3 or still on the box.
    for (id, batch) in world.batches.iter().enumerate() {
        match batch.state {
            BatchState::Discarded => {
                world.violations.push(format!(
                    "LOSS batch {id} of {} hour {}",
                    TABLES[batch.table].0, batch.hour
                ));
            }
            BatchState::Removed if !world.s3.values().any(|ids| ids.contains(&id)) => {
                world
                    .violations
                    .push(format!("LOSS batch {id} removed without a copy"));
            }
            _ => {}
        }
    }
    (std::mem::take(&mut world.violations), world)
}

/// The session's rows for one hour: ticks and depth, shed and stalled.
fn ingest_hour(world: &mut World, hour_of_day: i64) {
    let session_hour = (world.day.session && (9..=15).contains(&hour_of_day))
        || (world.day.muhurat && hour_of_day == 18);
    if !session_hour {
        return;
    }
    let batches = 1 + world.day.volume_pct / 100;
    let mut hour_rows = [0_u64; 2];
    for _ in 0..batches {
        for table in 0..TABLES.len() {
            let at = world.now_ist + i64::try_from(world.rng.below(3_600)).unwrap_or(0);
            let rows = 1 + world.rng.below(1_000) * world.day.volume_pct / 100;
            let depth = TABLES[table].1;
            let state = if depth && world.rng.pct(world.day.shed_pct) {
                BatchState::MarkPending
            } else if !depth && table == 1 && world.stall_active {
                if world.rng.pct(world.day.spill_pct) {
                    BatchState::Spilled
                } else {
                    BatchState::InFlight
                }
            } else {
                BatchState::Committed(0)
            };
            world.push_batch(table, at, rows, state);
            let id = world.batches.len() - 1;
            if matches!(state, BatchState::Committed(_)) {
                world.commit(id);
            }
            if table < 2 {
                hour_rows[table] += rows;
                if !matches!(state, BatchState::InFlight | BatchState::Spilled) {
                    let sink = World::sink(table);
                    world.acked_max[sink] = world.acked_max[sink].max(World::seq_at(at));
                }
            }
        }
    }
    let hour = world.hour_index(world.now_ist);
    for (table, rows) in hour_rows.iter().enumerate() {
        if *rows > TOO_BIG_ROWS {
            world.too_big.insert((table, hour));
        }
    }
}

fn kinds(violations: &[String]) -> BTreeSet<String> {
    violations
        .iter()
        .filter_map(|v| v.split_whitespace().next().map(str::to_string))
        .collect()
}

fn params_for(seed: u64) -> Params {
    let mut rng = Rng(seed ^ 0xA5A5_5A5A);
    Params {
        seed,
        start_offset_days: i64::try_from(rng.below(20)).unwrap_or(0),
        lag_txns: 1 + rng.below(5_000),
        background_pct: 100,
    }
}

/// Searches seeds for one where `mutation` breaks `invariant` while the
/// real protocol on the same seed keeps every invariant, so the break is the
/// mutation's.
fn bites(mutation: Mutation, invariant: &str) -> bool {
    (0..64_u64).any(|seed| {
        let params = params_for(seed);
        kinds(&simulate(params, mutation, 30)).contains(invariant)
            && simulate(params, Mutation::None, 30).is_empty()
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// 30 days × 256 cases: the real protocol keeps every invariant.
    #[test]
    fn proptest_archive_lifecycle_keeps_every_invariant(
        seed in any::<u64>(),
        start_offset_days in 0_i64..20,
        lag_txns in 0_u64..=5_000,
        background_pct in 0_u64..=100,
    ) {
        let params = Params { seed, start_offset_days, lag_txns, background_pct };
        let violations = simulate(params, Mutation::None, 30);
        prop_assert!(violations.is_empty(), "{params:?}: {violations:?}");
    }
}

/// The sim is not vacuous: over a month it drops hours, re-copies an hour
/// a late spill row re-created (sidecar key), holds hours, and checks clean
/// nights, with no violation.
#[test]
fn test_sim_exercises_drops_sidecars_and_holds() {
    let mut drops = 0_usize;
    let mut sidecars = 0_usize;
    for seed in 0..16_u64 {
        let (violations, world) = simulate_world(params_for(seed), Mutation::None, 30);
        assert!(violations.is_empty(), "seed {seed}: {violations:?}");
        drops += world
            .batches
            .iter()
            .filter(|b| b.state == BatchState::Removed)
            .count();
        sidecars += world.sidecars.values().map(|n| *n as usize).sum::<usize>();
    }
    assert!(drops > 0, "the sim must drop something");
    assert!(
        sidecars > 0,
        "an hour re-created by a late spill row goes to a sidecar key"
    );
}

#[test]
fn test_sql_hour_offset_reads_the_end_based_builder() {
    let floor = hot_hour_floor(HOT_HOURS).expect("on");
    assert_eq!(
        sql_hour_offset(&build_closed_hour_list_sql("market_depth", floor)),
        i64::from(floor) + 1
    );
}

#[test]
fn test_bite_exact_equality_gate_fails_the_disk_invariant() {
    assert!(bites(Mutation::ExactGate, "DISK"));
}

#[test]
fn test_bite_no_quiesce_lock_loses_rows() {
    assert!(bites(Mutation::NoLock, "LOSS"));
}

#[test]
fn test_bite_s_read_before_the_lock_loses_rows() {
    assert!(bites(Mutation::SBeforeLock, "LOSS"));
}

#[test]
fn test_bite_no_acked_watermark_check_drops_rows_in_flight() {
    assert!(bites(Mutation::NoAckCheck, "INFLIGHT_DROP"));
}

#[test]
fn test_bite_all_night_depth_hold_fails_the_disk_invariant() {
    assert!(bites(Mutation::AllNightDepthHold, "DISK"));
}

#[test]
fn test_bite_start_based_hour_sql_breaks_the_floor() {
    assert!(bites(Mutation::StartBasedSql, "FLOOR"));
}

/// PR AA review: invariant 7 can fire. The model reports every table's WAL
/// (it reported only the first two, so a candles or audit hour read "not
/// reported" and was never dropped, whatever the selection said).
#[test]
fn test_bite_hour_path_on_every_table_drops_candles_or_audit() {
    assert!(bites(Mutation::HourPathOnEveryTable, "EXEMPT_TABLE"));
}

/// PR AA review: the DISK check is anchored to the floor, so an hour SQL
/// that lists hours a night late is caught (it moved its own check back
/// with it before).
#[test]
fn test_bite_late_hour_sql_fails_the_disk_invariant() {
    assert!(bites(Mutation::LateHourSql, "DISK"));
}

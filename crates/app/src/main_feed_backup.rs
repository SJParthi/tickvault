//! Backup copies of the top contracts on a second main-feed socket.
//!
//! Operator, 2026-10-02 (`websocket-connection-scope-lock.md`, "2026-10-02 — A
//! BACKUP COPY OF THE TOP CONTRACTS ON A SECOND MAIN-FEED SOCKET"): when one
//! main-feed socket drops, the most important contracts must keep arriving on
//! another one, so no tick is missed for them.
//!
//! # Two halves
//!
//! **Subscribe (cold, once a day).** The contract attach dials the contracts
//! onto the four packed contract sockets; every free main-feed slot is left on
//! the SPOT socket. After the attach and its late top-up window finish, the
//! head of [`crate::dhan_contract_universe::ContractSelection::backup_rank`]
//! (option legs nearest the money) is subscribed a second time on the spot
//! socket — [`plan_backup_set`] keeps only contracts whose first copy is on a
//! contract socket, so the two copies are never on one socket, and never takes
//! more than the room that socket has left.
//!
//! **Dedup (hot, per packet).** Both copies of a packet reach the drain. The
//! WAL keeps both (capture happens at receipt, before this code). The fold and
//! the rows must see ONE: `ticks` keys on `capture_seq`, so two copies would be
//! two rows, and a stale copy arriving late would feed the fold a cumulative
//! volume that went backwards. [`BackupDedup`] keeps, per backup contract, a
//! ring of recent packet fingerprints (each with the socket it came on) and
//! the newest accepted `(cumulative volume, trade time)`. A packet
//! byte-identical to an unpaired ring entry from the OTHER socket, or older
//! than the newest accepted, is dropped and counted; a same-socket identical
//! repeat is a real repeat and is kept. Anything else is accepted, whichever
//! socket it came from — so when one socket is down the other's copies are
//! simply accepted.
//!
//! **A new publication.** The drain adopts it on its next frame. Contracts in
//! both the old and the new set keep their dedup state (ring and newest
//! accepted), so a truncated subscribe answer republishing the held subset
//! while both sockets already deliver does not re-accept the in-flight second
//! copies; contracts new to the set start fresh. State is carried only
//! between publications of the same IST day, because the vendor's cumulative
//! restarts each day.
//!
//! **Replay.** Each publication is also persisted ([`record_backup_set`]) into
//! a bounded HISTORY of publications (the two most recent IST days that have
//! one, at most [`BACKUP_HISTORY_MAX`] entries), so a later publication never
//! erases the set an earlier, still-unreplayed frame was deduplicated with.
//! The WAL replay ([`ReplayBackup`]) applies to each frame the publication
//! with the greatest instant at or before the frame's receipt on the same IST
//! day, switching state exactly as the live drain does. A replayed frame
//! carries no socket, so there an identical unpaired entry from any socket
//! matches.
//!
//! # Cost
//!
//! Disabled or not yet published: one `bool` test per packet and one relaxed
//! atomic load per frame. Published: one bit test in an 8 KiB filter per tick;
//! only a filter hit (the backup set, plus ~1.5% false positives at 1,000
//! entries) pays one hash probe, and a backup contract's packet pays a
//! fingerprint over its bytes (~20 word mixes for a 162-byte Full packet) and a
//! 16-entry compare. O(1), zero allocation per packet. A new set is built by
//! the publisher, off the drain; the drain adopts it with a pointer swap,
//! copies the state of the contracts the two sets share — O(new set) hash
//! probes and slot copies, ONCE per publication (a few a day, set ≤
//! [`BACKUP_RANK_MAX`], 1,000 by default), zero allocation — and hands its
//! previous state back for the publisher to free.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, TryLockError};

use arc_swap::ArcSwapOption;
use tickvault_common::tick_types::ParsedTick;
use tickvault_core::websocket::pool_supervisor::SubscribeInstrument;

/// Longest backup ranking kept on a selection: one main-feed socket's cap. The
/// backup set itself is further bounded by config and by the room left.
pub const BACKUP_RANK_MAX: usize = 5_000;

/// Recent packet fingerprints remembered per backup contract.
pub const FINGERPRINT_RING: usize = 16;

/// Recent previous-close / open-interest fingerprints per backup contract.
pub const AUX_FINGERPRINT_RING: usize = 4;

/// A copy accepted from one socket counts as a backup-only arrival when the
/// other socket of the pair has delivered no frame for this long.
pub const PEER_SILENT_MILLIS: u64 = 2_000;

/// Connection indices tracked for the silence test. The global index space is
/// `MAX_TOTAL_DHAN_CONNECTIONS` (26); larger indices (and the replay marker
/// `u8::MAX`) are not tracked.
const TRACKED_CONNECTIONS: usize = 32;

/// 65,536-bit membership pre-filter (8 KiB).
const FILTER_WORDS: usize = 1_024;

/// Counter: backup-set packets dropped as a second copy, by `reason`
/// (`identical` or `older`).
pub const BACKUP_DUPLICATES_DROPPED_COUNTER: &str = "tv_dhan_feed_backup_duplicates_dropped_total";

/// Counter: backup-set packets accepted from one socket while the other socket
/// of the pair had been silent for [`PEER_SILENT_MILLIS`] — ticks that arrived
/// only because the backup copy existed.
pub const BACKUP_ONLY_ARRIVALS_COUNTER: &str = "tv_dhan_feed_backup_only_arrivals_total";

/// Gauge: contracts in the backup set the drain is deduplicating.
pub const BACKUP_INSTRUMENTS_GAUGE: &str = "tv_dhan_feed_backup_instruments";

/// Counter: the once-a-day backup subscribe, by `outcome`.
pub const BACKUP_SUBSCRIBE_COUNTER: &str = "tv_dhan_feed_backup_subscribe_total";

/// What the drain does with one packet of a possibly-backed-up contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupVerdict {
    /// Not in the backup set (or the set is empty): the normal path.
    NotBackup,
    /// A backup contract's packet seen for the first time: fold and write it.
    Accept,
    /// A byte-identical copy of a packet already accepted.
    DropIdentical,
    /// A copy older than the newest accepted (lower cumulative volume).
    DropOlder,
}

impl BackupVerdict {
    /// Whether the packet must be skipped by the fold and the writers.
    #[must_use]
    pub const fn is_drop(self) -> bool {
        matches!(self, Self::DropIdentical | Self::DropOlder)
    }
}

/// Picks the backup set: the head of `ranked` whose first copy is on a
/// contract socket (`primary_elsewhere`), at most `min(top_n, room)` long.
///
/// `room` is the free room on the socket the copies go to; nothing beyond it
/// is returned, so the per-connection cap is never asked to stretch. A
/// contract not in `primary_elsewhere` is skipped rather than counted: its
/// only copy may already be on the target socket, and a second copy there
/// would be no backup. Order is preserved, so the most important come first.
/// O(ranked).
#[must_use]
pub fn plan_backup_set(
    ranked: &[SubscribeInstrument],
    primary_elsewhere: &HashSet<(u64, u8)>,
    room: usize,
    top_n: usize,
) -> Vec<SubscribeInstrument> {
    let want = top_n.min(room).min(BACKUP_RANK_MAX);
    let mut seen: HashSet<(u64, u8)> = HashSet::with_capacity(want);
    let mut out = Vec::with_capacity(want);
    for inst in ranked {
        if out.len() >= want {
            break;
        }
        let key = (inst.security_id, inst.segment.binary_code());
        if primary_elsewhere.contains(&key) && seen.insert(key) {
            out.push(*inst);
        }
    }
    out
}
/// The membership half of a backup set: the pre-filter and the index. Built
/// off the drain (by the publisher) and shared read-only.
struct BackupTable {
    filter: Box<[u64; FILTER_WORDS]>,
    index: HashMap<(u64, u8), u32>,
    len: usize,
}

impl BackupTable {
    /// Cold: allocates for the set. A repeated key is held once.
    fn build(set: &[(u64, u8)]) -> Self {
        let mut filter = Box::new([0u64; FILTER_WORDS]);
        let mut index = HashMap::with_capacity(set.len());
        for &(security_id, segment) in set {
            if index.contains_key(&(security_id, segment)) {
                continue;
            }
            let Ok(at) = u32::try_from(index.len()) else {
                break;
            };
            index.insert((security_id, segment), at);
            let bit = filter_bit(security_id, segment);
            if let Some(word) = filter.get_mut(bit / 64) {
                *word |= 1u64 << (bit % 64);
            }
        }
        let len = index.len();
        Self { filter, index, len }
    }
}

/// What the publisher hands the drain alongside the shared table: the
/// drain's fresh per-contract state, pre-built, and a slot the drain moves
/// its previous state into so the publisher's thread frees it, never the
/// drain.
#[derive(Default)]
struct Handoff {
    fresh_slots: Option<Vec<Slot>>,
    retired: Option<(Arc<BackupTable>, Vec<Slot>)>,
}

/// One publication.
struct Prepared {
    generation: u64,
    /// IST day of the publication instant: state carries over only between
    /// publications of one day.
    day: i64,
    table: Arc<BackupTable>,
    handoff: Mutex<Handoff>,
}

fn published() -> &'static ArcSwapOption<Prepared> {
    static SET: OnceLock<ArcSwapOption<Prepared>> = OnceLock::new();
    SET.get_or_init(ArcSwapOption::empty)
}

static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Serialises the tests that publish the process-global set, so one test's
/// `on_frame` never installs another test's set mid-assertion.
#[cfg(test)]
pub(crate) static TEST_PUBLISH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Publishes the backup set to the drain, which adopts it on its next frame.
/// Cold: the table and the drain's fresh state are built HERE, on the
/// publisher's task, so the drain pays a pointer swap plus the carry-over of
/// the shared contracts' state. Called BEFORE the subscribe goes out (a copy
/// can then never arrive undeduplicated), and again with the held subset or
/// an empty set when the socket answers. `published_at_nanos` is the instant
/// persisted with it ([`PersistedBackupSet::published_at_nanos`]).
pub fn publish_backup_set(set: &[SubscribeInstrument], published_at_nanos: i64) {
    let keys: Vec<(u64, u8)> = set
        .iter()
        .map(|i| (i.security_id, i.segment.binary_code()))
        .collect();
    let table = Arc::new(BackupTable::build(&keys));
    let fresh_slots = if table.len == 0 {
        Vec::new()
    } else {
        vec![Slot::fresh(); table.len]
    };
    let generation = GENERATION.load(Ordering::Acquire).saturating_add(1);
    // Replacing the previous publication drops it here, on the publisher,
    // with whatever state the drain retired into it.
    published().store(Some(Arc::new(Prepared {
        generation,
        day: ist_day_of_nanos(published_at_nanos),
        table,
        handoff: Mutex::new(Handoff {
            fresh_slots: Some(fresh_slots),
            retired: None,
        }),
    })));
    GENERATION.fetch_max(generation, Ordering::Release);
}

/// The contracts of the current publication (tests).
#[cfg(test)]
pub(crate) fn published_keys() -> Vec<(u64, u8)> {
    let mut keys: Vec<(u64, u8)> = published()
        .load_full()
        .map(|p| p.table.index.keys().copied().collect())
        .unwrap_or_default();
    keys.sort_unstable();
    keys
}

/// Per-contract dedup state, `Copy`.
#[derive(Debug, Clone, Copy, Default)]
struct Slot {
    fps: [u64; FINGERPRINT_RING],
    /// The connection each fingerprint arrived on.
    fp_conn: [u8; FINGERPRINT_RING],
    /// Bit `i`: entry `i` already had its second copy dropped.
    fp_paired: u32,
    aux: [u64; AUX_FINGERPRINT_RING],
    aux_conn: [u8; AUX_FINGERPRINT_RING],
    aux_paired: u32,
    next: u8,
    aux_next: u8,
    has_newest: bool,
    newest_cum: u32,
    newest_ltt: u32,
    /// The first two live connections this contract arrived on; `u8::MAX`
    /// until learned.
    conn_a: u8,
    conn_b: u8,
}

impl Slot {
    fn fresh() -> Self {
        Self {
            conn_a: u8::MAX,
            conn_b: u8::MAX,
            ..Self::default()
        }
    }

    /// The other socket of this contract's pair, once both are known.
    fn peer_of(&self, conn: u8) -> Option<u8> {
        if self.conn_b == u8::MAX {
            None
        } else if conn == self.conn_a {
            Some(self.conn_b)
        } else if conn == self.conn_b {
            Some(self.conn_a)
        } else {
            None
        }
    }

    fn learn(&mut self, conn: u8) {
        if conn == u8::MAX {
            return;
        }
        if self.conn_a == u8::MAX {
            self.conn_a = conn;
        } else if self.conn_b == u8::MAX && conn != self.conn_a {
            self.conn_b = conn;
        }
    }
}

/// The ring entry this packet is the SECOND copy of: the same fingerprint,
/// not yet paired, from the OTHER socket. A same-socket identical repeat is a
/// real repeat and matches nothing. A replayed frame carries no connection
/// (`u8::MAX`), so on either side an unknown socket matches any socket.
/// O(ring), a constant.
#[inline]
fn second_copy_of(fps: &[u64], conns: &[u8], paired: u32, fp: u64, conn: u8) -> Option<usize> {
    fps.iter().zip(conns).enumerate().find_map(|(i, (&f, &c))| {
        let other_socket = conn == u8::MAX || c == u8::MAX || c != conn;
        (f == fp && paired & (1u32 << i) == 0 && other_socket).then_some(i)
    })
}

/// No publication adopted yet: a day no publication instant maps to.
const NO_DAY: i64 = i64::MIN;

/// Copies the dedup state of every contract present in both tables from the
/// old slots into the new ones (review 2026-10-02, F2: a wholesale swap
/// re-accepted the in-flight second copies of contracts that stayed in the
/// set). O(new set) probes, zero allocation.
fn carry_over(
    new_slots: &mut [Slot],
    new_table: &BackupTable,
    old_slots: &[Slot],
    old_table: &BackupTable,
) {
    if old_table.len == 0 || new_table.len == 0 {
        return;
    }
    for (key, &at) in &new_table.index {
        if let Some(&was) = old_table.index.get(key)
            && let (Some(dst), Some(src)) =
                (new_slots.get_mut(at as usize), old_slots.get(was as usize))
        {
            *dst = *src;
        }
    }
}

/// The drain's backup dedup table. Owned by `LiveIngest`, single-threaded.
pub struct BackupDedup {
    active: bool,
    generation_seen: u64,
    /// IST day of the adopted publication; [`NO_DAY`] when none.
    pub_day: i64,
    table: Arc<BackupTable>,
    slots: Vec<Slot>,
    last_frame_millis: [u64; TRACKED_CONNECTIONS],
    dropped_identical: metrics::Counter,
    dropped_older: metrics::Counter,
    backup_only: metrics::Counter,
    instruments: metrics::Gauge,
}

impl Default for BackupDedup {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for BackupDedup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackupDedup")
            .field("active", &self.active)
            .field("instruments", &self.slots.len())
            .finish_non_exhaustive()
    }
}

/// Bit position in the pre-filter for one composite key.
#[inline]
fn filter_bit(security_id: u64, segment: u8) -> usize {
    let mixed = (security_id ^ (u64::from(segment) << 56)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    // Top 16 bits: 0..65_536.
    (mixed >> 48) as usize
}

/// 64-bit fingerprint of a packet's bytes. Never 0 (0 marks an empty ring
/// entry). Not cryptographic: two different packets of one contract collide
/// with probability ~2^-64.
#[must_use]
pub fn packet_fingerprint(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xCBF2_9CE4_8422_2325 ^ (bytes.len() as u64);
    let mut chunks = bytes.chunks_exact(8);
    for chunk in &mut chunks {
        let mut w = [0u8; 8];
        w.copy_from_slice(chunk);
        h = (h ^ u64::from_le_bytes(w))
            .wrapping_mul(0x0000_0100_0000_01B3)
            .rotate_left(29);
    }
    let mut tail = [0u8; 8];
    let rest = chunks.remainder();
    tail[..rest.len()].copy_from_slice(rest);
    h = (h ^ u64::from_le_bytes(tail)).wrapping_mul(0x0000_0100_0000_01B3);
    // splitmix64 finaliser.
    h ^= h >> 30;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94D0_49BB_1331_11EB);
    h ^= h >> 31;
    h | 1
}

impl BackupDedup {
    /// An empty, inactive table. Allocates the 8 KiB filter only. Pins this
    /// process's epoch ([`process_epoch_nanos`]) before any frame is drained
    /// or replayed.
    #[must_use]
    pub fn new() -> Self {
        let _pinned = process_epoch_nanos();
        Self {
            active: false,
            generation_seen: 0,
            pub_day: NO_DAY,
            table: Arc::new(BackupTable::build(&[])),
            slots: Vec::new(),
            last_frame_millis: [0; TRACKED_CONNECTIONS],
            dropped_identical: metrics::counter!(
                BACKUP_DUPLICATES_DROPPED_COUNTER,
                "reason" => "identical"
            ),
            dropped_older: metrics::counter!(BACKUP_DUPLICATES_DROPPED_COUNTER, "reason" => "older"),
            backup_only: metrics::counter!(BACKUP_ONLY_ARRIVALS_COUNTER),
            instruments: metrics::gauge!(BACKUP_INSTRUMENTS_GAUGE),
        }
    }

    /// A table for the WAL replay: its drops are reported by the replay's
    /// own outcome, so it does not touch the live series.
    #[must_use]
    fn for_replay(set: &[(u64, u8)]) -> Self {
        let mut d = Self {
            dropped_identical: metrics::Counter::noop(),
            dropped_older: metrics::Counter::noop(),
            backup_only: metrics::Counter::noop(),
            instruments: metrics::Gauge::noop(),
            ..Self::new()
        };
        d.install(set);
        d
    }

    /// Contracts currently deduplicated.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether no contract is deduplicated.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Replaces the set in place. Cold: allocates for the new set. The drain
    /// never calls this; it adopts a publication in [`Self::on_frame`].
    pub fn install(&mut self, set: &[(u64, u8)]) {
        self.table = Arc::new(BackupTable::build(set));
        self.slots = vec![Slot::fresh(); self.table.len];
        self.pub_day = NO_DAY;
        self.active = !self.slots.is_empty();
        self.instruments.set(self.slots.len() as f64);
    }

    /// Switches to a new publication's table and state; with `carry`, the
    /// state of every contract in both sets is carried over (callers pass it
    /// only between publications of one IST day, and the replay only within
    /// one process, as the live drain does). Returns the previous pair. The
    /// live adopt and the replay both switch through here, so the two cannot
    /// differ.
    ///
    /// # Complexity
    /// O(new set) hash probes and `Copy` slot copies when carrying from a
    /// non-empty set, else O(1). Once per publication, zero allocation
    /// (`slots` is pre-built by the caller).
    fn swap_in(
        &mut self,
        table: Arc<BackupTable>,
        mut slots: Vec<Slot>,
        day: i64,
        carry: bool,
    ) -> (Arc<BackupTable>, Vec<Slot>) {
        if carry {
            carry_over(&mut slots, &table, &self.slots, &self.table);
        }
        let old_slots = std::mem::replace(&mut self.slots, slots);
        let old_table = std::mem::replace(&mut self.table, table);
        self.pub_day = day;
        self.active = !self.slots.is_empty();
        self.instruments.set(self.slots.len() as f64);
        (old_table, old_slots)
    }

    /// Adopts one publication: a pointer swap for the table and the drain's
    /// state (plus the carry-over of [`Self::swap_in`]), the previous pair
    /// moved into the publication for the publisher to free. No allocation
    /// and no free in the normal case. `false` when the hand-off is busy this
    /// instant (retried on the next frame).
    fn adopt(&mut self, p: &Prepared) -> bool {
        let mut handoff = match p.handoff.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return false,
        };
        // A second reader of one publication (tests run several ingests)
        // builds its own state; the one production drain always finds it.
        let slots = handoff
            .fresh_slots
            .take()
            .unwrap_or_else(|| vec![Slot::fresh(); p.table.len]);
        let carry = self.pub_day == p.day && p.day != NO_DAY;
        let retired = self.swap_in(Arc::clone(&p.table), slots, p.day, carry);
        if handoff.retired.is_none() {
            handoff.retired = Some(retired);
        }
        true
    }

    /// Per frame: picks up a newly published set (one relaxed load when there
    /// is none) and stamps the frame's connection as alive.
    #[inline]
    pub fn on_frame(&mut self, connection_index: u8, recv_millis: u64) {
        if GENERATION.load(Ordering::Acquire) != self.generation_seen
            && let Some(p) = published().load_full()
            && p.generation != self.generation_seen
            && self.adopt(&p)
        {
            self.generation_seen = p.generation;
        }
        if self.active
            && let Some(at) = self
                .last_frame_millis
                .get_mut(usize::from(connection_index))
        {
            *at = recv_millis;
        }
    }

    #[inline]
    fn slot_of(&self, security_id: u64, segment: u8) -> Option<usize> {
        if !self.active {
            return None;
        }
        let bit = filter_bit(security_id, segment);
        let word = self.table.filter.get(bit / 64).copied().unwrap_or(0);
        if word & (1u64 << (bit % 64)) == 0 {
            return None;
        }
        self.table
            .index
            .get(&(security_id, segment))
            .map(|&i| i as usize)
    }

    /// Verdict for one tick packet (`packet` = that packet's bytes).
    /// O(1), zero allocation.
    #[inline]
    pub fn admit_tick(
        &mut self,
        tick: &ParsedTick,
        packet: &[u8],
        connection_index: u8,
        recv_millis: u64,
    ) -> BackupVerdict {
        let Some(at) = self.slot_of(tick.security_id, tick.exchange_segment_code) else {
            return BackupVerdict::NotBackup;
        };
        let fp = packet_fingerprint(packet);
        let peer_millis = {
            let Some(slot) = self.slots.get_mut(at) else {
                return BackupVerdict::NotBackup;
            };
            if let Some(i) = second_copy_of(
                &slot.fps,
                &slot.fp_conn,
                slot.fp_paired,
                fp,
                connection_index,
            ) {
                slot.fp_paired |= 1u32 << i;
                self.dropped_identical.increment(1);
                return BackupVerdict::DropIdentical;
            }
            let cum = tick.volume;
            let ltt = tick.exchange_timestamp;
            if slot.has_newest {
                // Serial-number compare, so a u32 wrap of the vendor's
                // cumulative reads as newer, not as a huge step back. A copy
                // older than the newest accepted is a stale copy, however far
                // behind the ring it lags.
                let step = cum.wrapping_sub(slot.newest_cum) as i32;
                if step < 0 || (step == 0 && ltt < slot.newest_ltt) {
                    self.dropped_older.increment(1);
                    return BackupVerdict::DropOlder;
                }
                if step > 0 {
                    slot.newest_cum = cum;
                }
                // The newest trade time never goes backwards.
                slot.newest_ltt = slot.newest_ltt.max(ltt);
            } else {
                slot.has_newest = true;
                slot.newest_cum = cum;
                slot.newest_ltt = ltt;
            }
            let i = usize::from(slot.next) % FINGERPRINT_RING;
            slot.fps[i] = fp;
            slot.fp_conn[i] = connection_index;
            slot.fp_paired &= !(1u32 << i);
            slot.next = ((i + 1) % FINGERPRINT_RING) as u8;
            slot.learn(connection_index);
            slot.peer_of(connection_index)
                .and_then(|peer| self.last_frame_millis.get(usize::from(peer)).copied())
        };
        if let Some(peer_last) = peer_millis
            && recv_millis.saturating_sub(peer_last) > PEER_SILENT_MILLIS
        {
            self.backup_only.increment(1);
        }
        BackupVerdict::Accept
    }

    /// Verdict for a previous-close or open-interest packet: the other
    /// socket's byte-identical copy is dropped, anything else accepted.
    /// O(1), zero allocation.
    #[inline]
    pub fn admit_aux(
        &mut self,
        security_id: u64,
        segment: u8,
        packet: &[u8],
        connection_index: u8,
    ) -> BackupVerdict {
        let Some(at) = self.slot_of(security_id, segment) else {
            return BackupVerdict::NotBackup;
        };
        let fp = packet_fingerprint(packet);
        let Some(slot) = self.slots.get_mut(at) else {
            return BackupVerdict::NotBackup;
        };
        if let Some(i) = second_copy_of(
            &slot.aux,
            &slot.aux_conn,
            slot.aux_paired,
            fp,
            connection_index,
        ) {
            slot.aux_paired |= 1u32 << i;
            self.dropped_identical.increment(1);
            return BackupVerdict::DropIdentical;
        }
        let i = usize::from(slot.aux_next) % AUX_FINGERPRINT_RING;
        slot.aux[i] = fp;
        slot.aux_conn[i] = connection_index;
        slot.aux_paired &= !(1u32 << i);
        slot.aux_next = ((i + 1) % AUX_FINGERPRINT_RING) as u8;
        BackupVerdict::Accept
    }
}

// ---------------------------------------------------------------------------
// The persisted set, for the WAL replay
// ---------------------------------------------------------------------------

/// Directory of the persisted set: the one every other daily artifact uses.
const BACKUP_SET_DIR: &str = "data/instrument-cache";

/// The persisted history of publications. One file, rewritten atomically by
/// each publication; the instant inside each entry says which frames it
/// covers.
const BACKUP_SET_FILE: &str = "main-feed-backup-set.json";

/// Most publications kept in the persisted history. A day normally has one
/// or two (the publication and its narrowing share an instant, so they are
/// one entry); a restart adds one. Bounds the file and the replay's table.
pub const BACKUP_HISTORY_MAX: usize = 16;

/// Counter: the WAL replay's backup set, by `outcome` (`loaded`, `missing`,
/// `unreadable`). Once per lane start.
pub const BACKUP_REPLAY_SET_COUNTER: &str = "tv_dhan_feed_backup_replay_set_total";

/// One publication: the set and the instant the drain started deduplicating
/// it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PersistedBackupSet {
    /// UTC epoch nanos of the publication.
    pub published_at_nanos: i64,
    /// `(security_id, exchange_segment wire byte)`, rank order.
    pub contracts: Vec<(u64, u8)>,
    /// [`process_epoch_nanos`] of the process that published it; `0` in a
    /// file an earlier build wrote (unknown). A publication covers no frame
    /// received after a LATER process started: that process's drain did not
    /// hold it.
    #[serde(default)]
    pub process_started_at_nanos: i64,
}

impl PersistedBackupSet {
    /// The set as it is published now, by this process.
    #[must_use]
    pub fn from_set(set: &[SubscribeInstrument], published_at_nanos: i64) -> Self {
        Self {
            published_at_nanos,
            process_started_at_nanos: process_epoch_nanos(),
            contracts: set
                .iter()
                .map(|i| (i.security_id, i.segment.binary_code()))
                .collect(),
        }
    }
}

/// What the replay needs to fold one copy of each packet exactly as the live
/// drain did: every recent publication, oldest first (review 2026-10-02, F1:
/// one overwritten set let a later publication erase the set an
/// unreplayed earlier frame was deduplicated with).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PersistedBackupHistory {
    /// Ascending by instant, one entry per instant, only the two most recent
    /// IST days that have one, at most [`BACKUP_HISTORY_MAX`].
    pub publications: Vec<PersistedBackupSet>,
}

impl PersistedBackupHistory {
    /// The newest publication.
    #[must_use]
    pub fn latest(&self) -> Option<&PersistedBackupSet> {
        self.publications.last()
    }

    /// Adds one publication. A publication with the same instant as one held
    /// replaces it (the narrowing of an attach shares the attach's instant).
    /// Cold: O(history), a constant.
    pub fn record(&mut self, set: PersistedBackupSet) {
        self.publications.push(set);
        self.normalise();
    }

    /// Restores the invariants: drops instant-less entries, orders by
    /// instant (stable, so of two entries with one instant the later
    /// recorded wins), keeps the two most recent IST days that have a
    /// publication — two days, not "today and yesterday", so Friday's set
    /// still covers Friday's frames replayed on Monday — and at most
    /// [`BACKUP_HISTORY_MAX`], the newest. Cold.
    fn normalise(&mut self) {
        self.publications.retain(|p| p.published_at_nanos > 0);
        self.publications.sort_by_key(|p| p.published_at_nanos);
        self.publications.reverse();
        self.publications.dedup_by_key(|p| p.published_at_nanos);
        self.publications.reverse();
        if let Some(latest_day) = self
            .latest()
            .map(|p| ist_day_of_nanos(p.published_at_nanos))
        {
            let oldest_kept_day = self
                .publications
                .iter()
                .rev()
                .map(|p| ist_day_of_nanos(p.published_at_nanos))
                .find(|&d| d < latest_day)
                .unwrap_or(latest_day);
            self.publications
                .retain(|p| ist_day_of_nanos(p.published_at_nanos) >= oldest_kept_day);
        }
        let excess = self.publications.len().saturating_sub(BACKUP_HISTORY_MAX);
        self.publications.drain(..excess);
    }
}

/// The file as it may be on disk: the history, or the single set an earlier
/// build wrote.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum OnDisk {
    History(PersistedBackupHistory),
    Single(PersistedBackupSet),
}

/// Where the set is persisted.
#[must_use]
pub fn backup_set_path() -> std::path::PathBuf {
    std::path::Path::new(BACKUP_SET_DIR).join(BACKUP_SET_FILE)
}

/// Writes the history atomically (temp file, synced, then rename), creating
/// the directory if needed. Cold.
pub fn write_backup_history(
    path: &std::path::Path,
    history: &PersistedBackupHistory,
) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec(history).map_err(std::io::Error::other)?;
    {
        use std::io::Write as _;
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&body)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent()
        && let Err(err) = std::fs::File::open(dir).and_then(|d| d.sync_all())
    {
        tracing::debug!(path = %path.display(), %err, "backup set directory sync failed");
    }
    Ok(())
}

/// Adds one publication to the persisted history: read, record, write
/// atomically. An unreadable file is replaced by a history holding this
/// publication alone (logged): the replay then loses the older entries,
/// never the new one. Serialised in-process, so two publications cannot
/// lose each other's entry. Cold.
pub fn record_backup_set(path: &std::path::Path, set: &PersistedBackupSet) -> std::io::Result<()> {
    static RECORD_LOCK: Mutex<()> = Mutex::new(());
    let _guard = RECORD_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut history = match read_backup_history(path) {
        Ok(Some(history)) => history,
        Ok(None) => PersistedBackupHistory::default(),
        Err(err) => {
            tracing::warn!(
                code = tickvault_common::error_code::ErrorCode::WsGapSubscriptionBatching
                    .code_str(),
                source = "backup_set_history_unreadable",
                path = %path.display(),
                %err,
                "main-feed backup: the saved publication history could not be read and is \
                 replaced by this publication alone — a crash replay of frames from before it \
                 may write both copies of a backed-up packet"
            );
            PersistedBackupHistory::default()
        }
    };
    history.record(set.clone());
    write_backup_history(path, &history)
}

/// Reads the persisted history (or an earlier build's single set, as a
/// history of one): `Ok(None)` when there is none, `Err` when a file exists
/// and cannot be read or parsed. Normalised, so a hand-edited or oversized
/// file is bounded. Cold.
pub fn read_backup_history(
    path: &std::path::Path,
) -> Result<Option<PersistedBackupHistory>, String> {
    match std::fs::read(path) {
        Ok(body) => {
            let mut history = match serde_json::from_slice::<OnDisk>(&body) {
                Ok(OnDisk::History(history)) => history,
                Ok(OnDisk::Single(set)) => PersistedBackupHistory {
                    publications: vec![set],
                },
                Err(err) => return Err(err.to_string()),
            };
            history.normalise();
            Ok(Some(history))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.to_string()),
    }
}

/// This process's epoch: UTC epoch nanos read once, the first time any
/// dedup table is built (before the drain or the replay sees a frame). Every
/// frame this process receives is stamped at or after it, and every frame an
/// earlier process received before it, so it bounds the earlier process's
/// publications in the replay. Set late, it only leaves a few of this
/// process's first frames judged by the earlier set, as before. Cold.
#[must_use]
pub fn process_epoch_nanos() -> i64 {
    static EPOCH: OnceLock<i64> = OnceLock::new();
    *EPOCH.get_or_init(|| chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0))
}

/// IST day number of a UTC epoch-nanos instant.
#[inline]
fn ist_day_of_nanos(nanos: i64) -> i64 {
    let secs = nanos.div_euclid(1_000_000_000).saturating_add(i64::from(
        tickvault_common::constants::IST_UTC_OFFSET_SECONDS,
    ));
    secs.div_euclid(86_400)
}

/// The WAL replay's dedup: each frame is judged with the publication the
/// live drain was using when it received it — the one with the greatest
/// instant at or before the frame's receipt, on the same IST day (the
/// vendor's cumulative restarts each day, so yesterday's newest volume must
/// never judge today's), and not after a later process started (a restarted
/// process's drain held nothing until its own publication). Switching
/// publications goes through the same [`BackupDedup::swap_in`] as the live
/// adopt, carry-over included, and carries only within one process.
///
/// Frames replay in receipt order, so a forward cursor finds the publication
/// in O(1) amortized. A frame received BEFORE the publication the cursor has
/// already reached (out of order) is not judged at all — accepted, state
/// untouched: the live drain judged it with an earlier set this table no
/// longer holds, and accepting can only keep a second copy, never drop one
/// live kept.
#[derive(Debug)]
pub struct ReplayBackup {
    publications: Vec<PersistedBackupSet>,
    /// The first publication not yet switched to.
    next: usize,
    dedup: BackupDedup,
    /// Instant, IST day and process of the publication switched to last.
    from_nanos: i64,
    day: i64,
    process: i64,
    /// When a later process started: frames from then on are not this
    /// publication's (`i64::MAX` when none, or unknown).
    until_nanos: i64,
}

impl ReplayBackup {
    /// Builds the replay's dedup, or `None` when no publication holds a
    /// contract (nothing to deduplicate). Cold, once per lane start.
    #[must_use]
    pub fn new(history: &PersistedBackupHistory) -> Option<Self> {
        let mut history = history.clone();
        history.normalise();
        if history.publications.iter().all(|p| p.contracts.is_empty()) {
            return None;
        }
        Some(Self {
            publications: history.publications,
            next: 0,
            dedup: BackupDedup::for_replay(&[]),
            from_nanos: i64::MAX,
            day: NO_DAY,
            process: 0,
            until_nanos: i64::MAX,
        })
    }

    /// Selects the publication for a frame received at `received_at_nanos`
    /// and says whether the live drain deduplicated the frame with it. O(1)
    /// amortized per frame (one compare and one division); a switch builds
    /// the next publication's table, cold, once per publication.
    #[inline]
    pub fn select(&mut self, received_at_nanos: i64) -> bool {
        if received_at_nanos <= 0 {
            return false;
        }
        while let Some(p) = self.publications.get(self.next)
            && p.published_at_nanos <= received_at_nanos
        {
            let day = ist_day_of_nanos(p.published_at_nanos);
            let process = p.process_started_at_nanos;
            // The live drain carries state only within one process (a new
            // process starts empty) and one IST day.
            let carry = self.next > 0 && day == self.day && process == self.process;
            // A later process's start ends this publication's frames. O(16).
            self.until_nanos = self
                .publications
                .iter()
                .skip(self.next + 1)
                .map(|later| later.process_started_at_nanos)
                .find(|&started| process > 0 && started > process)
                .unwrap_or(i64::MAX);
            let table = Arc::new(BackupTable::build(&p.contracts));
            // APPROVED: cold — one fresh state per publication switch (a few a day), never per frame.
            let slots = vec![Slot::fresh(); table.len];
            self.from_nanos = p.published_at_nanos;
            self.day = day;
            self.process = process;
            self.next += 1;
            let _retired = self.dedup.swap_in(table, slots, day, carry);
        }
        self.next > 0
            && !self.dedup.is_empty()
            && received_at_nanos >= self.from_nanos
            && received_at_nanos < self.until_nanos
            && ist_day_of_nanos(received_at_nanos) == self.day
    }

    /// The table, for the replayed packets [`Self::select`] said it covers.
    #[inline]
    pub fn dedup_mut(&mut self) -> &mut BackupDedup {
        &mut self.dedup
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use tickvault_common::types::ExchangeSegment;

    const SEG: u8 = 2; // NSE_FNO

    fn tick(sid: u64, cum: u32, ltt: u32, ltp: f32) -> ParsedTick {
        ParsedTick {
            security_id: sid,
            exchange_segment_code: SEG,
            last_traded_price: ltp,
            exchange_timestamp: ltt,
            volume: cum,
            ..ParsedTick::default()
        }
    }

    /// The packet bytes a socket would carry for `t` (stand-in: the fields
    /// that differ between states, laid out as bytes).
    fn bytes(t: &ParsedTick, book: u32) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&t.security_id.to_le_bytes());
        b.extend_from_slice(&t.volume.to_le_bytes());
        b.extend_from_slice(&t.exchange_timestamp.to_le_bytes());
        b.extend_from_slice(&t.last_traded_price.to_le_bytes());
        b.extend_from_slice(&book.to_le_bytes());
        b
    }

    fn dedup_with(sids: &[u64]) -> BackupDedup {
        let mut d = BackupDedup::new();
        let set: Vec<(u64, u8)> = sids.iter().map(|&s| (s, SEG)).collect();
        d.install(&set);
        d
    }

    #[test]
    fn test_plan_backup_set_keeps_rank_order_room_and_never_the_same_socket() {
        let inst = |s: u64| SubscribeInstrument {
            security_id: s,
            segment: ExchangeSegment::NseFno,
        };
        let ranked: Vec<_> = (1..=10).map(inst).collect();
        // 3 and 7 have their first copy on the backup socket itself.
        let elsewhere: HashSet<(u64, u8)> = (1..=10)
            .filter(|s| *s != 3 && *s != 7)
            .map(|s| (s, ExchangeSegment::NseFno.binary_code()))
            .collect();
        let set = plan_backup_set(&ranked, &elsewhere, 100, 5);
        let ids: Vec<u64> = set.iter().map(|i| i.security_id).collect();
        assert_eq!(ids, vec![1, 2, 4, 5, 6], "rank order, same-socket skipped");
        // Room binds before top_n: the per-connection cap is never stretched.
        assert_eq!(plan_backup_set(&ranked, &elsewhere, 2, 5).len(), 2);
        assert!(plan_backup_set(&ranked, &elsewhere, 0, 5).is_empty());
        assert!(plan_backup_set(&ranked, &elsewhere, 100, 0).is_empty());
        // A duplicate in the ranking is taken once.
        let doubled = [inst(1), inst(1), inst(2)];
        assert_eq!(plan_backup_set(&doubled, &elsewhere, 10, 10).len(), 2);
    }

    #[test]
    fn test_admit_tick_first_copy_wins_and_the_second_is_dropped() {
        let mut d = dedup_with(&[42]);
        let t = tick(42, 100, 1_000, 10.0);
        let b = bytes(&t, 7);
        assert_eq!(d.admit_tick(&t, &b, 0, 10), BackupVerdict::Accept);
        assert_eq!(d.admit_tick(&t, &b, 4, 12), BackupVerdict::DropIdentical);
        // A contract outside the set is never touched.
        let other = tick(43, 100, 1_000, 10.0);
        assert_eq!(
            d.admit_tick(&other, &bytes(&other, 7), 0, 10),
            BackupVerdict::NotBackup
        );
    }

    #[test]
    fn test_admit_tick_older_copy_is_dropped_newer_from_backup_accepted() {
        let mut d = dedup_with(&[42]);
        let (a, b2, c) = (
            tick(42, 100, 1_000, 10.0),
            tick(42, 110, 1_001, 10.5),
            tick(42, 120, 1_002, 10.4),
        );
        // Primary (conn 0) delivers a and b; the backup (conn 4) lags.
        assert_eq!(
            d.admit_tick(&a, &bytes(&a, 1), 0, 10),
            BackupVerdict::Accept
        );
        assert_eq!(
            d.admit_tick(&b2, &bytes(&b2, 1), 0, 11),
            BackupVerdict::Accept
        );
        // The backup's copy of `a` with a different book is OLDER: dropped.
        assert_eq!(
            d.admit_tick(&a, &bytes(&a, 9), 4, 12),
            BackupVerdict::DropOlder
        );
        // The primary goes silent; the backup's newer copy is accepted.
        assert_eq!(
            d.admit_tick(&c, &bytes(&c, 1), 4, 5_000),
            BackupVerdict::Accept
        );
        // A same-trade, new-book update is a real repeat quote: accepted.
        assert_eq!(
            d.admit_tick(&c, &bytes(&c, 2), 4, 5_001),
            BackupVerdict::Accept
        );
    }

    #[test]
    fn test_admit_tick_wrapped_cumulative_reads_as_newer() {
        let mut d = dedup_with(&[42]);
        let before = tick(42, u32::MAX - 5, 1_000, 10.0);
        let after = tick(42, 4, 1_001, 10.0);
        assert_eq!(
            d.admit_tick(&before, &bytes(&before, 0), 0, 1),
            BackupVerdict::Accept
        );
        assert_eq!(
            d.admit_tick(&after, &bytes(&after, 0), 0, 2),
            BackupVerdict::Accept
        );
        assert_eq!(
            d.admit_tick(&before, &bytes(&before, 3), 4, 3),
            BackupVerdict::DropOlder
        );
    }

    #[test]
    fn test_on_frame_installs_a_published_set_and_backup_only_is_counted() {
        // The published set is process-global: hold the lock, publish, and
        // put back an empty set so no other ingest keeps this one.
        let _guard = TEST_PUBLISH_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        publish_backup_set(
            &[SubscribeInstrument {
                security_id: 42,
                segment: ExchangeSegment::NseFno,
            }],
            1,
        );
        let mut d = BackupDedup::new();
        assert!(d.is_empty());
        d.on_frame(0, 100);
        d.on_frame(4, 100);
        // `d` calls no further `on_frame`, so the reset cannot reach it.
        publish_backup_set(&[], 1);
        assert_eq!(d.len(), 1, "the published set is installed on the frame");
        let a = tick(42, 100, 1_000, 10.0);
        assert_eq!(
            d.admit_tick(&a, &bytes(&a, 0), 0, 100),
            BackupVerdict::Accept
        );
        let a2 = tick(42, 100, 1_000, 10.0);
        assert_eq!(
            d.admit_tick(&a2, &bytes(&a2, 0), 4, 101),
            BackupVerdict::DropIdentical
        );
        let b = tick(42, 110, 1_001, 10.0);
        assert_eq!(
            d.admit_tick(&b, &bytes(&b, 0), 4, 102),
            BackupVerdict::Accept
        );
        let slot = d.slots[0];
        assert_eq!((slot.conn_a, slot.conn_b), (0, 4), "the pair is learned");
        assert_eq!(slot.peer_of(4), Some(0));
        // Replay frames (u8::MAX) never learn a pair.
        let mut r = dedup_with(&[42]);
        assert_eq!(
            r.admit_tick(&a, &bytes(&a, 0), u8::MAX, 1),
            BackupVerdict::Accept
        );
        assert_eq!(r.slots[0].conn_a, u8::MAX);
    }

    #[test]
    fn test_admit_aux_drops_the_identical_previous_close_copy() {
        let mut d = dedup_with(&[42]);
        let pkt = [6u8, 16, 0, 2, 42, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(d.admit_aux(42, SEG, &pkt, 0), BackupVerdict::Accept);
        assert_eq!(d.admit_aux(42, SEG, &pkt, 4), BackupVerdict::DropIdentical);
        assert_eq!(d.admit_aux(43, SEG, &pkt, 0), BackupVerdict::NotBackup);
    }

    #[test]
    fn test_install_empty_set_deactivates() {
        let mut d = dedup_with(&[42]);
        assert_eq!(d.len(), 1);
        d.install(&[]);
        assert!(d.is_empty());
        let t = tick(42, 1, 1, 1.0);
        assert_eq!(
            d.admit_tick(&t, &bytes(&t, 0), 0, 1),
            BackupVerdict::NotBackup
        );
    }

    #[test]
    fn test_new_len_is_empty_start_empty_and_count_distinct_keys() {
        let mut d = BackupDedup::new();
        assert!(d.is_empty());
        assert_eq!(d.len(), 0);
        // A repeated key is held once; the same id on another segment is a
        // different contract (I-P1-11).
        d.install(&[(42, SEG), (42, SEG), (42, 1)]);
        assert_eq!(d.len(), 2);
        assert!(!d.is_empty());
    }

    #[test]
    fn test_packet_fingerprint_is_never_zero_and_separates_bytes() {
        assert_ne!(packet_fingerprint(&[]), 0);
        assert_ne!(
            packet_fingerprint(&[1, 2, 3]),
            packet_fingerprint(&[1, 2, 4])
        );
        let long: Vec<u8> = (0..162u8).collect();
        let mut other = long.clone();
        other[161] ^= 1;
        assert_ne!(packet_fingerprint(&long), packet_fingerprint(&other));
        assert_eq!(packet_fingerprint(&long), packet_fingerprint(&long));
    }

    #[test]
    fn test_publish_backup_set_bumps_the_generation() {
        // Publishes an EMPTY set, so any other test's ingest that installs it
        // stays inactive.
        let _guard = TEST_PUBLISH_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = GENERATION.load(Ordering::Acquire);
        publish_backup_set(&[], 1);
        assert!(GENERATION.load(Ordering::Acquire) > before);
    }

    /// Attack-pass finding 4: a true same-socket identical repeat is a real
    /// repeat and is kept; only the OTHER socket's copy is dropped, once per
    /// accepted packet.
    #[test]
    fn test_regression_same_socket_identical_repeat_is_kept() {
        let mut d = dedup_with(&[42]);
        let t = tick(42, 100, 1_000, 10.0);
        let b = bytes(&t, 7);
        assert_eq!(d.admit_tick(&t, &b, 0, 10), BackupVerdict::Accept);
        assert_eq!(
            d.admit_tick(&t, &b, 0, 11),
            BackupVerdict::Accept,
            "the same socket repeating a packet is a real repeat"
        );
        // The other socket's two copies pair with the two accepted ones.
        assert_eq!(d.admit_tick(&t, &b, 4, 12), BackupVerdict::DropIdentical);
        assert_eq!(d.admit_tick(&t, &b, 4, 13), BackupVerdict::DropIdentical);
        // A third copy from the other socket has nothing left to pair with.
        assert_eq!(d.admit_tick(&t, &b, 4, 14), BackupVerdict::Accept);
        // The same for previous-close / open-interest packets.
        let pkt = [6u8, 16, 0, 2, 42, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(d.admit_aux(42, SEG, &pkt, 0), BackupVerdict::Accept);
        assert_eq!(d.admit_aux(42, SEG, &pkt, 0), BackupVerdict::Accept);
        assert_eq!(d.admit_aux(42, SEG, &pkt, 4), BackupVerdict::DropIdentical);
    }

    /// Attack-pass finding 5a: a newer cumulative with an earlier trade time
    /// no longer moves the newest trade time backwards, so a later lagging
    /// copy at that earlier time cannot slip through.
    #[test]
    fn test_regression_newest_ltt_never_goes_backwards() {
        let mut d = dedup_with(&[42]);
        let a = tick(42, 100, 1_005, 10.0);
        let b = tick(42, 110, 1_003, 10.0);
        assert_eq!(d.admit_tick(&a, &bytes(&a, 0), 0, 1), BackupVerdict::Accept);
        assert_eq!(d.admit_tick(&b, &bytes(&b, 0), 0, 2), BackupVerdict::Accept);
        assert_eq!(
            d.slots[0].newest_ltt, 1_005,
            "max, never the later packet's"
        );
        assert_eq!(d.slots[0].newest_cum, 110);
        let lag = tick(42, 110, 1_004, 10.0);
        assert_eq!(
            d.admit_tick(&lag, &bytes(&lag, 3), 4, 3),
            BackupVerdict::DropOlder
        );
    }

    /// Attack-pass finding 5b: a copy lagging more than the ring behind has
    /// lost its fingerprint, and is still dropped as stale because it is
    /// older than the newest accepted.
    #[test]
    fn test_regression_lagging_copy_beyond_the_ring_is_dropped_as_older() {
        let mut d = dedup_with(&[42]);
        let stream: Vec<ParsedTick> = (0..(FINGERPRINT_RING as u32 + 8))
            .map(|i| tick(42, 100 + i, 1_000 + i, 10.0))
            .collect();
        for t in &stream {
            assert_eq!(d.admit_tick(t, &bytes(t, 0), 0, 1), BackupVerdict::Accept);
        }
        // The backup socket delivers its copy of the very first packet now.
        let first = &stream[0];
        assert_eq!(
            d.admit_tick(first, &bytes(first, 0), 4, 2),
            BackupVerdict::DropOlder
        );
        // And a copy of the newest is still recognised as identical.
        let last = &stream[stream.len() - 1];
        assert_eq!(
            d.admit_tick(last, &bytes(last, 0), 4, 3),
            BackupVerdict::DropIdentical
        );
    }

    /// Attack-pass finding 3: the publisher builds the table AND the drain's
    /// fresh state; the drain adopts them with a swap and hands its previous
    /// state back for the publisher to free.
    #[test]
    fn test_regression_publish_backup_set_builds_off_the_drain_and_adopt_swaps() {
        let _guard = TEST_PUBLISH_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        publish_backup_set(
            &[
                SubscribeInstrument {
                    security_id: 42,
                    segment: ExchangeSegment::NseFno,
                },
                SubscribeInstrument {
                    security_id: 43,
                    segment: ExchangeSegment::NseFno,
                },
            ],
            1,
        );
        assert_eq!(published_keys(), vec![(42, SEG), (43, SEG)]);
        let p = published().load_full().expect("published");
        {
            let h = p.handoff.lock().expect("handoff");
            assert_eq!(
                h.fresh_slots.as_ref().map(Vec::len),
                Some(2),
                "the drain's state is pre-built by the publisher"
            );
            assert!(h.retired.is_none());
        }
        let mut d = BackupDedup::new();
        d.on_frame(0, 1);
        assert_eq!(d.len(), 2);
        assert!(
            Arc::ptr_eq(&d.table, &p.table),
            "the table is shared, not rebuilt"
        );
        {
            let h = p.handoff.lock().expect("handoff");
            assert!(
                h.fresh_slots.is_none(),
                "the drain took the pre-built state"
            );
            assert!(h.retired.is_some(), "and handed its old state back");
        }
        // The next frame adopts nothing new.
        d.on_frame(0, 2);
        assert_eq!(d.generation_seen, p.generation);
        publish_backup_set(&[], 1);
        d.on_frame(0, 3);
        assert!(
            d.is_empty(),
            "an empty publication deactivates the drain's table"
        );
    }

    /// 2026-10-02 10:00 IST = 04:30 UTC, in epoch nanos.
    const TEN_AM: i64 = 1_790_915_400 * 1_000_000_000;
    const MIN: i64 = 60 * 1_000_000_000;
    const DAY: i64 = 86_400 * 1_000_000_000;

    fn history_of(pubs: &[(i64, &[u64])]) -> PersistedBackupHistory {
        PersistedBackupHistory {
            publications: pubs
                .iter()
                .map(|(at, sids)| PersistedBackupSet {
                    published_at_nanos: *at,
                    process_started_at_nanos: 0,
                    contracts: sids.iter().map(|&s| (s, SEG)).collect(),
                })
                .collect(),
        }
    }

    fn temp_path(tag: &[u8]) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "tv-backup-set-{}-{}",
            std::process::id(),
            packet_fingerprint(tag)
        ));
        let path = dir.join("set.json");
        (dir, path)
    }

    #[test]
    fn test_record_backup_set_then_read_backup_history_roundtrips_and_missing_or_corrupt_reads_honestly()
     {
        let (dir, path) = temp_path(b"roundtrip");
        let _ = std::fs::remove_file(&path);
        assert_eq!(read_backup_history(&path), Ok(None), "absent reads as none");
        let set = PersistedBackupSet::from_set(
            &[SubscribeInstrument {
                security_id: 42,
                segment: ExchangeSegment::NseFno,
            }],
            TEN_AM,
        );
        assert_eq!(
            set.contracts,
            vec![(42, SEG)],
            "from_set keeps the composite key"
        );
        record_backup_set(&path, &set).expect("write");
        let read = read_backup_history(&path)
            .expect("readable")
            .expect("saved");
        assert_eq!(read.latest(), Some(&set));
        std::fs::write(&path, b"{not json").expect("corrupt");
        assert!(
            read_backup_history(&path).is_err(),
            "a corrupt file is an error"
        );
        // Recording over a corrupt file keeps the new publication.
        record_backup_set(&path, &set).expect("write over corrupt");
        let read = read_backup_history(&path)
            .expect("readable")
            .expect("saved");
        assert_eq!(read.publications, vec![set]);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    /// Review 2026-10-02, F1: each publication is ADDED to a bounded history
    /// instead of overwriting the one set, so a later publication cannot
    /// erase the set an earlier, unreplayed frame was deduplicated with.
    #[test]
    fn test_regression_persisted_history_keeps_earlier_publications_bounded() {
        let (dir, path) = temp_path(b"history");
        let _ = std::fs::remove_file(&path);
        let at_0920 = TEN_AM - 40 * MIN;
        let at_1105 = TEN_AM + 65 * MIN;
        let first = PersistedBackupSet {
            published_at_nanos: at_0920,
            process_started_at_nanos: 0,
            contracts: vec![(42, SEG)],
        };
        let second = PersistedBackupSet {
            published_at_nanos: at_1105,
            process_started_at_nanos: 0,
            contracts: vec![(43, SEG)],
        };
        record_backup_set(&path, &first).expect("first");
        record_backup_set(&path, &second).expect("second");
        let read = read_backup_history(&path)
            .expect("readable")
            .expect("saved");
        assert_eq!(
            read.publications,
            vec![first.clone(), second.clone()],
            "09:20 survives the 11:05 publication, oldest first"
        );
        // The narrowing of an attach shares its instant: it replaces.
        let narrowed = PersistedBackupSet {
            published_at_nanos: at_1105,
            process_started_at_nanos: 0,
            contracts: Vec::new(),
        };
        record_backup_set(&path, &narrowed).expect("narrowed");
        let read = read_backup_history(&path)
            .expect("readable")
            .expect("saved");
        assert_eq!(read.publications, vec![first.clone(), narrowed]);

        // An earlier build's single-object file reads as a history of one.
        std::fs::write(&path, serde_json::to_vec(&first).expect("json")).expect("old format");
        let read = read_backup_history(&path)
            .expect("readable")
            .expect("saved");
        assert_eq!(read.publications, vec![first]);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);

        // Only the two most recent IST days that have a publication: a
        // Friday set still covers Friday's frames replayed on Monday.
        let mut h = PersistedBackupHistory::default();
        let friday = TEN_AM - 3 * DAY;
        h.record(PersistedBackupSet {
            published_at_nanos: friday - DAY,
            process_started_at_nanos: 0,
            contracts: vec![(1, SEG)],
        });
        h.record(PersistedBackupSet {
            published_at_nanos: friday,
            process_started_at_nanos: 0,
            contracts: vec![(2, SEG)],
        });
        h.record(PersistedBackupSet {
            published_at_nanos: TEN_AM,
            process_started_at_nanos: 0,
            contracts: vec![(3, SEG)],
        });
        let kept: Vec<i64> = h
            .publications
            .iter()
            .map(|p| p.published_at_nanos)
            .collect();
        assert_eq!(kept, vec![friday, TEN_AM], "Thursday is dropped");
        // At most BACKUP_HISTORY_MAX, the newest; instant-less entries never.
        for i in 0..(BACKUP_HISTORY_MAX as i64 + 5) {
            h.record(PersistedBackupSet {
                published_at_nanos: TEN_AM + (i + 1) * MIN,
                process_started_at_nanos: 0,
                contracts: vec![(4, SEG)],
            });
        }
        h.record(PersistedBackupSet {
            published_at_nanos: 0,
            process_started_at_nanos: 0,
            contracts: vec![(5, SEG)],
        });
        assert_eq!(h.publications.len(), BACKUP_HISTORY_MAX);
        assert_eq!(
            h.latest().map(|p| p.published_at_nanos),
            Some(TEN_AM + (BACKUP_HISTORY_MAX as i64 + 5) * MIN)
        );
        assert!(h.publications.iter().all(|p| p.published_at_nanos > 0));
    }

    #[test]
    fn test_write_backup_history_round_trips_through_read_backup_history() {
        let (dir, _) = temp_path(b"write-history");
        // A directory that does not exist yet, two levels deep: the write
        // must create it.
        let nested = dir.join("a").join("b");
        let path = nested.join("set.json");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!nested.exists());

        let history = history_of(&[(TEN_AM - 30 * MIN, &[42, 43]), (TEN_AM, &[44])]);
        write_backup_history(&path, &history).expect("write");
        assert!(path.is_file(), "the history lands at the path");
        assert!(
            !path.with_extension("json.tmp").exists(),
            "the temp file is renamed away, never left behind"
        );
        assert_eq!(
            read_backup_history(&path).expect("readable"),
            Some(history.clone()),
            "what is written is what is read"
        );

        // A second write replaces the whole file, never appends to it.
        let replacement = history_of(&[(TEN_AM + MIN, &[45])]);
        write_backup_history(&path, &replacement).expect("rewrite");
        assert_eq!(
            read_backup_history(&path).expect("readable"),
            Some(replacement)
        );

        // An empty history round-trips as empty, not as absent.
        write_backup_history(&path, &PersistedBackupHistory::default()).expect("empty");
        assert_eq!(
            read_backup_history(&path).expect("readable"),
            Some(PersistedBackupHistory::default())
        );

        // The writer stores what it is given; the reader bounds it. An
        // oversized, unordered history with an instant-less entry reads back
        // ascending, at most BACKUP_HISTORY_MAX, newest kept, none instant-less.
        let seven: &[u64] = &[7];
        let eight: &[u64] = &[8];
        let mut oversized: Vec<(i64, &[u64])> = (0..(BACKUP_HISTORY_MAX as i64 + 4))
            .rev()
            .map(|i| (TEN_AM + i * MIN, seven))
            .collect();
        oversized.push((0, eight));
        write_backup_history(&path, &history_of(&oversized)).expect("oversized");
        let read = read_backup_history(&path)
            .expect("readable")
            .expect("saved");
        assert_eq!(read.publications.len(), BACKUP_HISTORY_MAX);
        assert!(read.publications.iter().all(|p| p.published_at_nanos > 0));
        assert!(
            read.publications
                .windows(2)
                .all(|w| w[0].published_at_nanos < w[1].published_at_nanos),
            "ascending by instant"
        );
        assert_eq!(
            read.latest().map(|p| p.published_at_nanos),
            Some(TEN_AM + (BACKUP_HISTORY_MAX as i64 + 3) * MIN)
        );

        // A path whose parent is a FILE cannot be written: an error, never a
        // silent success.
        let blocked = path.join("under-a-file.json");
        assert!(write_backup_history(&blocked, &history).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_process_epoch_nanos_is_stable_within_a_process() {
        let first = process_epoch_nanos();
        assert!(first > 0, "a real UTC instant, not the unset 0");
        let now = chrono::Utc::now()
            .timestamp_nanos_opt()
            .expect("now fits i64 nanos");
        assert!(first <= now, "read once, never in the future");
        // Every later read, from any thread, is the same instant.
        assert_eq!(process_epoch_nanos(), first);
        let from_threads: Vec<i64> = (0..4)
            .map(|_| std::thread::spawn(process_epoch_nanos))
            .map(|h| h.join().expect("thread"))
            .collect();
        assert!(from_threads.iter().all(|&e| e == first));
        // And it is what a publication from this process carries.
        let set = PersistedBackupSet::from_set(&[], TEN_AM);
        assert_eq!(set.process_started_at_nanos, first);
    }

    #[test]
    fn test_backup_set_path_sits_beside_the_other_daily_artifacts() {
        let path = backup_set_path();
        assert!(path.starts_with("data/instrument-cache"));
        assert!(path.ends_with(BACKUP_SET_FILE));
    }

    /// Attack-pass finding 1: the replay applies a set only to frames the
    /// live drain deduplicated with it.
    #[test]
    fn test_regression_replay_backup_covers_only_frames_after_publication_on_the_same_day() {
        let mut r = ReplayBackup::new(&history_of(&[(TEN_AM, &[42])])).expect("a set");
        assert!(
            !r.select(0),
            "a frame with no receipt clock is never judged"
        );
        assert!(
            !r.select(TEN_AM - 1),
            "before the publication: one copy only"
        );
        assert!(r.select(TEN_AM));
        assert!(r.select(TEN_AM + 60 * MIN));
        assert!(
            !r.select(TEN_AM + DAY),
            "the next day's cumulative restarts; yesterday's set never judges it"
        );
        assert!(
            ReplayBackup::new(&history_of(&[(0, &[42])])).is_none(),
            "a set with no instant covers nothing"
        );
        assert!(
            ReplayBackup::new(&history_of(&[(TEN_AM, &[])])).is_none(),
            "an empty history deduplicates nothing"
        );
    }

    /// Review 2026-10-02, F1: process 1 published at 09:20 and crashed at
    /// 11:00; process 2 published at 11:05. The replay judges each frame with
    /// the publication in force when it was received — 09:20 frames with the
    /// 09:20 set, 11:05 frames with the 11:05 set, yesterday's frames with
    /// yesterday's — and leaves an out-of-order frame unjudged (accepted).
    #[test]
    fn test_regression_replay_uses_the_publication_in_force_at_each_frame() {
        let at_0920 = TEN_AM - 40 * MIN;
        let at_1105 = TEN_AM + 65 * MIN;
        let yesterday = TEN_AM - DAY + 4 * 60 * MIN;
        let mut r = ReplayBackup::new(&history_of(&[
            (yesterday, &[44]),
            (at_0920, &[42]),
            (at_1105, &[43]),
        ]))
        .expect("a set");
        let both_copies = |r: &mut ReplayBackup, at: i64, sid: u64, cum: u32| {
            let t = tick(sid, cum, 1_000 + cum, 10.0);
            let b = bytes(&t, 0);
            if !r.select(at) {
                return (BackupVerdict::NotBackup, BackupVerdict::NotBackup);
            }
            let d = r.dedup_mut();
            (
                d.admit_tick(&t, &b, u8::MAX, 0),
                d.admit_tick(&t, &b, u8::MAX, 0),
            )
        };
        let pair = (BackupVerdict::Accept, BackupVerdict::DropIdentical);
        let unjudged = (BackupVerdict::NotBackup, BackupVerdict::NotBackup);
        // Yesterday's segment, replayed after today's publications.
        assert_eq!(both_copies(&mut r, yesterday + MIN, 44, 10), pair);
        // Today before the first publication: not judged.
        assert_eq!(both_copies(&mut r, at_0920 - MIN, 42, 20), unjudged);
        // 09:20–11:00 with the 09:20 set: the copy is dropped.
        assert_eq!(both_copies(&mut r, TEN_AM, 42, 30), pair);
        assert_eq!(both_copies(&mut r, TEN_AM, 43, 30), unjudged);
        // From 11:05 with the 11:05 set.
        assert_eq!(both_copies(&mut r, at_1105 + MIN, 43, 40), pair);
        assert_eq!(both_copies(&mut r, at_1105 + MIN, 42, 40), unjudged);
        // A 10:30 frame arriving after an 11:06 one: accepted, never judged
        // by the wrong set.
        assert!(!r.select(TEN_AM + 30 * MIN));
    }

    /// Review 2026-10-02, F1: process 2 started at 11:00 and published at
    /// 11:05. Its frames from 11:00 to 11:05 were folded live with NO set (a
    /// fresh drain), so the replay must not judge them with process 1's 09:20
    /// set — a reconnect snapshot identical to process 1's last packet would
    /// otherwise be dropped. And the 11:05 switch starts fresh, as process
    /// 2's drain did.
    #[test]
    fn test_regression_replay_stops_a_publication_at_a_later_process_start() {
        let at_0920 = TEN_AM - 40 * MIN;
        let p2_start = TEN_AM + 60 * MIN;
        let at_1105 = TEN_AM + 65 * MIN;
        let history = PersistedBackupHistory {
            publications: vec![
                PersistedBackupSet {
                    published_at_nanos: at_0920,
                    process_started_at_nanos: at_0920 - MIN,
                    contracts: vec![(42, SEG)],
                },
                PersistedBackupSet {
                    published_at_nanos: at_1105,
                    process_started_at_nanos: p2_start,
                    contracts: vec![(42, SEG)],
                },
            ],
        };
        let mut r = ReplayBackup::new(&history).expect("a set");
        let t = tick(42, 100, 1_000, 10.0);
        let b = bytes(&t, 0);
        assert!(r.select(p2_start - MIN), "process 1's frame: judged");
        assert_eq!(
            r.dedup_mut().admit_tick(&t, &b, u8::MAX, 0),
            BackupVerdict::Accept
        );
        assert!(
            !r.select(p2_start + MIN),
            "process 2 before its own publication: not judged (its snapshot is kept)"
        );
        assert!(r.select(at_1105 + MIN));
        assert_eq!(
            r.dedup_mut().admit_tick(&t, &b, u8::MAX, 0),
            BackupVerdict::Accept,
            "process 2's drain started empty; nothing carries across processes"
        );
    }

    /// Review 2026-10-02, F2, replay half: switching publications in the
    /// replay carries a staying contract's state exactly as the live adopt
    /// does — and only within one IST day.
    #[test]
    fn test_regression_replay_switch_carries_state_like_the_live_adopt() {
        let t = tick(42, 100, 1_000, 10.0);
        let b = bytes(&t, 0);
        let narrowed = TEN_AM + 1_250_000_000;
        let mut r =
            ReplayBackup::new(&history_of(&[(TEN_AM, &[42, 43]), (narrowed, &[42])])).expect("set");
        assert!(r.select(TEN_AM + 1));
        assert_eq!(
            r.dedup_mut().admit_tick(&t, &b, u8::MAX, 0),
            BackupVerdict::Accept
        );
        assert!(r.select(narrowed + 1));
        assert_eq!(
            r.dedup_mut().admit_tick(&t, &b, u8::MAX, 0),
            BackupVerdict::DropIdentical,
            "the in-flight second copy is still recognised after the switch"
        );
        // A publication on another day starts fresh: yesterday's newest
        // volume never judges today's restarted cumulative.
        let mut r =
            ReplayBackup::new(&history_of(&[(TEN_AM - DAY, &[42]), (TEN_AM, &[42])])).expect("set");
        assert!(r.select(TEN_AM - DAY + 1));
        assert_eq!(
            r.dedup_mut().admit_tick(&t, &b, u8::MAX, 0),
            BackupVerdict::Accept
        );
        assert!(r.select(TEN_AM + 1));
        let low = tick(42, 5, 900, 10.0);
        assert_eq!(
            r.dedup_mut().admit_tick(&low, &bytes(&low, 0), u8::MAX, 0),
            BackupVerdict::Accept,
            "today's lower cumulative is not judged older than yesterday's"
        );
    }

    /// Review 2026-10-02, F2: a republication (here the held subset after a
    /// truncated subscribe, 1.25 s later) keeps the dedup state of the
    /// contracts that stay, so the second copy already in flight is still
    /// dropped; a contract new to the set starts fresh; a publication of
    /// another IST day carries nothing.
    #[test]
    fn test_regression_adopt_carries_state_for_contracts_that_stay_in_the_set() {
        let inst = |s: u64| SubscribeInstrument {
            security_id: s,
            segment: ExchangeSegment::NseFno,
        };
        let _guard = TEST_PUBLISH_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut d = BackupDedup::new();
        publish_backup_set(&[inst(42), inst(43), inst(44)], TEN_AM);
        d.on_frame(0, 1);
        let t = tick(42, 100, 1_000, 10.0);
        let b = bytes(&t, 0);
        assert_eq!(d.admit_tick(&t, &b, 0, 1), BackupVerdict::Accept);
        // The held subset is republished; the drain adopts it.
        publish_backup_set(&[inst(42), inst(45)], TEN_AM + 1_250_000_000);
        d.on_frame(0, 2);
        assert_eq!(d.len(), 2);
        assert_eq!(
            d.admit_tick(&t, &b, 4, 3),
            BackupVerdict::DropIdentical,
            "the other socket's in-flight copy is still a second copy"
        );
        let fresh = tick(45, 7, 1_000, 10.0);
        assert_eq!(
            d.admit_tick(&fresh, &bytes(&fresh, 0), 4, 4),
            BackupVerdict::Accept
        );
        // The next day's publication carries nothing.
        publish_backup_set(&[inst(42)], TEN_AM + DAY);
        d.on_frame(0, 5);
        let low = tick(42, 5, 900, 10.0);
        assert_eq!(
            d.admit_tick(&low, &bytes(&low, 0), 0, 6),
            BackupVerdict::Accept,
            "a new day's restarted cumulative is not judged by yesterday's"
        );
        publish_backup_set(&[], TEN_AM + DAY);
        d.on_frame(0, 7);
        assert!(d.is_empty());
    }

    /// A replayed frame carries no socket: the second identical copy is still
    /// dropped, and a third (the other socket's repeat) pairs with the next.
    #[test]
    fn test_regression_replay_unknown_socket_drops_the_second_copy() {
        let mut r = ReplayBackup::new(&history_of(&[(1, &[42])])).expect("a set");
        assert!(r.select(1));
        let t = tick(42, 100, 1_000, 10.0);
        let b = bytes(&t, 0);
        let d = r.dedup_mut();
        assert_eq!(d.admit_tick(&t, &b, u8::MAX, 0), BackupVerdict::Accept);
        assert_eq!(
            d.admit_tick(&t, &b, u8::MAX, 0),
            BackupVerdict::DropIdentical
        );
        let pkt = [6u8, 16, 0, 2, 42, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(d.admit_aux(42, SEG, &pkt, u8::MAX), BackupVerdict::Accept);
        assert_eq!(
            d.admit_aux(42, SEG, &pkt, u8::MAX),
            BackupVerdict::DropIdentical
        );
    }
    proptest! {
        /// Two sockets carry the same stream of trades, each possibly
        /// delayed against the other, interleaved in any order. Whatever the
        /// order, the accepted stream's cumulative volume never goes
        /// backwards and sums to exactly the true total — volume is never
        /// double counted and never lost while either socket delivers.
        #[test]
        fn proptest_two_socket_interleave_never_double_counts(
            steps in proptest::collection::vec(1u32..50, 1..40),
            order in proptest::collection::vec(any::<bool>(), 0..120),
        ) {
            let mut cum = 0u32;
            let stream: Vec<(ParsedTick, Vec<u8>)> = steps
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    cum += *s;
                    let t = tick(42, cum, 1_000 + i as u32, 10.0);
                    let b = bytes(&t, 0);
                    (t, b)
                })
                .collect();
            let total = cum;
            let mut d = dedup_with(&[42]);
            let (mut ia, mut ib) = (0usize, 0usize);
            let mut accepted: Vec<u32> = Vec::new();
            let mut k = 0usize;
            while ia < stream.len() || ib < stream.len() {
                let take_a = if ia >= stream.len() {
                    false
                } else if ib >= stream.len() {
                    true
                } else {
                    order.get(k).copied().unwrap_or(true)
                };
                k += 1;
                let (conn, idx) = if take_a { (0u8, &mut ia) } else { (4u8, &mut ib) };
                let (t, b) = &stream[*idx];
                *idx += 1;
                if d.admit_tick(t, b, conn, k as u64) == BackupVerdict::Accept {
                    accepted.push(t.volume);
                }
            }
            prop_assert!(accepted.windows(2).all(|w| w[0] < w[1]), "cumulative went backwards");
            let mut prev = 0u32;
            let summed: u64 = accepted.iter().map(|c| { let d = c - prev; prev = *c; u64::from(d) }).sum();
            prop_assert_eq!(summed, u64::from(total));
            prop_assert_eq!(accepted.last().copied(), Some(total));
        }
    }
}

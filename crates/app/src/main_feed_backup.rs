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
//! **Replay.** Each publication is also persisted ([`write_backup_set`]); the
//! WAL replay rebuilds the table from it ([`ReplayBackup`]) and drops the
//! second copies of the frames the live drain deduplicated with it. A replayed
//! frame carries no socket, so there an identical unpaired entry from any
//! socket matches.
//!
//! # Cost
//!
//! Disabled or not yet published: one `bool` test per packet and one relaxed
//! atomic load per frame. Published: one bit test in an 8 KiB filter per tick;
//! only a filter hit (the backup set, plus ~1.5% false positives at 1,000
//! entries) pays one hash probe, and a backup contract's packet pays a
//! fingerprint over its bytes (~20 word mixes for a 162-byte Full packet) and a
//! 16-entry compare. O(1), zero allocation per packet. A new set is built by
//! the publisher, off the drain; the drain adopts it with a pointer swap and
//! hands its previous state back for the publisher to free.

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
/// publisher's task, so the drain pays a pointer swap. Called BEFORE the
/// subscribe goes out (a copy can then never arrive undeduplicated), and
/// again with the held subset or an empty set when the socket answers.
pub fn publish_backup_set(set: &[SubscribeInstrument]) {
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

/// The drain's backup dedup table. Owned by `LiveIngest`, single-threaded.
pub struct BackupDedup {
    active: bool,
    generation_seen: u64,
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
    /// An empty, inactive table. Allocates the 8 KiB filter only.
    #[must_use]
    pub fn new() -> Self {
        Self {
            active: false,
            generation_seen: 0,
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
        self.active = !self.slots.is_empty();
        self.instruments.set(self.slots.len() as f64);
    }

    /// Adopts one publication: a pointer swap for the table and the drain's
    /// state, the previous pair moved into the publication for the publisher
    /// to free. O(1), no allocation and no free in the normal case. `false`
    /// when the hand-off is busy this instant (retried on the next frame).
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
        let old_slots = std::mem::replace(&mut self.slots, slots);
        let old_table = std::mem::replace(&mut self.table, Arc::clone(&p.table));
        if handoff.retired.is_none() {
            handoff.retired = Some((old_table, old_slots));
        }
        self.active = !self.slots.is_empty();
        self.instruments.set(self.slots.len() as f64);
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

/// The persisted set. One file, overwritten by each publication; the
/// publication instant inside it says which frames it covers.
const BACKUP_SET_FILE: &str = "main-feed-backup-set.json";

/// Counter: the WAL replay's backup set, by `outcome` (`loaded`, `missing`,
/// `unreadable`). Once per lane start.
pub const BACKUP_REPLAY_SET_COUNTER: &str = "tv_dhan_feed_backup_replay_set_total";

/// What the replay needs to fold one copy of each packet exactly as the live
/// drain did: the set and the instant the drain started deduplicating it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PersistedBackupSet {
    /// UTC epoch nanos of the publication.
    pub published_at_nanos: i64,
    /// `(security_id, exchange_segment wire byte)`, rank order.
    pub contracts: Vec<(u64, u8)>,
}

impl PersistedBackupSet {
    /// The set as it is published now.
    #[must_use]
    pub fn from_set(set: &[SubscribeInstrument], published_at_nanos: i64) -> Self {
        Self {
            published_at_nanos,
            contracts: set
                .iter()
                .map(|i| (i.security_id, i.segment.binary_code()))
                .collect(),
        }
    }
}

/// Where the set is persisted.
#[must_use]
pub fn backup_set_path() -> std::path::PathBuf {
    std::path::Path::new(BACKUP_SET_DIR).join(BACKUP_SET_FILE)
}

/// Writes the set atomically (temp file, synced, then rename), creating the
/// directory if needed. Cold.
pub fn write_backup_set(path: &std::path::Path, set: &PersistedBackupSet) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec(set).map_err(std::io::Error::other)?;
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

/// Reads the persisted set: `Ok(None)` when there is none, `Err` when a
/// file exists and cannot be read or parsed. Cold.
pub fn read_backup_set(path: &std::path::Path) -> Result<Option<PersistedBackupSet>, String> {
    match std::fs::read(path) {
        Ok(body) => serde_json::from_slice(&body)
            .map(Some)
            .map_err(|e| e.to_string()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.to_string()),
    }
}

/// IST day number of a UTC epoch-nanos instant.
#[inline]
fn ist_day_of_nanos(nanos: i64) -> i64 {
    let secs = nanos.div_euclid(1_000_000_000).saturating_add(i64::from(
        tickvault_common::constants::IST_UTC_OFFSET_SECONDS,
    ));
    secs.div_euclid(86_400)
}

/// The WAL replay's dedup: the persisted set, applied only to frames the live
/// drain deduplicated with it — received at or after its publication, on the
/// same IST day (the vendor's cumulative restarts each day, so yesterday's
/// newest volume must never judge today's).
#[derive(Debug)]
pub struct ReplayBackup {
    dedup: BackupDedup,
    from_nanos: i64,
    day: i64,
}

impl ReplayBackup {
    /// Builds the replay's table. Cold, once per lane start.
    #[must_use]
    pub fn new(set: &PersistedBackupSet) -> Self {
        Self {
            dedup: BackupDedup::for_replay(&set.contracts),
            from_nanos: set.published_at_nanos,
            day: ist_day_of_nanos(set.published_at_nanos),
        }
    }

    /// Whether a frame received at `received_at_nanos` was deduplicated live
    /// with this set. O(1).
    #[inline]
    #[must_use]
    pub fn covers(&self, received_at_nanos: i64) -> bool {
        received_at_nanos >= self.from_nanos
            && self.from_nanos > 0
            && ist_day_of_nanos(received_at_nanos) == self.day
    }

    /// The table, for the replayed packets it covers.
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
        publish_backup_set(&[SubscribeInstrument {
            security_id: 42,
            segment: ExchangeSegment::NseFno,
        }]);
        let mut d = BackupDedup::new();
        assert!(d.is_empty());
        d.on_frame(0, 100);
        d.on_frame(4, 100);
        // `d` calls no further `on_frame`, so the reset cannot reach it.
        publish_backup_set(&[]);
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
        publish_backup_set(&[]);
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
        publish_backup_set(&[
            SubscribeInstrument {
                security_id: 42,
                segment: ExchangeSegment::NseFno,
            },
            SubscribeInstrument {
                security_id: 43,
                segment: ExchangeSegment::NseFno,
            },
        ]);
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
        publish_backup_set(&[]);
        d.on_frame(0, 3);
        assert!(
            d.is_empty(),
            "an empty publication deactivates the drain's table"
        );
    }

    #[test]
    fn test_write_backup_set_then_read_backup_set_roundtrips_and_missing_or_corrupt_reads_honestly()
    {
        let dir = std::env::temp_dir().join(format!(
            "tv-backup-set-{}-{}",
            std::process::id(),
            packet_fingerprint(b"roundtrip")
        ));
        let path = dir.join("set.json");
        assert_eq!(read_backup_set(&path), Ok(None), "absent reads as none");
        let set = PersistedBackupSet::from_set(
            &[SubscribeInstrument {
                security_id: 42,
                segment: ExchangeSegment::NseFno,
            }],
            1_700_000_000_000_000_000,
        );
        assert_eq!(
            set.contracts,
            vec![(42, SEG)],
            "from_set keeps the composite key"
        );
        write_backup_set(&path, &set).expect("write");
        assert_eq!(read_backup_set(&path), Ok(Some(set)));
        std::fs::write(&path, b"{not json").expect("corrupt");
        assert!(
            read_backup_set(&path).is_err(),
            "a corrupt file is an error"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_backup_set_path_sits_beside_the_other_daily_artifacts() {
        let path = backup_set_path();
        assert!(path.starts_with("data/instrument-cache"));
        assert!(path.ends_with(BACKUP_SET_FILE));
    }

    /// Attack-pass finding 1: the replay applies the set only to frames the
    /// live drain deduplicated with it.
    #[test]
    fn test_regression_replay_backup_covers_only_frames_after_publication_on_the_same_day() {
        // 2026-10-02 10:00 IST = 04:30 UTC.
        let published = 1_790_915_400i64 * 1_000_000_000;
        let r = ReplayBackup::new(&PersistedBackupSet {
            published_at_nanos: published,
            contracts: vec![(42, SEG)],
        });
        assert!(r.covers(published));
        assert!(r.covers(published + 3_600 * 1_000_000_000));
        assert!(
            !r.covers(published - 1),
            "before the publication: one copy only"
        );
        assert!(
            !r.covers(published + 86_400 * 1_000_000_000),
            "the next day's cumulative restarts; yesterday's set never judges it"
        );
        assert!(
            !r.covers(0),
            "a frame with no receipt clock is never deduplicated"
        );
        let never = ReplayBackup::new(&PersistedBackupSet {
            published_at_nanos: 0,
            contracts: vec![(42, SEG)],
        });
        assert!(
            !never.covers(published),
            "a set with no instant covers nothing"
        );
    }

    /// A replayed frame carries no socket: the second identical copy is still
    /// dropped, and a third (the other socket's repeat) pairs with the next.
    #[test]
    fn test_regression_replay_unknown_socket_drops_the_second_copy() {
        let mut r = ReplayBackup::new(&PersistedBackupSet {
            published_at_nanos: 1,
            contracts: vec![(42, SEG)],
        });
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

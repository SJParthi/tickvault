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
//! ring of recent packet fingerprints and the newest accepted
//! `(cumulative volume, trade time)`. A packet byte-identical to one in the
//! ring, or carrying an OLDER cumulative volume than the newest accepted, is
//! dropped and counted. Anything else is accepted, whichever socket it came
//! from — so when one socket is down the other's copies are simply accepted.
//!
//! # Cost
//!
//! Disabled or not yet published: one `bool` test per packet and one relaxed
//! atomic load per frame. Published: one bit test in an 8 KiB filter per tick;
//! only a filter hit (the backup set, plus ~1.5% false positives at 1,000
//! entries) pays one hash probe, and a backup contract's packet pays a
//! fingerprint over its bytes (~20 word mixes for a 162-byte Full packet) and a
//! 16-entry compare. O(1), zero allocation per packet. Installing a new set
//! allocates once, on the first frame after a publication (once a day).

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

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

fn published() -> &'static ArcSwapOption<Vec<(u64, u8)>> {
    static SET: OnceLock<ArcSwapOption<Vec<(u64, u8)>>> = OnceLock::new();
    SET.get_or_init(ArcSwapOption::empty)
}

static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Serialises the tests that publish the process-global set, so one test's
/// `on_frame` never installs another test's set mid-assertion.
#[cfg(test)]
pub(crate) static TEST_PUBLISH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Publishes the confirmed backup set to the drain, which installs it on its
/// next frame. Cold: once a day, after the subscribe is acknowledged.
pub fn publish_backup_set(set: &[SubscribeInstrument]) {
    let keys: Vec<(u64, u8)> = set
        .iter()
        .map(|i| (i.security_id, i.segment.binary_code()))
        .collect();
    published().store(Some(std::sync::Arc::new(keys)));
    GENERATION.fetch_add(1, Ordering::Release);
}

/// Per-contract dedup state. 200 bytes, `Copy`.
#[derive(Debug, Clone, Copy, Default)]
struct Slot {
    fps: [u64; FINGERPRINT_RING],
    aux: [u64; AUX_FINGERPRINT_RING],
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

/// The drain's backup dedup table. Owned by `LiveIngest`, single-threaded.
pub struct BackupDedup {
    active: bool,
    generation_seen: u64,
    filter: Box<[u64; FILTER_WORDS]>,
    index: HashMap<(u64, u8), u32>,
    slots: Vec<Slot>,
    last_frame_millis: [u64; TRACKED_CONNECTIONS],
    dropped_identical: metrics::Counter,
    dropped_older: metrics::Counter,
    backup_only: metrics::Counter,
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
            filter: Box::new([0u64; FILTER_WORDS]),
            index: HashMap::new(),
            slots: Vec::new(),
            last_frame_millis: [0; TRACKED_CONNECTIONS],
            dropped_identical: metrics::counter!(
                BACKUP_DUPLICATES_DROPPED_COUNTER,
                "reason" => "identical"
            ),
            dropped_older: metrics::counter!(BACKUP_DUPLICATES_DROPPED_COUNTER, "reason" => "older"),
            backup_only: metrics::counter!(BACKUP_ONLY_ARRIVALS_COUNTER),
        }
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

    /// Replaces the set. Cold: allocates for the new set.
    pub fn install(&mut self, set: &[(u64, u8)]) {
        self.filter.fill(0);
        self.index.clear();
        self.slots.clear();
        self.index.reserve(set.len());
        self.slots.reserve(set.len());
        for &(security_id, segment) in set {
            if self.index.contains_key(&(security_id, segment)) {
                continue;
            }
            let Ok(at) = u32::try_from(self.slots.len()) else {
                break;
            };
            self.index.insert((security_id, segment), at);
            self.slots.push(Slot::fresh());
            let bit = filter_bit(security_id, segment);
            if let Some(word) = self.filter.get_mut(bit / 64) {
                *word |= 1u64 << (bit % 64);
            }
        }
        self.active = !self.slots.is_empty();
        metrics::gauge!(BACKUP_INSTRUMENTS_GAUGE).set(self.slots.len() as f64);
    }

    /// Per frame: picks up a newly published set (one relaxed load when there
    /// is none) and stamps the frame's connection as alive.
    #[inline]
    pub fn on_frame(&mut self, connection_index: u8, recv_millis: u64) {
        let generation = GENERATION.load(Ordering::Acquire);
        if generation != self.generation_seen {
            self.generation_seen = generation;
            let set = published().load_full();
            self.install(set.as_deref().map_or(&[], Vec::as_slice));
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
        let word = self.filter.get(bit / 64).copied().unwrap_or(0);
        if word & (1u64 << (bit % 64)) == 0 {
            return None;
        }
        self.index.get(&(security_id, segment)).map(|&i| i as usize)
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
            if slot.fps.contains(&fp) {
                self.dropped_identical.increment(1);
                return BackupVerdict::DropIdentical;
            }
            let cum = tick.volume;
            let ltt = tick.exchange_timestamp;
            if slot.has_newest {
                // Serial-number compare, so a u32 wrap of the vendor's
                // cumulative reads as newer, not as a huge step back.
                let step = cum.wrapping_sub(slot.newest_cum) as i32;
                if step < 0 || (step == 0 && ltt < slot.newest_ltt) {
                    self.dropped_older.increment(1);
                    return BackupVerdict::DropOlder;
                }
                if step > 0 || ltt > slot.newest_ltt {
                    slot.newest_cum = cum;
                    slot.newest_ltt = ltt;
                }
            } else {
                slot.has_newest = true;
                slot.newest_cum = cum;
                slot.newest_ltt = ltt;
            }
            let i = usize::from(slot.next) % FINGERPRINT_RING;
            slot.fps[i] = fp;
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

    /// Verdict for a previous-close or open-interest packet: a byte-identical
    /// copy is dropped, anything else accepted. O(1), zero allocation.
    #[inline]
    pub fn admit_aux(&mut self, security_id: u64, segment: u8, packet: &[u8]) -> BackupVerdict {
        let Some(at) = self.slot_of(security_id, segment) else {
            return BackupVerdict::NotBackup;
        };
        let fp = packet_fingerprint(packet);
        let Some(slot) = self.slots.get_mut(at) else {
            return BackupVerdict::NotBackup;
        };
        if slot.aux.contains(&fp) {
            self.dropped_identical.increment(1);
            return BackupVerdict::DropIdentical;
        }
        let i = usize::from(slot.aux_next) % AUX_FINGERPRINT_RING;
        slot.aux[i] = fp;
        slot.aux_next = ((i + 1) % AUX_FINGERPRINT_RING) as u8;
        BackupVerdict::Accept
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
        assert_eq!(d.admit_aux(42, SEG, &pkt), BackupVerdict::Accept);
        assert_eq!(d.admit_aux(42, SEG, &pkt), BackupVerdict::DropIdentical);
        assert_eq!(d.admit_aux(43, SEG, &pkt), BackupVerdict::NotBackup);
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

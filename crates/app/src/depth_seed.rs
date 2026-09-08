//! Seeds the depth sockets at boot from what they held at the previous
//! session's close, so the first minutes of a session do not carry the index
//! at-the-money contracts the 2026-09-06 lock bans.
//!
//! # The gap this closes (2026-09-08)
//!
//! Depth-20 and depth-200 are steered by the VOLUME ranking once the frame
//! drain has published one — and the drain cannot rank before the open,
//! because pre-open has no volume. So from the dial (~08:30) until the first
//! 5-second sweep after 09:15, every depth socket carried whatever the boot
//! dial chose: NIFTY/BANKNIFTY at-the-money pairs on depth-200, the 2026-08-26
//! layout on depth-20. The scope lock records that plainly as an open item:
//! "the lock is met from ~09:16, not 09:00".
//!
//! Yesterday's close is the best available guess for today's open. The busiest
//! stock-option contracts at 15:39 are, far more often than not, the busiest at
//! 09:15 the next morning — and where they are not, the ranking corrects the
//! set within one window of the open, exactly as it does today. What the seed
//! removes is the guaranteed-wrong class: five deep sockets on index options
//! for the first fifteen minutes of every session.
//!
//! # Why the seed is VALIDATED, never trusted
//!
//! A contract id from yesterday can be dead today: an expiry rolled overnight,
//! or the master re-issued ids (Dhan documents derivative ids as unstable
//! across days). Subscribing a dead id sends a well-formed request for nothing
//! and receives silence that looks like a quiet book. So every seed row is
//! checked against TODAY's contract artifact before it is dialed, and a row
//! that is not a live `OPTSTK` `CE`/`PE` with expiry `>= today` is REFUSED and
//! counted. The refusal reasons are named so a morning where the whole seed
//! is refused reads as "expiry rolled" rather than as silence.
//!
//! # What is written, and when
//!
//! The steering loop writes the file ONCE per session, at the capture-window
//! close (15:40 IST), from what the sockets actually HOLD — never from the
//! published ranking. Holdings are the steered state after the hysteresis
//! band; the last published ranking is one five-second window and can be a
//! thin one. It is written only if a ranking was published that session: a
//! session whose sockets never left the boot dial would otherwise seed the
//! next morning with the very index options this module exists to remove.
//! (The read-side validation would refuse them anyway — index options are not
//! `OPTSTK` — but a file that is never written cannot be mis-read.)
//!
//! # Holding until the first ranking
//!
//! Seeding the dial is half the fix. The other half is that the pre-ranking
//! engines — the depth-200 at-the-money tracker and the depth-20 layout — must
//! NOT re-centre the seeded sockets back onto index contracts during the
//! minutes before the first ranking exists. So a pool that was seeded holds
//! still until the ranking arrives; [`boot_seeded`] is the flag the steering
//! loop reads. A pool that was NOT seeded (no file, or every row refused)
//! keeps today's behaviour exactly.
//!
//! # Complexity
//!
//! Cold path, twice a day. `apply_depth_seed` is O(rows) to index today's
//! artifact (~100,000 rows, once per attach attempt) plus O(seed) lookups.
//! The write is O(sockets). Nothing here is on any per-tick path.
//!
//! # What this does NOT do (honest limits)
//!
//! * It cannot seed a socket the dial does not open. If the artifact is
//!   unreadable the dial has nothing to validate against and the seed is
//!   skipped — the loud fallback is unchanged.
//! * It is a GUESS about the open, corrected by the ranking. It is not the
//!   ranking, and the first sweep after 09:15 still decides.
//! * A seed older than one trading day is applied if its contracts are still
//!   live today (a Monday reads Friday's file). That is deliberate: the
//!   validation is against today's contract set, not against the file's age.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

use tickvault_common::types::ExchangeSegment;
use tickvault_core::websocket::pool_supervisor::SubscribeInstrument;

use crate::dhan_contract_universe::ContractRow;
use crate::dhan_depth_universe::{
    DEPTH_20_MAX_INSTRUMENTS, DEPTH_200_TOTAL_SOCKETS, DepthSelection,
};

/// Directory the seed lives in — the same one every other daily artifact uses,
/// so a disk-full or permissions problem shows up in one place.
const SEED_DIR: &str = "data/instrument-cache";

/// The seed file. ONE file, not date-stamped: the reader wants "the most
/// recent close", and a Monday must find Friday's without guessing at
/// holidays. The date is carried INSIDE the file for the log line.
const SEED_FILE: &str = "depth-seed-latest.json";

/// The wire byte of the only segment a stock option can carry.
const STOCK_OPTION_SEGMENT: ExchangeSegment = ExchangeSegment::NseFno;

/// Counter: seed rows by `pool` and `outcome`.
///
/// Outcomes: `applied` (dialed from the seed), `refused_unknown` (not in
/// today's artifact), `refused_not_stock_option` (in the artifact but not an
/// `OPTSTK` CE/PE — an index option or a future), `refused_expired`,
/// `refused_segment` (a seed row not on `NSE_FNO`). NOT EMF-selected: local
/// `/metrics` only, for the same budget reason every other 2026-09 counter
/// gives.
pub const SEED_COUNTER: &str = "tv_depth_seed_rows_total";

/// Every label pair the counter can carry, for pre-registration at zero.
pub const SEED_OUTCOMES: [&str; 5] = [
    "applied",
    "refused_unknown",
    "refused_not_stock_option",
    "refused_expired",
    "refused_segment",
];

/// The two pools a seed can feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeedPool {
    /// The five single-instrument sockets.
    Depth200,
    /// The five 50-instrument sockets.
    Depth20,
}

impl SeedPool {
    /// Stable label for logs and the counter.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Depth200 => "depth200",
            Self::Depth20 => "depth20",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Depth200 => 0,
            Self::Depth20 => 1,
        }
    }
}

/// Path of the seed file.
#[must_use]
pub fn seed_path() -> std::path::PathBuf {
    std::path::Path::new(SEED_DIR).join(SEED_FILE)
}

/// One contract as persisted. Short field names for the same reason
/// [`ContractRow`] uses them; there are at most 255 of these, so it is
/// convention rather than size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SeedContract {
    /// The CONTRACT's own `security_id`.
    pub i: u64,
    /// The segment's wire byte — half of the I-P1-11 identity.
    pub g: u8,
}

/// What the depth pools held at the previous session's close.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct DepthSeed {
    /// The IST date the holdings were captured on, `YYYY-MM-DD`.
    pub date_ist: String,
    /// The depth-200 sockets' contracts, in socket order.
    pub depth_200: Vec<SeedContract>,
    /// Every contract the depth-20 sockets held, flattened.
    pub depth_20: Vec<SeedContract>,
}

impl DepthSeed {
    /// Builds a seed from what the sockets hold right now.
    pub fn from_holdings<A, B>(date_ist: &str, depth_200: A, depth_20: B) -> Self
    where
        A: IntoIterator<Item = SubscribeInstrument>,
        B: IntoIterator<Item = SubscribeInstrument>,
    {
        let to_seed = |i: SubscribeInstrument| SeedContract {
            i: i.security_id,
            g: i.segment.binary_code(),
        };
        Self {
            date_ist: date_ist.to_owned(),
            depth_200: depth_200.into_iter().map(to_seed).collect(),
            depth_20: depth_20.into_iter().map(to_seed).collect(),
        }
    }

    /// Whether there is anything worth writing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.depth_200.is_empty() && self.depth_20.is_empty()
    }
}

/// Writes the seed atomically (temp file, then rename), creating the
/// directory if needed.
pub fn write_depth_seed(path: &std::path::Path, seed: &DepthSeed) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(seed).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

/// Reads the seed, or `None` when there is none or it cannot be parsed.
///
/// Fail-soft on purpose: a missing or corrupt seed means "dial as before",
/// which is the behaviour every session had until this module existed. The
/// parse failure is logged once at warn so a corrupt file is not silent.
#[must_use]
pub fn read_depth_seed(path: &std::path::Path) -> Option<DepthSeed> {
    let body = std::fs::read(path).ok()?;
    match serde_json::from_slice::<DepthSeed>(&body) {
        Ok(seed) => Some(seed),
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                %err,
                "depth seed file exists but could not be parsed — dialing without it"
            );
            None
        }
    }
}

/// The per-pool tally of one application.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SeedTally {
    /// Rows dialed from the seed.
    pub applied: usize,
    /// Rows absent from today's artifact.
    pub refused_unknown: usize,
    /// Rows present but not a stock-option leg.
    pub refused_not_stock_option: usize,
    /// Rows whose contract expired before today.
    pub refused_expired: usize,
    /// Rows not on the stock-option segment.
    pub refused_segment: usize,
}

impl SeedTally {
    fn refused(&self) -> usize {
        self.refused_unknown
            .saturating_add(self.refused_not_stock_option)
            .saturating_add(self.refused_expired)
            .saturating_add(self.refused_segment)
    }
}

/// What applying a seed did to a selection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeedApplied {
    /// The date the seed was captured on.
    pub seed_date_ist: String,
    /// The depth-200 tally.
    pub depth_200: SeedTally,
    /// The depth-20 tally.
    pub depth_20: SeedTally,
}

impl SeedApplied {
    /// Whether any pool took at least one seeded contract.
    #[must_use]
    pub fn any_applied(&self) -> bool {
        self.depth_200.applied > 0 || self.depth_20.applied > 0
    }
}

/// Why one seed row was refused, or `Ok` with the live instrument.
enum Verdict {
    Live(SubscribeInstrument),
    Unknown,
    NotStockOption,
    Expired,
    Segment,
}

/// Today's artifact, indexed for O(1) seed validation.
struct TodayIndex {
    /// `security_id` → (is a stock-option CE/PE leg, expiry `YYYYMMDD`).
    rows: HashMap<u64, (bool, u32)>,
}

impl TodayIndex {
    fn build(rows: &[ContractRow]) -> Self {
        let mut index = HashMap::with_capacity(rows.len());
        for r in rows {
            let is_stock_option_leg = r.c == "OPTSTK" && (r.l == "CE" || r.l == "PE");
            // First write wins: the artifact should carry one row per id, and
            // if it did not, the FIRST row is the one every other reader sees.
            index.entry(r.i).or_insert((is_stock_option_leg, r.e));
        }
        Self { rows: index }
    }

    fn judge(&self, seed: SeedContract, today_ymd: u32) -> Verdict {
        if seed.g != STOCK_OPTION_SEGMENT.binary_code() {
            return Verdict::Segment;
        }
        let Some(&(is_leg, expiry)) = self.rows.get(&seed.i) else {
            return Verdict::Unknown;
        };
        if !is_leg {
            return Verdict::NotStockOption;
        }
        if expiry < today_ymd {
            return Verdict::Expired;
        }
        Verdict::Live(SubscribeInstrument {
            security_id: seed.i,
            segment: STOCK_OPTION_SEGMENT,
        })
    }
}

/// Validates one pool's seed rows and returns the survivors, deduplicated on
/// the composite key, in seed order.
fn survivors(
    index: &TodayIndex,
    rows: &[SeedContract],
    today_ymd: u32,
    tally: &mut SeedTally,
) -> Vec<SubscribeInstrument> {
    let mut seen: HashSet<(u64, u8)> = HashSet::with_capacity(rows.len());
    let mut out = Vec::with_capacity(rows.len());
    for &row in rows {
        match index.judge(row, today_ymd) {
            Verdict::Live(instrument) => {
                if seen.insert((instrument.security_id, instrument.segment.binary_code())) {
                    out.push(instrument);
                }
            }
            Verdict::Unknown => tally.refused_unknown = tally.refused_unknown.saturating_add(1),
            Verdict::NotStockOption => {
                tally.refused_not_stock_option = tally.refused_not_stock_option.saturating_add(1);
            }
            Verdict::Expired => tally.refused_expired = tally.refused_expired.saturating_add(1),
            Verdict::Segment => tally.refused_segment = tally.refused_segment.saturating_add(1),
        }
    }
    out
}

/// Seeded contracts first, then the dial's own choices that are not already
/// among them, truncated to `cap`. The seed WINS the leading slots because the
/// dial's choices are the index contracts the seed exists to displace.
fn seed_first(
    seeded: Vec<SubscribeInstrument>,
    dial: &[SubscribeInstrument],
    cap: usize,
) -> Vec<SubscribeInstrument> {
    let mut keys: HashSet<(u64, u8)> = seeded
        .iter()
        .map(|i| (i.security_id, i.segment.binary_code()))
        .collect();
    let mut out = seeded;
    out.truncate(cap);
    for &i in dial {
        if out.len() >= cap {
            break;
        }
        if keys.insert((i.security_id, i.segment.binary_code())) {
            out.push(i);
        }
    }
    out
}

/// Applies a seed to the dial's selection, validating every row against
/// today's contract artifact.
///
/// Pure apart from the counters: takes the selection and returns the tally,
/// so every refusal shape is a unit test. Marks the pools that took at least
/// one seeded contract as [`boot_seeded`].
pub fn apply_depth_seed(
    selection: &mut DepthSelection,
    seed: &DepthSeed,
    today_rows: &[ContractRow],
    today_ymd: u32,
) -> SeedApplied {
    let index = TodayIndex::build(today_rows);
    let mut applied = SeedApplied {
        seed_date_ist: seed.date_ist.clone(),
        ..SeedApplied::default()
    };

    let live_200 = survivors(&index, &seed.depth_200, today_ymd, &mut applied.depth_200);
    applied.depth_200.applied = live_200.len().min(DEPTH_200_TOTAL_SOCKETS);
    if !live_200.is_empty() {
        selection.depth_200 = seed_first(live_200, &selection.depth_200, DEPTH_200_TOTAL_SOCKETS);
        // The pair concept does not describe a seeded pool: the seed is
        // whatever was busiest, one contract per socket.
        selection.depth_200_lone_leg = false;
        mark_boot_seeded(SeedPool::Depth200);
    }

    let live_20 = survivors(&index, &seed.depth_20, today_ymd, &mut applied.depth_20);
    applied.depth_20.applied = live_20.len().min(DEPTH_20_MAX_INSTRUMENTS);
    if !live_20.is_empty() {
        selection.depth_20 = seed_first(live_20, &selection.depth_20, DEPTH_20_MAX_INSTRUMENTS);
        mark_boot_seeded(SeedPool::Depth20);
    }

    record_seed_tally(SeedPool::Depth200, applied.depth_200);
    record_seed_tally(SeedPool::Depth20, applied.depth_20);
    applied
}

fn record_seed_tally(pool: SeedPool, tally: SeedTally) {
    let pairs = [
        ("applied", tally.applied),
        ("refused_unknown", tally.refused_unknown),
        ("refused_not_stock_option", tally.refused_not_stock_option),
        ("refused_expired", tally.refused_expired),
        ("refused_segment", tally.refused_segment),
    ];
    for (outcome, n) in pairs {
        if n > 0 {
            metrics::counter!(SEED_COUNTER, "pool" => pool.as_str(), "outcome" => outcome)
                .increment(n as u64);
        }
    }
    if tally.applied > 0 || tally.refused() > 0 {
        tracing::info!(
            pool = pool.as_str(),
            applied = tally.applied,
            refused_unknown = tally.refused_unknown,
            refused_not_stock_option = tally.refused_not_stock_option,
            refused_expired = tally.refused_expired,
            refused_segment = tally.refused_segment,
            "depth seed: yesterday's holdings validated against today's contracts — the \
             applied rows dial first, the refused ones fall back to the dial's own choice"
        );
    }
}

/// Registers every counter series at zero so the first seed is a delta the
/// scrape can see, not a dropped first sample.
pub fn pre_register_seed_counters() {
    for pool in [SeedPool::Depth200, SeedPool::Depth20] {
        for outcome in SEED_OUTCOMES {
            metrics::counter!(SEED_COUNTER, "pool" => pool.as_str(), "outcome" => outcome)
                .increment(0);
        }
    }
}

/// Which pools were dialed from a seed this boot. Read by the steering loop
/// to hold a seeded pool still until the first ranking.
static BOOT_SEEDED: [AtomicBool; 2] = [AtomicBool::new(false), AtomicBool::new(false)];

fn mark_boot_seeded(pool: SeedPool) {
    BOOT_SEEDED[pool.index()].store(true, Ordering::Release);
}

/// Whether `pool` was dialed from the previous session's seed this boot.
#[must_use]
pub fn boot_seeded(pool: SeedPool) -> bool {
    BOOT_SEEDED[pool.index()].load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Test-only reset of the process-global flags.
    fn reset_boot_seeded_for_tests() {
        for flag in &BOOT_SEEDED {
            flag.store(false, Ordering::Release);
        }
    }

    // The boot-seeded flags are process-global; tests that set or read them
    // run one at a time so a parallel sibling cannot leave a flag set.
    static BOOT_FLAG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    const FNO: u8 = 2;

    fn row(i: u64, c: &str, l: &str, e: u32) -> ContractRow {
        ContractRow {
            i,
            x: "NSE".to_owned(),
            c: c.to_owned(),
            e,
            s: 100_000,
            l: l.to_owned(),
            u: "RELIANCE".to_owned(),
            z: 250,
        }
    }

    fn inst(id: u64) -> SubscribeInstrument {
        SubscribeInstrument {
            security_id: id,
            segment: ExchangeSegment::NseFno,
        }
    }

    fn today() -> Vec<ContractRow> {
        vec![
            row(101, "OPTSTK", "CE", 20_260_925),
            row(102, "OPTSTK", "PE", 20_260_925),
            row(103, "OPTSTK", "CE", 20_260_925),
            row(201, "OPTIDX", "CE", 20_260_925), // an index option
            row(301, "FUTSTK", "", 20_260_925),   // a future
            row(401, "OPTSTK", "CE", 20_260_901), // expired before today
        ]
    }

    #[test]
    fn apply_depth_seed_gives_live_stock_option_seeds_the_leading_slots_and_the_dial_fills_the_rest()
     {
        let _serialised = BOOT_FLAG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_boot_seeded_for_tests();
        let seed = DepthSeed {
            date_ist: "2026-09-07".into(),
            depth_200: vec![
                SeedContract { i: 101, g: FNO },
                SeedContract { i: 102, g: FNO },
            ],
            depth_20: vec![SeedContract { i: 103, g: FNO }],
        };
        let mut sel = DepthSelection {
            depth_200: vec![inst(9001), inst(9002), inst(9003), inst(9004), inst(9005)],
            depth_20: vec![inst(8001), inst(8002)],
            depth_200_lone_leg: true,
            ..DepthSelection::default()
        };
        let applied = apply_depth_seed(&mut sel, &seed, &today(), 20_260_908);
        assert_eq!(applied.depth_200.applied, 2);
        assert_eq!(applied.depth_20.applied, 1);
        let ids: Vec<u64> = sel.depth_200.iter().map(|i| i.security_id).collect();
        assert_eq!(
            ids,
            vec![101, 102, 9001, 9002, 9003],
            "seed first, dial fills, cap 5"
        );
        let ids20: Vec<u64> = sel.depth_20.iter().map(|i| i.security_id).collect();
        assert_eq!(ids20, vec![103, 8001, 8002]);
        assert!(!sel.depth_200_lone_leg);
        assert!(boot_seeded(SeedPool::Depth200));
        assert!(boot_seeded(SeedPool::Depth20));
    }

    #[test]
    fn every_refusal_reason_is_named_and_a_fully_refused_seed_leaves_the_dial_untouched_and_any_applied_false()
     {
        let _serialised = BOOT_FLAG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_boot_seeded_for_tests();
        let seed = DepthSeed {
            date_ist: "2026-09-07".into(),
            depth_200: vec![
                SeedContract { i: 201, g: FNO }, // index option
                SeedContract { i: 301, g: FNO }, // future
                SeedContract { i: 401, g: FNO }, // expired
                SeedContract { i: 999, g: FNO }, // unknown
                SeedContract { i: 101, g: 0 },   // wrong segment
            ],
            depth_20: vec![],
        };
        let dial = vec![inst(9001)];
        let mut sel = DepthSelection {
            depth_200: dial.clone(),
            ..DepthSelection::default()
        };
        let applied = apply_depth_seed(&mut sel, &seed, &today(), 20_260_908);
        assert_eq!(applied.depth_200.applied, 0);
        assert_eq!(applied.depth_200.refused_not_stock_option, 2);
        assert_eq!(applied.depth_200.refused_expired, 1);
        assert_eq!(applied.depth_200.refused_unknown, 1);
        assert_eq!(applied.depth_200.refused_segment, 1);
        assert_eq!(sel.depth_200, dial, "nothing survived, so the dial stands");
        assert!(!boot_seeded(SeedPool::Depth200));
        assert!(!applied.any_applied());
    }

    #[test]
    fn a_seed_row_already_in_the_dial_is_not_dialed_twice() {
        let _serialised = BOOT_FLAG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_boot_seeded_for_tests();
        let seed = DepthSeed {
            date_ist: "2026-09-07".into(),
            depth_200: vec![
                SeedContract { i: 101, g: FNO },
                SeedContract { i: 101, g: FNO },
            ],
            depth_20: vec![],
        };
        let mut sel = DepthSelection {
            depth_200: vec![inst(101), inst(9002)],
            ..DepthSelection::default()
        };
        apply_depth_seed(&mut sel, &seed, &today(), 20_260_908);
        let ids: Vec<u64> = sel.depth_200.iter().map(|i| i.security_id).collect();
        assert_eq!(ids, vec![101, 9002], "deduplicated on the composite key");
    }

    #[test]
    fn the_seed_is_capped_at_the_pool_size_and_boot_seeded_reports_it() {
        let _serialised = BOOT_FLAG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_boot_seeded_for_tests();
        let rows: Vec<ContractRow> = (1..=300u64)
            .map(|i| row(i, "OPTSTK", "CE", 20_260_925))
            .collect();
        let seed = DepthSeed {
            date_ist: "2026-09-07".into(),
            depth_200: (1..=7).map(|i| SeedContract { i, g: FNO }).collect(),
            depth_20: (1..=300).map(|i| SeedContract { i, g: FNO }).collect(),
        };
        let mut sel = DepthSelection::default();
        let applied = apply_depth_seed(&mut sel, &seed, &rows, 20_260_908);
        assert_eq!(sel.depth_200.len(), DEPTH_200_TOTAL_SOCKETS);
        assert_eq!(sel.depth_20.len(), DEPTH_20_MAX_INSTRUMENTS);
        assert_eq!(applied.depth_200.applied, DEPTH_200_TOTAL_SOCKETS);
        assert_eq!(applied.depth_20.applied, DEPTH_20_MAX_INSTRUMENTS);
    }

    #[test]
    fn write_depth_seed_then_read_depth_seed_roundtrips_and_a_missing_or_corrupt_file_reads_as_none()
     {
        let dir = std::env::temp_dir().join(format!("tv-depth-seed-{}", std::process::id()));
        let path = dir.join("seed.json");
        assert!(read_depth_seed(&path).is_none(), "no file yet");
        let seed = DepthSeed::from_holdings("2026-09-08", [inst(101), inst(102)], [inst(103)]);
        write_depth_seed(&path, &seed).expect("write");
        assert_eq!(read_depth_seed(&path), Some(seed));
        std::fs::write(&path, b"{ not json").expect("corrupt");
        assert!(
            read_depth_seed(&path).is_none(),
            "corrupt reads as absent, never panics"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_holdings_carries_the_composite_key_and_is_empty_only_when_both_pools_are() {
        let seed = DepthSeed::from_holdings("2026-09-08", [inst(101)], []);
        assert_eq!(seed.depth_200, vec![SeedContract { i: 101, g: FNO }]);
        assert!(!seed.is_empty());
        assert!(DepthSeed::from_holdings("2026-09-08", [], []).is_empty());
    }

    #[test]
    fn seed_path_sits_beside_the_other_daily_artifacts() {
        assert_eq!(
            seed_path().parent(),
            crate::dhan_universe::mapping_artifact_path("2026-09-08").parent()
        );
        assert!(seed_path().to_string_lossy().ends_with(SEED_FILE));
    }

    #[test]
    fn pre_register_seed_counters_covers_every_label() {
        pre_register_seed_counters();
        assert_eq!(SEED_OUTCOMES.len(), 5);
    }

    #[test]
    fn read_depth_seed_of_an_absent_or_empty_path_is_none() {
        assert!(
            read_depth_seed(std::path::Path::new("/nonexistent/tv-depth-seed/x.json")).is_none()
        );
        let dir = std::env::temp_dir().join(format!("tv-depth-seed-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("seed.json");
        std::fs::write(&path, b"").expect("empty file");
        assert!(
            read_depth_seed(&path).is_none(),
            "an empty file is absent, never a panic"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn any_applied_is_false_until_a_pool_takes_a_seed() {
        let mut applied = SeedApplied::default();
        assert!(!applied.any_applied());
        applied.depth_20.applied = 1;
        assert!(applied.any_applied());
        let mut only_200 = SeedApplied::default();
        only_200.depth_200.applied = 1;
        assert!(only_200.any_applied());
    }

    #[test]
    fn boot_seeded_reads_each_pools_own_flag() {
        let _serialised = BOOT_FLAG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_boot_seeded_for_tests();
        assert!(!boot_seeded(SeedPool::Depth200));
        assert!(!boot_seeded(SeedPool::Depth20));
        mark_boot_seeded(SeedPool::Depth20);
        assert!(boot_seeded(SeedPool::Depth20));
        assert!(
            !boot_seeded(SeedPool::Depth200),
            "the other pool's flag is untouched"
        );
        reset_boot_seeded_for_tests();
        assert!(!boot_seeded(SeedPool::Depth20));
    }
}

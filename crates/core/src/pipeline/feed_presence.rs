//! Per-instrument cross-feed presence registry — scoreboard PR-D
//! (operator directive 2026-07-10: *"run these two websockets live for a
//! month... all tracked, captured, visualized, logged, monitored, 100%
//! automated"*).
//!
//! Records, per feed and per instrument, WHICH of the 375 NSE session
//! minutes ([09:15, 15:30) IST) delivered at least one tick — so the 15:45
//! IST scoreboard can flip the daily `unique_win_minutes` / `both_minutes`
//! columns from the SQL minute-set approximation to REAL per-instrument
//! tick presence, and populate the `feed_coverage_daily` per-instrument
//! detail table (the month-end "worst offenders" drill).
//!
//! # Architecture (mirror of `feed_lag_monitor` — cold build, hot fold)
//!
//! **Cold-path slot map** (built at the daily-universe build for Dhan and
//! the Groww watch-set build): each registered instrument gets a SLOT in a
//! CANONICAL cross-feed instrument space. Cross-feed pairing keys
//! ([`PairingKey`]):
//!
//! - **Stocks:** ISIN (`CsvRow.isin` ↔ `WatchEntry.isin` — the §31.1
//!   PRIMARY join key, the verified `by_isin` precedent).
//! - **Indices:** the canonical index name (`canonicalize_index_symbol` —
//!   the FUTIDX-02 alias-drift lesson).
//! - **Index futures:** `(canonical underlying, expiry YYYY-MM-DD)` — the
//!   `record_index_future_selection` cross-feed pairing precedent (a Groww
//!   future's exchange_token is a DIFFERENT id space from the Dhan FUTIDX
//!   SID, so ids can never pair contracts; the contract identity does).
//!
//! An instrument with no pairing key (or whose partner never registers) is
//! an UNMAPPED singleton: it still gets a feed-local slot, still records
//! presence, and is REPORTED at drain time (named in logs, bounded sample)
//! — never silently dropped (Rule 11).
//!
//! **Hot fold** ([`record_presence`], co-located with the DHAT-proven
//! `record_dhan_tick` / `record_groww_tick` call sites): one lock-free
//! papaya read + one relaxed `fetch_or` into the slot's 375-bit minute
//! bitset (`[AtomicU64; 6]` per slot per feed). Minute index is pure
//! integer math over the exchange timestamp the sites ALREADY hold — zero
//! new clock reads, zero allocation, O(1). Gated by a boot-read
//! `AtomicBool` (`[scoreboard] presence_fold_enabled` — no per-tick config
//! read). Ticks for instruments not in the slot map (pre-registration boot
//! ticks, out-of-universe SIDs) bump a per-feed relaxed counter — visible
//! at drain, never silent.
//!
//! **Memory (fixed, honest):** [`MAX_PRESENCE_SLOTS`] = 2,048 slots ×
//! 2 feeds × 48 B of minute bits ≈ **192 KiB** fixed at first use (the
//! design sketch said ~148 KB for exact-size allocation; the fixed cap
//! buys lock-free hot indexing for +44 KiB — trivial on the r8g.large
//! 16 GiB host). Registrations beyond the cap are counted
//! ([`PresenceRegisterSummary::overflow_dropped`]) and surface on the
//! drain — fail-loud, never silent.
//!
//! **Day scoping + reset:** each slot stamps, per feed, the IST day it was
//! last registered for. The drain ([`drain_day`]) reads ONLY slots
//! registered for the target day, so stale slots from a previous session
//! never leak. Registration for a NEW day clears that slot's bits for that
//! feed (correct even if the IST-midnight reset was missed), and BOTH
//! existing midnight force-seal tasks additionally call [`reset_daily`]
//! (belt and braces, mirroring `reset_day_lag_histogram`).
//!
//! # Honest envelope (NOT claimed)
//!
//! - The registry is PROCESS-LOCAL: a mid-day restart loses the
//!   pre-restart presence bits. [`PresenceDrain::covers_full_session`] is
//!   `false` when the process started after the target day's 09:15 IST
//!   session open — the scoreboard stamps `coverage_source='mixed'` and
//!   the day partial, never fabricates (the established
//!   restart-day-partial-floor pattern).
//! - **Late first registration degrades the stamp too** (PR-D review
//!   round 1, 2026-07-11): on a midnight-crossing process (dev/manual
//!   runs; prod restarts daily) the maps keep routing D+1 session ticks
//!   into slots still stamped day D. When the D+1 registration finally
//!   lands (the §4 infinite CSV-retry ladder makes a post-09:15 landing
//!   a designed possibility), the day-change clear zeroes those
//!   genuinely-observed same-day pre-registration bits. The drain
//!   therefore folds each feed's FIRST registration instant for the
//!   target day into `covers_full_session` — a first registration after
//!   session open degrades the stamp to `mixed` instead of over-claiming
//!   full-session truth over a window it destroyed. Residual (bounded to
//!   the same dev/manual-run envelope as the FUTIDX per-boot split): the
//!   destroyed pre-registration minutes are not recovered — the SQL
//!   minute sets remain the full-day approximation for such a day.
//! - "Presence" = a tick WE received at our capture boundary — not proof
//!   the exchange traded that minute (both vendors sample; the
//!   tick-conservation §F wording applies).
//! - The drain is **O(slots × 12 words) ≈ 25K word loads — NOT O(1)**;
//!   cold path, once per 15:45 run (microseconds). Registration is
//!   **O(universe)** under a Mutex — cold path, once per feed per day.
//!   Only [`record_presence`] is hot, and only IT is O(1).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use tickvault_common::feed::Feed;

/// NSE regular session minutes: [09:15, 15:30) IST.
pub const PRESENCE_SESSION_MINUTES: usize = 375;

/// 375 bits round up to six 64-bit words per slot per feed.
pub const PRESENCE_WORDS_PER_SLOT: usize = 6;

/// Fixed slot capacity. Sized for the worst realistic day: the Dhan
/// universe envelope tops at 1,200 SIDs and the Groww watch set at ~1,000,
/// with the ~740 paired stocks SHARING slots — so distinct slots peak
/// around ~1,450. 2,048 leaves headroom; overflow is counted, never
/// silent. Memory: 2,048 × 2 feeds × 48 B = 192 KiB fixed.
pub const MAX_PRESENCE_SLOTS: usize = 2_048;

/// Session open, seconds-of-day IST (09:15:00) — the minute-0 anchor.
pub const PRESENCE_SESSION_START_SECS_OF_DAY_IST: u32 = 9 * 3600 + 15 * 60;

const SECS_PER_DAY: u32 = 86_400;
const NANOS_PER_SEC: i64 = 1_000_000_000;

/// Canonical cross-feed instrument identity — the slot-sharing key. The
/// CALLER canonicalizes (ISIN as-is; index names through
/// `canonicalize_index_symbol`; future underlyings likewise) so this
/// module stays pure data.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PairingKey {
    /// Stocks — the 12-char ISIN (§31.1 primary join key).
    Isin(String),
    /// Indices — the canonical index name (`NIFTY`, `BANKNIFTY`, ...).
    Index(String),
    /// Index futures — contract identity, never native ids.
    Future {
        /// Canonical underlying (`NIFTY`/`BANKNIFTY`/`MIDCPNIFTY`/`SENSEX`).
        underlying: String,
        /// ISO `YYYY-MM-DD` expiry.
        expiry: String,
    },
}

/// One instrument registration (cold path, once per feed per day).
#[derive(Clone, Debug)]
pub struct PresenceRegistration {
    /// The id ticks carry for this feed (Dhan SID / Groww native id).
    pub security_id: u64,
    /// The segment code ticks carry (`ExchangeSegment::binary_code`).
    pub segment_code: u8,
    /// The segment wire label (`"IDX_I"` / `"NSE_EQ"` / ...).
    pub segment_label: &'static str,
    /// Human symbol for the coverage drill-down rows.
    pub symbol: String,
    /// Cross-feed pairing key; `None` = feed-local singleton (unmapped).
    pub pairing: Option<PairingKey>,
}

/// What one [`register_instruments`] call did (logged by the caller).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PresenceRegisterSummary {
    /// Instruments registered (slot allocated or joined).
    pub registered: usize,
    /// Slots where BOTH feeds are now registered for this day.
    pub paired: usize,
    /// Registrations dropped because the slot table hit
    /// [`MAX_PRESENCE_SLOTS`] — fail-loud, the caller logs at `error!`.
    pub overflow_dropped: usize,
}

/// One slot's drained day coverage (a `feed_coverage_daily` row source).
#[derive(Clone, Debug, PartialEq)]
pub struct SlotCoverage {
    /// Canonical id: the Dhan SID for mapped pairs (Dhan registration wins
    /// the canonical fields), the native id for singletons.
    pub canonical_security_id: i64,
    pub segment_label: &'static str,
    pub symbol: String,
    /// `true` = both feeds registered this slot for the target day.
    pub mapped: bool,
    pub dhan_registered: bool,
    pub groww_registered: bool,
    /// Session minutes with ≥1 tick, per feed (0 when that feed did not
    /// register the slot for the day).
    pub dhan_minutes: i64,
    pub groww_minutes: i64,
    /// Cross-feed comparison columns — REAL only for mapped pairs; `-1`
    /// sentinels on singletons (exclusive-vs-nothing is not a measurement
    /// — the feed-off-day lesson, round 5 2026-07-10).
    pub dhan_only_minutes: i64,
    pub groww_only_minutes: i64,
    pub both_minutes: i64,
}

/// Per-feed aggregate of one day's drained presence.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FeedPresenceTotals {
    /// Slots this feed registered for the day.
    pub registered_instruments: i64,
    /// Slots where BOTH feeds registered (same for both feeds).
    pub mapped_instruments: i64,
    /// Slots ONLY this feed registered.
    pub unmapped_instruments: i64,
    /// Σ per-slot session minutes with ≥1 tick from this feed.
    pub covered_instrument_minutes: i64,
    /// Feed-level union minutes (≥1 tick from ANY instrument).
    pub streaming_minutes: i64,
    /// Feed-level union minutes where ONLY this feed delivered.
    pub unique_win_minutes: i64,
    /// Feed-level union minutes where BOTH feeds delivered.
    pub both_minutes: i64,
    /// Hot-path folds for (sid, segment) keys NOT in this feed's slot map
    /// (pre-registration boot ticks / out-of-universe SIDs) — visible,
    /// never silent (Rule 11).
    pub unregistered_folds: i64,
}

/// One day's drained presence (the 15:45 scoreboard input).
#[derive(Clone, Debug, PartialEq)]
pub struct PresenceDrain {
    /// Per-slot coverage for every slot registered for the target day.
    pub slots: Vec<SlotCoverage>,
    pub dhan: FeedPresenceTotals,
    pub groww: FeedPresenceTotals,
    /// `false` = this process started AFTER the target day's session open
    /// — the pre-restart window is invisible to the registry (the
    /// scoreboard stamps `coverage_source='mixed'` + partial).
    pub covers_full_session: bool,
    /// Registrations dropped at the slot cap (fail-loud).
    pub overflow_dropped: i64,
}

/// Session-minute index for an IST exchange timestamp: `Some(0..375)`
/// inside [09:15, 15:30) IST, `None` outside. Pure integer math — the ONE
/// gate that keeps pre-open / post-close / Muhurat ticks out of the
/// session bitsets (the daily row's denominator is the regular 375).
#[inline]
#[must_use]
pub fn presence_minute_index(exchange_ts_ist_secs: u32) -> Option<usize> {
    let secs_of_day = exchange_ts_ist_secs % SECS_PER_DAY;
    let rel = secs_of_day.checked_sub(PRESENCE_SESSION_START_SECS_OF_DAY_IST)?;
    let minute = (rel / 60) as usize;
    (minute < PRESENCE_SESSION_MINUTES).then_some(minute)
}

/// Slot-table key: paired slots share a canonical identity; unmapped
/// instruments key on `(feed, sid, segment)` so re-registration is
/// idempotent per feed.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum SlotKey {
    Paired(PairingKey),
    Local(u8, u64, u8),
}

/// Cold slot metadata (under the registration Mutex — never hot).
#[derive(Debug)]
struct SlotMeta {
    canonical_security_id: i64,
    segment_label: &'static str,
    symbol: String,
    /// `true` once Dhan stamped the canonical fields (Dhan wins pairs).
    canonical_from_dhan: bool,
    /// IST day number each feed last registered this slot for.
    registered_day: [Option<u64>; Feed::COUNT],
}

#[derive(Default)]
struct SlotTable {
    by_key: HashMap<SlotKey, u32>,
    meta: Vec<SlotMeta>,
    /// Per feed: `(ist_day, instant)` of the FIRST registration call for
    /// that day — drained into `covers_full_session` so a late first
    /// registration (whose day-change clear destroyed the same-day
    /// pre-registration bits) degrades the stamp to `mixed` (PR-D review
    /// round 1, 2026-07-11).
    first_registration: [Option<(u64, i64)>; Feed::COUNT],
}

/// The registry. One process-global instance behind [`init_feed_presence`].
pub struct FeedPresenceRegistry {
    /// Boot-read fold gate (`[scoreboard] presence_fold_enabled`).
    enabled: AtomicBool,
    /// Process-start instant (IST nanos) — the full-session honesty stamp.
    process_start_ist_nanos: i64,
    /// Hot lookup: per-feed `(security_id, segment_code)` → slot index.
    maps: [papaya::HashMap<(u64, u8), u32>; Feed::COUNT],
    /// Presence bits: per feed, `MAX_PRESENCE_SLOTS × 6` words, indexed
    /// `slot * 6 + minute/64`. Fixed allocation at construction.
    bits: [Box<[AtomicU64]>; Feed::COUNT],
    /// Cold slot metadata.
    slots: Mutex<SlotTable>,
    /// Hot-path folds whose key was not in the slot map (per feed).
    unregistered_folds: [AtomicU64; Feed::COUNT],
    /// Registrations dropped at the slot cap.
    overflow_dropped: AtomicU64,
}

fn word_plane() -> Box<[AtomicU64]> {
    let mut v = Vec::with_capacity(MAX_PRESENCE_SLOTS * PRESENCE_WORDS_PER_SLOT);
    for _ in 0..MAX_PRESENCE_SLOTS * PRESENCE_WORDS_PER_SLOT {
        v.push(AtomicU64::new(0));
    }
    v.into_boxed_slice()
}

impl FeedPresenceRegistry {
    /// Cold construction (once per process). Allocates the two fixed word
    /// planes (192 KiB total) — the hot fold never allocates.
    #[must_use]
    pub fn new(enabled: bool, process_start_ist_nanos: i64) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            process_start_ist_nanos,
            maps: std::array::from_fn(|_| papaya::HashMap::new()),
            bits: std::array::from_fn(|_| word_plane()),
            slots: Mutex::new(SlotTable::default()),
            unregistered_folds: std::array::from_fn(|_| AtomicU64::new(0)),
            overflow_dropped: AtomicU64::new(0),
        }
    }

    /// Hot-path fold: O(1), zero-alloc — one relaxed enabled check, one
    /// lock-free papaya read, one relaxed `fetch_or`. Out-of-session
    /// timestamps and unregistered keys return early (unregistered keys
    /// bump the per-feed counter — visible at drain).
    ///
    /// DHAT-ratcheted by `crates/core/tests/dhat_feed_presence.rs`;
    /// Criterion-budgeted by `feed_presence_record` in
    /// `quality/benchmark-budgets.toml`.
    #[inline]
    pub fn record_presence(
        &self,
        feed: Feed,
        security_id: u64,
        segment_code: u8,
        exchange_ts_ist_secs: u32,
    ) {
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        let Some(minute) = presence_minute_index(exchange_ts_ist_secs) else {
            return;
        };
        let fi = feed.index();
        let guard = self.maps[fi].pin();
        let Some(&slot) = guard.get(&(security_id, segment_code)) else {
            self.unregistered_folds[fi].fetch_add(1, Ordering::Relaxed);
            return;
        };
        let word = (slot as usize) * PRESENCE_WORDS_PER_SLOT + minute / 64;
        // Registration guarantees slot < MAX_PRESENCE_SLOTS; the bounds
        // check here is defensive (a corrupt index skips, never panics).
        if let Some(w) = self.bits[fi].get(word) {
            w.fetch_or(1u64 << (minute % 64), Ordering::Relaxed);
        }
    }

    /// Cold-path registration of one feed's instrument set for one IST
    /// day. Idempotent: re-registering the same set is a no-op beyond the
    /// day stamp; a NEW day clears the slot's bits for that feed first
    /// (correct even if the midnight reset was missed).
    ///
    /// `now_ist_nanos` stamps the feed's FIRST registration instant for
    /// `ist_day` (first call wins) — [`drain_day`] folds a post-session-open
    /// first registration into `covers_full_session` (PR-D review round 1).
    ///
    /// # Performance
    /// **O(universe) under a Mutex — cold path** (once per feed per day at
    /// the universe/watch build), never on the tick thread.
    pub fn register_instruments(
        &self,
        feed: Feed,
        ist_day: u64,
        regs: &[PresenceRegistration],
        now_ist_nanos: i64,
    ) -> PresenceRegisterSummary {
        let mut table = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let fi = feed.index();
        // First-registration instant for this (feed, day) — first call
        // wins; a later same-day re-registration never un-degrades.
        match table.first_registration[fi] {
            Some((day, _)) if day == ist_day => {}
            _ => table.first_registration[fi] = Some((ist_day, now_ist_nanos)),
        }
        let mut summary = PresenceRegisterSummary::default();
        let guard = self.maps[fi].pin();
        for reg in regs {
            let key = match &reg.pairing {
                Some(p) => SlotKey::Paired(p.clone()),
                None => SlotKey::Local(
                    u8::try_from(fi).unwrap_or(u8::MAX),
                    reg.security_id,
                    reg.segment_code,
                ),
            };
            let slot = if let Some(&s) = table.by_key.get(&key) {
                s
            } else {
                let idx = table.meta.len();
                if idx >= MAX_PRESENCE_SLOTS {
                    self.overflow_dropped.fetch_add(1, Ordering::Relaxed);
                    summary.overflow_dropped += 1;
                    continue;
                }
                table.meta.push(SlotMeta {
                    canonical_security_id: i64::try_from(reg.security_id).unwrap_or(i64::MAX),
                    segment_label: reg.segment_label,
                    symbol: reg.symbol.clone(),
                    canonical_from_dhan: matches!(feed, Feed::Dhan),
                    registered_day: [None; Feed::COUNT],
                });
                let idx32 = u32::try_from(idx).unwrap_or(u32::MAX);
                table.by_key.insert(key, idx32);
                idx32
            };
            let meta = &mut table.meta[slot as usize];
            // Canonical fields: the Dhan registration wins on a mapped
            // pair (design: canonical id = the Dhan SID), whichever feed
            // registered first.
            if matches!(feed, Feed::Dhan) && !meta.canonical_from_dhan {
                meta.canonical_security_id = i64::try_from(reg.security_id).unwrap_or(i64::MAX);
                meta.segment_label = reg.segment_label;
                meta.symbol = reg.symbol.clone();
                meta.canonical_from_dhan = true;
            }
            // Fresh day for this (slot, feed): clear its 6 words so a
            // long-lived process can never leak yesterday's minutes into
            // today (independent of the midnight reset).
            if meta.registered_day[fi] != Some(ist_day) {
                let base = (slot as usize) * PRESENCE_WORDS_PER_SLOT;
                for w in 0..PRESENCE_WORDS_PER_SLOT {
                    if let Some(word) = self.bits[fi].get(base + w) {
                        word.store(0, Ordering::Relaxed);
                    }
                }
                meta.registered_day[fi] = Some(ist_day);
            }
            guard.insert((reg.security_id, reg.segment_code), slot);
            summary.registered += 1;
        }
        // Day-scoped pair count (both feeds registered for THIS day).
        summary.paired = table
            .meta
            .iter()
            .filter(|m| m.registered_day.iter().all(|d| *d == Some(ist_day)))
            .count();
        summary
    }

    /// IST-midnight reset for one feed: zero its whole word plane + its
    /// unregistered-fold counter. Cold, O(slots × 6). Called from BOTH
    /// existing midnight force-seal tasks (mirror of
    /// `reset_day_lag_histogram`); the per-slot day-change clear at
    /// registration is the belt-and-braces backstop.
    pub fn reset_daily(&self, feed: Feed) {
        let fi = feed.index();
        for w in self.bits[fi].iter() {
            w.store(0, Ordering::Relaxed);
        }
        self.unregistered_folds[fi].store(0, Ordering::Relaxed);
    }

    /// Drain one day's presence (non-destructive — re-runs of the 15:45
    /// task read the same data; the midnight reset owns the day boundary).
    /// `None` when the fold is disabled or NOTHING is registered for the
    /// target day (a fresh post-close process / a past-day backfill) — the
    /// scoreboard then keeps its SQL fallback, honestly.
    ///
    /// # Performance
    /// **O(slots × 12 words) — cold path**, once per scoreboard run.
    #[must_use]
    pub fn drain_day(&self, target_ist_day: u64) -> Option<PresenceDrain> {
        if !self.enabled.load(Ordering::Relaxed) {
            return None;
        }
        let table = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut slots_out = Vec::new();
        // Feed::COUNT-sized (never hand-counted — the NTM 2-role→3-role
        // boot-panic lesson pinned in `feed.rs`; PR-D review round 1 LOW).
        let mut union = [[0u64; PRESENCE_WORDS_PER_SLOT]; Feed::COUNT];
        let mut totals = [FeedPresenceTotals::default(); Feed::COUNT];
        let to_i64 = |v: u32| i64::from(v);
        for (idx, meta) in table.meta.iter().enumerate() {
            let dhan_day = meta.registered_day[Feed::Dhan.index()] == Some(target_ist_day);
            let groww_day = meta.registered_day[Feed::Groww.index()] == Some(target_ist_day);
            if !dhan_day && !groww_day {
                continue;
            }
            let base = idx * PRESENCE_WORDS_PER_SLOT;
            let load = |fi: usize, registered: bool| -> [u64; PRESENCE_WORDS_PER_SLOT] {
                let mut words = [0u64; PRESENCE_WORDS_PER_SLOT];
                if registered {
                    for (w, out) in words.iter_mut().enumerate() {
                        if let Some(word) = self.bits[fi].get(base + w) {
                            *out = word.load(Ordering::Relaxed);
                        }
                    }
                }
                words
            };
            let dw = load(Feed::Dhan.index(), dhan_day);
            let gw = load(Feed::Groww.index(), groww_day);
            let pop = |ws: &[u64; PRESENCE_WORDS_PER_SLOT]| -> u32 {
                ws.iter().map(|w| w.count_ones()).sum()
            };
            let dhan_minutes = pop(&dw);
            let groww_minutes = pop(&gw);
            let mapped = dhan_day && groww_day;
            let (d_only, g_only, both) = if mapped {
                let mut d_only = 0u32;
                let mut g_only = 0u32;
                let mut both = 0u32;
                for w in 0..PRESENCE_WORDS_PER_SLOT {
                    d_only += (dw[w] & !gw[w]).count_ones();
                    g_only += (gw[w] & !dw[w]).count_ones();
                    both += (dw[w] & gw[w]).count_ones();
                }
                (to_i64(d_only), to_i64(g_only), to_i64(both))
            } else {
                // Singleton: exclusive-vs-nothing is not a measurement —
                // the comparison columns take the -1 sentinel (the
                // feed-off-day lesson, round 5 2026-07-10).
                (-1, -1, -1)
            };
            for w in 0..PRESENCE_WORDS_PER_SLOT {
                union[Feed::Dhan.index()][w] |= dw[w];
                union[Feed::Groww.index()][w] |= gw[w];
            }
            for (fi, registered, minutes) in [
                (Feed::Dhan.index(), dhan_day, dhan_minutes),
                (Feed::Groww.index(), groww_day, groww_minutes),
            ] {
                if registered {
                    totals[fi].registered_instruments += 1;
                    totals[fi].covered_instrument_minutes += i64::from(minutes);
                    if mapped {
                        totals[fi].mapped_instruments += 1;
                    } else {
                        totals[fi].unmapped_instruments += 1;
                    }
                }
            }
            slots_out.push(SlotCoverage {
                canonical_security_id: meta.canonical_security_id,
                segment_label: meta.segment_label,
                symbol: meta.symbol.clone(),
                mapped,
                dhan_registered: dhan_day,
                groww_registered: groww_day,
                dhan_minutes: i64::from(dhan_minutes),
                groww_minutes: i64::from(groww_minutes),
                dhan_only_minutes: d_only,
                groww_only_minutes: g_only,
                both_minutes: both,
            });
        }
        if slots_out.is_empty() {
            return None;
        }
        // Feed-level union overlap — the daily row's unique_win / both /
        // streaming semantics (same definition as the SQL minute sets,
        // sourced from real per-instrument presence).
        let du = &union[Feed::Dhan.index()];
        let gu = &union[Feed::Groww.index()];
        let mut d_stream = 0u32;
        let mut g_stream = 0u32;
        let mut d_only = 0u32;
        let mut g_only = 0u32;
        let mut both = 0u32;
        for w in 0..PRESENCE_WORDS_PER_SLOT {
            d_stream += du[w].count_ones();
            g_stream += gu[w].count_ones();
            d_only += (du[w] & !gu[w]).count_ones();
            g_only += (gu[w] & !du[w]).count_ones();
            both += (du[w] & gu[w]).count_ones();
        }
        totals[Feed::Dhan.index()].streaming_minutes = i64::from(d_stream);
        totals[Feed::Groww.index()].streaming_minutes = i64::from(g_stream);
        totals[Feed::Dhan.index()].unique_win_minutes = i64::from(d_only);
        totals[Feed::Groww.index()].unique_win_minutes = i64::from(g_only);
        totals[Feed::Dhan.index()].both_minutes = i64::from(both);
        totals[Feed::Groww.index()].both_minutes = i64::from(both);
        for feed in Feed::ALL {
            let fi = feed.index();
            totals[fi].unregistered_folds =
                i64::try_from(self.unregistered_folds[fi].load(Ordering::Relaxed))
                    .unwrap_or(i64::MAX);
        }
        // Full-session honesty: the process must have been alive at the
        // target day's 09:15 IST session open.
        let session_open_ist_nanos = i64::try_from(target_ist_day)
            .unwrap_or(i64::MAX / (86_400 * NANOS_PER_SEC))
            .saturating_mul(86_400 * NANOS_PER_SEC)
            .saturating_add(i64::from(PRESENCE_SESSION_START_SECS_OF_DAY_IST) * NANOS_PER_SEC);
        // ... AND every drained feed's FIRST registration for the target
        // day must precede the open (PR-D review round 1, 2026-07-11): a
        // late first registration destroyed (day-change clear) or missed
        // (unregistered folds) the 09:15→registration window, so the drain
        // must not claim full-session truth over it.
        let mut late_first_registration = false;
        for feed in Feed::ALL {
            let fi = feed.index();
            if totals[fi].registered_instruments > 0
                && let Some((day, at)) = table.first_registration[fi]
                && day == target_ist_day
                && at > session_open_ist_nanos
            {
                late_first_registration = true;
            }
        }
        Some(PresenceDrain {
            slots: slots_out,
            dhan: totals[Feed::Dhan.index()],
            groww: totals[Feed::Groww.index()],
            covers_full_session: self.process_start_ist_nanos <= session_open_ist_nanos
                && !late_first_registration,
            overflow_dropped: i64::try_from(self.overflow_dropped.load(Ordering::Relaxed))
                .unwrap_or(i64::MAX),
        })
    }
}

/// Process-global registry (house `OnceLock` pattern — mirrors
/// `DHAN_LAG_RING` / `global_tick_gap_detector`).
static GLOBAL: OnceLock<FeedPresenceRegistry> = OnceLock::new();

/// Current IST wall-clock in nanos (the shared init/registration stamp).
fn ist_now_nanos() -> i64 {
    chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(0)
        .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS)
}

/// Boot-time init — the main.rs PROCESS-GLOBAL boot prefix, BEFORE the
/// Groww activation watcher spawn and before either boot arm's
/// `load_instruments` (PR-D review round 1, 2026-07-11: `register_
/// instruments` is a `GLOBAL.get()` free fn that silently no-ops pre-init,
/// so a registration ordered before init lost that feed's whole day).
/// Idempotent: the first call constructs; later calls only refresh the
/// enabled gate. `enabled` = `[scoreboard] enabled && presence_fold_enabled`
/// (boot-read — no per-tick config access).
pub fn init_feed_presence(enabled: bool) {
    let start_ist_nanos = ist_now_nanos();
    let reg = GLOBAL.get_or_init(|| FeedPresenceRegistry::new(enabled, start_ist_nanos));
    reg.enabled.store(enabled, Ordering::Relaxed);
}

/// Hot-path free fn — no-op until [`init_feed_presence`] ran (a
/// pre-init tick can never panic or allocate).
#[inline]
pub fn record_presence(feed: Feed, security_id: u64, segment_code: u8, exchange_ts_ist_secs: u32) {
    if let Some(reg) = GLOBAL.get() {
        reg.record_presence(feed, security_id, segment_code, exchange_ts_ist_secs);
    }
}

/// Cold-path registration free fn. Returns `None` when the registry is
/// uninitialized (fold disabled / pre-init) — the caller logs and moves on.
pub fn register_instruments(
    feed: Feed,
    ist_day: u64,
    regs: &[PresenceRegistration],
) -> Option<PresenceRegisterSummary> {
    let now_ist_nanos = ist_now_nanos();
    GLOBAL
        .get()
        .map(|reg| reg.register_instruments(feed, ist_day, regs, now_ist_nanos))
}

/// Cold-path day drain free fn (the 15:45 scoreboard input).
#[must_use]
pub fn drain_day(target_ist_day: u64) -> Option<PresenceDrain> {
    GLOBAL.get().and_then(|reg| reg.drain_day(target_ist_day))
}

/// IST-midnight per-feed reset free fn (called from both existing
/// midnight force-seal tasks, next to `reset_day_lag_histogram`).
pub fn reset_daily(feed: Feed) {
    if let Some(reg) = GLOBAL.get() {
        reg.reset_daily(feed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 20_644;

    fn reg(
        sid: u64,
        seg: u8,
        label: &'static str,
        symbol: &str,
        pairing: Option<PairingKey>,
    ) -> PresenceRegistration {
        PresenceRegistration {
            security_id: sid,
            segment_code: seg,
            segment_label: label,
            symbol: symbol.to_string(),
            pairing,
        }
    }

    /// 09:15:00 IST on `DAY`, as IST epoch secs.
    fn session_secs(offset: u32) -> u32 {
        u32::try_from(DAY * 86_400).unwrap_or(0) + PRESENCE_SESSION_START_SECS_OF_DAY_IST + offset
    }

    /// Registration instant used by the default test registrations: IST
    /// midnight of `DAY` — safely BEFORE the 09:15 session open (so the
    /// late-first-registration degrade never fires unless a test wants it).
    fn reg_now() -> i64 {
        i64::try_from(DAY).unwrap_or(0) * 86_400 * NANOS_PER_SEC
    }

    fn registry_for_day() -> FeedPresenceRegistry {
        // Process "started" at IST midnight of DAY — covers the session.
        FeedPresenceRegistry::new(
            true,
            i64::try_from(DAY).unwrap_or(0) * 86_400 * NANOS_PER_SEC,
        )
    }

    #[test]
    fn test_presence_minute_index_session_boundaries() {
        // 09:14:59 IST — pre-open, discarded.
        assert_eq!(
            presence_minute_index(PRESENCE_SESSION_START_SECS_OF_DAY_IST - 1),
            None
        );
        // 09:15:00 → minute 0.
        assert_eq!(
            presence_minute_index(PRESENCE_SESSION_START_SECS_OF_DAY_IST),
            Some(0)
        );
        // 09:15:59 → still minute 0; 09:16:00 → minute 1.
        assert_eq!(
            presence_minute_index(PRESENCE_SESSION_START_SECS_OF_DAY_IST + 59),
            Some(0)
        );
        assert_eq!(
            presence_minute_index(PRESENCE_SESSION_START_SECS_OF_DAY_IST + 60),
            Some(1)
        );
        // 15:29:59 → minute 374 (the last session minute).
        assert_eq!(
            presence_minute_index(PRESENCE_SESSION_START_SECS_OF_DAY_IST + 375 * 60 - 1),
            Some(374)
        );
        // 15:30:00 — post-close, discarded.
        assert_eq!(
            presence_minute_index(PRESENCE_SESSION_START_SECS_OF_DAY_IST + 375 * 60),
            None
        );
        // A full epoch timestamp (not just seconds-of-day) works too.
        assert_eq!(presence_minute_index(session_secs(0)), Some(0));
    }

    #[test]
    fn test_record_presence_and_drain_day_isin_paired_slot_compared() {
        let r = registry_for_day();
        let isin = || Some(PairingKey::Isin("INE002A01018".to_string()));
        r.register_instruments(
            Feed::Dhan,
            DAY,
            &[reg(2885, 1, "NSE_EQ", "RELIANCE", isin())],
            reg_now(),
        );
        r.register_instruments(
            Feed::Groww,
            DAY,
            &[reg(1333, 1, "NSE_EQ", "RELIANCE", isin())],
            reg_now(),
        );
        // Dhan ticks minutes 0 + 1; Groww ticks minutes 1 + 2.
        r.record_presence(Feed::Dhan, 2885, 1, session_secs(0));
        r.record_presence(Feed::Dhan, 2885, 1, session_secs(70));
        r.record_presence(Feed::Groww, 1333, 1, session_secs(65));
        r.record_presence(Feed::Groww, 1333, 1, session_secs(125));
        let d = r.drain_day(DAY).expect("drain");
        assert_eq!(d.slots.len(), 1, "one shared canonical slot");
        let s = &d.slots[0];
        assert!(s.mapped);
        assert_eq!(s.canonical_security_id, 2885, "canonical id = the Dhan SID");
        assert_eq!(s.dhan_minutes, 2);
        assert_eq!(s.groww_minutes, 2);
        assert_eq!(s.dhan_only_minutes, 1);
        assert_eq!(s.groww_only_minutes, 1);
        assert_eq!(s.both_minutes, 1);
        assert_eq!(d.dhan.unique_win_minutes, 1);
        assert_eq!(d.groww.unique_win_minutes, 1);
        assert_eq!(d.dhan.both_minutes, 1);
        assert_eq!(d.dhan.mapped_instruments, 1);
        assert_eq!(d.dhan.unmapped_instruments, 0);
        assert!(d.covers_full_session);
    }

    #[test]
    fn test_canonical_fields_are_dhan_even_when_groww_registers_first() {
        let r = registry_for_day();
        let key = || Some(PairingKey::Index("NIFTY".to_string()));
        r.register_instruments(
            Feed::Groww,
            DAY,
            &[reg(0x4000_0000_0000_1234, 0, "IDX_I", "NSE-NIFTY", key())],
            reg_now(),
        );
        r.register_instruments(
            Feed::Dhan,
            DAY,
            &[reg(13, 0, "IDX_I", "NIFTY", key())],
            reg_now(),
        );
        let d = r.drain_day(DAY).expect("drain");
        assert_eq!(d.slots.len(), 1);
        assert_eq!(d.slots[0].canonical_security_id, 13);
        assert_eq!(d.slots[0].symbol, "NIFTY");
        assert!(d.slots[0].mapped);
    }

    #[test]
    fn test_future_pairing_key_is_contract_identity_not_native_id() {
        let r = registry_for_day();
        let key = || {
            Some(PairingKey::Future {
                underlying: "SENSEX".to_string(),
                expiry: "2026-07-30".to_string(),
            })
        };
        r.register_instruments(
            Feed::Dhan,
            DAY,
            &[reg(428_001, 8, "BSE_FNO", "SENSEX-Jul2026-FUT", key())],
            reg_now(),
        );
        r.register_instruments(
            Feed::Groww,
            DAY,
            &[reg(999_777, 8, "BSE_FNO", "SENSEX-Jul2026-FUT", key())],
            reg_now(),
        );
        let d = r.drain_day(DAY).expect("drain");
        assert_eq!(d.slots.len(), 1, "different native ids, one contract slot");
        assert!(d.slots[0].mapped);
    }

    #[test]
    fn test_unmapped_singleton_reported_with_sentinel_comparison_columns() {
        let r = registry_for_day();
        // A Groww-only stock with no ISIN — feed-local slot.
        r.register_instruments(
            Feed::Groww,
            DAY,
            &[reg(555, 1, "NSE_EQ", "ODDSTOCK", None)],
            reg_now(),
        );
        r.record_presence(Feed::Groww, 555, 1, session_secs(0));
        let d = r.drain_day(DAY).expect("drain");
        assert_eq!(d.slots.len(), 1);
        let s = &d.slots[0];
        assert!(!s.mapped);
        assert!(!s.dhan_registered);
        assert!(s.groww_registered);
        assert_eq!(s.groww_minutes, 1);
        assert_eq!(s.dhan_minutes, 0);
        // Exclusive-vs-nothing is not a measurement — sentinels.
        assert_eq!(s.dhan_only_minutes, -1);
        assert_eq!(s.groww_only_minutes, -1);
        assert_eq!(s.both_minutes, -1);
        assert_eq!(d.groww.unmapped_instruments, 1);
        assert_eq!(d.groww.covered_instrument_minutes, 1);
    }

    #[test]
    fn test_unregistered_fold_counts_never_silent() {
        let r = registry_for_day();
        r.register_instruments(
            Feed::Dhan,
            DAY,
            &[reg(13, 0, "IDX_I", "NIFTY", None)],
            reg_now(),
        );
        // A tick for a SID outside the registered universe.
        r.record_presence(Feed::Dhan, 99_999, 1, session_secs(0));
        r.record_presence(Feed::Dhan, 13, 0, session_secs(0));
        let d = r.drain_day(DAY).expect("drain");
        assert_eq!(d.dhan.unregistered_folds, 1);
        assert_eq!(d.dhan.covered_instrument_minutes, 1);
    }

    #[test]
    fn test_out_of_session_ticks_never_set_bits() {
        let r = registry_for_day();
        r.register_instruments(
            Feed::Dhan,
            DAY,
            &[reg(13, 0, "IDX_I", "NIFTY", None)],
            reg_now(),
        );
        // 09:00 IST pre-open tick + 15:31 post-close tick.
        let day_base = u32::try_from(DAY * 86_400).unwrap_or(0);
        r.record_presence(Feed::Dhan, 13, 0, day_base + 9 * 3600);
        r.record_presence(Feed::Dhan, 13, 0, day_base + 15 * 3600 + 31 * 60);
        let d = r.drain_day(DAY).expect("drain");
        assert_eq!(d.dhan.covered_instrument_minutes, 0);
        assert_eq!(d.dhan.streaming_minutes, 0);
    }

    #[test]
    fn test_disabled_gate_no_bits_and_no_drain() {
        let r = FeedPresenceRegistry::new(false, 0);
        r.register_instruments(
            Feed::Dhan,
            DAY,
            &[reg(13, 0, "IDX_I", "NIFTY", None)],
            reg_now(),
        );
        r.record_presence(Feed::Dhan, 13, 0, session_secs(0));
        assert_eq!(r.drain_day(DAY), None, "disabled fold drains nothing");
    }

    #[test]
    fn test_stale_previous_day_slot_excluded_and_bits_cleared_on_new_day() {
        let r = registry_for_day();
        r.register_instruments(
            Feed::Dhan,
            DAY,
            &[reg(13, 0, "IDX_I", "NIFTY", None)],
            reg_now(),
        );
        r.record_presence(Feed::Dhan, 13, 0, session_secs(0));
        // Next day: re-register — the slot's bits for Dhan must clear.
        r.register_instruments(
            Feed::Dhan,
            DAY + 1,
            &[reg(13, 0, "IDX_I", "NIFTY", None)],
            reg_now(),
        );
        // Yesterday's drain: the slot is no longer registered for DAY.
        assert_eq!(r.drain_day(DAY), None);
        let d1 = r.drain_day(DAY + 1).expect("drain new day");
        assert_eq!(
            d1.dhan.covered_instrument_minutes, 0,
            "day-change registration must clear yesterday's bits"
        );
    }

    #[test]
    fn test_reset_daily_clears_bits_and_counters() {
        let r = registry_for_day();
        r.register_instruments(
            Feed::Dhan,
            DAY,
            &[reg(13, 0, "IDX_I", "NIFTY", None)],
            reg_now(),
        );
        r.record_presence(Feed::Dhan, 13, 0, session_secs(0));
        r.record_presence(Feed::Dhan, 4242, 1, session_secs(0)); // unregistered
        r.reset_daily(Feed::Dhan);
        let d = r.drain_day(DAY).expect("slot still registered for DAY");
        assert_eq!(d.dhan.covered_instrument_minutes, 0);
        assert_eq!(d.dhan.unregistered_folds, 0);
    }

    #[test]
    fn test_register_instruments_reregistration_is_idempotent_same_day() {
        let r = registry_for_day();
        let regs = [reg(13, 0, "IDX_I", "NIFTY", None)];
        r.register_instruments(Feed::Dhan, DAY, &regs, reg_now());
        r.record_presence(Feed::Dhan, 13, 0, session_secs(0));
        // Same-day re-registration (warm-snapshot boot + background
        // reconcile both register) must NOT clear the day's bits.
        r.register_instruments(Feed::Dhan, DAY, &regs, reg_now());
        let d = r.drain_day(DAY).expect("drain");
        assert_eq!(d.dhan.covered_instrument_minutes, 1);
        assert_eq!(d.slots.len(), 1);
    }

    #[test]
    fn test_overflow_beyond_slot_cap_is_counted_never_silent() {
        let r = registry_for_day();
        let mut regs = Vec::new();
        for i in 0..(MAX_PRESENCE_SLOTS as u64 + 5) {
            regs.push(reg(i + 1, 1, "NSE_EQ", "S", None));
        }
        let s = r.register_instruments(Feed::Dhan, DAY, &regs, reg_now());
        assert_eq!(s.overflow_dropped, 5);
        assert_eq!(s.registered, MAX_PRESENCE_SLOTS);
        let d = r.drain_day(DAY).expect("drain");
        assert_eq!(d.overflow_dropped, 5);
    }

    #[test]
    fn test_mid_day_process_start_flags_partial_session() {
        // Process "started" at 11:00 IST on DAY — after session open.
        let start = (i64::try_from(DAY).unwrap_or(0) * 86_400 + 11 * 3600) * NANOS_PER_SEC;
        let r = FeedPresenceRegistry::new(true, start);
        r.register_instruments(
            Feed::Dhan,
            DAY,
            &[reg(13, 0, "IDX_I", "NIFTY", None)],
            reg_now(),
        );
        r.record_presence(Feed::Dhan, 13, 0, session_secs(2 * 3600));
        let d = r.drain_day(DAY).expect("drain");
        assert!(!d.covers_full_session);
    }

    #[test]
    fn test_late_first_registration_degrades_covers_full_session() {
        // PR-D review round 1 (MEDIUM): a midnight-crossing process whose
        // FIRST registration for the day lands AFTER the 09:15 open has
        // destroyed (day-change clear) or missed (unregistered folds) the
        // 09:15→registration window — the drain must degrade to a
        // partial-session stamp, never claim full-session truth.
        let r = registry_for_day(); // process alive from IST midnight
        let late = reg_now() + i64::from(10 * 3600) * NANOS_PER_SEC; // 10:00 IST
        r.register_instruments(Feed::Dhan, DAY, &[reg(13, 0, "IDX_I", "NIFTY", None)], late);
        r.record_presence(Feed::Dhan, 13, 0, session_secs(2 * 3600));
        let d = r.drain_day(DAY).expect("drain");
        assert!(
            !d.covers_full_session,
            "post-open first registration must degrade the stamp"
        );
        // A later SAME-day re-registration never un-degrades (first wins).
        r.register_instruments(
            Feed::Dhan,
            DAY,
            &[reg(13, 0, "IDX_I", "NIFTY", None)],
            reg_now(),
        );
        let d = r.drain_day(DAY).expect("drain");
        assert!(!d.covers_full_session, "first registration instant wins");
    }

    #[test]
    fn test_pre_open_first_registration_keeps_full_session_stamp() {
        // The normal prod morning: process up + registered before 09:15 —
        // the full-session stamp stands.
        let r = registry_for_day();
        r.register_instruments(
            Feed::Dhan,
            DAY,
            &[reg(13, 0, "IDX_I", "NIFTY", None)],
            reg_now() + i64::from(8 * 3600 + 45 * 60) * NANOS_PER_SEC, // 08:45
        );
        r.record_presence(Feed::Dhan, 13, 0, session_secs(0));
        let d = r.drain_day(DAY).expect("drain");
        assert!(d.covers_full_session);
    }

    #[test]
    fn test_init_feed_presence_and_global_free_fns_are_safe_pre_init() {
        // Must never panic or allocate state before init (the OnceLock in
        // THIS test binary may already be initialized by another test —
        // both outcomes are safe; the pin is "no panic").
        record_presence(Feed::Dhan, 1, 1, session_secs(0));
        let _ = drain_day(1);
        reset_daily(Feed::Groww);
        // And init is idempotent + refreshes the gate (safe to call from
        // both boot arms).
        init_feed_presence(false);
        init_feed_presence(false);
        record_presence(Feed::Dhan, 1, 1, session_secs(0));
    }
}

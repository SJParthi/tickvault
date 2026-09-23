//! `top_volume_<tf>` tables — a queryable record of WHICH option contracts
//! were the busiest, and whether we were actually watching them.
//!
//! ## ⚠ 2026-09-22 — FOUR DIRECT TABLES, one per timeframe
//!
//! Operator, verbatim: *"make the table itself per timeframe per timestamp
//! shoudl be always sorted by volume percentage desc always timestamp asc …
//! make it as direct tables dude why focusing on view"*.
//!
//! | Table | Holds | Written |
//! |---|---|---|
//! | `top_volume_1s` | the 1-second board | every second |
//! | `top_volume_3s` | the 3-second board | every 3 s |
//! | `top_volume_5s` | the 5-second board | every 5 s |
//! | `top_volume_1m` | the 1-minute board | every minute |
//!
//! Same 15 columns, same DEDUP key, `PARTITION BY HOUR` each. The shared
//! `top_volume` table and its four views are RETIRED; the writer routes each
//! row by [`SnapshotCadence::table_name`].
//!
//! **Order.** One sweep is appended in rank order — `volume_percentage_change`
//! DESCENDING — and sweeps run in time order, so rows are WRITTEN
//! `ts ASC, volume_percentage_change DESC`. That is pinned by test on the
//! projection. Whether QuestDB PRESERVES arrival order among rows sharing one
//! `ts` through WAL apply and DEDUP is **UNVERIFIED** (no live QuestDB is
//! reachable from the build environment); a reader who needs the order
//! guaranteed today writes `ORDER BY ts, volume_percentage_change DESC`, which
//! over already-ordered rows is close to free.
//!
//!
//! Operator directive 2026-09-06: *"ensure to capture the top volume gainers
//! of the entire options contracts starting 9.15 am till 3.39 pm ... meanwhile
//! ensure to have it as a separate table to have ticks 1 second and 5 second
//! to have these top volume gainers ... because only using db i can see this
//! right dude"*.
//!
//! ## Why this table exists
//!
//! The ranking itself lives in RAM (`crates/app/src/volume_leaderboard.rs`)
//! and dies with the process. That is correct for the DECISION path â a
//! trading decision never reads the database â but it means three questions
//! were unanswerable after a session ended:
//!
//! * which contracts were actually the heaviest, minute by minute?
//! * did the heaviest contract actually get a depth socket, or did we miss it?
//! * did the ranking churn, or was it stable?
//!
//! The `subscribed` column is what earns the table: it is the difference
//! between "we ranked it first" and "we were watching it". Without it the
//! rows are trivia; with it, one query audits the whole steering decision.
//!
//! ## What a row is
//!
//! ONE row per (snapshot boundary, timeframe, family, contract). The
//! designated `ts` is the SNAPSHOT boundary â a whole second â so a
//! re-emitted snapshot UPSERTs in place rather than duplicating (the
//! scoreboard mechanic). A contract appears at most once per (snapshot,
//! timeframe, family) already.
//!
//! ## â  `rank` was REMOVED 2026-09-13, and an existing table still has it
//!
//! Operator: *"i clelary told you to rmeve the rank in top volume"*. The
//! column is gone from the CREATE, from the ALTER manifest, from the row
//! struct and from the ILP line, and `console_views` no longer selects it.
//!
//! **What happens to a box that already has the table is worth stating
//! plainly, because it is not what "removed" sounds like.** This module's
//! self-heal is CREATE → `ADD COLUMN IF NOT EXISTS` → `DEDUP ENABLE`, and it
//! contains no DROP by design (`top_volume_ensure_statements_never_drop_
//! and_end_with_dedup_enable` fails the build on one, because a DROP in a path
//! that runs every boot deletes history on any boot). QuestDB cannot drop a
//! column through this path at all.
//!
//! So on an EXISTING table the `rank` column **stays on disk and simply stops
//! being written**: every row from 2026-09-13 onward carries NULL there, and
//! every row before it keeps the value it had. It is not removed from disk, it
//! is not reclaimed, and nothing here pretends otherwise. Actually dropping it
//! is a one-line `ALTER TABLE top_volume DROP COLUMN rank` run by an operator
//! against a table this repository never drops automatically â or it ages out
//! with the 15-day `RetentionClass::MarketData` window on its own. A FRESH
//! table (a new box, or after the retention sweep has cycled) never has the
//! column at all.
//!
//! ## Schema
//!
//! ```sql
//! -- identical for top_volume_1s / _3s / _5s / _1m
//! CREATE TABLE IF NOT EXISTS top_volume_1s (
//!     ts TIMESTAMP, tf SYMBOL, family SYMBOL, feed SYMBOL,
//!     segment SYMBOL, contract SYMBOL, security_id LONG,
//!     underlying_id LONG,
//!     volume LONG,
//!     per_lot_quantity LONG, total_lots_traded LONG,
//!     volume_percentage_change LONG,
//!     percentage_change DOUBLE,
//!     open_percentage_change DOUBLE,
//!     subscribed BOOLEAN
//! ) timestamp(ts) PARTITION BY HOUR
//!   DEDUP UPSERT KEYS(ts, tf, family, feed, security_id, segment);
//! ```
//!
//! `PARTITION BY HOUR` matches `ticks` and `market_depth` â this is
//! high-frequency snapshot data and the hour is the archival unit that makes
//! same-day archive-then-drop possible. `underlying_id` is stored as the
//! numeric id we actually hold, NOT a name: the ranking layer never sees a
//! symbol, and inventing one here would be fabrication. Join to
//! `instrument_lifecycle` for names.
//!
//! `volume_percentage_change` is THE SORT KEY and `volume` is not, which is
//! the one thing a reader of this table has to know. `total_lots_traded` is
//! the whole lots traded INSIDE the window that just closed, normalised by
//! `per_lot_quantity`: a 1s row measures one second, a 5s row measures five,
//! and `volume_percentage_change` is that lot count expressed as a
//! whole-number percentage against one lot.
//!
//! `volume` is the CANDLE's OWN signed volume for the same window -- the SAME
//! number `candles_<tf>` carries, read off the same fold probe rather than
//! recomputed, so the two tables agree with no derivation (operator,
//! 2026-09-19: *"no extra claucltion or derivation"*). It is NOT the sort key
//! and must never be used as one: a signed value orders sellers below an
//! untraded contract. It is NULL when the fold has no bar for the window --
//! never a zero, which would read as "traded nothing".
//!
//! ## Honest volume
//!
//! â  EVERY FIGURE IN THIS SECTION WAS RE-DERIVED 2026-09-12, and the previous
//! ones are recorded rather than deleted because all of them shared one
//! premise that the operator's own change removed.
//!
//! They read: "At the authorized 250 contracts per family, both families,
//! 09:15-15:39: 500 rows/second x 22,440 s = ~11.2M rows for the `1s` stream
//! plus ~2.2M for `5s` -- ~13.4M rows, ~1.19 GB per session at the ~88 B row
//! width ... ~14.5 GB resident over the 15-day window." Two of the three terms
//! moved on 2026-09-12: the per-family cut is GONE (every traded contract is
//! persisted) and there are FOUR cadences, not two.
//!
//! What replaces them is a BOUND and a range, not a point estimate, because
//! the row count is now a property of the market rather than of a constant:
//!
//! * Per sweep: one row per contract with a NON-ZERO window delta. A contract
//!   that did not trade in the window is skipped at the zero-lot check, so the
//!   count is the TRADED population, not the tracked one.
//! * Hard ceiling per sweep: `TOP_VOLUME_MAX_ROWS_PER_SWEEP` (50,000 =
//!   25,000/family x 2 families), the tracked-contract cap. Reaching it needs
//!   every tracked contract of both families to trade inside one second.
//! * Per session: the four cadences fire 23,100 + 7,700 + 4,620 + 385 = **35,805**
//!   times over 09:15-15:39. At an ASSUMED mean of 2,000 traded contracts per
//!   sweep that is **71.6M rows, ~6.30 GB** at the ~88 B row width â roughly 5x
//!   the old figure and ~2% of the ~307 GB a session already writes.
//!
//!   â  CORRECTED 2026-09-12: this read "22,440 + 7,480 + 4,488 + 374 = ~34,782",
//!   which is a 22,440-second window â i.e. a 15:29 close. The window is
//!   `TICK_PERSIST_END` 56,400 â `TICK_PERSIST_START` 33,300 = **23,100 s**, as
//!   `the_capture_window_closes_after_the_1539_minute` pins. Understated 2.9%.
//!   Small, and corrected because every figure below is derived from it.
//!
//! **The 2,000 figure is Assumed and is the one number here worth measuring.**
//! It has never been read: the 1-second traded population was measured once
//! (3,187,232 rows for a whole session under the 250 cut, which tells us
//! nothing about the uncut count), and the 3s/5s/1m figures were never
//! measured at all because the source partitions were archived to S3 and
//! dropped from EBS before they could be. Since 2026-09-22 each cadence is
//! its own table, so the measurement is one count per table --
//! `SELECT count(*) FROM top_volume_1s WHERE ts IN today()`, then the same for
//! `_3s`, `_5s` and `_1m` -- on the first session with this build. (The single
//! `GROUP BY tf` query that used to stand here names a table that no longer
//! exists.)
//!
//! The `5s` rows are numerically a subset of the `1s` rows and are kept
//! anyway because they mean something different: a `5s` row is the ranking
//! that ACTUALLY DROVE a re-steer decision, which is the row an audit wants.
//! The same now holds for `3s` and `1m` against the boards they describe.
//!
//! RETENTION, stated because it is a standing commitment and not a one-day
//! cost: `HOUR_PARTITIONED_TABLES` membership puts this table in
//! `RetentionClass::MarketData`, whose window is `market_data_hot_days`
//! (default 15). â  CORRECTED 2026-09-12: this said "**92 GB resident** â¦ about
//! 15%", which multiplied the per-session figure by 15. `market_data_hot_days`
//! is 15 CALENDAR days, and 15 calendar days is roughly **11 trading
//! sessions** â the volume writes nothing on a weekend. At the corrected
//! ~6.30 GB/session that is **â69 GB resident** on the 600 GB volume, about
//! **11.5%**, still well up from the 2.4% the old figures claimed. Wrong in
//! the SAFE direction (it over-stated the commitment by ~33%), and corrected
//! anyway: a sizing figure nobody can reproduce is a figure the next reader
//! either re-derives or, worse, quotes. That is a materially larger commitment than the old
//! figures implied and it is stated
//! plainly rather than left to be discovered: if the measured row count lands
//! near the top of the range, `market_data_hot_days` for this table is the
//! lever, and the 2026-09-04 zero-capture day is what happens when a table
//! growing at this rate is not watched. It is NOT exempt from the sweep.

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ProtocolVersion, Sender, TimestampNanos};
use tracing::{debug, error, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::types::ExchangeSegment;

/// The per-timeframe DIRECT tables — one per [`SnapshotCadence`].
///
/// ⚠ 2026-09-22 (operator: *"make the table itself per timeframe … make it as
/// direct tables dude why focusing on view"*). Until today every cadence was
/// written to ONE `top_volume` table and read through four views filtered on
/// `tf`. Each cadence now has its OWN table under the name the view used to
/// carry, so a reader of `top_volume_5s` reads rows that are nothing but the
/// 5-second board — no join, no filter over the other three cadences.
///
/// Declared as `…_TABLE` consts (not only inside `table_name()`) so
/// `partition_retention_coverage_guard`, which discovers tables by scanning
/// for that name pattern, sees all four and demands a retention decision for
/// each.
pub const TOP_VOLUME_1S_TABLE: &str = "top_volume_1s";
/// See [`TOP_VOLUME_1S_TABLE`].
pub const TOP_VOLUME_3S_TABLE: &str = "top_volume_3s";
/// See [`TOP_VOLUME_1S_TABLE`].
pub const TOP_VOLUME_5S_TABLE: &str = "top_volume_5s";
/// See [`TOP_VOLUME_1S_TABLE`].
pub const TOP_VOLUME_1M_TABLE: &str = "top_volume_1m";

/// The single shared table every cadence was written to from 2026-09-12 until
/// 2026-09-22. RETIRED as a write target — nothing writes it any more.
///
/// Kept as a named const, and kept in `HOUR_PARTITIONED_TABLES`, so any rows it
/// still holds age out under the 15-day market-data window instead of sitting
/// un-swept forever. The fresh-start reset drops it outright on a box that has
/// not yet run that reset.
pub const LEGACY_TOP_VOLUME_TABLE: &str = "top_volume";

/// The name the shared table carried until 2026-09-12. Also retired, also
/// dropped by the fresh-start reset; listed in `RETENTION_EXEMPT_TABLES`.
pub const LEGACY_TOP_VOLUME_RANK_TABLE: &str = "top_volume_rank";

/// DEDUP key. Designated `ts` FIRST (2026-04-28 regression rule); `segment`
/// alongside `security_id` (I-P1-11 â the bare id is reused across segments
/// and two instruments would silently collapse into one row); `feed` in-key
/// (operator override 2026-06-28, feed-in-key EVERYWHERE).
///
/// Declared as a `DEDUP_KEY_*` const rather than an inline literal because
/// `dedup_segment_meta_guard` discovers keys by scanning for that name
/// pattern â an inline literal would put this key OUTSIDE the guard.
///
/// The key is UNCHANGED by the 2026-09-13 column changes: `contract` and
/// `net_volume_chg_milli_pct` are both leaf display columns, and `contract` in
/// particular must stay OUT â it is a pure function of `security_id`, so
/// adding it would widen the key with no new identity while making a stale
/// label able to duplicate a row.
pub const DEDUP_KEY_TOP_VOLUME_RANK: &str = "ts, tf, family, feed, security_id, segment";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Snapshot cadence label â the `tf` SYMBOL column. Stable wire strings.
///
/// `#[repr(u8)]` with EXPLICIT discriminants, following `TfIndex` (the
/// workspace's other ordinal-indexed enum): the discriminant IS the array
/// slot, so [`Self::index`] is a repr cast rather than a hand-written
/// `match` that can disagree with [`Self::ALL`]. The previous shape kept a
/// separate `window_index` match in the ranking crate, and the two could
/// drift with nothing failing until a row landed in the wrong window.
///
/// Discriminants are FROZEN. They are the `baseline` array slot for every
/// tracked contract; renumbering them re-points every in-flight window at
/// another cadence's baseline mid-session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum SnapshotCadence {
    /// Every whole second â the fine-grained record.
    OneSecond = 0,
    /// Every three seconds (operator 2026-09-12).
    ThreeSecond = 1,
    /// Every five seconds â the boundary at which the depth set is re-steered,
    /// so these rows are the ones that actually drove a subscription decision.
    FiveSecond = 2,
    /// Every minute (operator 2026-09-12) â the coarse frame, and the only one
    /// whose window spans a depth re-steer rather than coinciding with one.
    OneMinute = 3,
}

impl SnapshotCadence {
    /// Every cadence, so callers that need one slot per cadence can size
    /// themselves from the enum instead of a literal.
    ///
    /// A const array rather than a derive, for the reason `LegRefusal::ALL`
    /// carries: adding a variant without adding it here is a silent
    /// under-count, and the ranking's per-cadence baselines are sized from
    /// `ALL.len()` â an under-count there would alias two cadences onto one
    /// baseline and make both boards report a window neither one measured.
    ///
    /// The const assert below proves this list is in ordinal order with no
    /// gaps and no duplicates. It does NOT prove the list is COMPLETE: no
    /// stable-Rust construct counts an enum's variants (`variant_count` is
    /// nightly), so a fifth variant added here and nowhere else still
    /// compiles. What stops that reaching production is
    /// [`Self::slot`] â a checked index that REFUSES an unlisted cadence and
    /// counts the refusal, instead of indexing out of bounds on the frame
    /// drain, where `panic = "abort"` would take the process down mid-session.
    pub const ALL: [Self; 4] = [
        Self::OneSecond,
        Self::ThreeSecond,
        Self::FiveSecond,
        Self::OneMinute,
    ];

    /// The array slot for this cadence â the `#[repr(u8)]` discriminant.
    ///
    /// Structural: there is no `match` to keep in step with [`Self::ALL`].
    /// Callers that index a `[_; ALL.len()]` array must go through
    /// [`Self::slot`], which bounds-checks this value.
    #[inline]
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// The array slot, bounds-checked against [`Self::ALL`].
    ///
    /// `None` means a variant exists that `ALL` does not list â the one
    /// failure the ordinal-order assert cannot rule out. Every caller that
    /// would otherwise write `array[cadence.index()]` uses this, so that
    /// mistake becomes a counted refusal on a cold path instead of an
    /// out-of-bounds index on the frame drain.
    #[inline]
    #[must_use]
    pub const fn slot(self) -> Option<usize> {
        let i = self as usize;
        if i < Self::ALL.len() { Some(i) } else { None }
    }

    /// Decodes an array slot back to a cadence. `None` for out-of-range.
    #[inline]
    #[must_use]
    pub const fn from_index(index: usize) -> Option<Self> {
        match index {
            0 => Some(Self::OneSecond),
            1 => Some(Self::ThreeSecond),
            2 => Some(Self::FiveSecond),
            3 => Some(Self::OneMinute),
            _ => None,
        }
    }

    /// Stable wire label. Never reworded â it is a persisted SYMBOL value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OneSecond => "1s",
            Self::ThreeSecond => "3s",
            Self::FiveSecond => "5s",
            Self::OneMinute => "1m",
        }
    }

    /// How often this cadence fires, in whole seconds.
    ///
    /// The scheduler's timer period comes from HERE rather than from a literal
    /// at the call site, so the interval a snapshot is actually taken at and
    /// the `cadence` SYMBOL it is stored under cannot drift apart. A row
    /// labelled `5s` written every four seconds would be a quiet lie in a
    /// table whose whole purpose is to be queried after the fact.
    #[must_use]
    pub const fn interval_secs(self) -> u64 {
        match self {
            Self::OneSecond => 1,
            Self::ThreeSecond => 3,
            Self::FiveSecond => 5,
            Self::OneMinute => 60,
        }
    }

    /// The DIRECT table this cadence's rows are written to (2026-09-22).
    ///
    /// Lives HERE, beside the `tf` label the same rows carry, because the
    /// table's name and its `tf` value are one claim: a table called
    /// `top_volume_3s` holding `tf = '5s'` rows is wrong in a way no reader of
    /// either file alone could see. A `const fn` match — O(1), no allocation —
    /// because the writer calls it once per row.
    ///
    /// *(Was `view_name()` until 2026-09-22, when these names were read-only
    /// views over one shared `top_volume` table.)*
    #[must_use]
    pub const fn table_name(self) -> &'static str {
        match self {
            Self::OneSecond => TOP_VOLUME_1S_TABLE,
            Self::ThreeSecond => TOP_VOLUME_3S_TABLE,
            Self::FiveSecond => TOP_VOLUME_5S_TABLE,
            Self::OneMinute => TOP_VOLUME_1M_TABLE,
        }
    }
}

/// `ALL` is in ordinal order, with no gaps and no duplicates â so
/// `ALL[i].index() == i` for every slot, and `index()` can be used as an
/// array subscript without a lookup. Adding a variant in the middle, or
/// listing one twice, fails the BUILD rather than silently re-pointing one
/// cadence's baselines at another's.
const _: () = {
    let mut i = 0;
    while i < SnapshotCadence::ALL.len() {
        assert!(
            SnapshotCadence::ALL[i].index() == i,
            "SnapshotCadence::ALL must be in #[repr(u8)] discriminant order: \
             the ranking indexes its per-cadence baseline array by the raw \
             discriminant, so a list out of order points a cadence at another \
             cadence's window."
        );
        i += 1;
    }
};

/// One ranked contract at one snapshot boundary, ready for ILP write.
#[derive(Clone, Debug, PartialEq)]
pub struct TopVolumeRankRow<'a> {
    /// Designated timestamp â the SNAPSHOT boundary in IST nanoseconds.
    /// A whole second, so a re-emit UPSERTs in place.
    pub snapshot_ts_ist_nanos: i64,
    /// Snapshot cadence (`1s` / `3s` / `5s` / `1m`).
    pub cadence: SnapshotCadence,
    /// Option family â stock and index options are ranked in SEPARATE
    /// leaderboards, and the column records which one this row came from.
    pub family: &'static str,
    /// Feed that produced the volume (`dhan`).
    pub feed: &'static str,
    /// Segment SYMBOL string (`NSE_FNO`, ...).
    ///
    /// `&'static str`, like `family` and `feed` beside it. It was `String`
    /// until 2026-09-08, and the producer filled it with
    /// `contract.segment.as_str().to_string()` â where `ExchangeSegment::as_str`
    /// ALREADY returns `&'static str` and this consumer immediately calls
    /// `.as_str()` on it again. A heap allocation and a copy, round-tripping a
    /// static string back to itself.
    ///
    /// That is 250 rows per family x 2 families x 2 cadences = ~500-600
    /// `String`s per second, on the frame-drain task, in a codebase whose first
    /// principle is zero allocation on the hot path. Its two neighbours were
    /// already `&'static str`, so the type was the odd one out as well as the
    /// costly one.
    pub segment: &'static str,
    /// The FULL human label for this contract â underlying, expiry, strike and
    /// leg in one column: `NIFTY-25Sep2026-24500-CE`.
    ///
    /// Operator 2026-09-13: *"i need to know the precise symbol anme as well
    /// dude see suposoe if it has symbol contartc options strieks expriy emasn
    /// we ened to know evryhtign in a single column also as well"*. Before it,
    /// answering "which contract is this row" needed a join to
    /// `instrument_lifecycle` â and `underlying_id` alone cannot distinguish
    /// two strikes of the same stock.
    ///
    /// # Why a borrow and not a `String`
    ///
    /// The projection runs on the FRAME-DRAIN task over up to
    /// `TOP_VOLUME_MAX_ROWS_PER_SWEEP` rows across four cadences. A `String`
    /// here is one heap allocation per row per sweep â order 80,000 a second
    /// at the ceiling â on the path whose first principle is zero allocation.
    /// The label is CONSTANT for a given contract (underlying, expiry, strike
    /// and leg never change for a security id), so it is rendered ONCE when the
    /// day's contract map is published and this field borrows it out of that
    /// snapshot. Per row: one pointer copy, no allocation, no refcount bump.
    ///
    /// The lifetime is the label snapshot's. Rows are appended inside the same
    /// sweep that built them, so it never outlives the `Arc` the caller holds.
    ///
    /// Stored as a QuestDB `SYMBOL`, which interns: ~22,000 distinct labels
    /// against tens of millions of rows a session, so the column costs a small
    /// integer per row rather than ~25 bytes of repeated text. That is the
    /// shape SYMBOL exists for; it is also why this must never become a
    /// per-ROW-unique value.
    ///
    /// `contract_underlying_map::UNLABELLED_CONTRACT` when the label snapshot
    /// has no entry â counted as `SnapshotRefusal::LabelUnavailable`, never
    /// silently blank, and the row is still written.
    pub contract: &'a str,
    /// The contract's own security id.
    pub security_id: i64,
    /// The underlying's numeric id. NOT a name â see the module header.
    pub underlying_id: i64,
    /// Units per contract, from the day's master — the DENOMINATOR of the
    /// rank key. Stored as `per_lot_quantity` since 2026-09-19 (renamed from
    /// `lot_size` on the operator's plain-names instruction).
    ///
    /// Added 2026-09-12. Sourced from the `LOT_SIZE` column of Dhan's
    /// `api-scrip-master-detailed.csv` and carried on `ContractOwner`, so
    /// it is the SAME value the ranking divided by rather than a re-lookup
    /// that could disagree with it.
    ///
    /// Guaranteed non-zero: a leg whose master row carries no lot size is
    /// refused at the map build (`LegRefusal::MissingLotSize`) and again by
    /// the lots computation, which returns `None` rather than dividing. So a
    /// zero in this column would mean the guarantee broke, and is worth
    /// seeing.
    pub per_lot_quantity: i64,
    /// **The RANK KEY, in WHOLE LOTS**: lots traded in the window that just
    /// closed.
    ///
    /// # Whole lots, never milli-lots (operator, 2026-09-19)
    ///
    /// *"See total lots traded shoudl be the long right why decimal here bro
    /// why the fuck bro why"*. Until this change the column was
    /// `window_lots_milli` and carried lots x 1000, so a reader saw `4_150`
    /// where 4.15 lots traded and `1_000` for one lot. The x1000 scale existed
    /// to keep the SORT KEY an integer while preserving sub-lot resolution;
    /// the operator has ruled that the displayed number is worth more than
    /// that resolution.
    ///
    /// ⚠ **The rounding is real, and since 2026-09-19 it is NOT recoverable
    /// from the row.** A contract that traded less than one whole lot in the
    /// window reads `0`, and two contracts that traded 1.2 and 1.8 lots both
    /// read `1`. Until the operator's 15-column narrowing the row also carried
    /// `delta_units` (the traded units) beside `per_lot_quantity`, so the
    /// exact figure was always recoverable by division; he removed that column
    /// by name, so only the denominator survives and the numerator is gone.
    /// Stated plainly rather than left for a reader to discover: this column
    /// and `volume_percentage_change` are now the only record of the window's
    /// size, both rounded, and the raw traded-unit count is not stored
    /// anywhere in this table.
    ///
    /// The COMPARATOR is unaffected: it still orders on the full-resolution
    /// integer the leaderboard computed, so two contracts that round to the
    /// same printed lot count are still ordered correctly against each other.
    pub total_lots_traded: i64,
    /// **The volume-percentage change, as a WHOLE NUMBER.**
    ///
    /// A 200-per-lot contract that traded 8,000 units in the window reads
    /// **3900** â not `3900.0`, not `39.00`, and not the `3_900_000`
    /// milli-percent the column carried until 2026-09-19.
    ///
    /// # Why the scale changed (operator, 2026-09-19)
    ///
    /// The same instruction that made `total_lots_traded` whole lots. The
    /// column was `net_volume_chg_milli_pct` and stored thousandths of a
    /// percent so that the printed number could not lose ordering information;
    /// the operator reads this table directly and has ruled that a whole
    /// number is worth more than the three hidden decimals.
    ///
    /// â  **It is a strictly-increasing transform of the lots figure, not
    /// independent information.** The two rank identically, which is why the
    /// comparator still sorts the leaderboard's own full-resolution integer â
    /// a float comparator is non-transitive on a NaN and corrupts a whole sort
    /// rather than misplacing one row. It is stored because the operator
    /// should not have to know the transform. If the two columns ever
    /// disagree, THIS one is wrong.
    ///
    /// Integer, never a `DOUBLE`: it is derived from an integer key, and a
    /// float here could round two distinct keys onto one printed value.
    pub volume_percentage_change: i64,
    /// **The candle fold's own signed volume for this window** — the exact
    /// number `candles_<tf>.volume` carries for the same
    /// `(ts, security_id, segment, feed)` — or `None` when the fold has no bar
    /// to read.
    ///
    /// # This column IS the candle's number, with no derivation (operator, 2026-09-19)
    ///
    /// Verbatim: *"just we need to sue the rpecise volume which is avialable
    /// in our candles tabels even in our top volume rigtht dude so that no
    /// extra claucltion or derivation"*. So the value is READ off the same
    /// `MultiTfAggregator` probe the candle writer seals from, never
    /// recomputed here — if the two tables ever disagree, the bug is in the
    /// probe, not in an arithmetic difference between two derivations.
    ///
    /// It is **signed gross**: negative when the bar closed below the previous
    /// bar's close of the same timeframe, positive otherwise (a flat bar and a
    /// first bar are both positive). `abs()` recovers the gross magnitude on
    /// every row, which is what makes the 10-minute view derivable from the
    /// 1-minute rows.
    ///
    /// # The two-phase rename is GONE, and that is the fresh-start dividend
    ///
    /// Until 2026-09-19 this number lived in `candle_volume_signed` while
    /// `volume` held the vendor's CUMULATIVE day counter, because QuestDB's
    /// self-heal can add a column but can neither rename nor drop one — so
    /// re-pointing the name on a live table would have given one column two
    /// meanings across a partition boundary, with nothing in the row to tell a
    /// reader which meaning theirs carried. The operator's fresh-scratch
    /// directive (*"fresh db eveyrhtign needs to ebe entirley fresh new"*)
    /// removes that constraint outright: a table with no legacy rows has no
    /// second meaning to collide with, so Phase 1 and Phase 2 collapse into
    /// one and the column is simply called what it is.
    ///
    /// Written as an ABSENT ILP field, i.e. NULL, when the fold has no bar —
    /// never a `0`, which would read as "traded nothing" when the truth is
    /// "did not trade in the window this grid measures".
    pub volume: Option<i64>,
    /// **Close vs YESTERDAY's close**, in percent — the headline day change,
    /// byte-identical to what `candles_<tf>.percentage_change` carries for the same
    /// `(ts, security_id, segment, feed)`.
    ///
    /// # Byte-identical by construction, not by coincidence (2026-09-19)
    ///
    /// Both columns are filled from the SAME `LiveCandleState` field,
    /// `close_pct_from_prev_day`, on the same bar: the candle writer reads it
    /// in `shadow_seal_columns::from_buffered_seal` (the row field keeps its
    /// old name `change_pct`; the COLUMN has been `percentage_change` since
    /// 2026-09-19), and this row reads it off the same probe that already
    /// fetches [`Self::volume`]. Nothing is re-derived here —
    /// operator 2026-09-19: *"no extra claucltion or derivation"*.
    ///
    /// `None` when the fold has no bar for this window, exactly as the
    /// candle-sourced columns beside it. A NULL here is not a defect: it means
    /// this contract did not trade inside the window the candle grid measures.
    pub percentage_change: Option<f64>,
    /// **Close vs TODAY's 09:15 session open**, in percent â byte-identical to
    /// `candles_<tf>.open_pct` for the same bar, from the same
    /// `LiveCandleState::open_pct` field.
    ///
    /// â  NOT the opening gap. That is `open_gap_pct`, a different candle
    /// column measuring the session OPEN against yesterday's close, and it is
    /// deliberately not carried here â the operator asked for "open percentage
    /// change", which is the column whose name it matches.
    ///
    /// `0.0` at the source when `session_open` is `0.0` (the fold's
    /// div-by-zero guard), so a zero here can mean "no session open recorded
    /// yet" as well as "closed exactly at the open"; `None` means no bar.
    pub open_percentage_change: Option<f64>,
    /// Whether this contract actually held a depth subscription at this
    /// snapshot. The column that makes the table an audit rather than trivia.
    ///
    /// Read from the steering loops' LAST publish, which happens once a
    /// minute per pool â so inside one minute the value can lag a swap by up
    /// to that minute, and always in the understating direction (a contract
    /// just swapped IN reads `false` until the next publish). See
    /// `depth_subscription_view` for why that direction is the honest one.
    pub subscribed: bool,
}

/// The four bands of the delay renderer, in nanoseconds.
///
/// Each band stops SHORT of the round number above it so a value never renders
/// as `1000 microseconds` when `1 millisecond` is the true reading: the
/// microsecond band ends at 999,499 ns (which rounds to 999 Âµs) and the
/// millisecond band begins at 999,500 ns (which rounds to 1 ms).
const DELAY_MICRO_FLOOR_NANOS: i64 = 1_000;
const DELAY_MILLI_FLOOR_NANOS: i64 = 999_500;
const DELAY_SECOND_FLOOR_NANOS: i64 = 999_500_000;

/// Renders a nanosecond delay as whole units of ONE band, into `out`.
///
/// # The contract (operator 2026-09-18, Â§19 Â§4)
///
/// > "if it is below microseconds I need to know it took 100 or 1000
/// > microseconds, or if it is in milliseconds then the data should be like
/// > this 100 or 200 or 123 milliseconds, or if it is in seconds then I want to
/// > see this as 1 second or 2 seconds"
///
/// Whole units, never a decimal point, and one band per value:
///
/// | nanoseconds | renders as |
/// |---|---|
/// | `< 1,000` | whole **nanoseconds** â `4 nanoseconds` |
/// | `1,000 .. 999,499` | whole **microseconds** â `100 microseconds` |
/// | `999,500 .. 999,499,999` | whole **milliseconds** â `123 milliseconds` |
/// | `>= 999,500,000` | whole **seconds** â `1 second` |
///
/// Rounding is to NEAREST within the band, and singular/plural follows the
/// number (`1 second`, `2 seconds`).
///
/// # Why this is lossy on purpose, and why the `_ns` twin exists
///
/// `1 second` covers 999,500,000 .. 1,499,999,999 ns. That loss is the
/// operator's own instruction and is confined to the SENTENCE: every delay is
/// stored twice, and the `_ns` column always carries the exact signed figure.
/// Nothing is lost from the table â only from the phrase.
///
/// **And the `_ns` twin is not a convenience.** Text sorted descending
/// compares the first character and stops, so four real delays â 1 second,
/// 2 milliseconds, 3 microseconds, 4 nanoseconds â sort to `4, 3, 2, 1`: the
/// exact REVERSE of their true order, and it looks entirely plausible. Any
/// `ORDER BY` must use the `_ns` column; the rendered string is for a human to
/// read, never for a query to sort.
///
/// # Negative delays are real and are rendered with a sign
///
/// The drain back-dates `received_at_nanos` by ring dwell
/// (`Utc::now() - frame.received_at.elapsed()`), so a tick can carry a receipt
/// instant BEFORE the window it lands in. `unsigned_abs` takes the magnitude â
/// never `abs()`, which panics on `i64::MIN` under the release profile's
/// `overflow-checks = true` â and the sign is carried as a leading `-` so the
/// sentence cannot silently claim a negative delay was positive.
///
/// # Allocation
///
/// Writes into a caller-owned buffer, which the caller CLEARS first. The naive
/// `format!` shape would be three fresh `String`s per row â at the measured
/// 20,220-contract ceiling that is 60,660 allocations per sweep, on the frame
/// drain, in a codebase whose first principle is zero allocation on the hot
/// path. One reused buffer is the whole difference.
pub fn render_delay_into(out: &mut String, nanos: i64) {
    use std::fmt::Write as _;

    out.clear();
    let magnitude = nanos.unsigned_abs();
    if nanos < 0 {
        out.push('-');
    }
    // Every band divides by its own unit with round-half-up, which is what
    // makes 999,500 ns read as `1 millisecond` rather than `0 milliseconds`.
    let (value, unit) = if magnitude < DELAY_MICRO_FLOOR_NANOS.unsigned_abs() {
        (magnitude, "nanosecond")
    } else if magnitude < DELAY_MILLI_FLOOR_NANOS.unsigned_abs() {
        ((magnitude + 500) / 1_000, "microsecond")
    } else if magnitude < DELAY_SECOND_FLOOR_NANOS.unsigned_abs() {
        ((magnitude + 500_000) / 1_000_000, "millisecond")
    } else {
        ((magnitude + 500_000_000) / 1_000_000_000, "second")
    };
    // `String`'s `fmt::Write` impl is infallible, so this Result can only ever
    // be `Ok`. It is DISCARDED rather than unwrapped because `unwrap_used` and
    // `expect_used` are both denied outside tests, and a panic path on the
    // frame drain â under `panic = "abort"` â for an error that cannot occur
    // is a worse trade than an ignored Ok.
    //
    // `_ =` rather than `let _ =`: clippy's `let_underscore_must_use` fires on
    // the second and not the first, and the destructuring-assignment form is
    // the modern idiom for a deliberate discard. No `#[allow]` is needed, so
    // none is added â the house rule requires an `// APPROVED:` beside every
    // allow, and an allow that can be avoided should be.
    _ = write!(out, "{value} {unit}");
    if value != 1 {
        out.push('s');
    }
}
/// The idempotent `CREATE TABLE` DDL for ONE per-timeframe `top_volume_<tf>`
/// table. Pure. All four tables share this exact schema and differ only in name.
///
/// # The three orphan columns this DDL deliberately no longer names
///
/// `lot_size`, `window_lots_milli` and `net_volume_chg_milli_pct` were RENAMED
/// on 2026-09-19 (`per_lot_quantity`, `total_lots_traded`,
/// `volume_percentage_change`), and `gain_pct` â the UNDERLYING's move, a
/// different instrument's number sitting in a contract's row â was DELETED on
/// the same instruction. QuestDB's self-heal can add a column but can neither
/// rename nor drop one, so an EXISTING table keeps all four as orphans that go
/// NULL from the deploy onward, while a FRESH table never grows them. Both
/// states are honest; a column that is written under one name and read under
/// another would not be.
///
/// `volume` is NAMED here and IS written: it carries the candle's signed
/// gross volume for the window (see [`TopVolumeRankRow::volume`]), the same
/// number the `candles_<tf>` row carries. The fresh-start reset collapsed the
/// old two-phase rename into one step, so there is no `candle_volume_signed`
/// and no `cumulative_day_volume` column any more.
///
/// *(CORRECTED 2026-09-22: this paragraph said `volume` was a Phase-2 target
/// that "nothing writes during Phase 1" and linked a
/// `TopVolumeRankRow::cumulative_day_volume` field that no longer exists.
/// Both described the pre-reset plan.)*
#[must_use]
pub fn top_volume_create_ddl(cadence: SnapshotCadence) -> String {
    let table = cadence.table_name();
    format!(
        "CREATE TABLE IF NOT EXISTS {table} (\
            ts            TIMESTAMP, \
            tf            SYMBOL, \
            family        SYMBOL, \
            feed          SYMBOL, \
            segment       SYMBOL, \
            contract      SYMBOL, \
            security_id   LONG, \
            underlying_id LONG, \
            volume        LONG, \
            per_lot_quantity LONG, \
            total_lots_traded LONG, \
            volume_percentage_change LONG, \
            percentage_change DOUBLE, \
            open_percentage_change DOUBLE, \
            subscribed    BOOLEAN\
        ) timestamp(ts) PARTITION BY HOUR \
        DEDUP UPSERT KEYS({DEDUP_KEY_TOP_VOLUME_RANK});"
    )
}

/// Every non-designated column, for the per-column self-heal ALTER manifest.
/// Kept beside the DDL so `table_schema_lockstep_guard` can compare them.
/// The SAME list for all four per-timeframe tables — they differ only in name.
const TOP_VOLUME_RANK_COLUMNS: &[(&str, &str)] = &[
    ("tf", "SYMBOL"),
    ("family", "SYMBOL"),
    ("feed", "SYMBOL"),
    ("segment", "SYMBOL"),
    ("contract", "SYMBOL"),
    ("security_id", "LONG"),
    ("underlying_id", "LONG"),
    ("volume", "LONG"),
    ("per_lot_quantity", "LONG"),
    ("total_lots_traded", "LONG"),
    ("volume_percentage_change", "LONG"),
    ("percentage_change", "DOUBLE"),
    ("open_percentage_change", "DOUBLE"),
    ("subscribed", "BOOLEAN"),
];

/// The `DROP VIEW` that clears a pre-2026-09-22 view of the SAME name before
/// the table is created.
///
/// A QuestDB view and a table share one namespace, and until 2026-09-22 each
/// of these four names was a VIEW over the shared `top_volume` table. A
/// `CREATE TABLE IF NOT EXISTS top_volume_1s` against a box where that name is
/// still a view does not create a table. So the view goes first.
///
/// From the second boot onward the name IS a table and no view of that name
/// exists; `IF EXISTS` then makes the statement a no-op, and should QuestDB
/// instead refuse a `DROP VIEW` that names a table, the refusal is EXPECTED —
/// it is logged at debug by [`ensure_top_volume_tables`] and never counted as
/// a failure (an error whose steady state is "once per boot, forever" trains
/// the operator to ignore the counter).
#[must_use]
pub fn top_volume_view_predrop_ddl(cadence: SnapshotCadence) -> String {
    format!("DROP VIEW IF EXISTS {};", cadence.table_name())
}

/// The full idempotent statement list for ONE per-timeframe table: CREATE,
/// then per-column `ADD COLUMN IF NOT EXISTS`, then `DEDUP ENABLE`. Never a
/// DROP (the view pre-drop is separate, see [`top_volume_view_predrop_ddl`]).
/// Pure, so the ordering is unit-testable without a live QuestDB.
#[must_use]
pub fn top_volume_ensure_statements(cadence: SnapshotCadence) -> Vec<String> {
    let table = cadence.table_name();
    let mut statements = Vec::with_capacity(TOP_VOLUME_RANK_COLUMNS.len() + 2);
    statements.push(top_volume_create_ddl(cadence));
    for (col, ty) in TOP_VOLUME_RANK_COLUMNS {
        statements.push(format!(
            "ALTER TABLE {table} ADD COLUMN IF NOT EXISTS {col} {ty};"
        ));
    }
    statements.push(format!(
        "ALTER TABLE {table} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_TOP_VOLUME_RANK});"
    ));
    statements
}

/// Creates the four per-timeframe `top_volume_<tf>` tables if absent
/// (schema-self-heal order per table: drop a same-named legacy VIEW ->
/// CREATE -> per-column ALTER -> DEDUP ENABLE; never a table drop).
///
/// Fail-SOFT per statement: every failure logs at `error!` with
/// `STORAGE-GAP-03` and the walk continues, so one refused statement never
/// hides the next, and one refused TABLE never hides the other three. The
/// honest consequence, stated rather than hidden: a failed ensure leaves that
/// table to be auto-created by the first ILP write WITHOUT `DEDUP UPSERT
/// KEYS` — a duplicate-row window until a later ensure succeeds. Blocking the
/// boot instead would trade a duplicate-row window for no session at all.
///
/// Returns `true` only when EVERY statement for EVERY table was accepted
/// (the view pre-drop excepted — its refusal is the normal steady state), so
/// the boot's bounded retry loop (`candle_ddl_boot::run_live_table_ddl_at_boot`)
/// can re-run it beside `ticks` and `market_depth`.
///
/// *(Until 2026-09-22 this was `ensure_top_volume_rank_table`, ensuring ONE
/// shared `top_volume` table and first renaming a legacy `top_volume_rank`
/// into it. The rename is gone: nothing writes the shared table any more, and
/// the fresh-start reset drops both legacy names outright.)*
// TEST-EXEMPT: live-QuestDB DDL runner; the statement lists it sends are pure and are asserted by the ensure-statement tests below, and the boot call site is pinned by crates/app/tests/ensure_ddl_boot_wiring_guard.rs.
pub async fn ensure_top_volume_tables(questdb_config: &QuestDbConfig) -> bool {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            error!(
                code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                stage = "ensure_client_build",
                ?err,
                "STORAGE-GAP-03: HTTP client build failed — top_volume_<tf> tables not \
                 ensured (the first ILP write may auto-create them WITHOUT dedup, \
                 a duplicate-row window until the next successful boot)"
            );
            return false;
        }
    };

    let mut all_accepted = true;
    for cadence in SnapshotCadence::ALL {
        // The legacy view of the same name, FIRST. Its refusal is expected
        // from the second boot onward and is never a failure.
        let predrop = top_volume_view_predrop_ddl(cadence);
        match client
            .get(&base_url)
            .query(&[("query", predrop.as_str())])
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => {
                debug!(
                    table = cadence.table_name(),
                    status = %resp.status(),
                    "top_volume: legacy view pre-drop refused — expected once the name \
                     is a table"
                );
            }
            Err(err) => {
                debug!(
                    table = cadence.table_name(),
                    ?err,
                    "top_volume: legacy view pre-drop request failed — the CREATE below \
                     reports if it matters"
                );
            }
        }

        for ddl in &top_volume_ensure_statements(cadence) {
            match client
                .get(&base_url)
                .query(&[("query", ddl.as_str())])
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {}
                Ok(resp) => {
                    all_accepted = false;
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    error!(
                        code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                        stage = "ensure_ddl",
                        table = cadence.table_name(),
                        %status,
                        ddl = ddl.as_str(),
                        body = %body.chars().take(200).collect::<String>(),
                        "STORAGE-GAP-03: top_volume DDL returned non-2xx \
                         (dedup may be missing — duplicate-row window)"
                    );
                }
                Err(err) => {
                    all_accepted = false;
                    error!(
                        code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                        stage = "ensure_ddl",
                        table = cadence.table_name(),
                        ?err,
                        ddl = ddl.as_str(),
                        "STORAGE-GAP-03: top_volume DDL request failed"
                    );
                }
            }
        }
    }
    all_accepted
}

/// ILP-over-HTTP conf â per-flush server ACK (the 2026-07-05
/// fire-and-forget lesson): `retry_timeout=0` so questdb-rs does not run its
/// own sleep-and-resend loop inside our flush, and a bounded
/// `request_timeout` so a stalled server cannot hold the caller.
fn top_volume_rank_ilp_http_conf(config: &QuestDbConfig) -> String {
    format!(
        "http::addr={}:{};protocol_version=1;retry_timeout=0;request_timeout=5000;",
        config.host, config.http_port
    )
}

/// ILP-over-HTTP writer for `top_volume_rank`.
///
/// Lazy: an unreachable QuestDB at construction still builds and buffers
/// locally. On ANY failed flush the pending buffer is DISCARDED rather than
/// retained â a server-REJECTED row kept across flushes replays forever and
/// poisons every later flush with it. Discarding is safe HERE specifically
/// because these rows are an observability record of a ranking that is
/// recomputed from scratch on the next tick: nothing downstream depends on
/// a snapshot having been written, and the loss is loud.
pub struct TopVolumeRankWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
    /// Running count of DISCARD EPISODES, for the power-of-two log throttle.
    ///
    /// Episodes, not rows: a dead writer thread produces one episode per flush
    /// forever, and it is the onset and the order of magnitude an operator
    /// needs, never 180,000 identical lines. Saturating, so a pathological
    /// session cannot panic under `overflow-checks`.
    discard_episodes: usize,
    /// Set by [`TopVolumeRankWriter::split_for_offload`]. When present, `flush`
    /// hands the buffer to the writer thread instead of touching the network.
    ///
    /// `None` means this writer still flushes synchronously â the shape that
    /// must never reach the frame drain, because a 5,000 ms `request_timeout`
    /// on the drain task stalls the fold, fills the socket receive buffer, and
    /// Dhan skips a slow consumer forward to "the latest available state" with
    /// no sequence number. The ticks lost that way are lost at THEIR side and
    /// are invisible to every counter we own. That is the 2026-08-28 depth
    /// lesson, and this field is how this table avoids repeating it.
    offload: Option<std::sync::mpsc::SyncSender<TopVolumeFlushBatch>>,
    /// Consecutive flushes the producer has retained because the queue was
    /// full. Bounded by [`MAX_TOP_VOLUME_RETAINED_FLUSH_SPANS`].
    retained_spans: u32,
}

impl TopVolumeRankWriter {
    /// Production constructor â ILP-over-HTTP sender, lazy on failure.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor; the lazy contract and every append/flush path are covered via for_test() below.
    pub fn new(config: &QuestDbConfig) -> Self {
        // Seed the discard series at zero BEFORE the first row can be
        // dropped. A counter never incremented is ABSENT from the exporter,
        // not zero â and an absent series reads as health. If this name is
        // ever EMF-selected, the CloudWatch agent's dropped-first-sample rule
        // turns that into total silence on the one episode that matters; that
        // rule cost 104,540 depth rows their classification on 2026-08-28.
        metrics::counter!("tv_top_volume_rank_rows_discarded_total").increment(0);
        // The SAME rule for every other outcome this writer can report. Only
        // `rows_discarded` was seeded until 2026-09-13; the other four were
        // registered by their own first increment, which is exactly the shape
        // the comment above warns about â and `flush_width_capped` is a
        // ROW-DROPPING arm (it calls `discard_pending`), so its series was
        // absent from the exporter until the first loss. An operator or a
        // dashboard asking "has this writer ever capped?" saw no series at
        // all, which reads identically to health.
        //
        // Seeding the SUCCESS arm (`flush_offloaded`) too is deliberate: a
        // loss counter is only legible beside the denominator it is a fraction
        // of, and a denominator that appears only after the first success is
        // the same absence problem one column over.
        metrics::counter!("tv_top_volume_rank_flush_offloaded_total").increment(0);
        metrics::counter!("tv_top_volume_rank_flush_queue_full_total").increment(0);
        metrics::counter!("tv_top_volume_rank_flush_width_capped_total").increment(0);
        metrics::counter!("tv_top_volume_rank_flush_retries_total").increment(0);
        let conf = top_volume_rank_ilp_http_conf(config);
        match Sender::from_conf(&conf) {
            Ok(s) => {
                let b = s.new_buffer();
                Self {
                    sender: Some(s),
                    buffer: b,
                    pending: 0,
                    discard_episodes: 0,
                    offload: None,
                    retained_spans: 0,
                }
            }
            Err(err) => {
                warn!(
                    ?err,
                    "top_volume_rank writer: QuestDB unreachable â buffering locally"
                );
                Self {
                    sender: None,
                    buffer: Buffer::new(ProtocolVersion::V1),
                    pending: 0,
                    discard_episodes: 0,
                    offload: None,
                    retained_spans: 0,
                }
            }
        }
    }

    /// Test constructor â disconnected writer, empty buffer.
    #[must_use]
    // TEST-EXEMPT: test-only helper used by the append/flush unit tests below.
    pub fn for_test() -> Self {
        Self {
            sender: None,
            buffer: Buffer::new(ProtocolVersion::V1),
            pending: 0,
            discard_episodes: 0,
            offload: None,
            retained_spans: 0,
        }
    }

    /// Rows appended but not yet flushed.
    #[must_use]
    // TEST-EXEMPT: observability accessor, exercised by the append tests below.
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// Test-only view of the ILP buffer bytes (shape assertions).
    #[cfg(test)]
    fn buffer_utf8(&self) -> String {
        String::from_utf8(self.buffer.as_bytes().to_vec()).unwrap_or_default()
    }

    /// Appends one ranked-contract row.
    ///
    /// A MARKER is set before the row and rewound if any step fails, so a
    /// half-written row can never poison the buffer â see [`Self::append_row`].
    ///
    /// # Errors
    /// Propagates ILP buffer errors (table/column append failure).
    fn write_row(buffer: &mut Buffer, r: &TopVolumeRankRow<'_>) -> Result<()> {
        buffer
            .table(r.cadence.table_name())
            .context("table")?
            // Symbols BEFORE columns (ILP tags-before-fields rule).
            .symbol("tf", r.cadence.as_str())
            .context("tf")?
            .symbol("family", r.family)
            .context("family")?
            .symbol("feed", r.feed)
            .context("feed")?
            .symbol("segment", r.segment)
            .context("segment")?
            // The human label, as a SYMBOL beside the other four â interned by
            // QuestDB, so ~22,000 distinct values cost a small integer per row.
            .symbol("contract", r.contract)
            .context("contract")?
            .column_i64("security_id", r.security_id)
            .context("security_id")?
            .column_i64("underlying_id", r.underlying_id)
            .context("underlying_id")?
            .column_i64("per_lot_quantity", r.per_lot_quantity)
            .context("per_lot_quantity")?
            .column_i64("total_lots_traded", r.total_lots_traded)
            .context("total_lots_traded")?
            .column_i64("volume_percentage_change", r.volume_percentage_change)
            .context("volume_percentage_change")?;
        // OPTIONAL columns, on the absent-field-is-NULL rule -- the
        // `tick_persistence` per-feed-optional precedent. The row still
        // carries 4 symbols and 7 mandatory fields, so it can never become
        // field-less however many of these are absent.
        //
        // All the candle-sourced values come from ONE
        // `MultiTfAggregator::bar_for_window` probe in the projection, so they
        // are present together or absent together: a contract with no
        // aggregator slot, or whose bar has never opened, has no fold reading
        // to report and stores NULL rather than a zero that would read as "no
        // trades" or "flat". `close_vs_prev_bar_pct` can additionally be NULL
        // on its own when the bar has no usable baseline (the session's first
        // bar of this timeframe) -- the same gate the signed volume uses to
        // decide it stays positive.
        if let Some(volume) = r.volume {
            buffer.column_i64("volume", volume).context("volume")?;
        }
        if let Some(percentage_change) = r.percentage_change {
            buffer
                .column_f64("percentage_change", percentage_change)
                .context("percentage_change")?;
        }
        if let Some(open_percentage_change) = r.open_percentage_change {
            buffer
                .column_f64("open_percentage_change", open_percentage_change)
                .context("open_percentage_change")?;
        }
        buffer
            .column_bool("subscribed", r.subscribed)
            .context("subscribed")?
            .at(TimestampNanos::new(r.snapshot_ts_ist_nanos))
            .context("designated timestamp")?;
        Ok(())
    }

    /// Appends one ranked-contract row, atomically with respect to the buffer.
    ///
    /// # Why the marker (2026-09-09)
    ///
    /// `write_row` is a chain of `?`. If ANY step fails part-way, questdb-rs
    /// has already written bytes AND advanced its own state machine: `table()`
    /// sets `op_case = TableWritten`, and `check_op(Op::Table)` then REFUSES
    /// every later `.table()` call â so one bad row silently killed this
    /// writer for the rest of the process. `at()` is the realistic trigger: it
    /// validates the timestamp only after the table, symbols and columns are
    /// already in the buffer.
    ///
    /// Nothing recovered it, either. `pending` is bumped only AFTER the last
    /// `?`, so a part-way failure leaves it at 0 â and `flush` early-returns on
    /// `pending == 0`, so the one path that calls `discard_pending` (the only
    /// code that clears the buffer) was unreachable in exactly the case that
    /// needed it.
    ///
    /// `rewind_to_marker` truncates the output back to the row boundary and
    /// restores the saved `BufferState`, so ONLY the bad row is dropped and
    /// `pending` stays truthful for the good rows already buffered this sweep.
    /// `clear()` was rejected: a sweep appends up to 250 rows per family, and
    /// clearing on row 200 would throw away 199 good ones.
    ///
    /// # Errors
    /// Propagates ILP buffer errors (table/column append failure).
    pub fn append_row(&mut self, r: &TopVolumeRankRow<'_>) -> Result<()> {
        // A marker may only be set on an empty buffer or after `at` â i.e.
        // exactly at a row boundary, which is where this always runs. If it is
        // refused the buffer is ALREADY mid-row from some path this reasoning
        // did not cover, and a total reset is the only way back.
        if self.buffer.set_marker().is_err() {
            self.discard_pending();
            self.buffer
                .set_marker()
                .context("top_volume_rank: marker refused on a cleared buffer")?;
        }
        match Self::write_row(&mut self.buffer, r) {
            Ok(()) => {
                self.buffer.clear_marker();
                self.pending = self.pending.saturating_add(1);
                Ok(())
            }
            Err(err) => {
                if self.buffer.rewind_to_marker().is_err() {
                    // Cannot happen (the marker was just set), but a silent
                    // half-row here is the exact defect this guards, so fall
                    // back to the reset rather than trusting the reasoning.
                    self.discard_pending();
                }
                Err(err)
            }
        }
    }

    /// Flushes buffered rows over ILP-HTTP (per-flush server ACK).
    ///
    /// # Errors
    /// `Err` when disconnected or the HTTP flush fails (pending discarded).
    pub fn flush(&mut self) -> Result<()> {
        if self.pending == 0 {
            return Ok(());
        }
        // OFF-DRAIN arm first: once split, the network is the sink thread's
        // problem and this call must not touch it.
        if self.offload.is_some() {
            return match self.offload_flush() {
                // Handed off, or held for the next flush â neither is an error
                // to the caller. `QueueFull` in particular is BACKPRESSURE:
                // reporting it as a failure would decay a health signal for
                // rows that are still safely held.
                TopVolumeOffloadOutcome::Sent(_) | TopVolumeOffloadOutcome::QueueFull(_) => Ok(()),
                TopVolumeOffloadOutcome::WidthCapped(dropped) => anyhow::bail!(
                    "top_volume_rank: writer thread stayed behind â {dropped} pending \
                     row(s) dropped (no spill tier for a periodic snapshot)"
                ),
                TopVolumeOffloadOutcome::SinkGone(dropped) => anyhow::bail!(
                    "top_volume_rank: writer thread is gone â {dropped} pending row(s) dropped"
                ),
            };
        }
        if self.sender.is_none() {
            let dropped = self.discard_pending();
            anyhow::bail!(
                "top_volume_rank: no ILP sender (QuestDB unreachable) â \
                 {dropped} pending row(s) discarded"
            );
        }
        let flushed = self
            .sender
            .as_mut()
            .map(|sender| sender.flush(&mut self.buffer));
        match flushed {
            Some(Ok(())) => {
                self.pending = 0;
                Ok(())
            }
            Some(Err(err)) => {
                let dropped = self.discard_pending();
                Err(anyhow::Error::new(err).context(format!(
                    "top_volume_rank ILP flush failed â {dropped} pending row(s) \
                     discarded (poisoned-buffer defense)"
                )))
            }
            // Unreachable (checked above) â treated as the no-sender arm.
            None => {
                let dropped = self.discard_pending();
                anyhow::bail!(
                    "top_volume_rank: sender vanished â {dropped} pending row(s) discarded"
                );
            }
        }
    }

    /// Splits this writer into a drain-side PRODUCER and a thread-side SINK.
    ///
    /// After this, `flush` hands the buffer to a bounded queue and returns
    /// without touching the network. That is the whole point: the caller is the
    /// frame drain, and a 5,000 ms `request_timeout` on the drain task stalls
    /// the fold â after which the socket receive buffer fills and Dhan skips a
    /// slow consumer forward with no sequence number, losing ticks at THEIR
    /// side where no counter of ours can see it.
    ///
    /// Consuming `self` makes the split a ONE-WAY DOOR at the type level: there
    /// is no way to keep a synchronous handle to the same writer and
    /// accidentally flush it on the drain later. The tick and depth writers use
    /// the same signature for the same reason.
    #[must_use]
    // TEST-EXEMPT: the split itself is exercised by every offload test below.
    pub fn split_for_offload(
        mut self,
    ) -> (
        Self,
        TopVolumeRankWriterSink,
        std::sync::mpsc::Receiver<TopVolumeFlushBatch>,
    ) {
        let (tx, rx) = std::sync::mpsc::sync_channel(TOP_VOLUME_FLUSH_QUEUE_DEPTH);
        let sink = TopVolumeRankWriterSink {
            sender: self.sender.take(),
        };
        self.offload = Some(tx);
        (self, sink, rx)
    }

    /// True once [`Self::split_for_offload`] has run and the queue is open.
    #[must_use]
    // TEST-EXEMPT: read by the offload tests below and by the lane's wiring.
    pub const fn is_offloaded(&self) -> bool {
        self.offload.is_some()
    }

    /// Hands the pending buffer to the writer thread without touching the
    /// network.
    ///
    /// Uses `try_send`, NEVER `send`: a blocking send would re-create the exact
    /// coupling the split exists to remove, one queue further out.
    fn offload_flush(&mut self) -> TopVolumeOffloadOutcome {
        let rows = self.pending;
        // Read the protocol version BEFORE the replace: a fresh buffer must
        // speak the same protocol the sender negotiated, and borrowing rules
        // will not let both happen in one expression.
        let protocol = self.buffer.protocol_version();
        let batch = TopVolumeFlushBatch {
            buffer: std::mem::replace(&mut self.buffer, Buffer::new(protocol)),
            rows,
        };
        let Some(tx) = self.offload.as_ref() else {
            // Unreachable â `flush` checks. Treated as the gone arm rather than
            // silently succeeding, because "we sent it" when nothing was sent
            // is the one report that must never be wrong.
            self.buffer = batch.buffer;
            return TopVolumeOffloadOutcome::SinkGone(rows);
        };
        match tx.try_send(batch) {
            Ok(()) => {
                self.pending = 0;
                self.retained_spans = 0;
                metrics::counter!("tv_top_volume_rank_flush_offloaded_total").increment(1);
                TopVolumeOffloadOutcome::Sent(rows)
            }
            Err(std::sync::mpsc::TrySendError::Full(returned)) => {
                // Backpressure, not loss. Put the rows BACK and keep appending
                // â the next flush retries. This arm is what makes the bounded
                // queue safe: without it a full queue would either block the
                // drain (the original defect) or drop rows (a worse one).
                metrics::counter!("tv_top_volume_rank_flush_queue_full_total").increment(1);
                let held = returned.buffer.as_bytes().len();
                self.buffer = returned.buffer;
                self.retained_spans = self.retained_spans.saturating_add(1);
                // TWO independent cuts, spans and bytes. `>` and not `>=`: the
                // constant names how many spans may be RETAINED, so the cut
                // belongs on the span AFTER them.
                if self.retained_spans > MAX_TOP_VOLUME_RETAINED_FLUSH_SPANS
                    || held >= MAX_TOP_VOLUME_PRODUCER_BUFFER_BYTES
                {
                    metrics::counter!("tv_top_volume_rank_flush_width_capped_total").increment(1);
                    self.retained_spans = 0;
                    // No spill tier for this table â see
                    // `TopVolumeOffloadOutcome::WidthCapped` for why dropping
                    // is the right call here and rescuing is not.
                    let dropped = self.discard_pending();
                    return TopVolumeOffloadOutcome::WidthCapped(dropped);
                }
                TopVolumeOffloadOutcome::QueueFull(rows)
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(returned)) => {
                self.buffer = returned.buffer;
                let dropped = self.discard_pending();
                TopVolumeOffloadOutcome::SinkGone(dropped)
            }
        }
    }

    /// Drops the pending buffer, counts and LOGS the loss, returns the count.
    /// `pub` since 2026-09-13 so the lane can account for retained rows at
    /// SHUTDOWN, the one moment nothing else calls it.
    ///
    /// On a `QueueFull` the producer keeps its rows and `flush` returns `Ok` â
    /// correct, that is backpressure, not loss. But if the process then exits
    /// while rows are still retained, no later flush ever runs, and until this
    /// was reachable from the lane those rows skipped even the discard counter:
    /// a real drop with every surface reading green. The tick path has always
    /// called its own equivalent from `close_offload_queues`; this writer had
    /// no way to be asked.
    pub fn discard_pending(&mut self) -> usize {
        let dropped = self.pending;
        if dropped > 0 {
            metrics::counter!("tv_top_volume_rank_rows_discarded_total").increment(dropped as u64);
            self.discard_episodes = self.discard_episodes.saturating_add(1);
            // The counter is NOT EMF-selected â deliberately. Shipping a new
            // metric name costs ~$0.30/mo against a September forecast of
            // $142.24 with the automatic STOP_EC2_INSTANCES line at $135.00,
            // and this table is an observability record whose loss costs no
            // tick. `loss_counter_visibility_guard` accepts the LOGGED route,
            // and this is it: the error! below sits beside the increment.
            //
            // THROTTLED to powers of two since 2026-09-13, matching both
            // app-side counterparts. Unthrottled, a DEAD writer thread makes
            // every subsequent flush take the `SinkGone` arm, which calls this
            // â four cadences at roughly two flushes a second is ~8 coded
            // `error!` lines per second for the rest of the session, order
            // 180,000 lines, into the `errors.jsonl` stream that 25 CloudWatch
            // metric filters read. The onset and the magnitude are what an
            // operator needs; the repetition is what buries them.
            //
            // Powers of two rather than a rate limit so the FIRST occurrence
            // always logs: a sampled throttle can swallow the one event that
            // opens an episode.
            if self.discard_episodes.is_power_of_two() {
                error!(
                    episodes = self.discard_episodes,
                    code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                    metric = "tv_top_volume_rank_rows_discarded_total",
                    dropped,
                    "STORAGE-GAP-03: top_volume_rank rows discarded after a \
                     failed flush â the ranking record has a hole for those \
                     snapshots. No tick is lost by this: the leaderboard is in \
                     RAM and the next snapshot rebuilds it. `episodes` is the \
                     running count for this writer, not the count since the \
                     last line: the line is throttled to powers of two, so a \
                     jump from 8 to 16 means eight silent episodes between."
                );
            }
        }
        self.buffer.clear();
        self.pending = 0;
        dropped
    }
}

// ---------------------------------------------------------------------------
// Off-drain WRITE for top_volume_rank (2026-09-07)
// ---------------------------------------------------------------------------

/// Depth of the hand-off queue between the drain and the top-volume writer.
///
/// FOUR, the same as [`crate::depth_persistence::DEPTH_FLUSH_QUEUE_DEPTH`] and
/// deliberately not tuned differently: one number is one thing to keep true,
/// and the three writer paths sharing a shape is what stops them drifting into
/// different failure semantics.
///
/// â  The rows/s figure this doc used to carry is RETIRED, not updated. It
/// read "~600 rows/s at most (500/s from the 1 s arm, ~100/s from the 5 s
/// arm)", which was 250 x 2 families x 2 cadences â every term of which the
/// 2026-09-12 change moved: the per-family cut is gone and there are four
/// cadences. There is no longer a constant to quote. The row count per sweep
/// is one per contract that TRADED in the window, bounded above by
/// `TOP_VOLUME_MAX_ROWS_PER_SWEEP` and in practice by the market.
///
/// (Until 2026-09-08 the line before that read "250 rows at 1 s and 5 at 5 s,
/// ~255 rows/s" â the 5 s figure was the depth-200 SOCKET count, not the row
/// count, and the index family was not counted at all. Three versions, three
/// wrong figures; the fourth is a derivation instead.)
///
/// What four batches buy is therefore stated as a SHAPE, not a duration: four
/// sweeps of stall absorbed before the producer starts retaining, and
/// `MAX_TOP_VOLUME_PRODUCER_BUFFER_BYTES` worth after that. The queue should
/// never fill in practice; bounded is still bounded.
pub const TOP_VOLUME_FLUSH_QUEUE_DEPTH: usize = 4;

/// Consecutive full-queue flushes the producer may RETAIN before it stops
/// widening the buffer and drops instead.
///
/// TWO, matching [`crate::depth_persistence::MAX_DEPTH_RETAINED_FLUSH_SPANS`]
/// for the same reason as the queue depth: one shape across the three writer
/// paths is what stops them drifting into different failure semantics.
///
/// â  RE-DERIVED 2026-09-12 for the cadence expansion, and the intra-doc link
/// above was BROKEN â it read `depth_persistence::MAX_TOP_VOLUME_RETAINED_FLUSH_SPANS`,
/// a path that does not exist, so the "matching" claim pointed at nothing.
///
/// The CONSTANT does not move; what moved is what it buys in WALL-CLOCK time,
/// and that is worth stating because the number is a SPAN count and reads like
/// a duration. A span is one flush attempt that found the queue full, so the
/// tolerance is `spans Ã· flush rate`, and the flush rate is the cadence count:
///
/// | | flushes in the worst second | tolerance past a full queue |
/// |---|---|---|
/// | 2 cadences (1 s, 5 s), before 2026-09-12 | 2 | ~3 s |
/// | 4 cadences (1 s, 3 s, 5 s, 1 m), now | 4 | ~2.2 s |
///
/// One flush per `snapshot_top_volume` call, NOT one per family â the flush
/// sits outside the family loop (`dhan_feed_stack::snapshot_top_volume`), so
/// the four arms coinciding at the minute mark is four attempts, not eight.
/// Verified in source 2026-09-12 rather than assumed; a hostile review of this
/// change asserted the per-family doubling and it is not there.
///
/// So the stall a healthy queue absorbs shrank from ~7 s to ~6.2 s
/// (4 queued batches + the retained spans). That is a REDUCTION and it is not
/// raised here, for two reasons stated plainly rather than waved past: the
/// failure it guards is a QuestDB stall, which this repository has on record
/// at the 2026-08-25 and 2026-09-02 scale â MINUTES, not seconds, so neither 6
/// nor 7 saves the snapshot; and what is lost when it fires is one snapshot of
/// a leaderboard that is rebuilt from RAM a second later, never a tick. Raising
/// it trades a bounded, logged, tickless hole for unbounded producer memory on
/// the drain, which is the worse side of that trade.
pub const MAX_TOP_VOLUME_RETAINED_FLUSH_SPANS: u32 = 2;

/// Byte ceiling on the buffer the producer retains while the writer is behind.
///
/// â  RE-DERIVED 2026-09-12, and the previous value was a live row-drop.
///
/// It was a flat 4 MiB, justified as "~58 seconds of backlog" at a rate of
/// "~600 rows/s of ~120 B ILP â 72 KB/s". Both halves of that came from the
/// top-250-per-family cut. With the cut removed (operator: "pick the entire
/// options contracts") a SINGLE worst-case sweep is
/// [`TOP_VOLUME_MAX_ROWS_PER_SWEEP`] rows â 6 MB â so one sweep alone breached
/// a 4 MiB ceiling. Past the ceiling this producer DROPS: there is no spill
/// tier for this table, unlike ticks and depth. The stale figure was not a
/// stale comment; it was the number the drop decision was made against.
///
/// Now DERIVED from the population rather than asserted, so removing a cut
/// cannot silently invalidate it again. Still well under the depth path's
/// 32 MiB, which is the property the sizing test pins.
///
/// â  **DERIVED since 2026-09-12 â both terms used to be bare literals.**
/// `25_000 * 2` restated the per-family cap and the family count with nothing
/// linking either to its source, so raising `MAX_TRACKED_CONTRACTS` or adding
/// a third family would have silently under-sized the drop ceiling on a table
/// with no spill tier. The per-family term now comes from
/// `tickvault_common::constants::TOP_VOLUME_PERSIST_PER_FAMILY`, which
/// `volume_leaderboard.rs` already const-asserts is at least
/// `MAX_TRACKED_CONTRACTS`.
///
/// The family count stays a named literal: `OptionFamily` lives in the `app`
/// crate, which `storage` cannot import (the dependency runs the other way).
/// Naming it is the honest middle â a third family is now a visible edit here
/// rather than an invisible factor inside an arithmetic expression.
///
/// # â  CORRECTED 2026-09-19 â 2 â 1, and it had been wrong since 2026-09-12
///
/// The operator's 2026-09-12 (later) directive took `top_volume` to STOCK
/// OPTIONS ONLY â the index board was dropped because it steered nothing and
/// poisoned the default sort (`websocket-connection-scope-lock.md`
/// Â§ "2026-09-18 (FOURTH)"). The single source of that policy,
/// `dhan_feed_stack::RANKED_OPTION_FAMILIES`, has been a ONE-element array ever
/// since, pinned by `top_volume_stock_only_guard`. This constant was not moved
/// with it.
///
/// So the sweep has been sized for **50,000** rows while the pipeline can
/// produce at most **25,000**. Wrong in the SAFE direction â the ceiling was
/// twice what it needed to be, so nothing was ever dropped by it â and wrong in
/// a way that matters anyway, because it is the DENOMINATOR every row-width
/// decision divides by. At 50,000 rows the depth comparison
/// (`the_producer_byte_ceiling_stays_tighter_than_the_depth_path`) fails at
/// roughly **671 B** per row; at the true 25,000 it fails at roughly
/// **1,342 B**. A column set needing ~725 B reads as unaffordable against the
/// first number and sits at a 46% margin against the second.
///
/// That is the whole cost of this correction being late: a design was priced
/// against a ceiling half its real size, and the argument that came out of it
/// was wrong. Recorded rather than quietly edited, because the same stale
/// factor of 2 has now been found in FOUR places â here, and in CLAUDE.md's
/// "8 sweeps â 23.6 ms â 2.4% duty", which is 4 cadences Ã 2 families and is
/// really 4 sweeps.
///
/// The comment below is unchanged and still governs: a third family is a
/// VISIBLE edit here, never an invisible factor. It is now also a visible edit
/// in `RANKED_OPTION_FAMILIES`, and the two must move together â the const
/// assert below is what makes that non-optional.
const OPTION_FAMILIES: usize = 1;
const TOP_VOLUME_MAX_ROWS_PER_SWEEP: usize =
    tickvault_common::constants::TOP_VOLUME_PERSIST_PER_FAMILY * OPTION_FAMILIES;
/// Worst-case ILP line width for one `top_volume` row, DERIVED below.
///
/// # â  CORRECTED 2026-09-12 â this was `120`, and it was wrong by ~2.2Ã
///
/// The old value was labelled "measured â¦ rounded up" and no test measured it.
/// Counted against the real `write_row` line â 4 symbols, 7 `i64` fields, an
/// optional `f64`, a bool and a 19-digit nanosecond stamp â a worst-case row is
/// **~265 B** by hand and a typical one ~233 B:
///
/// ⚠ The sketch below is the 2026-09-12 LINE SHAPE, kept as the audit
/// trail of that day's hand count. Five of its columns are gone from the table
/// (`net_volume_chg_milli_pct`, `delta_units`, `lot_size`, `window_lots_milli`,
/// `gain_pct`); the 15-column writer's real width is the 447 B measured at the
/// bottom of this chain. Read it as history, never as today's row.
///
/// ```text
///   top_volume                                       11
///   ,tf=1m,family=stock,feed=dhan,segment=NSE_FNO    ~45
///   ,contract=<UNDERLYING>-25Sep2026-123456.75-CE   ~ 48
///   security_id=â¦i, underlying_id=â¦i,                ~ 53
///   net_volume_chg_milli_pct=â¦i,                     ~ 45
///   volume=â¦i, delta_units=â¦i, lot_size=â¦i,         ~ 58
///   window_lots_milli=â¦i,                            ~33
///   gain_pct=-12.345678901234567,                    ~27
///   subscribed=t                                      13
///   <19-digit nanos>\n                                20
/// ```
///
/// The consequence was not cosmetic. `MAX_TOP_VOLUME_PRODUCER_BUFFER_BYTES` is
/// this number times the sweep size times [`TOP_VOLUME_SWEEPS_HELD`], and past
/// that ceiling this producer **DROPS** â this table has no spill tier, so a
/// dropped row is gone. At 120 B the ceiling claimed to hold two worst-case
/// sweeps while actually holding less than one.
///
/// â  **And the hand-derivation above was ITSELF short.** A first pass at this
/// correction set the constant to 300 on the strength of that ~265 count, and
/// `the_assumed_ilp_row_width_covers_a_real_worst_case_line` â written in the
/// same change â immediately failed it: a real worst-case line **measures
/// 324 B**. The count under-estimated the `i64::MAX` fields, which are 19
/// digits each across four columns.
///
/// So the original constant was wrong by **2.7Ã**, not the 2.2Ã the hand count
/// suggested, and the reusable half is that **a width is a measurement**: two
/// successive careful derivations both came in low, and only running the line
/// through `write_row` settled it. That is why the guard is a test that builds
/// the widest row this writer can emit, not another literal.
///
/// 384 rather than 324: headroom for a longer symbol value or a wider `f64`
/// repr without a third under-count, and the cost is only reserved address
/// space in a buffer that is flushed every sweep.
/// â  384 -> 448 on 2026-09-13, RE-MEASURED not re-derived. The schema change
/// that day dropped `rank` (~26 B) and added `contract` (a SYMBOL up to ~48 B)
/// and `net_volume_chg_milli_pct` (~45 B), a net widening the harness measured
/// at 324 -> 401 B (the harness measures it; the hand count came in at ~393, low
/// again, which is the third time a width derivation here has under-counted).
/// 448 keeps the same ~12% headroom the 384 carried over 324.
///
/// â  448 -> 596 on 2026-09-18, MEASURED by the same harness. The three fold
/// columns added that day (`candle_volume_signed`, `candle_bucket_skew_secs`,
/// `candle_price_chg_pct`) widened the worst-case line 401 -> 531 B, and 596
/// keeps the same ~12% headroom. **No hand count was attempted**, which is now
/// the standing rule here: three successive derivations under-counted, and the
/// harness is cheaper than any of them.
///
/// # â  The binding constraint has MOVED, and it is no longer the wedge
///
/// The old text read "500 B would be the arithmetic limit" for the wedge
/// assert. That figure was computed when `TOP_VOLUME_SWEEPS_HELD` was 2; it
/// has been 1 since 2026-09-12, so the wedge limit is now
/// `100 MB / 2 / 50,000` = **1048 B** and this column is nowhere near it.
///
/// What DOES bind is the depth comparison
/// (`the_producer_byte_ceiling_stays_tighter_than_the_depth_path`): the ceiling
/// is 50,000 x 596 = **29.8 MB** against depth's 32 MiB, an 11% margin where
/// 448 carried 33%. So the next column added here can NOT be paid for by
/// raising this constant again â roughly 671 B is where the depth assert
/// fails â and would have to come from a lower `TOP_VOLUME_MAX_ROWS_PER_SWEEP`
/// or a deliberate, argued change to that relationship. Recorded here because
/// the previous occupant of this paragraph named the wrong ceiling, and a
/// reader sizing the next change from it would have had ~450 B of imaginary
/// room.
///
/// # â  CORRECTED 2026-09-19 â both figures above divide by 50,000, and the
/// # real row count is 25,000
///
/// `OPTION_FAMILIES` went 2 â 1 today (see its own note): the index board was
/// dropped on 2026-09-12 and this factor was not moved with it. Every number in
/// the two paragraphs above is therefore computed against twice the rows this
/// pipeline can produce. Corrected:
///
/// | | stated above (50,000 rows) | true (25,000 rows) |
/// |---|---|---|
/// | wedge limit | 1,048 B | **2,097 B** |
/// | ceiling at 596 B | 29.8 MB, 11% margin | **14.9 MB, 55% margin** |
/// | where the depth assert fails | ~671 B | **~1,342 B** |
///
/// The paragraphs are left standing per house convention; this table is the
/// operative set. The sentence "the next column added here can NOT be paid for
/// by raising this constant again" was TRUE against 671 B and is FALSE against
/// 1,342 B â and that mattered: a 2026-09-19 design needing ~725 B/row was
/// priced as unaffordable against the stale ceiling before the factor was
/// checked. **A derived constant carries the staleness of every term it
/// derives from**, and this one had a stale term for a week while three
/// separate notes above it were being carefully re-measured.
///
/// â  596 -> 792 on 2026-09-19, MEASURED by the same harness, never derived.
/// The schema change that day removed `gain_pct` (~27 B) and added SEVEN
/// columns â `cumulative_day_volume`, and the six candle passthroughs
/// `percentage_change`, `open_percentage_change`, `open`, `high`, `low`,
/// `close` â on top of the three renames, and the worst-case line went
/// **531 -> 707 B**. 792 keeps the same ~12% headroom every step since
/// 2026-09-13 has carried.
///
/// This is the fourth consecutive measurement and the standing rule held: no
/// hand count was attempted. The corrected 25,000-row table below is what
/// made it affordable â at 792 B the ceiling is 25,000 x 792 = **19.8 MB**
/// against depth's 32 MiB, a **41% margin**, and the wedge limit is 2,097 B.
/// Against the stale 50,000-row figure the same width would have read 39.6 MB
/// and breached depth, which is exactly the false unaffordability the note
/// below records.
///
/// â  792 -> 1040 on 2026-09-19 (LATER the same day), MEASURED by the same
/// harness. The three delay PAIRS added six columns, and the worst-case line
/// went **707 -> 926 B**. 1040 keeps the same ~12% headroom every step since
/// 2026-09-13 has carried.
///
/// **The measurement only happened because the harness was fixed first.** Its
/// fixture left all three delays `None`, so it would have reported 707 B
/// unchanged and the ceiling would have been under-sized by 219 B per row with
/// every assert green â the exact silent row loss these constants exist to
/// prevent, arriving through the guard rather than around it. The fixture now
/// sets all three to `i64::MIN`, their widest.
///
/// At 1040 B the ceiling is 25,000 x 1040 = **26.0 MB** against depth's
/// 32 MiB, a **22.5% margin** (was 41%). That margin is real and it is
/// shrinking: two more schema additions of this size would breach the depth
/// relationship, and the next one should re-derive rather than assume.
/// ⚠ 1040 -> 748 on 2026-09-19 (LATER STILL the same day), MEASURED by the
/// same harness, and this is the first time the number has gone DOWN.
///
/// The operator stripped `top_volume` to its ranking columns that afternoon —
/// `cumulative_day_volume`, `delta_units`, `candle_bucket_skew_secs`,
/// `close_vs_prev_bar_pct` and the four OHLC passthroughs left the table, and
/// `candle_volume_signed` became `volume`. Nine columns out, and the
/// worst-case line went **926 -> 666 B**. 748 keeps the same ~12% headroom
/// every step since 2026-09-13 has carried.
///
/// **Re-derived, not left high.** A constant that is too LARGE is the safe
/// direction for row loss and the WRONG direction for the sizing relationship:
/// it makes the producer ceiling claim more of the byte budget than the rows
/// can possibly use, which is what made the six delay columns read as barely
/// affordable one revision ago. At 748 B the ceiling is 25,000 x 748 =
/// **18.7 MB** against depth's 32 MiB, a **44% margin** (was 22.5%) — room the
/// three delay pairs moving into the candle fold will hand straight back.
/// ⚠ 748 -> 502 on 2026-09-19 (LATER STILL the same day), MEASURED by the
/// same harness, and the second DECREASE in one afternoon.
///
/// The three delay pairs moved OFF this table into the candle fold, where the
/// operator asked for them. Six columns out and the worst-case line went
/// **666 -> 447 B** — the exact 219 B the pairs added when they arrived, handed
/// straight back, which is the arithmetic that says nothing else moved with
/// them. 502 keeps the same ~12% headroom every step since 2026-09-13 has
/// carried.
///
/// At 502 B the ceiling is 25,000 x 502 = **12.55 MB** against depth's 32 MiB,
/// a **63% margin** (was 44%). Re-derived rather than left at 748 for the
/// reason the previous decrease records: a constant that is too LARGE is safe
/// against row loss and WRONG for the sizing relationship — it makes this
/// producer claim byte budget the rows cannot use, which is what made the
/// delay columns read as barely affordable two revisions ago.
const TOP_VOLUME_ILP_ROW_BYTES: usize = 502;
/// Worst-case sweeps the producer may hold before it drops.
///
/// # â  2 â 1, forced by the corrected width above (2026-09-12)
///
/// This was `2`, and at the wrong 120 B width the ceiling it produced â
/// 12 MB â could not hold even **one** worst-case sweep (50,000 Ã 324 B =
/// 16.2 MB). So the declared "two sweeps" was never what shipped; the real
/// behaviour was ~0.74 of one, and the vacuous assert could not say so.
///
/// At the corrected width a true two sweeps is 32.4 MB, which collides with
/// the sizing relationship `the_producer_byte_ceiling_stays_tighter_than_the_depth_path`
/// pins against depth's 32 MiB. One sweep at 384 B is **19.2 MB** â still 60%
/// MORE capacity than shipped today, comfortably under depth, and it satisfies
/// the const-assert's actual requirement: hold at least one worst-case sweep.
///
/// Both cuts stay live, which is the point of having two. Under a few HUGE
/// sweeps the byte cut fires first; under many SMALL ones (a typical ~2,000
/// traded contracts is 0.65 MB, so three retained spans is ~1.9 MB) the byte
/// ceiling is nowhere near and [`MAX_TOP_VOLUME_RETAINED_FLUSH_SPANS`] is what
/// fires. Lowering this to 1 does not make the spans cut unreachable.
const TOP_VOLUME_SWEEPS_HELD: usize = 1;
pub const MAX_TOP_VOLUME_PRODUCER_BUFFER_BYTES: usize =
    TOP_VOLUME_MAX_ROWS_PER_SWEEP * TOP_VOLUME_ILP_ROW_BYTES * TOP_VOLUME_SWEEPS_HELD;

// â  THE ASSERT THAT USED TO STAND HERE WAS VACUOUS, and that is why the wrong
// constant above survived.
//
// It read `MAX_TOP_VOLUME_PRODUCER_BUFFER_BYTES >= ROWS * BYTES`, where the
// left-hand side is defined as `ROWS * BYTES * 2`. So it asserted `2X >= X` â
// TRUE FOR EVERY VALUE OF `BYTES`, including a value off by a factor of ten. Its
// message named a real hazard ("a single sweep drops rows â¦ a dropped row is
// gone") and it could not detect that hazard, which is worse than no assert: it
// reads, in review, as though the hazard were checked.
//
// This is the third self-satisfying guard this repository has recorded (after a
// saturated ceiling and an unreachable counter). The rule it costs: an assert
// whose two sides are built from the same term proves arithmetic, not a fact.
// Both replacements below compare against something INDEPENDENT.

// (1) The wedge guard both sibling writers carry and this one did not.
//     `tick_persistence.rs` and `depth_persistence.rs` each assert the producer
//     ceiling sits at or below half the questdb-rs `max_buf_size`; a grep for
//     the same shape here returned nothing. Independent term: the vendor limit.
const _: () = assert!(
    MAX_TOP_VOLUME_PRODUCER_BUFFER_BYTES * 2 <= crate::tick_persistence::QUESTDB_MAX_BUF_SIZE_BYTES,
    "the top_volume producer ceiling must sit at or below half the questdb-rs \
     max_buf_size wedge, matching the tick and depth writers"
);

// (2) The width itself, against the hand-derived worst case above. Independent
//     term: a literal that changes only when someone re-derives the line.
const _: () = assert!(
    TOP_VOLUME_ILP_ROW_BYTES >= MEASURED_WORST_CASE_ILP_ROW_BYTES,
    "the assumed ILP row width must cover the MEASURED worst-case line (see \
     the_assumed_ilp_row_width_covers_a_real_worst_case_line). Below it, one \
     sweep overruns the producer ceiling and this table â which has NO spill \
     tier â drops rows."
);

/// The worst-case ILP line width MEASURED by
/// `the_assumed_ilp_row_width_covers_a_real_worst_case_line`.
///
/// Separate from [`TOP_VOLUME_ILP_ROW_BYTES`] on purpose: that one is the
/// ASSUMPTION the ceiling is sized from, this one is the OBSERVATION it must
/// cover. Two independent terms are what make the asserts below capable of
/// failing â the vacuous assert this replaced compared the ceiling to a factor
/// of itself.
/// â  531 -> 707 on 2026-09-19. Not a re-derivation: the harness built the
/// widest line the new 23-column `write_row` can emit â every integer at its
/// column's extreme and a full-width signed `f64` in all seven optional
/// DOUBLEs â and reported 707 B. Update this by running the test and reading
/// its message, never by counting.
/// â  707 -> 926 on 2026-09-19 (LATER the same day). The three delay pairs
/// added six columns to `write_row`, and the widest line the 29-column writer
/// can emit is 926 B â every integer at its column's extreme, a full-width
/// signed `f64` in all seven optional DOUBLEs, and all three delays at
/// `i64::MIN` (a 20-character exact column beside a 19-character
/// `-9223372037 seconds` twin). Update this by running the test and reading
/// its message, never by counting.
/// ⚠ 926 -> 666 on 2026-09-19 (LATER STILL the same day), and this is the
/// first DECREASE this constant has ever recorded. The operator stripped nine
/// columns off `top_volume` that afternoon, so the widest line the 21-column
/// writer can emit is 666 B. Read from the test's own message, never counted —
/// the same rule that has governed every rise.
/// ⚠ 666 -> 447 on 2026-09-19 (LATER STILL the same day), the second
/// DECREASE in one afternoon. The three delay pairs left this table for the
/// candle fold, so the widest line the 15-column writer can emit is 447 B.
/// Read from the test's own message — the constant was floored to 1, the test
/// run, and the panic named the width — never counted. The same rule that has
/// governed every rise.
/// ⚠ 447 -> 450 on 2026-09-22. The row is unchanged; the TABLE NAME is
/// what grew: every line now opens with `top_volume_1m` (the widest of the four
/// per-cadence tables) instead of `top_volume`, three bytes longer. Read from
/// the test's own message. The assumed 502 still covers it (11.6% headroom),
/// so the producer ceiling does not move.
const MEASURED_WORST_CASE_ILP_ROW_BYTES: usize = 450;

// (3) The requirement the ORIGINAL assert's message named and its arithmetic
//     could not check: the ceiling must hold at least one worst-case sweep at
//     the MEASURED width, not at the assumed one.
const _: () = assert!(
    MAX_TOP_VOLUME_PRODUCER_BUFFER_BYTES
        >= TOP_VOLUME_MAX_ROWS_PER_SWEEP * MEASURED_WORST_CASE_ILP_ROW_BYTES,
    "the producer ceiling must hold at least ONE worst-case sweep. Below that, \
     a single sweep drops rows even with a perfectly healthy writer â and this \
     table has no spill tier, so a dropped row is gone."
);

/// One ILP payload on its way from the drain to the writer thread.
pub struct TopVolumeFlushBatch {
    buffer: Buffer,
    rows: usize,
}

impl TopVolumeFlushBatch {
    /// Rows this batch covers.
    #[must_use]
    // TEST-EXEMPT: accessor, exercised by the offload tests below.
    pub const fn rows(&self) -> usize {
        self.rows
    }
}

/// What happened to a batch the producer tried to hand off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopVolumeOffloadOutcome {
    /// Handed to the writer thread. The rows are no longer the drain's.
    Sent(usize),
    /// The writer is behind. Rows RETAINED by the producer, nothing lost.
    QueueFull(usize),
    /// The writer stayed behind long enough that retaining further would breach
    /// [`MAX_TOP_VOLUME_RETAINED_FLUSH_SPANS`] or
    /// [`MAX_TOP_VOLUME_PRODUCER_BUFFER_BYTES`]. Rows DROPPED.
    ///
    /// # Why dropped and not rescued â the one place this differs from depth
    ///
    /// The depth and tick paths rescue an over-wide payload to a spill tier,
    /// durable and re-ingestable. **There is no spill tier for this table, and
    /// building one would be the wrong trade.** A tick is a unique event that
    /// exists nowhere else; a top-volume snapshot is a PERIODIC SAMPLE of a
    /// leaderboard that lives in RAM and is recomputed from scratch a second
    /// later. Dropping one thins the record; it cannot corrupt it, and it
    /// cannot lose a tick.
    ///
    /// That is a real loss of an observability record and it is never silent:
    /// `discard_pending` counts it and logs a coded error naming the hole.
    WidthCapped(usize),
    /// The writer thread is gone. Rows DROPPED, counted and logged.
    SinkGone(usize),
}

/// The network half of a split [`TopVolumeRankWriter`] â owns the ILP `Sender`.
///
/// Lives on its own OS thread. It never touches the aggregator, the ring, the
/// leaderboard or anything the drain owns, which is the entire reason the split
/// exists: a five-second ILP timeout now blocks a thread whose only job is
/// waiting, instead of the task that empties the socket.
pub struct TopVolumeRankWriterSink {
    sender: Option<Sender>,
}

impl TopVolumeRankWriterSink {
    /// Writes one batch. Returns the rows that actually LANDED in QuestDB.
    ///
    /// Zero on any failure â the same contract [`TopVolumeRankWriter::flush`]
    /// has, and for the same reason: a failed write must be reported as a
    /// failure rather than forged into a success.
    ///
    /// # The bounded retry, and why it is here from the start
    ///
    /// `depth_persistence` records the cost of NOT doing this: its
    /// synchronous path carried a single retry from 2026-08-25, the offload
    /// shipped three days later without it, and the measured price on
    /// 2026-09-03 was **41,204 coded flush failures in one session**, every one
    /// reading `Connection reset by peer (os error 104)` â a transport class
    /// the buffer survives intact. Shipping this sink without the retry would
    /// repeat that mistake knowingly.
    ///
    /// Retrying is idempotent BY CONSTRUCTION, which is what makes one attempt
    /// safe rather than merely cheap: [`DEDUP_KEY_TOP_VOLUME_RANK`] is
    /// `ts, tf, family, feed, security_id, segment`, and every one of those is
    /// fixed for a given snapshot â so a first POST that landed before the
    /// reset is collapsed by the same UPSERT that makes a manual re-ingest
    /// safe to repeat.
    ///
    /// Bounded at ONE and gated on the first failure being FAST, mirroring the
    /// depth arm: `request_timeout` is 5,000 ms, so an unconditional retry
    /// makes the worst case ten seconds. A reset returns in microseconds; a
    /// timeout consumes the whole budget. The window separates them with three
    /// orders of magnitude to spare.
    pub fn write(&mut self, batch: &mut TopVolumeFlushBatch) -> usize {
        if batch.rows == 0 {
            return 0;
        }
        let Some(sender) = self.sender.as_mut() else {
            Self::report_lost(batch, "no ILP sender (QuestDB unreachable)");
            return 0;
        };
        let started = std::time::Instant::now();
        let first = sender.flush(&mut batch.buffer);
        let first_elapsed = started.elapsed();
        let outcome = match first {
            Ok(()) => Ok(()),
            Err(err)
                if crate::depth_persistence::flush_failure_is_retryable(&err)
                    && first_elapsed
                        < crate::depth_persistence::DEPTH_FLUSH_RETRY_FAST_FAILURE_WINDOW =>
            {
                metrics::counter!("tv_top_volume_rank_flush_retries_total").increment(1);
                sender.flush(&mut batch.buffer)
            }
            Err(err) => Err(err),
        };
        match outcome {
            Ok(()) => {
                let landed = batch.rows;
                batch.rows = 0;
                landed
            }
            Err(err) => {
                let why = format!("{err}"); // APPROVED: loss arm after a flush already failed, never per row
                Self::report_lost(batch, &why);
                0
            }
        }
    }

    /// Counts and LOGS a batch the network refused, through the same counter
    /// and the same coded error the synchronous `discard_pending` uses â so an
    /// operator sees no difference between a synchronous and an offloaded loss.
    fn report_lost(batch: &mut TopVolumeFlushBatch, why: &str) {
        let dropped = batch.rows;
        if dropped == 0 {
            return;
        }
        metrics::counter!("tv_top_volume_rank_rows_discarded_total").increment(dropped as u64);
        error!(
            code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
            metric = "tv_top_volume_rank_rows_discarded_total",
            dropped,
            why,
            "STORAGE-GAP-03: top_volume_rank rows discarded by the offload \
             writer â the ranking record has a hole for those snapshots. No \
             tick is lost by this: the leaderboard is in RAM and the next \
             snapshot rebuilds it."
        );
        batch.buffer.clear();
        batch.rows = 0;
    }
}

// ---------------------------------------------------------------------------
// Off-drain ROW hand-off for top_volume (2026-09-22, item 44e)
// ---------------------------------------------------------------------------
//
// Until 2026-09-22 the drain moved the network off its task but still did the
// per-row ILP APPEND itself: float formatting, symbol escaping and the delay
// renderer for every row, measured at 14,932 µs for a 20,220-row sweep --
// 14.5x the sort it followed. The split below moves the append too. The drain
// now copies plain row DATA into a pre-sized, recycled `Vec` and hands it over
// with one `try_send`; the writer thread resolves each contract label, appends
// the ILP line and flushes. Nothing on the drain formats a byte.

/// Depth of the row hand-off queue: every cadence plus one.
///
/// All four cadences (1s/3s/5s/1m) can hand off in the SAME drain tick at the
/// top of a minute. A depth equal to the cadence count (the first version,
/// which borrowed the flush queue's four) dropped the last sweep of that
/// tick whenever even one earlier batch was still queued — usually the 1m
/// sweep (2026-09-22 hostile review). One spare slot absorbs that tick.
pub const TOP_VOLUME_ROW_QUEUE_DEPTH: usize = SnapshotCadence::ALL.len() + 1;

const _: () = assert!(
    TOP_VOLUME_ROW_QUEUE_DEPTH > SnapshotCadence::ALL.len(),
    "one drain tick hands off every cadence at once; the queue must hold them all"
);

/// Spare row vectors the writer thread returns for reuse. Queue depth plus the
/// one in flight plus the one being filled: enough that the steady state never
/// allocates, bounded so a stalled drain cannot pile vectors up.
const TOP_VOLUME_ROW_RECYCLE_DEPTH: usize = TOP_VOLUME_ROW_QUEUE_DEPTH + 2;

/// The day's contract labels, keyed exactly as the app's label snapshot is:
/// `(security_id, segment)` per I-P1-11. A type ALIAS rather than a new type,
/// so the drain hands over the SAME `Arc` its projection just read and the
/// writer thread resolves every row against the identical snapshot.
pub type TopVolumeLabelMap = std::collections::HashMap<(u64, ExchangeSegment), std::sync::Arc<str>>;

/// A snapshot row staged on the drain: every column EXCEPT the contract label,
/// which the writer thread resolves from the batch's label snapshot.
///
/// A newtype over a row whose label is empty, never the row itself, so an
/// unresolved row cannot be appended by mistake: the only way back to an
/// appendable [`TopVolumeRankRow`] is [`TopVolumeStagedRow::with_contract`].
#[derive(Clone, Debug, PartialEq)]
pub struct TopVolumeStagedRow {
    row: TopVolumeRankRow<'static>,
}

impl TopVolumeStagedRow {
    /// Copies a projected row's data. O(1), no allocation: every field is a
    /// scalar or a `&'static str`, and the borrowed label is dropped here.
    ///
    /// Written field by field, without `..`, so a column added to the row
    /// fails to compile here instead of being silently lost on the way to
    /// the writer thread.
    #[must_use]
    pub fn from_row(r: &TopVolumeRankRow<'_>) -> Self {
        Self {
            row: TopVolumeRankRow {
                snapshot_ts_ist_nanos: r.snapshot_ts_ist_nanos,
                cadence: r.cadence,
                family: r.family,
                feed: r.feed,
                segment: r.segment,
                contract: "",
                security_id: r.security_id,
                underlying_id: r.underlying_id,
                per_lot_quantity: r.per_lot_quantity,
                total_lots_traded: r.total_lots_traded,
                volume_percentage_change: r.volume_percentage_change,
                volume: r.volume,
                percentage_change: r.percentage_change,
                open_percentage_change: r.open_percentage_change,
                subscribed: r.subscribed,
            },
        }
    }

    /// The appendable row, carrying `contract` as its label.
    #[must_use]
    pub fn with_contract<'b>(&self, contract: &'b str) -> TopVolumeRankRow<'b> {
        let r = &self.row;
        TopVolumeRankRow {
            snapshot_ts_ist_nanos: r.snapshot_ts_ist_nanos,
            cadence: r.cadence,
            family: r.family,
            feed: r.feed,
            segment: r.segment,
            contract,
            security_id: r.security_id,
            underlying_id: r.underlying_id,
            per_lot_quantity: r.per_lot_quantity,
            total_lots_traded: r.total_lots_traded,
            volume_percentage_change: r.volume_percentage_change,
            volume: r.volume,
            percentage_change: r.percentage_change,
            open_percentage_change: r.open_percentage_change,
            subscribed: r.subscribed,
        }
    }

    /// The label-snapshot key, rebuilt from the row's own id and segment
    /// symbol. `None` for a negative id or a segment symbol no
    /// [`ExchangeSegment`] renders -- neither is reachable from a projected
    /// row, and both resolve to the unlabelled fallback rather than a guess.
    fn label_key(&self) -> Option<(u64, ExchangeSegment)> {
        let id = u64::try_from(self.row.security_id).ok()?;
        Some((id, segment_from_symbol(self.row.segment)?))
    }
}

/// The [`ExchangeSegment`] whose `as_str()` is `symbol`. Bounded scan of the
/// wire codes 0..=8 (the enum has eight variants and a gap at 6), so O(1).
fn segment_from_symbol(symbol: &str) -> Option<ExchangeSegment> {
    (0_u8..=8)
        .filter_map(ExchangeSegment::from_byte)
        .find(|seg| seg.as_str() == symbol)
}

/// The label for one staged row: the snapshot's entry, or `unlabelled`.
fn resolve_label<'m>(
    labels: &'m TopVolumeLabelMap,
    staged: &TopVolumeStagedRow,
    unlabelled: &'m str,
) -> &'m str {
    staged
        .label_key()
        .and_then(|key| labels.get(&key))
        .map_or(unlabelled, AsRef::as_ref)
}

/// One sweep's staged rows on their way to the writer thread.
pub struct TopVolumeRowBatch {
    labels: std::sync::Arc<TopVolumeLabelMap>,
    rows: Vec<TopVolumeStagedRow>,
}

impl TopVolumeRowBatch {
    /// Rows this batch carries.
    #[must_use]
    // TEST-EXEMPT: accessor, exercised by the row hand-off tests below.
    pub fn rows(&self) -> usize {
        self.rows.len()
    }
}

/// What happened to a sweep the drain tried to hand off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopVolumeHandoff {
    /// Nothing was staged; nothing was sent.
    Empty,
    /// Handed to the writer thread.
    Sent(usize),
    /// The queue was full. Rows DROPPED, counted and logged -- never retained,
    /// never waited for (a snapshot is a periodic sample, not a unique event).
    QueueFull(usize),
    /// The writer thread is gone. Rows DROPPED, counted and logged.
    SinkGone(usize),
}

/// The drain half of the row hand-off. Holds no ILP buffer and no `Sender`.
pub struct TopVolumeRowProducer {
    tx: std::sync::mpsc::SyncSender<TopVolumeRowBatch>,
    spares: std::sync::mpsc::Receiver<Vec<TopVolumeStagedRow>>,
    rows: Vec<TopVolumeStagedRow>,
    discard_episodes: usize,
}

impl TopVolumeRowProducer {
    fn with_channels(
        tx: std::sync::mpsc::SyncSender<TopVolumeRowBatch>,
        spares: std::sync::mpsc::Receiver<Vec<TopVolumeStagedRow>>,
    ) -> Self {
        metrics::counter!("tv_top_volume_rank_rows_discarded_total").increment(0);
        metrics::counter!("tv_top_volume_rank_flush_offloaded_total").increment(0);
        metrics::counter!("tv_top_volume_rank_flush_queue_full_total").increment(0);
        Self {
            tx,
            spares,
            rows: Vec::with_capacity(TOP_VOLUME_MAX_ROWS_PER_SWEEP),
            discard_episodes: 0,
        }
    }

    /// Test producer whose queue is open, with the receiving end returned.
    #[must_use]
    // TEST-EXEMPT: test-only constructor for the app crate's snapshot tests.
    pub fn for_test() -> (Self, std::sync::mpsc::Receiver<TopVolumeRowBatch>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(TOP_VOLUME_ROW_QUEUE_DEPTH);
        let (_spare_tx, spare_rx) = std::sync::mpsc::sync_channel(TOP_VOLUME_ROW_RECYCLE_DEPTH);
        (Self::with_channels(tx, spare_rx), rx)
    }

    /// Stages one projected row. O(1), a copy into a pre-sized vector.
    ///
    /// Refuses -- and counts the refusal as a discard -- past
    /// `TOP_VOLUME_MAX_ROWS_PER_SWEEP` rows, so the vector never grows past
    /// its pre-sized capacity and the drain never reallocates.
    pub fn stage(&mut self, row: &TopVolumeRankRow<'_>) -> bool {
        if self.rows.len() >= TOP_VOLUME_MAX_ROWS_PER_SWEEP {
            self.count_discard(1, "the sweep exceeded its row bound");
            return false;
        }
        self.rows.push(TopVolumeStagedRow::from_row(row));
        true
    }

    /// Rows staged and not yet handed off.
    #[must_use]
    // TEST-EXEMPT: observability accessor, exercised by the hand-off tests below.
    pub fn pending(&self) -> usize {
        self.rows.len()
    }

    /// Hands the staged rows to the writer thread with ONE `try_send`.
    ///
    /// Never blocks: a blocking send would re-create the drain coupling the
    /// split exists to remove. A full queue DROPS the sweep and counts it on
    /// the discard series -- it does not retain it for a later sweep, because
    /// the next sweep re-ranks the whole board anyway.
    pub fn hand_off(&mut self, labels: std::sync::Arc<TopVolumeLabelMap>) -> TopVolumeHandoff {
        let rows = self.rows.len();
        if rows == 0 {
            return TopVolumeHandoff::Empty;
        }
        let batch = TopVolumeRowBatch {
            labels,
            rows: std::mem::take(&mut self.rows),
        };
        match self.tx.try_send(batch) {
            Ok(()) => {
                metrics::counter!("tv_top_volume_rank_flush_offloaded_total").increment(1);
                // Steady state: a vector the writer returned. Only when none
                // has come back yet (the first sweeps, or a writer that is
                // behind) is a fresh one pre-sized -- once per sweep at most,
                // never per row.
                self.rows = self
                    .spares
                    .try_recv()
                    .unwrap_or_else(|_| Vec::with_capacity(TOP_VOLUME_MAX_ROWS_PER_SWEEP));
                TopVolumeHandoff::Sent(rows)
            }
            Err(std::sync::mpsc::TrySendError::Full(returned)) => {
                metrics::counter!("tv_top_volume_rank_flush_queue_full_total").increment(1);
                self.reclaim(returned);
                self.count_discard(rows, "the writer queue was full");
                TopVolumeHandoff::QueueFull(rows)
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(returned)) => {
                self.reclaim(returned);
                self.count_discard(rows, "the writer thread is gone");
                TopVolumeHandoff::SinkGone(rows)
            }
        }
    }

    /// Drops staged rows that will never be handed off, counts and logs them.
    /// Called at shutdown, the one moment nothing else hands them off.
    pub fn discard_pending(&mut self) -> usize {
        let dropped = self.rows.len();
        self.rows.clear();
        self.count_discard(dropped, "the lane shut down with rows staged");
        dropped
    }

    /// Takes the vector back so its capacity is reused, then empties it.
    fn reclaim(&mut self, returned: TopVolumeRowBatch) {
        let mut rows = returned.rows;
        rows.clear();
        self.rows = rows;
    }

    fn count_discard(&mut self, dropped: usize, why: &'static str) {
        if dropped == 0 {
            return;
        }
        metrics::counter!("tv_top_volume_rank_rows_discarded_total").increment(dropped as u64);
        self.discard_episodes = self.discard_episodes.saturating_add(1);
        // Powers of two, as in `discard_pending` above: a dead writer thread
        // turns every sweep into an episode, and the onset and the magnitude
        // are what an operator needs, not thousands of identical lines.
        if self.discard_episodes.is_power_of_two() {
            error!(
                episodes = self.discard_episodes,
                code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                metric = "tv_top_volume_rank_rows_discarded_total",
                dropped,
                why,
                "STORAGE-GAP-03: top_volume_rank rows discarded before reaching \
                 the writer thread -- the ranking record has a hole for those \
                 snapshots. No tick is lost by this: the leaderboard is in RAM \
                 and the next snapshot rebuilds it."
            );
        }
    }
}

/// What the writer thread did with one batch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TopVolumeBatchOutcome {
    /// Rows the batch carried.
    pub staged: usize,
    /// Rows appended to the ILP buffer.
    pub appended: usize,
    /// Rows the ILP buffer refused at append time.
    pub append_failed: usize,
    /// Rows that LANDED in QuestDB (zero on any flush failure).
    pub landed: usize,
}

/// The thread half of the row hand-off: formats ILP lines and flushes them.
pub struct TopVolumeRowWriter {
    formatter: TopVolumeRankWriter,
    sink: TopVolumeRankWriterSink,
    spares: std::sync::mpsc::SyncSender<Vec<TopVolumeStagedRow>>,
    unlabelled: &'static str,
}

impl TopVolumeRowWriter {
    /// Appends and flushes one batch, then returns its vector for reuse.
    ///
    /// Runs on the writer thread, never the drain: this is where the per-row
    /// ILP formatting the drain used to pay now happens.
    pub fn write_batch(&mut self, batch: TopVolumeRowBatch) -> TopVolumeBatchOutcome {
        let TopVolumeRowBatch { labels, mut rows } = batch;
        let mut out = TopVolumeBatchOutcome {
            staged: rows.len(),
            ..TopVolumeBatchOutcome::default()
        };
        for staged in &rows {
            let contract = resolve_label(&labels, staged, self.unlabelled);
            if self
                .formatter
                .append_row(&staged.with_contract(contract))
                .is_ok()
            {
                out.appended = out.appended.saturating_add(1);
            } else {
                out.append_failed = out.append_failed.saturating_add(1);
            }
        }
        if self.formatter.pending > 0 {
            let protocol = self.formatter.buffer.protocol_version();
            let mut flush = TopVolumeFlushBatch {
                buffer: std::mem::replace(&mut self.formatter.buffer, Buffer::new(protocol)),
                rows: self.formatter.pending,
            };
            self.formatter.pending = 0;
            out.landed = self.sink.write(&mut flush);
            // The ILP buffer is recycled too: cleared, its capacity kept.
            flush.buffer.clear();
            self.formatter.buffer = flush.buffer;
        }
        rows.clear();
        // Full or disconnected: the vector is simply dropped. Recycling is an
        // optimisation, never a correctness path.
        drop(self.spares.try_send(rows));
        out
    }
}

impl TopVolumeRankWriter {
    /// Splits this writer into a drain-side ROW producer and a thread-side
    /// row writer. Supersedes [`Self::split_for_offload`] on the lane: that
    /// split moved the network off the drain, this one moves the per-row ILP
    /// append off it as well.
    ///
    /// `unlabelled` is the label written when a row's contract has no entry in
    /// the batch's label snapshot -- the same fallback the projection counts
    /// as `label_unavailable`.
    #[must_use]
    pub fn split_rows_for_offload(
        mut self,
        unlabelled: &'static str,
    ) -> (
        TopVolumeRowProducer,
        TopVolumeRowWriter,
        std::sync::mpsc::Receiver<TopVolumeRowBatch>,
    ) {
        let (tx, rx) = std::sync::mpsc::sync_channel(TOP_VOLUME_ROW_QUEUE_DEPTH);
        let (spare_tx, spare_rx) = std::sync::mpsc::sync_channel(TOP_VOLUME_ROW_RECYCLE_DEPTH);
        let sink = TopVolumeRankWriterSink {
            sender: self.sender.take(),
        };
        self.offload = None;
        let writer = TopVolumeRowWriter {
            formatter: self,
            sink,
            spares: spare_tx,
            unlabelled,
        };
        (
            TopVolumeRowProducer::with_channels(tx, spare_rx),
            writer,
            rx,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_cadence_is_in_the_all_list() {
        // `ALL.len()` sizes the ranking's per-contract baselines, one slot per
        // cadence. A variant missing from this list under-counts that array,
        // and two cadences would then share a baseline â each board reporting
        // a window neither one measured, with nothing erroring.
        //
        // â  The 2026-09-06 version of this test could not detect that. It
        // iterated `ALL` and matched on the label with a `_ => panic!` arm â
        // but a variant absent from `ALL` is never YIELDED by that loop, so
        // the arm it was supposed to trip could not be reached. It asserted
        // against itself and passed vacuously.
        //
        // What actually bites, in three parts:
        //   1. `from_index` is an exhaustive-by-construction decode â the
        //      slot a new variant takes must be spelled there or it decodes
        //      to `None` below;
        //   2. every slot `0..ALL.len()` must round-trip through
        //      `from_index` â `index`, which catches a gap or a duplicate;
        //   3. `slot()` must be `Some` for every listed cadence and the
        //      first UNLISTED slot must decode to `None`, which is the
        //      boundary a fifth variant crosses.
        for (expected_slot, cadence) in SnapshotCadence::ALL.iter().enumerate() {
            assert_eq!(
                cadence.index(),
                expected_slot,
                "{} sits at ALL[{expected_slot}] but its discriminant is {}",
                cadence.as_str(),
                cadence.index()
            );
            assert_eq!(
                SnapshotCadence::from_index(expected_slot),
                Some(*cadence),
                "slot {expected_slot} must decode back to {}",
                cadence.as_str()
            );
            assert_eq!(
                cadence.slot(),
                Some(expected_slot),
                "{} must be in bounds of the baseline array",
                cadence.as_str()
            );
        }
        assert_eq!(
            SnapshotCadence::from_index(SnapshotCadence::ALL.len()),
            None,
            "the slot one past the end must not decode â if it does, a \
             variant exists that ALL does not list, and every array sized \
             from ALL.len() is one slot short"
        );

        // Labels are distinct: two cadences sharing a `tf` SYMBOL would
        // collide in the DEDUP key and silently overwrite each other.
        let mut labels: Vec<&str> = SnapshotCadence::ALL.iter().map(|c| c.as_str()).collect();
        labels.sort_unstable();
        let distinct = labels.len();
        labels.dedup();
        assert_eq!(
            labels.len(),
            distinct,
            "two cadences share a wire label; the DEDUP key is \
             (ts, tf, family, feed, security_id, segment), so their rows \
             would overwrite one another"
        );
    }

    /// The traded-unit count behind [`row`]'s figures.
    ///
    /// It is a CONSTANT here rather than a field because the operator removed
    /// the traded-unit column from `top_volume` on 2026-09-19. The row still
    /// has to be internally consistent -- 8,500 units on a 200-unit lot is
    /// 42.5 lots, so 42 whole lots and +4150% -- and this is where the
    /// numerator that makes that arithmetic checkable now lives.
    const FIXTURE_TRADED_UNITS: i64 = 8_500;

    fn row() -> TopVolumeRankRow<'static> {
        TopVolumeRankRow {
            snapshot_ts_ist_nanos: 1_757_000_000_000_000_000,
            cadence: SnapshotCadence::FiveSecond,
            family: "stock_option",
            feed: "dhan",
            segment: "NSE_FNO",
            contract: "RELIANCE-25Sep2026-1400-CE",
            security_id: 44_321,
            underlying_id: 2885,
            // 8,500 units traded on a 200-unit lot is 42.5 lots, so
            // `total_lots_traded` is 42 (whole lots) and the percentage is
            // +4150%. The unit count itself is no longer a column -- the
            // operator removed it on 2026-09-19 -- so the fixture keeps the
            // arithmetic in this comment rather than in a field, and the two
            // stored figures still have to agree with each other.
            per_lot_quantity: 200,
            total_lots_traded: 42,
            // 42,500 milli-lots -> 42_500 * 100 - 100_000 = 4,150,000
            // milli-percent -> 4150 whole percent. Deliberately NOT
            // `total_lots_traded * 100 - 100` (which would be 4100): the
            // percentage keeps the fractional lot the whole count truncates,
            // and a fixture that agreed with the truncated derivation would
            // pass on a projection that had silently switched to it.
            volume_percentage_change: 4_150,
            // The fold's own reading of the SAME bar, copied by the
            // projection. NEGATIVE deliberately: the candles table's volume is
            // signed, and a fixture that only ever carried a positive one
            // could not tell a lost sign from a working one. Its MAGNITUDE is
            // the 8,500 units the comment above works from, so this row's two
            // volume figures and its lot arithmetic all describe one trade.
            volume: Some(-8_500),
            // The candles table's OWN percentages, copied. Distinct VALUES
            // from each other, because they are two different measurements and
            // a fixture that reused one number could not catch them being
            // wired to the same field.
            percentage_change: Some(2.14),
            open_percentage_change: Some(-1.08),
            // Real delays, because the anti-vacuity guard below derives its
            // expectation from the column manifest: a fixture that left these
            // `None` would declare six columns that never reach the wire.
            //
            // The three are internally consistent, so a projection that mixed
            // them up would be visible here: the window is 5 s, the first
            // trade arrived 123 ms after it opened, the last one 1 Âµs before
            // it closed, and the span between them is therefore
            // 5s - 123ms - 1us = 4.876999 s -- which RENDERS as "5 seconds",
            // because the second band is whole-unit by the operator's own
            // instruction. The `_ns` twin carries the exact figure, which is
            // precisely the division of labour the pair exists for.
            subscribed: true,
        }
    }

    /// I-P1-11: the bare `security_id` is reused across segments, so a key
    /// without `segment` silently collapses two distinct instruments.
    #[test]
    fn the_dedup_key_carries_segment_beside_security_id() {
        assert!(DEDUP_KEY_TOP_VOLUME_RANK.contains("security_id"));
        assert!(
            DEDUP_KEY_TOP_VOLUME_RANK.contains("segment"),
            "a key naming security_id without segment collapses two instruments \
             that share an id across segments"
        );
    }

    /// feed-in-key EVERYWHERE (operator override 2026-06-28). Whole-token, so
    /// a column merely CONTAINING "feed" cannot satisfy it.
    #[test]
    fn the_dedup_key_carries_feed_as_a_whole_token() {
        assert!(
            DEDUP_KEY_TOP_VOLUME_RANK
                .split(',')
                .any(|t| t.trim() == "feed"),
            "feed must be a whole key token, not a substring of another column"
        );
    }

    /// The designated timestamp comes FIRST (2026-04-28 regression rule).
    #[test]
    fn the_designated_timestamp_leads_the_dedup_key() {
        assert_eq!(
            DEDUP_KEY_TOP_VOLUME_RANK.split(',').next().map(str::trim),
            Some("ts")
        );
    }

    /// The DISPLAY columns stay OUT of the row's identity.
    ///
    /// `contract` is a pure function of `security_id`, so it adds no identity
    /// â but a stale label on a re-emit would make the same contract a
    /// DIFFERENT key and duplicate the row, which is the collision the key
    /// exists to prevent. `volume_percentage_change` is the recorded value.
    ///
    /// `rank` is asserted absent too, and stays asserted after the column was
    /// removed on 2026-09-13: an existing table still HAS the column (the
    /// self-heal path has no DROP), so a future change that started writing it
    /// again must not be able to put it in the key by accident. The RETIRED
    /// names (`net_volume_chg_milli_pct`, `window_lots_milli`, `lot_size`,
    /// `gain_pct`, `candle_price_chg_pct`) stay listed for exactly that
    /// reason â a table written before 2026-09-19 still carries them.
    #[test]
    fn the_display_columns_are_deliberately_absent_from_the_dedup_key() {
        for banned in [
            "rank",
            "contract",
            "volume_percentage_change",
            "total_lots_traded",
            "per_lot_quantity",
            "cumulative_day_volume",
            "percentage_change",
            "open_percentage_change",
            "close_vs_prev_bar_pct",
            // Retired names, still asserted: an existing table has the columns.
            "net_volume_chg_milli_pct",
            "window_lots_milli",
            "lot_size",
            "gain_pct",
            "candle_price_chg_pct",
        ] {
            assert!(
                !DEDUP_KEY_TOP_VOLUME_RANK
                    .split(',')
                    .any(|t| t.trim() == banned),
                "{banned} in the key lets one contract hold several rows per snapshot"
            );
        }
    }

    /// The `rank` column is gone from every schema surface at once.
    ///
    /// Three surfaces, because a column half-removed is worse than one left
    /// alone: a CREATE without it plus an ALTER manifest with it would re-add
    /// it on every boot, and an ILP line naming it against a fresh table
    /// auto-creates the column the CREATE just declined to make.
    #[test]
    fn the_rank_column_is_absent_from_the_ddl_the_manifest_and_the_wire() {
        assert!(
            !top_volume_create_ddl(SnapshotCadence::OneSecond).contains("rank "),
            "the CREATE still declares rank"
        );
        assert!(
            !TOP_VOLUME_RANK_COLUMNS.iter().any(|(c, _)| *c == "rank"),
            "the self-heal manifest still ADDs rank, so it returns every boot"
        );
        let mut w = TopVolumeRankWriter::for_test();
        w.append_row(&row()).expect("append");
        let line = w.buffer_utf8();
        assert!(
            !line.contains("rank="),
            "the ILP line still writes rank: {line}"
        );
    }

    /// The stored percentage must FOLLOW from the stored inputs, exactly.
    ///
    /// It is a monotone transform of the lot count, not independent
    /// information â so the one thing worth pinning is that the two never
    /// drift, in the direction the doc names: if they disagree, the percentage
    /// is the wrong one.
    ///
    /// The derivation goes through the MILLI-lot figure the leaderboard
    /// computes, never through the whole `total_lots_traded` beside it: the
    /// whole count truncates the fractional lot, and re-deriving from it would
    /// silently round the percentage down by up to 100. The fixture carries
    /// 42.5 lots precisely so the two derivations give DIFFERENT answers and
    /// the wrong one cannot pass.
    #[test]
    fn the_stored_percentage_follows_from_the_stored_inputs() {
        let r = row();
        // The traded-unit count is NOT a column any more -- the operator
        // removed it on 2026-09-19 -- so the numerator lives beside the
        // fixture that uses it. The derivation it pins is unchanged.
        let milli_lots = FIXTURE_TRADED_UNITS * 1_000 / r.per_lot_quantity;
        assert_eq!(
            r.volume_percentage_change,
            (milli_lots * 100 - 100_000) / 1_000,
            "volume_percentage_change must equal (milli_lots * 100 - 100_000) / 1_000"
        );
        assert_eq!(
            r.total_lots_traded,
            milli_lots / 1_000,
            "total_lots_traded must be the WHOLE lot count"
        );
        // The truncating derivation is a DIFFERENT number on this fixture, so
        // the assertion above is not satisfied by both and cannot pass on a
        // projection that switched to the cheaper form.
        assert_ne!(
            r.volume_percentage_change,
            r.total_lots_traded * 100 - 100,
            "the fixture must separate the two derivations, or neither is pinned"
        );
        // One lot is exactly zero change; below one lot is NEGATIVE, which is
        // the whole reason the change form was chosen over a ratio.
        // `black_box` so clippy does not fold the one-lot case to a constant
        // (`erasing_op`); the arithmetic is the point of the assertion.
        assert_eq!((std::hint::black_box(1_000_i64) * 100 - 100_000) / 1_000, 0);
        assert!((500_i64 * 100 - 100_000) / 1_000 < 0);
    }

    /// The two cadences must be distinguishable in the key, or the 5s
    /// snapshot silently overwrites the 1s snapshot that shares its second.
    #[test]
    fn the_cadence_is_in_the_key_so_5s_never_overwrites_1s() {
        assert!(
            DEDUP_KEY_TOP_VOLUME_RANK
                .split(',')
                .any(|t| t.trim() == "tf"),
            "without tf in the key, every 5s row lands on the 1s row of the \
             same second and one of the two is lost"
        );
    }

    /// Families are ranked separately. Without `family` in the key that is
    /// usually fine (security_id differs) â but a contract listed in BOTH
    /// families would collide. Pinned so the separation survives a refactor.
    #[test]
    fn the_family_is_in_the_key() {
        assert!(
            DEDUP_KEY_TOP_VOLUME_RANK
                .split(',')
                .any(|t| t.trim() == "family")
        );
    }

    #[test]
    fn top_volume_rank_create_ddl_partitions_by_hour_and_enables_dedup() {
        let ddl = top_volume_create_ddl(SnapshotCadence::OneSecond);
        assert!(ddl.contains("PARTITION BY HOUR"), "{ddl}");
        assert!(
            ddl.contains(&format!("DEDUP UPSERT KEYS({DEDUP_KEY_TOP_VOLUME_RANK})")),
            "{ddl}"
        );
        assert!(ddl.contains("timestamp(ts)"), "{ddl}");
    }

    /// Schema-self-heal order: CREATE, then every column, then DEDUP ENABLE.
    /// Never a DROP â this table is append-only history.
    #[test]
    fn top_volume_rank_ensure_statements_never_drop_and_end_with_dedup_enable() {
        let statements = top_volume_ensure_statements(SnapshotCadence::OneSecond);
        assert!(statements[0].starts_with("CREATE TABLE IF NOT EXISTS"));
        assert!(
            statements
                .last()
                .is_some_and(|s| s.contains("DEDUP ENABLE")),
            "DEDUP ENABLE must come last, after the columns it names exist"
        );
        assert!(
            !statements.iter().any(|s| s.to_uppercase().contains("DROP")),
            "a DROP in the self-heal path would delete history on any boot"
        );
    }

    /// Every column in the CREATE must also appear in the ALTER manifest, or
    /// a table created by an older build never gains it and every write of
    /// that column fails forever.
    #[test]
    fn every_created_column_is_also_in_the_self_heal_manifest() {
        // Both directions, and the name is the direction that was MISSING.
        //
        // Until 2026-09-12 this test iterated the MANIFEST and looked for each
        // entry in the CREATE â the opposite of what its name promises, and
        // the harmless direction. A column added to the CREATE and forgotten
        // in the manifest passed green, which is the direction that actually
        // bites: an ALREADY-CREATED table on the box never receives the
        // `ADD COLUMN`, so the first ILP write auto-creates it at whatever
        // type the wire value infers. Dev and prod then hold the same column
        // at different types from the same binary â exactly the drift
        // `table_schema_lockstep_guard`'s header describes.
        //
        // The type half was near-vacuous too: `ddl.contains("LONG")` is true
        // for any table with one LONG column anywhere, so it could not catch a
        // manifest that declared the wrong type. It now compares the type
        // parsed from the column's OWN position in the DDL.
        let ddl = top_volume_create_ddl(SnapshotCadence::OneSecond);
        let created = ddl_columns(&ddl);

        for (col, ty) in TOP_VOLUME_RANK_COLUMNS {
            let found = created
                .iter()
                .find(|(name, _)| name == col)
                .unwrap_or_else(|| panic!("column {col} is in the manifest but not in the CREATE"));
            assert_eq!(
                &found.1, ty,
                "column {col} is {} in the CREATE and {ty} in the manifest",
                found.1
            );
        }

        for (name, ty) in &created {
            // The designated timestamp is deliberately absent: a table's
            // designated column cannot be added by `ALTER ADD COLUMN`, so it
            // has no place in a self-heal manifest.
            if name == "ts" {
                continue;
            }
            let declared = TOP_VOLUME_RANK_COLUMNS
                .iter()
                .find(|(col, _)| col == name)
                .unwrap_or_else(|| {
                    panic!(
                        "column {name} is in the CREATE but NOT in the self-heal manifest â a \
                         box that already has this table would never receive it"
                    )
                });
            assert_eq!(
                declared.1, ty,
                "column {name} is {ty} in the CREATE and {} in the manifest",
                declared.1
            );
        }
    }

    /// The column list of a `CREATE TABLE` as `(name, type)` pairs.
    fn ddl_columns(ddl: &str) -> Vec<(String, String)> {
        let open = ddl.find('(').expect("the CREATE opens a column list");
        let close = ddl
            .find(") timestamp(")
            .expect("the CREATE closes its column list before the designated clause");
        ddl[open + 1..close]
            .split(',')
            .filter_map(|part| {
                let mut it = part.split_whitespace();
                let name = it.next()?;
                let ty = it.next()?;
                Some((name.to_string(), ty.to_string()))
            })
            .collect()
    }

    /// The parser above must actually see every column, or the test that
    /// depends on it passes by reading an empty list.
    #[test]
    fn the_ddl_column_parser_reads_the_whole_list_and_not_a_prefix() {
        let created = ddl_columns(&top_volume_create_ddl(SnapshotCadence::OneSecond));
        assert_eq!(
            created.len(),
            TOP_VOLUME_RANK_COLUMNS.len() + 1,
            "parsed {created:?} â expected the manifest plus the designated `ts`"
        );
        assert_eq!(created.first().map(|c| c.0.as_str()), Some("ts"));
        assert!(
            created.iter().any(|(n, t)| n == "volume" && t == "LONG"),
            "parsed {created:?}"
        );
        assert!(
            created
                .iter()
                .any(|(n, t)| n == "subscribed" && t == "BOOLEAN"),
            "the LAST column must parse, or the closing bracket was mis-located: {created:?}"
        );
    }

    /// A row that fails PART-WAY must not poison the writer.
    ///
    /// `at()` validates the timestamp only after the table, symbols and columns
    /// are already in the buffer, and `table()` has by then set questdb-rs to
    /// `TableWritten`, where every later `.table()` is REFUSED. Before the
    /// 2026-09-09 marker fix, one negative timestamp killed this writer for the
    /// rest of the process â and `pending` stayed 0, so `flush`'s early return
    /// meant nothing ever cleared it either.
    #[test]
    fn a_row_that_fails_part_way_does_not_poison_the_buffer() {
        let mut w = TopVolumeRankWriter::for_test();

        // A good row first, so the test also proves the rewind keeps it.
        w.append_row(&row()).expect("first good row");
        assert_eq!(w.pending(), 1);

        // Negative designated timestamp: accepted by `TimestampNanos::new`'s
        // caller chain up to `at()`, which rejects it AFTER the row's bytes
        // are already written.
        let mut bad = row();
        bad.snapshot_ts_ist_nanos = -1;
        assert!(
            w.append_row(&bad).is_err(),
            "a negative designated timestamp must be refused"
        );
        assert_eq!(
            w.pending(),
            1,
            "the refused row must not be counted, and the good row must survive"
        );

        // The real assertion: the writer still works.
        w.append_row(&row())
            .expect("the writer must still accept rows after a part-way failure");
        assert_eq!(w.pending(), 2);

        let body = w.buffer_utf8();
        assert_eq!(
            body.lines().filter(|l| !l.trim().is_empty()).count(),
            2,
            "exactly the two GOOD rows are buffered; the partial row was rewound"
        );
    }

    #[test]
    fn append_row_carries_every_symbol_and_column() {
        let mut w = TopVolumeRankWriter::for_test();
        w.append_row(&row()).expect("append");
        assert_eq!(w.pending(), 1);
        let line = w.buffer_utf8();
        for expected in [
            // The TRAILING COMMA is the assertion. An ILP table name ends at
            // the first comma, so "top_volume_5s," rejects both the retired
            // shared "top_volume," and any other cadence's table.
            "top_volume_5s,",
            "tf=5s",
            "family=stock_option",
            "feed=dhan",
            "segment=NSE_FNO",
            "contract=RELIANCE-25Sep2026-1400-CE",
            "volume_percentage_change=4150i",
            "security_id=44321i",
            "total_lots_traded=42i",
            "per_lot_quantity=200i",
            "underlying_id=2885i",
            "volume=-8500i",
            "subscribed=t",
        ] {
            assert!(line.contains(expected), "missing {expected} in: {line}");
        }
    }

    /// The wire line must carry EVERY declared column, not merely the ones a
    /// reviewer remembered to list above.
    ///
    /// The `.contains()` loop is additive: a column dropped from `write_row`
    /// keeps that test green unless someone also thinks to add it to the
    /// array. This one derives its expectation from the manifest, so a new
    /// column is covered the moment it is declared.
    ///
    /// # â  Why the match is ANCHORED (2026-09-19), and a correction
    ///
    /// It used to ask `line.contains("{col}=")`, which is satisfied by any
    /// other column whose name ENDS with this one â `volume=` is a substring
    /// of `cumulative_day_volume=`, so `volume` has been passing vacuously
    /// since Phase 1 began, on a line that never carried it.
    ///
    /// **An earlier draft of this note claimed the delay pairs added three
    /// more of these â `open=` inside `open_latency=`, `close=` inside
    /// `close_latency=`. That is FALSE and was refuted by bite-testing it:
    /// the `=` sits between them, so `open_latency=` does not contain
    /// `open=`.** The hazard is a SUFFIX collision, never a prefix one, and
    /// recording the wrong shape would have sent the next reader looking for
    /// the wrong thing. `volume` was and remains the only live instance.
    ///
    /// ⚠ 2026-09-19: the six delay columns that example names left this
    /// table with the 21→15 strip (they live on `candles_<tf>` now), so the
    /// names no longer resolve here. The correction is kept because the
    /// LESSON is the durable half — suffix, never prefix.
    ///
    /// The anchoring is kept regardless, for two reasons that survive the
    /// correction: it is what makes the `volume` carve-out below honest
    /// rather than accidental, and the next suffix collision costs nothing to
    /// have already been guarded against. It is the same substring trap
    /// `append_row_omits_the_candle_percentages_when_they_are_unknown`
    /// already records (`percentage_change=` inside
    /// `volume_percentage_change=`), and the ILP field separator settles it:
    /// a field is preceded by `,`, or by the single space that ends the
    /// symbol section.
    #[test]
    fn every_declared_column_actually_reaches_the_wire() {
        let mut w = TopVolumeRankWriter::for_test();
        w.append_row(&row()).expect("append");
        let line = w.buffer_utf8();
        for (col, _) in TOP_VOLUME_RANK_COLUMNS {
            // ⚠ RETIRED 2026-09-19 — the `volume` carve-out that stood here
            // asserted the column must NOT reach the wire, because a two-phase
            // rename was holding `volume` empty for a retention window so one
            // column could never mean the vendor day total on Monday's rows and
            // the candle's signed number on Tuesday's.
            //
            // Both halves of that reason are gone. The operator ordered a fresh
            // scratch database the same day, so no row anywhere holds the old
            // meaning and there is no partition boundary to straddle; and
            // `cumulative_day_volume` — the column whose suffix made the
            // unanchored match vacuous in the first place — left the table in
            // the same change. `volume` is now an ordinary written column and
            // is proven by the loop below exactly like every other one.
            assert!(
                line.contains(&format!(",{col}=")) || line.contains(&format!(" {col}=")),
                "declared column {col} never reached the ILP line: {line}"
            );
        }
    }

    /// The stored inputs must DIVIDE BACK to the stored lot count.
    ///
    /// `total_lots_traded` is what the board was ordered by (through its
    /// milli-lot form); `delta_units` and `per_lot_quantity` are the two
    /// numbers it was computed from. If the row can carry a trio that does not
    /// satisfy its own arithmetic, the columns added "so the ordering is
    /// checkable from the table alone" do not make it checkable.
    #[test]
    fn the_stored_inputs_reproduce_the_stored_lot_count() {
        let r = row();
        assert_eq!(r.per_lot_quantity, 200, "the fixture's denominator");
        assert_eq!(
            FIXTURE_TRADED_UNITS / r.per_lot_quantity,
            r.total_lots_traded,
            "8,500 units on a 200-unit lot is 42 whole lots"
        );
    }

    #[test]
    fn the_cadence_labels_are_the_stable_wire_strings() {
        assert_eq!(SnapshotCadence::OneSecond.as_str(), "1s");
        assert_eq!(SnapshotCadence::ThreeSecond.as_str(), "3s");
        assert_eq!(SnapshotCadence::FiveSecond.as_str(), "5s");
        assert_eq!(SnapshotCadence::OneMinute.as_str(), "1m");
    }

    /// The label and the interval are two halves of the same claim: a row
    /// stamped `5s` asserts that the snapshot behind it was taken every five
    /// seconds. They are separate `match` arms, so nothing but a test stops
    /// one from being edited without the other â and the drift would be
    /// invisible, because a wrongly-paced snapshot still writes a
    /// well-formed row under a label that reads correct.
    ///
    /// â  The pre-2026-09-12 form of this test did
    /// `as_str().trim_end_matches('s').parse::<u64>()`, which reads `"1m"` as
    /// `"1m"` and PANICS. It was written when every label happened to end in
    /// `s`, and it would have failed the build the moment a minute cadence
    /// arrived â as a parse panic naming nothing, not as a drift report. The
    /// unit suffix is now part of what is checked.
    #[test]
    fn interval_secs_agrees_with_the_label_it_is_stored_under() {
        for cadence in SnapshotCadence::ALL {
            let label = cadence.as_str();
            let (digits, unit) = label.split_at(label.len() - 1);
            let count: u64 = digits
                .parse()
                .unwrap_or_else(|_| panic!("cadence label {label} must be <n><unit>"));
            let from_label = match unit {
                "s" => count,
                "m" => count * 60,
                other => panic!(
                    "cadence label {label} ends in {other:?}; only 's' and 'm' \
                     have a defined seconds-per-unit here"
                ),
            };
            assert_eq!(
                cadence.interval_secs(),
                from_label,
                "{label} claims {from_label}s in its label but fires every {}s",
                cadence.interval_secs()
            );
        }
    }

    /// A disconnected writer must not silently retain rows: a buffer kept
    /// across flushes replays a rejected row forever and poisons every later
    /// flush with it.
    #[test]
    fn a_flush_without_a_sender_discards_and_reports() {
        let mut w = TopVolumeRankWriter::for_test();
        w.append_row(&row()).expect("append");
        let err = w.flush().expect_err("no sender must be an error");
        assert!(
            format!("{err}").contains("discarded"),
            "the error must say the rows were discarded: {err}"
        );
        assert_eq!(w.pending(), 0, "pending must be cleared after a discard");
    }

    /// Flushing nothing is a no-op, not an error â otherwise a quiet second
    /// produces a spurious failure every time.
    #[test]
    fn flushing_an_empty_buffer_is_not_an_error() {
        let mut w = TopVolumeRankWriter::for_test();
        assert!(w.flush().is_ok());
    }

    /// `subscribed=false` is the whole point of the column â it must be
    /// written, not skipped as a default.
    #[test]
    fn append_row_writes_subscribed_false_rather_than_omitting_it() {
        let mut w = TopVolumeRankWriter::for_test();
        let mut r = row();
        r.subscribed = false;
        w.append_row(&r).expect("append");
        assert!(
            w.buffer_utf8().contains("subscribed=f"),
            "a false must be recorded, or the audit cannot tell 'we missed it' \
             from 'no row was written'"
        );
    }

    /// An unknown candle reading must OMIT the ILP fields (QuestDB NULL) and
    /// leave every other column of the row intact.
    ///
    /// This is the bite for the 2026-09-12 fix, re-pointed on 2026-09-19 at
    /// the columns that survived it. Before that fix, a percentage the row
    /// could not resolve took the ENTIRE row out of the table -- so the
    /// assertion that matters is not the absence of `percentage_change=`, it
    /// is the PRESENCE of `cumulative_day_volume=` beside it.
    ///
    /// It is asserted on the CANDLE-sourced percentages because those are the
    /// ones that can legitimately be absent now: a contract with no aggregator
    /// slot, or one whose bar belongs to a neighbouring window, resolves none
    /// of them. (`gain_pct`, the column this test was originally written for,
    /// was deleted with the operator's removal of the underlying percentage
    /// change on 2026-09-19.)
    #[test]
    fn append_row_omits_the_candle_percentages_when_they_are_unknown() {
        let mut w = TopVolumeRankWriter::for_test();
        let mut r = row();
        r.percentage_change = None;
        r.open_percentage_change = None;
        w.append_row(&r).expect("append");
        let line = w.buffer_utf8();
        // â  The LEADING COMMA is load-bearing and is not decoration. Every ILP
        // field after the first is comma-prefixed, and `percentage_change` is a
        // SUFFIX of both `volume_percentage_change` and
        // `open_percentage_change` -- so a bare `"percentage_change="` matches
        // the volume column that is always present and this test fails on a
        // perfectly correct row. Found exactly that way on 2026-09-19.
        for absent in [
            ",percentage_change=",
            ",open_percentage_change=",
            ",close_vs_prev_bar_pct=",
        ] {
            assert!(
                !line.contains(absent),
                "an unknown percentage must be an ABSENT field (NULL), never a \
                 written value -- a 0.0 there reads as a flat contract: \
                 {absent} in {line}"
            );
        }
        assert!(
            line.contains(&format!(
                "volume={}i",
                r.volume.expect("fixture carries a volume")
            )),
            "the row must still carry its volume -- that is the column this \
             table exists for, and the whole reason a missing percentage no \
             longer drops it: {line}"
        );
        assert!(
            line.contains(&format!("total_lots_traded={}i", r.total_lots_traded)),
            "and the ranking key, or the ordering is uncheckable: {line}"
        );
        // No `flush` assertion: `for_test` has no sender, so a NON-EMPTY
        // flush is an error BY DESIGN (see
        // `a_flush_without_a_sender_discards_and_reports`). The proof that the
        // line is well-formed is that `append_row` returned `Ok` -- it commits
        // the marker only after the whole chain, `at()` included, succeeded.
    }

    /// The assumed ILP row width must cover a REAL worst-case line.
    ///
    /// # The guard the old constant did not have
    ///
    /// `TOP_VOLUME_ILP_ROW_BYTES` was `120` and labelled "measured", and
    /// nothing measured it â the only assert over it compared `2X >= X`, which
    /// is true for every value. This test builds the widest line `write_row`
    /// can actually emit (every integer at its column's extreme, a full-width
    /// `f64` in every optional DOUBLE, the longest real symbol values) and
    /// measures the bytes.
    ///
    /// It matters because past the producer ceiling this writer DROPS, and
    /// this table has no spill tier: an under-counted width means a single
    /// sweep silently loses rows with a perfectly healthy writer.
    #[test]
    fn the_assumed_ilp_row_width_covers_a_real_worst_case_line() {
        let mut w = TopVolumeRankWriter::for_test();
        let r = TopVolumeRankRow {
            snapshot_ts_ist_nanos: i64::MAX / 2,
            cadence: SnapshotCadence::OneMinute,
            family: "stock",
            feed: "dhan",
            segment: "NSE_FNO",
            // The widest label this column can realistically carry: a long
            // underlying, a full date, a fractional strike and a leg.
            contract: "MAZAGONDOCKSHIPBUILDERS-25Sep2026-123456.75-CE",
            security_id: i64::MAX,
            underlying_id: i64::MAX,
            per_lot_quantity: i64::MAX,
            total_lots_traded: i64::MAX,
            volume_percentage_change: i64::MIN,
            // Every OPTIONAL column at ITS extreme too. `i64::MIN` is the
            // widest signed integer this column can print (20 chars including
            // the sign) and a full-width shortest-repr `f64` with a sign is
            // the widest a percentage or a price can be -- the point of this
            // test is that no real row can exceed the assumed width, so an
            // optional column omitted here would under-measure exactly the
            // case it exists to bound.
            volume: Some(i64::MIN),
            percentage_change: Some(-1.234_567_890_123_456_7_f64),
            open_percentage_change: Some(-1.234_567_890_123_456_7_f64),
            subscribed: true,
        };
        w.append_row(&r).expect("append");
        let width = w.buffer_utf8().len();

        assert!(
            width <= MEASURED_WORST_CASE_ILP_ROW_BYTES,
            "one worst-case row measured {width} B against an assumed \
             {MEASURED_WORST_CASE_ILP_ROW_BYTES} B. The producer ceiling is derived from \
             the assumption, and past it this writer DROPS with no spill tier â \
             so an under-count is silent row loss. Raise the constant (and the \
             literal in its sibling assert) to cover the measurement."
        );
        // Non-vacuous in the other direction: if a future change made rows tiny
        // this test would pass while the constant stayed wildly oversized, so
        // pin that the measurement is in the right order of magnitude too.
        assert!(
            width > TOP_VOLUME_ILP_ROW_BYTES / 4,
            "a worst-case row measured only {width} B against an assumed \
             {TOP_VOLUME_ILP_ROW_BYTES} B. Either the row shrank dramatically \
             (re-derive the constant down) or this test stopped building a \
             worst-case row and is no longer measuring anything."
        );
    }

    /// The complement: a known candle percentage is still written.
    ///
    /// Renamed from `append_row_writes_gain_pct_when_it_is_known` on
    /// 2026-09-19 with the `gain_pct` column it covered. The property it
    /// pinned -- an optional DOUBLE is written VERBATIM, sign included, and
    /// is not silently rounded or dropped -- still matters, so it moves to
    /// the column that replaced it rather than going away with the name.
    #[test]
    fn append_row_writes_a_candle_percentage_when_it_is_known() {
        let mut w = TopVolumeRankWriter::for_test();
        let mut r = row();
        r.percentage_change = Some(-3.5);
        w.append_row(&r).expect("append");
        let line = w.buffer_utf8();
        assert!(
            line.contains("percentage_change=-3.5"),
            "a known percentage must be stored verbatim, negatives included: {line}"
        );
    }

    // -----------------------------------------------------------------------
    // Off-drain write (2026-09-07)
    // -----------------------------------------------------------------------

    #[test]
    fn split_for_offload_moves_the_sender_and_opens_the_queue() {
        let (producer, sink, _rx) = TopVolumeRankWriter::for_test().split_for_offload();
        assert!(
            producer.is_offloaded(),
            "the producer must know it is split â the drain reads this to refuse a synchronous flush"
        );
        assert!(
            producer.sender.is_none(),
            "the ILP sender must MOVE to the sink; leaving a copy behind is how a \
             5-second network call finds its way back onto the drain"
        );
        assert!(
            sink.sender.is_none(),
            "for_test() has no sender to move â this pins the MOVE, not the presence"
        );
    }

    #[test]
    fn a_flush_after_the_split_hands_off_instead_of_writing() {
        let (mut producer, _sink, rx) = TopVolumeRankWriter::for_test().split_for_offload();
        producer.append_row(&row()).expect("append");
        assert_eq!(producer.pending(), 1);

        producer.flush().expect("hand-off is not an error");

        assert_eq!(
            producer.pending(),
            0,
            "the rows are the writer thread's now, not the drain's"
        );
        let batch = rx.try_recv().expect("the batch must be on the queue");
        assert_eq!(batch.rows(), 1, "the batch must carry the row count");
    }

    #[test]
    fn a_full_queue_is_backpressure_and_never_loss() {
        let (mut producer, _sink, _rx) = TopVolumeRankWriter::for_test().split_for_offload();
        // Fill every slot. `_rx` is held so nothing drains it.
        for _ in 0..TOP_VOLUME_FLUSH_QUEUE_DEPTH {
            producer.append_row(&row()).expect("append");
            assert!(matches!(
                producer.offload_flush(),
                TopVolumeOffloadOutcome::Sent(1)
            ));
        }

        producer.append_row(&row()).expect("append");
        let outcome = producer.offload_flush();

        assert_eq!(
            outcome,
            TopVolumeOffloadOutcome::QueueFull(1),
            "a full queue must report backpressure, not success and not loss"
        );
        assert_eq!(
            producer.pending(),
            1,
            "the rows must be RETAINED â this is the arm that makes the bounded \
             queue safe. Without it a full queue either blocks the drain (the \
             original defect) or drops rows (a worse one)."
        );
    }

    #[test]
    fn queue_full_is_not_reported_to_the_caller_as_a_failure() {
        let (mut producer, _sink, _rx) = TopVolumeRankWriter::for_test().split_for_offload();
        for _ in 0..TOP_VOLUME_FLUSH_QUEUE_DEPTH {
            producer.append_row(&row()).expect("append");
            producer.flush().expect("hand-off");
        }
        producer.append_row(&row()).expect("append");

        assert!(
            producer.flush().is_ok(),
            "backpressure is not a failure: reporting it as one would decay a \
             health signal for rows that are still safely held"
        );
    }

    #[test]
    fn retaining_past_the_span_cap_drops_rather_than_widening_forever() {
        let (mut producer, _sink, _rx) = TopVolumeRankWriter::for_test().split_for_offload();
        for _ in 0..TOP_VOLUME_FLUSH_QUEUE_DEPTH {
            producer.append_row(&row()).expect("append");
            producer.flush().expect("hand-off");
        }

        // Every flush from here retains, until the span cut fires.
        let mut outcomes = Vec::new();
        for _ in 0..=MAX_TOP_VOLUME_RETAINED_FLUSH_SPANS {
            producer.append_row(&row()).expect("append");
            outcomes.push(producer.offload_flush());
        }

        let capped = outcomes
            .iter()
            .filter(|o| matches!(o, TopVolumeOffloadOutcome::WidthCapped(_)))
            .count();
        assert_eq!(
            capped, 1,
            "exactly one cut on the span AFTER the retained ones â the constant \
             names how many may be RETAINED, so `>` and not `>=`. Got {outcomes:?}"
        );
        assert_eq!(
            producer.pending(),
            0,
            "the cut must clear the buffer; retaining past it is the unbounded-\
             memory path the cap exists to close"
        );
    }

    #[test]
    fn a_disconnected_sink_reports_gone_and_never_forges_success() {
        let (mut producer, _sink, rx) = TopVolumeRankWriter::for_test().split_for_offload();
        drop(rx);
        producer.append_row(&row()).expect("append");

        let outcome = producer.offload_flush();

        assert!(
            matches!(outcome, TopVolumeOffloadOutcome::SinkGone(1)),
            "a vanished writer thread must be reported as gone. \"We sent it\" \
             when nothing was sent is the one report that must never be wrong. \
             Got {outcome:?}"
        );
        // A fresh row, because the outcome above already discarded the first:
        // `flush` returns early on an empty buffer, so asserting on it without
        // re-appending would test the early return rather than the gone arm.
        producer.append_row(&row()).expect("append");
        assert!(
            producer.flush().is_err(),
            "and the caller must see the failure, not a silent Ok"
        );
    }

    #[test]
    fn the_sink_reports_zero_landed_when_it_cannot_write() {
        let (mut producer, mut sink, rx) = TopVolumeRankWriter::for_test().split_for_offload();
        producer.append_row(&row()).expect("append");
        producer.flush().expect("hand-off");
        let mut batch = rx.try_recv().expect("batch");

        // `for_test()` has no ILP sender, so this is the no-sender arm.
        let landed = sink.write(&mut batch);

        assert_eq!(
            landed, 0,
            "a failed write must report ZERO landed. The caller reports health \
             from this number, so forging it is worse than the failure."
        );
        assert_eq!(
            batch.rows(),
            0,
            "and the batch must be cleared so a retry cannot double-count it"
        );
    }

    #[test]
    fn the_sink_reports_the_rows_that_actually_landed() {
        // No sender means nothing can land; the positive direction is covered
        // by the live QuestDB path. What this pins is that an EMPTY batch is a
        // no-op rather than an error â the drain flushes on a timer, and a
        // timer that fires on an idle second must not log a loss.
        let (_producer, mut sink, _rx) = TopVolumeRankWriter::for_test().split_for_offload();
        let mut empty = TopVolumeFlushBatch {
            buffer: Buffer::new(ProtocolVersion::V1),
            rows: 0,
        };
        assert_eq!(sink.write(&mut empty), 0);
    }

    #[test]
    fn the_producer_byte_ceiling_stays_tighter_than_the_depth_path() {
        // Not a style preference â a sizing claim, pinned so it cannot drift
        // into depth's 32 MiB by copy-paste. Depth is a MEASURED ~63,800
        // rows/s and bursts 400 rows in ONE depth-200 snapshot; this table
        // emits at most one row per traded contract per sweep. A ceiling
        // sized for depth would hold minutes of stale rankings nothing
        // downstream wants â every sweep supersedes the last.
        //
        // â  RENAMED 2026-09-18, from `..._is_far_tighter_than_...`. The three
        // fold columns added that day took the row width 401 -> 531 B measured,
        // the assumed width 448 -> 596, and the ceiling 22.4 -> 29.8 MB. That
        // is an 11% margin under depth's 33.55 MB where it used to be 33%, and
        // "far" had stopped being true. The ASSERTION is unchanged and still
        // load-bearing; only the word that had gone stale is gone. Recorded
        // rather than quietly renamed because the narrowing is the finding:
        // this comparison, not the questdb-rs wedge, is now what caps the next
        // column added to this table.
        assert!(
            MAX_TOP_VOLUME_PRODUCER_BUFFER_BYTES
                < crate::depth_persistence::MAX_DEPTH_PRODUCER_BUFFER_BYTES,
            "the top-volume ceiling must stay tighter than depth's"
        );
        assert_eq!(
            TOP_VOLUME_FLUSH_QUEUE_DEPTH,
            crate::depth_persistence::DEPTH_FLUSH_QUEUE_DEPTH,
            "the queue DEPTH is deliberately the same across writers â one \
             number is one thing to keep true, and a shared shape is what stops \
             the three paths drifting into different failure semantics"
        );
    }

    /// `discard_pending` became `pub` on 2026-09-13 so the lane can account for
    /// retained rows at SHUTDOWN â the one moment nothing else calls it.
    ///
    /// On a `QueueFull` the producer keeps its rows and `flush` returns `Ok`,
    /// which is correct: that is backpressure, not loss. But if the process
    /// then exits while rows are retained, no later flush ever runs, and until
    /// this was reachable from the lane those rows skipped even the discard
    /// counter â a real drop with every surface reading green.
    #[test]
    fn discard_pending_clears_the_buffer_and_reports_the_count() {
        let mut writer = TopVolumeRankWriter::for_test();

        // Nothing pending: zero, no log, no counter movement, and â the part
        // that matters for a shutdown path called unconditionally â no panic.
        assert_eq!(
            writer.discard_pending(),
            0,
            "an empty producer must report zero discarded, so the lane's \
             shutdown can call this unconditionally without inventing a loss"
        );

        // With rows pending, the count is REPORTED and the buffer is cleared,
        // so a second call cannot double-count the same rows into the loss
        // series.
        let rows = [row(), row(), row()];
        for row in &rows {
            writer
                .append_row(row)
                .expect("append must accept a valid row");
        }
        assert_eq!(
            writer.discard_pending(),
            rows.len(),
            "every retained row must be reported exactly once"
        );
        assert_eq!(
            writer.discard_pending(),
            0,
            "the buffer must be CLEARED by the discard â a second call that \
             re-reported the same rows would inflate the loss series on a \
             shutdown path that is reached from more than one place"
        );
    }

    /// Every counter this writer can increment is also SEEDED at zero in the
    /// production constructor.
    ///
    /// A counter that is only ever registered by its own first increment is
    /// ABSENT from the exporter until the event it reports happens â and an
    /// absent series reads exactly like a healthy zero. Worse, if the name is
    /// EMF-selected the CloudWatch agent computes counter deltas and DROPS the
    /// first sample of a series it has never seen, so the one episode that
    /// matters produces no datapoint at all. That rule cost 104,540 depth rows
    /// their classification on 2026-08-28.
    ///
    /// Until 2026-09-13 exactly ONE of the five names here was seeded. This
    /// test generalises the rule instead of re-listing it: it DERIVES the name
    /// set from the increment sites, so a sixth counter added tomorrow fails
    /// the build unless it is seeded too.
    ///
    /// Anti-vacuity: the derived set must be non-empty AND must contain the
    /// row-dropping arm by name. A scan that silently found nothing would
    /// otherwise pass.
    #[test]
    fn every_writer_counter_is_seeded_at_zero_in_the_constructor() {
        let src = include_str!("top_volume_rank_persistence.rs");
        let prod = src
            .split_once("\n#[cfg(test)]")
            .map_or(src, |(before, _)| before);

        const OPEN: &str = "metrics::counter!(\"";
        let mut names: Vec<&str> = Vec::new();
        let mut rest = prod;
        while let Some(at) = rest.find(OPEN) {
            let after = &rest[at + OPEN.len()..];
            let end = after.find('"').expect("counter! literal must close");
            let name = &after[..end];
            if !names.contains(&name) {
                names.push(name);
            }
            rest = &after[end..];
        }

        assert!(
            names.len() >= 5,
            "expected at least the five known writer counters; the scan found \
             {} â a scan that finds nothing passes vacuously, which is the \
             failure this assertion exists to prevent: {names:?}",
            names.len()
        );
        assert!(
            names.contains(&"tv_top_volume_rank_flush_width_capped_total"),
            "the row-DROPPING arm must be in the derived set, or this test is \
             not looking where it thinks it is: {names:?}"
        );

        for name in &names {
            let seed = format!("metrics::counter!(\"{name}\").increment(0);");
            assert!(
                prod.contains(&seed),
                "`{name}` is incremented somewhere in this writer but is never \
                 seeded at zero. Add `{seed}` to `TopVolumeRankWriter::new` \
                 beside the others. An unseeded counter is ABSENT from the \
                 exporter until its first event, and an absent series reads as \
                 health â on a loss counter that is a false OK."
            );
        }
    }

    /// Every band boundary, from BOTH sides, plus the two rounding hinges.
    ///
    /// The hinges are the point: 999,499 ns must read `999 microseconds` and
    /// 999,500 ns must read `1 millisecond`. A band that ended on the round
    /// number instead would render the second one as `1000 microseconds`,
    /// which is exactly the shape the operator's instruction rules out.
    #[test]
    fn render_delay_into_uses_whole_units_and_never_the_round_number_above_a_band() {
        let mut out = String::new();
        for (nanos, expected) in [
            (0_i64, "0 nanoseconds"),
            (1, "1 nanosecond"),
            (4, "4 nanoseconds"),
            (999, "999 nanoseconds"),
            (1_000, "1 microsecond"),
            (100_000, "100 microseconds"),
            (999_499, "999 microseconds"),
            (999_500, "1 millisecond"),
            (123_000_000, "123 milliseconds"),
            (999_499_999, "999 milliseconds"),
            (999_500_000, "1 second"),
            (2_000_000_000, "2 seconds"),
            (60_000_000_000, "60 seconds"),
        ] {
            render_delay_into(&mut out, nanos);
            assert_eq!(out, expected, "{nanos} ns");
        }
    }

    /// A negative delay renders with its sign and its magnitude, and does not
    /// panic on `i64::MIN`.
    ///
    /// Both halves matter. The drain back-dates a receipt by ring dwell, so a
    /// tick can legitimately carry an instant a hair before the boundary it
    /// lands in â a clamp to zero would hide that. And `abs()` on `i64::MIN`
    /// PANICS under this workspace's release profile (`overflow-checks = true`),
    /// on the frame-drain task, so the renderer uses `unsigned_abs`.
    #[test]
    fn a_negative_delay_keeps_its_sign_and_i64_min_does_not_panic() {
        let mut out = String::new();
        render_delay_into(&mut out, -1_500_000);
        assert_eq!(out, "-2 milliseconds");
        render_delay_into(&mut out, -1);
        assert_eq!(out, "-1 nanosecond");
        // The whole point: this line panics if anyone swaps in `abs()`.
        render_delay_into(&mut out, i64::MIN);
        assert!(out.starts_with('-'), "sign lost on i64::MIN: {out}");
        assert!(out.ends_with(" seconds"), "band wrong on i64::MIN: {out}");
    }

    /// The buffer is REUSED, so a longer previous value must never bleed into
    /// a shorter next one.
    ///
    /// `render_delay_into` clears first. Without that, rendering `999
    /// milliseconds` and then `1 second` would produce `1 secondiseconds`.
    #[test]
    fn the_reused_buffer_is_cleared_so_a_short_value_cannot_inherit_a_long_one() {
        let mut out = String::new();
        render_delay_into(&mut out, 999_499_999);
        assert_eq!(out, "999 milliseconds");
        render_delay_into(&mut out, 1);
        assert_eq!(out, "1 nanosecond");
    }

    /// THE reason the `_ns` twin exists, demonstrated rather than asserted in
    /// a comment.
    ///
    /// Four real delays, an order of magnitude apart each time. Sorted as TEXT
    /// they come out in the EXACT REVERSE of their true order, and the result
    /// looks entirely plausible â which is what makes it dangerous. Sorted on
    /// the nanosecond twin they come out right. Any `ORDER BY` must use the
    /// `_ns` column; this test fails if someone ever decides one column was
    /// enough.
    #[test]
    fn sorting_the_readable_column_reverses_the_true_order_which_is_why_the_ns_twin_exists() {
        let mut out = String::new();
        let mut rendered: Vec<(i64, String)> = Vec::new();
        for nanos in [1_000_000_000_i64, 2_000_000, 3_000, 4] {
            render_delay_into(&mut out, nanos);
            rendered.push((nanos, out.clone()));
        }

        let mut by_text = rendered.clone();
        by_text.sort_by(|a, b| b.1.cmp(&a.1));
        assert_eq!(
            by_text.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            vec![4, 3_000, 2_000_000, 1_000_000_000],
            "text descending should be wrong â if this now matches the true \
             order the fixture stopped demonstrating the hazard"
        );

        let mut by_nanos = rendered;
        by_nanos.sort_unstable_by_key(|(n, _)| std::cmp::Reverse(*n));
        assert_eq!(
            by_nanos.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            vec![1_000_000_000, 2_000_000, 3_000, 4],
        );
    }

    // ------------------------------------------------------------------
    // 2026-09-22 — four DIRECT per-timeframe tables.
    // ------------------------------------------------------------------

    /// Every cadence has its own table, named `top_volume_<label>`, and no two
    /// cadences share one. The suffix IS the `tf` label, so a row's table and
    /// its `tf` column can never disagree.
    #[test]
    fn every_cadence_has_its_own_table_named_after_its_label() {
        let mut seen = std::collections::HashSet::new();
        for c in SnapshotCadence::ALL {
            let table = c.table_name();
            assert_eq!(table, format!("top_volume_{}", c.as_str()));
            assert!(seen.insert(table), "{table} is shared by two cadences");
            assert_ne!(table, LEGACY_TOP_VOLUME_TABLE);
            assert_ne!(table, LEGACY_TOP_VOLUME_RANK_TABLE);
        }
        assert_eq!(seen.len(), 4);
        // The four consts are what the retention guard discovers; they must be
        // exactly what `table_name()` returns.
        let consts = [
            TOP_VOLUME_1S_TABLE,
            TOP_VOLUME_3S_TABLE,
            TOP_VOLUME_5S_TABLE,
            TOP_VOLUME_1M_TABLE,
        ];
        for (c, k) in SnapshotCadence::ALL.iter().zip(consts) {
            assert_eq!(c.table_name(), k);
        }
    }

    /// `table_name()` is a `const fn` — callable at compile time, which is the
    /// proof it allocates nothing and does no lookup at run time.
    #[test]
    fn table_name_is_evaluable_at_compile_time() {
        const NAME: &str = SnapshotCadence::OneMinute.table_name();
        assert_eq!(NAME, "top_volume_1m");
    }

    /// Each cadence's row reaches ITS table and no other, for all four.
    #[test]
    fn the_writer_routes_every_cadence_to_its_own_table() {
        for c in SnapshotCadence::ALL {
            let mut w = TopVolumeRankWriter::for_test();
            w.append_row(&TopVolumeRankRow {
                cadence: c,
                ..row()
            })
            .expect("append");
            let line = w.buffer_utf8();
            let prefix = format!("{},", c.table_name());
            assert!(line.starts_with(&prefix), "{c:?} wrote {line}");
            assert!(line.contains(&format!("tf={}", c.as_str())), "{line}");
            for other in SnapshotCadence::ALL {
                if other != c {
                    assert!(
                        !line.starts_with(&format!("{},", other.table_name())),
                        "{c:?} leaked into {}",
                        other.table_name()
                    );
                }
            }
            assert!(!line.starts_with("top_volume,"), "wrote the retired table");
        }
    }

    /// Every ORDER in which the four cadences can arrive in one buffer — all
    /// 24 permutations — routes every line correctly. A writer that remembered
    /// the previous row's table would fail some ordering, never all of them.
    #[test]
    fn every_arrival_order_of_the_four_cadences_routes_each_line_correctly() {
        fn permutations(items: &[SnapshotCadence]) -> Vec<Vec<SnapshotCadence>> {
            if items.len() <= 1 {
                return vec![items.to_vec()];
            }
            let mut out = Vec::new();
            for i in 0..items.len() {
                let mut rest = items.to_vec();
                let head = rest.remove(i);
                for mut tail in permutations(&rest) {
                    tail.insert(0, head);
                    out.push(tail);
                }
            }
            out
        }
        let all = permutations(&SnapshotCadence::ALL);
        assert_eq!(all.len(), 24);
        for order in all {
            let mut w = TopVolumeRankWriter::for_test();
            for c in &order {
                w.append_row(&TopVolumeRankRow {
                    cadence: *c,
                    ..row()
                })
                .expect("append");
            }
            assert_eq!(w.pending(), 4);
            let text = w.buffer_utf8();
            let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
            assert_eq!(lines.len(), 4, "{text}");
            for (line, c) in lines.iter().zip(&order) {
                assert!(
                    line.starts_with(&format!("{},", c.table_name())),
                    "order {order:?}: {line}"
                );
            }
        }
    }

    /// The four CREATEs are identical apart from the table name — one schema,
    /// four tables. A column added to one and not the others is caught here.
    #[test]
    fn top_volume_create_ddl_is_one_schema_byte_for_byte_across_the_four_tables() {
        let normalised: Vec<String> = SnapshotCadence::ALL
            .iter()
            .map(|c| top_volume_create_ddl(*c).replace(c.table_name(), "T"))
            .collect();
        for ddl in &normalised[1..] {
            assert_eq!(ddl, &normalised[0]);
        }
        for c in SnapshotCadence::ALL {
            let ddl = top_volume_create_ddl(c);
            assert!(
                ddl.starts_with(&format!("CREATE TABLE IF NOT EXISTS {} (", c.table_name())),
                "{ddl}"
            );
            assert!(ddl.contains("PARTITION BY HOUR"));
            assert!(ddl.contains(&format!("DEDUP UPSERT KEYS({DEDUP_KEY_TOP_VOLUME_RANK})")));
            // `tf` stays in the key even though it is constant per table: it
            // keeps a UNION across the four self-describing.
            assert!(DEDUP_KEY_TOP_VOLUME_RANK.contains("tf"));
        }
    }

    /// Every self-heal statement for a cadence names THAT cadence's table and
    /// no other; none drops anything; DEDUP ENABLE is last.
    #[test]
    fn top_volume_ensure_statements_touch_only_that_table_and_never_drop() {
        for c in SnapshotCadence::ALL {
            let statements = top_volume_ensure_statements(c);
            assert_eq!(statements.len(), TOP_VOLUME_RANK_COLUMNS.len() + 2);
            for s in &statements {
                assert!(s.contains(c.table_name()), "{s}");
                assert!(!s.to_uppercase().contains("DROP"), "{s}");
                for other in SnapshotCadence::ALL {
                    if other != c {
                        assert!(!s.contains(other.table_name()), "{s}");
                    }
                }
                assert!(
                    !s.contains(&format!("{LEGACY_TOP_VOLUME_TABLE} ")),
                    "names the retired shared table: {s}"
                );
            }
            assert!(statements[0].starts_with("CREATE TABLE IF NOT EXISTS"));
            assert!(
                statements
                    .last()
                    .is_some_and(|s| s.contains("DEDUP ENABLE"))
            );
        }
    }

    /// The pre-drop removes a same-named legacy VIEW only — never a table.
    #[test]
    fn top_volume_view_predrop_ddl_drops_a_view_never_a_table() {
        for c in SnapshotCadence::ALL {
            let sql = top_volume_view_predrop_ddl(c);
            assert_eq!(sql, format!("DROP VIEW IF EXISTS {};", c.table_name()));
            assert!(!sql.to_uppercase().contains("TABLE"), "{sql}");
        }
    }

    /// The ensure fn issues the view pre-drop BEFORE the CREATE for each table
    /// (a CREATE against a name that is still a view creates nothing), and
    /// does not count the pre-drop's refusal as a failure.
    #[test]
    fn ensure_top_volume_tables_predrops_the_view_before_creating_and_tolerates_its_refusal() {
        let src = include_str!("top_volume_rank_persistence.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap_or("");
        let body_start = prod
            .find("pub async fn ensure_top_volume_tables(")
            .expect("ensure fn present");
        let body = &prod[body_start..];
        let predrop = body
            .find("top_volume_view_predrop_ddl(cadence)")
            .expect("pre-drop call");
        let ensure = body
            .find("top_volume_ensure_statements(cadence)")
            .expect("ensure-statements call");
        assert!(
            predrop < ensure,
            "the view must be dropped before the CREATE"
        );
        let between = &body[predrop..ensure];
        assert!(
            !between.contains("all_accepted = false"),
            "a refused view pre-drop must not fail the ensure — it is the normal \
             steady state from the second boot onward"
        );
        assert!(
            between.contains("debug!"),
            "pre-drop refusal is logged at debug"
        );
    }
}

// 2026-09-22 — item 44e: the drain stages plain row data and the writer thread
// does the per-row ILP append. These pin the hand-off contract.
#[cfg(test)]
mod row_handoff_tests {
    use super::*;
    use std::sync::Arc;

    fn row(security_id: i64) -> TopVolumeRankRow<'static> {
        TopVolumeRankRow {
            snapshot_ts_ist_nanos: 1_757_000_000_000_000_000,
            cadence: SnapshotCadence::OneSecond,
            family: "stock_option",
            feed: "dhan",
            segment: "NSE_FNO",
            contract: "ignored-by-staging",
            security_id,
            underlying_id: 2885,
            per_lot_quantity: 200,
            total_lots_traded: 42,
            volume_percentage_change: 4_150,
            volume: Some(-8_500),
            percentage_change: Some(1.25),
            open_percentage_change: Some(-0.5),
            subscribed: true,
        }
    }

    fn labels() -> Arc<TopVolumeLabelMap> {
        let mut m = TopVolumeLabelMap::new();
        m.insert(
            (7, ExchangeSegment::NseFno),
            Arc::from("RELIANCE-25Sep2026-1400-CE"),
        );
        Arc::new(m)
    }

    #[test]
    fn from_row_keeps_every_column_and_drops_only_the_label() {
        let original = row(7);
        let staged = TopVolumeStagedRow::from_row(&original);
        assert_eq!(staged.with_contract(original.contract), original);
    }

    #[test]
    fn with_contract_swaps_in_the_label_and_nothing_else() {
        let original = row(7);
        let relabelled = TopVolumeStagedRow::from_row(&original).with_contract("x");
        assert_eq!(relabelled.contract, "x");
        assert_eq!(
            TopVolumeRankRow {
                contract: original.contract,
                ..relabelled
            },
            original,
            "only the contract column may differ"
        );
    }

    #[test]
    fn every_segment_symbol_round_trips() {
        for code in 0_u8..=8 {
            if let Some(seg) = ExchangeSegment::from_byte(code) {
                assert_eq!(segment_from_symbol(seg.as_str()), Some(seg));
            }
        }
        assert_eq!(segment_from_symbol("NOT_A_SEGMENT"), None);
    }

    #[test]
    fn labels_resolve_from_the_snapshot_and_fall_back_when_absent() {
        let map = labels();
        let hit = TopVolumeStagedRow::from_row(&row(7));
        let miss = TopVolumeStagedRow::from_row(&row(8));
        let negative = TopVolumeStagedRow::from_row(&row(-1));
        assert_eq!(
            resolve_label(&map, &hit, "unmapped"),
            "RELIANCE-25Sep2026-1400-CE"
        );
        assert_eq!(resolve_label(&map, &miss, "unmapped"), "unmapped");
        assert_eq!(resolve_label(&map, &negative, "unmapped"), "unmapped");
    }

    #[test]
    fn write_batch_round_trips_a_handed_off_batch_and_recycles_its_vector() {
        let (mut producer, mut writer, rx) =
            TopVolumeRankWriter::for_test().split_rows_for_offload("unmapped");
        for id in [7, 8, 9] {
            assert!(producer.stage(&row(id)));
        }
        assert_eq!(producer.pending(), 3);
        assert_eq!(producer.hand_off(labels()), TopVolumeHandoff::Sent(3));
        assert_eq!(producer.pending(), 0);
        let batch = rx.try_recv().expect("the batch must reach the writer end");
        assert_eq!(batch.rows(), 3);
        let outcome = writer.write_batch(batch);
        assert_eq!(outcome.staged, 3);
        assert_eq!(outcome.appended, 3);
        assert_eq!(outcome.append_failed, 0);
        // `for_test` has no ILP sender, so nothing LANDS -- and that must be
        // reported as zero, never forged into a success.
        assert_eq!(outcome.landed, 0);
        // The writer returned the vector; the next sweep reuses it.
        let spare = producer
            .spares
            .try_recv()
            .expect("the vector must come back");
        assert!(spare.is_empty());
        assert!(spare.capacity() >= TOP_VOLUME_MAX_ROWS_PER_SWEEP);
        assert_eq!(writer.formatter.pending(), 0);
    }

    #[test]
    fn an_empty_sweep_sends_nothing() {
        let (mut producer, _writer, rx) =
            TopVolumeRankWriter::for_test().split_rows_for_offload("unmapped");
        assert_eq!(producer.hand_off(labels()), TopVolumeHandoff::Empty);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_full_queue_drops_the_sweep_and_never_blocks() {
        let (mut producer, _writer, _rx) =
            TopVolumeRankWriter::for_test().split_rows_for_offload("unmapped");
        for _ in 0..TOP_VOLUME_ROW_QUEUE_DEPTH {
            assert!(producer.stage(&row(7)));
            assert_eq!(producer.hand_off(labels()), TopVolumeHandoff::Sent(1));
        }
        assert!(producer.stage(&row(7)));
        assert!(producer.stage(&row(8)));
        // If this blocked, the test would hang rather than fail -- itself the
        // assertion.
        assert_eq!(producer.hand_off(labels()), TopVolumeHandoff::QueueFull(2));
        assert_eq!(producer.pending(), 0, "dropped, never retained");
        assert!(producer.rows.capacity() >= TOP_VOLUME_MAX_ROWS_PER_SWEEP);
    }

    #[test]
    fn hand_off_to_a_gone_writer_drops_the_sweep() {
        let (mut producer, _writer, rx) =
            TopVolumeRankWriter::for_test().split_rows_for_offload("unmapped");
        drop(rx);
        assert!(producer.stage(&row(7)));
        assert_eq!(producer.hand_off(labels()), TopVolumeHandoff::SinkGone(1));
        assert_eq!(producer.pending(), 0);
    }

    #[test]
    fn staging_refuses_past_the_row_bound_without_reallocating() {
        let (mut producer, _writer, _rx) =
            TopVolumeRankWriter::for_test().split_rows_for_offload("unmapped");
        let capacity = producer.rows.capacity();
        for _ in 0..TOP_VOLUME_MAX_ROWS_PER_SWEEP {
            assert!(producer.stage(&row(7)));
        }
        assert!(!producer.stage(&row(7)), "one past the bound is refused");
        assert_eq!(producer.pending(), TOP_VOLUME_MAX_ROWS_PER_SWEEP);
        assert_eq!(producer.rows.capacity(), capacity, "the vector never grew");
        assert_eq!(producer.discard_pending(), TOP_VOLUME_MAX_ROWS_PER_SWEEP);
        assert_eq!(producer.pending(), 0);
    }

    #[test]
    fn the_hand_off_uses_try_send_and_never_a_blocking_send() {
        let src = include_str!("top_volume_rank_persistence.rs");
        let start = src
            .find("pub fn hand_off(&mut self")
            .expect("hand_off must exist");
        let body = &src[start..];
        let end = body.find("pub fn discard_pending").unwrap_or(body.len());
        let body = &body[..end];
        assert!(body.contains(".try_send(batch)"));
        assert!(
            !body.contains(".send(batch)"),
            "a blocking send re-couples the drain"
        );
    }
}

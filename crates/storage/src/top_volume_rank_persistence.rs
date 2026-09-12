//! `top_volume_rank` table — a queryable record of WHICH option contracts
//! were the busiest, and whether we were actually watching them.
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
//! and dies with the process. That is correct for the DECISION path — a
//! trading decision never reads the database — but it means three questions
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
//! designated `ts` is the SNAPSHOT boundary — a whole second — so a
//! re-emitted snapshot UPSERTs in place rather than duplicating (the
//! scoreboard mechanic). `rank` is deliberately NOT in the DEDUP key: it is
//! the value being recorded, and a contract appears at most once per
//! (snapshot, timeframe, family) already.
//!
//! ## Schema
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS top_volume_rank (
//!     ts TIMESTAMP, tf SYMBOL, family SYMBOL, feed SYMBOL,
//!     segment SYMBOL, rank LONG, security_id LONG,
//!     underlying_id LONG, volume LONG, delta_units LONG,
//!     lot_size LONG, window_lots_milli LONG,
//!     gain_pct DOUBLE,
//!     subscribed BOOLEAN
//! ) timestamp(ts) PARTITION BY HOUR
//!   DEDUP UPSERT KEYS(ts, tf, family, feed, security_id, segment);
//! ```
//!
//! `PARTITION BY HOUR` matches `ticks` and `market_depth` — this is
//! high-frequency snapshot data and the hour is the archival unit that makes
//! same-day archive-then-drop possible. `underlying_id` is stored as the
//! numeric id we actually hold, NOT a name: the ranking layer never sees a
//! symbol, and inventing one here would be fabrication. Join to
//! `instrument_lifecycle` for names.
//!
//! `window_lots_milli` is THE SORT KEY and `volume` is not, which is the one
//! thing a reader of this table has to know. `volume` is the vendor's
//! CUMULATIVE day volume for the contract -- it only ever rises, so ordering
//! by it would rank "busy since 09:15", not "busy now". `window_lots_milli`
//! is the lots traded INSIDE the window that just closed, normalised by lot
//! size and scaled by 1000 (integer milli-lots, never a float): a 1s row
//! measures one second, a 5s row measures five. Both columns are stored so a
//! row can be checked against the ranking that produced it.
//!
//! ## Honest volume
//!
//! ⚠ EVERY FIGURE IN THIS SECTION WAS RE-DERIVED 2026-09-12, and the previous
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
//!   sweep that is **71.6M rows, ~6.30 GB** at the ~88 B row width — roughly 5x
//!   the old figure and ~2% of the ~307 GB a session already writes.
//!
//!   ⚠ CORRECTED 2026-09-12: this read "22,440 + 7,480 + 4,488 + 374 = ~34,782",
//!   which is a 22,440-second window — i.e. a 15:29 close. The window is
//!   `TICK_PERSIST_END` 56,400 − `TICK_PERSIST_START` 33,300 = **23,100 s**, as
//!   `the_capture_window_closes_after_the_1539_minute` pins. Understated 2.9%.
//!   Small, and corrected because every figure below is derived from it.
//!
//! **The 2,000 figure is Assumed and is the one number here worth measuring.**
//! It has never been read: the 1-second traded population was measured once
//! (3,187,232 rows for a whole session under the 250 cut, which tells us
//! nothing about the uncut count), and the 3s/5s/1m figures were never
//! measured at all because the source partitions were archived to S3 and
//! dropped from EBS before they could be. `SELECT tf, count(*) FROM
//! top_volume_rank WHERE ts IN today() GROUP BY tf` on the first session with
//! this build is what turns the range into a number.
//!
//! The `5s` rows are numerically a subset of the `1s` rows and are kept
//! anyway because they mean something different: a `5s` row is the ranking
//! that ACTUALLY DROVE a re-steer decision, which is the row an audit wants.
//! The same now holds for `3s` and `1m` against the boards they describe.
//!
//! RETENTION, stated because it is a standing commitment and not a one-day
//! cost: `HOUR_PARTITIONED_TABLES` membership puts this table in
//! `RetentionClass::MarketData`, whose window is `market_data_hot_days`
//! (default 15). ⚠ CORRECTED 2026-09-12: this said "**92 GB resident** … about
//! 15%", which multiplied the per-session figure by 15. `market_data_hot_days`
//! is 15 CALENDAR days, and 15 calendar days is roughly **11 trading
//! sessions** — the volume writes nothing on a weekend. At the corrected
//! ~6.30 GB/session that is **≈69 GB resident** on the 600 GB volume, about
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
use tracing::{error, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;

/// QuestDB table name — one row per (snapshot, timeframe, family, contract).
///
/// ⚠ RENAMED 2026-09-12 from `top_volume_rank` to `top_volume` (operator:
/// "dotnt make it as top volume rank table meake the table name as top volume
/// alone"). The rows already written under the old name are CARRIED FORWARD by
/// [`LEGACY_TOP_VOLUME_RANK_TABLE`] below, never abandoned — a second table
/// holding half the history is the split this repo's own rename helper exists
/// to detect and report.
pub const TOP_VOLUME_RANK_TABLE: &str = "top_volume";

/// The name this table was created under until 2026-09-12.
///
/// `ensure_top_volume_rank_table` runs a one-shot `RENAME TABLE` from this to
/// [`TOP_VOLUME_RANK_TABLE`] BEFORE its CREATE, so a box that already holds
/// rows carries them into the new name instead of stranding them in a table
/// that is no longer swept by the partition manager. The rename fails on every
/// boot after the first, and that failure is EXPECTED and not an error — see
/// `try_rename_legacy_table`, which additionally reports a SPLIT if both names
/// somehow exist at once.
pub const LEGACY_TOP_VOLUME_RANK_TABLE: &str = "top_volume_rank";

/// DEDUP key. Designated `ts` FIRST (2026-04-28 regression rule); `segment`
/// alongside `security_id` (I-P1-11 — the bare id is reused across segments
/// and two instruments would silently collapse into one row); `feed` in-key
/// (operator override 2026-06-28, feed-in-key EVERYWHERE).
///
/// Declared as a `DEDUP_KEY_*` const rather than an inline literal because
/// `dedup_segment_meta_guard` discovers keys by scanning for that name
/// pattern — an inline literal would put this key OUTSIDE the guard.
///
/// `rank` is deliberately ABSENT: it is the recorded value, not part of the
/// identity, and including it would let one contract occupy several rows in
/// one snapshot as its rank moved between a retry and its re-emit.
pub const DEDUP_KEY_TOP_VOLUME_RANK: &str = "ts, tf, family, feed, security_id, segment";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Snapshot cadence label — the `tf` SYMBOL column. Stable wire strings.
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
    /// Every whole second — the fine-grained record.
    OneSecond = 0,
    /// Every three seconds (operator 2026-09-12).
    ThreeSecond = 1,
    /// Every five seconds — the boundary at which the depth set is re-steered,
    /// so these rows are the ones that actually drove a subscription decision.
    FiveSecond = 2,
    /// Every minute (operator 2026-09-12) — the coarse frame, and the only one
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
    /// `ALL.len()` — an under-count there would alias two cadences onto one
    /// baseline and make both boards report a window neither one measured.
    ///
    /// The const assert below proves this list is in ordinal order with no
    /// gaps and no duplicates. It does NOT prove the list is COMPLETE: no
    /// stable-Rust construct counts an enum's variants (`variant_count` is
    /// nightly), so a fifth variant added here and nowhere else still
    /// compiles. What stops that reaching production is
    /// [`Self::slot`] — a checked index that REFUSES an unlisted cadence and
    /// counts the refusal, instead of indexing out of bounds on the frame
    /// drain, where `panic = "abort"` would take the process down mid-session.
    pub const ALL: [Self; 4] = [
        Self::OneSecond,
        Self::ThreeSecond,
        Self::FiveSecond,
        Self::OneMinute,
    ];

    /// The array slot for this cadence — the `#[repr(u8)]` discriminant.
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
    /// `None` means a variant exists that `ALL` does not list — the one
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

    /// Stable wire label. Never reworded — it is a persisted SYMBOL value.
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

    /// The named QuestDB view this cadence is read through.
    ///
    /// Lives HERE, beside the label it filters on, because the view's `WHERE
    /// t.tf = '<label>'` clause and the view's own name are one claim: a view
    /// called `top_volume_3s` that filters `tf = '5s'` is wrong in a way
    /// no reader of either file alone could see. `console_views` builds the
    /// DDL from this pair rather than from a second enum of its own, which is
    /// what the deleted `TopVolumeCadence` was.
    #[must_use]
    pub const fn view_name(self) -> &'static str {
        match self {
            Self::OneSecond => "top_volume_1s",
            Self::ThreeSecond => "top_volume_3s",
            Self::FiveSecond => "top_volume_5s",
            Self::OneMinute => "top_volume_1m",
        }
    }
}

/// `ALL` is in ordinal order, with no gaps and no duplicates — so
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
pub struct TopVolumeRankRow {
    /// Designated timestamp — the SNAPSHOT boundary in IST nanoseconds.
    /// A whole second, so a re-emit UPSERTs in place.
    pub snapshot_ts_ist_nanos: i64,
    /// Snapshot cadence (`1s` / `3s` / `5s` / `1m`).
    pub cadence: SnapshotCadence,
    /// Option family — stock and index options are ranked in SEPARATE
    /// leaderboards, and the column records which one this row came from.
    pub family: &'static str,
    /// Feed that produced the volume (`dhan`).
    pub feed: &'static str,
    /// Segment SYMBOL string (`NSE_FNO`, ...).
    ///
    /// `&'static str`, like `family` and `feed` beside it. It was `String`
    /// until 2026-09-08, and the producer filled it with
    /// `contract.segment.as_str().to_string()` — where `ExchangeSegment::as_str`
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
    /// 1-based position within its family at this snapshot.
    pub rank: i64,
    /// The contract's own security id.
    pub security_id: i64,
    /// The underlying's numeric id. NOT a name — see the module header.
    pub underlying_id: i64,
    /// Cumulative day volume as observed, AFTER the monotonicity gate.
    pub volume: i64,
    /// Traded UNITS inside the window that just closed — the NUMERATOR of
    /// the rank key, and the number an operator calls "net volume".
    ///
    /// Added 2026-09-12. Until then the table stored the cumulative
    /// `volume` and the derived `window_lots_milli`, and NEITHER input of
    /// the division was present: a reader looking at a row could not see
    /// the traded quantity the rank was computed from, and could not
    /// recover it, because `window_lots_milli = delta * 1000 / lot_size`
    /// is one equation in two unknowns. Consecutive rows do not rescue it
    /// either — a contract only appears while it is inside the top
    /// `TOP_VOLUME_RANK_PER_FAMILY`, so the `volume` series for any one
    /// contract has holes exactly where it stopped being interesting.
    ///
    /// `u32` at the source (the vendor's counter width); a value that
    /// cannot fit `i64` is impossible, but the projection converts rather
    /// than casts so the impossibility is enforced rather than assumed.
    pub delta_units: i64,
    /// Units per contract, from the day's master — the DENOMINATOR of the
    /// rank key.
    ///
    /// Added 2026-09-12, for the same reason as `delta_units` above: it is
    /// the other half of the division and it was not recoverable from the
    /// stored row. Sourced from the `LOT_SIZE` column of Dhan's
    /// `api-scrip-master-detailed.csv` and carried on `ContractOwner`, so
    /// it is the SAME value the ranking divided by rather than a re-lookup
    /// that could disagree with it.
    ///
    /// Guaranteed non-zero: a leg whose master row carries no lot size is
    /// refused at the map build (`LegRefusal::MissingLotSize`) and again in
    /// `window_lots_milli`, which returns `None` rather than dividing. So a
    /// zero in this column would mean the guarantee broke, and is worth
    /// seeing.
    pub lot_size: i64,
    /// **The RANK KEY**: lots traded in the window that just closed, x 1000.
    ///
    /// Added 2026-09-09. Until then the table stored only `volume`, the
    /// CUMULATIVE day count -- which has not been the sort key since
    /// 2026-09-07, when the scope lock moved ranking to lots-in-window. So a
    /// reader could see rank 1 hold less cumulative volume than rank 40 and
    /// have nothing in the row to explain it. This column IS the number the
    /// order was computed from, so the ordering is checkable from the table
    /// alone rather than taken on trust.
    ///
    /// Milli-lots, so 1_000 is one lot. `u64` at the source; a value that
    /// cannot fit `i64` is refused by the projection rather than wrapped
    /// negative.
    pub window_lots_milli: i64,
    /// The UNDERLYING's percentage change from its previous close, or `None`
    /// when that is not knowable yet.
    ///
    /// `None` is written as an ABSENT ILP field, which QuestDB stores as NULL
    /// — the `tick_persistence` per-feed-optional-column precedent. It is a
    /// real state, not a defect: both inputs live in RAM stores fed only by
    /// ticks (`SpotPriceStore`, `PrevCloseStore`), so an underlying that has
    /// not printed since boot has neither. Equities give one stale ~08:30
    /// snapshot and then nothing until the 09:07 auction print, and a
    /// mid-session redeploy restarts both stores empty.
    ///
    /// # Why NULL and not a dropped row (2026-09-12)
    ///
    /// Until today an unknown gain DELETED the whole row. `gain_pct` is a leaf
    /// DISPLAY column: it is not in the DEDUP key, the views pass it through
    /// with no arithmetic, and nothing orders by it. So a `None` cost the
    /// table the contract's VOLUME — the one thing it exists to record —
    /// because a column beside it could not be computed.
    ///
    /// `Some` is still proven FINITE by the projection; a `NaN` never reaches
    /// this field, it becomes `None` and is counted.
    pub gain_pct: Option<f64>,
    /// Whether this contract actually held a depth subscription at this
    /// snapshot. The column that makes the table an audit rather than trivia.
    ///
    /// Read from the steering loops' LAST publish, which happens once a
    /// minute per pool — so inside one minute the value can lag a swap by up
    /// to that minute, and always in the understating direction (a contract
    /// just swapped IN reads `false` until the next publish). See
    /// `depth_subscription_view` for why that direction is the honest one.
    pub subscribed: bool,
}

/// The idempotent `CREATE TABLE` DDL for `top_volume_rank`. Pure.
#[must_use]
pub fn top_volume_rank_create_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {TOP_VOLUME_RANK_TABLE} (\
            ts            TIMESTAMP, \
            tf            SYMBOL, \
            family        SYMBOL, \
            feed          SYMBOL, \
            segment       SYMBOL, \
            rank          LONG, \
            security_id   LONG, \
            underlying_id LONG, \
            volume        LONG, \
            delta_units   LONG, \
            lot_size      LONG, \
            window_lots_milli LONG, \
            gain_pct      DOUBLE, \
            subscribed    BOOLEAN\
        ) timestamp(ts) PARTITION BY HOUR \
        DEDUP UPSERT KEYS({DEDUP_KEY_TOP_VOLUME_RANK});"
    )
}

/// Every non-designated column, for the per-column self-heal ALTER manifest.
/// Kept beside the DDL so `table_schema_lockstep_guard` can compare them.
const TOP_VOLUME_RANK_COLUMNS: &[(&str, &str)] = &[
    ("tf", "SYMBOL"),
    ("family", "SYMBOL"),
    ("feed", "SYMBOL"),
    ("segment", "SYMBOL"),
    ("rank", "LONG"),
    ("security_id", "LONG"),
    ("underlying_id", "LONG"),
    ("volume", "LONG"),
    ("delta_units", "LONG"),
    ("lot_size", "LONG"),
    ("window_lots_milli", "LONG"),
    ("gain_pct", "DOUBLE"),
    ("subscribed", "BOOLEAN"),
];

/// The full idempotent statement list: CREATE, then per-column
/// `ADD COLUMN IF NOT EXISTS`, then `DEDUP ENABLE`. Never a DROP. Pure, so
/// the ordering is unit-testable without a live QuestDB.
#[must_use]
pub fn top_volume_rank_ensure_statements() -> Vec<String> {
    let mut statements = vec![top_volume_rank_create_ddl()];
    for (col, ty) in TOP_VOLUME_RANK_COLUMNS {
        statements.push(format!(
            "ALTER TABLE {TOP_VOLUME_RANK_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty};"
        ));
    }
    statements.push(format!(
        "ALTER TABLE {TOP_VOLUME_RANK_TABLE} DEDUP ENABLE \
         UPSERT KEYS({DEDUP_KEY_TOP_VOLUME_RANK});"
    ));
    statements
}

/// Creates the `top_volume_rank` table if absent (schema-self-heal order:
/// CREATE -> per-column ALTER -> DEDUP ENABLE; never a table drop).
///
/// Fail-SOFT per statement: every failure logs at `error!` with
/// `STORAGE-GAP-03` and the walk continues, so one refused statement never
/// hides the next. The honest consequence, stated rather than hidden: a
/// failed ensure leaves the table to be auto-created by the first ILP write
/// WITHOUT `DEDUP UPSERT KEYS` — a duplicate-row window until a later ensure
/// succeeds. Blocking the boot instead would trade a duplicate-row window
/// for no session at all.
///
/// Returns `true` only when EVERY statement was accepted, so the boot's
/// bounded retry loop (`candle_ddl_boot::run_live_table_ddl_at_boot`) can
/// re-run it beside `ticks` and `market_depth`. Until 2026-09-08 this fn
/// returned `()` and had ZERO production callers — the table was written by
/// its offload writer every second and ensured by nothing, so a fresh
/// volume (the 2026-09-08 nuke) would have let the first ILP row auto-create
/// it with no DEDUP key at all.
// TEST-EXEMPT: live-QuestDB DDL runner; the statement list it sends is pure and is asserted by the ensure-statement tests below, and the boot call site is pinned by crates/app/tests/ensure_ddl_boot_wiring_guard.rs.
pub async fn ensure_top_volume_rank_table(questdb_config: &QuestDbConfig) -> bool {
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
                "STORAGE-GAP-03: HTTP client build failed — top_volume_rank not \
                 ensured (the first ILP write may auto-create it WITHOUT dedup, \
                 a duplicate-row window until the next successful boot)"
            );
            return false;
        }
    };
    // BEFORE the CREATE, deliberately. A `CREATE TABLE IF NOT EXISTS
    // top_volume` on a box that already holds `top_volume_rank` rows would
    // succeed against an EMPTY new table and strand the old one: no longer in
    // `HOUR_PARTITIONED_TABLES`, no longer swept, growing forever on a volume
    // this repository has already filled twice. Renaming first carries the
    // history across.
    //
    // The refusal is the NORMAL case from the second boot onward, which is why
    // this goes through `try_rename_legacy_table` rather than the DDL loop
    // below — that loop fires a coded error and increments a persist-error
    // counter, and an error whose steady state is "one per boot, forever"
    // trains the operator to discount the counter. The helper additionally
    // probes whether the legacy table still exists on a refusal and reports a
    // SPLIT, which is the only case here that needs a human.
    //
    // The `== Split` arm is NOT optional, and an earlier draft of this call
    // discarded the verdict with `let _ =`. Both tables present means the
    // history is halved — new rows in `top_volume`, everything before the
    // rename stranded in `top_volume_rank`, which is no longer in
    // `HOUR_PARTITIONED_TABLES` and so is never swept. That is the exact
    // failure the comment above is about, and swallowing the verdict made it
    // SILENT. Same shape as the three sibling renames
    // (`spot_1m_rest_persistence`, `option_chain_1m_persistence`,
    // `option_contract_1m_rest_persistence`): the helper deliberately does not
    // log `Split` because the caller owns the coded error.
    if crate::http_client::try_rename_legacy_table(
        &client,
        &base_url,
        LEGACY_TOP_VOLUME_RANK_TABLE,
        TOP_VOLUME_RANK_TABLE,
    )
    .await
        == crate::http_client::LegacyRenameOutcome::Split
    {
        metrics::counter!(
            "tv_top_volume_rank_rows_discarded_total",
            "stage" => "legacy_table_split"
        )
        .increment(0);
        error!(
            code = "STORAGE-GAP-03",
            stage = "legacy_table_split",
            legacy_table = LEGACY_TOP_VOLUME_RANK_TABLE,
            current_table = TOP_VOLUME_RANK_TABLE,
            "STORAGE-GAP-03: both the legacy and current top-volume tables exist \
             — the ranking history is SPLIT across two tables. New rows land in \
             the current name; everything written before the rename stays in the \
             legacy one, which is NOT in the hour-partitioned retention list and \
             is therefore never swept. Neither is dropped; merging is an operator \
             decision"
        );
    }

    let mut all_accepted = true;
    for ddl in &top_volume_rank_ensure_statements() {
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
                    %status,
                    ddl = ddl.as_str(),
                    body = %body.chars().take(200).collect::<String>(),
                    "STORAGE-GAP-03: top_volume_rank DDL returned non-2xx \
                     (dedup may be missing — duplicate-row window)"
                );
            }
            Err(err) => {
                all_accepted = false;
                error!(
                    code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                    stage = "ensure_ddl",
                    ?err,
                    ddl = ddl.as_str(),
                    "STORAGE-GAP-03: top_volume_rank DDL request failed"
                );
            }
        }
    }
    all_accepted
}

/// ILP-over-HTTP conf — per-flush server ACK (the 2026-07-05
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
/// retained — a server-REJECTED row kept across flushes replays forever and
/// poisons every later flush with it. Discarding is safe HERE specifically
/// because these rows are an observability record of a ranking that is
/// recomputed from scratch on the next tick: nothing downstream depends on
/// a snapshot having been written, and the loss is loud.
pub struct TopVolumeRankWriter {
    sender: Option<Sender>,
    buffer: Buffer,
    pending: usize,
    /// Set by [`TopVolumeRankWriter::split_for_offload`]. When present, `flush`
    /// hands the buffer to the writer thread instead of touching the network.
    ///
    /// `None` means this writer still flushes synchronously — the shape that
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
    /// Production constructor — ILP-over-HTTP sender, lazy on failure.
    #[must_use]
    // TEST-EXEMPT: production ILP-connect constructor; the lazy contract and every append/flush path are covered via for_test() below.
    pub fn new(config: &QuestDbConfig) -> Self {
        // Seed the discard series at zero BEFORE the first row can be
        // dropped. A counter never incremented is ABSENT from the exporter,
        // not zero — and an absent series reads as health. If this name is
        // ever EMF-selected, the CloudWatch agent's dropped-first-sample rule
        // turns that into total silence on the one episode that matters; that
        // rule cost 104,540 depth rows their classification on 2026-08-28.
        metrics::counter!("tv_top_volume_rank_rows_discarded_total").increment(0);
        let conf = top_volume_rank_ilp_http_conf(config);
        match Sender::from_conf(&conf) {
            Ok(s) => {
                let b = s.new_buffer();
                Self {
                    sender: Some(s),
                    buffer: b,
                    pending: 0,
                    offload: None,
                    retained_spans: 0,
                }
            }
            Err(err) => {
                warn!(
                    ?err,
                    "top_volume_rank writer: QuestDB unreachable — buffering locally"
                );
                Self {
                    sender: None,
                    buffer: Buffer::new(ProtocolVersion::V1),
                    pending: 0,
                    offload: None,
                    retained_spans: 0,
                }
            }
        }
    }

    /// Test constructor — disconnected writer, empty buffer.
    #[must_use]
    // TEST-EXEMPT: test-only helper used by the append/flush unit tests below.
    pub fn for_test() -> Self {
        Self {
            sender: None,
            buffer: Buffer::new(ProtocolVersion::V1),
            pending: 0,
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
    /// half-written row can never poison the buffer — see [`Self::append_row`].
    ///
    /// # Errors
    /// Propagates ILP buffer errors (table/column append failure).
    fn write_row(buffer: &mut Buffer, r: &TopVolumeRankRow) -> Result<()> {
        buffer
            .table(TOP_VOLUME_RANK_TABLE)
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
            .column_i64("rank", r.rank)
            .context("rank")?
            .column_i64("security_id", r.security_id)
            .context("security_id")?
            .column_i64("underlying_id", r.underlying_id)
            .context("underlying_id")?
            .column_i64("volume", r.volume)
            .context("volume")?
            .column_i64("delta_units", r.delta_units)
            .context("delta_units")?
            .column_i64("lot_size", r.lot_size)
            .context("lot_size")?
            .column_i64("window_lots_milli", r.window_lots_milli)
            .context("window_lots_milli")?;
        // OPTIONAL column -- omitted (NULL) when the underlying's gain is not
        // knowable yet. Absent-field-is-NULL is the `tick_persistence`
        // per-feed-optional precedent, and the row still carries 4 symbols and
        // 7 fields so it can never become field-less.
        if let Some(gain_pct) = r.gain_pct {
            buffer
                .column_f64("gain_pct", gain_pct)
                .context("gain_pct")?;
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
    /// every later `.table()` call — so one bad row silently killed this
    /// writer for the rest of the process. `at()` is the realistic trigger: it
    /// validates the timestamp only after the table, symbols and columns are
    /// already in the buffer.
    ///
    /// Nothing recovered it, either. `pending` is bumped only AFTER the last
    /// `?`, so a part-way failure leaves it at 0 — and `flush` early-returns on
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
    pub fn append_row(&mut self, r: &TopVolumeRankRow) -> Result<()> {
        // A marker may only be set on an empty buffer or after `at` — i.e.
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
                // Handed off, or held for the next flush — neither is an error
                // to the caller. `QueueFull` in particular is BACKPRESSURE:
                // reporting it as a failure would decay a health signal for
                // rows that are still safely held.
                TopVolumeOffloadOutcome::Sent(_) | TopVolumeOffloadOutcome::QueueFull(_) => Ok(()),
                TopVolumeOffloadOutcome::WidthCapped(dropped) => anyhow::bail!(
                    "top_volume_rank: writer thread stayed behind — {dropped} pending \
                     row(s) dropped (no spill tier for a periodic snapshot)"
                ),
                TopVolumeOffloadOutcome::SinkGone(dropped) => anyhow::bail!(
                    "top_volume_rank: writer thread is gone — {dropped} pending row(s) dropped"
                ),
            };
        }
        if self.sender.is_none() {
            let dropped = self.discard_pending();
            anyhow::bail!(
                "top_volume_rank: no ILP sender (QuestDB unreachable) — \
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
                    "top_volume_rank ILP flush failed — {dropped} pending row(s) \
                     discarded (poisoned-buffer defense)"
                )))
            }
            // Unreachable (checked above) — treated as the no-sender arm.
            None => {
                let dropped = self.discard_pending();
                anyhow::bail!(
                    "top_volume_rank: sender vanished — {dropped} pending row(s) discarded"
                );
            }
        }
    }

    /// Splits this writer into a drain-side PRODUCER and a thread-side SINK.
    ///
    /// After this, `flush` hands the buffer to a bounded queue and returns
    /// without touching the network. That is the whole point: the caller is the
    /// frame drain, and a 5,000 ms `request_timeout` on the drain task stalls
    /// the fold — after which the socket receive buffer fills and Dhan skips a
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
            // Unreachable — `flush` checks. Treated as the gone arm rather than
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
                // — the next flush retries. This arm is what makes the bounded
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
                    // No spill tier for this table — see
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
    fn discard_pending(&mut self) -> usize {
        let dropped = self.pending;
        if dropped > 0 {
            metrics::counter!("tv_top_volume_rank_rows_discarded_total").increment(dropped as u64);
            // The counter is NOT EMF-selected — deliberately. Shipping a new
            // metric name costs ~$0.30/mo against a September forecast of
            // $142.24 with the automatic STOP_EC2_INSTANCES line at $135.00,
            // and this table is an observability record whose loss costs no
            // tick. `loss_counter_visibility_guard` accepts the LOGGED route,
            // and this is it: the error! below sits beside the increment.
            error!(
                code = ErrorCode::StorageGap03AuditWriteFailed.code_str(),
                metric = "tv_top_volume_rank_rows_discarded_total",
                dropped,
                "STORAGE-GAP-03: top_volume_rank rows discarded after a failed \
                 flush — the ranking record has a hole for those snapshots. No \
                 tick is lost by this: the leaderboard is in RAM and the next \
                 snapshot rebuilds it."
            );
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
/// ⚠ The rows/s figure this doc used to carry is RETIRED, not updated. It
/// read "~600 rows/s at most (500/s from the 1 s arm, ~100/s from the 5 s
/// arm)", which was 250 x 2 families x 2 cadences — every term of which the
/// 2026-09-12 change moved: the per-family cut is gone and there are four
/// cadences. There is no longer a constant to quote. The row count per sweep
/// is one per contract that TRADED in the window, bounded above by
/// `TOP_VOLUME_MAX_ROWS_PER_SWEEP` and in practice by the market.
///
/// (Until 2026-09-08 the line before that read "250 rows at 1 s and 5 at 5 s,
/// ~255 rows/s" — the 5 s figure was the depth-200 SOCKET count, not the row
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
/// ⚠ RE-DERIVED 2026-09-12 for the cadence expansion, and the intra-doc link
/// above was BROKEN — it read `depth_persistence::MAX_TOP_VOLUME_RETAINED_FLUSH_SPANS`,
/// a path that does not exist, so the "matching" claim pointed at nothing.
///
/// The CONSTANT does not move; what moved is what it buys in WALL-CLOCK time,
/// and that is worth stating because the number is a SPAN count and reads like
/// a duration. A span is one flush attempt that found the queue full, so the
/// tolerance is `spans ÷ flush rate`, and the flush rate is the cadence count:
///
/// | | flushes in the worst second | tolerance past a full queue |
/// |---|---|---|
/// | 2 cadences (1 s, 5 s), before 2026-09-12 | 2 | ~3 s |
/// | 4 cadences (1 s, 3 s, 5 s, 1 m), now | 4 | ~2.2 s |
///
/// One flush per `snapshot_top_volume` call, NOT one per family — the flush
/// sits outside the family loop (`dhan_feed_stack::snapshot_top_volume`), so
/// the four arms coinciding at the minute mark is four attempts, not eight.
/// Verified in source 2026-09-12 rather than assumed; a hostile review of this
/// change asserted the per-family doubling and it is not there.
///
/// So the stall a healthy queue absorbs shrank from ~7 s to ~6.2 s
/// (4 queued batches + the retained spans). That is a REDUCTION and it is not
/// raised here, for two reasons stated plainly rather than waved past: the
/// failure it guards is a QuestDB stall, which this repository has on record
/// at the 2026-08-25 and 2026-09-02 scale — MINUTES, not seconds, so neither 6
/// nor 7 saves the snapshot; and what is lost when it fires is one snapshot of
/// a leaderboard that is rebuilt from RAM a second later, never a tick. Raising
/// it trades a bounded, logged, tickless hole for unbounded producer memory on
/// the drain, which is the worse side of that trade.
pub const MAX_TOP_VOLUME_RETAINED_FLUSH_SPANS: u32 = 2;

/// Byte ceiling on the buffer the producer retains while the writer is behind.
///
/// ⚠ RE-DERIVED 2026-09-12, and the previous value was a live row-drop.
///
/// It was a flat 4 MiB, justified as "~58 seconds of backlog" at a rate of
/// "~600 rows/s of ~120 B ILP ≈ 72 KB/s". Both halves of that came from the
/// top-250-per-family cut. With the cut removed (operator: "pick the entire
/// options contracts") a SINGLE worst-case sweep is
/// [`TOP_VOLUME_MAX_ROWS_PER_SWEEP`] rows — 6 MB — so one sweep alone breached
/// a 4 MiB ceiling. Past the ceiling this producer DROPS: there is no spill
/// tier for this table, unlike ticks and depth. The stale figure was not a
/// stale comment; it was the number the drop decision was made against.
///
/// Now DERIVED from the population rather than asserted, so removing a cut
/// cannot silently invalidate it again. Still well under the depth path's
/// 32 MiB, which is the property the sizing test pins.
///
/// ⚠ **DERIVED since 2026-09-12 — both terms used to be bare literals.**
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
/// Naming it is the honest middle — a third family is now a visible edit here
/// rather than an invisible factor inside an arithmetic expression.
const OPTION_FAMILIES: usize = 2;
const TOP_VOLUME_MAX_ROWS_PER_SWEEP: usize =
    tickvault_common::constants::TOP_VOLUME_PERSIST_PER_FAMILY * OPTION_FAMILIES;
/// Worst-case ILP line width for one `top_volume` row, DERIVED below.
///
/// # ⚠ CORRECTED 2026-09-12 — this was `120`, and it was wrong by ~2.2×
///
/// The old value was labelled "measured … rounded up" and no test measured it.
/// Counted against the real `write_row` line — 4 symbols, 7 `i64` fields, an
/// optional `f64`, a bool and a 19-digit nanosecond stamp — a worst-case row is
/// **~265 B** by hand and a typical one ~233 B:
///
/// ```text
///   top_volume                                       11
///   ,tf=1m,family=stock,feed=dhan,segment=NSE_FNO    ~45
///   rank=…i, security_id=…i, underlying_id=…i,      ~ 79
///   volume=…i, delta_units=…i, lot_size=…i,         ~ 58
///   window_lots_milli=…i,                            ~33
///   gain_pct=-12.345678901234567,                    ~27
///   subscribed=t                                      13
///   <19-digit nanos>\n                                20
/// ```
///
/// The consequence was not cosmetic. `MAX_TOP_VOLUME_PRODUCER_BUFFER_BYTES` is
/// this number times the sweep size times [`TOP_VOLUME_SWEEPS_HELD`], and past
/// that ceiling this producer **DROPS** — this table has no spill tier, so a
/// dropped row is gone. At 120 B the ceiling claimed to hold two worst-case
/// sweeps while actually holding less than one.
///
/// ⚠ **And the hand-derivation above was ITSELF short.** A first pass at this
/// correction set the constant to 300 on the strength of that ~265 count, and
/// `the_assumed_ilp_row_width_covers_a_real_worst_case_line` — written in the
/// same change — immediately failed it: a real worst-case line **measures
/// 324 B**. The count under-estimated the `i64::MAX` fields, which are 19
/// digits each across four columns.
///
/// So the original constant was wrong by **2.7×**, not the 2.2× the hand count
/// suggested, and the reusable half is that **a width is a measurement**: two
/// successive careful derivations both came in low, and only running the line
/// through `write_row` settled it. That is why the guard is a test that builds
/// the widest row this writer can emit, not another literal.
///
/// 384 rather than 324: headroom for a longer symbol value or a wider `f64`
/// repr without a third under-count, and the cost is only reserved address
/// space in a buffer that is flushed every sweep.
const TOP_VOLUME_ILP_ROW_BYTES: usize = 384;
/// Worst-case sweeps the producer may hold before it drops.
///
/// # ⚠ 2 → 1, forced by the corrected width above (2026-09-12)
///
/// This was `2`, and at the wrong 120 B width the ceiling it produced —
/// 12 MB — could not hold even **one** worst-case sweep (50,000 × 324 B =
/// 16.2 MB). So the declared "two sweeps" was never what shipped; the real
/// behaviour was ~0.74 of one, and the vacuous assert could not say so.
///
/// At the corrected width a true two sweeps is 32.4 MB, which collides with
/// the sizing relationship `the_producer_byte_ceiling_is_far_tighter_than_the_depth_path`
/// pins against depth's 32 MiB. One sweep at 384 B is **19.2 MB** — still 60%
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

// ⚠ THE ASSERT THAT USED TO STAND HERE WAS VACUOUS, and that is why the wrong
// constant above survived.
//
// It read `MAX_TOP_VOLUME_PRODUCER_BUFFER_BYTES >= ROWS * BYTES`, where the
// left-hand side is defined as `ROWS * BYTES * 2`. So it asserted `2X >= X` —
// TRUE FOR EVERY VALUE OF `BYTES`, including a value off by a factor of ten. Its
// message named a real hazard ("a single sweep drops rows … a dropped row is
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
     sweep overruns the producer ceiling and this table — which has NO spill \
     tier — drops rows."
);

/// The worst-case ILP line width MEASURED by
/// `the_assumed_ilp_row_width_covers_a_real_worst_case_line`.
///
/// Separate from [`TOP_VOLUME_ILP_ROW_BYTES`] on purpose: that one is the
/// ASSUMPTION the ceiling is sized from, this one is the OBSERVATION it must
/// cover. Two independent terms are what make the asserts below capable of
/// failing — the vacuous assert this replaced compared the ceiling to a factor
/// of itself.
const MEASURED_WORST_CASE_ILP_ROW_BYTES: usize = 324;

// (3) The requirement the ORIGINAL assert's message named and its arithmetic
//     could not check: the ceiling must hold at least one worst-case sweep at
//     the MEASURED width, not at the assumed one.
const _: () = assert!(
    MAX_TOP_VOLUME_PRODUCER_BUFFER_BYTES
        >= TOP_VOLUME_MAX_ROWS_PER_SWEEP * MEASURED_WORST_CASE_ILP_ROW_BYTES,
    "the producer ceiling must hold at least ONE worst-case sweep. Below that, \
     a single sweep drops rows even with a perfectly healthy writer — and this \
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
    /// # Why dropped and not rescued — the one place this differs from depth
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

/// The network half of a split [`TopVolumeRankWriter`] — owns the ILP `Sender`.
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
    /// Zero on any failure — the same contract [`TopVolumeRankWriter::flush`]
    /// has, and for the same reason: a failed write must be reported as a
    /// failure rather than forged into a success.
    ///
    /// # The bounded retry, and why it is here from the start
    ///
    /// `depth_persistence` records the cost of NOT doing this: its
    /// synchronous path carried a single retry from 2026-08-25, the offload
    /// shipped three days later without it, and the measured price on
    /// 2026-09-03 was **41,204 coded flush failures in one session**, every one
    /// reading `Connection reset by peer (os error 104)` — a transport class
    /// the buffer survives intact. Shipping this sink without the retry would
    /// repeat that mistake knowingly.
    ///
    /// Retrying is idempotent BY CONSTRUCTION, which is what makes one attempt
    /// safe rather than merely cheap: [`DEDUP_KEY_TOP_VOLUME_RANK`] is
    /// `ts, tf, family, feed, security_id, segment`, and every one of those is
    /// fixed for a given snapshot — so a first POST that landed before the
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
    /// and the same coded error the synchronous `discard_pending` uses — so an
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
             writer — the ranking record has a hole for those snapshots. No \
             tick is lost by this: the leaderboard is in RAM and the next \
             snapshot rebuilds it."
        );
        batch.buffer.clear();
        batch.rows = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_cadence_is_in_the_all_list() {
        // `ALL.len()` sizes the ranking's per-contract baselines, one slot per
        // cadence. A variant missing from this list under-counts that array,
        // and two cadences would then share a baseline — each board reporting
        // a window neither one measured, with nothing erroring.
        //
        // ⚠ The 2026-09-06 version of this test could not detect that. It
        // iterated `ALL` and matched on the label with a `_ => panic!` arm —
        // but a variant absent from `ALL` is never YIELDED by that loop, so
        // the arm it was supposed to trip could not be reached. It asserted
        // against itself and passed vacuously.
        //
        // What actually bites, in three parts:
        //   1. `from_index` is an exhaustive-by-construction decode — the
        //      slot a new variant takes must be spelled there or it decodes
        //      to `None` below;
        //   2. every slot `0..ALL.len()` must round-trip through
        //      `from_index` → `index`, which catches a gap or a duplicate;
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
            "the slot one past the end must not decode — if it does, a \
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

    fn row() -> TopVolumeRankRow {
        TopVolumeRankRow {
            snapshot_ts_ist_nanos: 1_757_000_000_000_000_000,
            cadence: SnapshotCadence::FiveSecond,
            family: "stock_option",
            feed: "dhan",
            segment: "NSE_FNO",
            rank: 1,
            security_id: 44_321,
            underlying_id: 2885,
            volume: 117_567_970,
            // The two inputs REPRODUCE the key beside them: 8,500 units on a
            // 200-unit lot is 8,500 * 1000 / 200 = 42,500 milli-lots. A
            // fixture whose inputs do not divide back to its own output would
            // let a broken projection look right here.
            delta_units: 8_500,
            lot_size: 200,
            window_lots_milli: 42_500,
            gain_pct: Some(4.25),
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

    /// `rank` is the recorded VALUE, not part of the row's identity. In the
    /// key, one contract could occupy several rows in one snapshot as its
    /// rank moved between an emit and a re-emit — the duplicate the DEDUP
    /// key exists to prevent.
    #[test]
    fn rank_is_deliberately_absent_from_the_dedup_key() {
        assert!(
            !DEDUP_KEY_TOP_VOLUME_RANK
                .split(',')
                .any(|t| t.trim() == "rank"),
            "rank in the key lets one contract hold several rows per snapshot"
        );
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

    /// Families are ranked separately, so both can hold rank 1 in the same
    /// second for different contracts. Without `family` in the key that is
    /// still fine (security_id differs) — but a contract listed in BOTH
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
        let ddl = top_volume_rank_create_ddl();
        assert!(ddl.contains("PARTITION BY HOUR"), "{ddl}");
        assert!(
            ddl.contains(&format!("DEDUP UPSERT KEYS({DEDUP_KEY_TOP_VOLUME_RANK})")),
            "{ddl}"
        );
        assert!(ddl.contains("timestamp(ts)"), "{ddl}");
    }

    /// Schema-self-heal order: CREATE, then every column, then DEDUP ENABLE.
    /// Never a DROP — this table is append-only history.
    #[test]
    fn top_volume_rank_ensure_statements_never_drop_and_end_with_dedup_enable() {
        let statements = top_volume_rank_ensure_statements();
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
        // entry in the CREATE — the opposite of what its name promises, and
        // the harmless direction. A column added to the CREATE and forgotten
        // in the manifest passed green, which is the direction that actually
        // bites: an ALREADY-CREATED table on the box never receives the
        // `ADD COLUMN`, so the first ILP write auto-creates it at whatever
        // type the wire value infers. Dev and prod then hold the same column
        // at different types from the same binary — exactly the drift
        // `table_schema_lockstep_guard`'s header describes.
        //
        // The type half was near-vacuous too: `ddl.contains("LONG")` is true
        // for any table with one LONG column anywhere, so it could not catch a
        // manifest that declared the wrong type. It now compares the type
        // parsed from the column's OWN position in the DDL.
        let ddl = top_volume_rank_create_ddl();
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
                        "column {name} is in the CREATE but NOT in the self-heal manifest — a \
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
        let created = ddl_columns(&top_volume_rank_create_ddl());
        assert_eq!(
            created.len(),
            TOP_VOLUME_RANK_COLUMNS.len() + 1,
            "parsed {created:?} — expected the manifest plus the designated `ts`"
        );
        assert_eq!(created.first().map(|c| c.0.as_str()), Some("ts"));
        assert!(
            created
                .iter()
                .any(|(n, t)| n == "delta_units" && t == "LONG"),
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
    /// rest of the process — and `pending` stayed 0, so `flush`'s early return
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
            // the first comma, so "top_volume," rejects the pre-2026-09-12
            // "top_volume_rank," that a bare "top_volume" would accept.
            "top_volume,",
            "tf=5s",
            "family=stock_option",
            "feed=dhan",
            "segment=NSE_FNO",
            "rank=1i",
            "security_id=44321i",
            "window_lots_milli=42500i",
            "delta_units=8500i",
            "lot_size=200i",
            "underlying_id=2885i",
            "volume=117567970i",
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
    #[test]
    fn every_declared_column_actually_reaches_the_wire() {
        let mut w = TopVolumeRankWriter::for_test();
        w.append_row(&row()).expect("append");
        let line = w.buffer_utf8();
        for (col, _) in TOP_VOLUME_RANK_COLUMNS {
            assert!(
                line.contains(&format!("{col}=")) || line.contains(&format!(",{col}=")),
                "declared column {col} never reached the ILP line: {line}"
            );
        }
    }

    /// The stored inputs must DIVIDE BACK to the stored key.
    ///
    /// `window_lots_milli` is the number the board was ordered by;
    /// `delta_units` and `lot_size` are the two numbers it was computed from.
    /// If the row can carry a trio that does not satisfy its own arithmetic,
    /// the column added "so the ordering is checkable from the table alone"
    /// does not make it checkable.
    #[test]
    fn the_stored_inputs_reproduce_the_stored_rank_key() {
        let r = row();
        assert_eq!(r.lot_size, 200, "the fixture's denominator");
        assert_eq!(r.delta_units, 8_500, "the fixture's numerator");
        assert_eq!(
            r.delta_units * 1_000 / r.lot_size,
            r.window_lots_milli,
            "8,500 units on a 200-unit lot is 42,500 milli-lots"
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
    /// one from being edited without the other — and the drift would be
    /// invisible, because a wrongly-paced snapshot still writes a
    /// well-formed row under a label that reads correct.
    ///
    /// ⚠ The pre-2026-09-12 form of this test did
    /// `as_str().trim_end_matches('s').parse::<u64>()`, which reads `"1m"` as
    /// `"1m"` and PANICS. It was written when every label happened to end in
    /// `s`, and it would have failed the build the moment a minute cadence
    /// arrived — as a parse panic naming nothing, not as a drift report. The
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

    /// Flushing nothing is a no-op, not an error — otherwise a quiet second
    /// produces a spurious failure every time.
    #[test]
    fn flushing_an_empty_buffer_is_not_an_error() {
        let mut w = TopVolumeRankWriter::for_test();
        assert!(w.flush().is_ok());
    }

    /// `subscribed=false` is the whole point of the column — it must be
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

    /// An unknown underlying gain must OMIT the ILP field (QuestDB NULL) and
    /// leave every other column of the row intact.
    ///
    /// This is the bite for the 2026-09-12 fix. Before it, `gain_pct` was a
    /// bare `f64` and an unknown gain took the ENTIRE row out of the table --
    /// so the assertion that matters is not the absence of `gain_pct=`, it is
    /// the PRESENCE of `volume=` beside it.
    #[test]
    fn append_row_omits_gain_pct_when_it_is_unknown() {
        let mut w = TopVolumeRankWriter::for_test();
        let mut r = row();
        r.gain_pct = None;
        w.append_row(&r).expect("append");
        let line = w.buffer_utf8();
        assert!(
            !line.contains("gain_pct="),
            "an unknown gain must be an ABSENT field (NULL), never a written \
             value -- a 0.0 there reads as a flat underlying: {line}"
        );
        assert!(
            line.contains(&format!("volume={}i", r.volume)),
            "the row must still carry its volume -- that is the column this \
             table exists for, and the whole reason the gain no longer drops \
             it: {line}"
        );
        assert!(
            line.contains(&format!("window_lots_milli={}i", r.window_lots_milli)),
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
    /// nothing measured it — the only assert over it compared `2X >= X`, which
    /// is true for every value. This test builds the widest line `write_row`
    /// can actually emit (every integer at its column's extreme, a full-width
    /// `f64` gain, the longest real symbol values) and measures the bytes.
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
            rank: i64::MAX,
            security_id: i64::MAX,
            underlying_id: i64::MAX,
            volume: i64::from(u32::MAX),
            delta_units: i64::from(u32::MAX),
            lot_size: i64::MAX,
            window_lots_milli: i64::MAX,
            // A full-width shortest-repr f64 with a sign, so the widest
            // `gain_pct` field this column can carry.
            gain_pct: Some(-1.234_567_890_123_456_7_f64),
            subscribed: true,
        };
        w.append_row(&r).expect("append");
        let width = w.buffer_utf8().len();

        assert!(
            width <= MEASURED_WORST_CASE_ILP_ROW_BYTES,
            "one worst-case row measured {width} B against an assumed \
             {MEASURED_WORST_CASE_ILP_ROW_BYTES} B. The producer ceiling is derived from \
             the assumption, and past it this writer DROPS with no spill tier — \
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

    /// The complement: a known gain is still written.
    #[test]
    fn append_row_writes_gain_pct_when_it_is_known() {
        let mut w = TopVolumeRankWriter::for_test();
        let mut r = row();
        r.gain_pct = Some(-3.5);
        w.append_row(&r).expect("append");
        let line = w.buffer_utf8();
        assert!(
            line.contains("gain_pct=-3.5"),
            "a known gain must be stored verbatim, negatives included: {line}"
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
            "the producer must know it is split — the drain reads this to refuse a synchronous flush"
        );
        assert!(
            producer.sender.is_none(),
            "the ILP sender must MOVE to the sink; leaving a copy behind is how a \
             5-second network call finds its way back onto the drain"
        );
        assert!(
            sink.sender.is_none(),
            "for_test() has no sender to move — this pins the MOVE, not the presence"
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
            "the rows must be RETAINED — this is the arm that makes the bounded \
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
            "exactly one cut on the span AFTER the retained ones — the constant \
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
        // no-op rather than an error — the drain flushes on a timer, and a
        // timer that fires on an idle second must not log a loss.
        let (_producer, mut sink, _rx) = TopVolumeRankWriter::for_test().split_for_offload();
        let mut empty = TopVolumeFlushBatch {
            buffer: Buffer::new(ProtocolVersion::V1),
            rows: 0,
        };
        assert_eq!(sink.write(&mut empty), 0);
    }

    #[test]
    fn the_producer_byte_ceiling_is_far_tighter_than_the_depth_path() {
        // Not a style preference — a sizing claim, pinned so it cannot drift
        // into depth's 32 MiB by copy-paste. Depth is a MEASURED ~63,800
        // rows/s and bursts 400 rows in ONE depth-200 snapshot; this table
        // emits at most one row per traded contract per sweep. A ceiling
        // sized for depth would hold minutes of stale rankings nothing
        // downstream wants — every sweep supersedes the last.
        assert!(
            MAX_TOP_VOLUME_PRODUCER_BUFFER_BYTES
                < crate::depth_persistence::MAX_DEPTH_PRODUCER_BUFFER_BYTES,
            "the top-volume ceiling must stay tighter than depth's"
        );
        assert_eq!(
            TOP_VOLUME_FLUSH_QUEUE_DEPTH,
            crate::depth_persistence::DEPTH_FLUSH_QUEUE_DEPTH,
            "the queue DEPTH is deliberately the same across writers — one \
             number is one thing to keep true, and a shared shape is what stops \
             the three paths drifting into different failure semantics"
        );
    }
}

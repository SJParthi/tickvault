//! Daily 15:31 IST Dhan LIVE-vs-REST cross-verification — the comparator.
//!
//! # Why this module is the load-bearing check of the whole Dhan revival
//!
//! The Dhan India live feed has **no sequence number** and **no
//! snapshot-on-subscribe**, so packet loss is undetectable at the protocol
//! level. Nothing inside our own pipeline can prove a tick never arrived.
//! Comparing our aggregated `candles_1m` (`feed='dhan'`) against Dhan's OWN
//! official 1-minute tape is therefore the **only ground truth available** —
//! which is why the design (PR #1731) requires it live from day one rather
//! than as a supplementary check.
//!
//! REST source: `POST /v2/charts/intraday`, `interval="1"` — the
//! `no-rest-except-live-feed-2026-06-27.md` §8 KEEP class. NO other endpoint
//! is contacted; the request body is built by the shared
//! [`crate::dhan_intraday_parse::intraday_request_body`] with the same
//! day-granular window convention the spot-1m leg uses.
//!
//! # The predecessor was BLIND SINCE BIRTH — read this before editing
//!
//! The retired 15:31 cross-verify compared NANOSECOND literals against
//! QuestDB's MICROSECOND `TIMESTAMP` column. Its `WHERE` window therefore sat
//! around **year 58502**, matched ZERO rows on every run the feature ever
//! made, honestly reported `compared=0` — and *for that exact reason never
//! fired a single mismatch page*. Fixed by PR #1474 (commit `f84b4398`); see
//! `websocket-connection-scope-lock.md` §E row 4.
//!
//! Two defenses make that class impossible here:
//!
//! 1. **Units.** [`live_candles_select_sql`] divides nanos → micros for the
//!    `WHERE` bounds and re-scales `(ts / 1) * 1000` back to nanos for the
//!    projected key. Pinned by [`tests::where_window_is_microseconds_not_nanoseconds`],
//!    which fails if the magnitude is wrong by the 1000× factor, and by
//!    [`tests::nanosecond_window_would_land_in_the_year_58502`], which
//!    reproduces the original bug and asserts we reject it.
//! 2. **Non-vacuity.** `compared == 0` is a distinct, loud
//!    [`DhanLiveXverifyOutcome::Blind`] / `NoData` verdict that can never
//!    render as a pass. Pinned by [`tests::zero_comparisons_is_never_a_pass`].
//!
//! # Doctrine — NEITHER side is ground truth
//!
//! Our live capture is a conflated ~1/sec SAMPLED stream; Dhan's candle API is
//! their FULL tape. Divergence is EXPECTED and partly STRUCTURAL, not a bug
//! (`groww-second-feed-scope-2026-06-19.md` §37.5). This module therefore:
//!
//! * compares OHLC by **integer paise** with a configurable tolerance —
//!   NEVER a float epsilon compare;
//! * QUANTIFIES the typical fluctuation (p50 / p95 / max divergence in paise)
//!   so the operator can SEE what normal looks like;
//! * keeps `missing_live` / `missing_rest` as their own categories, reported
//!   but never conflated with real divergence;
//! * excuses the last two session minutes, which may legitimately be unsealed
//!   at 15:31 (`tail_unsealed`);
//! * never assumes the live side is correct.
//!
//! COLD PATH, once a day, default OFF. Writes ONLY
//! `dhan_live_crossverify_cell_audit` / `_daily` — never `ticks`,
//! `candles_*` or `historical_candles` (the live-feed purity rule).

use serde::Deserialize;

use tickvault_storage::dhan_live_crossverify_persistence::{
    DhanLiveXverifyCellFinding, DhanLiveXverifyCellKind, DhanLiveXverifyDailyRow,
    DhanLiveXverifyOutcome,
};

// ---------------------------------------------------------------------------
// Time constants — every one of these is in an EXPLICIT unit
// ---------------------------------------------------------------------------

/// Nanoseconds per second.
const NANOS_PER_SEC: i64 = 1_000_000_000;
/// Microseconds per second.
const MICROS_PER_SEC: i64 = 1_000_000;
/// Nanoseconds per microsecond — the 1000× factor that blinded the
/// predecessor. Named, never inlined.
const NANOS_PER_MICRO: i64 = 1_000;
/// Seconds per day.
const SECS_PER_DAY: i64 = 86_400;
/// Seconds per minute.
const SECS_PER_MINUTE: i64 = 60;

/// NSE session open — 09:15:00 IST, as seconds-of-day.
pub const SESSION_OPEN_SECS_OF_DAY_IST: i64 = 9 * 3600 + 15 * 60;
/// NSE session close — 15:40:00 IST, as seconds-of-day. The window is
/// half-open `[open, close)`, so the last comparable bucket opens at 15:39.
///
/// **CORRECTED 2026-08-25.** This was `15 * 3600 + 30 * 60` (15:30) — a PRIVATE
/// DUPLICATE of the session end that the 2026-08-07 NSE CAS migration missed.
/// Every other production site moved 55,800 -> 56,400 that day with a dated
/// comment (`constants.rs`, `rest_candle_fold.rs`, `tf_consistency_boot.rs`,
/// `feed_scoreboard_boot.rs`, `trading_pipeline.rs`, `day_ohlc_orchestrator.rs`);
/// this file kept its own copy and drifted.
///
/// The consequence was NOT false findings — `is_in_session` dropped 15:30-15:39
/// from BOTH sides before the join, so the window was symmetric and the verdict
/// honest. It was a BLIND SPOT: ten minutes of every session, and specifically
/// the CAS window the migration was made for, were structurally unverifiable by
/// the one check the scope-lock calls the revived feed's only ground truth.
///
/// It also mis-aimed the tail amnesty: `is_tail_minute` derives from this
/// constant, so it excused 15:28-15:29 while the genuinely-unsealed tail had
/// moved to 15:38-15:39. Deriving the value below fixes both at once.
///
/// Now DERIVED from the canonical constant rather than restated, so a future
/// session-hours change cannot leave this file behind again. The const assert
/// below is the actual guarantee.
pub const SESSION_CLOSE_SECS_OF_DAY_IST: i64 =
    tickvault_common::constants::TICK_PERSIST_END_SECS_OF_DAY_IST as i64;

// A private duplicate of a session boundary drifted silently for 18 days. This
// makes that a COMPILE error rather than a blind spot nobody can see.
const _: () = assert!(
    SESSION_CLOSE_SECS_OF_DAY_IST
        == tickvault_common::constants::TICK_PERSIST_END_SECS_OF_DAY_IST as i64,
    "the cross-verify session close must track the canonical persistence end"
);
const _: () = assert!(
    SESSION_OPEN_SECS_OF_DAY_IST < SESSION_CLOSE_SECS_OF_DAY_IST,
    "the comparison window must be non-empty"
);

/// The run's deterministic stamp — 15:41:00 IST, as seconds-of-day. Used as
/// the designated audit timestamp regardless of when the run actually fired,
/// so a catch-up / forced rerun UPSERTs the same rows.
///
/// **CORRECTED 2026-08-25** from 15:31 in lockstep with the close above. It
/// MUST stay one minute past the close: the stamp and the fire time
/// (`dhan_feed_stack::XVERIFY_RUN_AT_SECS_OF_DAY_IST`) move together, and a
/// stamp earlier than the window's end would date the audit row before the
/// last minute it claims to have compared.
pub const RUN_SECS_OF_DAY_IST: i64 = SESSION_CLOSE_SECS_OF_DAY_IST + 60;

const _: () = assert!(
    RUN_SECS_OF_DAY_IST > SESSION_CLOSE_SECS_OF_DAY_IST,
    "the run must be stamped after the last minute it compares"
);

/// How many trailing session minutes are excused when absent on the LIVE side.
///
/// At 15:31 the 15:29 bucket has only just closed and the 15:28 seal may still
/// be in flight, so a live-side gap there is EXPECTED, not evidence of loss.
/// Dhan's REST tape, being a post-hoc query, always has them.
pub const TAIL_UNSEALED_MINUTES: i64 = 2;

/// Minutes in the COMPARED session window, derived from this module's own two
/// bounds so the row cap below can never disagree with the window the
/// comparator actually classifies against.
///
/// **MERGE RESOLUTION 2026-08-25 — this bound MOVED, and the reasoning that
/// argued against moving it is preserved rather than deleted.**
///
/// This branch deliberately held the compared close at **15:30** while
/// `main` changed it to track `TICK_PERSIST_END_SECS_OF_DAY_IST` (**15:40**,
/// the 2026-08-03 NSE CAS change), with a `const_assert` binding the two.
/// The merge takes main's, for the reason main gives: a row that was
/// PERSISTED and never COMPARED is a silently unverified row, and the
/// comparator exists precisely so no such row exists. At ~868 instruments
/// that is ~81 rows a day moving from quietly-unverified to verified.
///
/// **The risk the 15:30 position identified is real and is NOT retired by
/// this choice.** If Dhan's REST tape does not publish 1-minute candles for
/// the closing-auction window, those same ~81 rows become thousands of
/// `missing_rest` entries a day and the verdict looks worse for a reason that
/// is not loss. That is a REPORTING failure, not a data one — which is why it
/// is the acceptable side to be wrong on, and why it is stated here instead
/// of discovered from a puzzling verdict. **The next 15:31 run measures it:**
/// a `missing_rest` that jumps by roughly the instrument count times ten
/// minutes is this, not packet loss.
pub const SESSION_MINUTES: usize =
    ((SESSION_CLOSE_SECS_OF_DAY_IST - SESSION_OPEN_SECS_OF_DAY_IST) / SECS_PER_MINUTE) as usize;

/// How many instruments one run can read a FULL day for.
///
/// Stated as a coverage promise rather than left implicit in a row count: at
/// or below this many instruments the live read is complete, and above it the
/// run reports `partial`. Today's live universe is ~868 distinct instruments
/// (~119 NSE indices + ~750 NTM constituents), so this carries better than 2×
/// headroom.
pub const LIVE_COVERED_INSTRUMENTS: usize = 2_000;

/// Row cap for the live-side `/exec` read. Beyond it the run is stamped
/// `partial` rather than silently comparing a truncated day.
///
/// **Derived, not a round number (2026-08-25).** This was the literal
/// `200_000`, which at today's ~868 instruments covers `200_000 / 868 ≈ 230`
/// of the session's 385 minutes — and because the query is
/// `ORDER BY ts ASC LIMIT n`, truncation drops the TAIL: the comparator was
/// verifying the morning and never the afternoon, every day. It reported
/// `partial` for it, so this was a coverage limit rather than a false-OK — but
/// nothing connected the number to the universe it had to cover, so nobody
/// could see that it had stopped being enough.
///
/// **⚠ It does NOT cover the authorized ceiling.** `MAX_DAILY_UNIVERSE_SIZE`
/// is 25,000, which would need `25_000 × 385 ≈ 9.6M` rows in one response —
/// not a cap raise but a paginated or per-instrument read, i.e. a design
/// change. Until then a universe past [`LIVE_COVERED_INSTRUMENTS`] verifies
/// its morning and says `partial`. Stated here so the next widening does not
/// discover it from a puzzling verdict.
pub const LIVE_ROW_LIMIT: usize = LIVE_COVERED_INSTRUMENTS * SESSION_MINUTES;

// ---------------------------------------------------------------------------
// Config — DEFAULT OFF
// ---------------------------------------------------------------------------

/// `[dhan_live_crossverify]` — the daily 15:31 IST comparator's knobs.
///
/// **Default OFF** (fail-safe): an absent config section leaves the comparator
/// disabled, exactly like every other scheduled leg in this repo.
///
/// NOTE (wiring follow-up): this struct lives here rather than in
/// `crates/common/src/config.rs` because this PR does not own that file
/// (parallel feed-revival agents). Wiring it into `AppConfig` as
/// `#[serde(default)] pub dhan_live_crossverify: DhanLiveCrossverifyConfig`
/// is a one-line follow-up; the defaults below are already fail-safe, so the
/// unwired state is "disabled", never "enabled by accident".
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DhanLiveCrossverifyConfig {
    // 2026-08-25 — the `enabled: bool` field that stood here is DELETED.
    //
    // It documented itself as "Master switch. Default OFF" and defaulted to
    // `false`, and it had ZERO production readers: a workspace scan found it
    // referenced only by two of its own tests. The comparator's real and only
    // gate is `crossverify_deps_installed()` in `dhan_feed_stack`, which
    // refuses loudly when no provider is installed. So the field asserted the
    // opposite of the truth in BOTH directions — a reader could set it false
    // and the comparator would keep running, or read "default OFF" and
    // conclude the comparator never runs when in fact it runs every day.
    //
    // Deleted rather than wired, deliberately. Wiring it would honour its
    // stated default and SILENTLY STOP the only ground truth a feed with no
    // sequence number and no snapshot-on-subscribe has — nothing sets it to
    // true, because the struct is never deserialised from any config file and
    // has no section in any TOML. Turning the comparator off is an operator
    // decision with its own dated quote, not a side effect of tidying a
    // struct. The remaining three knobs ARE read (`run_budget_secs`,
    // `fetch_timeout_secs` and `tolerance_paise`, all in `run_once`).
    /// OHLC comparison tolerance in PAISE. Default `0` = paise-exact.
    ///
    /// Integer paise, never a float epsilon: a float `|a - b| <= eps` compare
    /// is exactly how rounding noise turns into phantom divergence (and how
    /// real divergence hides inside a generous epsilon).
    #[serde(default)]
    pub tolerance_paise: i64,
    /// Per-instrument REST fetch timeout (seconds).
    #[serde(default = "default_fetch_timeout_secs")]
    pub fetch_timeout_secs: u64,
    /// Timeout for the ONE live-side QuestDB read (seconds).
    ///
    /// Separate from [`Self::fetch_timeout_secs`] since 2026-08-25. The live
    /// read used to share it, which coupled two operations that differ by
    /// three orders of magnitude: one 8-column REST call for a single
    /// instrument's day, versus a scan returning up to [`LIVE_ROW_LIMIT`] rows
    /// for the whole universe. Sharing the knob meant any raise of the row cap
    /// converted honest truncation into a hard `Err` that aborts the entire
    /// run — strictly worse than the partial verdict it replaced.
    #[serde(default = "default_live_read_timeout_secs")]
    pub live_read_timeout_secs: u64,
    /// Whole-run wall-clock budget (seconds). On elapse the run is stamped
    /// `partial` / `degraded` — never a fabricated clean verdict.
    #[serde(default = "default_run_budget_secs")]
    pub run_budget_secs: u64,
}

const fn default_fetch_timeout_secs() -> u64 {
    10
}
/// 60s for one bulk scan of up to [`LIVE_ROW_LIMIT`] rows. Generous on
/// purpose: this read happens ONCE per day on a cold path at 15:31, an hour
/// before the box stops, and its failure costs the whole day's verification.
const fn default_live_read_timeout_secs() -> u64 {
    60
}
/// Whole-run budget.
///
/// **Derived from the vendor's own rate limit (2026-08-25), not a round
/// number.** The REST leg is a SEQUENTIAL loop over every target, and the Dhan
/// Data API budget is 5 requests/sec (`no-rest-except-live-feed-2026-06-27.md`
/// §8), so ~868 targets need at least `868 / 5 ≈ 174s` even at a perfect rate.
/// The old `240` left ~66s of margin for the live read plus every slow
/// response — and the moment the 2026-08-25 `instrument` fix makes the ~750
/// equity targets return real bars instead of empty sets, those responses stop
/// being fast failures and start costing real time. 600s restores real margin
/// and still finishes well inside the ~2 hours between the 15:31 fire and the
/// 17:30 stop.
///
/// This does NOT make the run cover more instruments than the budget allows —
/// it stops the budget from being the binding constraint at today's size. Past
/// ~3,000 targets it becomes binding again, and the run reports
/// `budget_elapsed` rather than pretending the un-fetched targets were clean.
const fn default_run_budget_secs() -> u64 {
    600
}

impl Default for DhanLiveCrossverifyConfig {
    fn default() -> Self {
        Self {
            // (no `enabled` field — see the note on the struct)
            tolerance_paise: 0,
            fetch_timeout_secs: default_fetch_timeout_secs(),
            live_read_timeout_secs: default_live_read_timeout_secs(),
            run_budget_secs: default_run_budget_secs(),
        }
    }
}

impl DhanLiveCrossverifyConfig {
    /// Boot-time validation.
    ///
    /// A negative tolerance is nonsense; an absurdly large one would silently
    /// demote every real divergence to "clean" — the false-OK class this whole
    /// module exists to prevent — so both are rejected at boot. Zero timeouts
    /// are rejected because they would make every fetch fail instantly and the
    /// run permanently `degraded`.
    ///
    /// # Errors
    /// Returns a descriptive error when any knob is outside its band.
    pub fn validate(&self) -> Result<(), String> {
        // ₹500 in paise — far above any plausible index-level sampling skew.
        const MAX_TOLERANCE_PAISE: i64 = 50_000;
        if self.tolerance_paise < 0 {
            return Err(format!(
                "[dhan_live_crossverify] tolerance_paise must be >= 0 (got {})",
                self.tolerance_paise
            ));
        }
        if self.tolerance_paise > MAX_TOLERANCE_PAISE {
            return Err(format!(
                "[dhan_live_crossverify] tolerance_paise {} exceeds {MAX_TOLERANCE_PAISE} \
                 (₹500) — a tolerance that wide would silently swallow real divergence",
                self.tolerance_paise
            ));
        }
        if self.fetch_timeout_secs == 0
            || self.run_budget_secs == 0
            || self.live_read_timeout_secs == 0
        {
            return Err(
                "[dhan_live_crossverify] fetch_timeout_secs, run_budget_secs and \
                 live_read_timeout_secs must be > 0"
                    .to_string(),
            );
        }
        // The live read happens BEFORE the REST loop and inside the same
        // budget, so a live timeout at or above the budget would spend the
        // entire run on one query and fetch nothing to compare it against.
        if self.live_read_timeout_secs >= self.run_budget_secs {
            return Err(format!(
                "[dhan_live_crossverify] live_read_timeout_secs ({}) must be below \
                 run_budget_secs ({}) — otherwise one query can consume the whole run",
                self.live_read_timeout_secs, self.run_budget_secs
            ));
        }
        if self.fetch_timeout_secs > self.run_budget_secs {
            return Err(format!(
                "[dhan_live_crossverify] fetch_timeout_secs ({}) exceeds run_budget_secs ({}) \
                 — a single fetch could consume the whole run",
                self.fetch_timeout_secs, self.run_budget_secs
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Time helpers (pure)
// ---------------------------------------------------------------------------

/// The deterministic audit run stamp for a target day — that day's 15:31:00
/// IST in NANOSECONDS, regardless of when the run actually fired. Pure.
#[must_use]
pub fn deterministic_run_ts_nanos(day_start_ist_nanos: i64) -> i64 {
    day_start_ist_nanos.saturating_add(RUN_SECS_OF_DAY_IST.saturating_mul(NANOS_PER_SEC))
}

/// Seconds-of-day for an IST minute bucket, relative to its day start. Pure.
#[must_use]
pub fn secs_of_day(minute_ts_ist_nanos: i64, day_start_ist_nanos: i64) -> i64 {
    minute_ts_ist_nanos
        .saturating_sub(day_start_ist_nanos)
        .div_euclid(NANOS_PER_SEC)
}

/// `true` when the bucket lies inside `[09:15, 15:30)` IST. Pure.
#[must_use]
pub fn is_in_session(minute_ts_ist_nanos: i64, day_start_ist_nanos: i64) -> bool {
    let s = secs_of_day(minute_ts_ist_nanos, day_start_ist_nanos);
    (SESSION_OPEN_SECS_OF_DAY_IST..SESSION_CLOSE_SECS_OF_DAY_IST).contains(&s)
}

/// `true` when the bucket is one of the trailing [`TAIL_UNSEALED_MINUTES`]
/// session minutes — the buckets a 15:31 run may legitimately find unsealed on
/// the live side. Pure.
#[must_use]
pub fn is_tail_minute(minute_ts_ist_nanos: i64, day_start_ist_nanos: i64) -> bool {
    let s = secs_of_day(minute_ts_ist_nanos, day_start_ist_nanos);
    let first_tail =
        SESSION_CLOSE_SECS_OF_DAY_IST.saturating_sub(TAIL_UNSEALED_MINUTES * SECS_PER_MINUTE);
    (first_tail..SESSION_CLOSE_SECS_OF_DAY_IST).contains(&s)
}

// ---------------------------------------------------------------------------
// The live-side SELECT — THE regression lock
// ---------------------------------------------------------------------------

/// Day bounds for a QuestDB `TIMESTAMP` comparison, in MICROSECONDS.
///
/// **This function is the fix for the bug that blinded the predecessor.**
/// Empirically confirmed on QuestDB 9.3.5: a bare integer literal compared
/// against a `TIMESTAMP` column is interpreted as epoch **microseconds**.
/// Feeding it nanoseconds shifts the window ~1000× into the future (the
/// original bug landed around year 58502) where it matches nothing, forever,
/// silently. Pure.
#[must_use]
pub fn day_bounds_micros(day_start_ist_nanos: i64) -> (i64, i64) {
    let start = day_start_ist_nanos / NANOS_PER_MICRO;
    (
        start,
        start.saturating_add(SECS_PER_DAY.saturating_mul(MICROS_PER_SEC)),
    )
}

/// The day's live `candles_1m` SELECT for `feed='dhan'`.
///
/// * `WHERE` bounds are MICROSECONDS (see [`day_bounds_micros`]).
/// * The projected key is re-scaled `(ts / 1) * 1000` back to NANOSECONDS,
///   because `(ts / 1)` yields micros.
/// * `LIMIT cap + 1` is a truncation PROBE: asking for one row past the cap
///   makes `rows > cap` the only truncation signal, so an exactly-cap day is
///   complete rather than a false `partial`.
///
/// Pure. Pinned by the digit-magnitude tests below.
#[must_use]
pub fn live_candles_select_sql(day_start_ist_nanos: i64) -> String {
    let (start, end) = day_bounds_micros(day_start_ist_nanos);
    let probe_limit = LIVE_ROW_LIMIT.saturating_add(1);
    format!(
        "SELECT (ts / 1) * 1000 AS ts_nanos, security_id, segment, open, high, \
         low, close, volume \
         FROM candles_1m \
         WHERE feed = 'dhan' AND ts >= {start} AND ts < {end} \
         ORDER BY ts ASC LIMIT {probe_limit}"
    )
}

// ---------------------------------------------------------------------------
// Paise comparison (integer, never float epsilon)
// ---------------------------------------------------------------------------

/// Rupees → integer paise, half-away-from-zero.
///
/// Comparison key only — nothing is ever written back rounded. Returns `None`
/// for a non-finite price: `(NaN * 100.0).round() as i64` saturates to `0` in
/// Rust, which would silently compare a corrupt row as ₹0.00 and manufacture a
/// divergence (or hide one). A non-finite cell is a malformed row, counted,
/// never compared. Pure.
#[must_use]
pub fn to_paise(v: f64) -> Option<i64> {
    if !v.is_finite() {
        return None;
    }
    // Bounded: 2dp NSE prices ×100 fit i64 with ~14 orders of magnitude spare.
    // APPROVED: cold-path paise comparison key — truncation impossible in range
    #[allow(clippy::cast_possible_truncation)]
    Some((v * 100.0).round() as i64)
}

/// One bar reduced to its integer-paise comparison key, with the raw rupee
/// values retained verbatim for the audit row.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaiseBar {
    pub open_paise: i64,
    pub high_paise: i64,
    pub low_paise: i64,
    pub close_paise: i64,
    pub open_r: f64,
    pub high_r: f64,
    pub low_r: f64,
    pub close_r: f64,
    pub volume: i64,
}

impl PaiseBar {
    /// Build from rupee OHLC. `None` when ANY price is non-finite — the row is
    /// malformed and must be counted, never silently compared. Pure.
    #[must_use]
    pub fn from_rupees(open: f64, high: f64, low: f64, close: f64, volume: i64) -> Option<Self> {
        Some(Self {
            open_paise: to_paise(open)?,
            high_paise: to_paise(high)?,
            low_paise: to_paise(low)?,
            close_paise: to_paise(close)?,
            open_r: open,
            high_r: high,
            low_r: low,
            close_r: close,
            volume,
        })
    }

    /// The four comparable fields, in stable audit order.
    fn fields(&self) -> [(&'static str, i64, f64); 4] {
        [
            ("open", self.open_paise, self.open_r),
            ("high", self.high_paise, self.high_r),
            ("low", self.low_paise, self.low_r),
            ("close", self.close_paise, self.close_r),
        ]
    }
}

// ---------------------------------------------------------------------------
// The comparison inputs + result
// ---------------------------------------------------------------------------

/// A single instrument's minute on one side of the comparison.
#[derive(Clone, Debug, PartialEq)]
pub struct SideBar {
    pub security_id: i64,
    pub segment: String,
    pub minute_ts_ist_nanos: i64,
    pub bar: PaiseBar,
}

/// The full outcome of one day's comparison — counts, findings and the
/// quantified noise profile.
#[derive(Clone, Debug, PartialEq)]
pub struct DayComparison {
    pub outcome: DhanLiveXverifyOutcome,
    pub findings: Vec<DhanLiveXverifyCellFinding>,
    pub instruments: i64,
    /// Minutes present on BOTH sides — the non-vacuity denominator.
    pub minutes_compared: i64,
    pub cells_diverged: i64,
    pub missing_live: i64,
    /// The `missing_live` minutes where Dhan's own bar carried a NON-ZERO
    /// volume — the exchange recorded trades in that minute and we hold no
    /// candle for it.
    ///
    /// # Why this is split out (2026-08-26)
    ///
    /// `missing_live` is the closest proxy this system has for packet loss,
    /// and on 2026-08-25 it read **5,783 of 18,510** minutes — 31.2% — for
    /// the 50 instruments the run managed to fetch. That number is alarming
    /// and, as a single figure, unanswerable: it conflates two situations
    /// that mean opposite things.
    ///
    /// * Dhan's bar has volume > 0 → the instrument TRADED that minute and
    ///   we have nothing. That is a genuine gap in our capture.
    /// * Dhan's bar has volume == 0 → nothing traded. Our fold correctly
    ///   emits nothing for a bucket no tick opened (`live_candle_state.rs`
    ///   makes an unopened bucket a sentinel), so this is not loss at all.
    ///
    /// Reporting them as one number is why "31.2% missing" could be read
    /// either as a catastrophe or as a non-event, with no way to tell.
    ///
    /// # What this split does NOT decide
    ///
    /// It is a MEASUREMENT, not a verdict. The `outcome` logic is unchanged:
    /// `missing_live` in total still counts as a real divergence, because
    /// narrowing that to the traded half would be a behaviour change resting
    /// on an assumption about vendor tape-filling that nothing here has
    /// verified.
    ///
    /// **It says nothing at all for `IDX_I`.** An index has no volume by
    /// construction, so every index bar lands in the zero bucket regardless
    /// of whether the index was being computed. For index instruments this
    /// pair carries no information and must not be read as one.
    pub missing_live_traded: i64,
    /// The `missing_live` minutes where Dhan's bar carried ZERO volume.
    ///
    /// See [`DayComparison::missing_live_traded`] for what this does and does
    /// not mean — in particular that it is uninformative for `IDX_I`.
    pub missing_live_zero_volume: i64,
    pub missing_rest: i64,
    pub tail_unsealed: i64,
    pub out_of_session: i64,
    /// Median divergence magnitude in paise — what NORMAL looks like.
    pub noise_p50_paise: i64,
    pub noise_p95_paise: i64,
    pub noise_max_paise: i64,

    // ---- volume, added 2026-08-26 ----------------------------------
    //
    // Until today the comparison checked open/high/low/close and NOTHING
    // else. Volume rode along on every finding row and was never once
    // compared, so "does our volume agree with the exchange's?" had no
    // answer anywhere in the system — while a live volume defect measured
    // on 2026-08-24 had the intraday frames at ~9.2x the day bar with 6,088
    // instruments disagreeing with their own minute sums.
    //
    // Volume is reported as a CAPTURE PERCENTAGE (ours as a share of
    // theirs), not as a pass/fail. That is the honest shape for this pair:
    // our live feed is a conflated ~1/sec sample and theirs is the full
    // tape, so under-capture is STRUCTURAL and expected. A strict equality
    // check would flag nearly every cell and drown the price signal it sits
    // beside.
    //
    // It deliberately does NOT feed `cells_diverged` or the outcome. A
    // threshold needs a baseline, and no baseline exists yet — this is the
    // measurement that creates one. Same discipline the price side already
    // applies by quantifying its noise instead of asserting a limit.
    /// Cells where both sides carried a usable volume (theirs > 0).
    pub volume_cells: i64,
    /// Of those, how many matched to the unit.
    pub volume_exact: i64,
    /// Our volume as a percentage of theirs — median, and the 5th
    /// percentile (the bad tail). 100 means we saw everything they did.
    pub volume_capture_p50_pct: i64,
    pub volume_capture_p05_pct: i64,
    /// Worst SINGLE cell. Added after a test caught the gap: `percentile`
    /// here is linear-rank, so at n=20 the 5th percentile lands on the
    /// second-smallest value and one catastrophic minute is invisible to
    /// it. p05 describes the distribution; this describes the worst thing
    /// that actually happened. The price side reports `max` for exactly
    /// the same reason — volume's bad direction is simply down.
    pub volume_capture_min_pct: i64,
}

impl DayComparison {
    /// `true` when this run compared nothing and therefore proves nothing.
    /// The caller MUST log loudly on this — never render it as success.
    #[must_use]
    pub fn is_vacuous(&self) -> bool {
        self.minutes_compared == 0
    }
}

/// Percentile of a SORTED slice, nearest-rank. Pure.
fn percentile(sorted: &[i64], p: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    // APPROVED: cold-path percentile index — truncation impossible in range
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss
    )]
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Compare one day of our LIVE capture against Dhan's official REST tape.
///
/// Pure and total — no I/O, no panics. `degraded` marks that a leg failed or
/// truncated upstream, which downgrades a would-be `Clean`/`Diverged` verdict
/// to `Partial` (never up).
///
/// Classification:
///
/// | Situation | Category | Counted as divergence? |
/// |---|---|---|
/// | both sides, field differs > tolerance | `diverged` | **yes** |
/// | REST has it, live doesn't (mid-session) | `missing_live` | **yes** — the closest proxy we have for packet loss |
/// | REST has it, live doesn't (last 2 min) | `tail_unsealed` | no — expected at 15:31 |
/// | live has it, REST doesn't | `missing_rest` | **no** — the REST tape is sparse by construction; reported as `Partial`, never `Clean`, never `Diverged` |
/// | either side outside `[09:15, 15:30)` | `out_of_session` | no |
#[must_use]
/// Compares one day's live capture against the vendor's own tape.
///
/// `in_scope` is the set of `(security_id, segment)` pairs the run actually
/// ASKED the vendor for. **Live instruments outside it are skipped entirely.**
///
/// # Why the scope filter exists (MEASURED, prod, 2026-08-26)
///
/// The live universe carries ~8,144 instruments (spots AND every option and
/// future contract); the REST leg fetches ~865 spot targets. Without this
/// filter every minute of the ~7,279 never-requested instruments was recorded
/// as `missing_rest` — *"the vendor is missing this"* — for data we never
/// asked the vendor for.
///
/// That was not a cosmetic mislabel. It produced **757,273 of the run's
/// 764,003 findings (99.1%)**, and the resulting ILP buffer reached
/// **207,965,278 bytes against a 104,857,600-byte ceiling**, so the flush
/// failed and **the entire day's comparison was discarded** — in the one check
/// that exists to prove nothing was lost.
///
/// An empty `in_scope` disables the filter, which keeps every existing caller
/// and test meaning exactly what it did before.
pub fn compare_day_in_scope(
    live: &[SideBar],
    rest: &[SideBar],
    in_scope: &std::collections::BTreeSet<(i64, String)>,
    day_start_ist_nanos: i64,
    run_ts_ist_nanos: i64,
    tolerance_paise: i64,
    degraded: bool,
) -> DayComparison {
    use std::collections::{BTreeMap, BTreeSet};

    // Key on (security_id, segment, minute) — segment is in the key because
    // Dhan reuses numeric security_ids across segments (I-P1-11); keying on
    // the id alone would silently merge two instruments.
    type Key = (i64, String, i64);

    let mut live_map: BTreeMap<Key, &SideBar> = BTreeMap::new();
    let mut rest_map: BTreeMap<Key, &SideBar> = BTreeMap::new();
    let mut instruments: BTreeSet<(i64, String)> = BTreeSet::new();
    let mut out_of_session: i64 = 0;

    for b in live {
        // Out of scope = never requested. Skipping BEFORE the instrument
        // count matters: counting it would report a universe far larger than
        // the one actually verified, which is how "8,144 instruments" sat in
        // the verdict beside 865 targets and read as coverage.
        if !in_scope.is_empty() && !in_scope.contains(&(b.security_id, b.segment.clone())) {
            continue;
        }
        instruments.insert((b.security_id, b.segment.clone()));
        if !is_in_session(b.minute_ts_ist_nanos, day_start_ist_nanos) {
            out_of_session = out_of_session.saturating_add(1);
            continue;
        }
        live_map.insert((b.security_id, b.segment.clone(), b.minute_ts_ist_nanos), b);
    }
    for b in rest {
        instruments.insert((b.security_id, b.segment.clone()));
        if !is_in_session(b.minute_ts_ist_nanos, day_start_ist_nanos) {
            out_of_session = out_of_session.saturating_add(1);
            continue;
        }
        rest_map.insert((b.security_id, b.segment.clone(), b.minute_ts_ist_nanos), b);
    }

    let mut findings: Vec<DhanLiveXverifyCellFinding> = Vec::new();
    let mut diffs: Vec<i64> = Vec::new();
    let mut minutes_compared: i64 = 0;
    let mut cells_diverged: i64 = 0;
    let mut volume_cells: i64 = 0;
    let mut volume_exact: i64 = 0;
    let mut volume_capture: Vec<i64> = Vec::new();
    let mut missing_live: i64 = 0;
    let mut missing_live_traded: i64 = 0;
    let mut missing_live_zero_volume: i64 = 0;
    let mut missing_rest: i64 = 0;
    let mut tail_unsealed: i64 = 0;

    let all_keys: BTreeSet<&Key> = live_map.keys().chain(rest_map.keys()).collect();

    for key in all_keys {
        let (security_id, segment, minute) = (key.0, key.1.clone(), key.2);
        match (live_map.get(key), rest_map.get(key)) {
            (Some(l), Some(r)) => {
                minutes_compared = minutes_compared.saturating_add(1);
                for ((field, l_paise, l_r), (_, r_paise, r_r)) in
                    l.bar.fields().iter().zip(r.bar.fields().iter())
                {
                    let diff = l_paise.saturating_sub(*r_paise).abs();
                    if diff > 0 {
                        // Every non-zero delta feeds the noise profile, even
                        // inside tolerance — that is HOW we quantify normal.
                        diffs.push(diff);
                    }
                    if diff > tolerance_paise {
                        cells_diverged = cells_diverged.saturating_add(1);
                        findings.push(DhanLiveXverifyCellFinding {
                            run_ts_ist_nanos,
                            trading_date_ist_nanos: day_start_ist_nanos,
                            security_id,
                            segment: segment.clone(),
                            minute_ts_ist_nanos: minute,
                            kind: DhanLiveXverifyCellKind::Diverged,
                            field,
                            live_value: *l_r,
                            rest_value: *r_r,
                            live_volume: l.bar.volume,
                            rest_volume: r.bar.volume,
                            diff_paise: diff,
                        });
                    }
                }

                // Volume — MEASURED, never a verdict. See the volume block
                // on `DayComparison` for why this is a capture percentage
                // and not a pass/fail.
                //
                // Their volume is the denominator, so a zero or negative one
                // is skipped rather than divided by: a minute the vendor
                // reports as untraded says nothing about our capture, and
                // counting it as 0% would drag the median toward a number
                // that means "no trades happened", not "we missed them".
                if r.bar.volume > 0 {
                    volume_cells = volume_cells.saturating_add(1);
                    if l.bar.volume == r.bar.volume {
                        volume_exact = volume_exact.saturating_add(1);
                    }
                    // Integer arithmetic throughout — a float ratio here
                    // would be the one place in this module that compares
                    // by epsilon, which its own header forbids.
                    let pct = l
                        .bar
                        .volume
                        .saturating_mul(100)
                        .checked_div(r.bar.volume)
                        .unwrap_or(0);
                    volume_capture.push(pct);
                }
            }
            (None, Some(r)) => {
                // In Dhan's own tape but absent from our live capture. The
                // trailing minutes get an amnesty — at 15:31 they may simply
                // not have sealed yet.
                let kind = if is_tail_minute(minute, day_start_ist_nanos) {
                    tail_unsealed = tail_unsealed.saturating_add(1);
                    DhanLiveXverifyCellKind::TailUnsealed
                } else {
                    missing_live = missing_live.saturating_add(1);
                    // Split the one number that could not be acted on. See
                    // `DayComparison::missing_live_traded` for why, and for
                    // the IDX_I caveat that makes this pair meaningless on
                    // an index.
                    if r.bar.volume > 0 {
                        missing_live_traded = missing_live_traded.saturating_add(1);
                    } else {
                        missing_live_zero_volume = missing_live_zero_volume.saturating_add(1);
                    }
                    DhanLiveXverifyCellKind::MissingLive
                };
                findings.push(DhanLiveXverifyCellFinding {
                    run_ts_ist_nanos,
                    trading_date_ist_nanos: day_start_ist_nanos,
                    security_id,
                    segment: segment.clone(),
                    minute_ts_ist_nanos: minute,
                    kind,
                    field: "none",
                    live_value: 0.0,
                    rest_value: r.bar.close_r,
                    live_volume: 0,
                    rest_volume: r.bar.volume,
                    diff_paise: 0,
                });
            }
            (Some(l), None) => {
                missing_rest = missing_rest.saturating_add(1);
                findings.push(DhanLiveXverifyCellFinding {
                    run_ts_ist_nanos,
                    trading_date_ist_nanos: day_start_ist_nanos,
                    security_id,
                    segment: segment.clone(),
                    minute_ts_ist_nanos: minute,
                    kind: DhanLiveXverifyCellKind::MissingRest,
                    field: "none",
                    live_value: l.bar.close_r,
                    rest_value: 0.0,
                    live_volume: l.bar.volume,
                    rest_volume: 0,
                    diff_paise: 0,
                });
            }
            (None, None) => {}
        }
    }

    diffs.sort_unstable();
    volume_capture.sort_unstable();
    let volume_capture_p50_pct = percentile(&volume_capture, 0.50);
    let volume_capture_p05_pct = percentile(&volume_capture, 0.05);
    let volume_capture_min_pct = volume_capture.first().copied().unwrap_or(0);
    let noise_p50_paise = percentile(&diffs, 0.50);
    let noise_p95_paise = percentile(&diffs, 0.95);
    let noise_max_paise = diffs.last().copied().unwrap_or(0);

    let rows_seen = !live.is_empty() || !rest.is_empty();

    // ---- The non-vacuity contract -------------------------------------
    // `Clean` is reachable ONLY from the `minutes_compared > 0` branch. A run
    // that compared nothing lands on Blind / NoData / Degraded — every one of
    // which `is_pass() == false`. This is the structural answer to the
    // predecessor's blind-since-birth failure.
    let outcome = if minutes_compared > 0 {
        // The asymmetry here is deliberate and was WRONG until 2026-08-25.
        //
        // `missing_live` (REST has the minute, we don't) is the closest proxy
        // this system has for packet loss on our own feed. It stays a real
        // divergence.
        //
        // `missing_rest` (we have the minute, the vendor's REST tape doesn't)
        // is NOT evidence against the live feed. The REST tape is sparse by
        // construction — it publishes a candle where trading happened — so an
        // illiquid strike that we correctly recorded shows up here through no
        // fault of ours. Counting it as divergence made a vendor-side hole
        // read as a live-feed failure, and inflated the one signal this lane
        // exists to produce.
        //
        // This module's own header has always said `missing_rest` is
        // "reported but never conflated with real divergence". The
        // classification table below it said the opposite, and the code
        // followed the table. The header was right.
        //
        // It is NOT silenced, and it must never be: a bar we hold for a
        // minute the exchange never traded could also mean we FABRICATED one
        // — which is not hypothetical, since the aggregator was proven on
        // 2026-08-24 to inflate candle volumes 9.2x. So a run whose only
        // anomaly is `missing_rest` lands on `Partial`, which `is_pass()`
        // refuses and `is_measured()` accepts: visible, never a pass, and
        // never crying feed-loss either.
        let real = cells_diverged > 0 || missing_live > 0;
        if degraded {
            DhanLiveXverifyOutcome::Partial
        } else if real {
            DhanLiveXverifyOutcome::Diverged
        } else if missing_rest > 0 {
            DhanLiveXverifyOutcome::Partial
        } else {
            DhanLiveXverifyOutcome::Clean
        }
    } else if degraded {
        DhanLiveXverifyOutcome::Degraded
    } else if rows_seen {
        // Rows existed on at least one side yet nothing overlapped. THIS is
        // the state the year-58502 window produced on every single run.
        DhanLiveXverifyOutcome::Blind
    } else {
        DhanLiveXverifyOutcome::NoData
    };

    DayComparison {
        outcome,
        findings,
        instruments: instruments.len() as i64,
        minutes_compared,
        cells_diverged,
        missing_live,
        missing_live_traded,
        missing_live_zero_volume,
        missing_rest,
        tail_unsealed,
        out_of_session,
        noise_p50_paise,
        noise_p95_paise,
        noise_max_paise,
        volume_cells,
        volume_exact,
        volume_capture_p50_pct,
        volume_capture_p05_pct,
        volume_capture_min_pct,
    }
}

/// Build the daily audit row from a comparison. Pure.
#[must_use]
pub fn daily_row(
    cmp: &DayComparison,
    day_start_ist_nanos: i64,
    run_ts_ist_nanos: i64,
    tolerance_paise: i64,
) -> DhanLiveXverifyDailyRow {
    DhanLiveXverifyDailyRow {
        run_ts_ist_nanos,
        trading_date_ist_nanos: day_start_ist_nanos,
        instruments: cmp.instruments,
        minutes_compared: cmp.minutes_compared,
        cells_diverged: cmp.cells_diverged,
        missing_live: cmp.missing_live,
        missing_live_traded: cmp.missing_live_traded,
        missing_live_zero_volume: cmp.missing_live_zero_volume,
        missing_rest: cmp.missing_rest,
        tail_unsealed: cmp.tail_unsealed,
        out_of_session: cmp.out_of_session,
        noise_p50_paise: cmp.noise_p50_paise,
        noise_p95_paise: cmp.noise_p95_paise,
        noise_max_paise: cmp.noise_max_paise,
        tolerance_paise,
        outcome: cmp.outcome,
    }
}

/// Keep-better rerun guard: `true` when `candidate` may overwrite `stored`.
///
/// A MEASURED verdict is never replaced by a later vacuous one — otherwise a
/// blind rerun would erase the day's real evidence. Pure.
#[must_use]
pub fn may_overwrite(
    stored: Option<DhanLiveXverifyOutcome>,
    candidate: DhanLiveXverifyOutcome,
) -> bool {
    match stored {
        None => true,
        Some(s) => candidate.is_measured() || !s.is_measured(),
    }
}

/// One-line operator summary, plain English — no library names, no file paths,
/// rupees not paise (the 10 Telegram commandments). Pure.
///
/// A vacuous run says so in the FIRST words; it can never be mistaken for a
/// clean day at a glance.
#[must_use]
pub fn format_summary_line(cmp: &DayComparison) -> String {
    // APPROVED: cold-path display-only paise→rupee division.
    let rupees = |p: i64| p as f64 / 100.0;
    match cmp.outcome {
        DhanLiveXverifyOutcome::Blind => format!(
            "🆘 Dhan live-vs-official check PROVED NOTHING today: {} minute(s) \
             of data existed but NONE lined up, so nothing was actually \
             compared. This is not a pass — the check itself needs attention.",
            cmp.missing_live + cmp.missing_rest
        ),
        DhanLiveXverifyOutcome::NoData => "⚠️ Dhan live-vs-official check found no data on \
             either side today — nothing was compared, so nothing is proven."
            .to_string(),
        DhanLiveXverifyOutcome::Degraded => "🆘 Dhan live-vs-official check could not finish \
             today — nothing was compared, so nothing is proven."
            .to_string(),
        DhanLiveXverifyOutcome::Clean => format!(
            "✅ Dhan live prices matched Dhan's own official record for all {} \
             minute(s) checked today.",
            cmp.minutes_compared
        ),
        DhanLiveXverifyOutcome::Partial | DhanLiveXverifyOutcome::Diverged => format!(
            "⚠️ Dhan live-vs-official check: {} minute(s) compared, {} price \
             difference(s), {} minute(s) we never received, {} minute(s) only \
             we had. Typical difference ₹{:.2}, worst ₹{:.2}. Neither side is \
             the official truth on its own — compare against previous days \
             before acting.",
            cmp.minutes_compared,
            cmp.cells_diverged,
            cmp.missing_live,
            cmp.missing_rest,
            rupees(cmp.noise_p50_paise),
            rupees(cmp.noise_max_paise),
        ),
    }
}

// ---------------------------------------------------------------------------
// Reading the two sides
// ---------------------------------------------------------------------------

/// Parse the live-side QuestDB `/exec` dataset into [`SideBar`]s.
///
/// Column order matches [`live_candles_select_sql`]:
/// `ts_nanos, security_id, segment, open, high, low, close, volume`.
///
/// `cap` is the row CAP; the query asks for `cap + 1` (the truncation probe),
/// so `truncated` fires ONLY when the dataset carries MORE than the cap — an
/// exactly-cap day is complete, never a false `partial`. When truncated only
/// the first `cap` rows are returned, so the probe row never enters the
/// compare. Individual malformed rows are SKIPPED and counted, never silently
/// coerced.
///
/// # Errors
/// Malformed body (not JSON / no `dataset` array).
pub fn parse_live_dataset(body: &str, cap: usize) -> Result<(Vec<SideBar>, bool, usize), String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("malformed JSON: {e}"))?;
    let rows = v
        .get("dataset")
        .and_then(|d| d.as_array())
        .ok_or_else(|| "missing dataset".to_string())?;
    let truncated = rows.len() > cap;

    let num = |c: &[serde_json::Value], i: usize| -> Option<f64> {
        c.get(i).and_then(serde_json::Value::as_f64)
    };
    let int = |c: &[serde_json::Value], i: usize| -> Option<i64> {
        c.get(i).and_then(|x| {
            x.as_i64().or_else(|| {
                // APPROVED: cold-path float-to-i64 column fallback precedent
                #[allow(clippy::cast_possible_truncation)]
                x.as_f64().filter(|f| f.is_finite()).map(|f| f as i64)
            })
        })
    };

    let mut out = Vec::new();
    let mut malformed = 0usize;
    for row in rows.iter().take(cap) {
        let Some(cols) = row.as_array() else {
            malformed = malformed.saturating_add(1);
            continue;
        };
        let (Some(ts_nanos), Some(security_id)) = (int(cols, 0), int(cols, 1)) else {
            malformed = malformed.saturating_add(1);
            continue;
        };
        let Some(segment) = cols.get(2).and_then(|s| s.as_str()) else {
            malformed = malformed.saturating_add(1);
            continue;
        };
        let (Some(o), Some(h), Some(l), Some(c)) =
            (num(cols, 3), num(cols, 4), num(cols, 5), num(cols, 6))
        else {
            malformed = malformed.saturating_add(1);
            continue;
        };
        let volume = int(cols, 7).unwrap_or(0);
        // `from_rupees` rejects non-finite prices — a malformed row, never a
        // silent ₹0.00 comparison.
        let Some(bar) = PaiseBar::from_rupees(o, h, l, c, volume) else {
            malformed = malformed.saturating_add(1);
            continue;
        };
        out.push(SideBar {
            security_id,
            segment: segment.to_string(),
            minute_ts_ist_nanos: ts_nanos,
            bar,
        });
    }
    Ok((out, truncated, malformed))
}

/// Where today's sweep begins in the target list.
///
/// The loop has a hard wall-clock budget and `break`s on it. With a FIXED
/// start that is not "we cover fewer instruments" — it is "we never reach the
/// tail", identically, every single day. A rotating start converts permanent
/// starvation into a sweep that comes back around.
///
/// **The stride is a third of the list, and that number is the guarantee.**
/// Consecutive runs cover `[k·s, k·s + c)` for coverage `c`; their union is
/// the whole list exactly when `c >= s`. At `s = ceil(n/3)` any run reaching a
/// third of the list gives FULL coverage within three days, and the measured
/// worst case (77% at the limiter's floor) closes it in two.
///
/// **Stateless on purpose.** A persisted cursor would resume exactly, and it
/// would also be a file to lose, corrupt, or have differ between a laptop and
/// the box — a new failure mode in the one check that exists to catch
/// failures. The trading date is state everyone already agrees on.
///
/// The guarantee is only as good as the coverage, so the run reports the
/// fraction it reached: if that ever falls below a third, this rotation no
/// longer covers everything and the number says so rather than the shape
/// quietly breaking.
pub fn rotation_start(trading_date: chrono::NaiveDate, n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    // `div_ceil`, NOT `n / 3`. Rounding DOWN leaves `n mod 3` indexes that
    // three strides never reach — 868 gives stride 289, three of which is
    // 867, so exactly one instrument stays starved forever. A test caught
    // that off-by-one; it is the same defect this rotation exists to kill,
    // one index wide instead of two hundred.
    let stride = n.div_ceil(3).max(1);
    // `num_days_from_ce` is monotonic across years, so a year boundary does
    // not reset the sweep — a January restart would re-starve whatever
    // December was on.
    let day = i64::from(chrono::Datelike::num_days_from_ce(&trading_date));
    // Reduce BEFORE multiplying: `day` is ~739,000 and a large `n` would
    // overflow the product on a 32-bit usize. Wrong start indexes are
    // survivable; a panic in the only loss detector is not.
    let day_mod = day.rem_euclid(n as i64).unsigned_abs() as usize;
    day_mod.wrapping_mul(stride) % n
}

/// Adapt Dhan's parsed REST intraday candles into [`SideBar`]s for one
/// instrument. Returns the bars plus the count of candles rejected as
/// malformed (non-finite price). Pure.
#[must_use]
pub fn rest_candles_to_side_bars(
    candles: &[crate::dhan_intraday_parse::MinuteCandle],
    security_id: i64,
    segment: &str,
) -> (Vec<SideBar>, usize) {
    let mut out = Vec::with_capacity(candles.len());
    let mut malformed = 0usize;
    for c in candles {
        match PaiseBar::from_rupees(c.open, c.high, c.low, c.close, c.volume) {
            Some(bar) => out.push(SideBar {
                security_id,
                segment: segment.to_string(),
                minute_ts_ist_nanos: c.minute_ts_ist_nanos,
                bar,
            }),
            None => malformed = malformed.saturating_add(1),
        }
    }
    (out, malformed)
}

// ---------------------------------------------------------------------------
// The bounded runner
// ---------------------------------------------------------------------------

/// One instrument to cross-verify.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XverifyTarget {
    /// Dhan `security_id`, as the REST body wants it (string) and as the
    /// live rows carry it (integer).
    pub security_id: i64,
    /// Dhan `exchangeSegment` string, e.g. `IDX_I` / `NSE_FNO`.
    pub segment: String,
    /// Dhan `instrument` string, e.g. `INDEX` / `FUTIDX` / `OPTIDX`.
    pub instrument: String,
}

// ---------------------------------------------------------------------------
// REST fetch failure — typed, because the reason used to be thrown away
// ---------------------------------------------------------------------------

/// Why one target's REST fetch did not produce comparable bars.
///
/// # Why this type exists (measured, 2026-08-26)
///
/// [`fetch_rest_side_for_target`] has always built a precise reason string for
/// every failure mode — and its only caller matched it as `Err(_)`, counted
/// `rest_failures += 1`, and dropped the reason on the floor. The consequence
/// was visible on the live box and unexplainable from it:
///
/// | session | targets | `rest_failures` | actually compared |
/// |---|---:|---:|---:|
/// | 2026-08-24 | 864 | **814** | ~50 |
/// | 2026-08-25 | 865 | **815** | ~50 |
///
/// So the lane's ONLY ground truth ran every day at 94% blind, reported the
/// blindness honestly as a single number, and gave nobody a way to act on it.
/// A count without a cause is a symptom you cannot treat.
///
/// The kinds below are a CLOSED, bounded set so the breakdown can be logged
/// (and, if it ever earns a metric, labelled) without unbounded cardinality —
/// an HTTP status is folded to its class, never carried verbatim.
///
/// [`Self::RateLimited`] is derived from the REAL [`reqwest::StatusCode`],
/// never a substring scan of the message — the same discipline the spot-1m leg
/// records for feeding its self-tuner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XverifyFetchFailureKind {
    /// The vendor answered 2xx with a body carrying no usable candle for the
    /// day. This is the shape a WRONG `instrument` label produces, and it is
    /// also the shape a genuinely untraded instrument produces — the two are
    /// indistinguishable from one response, which is why the count matters.
    NoCandles,
    /// HTTP 429 — the Dhan Data-API budget was exceeded.
    RateLimited,
    /// Any other 4xx. A wrong request shape, a bad token, a rejected id.
    Http4xx,
    /// Any 5xx — the vendor's own failure.
    Http5xx,
    /// The request or the body read exceeded the per-fetch timeout.
    Timeout,
    /// Transport-level failure before a status existed (DNS, TLS, reset).
    Transport,
    /// Anything the arms above do not cover — kept so the set is total and a
    /// future arm can never silently fold into a neighbour's count.
    Other,
}

impl XverifyFetchFailureKind {
    /// Stable, lowercase, bounded label — safe for a log field or a metric.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoCandles => "no_candles",
            Self::RateLimited => "rate_limited",
            Self::Http4xx => "http_4xx",
            Self::Http5xx => "http_5xx",
            Self::Timeout => "timeout",
            Self::Transport => "transport",
            Self::Other => "other",
        }
    }

    /// Every kind, in report order. Used by the breakdown so a new arm cannot
    /// be added without appearing in the log line.
    #[must_use]
    pub const fn all() -> [Self; 7] {
        [
            Self::NoCandles,
            Self::RateLimited,
            Self::Http4xx,
            Self::Http5xx,
            Self::Timeout,
            Self::Transport,
            Self::Other,
        ]
    }
}

/// A typed fetch failure: the bounded kind for counting, plus the full message
/// for a bounded log sample.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XverifyFetchFailure {
    pub kind: XverifyFetchFailureKind,
    pub msg: String,
}

impl XverifyFetchFailure {
    fn new(kind: XverifyFetchFailureKind, msg: impl Into<String>) -> Self {
        Self {
            kind,
            msg: msg.into(),
        }
    }
}

/// Per-kind tally of one run's REST failures.
///
/// Deliberately a fixed-width array over [`XverifyFetchFailureKind::all`], not
/// a map: the kind set is closed, so the tally is O(1) in space and the log
/// line's shape is stable from run to run — an operator comparing two days is
/// comparing the same fields.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RestFailureBreakdown {
    counts: [usize; 7],
}

impl RestFailureBreakdown {
    /// Records one failure.
    pub fn record(&mut self, kind: XverifyFetchFailureKind) {
        let idx = XverifyFetchFailureKind::all()
            .iter()
            .position(|k| *k == kind)
            .unwrap_or(XverifyFetchFailureKind::all().len() - 1);
        if let Some(slot) = self.counts.get_mut(idx) {
            *slot = slot.saturating_add(1);
        }
    }

    /// Count for one kind.
    #[must_use]
    pub fn get(&self, kind: XverifyFetchFailureKind) -> usize {
        XverifyFetchFailureKind::all()
            .iter()
            .position(|k| *k == kind)
            .and_then(|i| self.counts.get(i))
            .copied()
            .unwrap_or(0)
    }

    /// Total across every kind — must equal `RunReport::rest_failures`.
    #[must_use]
    pub fn total(&self) -> usize {
        self.counts.iter().fold(0usize, |a, b| a.saturating_add(*b))
    }

    /// `kind=count` pairs, dominant kind FIRST, for the log line. Zero-count
    /// kinds are omitted so a quiet run stays short; an all-zero breakdown
    /// renders as `none`.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut pairs: Vec<(XverifyFetchFailureKind, usize)> = XverifyFetchFailureKind::all()
            .into_iter()
            .map(|k| (k, self.get(k)))
            .filter(|(_, n)| *n > 0)
            .collect();
        pairs.sort_by_key(|p| std::cmp::Reverse(p.1));
        if pairs.is_empty() {
            return "none".to_string();
        }
        pairs
            .into_iter()
            .map(|(k, n)| format!("{}={n}", k.as_str()))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// What one run did — returned to the caller for logging and metrics.
#[derive(Clone, Debug, PartialEq)]
pub struct RunReport {
    pub comparison: DayComparison,
    /// Targets whose REST fetch failed, timed out, or returned no candles.
    pub rest_failures: usize,
    /// Per-reason tally of those failures. Added 2026-08-26: `rest_failures`
    /// alone said 815-of-865 without ever saying WHY.
    pub rest_failure_breakdown: RestFailureBreakdown,
    /// `true` when any leg failed, the live read truncated, or the run budget
    /// elapsed — forces the verdict down to `partial` / `degraded`.
    pub degraded: bool,
    /// Rows skipped as malformed on either side (counted, never coerced).
    pub malformed_rows: usize,
    /// `true` when the run budget elapsed before every target was fetched.
    pub budget_elapsed: bool,
    /// The vendor's own minute candles, exactly as fetched — added
    /// 2026-08-26 on operator instruction.
    ///
    /// These bars ALREADY existed in memory: the comparison built them,
    /// judged them, and dropped them. Carrying them out is a move, not a
    /// copy, so it costs no extra allocation — and it is the difference
    /// between a comparison that can be re-run offline and one that must
    /// re-fetch ~868 rate-limited requests to say anything twice.
    pub rest_tape: Vec<tickvault_storage::dhan_live_crossverify_persistence::DhanRestTapeRow>,
}

/// Read the live side out of QuestDB via `/exec`, bounded by `timeout`.
///
/// Returns the parsed rows plus the truncation and malformed-row counts.
///
/// # Errors
/// Transport failure, non-2xx, timeout, or an unparseable body.
pub async fn read_live_side(
    client: &reqwest::Client,
    questdb_exec_url: &str,
    day_start_ist_nanos: i64,
    timeout: std::time::Duration,
) -> Result<(Vec<SideBar>, bool, usize), String> {
    let sql = live_candles_select_sql(day_start_ist_nanos);
    let fut = client
        .get(questdb_exec_url)
        .query(&[("query", sql.as_str())])
        .send();
    let resp = tokio::time::timeout(timeout, fut)
        .await
        .map_err(|_| "live read timed out".to_string())?
        .map_err(|e| format!("live read transport: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("live read http {status}"));
    }
    let body = tokio::time::timeout(timeout, resp.text())
        .await
        .map_err(|_| "live read body timed out".to_string())?
        .map_err(|e| format!("live read body: {e}"))?;
    parse_live_dataset(&body, LIVE_ROW_LIMIT)
}

/// Maps a non-success HTTP status to a bounded failure kind. Pure.
///
/// Split out of the fetch path so it is directly testable — the alternative is
/// a guard that greps for the branch spelling, and this session has already
/// found three such guards asserting a spelling while the consequence was free
/// to change underneath them.
///
/// Classification is from the REAL [`reqwest::StatusCode`], never a substring
/// scan of a rendered message.
#[must_use]
pub fn classify_http_status(status: reqwest::StatusCode) -> XverifyFetchFailureKind {
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        XverifyFetchFailureKind::RateLimited
    } else if status.is_client_error() {
        XverifyFetchFailureKind::Http4xx
    } else if status.is_server_error() {
        XverifyFetchFailureKind::Http5xx
    } else {
        XverifyFetchFailureKind::Other
    }
}

/// Fetch ONE target's official 1-minute tape from Dhan, bounded by `timeout`.
///
/// The ONLY endpoint this module ever contacts:
/// `POST /v2/charts/intraday`, `interval="1"` — the
/// `no-rest-except-live-feed-2026-06-27.md` §8 KEEP class. The request body
/// is built by the SHARED [`crate::dhan_intraday_parse::intraday_request_body`]
/// so the day-granular window convention matches the spot-1m leg exactly.
///
/// # Errors
/// Transport failure, non-2xx, or timeout. The token is never logged.
pub async fn fetch_rest_side_for_target(
    client: &reqwest::Client,
    intraday_url: &str,
    jwt: &str,
    target: &XverifyTarget,
    trading_date: chrono::NaiveDate,
    timeout: std::time::Duration,
) -> Result<Vec<SideBar>, XverifyFetchFailure> {
    use XverifyFetchFailureKind as K;
    let next_day = trading_date
        .succ_opt()
        .ok_or_else(|| XverifyFetchFailure::new(K::Other, "trading date has no successor"))?;
    let body = crate::dhan_intraday_parse::intraday_request_body(
        &target.security_id.to_string(),
        &target.segment,
        &target.instrument,
        trading_date,
        next_day,
    );
    let fut = client
        .post(intraday_url)
        .header("access-token", jwt)
        .header("Content-Type", "application/json")
        .json(&body)
        .send();
    let resp = tokio::time::timeout(timeout, fut)
        .await
        .map_err(|_| XverifyFetchFailure::new(K::Timeout, "rest fetch timed out"))?
        // The token never appears in the error: reqwest renders the URL, not
        // headers, and the URL carries no query params here.
        .map_err(|e| {
            XverifyFetchFailure::new(K::Transport, format!("rest fetch transport: {e}"))
        })?;
    let status = resp.status();
    if !status.is_success() {
        // Classified from the REAL StatusCode, never a substring scan of the
        // rendered message — the discipline the spot-1m leg records for
        // feeding its self-tuner, and the reason `rate_limited` here can be
        // trusted as an answer rather than a guess.
        return Err(XverifyFetchFailure::new(
            classify_http_status(status),
            format!("rest fetch http {status}"),
        ));
    }
    let text = tokio::time::timeout(timeout, resp.text())
        .await
        .map_err(|_| XverifyFetchFailure::new(K::Timeout, "rest body timed out"))?
        .map_err(|e| XverifyFetchFailure::new(K::Transport, format!("rest body: {e}")))?;
    let candles = crate::dhan_intraday_parse::parse_intraday_1m_candles(&text);
    let (bars, malformed) =
        rest_candles_to_side_bars(&candles, target.security_id, &target.segment);
    if bars.is_empty() && malformed == 0 {
        return Err(XverifyFetchFailure::new(
            K::NoCandles,
            "rest returned no candles",
        ));
    }
    Ok(bars)
}

/// [`fetch_rest_side_for_target`], paced against the shared Dhan Data-API
/// budget and feeding the self-tuner on a real 429.
///
/// # Why this wrapper exists (2026-08-26)
///
/// The run's fetch loop issued its targets **back-to-back with no pacing at
/// all** — ~865 requests against a documented 5-per-second Data-API budget —
/// while the two sibling REST legs (spot-1m and the option chain) have both
/// gone through the shared self-tuning limiter since 2026-07-14. This leg was
/// the odd one out, and an unpaced burst is a defect on its own terms whether
/// or not it is the cause of any particular day's failures.
///
/// **Stated honestly: this is hygiene, not a claimed cure.** Whether rate
/// limiting is what blinded the comparator is exactly what the new
/// [`RestFailureBreakdown`] answers — and the two changes do not confound each
/// other, because the breakdown labels the outcome that actually occurred. If
/// tomorrow reports `rate_limited=0`, pacing was never the cause and the
/// dominant label names the real one.
async fn fetch_rest_side_paced(
    client: &reqwest::Client,
    intraday_url: &str,
    jwt: &str,
    target: &XverifyTarget,
    trading_date: chrono::NaiveDate,
    timeout: std::time::Duration,
) -> Result<Vec<SideBar>, XverifyFetchFailure> {
    crate::dhan_data_api_limiter::shared_dhan_data_api_limiter()
        .acquire()
        .await;
    let out =
        fetch_rest_side_for_target(client, intraday_url, jwt, target, trading_date, timeout).await;
    if let Err(f) = &out
        && f.kind == XverifyFetchFailureKind::RateLimited
    {
        crate::dhan_data_api_limiter::shared_dhan_data_api_limiter().record_429();
    }
    out
}

/// Run one full cross-verification and return the report. COLD PATH.
///
/// Bounded on every axis, and it never blocks anything else: each REST fetch
/// and the live read carry their own `tokio::time::timeout`; the whole run
/// stops issuing new fetches once `run_budget_secs` has elapsed (stamping
/// `budget_elapsed`, which forces the verdict down). A failing leg degrades
/// the verdict — it never fabricates a clean one.
///
/// This function is I/O only; every judgement lives in the pure
/// [`compare_day`], which is what the tests exercise.
///
/// # Errors
/// Only when the LIVE read fails outright — in that case there is nothing to
/// compare against and the caller stamps the day `degraded`.
pub async fn run_cross_verification(
    client: &reqwest::Client,
    questdb_exec_url: &str,
    intraday_url: &str,
    jwt: &str,
    targets: &[XverifyTarget],
    trading_date: chrono::NaiveDate,
    day_start_ist_nanos: i64,
    cfg: &DhanLiveCrossverifyConfig,
) -> Result<RunReport, String> {
    let started = std::time::Instant::now();
    let budget = std::time::Duration::from_secs(cfg.run_budget_secs);
    let fetch_timeout = std::time::Duration::from_secs(cfg.fetch_timeout_secs);

    let (live, truncated, live_malformed) = read_live_side(
        client,
        questdb_exec_url,
        day_start_ist_nanos,
        std::time::Duration::from_secs(cfg.live_read_timeout_secs),
    )
    .await?;

    let mut rest: Vec<SideBar> = Vec::new();
    // The vendor tape, captured as it arrives. Stamped PER TARGET rather than
    // once for the run: this loop runs for up to ten minutes, so a single
    // run-level stamp would claim the last instrument was fetched at the same
    // instant as the first and destroy the one number that says how stale the
    // vendor's own record was when we read it.
    let mut rest_tape: Vec<tickvault_storage::dhan_live_crossverify_persistence::DhanRestTapeRow> =
        Vec::new();
    let mut rest_failures = 0usize;
    let mut rest_failure_breakdown = RestFailureBreakdown::default();
    // Bounded sample of the actual messages. A run can fail 815 times; logging
    // 815 lines would bury the verdict it exists to deliver, and logging none
    // is what left the failure unexplainable for two sessions. Three is enough
    // to read the shape and small enough to never flood.
    const FAILURE_SAMPLE_MAX: usize = 3;
    let mut failure_samples: Vec<String> = Vec::with_capacity(FAILURE_SAMPLE_MAX);
    let mut budget_elapsed = false;
    let target_count = targets.len();

    // ---- the sweep starts somewhere different every day ---------------
    //
    // MEASURED DEFECT (2026-08-26). This loop walked `targets` in FIXED order
    // and `break`s on the budget. Fixed order plus truncation is not "we get
    // to fewer instruments" — it is "the tail is NEVER reached", on any day,
    // ever. Coverage would look like a respectable 77% while a specific 23%
    // of the universe went permanently unverified, and nothing in the report
    // would say which.
    //
    // Today's pacing change is what makes it bite: at the limiter's 2 rps
    // floor with 0.4s answers, 868 sequential targets need ~781s against a
    // 600s budget. So the truncation is no longer hypothetical.
    //
    // The fix is a rotating start, and it is deliberately STATELESS —
    // derived from the trading date, not from a cursor file. A file would be
    // one more thing to lose, corrupt, or have differ between a laptop and
    // the box; the date is already agreed by everyone. See `rotation_start`
    // for why the stride is a third.
    let start = rotation_start(trading_date, target_count);
    for offset in 0..target_count {
        // Wrap: the sweep runs to the end of the list and continues from the
        // front, so a run always covers a CONTIGUOUS window — just not always
        // the same one.
        let Some(target) = targets.get((start + offset) % target_count) else {
            break;
        };
        if started.elapsed() >= budget {
            // Stop issuing work; the verdict degrades rather than pretending
            // the un-fetched targets were clean.
            budget_elapsed = true;
            break;
        }
        match fetch_rest_side_paced(
            client,
            intraday_url,
            jwt,
            target,
            trading_date,
            fetch_timeout,
        )
        .await
        {
            Ok(mut bars) => {
                let fetched_at_nanos = crate::spot_1m_rest_boot::fetched_at_ist_nanos_now();
                rest_tape.extend(bars.iter().map(|b| {
                    tickvault_storage::dhan_live_crossverify_persistence::DhanRestTapeRow {
                        minute_ts_ist_nanos: b.minute_ts_ist_nanos,
                        trading_date_ist_nanos: day_start_ist_nanos,
                        security_id: b.security_id,
                        segment: b.segment.clone(),
                        instrument: target.instrument.clone(),
                        open: b.bar.open_r,
                        high: b.bar.high_r,
                        low: b.bar.low_r,
                        close: b.bar.close_r,
                        volume: b.bar.volume,
                        fetched_at_nanos,
                    }
                }));
                rest.append(&mut bars);
            }
            Err(failure) => {
                // The reason used to die here as `Err(_)`. See
                // `XverifyFetchFailureKind` for what that cost.
                rest_failures = rest_failures.saturating_add(1);
                rest_failure_breakdown.record(failure.kind);
                if failure_samples.len() < FAILURE_SAMPLE_MAX {
                    failure_samples.push(format!(
                        "{}/{} {}: {}",
                        target.segment, target.security_id, target.instrument, failure.msg
                    ));
                }
            }
        }
    }

    let degraded = truncated || budget_elapsed || rest_failures > 0;
    let run_ts = deterministic_run_ts_nanos(day_start_ist_nanos);
    // The scope set: exactly what this run ASKED the vendor for. A live
    // instrument outside it was never requested, so calling its minutes
    // "missing from the vendor" is a scope mismatch dressed as data loss —
    // and on 2026-08-26 that dressing produced 757,273 of 764,003 findings
    // and blew the write buffer past its ceiling, discarding the whole day.
    let in_scope: std::collections::BTreeSet<(i64, String)> = targets
        .iter()
        .map(|t| (t.security_id, t.segment.clone()))
        .collect();
    let comparison = compare_day_in_scope(
        &live,
        &rest,
        &in_scope,
        day_start_ist_nanos,
        run_ts,
        cfg.tolerance_paise,
        degraded,
    );

    if rest_failures > 0 {
        // ONE line, not 815. The breakdown answers "why", the sample shows
        // what one looks like, and both live beside the verdict rather than in
        // a separate stream the operator would have to correlate by hand.
        tracing::warn!(
            rest_failures,
            targets = target_count,
            reasons = %rest_failure_breakdown.summary(),
            sample = ?failure_samples,
            "Dhan cross-verification could not fetch the vendor tape for some targets —              each one is an instrument captured but NOT verified today"
        );
    }
    Ok(RunReport {
        comparison,
        rest_failures,
        rest_failure_breakdown,
        degraded,
        malformed_rows: live_malformed,
        budget_elapsed,
        rest_tape,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-08-10 00:00:00 IST expressed the way the repo does it: the naive
    /// IST wall-clock date treated as UTC-epoch seconds, ×1e9.
    const DAY_START_NANOS: i64 = 1_786_233_600 * NANOS_PER_SEC;

    fn minute(secs_of_day: i64) -> i64 {
        DAY_START_NANOS.saturating_add(secs_of_day * NANOS_PER_SEC)
    }

    /// Unscoped comparison — every live instrument participates.
    ///
    /// Retained so the existing tests read unchanged; production goes through
    /// [`compare_day_in_scope`] with the run's real target set.
    ///
    /// Lives INSIDE the test module rather than carrying a test-only
    /// attribute in production code. A guard written this morning asserts
    /// this file has exactly ONE test-module marker, and it fired twice while
    /// this helper was being placed: once for the attribute itself, and once
    /// for a comment that merely NAMED the attribute. Both times it was
    /// right — a source scanner cannot tell quoting from doing, so nothing
    /// here spells that attribute out.
    pub fn compare_day(
        live: &[SideBar],
        rest: &[SideBar],
        day_start_ist_nanos: i64,
        run_ts_ist_nanos: i64,
        tolerance_paise: i64,
        degraded: bool,
    ) -> DayComparison {
        compare_day_in_scope(
            live,
            rest,
            &std::collections::BTreeSet::new(),
            day_start_ist_nanos,
            run_ts_ist_nanos,
            tolerance_paise,
            degraded,
        )
    }

    fn bar(o: f64, h: f64, l: f64, c: f64) -> PaiseBar {
        PaiseBar::from_rupees(o, h, l, c, 100).expect("finite")
    }

    fn side(sid: i64, secs: i64, b: PaiseBar) -> SideBar {
        SideBar {
            security_id: sid,
            segment: "IDX_I".to_string(),
            minute_ts_ist_nanos: minute(secs),
            bar: b,
        }
    }

    const OPEN: i64 = SESSION_OPEN_SECS_OF_DAY_IST;
    const RUN_TS: i64 = 42;

    // =======================================================================
    // THE #1474 REGRESSION LOCK — the bug that blinded the predecessor
    // =======================================================================

    /// The `WHERE` bounds must be MICROSECONDS. If a future edit passes
    /// nanoseconds straight through, the magnitude jumps 1000× and this test
    /// fails — which is precisely the failure that went undetected for the
    /// entire life of the retired cross-verify.
    #[test]
    fn day_bounds_micros_where_window_is_microseconds_not_nanoseconds() {
        let (start, end) = day_bounds_micros(DAY_START_NANOS);

        // Exact relationship, stated in units.
        assert_eq!(
            start,
            DAY_START_NANOS / 1_000,
            "WHERE bounds must be nanos / 1000 (micros)"
        );
        assert_eq!(end - start, 86_400 * 1_000_000, "one day, in micros");

        // Digit-magnitude guard: micros for a 2026 date has 16 digits;
        // nanoseconds has 19. A 1000× unit error is a 3-digit jump.
        let digits = start.to_string().len();
        assert_eq!(
            digits, 16,
            "epoch-MICROS for a 2026 date must be 16 digits; got {digits} \
             from {start}. 19 digits means nanoseconds leaked into the WHERE \
             clause — the exact PR #1474 bug."
        );
        assert!(
            start < 100_000_000_000_000_000,
            "bound {start} is far too large to be microseconds"
        );

        // And the same guard on the rendered SQL, so a hand-edited format!
        // string cannot regress past the helper.
        let sql = live_candles_select_sql(DAY_START_NANOS);
        assert!(sql.contains(&format!("ts >= {start}")));
        assert!(sql.contains(&format!("ts < {end}")));
        assert!(
            !sql.contains(&DAY_START_NANOS.to_string()),
            "the raw NANOSECOND value must never appear in the WHERE clause: {sql}"
        );
    }

    /// Reproduces the original defect and asserts we would reject it.
    ///
    /// QuestDB reads a bare integer against a TIMESTAMP column as epoch
    /// MICROSECONDS. Feeding nanoseconds therefore lands the window ~1000×
    /// into the future — the retired module's window sat around year 58502,
    /// matched zero rows on every run, and reported a harmless-looking
    /// `compared=0`.
    #[test]
    fn day_bounds_micros_rejects_the_nanosecond_window_of_year_58502() {
        // What the buggy code did: use nanos directly as the micros literal.
        let buggy_bound_micros = DAY_START_NANOS;
        let buggy_epoch_secs = buggy_bound_micros / 1_000_000;
        let buggy_year = 1970 + buggy_epoch_secs / 31_556_952;

        assert!(
            buggy_year > 50_000,
            "sanity: feeding nanos as micros must land in the far future \
             (got year {buggy_year})"
        );

        // What the fixed code does.
        let (good_bound_micros, _) = day_bounds_micros(DAY_START_NANOS);
        let good_epoch_secs = good_bound_micros / 1_000_000;
        let good_year = 1970 + good_epoch_secs / 31_556_952;

        assert!(
            (2020..=2100).contains(&good_year),
            "the WHERE window must land in the present era, got year {good_year}"
        );
        assert_ne!(
            good_bound_micros, buggy_bound_micros,
            "the fixed bound must NOT equal the raw nanosecond value"
        );
        // The whole point: exactly a 1000× separation.
        assert_eq!(buggy_bound_micros / good_bound_micros, 1_000);
    }

    /// The projected key must come back as NANOSECONDS, because `(ts / 1)`
    /// yields micros. Getting this half right (fixed WHERE, unfixed
    /// projection) would join on keys 1000× too small and compare nothing.
    #[test]
    fn live_candles_select_sql_rescales_projected_key_to_nanoseconds() {
        let sql = live_candles_select_sql(DAY_START_NANOS);
        assert!(
            sql.contains("(ts / 1) * 1000 AS ts_nanos"),
            "projection must re-scale micros back to nanos: {sql}"
        );
    }

    #[test]
    fn live_candles_select_sql_is_feed_scoped_ordered_and_limit_probed() {
        let sql = live_candles_select_sql(DAY_START_NANOS);
        assert!(sql.contains("FROM candles_1m"));
        assert!(sql.contains("feed = 'dhan'"), "must not read another feed");
        assert!(sql.contains("ORDER BY ts ASC"));
        assert!(
            sql.contains(&format!("LIMIT {}", LIVE_ROW_LIMIT + 1)),
            "LIMIT must be cap+1 (the truncation probe): {sql}"
        );
        // Live-feed purity: this module reads candles_1m and writes nothing here.
        for forbidden in ["INSERT", "UPDATE", "DELETE", "DROP"] {
            assert!(!sql.to_ascii_uppercase().contains(forbidden));
        }
    }

    // =======================================================================
    // THE NON-VACUITY CONTRACT — compared == 0 can never be a pass
    // =======================================================================

    /// `compared == 0` must be a loud, DISTINCT outcome — never a pass.
    ///
    /// This is the behavioural half of the #1474 lesson: the predecessor's
    /// window matched nothing and that state was operationally
    /// indistinguishable from "all good".
    #[test]
    fn compare_day_zero_comparisons_is_never_a_pass_and_is_vacuous() {
        // Case 1: rows on both sides, ZERO overlap (the year-58502 signature
        // — here reproduced by disjoint minutes).
        let live = vec![side(13, OPEN, bar(100.0, 101.0, 99.0, 100.5))];
        let rest = vec![side(13, OPEN + 60, bar(100.0, 101.0, 99.0, 100.5))];
        let cmp = compare_day(&live, &rest, DAY_START_NANOS, RUN_TS, 0, false);
        assert_eq!(cmp.minutes_compared, 0);
        assert_eq!(
            cmp.outcome,
            DhanLiveXverifyOutcome::Blind,
            "rows seen but nothing compared MUST be `blind`"
        );
        assert!(!cmp.outcome.is_pass(), "blind is not a pass");
        assert!(cmp.is_vacuous());
        assert!(
            format_summary_line(&cmp).contains("PROVED NOTHING"),
            "the operator line must say so in the first words: {}",
            format_summary_line(&cmp)
        );

        // Case 2: literally nothing on either side.
        let cmp = compare_day(&[], &[], DAY_START_NANOS, RUN_TS, 0, false);
        assert_eq!(cmp.minutes_compared, 0);
        assert_eq!(cmp.outcome, DhanLiveXverifyOutcome::NoData);
        assert!(!cmp.outcome.is_pass());
        assert!(cmp.is_vacuous());

        // Case 3: a degraded run that compared nothing.
        let cmp = compare_day(&[], &[], DAY_START_NANOS, RUN_TS, 0, true);
        assert_eq!(cmp.outcome, DhanLiveXverifyOutcome::Degraded);
        assert!(!cmp.outcome.is_pass());
        assert!(cmp.is_vacuous());
    }

    /// Exhaustive structural proof: `Clean` is unreachable whenever
    /// `minutes_compared == 0`, for every combination of inputs that can
    /// produce a zero comparison.
    #[test]
    fn compare_day_clean_is_structurally_unreachable_without_a_comparison() {
        let one = vec![side(13, OPEN, bar(1.0, 1.0, 1.0, 1.0))];
        let other = vec![side(99, OPEN, bar(1.0, 1.0, 1.0, 1.0))];
        let cases: Vec<(&[SideBar], &[SideBar], bool)> = vec![
            (&[], &[], false),
            (&[], &[], true),
            (&one, &[], false),
            (&[], &one, false),
            (&one, &other, false),
            (&one, &other, true),
        ];
        for (live, rest, degraded) in cases {
            let cmp = compare_day(live, rest, DAY_START_NANOS, RUN_TS, 0, degraded);
            if cmp.minutes_compared == 0 {
                assert_ne!(
                    cmp.outcome,
                    DhanLiveXverifyOutcome::Clean,
                    "a run that compared nothing rendered CLEAN — this is the \
                     blind-since-birth failure class"
                );
                assert!(!cmp.outcome.is_pass());
            }
        }
    }

    /// A vacuous rerun must never erase a measured verdict.
    #[test]
    fn may_overwrite_refuses_to_erase_a_measured_verdict() {
        for vacuous in [
            DhanLiveXverifyOutcome::Blind,
            DhanLiveXverifyOutcome::NoData,
            DhanLiveXverifyOutcome::Degraded,
        ] {
            assert!(
                !may_overwrite(Some(DhanLiveXverifyOutcome::Diverged), vacuous),
                "{} must not erase a measured `diverged` day",
                vacuous.as_str()
            );
            assert!(
                !may_overwrite(Some(DhanLiveXverifyOutcome::Clean), vacuous),
                "{} must not erase a measured `clean` day",
                vacuous.as_str()
            );
            // ...but a vacuous day may be replaced by anything.
            assert!(may_overwrite(Some(vacuous), DhanLiveXverifyOutcome::Clean));
            assert!(may_overwrite(Some(vacuous), vacuous));
        }
        assert!(may_overwrite(None, DhanLiveXverifyOutcome::Blind));
    }

    // =======================================================================
    // Comparison semantics
    // =======================================================================

    #[test]
    fn compare_day_matching_bars_are_clean() {
        let b = bar(100.0, 101.0, 99.0, 100.5);
        let live = vec![side(13, OPEN, b)];
        let rest = vec![side(13, OPEN, b)];
        let cmp = compare_day(&live, &rest, DAY_START_NANOS, RUN_TS, 0, false);
        assert_eq!(cmp.minutes_compared, 1);
        assert_eq!(cmp.cells_diverged, 0);
        assert_eq!(cmp.outcome, DhanLiveXverifyOutcome::Clean);
        assert!(cmp.outcome.is_pass());
        assert!(cmp.findings.is_empty());
        assert_eq!(cmp.instruments, 1);
    }

    /// Integer paise, NOT a float epsilon. `0.1 + 0.2 != 0.3` in binary
    /// floating point, but both sides round to the same paise, so this must
    /// compare EQUAL.
    #[test]
    fn compare_day_uses_integer_paise_never_float_epsilon() {
        let live = vec![side(13, OPEN, bar(0.1 + 0.2, 1.0, 1.0, 1.0))];
        let rest = vec![side(13, OPEN, bar(0.3, 1.0, 1.0, 1.0))];
        assert_ne!(0.1_f64 + 0.2, 0.3, "premise: these differ as raw f64");
        let cmp = compare_day(&live, &rest, DAY_START_NANOS, RUN_TS, 0, false);
        assert_eq!(
            cmp.cells_diverged, 0,
            "float representation noise must not manufacture divergence"
        );
        assert_eq!(cmp.outcome, DhanLiveXverifyOutcome::Clean);
        assert_eq!(to_paise(0.1 + 0.2), to_paise(0.3));
    }

    #[test]
    fn compare_day_tolerance_is_paise_and_excludes_the_boundary() {
        // 5-paise delta on `high`.
        let live = vec![side(13, OPEN, bar(100.0, 101.05, 99.0, 100.5))];
        let rest = vec![side(13, OPEN, bar(100.0, 101.00, 99.0, 100.5))];

        // tolerance 0 → diverged.
        let cmp = compare_day(&live, &rest, DAY_START_NANOS, RUN_TS, 0, false);
        assert_eq!(cmp.cells_diverged, 1);
        assert_eq!(cmp.findings[0].field, "high");
        assert_eq!(cmp.findings[0].diff_paise, 5);
        assert_eq!(cmp.outcome, DhanLiveXverifyOutcome::Diverged);

        // tolerance 5 → `diff > tolerance` is false, so it is within band.
        let cmp = compare_day(&live, &rest, DAY_START_NANOS, RUN_TS, 5, false);
        assert_eq!(cmp.cells_diverged, 0);
        assert_eq!(cmp.outcome, DhanLiveXverifyOutcome::Clean);
        // ...but it STILL counts toward the noise profile.
        assert_eq!(
            cmp.noise_max_paise, 5,
            "in-band deltas must still be measured"
        );

        // tolerance 4 → still diverged.
        let cmp = compare_day(&live, &rest, DAY_START_NANOS, RUN_TS, 4, false);
        assert_eq!(cmp.cells_diverged, 1);
    }

    #[test]
    fn compare_day_missing_minutes_are_their_own_categories() {
        let b = bar(100.0, 101.0, 99.0, 100.5);
        // REST has 09:16, live doesn't → missing_live (the packet-loss proxy).
        let live = vec![side(13, OPEN, b)];
        let rest = vec![side(13, OPEN, b), side(13, OPEN + 60, b)];
        let cmp = compare_day(&live, &rest, DAY_START_NANOS, RUN_TS, 0, false);
        assert_eq!(cmp.minutes_compared, 1);
        assert_eq!(cmp.missing_live, 1);
        assert_eq!(cmp.missing_rest, 0);
        assert_eq!(
            cmp.cells_diverged, 0,
            "a missing minute is NOT an OHLC divergence"
        );
        let f = cmp
            .findings
            .iter()
            .find(|f| f.kind == DhanLiveXverifyCellKind::MissingLive)
            .expect("missing_live finding");
        assert_eq!(f.field, "none");
        assert_eq!(f.live_value, 0.0);

        // Reverse: live has a minute REST doesn't.
        let cmp = compare_day(&rest, &live, DAY_START_NANOS, RUN_TS, 0, false);
        assert_eq!(cmp.missing_rest, 1);
        assert_eq!(cmp.missing_live, 0);
    }

    /// A REST-side hole is not a live-feed failure — but it is not a pass either.
    ///
    /// Until 2026-08-25 `missing_rest` was folded into the same `real` term as
    /// `cells_diverged` and `missing_live`, so a minute the vendor's sparse REST
    /// tape simply never published rendered as `Diverged` — the lane crying
    /// feed-loss about a hole on the other side. This module's header had always
    /// promised the opposite ("reported but never conflated with real
    /// divergence"); the classification table said "yes" and the code followed
    /// the table.
    ///
    /// The verdict must now be `Partial`: `is_pass()` refuses it, so a bar we
    /// hold for a minute the exchange never traded can never be waved through as
    /// clean (it could mean we FABRICATED one — not hypothetical, the aggregator
    /// was proven to inflate volumes 9.2x on 2026-08-24), while `is_measured()`
    /// accepts it, so the keep-better rerun guard still treats the run as real.
    #[test]
    fn a_rest_side_hole_is_partial_never_diverged_and_never_clean() {
        let live = vec![
            side(1, OPEN, bar(100.0, 101.0, 99.0, 100.5)),
            side(1, OPEN + 60, bar(100.5, 102.0, 100.0, 101.0)),
        ];
        // REST published only the first minute.
        let rest = vec![side(1, OPEN, bar(100.0, 101.0, 99.0, 100.5))];

        let cmp = compare_day(&live, &rest, DAY_START_NANOS, RUN_TS, 0, false);

        assert_eq!(cmp.missing_rest, 1, "the REST hole must still be counted");
        assert_eq!(cmp.missing_live, 0);
        assert_eq!(cmp.cells_diverged, 0, "no OHLC field actually disagreed");
        assert!(cmp.minutes_compared > 0, "the run must be non-vacuous");

        assert_eq!(
            cmp.outcome,
            DhanLiveXverifyOutcome::Partial,
            "a REST-side hole alone must be Partial — not Diverged (that blames \
             our feed for the vendor's sparsity) and not Clean (that would wave \
             through a bar we may have fabricated)"
        );
        assert!(
            !cmp.outcome.is_pass(),
            "Partial must never read as a pass — audit Rule 11, no false-OK"
        );
        assert!(
            cmp.outcome.is_measured(),
            "the run compared real minutes, so the rerun guard must treat it as measured"
        );
    }

    /// The other half of the asymmetry: a minute WE missed is still a real
    /// divergence, because it is the closest proxy this system has for packet
    /// loss on our own feed. Narrowing `missing_rest` must not narrow this.
    #[test]
    fn a_minute_missing_from_our_own_feed_is_still_diverged() {
        let live = vec![side(1, OPEN, bar(100.0, 101.0, 99.0, 100.5))];
        let rest = vec![
            side(1, OPEN, bar(100.0, 101.0, 99.0, 100.5)),
            side(1, OPEN + 60, bar(100.5, 102.0, 100.0, 101.0)),
        ];

        let cmp = compare_day(&live, &rest, DAY_START_NANOS, RUN_TS, 0, false);

        assert_eq!(cmp.missing_live, 1);
        assert_eq!(
            cmp.outcome,
            DhanLiveXverifyOutcome::Diverged,
            "a minute absent from OUR side stays a real divergence"
        );
        assert!(!cmp.outcome.is_pass());
    }
    /// The last two session minutes may legitimately be unsealed at 15:31 —
    /// recorded, but never counted as loss.
    #[test]
    fn is_tail_minute_excuses_trailing_two_minutes_not_counted_as_missing() {
        let b = bar(100.0, 101.0, 99.0, 100.5);
        let close = SESSION_CLOSE_SECS_OF_DAY_IST;
        // 15:28 and 15:29 present only in Dhan's REST tape.
        let rest = vec![side(13, close - 120, b), side(13, close - 60, b)];
        let cmp = compare_day(&[], &rest, DAY_START_NANOS, RUN_TS, 0, false);
        assert_eq!(cmp.tail_unsealed, 2);
        assert_eq!(
            cmp.missing_live, 0,
            "unsealed tail minutes must NOT be reported as lost packets"
        );
        assert!(
            cmp.findings
                .iter()
                .all(|f| f.kind == DhanLiveXverifyCellKind::TailUnsealed)
        );
        // Nothing overlapped, so it is still vacuous — never a pass.
        assert!(!cmp.outcome.is_pass());

        // 15:27 is NOT excused — it is a real gap.
        let rest = vec![side(13, close - 180, b)];
        let cmp = compare_day(&[], &rest, DAY_START_NANOS, RUN_TS, 0, false);
        assert_eq!(cmp.tail_unsealed, 0);
        assert_eq!(cmp.missing_live, 1);
    }

    /// The split that makes `missing_live` actionable (2026-08-26).
    ///
    /// On 2026-08-25 the run reported 5,783 missing minutes out of 18,510 —
    /// 31.2% — as ONE number, which could be read as a catastrophe or as a
    /// non-event with no way to choose. These two counters separate the case
    /// where the exchange recorded trades and we hold nothing (a real gap)
    /// from the case where nothing traded at all (not a loss).
    ///
    /// The invariant asserted here is the one that keeps the split honest:
    /// the two halves must SUM to the whole. A future edit that adds a third
    /// classification without updating the total would otherwise report three
    /// numbers that quietly disagree — the exact failure this module's own
    /// `error_code_liveness` doc warns about.
    #[test]
    fn compare_day_splits_missing_live_by_whether_the_minute_actually_traded() {
        // Two REST minutes we have no candle for: one with volume, one without.
        let traded = PaiseBar::from_rupees(100.0, 101.0, 99.0, 100.5, 250).expect("finite");
        let untraded = PaiseBar::from_rupees(100.0, 100.0, 100.0, 100.0, 0).expect("finite");
        let rest = vec![side(13, OPEN, traded), side(13, OPEN + 60, untraded)];

        let cmp = compare_day(&[], &rest, DAY_START_NANOS, RUN_TS, 0, false);

        assert_eq!(cmp.missing_live, 2, "both minutes are missing from live");
        assert_eq!(
            cmp.missing_live_traded, 1,
            "the minute Dhan recorded trades in is a REAL gap in our capture"
        );
        assert_eq!(
            cmp.missing_live_zero_volume, 1,
            "a minute nothing traded in is not loss — our fold correctly emits \
             nothing for a bucket no tick opened"
        );
        assert_eq!(
            cmp.missing_live_traded + cmp.missing_live_zero_volume,
            cmp.missing_live,
            "the split must SUM to the total, or the report shows three numbers \
             that disagree with each other"
        );
    }

    /// A tail minute is excused BEFORE the split, so it must reach neither half.
    ///
    /// Without this the amnesty would leak: an unsealed 15:29 bar would be
    /// counted as a real gap by `missing_live_traded` while the total that
    /// excused it stayed at zero — a split that contradicts its own parent.
    #[test]
    fn compare_day_tail_amnesty_reaches_neither_half_of_the_split() {
        let traded = PaiseBar::from_rupees(100.0, 101.0, 99.0, 100.5, 250).expect("finite");
        // 15:29 — inside the tail amnesty window.
        let rest = vec![side(13, SESSION_CLOSE_SECS_OF_DAY_IST - 60, traded)];

        let cmp = compare_day(&[], &rest, DAY_START_NANOS, RUN_TS, 0, false);

        assert_eq!(cmp.tail_unsealed, 1);
        assert_eq!(cmp.missing_live, 0);
        assert_eq!(cmp.missing_live_traded, 0);
        assert_eq!(cmp.missing_live_zero_volume, 0);
    }

    #[test]
    fn compare_day_out_of_session_rows_recorded_never_classified() {
        let b = bar(100.0, 101.0, 99.0, 100.5);
        let live = vec![
            side(13, OPEN - 60, b),                     // 09:14 — pre-open
            side(13, SESSION_CLOSE_SECS_OF_DAY_IST, b), // 15:30 — closed
            side(13, OPEN, b),                          // in session
        ];
        let rest = vec![side(13, OPEN, b)];
        let cmp = compare_day(&live, &rest, DAY_START_NANOS, RUN_TS, 0, false);
        assert_eq!(cmp.out_of_session, 2);
        assert_eq!(cmp.minutes_compared, 1);
        assert_eq!(
            cmp.missing_rest, 0,
            "out-of-session rows never become findings"
        );
        assert_eq!(cmp.outcome, DhanLiveXverifyOutcome::Clean);
    }

    #[test]
    fn secs_of_day_and_is_in_session_boundaries_are_half_open() {
        assert!(is_in_session(minute(OPEN), DAY_START_NANOS), "09:15 is in");
        assert!(
            !is_in_session(minute(OPEN - 60), DAY_START_NANOS),
            "09:14 is out"
        );
        assert!(
            is_in_session(minute(SESSION_CLOSE_SECS_OF_DAY_IST - 60), DAY_START_NANOS),
            "15:39 is the last in-session bucket"
        );
        assert!(
            !is_in_session(minute(SESSION_CLOSE_SECS_OF_DAY_IST), DAY_START_NANOS),
            "15:40 is out (half-open window)"
        );
        // 385 comparable minutes in a full session: 09:15-15:40.
        //
        // RE-BLESSED 2026-08-25 from 375. That figure was correct for the
        // pre-CAS 09:15-15:30 session and became wrong on 2026-08-07, when the
        // NSE CAS migration moved the close to 15:40 everywhere EXCEPT this
        // file's private duplicate. The ten extra minutes are real session
        // minutes that were previously dropped from both sides before the join
        // and therefore never verified at all.
        let total = (SESSION_CLOSE_SECS_OF_DAY_IST - SESSION_OPEN_SECS_OF_DAY_IST) / 60;
        assert_eq!(total, 385, "09:15-15:40 is 385 one-minute buckets");
    }

    /// I-P1-11: Dhan reuses numeric security_ids across segments, so the join
    /// key must carry the segment. Same id + different segment = different
    /// instruments, never a comparison.
    #[test]
    fn compare_day_join_key_pairs_security_id_with_segment() {
        let b = bar(100.0, 101.0, 99.0, 100.5);
        let live = vec![SideBar {
            security_id: 27,
            segment: "IDX_I".to_string(),
            minute_ts_ist_nanos: minute(OPEN),
            bar: b,
        }];
        let rest = vec![SideBar {
            security_id: 27,
            segment: "NSE_EQ".to_string(),
            minute_ts_ist_nanos: minute(OPEN),
            bar: b,
        }];
        let cmp = compare_day(&live, &rest, DAY_START_NANOS, RUN_TS, 0, false);
        assert_eq!(
            cmp.minutes_compared, 0,
            "same id in different segments must NOT be compared (I-P1-11)"
        );
        assert_eq!(cmp.instruments, 2);
        assert_eq!(cmp.outcome, DhanLiveXverifyOutcome::Blind);
    }

    #[test]
    fn compare_day_degraded_downgrades_clean_to_partial_never_up() {
        let b = bar(100.0, 101.0, 99.0, 100.5);
        let live = vec![side(13, OPEN, b)];
        let rest = vec![side(13, OPEN, b)];
        let cmp = compare_day(&live, &rest, DAY_START_NANOS, RUN_TS, 0, true);
        assert_eq!(cmp.outcome, DhanLiveXverifyOutcome::Partial);
        assert!(!cmp.outcome.is_pass(), "a degraded run is never a pass");
        assert!(cmp.outcome.is_measured(), "but it did compare something");
    }

    // =======================================================================
    // Noise quantification — SHOW the operator what normal looks like
    // =======================================================================

    #[test]
    fn compare_day_noise_percentiles_quantify_typical_fluctuation() {
        let mut live = Vec::new();
        let mut rest = Vec::new();
        // 100 minutes, `high` drifting by 1..=100 paise.
        for i in 0..100i64 {
            let drift = (i + 1) as f64 / 100.0;
            live.push(side(
                13,
                OPEN + i * 60,
                bar(100.0, 100.0 + drift, 99.0, 100.0),
            ));
            rest.push(side(13, OPEN + i * 60, bar(100.0, 100.0, 99.0, 100.0)));
        }
        // Tolerance wide enough that nothing is "divergence" — we still
        // measure the noise, which is the entire point.
        let cmp = compare_day(&live, &rest, DAY_START_NANOS, RUN_TS, 10_000, false);
        assert_eq!(cmp.minutes_compared, 100);
        assert_eq!(cmp.cells_diverged, 0);
        // Nearest-rank over the sorted deltas [1..=100]:
        //   p50 -> idx round(99 * 0.50) = 50 -> 51
        //   p95 -> idx round(99 * 0.95) = 94 -> 95
        assert_eq!(cmp.noise_max_paise, 100);
        assert_eq!(cmp.noise_p50_paise, 51);
        assert_eq!(cmp.noise_p95_paise, 95);
        assert!(
            cmp.noise_p50_paise < cmp.noise_p95_paise && cmp.noise_p95_paise <= cmp.noise_max_paise
        );
    }

    #[test]
    fn compare_day_noise_stats_are_zero_when_nothing_differs() {
        let b = bar(100.0, 101.0, 99.0, 100.5);
        let cmp = compare_day(
            &[side(13, OPEN, b)],
            &[side(13, OPEN, b)],
            DAY_START_NANOS,
            RUN_TS,
            0,
            false,
        );
        assert_eq!(
            (
                cmp.noise_p50_paise,
                cmp.noise_p95_paise,
                cmp.noise_max_paise
            ),
            (0, 0, 0)
        );
    }

    #[test]
    fn percentile_is_total_on_empty_and_single_element_slices() {
        assert_eq!(percentile(&[], 0.5), 0);
        assert_eq!(percentile(&[7], 0.0), 7);
        assert_eq!(percentile(&[7], 1.0), 7);
        assert_eq!(percentile(&[1, 2, 3], 1.0), 3);
    }

    // =======================================================================
    // Paise conversion — non-finite must never silently become ₹0.00
    // =======================================================================

    #[test]
    fn to_paise_rejects_non_finite_prices() {
        assert_eq!(to_paise(100.0), Some(10_000));
        assert_eq!(to_paise(100.005), Some(10_001)); // half away from zero
        assert_eq!(to_paise(-1.5), Some(-150));
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                to_paise(bad),
                None,
                "a non-finite price must be rejected, not saturated to 0 — \
                 `(NaN * 100.0).round() as i64` is 0 in Rust and would compare \
                 a corrupt row as ₹0.00"
            );
        }
    }

    #[test]
    fn paise_bar_from_rupees_rejects_any_non_finite_field() {
        assert!(PaiseBar::from_rupees(1.0, 2.0, 0.5, 1.5, 10).is_some());
        assert!(PaiseBar::from_rupees(f64::NAN, 2.0, 0.5, 1.5, 10).is_none());
        assert!(PaiseBar::from_rupees(1.0, f64::INFINITY, 0.5, 1.5, 10).is_none());
        assert!(PaiseBar::from_rupees(1.0, 2.0, f64::NAN, 1.5, 10).is_none());
        assert!(PaiseBar::from_rupees(1.0, 2.0, 0.5, f64::NAN, 10).is_none());
    }

    // =======================================================================
    // Parsing
    // =======================================================================

    #[test]
    fn parse_live_dataset_reads_rows_and_skips_malformed_ones() {
        let body = format!(
            r#"{{"dataset":[
                [{ts},13,"IDX_I",100.0,101.0,99.0,100.5,7],
                [{ts},25,"IDX_I",200.0,201.0,199.0,200.5,9],
                ["bad",26,"IDX_I",1.0,1.0,1.0,1.0,0],
                [{ts},27,"IDX_I",null,1.0,1.0,1.0,0]
            ]}}"#,
            ts = minute(OPEN)
        );
        let (rows, truncated, malformed) = parse_live_dataset(&body, 100).expect("parse");
        assert_eq!(rows.len(), 2);
        assert_eq!(malformed, 2, "bad rows must be counted, never coerced");
        assert!(!truncated);
        assert_eq!(rows[0].security_id, 13);
        assert_eq!(rows[0].segment, "IDX_I");
        assert_eq!(rows[0].minute_ts_ist_nanos, minute(OPEN));
        assert_eq!(rows[0].bar.close_paise, 10_050);
        assert_eq!(rows[0].bar.volume, 7);
    }

    /// The LIMIT+1 probe: exactly-cap is COMPLETE, cap+1 is truncated, and the
    /// probe row never enters the comparison.
    #[test]
    fn truncation_probe_treats_exactly_cap_as_complete() {
        let row = format!(r#"[{},13,"IDX_I",1.0,1.0,1.0,1.0,0]"#, minute(OPEN));
        let two = format!(r#"{{"dataset":[{row},{row}]}}"#);

        let (rows, truncated, _) = parse_live_dataset(&two, 2).expect("parse");
        assert_eq!(rows.len(), 2);
        assert!(!truncated, "exactly-cap must not be a false PARTIAL");

        let (rows, truncated, _) = parse_live_dataset(&two, 1).expect("parse");
        assert_eq!(rows.len(), 1, "the probe row must not enter the compare");
        assert!(truncated);
    }

    #[test]
    fn parse_live_dataset_errors_loudly_on_a_malformed_body() {
        assert!(parse_live_dataset("not json", 10).is_err());
        assert!(parse_live_dataset(r#"{"no_dataset":1}"#, 10).is_err());
        let (rows, truncated, malformed) =
            parse_live_dataset(r#"{"dataset":[]}"#, 10).expect("empty is valid");
        assert!(rows.is_empty() && !truncated && malformed == 0);
    }

    #[test]
    fn rest_candles_to_side_bars_converts_and_counts_malformed() {
        use crate::dhan_intraday_parse::MinuteCandle;
        let candles = vec![
            MinuteCandle {
                minute_ts_ist_nanos: minute(OPEN),
                open: 100.0,
                high: 101.0,
                low: 99.0,
                close: 100.5,
                volume: 5,
            },
            MinuteCandle {
                minute_ts_ist_nanos: minute(OPEN + 60),
                open: f64::NAN,
                high: 101.0,
                low: 99.0,
                close: 100.5,
                volume: 5,
            },
        ];
        let (bars, malformed) = rest_candles_to_side_bars(&candles, 13, "IDX_I");
        assert_eq!(bars.len(), 1);
        assert_eq!(malformed, 1);
        assert_eq!(bars[0].security_id, 13);
        assert_eq!(bars[0].segment, "IDX_I");
    }

    /// End-to-end unit joinability: a REST candle parsed by the shared Dhan
    /// parser must land on the SAME nanosecond key as a live `candles_1m` row
    /// read through our SQL projection. If either side's units drift, this
    /// fails — which is the compound version of the #1474 bug.
    #[test]
    fn rest_and_live_keys_are_the_same_unit_and_actually_join() {
        use crate::dhan_intraday_parse::intraday_utc_secs_to_ist_minute_nanos;

        // A Dhan REST timestamp is UTC epoch SECONDS.
        let utc_secs = 1_786_233_600 - 19_800 + OPEN; // IST 09:15 that day
        let rest_key = intraday_utc_secs_to_ist_minute_nanos(utc_secs);

        // The live side's projected key, in nanoseconds.
        let live_key = minute(OPEN);

        assert_eq!(
            rest_key, live_key,
            "both sides must key on IST-minute NANOSECONDS or nothing ever joins"
        );
        assert_eq!(rest_key % (60 * NANOS_PER_SEC), 0, "must be minute-aligned");
        assert_eq!(rest_key.to_string().len(), 19, "nanoseconds = 19 digits");

        // And prove they actually compare.
        let b = bar(100.0, 101.0, 99.0, 100.5);
        let cmp = compare_day(
            &[SideBar {
                security_id: 13,
                segment: "IDX_I".into(),
                minute_ts_ist_nanos: live_key,
                bar: b,
            }],
            &[SideBar {
                security_id: 13,
                segment: "IDX_I".into(),
                minute_ts_ist_nanos: rest_key,
                bar: b,
            }],
            DAY_START_NANOS,
            RUN_TS,
            0,
            false,
        );
        assert_eq!(cmp.minutes_compared, 1);
        assert_eq!(cmp.outcome, DhanLiveXverifyOutcome::Clean);
    }

    // =======================================================================
    // Config + misc
    // =======================================================================

    #[test]
    fn the_live_row_cap_covers_a_whole_day_for_the_stated_instrument_count() {
        // The cap used to be the literal 200_000, which at today's ~868
        // instruments covers 230 of the session's 385 minutes — and because
        // the query is `ORDER BY ts ASC LIMIT n`, truncation drops the TAIL,
        // so the comparator verified every morning and no afternoon. Nothing
        // connected the number to the universe it had to cover, so nobody
        // could see it had stopped being enough.
        assert_eq!(
            SESSION_MINUTES, 385,
            "09:15 to 15:40 is 385 minutes — the close tracks the canonical \
             persistence end, not a literal (merge resolution 2026-08-25)"
        );
        assert_eq!(
            LIVE_ROW_LIMIT,
            LIVE_COVERED_INSTRUMENTS * SESSION_MINUTES,
            "the cap must stay DERIVED — a literal cannot track the universe"
        );

        // The promise, stated as arithmetic rather than trusted: a full day
        // for every instrument up to the covered count fits.
        const TODAYS_LIVE_UNIVERSE: usize = 868;
        assert!(
            TODAYS_LIVE_UNIVERSE * SESSION_MINUTES <= LIVE_ROW_LIMIT,
            "today's universe must fit without truncation"
        );
        assert!(
            LIVE_COVERED_INSTRUMENTS >= 2 * TODAYS_LIVE_UNIVERSE,
            "keep real headroom, not a cap that today only just clears"
        );

        // And the limit of that promise, stated too: the authorized ceiling
        // does NOT fit, and the fix for that is pagination, not a bigger
        // number. A run past the covered count reports `partial`.
        const AUTHORIZED_CEILING: usize = 25_000;
        assert!(
            AUTHORIZED_CEILING * SESSION_MINUTES > LIVE_ROW_LIMIT,
            "if this ever passes, the pagination follow-up was done or the \
             ceiling moved — update the doc comment either way"
        );
    }

    #[test]
    fn the_live_read_timeout_is_its_own_knob_and_fits_inside_the_budget() {
        // Sharing `fetch_timeout_secs` coupled a single-instrument REST call
        // to a whole-universe scan. Any raise of the row cap then turned
        // honest truncation into a hard error that aborts the entire run —
        // strictly worse than the partial verdict it replaced.
        let c = DhanLiveCrossverifyConfig::default();
        assert!(
            c.live_read_timeout_secs > c.fetch_timeout_secs,
            "a whole-universe scan needs longer than one instrument's fetch"
        );
        assert!(
            c.live_read_timeout_secs < c.run_budget_secs,
            "one query must not be able to consume the whole run"
        );
        // The budget must clear the vendor's 5 req/sec pace for today's
        // universe, or the REST leg is cut off before it finishes.
        const TODAYS_TARGETS: u64 = 868;
        const DHAN_DATA_API_PER_SEC: u64 = 5;
        assert!(
            c.run_budget_secs > TODAYS_TARGETS / DHAN_DATA_API_PER_SEC,
            "the budget must exceed the minimum time the rate limit imposes"
        );

        let mut bad = c.clone();
        bad.live_read_timeout_secs = bad.run_budget_secs;
        assert!(
            bad.validate().is_err(),
            "a live timeout at or above the budget must be refused at boot"
        );
        let mut zero = c;
        zero.live_read_timeout_secs = 0;
        assert!(zero.validate().is_err(), "a zero timeout fails every read");
    }

    #[test]
    fn config_default_is_paise_exact() {
        let c = DhanLiveCrossverifyConfig::default();
        assert_eq!(c.tolerance_paise, 0, "default is paise-exact");
        assert!(c.validate().is_ok());
    }

    #[test]
    fn absent_config_section_deserializes_to_the_working_defaults() {
        // An entirely absent section (no keys at all) must still deserialize
        // to usable knobs rather than zeros — a zero timeout would make every
        // fetch fail instantly and the run permanently `degraded`.
        //
        // This used to additionally assert `!c.enabled`. That field is gone
        // (2026-08-25): it had no production reader, so it asserted a
        // "fail-safe posture" the code never implemented. The comparator's
        // real gate is `crossverify_deps_installed()`.
        let c: DhanLiveCrossverifyConfig =
            serde_json::from_str("{}").expect("absent section deserializes");
        assert_eq!(c, DhanLiveCrossverifyConfig::default());
        assert!(c.fetch_timeout_secs > 0, "a zero timeout fails every fetch");
        assert!(c.run_budget_secs > 0, "a zero budget completes no target");
    }

    #[test]
    fn the_config_carries_no_dead_enabled_switch() {
        // Pins the 2026-08-25 deletion. A `bool` named `enabled` on this
        // struct with no reader is worse than no switch at all: it tells the
        // next reader the comparator can be turned off here, and it cannot.
        // Re-adding one must come with a real gate and an operator decision,
        // because honouring its stated default would silently stop the only
        // ground truth this feed has.
        let src = include_str!("dhan_live_crossverify.rs");
        // Assembled at runtime so this line does not match itself — the first
        // version of this test failed against its own source, which is the
        // classic way a source-scanning guard reports a defect it invented.
        let needle = format!("pub {}", "enabled");
        let decl = src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .any(|l| l.contains(&needle));
        assert!(
            !decl,
            "DhanLiveCrossverifyConfig must not declare an `enabled` field \
             unless something actually reads it"
        );
    }

    #[test]
    fn config_validate_rejects_nonsense_and_false_ok_tolerances() {
        let bad = DhanLiveCrossverifyConfig {
            tolerance_paise: -1,
            ..Default::default()
        };
        assert!(bad.validate().is_err(), "negative tolerance is nonsense");

        let bad = DhanLiveCrossverifyConfig {
            tolerance_paise: 50_001,
            ..Default::default()
        };
        assert!(
            bad.validate().is_err(),
            "an absurd tolerance would silently swallow real divergence"
        );

        let bad = DhanLiveCrossverifyConfig {
            fetch_timeout_secs: 0,
            ..Default::default()
        };
        assert!(bad.validate().is_err());

        let bad = DhanLiveCrossverifyConfig {
            fetch_timeout_secs: 500,
            run_budget_secs: 100,
            ..Default::default()
        };
        assert!(
            bad.validate().is_err(),
            "one fetch must not eat the whole run"
        );
    }

    #[test]
    fn deterministic_run_ts_nanos_is_one_minute_past_the_close_regardless_of_fire_time() {
        let stamp = deterministic_run_ts_nanos(DAY_START_NANOS);
        assert_eq!(
            secs_of_day(stamp, DAY_START_NANOS),
            SESSION_CLOSE_SECS_OF_DAY_IST + 60,
            "the audit stamp must be one minute past the session close, whatever \
             the actual fire time -- DERIVED, not a restated literal, so a \
             future session-hours change cannot leave it behind the way the \
             hardcoded 15:31 did through the 2026-08-07 CAS migration"
        );
        // Idempotent: a catch-up rerun produces the identical stamp.
        assert_eq!(stamp, deterministic_run_ts_nanos(DAY_START_NANOS));
        // ...and it lands AFTER the session close, so it never collides with
        // a compared minute.
        assert!(secs_of_day(stamp, DAY_START_NANOS) > SESSION_CLOSE_SECS_OF_DAY_IST);
    }

    #[test]
    fn daily_row_carries_every_count_and_the_applied_tolerance() {
        let b = bar(100.0, 101.05, 99.0, 100.5);
        let b2 = bar(100.0, 101.0, 99.0, 100.5);
        let cmp = compare_day(
            &[side(13, OPEN, b)],
            &[side(13, OPEN, b2)],
            DAY_START_NANOS,
            RUN_TS,
            0,
            false,
        );
        let row = daily_row(&cmp, DAY_START_NANOS, RUN_TS, 0);
        assert_eq!(row.trading_date_ist_nanos, DAY_START_NANOS);
        assert_eq!(row.run_ts_ist_nanos, RUN_TS);
        assert_eq!(row.minutes_compared, cmp.minutes_compared);
        assert_eq!(row.cells_diverged, cmp.cells_diverged);
        assert_eq!(row.noise_max_paise, cmp.noise_max_paise);
        assert_eq!(
            row.tolerance_paise, 0,
            "the applied tolerance is recorded so a knob change is visible in the trend"
        );
        assert_eq!(row.outcome, DhanLiveXverifyOutcome::Diverged);
    }

    /// Operator-facing text: plain English, no library names, no file paths
    /// (the 10 Telegram commandments), and never claims our side is truth.
    #[test]
    fn format_summary_line_is_plain_english_and_never_claims_live_is_truth() {
        let b = bar(100.0, 101.05, 99.0, 100.5);
        let b2 = bar(100.0, 101.0, 99.0, 100.5);
        let cmp = compare_day(
            &[side(13, OPEN, b)],
            &[side(13, OPEN, b2)],
            DAY_START_NANOS,
            RUN_TS,
            0,
            false,
        );
        let line = format_summary_line(&cmp);
        assert!(line.contains("Neither side is the official truth"));
        assert!(line.contains('₹'), "money in rupees, not paise");
        for jargon in [
            "candles_1m",
            "QuestDB",
            "paise",
            "crates/",
            ".rs",
            "DEDUP",
            "ILP",
            "SQL",
        ] {
            assert!(
                !line.contains(jargon),
                "operator text must not contain `{jargon}`: {line}"
            );
        }
        // The clean line names the count so it can never be vacuously reassuring.
        let clean = compare_day(
            &[side(13, OPEN, b2)],
            &[side(13, OPEN, b2)],
            DAY_START_NANOS,
            RUN_TS,
            0,
            false,
        );
        assert!(format_summary_line(&clean).contains('1'));
    }

    /// `PaiseBar::from_rupees` is the ONLY door into a comparison, so it is
    /// the single choke point where a corrupt price must be stopped.
    #[test]
    fn test_from_rupees_is_the_choke_point_for_corrupt_prices() {
        let good = PaiseBar::from_rupees(100.0, 101.0, 99.0, 100.5, 42).expect("finite");
        assert_eq!(good.open_paise, 10_000);
        assert_eq!(good.close_paise, 10_050);
        assert_eq!(good.volume, 42, "volume is carried through exactly");
        // Raw rupees are retained verbatim for the audit row.
        assert_eq!(good.high_r, 101.0);

        // Every field is guarded, not just `close`.
        for (o, h, l, c) in [
            (f64::NAN, 1.0, 1.0, 1.0),
            (1.0, f64::NAN, 1.0, 1.0),
            (1.0, 1.0, f64::NAN, 1.0),
            (1.0, 1.0, 1.0, f64::NAN),
            (1.0, f64::INFINITY, 1.0, 1.0),
        ] {
            assert!(
                PaiseBar::from_rupees(o, h, l, c, 0).is_none(),
                "a non-finite field must reject the whole bar"
            );
        }
    }

    /// The session gate decides what is even eligible for comparison, so its
    /// boundaries are load-bearing: one minute wrong at either end silently
    /// changes the denominator of every verdict.
    #[test]
    fn test_is_in_session_gate_is_half_open_and_excludes_pre_open_and_close() {
        assert!(
            !is_in_session(minute(0), DAY_START_NANOS),
            "midnight is out"
        );
        assert!(!is_in_session(minute(OPEN - 1), DAY_START_NANOS));
        assert!(is_in_session(minute(OPEN), DAY_START_NANOS), "09:15 is in");
        assert!(is_in_session(minute(OPEN + 60), DAY_START_NANOS));
        assert!(is_in_session(
            minute(SESSION_CLOSE_SECS_OF_DAY_IST - 1),
            DAY_START_NANOS
        ));
        assert!(
            !is_in_session(minute(SESSION_CLOSE_SECS_OF_DAY_IST), DAY_START_NANOS),
            "15:30 is excluded — the window is half-open"
        );
        assert!(!is_in_session(
            minute(SESSION_CLOSE_SECS_OF_DAY_IST + 3600),
            DAY_START_NANOS
        ));
        // A bucket from the PREVIOUS day must never be counted as in-session.
        assert!(!is_in_session(
            minute(OPEN) - 86_400 * NANOS_PER_SEC,
            DAY_START_NANOS
        ));
    }

    /// `DayComparison::is_vacuous` is what the caller reads to decide whether
    /// the run proved anything at all. It must be true for EVERY
    /// zero-comparison run — the #1474 state — and false whenever a real
    /// comparison happened.
    #[test]
    fn test_is_vacuous_is_true_for_every_zero_comparison_run() {
        let b = bar(100.0, 101.0, 99.0, 100.5);

        // Nothing at all.
        assert!(compare_day(&[], &[], DAY_START_NANOS, RUN_TS, 0, false).is_vacuous());
        // Rows on one side only.
        assert!(
            compare_day(&[side(13, OPEN, b)], &[], DAY_START_NANOS, RUN_TS, 0, false).is_vacuous()
        );
        // Rows on both sides that do not overlap (the year-58502 signature).
        assert!(
            compare_day(
                &[side(13, OPEN, b)],
                &[side(13, OPEN + 60, b)],
                DAY_START_NANOS,
                RUN_TS,
                0,
                false
            )
            .is_vacuous()
        );
        // A real comparison is NOT vacuous — clean or diverged alike.
        let clean = compare_day(
            &[side(13, OPEN, b)],
            &[side(13, OPEN, b)],
            DAY_START_NANOS,
            RUN_TS,
            0,
            false,
        );
        assert!(!clean.is_vacuous());
        assert!(clean.outcome.is_pass());
        // is_vacuous and outcome.is_vacuous must never disagree.
        for cmp in [
            compare_day(&[], &[], DAY_START_NANOS, RUN_TS, 0, false),
            clean,
        ] {
            assert_eq!(
                cmp.is_vacuous(),
                cmp.outcome.is_vacuous(),
                "the comparison-level and outcome-level vacuity views must agree"
            );
        }
    }

    // =======================================================================
    // The bounded runner (pure-shape assertions; no live I/O in CI)
    // =======================================================================

    #[test]
    fn test_run_cross_verification_contacts_only_the_granted_intraday_endpoint() {
        // The §8 KEEP class is the ONLY Dhan endpoint this module may touch.
        let src = include_str!("dhan_live_crossverify.rs");
        // Split the token so this assertion cannot match itself.
        let charts = concat!("/charts/", "intraday");
        assert!(
            src.contains(charts),
            "the granted endpoint must be the one documented"
        );
        // Each needle is assembled from split fragments so this very list
        // cannot match itself in the source it scans.
        for forbidden in [
            concat!("marketfeed", "/quote"),
            concat!("marketfeed", "/ltp"),
            concat!("option", "chain"),
            concat!("charts/", "historical"),
            concat!("/v2/", "profile"),
        ] {
            assert!(
                !src.contains(forbidden),
                "`{forbidden}` is outside the §8 grant and must never appear here"
            );
        }
    }

    #[test]
    fn test_fetch_rest_side_for_target_builds_the_shared_day_granular_body() {
        use chrono::NaiveDate;
        let date = NaiveDate::from_ymd_opt(2026, 8, 10).expect("date");
        let next = date.succ_opt().expect("next");
        let t = XverifyTarget {
            security_id: 13,
            segment: "IDX_I".to_string(),
            instrument: "INDEX".to_string(),
        };
        // Byte-equality with the SHARED builder the spot-1m leg uses — so the
        // window convention can never silently diverge between the two legs.
        let body = crate::dhan_intraday_parse::intraday_request_body(
            &t.security_id.to_string(),
            &t.segment,
            &t.instrument,
            date,
            next,
        );
        assert_eq!(body["securityId"], "13", "securityId is a STRING for Dhan");
        assert_eq!(body["exchangeSegment"], "IDX_I");
        assert_eq!(body["instrument"], "INDEX");
        assert_eq!(body["interval"], "1", "1-minute interval");
        assert_eq!(body["fromDate"], "2026-08-10 00:00:00");
        assert_eq!(
            body["toDate"], "2026-08-11 00:00:00",
            "day-granular window: toDate is the NEXT day (the #1499 lesson)"
        );
    }

    #[test]
    fn test_read_live_side_uses_the_microsecond_window_query() {
        // The runner must go through `live_candles_select_sql`, not a
        // hand-rolled query — that helper is where the #1474 fix lives.
        let sql = live_candles_select_sql(DAY_START_NANOS);
        let (start, _) = day_bounds_micros(DAY_START_NANOS);
        assert!(sql.contains(&start.to_string()));
        assert!(!sql.contains(&DAY_START_NANOS.to_string()));
    }

    /// A degraded leg must DEGRADE the verdict, never fabricate a clean one —
    /// mirrors what `run_cross_verification` passes into `compare_day`.
    #[test]
    fn test_run_report_degraded_flag_forces_the_verdict_down() {
        let b = bar(100.0, 101.0, 99.0, 100.5);
        let live = vec![side(13, OPEN, b)];
        let rest = vec![side(13, OPEN, b)];

        // Healthy run over identical data -> clean.
        let healthy = compare_day(&live, &rest, DAY_START_NANOS, RUN_TS, 0, false);
        assert_eq!(healthy.outcome, DhanLiveXverifyOutcome::Clean);

        // Same data, but a leg failed / the budget elapsed -> partial.
        let degraded = compare_day(&live, &rest, DAY_START_NANOS, RUN_TS, 0, true);
        assert_eq!(degraded.outcome, DhanLiveXverifyOutcome::Partial);
        assert!(
            !degraded.outcome.is_pass(),
            "identical data plus a failed leg must NOT report as a pass"
        );

        let report = RunReport {
            comparison: degraded,
            rest_failures: 3,
            rest_failure_breakdown: RestFailureBreakdown::default(),
            degraded: true,
            malformed_rows: 0,
            budget_elapsed: true,
            rest_tape: Vec::new(),
        };
        assert!(report.degraded);
        assert!(report.budget_elapsed);
        assert!(!report.comparison.outcome.is_pass());
    }

    // -----------------------------------------------------------------
    // REST failure classification — added 2026-08-26
    //
    // The comparator ran at 94% blind on two consecutive sessions
    // (814/864 then 815/865 fetches failed) and could not say why,
    // because its only caller matched `Err(_)` and dropped the reason.
    // These tests pin the reason surviving.
    // -----------------------------------------------------------------

    #[test]
    fn every_failure_kind_has_a_distinct_stable_label() {
        let all = XverifyFetchFailureKind::all();
        let labels: std::collections::BTreeSet<&str> = all.iter().map(|k| k.as_str()).collect();
        assert_eq!(
            labels.len(),
            all.len(),
            "two kinds share a label — their counts would silently merge, \
             which is the collapse this classification exists to prevent"
        );
        for l in &labels {
            assert!(
                !l.is_empty()
                    && l.chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "label {l:?} must stay a bounded lowercase token — it is a log field \
                 and a candidate metric label"
            );
        }
    }

    #[test]
    fn a_real_429_classifies_as_rate_limited_not_as_a_generic_4xx() {
        // The distinction is the whole point: `rate_limited` is actionable
        // (pace the loop), a generic 4xx is not (fix the request).
        assert_eq!(
            classify_http_status(reqwest::StatusCode::TOO_MANY_REQUESTS),
            XverifyFetchFailureKind::RateLimited
        );
        assert_eq!(
            classify_http_status(reqwest::StatusCode::BAD_REQUEST),
            XverifyFetchFailureKind::Http4xx
        );
        assert_eq!(
            classify_http_status(reqwest::StatusCode::UNAUTHORIZED),
            XverifyFetchFailureKind::Http4xx
        );
        assert_eq!(
            classify_http_status(reqwest::StatusCode::INTERNAL_SERVER_ERROR),
            XverifyFetchFailureKind::Http5xx
        );
        assert_eq!(
            classify_http_status(reqwest::StatusCode::BAD_GATEWAY),
            XverifyFetchFailureKind::Http5xx
        );
        // 429 is a client error, so a naive `is_client_error()` first would
        // swallow it. Order matters, and this asserts the order.
        assert!(reqwest::StatusCode::TOO_MANY_REQUESTS.is_client_error());
    }

    #[test]
    fn breakdown_total_always_equals_the_failure_count() {
        let mut b = RestFailureBreakdown::default();
        assert_eq!(b.total(), 0);
        assert_eq!(b.summary(), "none");
        for _ in 0..815 {
            b.record(XverifyFetchFailureKind::NoCandles);
        }
        for _ in 0..4 {
            b.record(XverifyFetchFailureKind::RateLimited);
        }
        b.record(XverifyFetchFailureKind::Timeout);
        assert_eq!(b.get(XverifyFetchFailureKind::NoCandles), 815);
        assert_eq!(b.get(XverifyFetchFailureKind::RateLimited), 4);
        assert_eq!(b.get(XverifyFetchFailureKind::Timeout), 1);
        assert_eq!(b.get(XverifyFetchFailureKind::Http5xx), 0);
        assert_eq!(
            b.total(),
            820,
            "the breakdown must account for EVERY failure — a total that \
             undercounts would let a whole reason class hide"
        );
    }

    #[test]
    fn the_summary_names_the_dominant_reason_first_and_omits_the_zeroes() {
        let mut b = RestFailureBreakdown::default();
        b.record(XverifyFetchFailureKind::Timeout);
        for _ in 0..9 {
            b.record(XverifyFetchFailureKind::NoCandles);
        }
        let s = b.summary();
        assert!(
            s.starts_with("no_candles=9"),
            "the dominant reason must lead — an operator reading one line \
             should see the cause first, got {s:?}"
        );
        assert!(s.contains("timeout=1"), "got {s:?}");
        assert!(
            !s.contains("http_5xx"),
            "a zero-count reason must not appear — a quiet run stays short, got {s:?}"
        );
    }

    /// The regression pin that matters: the fetch loop must TALLY the reason,
    /// never discard it.
    ///
    /// This asserts a CONSEQUENCE (the tally call exists, and the exact
    /// discard pattern does not), not a spelling or a proximity window.
    #[test]
    fn the_fetch_loop_never_discards_the_failure_reason_again() {
        let src = include_str!("dhan_live_crossverify.rs");
        // Scan the PRODUCTION half only. `include_str!` reads this file
        // INCLUDING these tests, and the assertion strings below contain the
        // very pattern being banned — without this split the test reads its
        // own message and fails on itself. It did, on the first run.
        //
        // The marker is assembled from fragments rather than written whole,
        // for the same reason one level deeper: spelling it out here would
        // put a second copy in the file and split the source in the wrong
        // place. That also failed, on the second run. Both are recorded
        // because a self-scanning test is a genuinely easy thing to get
        // subtly wrong, and it fails OPEN — it would have passed while
        // reading nothing.
        let marker = concat!("#[cfg(", "test)]");
        assert_eq!(
            src.matches(marker).count(),
            1,
            "the production/test split below assumes a single marker"
        );
        let production = src.split(marker).next().expect("production half");
        let body = production
            .split("pub async fn run_cross_verification")
            .nth(1)
            .expect("run_cross_verification must exist");
        // Bound the scan to the function, not the rest of the file.
        let end = body.find("\n}\n").unwrap_or(body.len());
        let body = &body[..end];
        assert!(
            body.contains("rest_failure_breakdown.record("),
            "run_once must tally the failure kind"
        );
        assert!(
            !body.contains("Err(_) =>"),
            "run_once must not throw a fetch reason away — that discard is \
             what left 815 daily failures unexplainable for two sessions"
        );
        assert!(
            body.contains("fetch_rest_side_paced("),
            "the loop must go through the PACED wrapper: ~865 unpaced requests \
             against a 5-per-second vendor budget is the shape both sibling \
             REST legs were fixed out of on 2026-07-14"
        );
    }

    // -----------------------------------------------------------------
    // VOLUME — added 2026-08-26
    //
    // Until today this comparison checked open/high/low/close and nothing
    // else. Volume rode along on every finding row and was never once
    // compared, so "does our volume agree with the exchange's?" had no
    // answer anywhere in the system.
    // -----------------------------------------------------------------

    /// Builds on the existing `bar` helper rather than calling the
    /// constructor again, so this file gains no second fallible-unwrap site.
    fn bar_vol(o: f64, h: f64, l: f64, c: f64, v: i64) -> PaiseBar {
        let mut b = bar(o, h, l, c);
        b.volume = v;
        b
    }

    #[test]
    fn volume_is_compared_at_all() {
        // The whole point. Before today this assertion was unwritable:
        // there was no field to read.
        let live = vec![side(1, OPEN, bar_vol(10.0, 10.0, 10.0, 10.0, 700))];
        let rest = vec![side(1, OPEN, bar_vol(10.0, 10.0, 10.0, 10.0, 1_000))];
        let cmp = compare_day(&live, &rest, DAY_START_NANOS, RUN_TS, 0, false);

        assert_eq!(cmp.volume_cells, 1, "the cell must be counted");
        assert_eq!(cmp.volume_exact, 0, "700 is not 1000");
        assert_eq!(
            cmp.volume_capture_p50_pct, 70,
            "we saw 700 of their 1000 — that is 70% capture, and saying so \
             is the entire deliverable"
        );
    }

    #[test]
    fn a_volume_disagreement_never_becomes_a_price_divergence() {
        // The trap this design exists to avoid. Our feed is a conflated
        // ~1/sec sample and theirs is the full tape, so under-capture is
        // STRUCTURAL. Folding it into `cells_diverged` would flag nearly
        // every cell and drown the price signal beside it.
        let live = vec![side(1, OPEN, bar_vol(10.0, 10.0, 10.0, 10.0, 1))];
        let rest = vec![side(1, OPEN, bar_vol(10.0, 10.0, 10.0, 10.0, 9_999))];
        let cmp = compare_day(&live, &rest, DAY_START_NANOS, RUN_TS, 0, false);

        assert_eq!(
            cmp.cells_diverged, 0,
            "prices agree exactly — a volume gap must not be reported as a \
             price divergence"
        );
        assert!(
            cmp.outcome.is_pass(),
            "and it must not flip the verdict: a threshold needs a baseline, \
             and this measurement is what creates one"
        );
        assert_eq!(cmp.volume_capture_p50_pct, 0, "1 of 9,999 rounds to 0%");
    }

    #[test]
    fn a_minute_the_vendor_reports_as_untraded_is_skipped_not_scored_zero() {
        // Their volume is the denominator. A zero one says the vendor saw no
        // trades that minute — which says NOTHING about our capture. Scoring
        // it 0% would drag the median toward "we missed everything" when the
        // truth is "there was nothing to miss".
        let live = vec![
            side(1, OPEN, bar_vol(10.0, 10.0, 10.0, 10.0, 0)),
            side(2, OPEN, bar_vol(10.0, 10.0, 10.0, 10.0, 500)),
        ];
        let rest = vec![
            side(1, OPEN, bar_vol(10.0, 10.0, 10.0, 10.0, 0)),
            side(2, OPEN, bar_vol(10.0, 10.0, 10.0, 10.0, 500)),
        ];
        let cmp = compare_day(&live, &rest, DAY_START_NANOS, RUN_TS, 0, false);

        assert_eq!(
            cmp.volume_cells, 1,
            "only the traded minute counts toward the profile"
        );
        assert_eq!(cmp.volume_exact, 1);
        assert_eq!(
            cmp.volume_capture_p50_pct, 100,
            "a clean capture must read 100, not be dragged down by a minute \
             nobody traded in"
        );
    }

    #[test]
    fn capturing_everything_reads_as_one_hundred_percent() {
        let live: Vec<SideBar> = (0..10)
            .map(|i| side(1, OPEN + i * 60, bar_vol(10.0, 10.0, 10.0, 10.0, 250)))
            .collect();
        let rest = live.clone();
        let cmp = compare_day(&live, &rest, DAY_START_NANOS, RUN_TS, 0, false);

        assert_eq!(cmp.volume_cells, 10);
        assert_eq!(cmp.volume_exact, 10);
        assert_eq!(cmp.volume_capture_p50_pct, 100);
        assert_eq!(
            cmp.volume_capture_p05_pct, 100,
            "the bad tail of a perfect run is still perfect"
        );
    }

    #[test]
    fn the_fifth_percentile_surfaces_the_bad_tail_the_median_hides() {
        // Nine good minutes and one terrible one. The median says everything
        // is fine; only the tail says an instrument had a hole in it. That
        // asymmetry is why both are reported.
        let mut live: Vec<SideBar> = (0..19)
            .map(|i| side(1, OPEN + i * 60, bar_vol(10.0, 10.0, 10.0, 10.0, 1_000)))
            .collect();
        live.push(side(1, OPEN + 19 * 60, bar_vol(10.0, 10.0, 10.0, 10.0, 20)));
        let rest: Vec<SideBar> = (0..20)
            .map(|i| side(1, OPEN + i * 60, bar_vol(10.0, 10.0, 10.0, 10.0, 1_000)))
            .collect();
        let cmp = compare_day(&live, &rest, DAY_START_NANOS, RUN_TS, 0, false);

        assert_eq!(cmp.volume_cells, 20);
        assert_eq!(
            cmp.volume_capture_p50_pct, 100,
            "the median is blind to one bad minute in twenty"
        );
        // And so is the 5th percentile, at this sample size. `percentile` is
        // linear-rank: at n=20 it lands on the SECOND-smallest value, so one
        // catastrophic minute never reaches it. This test was written
        // asserting 2 here, failed, and that failure is what added
        // `volume_capture_min_pct` — the statistic that does surface it.
        assert_eq!(
            cmp.volume_capture_p05_pct, 100,
            "p05 describes the distribution, and at n=20 one bad cell is not \
             the distribution"
        );
        assert_eq!(
            cmp.volume_capture_min_pct, 2,
            "the MINIMUM is what surfaces one bad minute — 20 of 1,000 is 2%, \
             and without this field that hole was invisible in every reported \
             number"
        );
    }

    #[test]
    fn over_capture_is_reported_rather_than_clamped() {
        // Reading ABOVE 100% is not noise to be tidied away — it means our
        // volume exceeds the vendor's own tape for that minute, which cannot
        // happen honestly. It is the signature of the double-counting defect
        // measured on 2026-08-24 (intraday frames at ~9.2x the day bar), and
        // clamping it to 100 would erase the only evidence of it.
        let live = vec![side(1, OPEN, bar_vol(10.0, 10.0, 10.0, 10.0, 9_200))];
        let rest = vec![side(1, OPEN, bar_vol(10.0, 10.0, 10.0, 10.0, 1_000))];
        let cmp = compare_day(&live, &rest, DAY_START_NANOS, RUN_TS, 0, false);

        assert_eq!(
            cmp.volume_capture_p50_pct, 920,
            "920% must survive to the report — a clamp here would hide a \
             real, already-measured defect"
        );
    }

    // -----------------------------------------------------------------
    // THE ROTATING SWEEP — added 2026-08-26
    //
    // The loop has a hard budget and breaks on it. With a FIXED start that is
    // not "fewer instruments" — it is "the tail is NEVER reached", every day,
    // forever, while coverage reads a respectable 77%.
    // -----------------------------------------------------------------

    fn day(n: i32) -> chrono::NaiveDate {
        chrono::NaiveDate::from_num_days_from_ce_opt(700_000 + n).unwrap_or_default()
    }

    #[test]
    fn an_empty_target_list_does_not_divide_by_zero() {
        // The only loss detector in the system must not panic on a universe
        // that failed to resolve. A wrong start is survivable; a panic here
        // takes the day's verification with it.
        assert_eq!(rotation_start(day(0), 0), 0);
        assert_eq!(rotation_start(day(1), 1), 0);
    }

    #[test]
    fn the_start_moves_between_consecutive_days() {
        // The whole point. Same start every day = same tail starved every day.
        let n = 868;
        let a = rotation_start(day(0), n);
        let b = rotation_start(day(1), n);
        assert_ne!(
            a, b,
            "two consecutive days must not begin at the same index"
        );
    }

    #[test]
    fn three_days_cover_the_whole_list_at_one_third_coverage() {
        // THE GUARANTEE, stated as a test rather than as a comment.
        //
        // Consecutive runs cover [k·stride, k·stride + c). Their union is the
        // whole list exactly when c >= stride. At stride = n/3, a run reaching
        // a third of the list closes the universe in three days.
        let n: usize = 868;
        let coverage = n.div_ceil(3);
        let mut seen = vec![false; n];
        for d in 0..3 {
            let start = rotation_start(day(d), n);
            for offset in 0..coverage {
                seen[(start + offset) % n] = true;
            }
        }
        let missed = seen.iter().filter(|s| !**s).count();
        assert_eq!(
            missed, 0,
            "at exactly one-third coverage every instrument must be reached \
             within three days — {missed} were not"
        );
    }

    #[test]
    fn the_measured_worst_case_closes_in_two_days() {
        // 77% is the measured floor: the limiter's 2 rps with 0.4s answers
        // against the 600s budget. It should not need the full three days.
        let n = 868;
        let coverage = n * 77 / 100;
        let mut seen = vec![false; n];
        for d in 0..2 {
            let start = rotation_start(day(d), n);
            for offset in 0..coverage {
                seen[(start + offset) % n] = true;
            }
        }
        assert_eq!(seen.iter().filter(|s| !**s).count(), 0);
    }

    #[test]
    fn a_full_sweep_visits_every_target_exactly_once() {
        // Rotation must be a PERMUTATION. If it skipped or repeated an index,
        // an unbudgeted run would quietly compare one instrument twice and
        // another never — which is the defect wearing a different hat.
        for n in [1usize, 2, 7, 868] {
            let start = rotation_start(day(5), n);
            let mut seen = vec![0u32; n];
            for offset in 0..n {
                seen[(start + offset) % n] += 1;
            }
            assert!(
                seen.iter().all(|c| *c == 1),
                "n={n}: a full sweep must touch every index exactly once"
            );
        }
    }

    #[test]
    fn a_year_boundary_does_not_reset_the_sweep() {
        // Day-of-YEAR would snap back to 0 every January and re-starve
        // whatever December was working through. Days-from-epoch is monotonic,
        // so 31 Dec and 1 Jan differ like any other pair.
        let n = 868;
        let dec31 = chrono::NaiveDate::from_ymd_opt(2026, 12, 31).unwrap_or_default();
        let jan01 = chrono::NaiveDate::from_ymd_opt(2027, 1, 1).unwrap_or_default();
        assert_ne!(rotation_start(dec31, n), rotation_start(jan01, n));
    }

    #[test]
    fn a_huge_universe_does_not_overflow_the_start_computation() {
        // `num_days_from_ce` is ~739,000. Multiplied by a large stride before
        // reduction that overflows a 32-bit usize. The reduction happens
        // FIRST for exactly this reason; this pins it.
        let n = 25_000; // the authorized ceiling
        let start = rotation_start(day(9), n);
        assert!(start < n, "start must stay inside the list");
    }

    // -----------------------------------------------------------------
    // THE SCOPE FILTER — added 2026-08-26 from a live total data loss
    //
    // Prod produced 764,003 findings and a 207,965,278-byte buffer against a
    // 104,857,600-byte ceiling; the flush failed and the ENTIRE day's
    // comparison was discarded. 757,273 of those findings (99.1%) were
    // `missing_rest` for instruments the run never asked the vendor for.
    // -----------------------------------------------------------------

    fn scope_of(pairs: &[(i64, &str)]) -> std::collections::BTreeSet<(i64, String)> {
        pairs
            .iter()
            .map(|(id, s)| (*id, (*s).to_string()))
            .collect()
    }

    #[test]
    fn an_instrument_never_requested_produces_no_findings_at_all() {
        // THE DEFECT, in one assertion. Live carries contracts we never fetch;
        // calling their minutes "missing from the vendor" is a scope mismatch
        // dressed as data loss.
        let live = vec![
            side(13, OPEN, bar(100.0, 101.0, 99.0, 100.5)),
            side(999, OPEN, bar(50.0, 51.0, 49.0, 50.5)),
        ];
        let rest = vec![side(13, OPEN, bar(100.0, 101.0, 99.0, 100.5))];
        let cmp = compare_day_in_scope(
            &live,
            &rest,
            &scope_of(&[(13, "IDX_I")]),
            DAY_START_NANOS,
            RUN_TS,
            0,
            false,
        );
        assert_eq!(
            cmp.missing_rest, 0,
            "instrument 999 was never requested — it must produce NO finding, \
             not a 'the vendor is missing this' row"
        );
        assert_eq!(
            cmp.minutes_compared, 1,
            "the in-scope instrument still compares"
        );
    }

    #[test]
    fn an_out_of_scope_instrument_is_not_counted_in_the_universe() {
        // `instruments: 8144` sat in the verdict beside `targets: 865` and
        // read as coverage. It was the live universe, not the verified one.
        let live = vec![
            side(13, OPEN, bar(100.0, 101.0, 99.0, 100.5)),
            side(999, OPEN, bar(50.0, 51.0, 49.0, 50.5)),
            side(777, OPEN, bar(10.0, 11.0, 9.0, 10.5)),
        ];
        let cmp = compare_day_in_scope(
            &live,
            &[],
            &scope_of(&[(13, "IDX_I")]),
            DAY_START_NANOS,
            RUN_TS,
            0,
            false,
        );
        assert_eq!(
            cmp.instruments, 1,
            "the reported universe must be the VERIFIED one, not everything \
             the feed happened to carry"
        );
    }

    #[test]
    fn a_requested_instrument_the_vendor_did_not_serve_is_still_missing_rest() {
        // The filter must not silence the real signal it sits beside. An
        // instrument we DID ask for and the vendor did not serve is the one
        // case `missing_rest` exists to report.
        let live = vec![side(13, OPEN, bar(100.0, 101.0, 99.0, 100.5))];
        let cmp = compare_day_in_scope(
            &live,
            &[],
            &scope_of(&[(13, "IDX_I")]),
            DAY_START_NANOS,
            RUN_TS,
            0,
            false,
        );
        assert_eq!(
            cmp.missing_rest, 1,
            "asked for and not served MUST still be reported — otherwise the \
             filter hides the very loss it was added beside"
        );
    }

    #[test]
    fn an_empty_scope_disables_the_filter() {
        // Every existing caller and test passes an empty set and must mean
        // exactly what it meant before.
        let live = vec![side(999, OPEN, bar(50.0, 51.0, 49.0, 50.5))];
        let cmp = compare_day_in_scope(
            &live,
            &[],
            &std::collections::BTreeSet::new(),
            DAY_START_NANOS,
            RUN_TS,
            0,
            false,
        );
        assert_eq!(cmp.missing_rest, 1, "an empty scope filters nothing");
    }

    #[test]
    fn the_scope_key_pairs_security_id_with_segment() {
        // I-P1-11. The same numeric id in a different segment is a DIFFERENT
        // instrument; matching on the id alone would admit one we never asked
        // for and re-create the defect one instrument at a time.
        let live = vec![side(13, OPEN, bar(100.0, 101.0, 99.0, 100.5))];
        let cmp = compare_day_in_scope(
            &live,
            &[],
            &scope_of(&[(13, "NSE_EQ")]), // same id, WRONG segment
            DAY_START_NANOS,
            RUN_TS,
            0,
            false,
        );
        assert_eq!(
            cmp.instruments, 0,
            "id 13 in IDX_I is not id 13 in NSE_EQ — the pair is the key"
        );
    }

    #[test]
    fn the_measured_prod_shape_collapses_by_two_orders_of_magnitude() {
        // The real numbers: ~8,144 live instruments, 865 targeted. Findings
        // should follow the TARGETED set, not the carried one.
        let mut live = Vec::new();
        for id in 0..200_i64 {
            live.push(side(id, OPEN, bar(100.0, 101.0, 99.0, 100.5)));
        }
        let scope = scope_of(&[(0, "IDX_I"), (1, "IDX_I"), (2, "IDX_I")]);
        let cmp = compare_day_in_scope(&live, &[], &scope, DAY_START_NANOS, RUN_TS, 0, false);
        assert_eq!(
            cmp.missing_rest, 3,
            "3 targeted instruments produce 3 findings — not 200. That ratio \
             is what took the buffer from 208 MB back under its ceiling"
        );
    }
}

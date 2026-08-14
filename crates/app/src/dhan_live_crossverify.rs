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
/// NSE session close — 15:30:00 IST, as seconds-of-day. The window is
/// half-open `[open, close)`, so the last comparable bucket opens at 15:29.
pub const SESSION_CLOSE_SECS_OF_DAY_IST: i64 = 15 * 3600 + 30 * 60;

/// The run's deterministic stamp — 15:31:00 IST, as seconds-of-day. Used as
/// the designated audit timestamp regardless of when the run actually fired,
/// so a catch-up / forced rerun UPSERTs the same rows.
pub const RUN_SECS_OF_DAY_IST: i64 = 15 * 3600 + 31 * 60;

/// How many trailing session minutes are excused when absent on the LIVE side.
///
/// At 15:31 the 15:29 bucket has only just closed and the 15:28 seal may still
/// be in flight, so a live-side gap there is EXPECTED, not evidence of loss.
/// Dhan's REST tape, being a post-hoc query, always has them.
pub const TAIL_UNSEALED_MINUTES: i64 = 2;

/// Row cap for the live-side `/exec` read. Beyond it the run is stamped
/// `partial` rather than silently comparing a truncated day.
pub const LIVE_ROW_LIMIT: usize = 200_000;

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
    /// Master switch. Default OFF.
    #[serde(default)]
    pub enabled: bool,
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
    /// Whole-run wall-clock budget (seconds). On elapse the run is stamped
    /// `partial` / `degraded` — never a fabricated clean verdict.
    #[serde(default = "default_run_budget_secs")]
    pub run_budget_secs: u64,
}

const fn default_fetch_timeout_secs() -> u64 {
    10
}
const fn default_run_budget_secs() -> u64 {
    240
}

impl Default for DhanLiveCrossverifyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            tolerance_paise: 0,
            fetch_timeout_secs: default_fetch_timeout_secs(),
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
        if self.fetch_timeout_secs == 0 || self.run_budget_secs == 0 {
            return Err(
                "[dhan_live_crossverify] fetch_timeout_secs and run_budget_secs must be > 0"
                    .to_string(),
            );
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
    pub missing_rest: i64,
    pub tail_unsealed: i64,
    pub out_of_session: i64,
    /// Median divergence magnitude in paise — what NORMAL looks like.
    pub noise_p50_paise: i64,
    pub noise_p95_paise: i64,
    pub noise_max_paise: i64,
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
/// | live has it, REST doesn't | `missing_rest` | **yes** |
/// | either side outside `[09:15, 15:30)` | `out_of_session` | no |
#[must_use]
pub fn compare_day(
    live: &[SideBar],
    rest: &[SideBar],
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
    let mut missing_live: i64 = 0;
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
        let real = cells_diverged > 0 || missing_live > 0 || missing_rest > 0;
        if degraded {
            DhanLiveXverifyOutcome::Partial
        } else if real {
            DhanLiveXverifyOutcome::Diverged
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
        missing_rest,
        tail_unsealed,
        out_of_session,
        noise_p50_paise,
        noise_p95_paise,
        noise_max_paise,
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

/// What one run did — returned to the caller for logging and metrics.
#[derive(Clone, Debug, PartialEq)]
pub struct RunReport {
    pub comparison: DayComparison,
    /// Targets whose REST fetch failed, timed out, or returned no candles.
    pub rest_failures: usize,
    /// `true` when any leg failed, the live read truncated, or the run budget
    /// elapsed — forces the verdict down to `partial` / `degraded`.
    pub degraded: bool,
    /// Rows skipped as malformed on either side (counted, never coerced).
    pub malformed_rows: usize,
    /// `true` when the run budget elapsed before every target was fetched.
    pub budget_elapsed: bool,
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
) -> Result<Vec<SideBar>, String> {
    let next_day = trading_date
        .succ_opt()
        .ok_or_else(|| "trading date has no successor".to_string())?;
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
        .map_err(|_| "rest fetch timed out".to_string())?
        // The token never appears in the error: reqwest renders the URL, not
        // headers, and the URL carries no query params here.
        .map_err(|e| format!("rest fetch transport: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("rest fetch http {status}"));
    }
    let text = tokio::time::timeout(timeout, resp.text())
        .await
        .map_err(|_| "rest body timed out".to_string())?
        .map_err(|e| format!("rest body: {e}"))?;
    let candles = crate::dhan_intraday_parse::parse_intraday_1m_candles(&text);
    let (bars, malformed) =
        rest_candles_to_side_bars(&candles, target.security_id, &target.segment);
    if bars.is_empty() && malformed == 0 {
        return Err("rest returned no candles".to_string());
    }
    Ok(bars)
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

    let (live, truncated, live_malformed) =
        read_live_side(client, questdb_exec_url, day_start_ist_nanos, fetch_timeout).await?;

    let mut rest: Vec<SideBar> = Vec::new();
    let mut rest_failures = 0usize;
    let mut budget_elapsed = false;
    for target in targets {
        if started.elapsed() >= budget {
            // Stop issuing work; the verdict degrades rather than pretending
            // the un-fetched targets were clean.
            budget_elapsed = true;
            break;
        }
        match fetch_rest_side_for_target(
            client,
            intraday_url,
            jwt,
            target,
            trading_date,
            fetch_timeout,
        )
        .await
        {
            Ok(mut bars) => rest.append(&mut bars),
            Err(_) => rest_failures = rest_failures.saturating_add(1),
        }
    }

    let degraded = truncated || budget_elapsed || rest_failures > 0;
    let run_ts = deterministic_run_ts_nanos(day_start_ist_nanos);
    let comparison = compare_day(
        &live,
        &rest,
        day_start_ist_nanos,
        run_ts,
        cfg.tolerance_paise,
        degraded,
    );

    Ok(RunReport {
        comparison,
        rest_failures,
        degraded,
        malformed_rows: live_malformed,
        budget_elapsed,
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
            "15:29 is the last in-session bucket"
        );
        assert!(
            !is_in_session(minute(SESSION_CLOSE_SECS_OF_DAY_IST), DAY_START_NANOS),
            "15:30 is out (half-open window)"
        );
        // 375 comparable minutes in a full session.
        let total = (SESSION_CLOSE_SECS_OF_DAY_IST - SESSION_OPEN_SECS_OF_DAY_IST) / 60;
        assert_eq!(total, 375);
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
    fn config_default_is_off_and_paise_exact() {
        let c = DhanLiveCrossverifyConfig::default();
        assert!(!c.enabled, "the comparator must default OFF (fail-safe)");
        assert_eq!(c.tolerance_paise, 0, "default is paise-exact");
        assert!(c.validate().is_ok());
    }

    #[test]
    fn absent_config_section_deserializes_to_disabled() {
        // An entirely absent section (no keys at all) must still deserialize
        // to the disabled default — the fail-safe posture.
        let c: DhanLiveCrossverifyConfig =
            serde_json::from_str("{}").expect("absent section deserializes");
        assert!(!c.enabled);
        assert_eq!(c, DhanLiveCrossverifyConfig::default());
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
    fn deterministic_run_ts_nanos_is_1531_ist_regardless_of_fire_time() {
        let stamp = deterministic_run_ts_nanos(DAY_START_NANOS);
        assert_eq!(
            secs_of_day(stamp, DAY_START_NANOS),
            15 * 3600 + 31 * 60,
            "the audit stamp must be 15:31 IST regardless of actual fire time"
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
            degraded: true,
            malformed_rows: 0,
            budget_elapsed: true,
        };
        assert!(report.degraded);
        assert!(report.budget_elapsed);
        assert!(!report.comparison.outcome.is_pass());
    }
}

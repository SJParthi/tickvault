//! Pre-market expiry resolution (operator spec 2026-07-15): the PURE
//! per-underlying selection policy over vendor-raw `yyyymmdd` expiry
//! lists, plus the process-global DAY-LOCKED store whose read facade IS
//! the [`ExpiryResolver`] seam the runner stamps chain requests from.
//!
//! Split of duties (locked): executors only FETCH (vendor-raw, unsorted
//! tolerated, garbage tolerated — [`super::executor::ExpiryListRequest`]);
//! THIS module owns the policy math and the day lock. The scheduler NEVER
//! guesses an expiry: every fail path here is `None` (fail-closed).
//!
//! # The per-underlying policy
//! - NIFTY / SENSEX — [`ExpiryPolicy::NearestActiveDate`]: the minimum
//!   valid date ≥ today (weekly serials exist; the nearest active date IS
//!   the current contract).
//! - BANKNIFTY — [`ExpiryPolicy::LastExpiryOfNearestActiveMonth`]: group
//!   the dates by (year, month), pick the NEAREST group containing ANY
//!   date ≥ today, and return that group's LAST date. NEVER a flat
//!   `min()` and never a reliance on vendor list shape. Assumed (flagged):
//!   BANKNIFTY has no weekly expiries post the Nov-2024 NSE expiry
//!   rationalisation — the month-grouping is robust EITHER WAY (a vendor
//!   list still carrying stale weekly rows resolves to the month's LAST
//!   date regardless).
//!
//! # The day lock
//! [`DayLockedExpiryStore`] is keyed by the IST TRADING DAY the runner's
//! injected clock reports ([`super::runner::CadenceClock::ist_date`] —
//! derived via the house `tickvault_common::trading_calendar::ist_offset`
//! helper in production, NEVER UTC). Re-resolution happens ONLY at a day
//! flip; a supervisor respawn re-reads the lock (the store is
//! process-global) and never re-resolves mid-day. First write wins per
//! (broker, underlying, day).

use std::sync::{Arc, Mutex, OnceLock};

use chrono::{Datelike, NaiveDate};
use tickvault_common::feed::Feed;

use super::executor::ExpiryResolver;
use crate::pipeline::chain_snapshot::ChainUnderlying;

/// A VALIDATED expiry date: `yyyymmdd` (`u32`) that parses to a real
/// calendar date — constructible only via [`ExpiryDate::from_yyyymmdd`],
/// so every carried value is provably valid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExpiryDate(u32);

impl ExpiryDate {
    /// Validate a vendor-raw `yyyymmdd` integer — `None` for anything
    /// that is not a real calendar date (month 0/13, day 0/32, a
    /// truncated integer, …) or whose year sits outside the plausible
    /// market range `[2000, 2100]` (a truncated integer like `123`
    /// parses as year-0 Jan-23 in the proleptic calendar — the year
    /// bound keeps it garbage). Fail-closed: garbage never becomes a
    /// date.
    #[must_use]
    // TEST-EXEMPT: covered by test_cadence_expiry_date_yyyymmdd_iso_naive_helpers (guard name-pattern mismatch).
    pub fn from_yyyymmdd(raw: u32) -> Option<Self> {
        let year = raw / 10_000;
        if !(2_000..=2_100).contains(&year) {
            return None;
        }
        yyyymmdd_to_naive(raw).map(|_| Self(raw))
    }

    /// The wire-format `yyyymmdd` integer (what
    /// `ChainFetchRequest.expiry_yyyymmdd` carries).
    #[must_use]
    pub const fn yyyymmdd(self) -> u32 {
        self.0
    }

    /// ISO-8601 date string (`"YYYY-MM-DD"`) — the shape the broker
    /// expiry-keyed REST bodies want. Cold-path allocation, honestly
    /// accepted (a handful per day).
    #[must_use]
    // TEST-EXEMPT: covered by test_cadence_expiry_date_yyyymmdd_iso_naive_helpers (guard name-pattern mismatch).
    pub fn as_iso_string(self) -> String {
        format!(
            "{:04}-{:02}-{:02}",
            self.0 / 10_000,
            (self.0 / 100) % 100,
            self.0 % 100
        )
    }

    /// The [`NaiveDate`] this expiry names (validated at construction, so
    /// this is total; the defensive fallback is unreachable).
    #[must_use]
    pub fn naive_date(self) -> NaiveDate {
        yyyymmdd_to_naive(self.0).unwrap_or(NaiveDate::MIN)
    }
}

/// Parse a `yyyymmdd` integer to a real calendar date (`None` = garbage).
#[must_use]
// TEST-EXEMPT: covered by test_cadence_expiry_date_yyyymmdd_iso_naive_helpers (guard name-pattern mismatch).
pub fn yyyymmdd_to_naive(raw: u32) -> Option<NaiveDate> {
    let year = i32::try_from(raw / 10_000).ok()?;
    let month = (raw / 100) % 100;
    let day = raw % 100;
    NaiveDate::from_ymd_opt(year, month, day)
}

/// A [`NaiveDate`] as its `yyyymmdd` integer (`None` for years outside
/// the u32-encodable range — unreachable for market dates).
#[must_use]
// TEST-EXEMPT: covered by test_cadence_expiry_date_yyyymmdd_iso_naive_helpers (guard name-pattern mismatch).
pub fn naive_to_yyyymmdd(date: NaiveDate) -> Option<u32> {
    let year = u32::try_from(date.year()).ok()?;
    Some(year * 10_000 + date.month() * 100 + date.day())
}

/// The per-underlying expiry selection policy (operator spec 2026-07-15).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExpiryPolicy {
    /// The minimum valid date ≥ today (NIFTY / SENSEX — weeklies exist).
    NearestActiveDate,
    /// Group by (year, month); pick the nearest group containing ANY date
    /// ≥ today; return that group's LAST date (BANKNIFTY monthlies —
    /// never a flat `min()`, never vendor-shape reliance).
    LastExpiryOfNearestActiveMonth,
}

/// Which policy governs `underlying` (operator spec 2026-07-15).
#[must_use]
pub const fn policy_for(underlying: ChainUnderlying) -> ExpiryPolicy {
    match underlying {
        ChainUnderlying::Nifty | ChainUnderlying::Sensex => ExpiryPolicy::NearestActiveDate,
        ChainUnderlying::Banknifty => ExpiryPolicy::LastExpiryOfNearestActiveMonth,
    }
}

/// Apply `policy` to a VENDOR-RAW date list (unsorted tolerated, garbage
/// entries dropped) against `today_yyyymmdd`. Pure. Fail-closed: an
/// empty/all-garbage list, or a garbage `today`, is `None` — never a
/// guess. On an EXPIRY DAY the selected date may equal today (the
/// contract holds through its own close); the NEXT day's resolution rolls
/// forward naturally (`today` advances past it).
///
/// Validated-`yyyymmdd` ordering == calendar ordering, so the selection
/// compares raw integers after validation.
#[must_use]
// TEST-EXEMPT: covered by the policy unit tests + the 4 expiry proptests in this module (guard name-pattern mismatch).
pub fn resolve_policy_expiry(
    policy: ExpiryPolicy,
    raw_dates: &[u32],
    today_yyyymmdd: u32,
) -> Option<ExpiryDate> {
    // A garbage "today" can never anchor a selection.
    ExpiryDate::from_yyyymmdd(today_yyyymmdd)?;
    // The nearest ACTIVE (≥ today) valid date — both policies anchor on
    // it (for the month policy, its (year, month) IS the nearest group
    // containing any active date: an earlier qualifying group would have
    // to hold an active date smaller than the minimum active date).
    let nearest_active = raw_dates
        .iter()
        .copied()
        .filter(|raw| ExpiryDate::from_yyyymmdd(*raw).is_some())
        .filter(|raw| *raw >= today_yyyymmdd)
        .min()?;
    match policy {
        ExpiryPolicy::NearestActiveDate => ExpiryDate::from_yyyymmdd(nearest_active),
        ExpiryPolicy::LastExpiryOfNearestActiveMonth => {
            let group = nearest_active / 100; // yyyymm
            let last_of_group = raw_dates
                .iter()
                .copied()
                .filter(|raw| ExpiryDate::from_yyyymmdd(*raw).is_some())
                .filter(|raw| *raw / 100 == group)
                .max()?;
            ExpiryDate::from_yyyymmdd(last_of_group)
        }
    }
}

/// Should the pre-market deadline PAGE fire now for one
/// (broker, underlying) pair? Pure edge-latch decision — TRUE exactly
/// once per episode: past the deadline, still unresolved, not already
/// paged this day. The deadline gates the PAGE, never the attempts.
#[must_use]
pub fn expiry_page_due(
    now_secs_of_day: u32,
    deadline_secs_of_day: u32,
    resolved: bool,
    already_paged: bool,
) -> bool {
    now_secs_of_day >= deadline_secs_of_day && !resolved && !already_paged
}

/// E4 (2026-07-15): the minimum CONSECUTIVE failed attempt waves a
/// process that BOOTED after the deadline must observe before the
/// edge-latched `expiry_unresolved` page fires. A crash-boot at e.g.
/// 11:00 IST previously paged on the very FIRST failed wave (hair
/// trigger — one transient vendor blip at boot = a page); it now needs
/// ≥2 consecutive failed waves. The pre-deadline path is unchanged: the
/// deadline itself still gates the page (threshold 1 — an unresolved
/// pair at the deadline crossing has, by loop construction, just failed
/// its wave).
pub const POST_DEADLINE_BOOT_MIN_FAILED_WAVES: u32 = 2;

/// Wave-aware page decision (E4, 2026-07-15): [`expiry_page_due`] AND the
/// boot-mode wave threshold — `booted_after_deadline` is whether the
/// resolution loop's FIRST observation of this IST day was already past
/// the deadline (a post-deadline crash-boot), in which case
/// [`POST_DEADLINE_BOOT_MIN_FAILED_WAVES`] consecutive failed waves are
/// required; otherwise 1 (the pre-deadline path, unchanged).
#[must_use]
pub fn expiry_page_due_after_wave(
    now_secs_of_day: u32,
    deadline_secs_of_day: u32,
    resolved: bool,
    already_paged: bool,
    booted_after_deadline: bool,
    consecutive_failed_waves: u32,
) -> bool {
    let min_waves = if booted_after_deadline {
        POST_DEADLINE_BOOT_MIN_FAILED_WAVES
    } else {
        1
    };
    consecutive_failed_waves >= min_waves
        && expiry_page_due(
            now_secs_of_day,
            deadline_secs_of_day,
            resolved,
            already_paged,
        )
}

/// One underlying's day-locked resolution view (the read API surface the
/// future capture-leg delegation consumes — see the ONE-SOURCE-OF-TRUTH
/// DELEGATION section of `cadence-error-codes.md`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UnderlyingExpiryView {
    /// The WINNING expiry keying BOTH lanes: Dhan's policy date when
    /// present (exchange-sourced expirylist — the disagreement-arm
    /// authority), else Groww's.
    pub winner: Option<ExpiryDate>,
    /// Dhan's raw policy date (provenance — kept even when it loses
    /// nothing: the winner IS this when present).
    pub dhan_raw: Option<ExpiryDate>,
    /// Groww's raw policy date (provenance).
    pub groww_raw: Option<ExpiryDate>,
    /// TRUE when both brokers resolved for the day and their policy dates
    /// differ — Dhan won, loudly (CADENCE-01 `expiry_disagreement`),
    /// never a silent override.
    pub disagreement: bool,
}

/// The verdict of one [`DayLockedExpiryStore::record_policy_date`] call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordVerdict {
    /// TRUE when the write landed (first write for the pair this day);
    /// FALSE when the day lock kept the existing value.
    pub recorded: bool,
    /// TRUE exactly once per (underlying, day): this write made both
    /// brokers' dates present AND differing — the caller emits the
    /// edge-latched disagreement page.
    pub newly_disagreeing: bool,
}

/// Per-(broker, underlying, IST-trading-day) resolution state, locked for
/// the day (respawn-proof: the store is process-global; a respawned
/// runner re-READS, never re-resolves mid-day). Interior mutability via a
/// poison-recovering `Mutex` — cold path (a handful of writes per day).
#[derive(Debug, Default)]
pub struct DayLockedExpiryStore {
    state: Mutex<StoreState>,
}

#[derive(Debug, Default)]
struct StoreState {
    /// The IST trading day the lock currently keys (None = never keyed).
    day: Option<NaiveDate>,
    /// Raw policy dates per `[feed][underlying]` (first write wins).
    raw: [[Option<ExpiryDate>; ChainUnderlying::COUNT]; Feed::COUNT],
    /// Per-underlying disagreement latch (edge — fires once per day).
    disagreement: [bool; ChainUnderlying::COUNT],
}

impl StoreState {
    /// Re-key to `day`, dropping the previous day's lock (the ONLY
    /// re-resolution trigger).
    fn ensure_day(&mut self, day: NaiveDate) {
        if self.day != Some(day) {
            *self = StoreState {
                day: Some(day),
                ..StoreState::default()
            };
        }
    }

    fn view(&self, underlying: ChainUnderlying) -> UnderlyingExpiryView {
        let dhan_raw = self.raw[Feed::Dhan.index()][underlying.index()];
        let groww_raw = self.raw[Feed::Groww.index()][underlying.index()];
        UnderlyingExpiryView {
            // Dhan (exchange-sourced expirylist) WINS for keying BOTH
            // lanes whenever it resolved; Groww's date drives otherwise.
            winner: dhan_raw.or(groww_raw),
            dhan_raw,
            groww_raw,
            disagreement: self.disagreement[underlying.index()],
        }
    }
}

impl DayLockedExpiryStore {
    /// A fresh, un-keyed store (nothing resolved).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, StoreState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Record one broker's POLICY date for `day` (first write wins per
    /// (broker, underlying, day) — re-resolution ONLY at a day flip,
    /// which this call performs when `day` differs from the locked one).
    // TEST-EXEMPT: covered by test_cadence_expiry_store_day_lock_first_write_wins_and_day_flip + the disagreement/facade store tests (guard name-pattern mismatch).
    pub fn record_policy_date(
        &self,
        day: NaiveDate,
        broker: Feed,
        underlying: ChainUnderlying,
        date: ExpiryDate,
    ) -> RecordVerdict {
        let mut state = self.lock();
        state.ensure_day(day);
        let slot = &mut state.raw[broker.index()][underlying.index()];
        let recorded = if slot.is_some() {
            false // day-locked: the first resolution holds through close
        } else {
            *slot = Some(date);
            true
        };
        let view = state.view(underlying);
        let newly_disagreeing = match (view.dhan_raw, view.groww_raw) {
            (Some(d), Some(g)) if d != g && !state.disagreement[underlying.index()] => {
                state.disagreement[underlying.index()] = true;
                true
            }
            _ => false,
        };
        RecordVerdict {
            recorded,
            newly_disagreeing,
        }
    }

    /// Is `(broker, underlying)` resolved for `day`? (A different locked
    /// day reads unresolved — never a stale answer.)
    #[must_use]
    // TEST-EXEMPT: covered by test_cadence_expiry_store_day_lock_first_write_wins_and_day_flip (guard name-pattern mismatch).
    pub fn is_resolved(&self, day: NaiveDate, broker: Feed, underlying: ChainUnderlying) -> bool {
        let state = self.lock();
        state.day == Some(day) && state.raw[broker.index()][underlying.index()].is_some()
    }

    /// The full day-checked view for `underlying` (winner + per-broker
    /// raw provenance + the disagreement verdict). A `day` differing from
    /// the locked one reads the empty default — never a stale answer.
    #[must_use]
    pub fn view(&self, day: NaiveDate, underlying: ChainUnderlying) -> UnderlyingExpiryView {
        let state = self.lock();
        if state.day == Some(day) {
            state.view(underlying)
        } else {
            UnderlyingExpiryView::default()
        }
    }

    /// The winning expiry for `underlying` on the IST trading day `day` —
    /// the [`ExpiryResolver`] read facade's core, DAY-CHECKED (E1 fix,
    /// 2026-07-15): a `day` differing from the locked one returns `None`,
    /// never a stale answer. Before this fix the facade read whatever day
    /// the resolution loop LAST keyed — a process crossing IST midnight
    /// whose morning re-resolution kept FAILING served YESTERDAY'S winner
    /// (on day-after-expiry: the EXPIRED contract) until the first
    /// SUCCESSFUL re-resolution, not "at most one retry interval" as the
    /// old comment falsely claimed. Callers thread `day` from the SAME
    /// injected-clock IST path the store's day keying uses
    /// (`CadenceClock::ist_date` / `trading_calendar::ist_offset()` —
    /// NEVER UTC).
    #[must_use]
    // TEST-EXEMPT: covered by test_cadence_expiry_winning_facade_never_serves_yesterdays_winner_across_day_flip + test_cadence_expiry_store_resolver_facade_reads_winner_for_both_brokers (guard name-pattern mismatch).
    pub fn winning_expiry(
        &self,
        day: NaiveDate,
        underlying: ChainUnderlying,
    ) -> Option<ExpiryDate> {
        self.view(day, underlying).winner
    }
}

/// The store IS the increment-1 [`ExpiryResolver`] seam's production
/// implementation (its read facade): the runner stamps every chain
/// request from the WINNING date. The `broker` parameter selects nothing
/// here — the disagreement rule keys BOTH lanes on the Dhan-preferred
/// winner (design: the exchange-sourced expirylist is authoritative).
impl ExpiryResolver for DayLockedExpiryStore {
    fn resolved_expiry(
        &self,
        _broker: Feed,
        underlying: ChainUnderlying,
        day: NaiveDate,
    ) -> Option<u32> {
        self.winning_expiry(day, underlying)
            .map(ExpiryDate::yyyymmdd)
    }
}

/// The process-global day-locked expiry store (`OnceLock` — the
/// respawn-proof single source every consumer reads; the future
/// capture-leg delegation named in `cadence-error-codes.md` consults THIS
/// handle). Tests construct their OWN stores — only the production boot
/// wiring touches the global.
static GLOBAL_EXPIRY_STORE: OnceLock<Arc<DayLockedExpiryStore>> = OnceLock::new();

/// The process-global store handle (created empty on first access).
// TEST-EXEMPT: OnceLock get_or_init one-liner; DayLockedExpiryStore behavior is unit-tested above.
pub fn global_expiry_store() -> &'static Arc<DayLockedExpiryStore> {
    GLOBAL_EXPIRY_STORE.get_or_init(|| Arc::new(DayLockedExpiryStore::new()))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn test_cadence_expiry_date_yyyymmdd_iso_naive_helpers() {
        let d = ExpiryDate::from_yyyymmdd(20_260_730).expect("valid date");
        assert_eq!(d.yyyymmdd(), 20_260_730);
        assert_eq!(d.as_iso_string(), "2026-07-30");
        assert_eq!(
            d.naive_date(),
            NaiveDate::from_ymd_opt(2026, 7, 30).expect("valid")
        );
        // Round-trip through the NaiveDate helper.
        assert_eq!(naive_to_yyyymmdd(d.naive_date()), Some(20_260_730));
        // Garbage never becomes a date (fail-closed).
        assert_eq!(ExpiryDate::from_yyyymmdd(20_261_301), None); // month 13
        assert_eq!(ExpiryDate::from_yyyymmdd(20_260_732), None); // day 32
        assert_eq!(ExpiryDate::from_yyyymmdd(20_260_200 + 30), None); // Feb 30
        assert_eq!(ExpiryDate::from_yyyymmdd(0), None);
        assert_eq!(ExpiryDate::from_yyyymmdd(123), None);
    }

    #[test]
    fn test_cadence_expiry_policy_for_underlyings_locked_mapping() {
        // The operator's 2026-07-15 per-underlying policy mapping.
        assert_eq!(
            policy_for(ChainUnderlying::Nifty),
            ExpiryPolicy::NearestActiveDate
        );
        assert_eq!(
            policy_for(ChainUnderlying::Sensex),
            ExpiryPolicy::NearestActiveDate
        );
        assert_eq!(
            policy_for(ChainUnderlying::Banknifty),
            ExpiryPolicy::LastExpiryOfNearestActiveMonth
        );
    }

    #[test]
    fn test_cadence_expiry_nearest_active_date_unsorted_vendor_list() {
        // Unsorted vendor-raw list with past dates and garbage entries:
        // the nearest ≥ today wins; garbage is dropped, never selected.
        let dates = [20_260_820, 20_260_716, 99_999_999, 20_260_709, 20_260_723];
        let got = resolve_policy_expiry(ExpiryPolicy::NearestActiveDate, &dates, 20_260_715);
        assert_eq!(got.map(ExpiryDate::yyyymmdd), Some(20_260_716));
        // Expiry day: today itself qualifies (holds through close)…
        let got = resolve_policy_expiry(ExpiryPolicy::NearestActiveDate, &dates, 20_260_716);
        assert_eq!(got.map(ExpiryDate::yyyymmdd), Some(20_260_716));
        // …and the NEXT day's resolution rolls forward.
        let got = resolve_policy_expiry(ExpiryPolicy::NearestActiveDate, &dates, 20_260_717);
        assert_eq!(got.map(ExpiryDate::yyyymmdd), Some(20_260_723));
    }

    #[test]
    fn test_cadence_expiry_banknifty_month_last_never_flat_min() {
        // The BANKNIFTY trap the policy exists for: the vendor list holds
        // stale weekly-shaped rows inside the nearest active month — a
        // flat min() would pick 20260716; the month rule picks the
        // month's LAST date (20260728). Stale past-month groups + a gap
        // month are ignored.
        let dates = [
            20_260_527, // stale past-month group
            20_260_716, 20_260_721, 20_260_728, // July group (nearest active)
            20_260_929, // September (gap over August)
        ];
        let got = resolve_policy_expiry(
            ExpiryPolicy::LastExpiryOfNearestActiveMonth,
            &dates,
            20_260_715,
        );
        assert_eq!(got.map(ExpiryDate::yyyymmdd), Some(20_260_728));
        // Expiry day (July's last): holds through its own close…
        let got = resolve_policy_expiry(
            ExpiryPolicy::LastExpiryOfNearestActiveMonth,
            &dates,
            20_260_728,
        );
        assert_eq!(got.map(ExpiryDate::yyyymmdd), Some(20_260_728));
        // …and the next day rolls to the NEXT group across the gap.
        let got = resolve_policy_expiry(
            ExpiryPolicy::LastExpiryOfNearestActiveMonth,
            &dates,
            20_260_729,
        );
        assert_eq!(got.map(ExpiryDate::yyyymmdd), Some(20_260_929));
        // Mid-month with only LATER dates of the same month active: the
        // month still qualifies via its ≥ today member and the LAST date
        // is returned — which is provably ≥ today.
        let got = resolve_policy_expiry(
            ExpiryPolicy::LastExpiryOfNearestActiveMonth,
            &dates,
            20_260_722,
        );
        assert_eq!(got.map(ExpiryDate::yyyymmdd), Some(20_260_728));
    }

    #[test]
    fn test_cadence_expiry_empty_and_garbage_lists_fail_closed() {
        for policy in [
            ExpiryPolicy::NearestActiveDate,
            ExpiryPolicy::LastExpiryOfNearestActiveMonth,
        ] {
            // Empty list → None.
            assert_eq!(resolve_policy_expiry(policy, &[], 20_260_715), None);
            // All-garbage list → None (never a guess).
            assert_eq!(
                resolve_policy_expiry(policy, &[0, 99_999_999, 20_261_399], 20_260_715),
                None
            );
            // All-past list → None (an all-past group can never be
            // selected).
            assert_eq!(
                resolve_policy_expiry(policy, &[20_260_101, 20_260_212], 20_260_715),
                None
            );
            // A garbage TODAY anchor → None.
            assert_eq!(
                resolve_policy_expiry(policy, &[20_260_716], 99_999_999),
                None
            );
        }
    }

    #[test]
    fn test_cadence_expiry_process_restart_reresolution_rolls_forward_when_vendor_drops_today() {
        // E2 demo (verifier, 2026-07-15 — passes BY DESIGN, documenting
        // the residual named in cadence-error-codes.md §0): the day lock
        // is IN-MEMORY, so a mid-session process RESTART re-resolves
        // from scratch. On expiry day, a vendor list that dropped
        // today's date intraday makes the fresh resolution roll BOTH
        // lanes forward to the next series mid-session — the pure policy
        // math is doing exactly what it is told; only a persisted day
        // lock (flagged follow-up) would pin the morning's verdict
        // across a restart. Task RESPAWNS are covered (process-global
        // store); process restarts are not.
        let morning = [20_260_716, 20_260_723];
        let held = resolve_policy_expiry(ExpiryPolicy::NearestActiveDate, &morning, 20_260_716);
        assert_eq!(held.map(ExpiryDate::yyyymmdd), Some(20_260_716));
        let intraday = [20_260_723];
        let rolled = resolve_policy_expiry(ExpiryPolicy::NearestActiveDate, &intraday, 20_260_716);
        assert_eq!(rolled.map(ExpiryDate::yyyymmdd), Some(20_260_723));
    }

    #[test]
    fn test_cadence_expiry_store_day_lock_first_write_wins_and_day_flip() {
        let store = DayLockedExpiryStore::new();
        let day = NaiveDate::from_ymd_opt(2026, 7, 15).expect("valid");
        let d1 = ExpiryDate::from_yyyymmdd(20_260_716).expect("valid");
        let d2 = ExpiryDate::from_yyyymmdd(20_260_723).expect("valid");
        assert!(!store.is_resolved(day, Feed::Dhan, ChainUnderlying::Nifty));
        let v = store.record_policy_date(day, Feed::Dhan, ChainUnderlying::Nifty, d1);
        assert!(v.recorded && !v.newly_disagreeing);
        // First write wins for the day — a re-resolution attempt is a
        // no-op (day-locked; NEVER re-resolved mid-day).
        let v = store.record_policy_date(day, Feed::Dhan, ChainUnderlying::Nifty, d2);
        assert!(!v.recorded);
        assert_eq!(
            store.view(day, ChainUnderlying::Nifty).winner,
            Some(d1),
            "the day lock holds the FIRST resolution"
        );
        // A respawn re-READS the lock (same store, same day) — resolved.
        assert!(store.is_resolved(day, Feed::Dhan, ChainUnderlying::Nifty));
        // A different day reads unresolved/default — never stale.
        let next = NaiveDate::from_ymd_opt(2026, 7, 16).expect("valid");
        assert!(!store.is_resolved(next, Feed::Dhan, ChainUnderlying::Nifty));
        assert_eq!(
            store.view(next, ChainUnderlying::Nifty),
            UnderlyingExpiryView::default()
        );
        // The day FLIP re-keys (the only re-resolution trigger).
        let v = store.record_policy_date(next, Feed::Dhan, ChainUnderlying::Nifty, d2);
        assert!(v.recorded);
        assert_eq!(store.view(next, ChainUnderlying::Nifty).winner, Some(d2));
        assert_eq!(
            store.view(day, ChainUnderlying::Nifty),
            UnderlyingExpiryView::default(),
            "the previous day's lock is gone after the flip"
        );
    }

    #[test]
    fn test_cadence_expiry_store_disagreement_dhan_wins_edge_latched() {
        let store = DayLockedExpiryStore::new();
        let day = NaiveDate::from_ymd_opt(2026, 7, 15).expect("valid");
        let groww_date = ExpiryDate::from_yyyymmdd(20_260_723).expect("valid");
        let dhan_date = ExpiryDate::from_yyyymmdd(20_260_716).expect("valid");
        // Groww resolves first — it drives the winner alone.
        let v = store.record_policy_date(day, Feed::Groww, ChainUnderlying::Nifty, groww_date);
        assert!(v.recorded && !v.newly_disagreeing);
        assert_eq!(
            store.view(day, ChainUnderlying::Nifty).winner,
            Some(groww_date)
        );
        // Dhan resolves later with a DIFFERENT date: Dhan WINS for keying
        // BOTH lanes, the disagreement fires exactly once (edge), and
        // both raw dates stay recorded (provenance).
        let v = store.record_policy_date(day, Feed::Dhan, ChainUnderlying::Nifty, dhan_date);
        assert!(v.recorded && v.newly_disagreeing);
        let view = store.view(day, ChainUnderlying::Nifty);
        assert_eq!(view.winner, Some(dhan_date), "Dhan wins on disagreement");
        assert_eq!(view.dhan_raw, Some(dhan_date));
        assert_eq!(view.groww_raw, Some(groww_date));
        assert!(view.disagreement);
        // A repeat write can never re-fire the edge.
        let v = store.record_policy_date(day, Feed::Dhan, ChainUnderlying::Nifty, dhan_date);
        assert!(!v.recorded && !v.newly_disagreeing);
        // Agreement on another underlying never flags.
        let v = store.record_policy_date(day, Feed::Dhan, ChainUnderlying::Sensex, dhan_date);
        assert!(v.recorded && !v.newly_disagreeing);
        let v = store.record_policy_date(day, Feed::Groww, ChainUnderlying::Sensex, dhan_date);
        assert!(v.recorded && !v.newly_disagreeing);
        assert!(!store.view(day, ChainUnderlying::Sensex).disagreement);
    }

    #[test]
    fn test_cadence_expiry_store_resolver_facade_reads_winner_for_both_brokers() {
        // The increment-1 ExpiryResolver trait IS the store's read
        // facade: both brokers' lookups read the ONE winning date (the
        // disagreement rule keys BOTH lanes on it).
        let store = DayLockedExpiryStore::new();
        let day = NaiveDate::from_ymd_opt(2026, 7, 15).expect("valid");
        assert_eq!(
            store.resolved_expiry(Feed::Dhan, ChainUnderlying::Banknifty, day),
            None
        );
        let date = ExpiryDate::from_yyyymmdd(20_260_728).expect("valid");
        let _ = store.record_policy_date(day, Feed::Groww, ChainUnderlying::Banknifty, date);
        for broker in [Feed::Dhan, Feed::Groww] {
            assert_eq!(
                store.resolved_expiry(broker, ChainUnderlying::Banknifty, day),
                Some(20_260_728)
            );
        }
        // The global handle exists and hands the SAME store to every
        // caller (the future capture-leg delegation seam).
        assert!(Arc::ptr_eq(global_expiry_store(), global_expiry_store()));
    }

    #[test]
    fn test_cadence_expiry_winning_facade_never_serves_yesterdays_winner_across_day_flip() {
        // E1 (verifier, 2026-07-15): Tuesday resolves + day-locks the
        // expiry-day contract; Wednesday morning the vendor is DOWN, so
        // no re-resolution ever lands. The read facade the runner stamps
        // ChainFetchRequest.expiry_yyyymmdd from must return None for
        // Wednesday — NEVER Tuesday's (now expired) winner.
        let store = DayLockedExpiryStore::new();
        let tue = NaiveDate::from_ymd_opt(2026, 7, 16).expect("valid");
        let d = ExpiryDate::from_yyyymmdd(20_260_716).expect("valid");
        let _ = store.record_policy_date(tue, Feed::Dhan, ChainUnderlying::Nifty, d);
        // Same day: the facade serves the locked winner (both brokers —
        // the Dhan-wins rule keys BOTH lanes on it).
        assert_eq!(
            store.resolved_expiry(Feed::Dhan, ChainUnderlying::Nifty, tue),
            Some(20_260_716)
        );
        assert_eq!(store.winning_expiry(tue, ChainUnderlying::Nifty), Some(d));
        // Wednesday: resolution failing (no record_policy_date ever runs
        // for the new day) — the store is still keyed to Tuesday.
        let wed = NaiveDate::from_ymd_opt(2026, 7, 17).expect("valid");
        assert!(!store.is_resolved(wed, Feed::Dhan, ChainUnderlying::Nifty));
        // The day-checked facade must fail closed: None, not the stale
        // expired contract.
        assert_eq!(
            store.winning_expiry(wed, ChainUnderlying::Nifty),
            None,
            "the facade must never serve yesterday's winner across an IST day flip"
        );
        assert_eq!(
            store.resolved_expiry(Feed::Dhan, ChainUnderlying::Nifty, wed),
            None
        );
    }

    #[test]
    fn test_cadence_expiry_page_due_edge_latch_deadline_gates_page_not_attempts() {
        // Before the deadline: never due, resolved or not.
        assert!(!expiry_page_due(32_099, 32_100, false, false));
        // At/past the deadline + unresolved + not yet paged: due ONCE.
        assert!(expiry_page_due(32_100, 32_100, false, false));
        assert!(expiry_page_due(40_000, 32_100, false, false));
        // Already paged this episode: never re-due (edge latch).
        assert!(!expiry_page_due(40_000, 32_100, false, true));
        // Resolved (even late — the deadline gates the PAGE, not the
        // attempts): never due.
        assert!(!expiry_page_due(40_000, 32_100, true, false));
    }

    #[test]
    fn test_cadence_expiry_page_post_deadline_boot_needs_two_failed_waves() {
        // E4 (2026-07-15): a process BOOTING after the deadline (e.g. a
        // crash-boot at 11:00 IST) must observe ≥2 consecutive failed
        // waves before the page fires — never the first-wave hair
        // trigger.
        assert_eq!(POST_DEADLINE_BOOT_MIN_FAILED_WAVES, 2);
        // Post-deadline boot, first failed wave: NOT due.
        assert!(!expiry_page_due_after_wave(
            40_000, 32_100, false, false, true, 1
        ));
        // Second consecutive failed wave: due.
        assert!(expiry_page_due_after_wave(
            40_000, 32_100, false, false, true, 2
        ));
        // Pre-deadline boot path UNCHANGED: the deadline gates the page
        // and one wave suffices.
        assert!(expiry_page_due_after_wave(
            32_100, 32_100, false, false, false, 1
        ));
        // Still never before the deadline, resolved, or already paged.
        assert!(!expiry_page_due_after_wave(
            32_099, 32_100, false, false, true, 9
        ));
        assert!(!expiry_page_due_after_wave(
            40_000, 32_100, true, false, true, 9
        ));
        assert!(!expiry_page_due_after_wave(
            40_000, 32_100, false, true, true, 9
        ));
        // Zero waves never page in either boot mode.
        assert!(!expiry_page_due_after_wave(
            40_000, 32_100, false, false, false, 0
        ));
    }

    // -----------------------------------------------------------------
    // Property tests (operator spec 2026-07-15 — arbitrary date sets)
    // -----------------------------------------------------------------

    /// Strategy: vendor-raw date components (some invalid by
    /// construction — day up to 32, month up to 13) so garbage rides
    /// every generated list.
    fn raw_dates_strategy() -> impl Strategy<Value = Vec<u32>> {
        proptest::collection::vec(
            (2024_u32..2028, 0_u32..14, 0_u32..33).prop_map(|(y, m, d)| y * 10_000 + m * 100 + d),
            0..24,
        )
    }

    /// A always-valid `today` anchor.
    fn today_strategy() -> impl Strategy<Value = u32> {
        (2024_u32..2028, 1_u32..13, 1_u32..29).prop_map(|(y, m, d)| y * 10_000 + m * 100 + d)
    }

    proptest! {
        /// (a) Whenever the BANKNIFTY month policy selects, the selected
        /// date is provably ≥ today, is a VALID date from the input, and
        /// is the LAST valid date of its (year, month) group — and that
        /// group qualifies (holds ≥1 date ≥ today).
        #[test]
        fn proptest_cadence_expiry_banknifty_group_last_is_active_and_group_max(
            dates in raw_dates_strategy(),
            today in today_strategy(),
        ) {
            let got = resolve_policy_expiry(
                ExpiryPolicy::LastExpiryOfNearestActiveMonth,
                &dates,
                today,
            );
            if let Some(sel) = got {
                let raw = sel.yyyymmdd();
                prop_assert!(raw >= today, "selected {raw} < today {today}");
                prop_assert!(dates.contains(&raw), "selected {raw} not in input");
                prop_assert!(ExpiryDate::from_yyyymmdd(raw).is_some());
                let group = raw / 100;
                let group_max = dates
                    .iter()
                    .copied()
                    .filter(|d| ExpiryDate::from_yyyymmdd(*d).is_some())
                    .filter(|d| *d / 100 == group)
                    .max();
                prop_assert_eq!(group_max, Some(raw), "not the group's LAST date");
                prop_assert!(
                    dates
                        .iter()
                        .copied()
                        .filter(|d| ExpiryDate::from_yyyymmdd(*d).is_some())
                        .any(|d| d / 100 == group && d >= today),
                    "selected group does not qualify"
                );
                // NEVER the flat min when the qualifying group holds an
                // earlier active date: min-active ≤ selected always, and
                // strict whenever the group has >1 active member.
                let min_active = dates
                    .iter()
                    .copied()
                    .filter(|d| ExpiryDate::from_yyyymmdd(*d).is_some())
                    .filter(|d| *d >= today)
                    .min();
                prop_assert!(min_active.is_some_and(|m| m <= raw));
            }
        }

        /// (b) An all-past (or all-garbage, or empty) list can never
        /// select — fail-closed None for BOTH policies.
        #[test]
        fn proptest_cadence_expiry_all_past_or_garbage_never_selected(
            dates in raw_dates_strategy(),
            today in today_strategy(),
        ) {
            let all_inactive = !dates
                .iter()
                .copied()
                .filter(|d| ExpiryDate::from_yyyymmdd(*d).is_some())
                .any(|d| d >= today);
            if all_inactive {
                for policy in [
                    ExpiryPolicy::NearestActiveDate,
                    ExpiryPolicy::LastExpiryOfNearestActiveMonth,
                ] {
                    prop_assert_eq!(resolve_policy_expiry(policy, &dates, today), None);
                }
            }
        }

        /// (c) Expiry-day hold-through-close + next-day roll: when the
        /// selected month-group's last date IS today, resolving with
        /// today returns today, and re-resolving with the NEXT calendar
        /// day (the pure-math proxy for the next trading day — the store
        /// owns the trading-day flip) selects a strictly LATER group or
        /// fails closed.
        #[test]
        fn proptest_cadence_expiry_day_holds_then_rolls_next_group(
            dates in raw_dates_strategy(),
            today in today_strategy(),
        ) {
            let with_today: Vec<u32> = dates.iter().copied().chain([today]).collect();
            let sel = resolve_policy_expiry(
                ExpiryPolicy::LastExpiryOfNearestActiveMonth,
                &with_today,
                today,
            )
            .expect("today is in the list — a selection must exist");
            prop_assert!(sel.yyyymmdd() >= today);
            if sel.yyyymmdd() == today {
                // Hold through close: the expiry day still selects today.
                // The NEXT day rolls PAST today's group entirely.
                let tomorrow = ExpiryDate::from_yyyymmdd(today)
                    .expect("valid today")
                    .naive_date()
                    .succ_opt()
                    .and_then(naive_to_yyyymmdd)
                    .expect("valid tomorrow");
                let next = resolve_policy_expiry(
                    ExpiryPolicy::LastExpiryOfNearestActiveMonth,
                    &with_today,
                    tomorrow,
                );
                if let Some(next_sel) = next {
                    prop_assert!(
                        next_sel.yyyymmdd() / 100 > today / 100,
                        "the next day must roll to a LATER group (got {}, today group {})",
                        next_sel.yyyymmdd(),
                        today / 100
                    );
                }
            }
        }

        /// The NIFTY/SENSEX policy is exactly the minimum valid date ≥
        /// today (the nearest active date).
        #[test]
        fn proptest_cadence_expiry_nearest_active_is_min_geq_today(
            dates in raw_dates_strategy(),
            today in today_strategy(),
        ) {
            let expect = dates
                .iter()
                .copied()
                .filter(|d| ExpiryDate::from_yyyymmdd(*d).is_some())
                .filter(|d| *d >= today)
                .min();
            let got = resolve_policy_expiry(ExpiryPolicy::NearestActiveDate, &dates, today)
                .map(ExpiryDate::yyyymmdd);
            prop_assert_eq!(got, expect);
        }
    }
}

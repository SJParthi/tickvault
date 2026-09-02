//! Pressure-triggered partition archival — the *decision*, with no I/O.
//!
//! # Why this exists
//!
//! `partition_archive` already frees disk correctly and fail-closed: export →
//! upload → verify (row count, object size, SHA-256) → audit → drop, with a
//! [`VerifiedArchive`](crate::partition_archive::VerifiedArchive) type-state
//! that makes "drop without a verified S3 copy" unrepresentable rather than
//! merely unlikely.
//!
//! What it does NOT have is a reason to run at the moment it is needed. It is
//! triggered by partition **age** and executes **once, post-market**. At the
//! authorized scale the modelled tick volume fills a 100 GB root in ~16 hours
//! — inside the 2-day minimum eligibility window and hours before the
//! post-market run. So the existing cleanup can never fire in time, and the
//! failure that follows is not "old data lingers": a full volume stops every
//! writer, so **today's capture stops**.
//!
//! This module decides *when* to run that unchanged machinery. It adds a
//! trigger, never a delete path — which is the entire safety argument, because
//! anything the pressure path drops was proven byte-present in S3 by code that
//! predates it and is already ratcheted.
//!
//! # The three rules that shape every branch below
//!
//! 1. **`MIN_HOT_DAYS = 2` is inviolate at any pressure.** The archiver's
//!    verify re-counts rows AFTER the export, which closes the export→count
//!    race; it does not close the count→drop race. On a partition still being
//!    written, a tick landing in that window would be dropped with it. One
//!    lost tick is one too many, so today's and yesterday's partitions are
//!    never eligible — and this module cannot override that clamp even if the
//!    config asks it to.
//! 2. **Pressure never escalates into deletion.** When nothing reclaimable
//!    remains, the loop STOPS and says so loudly ([`PressureAction::Escalate`]
//!    → `STORAGE-GAP-05`, Critical). A system that deletes unarchived data to
//!    save itself has converted a disk problem into a data-loss problem.
//! 3. **Bounded and hysteretic.** Entry at high water, exit strictly below low
//!    water, a cooldown between episodes and a cap on passes — so a volume
//!    hovering at the threshold cannot thrash QuestDB with export queries
//!    during a live session.
//!
//! # Why a pure function
//!
//! Every branch here is reachable in a unit test with no disk, no QuestDB and
//! no S3. The loop that calls it does I/O and nothing else. That split is what
//! makes "does a blind probe trigger a drop?" a question answerable by a test
//! instead of by reading a 200-line async function.

use tickvault_common::config::PartitionRetentionConfig;

/// Hard floor on the hot window, mirrored from
/// [`crate::partition_archive::MIN_HOT_DAYS`].
///
/// Duplicated as a named re-export rather than imported blindly so the
/// invariant is visible at the site that would most like to violate it, and
/// so `pressure_floor_matches_archive_floor` fails the build if the two ever
/// drift apart.
pub const PRESSURE_MIN_HOT_DAYS: u32 = crate::partition_archive::MIN_HOT_DAYS;

/// How long an ESCALATED episode waits before it is allowed one more pass.
///
/// One hour, and the number is not arbitrary: `market_depth` is partitioned
/// BY HOUR, so one hour is exactly the granularity at which a partition that
/// was too young to touch can become eligible. Waiting less would re-export
/// against an unchanged worklist; waiting more would leave reclaimable space
/// on a filling volume.
pub const PRESSURE_REARM_AFTER_SECS: u64 = 3_600;

/// How many times one episode may re-arm after escalating.
///
/// Three, against a ~9-hour session: enough to cover the morning case the
/// re-arm exists for (an episode opening before the first depth hour is
/// eligible), and bounded so a volume that is genuinely unreclaimable stops
/// instead of looping exports against a full disk for the rest of the day.
/// The Critical page fires ONCE either way — a re-arm restores the ability to
/// try, never the ability to page again.
pub const PRESSURE_MAX_REARMS: u32 = 3;

/// One observation of the data volume.
///
/// `used_pct: None` means the probe FAILED (e.g. `df` unavailable or
/// unparseable). It is deliberately a distinct state from "0% used": a blind
/// probe must never be read as pressure, because acting on an unknown is how
/// an automated response deletes data it did not need to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PressureProbe {
    /// Percentage of the volume in use, 0–100.
    pub used_pct: Option<u8>,
}

impl PressureProbe {
    /// A successful probe.
    #[must_use]
    pub const fn used(pct: u8) -> Self {
        Self {
            used_pct: Some(pct),
        }
    }

    /// A failed probe — treated as [`PressureAction::Idle`], never pressure.
    #[must_use]
    pub const fn failed() -> Self {
        Self { used_pct: None }
    }
}

/// Convert a `df` reading into a used-percentage.
///
/// Returns `None` for a zero or nonsensical total, and for `free > total` —
/// both mean the probe told us something impossible, and an impossible reading
/// must land in the same bucket as a failed one rather than being coerced into
/// a plausible-looking number that could trigger a drop.
///
/// Rounds UP, so 74.2% used reads as 75. Under-reporting pressure delays the
/// remediation; over-reporting it by at most one point costs nothing.
#[must_use]
pub fn used_pct_from(free_bytes: u64, total_bytes: u64) -> Option<u8> {
    if total_bytes == 0 || free_bytes > total_bytes {
        return None;
    }
    let used = total_bytes - free_bytes;
    // u128 so the ×100 cannot overflow on a multi-exabyte total.
    let pct = (u128::from(used) * 100).div_ceil(u128::from(total_bytes));
    u8::try_from(pct.min(100)).ok()
}

/// State carried across polls by the caller. Plain data so the decision stays
/// pure and the test can construct any situation directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PressureState {
    /// `Some` while an episode is active.
    pub episode: Option<EpisodeState>,
    /// Monotonic seconds at which the last episode ENDED, for the cooldown.
    pub last_episode_end_secs: Option<u64>,
}

/// The live half of an episode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EpisodeState {
    /// Archive passes already executed in this episode.
    pub passes_used: u32,
    /// Partitions dropped by the MOST RECENT pass. `None` before the first
    /// pass completes.
    pub last_pass_dropped: Option<u64>,
    /// Whether `STORAGE-GAP-05` has already fired for this episode. The latch
    /// is what turns a Critical into one page per episode instead of one per
    /// poll — the difference between an alert and a flood.
    pub escalated: bool,
    /// Monotonic seconds at which this episode escalated, for the re-arm.
    ///
    /// `None` until escalation, and it is what makes the escalation a PAGE
    /// rather than a permanent stand-down — see [`PRESSURE_REARM_AFTER_SECS`].
    pub escalated_at_secs: Option<u64>,
    /// Re-arms already spent in this episode. Bounded by
    /// [`PRESSURE_MAX_REARMS`] so a volume that is genuinely unreclaimable
    /// cannot loop exports forever.
    pub rearms_used: u32,
}

/// What the caller should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressureAction {
    /// Feature off, or the config is self-contradictory. Do nothing, ever.
    Disabled,
    /// Below high water, or the probe failed. Do nothing this poll.
    Idle,
    /// Above high water but too soon after the last episode. Do nothing.
    Cooldown,
    /// Begin an episode and run one archive pass.
    StartEpisode,
    /// Still above low water with passes remaining — run another pass.
    ContinuePass,
    /// Back below low water. Close the episode and start the cooldown.
    EndEpisode,
    /// Nothing reclaimable remains. Fire `STORAGE-GAP-05` (Critical) ONCE and
    /// take no destructive action.
    Escalate,
    /// Already escalated and still above low water. Keep polling, stay quiet,
    /// change nothing.
    Hold,
}

/// True when the thresholds are usable.
///
/// An inverted or out-of-range pair disables the trigger outright instead of
/// oscillating. Silently "fixing" a misconfigured threshold would hide the
/// operator's mistake behind behaviour they did not ask for; refusing is the
/// honest reading, and the caller logs it once at boot.
#[must_use]
pub fn thresholds_are_sane(cfg: &PartitionRetentionConfig) -> bool {
    cfg.pressure_high_water_pct > 0
        && cfg.pressure_high_water_pct <= 100
        && cfg.pressure_low_water_pct < cfg.pressure_high_water_pct
}

/// The hot window an archive pass should use while under pressure.
///
/// Clamped UP to [`PRESSURE_MIN_HOT_DAYS`], so a config of 0 (or 1) cannot
/// reach today's or yesterday's partitions. The clamp lives here as well as in
/// the archiver because defence-in-depth on the one rule whose violation costs
/// a tick is worth the duplication.
#[must_use]
pub fn pressure_hot_days(cfg: &PartitionRetentionConfig) -> u32 {
    cfg.pressure_hot_days.max(PRESSURE_MIN_HOT_DAYS)
}

/// Decide the next action. Pure: no clock, no disk, no network.
///
/// `now_secs` is a monotonic seconds counter supplied by the caller.
#[must_use]
pub fn decide_pressure_action(
    probe: PressureProbe,
    state: &PressureState,
    cfg: &PartitionRetentionConfig,
    now_secs: u64,
) -> PressureAction {
    if !cfg.pressure_archive_enabled || !thresholds_are_sane(cfg) {
        return PressureAction::Disabled;
    }

    // A failed probe is NOT pressure. This is the single most important line
    // in the module: every destructive path downstream is gated on a number
    // we actually measured.
    let Some(used) = probe.used_pct else {
        return PressureAction::Idle;
    };

    match state.episode {
        Some(ep) => {
            // Exit first, so a recovered volume always ends the episode even
            // if it also happens to be out of passes.
            if used < cfg.pressure_low_water_pct {
                return PressureAction::EndEpisode;
            }
            if ep.escalated {
                // RE-ARM, added 2026-09-01 after an adversarial audit found
                // this arm was a permanent stand-down, not a page.
                //
                // The escalate comment below reasons that a zero-reclaim pass
                // means "every partition old enough is already gone, or none
                // could be verified into S3 … neither improves by trying
                // again". The first half of that is FALSE, and it is false in
                // the exact case the pressure archiver exists for: partitions
                // become eligible AS THE CLOCK ADVANCES. `market_depth` is
                // hour-partitioned with a 4-hour floor, so an episode opening
                // at 11:26 IST — which is when the measured 185.7 GB/day depth
                // load crosses 75% of a 300 GB volume — finds the 09:00 hour
                // not yet eligible, drops zero, escalates on its FIRST pass,
                // and then holds for the rest of the session. By 14:00 several
                // hours ARE eligible and nothing goes back to look.
                //
                // The exit condition made it permanent: `used < low_water`
                // requires a pass to free space, and `Hold` runs no pass. So
                // the one automatic mechanism that reclaims disk switched
                // itself off precisely on the morning it was needed.
                //
                // One hour, because that is the granularity at which new
                // partitions can become eligible; bounded by
                // `PRESSURE_MAX_REARMS` so a genuinely unreclaimable volume
                // still stops rather than looping exports. The page already
                // fired and is NOT repeated — `escalated` stays latched for
                // alerting; only the ability to try again is restored.
                let rearm_due = ep
                    .escalated_at_secs
                    .is_some_and(|at| now_secs.saturating_sub(at) >= PRESSURE_REARM_AFTER_SECS);
                if rearm_due && ep.rearms_used < PRESSURE_MAX_REARMS {
                    return PressureAction::ContinuePass;
                }
                return PressureAction::Hold;
            }
            // A pass that reclaimed nothing means one of two things: every
            // partition old enough is already gone, or none could be verified
            // into S3 (so they were correctly kept). Neither improves by
            // trying again, and both need a human — so escalate on the FIRST
            // zero-reclaim pass rather than burning three more exports to
            // reach the same answer slower.
            if ep.last_pass_dropped == Some(0) {
                return PressureAction::Escalate;
            }
            if ep.passes_used >= cfg.pressure_max_passes {
                return PressureAction::Escalate;
            }
            PressureAction::ContinuePass
        }
        None => {
            if used < cfg.pressure_high_water_pct {
                return PressureAction::Idle;
            }
            if let Some(end) = state.last_episode_end_secs
                && now_secs.saturating_sub(end) < cfg.pressure_min_interval_secs
            {
                return PressureAction::Cooldown;
            }
            PressureAction::StartEpisode
        }
    }
}

/// Fold an action back into the state. Kept beside the decision so the two
/// can never disagree about what an action means.
pub fn apply_action(
    state: &mut PressureState,
    action: PressureAction,
    now_secs: u64,
    dropped_this_pass: Option<u64>,
) {
    match action {
        PressureAction::StartEpisode => {
            state.episode = Some(EpisodeState {
                passes_used: 1,
                last_pass_dropped: dropped_this_pass,
                escalated: false,
                escalated_at_secs: None,
                rearms_used: 0,
            });
        }
        PressureAction::ContinuePass => {
            if let Some(ep) = state.episode.as_mut() {
                ep.passes_used = ep.passes_used.saturating_add(1);
                ep.last_pass_dropped = dropped_this_pass;
                // A ContinuePass reached while ALREADY escalated is a re-arm,
                // and it is the only way `rearms_used` can move. Counting it
                // here rather than at the decision keeps the two in lockstep:
                // a decision the caller never executed cannot spend a re-arm.
                if ep.escalated {
                    ep.rearms_used = ep.rearms_used.saturating_add(1);
                    // Re-stamp so the NEXT re-arm is another full hour away
                    // rather than firing on every poll once the first hour has
                    // elapsed. Without this the bound would be spent in three
                    // consecutive polls, which is three exports in three
                    // minutes against a volume that just told us it has
                    // nothing to give.
                    ep.escalated_at_secs = Some(now_secs);
                }
            }
        }
        PressureAction::Escalate => {
            if let Some(ep) = state.episode.as_mut() {
                ep.escalated = true;
                // Stamped ONCE. A second Escalate cannot reach here — the
                // decision returns Hold (or a re-arm) while `escalated` is
                // true — but `get_or_insert` states the intent rather than
                // relying on that, so a future edit to the decision cannot
                // silently reset the re-arm clock and make the wait perpetual.
                ep.escalated_at_secs.get_or_insert(now_secs);
            }
        }
        PressureAction::EndEpisode => {
            state.episode = None;
            state.last_episode_end_secs = Some(now_secs);
        }
        PressureAction::Disabled
        | PressureAction::Idle
        | PressureAction::Cooldown
        | PressureAction::Hold => {}
    }
}

/// What one poll's probe verdict did to the probe-health latch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeHealthTransition {
    /// Same state as last poll: healthy-and-still-healthy, or failing but not
    /// yet for long enough, or already-reported-and-still-failing.
    Unchanged,
    /// The probe has now failed `failing_after` polls in a row. Fired ONCE
    /// per episode — the caller pages on this edge.
    FailingSustained,
    /// A successful probe after a reported sustained failure. Fired ONCE — the
    /// caller may log the recovery.
    Recovered,
}

/// Edge latch for a PERSISTENTLY failing disk-pressure probe (2026-09-02,
/// audit finding 10).
///
/// # The gap this closes
///
/// [`decide_pressure_action`] maps a failed probe to [`PressureAction::Idle`],
/// and that is correct: a blind probe must never trigger a drop. But the
/// consequence is that a probe which fails EVERY poll idles the pressure
/// archiver for the whole session — the one automatic mechanism that reclaims
/// disk switches itself off — and the only trace was a `warn!` per poll plus
/// `tv_disk_pressure_probe_failed_total`, neither of which is a page. A
/// `df` that stops working (an unmounted data volume, a permissions change,
/// a wedged `statvfs`) therefore looked identical to a quiet disk.
///
/// The decision function is deliberately UNTOUCHED — a failed probe still
/// yields `Idle`. This latch sits beside it and answers a different question:
/// "has the probe been blind for long enough that its silence is itself the
/// finding?" Pure: no clock, no disk; the caller supplies the poll verdict
/// and the threshold in polls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProbeHealth {
    /// Consecutive failed polls, reset to 0 by any success.
    consecutive_failures: u32,
    /// Whether `FailingSustained` has been fired for the current episode.
    reported: bool,
}

impl ProbeHealth {
    /// A healthy, never-failed latch.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            consecutive_failures: 0,
            reported: false,
        }
    }

    /// Fold one poll's verdict in and report the edge, if any.
    ///
    /// `failing_after` is the number of CONSECUTIVE failed polls that
    /// constitutes a sustained failure; the caller derives it from its poll
    /// cadence (see `disk_pressure_boot::PRESSURE_PROBE_FAILING_POLLS`). A
    /// threshold of 0 or 1 fires on the very first failure, which a caller
    /// polling once a minute would find noisy — the boot-side constant is
    /// const-asserted to be at least 2.
    #[must_use]
    pub fn observe(&mut self, probe_ok: bool, failing_after: u32) -> ProbeHealthTransition {
        if probe_ok {
            self.consecutive_failures = 0;
            if self.reported {
                self.reported = false;
                return ProbeHealthTransition::Recovered;
            }
            return ProbeHealthTransition::Unchanged;
        }
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if !self.reported && self.consecutive_failures >= failing_after {
            self.reported = true;
            return ProbeHealthTransition::FailingSustained;
        }
        ProbeHealthTransition::Unchanged
    }

    /// Consecutive failed polls so far (for the log payload).
    #[must_use]
    pub const fn consecutive_failures(self) -> u32 {
        self.consecutive_failures
    }
}

#[cfg(test)]
mod probe_health_tests {
    use super::{ProbeHealth, ProbeHealthTransition};

    #[test]
    fn a_healthy_probe_never_transitions() {
        let mut h = ProbeHealth::new();
        for _ in 0..100 {
            assert_eq!(h.observe(true, 5), ProbeHealthTransition::Unchanged);
        }
        assert_eq!(h.consecutive_failures(), 0);
    }

    #[test]
    fn a_failure_shorter_than_the_threshold_is_not_sustained() {
        let mut h = ProbeHealth::new();
        for _ in 0..4 {
            assert_eq!(h.observe(false, 5), ProbeHealthTransition::Unchanged);
        }
        assert_eq!(h.consecutive_failures(), 4);
        // A success before the fifth failure resets the run and is NOT a
        // recovery, because nothing was ever reported.
        assert_eq!(h.observe(true, 5), ProbeHealthTransition::Unchanged);
        assert_eq!(h.consecutive_failures(), 0);
    }

    #[test]
    fn the_threshold_poll_fires_sustained_exactly_once() {
        let mut h = ProbeHealth::new();
        for _ in 0..4 {
            assert_eq!(h.observe(false, 5), ProbeHealthTransition::Unchanged);
        }
        assert_eq!(
            h.observe(false, 5),
            ProbeHealthTransition::FailingSustained,
            "the FIFTH consecutive failure is the edge"
        );
        for _ in 0..1_000 {
            assert_eq!(
                h.observe(false, 5),
                ProbeHealthTransition::Unchanged,
                "a still-failing probe is counted, never re-paged"
            );
        }
    }

    #[test]
    fn a_success_after_a_reported_failure_is_a_recovery_and_re_arms() {
        let mut h = ProbeHealth::new();
        for _ in 0..3 {
            let _ = h.observe(false, 3);
        }
        assert_eq!(h.observe(true, 3), ProbeHealthTransition::Recovered);
        assert_eq!(h.observe(true, 3), ProbeHealthTransition::Unchanged);
        // The next sustained failure is a NEW episode and must page again.
        let _ = h.observe(false, 3);
        let _ = h.observe(false, 3);
        assert_eq!(h.observe(false, 3), ProbeHealthTransition::FailingSustained);
    }

    #[test]
    fn a_failed_probe_still_decides_idle_the_latch_changes_no_action() {
        // The load-bearing property: adding the latch did NOT touch the
        // decision. A blind probe under pressure-enabled config is still Idle.
        use super::{
            PartitionRetentionConfig, PressureProbe, PressureState, decide_pressure_action,
        };
        let cfg = PartitionRetentionConfig {
            pressure_archive_enabled: true,
            pressure_high_water_pct: 75,
            pressure_low_water_pct: 60,
            ..PartitionRetentionConfig::default()
        };
        let mut h = ProbeHealth::new();
        for _ in 0..10 {
            let _ = h.observe(false, 2);
            assert_eq!(
                decide_pressure_action(PressureProbe::failed(), &PressureState::default(), &cfg, 0),
                super::PressureAction::Idle
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> PartitionRetentionConfig {
        PartitionRetentionConfig {
            pressure_archive_enabled: true,
            pressure_high_water_pct: 75,
            pressure_low_water_pct: 60,
            pressure_hot_days: 2,
            pressure_min_interval_secs: 900,
            pressure_max_passes: 4,
            ..PartitionRetentionConfig::default()
        }
    }

    #[test]
    fn disabled_by_default_so_an_absent_config_block_changes_nothing() {
        let c = PartitionRetentionConfig::default();
        assert!(!c.pressure_archive_enabled);
        assert_eq!(
            decide_pressure_action(PressureProbe::used(99), &PressureState::default(), &c, 0),
            PressureAction::Disabled,
            "a volume at 99% must still do nothing when the feature is off"
        );
    }

    #[test]
    fn test_probe_failed_is_idle_never_pressure() {
        // The whole point: we never act on an unknown.
        assert_eq!(
            decide_pressure_action(
                PressureProbe::failed(),
                &PressureState::default(),
                &cfg(),
                0
            ),
            PressureAction::Idle
        );
    }

    #[test]
    fn decide_pressure_action_below_high_water_is_idle() {
        assert_eq!(
            decide_pressure_action(
                PressureProbe::used(74),
                &PressureState::default(),
                &cfg(),
                0
            ),
            PressureAction::Idle
        );
    }

    #[test]
    fn test_probe_used_at_high_water_starts_an_episode() {
        assert_eq!(
            decide_pressure_action(
                PressureProbe::used(75),
                &PressureState::default(),
                &cfg(),
                0
            ),
            PressureAction::StartEpisode,
            "the threshold is inclusive — 'at or above'"
        );
    }

    #[test]
    fn a_recent_episode_end_holds_the_next_one_in_cooldown() {
        let state = PressureState {
            episode: None,
            last_episode_end_secs: Some(1_000),
        };
        assert_eq!(
            decide_pressure_action(PressureProbe::used(90), &state, &cfg(), 1_500),
            PressureAction::Cooldown
        );
        assert_eq!(
            decide_pressure_action(PressureProbe::used(90), &state, &cfg(), 1_900),
            PressureAction::StartEpisode,
            "900s cooldown elapsed"
        );
    }

    /// The Critical found by adversarial audit 2026-09-01: escalation was a
    /// permanent stand-down, not a page.
    ///
    /// Once `escalated`, every later poll returned `Hold`, which runs no pass
    /// — and the only exit is `used < low_water`, which needs a pass to reach.
    /// So the one automatic disk reclaimer switched itself off for the rest of
    /// the session, and it did so preferentially in the MORNING: an episode
    /// opening at ~11:26 IST (where the measured 185.7 GB/day depth load
    /// crosses 75% of 300 GB) finds no hour-partition past the 4-hour floor,
    /// drops zero, and escalates on its FIRST pass. Hours become eligible from
    /// 13:00 onward and nothing went back to look.
    #[test]
    fn an_escalated_episode_rearms_after_an_hour_and_then_stops() {
        let escalated_at = |at: u64, rearms: u32| PressureState {
            episode: Some(EpisodeState {
                passes_used: 1,
                last_pass_dropped: Some(0),
                escalated: true,
                escalated_at_secs: Some(at),
                rearms_used: rearms,
            }),
            last_episode_end_secs: None,
        };

        // Still above low water, one minute after escalating: HOLD. Re-trying
        // immediately would export against a worklist that cannot have changed.
        assert_eq!(
            decide_pressure_action(PressureProbe::used(80), &escalated_at(0, 0), &cfg(), 60),
            PressureAction::Hold,
            "a re-arm before the partition granularity has elapsed is wasted work"
        );

        // One second short of the hour: still HOLD. Pins the boundary rather
        // than the neighbourhood of it.
        assert_eq!(
            decide_pressure_action(
                PressureProbe::used(80),
                &escalated_at(0, 0),
                &cfg(),
                PRESSURE_REARM_AFTER_SECS - 1
            ),
            PressureAction::Hold
        );

        // At the hour: one more pass. THIS is the fix — before it, this poll
        // and every poll after it returned Hold forever.
        assert_eq!(
            decide_pressure_action(
                PressureProbe::used(80),
                &escalated_at(0, 0),
                &cfg(),
                PRESSURE_REARM_AFTER_SECS
            ),
            PressureAction::ContinuePass,
            "hour-partitions become eligible as the clock advances, so a \
             zero-reclaim pass is not proof that the next one reclaims nothing"
        );

        // Bounded: at the cap it goes back to holding, however long it waits.
        // A volume that is genuinely unreclaimable must stop, not loop exports
        // against a full disk for the rest of the day.
        assert_eq!(
            decide_pressure_action(
                PressureProbe::used(80),
                &escalated_at(0, PRESSURE_MAX_REARMS),
                &cfg(),
                PRESSURE_REARM_AFTER_SECS * 10
            ),
            PressureAction::Hold,
            "the re-arm is bounded by PRESSURE_MAX_REARMS"
        );

        // And a recovered volume still ends the episode first, re-arm or not —
        // the exit check runs before the escalation arm and must stay there.
        assert_eq!(
            decide_pressure_action(
                PressureProbe::used(59),
                &escalated_at(0, 0),
                &cfg(),
                PRESSURE_REARM_AFTER_SECS
            ),
            PressureAction::EndEpisode
        );
    }

    /// A re-arm restores the ability to TRY, never the ability to page again,
    /// and it must not fire on every poll once the first hour has elapsed.
    #[test]
    fn a_rearm_spends_its_budget_and_restarts_the_clock() {
        let mut state = PressureState {
            episode: Some(EpisodeState {
                passes_used: 1,
                last_pass_dropped: Some(0),
                escalated: true,
                escalated_at_secs: Some(0),
                rearms_used: 0,
            }),
            last_episode_end_secs: None,
        };

        apply_action(
            &mut state,
            PressureAction::ContinuePass,
            PRESSURE_REARM_AFTER_SECS,
            Some(0),
        );
        let ep = state.episode.expect("episode must survive a re-arm");
        assert_eq!(ep.rearms_used, 1, "the re-arm must be counted");
        assert!(
            ep.escalated,
            "the page latch must STAY latched — a re-arm restores the ability \
             to try, never the ability to page again"
        );
        assert_eq!(
            ep.escalated_at_secs,
            Some(PRESSURE_REARM_AFTER_SECS),
            "the clock must restart, or the whole budget is spent in three \
             consecutive polls — three exports in three minutes against a \
             volume that just said it has nothing to give"
        );

        // Immediately after the re-arm the next poll holds again.
        assert_eq!(
            decide_pressure_action(
                PressureProbe::used(80),
                &state,
                &cfg(),
                PRESSURE_REARM_AFTER_SECS + 60
            ),
            PressureAction::Hold
        );
    }

    #[test]
    fn hysteresis_exit_requires_below_low_water_not_below_high_water() {
        let state = PressureState {
            episode: Some(EpisodeState {
                passes_used: 1,
                last_pass_dropped: Some(10),
                escalated: false,
                ..EpisodeState::default()
            }),
            last_episode_end_secs: None,
        };
        // 70 is below the 75 entry but above the 60 exit: still working.
        assert_eq!(
            decide_pressure_action(PressureProbe::used(70), &state, &cfg(), 0),
            PressureAction::ContinuePass,
            "exiting at the entry threshold would restart on the next poll"
        );
        assert_eq!(
            decide_pressure_action(PressureProbe::used(59), &state, &cfg(), 0),
            PressureAction::EndEpisode
        );
    }

    #[test]
    fn a_pass_that_reclaimed_nothing_escalates_immediately() {
        let state = PressureState {
            episode: Some(EpisodeState {
                passes_used: 1,
                last_pass_dropped: Some(0),
                escalated: false,
                ..EpisodeState::default()
            }),
            last_episode_end_secs: None,
        };
        assert_eq!(
            decide_pressure_action(PressureProbe::used(80), &state, &cfg(), 0),
            PressureAction::Escalate,
            "zero reclaim does not improve by retrying — it needs a human"
        );
    }

    #[test]
    fn exhausted_passes_escalate() {
        let state = PressureState {
            episode: Some(EpisodeState {
                passes_used: 4,
                last_pass_dropped: Some(5),
                escalated: false,
                ..EpisodeState::default()
            }),
            last_episode_end_secs: None,
        };
        assert_eq!(
            decide_pressure_action(PressureProbe::used(80), &state, &cfg(), 0),
            PressureAction::Escalate
        );
    }

    #[test]
    fn escalation_pages_once_then_holds() {
        let state = PressureState {
            episode: Some(EpisodeState {
                passes_used: 4,
                last_pass_dropped: Some(0),
                escalated: true,
                ..EpisodeState::default()
            }),
            last_episode_end_secs: None,
        };
        assert_eq!(
            decide_pressure_action(PressureProbe::used(99), &state, &cfg(), 0),
            PressureAction::Hold,
            "a Critical must page once per episode, not once per poll"
        );
    }

    #[test]
    fn a_recovered_volume_ends_the_episode_even_after_escalation() {
        let state = PressureState {
            episode: Some(EpisodeState {
                passes_used: 4,
                last_pass_dropped: Some(0),
                escalated: true,
                ..EpisodeState::default()
            }),
            last_episode_end_secs: None,
        };
        assert_eq!(
            decide_pressure_action(PressureProbe::used(10), &state, &cfg(), 0),
            PressureAction::EndEpisode,
            "recovery must clear the latch path so a later episode can page again"
        );
    }

    #[test]
    fn thresholds_are_sane_rejects_an_inverted_pair() {
        let mut c = cfg();
        c.pressure_low_water_pct = 80; // above the 75 high water
        assert!(!thresholds_are_sane(&c));
        assert_eq!(
            decide_pressure_action(PressureProbe::used(99), &PressureState::default(), &c, 0),
            PressureAction::Disabled
        );
    }

    #[test]
    fn thresholds_are_sane_rejects_zero_high_water() {
        let mut c = cfg();
        c.pressure_high_water_pct = 0;
        assert!(!thresholds_are_sane(&c));
    }

    #[test]
    fn pressure_hot_days_floor_survives_a_config_asking_for_less() {
        let mut c = cfg();
        c.pressure_hot_days = 0;
        assert_eq!(
            pressure_hot_days(&c),
            PRESSURE_MIN_HOT_DAYS,
            "today and yesterday are untouchable AT ANY PRESSURE"
        );
        c.pressure_hot_days = 1;
        assert_eq!(pressure_hot_days(&c), PRESSURE_MIN_HOT_DAYS);
        c.pressure_hot_days = 7;
        assert_eq!(pressure_hot_days(&c), 7, "a wider window is honoured as-is");
    }

    #[test]
    fn pressure_floor_matches_archive_floor() {
        assert_eq!(
            PRESSURE_MIN_HOT_DAYS,
            crate::partition_archive::MIN_HOT_DAYS,
            "the two floors must never drift — this one exists to be checked"
        );
        assert_eq!(
            PRESSURE_MIN_HOT_DAYS, 2,
            "lowering the floor re-opens the count->drop race on a partition \
             that is still being written; it is not a tuning knob"
        );
    }

    #[test]
    fn used_pct_from_rounds_up_and_refuses_impossible_readings() {
        assert_eq!(used_pct_from(0, 100), Some(100), "nothing free = 100% used");
        assert_eq!(used_pct_from(100, 100), Some(0), "all free = 0% used");
        assert_eq!(used_pct_from(50, 100), Some(50));
        // 74.2% used must read as 75, not 74 — under-reporting delays the fix.
        assert_eq!(used_pct_from(258, 1_000), Some(75));
        // Impossible readings join the failed-probe bucket.
        assert_eq!(used_pct_from(10, 0), None, "zero total is not 0% used");
        assert_eq!(used_pct_from(200, 100), None, "free > total is impossible");
        // No overflow at realistic and absurd volume sizes.
        assert_eq!(
            used_pct_from(50 * 1024 * 1024 * 1024, 200 * 1024 * 1024 * 1024),
            Some(75)
        );
        assert_eq!(used_pct_from(0, u64::MAX), Some(100));
    }

    #[test]
    fn an_impossible_probe_reading_cannot_trigger_an_episode() {
        // The end-to-end shape of the guarantee: a nonsense `df` result must
        // reach the decision as `failed()`, not as a number.
        let probe = match used_pct_from(200, 100) {
            Some(pct) => PressureProbe::used(pct),
            None => PressureProbe::failed(),
        };
        assert_eq!(
            decide_pressure_action(probe, &PressureState::default(), &cfg(), 0),
            PressureAction::Idle
        );
    }

    #[test]
    fn apply_action_walks_a_whole_episode() {
        let c = cfg();
        let mut state = PressureState::default();

        let a = decide_pressure_action(PressureProbe::used(80), &state, &c, 100);
        assert_eq!(a, PressureAction::StartEpisode);
        apply_action(&mut state, a, 100, Some(12));
        assert_eq!(state.episode.expect("episode").passes_used, 1);

        let a = decide_pressure_action(PressureProbe::used(78), &state, &c, 160);
        assert_eq!(a, PressureAction::ContinuePass);
        apply_action(&mut state, a, 160, Some(9));
        assert_eq!(state.episode.expect("episode").passes_used, 2);

        let a = decide_pressure_action(PressureProbe::used(55), &state, &c, 220);
        assert_eq!(a, PressureAction::EndEpisode);
        apply_action(&mut state, a, 220, None);
        assert!(state.episode.is_none());
        assert_eq!(state.last_episode_end_secs, Some(220));

        // And the cooldown now applies to the next spike.
        assert_eq!(
            decide_pressure_action(PressureProbe::used(99), &state, &c, 300),
            PressureAction::Cooldown
        );
    }

    #[test]
    fn escalation_is_recorded_by_apply_action_so_hold_is_reachable() {
        let c = cfg();
        let mut state = PressureState {
            episode: Some(EpisodeState {
                passes_used: 1,
                last_pass_dropped: Some(0),
                escalated: false,
                ..EpisodeState::default()
            }),
            last_episode_end_secs: None,
        };
        let a = decide_pressure_action(PressureProbe::used(88), &state, &c, 0);
        assert_eq!(a, PressureAction::Escalate);
        apply_action(&mut state, a, 0, None);
        assert!(state.episode.expect("episode").escalated);
        assert_eq!(
            decide_pressure_action(PressureProbe::used(88), &state, &c, 10),
            PressureAction::Hold
        );
    }
}

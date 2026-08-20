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
            });
        }
        PressureAction::ContinuePass => {
            if let Some(ep) = state.episode.as_mut() {
                ep.passes_used = ep.passes_used.saturating_add(1);
                ep.last_pass_dropped = dropped_this_pass;
            }
        }
        PressureAction::Escalate => {
            if let Some(ep) = state.episode.as_mut() {
                ep.escalated = true;
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

    #[test]
    fn hysteresis_exit_requires_below_low_water_not_below_high_water() {
        let state = PressureState {
            episode: Some(EpisodeState {
                passes_used: 1,
                last_pass_dropped: Some(10),
                escalated: false,
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

//! The last lever: shed ingest when nothing else can reclaim space.
//!
//! Operator directive 2026-08-19 — the box must manage itself on a fixed
//! instance, with no human in the loop.
//!
//! # Why this exists
//!
//! The disk-pressure loop has exactly ONE lever: shorten retention and archive
//! to cold storage. It works, and it is bounded — retention has a floor
//! (`PRESSURE_MIN_HOT_DAYS`), and once there a pressure pass reclaims nothing.
//! At that point the loop raises a critical alert and deliberately stops
//! deleting, which is the right call: the alternative is deleting data the
//! operator asked never to lose.
//!
//! But stopping there means the disk keeps filling. The missing lever is the
//! other side of the equation — writing LESS rather than deleting more.
//!
//! # The order matters more than the mechanism
//!
//! Reclaim first, shed second, and never shed the thing the system exists to
//! capture. So:
//!
//! | Level | What stops | What survives |
//! |---|---|---|
//! | [`ShedLevel::None`] | nothing | everything |
//! | [`ShedLevel::InlineDepth`] | the 5 order-book levels riding inside every tick packet | ticks, both dedicated depth feeds |
//! | [`ShedLevel::AllDepth`] | every order-book row, from all three sources | **ticks, always** |
//!
//! Inline depth goes first because it is the largest and the most redundant:
//! ten rows per packet across the whole universe, on instruments the dedicated
//! feeds already cover more deeply. Dedicated depth goes second because it is
//! narrow — 255 instruments — and deep, so it is the last order-book data
//! worth keeping.
//!
//! **Ticks are never shed.** There is no level that stops them. A system that
//! discards prices to save space has stopped being the thing it was built to
//! be, and at that point the correct behaviour is to fill the disk loudly and
//! let the operator decide.
//!
//! # Hysteresis, because a flapping shed is worse than either state
//!
//! Shedding and restoring use DIFFERENT thresholds, with a wide gap. A single
//! threshold would toggle every poll while free space hovered around it —
//! producing a dataset with holes at both edges of every oscillation, which is
//! harder to reason about than a clean gap.
//!
//! # Complexity
//!
//! The gate is one `AtomicU8`. Reading it on the hot path is a single relaxed
//! load — O(1), no allocation, no lock, no contention (the writer touches it
//! once a minute at most). The decision function is pure arithmetic over two
//! numbers.
//!
//! # 2026-08-28 — the trigger was measuring the wrong thing
//!
//! Operator directive, on the session that filled the disk and stalled the
//! drain: *"whyt eh fuckign abckpressure is happenign i dont evenw ant to face
//! this fuckign backpressure ... not to lose any single ticks espeicllay to
//! chieve O(1)"*.
//!
//! Everything above this line was already the right SHAPE — it sheds depth and
//! never ticks, which is exactly the priority that directive states. What was
//! wrong is the UNIT. Every threshold above is a fraction of the volume, and
//! that session ended at **55% free** having burned **138 GB**: a
//! healthy-looking disk roughly one session of writes from full, with the 15%
//! shed bar never coming close while the frame ring refused 508,598 times.
//!
//! The failure compounds. Grow the volume and every fractional bar moves
//! FURTHER AWAY in bytes while the daily burn is unchanged, so the most
//! obvious remedy — a bigger disk — disarms the safety net.
//!
//! So a second trigger measured in SESSIONS OF RUNWAY joins the first, and the
//! two combine as a `max`: the more protective of the two, never the less.
//! See [`SESSION_BURN_BYTES_DEFAULT`], which also records why it ships INERT.
//!
//! **What this does NOT do.** It does not stop the disk burning 138 GB a
//! session — depth is 80% of that and untouched here. It does not make the ILP
//! flush faster, and it does not remove backpressure; it arms the existing
//! relief valve early enough to matter instead of after the volume is full.

use std::sync::atomic::{AtomicU8, Ordering};

/// Free-space fraction below which inline depth stops being captured.
pub const SHED_INLINE_DEPTH_BELOW_FREE: f64 = 0.15;

/// Free-space fraction below which ALL depth stops being captured.
pub const SHED_ALL_DEPTH_BELOW_FREE: f64 = 0.08;

/// Free-space fraction above which dedicated depth resumes.
///
/// Well clear of [`SHED_ALL_DEPTH_BELOW_FREE`] so the two cannot chatter.
pub const RESTORE_ALL_DEPTH_ABOVE_FREE: f64 = 0.18;

/// Free-space fraction above which inline depth resumes.
pub const RESTORE_INLINE_DEPTH_ABOVE_FREE: f64 = 0.28;

/// Free-space fraction at which shedding fires REGARDLESS of whether
/// retention has bottomed out.
///
/// Shedding is normally gated on retention having nothing left to give —
/// reclaim first, shed second. This is the escape hatch for the case where
/// the disk fills faster than the pressure loop can walk down its ladder: at
/// 4% free there is no time left to be polite about the ordering.
pub const SHED_REGARDLESS_BELOW_FREE: f64 = 0.04;

/// How much ingest is currently being dropped.
///
/// Ordered by severity and `repr(u8)` so the gate is one atomic. The ordering
/// is load-bearing: escalation compares levels numerically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum ShedLevel {
    /// Everything is being captured.
    None = 0,
    /// The 5 order-book levels inside each tick packet are dropped.
    InlineDepth = 1,
    /// Every order-book row is dropped. Ticks continue.
    AllDepth = 2,
}

impl ShedLevel {
    /// Round-trips from the atomic. An unrecognised byte reads as
    /// [`Self::None`] — a corrupt gate must not silently stop capturing.
    #[must_use]
    pub const fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::InlineDepth,
            2 => Self::AllDepth,
            _ => Self::None,
        }
    }

    /// The stable label for logs and metrics. `&'static str`, so a
    /// `metrics!` label built from it takes the non-allocating arm.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::InlineDepth => "inline_depth",
            Self::AllDepth => "all_depth",
        }
    }

    /// Whether the 5 inline levels are still captured.
    #[must_use]
    pub const fn allows_inline_depth(self) -> bool {
        matches!(self, Self::None)
    }

    /// Whether the dedicated depth feeds are still captured.
    #[must_use]
    pub const fn allows_dedicated_depth(self) -> bool {
        matches!(self, Self::None | Self::InlineDepth)
    }

    /// Whether ticks are captured. Always true, at every level.
    ///
    /// A method rather than an absence, so the guarantee is greppable and a
    /// future level that tried to shed ticks would have to change a function
    /// that says in its name what it is for.
    #[must_use]
    pub const fn allows_ticks(self) -> bool {
        true
    }
}

/// The shared gate the hot path reads and the pressure loop writes.
#[derive(Debug)]
pub struct IngestShedGate {
    level: AtomicU8,
}

impl Default for IngestShedGate {
    fn default() -> Self {
        Self::new()
    }
}

impl IngestShedGate {
    /// A gate shedding nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            level: AtomicU8::new(ShedLevel::None as u8),
        }
    }

    /// The current level. One relaxed load — this is the hot-path read.
    ///
    /// `Relaxed` is correct here and not a shortcut: the value is a single
    /// byte with no other state ordered against it, and a reader that sees
    /// the previous level for a few microseconds captures a few more depth
    /// rows than it strictly needed to. Paying for an `Acquire` on every
    /// packet to tighten that would be the wrong trade.
    #[must_use]
    pub fn level(&self) -> ShedLevel {
        ShedLevel::from_u8(self.level.load(Ordering::Relaxed))
    }

    /// Sets the level. Returns `true` when it actually changed, so the caller
    /// logs an edge rather than repeating itself every poll.
    pub fn set(&self, level: ShedLevel) -> bool {
        let prev = self.level.swap(level as u8, Ordering::Relaxed);
        prev != level as u8
    }

    /// Convenience for the hot path.
    #[must_use]
    pub fn allows_inline_depth(&self) -> bool {
        self.level().allows_inline_depth()
    }

    /// Convenience for the depth routing.
    #[must_use]
    pub fn allows_dedicated_depth(&self) -> bool {
        self.level().allows_dedicated_depth()
    }
}

/// The one gate for this process.
///
/// A `static` rather than an `Arc` threaded through the drain: the resource
/// being defended is the machine's disk, which is process-global, so the gate
/// defending it is too. Threading an `Arc` through six signatures to reach the
/// same single byte would buy nothing and would put a pointer chase on the
/// per-packet path. `IngestShedGate::new` is `const`, so this needs no lazy
/// initialiser, no lock, and no `unsafe`.
///
/// One writer (the disk-pressure loop, once a minute at most), many readers
/// (the drain, per packet). That is the access pattern an atomic is for.
pub static INGEST_SHED: IngestShedGate = IngestShedGate::new();

/// Converts the disk-pressure loop's percent-USED reading into the free
/// fraction this module decides on.
///
/// The loop measures used; every threshold here is expressed as free, because
/// "how much room is left" is the question shedding answers. Keeping the
/// conversion in one named function stops the two conventions being mixed at
/// a call site, which would invert every threshold silently.
///
/// `used_pct` is rounded UP by the probe, so this rounds free DOWN — the
/// conservative direction for a decision about running out of space.
#[must_use]
pub fn free_fraction_from_used_pct(used_pct: u8) -> f64 {
    let used = f64::from(used_pct.min(100));
    (100.0 - used) / 100.0
}

/// Decides the shed level from the current one and what the disk looks like.
///
/// `retention_at_floor` is whether the disk-pressure loop has already walked
/// retention to its minimum and has nothing left to reclaim. Shedding is
/// normally gated on it — reclaim first, shed second — with
/// [`SHED_REGARDLESS_BELOW_FREE`] as the escape hatch for a disk filling
/// faster than the ladder can be walked.
///
/// Restoring is NOT gated on retention: once space is comfortable again there
/// is no reason to keep dropping data, whatever the retention window is doing.
#[must_use]
pub fn decide_shed_level(
    current: ShedLevel,
    free_fraction: f64,
    retention_at_floor: bool,
) -> ShedLevel {
    // A non-finite or out-of-range measurement is not evidence. Hold the
    // current level rather than acting on a number that cannot be true —
    // shedding on garbage loses data, and restoring on garbage fills a disk.
    if !free_fraction.is_finite() || !(0.0..=1.0).contains(&free_fraction) {
        return current;
    }

    // Restore first, so a recovered disk cannot be re-shed in the same call
    // by a threshold it no longer meets.
    if free_fraction >= RESTORE_INLINE_DEPTH_ABOVE_FREE {
        return ShedLevel::None;
    }
    if free_fraction >= RESTORE_ALL_DEPTH_ABOVE_FREE && current == ShedLevel::AllDepth {
        return ShedLevel::InlineDepth;
    }

    let may_shed = retention_at_floor || free_fraction < SHED_REGARDLESS_BELOW_FREE;
    if !may_shed {
        return current;
    }

    if free_fraction < SHED_ALL_DEPTH_BELOW_FREE {
        return ShedLevel::AllDepth;
    }
    if free_fraction < SHED_INLINE_DEPTH_BELOW_FREE {
        // `max` so a call at this threshold can never DE-escalate a gate
        // already at AllDepth. De-escalation belongs to the restore
        // thresholds above, which have the hysteresis gap.
        return current.max(ShedLevel::InlineDepth);
    }
    current
}

// ---------------------------------------------------------------------------
// Runway trigger — the same decision measured in SESSIONS, not percentages
// ---------------------------------------------------------------------------

/// One session's disk burn, in bytes, or `0` to leave the runway trigger inert.
///
/// # Why a fraction is the wrong unit for this
///
/// Every threshold above is a fraction of the volume, and that is a trap this
/// system walked into on 2026-08-28. Measured that session:
///
/// | | |
/// |---|---:|
/// | free at open | 314 GB |
/// | free at close | 176 GB |
/// | burned | **138 GB** |
/// | free at close, as a fraction | **55%** |
/// | `SHED_INLINE_DEPTH_BELOW_FREE` | 15% |
///
/// So the box spent the day at 55% free and the shed gate never armed — while
/// the frame ring refused 508,598 times and the drain stalled behind ILP
/// timeouts. The gate was not wrong; it was measuring the wrong thing. Free
/// space as a FRACTION says how full the disk is. What decides whether the
/// next session survives is free space divided by what a session BURNS, and
/// those two numbers move independently.
///
/// The consequence is worse than a missed trigger. Grow the volume and every
/// fractional threshold moves FURTHER AWAY in bytes while the daily burn is
/// unchanged — so buying a bigger disk makes the safety net arm LATER. A net
/// that is disarmed by the most obvious remedy is not a net.
///
/// # Why it defaults to inert
///
/// `0` disables the runway trigger entirely, and that is the shipped default.
/// Turning it on changes what the box CAPTURES — depth would be shed on an
/// ordinary busy afternoon rather than only on a nearly-full disk — and that
/// is an operator decision about data, not a bug fix. The mechanism ships
/// ready; the number that arms it is config.
///
/// Set it to the MEASURED burn of a real session, not to an estimate. The
/// 2026-08-28 figure is 138 GB.
pub const SESSION_BURN_BYTES_DEFAULT: u64 = 0;

/// Sessions of runway below which inline depth stops being captured.
///
/// 1.5 rather than 1.0: at exactly one session of runway the disk fills at
/// close, which is too late to help. On 2026-08-28 a 1.5 bar sits at
/// ~207 GB free, which the box crossed at roughly 14:00 IST — the hour the
/// ring-full count first became significant (35,982, rising to 189,332 in the
/// hour after). Arming there trades the afternoon's inline depth for the
/// afternoon's ticks, which is the trade this module's doctrine already makes:
/// ticks are never shed, depth is.
pub const SHED_INLINE_DEPTH_BELOW_RUNWAY: f64 = 1.5;

/// Sessions of runway below which ALL depth stops being captured.
pub const SHED_ALL_DEPTH_BELOW_RUNWAY: f64 = 1.0;

/// Sessions of runway above which inline depth resumes.
///
/// Clear of the shed bar by a wide margin for the same reason the fractional
/// thresholds are: a single bar toggles every poll while the measurement
/// hovers, producing holes at both edges of every oscillation.
pub const RESTORE_INLINE_DEPTH_ABOVE_RUNWAY: f64 = 2.0;

/// Sessions of runway above which dedicated depth resumes.
pub const RESTORE_ALL_DEPTH_ABOVE_RUNWAY: f64 = 1.75;

/// How many more sessions this much free space can absorb.
///
/// `None` when the burn is unknown (`0`) or the arithmetic cannot be trusted,
/// which is the same thing as "do not act on this" — an unknown runway must
/// never shed and must never restore.
#[must_use]
pub fn runway_sessions(free_bytes: u64, session_burn_bytes: u64) -> Option<f64> {
    if session_burn_bytes == 0 {
        return None;
    }
    // Precision: an f64 holds every integer below 2^53, i.e. 9 PB of bytes.
    // A volume that large would break far more than this function.
    #[allow(clippy::cast_precision_loss)] // APPROVED: see the note above.
    let runway = free_bytes as f64 / session_burn_bytes as f64;
    if runway.is_finite() {
        Some(runway)
    } else {
        None
    }
}

/// [`decide_shed_level`], plus the runway trigger.
///
/// The two triggers are combined so the result is the MORE protective of the
/// two, never the less. That ordering is the whole safety argument: adding a
/// second trigger must not be able to make the gate quieter than it was, or a
/// change meant to catch more would silently catch less.
///
/// With `session_burn_bytes == 0` the runway half is inert and this returns
/// exactly what [`decide_shed_level`] returns — which is why enabling the
/// feature is a config change and not a code change.
///
/// # Restoring needs BOTH halves
///
/// Because the combination is a `max`, a restore only lands when the fraction
/// AND the runway both permit it. That is not a side effect worth tidying: a
/// comfortable percentage on a volume with half a session of runway left is
/// not comfortable, and restoring on the fraction alone would undo the runway
/// trigger on its very next poll.
#[must_use]
pub fn decide_shed_level_with_runway(
    current: ShedLevel,
    free_fraction: f64,
    free_bytes: u64,
    session_burn_bytes: u64,
    retention_at_floor: bool,
) -> ShedLevel {
    let by_fraction = decide_shed_level(current, free_fraction, retention_at_floor);

    let Some(runway) = runway_sessions(free_bytes, session_burn_bytes) else {
        return by_fraction;
    };

    by_fraction.max(decide_shed_level_by_runway(current, runway))
}

/// The runway half of [`decide_shed_level_with_runway`], in isolation.
///
/// Deliberately the same SHAPE as [`decide_shed_level`] — restore first, then
/// shed, with `max` on the middle rung so a call at that threshold can never
/// de-escalate a gate already at [`ShedLevel::AllDepth`]. Two triggers that
/// decided differently would be two bugs to be found separately; one shape,
/// reviewed once, is the point.
///
/// Two deliberate differences from the fractional path, each load-bearing:
///
/// 1. **No retention gate.** Retention cannot help here by construction: its
///    floor is one day, and one day IS one session's writes — so waiting for
///    retention to give up first would mean waiting for the disk to be full.
/// 2. **No escape hatch.** [`SHED_REGARDLESS_BELOW_FREE`] exists because a
///    fraction can fall faster than the retention ladder is walked. A runway
///    is already expressed in the unit that matters, so there is nothing for
///    an escape hatch to escape.
fn decide_shed_level_by_runway(current: ShedLevel, runway: f64) -> ShedLevel {
    // Restore first, so a recovered runway cannot be re-shed in the same call
    // by a threshold it no longer meets.
    if runway >= RESTORE_INLINE_DEPTH_ABOVE_RUNWAY {
        return ShedLevel::None;
    }
    if runway >= RESTORE_ALL_DEPTH_ABOVE_RUNWAY && current == ShedLevel::AllDepth {
        return ShedLevel::InlineDepth;
    }

    if runway < SHED_ALL_DEPTH_BELOW_RUNWAY {
        return ShedLevel::AllDepth;
    }
    if runway < SHED_INLINE_DEPTH_BELOW_RUNWAY {
        return current.max(ShedLevel::InlineDepth);
    }
    current
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shed_level_from_u8_reads_a_corrupt_byte_as_capturing_everything() {
        assert_eq!(ShedLevel::from_u8(0), ShedLevel::None);
        assert_eq!(ShedLevel::from_u8(1), ShedLevel::InlineDepth);
        assert_eq!(ShedLevel::from_u8(2), ShedLevel::AllDepth);
        // A corrupt gate must fail toward CAPTURING, never toward dropping —
        // a byte nobody wrote must not silently stop the order book.
        assert_eq!(ShedLevel::from_u8(3), ShedLevel::None);
        assert_eq!(ShedLevel::from_u8(255), ShedLevel::None);
    }

    #[test]
    fn allows_ticks_is_true_at_every_level_that_exists() {
        // The guarantee that makes shedding acceptable at all. If a future
        // level ever sheds ticks, this test is where it has to be argued.
        for level in [ShedLevel::None, ShedLevel::InlineDepth, ShedLevel::AllDepth] {
            assert!(level.allows_ticks(), "{level:?} must never shed ticks");
        }
    }

    #[test]
    fn allows_inline_depth_goes_first_because_it_is_widest_and_most_redundant() {
        assert!(ShedLevel::None.allows_inline_depth());
        // The FIRST thing shed: 10 rows per packet across the whole universe,
        // on instruments the dedicated feeds already cover more deeply.
        assert!(!ShedLevel::InlineDepth.allows_inline_depth());
        assert!(!ShedLevel::AllDepth.allows_inline_depth());
    }

    #[test]
    fn allows_dedicated_depth_survives_the_first_level_and_dies_at_the_second() {
        assert!(ShedLevel::None.allows_dedicated_depth());
        // Narrow (255 instruments) and deep, so it is the LAST order-book
        // data worth keeping — it must still be captured at the level that
        // sheds inline depth, or the escalation order has inverted.
        assert!(ShedLevel::InlineDepth.allows_dedicated_depth());
        assert!(!ShedLevel::AllDepth.allows_dedicated_depth());
    }

    #[test]
    fn decide_shed_level_does_nothing_while_retention_can_still_reclaim() {
        // Reclaim first, shed second. At 10% free with retention still able
        // to give, deleting old data is the better lever and this must not
        // pre-empt it.
        assert_eq!(
            decide_shed_level(ShedLevel::None, 0.10, false),
            ShedLevel::None
        );
    }

    #[test]
    fn decide_shed_level_sheds_once_retention_has_nothing_left() {
        assert_eq!(
            decide_shed_level(ShedLevel::None, 0.10, true),
            ShedLevel::InlineDepth
        );
        assert_eq!(
            decide_shed_level(ShedLevel::InlineDepth, 0.05, true),
            ShedLevel::AllDepth
        );
    }

    #[test]
    fn decide_shed_level_sheds_regardless_when_the_disk_is_nearly_gone() {
        // The escape hatch: a disk filling faster than the retention ladder
        // can be walked leaves no time to be polite about ordering.
        assert_eq!(
            decide_shed_level(ShedLevel::None, 0.03, false),
            ShedLevel::AllDepth
        );
    }

    #[test]
    fn decide_shed_level_restores_in_reverse_order_with_a_gap() {
        // Dedicated depth comes back first, inline second — the reverse of
        // the shed order, so the narrow deep data returns before the wide
        // redundant data.
        assert_eq!(
            decide_shed_level(ShedLevel::AllDepth, 0.20, true),
            ShedLevel::InlineDepth
        );
        assert_eq!(
            decide_shed_level(ShedLevel::InlineDepth, 0.30, true),
            ShedLevel::None
        );
    }

    #[test]
    fn decide_shed_level_restores_even_while_retention_is_still_at_its_floor() {
        // Retention gates SHEDDING, not restoring. Once space is comfortable
        // there is no reason to keep dropping data, whatever the window is
        // doing.
        assert_eq!(
            decide_shed_level(ShedLevel::AllDepth, 0.50, true),
            ShedLevel::None
        );
    }

    #[test]
    fn decide_shed_level_does_not_flap_across_a_single_threshold() {
        // The property the two threshold sets exist for. Walk free space
        // slowly up and down through the shed band; the level must change at
        // most once per direction, never oscillate.
        let mut level = ShedLevel::None;
        let mut transitions = 0;
        // Fall from comfortable to critical.
        for step in 0..=40 {
            let free = 0.40 - f64::from(step) * 0.01;
            let next = decide_shed_level(level, free, true);
            if next != level {
                transitions += 1;
                level = next;
            }
        }
        assert_eq!(level, ShedLevel::AllDepth);
        assert_eq!(transitions, 2, "None -> InlineDepth -> AllDepth, no more");

        // And back up.
        transitions = 0;
        for step in 0..=40 {
            let free = f64::from(step) * 0.01;
            let next = decide_shed_level(level, free, true);
            if next != level {
                transitions += 1;
                level = next;
            }
        }
        assert_eq!(level, ShedLevel::None);
        assert_eq!(transitions, 2, "AllDepth -> InlineDepth -> None, no more");
    }

    #[test]
    fn decide_shed_level_never_de_escalates_inside_the_shed_band() {
        // Between the shed thresholds and the restore thresholds the level
        // must HOLD. A call at 0.10 with the gate already at AllDepth must
        // not drop it back to InlineDepth — that is what the restore band is
        // for, and doing it here would reopen the flapping this design closes.
        assert_eq!(
            decide_shed_level(ShedLevel::AllDepth, 0.10, true),
            ShedLevel::AllDepth
        );
        assert_eq!(
            decide_shed_level(ShedLevel::AllDepth, 0.16, true),
            ShedLevel::AllDepth
        );
    }

    #[test]
    fn decide_shed_level_holds_on_a_measurement_that_cannot_be_true() {
        // Shedding on garbage loses data; restoring on garbage fills a disk.
        // Holding does neither.
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.5, 1.5] {
            assert_eq!(
                decide_shed_level(ShedLevel::InlineDepth, bad, true),
                ShedLevel::InlineDepth,
                "free_fraction {bad} must not move the level"
            );
            assert_eq!(
                decide_shed_level(ShedLevel::None, bad, true),
                ShedLevel::None
            );
        }
    }

    #[test]
    fn the_thresholds_leave_a_real_gap_in_both_directions() {
        // If a shed threshold ever met or crossed its restore partner, the
        // hysteresis is gone and the level chatters every poll.
        assert!(
            RESTORE_ALL_DEPTH_ABOVE_FREE > SHED_ALL_DEPTH_BELOW_FREE,
            "dedicated depth would chatter"
        );
        assert!(
            RESTORE_INLINE_DEPTH_ABOVE_FREE > SHED_INLINE_DEPTH_BELOW_FREE,
            "inline depth would chatter"
        );
        assert!(
            SHED_ALL_DEPTH_BELOW_FREE < SHED_INLINE_DEPTH_BELOW_FREE,
            "all-depth must shed at a TIGHTER disk than inline, or the \
             escalation order inverts"
        );
        assert!(
            SHED_REGARDLESS_BELOW_FREE < SHED_ALL_DEPTH_BELOW_FREE,
            "the escape hatch must be the last threshold, not the first"
        );
    }

    #[test]
    fn level_and_set_round_trip_and_report_only_real_edges() {
        let gate = IngestShedGate::new();
        assert_eq!(gate.level(), ShedLevel::None);
        assert!(gate.allows_inline_depth());
        assert!(gate.allows_dedicated_depth());

        assert!(gate.set(ShedLevel::InlineDepth), "a change is an edge");
        assert!(
            !gate.set(ShedLevel::InlineDepth),
            "setting the same level twice is NOT an edge — otherwise the \
             pressure loop logs the same line every minute for hours"
        );
        assert_eq!(gate.level(), ShedLevel::InlineDepth);
        assert!(!gate.allows_inline_depth());
        assert!(gate.allows_dedicated_depth());

        assert!(gate.set(ShedLevel::AllDepth));
        assert!(!gate.allows_dedicated_depth());
        assert!(gate.set(ShedLevel::None));
        assert!(gate.allows_inline_depth());
    }

    #[test]
    fn as_str_labels_are_static_so_a_metric_label_does_not_allocate() {
        // A `metrics!` macro given a non-literal String label takes its
        // ALLOCATING arm — the exact bug that cost ~36M allocations/hour in
        // `record_ws_lag`. These are `&'static str` by construction.
        let labels: [&'static str; 3] = [
            ShedLevel::None.as_str(),
            ShedLevel::InlineDepth.as_str(),
            ShedLevel::AllDepth.as_str(),
        ];
        assert_eq!(labels, ["none", "inline_depth", "all_depth"]);
    }

    #[test]
    fn free_fraction_from_used_pct_maps_the_probes_units_onto_this_modules() {
        assert!((free_fraction_from_used_pct(0) - 1.0).abs() < f64::EPSILON);
        assert!((free_fraction_from_used_pct(100) - 0.0).abs() < f64::EPSILON);
        assert!((free_fraction_from_used_pct(75) - 0.25).abs() < 1e-12);
        // The probe cannot report above 100, but an impossible reading must
        // clamp rather than produce a NEGATIVE free fraction, which
        // `decide_shed_level` would then reject as garbage and hold on —
        // silently declining to shed on the fullest possible disk.
        assert!((free_fraction_from_used_pct(255) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn free_fraction_from_used_pct_lands_on_the_right_side_of_every_threshold() {
        // The conversion and the thresholds have to agree, or the loop sheds
        // at a disk level nobody chose. Walk the real percentages.
        assert_eq!(
            decide_shed_level(ShedLevel::None, free_fraction_from_used_pct(80), true),
            ShedLevel::None,
            "20% free is above the inline shed threshold"
        );
        assert_eq!(
            decide_shed_level(ShedLevel::None, free_fraction_from_used_pct(90), true),
            ShedLevel::InlineDepth,
            "10% free sheds inline depth"
        );
        assert_eq!(
            decide_shed_level(
                ShedLevel::InlineDepth,
                free_fraction_from_used_pct(95),
                true
            ),
            ShedLevel::AllDepth,
            "5% free sheds all depth"
        );
        assert_eq!(
            decide_shed_level(ShedLevel::AllDepth, free_fraction_from_used_pct(70), true),
            ShedLevel::None,
            "30% free restores everything"
        );
    }

    #[test]
    fn ingest_shed_static_starts_open_and_round_trips() {
        // The process-global gate the drain reads. It must START capturing —
        // a gate that defaulted to shedding would drop depth on every boot
        // until the first pressure poll ran.
        assert_eq!(INGEST_SHED.level(), ShedLevel::None);
        assert!(INGEST_SHED.allows_inline_depth());
        assert!(INGEST_SHED.allows_dedicated_depth());

        INGEST_SHED.set(ShedLevel::AllDepth);
        assert!(!INGEST_SHED.allows_dedicated_depth());
        // Restore it: this static is shared by every test in the binary, so
        // leaving it shed would be a cross-test dependency.
        INGEST_SHED.set(ShedLevel::None);
        assert!(INGEST_SHED.allows_inline_depth());
    }

    // -----------------------------------------------------------------------
    // Runway trigger
    // -----------------------------------------------------------------------

    /// The measured 2026-08-28 session: 138 GB burned between open and close.
    const BURN_2026_08_28: u64 = 148_000_000_000;

    #[test]
    fn runway_is_none_when_the_burn_is_unknown() {
        // Zero is the shipped default and it means "nobody has measured this
        // box yet". An unmeasured burn must never shed and must never
        // restore — acting on a number nobody supplied is how a safety net
        // becomes a data-loss mechanism.
        assert_eq!(runway_sessions(200_000_000_000, 0), None);
    }

    #[test]
    fn runway_divides_free_space_by_one_session_of_writes() {
        // 296 GB of free space against a 148 GB session is exactly 2.
        let runway = runway_sessions(296_000_000_000, BURN_2026_08_28)
            .expect("a measured burn has a runway");
        assert!(
            (runway - 2.0).abs() < 1e-9,
            "expected 2 sessions of runway, got {runway}"
        );
    }

    #[test]
    fn a_zero_burn_makes_the_runway_path_byte_identical_to_the_fraction_path() {
        // This is the shipped configuration, so it is the case that must be
        // provably inert: turning the feature ON is an operator decision, and
        // until they take it the new code path may not change one verdict.
        for &current in &[ShedLevel::None, ShedLevel::InlineDepth, ShedLevel::AllDepth] {
            for &free_fraction in &[0.0, 0.03, 0.05, 0.10, 0.15, 0.20, 0.28, 0.55, 1.0] {
                for &at_floor in &[false, true] {
                    // Free bytes deliberately varied too: with a zero burn it
                    // must not matter what they are.
                    for &free_bytes in &[0_u64, 1, 200_000_000_000] {
                        assert_eq!(
                            decide_shed_level_with_runway(
                                current,
                                free_fraction,
                                free_bytes,
                                0,
                                at_floor,
                            ),
                            decide_shed_level(current, free_fraction, at_floor),
                            "runway path drifted from the fraction path at \
                             current={current:?} free_fraction={free_fraction} \
                             free_bytes={free_bytes} at_floor={at_floor}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_2026_08_28_session_sheds_on_runway_where_the_fraction_never_armed() {
        // The whole reason this trigger exists. That session closed at 176 GB
        // free — 55% of the volume, nowhere near the 15% fractional bar — yet
        // barely one session of writes from a full disk.
        let free_at_close = 176_000_000_000;
        let fraction_at_close = 0.55;

        assert_eq!(
            decide_shed_level(ShedLevel::None, fraction_at_close, true),
            ShedLevel::None,
            "the fractional gate did not arm that day; if this ever changes, \
             the premise of the runway trigger has changed with it"
        );

        assert_eq!(
            decide_shed_level_with_runway(
                ShedLevel::None,
                fraction_at_close,
                free_at_close,
                BURN_2026_08_28,
                true,
            ),
            ShedLevel::InlineDepth,
            "1.19 sessions of runway must shed inline depth"
        );
    }

    #[test]
    fn under_one_session_of_runway_sheds_every_order_book_row() {
        // 74 GB against a 148 GB session: half a session left. Ticks still
        // survive — there is no level that stops them, which is the doctrine
        // this module opens with.
        let level = decide_shed_level_with_runway(
            ShedLevel::None,
            0.30,
            74_000_000_000,
            BURN_2026_08_28,
            false,
        );
        assert_eq!(level, ShedLevel::AllDepth);
        assert!(level.allows_ticks(), "ticks are never shed, at any level");
    }

    #[test]
    fn runway_sheds_without_waiting_for_retention_to_give_up() {
        // The fractional path is gated on `retention_at_floor` — reclaim
        // first, shed second. The runway path deliberately is not: retention's
        // floor is one day, and one day IS one session's writes, so waiting
        // for it would mean waiting for the disk to be full.
        assert_eq!(
            decide_shed_level_with_runway(
                ShedLevel::None,
                0.40,
                74_000_000_000,
                BURN_2026_08_28,
                false, // retention has NOT bottomed out
            ),
            ShedLevel::AllDepth
        );
    }

    #[test]
    fn a_comfortable_percentage_cannot_restore_while_the_runway_is_short() {
        // 55% free reads as a healthy disk to the fractional path, which would
        // restore all the way to None. Half a session of runway says otherwise,
        // and the more protective of the two wins.
        assert_eq!(
            decide_shed_level_with_runway(
                ShedLevel::AllDepth,
                0.55,
                74_000_000_000,
                BURN_2026_08_28,
                true,
            ),
            ShedLevel::AllDepth
        );
    }

    #[test]
    fn runway_restores_in_reverse_order_with_a_gap() {
        // 1.8 sessions: past the 1.75 bar that returns dedicated depth, short
        // of the 2.0 bar that returns inline depth. A partial restore.
        assert_eq!(
            decide_shed_level_with_runway(
                ShedLevel::AllDepth,
                0.55,
                266_400_000_000, // 1.8 sessions
                BURN_2026_08_28,
                true,
            ),
            ShedLevel::InlineDepth
        );

        // 2.1 sessions clears both bars, so everything comes back.
        assert_eq!(
            decide_shed_level_with_runway(
                ShedLevel::InlineDepth,
                0.55,
                310_800_000_000,
                BURN_2026_08_28,
                true,
            ),
            ShedLevel::None
        );
    }

    #[test]
    fn a_short_runway_cannot_be_dragged_down_by_a_healthy_fraction() {
        // The `max` combination, stated as a property: adding the runway
        // trigger may only ever make the gate MORE protective. If this can
        // fail, a change meant to catch more is silently catching less.
        for &current in &[ShedLevel::None, ShedLevel::InlineDepth, ShedLevel::AllDepth] {
            for &free_fraction in &[0.0, 0.05, 0.10, 0.20, 0.55, 1.0] {
                for &at_floor in &[false, true] {
                    for &free_bytes in &[0_u64, 74_000_000_000, 296_000_000_000] {
                        let with = decide_shed_level_with_runway(
                            current,
                            free_fraction,
                            free_bytes,
                            BURN_2026_08_28,
                            at_floor,
                        );
                        let without = decide_shed_level(current, free_fraction, at_floor);
                        assert!(
                            with >= without,
                            "runway made the gate QUIETER at current={current:?} \
                             free_fraction={free_fraction} free_bytes={free_bytes} \
                             at_floor={at_floor}: {with:?} < {without:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn an_empty_volume_with_a_measured_burn_restores_everything() {
        // The far end: nothing shed, nothing to shed, and the runway trigger
        // must not invent a reason to act.
        assert_eq!(
            decide_shed_level_with_runway(ShedLevel::None, 1.0, u64::MAX, BURN_2026_08_28, false),
            ShedLevel::None
        );
    }

    #[test]
    fn a_garbage_fraction_still_holds_even_with_a_healthy_runway() {
        // `decide_shed_level` refuses to act on a non-finite measurement. The
        // runway half must not launder that refusal into a restore.
        assert_eq!(
            decide_shed_level_with_runway(
                ShedLevel::AllDepth,
                f64::NAN,
                296_000_000_000,
                BURN_2026_08_28,
                true,
            ),
            ShedLevel::AllDepth,
            "a NaN fraction must not restore a shed gate"
        );
    }
}

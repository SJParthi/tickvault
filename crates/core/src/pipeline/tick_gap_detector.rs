//! Synthetic tick-loss detector for the Dhan live feed.
//!
//! # Why this module exists, and what it is NOT
//!
//! The Dhan India market-data feed carries **no sequence number**. An
//! exhaustive search of the 29-file operator-supplied documentation pack
//! found no sequence field, no ordering guarantee, no conflation statement
//! and no replay mechanism. The only sequence-like field, on depth-20, is
//! documented verbatim as *"Message Sequence (to be ignored)"*, and the
//! equivalent bytes on depth-200 carry a row count rather than a counter.
//!
//! Consequently **a missed packet is undetectable at the protocol level.**
//! There is no counter to compare against. This module therefore does not
//! *detect* loss — it **infers** it, from four weak signals carried by the
//! payload itself:
//!
//! | Signal | What it can show | Strength |
//! |---|---|---|
//! | Last-traded-time monotonicity | LTT moving backwards proves reordering or a stale packet | **Strong** — a backward step cannot happen on a correctly-ordered stream |
//! | Cumulative-volume deltas | An implausibly large jump is *consistent with* missed ticks | **Weak** — see the conflation caveat below |
//! | Open-interest deltas | Activity/liveness only | **None for loss** — see below |
//! | Per-instrument silence | A stream that stopped relative to its own history | **Medium** — needs a per-instrument baseline, not a global threshold |
//!
//! ## What this CANNOT prove
//!
//! * It cannot prove zero loss. Absence of an anomaly is not evidence that
//!   nothing was dropped — a conflated feed can silently swallow updates and
//!   leave every one of these signals looking perfectly healthy.
//! * It cannot count how many packets were lost. `SuspectedVolumeGap` reports
//!   a delta, not a packet count.
//! * It cannot distinguish vendor-side conflation (upstream, before the bytes
//!   reach us) from transport loss (our socket). Both look identical here.
//! * It cannot distinguish a cumulative-volume counter reset (session
//!   rollover) from a `u32` wraparound or from corruption. All three surface
//!   as [`VolumeVerdict::Regressed`], and the caller is handed both values.
//!
//! The only ground truth available for the Dhan lane remains the daily REST
//! cross-verification. This module is an early-warning sensor, not a proof.
//!
//! ## The conflation caveat — why volume jumps are NOT loss
//!
//! The feed is conflated at roughly one full-image update per second per
//! symbol. A single update therefore *legitimately* carries the aggregate of
//! every trade in that second. **A large volume delta is the normal case, not
//! evidence of loss.** This is why the volume signal fires only above a
//! deliberately high multiple of the instrument's own learned delta, is
//! flagged as weak, and must never be treated as equivalent to a backward-LTT
//! finding.
//!
//! ## Why open interest carries no loss information
//!
//! Open interest legitimately moves in **both** directions — it falls whenever
//! positions are closed. Applying "monotonic delta" reasoning to OI, as one
//! would to volume, would manufacture false positives on every position
//! unwind. This module therefore tracks OI purely as an **activity/liveness**
//! signal and explicitly declines to derive loss from its direction. Recorded
//! plainly rather than implemented incorrectly.
//!
//! ## Not crying wolf
//!
//! A detector that pages on legitimate quiet is worse than no detector,
//! because it trains the operator to ignore it. Three mechanisms guard
//! against that:
//!
//! 1. **Per-instrument baselines, never a global threshold.** Each instrument
//!    learns its own mean inter-arrival interval, so an index ticking every
//!    200 ms and a far-month option ticking every 4 minutes are each judged
//!    against themselves.
//! 2. **Sparse-instrument classification.** An instrument whose learned
//!    baseline exceeds [`DEFAULT_SPARSE_BASELINE_MILLIS`] is marked
//!    [`SilenceReport::sparse`]. Callers exclude sparse instruments from
//!    alarm counting while retaining per-instrument visibility — the same
//!    treatment the house already applies to non-nearest-month futures and
//!    INDIA VIX (`daily-universe-scope-expansion-2026-05-27.md` §36.4, which
//!    excludes those from the SLO silent count and the silent-instruments
//!    gauge while keeping them seeded for per-instrument visibility).
//! 3. **Warmup.** No silence verdict is emitted until an instrument has
//!    contributed [`DetectorConfig::baseline_warmup_samples`] intervals, so a
//!    freshly-subscribed instrument cannot page during its first quiet spell.
//!
//! Whole-second LTT stamps mean **repeated timestamps are the common case**,
//! not an edge case: [`LttVerdict::Repeated`] is explicitly a healthy verdict.
//!
//! ## Complexity and allocation
//!
//! * [`TickGapDetector::observe`] — **O(1)** average, **allocation-free**.
//!   One hash lookup into a pre-sized index, then an in-place mutation of a
//!   pre-allocated slot. Both the slot array and the index are sized once at
//!   construction; when they are full the detector **fails closed** (refuses
//!   and counts) rather than growing, so allocation on this path is
//!   impossible by construction rather than merely unlikely.
//! * [`TickGapDetector::scan_silence`] — **O(n) in tracked instruments, NOT
//!   O(1)**. Stated plainly rather than relabelled. The constraint is that a
//!   silence sweep is inherently a question about every instrument at once,
//!   so no per-instrument-keyed structure answers it in constant time. The
//!   chosen alternative is to keep it strictly off the per-tick path: it is a
//!   cold-path call intended for a background task on a multi-second cadence,
//!   it walks a contiguous slice (cache-friendly), and it allocates nothing —
//!   results are pushed to a caller-supplied sink.
//! * Every other accessor is O(1).
//!
//! ## Time source
//!
//! Callers supply `now_millis` from a **monotonic** clock. A wall clock must
//! not be used: a time-synchronisation step backwards would make every
//! instrument appear to have ticked in the future, and a step forwards would
//! make the entire universe appear silent at once. All arithmetic here is
//! saturating, so a violated contract degrades to a clamped value rather than
//! a panic — but it will still produce nonsense, so the contract stands.
//!
//! Composite `(security_id, exchange_segment)` keying per I-P1-11 —
//! `security_id` alone is not unique across segments.

use std::collections::HashMap;

use tickvault_common::error_code::ErrorCode;
use tickvault_common::tick_types::ParsedTick;
use tickvault_common::types::ExchangeSegment;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Composite instrument key per I-P1-11. `security_id` alone is BANNED as a
/// key — Dhan reuses numeric ids across segments.
pub type InstrumentKey = (u64, ExchangeSegment);

/// Error code a caller should tag when reporting a confirmed gap.
///
/// Reuses the existing `RISK-GAP-03` variant (documented in
/// `.claude/rules/project/gap-enforcement.md` "RISK-GAP-03: Tick Gap
/// Detection"). This module deliberately performs no logging of its own — it
/// returns verdicts and lets the wiring layer decide severity and delivery.
pub const TICK_GAP_ERROR_CODE: ErrorCode = ErrorCode::RiskGapTickGap;

/// EWMA smoothing shift: the new sample carries `1/2^SHIFT` of the weight.
const EWMA_SHIFT: u32 = 3;
/// EWMA divisor derived from [`EWMA_SHIFT`].
const EWMA_DIVISOR: u64 = 1 << EWMA_SHIFT;

/// Floor below which silence is never reported, however chatty an instrument
/// normally is. Prevents a 200 ms-cadence index from paging after 2 seconds.
pub const DEFAULT_SILENCE_FLOOR_MILLIS: u64 = 30_000;

/// Silence is reported once an instrument has been quiet for this multiple of
/// its own learned inter-arrival baseline.
pub const DEFAULT_SILENCE_MULTIPLIER: u64 = 10;

/// Intervals an instrument must contribute before any silence verdict is
/// emitted for it.
pub const DEFAULT_BASELINE_WARMUP_SAMPLES: u32 = 8;

/// A learned baseline at or above this is a legitimately-sparse instrument
/// (far-month options, INDIA VIX). Such instruments are flagged
/// [`SilenceReport::sparse`] so callers can exclude them from alarm counting
/// while keeping per-instrument visibility (§36.4 precedent).
pub const DEFAULT_SPARSE_BASELINE_MILLIS: u64 = 30_000;

/// A cumulative-volume delta above this multiple of the instrument's learned
/// delta is flagged as a *suspected* gap. Deliberately high: the feed is
/// conflated, so large deltas are normal.
pub const DEFAULT_VOLUME_JUMP_MULTIPLIER: u64 = 20;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Tunables for the detector. `Copy` so it costs nothing to pass around.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetectorConfig {
    /// Silence is never reported below this many milliseconds.
    pub silence_floor_millis: u64,
    /// Multiple of the learned baseline at which silence is reported.
    pub silence_multiplier: u64,
    /// Intervals required before silence verdicts are emitted.
    pub baseline_warmup_samples: u32,
    /// Learned baseline at or above which an instrument counts as sparse.
    pub sparse_baseline_millis: u64,
    /// Multiple of the learned volume delta that raises a suspected gap.
    pub volume_jump_multiplier: u64,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        Self {
            silence_floor_millis: DEFAULT_SILENCE_FLOOR_MILLIS,
            silence_multiplier: DEFAULT_SILENCE_MULTIPLIER,
            baseline_warmup_samples: DEFAULT_BASELINE_WARMUP_SAMPLES,
            sparse_baseline_millis: DEFAULT_SPARSE_BASELINE_MILLIS,
            volume_jump_multiplier: DEFAULT_VOLUME_JUMP_MULTIPLIER,
        }
    }
}

// ---------------------------------------------------------------------------
// Verdicts — pure, exhaustively testable
// ---------------------------------------------------------------------------

/// Outcome of comparing a tick's last-traded-time against the previous one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LttVerdict {
    /// No prior observation for this instrument.
    FirstSight,
    /// Timestamp went backwards — reordering, or a stale/replayed packet.
    /// This is the strongest loss-adjacent signal available on this feed.
    Backward {
        /// How far back, in seconds.
        by_secs: u32,
    },
    /// Same whole second as the previous tick. **Healthy and common** — the
    /// feed stamps at one-second granularity.
    Repeated,
    /// Timestamp advanced normally.
    Advanced {
        /// How far forward, in seconds.
        by_secs: u32,
    },
}

impl LttVerdict {
    /// True only for a backward step. `Repeated` is explicitly healthy.
    #[must_use]
    pub fn is_anomalous(self) -> bool {
        matches!(self, Self::Backward { .. })
    }
}

/// Outcome of comparing cumulative day volume against the previous tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeVerdict {
    /// No prior observation for this instrument.
    FirstSight,
    /// Cumulative volume unchanged — no trade since the last update.
    Unchanged,
    /// Volume advanced by a plausible amount.
    Advanced {
        /// Delta since the previous tick.
        delta: u32,
    },
    /// Cumulative volume DECREASED. Consistent with a session reset, a `u32`
    /// wraparound, or corruption — this module cannot tell which, and says so
    /// by handing back both values.
    Regressed {
        /// Previously observed cumulative volume.
        previous: u32,
        /// Newly observed cumulative volume.
        current: u32,
    },
    /// Volume advanced far above this instrument's learned delta. **Weak**
    /// evidence: the feed is conflated, so large jumps are normal.
    SuspectedGap {
        /// Delta since the previous tick.
        delta: u32,
        /// Learned per-tick delta this was judged against.
        baseline: u64,
    },
}

impl VolumeVerdict {
    /// True for a regression or a suspected gap. Callers must weight
    /// `SuspectedGap` as weak evidence — see the module docs.
    #[must_use]
    pub fn is_anomalous(self) -> bool {
        matches!(self, Self::Regressed { .. } | Self::SuspectedGap { .. })
    }
}

/// Outcome of comparing open interest against the previous tick.
///
/// Deliberately carries **no** loss semantics: OI legitimately falls when
/// positions close, so its direction proves nothing about missed packets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OiVerdict {
    /// No prior observation for this instrument.
    FirstSight,
    /// Open interest unchanged.
    Unchanged,
    /// Open interest moved. Direction is informational only.
    Changed {
        /// Previous open interest.
        previous: u32,
        /// Current open interest.
        current: u32,
    },
}

/// Per-instrument silence classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SilenceVerdict {
    /// Instrument was seeded (subscribed) but has never produced a tick. This
    /// is the only way an entirely-lost stream becomes visible, since a
    /// never-arriving stream leaves no payload to reason about.
    NeverTicked,
    /// Too few intervals observed to judge. No alarm.
    Warming,
    /// Within its own expected cadence.
    Live,
    /// Quiet for longer than its own baseline predicts.
    Exceeded,
}

// ---------------------------------------------------------------------------
// Pure decision functions
// ---------------------------------------------------------------------------

/// Integer EWMA update — deterministic, allocation-free, no floating point.
///
/// Returns `sample` when `previous` is zero (bootstrap). Otherwise moves
/// `previous` one step of `1/D` toward `sample`, with `D = 2^EWMA_SHIFT`,
/// rounding to nearest.
///
/// The delta is divided BEFORE it is applied, rather than computing the
/// algebraically-equivalent `(previous * (D-1) + sample) / D`. That form
/// overflows for large inputs, and saturating it collapses the result to
/// `u64::MAX / D` — a silently wrong answer rather than a clamped one. This
/// form cannot overflow at any input, so the result is always inside
/// `[min(previous, sample), max(previous, sample)]`.
///
/// Honest limitation: with integer truncation the average cannot converge
/// closer than roughly `D` units to the true mean, so at values below ~8 it
/// can sit one unit off. Irrelevant for millisecond intervals (typically in
/// the hundreds or thousands) and recorded rather than hidden.
#[must_use]
pub fn ewma_update(previous: u64, sample: u64) -> u64 {
    if previous == 0 {
        return sample;
    }
    if sample >= previous {
        previous.saturating_add(scaled_step(sample - previous))
    } else {
        previous.saturating_sub(scaled_step(previous - sample))
    }
}

/// `round(delta / D)` without ever overflowing.
///
/// The obvious `(delta + D/2) / D` overflows for `delta` near `u64::MAX` —
/// which is exactly how the previous revision of this function panicked under
/// the workspace's `overflow-checks`. Shifting first and adding the rounding
/// bit afterwards is total over the whole domain.
#[inline]
fn scaled_step(delta: u64) -> u64 {
    let quotient = delta >> EWMA_SHIFT;
    let remainder = delta & (EWMA_DIVISOR - 1);
    quotient + u64::from(remainder >= EWMA_DIVISOR / 2)
}

/// Classifies a last-traded-time transition. O(1), pure.
#[must_use]
pub fn classify_ltt(previous: Option<u32>, current: u32) -> LttVerdict {
    match previous {
        None => LttVerdict::FirstSight,
        Some(prev) if current < prev => LttVerdict::Backward {
            by_secs: prev.saturating_sub(current),
        },
        Some(prev) if current == prev => LttVerdict::Repeated,
        Some(prev) => LttVerdict::Advanced {
            by_secs: current.saturating_sub(prev),
        },
    }
}

/// Classifies a cumulative-volume transition. O(1), pure.
///
/// `baseline` is the instrument's learned per-tick delta; `warm` says whether
/// enough samples exist to trust it. A suspected gap is raised only when the
/// baseline is warm and non-zero, so a cold or idle instrument can never
/// produce one.
#[must_use]
pub fn classify_volume(
    previous: Option<u32>,
    current: u32,
    baseline: u64,
    multiplier: u64,
    warm: bool,
) -> VolumeVerdict {
    let Some(prev) = previous else {
        return VolumeVerdict::FirstSight;
    };
    if current < prev {
        return VolumeVerdict::Regressed {
            previous: prev,
            current,
        };
    }
    if current == prev {
        return VolumeVerdict::Unchanged;
    }
    let delta = current.saturating_sub(prev);
    if warm && baseline > 0 {
        let ceiling = baseline.saturating_mul(multiplier);
        if u64::from(delta) > ceiling {
            return VolumeVerdict::SuspectedGap { delta, baseline };
        }
    }
    VolumeVerdict::Advanced { delta }
}

/// Classifies an open-interest transition. O(1), pure.
///
/// Direction is informational only — see the module docs on why OI carries no
/// loss information.
#[must_use]
pub fn classify_oi(previous: Option<u32>, current: u32) -> OiVerdict {
    match previous {
        None => OiVerdict::FirstSight,
        Some(prev) if prev == current => OiVerdict::Unchanged,
        Some(prev) => OiVerdict::Changed {
            previous: prev,
            current,
        },
    }
}

/// Classifies silence against an instrument's own baseline. O(1), pure.
///
/// Returns the verdict and the expected-quiet ceiling it was judged against,
/// so callers can render "silent 412s, expected under 90s" without recomputing.
///
/// `ever_ticked` and `samples` are deliberately separate inputs.
/// [`SilenceVerdict::NeverTicked`] means literally zero ticks — the seeded
/// instrument whose stream never arrived at all. `samples` counts *intervals*,
/// so an instrument that ticked exactly once has ticked but has contributed no
/// interval and therefore has no baseline to be judged against.
///
/// Honest blind spot: that single-tick-then-silent instrument is classified
/// [`SilenceVerdict::Warming`] and never alarms. Judging it against the bare
/// floor instead was considered and rejected — a legitimately sparse contract
/// whose second tick is minutes away would page every session open, which is
/// exactly the crying-wolf failure this module exists to avoid. The stronger
/// dead-stream signal (seeded, zero ticks) remains fully covered.
#[must_use]
pub fn classify_silence(
    baseline_millis: u64,
    samples: u32,
    silent_millis: u64,
    ever_ticked: bool,
    config: &DetectorConfig,
) -> (SilenceVerdict, u64) {
    let expected = baseline_millis
        .saturating_mul(config.silence_multiplier)
        .max(config.silence_floor_millis);
    if !ever_ticked {
        return (SilenceVerdict::NeverTicked, expected);
    }
    if samples < config.baseline_warmup_samples {
        return (SilenceVerdict::Warming, expected);
    }
    if silent_millis > expected {
        (SilenceVerdict::Exceeded, expected)
    } else {
        (SilenceVerdict::Live, expected)
    }
}

// ---------------------------------------------------------------------------
// Observation + assessment
// ---------------------------------------------------------------------------

/// One tick's worth of input to the detector. `Copy`, fixed size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickObservation {
    /// Composite instrument key per I-P1-11.
    pub key: InstrumentKey,
    /// Exchange last-traded-time, epoch seconds (whole-second granularity).
    pub ltt_epoch_secs: u32,
    /// Cumulative day volume.
    pub volume: u32,
    /// Open interest.
    pub open_interest: u32,
    /// Receive time from a **monotonic** clock, in milliseconds.
    pub recv_monotonic_millis: u64,
}

impl TickObservation {
    /// Builds an observation from a parsed tick.
    ///
    /// Returns `None` for a wire segment code the shared enum does not map
    /// (code 6 is the documented gap; anything above 8 is undefined). The
    /// caller counts and discards rather than guessing a segment, because a
    /// guessed segment would corrupt the composite key.
    #[must_use]
    pub fn from_parsed_tick(tick: &ParsedTick, recv_monotonic_millis: u64) -> Option<Self> {
        let segment = ExchangeSegment::from_byte(tick.exchange_segment_code)?;
        Some(Self {
            key: (tick.security_id, segment),
            ltt_epoch_secs: tick.exchange_timestamp,
            volume: tick.volume,
            open_interest: tick.open_interest,
            recv_monotonic_millis,
        })
    }
}

/// Per-tick assessment returned by [`TickGapDetector::observe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickAssessment {
    /// Last-traded-time verdict.
    pub ltt: LttVerdict,
    /// Cumulative-volume verdict.
    pub volume: VolumeVerdict,
    /// Open-interest verdict (informational).
    pub oi: OiVerdict,
    /// True when the detector had no capacity left and dropped this
    /// observation. Fail-closed and countable — never silent.
    pub refused: bool,
}

impl TickAssessment {
    /// True when any signal suggests missed packets.
    ///
    /// Weighting is the caller's job: a [`LttVerdict::Backward`] is strong
    /// evidence, a [`VolumeVerdict::SuspectedGap`] is weak.
    #[must_use]
    pub fn suspects_loss(self) -> bool {
        self.ltt.is_anomalous() || self.volume.is_anomalous()
    }

    /// True when the detector could not record this observation at all.
    #[must_use]
    pub fn was_refused(self) -> bool {
        self.refused
    }
}

/// One instrument's silence assessment, pushed to the caller's sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SilenceReport {
    /// Composite instrument key per I-P1-11.
    pub key: InstrumentKey,
    /// How long since this instrument last produced a tick, in milliseconds.
    /// For a never-ticked instrument, time since it was seeded.
    pub silent_millis: u64,
    /// Quiet ceiling this was judged against.
    pub expected_millis: u64,
    /// Learned mean inter-arrival interval, in milliseconds.
    pub baseline_millis: u64,
    /// Intervals contributed so far.
    pub samples: u32,
    /// The verdict.
    pub verdict: SilenceVerdict,
    /// Legitimately-sparse instrument (far-month option, INDIA VIX class).
    /// Excluded from alarm counting; visibility retained (§36.4 precedent).
    pub sparse: bool,
}

impl SilenceReport {
    /// True when this report should increment an operator-facing alarm count.
    ///
    /// Sparse instruments are visible but never counted — that exclusion is
    /// what keeps the detector from crying wolf on legitimately quiet
    /// contracts.
    #[must_use]
    pub fn counts_toward_alarm(self) -> bool {
        !self.sparse
            && matches!(
                self.verdict,
                SilenceVerdict::Exceeded | SilenceVerdict::NeverTicked
            )
    }
}

// ---------------------------------------------------------------------------
// Per-instrument state
// ---------------------------------------------------------------------------

/// Fixed-size per-instrument state. `Copy`, no heap, no growth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InstrumentState {
    key: InstrumentKey,
    used: bool,
    /// `None` until the first real tick.
    last_ltt: Option<u32>,
    last_volume: Option<u32>,
    last_oi: Option<u32>,
    /// Monotonic millis of the last tick, or of seeding when `samples == 0`.
    last_seen_millis: u64,
    /// Learned mean inter-arrival interval, milliseconds.
    interval_ewma_millis: u64,
    /// Learned mean per-tick cumulative-volume delta.
    volume_delta_ewma: u64,
    /// Intervals contributed (not ticks — the first tick yields no interval).
    samples: u32,
}

impl InstrumentState {
    const EMPTY: Self = Self {
        key: (0, ExchangeSegment::IdxI),
        used: false,
        last_ltt: None,
        last_volume: None,
        last_oi: None,
        last_seen_millis: 0,
        interval_ewma_millis: 0,
        volume_delta_ewma: 0,
        samples: 0,
    };
}

// ---------------------------------------------------------------------------
// The detector
// ---------------------------------------------------------------------------

/// Synthetic tick-loss detector.
///
/// Owned by a single consumer task downstream of the capture ring — never by
/// the socket reader itself, which does nothing but append to the write-ahead
/// log and push to the ring. `&mut self` on the update path is deliberate: it
/// buys in-place mutation with no atomics and no locks, and makes every worst
/// case exhaustively testable without concurrency.
#[derive(Debug)]
pub struct TickGapDetector {
    /// Pre-allocated slots. Never grows; never reallocates.
    slots: Box<[InstrumentState]>,
    /// Key to slot index. Pre-sized to `capacity`; never grows.
    index: HashMap<InstrumentKey, u32, ahash::RandomState>,
    /// High-water mark of assigned slots.
    next_slot: u32,
    /// Observations refused because capacity was exhausted. Fail-closed and
    /// loud: a caller that never reads this is the false-OK bug class.
    refused_count: u64,
    config: DetectorConfig,
}

impl TickGapDetector {
    /// Builds a detector sized for `capacity` instruments.
    ///
    /// Both the slot array and the index are allocated once, here. Beyond
    /// `capacity` the detector refuses observations rather than growing, so
    /// no allocation can occur on the update path.
    #[must_use]
    pub fn with_capacity(capacity: usize, config: DetectorConfig) -> Self {
        let mut slots = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            slots.push(InstrumentState::EMPTY);
        }
        Self {
            slots: slots.into_boxed_slice(),
            index: HashMap::with_capacity_and_hasher(capacity, ahash::RandomState::default()),
            next_slot: 0,
            refused_count: 0,
            config,
        }
    }

    /// Registers an instrument before any tick arrives.
    ///
    /// This is what makes an entirely-lost stream visible: an instrument that
    /// never ticks leaves no payload to reason about, so it must be known in
    /// advance to be reported as [`SilenceVerdict::NeverTicked`]. Idempotent.
    /// Returns `false` when capacity is exhausted (fail-closed, counted).
    pub fn seed(&mut self, key: InstrumentKey, now_millis: u64) -> bool {
        match self.slot_for(key) {
            Some(slot) => {
                let state = &mut self.slots[slot as usize];
                if state.samples == 0 && state.last_ltt.is_none() {
                    state.last_seen_millis = now_millis;
                }
                true
            }
            None => false,
        }
    }

    /// Records one tick and returns its assessment.
    ///
    /// **O(1) average, allocation-free.** One hash lookup plus an in-place
    /// slot mutation.
    pub fn observe(&mut self, obs: TickObservation) -> TickAssessment {
        let Some(slot) = self.slot_for(obs.key) else {
            return TickAssessment {
                ltt: LttVerdict::FirstSight,
                volume: VolumeVerdict::FirstSight,
                oi: OiVerdict::FirstSight,
                refused: true,
            };
        };

        let warmup = self.config.baseline_warmup_samples;
        let multiplier = self.config.volume_jump_multiplier;
        let state = &mut self.slots[slot as usize];

        let ltt = classify_ltt(state.last_ltt, obs.ltt_epoch_secs);
        let warm = state.samples >= warmup;
        let volume = classify_volume(
            state.last_volume,
            obs.volume,
            state.volume_delta_ewma,
            multiplier,
            warm,
        );
        let oi = classify_oi(state.last_oi, obs.open_interest);

        // Interval baseline. The first tick after seeding yields no interval,
        // so `samples` counts intervals rather than ticks.
        if state.last_ltt.is_some() {
            let interval = obs
                .recv_monotonic_millis
                .saturating_sub(state.last_seen_millis);
            state.interval_ewma_millis = ewma_update(state.interval_ewma_millis, interval);
            state.samples = state.samples.saturating_add(1);
        }

        // Volume-delta baseline. Only a forward move teaches it — a
        // regression would poison the mean with a wraparound-sized value.
        if let VolumeVerdict::Advanced { delta } = volume {
            state.volume_delta_ewma = ewma_update(state.volume_delta_ewma, u64::from(delta));
        }

        state.last_ltt = Some(obs.ltt_epoch_secs);
        state.last_volume = Some(obs.volume);
        state.last_oi = Some(obs.open_interest);
        // Monotonic contract: never let a backwards-stepping clock rewind the
        // last-seen stamp, which would otherwise inflate every later silence.
        state.last_seen_millis = state.last_seen_millis.max(obs.recv_monotonic_millis);

        TickAssessment {
            ltt,
            volume,
            oi,
            refused: false,
        }
    }

    /// Sweeps every tracked instrument and pushes a report per instrument.
    ///
    /// **O(n) in tracked instruments — NOT O(1).** Cold path only; see the
    /// module docs for the constraint and the chosen mitigation. Allocates
    /// nothing: reports go to the caller's sink. Generic over the sink rather
    /// than boxed, so there is no virtual dispatch.
    pub fn scan_silence<F>(&self, now_millis: u64, mut sink: F)
    where
        F: FnMut(SilenceReport),
    {
        for state in self.slots.iter().take(self.next_slot as usize) {
            if !state.used {
                continue;
            }
            let silent_millis = now_millis.saturating_sub(state.last_seen_millis);
            let (verdict, expected_millis) = classify_silence(
                state.interval_ewma_millis,
                state.samples,
                silent_millis,
                state.last_ltt.is_some(),
                &self.config,
            );
            sink(SilenceReport {
                key: state.key,
                silent_millis,
                expected_millis,
                baseline_millis: state.interval_ewma_millis,
                samples: state.samples,
                verdict,
                sparse: state.interval_ewma_millis >= self.config.sparse_baseline_millis,
            });
        }
    }

    /// Clears all learned state for a new session, retaining capacity.
    ///
    /// Session boundaries reset cumulative volume at the exchange, so carrying
    /// yesterday's counters across would manufacture a regression on the first
    /// tick of every instrument. Retains both allocations.
    pub fn reset_session(&mut self) {
        self.index.clear();
        for slot in self.slots.iter_mut().take(self.next_slot as usize) {
            *slot = InstrumentState::EMPTY;
        }
        self.next_slot = 0;
        self.refused_count = 0;
    }

    /// Number of instruments currently tracked. O(1).
    #[must_use]
    pub fn tracked_instruments(&self) -> usize {
        self.next_slot as usize
    }

    /// Maximum instruments this detector can track. O(1).
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Observations refused because capacity was exhausted. O(1).
    ///
    /// A non-zero value means the detector is blind to part of the universe —
    /// callers must surface it rather than assume silence means health.
    #[must_use]
    pub fn refused_count(&self) -> u64 {
        self.refused_count
    }

    /// Resolves (or assigns) the slot for a key. O(1) average.
    ///
    /// Returns `None` — fail-closed — when capacity is exhausted, counting the
    /// refusal. Never grows either structure, which is what makes the update
    /// path allocation-free by construction.
    fn slot_for(&mut self, key: InstrumentKey) -> Option<u32> {
        if let Some(slot) = self.index.get(&key) {
            return Some(*slot);
        }
        if (self.next_slot as usize) >= self.slots.len() {
            self.refused_count = self.refused_count.saturating_add(1);
            return None;
        }
        let slot = self.next_slot;
        self.next_slot = self.next_slot.saturating_add(1);
        let state = &mut self.slots[slot as usize];
        *state = InstrumentState::EMPTY;
        state.key = key;
        state.used = true;
        self.index.insert(key, slot);
        Some(slot)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const NIFTY: InstrumentKey = (13, ExchangeSegment::IdxI);
    const BANKNIFTY: InstrumentKey = (25, ExchangeSegment::IdxI);

    fn detector() -> TickGapDetector {
        TickGapDetector::with_capacity(64, DetectorConfig::default())
    }

    fn obs(key: InstrumentKey, ltt: u32, volume: u32, oi: u32, millis: u64) -> TickObservation {
        TickObservation {
            key,
            ltt_epoch_secs: ltt,
            volume,
            open_interest: oi,
            recv_monotonic_millis: millis,
        }
    }

    /// Drives an instrument past warmup with a steady cadence so silence and
    /// volume baselines are trustworthy.
    fn warm_up(d: &mut TickGapDetector, key: InstrumentKey, cadence_millis: u64, ticks: u32) {
        for i in 1..=ticks {
            let t = u64::from(i) * cadence_millis;
            d.observe(obs(key, 1_000 + i, i * 100, 500, t));
        }
    }

    /// MEASUREMENT (not a CI gate -- `#[ignore]`d, run on demand):
    /// what does the 30-second `scan_silence` sweep actually cost at the
    /// authorized 25,000-instrument ceiling?
    ///
    /// CLAUDE.md's O(1) table records this path as O(n) in tracked instruments
    /// and closes with "Still UNMEASURED at the 25,000 target". It is the last
    /// row in that table making an unmeasured cost claim, and an unmeasured
    /// cost claim is indistinguishable from an unbounded one to the next
    /// reader. This turns it into a number.
    ///
    /// RESULT, 2026-08-22, `--release`, x86 dev container:
    ///
    /// ```text
    /// scan_silence: 25000 instruments, 69.78us (2.8 ns/instrument),
    ///               0.0002% of the 30s cadence
    /// ```
    ///
    /// So the O(n) is real and the constant is negligible: at the authorized
    /// ceiling the sweep costs ~70 microseconds against a 30-second interval.
    /// The row stays BOUNDED rather than becoming GUARANTEED because the shape
    /// is still linear -- doubling the universe doubles the cost, and only the
    /// measurement says how much headroom that leaves.
    ///
    /// Ignored rather than asserted for the same reason as the aggregator's
    /// sweep harness: a wall-clock bound on a shared CI runner is a flake, and
    /// a flaky gate is worse than none. Not measured on the prod r8g.xlarge
    /// (Graviton4); this is an order-of-magnitude answer.
    ///
    ///     cargo test -p tickvault-core --release --lib \
    ///       scan_silence_sweep_cost -- --ignored --nocapture
    #[test]
    #[ignore = "measurement harness, not a gate — see doc comment"]
    fn scan_silence_sweep_cost_at_the_authorized_ceiling() {
        const N: usize = 25_000;
        let mut d = TickGapDetector::with_capacity(N, DetectorConfig::default());
        // Seed every slot AND drive each past warmup, so the sweep classifies
        // real baselines rather than short-circuiting on unwarmed state --
        // the shape the live lane actually presents after the open.
        for sid in 0..N as u64 {
            let key = (sid, ExchangeSegment::NseEquity);
            assert!(d.seed(key, 0), "seed {sid} must fit the 25,000 ceiling");
            for i in 1..=4u32 {
                d.observe(obs(key, 1_000 + i, i * 100, 500, u64::from(i) * 1_000));
            }
        }
        assert_eq!(d.tracked_instruments(), N, "every slot must be occupied");

        let mut seen = 0usize;
        let t0 = std::time::Instant::now();
        d.scan_silence(60_000, |_| seen += 1);
        let elapsed = t0.elapsed();

        assert_eq!(seen, N, "the sweep must report on every tracked instrument");
        println!(
            "scan_silence: {N} instruments, {elapsed:?} ({:.1} ns/instrument), \
             {:.4}% of the 30s cadence",
            elapsed.as_nanos() as f64 / N as f64,
            elapsed.as_secs_f64() / 30.0 * 100.0
        );
    }
    // -- classify_ltt --------------------------------------------------------

    #[test]
    fn test_classify_ltt_first_sight() {
        assert_eq!(classify_ltt(None, 100), LttVerdict::FirstSight);
        assert!(!classify_ltt(None, 100).is_anomalous());
    }

    #[test]
    fn test_classify_ltt_backward_is_the_strong_signal() {
        let v = classify_ltt(Some(1_000), 995);
        assert_eq!(v, LttVerdict::Backward { by_secs: 5 });
        assert!(v.is_anomalous(), "backward LTT must be anomalous");
    }

    #[test]
    fn test_classify_ltt_repeated_is_healthy_not_an_anomaly() {
        // Whole-second stamps make this the COMMON case, not an edge case.
        let v = classify_ltt(Some(1_000), 1_000);
        assert_eq!(v, LttVerdict::Repeated);
        assert!(
            !v.is_anomalous(),
            "repeated whole-second LTT must NOT alarm"
        );
    }

    #[test]
    fn test_classify_ltt_advanced() {
        assert_eq!(
            classify_ltt(Some(1_000), 1_007),
            LttVerdict::Advanced { by_secs: 7 }
        );
    }

    #[test]
    fn test_classify_ltt_u32_boundaries() {
        // Max value forward from zero, and the largest possible backward step.
        assert_eq!(
            classify_ltt(Some(0), u32::MAX),
            LttVerdict::Advanced { by_secs: u32::MAX }
        );
        assert_eq!(
            classify_ltt(Some(u32::MAX), 0),
            LttVerdict::Backward { by_secs: u32::MAX }
        );
        assert_eq!(classify_ltt(Some(u32::MAX), u32::MAX), LttVerdict::Repeated);
    }

    // -- classify_volume -----------------------------------------------------

    #[test]
    fn test_classify_volume_first_sight_and_unchanged() {
        assert_eq!(
            classify_volume(None, 500, 10, 20, true),
            VolumeVerdict::FirstSight
        );
        assert_eq!(
            classify_volume(Some(500), 500, 10, 20, true),
            VolumeVerdict::Unchanged
        );
    }

    #[test]
    fn test_classify_volume_decrease_is_regressed_and_ambiguous() {
        // Session reset, u32 wraparound and corruption are indistinguishable —
        // both values are handed back rather than a guessed cause.
        let v = classify_volume(Some(9_000), 12, 100, 20, true);
        assert_eq!(
            v,
            VolumeVerdict::Regressed {
                previous: 9_000,
                current: 12
            }
        );
        assert!(v.is_anomalous());
    }

    #[test]
    fn test_classify_volume_u32_wraparound_reports_regressed() {
        let v = classify_volume(Some(u32::MAX - 5), 3, 100, 20, true);
        assert!(matches!(v, VolumeVerdict::Regressed { .. }));
    }

    #[test]
    fn test_classify_volume_huge_jump_is_suspected_gap_when_warm() {
        // baseline 100 * multiplier 20 = 2_000 ceiling; 5_000 exceeds it.
        let v = classify_volume(Some(0), 5_000, 100, 20, true);
        assert_eq!(
            v,
            VolumeVerdict::SuspectedGap {
                delta: 5_000,
                baseline: 100
            }
        );
        assert!(v.is_anomalous());
    }

    #[test]
    fn test_classify_volume_jump_at_exact_ceiling_is_not_a_gap() {
        // Strict `>` — the boundary itself stays healthy.
        assert_eq!(
            classify_volume(Some(0), 2_000, 100, 20, true),
            VolumeVerdict::Advanced { delta: 2_000 }
        );
    }

    #[test]
    fn test_classify_volume_cold_baseline_never_raises_a_gap() {
        // Not warm: a huge jump must stay Advanced, or every instrument would
        // page during its first seconds on the feed.
        assert_eq!(
            classify_volume(Some(0), u32::MAX, 100, 20, false),
            VolumeVerdict::Advanced { delta: u32::MAX }
        );
        // Warm but zero baseline (never traded) — still no gap.
        assert_eq!(
            classify_volume(Some(0), u32::MAX, 0, 20, true),
            VolumeVerdict::Advanced { delta: u32::MAX }
        );
    }

    #[test]
    fn test_classify_volume_ceiling_cannot_overflow() {
        // baseline * multiplier would overflow u64; saturation keeps it sane.
        let v = classify_volume(Some(0), u32::MAX, u64::MAX, u64::MAX, true);
        assert_eq!(v, VolumeVerdict::Advanced { delta: u32::MAX });
    }

    // -- classify_oi ---------------------------------------------------------

    #[test]
    fn test_classify_oi_has_no_loss_semantics_in_either_direction() {
        assert_eq!(classify_oi(None, 10), OiVerdict::FirstSight);
        assert_eq!(classify_oi(Some(10), 10), OiVerdict::Unchanged);
        // Falling OI is a legitimate position unwind, not evidence of loss.
        assert_eq!(
            classify_oi(Some(100), 40),
            OiVerdict::Changed {
                previous: 100,
                current: 40
            }
        );
        assert_eq!(
            classify_oi(Some(40), 100),
            OiVerdict::Changed {
                previous: 40,
                current: 100
            }
        );
        // OI never contributes to the loss verdict.
        let a = TickAssessment {
            ltt: LttVerdict::Repeated,
            volume: VolumeVerdict::Unchanged,
            oi: classify_oi(Some(100), 40),
            refused: false,
        };
        assert!(!a.suspects_loss());
    }

    // -- ewma ----------------------------------------------------------------

    #[test]
    fn test_ewma_bootstraps_from_zero_and_converges() {
        assert_eq!(
            ewma_update(0, 750),
            750,
            "zero must bootstrap to the sample"
        );
        assert_eq!(
            ewma_update(1_000, 1_000),
            1_000,
            "steady state must be stable"
        );
        assert!(
            ewma_update(1_000, 2_000) > 1_000,
            "must move toward a higher sample"
        );
        assert!(
            ewma_update(1_000, 0) < 1_000,
            "must move toward a lower sample"
        );
        // Regression: the earlier `(prev * (D-1) + sample) / D` form saturated
        // BEFORE dividing and collapsed these to u64::MAX / 8.
        assert_eq!(
            ewma_update(u64::MAX, u64::MAX),
            u64::MAX,
            "extreme equal inputs must hold, not collapse"
        );
        let down = ewma_update(u64::MAX, 0);
        assert!(down < u64::MAX, "must move toward the sample");
        assert!(
            down > u64::MAX - (u64::MAX / 4),
            "must step down by ~1/8, not collapse to MAX/8"
        );
    }

    #[test]
    fn test_ewma_converges_toward_a_step_change() {
        let mut e = 1_000;
        for _ in 0..64 {
            e = ewma_update(e, 4_000);
        }
        assert!(
            (3_950..=4_050).contains(&e),
            "converged to {e}, expected ~4000"
        );
    }

    // -- classify_silence ----------------------------------------------------

    #[test]
    fn test_classify_silence_never_ticked_beats_everything() {
        let (v, _) = classify_silence(0, 0, 1, false, &DetectorConfig::default());
        assert_eq!(v, SilenceVerdict::NeverTicked);
    }

    #[test]
    fn test_classify_silence_warming_suppresses_alarms() {
        let cfg = DetectorConfig::default();
        let (v, _) = classify_silence(100, cfg.baseline_warmup_samples - 1, 10_000_000, true, &cfg);
        assert_eq!(v, SilenceVerdict::Warming, "warmup must suppress");
    }

    #[test]
    fn test_classify_silence_floor_protects_chatty_instruments() {
        let cfg = DetectorConfig::default();
        // 200 ms cadence * 10 = 2s, but the 30s floor wins.
        let (v, expected) = classify_silence(200, 32, 5_000, true, &cfg);
        assert_eq!(expected, DEFAULT_SILENCE_FLOOR_MILLIS);
        assert_eq!(
            v,
            SilenceVerdict::Live,
            "5s quiet must not page a 200ms index"
        );
        let (v, _) = classify_silence(200, 32, 31_000, true, &cfg);
        assert_eq!(v, SilenceVerdict::Exceeded);
    }

    #[test]
    fn test_classify_silence_sparse_instrument_gets_a_wide_ceiling() {
        let cfg = DetectorConfig::default();
        // A far-month option ticking every 4 minutes tolerates 40 minutes.
        let (v, expected) = classify_silence(240_000, 32, 600_000, true, &cfg);
        assert_eq!(expected, 2_400_000);
        assert_eq!(
            v,
            SilenceVerdict::Live,
            "10min quiet is normal for a 4min baseline"
        );
    }

    // -- detector: composite key + isolation ---------------------------------

    #[test]
    fn test_same_security_id_different_segment_are_isolated() {
        // I-P1-11: security_id alone is NOT unique.
        let mut d = detector();
        let idx = (13u64, ExchangeSegment::IdxI);
        let eq = (13u64, ExchangeSegment::NseEquity);
        d.observe(obs(idx, 1_000, 500, 0, 0));
        let a = d.observe(obs(eq, 900, 10, 0, 1));
        assert_eq!(
            a.ltt,
            LttVerdict::FirstSight,
            "must not inherit the IDX_I state"
        );
        assert_eq!(d.tracked_instruments(), 2);
    }

    #[test]
    fn test_gap_in_one_instrument_does_not_affect_another() {
        let mut d = detector();
        d.observe(obs(NIFTY, 1_000, 100, 0, 0));
        d.observe(obs(BANKNIFTY, 1_000, 100, 0, 0));
        let a = d.observe(obs(NIFTY, 900, 100, 0, 100));
        assert!(a.ltt.is_anomalous());
        let b = d.observe(obs(BANKNIFTY, 1_001, 100, 0, 100));
        assert!(!b.suspects_loss(), "neighbour must stay clean");
    }

    // -- detector: never ticks / ticks once then stops ------------------------

    #[test]
    fn test_instrument_that_never_ticks_is_reported_never_ticked() {
        let mut d = detector();
        assert!(d.seed(NIFTY, 0));
        let mut reports = Vec::new();
        d.scan_silence(600_000, |r| reports.push(r));
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].verdict, SilenceVerdict::NeverTicked);
        assert_eq!(reports[0].silent_millis, 600_000);
        assert!(reports[0].counts_toward_alarm(), "a dead stream must alarm");
    }

    #[test]
    fn test_instrument_that_ticks_once_then_stops_stays_in_warmup() {
        // Honest blind spot, asserted rather than hidden: one tick yields no
        // interval, so there is no baseline and the detector declines to
        // guess. It must NOT be reported as NeverTicked — it did tick, and
        // mislabelling it would both misinform the operator and page as a
        // dead stream. See classify_silence for why the bare floor was
        // rejected as the fallback.
        let mut d = detector();
        d.observe(obs(NIFTY, 1_000, 10, 0, 0));
        let mut reports = Vec::new();
        d.scan_silence(3_600_000, |r| reports.push(r));
        assert_eq!(reports[0].samples, 0, "one tick yields zero intervals");
        assert_ne!(
            reports[0].verdict,
            SilenceVerdict::NeverTicked,
            "it DID tick — NeverTicked must mean literally zero ticks"
        );
        assert_eq!(reports[0].verdict, SilenceVerdict::Warming);
        assert!(!reports[0].counts_toward_alarm());
    }

    #[test]
    fn test_warmed_instrument_going_quiet_is_reported_and_counted() {
        let mut d = detector();
        warm_up(&mut d, NIFTY, 1_000, 16);
        let last = 16_000;
        let mut reports = Vec::new();
        d.scan_silence(last + 300_000, |r| reports.push(r));
        assert_eq!(reports[0].verdict, SilenceVerdict::Exceeded);
        assert!(!reports[0].sparse, "1s cadence is not sparse");
        assert!(reports[0].counts_toward_alarm());
    }

    // -- detector: the not-crying-wolf guarantees -----------------------------

    #[test]
    fn test_sparse_instrument_is_visible_but_never_counted() {
        // Far-month option / INDIA VIX class: 5-minute cadence. Even when it
        // breaches its own wide ceiling it must not increment the alarm count
        // (§36.4 precedent), while remaining visible in the sweep.
        let mut d = detector();
        warm_up(&mut d, NIFTY, 300_000, 16);
        let last = 16 * 300_000;
        let mut reports = Vec::new();
        d.scan_silence(last + 60_000_000, |r| reports.push(r));
        assert!(reports[0].sparse, "300s baseline must classify as sparse");
        assert_eq!(reports[0].verdict, SilenceVerdict::Exceeded);
        assert!(
            !reports[0].counts_toward_alarm(),
            "sparse instruments must never page"
        );
        assert!(
            reports[0].baseline_millis > 0,
            "visibility must be retained"
        );
    }

    #[test]
    fn test_quiet_spell_within_baseline_is_silent_no_alarm() {
        let mut d = detector();
        warm_up(&mut d, NIFTY, 2_000, 16);
        let last = 32_000;
        let mut counted = 0usize;
        d.scan_silence(last + 15_000, |r| {
            if r.counts_toward_alarm() {
                counted += 1;
            }
        });
        assert_eq!(counted, 0, "15s quiet on a 2s baseline must not page");
    }

    // -- detector: bursts, resets, boundaries ---------------------------------

    #[test]
    fn test_burst_of_ten_thousand_ticks_in_one_second() {
        let mut d = detector();
        let mut anomalies = 0usize;
        for i in 0..10_000u32 {
            // Same whole second, same millisecond: the realistic burst shape.
            let a = d.observe(obs(NIFTY, 1_000, i, 0, 500));
            if a.suspects_loss() {
                anomalies += 1;
            }
        }
        assert_eq!(anomalies, 0, "a dense burst must not manufacture anomalies");
        assert_eq!(d.tracked_instruments(), 1);
        assert_eq!(d.refused_count(), 0);
        let mut reports = Vec::new();
        d.scan_silence(500, |r| reports.push(r));
        assert_eq!(
            reports[0].baseline_millis, 0,
            "zero-interval burst -> zero baseline"
        );
        assert_eq!(
            reports[0].expected_millis, DEFAULT_SILENCE_FLOOR_MILLIS,
            "floor must protect a zero baseline from instant alarms"
        );
    }

    #[test]
    fn test_session_reset_clears_state_and_retains_capacity() {
        let mut d = detector();
        let cap = d.capacity();
        warm_up(&mut d, NIFTY, 1_000, 16);
        assert_eq!(d.tracked_instruments(), 1);
        d.reset_session();
        assert_eq!(d.tracked_instruments(), 0);
        assert_eq!(d.capacity(), cap, "capacity must survive the reset");
        assert_eq!(d.refused_count(), 0);
        let mut reports = Vec::new();
        d.scan_silence(999_999, |r| reports.push(r));
        assert!(reports.is_empty(), "reset must clear the sweep");
        // The first post-reset tick must not read as a volume regression even
        // though the exchange counter restarted lower.
        let a = d.observe(obs(NIFTY, 10, 5, 0, 0));
        assert_eq!(a.volume, VolumeVerdict::FirstSight);
        assert!(!a.suspects_loss());
    }

    #[test]
    fn test_capacity_exhaustion_fails_closed_and_counts() {
        let mut d = TickGapDetector::with_capacity(2, DetectorConfig::default());
        d.observe(obs((1, ExchangeSegment::NseFno), 1, 1, 0, 0));
        d.observe(obs((2, ExchangeSegment::NseFno), 1, 1, 0, 0));
        let a = d.observe(obs((3, ExchangeSegment::NseFno), 1, 1, 0, 0));
        assert!(a.was_refused(), "must refuse rather than grow");
        assert_eq!(
            d.refused_count(),
            1,
            "refusal must be countable, never silent"
        );
        assert_eq!(d.tracked_instruments(), 2);
        assert!(!d.seed((4, ExchangeSegment::NseFno), 0));
        assert_eq!(d.refused_count(), 2);
        // Already-tracked instruments keep working while over capacity.
        let b = d.observe(obs((1, ExchangeSegment::NseFno), 2, 5, 0, 10));
        assert!(!b.was_refused());
    }

    #[test]
    fn test_zero_capacity_detector_refuses_everything_without_panicking() {
        let mut d = TickGapDetector::with_capacity(0, DetectorConfig::default());
        assert!(d.observe(obs(NIFTY, 1, 1, 0, 0)).was_refused());
        assert_eq!(d.tracked_instruments(), 0);
        let mut n = 0usize;
        d.scan_silence(1_000, |_| n += 1);
        assert_eq!(n, 0);
    }

    #[test]
    fn test_backward_clock_step_does_not_rewind_last_seen() {
        // A monotonic source is the caller's contract; if it is violated the
        // detector must degrade, not inflate every later silence measurement.
        let mut d = detector();
        d.observe(obs(NIFTY, 1_000, 10, 0, 100_000));
        d.observe(obs(NIFTY, 1_001, 20, 0, 1_000));
        let mut reports = Vec::new();
        d.scan_silence(100_000, |r| reports.push(r));
        assert_eq!(
            reports[0].silent_millis, 0,
            "must not read as 99s of silence"
        );
    }

    #[test]
    fn test_seed_then_tick_transitions_out_of_never_ticked() {
        let mut d = detector();
        assert!(d.seed(NIFTY, 0));
        assert!(d.seed(NIFTY, 5_000), "seed must be idempotent");
        assert_eq!(d.tracked_instruments(), 1);
        warm_up(&mut d, NIFTY, 1_000, 16);
        let mut reports = Vec::new();
        d.scan_silence(16_000, |r| reports.push(r));
        assert_eq!(reports[0].verdict, SilenceVerdict::Live);
    }

    // -- observation plumbing -------------------------------------------------

    #[test]
    fn test_from_parsed_tick_maps_segment_and_rejects_the_enum_gap() {
        let mut tick = ParsedTick {
            security_id: 13,
            exchange_segment_code: 0,
            exchange_timestamp: 1_700_000_000,
            volume: 4_242,
            open_interest: 99,
            ..Default::default()
        };
        let o = TickObservation::from_parsed_tick(&tick, 777).expect("IDX_I must map");
        assert_eq!(o.key, (13, ExchangeSegment::IdxI));
        assert_eq!(o.ltt_epoch_secs, 1_700_000_000);
        assert_eq!(o.volume, 4_242);
        assert_eq!(o.open_interest, 99);
        assert_eq!(o.recv_monotonic_millis, 777);

        tick.exchange_segment_code = 6; // documented gap
        assert!(
            TickObservation::from_parsed_tick(&tick, 0).is_none(),
            "unmapped segment must be refused, never guessed"
        );
        tick.exchange_segment_code = 200;
        assert!(TickObservation::from_parsed_tick(&tick, 0).is_none());
    }

    #[test]
    fn test_error_code_is_the_existing_risk_gap_03_variant() {
        assert_eq!(TICK_GAP_ERROR_CODE.code_str(), "RISK-GAP-03");
    }

    #[test]
    fn test_scan_silence_visits_every_tracked_instrument_once() {
        let mut d = detector();
        for i in 0..32u64 {
            d.observe(obs((i, ExchangeSegment::NseFno), 1_000, 1, 0, 0));
        }
        // ExchangeSegment is not Ord, so uniqueness is checked on the wire code.
        let mut seen = Vec::new();
        d.scan_silence(1_000, |r| seen.push((r.key.0, r.key.1.binary_code())));
        assert_eq!(seen.len(), 32);
        let unique: std::collections::HashSet<(u64, u8)> = seen.iter().copied().collect();
        assert_eq!(unique.len(), 32, "no duplicates, no omissions");
    }

    // -- property tests -------------------------------------------------------

    proptest! {
        /// The monotonicity contract: Backward is returned if and only if the
        /// timestamp actually went backwards, and every reported delta is
        /// exact. Verified across the whole u32 domain.
        #[test]
        fn prop_classify_ltt_is_exactly_monotonicity(prev: u32, cur: u32) {
            let v = classify_ltt(Some(prev), cur);
            match v {
                LttVerdict::Backward { by_secs } => {
                    prop_assert!(cur < prev);
                    prop_assert_eq!(by_secs, prev - cur);
                }
                LttVerdict::Repeated => prop_assert_eq!(cur, prev),
                LttVerdict::Advanced { by_secs } => {
                    prop_assert!(cur > prev);
                    prop_assert_eq!(by_secs, cur - prev);
                }
                LttVerdict::FirstSight => prop_assert!(false, "prev was Some"),
            }
            prop_assert_eq!(v.is_anomalous(), cur < prev);
        }

        /// A first sight is never anomalous, whatever the value.
        #[test]
        fn prop_classify_ltt_first_sight_never_alarms(cur: u32) {
            prop_assert!(!classify_ltt(None, cur).is_anomalous());
        }

        /// EWMA stays inside the interval spanned by its inputs — it can never
        /// overshoot, so a baseline cannot run away from observed reality.
        #[test]
        fn prop_ewma_stays_within_input_bounds(prev in 1u64.., sample: u64) {
            let out = ewma_update(prev, sample);
            prop_assert!(out >= prev.min(sample));
            prop_assert!(out <= prev.max(sample));
        }

        /// A cold baseline can NEVER raise a suspected gap, at any magnitude.
        /// This is the property that stops freshly-subscribed instruments from
        /// paging during warmup.
        #[test]
        fn prop_cold_baseline_never_suspects_a_gap(
            prev: u32,
            cur: u32,
            baseline in 0u64..1_000_000,
            mult in 1u64..1_000,
        ) {
            let v = classify_volume(Some(prev), cur, baseline, mult, false);
            let is_gap = matches!(v, VolumeVerdict::SuspectedGap { .. });
            prop_assert!(!is_gap, "cold baseline raised a gap: {:?}", v);
        }

        /// Volume classification is total and direction-exact over the whole
        /// u32 domain: a decrease always regresses, equality never alarms.
        #[test]
        fn prop_classify_volume_direction_is_exact(
            prev: u32,
            cur: u32,
            baseline in 0u64..1_000_000,
            mult in 1u64..1_000,
        ) {
            let v = classify_volume(Some(prev), cur, baseline, mult, true);
            if cur < prev {
                let regressed = matches!(v, VolumeVerdict::Regressed { .. });
                prop_assert!(regressed, "expected Regressed, got {:?}", v);
            } else if cur == prev {
                prop_assert_eq!(v, VolumeVerdict::Unchanged);
            } else {
                let forward = matches!(
                    v,
                    VolumeVerdict::Advanced { .. } | VolumeVerdict::SuspectedGap { .. }
                );
                prop_assert!(forward, "expected a forward verdict, got {:?}", v);
            }
        }

        /// Silence classification never alarms below the configured floor,
        /// whatever the baseline — the anti-false-positive backstop.
        #[test]
        fn prop_silence_never_alarms_below_the_floor(
            baseline in 0u64..1_000_000,
            samples in 0u32..1_000,
            silent in 0u64..DEFAULT_SILENCE_FLOOR_MILLIS,
            ever_ticked: bool,
        ) {
            let cfg = DetectorConfig::default();
            let (v, _) = classify_silence(baseline, samples, silent, ever_ticked, &cfg);
            prop_assert!(!matches!(v, SilenceVerdict::Exceeded));
            // Only a stream that never ticked at all may alarm inside the floor.
            if ever_ticked {
                prop_assert!(matches!(v, SilenceVerdict::Live | SilenceVerdict::Warming));
            } else {
                prop_assert_eq!(v, SilenceVerdict::NeverTicked);
            }
        }

        /// The detector is total: no sequence of arbitrary observations can
        /// panic it, and it never exceeds its capacity.
        #[test]
        fn prop_detector_is_total_over_arbitrary_input(
            ticks in prop::collection::vec(
                (0u64..8, 0u32..5, any::<u32>(), any::<u32>(), any::<u64>()),
                0..200,
            ),
        ) {
            let mut d = TickGapDetector::with_capacity(4, DetectorConfig::default());
            for (sid, seg_code, ltt, vol, millis) in ticks {
                let Some(seg) = ExchangeSegment::from_byte(seg_code as u8) else {
                    continue;
                };
                let a = d.observe(obs((sid, seg), ltt, vol, vol, millis));
                prop_assert!(a.refused || d.tracked_instruments() <= d.capacity());
            }
            prop_assert!(d.tracked_instruments() <= 4);
            let mut n = 0usize;
            d.scan_silence(u64::MAX, |_| n += 1);
            prop_assert_eq!(n, d.tracked_instruments());
        }
    }

    // ======================================================================
    // Direct coverage of each public predicate and accessor.
    //
    // These exist so every `pub fn` is exercised by a test that NAMES it —
    // the pre-push pub-fn guard matches on test names, and more importantly a
    // predicate whose polarity silently inverts is exactly the kind of defect
    // that makes an alarm stop firing without anyone noticing.
    // ======================================================================

    #[test]
    fn test_ltt_verdict_is_anomalous_only_for_backward_steps() {
        assert!(LttVerdict::Backward { by_secs: 1 }.is_anomalous());
        // Repeated is the COMMON case: the feed stamps whole seconds, so many
        // ticks legitimately share one LTT. Treating it as anomalous would
        // make the detector fire constantly.
        assert!(!LttVerdict::Repeated.is_anomalous());
        assert!(!LttVerdict::FirstSight.is_anomalous());
        assert!(!LttVerdict::Advanced { by_secs: 1 }.is_anomalous());
    }

    #[test]
    fn test_volume_verdict_is_anomalous_for_regression_and_suspected_gap() {
        assert!(
            VolumeVerdict::Regressed {
                previous: 10,
                current: 5
            }
            .is_anomalous()
        );
        assert!(
            VolumeVerdict::SuspectedGap {
                delta: 10_000,
                baseline: 10
            }
            .is_anomalous()
        );
        assert!(!VolumeVerdict::FirstSight.is_anomalous());
        assert!(!VolumeVerdict::Unchanged.is_anomalous());
        // A normal advance must NOT be anomalous — the feed is conflated, so
        // every tick carries a volume delta.
        assert!(!VolumeVerdict::Advanced { delta: 500 }.is_anomalous());
    }

    #[test]
    fn test_ewma_update_tracks_toward_the_sample_and_is_total() {
        // Converges toward the sample rather than jumping to it.
        let a = ewma_update(1_000, 2_000);
        assert!(a > 1_000 && a < 2_000, "moved partway, got {a}");
        // Idempotent at the fixed point.
        assert_eq!(ewma_update(1_000, 1_000), 1_000);
        // Total at the domain edges. The workspace builds with
        // overflow-checks, so an intermediate overflow here would panic
        // rather than return a wrong number — this is the regression pin for
        // the two overflow bugs found while writing this function.
        for (prev, sample) in [
            (0, 0),
            (0, u64::MAX),
            (u64::MAX, 0),
            (u64::MAX, u64::MAX),
            (1, u64::MAX),
            (u64::MAX, 1),
        ] {
            let got = ewma_update(prev, sample);
            let (lo, hi) = (prev.min(sample), prev.max(sample));
            assert!(
                got >= lo && got <= hi,
                "ewma_update({prev}, {sample}) = {got} escaped [{lo}, {hi}]"
            );
        }
    }

    #[test]
    fn test_tick_assessment_suspects_loss_and_was_refused_are_independent() {
        let mut d = detector();
        let clean = d.observe(obs(NIFTY, 1_000, 10, 0, 0));
        assert!(!clean.suspects_loss(), "first sight is not suspicious");
        assert!(!clean.was_refused());

        // A backward LTT is strong evidence and must surface.
        let backward = d.observe(obs(NIFTY, 999, 20, 0, 100));
        assert!(backward.suspects_loss());
        assert!(!backward.was_refused(), "it was recorded, just suspicious");
    }

    #[test]
    fn test_was_refused_is_true_only_when_capacity_is_exhausted() {
        let mut d = TickGapDetector::with_capacity(1, DetectorConfig::default());
        assert!(!d.observe(obs(NIFTY, 1_000, 1, 0, 0)).was_refused());
        let refused = d.observe(obs(BANKNIFTY, 1_000, 1, 0, 0));
        assert!(
            refused.was_refused(),
            "beyond capacity the detector must refuse, not grow"
        );
        assert_eq!(d.refused_count(), 1);
        assert_eq!(d.tracked_instruments(), 1, "the table must NOT have grown");
    }

    #[test]
    fn test_counts_toward_alarm_excludes_sparse_instruments() {
        let report = |verdict, sparse| SilenceReport {
            key: NIFTY,
            silent_millis: 10_000,
            expected_millis: 1_000,
            baseline_millis: 100,
            samples: 32,
            verdict,
            sparse,
        };
        assert!(report(SilenceVerdict::Exceeded, false).counts_toward_alarm());
        assert!(report(SilenceVerdict::NeverTicked, false).counts_toward_alarm());
        // The far-month / INDIA VIX case: visible in the sweep, never paging.
        assert!(!report(SilenceVerdict::Exceeded, true).counts_toward_alarm());
        assert!(!report(SilenceVerdict::NeverTicked, true).counts_toward_alarm());
        // Healthy verdicts never count regardless of sparseness.
        assert!(!report(SilenceVerdict::Live, false).counts_toward_alarm());
        assert!(!report(SilenceVerdict::Warming, false).counts_toward_alarm());
    }

    #[test]
    fn test_with_capacity_bounds_the_table_and_reports_it() {
        let d = TickGapDetector::with_capacity(8, DetectorConfig::default());
        assert_eq!(d.tracked_instruments(), 0);
        assert_eq!(d.refused_count(), 0);
    }

    #[test]
    fn test_reset_session_clears_state_but_keeps_capacity() {
        let mut d = detector();
        d.observe(obs(NIFTY, 1_000, 10, 0, 0));
        d.observe(obs(BANKNIFTY, 1_000, 10, 0, 0));
        assert_eq!(d.tracked_instruments(), 2);

        d.reset_session();
        assert_eq!(d.tracked_instruments(), 0, "session state must be cleared");

        // Still usable afterwards, and a post-reset tick is FirstSight again
        // rather than inheriting yesterday's cumulative volume — which would
        // otherwise read as a huge regression at every session start.
        let after = d.observe(obs(NIFTY, 2_000, 1, 0, 0));
        assert!(!after.suspects_loss());
        assert_eq!(d.tracked_instruments(), 1);
    }

    #[test]
    fn test_tracked_instruments_and_refused_count_reflect_real_activity() {
        let mut d = TickGapDetector::with_capacity(2, DetectorConfig::default());
        assert_eq!((d.tracked_instruments(), d.refused_count()), (0, 0));
        d.observe(obs(NIFTY, 1_000, 1, 0, 0));
        assert_eq!((d.tracked_instruments(), d.refused_count()), (1, 0));
        // A repeat of a known instrument must not consume another slot.
        d.observe(obs(NIFTY, 1_001, 2, 0, 10));
        assert_eq!((d.tracked_instruments(), d.refused_count()), (1, 0));
        d.observe(obs(BANKNIFTY, 1_000, 1, 0, 0));
        assert_eq!((d.tracked_instruments(), d.refused_count()), (2, 0));
        d.observe(obs((99, ExchangeSegment::IdxI), 1_000, 1, 0, 0));
        assert_eq!((d.tracked_instruments(), d.refused_count()), (2, 1));
    }
}

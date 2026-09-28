//! The live main-feed subscription set, optionally sourced from the daily
//! resolved instrument master.
//!
//! Authorized to be BUILT by the operator's 2026-08-11 fourth quote; the
//! verbatim quote, the tension with the same day's third quote, and the
//! reasoning for shipping DEFAULT-OFF are recorded in
//! `.claude/rules/project/websocket-connection-scope-lock.md`.
//!
//! # The switch, and where it stands
//!
//! `[dhan_universe] live_subscription_from_master` gates the master-sourced
//! path. It was built DEFAULT-OFF (the third quote's carve-out, "re-pointing
//! the lane… must not be smuggled in"), and the operator turned it ON on
//! 2026-08-12: `config/base.toml` ships `true`. With it off,
//! [`select_live_universe`] returns the 4 hardcoded index SIDs; with it on —
//! every production boot today — the session subscribes the resolved list.
//!
//! # Never the 4 index SIDs when a list exists (audit D3, 2026-09-28)
//!
//! Owner, 2026-09-26: no 4-index fallback. Two arms that used to collapse the
//! session to the 4 index SIDs no longer do:
//!
//! * a list larger than the authorized envelope FILLS the envelope by priority
//!   (indices first) and pages the excess;
//! * a missing or unreadable list for today takes the newest earlier day's list
//!   still on disk (the rider keeps 7 days) and pages when that was not
//!   expected.
//!
//! Only when no list at all is on disk does the session run on the index set.
//!
//! # Fallback is never silent
//!
//! Every path that cannot produce today's master-sourced set says so: at
//! `error!` with a paging counter when the boot should have widened, at `warn!`
//! on a boot where today's list cannot exist yet. A fallback that logged
//! nothing would look identical to a successful widening in every metric the
//! lane exposes — the connection count would simply be lower, and nobody reads
//! a connection count expecting it to carry an error.

use std::sync::atomic::{AtomicUsize, Ordering};

use tickvault_common::types::{ExchangeSegment, SecurityId};
use tickvault_core::websocket::pool_supervisor::SubscribeInstrument;

/// Where the session's subscription set actually came from.
///
/// Carried rather than inferred: "we widened" and "we tried to widen and fell
/// back" produce very different instrument counts and must never be told apart
/// by guessing from the count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UniverseSource {
    /// The 4 hardcoded index SIDs — the historical, always-safe set.
    HardcodedIndices,
    /// The index SIDs plus constituents resolved from today's master.
    MasterSourced,
    /// Master sourcing was requested but could not be trusted; the index set
    /// is in use and the reason has been logged.
    FellBackToIndices,
    /// The resolved set was larger than the authorized main-feed envelope, so
    /// the envelope was FILLED by priority (indices first, then by
    /// `(segment, security_id)`) and the excess was counted and paged. Owner decision
    /// (audit D3, 2026-09-26): fill 25,000 by priority with a critical page,
    /// never fall back to the 4 index SIDs.
    TruncatedToCapacity,
}

impl UniverseSource {
    /// Stable label for logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HardcodedIndices => "hardcoded_indices",
            Self::MasterSourced => "master_sourced",
            Self::FellBackToIndices => "fell_back_to_indices",
            Self::TruncatedToCapacity => "truncated_to_capacity",
        }
    }
}

/// The chosen subscription set plus an account of what was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveUniverseSelection {
    /// What the main feed subscribes this session.
    pub instruments: Vec<SubscribeInstrument>,
    /// Where it came from.
    pub source: UniverseSource,
    /// Master entries whose `security_id` was 0.
    pub refused_zero_id: usize,
    /// Master entries whose segment byte is not a known Dhan segment.
    ///
    /// Dhan's segment numbering has a GAP at 6 (`MCX_COMM` is 5, `BSE_CURRENCY`
    /// is 7). A byte that does not decode is refused rather than coerced —
    /// coercing would subscribe a real id under the wrong segment, and
    /// `(security_id, segment)` is the composite identity everything downstream
    /// keys on (I-P1-11).
    pub refused_unknown_segment: usize,
    /// Entries dropped because they duplicate an already-selected
    /// `(security_id, segment)` pair.
    pub deduped: usize,
    /// Distinct instruments left out because the set was larger than the
    /// authorized envelope. Non-zero only with
    /// [`UniverseSource::TruncatedToCapacity`].
    pub refused_over_capacity: usize,
}

/// One resolved constituent, as the daily rider wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MasterEntry {
    /// Resolved Dhan `security_id`.
    pub security_id: u64,
    /// Segment as the artifact's wire byte — decoded, never cast.
    pub exchange_segment_code: u8,
}

/// Parse the daily mapping artifact.
///
/// Fail-LOUD on a malformed body or a missing `mappings` key; fail-soft per
/// entry. The distinction is the same one `parse_lifecycle_dataset` draws, and
/// it matters more here: an empty `Vec` returned for garbage is
/// indistinguishable from "the master legitimately resolved nothing", and those
/// two must produce different behaviour — one is a parse bug, the other is a
/// vendor problem.
///
/// # Errors
/// Returns `Err` when the body is not JSON or carries no `mappings` array.
pub fn parse_mapping_artifact(body: &str) -> Result<Vec<MasterEntry>, String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Err("mapping artifact is not valid JSON".to_owned());
    };
    let Some(rows) = v.get("mappings").and_then(|m| m.as_array()) else {
        return Err("mapping artifact has no `mappings` array".to_owned());
    };
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(security_id) = row.get("security_id").and_then(serde_json::Value::as_u64) else {
            continue;
        };
        let Some(seg) = row
            .get("exchange_segment")
            .and_then(serde_json::Value::as_u64)
        else {
            continue;
        };
        let Ok(exchange_segment_code) = u8::try_from(seg) else {
            continue;
        };
        out.push(MasterEntry {
            security_id,
            exchange_segment_code,
        });
    }
    Ok(out)
}

/// Build the session's subscription set.
///
/// `index_universe` is always included — the index spot values are the
/// reference every option and future is priced against, so widening ADDS to
/// them rather than replacing them.
///
/// `capacity` is the authorized main-feed envelope (connections ×
/// instruments-per-connection). `plan_pool` refuses the ENTIRE pool when a set
/// does not fit, so an oversized set must never reach it. Until 2026-09-28 the
/// answer was to replace the whole widened set with the 4 index SIDs — a
/// 99.98% loss to avoid a loss of the excess. The owner's D3 decision
/// (2026-09-26) replaces that: the envelope is FILLED by priority — indices
/// first, then by `(segment, security_id)` — the excess is counted in
/// `refused_over_capacity`, and the caller pages it. Nothing is trimmed
/// silently; the set that does fit is never thrown away.
///
/// O(master) time and space: one hash probe per entry, one stable partition.
#[must_use]
pub fn select_live_universe(
    index_universe: &[SubscribeInstrument],
    master: Option<&[MasterEntry]>,
    capacity: usize,
) -> LiveUniverseSelection {
    let Some(master) = master else {
        return LiveUniverseSelection {
            instruments: index_universe.to_vec(),
            source: UniverseSource::HardcodedIndices,
            refused_zero_id: 0,
            refused_unknown_segment: 0,
            deduped: 0,
            refused_over_capacity: 0,
        };
    };

    let mut refused_zero_id = 0usize;
    let mut refused_unknown_segment = 0usize;
    let mut deduped = 0usize;

    // Seeded with the index set so a master entry that repeats an index SID
    // cannot be subscribed twice. `(security_id, segment)` is the composite
    // identity per I-P1-11 — a bare id would collapse two real instruments.
    //
    // A `HashSet`, not a `Vec`. This was `Vec::contains` inside the loop
    // below, which is O(n²): at today's 4,565 SIDs that is ~10M comparisons
    // and survivable, but the authorized target is 25,000, where it becomes
    // ~312M — a boot path doing a third of a billion comparisons to answer a
    // question a hash answers in one probe.
    //
    // The sibling deduper `dhan_feed_stack::dedup_subscribe_set` has always
    // used a `HashSet` on this exact key for this exact job. Two dedup paths,
    // same key, different complexity; now the same.
    // The hardcoded index seeds are held SEPARATELY from the master rows so
    // they can be dropped if the master supplies real ones — see the swap
    // below. They still seed `seen`, so a master row repeating a hardcoded id
    // is deduped rather than subscribed twice.
    let mut seen: std::collections::HashSet<(SecurityId, ExchangeSegment)> = index_universe
        .iter()
        .map(|i| (i.security_id, i.segment))
        .collect();
    let mut instruments = index_universe.to_vec();

    for entry in master {
        if entry.security_id == 0 {
            refused_zero_id += 1;
            continue;
        }
        let Some(segment) = ExchangeSegment::from_byte(entry.exchange_segment_code) else {
            refused_unknown_segment += 1;
            continue;
        };
        let key = (entry.security_id as SecurityId, segment);
        if !seen.insert(key) {
            deduped += 1;
            continue;
        }
        instruments.push(SubscribeInstrument {
            security_id: key.0,
            segment,
        });
    }

    // ---------------------------------------------------------------------
    // Drop the hardcoded index seeds once the master supplies real ones.
    //
    // Dhan's own live-feed reference is emphatic: "SecurityId values come from
    // the instrument master CSV — it is the sole source of truth. Do NOT
    // hardcode or guess these values."
    //
    // The four seeds violate that. They are `SPOT_1M_REST_INDICES` — ids
    // chosen for the REST Data API (`/v2/charts/intraday`, `/v2/optionchain`)
    // and reused verbatim on the WebSocket because one constant was
    // convenient. Nothing ever validated them against the master.
    //
    // Measured on the box: those four ids received ZERO packets of ANY
    // response code, on every recorded day — including zero code-6 PrevClose,
    // which Dhan support confirmed (Ticket #5525125) is emitted for IDX_I on
    // ANY subscription in ANY mode. A subscription that never draws even its
    // one guaranteed packet was not accepted. Master-sourced NSE_EQ and
    // NSE_FNO ids on the SAME socket delivered normally throughout.
    //
    // So when the master yields indices, they REPLACE the seeds rather than
    // joining them: keeping both would subscribe an id we have evidence is
    // dead beside the one that should work, and then report the pair as
    // healthy coverage.
    //
    // The swap is conditional, never unconditional. A master with no INDEX
    // rows leaves the seeds in place — a broken master must degrade to the
    // old behaviour, not to no indices at all, which would turn a data
    // problem into an outage.
    //
    // The master's index ids are collected into a SET first, and the swap
    // fires on that set rather than on how many index rows survived the loop
    // above. Two defects lived in the difference, and a property test found
    // both in one counterexample:
    //
    //   DUPLICATE. The old loop re-pushed every master index row without
    //   consulting `seen`, so an index listed twice in the artifact landed
    //   twice in the subscription. Duplicates are not exotic in that file --
    //   its `mappings` array carries one row per (index list, stock)
    //   membership pair, which is why 4,565 rows resolve to ~870 instruments.
    //   Downstream `dedup_subscribe_set` would have caught it before the wire,
    //   but only after this function had already counted the duplicate against
    //   the capacity envelope, and an over-count here falls the WHOLE universe
    //   back to four ids.
    //
    //   MISSED SWAP. The trigger counted NEWLY INSERTED index rows, so a
    //   master whose index ids all coincide with the hardcoded seeds inserted
    //   nothing new, the swap never fired, and the remaining seeds survived --
    //   the ids measured receiving zero packets of any code. The comment above
    //   says the swap fires "when the master yields indices"; counting
    //   insertions asked a different question.
    let mut master_index_ids: Vec<SecurityId> = Vec::new();
    let mut master_index_seen: std::collections::HashSet<SecurityId> =
        std::collections::HashSet::new();
    for entry in master {
        if entry.security_id == 0 {
            continue;
        }
        if ExchangeSegment::from_byte(entry.exchange_segment_code) != Some(ExchangeSegment::IdxI) {
            continue;
        }
        let id = entry.security_id as SecurityId;
        if master_index_seen.insert(id) {
            master_index_ids.push(id);
        }
    }
    if !master_index_ids.is_empty() {
        instruments.retain(|i| i.segment != ExchangeSegment::IdxI);
        for security_id in master_index_ids {
            instruments.push(SubscribeInstrument {
                security_id,
                segment: ExchangeSegment::IdxI,
            });
        }
    }

    if instruments.len() > capacity {
        // Fill by priority, never fall back to the index set (owner, audit D3).
        // Indices first — every option and future is priced against them.
        // The list carries no rank of its own, and its row order is not a
        // contract (pinned by `reversing_the_master_does_not_change_the_
        // subscribed_set`), so past the indices the order is the composite key:
        // the same list in any row order keeps the same instruments.
        // O(n log n) on the cold boot path, once.
        let mut ordered = instruments;
        ordered.sort_unstable_by_key(|i| {
            (
                i.segment != ExchangeSegment::IdxI,
                i.segment.binary_code(),
                i.security_id,
            )
        });
        let refused_over_capacity = ordered.len() - capacity;
        ordered.truncate(capacity);
        return LiveUniverseSelection {
            instruments: ordered,
            source: UniverseSource::TruncatedToCapacity,
            refused_zero_id,
            refused_unknown_segment,
            deduped,
            refused_over_capacity,
        };
    }

    // A master that resolved nothing usable adds nothing, and reporting that as
    // "widened" would be a false-OK: the count would equal the index set and
    // every downstream signal would look like an ordinary narrow session.
    let source = if instruments.len() > index_universe.len() {
        UniverseSource::MasterSourced
    } else {
        UniverseSource::FellBackToIndices
    };

    LiveUniverseSelection {
        instruments,
        source,
        refused_zero_id,
        refused_unknown_segment,
        deduped,
        refused_over_capacity: 0,
    }
}

// The artifact path is deliberately NOT restated here — it comes from
// `dhan_universe::mapping_artifact_path`, the same function the writer uses.
// A second copy of that filename fails silently: the reader looks for a name
// the writer never produces, finds nothing, falls back, and the result is
// indistinguishable from "the master resolved nothing usable".

/// Fraction of the authorized envelope that must remain free before the
/// headroom warning fires, expressed as a divisor: `capacity / 10` = 10%.
///
/// Chosen against the measured shape, not picked round. The 2026-08-22 live
/// reading was 22,996 of 25,000 — **8% free** — and index option chains are
/// deliberately UNCAPPED, having been observed at 2,037 contracts for three
/// underlyings. One volatile expiry closes a gap that size.
pub const UNIVERSE_HEADROOM_WARN_DIVISOR: usize = 10;

/// Warns while there is still time to act on a universe approaching the
/// subscription ceiling.
///
/// # Why this exists at all
///
/// Overflow is not graceful here and must not be made graceful: `plan_pool`
/// refuses the WHOLE pool rather than truncating it, so crossing the ceiling
/// costs the entire session's feed rather than its excess. That fail-closed
/// shape is correct — silently subscribing a subset would be a false-OK about
/// coverage — but it means the only safe place to notice the problem is
/// BEFORE it happens, and until now nothing did. The size gauge shows today's
/// number; nothing said how close today's number was to the edge.
///
/// # Why the constant is read here
///
/// `MAX_DAILY_UNIVERSE_SIZE` had ZERO production readers. Its own doc records
/// that the function which once enforced it was deleted on 2026-07-13, and
/// that the live lane subsequently ran at 4,565 SIDs against a stated cap of
/// 1,200 with nothing halting — a documented limit that enforced nothing, so
/// anyone reading it believed a check existed where none did. It is read here
/// as a cross-check on the capacity the caller passed: if the two ever
/// disagree, the constant is stale and says so, instead of sitting in the
/// tree looking authoritative.
///
/// Pure apart from the log/metric side effects. O(1).
fn report_universe_headroom(instruments: usize, capacity: usize) {
    let headroom = capacity.saturating_sub(instruments);
    let warn_below = capacity / UNIVERSE_HEADROOM_WARN_DIVISOR;

    if capacity != tickvault_common::constants::MAX_DAILY_UNIVERSE_SIZE {
        // Not an error — the caller's capacity is the REAL bound (it comes
        // from the endpoint's subscription capacity) and is allowed to differ.
        // What is not allowed is for the documented constant to drift out of
        // step with it unnoticed, which is how it became decorative.
        tracing::warn!(
            capacity,
            documented_max = tickvault_common::constants::MAX_DAILY_UNIVERSE_SIZE,
            "the live subscription capacity and MAX_DAILY_UNIVERSE_SIZE disagree — the \
             capacity in force is the one logged here; the constant is documentation and \
             is now stale. Bring them back into lockstep or the constant misleads the \
             next reader."
        );
    }

    if headroom < warn_below {
        tracing::error!(
            code = tickvault_common::error_code::ErrorCode::WsGapConnectionState.code_str(),
            source = "universe_headroom_low",
            instruments,
            capacity,
            headroom,
            warn_below,
            "live universe is within {headroom} instruments of the {capacity} ceiling. \
             Crossing it does NOT drop the excess — the whole subscription is refused \
             and the session runs with no feed at all. Index option chains are uncapped \
             by design, so one volatile expiry can close a gap this size. Act before the \
             next session: raise the ceiling or narrow the spot universe."
        );
    } else {
        tracing::info!(instruments, capacity, headroom, "live universe headroom");
    }
}

/// Why a daily artifact could not be turned into a master list.
///
/// Two variants rather than one because they mean different things and want
/// different triage: `Unreadable` is "the rider never wrote the file" (a
/// scheduling, timing or permissions problem) and `Unparseable` is "the rider
/// wrote something wrong" (a producer bug). Collapsing them into one reason
/// label would send both to the same runbook, and only one of them is ours.
enum ArtifactFailure {
    Unreadable(String),
    Unparseable(String),
}

impl ArtifactFailure {
    /// The counter label when the FULL master artifact failed. These two
    /// strings predate the narrowed set and are kept verbatim so an existing
    /// alarm or dashboard filter on them keeps matching.
    const fn mapping_reason(&self) -> &'static str {
        match self {
            Self::Unreadable(_) => "artifact_unreadable",
            Self::Unparseable(_) => "artifact_unparseable",
        }
    }

    /// The counter label when the NARROWED F&O artifact failed. Distinct from
    /// `mapping_reason` because the consequence is the opposite: this one
    /// widens the session, the other one collapses it to four instruments.
    const fn fno_reason(&self) -> &'static str {
        match self {
            Self::Unreadable(_) => "fno_artifact_unreadable",
            Self::Unparseable(_) => "fno_artifact_unparseable",
        }
    }

    /// The counter label when the NTM artifact failed. A THIRD distinct pair
    /// rather than reusing the F&O one: all three narrowings fail through to
    /// different sets, and a shared label would make a dashboard unable to say
    /// which set the session actually ended up carrying.
    const fn ntm_reason(&self) -> &'static str {
        match self {
            Self::Unreadable(_) => "ntm_artifact_unreadable",
            Self::Unparseable(_) => "ntm_artifact_unparseable",
        }
    }

    fn detail(&self) -> &str {
        match self {
            Self::Unreadable(d) | Self::Unparseable(d) => d,
        }
    }
}

/// Read one daily artifact and parse it, keeping the read failure and the
/// parse failure distinguishable all the way to the log line.
fn read_master_artifact(path: &std::path::Path) -> Result<Vec<MasterEntry>, ArtifactFailure> {
    let body = std::fs::read_to_string(path)
        .map_err(|err| ArtifactFailure::Unreadable(err.to_string()))?;
    parse_mapping_artifact(&body).map_err(ArtifactFailure::Unparseable)
}

/// The `days` IST dates before `date_ist`, newest first.
///
/// Empty when `date_ist` is not `YYYY-MM-DD`: a lookback computed from a
/// guessed date could read the wrong day's file, and an empty lookback falls
/// to the existing loud fallback instead.
#[must_use]
pub fn earlier_ist_dates(date_ist: &str, days: i64) -> Vec<String> {
    let Ok(today) = chrono::NaiveDate::parse_from_str(date_ist, "%Y-%m-%d") else {
        return Vec::new();
    };
    (1..=days.max(0))
        .filter_map(|back| today.checked_sub_signed(chrono::TimeDelta::days(back)))
        .map(|d| d.format("%Y-%m-%d").to_string())
        .collect()
}

/// An earlier day's list, used in place of today's when today's cannot be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EarlierMaster {
    /// The rows, parsed exactly as today's would be.
    pub entries: Vec<MasterEntry>,
    /// The IST date the file was written for.
    pub date_ist: String,
    /// Which of the three spot lists it is (`ntm`, `fno_underlyings`,
    /// `full_master`), for the log line.
    pub kind: &'static str,
}

/// `source` label on the line that reports a boot running on an earlier day's
/// list. NOT `fell_back_to_indices`: the session is not collapsed, so the
/// collapse alarm must not match it. The fallback COUNTER still moves (with the
/// same `artifact_*` reason as before) and that alarm pages it.
pub const EARLIER_ARTIFACT_SOURCE: &str = "earlier_day_artifact";

/// Find the newest earlier day's spot list, trying the same kinds, in the same
/// precedence, as today's resolve: NTM, then F&O underlyings (each only when
/// its flag is on), then the full mapping.
///
/// Why an earlier list is safe to subscribe: every row is an `NSE_EQ` stock or
/// an `IDX_I` index, and those security ids are stable from day to day — only
/// DERIVATIVE ids are re-issued (instrument-master rule). The cost is honest
/// and bounded: a stock that joined the list today is missing until today's
/// list is read, and one that left it is subscribed for one more session.
/// Both are far better than the 4 index SIDs, which is what this replaces.
///
/// An empty list is skipped, never returned: it would resolve to nothing and
/// report a collapse under a different name.
///
/// O(days × kinds) file probes (at most 7 × 3), once per boot, cold path.
fn newest_earlier_master_with(
    cfg: &tickvault_common::config::DhanUniverseConfig,
    date_ist: &str,
    days: i64,
    mut read: impl FnMut(&std::path::Path) -> Result<Vec<MasterEntry>, ArtifactFailure>,
) -> Option<EarlierMaster> {
    for date in earlier_ist_dates(date_ist, days) {
        let kinds: [(bool, std::path::PathBuf, &'static str); 3] = [
            (
                cfg.spot_universe_ntm_only,
                crate::dhan_universe::ntm_spot_artifact_path(&date),
                "ntm",
            ),
            (
                cfg.spot_universe_fno_underlyings_only,
                crate::dhan_universe::fno_underlying_artifact_path(&date),
                "fno_underlyings",
            ),
            (
                true,
                crate::dhan_universe::mapping_artifact_path(&date),
                "full_master",
            ),
        ];
        for (enabled, path, kind) in kinds {
            if !enabled {
                continue;
            }
            if let Ok(entries) = read(&path)
                && !entries.is_empty()
            {
                return Some(EarlierMaster {
                    entries,
                    date_ist: date,
                    kind,
                });
            }
        }
    }
    None
}

/// Counter: master sourcing was REQUESTED but did not take effect, by reason.
///
/// Non-zero means the live lane is subscribing the 4 hardcoded index SIDs while
/// the operator's config asks for the resolved master — on 2026-08-12 that was
/// **4,565** instruments, so this is a 99.9% collapse of the subscribed set.
///
/// It needs its own signal because the collapse is otherwise INDISTINGUISHABLE
/// from a healthy 4-SID day on every other gauge: the gap detector seeds only
/// what was actually subscribed, so `instruments_never_ticked` reads 0, the
/// lane-up gauge reads 1, and ticks flow normally — for four instruments.
/// Before this counter the only evidence was one uncoded `error!` line, which
/// no triage path and no alarm could see.
pub const MASTER_SOURCING_FALLBACK_COUNTER: &str = "tv_dhan_live_universe_fallback_total";

/// Count one fallback, and publish the resulting subscribed size so the size
/// itself is visible without parsing a log line.
fn record_master_sourcing_fallback(reason: &'static str, fell_back_to: usize) {
    metrics::counter!(MASTER_SOURCING_FALLBACK_COUNTER, "reason" => reason).increment(1);
    mark_live_universe_degraded(reason);
    // Cold path — once per boot at most — so the macro's key build is fine here.
    #[allow(clippy::cast_precision_loss)]
    // APPROVED: instrument counts are bounded by MAX_DAILY_UNIVERSE_SIZE, far below 2^53.
    metrics::gauge!(LIVE_UNIVERSE_SIZE_GAUGE).set(fell_back_to as f64);
}

/// Every `reason` label `record_master_sourcing_fallback` can emit.
///
/// One list so the seed and the emit sites cannot drift. A reason that
/// exists only at an emit site is a label whose FIRST occurrence the
/// operator can never see — and for this counter the first occurrence is
/// the whole event.
pub const MASTER_SOURCING_FALLBACK_REASONS: &[&str] = &[
    "artifact_unreadable",
    "artifact_unparseable",
    "fno_artifact_unreadable",
    "fno_artifact_unparseable",
    "no_usable_widening",
    "truncated_to_capacity",
];

/// Gauge: how many instruments the live lane actually subscribed.
pub const LIVE_UNIVERSE_SIZE_GAUGE: &str = "tv_dhan_live_universe_instruments";

/// Which paged reason this session is running on, if any.
///
/// `0` means the session is on today's list (or an EXPECTED pre-rider
/// fallback, which is deliberately not paged). Otherwise it is `1 +` the
/// reason's index in [`MASTER_SOURCING_FALLBACK_REASONS`]. One atomic, set
/// at most once per boot, read once a minute by the heartbeat.
static LIVE_UNIVERSE_DEGRADED: AtomicUsize = AtomicUsize::new(0);

fn mark_live_universe_degraded(reason: &'static str) {
    if let Some(index) = MASTER_SOURCING_FALLBACK_REASONS
        .iter()
        .position(|known| *known == reason)
    {
        LIVE_UNIVERSE_DEGRADED.store(index + 1, Ordering::Relaxed);
    }
}

/// The paged reason this session is running on, or `None` when it is on
/// today's list.
#[must_use]
pub fn live_universe_degraded_reason() -> Option<&'static str> {
    degraded_reason_from_slot(LIVE_UNIVERSE_DEGRADED.load(Ordering::Relaxed))
}

fn degraded_reason_from_slot(slot: usize) -> Option<&'static str> {
    slot.checked_sub(1)
        .and_then(|index| MASTER_SOURCING_FALLBACK_REASONS.get(index))
        .copied()
}

/// How often a degraded session re-counts its fallback.
pub const DEGRADED_UNIVERSE_HEARTBEAT_SECS: u64 = 60;

/// Keep `tv-<env>-live-universe-fallback` fed for as long as the session runs
/// on something other than today's list.
///
/// # Why (audit re-check 7, 2026-09-28)
///
/// The universe is resolved once, and the counter's zero seed and its one real
/// increment happen in the same function, microseconds apart. The CloudWatch
/// agent scrapes every 60 s and drops the FIRST sample of a series as the
/// delta baseline, so on a fresh process that first sample is already `1` and
/// the single event this alarm exists for is exactly the one it never sees.
/// The index-only collapse also pages through a log filter, which has no
/// baseline problem; the earlier-day list and the over-capacity fill page
/// ONLY through this counter.
///
/// One increment a minute while degraded gives every scrape after the first a
/// positive delta, whenever the baseline landed. The alarm has no
/// `ok_actions`, so a session that stays degraded pages once, stays red, and
/// clears silently after a healthy restart. Returns as soon as the session is
/// no longer degraded.
///
/// O(1) per tick: one atomic load and one counter increment.
// The slot decode is unit-tested
// (`live_universe_degraded_reason_decodes_every_reason_and_nothing_else`) and the spawn is
// pinned by `run_degraded_universe_heartbeat_is_spawned_by_main_while_degraded`.
// TEST-EXEMPT: timer loop over one atomic load and one counter increment.
pub async fn run_degraded_universe_heartbeat(reason: &'static str) {
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(
        DEGRADED_UNIVERSE_HEARTBEAT_SECS,
    ));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick completes immediately; the boot's own increment covers it.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        if live_universe_degraded_reason().is_none() {
            return;
        }
        metrics::counter!(MASTER_SOURCING_FALLBACK_COUNTER, "reason" => reason).increment(1);
    }
}

/// Is this boot one whose index-universe fallback is EXPECTED rather than a
/// defect?
///
/// True on a non-trading day, and on a trading day before the daily rider's
/// build hour (`[dhan_universe] target_secs_of_day_ist`). Either way today's
/// mapping artifact cannot exist yet, so a collapse to the index universe is
/// the correct outcome of THIS boot — and the scheduled 08:30 IST start is the
/// boot that widens the session.
///
/// # Why this exists (MEASURED 2026-09-23, CloudWatch alarm history)
///
/// `tv-<env>-errcode-ws-gap-03-universe-collapse` fired on every off-hours
/// boot: 2026-09-23 05:08 IST (Wed), Sun 2026-09-20, Sat 2026-09-19,
/// 2026-09-17 03:42 IST, Sun 2026-09-13, Sat 2026-09-12 and more. The app
/// log for each shows the same pair — "not waiting... the rider's build hour
/// is further away" then "mapping artifact is unusable — falling back" — and
/// the 08:30 boot of every one of those trading days widened correctly
/// (863/864 instruments). A page that fires every time the box is touched
/// outside the session and never once on a real collapse trains the operator
/// to ignore the one alarm that reports a 99.98% loss of market data.
///
/// # Honest residual
///
/// A pre-rider boot on a trading day that stays up INTO the session would
/// stay collapsed without a page. That shape needs the box to be started
/// before 08:00 IST and not restarted: the start-watchdog curfew stops it
/// outside the operating window and the 08:30 schedule starts a fresh boot,
/// which is judged on its own clock. A boot at or after the rider hour on a
/// trading day — including every mid-session restart — still pages.
#[must_use]
pub const fn collapse_is_expected_for_this_boot(
    trading_day: bool,
    now_ist_secs: u32,
    rider_target_ist_secs: u32,
) -> bool {
    !trading_day || now_ist_secs < rider_target_ist_secs
}

/// `source` label carried by the EXPECTED (pre-rider / non-trading-day)
/// fallback line. Deliberately NOT `fell_back_to_indices`, so the collapse
/// alarm's metric filter cannot match it.
pub const PRE_RIDER_BOOT_SOURCE: &str = "pre_rider_boot";

/// Resolve the session's subscription set, reading the master only when the
/// operator has turned that on.
///
/// Returns the index universe unchanged in every failure mode, always with a
/// logged reason.
// The parse, the selection, the envelope refusal and the fallback
// classification are all delegated to unit-tested pure fns above; this wrapper
// only reads a file and logs.
//
// The TEST-EXEMPT marker below MUST stay on the line immediately preceding
// `pub fn` — the guard reads exactly one line back. Inserting anything between
// them silently orphans the exemption and the function reads as newly untested,
// which is how this very block got separated from its function on 2026-08-14.
// TEST-EXEMPT: filesystem I/O — see the note above.
pub fn resolve_live_universe(
    cfg: &tickvault_common::config::DhanUniverseConfig,
    index_universe: Vec<SubscribeInstrument>,
    date_ist: &str,
    capacity: usize,
    collapse_expected: bool,
) -> Vec<SubscribeInstrument> {
    // Seed every fallback reason before the decision is made.
    //
    // This counter carries a LIVE CloudWatch alarm, and it is the one that
    // tells the operator the subscription set COLLAPSED from ~24,600
    // instruments to the 4 hardcoded index SIDs — a 99.98% loss of market data
    // that otherwise presents as a completely normal session, because 4
    // indices still tick and every other gauge stays green.
    //
    // The universe is resolved ONCE per boot, so a collapse is a single
    // increment on a series CloudWatch has never seen — and the agent drops
    // exactly that first sample. The one event this alarm exists for was the
    // one it could never report. Found by the 2026-08-29 adversarial sweep.
    //
    // All five reasons, because the agent's delta is computed per LABEL SET:
    // seeding one leaves the other four exactly as blind as before.
    for reason in MASTER_SOURCING_FALLBACK_REASONS {
        metrics::counter!(MASTER_SOURCING_FALLBACK_COUNTER, "reason" => *reason).increment(0);
    }

    if !cfg.live_subscription_from_master {
        tracing::info!(
            instruments = index_universe.len(),
            source = UniverseSource::HardcodedIndices.as_str(),
            "live universe: the 4 hardcoded index SIDs (master sourcing is switched \
             OFF in config; turning it on is a config change plus a restart)"
        );
        return index_universe;
    }

    // The narrowed spot universe (operator 2026-08-21): NSE indices + the F&O
    // stock underlyings, instead of every constituent the master resolves. It
    // is read from its OWN daily artifact, written by the same rider, in the
    // SAME shape — so `parse_mapping_artifact`, already hardened to fail loud
    // on garbage, stays the one parser for both files.
    //
    // An unreadable or unparseable F&O artifact falls THROUGH to the full
    // master, loudly, rather than to the index universe. That direction is
    // deliberate: the wide set is a superset, so no coverage is silently lost,
    // and if it does not fit the authorized envelope the capacity refusal below
    // reports it. The reverse — quietly narrowing when a file is missing —
    // would drop roughly 4,200 instruments with nothing anywhere to say so.
    let mut master: Option<Vec<MasterEntry>> = None;

    // NTM is tried FIRST and, when it succeeds, the F&O branch below never
    // runs. That ordering IS the precedence rule (operator 2026-08-22 is later
    // than 2026-08-21), and it is pinned by
    // `ntm_wins_when_both_narrowing_flags_are_on` so it cannot be reversed by
    // someone tidying the two branches into a different order.
    let mut ntm_used = false;
    if cfg.spot_universe_ntm_only {
        let ntm_path = crate::dhan_universe::ntm_spot_artifact_path(date_ist);
        match read_master_artifact(&ntm_path) {
            Ok(m) => {
                master = Some(m);
                ntm_used = true;
            }
            Err(failure) => {
                metrics::counter!(
                    MASTER_SOURCING_FALLBACK_COUNTER,
                    "reason" => failure.ntm_reason()
                )
                .increment(1);
                // COLLAPSE-ALARM-EXEMPT: this is a WIDENING, not a collapse.
                // The NTM narrowing was asked for and could not be applied, so
                // the session falls THROUGH to the full master-sourced set —
                // strictly more instruments than requested, never fewer. The
                // collapse alarm exists to page when the universe drops to the
                // 4 index SIDs; firing it here would page on a session that is
                // subscribing ~4,600 instruments instead of ~870, which is a
                // config observation, not an outage. The fallback COUNTER
                // (`tv_dhan_live_universe_master_fallback_total{reason}`)
                // carries this event.
                tracing::error!(
                    code = tickvault_common::error_code::ErrorCode::WsGapConnectionState.code_str(),
                    detail = failure.detail(),
                    path = %ntm_path.display(),
                    "live universe: the NTM spot set (NSE indices + Nifty Total Market) was \
                     REQUESTED but today's NTM artifact is unusable — falling through. This \
                     session subscribes MORE than was asked for, never less."
                );
            }
        }
    }

    if master.is_none() && cfg.spot_universe_fno_underlyings_only {
        let fno_path = crate::dhan_universe::fno_underlying_artifact_path(date_ist);
        match read_master_artifact(&fno_path) {
            Ok(m) => master = Some(m),
            Err(failure) => {
                // Counted but NOT gauged: this is not the final subscribed size,
                // it is a widening of what was asked for, and the success path
                // below publishes the real number.
                metrics::counter!(
                    MASTER_SOURCING_FALLBACK_COUNTER,
                    "reason" => failure.fno_reason()
                )
                .increment(1);
                // COLLAPSE-ALARM-EXEMPT: same shape as the NTM arm above — a
                // WIDENING. The narrowed indices+F&O-underlyings set was asked
                // for and is unavailable, so the session falls through to the
                // full master. More than requested, never less; not a collapse.
                tracing::error!(
                    code = tickvault_common::error_code::ErrorCode::WsGapConnectionState.code_str(),
                    detail = failure.detail(),
                    path = %fno_path.display(),
                    "live universe: the narrowed spot set (indices + F&O underlyings) was \
                     REQUESTED but today's F&O artifact is unusable — falling through to the \
                     full master-sourced set. This session subscribes MORE than was asked \
                     for, never less."
                );
            }
        }
    }
    let narrowed = master.is_some();
    // WHICH narrowing actually produced the master, for the log label.
    //
    // The label used to be a two-arm ternary — `narrowed ? "fno_underlyings"
    // : "full_master"` — with NO NTM arm, even though NTM is tried FIRST and
    // wins when both flags are on. So an NTM universe was reported as
    // `fno_underlyings`, naming a narrowing that had not run.
    //
    // MEASURED 2026-08-25: the box logged `spot_universe: "fno_underlyings",
    // instruments: 865` while the artifact line the same session read
    // `ntm_constituents: 746, nse_indices: 119` — 746 + 119 = exactly the 865
    // subscribed. NTM was working perfectly and the label said otherwise.
    //
    // That cost a real false finding: an audit read the label, concluded the
    // operator's NTM requirement was NOT being met, and was about to report a
    // gap that did not exist. A label is not decoration — it is what the next
    // reader trusts instead of re-deriving the answer.
    let spot_universe_label = if !narrowed {
        "full_master"
    } else if ntm_used {
        "ntm"
    } else {
        "fno_underlyings"
    };

    // Which day's list the session actually runs on. Today's in every normal
    // boot; an earlier day's when today's cannot be read (audit D3, below).
    let (master, artifact_date, spot_universe_label) = match master {
        Some(m) => (m, date_ist.to_owned(), spot_universe_label),
        None => {
            let path = crate::dhan_universe::mapping_artifact_path(date_ist);
            match read_master_artifact(&path) {
                Ok(m) => (m, date_ist.to_owned(), spot_universe_label),
                Err(failure) => {
                    // Audit D3 (owner, 2026-09-26: "no 4-index fallback").
                    // Before falling to the 4 index SIDs, take the newest
                    // earlier day's list the rider left on disk (it keeps 7
                    // days). That turns a 99.98% collapse into, at worst, a
                    // day-old list of stocks whose ids do not change.
                    if let Some(earlier) = newest_earlier_master_with(
                        cfg,
                        date_ist,
                        crate::dhan_universe::ARTIFACT_RETENTION_DAYS,
                        read_master_artifact,
                    ) {
                        if collapse_expected {
                            // Expected (pre-rider or non-trading-day boot):
                            // no page, same label the collapse filter ignores.
                            tracing::warn!(
                                code = tickvault_common::error_code::ErrorCode::WsGapConnectionState
                                    .code_str(),
                                source = PRE_RIDER_BOOT_SOURCE,
                                detail = failure.detail(),
                                path = %path.display(),
                                artifact_date = %earlier.date_ist,
                                spot_universe = earlier.kind,
                                entries = earlier.entries.len(),
                                "live universe: today's list does not exist yet (non-trading \
                                 day, or before the daily rider's build hour) — subscribing \
                                 the newest earlier list instead of the 4 index SIDs. \
                                 Expected for this boot."
                            );
                        } else {
                            // Pages through the fallback counter's alarm, with
                            // the same reason label as before. Counted only;
                            // the size gauge is published by the success path
                            // below with the real subscribed number.
                            metrics::counter!(
                                MASTER_SOURCING_FALLBACK_COUNTER,
                                "reason" => failure.mapping_reason()
                            )
                            .increment(1);
                            mark_live_universe_degraded(failure.mapping_reason());
                            tracing::error!(
                                code = tickvault_common::error_code::ErrorCode::WsGapConnectionState
                                    .code_str(),
                                source = EARLIER_ARTIFACT_SOURCE,
                                detail = failure.detail(),
                                path = %path.display(),
                                artifact_date = %earlier.date_ist,
                                spot_universe = earlier.kind,
                                entries = earlier.entries.len(),
                                "live universe: today's list is unusable — subscribing the \
                                 newest earlier list instead of the 4 index SIDs. Stocks \
                                 added today are missing and stocks removed today are still \
                                 subscribed until today's list is read."
                            );
                        }
                        (earlier.entries, earlier.date_ist, earlier.kind)
                    } else if collapse_expected {
                        // EXPECTED fallback: a non-trading day, or a trading
                        // day before the rider's build hour — today's artifact
                        // cannot exist yet. See
                        // `collapse_is_expected_for_this_boot`. The gauge is
                        // still published so the size is visible, but the
                        // alarmed fallback COUNTER is not moved and the line
                        // carries `source = pre_rider_boot`, which the
                        // collapse alarm's filter cannot match.
                        #[allow(clippy::cast_precision_loss)]
                        // APPROVED: bounded by MAX_DAILY_UNIVERSE_SIZE, far below 2^53.
                        metrics::gauge!(LIVE_UNIVERSE_SIZE_GAUGE).set(index_universe.len() as f64);
                        tracing::warn!(
                            code = tickvault_common::error_code::ErrorCode::WsGapConnectionState
                                .code_str(),
                            source = PRE_RIDER_BOOT_SOURCE,
                            detail = failure.detail(),
                            path = %path.display(),
                            "live universe: today's mapping artifact does not exist yet \
                             (non-trading day, or before the daily rider's build hour) and no \
                             earlier list is on disk — subscribing the index universe for this \
                             boot. Expected; the scheduled morning start is the boot that \
                             widens the session."
                        );
                        return index_universe;
                    } else {
                        // No earlier list on disk either: the old fallback, still paged.
                        record_master_sourcing_fallback(
                            failure.mapping_reason(),
                            index_universe.len(),
                        );
                        tracing::error!(
                            code =
                                tickvault_common::error_code::ErrorCode::WsGapConnectionState
                                    .code_str(),
                            // LOAD-BEARING FIELD — DO NOT REMOVE.
                            //
                            // `tv-<env>-errcode-ws-gap-03-universe-collapse` matches
                            // `{ $.code = "WS-GAP-03" && $.level = "ERROR"
                            //    && $.source = "fell_back_to_indices" }`
                            // (`error-code-alarms.tf:708`). WS-GAP-03 has ~50 emit
                            // sites, so the `source` term is what stops the alarm
                            // paging on ordinary reconnect churn — and it is
                            // therefore also what decides whether it can fire AT ALL.
                            //
                            // This field was MISSING here until 2026-09-06, and this
                            // is the arm that actually runs: the sibling arm below
                            // (master read OK, no usable widening) carried `source`
                            // and could page; THIS arm — today's artifact absent or
                            // unreadable, the overwhelmingly common case — could not.
                            //
                            // Proven on the box, 2026-09-05: the artifact was missing,
                            // this line fired twice with fields {code, detail, path}
                            // and no `source`, the session ran on 4 instruments
                            // instead of ~22,996, ZERO ticks were captured all day —
                            // and `describe-alarm-history` for the collapse alarm and
                            // for `tv-prod-live-universe-fallback` returns EMPTY. The
                            // operator was never paged for the failure both alarms
                            // exist to catch.
                            //
                            // The module doc above stated the opposite ("the OUTCOME
                            // is covered"), which is the false-OK class this
                            // repository keeps paying for: an alarm that is enabled,
                            // documented as covering the case, and structurally
                            // unable to match it.
                            source = UniverseSource::FellBackToIndices.as_str(),
                            detail = failure.detail(),
                            path = %path.display(),
                            "live universe: today's mapping artifact is unusable — falling back \
                             to the index universe. Master sourcing was REQUESTED and is NOT in \
                             effect; the subscribed set is the index universe, not the widened \
                             set."
                        );
                        return index_universe;
                    }
                }
            }
        }
    };

    let selection = select_live_universe(&index_universe, Some(&master), capacity);
    match selection.source {
        UniverseSource::MasterSourced => {
            // Publish the size on the SUCCESS path too, so the gauge is a live
            // reading of the universe rather than a fallback-only tripwire. A
            // metric that only ever appears when something breaks cannot be
            // compared against a known-good value.
            #[allow(clippy::cast_precision_loss)]
            // APPROVED: bounded by the capacity envelope, far below 2^53.
            metrics::gauge!(LIVE_UNIVERSE_SIZE_GAUGE).set(selection.instruments.len() as f64);
            report_universe_headroom(selection.instruments.len(), capacity);
            tracing::info!(
                instruments = selection.instruments.len(),
                master_entries = master.len(),
                spot_universe = spot_universe_label,
                artifact_date = %artifact_date,
                refused_zero_id = selection.refused_zero_id,
                refused_unknown_segment = selection.refused_unknown_segment,
                deduped = selection.deduped,
                capacity,
                source = selection.source.as_str(),
                "live universe: widened from the resolved master"
            );
        }
        UniverseSource::TruncatedToCapacity => {
            // Owner decision (audit D3): fill the envelope by priority and page
            // critically. Paged by the fallback counter's alarm; the line's
            // `source` is not the collapse filter's, because the session is
            // full, not collapsed.
            record_master_sourcing_fallback("truncated_to_capacity", selection.instruments.len());
            tracing::error!(
                code = tickvault_common::error_code::ErrorCode::WsGapConnectionState.code_str(),
                instruments = selection.instruments.len(),
                refused_over_capacity = selection.refused_over_capacity,
                master_entries = master.len(),
                spot_universe = spot_universe_label,
                artifact_date = %artifact_date,
                capacity,
                source = selection.source.as_str(),
                "live universe: the resolved list is larger than the authorized main-feed \
                 capacity — the capacity is filled by priority (indices first) and the rest \
                 are NOT subscribed this session"
            );
        }
        UniverseSource::HardcodedIndices | UniverseSource::FellBackToIndices => {
            record_master_sourcing_fallback("no_usable_widening", selection.instruments.len());
            tracing::error!(
                code = tickvault_common::error_code::ErrorCode::WsGapConnectionState.code_str(),
                master_entries = master.len(),
                spot_universe = spot_universe_label,
                artifact_date = %artifact_date,
                refused_zero_id = selection.refused_zero_id,
                refused_unknown_segment = selection.refused_unknown_segment,
                deduped = selection.deduped,
                capacity,
                source = UniverseSource::FellBackToIndices.as_str(),
                "live universe: master sourcing was REQUESTED but the list produced no \
                 usable widening — subscribing the index universe instead. This is NOT a \
                 widened session."
            );
        }
    }
    selection.instruments
}

// ===========================================================================
// Boot-time wait for today's mapping artifact
// ===========================================================================

/// How long the boot path waits for today's mapping artifact before giving up
/// and booting on the index universe.
///
/// # Why this is 600 s and no longer 120 s (raised 2026-08-21)
///
/// The old 120 s was derived honestly and then went stale. It was sized from a
/// real measurement — the 2026-08-18 production build took **9 seconds** end to
/// end (`downloading instrument master` 08:11:48 → `instrument mapping written`
/// 08:11:57) — and 120 s was ~13× that.
///
/// Two things then landed on top of that build and nobody re-derived the
/// number:
///
/// * the instrument-lifecycle write (#1773, 2026-08-20) — a ~150,000-row
///   QuestDB round trip. It did not exist when 120 s was chosen. It has since
///   been moved BELOW the artifact write (`dhan_universe::build_once`), so it
///   is off this critical path again;
/// * the every-NSE-index expansion (#1790) — up to 49 index-constituent CSVs
///   fetched SEQUENTIALLY, each with its own timeout. These still run BEFORE
///   the artifact, and they are now the bulk of the build.
///
/// A budget measured against a 9-second build cannot describe that, and the
/// cost of being wrong is the whole session: the lane reads the artifact ONCE
/// at boot, so a timeout pins it to 4 index SIDs until someone restarts the
/// process. `dhan_universe::BUILD_DEADLINE_SECS` — the build's own budget for
/// the same work — is 900 s, so the two numbers disagreed by 7.5×.
///
/// 600 s is measured against the SLOW path rather than the fast one, and it
/// still costs the session nothing on a normal morning: the box starts at
/// 08:30 IST, so even a full timeout lands at ~08:41, well before the 09:15
/// open. Waiting is strictly cheaper than the fallback it avoids.
pub const MAPPING_WAIT_DEADLINE_SECS: u64 = 600;

/// Wall-clock IST second-of-day past which the boot wait stops regardless of
/// how much of the deadline is left. 09:10 IST.
///
/// The raised deadline above is safe on a normal 08:30 boot and dangerous on a
/// late one: a box that comes up at 09:05 would otherwise still be waiting when
/// the market opens, and dialing nothing at 09:15 is worse than dialing a
/// partial set. So the wait is bounded by BOTH a duration and a clock, and
/// whichever arrives first wins.
///
/// 09:10 leaves five minutes to resolve the universe, plan the pool and dial
/// the sockets before the first tick. This makes the raise strictly safer than
/// the 120 s it replaces in both directions: a normal morning gets 5× the
/// budget, and a late morning is cut off EARLIER than a flat 600 s would.
pub const MAPPING_WAIT_NEVER_PAST_IST_SECS: u32 = 9 * 3_600 + 10 * 60;

/// Poll cadence while waiting. Cold path, once per boot: at the deadline above
/// this is at most 240 `stat` calls total.
const MAPPING_POLL_INTERVAL_MS: u64 = 500;

/// How long the boot may settle-poll for the NARROWED spot artifact after the
/// mapping artifact has appeared.
///
/// # The race this closes (2026-09-13)
///
/// `await_mapping_artifact` waits on `dhan-mapping-<date>.json`, but with
/// `spot_universe_ntm_only` the consumer -- `resolve_live_universe` -- reads
/// `dhan-ntm-spot-<date>.json` FIRST. The rider writes BOTH inside a single
/// `write_mapping_atomic` call, the mapping first and the narrowed sets a few
/// milliseconds later, so for that brief window the file this function waits
/// on EXISTS while the file the consumer reads does NOT. The 54 lines between
/// the wait returning and the read are pure in-memory setup, so the read lands
/// microseconds later -- there is nothing to absorb the gap.
///
/// Losing that race is SILENT: the NTM read fails, the consumer takes its
/// documented widening fallback, and the session subscribes the full
/// master-sourced set (~4,565 mapping rows) instead of the operator-locked NTM
/// set (~870 instruments). It is a widening, so no collapse alarm fires, and
/// the only signal is a counter that reaches no CloudWatch alarm.
///
/// # Why a bounded SETTLE and not a wait on the narrowed artifact itself
///
/// The rider deliberately does NOT write the NTM artifact when the list
/// resolves zero constituents -- refusing to write it is what makes the
/// consumer fall through loudly instead of accepting an indices-only set that
/// looks like a successful narrowing. So a plain wait on that path would stall
/// boot to the 09:10 cutoff on a day the rider behaved correctly. A short
/// bounded settle fixes the millisecond race without ever paying the deadline
/// for a legitimately-absent file.
const NARROWED_ARTIFACT_SETTLE_MAX_MS: u64 = 5_000;

/// Poll cadence inside the settle window.
///
/// Deliberately 10x tighter than [`MAPPING_POLL_INTERVAL_MS`]: the gap being
/// closed is a few milliseconds of one `write_mapping_atomic` call, so a 500 ms
/// cadence would be most of the window it is meant to cover. At the budget
/// above this is at most 100 `stat` calls, once per boot, on a cold path.
const NARROWED_ARTIFACT_POLL_INTERVAL_MS: u64 = 50;

/// The longest this function may stall boot, in seconds. One hour.
///
/// [`mapping_wait_end_ist_secs`] extends the wait to cover the rider's build
/// hour, and without a ceiling that extension would stall an overnight boot
/// until morning. This is the ceiling, and the number is chosen from the
/// MEASURED spread rather than picked round: on 2026-09-07 the boot that
/// deserved the extension needed **35.7 minutes** of it and the three that
/// deserved nothing were **4h37, 5h02 and 7h04** away. One hour sits in the
/// middle of a gap four hours wide, so this is not a knife-edge.
pub const MAPPING_WAIT_MAX_STALL_SECS: u32 = 3_600;

/// Decide the IST second-of-day at which the boot wait must stop, or `None`
/// when waiting cannot pay.
///
/// # The defect this closes (MEASURED, 2026-09-07)
///
/// The wait used to be bounded by a duration measured from BOOT, and that is
/// the wrong clock: the artifact has one producer, the daily rider, and the
/// rider does not write before its own target hour. A boot that starts more
/// than [`MAPPING_WAIT_DEADLINE_SECS`] before that target therefore burned the
/// entire deadline against a producer that had not run, gave up, and collapsed
/// the session to 4 index SIDs.
///
/// Every boot on 2026-09-07, from the app log, against a rider target of 08:00:
///
/// | Boot (IST) | Old bound | Outcome then | This function |
/// |---|---|---|---|
/// | 01:06:22 | 01:16 | timed out, collapsed | **no wait** — 7h04 away |
/// | 03:08:30 | 03:19 | timed out, collapsed | **no wait** — 5h02 away |
/// | 03:32:33 | 03:43 | timed out, collapsed | **no wait** — 4h37 away |
/// | **07:34:15** | **07:44** | **timed out, collapsed** | **wait to 08:10** |
/// | 08:09:28 | — | artifact present, widened | unchanged |
///
/// The 07:34 boot is the one that mattered: it gave up sixteen minutes before
/// the artifact existed, and only a manual restart at 08:09 rescued the
/// session. The three overnight boots gain something smaller but real — they
/// stop stalling boot for ten minutes apiece to reach a fallback that was
/// certain from the first poll.
///
/// # Why the three clauses, and what each one is for
///
/// * Past [`MAPPING_WAIT_NEVER_PAST_IST_SECS`] nothing waits at all — dialing a
///   partial set beats dialing nothing at 09:15. That bound is unchanged and
///   still wins over everything below.
/// * The end is pushed out to `rider_target + MAPPING_WAIT_DEADLINE_SECS` so
///   the rider gets its full build budget from the moment it may start, rather
///   than from the moment we happened to boot.
/// * [`MAPPING_WAIT_MAX_STALL_SECS`] caps the result, because an overnight boot
///   is hours from any producer and must not hold boot open until morning.
///
/// Returns the end instant as an IST second-of-day, so the caller compares
/// wall clock to wall clock and no elapsed-time accounting can drift from it.
#[must_use]
pub fn mapping_wait_end_ist_secs(now_ist_secs: u32, rider_target_ist_secs: u32) -> Option<u32> {
    // The clock bound is absolute and is checked first: a boot already past it
    // must not wait even a single poll interval.
    if now_ist_secs >= MAPPING_WAIT_NEVER_PAST_IST_SECS {
        return None;
    }

    // Saturating throughout: `now + deadline` is at most 86_400 + 600 and the
    // target is operator config, so neither can overflow a u32 in practice —
    // but a wrapped bound here would silently produce a wait of the wrong
    // length, which is exactly the class of defect this function exists to fix.
    let from_boot =
        now_ist_secs.saturating_add(u32::try_from(MAPPING_WAIT_DEADLINE_SECS).unwrap_or(u32::MAX));
    let from_rider = rider_target_ist_secs
        .saturating_add(u32::try_from(MAPPING_WAIT_DEADLINE_SECS).unwrap_or(u32::MAX));

    let end = from_boot
        .max(from_rider)
        .min(MAPPING_WAIT_NEVER_PAST_IST_SECS);

    // `end` is strictly greater than `now` here: it is at least `from_boot`
    // capped at the cutoff, and the arm above proved `now < cutoff`.
    let stall = end.saturating_sub(now_ist_secs);
    if stall > MAPPING_WAIT_MAX_STALL_SECS {
        return None;
    }

    Some(end)
}

/// Counter: how each boot's wait for the mapping artifact ended.
///
/// `outcome` is one of `not_requested`, `rider_disabled`, `already_present`,
/// `became_ready`, `timed_out`, `pre_open_cutoff`.
///
/// # NOT shipped to CloudWatch — deliberately, and this is a real limitation
///
/// This counter is readable on the box's `/metrics` endpoint and nowhere else.
///
/// ⚠ CORRECTED 2026-09-01 — the reason recorded here is DEAD, and it had
/// already been dead for a week when it was blocking this.
///
/// It used to read: *"Adding it to the EMF `metric_selectors` list was
/// attempted and REVERTED: `user-data.sh.tftpl` currently renders to 15,870
/// bytes against a 15,872 byte budget."* Both halves stopped being true on
/// 2026-08-25, when the selector moved OUT of that template into
/// `deploy/aws/cloudwatch-agent.json` — a guard now *forbids* a second copy in
/// the template. Measured 2026-09-01 by running the size guard: the template
/// renders **13,869 of 15,872 bytes, with 2,003 free**, and adding an EMF name
/// costs **zero** user-data bytes.
///
/// # The real blocker today is COST, not bytes — and it is smaller than it looks
///
/// An EMF name is ~$0.30/mo against a maximal month already ~$8.58 above the
/// automatic `STOP_EC2_INSTANCES` line. That is an operator decision, not an
/// executor one, so it is stated rather than taken.
///
/// It is also less urgent than it reads. The consequence this counter reports
/// — a boot that timed out waiting for the mapping artifact — is exactly the
/// universe-collapse case, and that case DOES page: the fall-back arm emits
/// `WS-GAP-03` with `source = "fell_back_to_indices"`, which has a CloudWatch
/// metric filter and an alarm. So the OUTCOME is covered; what is missing is
/// the ability to see the near-misses that did not collapse.
///
/// **⚠ CORRECTED 2026-09-06 — "the OUTCOME is covered" was FALSE for the arm
/// that actually runs, and it cost a whole trading session.**
///
/// `resolve_live_universe` has TWO fallback arms. The one reached when the
/// master was read but produced no usable widening carried
/// `source = selection.source.as_str()` and could match the alarm. The one
/// reached when today's artifact is ABSENT or unreadable — the common case, and
/// the one this very counter exists to describe — carried only
/// `{code, detail, path}`. The alarm's filter requires `$.source`, so that arm
/// was structurally unable to fire.
///
/// Measured on the box, 2026-09-05: the artifact was missing, the uncovered arm
/// fired twice, the session ran on **4 instruments instead of ~22,996**, and
/// `tv_dhan_feed_ingest_ticks_total` finished the day at **0**.
/// `describe-alarm-history` returns EMPTY for BOTH
/// `tv-prod-errcode-ws-gap-03-universe-collapse` and
/// `tv-prod-live-universe-fallback` across that entire day. Nobody was paged
/// for the failure both alarms exist to catch.
///
/// The `source` field is now on both arms and pinned by
/// [`tests::every_ws_gap_03_error_in_this_module_carries_the_source_the_alarm_filters_on`].
/// The paragraph above is left standing rather than rewritten because the
/// failure it describes is the reusable part: a documented, enabled alarm that
/// cannot match its own emit site reads greener than no alarm at all.
///
/// The reusable lesson is the one this correction exists for: a MEASUREMENT
/// copied into a justification carries no date, and this one outlived the
/// thing it measured by a week while still stopping work.
pub const MAPPING_WAIT_COUNTER: &str = "tv_dhan_live_universe_mapping_wait_total";

/// Wait — bounded — for today's mapping artifact to exist before the lane
/// resolves its subscription set.
///
/// # The race this closes
///
/// The daily rider and the live lane are started from the same boot. The rider
/// *writes* `dhan-nse-mapping-<today>.json`; the lane *reads* it. On
/// 2026-08-18 the lane read it at 08:11:48 and the rider wrote it at 08:11:57
/// — the lane lost by 9 seconds, fell back to 4 instruments, and only reached
/// the full 4,565 because the box happened to boot a second time at 08:31.
///
/// Because the filename is date-stamped, yesterday's artifact cannot stand in
/// for today's, so a box that boots **once** — the normal case — loses the race
/// every single day. It is not intermittent. Production evidence: on
/// 2026-08-17 the single 08:31:37 boot fell back and the **entire trading
/// session ran on 4 instruments instead of 4,565**, with no page, because
/// `MASTER_SOURCING_FALLBACK_COUNTER` has no alarm attached.
///
/// # Why a poll and not a signal from the rider
///
/// The artifact is the real contract here, and it has more than one legitimate
/// producer: today's rider, or an earlier boot of the same IST day. A readiness
/// channel would only cover the first. Polling the file covers both, and stays
/// correct if the rider is ever restarted by its supervisor mid-build.
///
/// # This never blocks boot
///
/// Every exit is bounded and logged. If the flag is off, the rider is
/// disabled, or the deadline expires, this returns and
/// [`resolve_live_universe`] takes its existing fallback path — which already
/// logs the collapse at `error!` and counts it. The wait can only ever turn a
/// *guaranteed* fallback into a *possible* one; it can never turn a working
/// boot into a failing one.
// The behaviour this wraps is pinned by `universe_boot_race_guard.rs`; there is
// no pure decision to isolate here, since every branch is an I/O or a timer
// observation. The marker below MUST stay on the line immediately preceding
// `pub async fn` — the guard reads exactly one line back, and anything inserted
// between them silently orphans the exemption.
// TEST-EXEMPT: filesystem polling + wall-clock sleep — see the note above.
/// Counter for the narrowed-artifact settle (see [`NARROWED_ARTIFACT_SETTLE_MAX_MS`]).
///
/// `absent` is the one outcome worth reading: it means the mapping landed, the
/// config asked for a narrowed spot set, and the narrowed file never appeared
/// inside the settle window -- so this session widened. That is legitimate on a
/// zero-constituent day and a defect otherwise, and before this counter existed
/// the two were indistinguishable.
pub const NARROWED_SETTLE_COUNTER: &str = "tv_dhan_live_universe_narrowed_settle_total";

/// Settle-poll for the narrowed spot artifact the CONFIG will actually read.
///
/// Called only once the mapping artifact is known to exist. Returns as soon as
/// the selected artifact appears, or after [`NARROWED_ARTIFACT_SETTLE_MAX_MS`],
/// whichever is first. Never stalls boot past that bound.
async fn settle_for_narrowed_spot_artifact(
    cfg: &tickvault_common::config::DhanUniverseConfig,
    date_ist: &str,
) {
    // Mirrors the precedence in `resolve_live_universe`: NTM wins when both
    // narrowing flags are on (operator 2026-08-22 is later than 2026-08-21).
    // If that precedence is ever reversed there, reverse it here too -- this
    // function exists to wait for the file that one READS.
    let (path, which) = if cfg.spot_universe_ntm_only {
        (
            crate::dhan_universe::ntm_spot_artifact_path(date_ist),
            "ntm",
        )
    } else if cfg.spot_universe_fno_underlyings_only {
        (
            crate::dhan_universe::fno_underlying_artifact_path(date_ist),
            "fno",
        )
    } else {
        metrics::counter!(NARROWED_SETTLE_COUNTER, "outcome" => "not_narrowed").increment(1);
        return;
    };

    if path.exists() {
        metrics::counter!(NARROWED_SETTLE_COUNTER, "outcome" => "already_present").increment(1);
        return;
    }

    let started = std::time::Instant::now();
    let budget = std::time::Duration::from_millis(NARROWED_ARTIFACT_SETTLE_MAX_MS);
    let interval = std::time::Duration::from_millis(NARROWED_ARTIFACT_POLL_INTERVAL_MS);

    while started.elapsed() < budget {
        tokio::time::sleep(interval).await;
        if path.exists() {
            metrics::counter!(NARROWED_SETTLE_COUNTER, "outcome" => "settled").increment(1);
            tracing::info!(
                which,
                settled_ms = started.elapsed().as_millis() as u64,
                path = %path.display(),
                "live universe: the narrowed spot artifact landed just after the mapping -- \
                 waited for it rather than reading a half-written build and widening"
            );
            return;
        }
    }

    metrics::counter!(NARROWED_SETTLE_COUNTER, "outcome" => "absent").increment(1);
    // Deliberately NOT an error: the rider refuses to write this file when the
    // list resolves zero constituents, and that refusal is correct behaviour.
    // `resolve_live_universe` emits the loud, labelled widening line moments
    // later; a second line here would double-report one event.
    tracing::info!(
        which,
        settle_ms = NARROWED_ARTIFACT_SETTLE_MAX_MS,
        path = %path.display(),
        "live universe: the narrowed spot artifact did not appear within the settle window -- \
         this session will widen to the master-sourced set and say so"
    );
}

pub async fn await_mapping_artifact(
    cfg: &tickvault_common::config::DhanUniverseConfig,
    date_ist: &str,
    collapse_expected: bool,
) {
    // Nothing to wait for: the lane is not master-sourced this boot.
    if !cfg.live_subscription_from_master {
        metrics::counter!(MAPPING_WAIT_COUNTER, "outcome" => "not_requested").increment(1);
        return;
    }

    let path = crate::dhan_universe::mapping_artifact_path(date_ist);

    if path.exists() {
        metrics::counter!(MAPPING_WAIT_COUNTER, "outcome" => "already_present").increment(1);
        tracing::info!(
            path = %path.display(),
            "live universe: today's mapping artifact is already on disk — no wait needed"
        );
        // The mapping is not the file the consumer reads when a narrowing flag
        // is on. Settle for that one too -- see `NARROWED_ARTIFACT_SETTLE_MAX_MS`.
        settle_for_narrowed_spot_artifact(cfg, date_ist).await;
        return;
    }

    // The rider is the only writer. With it disabled nobody will ever produce
    // the artifact, so waiting would burn the full deadline to reach the same
    // fallback. Fail fast and say why.
    if !cfg.enabled {
        metrics::counter!(MAPPING_WAIT_COUNTER, "outcome" => "rider_disabled").increment(1);
        // COLLAPSE-ALARM-EXEMPT: a PREDICTION, not the outcome. This function
        // only decides how long to wait; it subscribes nothing. Every one of
        // its three give-up arms returns straight into `resolve_live_universe`,
        // which reads the same absent artifact and emits the collapse line that
        // DOES carry `source = fell_back_to_indices`. Labelling the prediction
        // too would page twice for one collapse, and would page even in the
        // race where the rider lands the artifact between the give-up and the
        // read — a page for a session that widened correctly.
        tracing::error!(
            code = tickvault_common::error_code::ErrorCode::WsGapConnectionState.code_str(),
            path = %path.display(),
            "live universe: master sourcing is REQUESTED but [dhan_universe] enabled = false, \
             so nothing will ever write today's mapping. Not waiting — the lane will subscribe \
             the newest earlier list on disk, or the 4 index SIDs if there is none. Enable the rider or turn master sourcing off; the two flags \
             disagree."
        );
        return;
    }

    let now_ist = tickvault_common::market_hours::now_ist_secs_of_day();
    let Some(end_ist) = mapping_wait_end_ist_secs(now_ist, cfg.target_secs_of_day_ist) else {
        metrics::counter!(MAPPING_WAIT_COUNTER, "outcome" => "producer_too_far").increment(1);
        if collapse_expected {
            // The overnight / weekend shape: nothing is wrong, the rider has
            // simply not run yet. Logged, never an ERROR.
            tracing::info!(
                now_ist_secs = now_ist,
                rider_target_ist_secs = cfg.target_secs_of_day_ist,
                path = %path.display(),
                "live universe: not waiting for today's mapping — non-trading day or before \
                 the daily rider's build hour. This boot subscribes the newest earlier list \
                 (or the index universe if none is on disk); the scheduled morning start \
                 widens the session."
            );
            return;
        }
        // COLLAPSE-ALARM-EXEMPT: prediction, not outcome — see the arm at the
        // top of this function. `resolve_live_universe` emits the labelled
        // collapse line immediately after this returns.
        tracing::error!(
            code = tickvault_common::error_code::ErrorCode::WsGapConnectionState.code_str(),
            now_ist_secs = now_ist,
            rider_target_ist_secs = cfg.target_secs_of_day_ist,
            path = %path.display(),
            "live universe: not waiting for today's mapping — the daily rider's build hour is \
             further away than this boot may stall for, so no amount of waiting can produce the \
             artifact. Subscribing the newest earlier list, or the 4 index SIDs if none is on \
             disk. This is the expected shape for an overnight or off-hours boot; the scheduled morning start is what widens the session."
        );
        return;
    };

    tracing::info!(
        path = %path.display(),
        now_ist_secs = now_ist,
        end_ist_secs = end_ist,
        rider_target_ist_secs = cfg.target_secs_of_day_ist,
        "live universe: today's mapping artifact is not written yet — waiting for the daily \
         rider before subscribing, so the lane does not lose the boot race and collapse to 4 \
         instruments"
    );

    let started = std::time::Instant::now();
    let interval = std::time::Duration::from_millis(MAPPING_POLL_INTERVAL_MS);

    // ONE clock, not two. The end instant already carries the rider's build
    // hour AND the 09:10 cutoff, so the loop compares wall clock to wall clock
    // and there is no elapsed-time bound left to disagree with it — that
    // disagreement is exactly what cost 2026-09-07 its first four boots.
    while tickvault_common::market_hours::now_ist_secs_of_day() < end_ist {
        tokio::time::sleep(interval).await;
        if path.exists() {
            metrics::counter!(MAPPING_WAIT_COUNTER, "outcome" => "became_ready").increment(1);
            tracing::info!(
                waited_secs = started.elapsed().as_secs_f64(),
                path = %path.display(),
                "live universe: mapping artifact is ready — subscribing the widened set"
            );
            // The mapping landing does NOT mean the narrowed set has landed --
            // the rider writes them milliseconds apart inside one call, and this
            // poll can land between the two. See `NARROWED_ARTIFACT_SETTLE_MAX_MS`.
            settle_for_narrowed_spot_artifact(cfg, date_ist).await;
            return;
        }
    }

    // The two give-up shapes are NOT the same operator problem, so they keep
    // separate labels: the cutoff means the box booted late or the rider is
    // failing, while the rider budget means the rider ran and did not finish.
    // Classified at the single exit rather than by a second in-loop arm — the
    // end instant is capped at the cutoff, so an in-loop cutoff check could
    // never fire and would read as live cover it does not provide.
    if end_ist >= MAPPING_WAIT_NEVER_PAST_IST_SECS {
        metrics::counter!(MAPPING_WAIT_COUNTER, "outcome" => "pre_open_cutoff").increment(1);
        // COLLAPSE-ALARM-EXEMPT: prediction, not outcome — see the arm at the
        // top of this function. `resolve_live_universe` emits the labelled
        // collapse line immediately after this returns.
        tracing::error!(
            code = tickvault_common::error_code::ErrorCode::WsGapConnectionState.code_str(),
            waited_secs = started.elapsed().as_secs_f64(),
            path = %path.display(),
            "live universe: stopped waiting for today's mapping at the 09:10 IST pre-open \
             cutoff — the market opens at 09:15 and dialing a partial set beats dialing \
             nothing. This session subscribes the newest earlier list, or the 4 index SIDs \
             if none is on disk. It means the box booted late or the daily rider is failing; the rider keeps retrying, but the lane \
             reads the artifact once at boot, so only a restart widens this session."
        );
        return;
    }

    metrics::counter!(MAPPING_WAIT_COUNTER, "outcome" => "timed_out").increment(1);
    // COLLAPSE-ALARM-EXEMPT: prediction, not outcome — see the arm at the top
    // of this function. `resolve_live_universe` emits the labelled collapse
    // line immediately after this returns.
    tracing::error!(
        code = tickvault_common::error_code::ErrorCode::WsGapConnectionState.code_str(),
        waited_secs = started.elapsed().as_secs_f64(),
        end_ist_secs = end_ist,
        path = %path.display(),
        "live universe: the daily rider did not produce today's mapping within \
         {MAPPING_WAIT_DEADLINE_SECS}s of its own build hour. Subscribing the newest earlier list, or the 4 \
         index SIDs if none is on disk. The rider keeps retrying, but the lane reads the \
         artifact once at boot — so this session stays on that list until a restart."
    );
}

#[cfg(test)]
mod tests {

    /// The live rider target, 08:00 IST (`config/base.toml`
    /// `[dhan_universe] target_secs_of_day_ist = 28800`).
    const RIDER_TARGET: u32 = 8 * 3_600;

    /// Every boot of 2026-09-07, replayed against the rule that now decides.
    ///
    /// These are not invented fixtures: each row is a real `await_mapping_artifact`
    /// log line from `/tickvault/prod/app` that day. Four of the five collapsed
    /// the session to 4 index SIDs, and the fifth only widened because a human
    /// restarted the box.
    #[test]
    fn the_five_boots_of_2026_09_07_resolve_the_way_the_evidence_says_they_should() {
        // 01:06:22, 03:08:30, 03:32:33 — overnight. The rider cannot write for
        // hours, so the old 600 s wait was ten minutes spent reaching a
        // certainty. Not waiting is the honest answer AND the faster one.
        for (h, m, s) in [(1, 6, 22), (3, 8, 30), (3, 32, 33)] {
            let now = h * 3_600 + m * 60 + s;
            assert_eq!(
                super::mapping_wait_end_ist_secs(now, RIDER_TARGET),
                None,
                "a {h:02}:{m:02} boot is hours from the rider's build hour — waiting cannot \
                 produce the artifact, so boot must not stall for it"
            );
        }

        // 07:34:15 — THE ONE THAT MATTERED. It gave up at 07:44 and the
        // artifact appeared by 08:09. The rule must now carry it past 08:00.
        let boot = 7 * 3_600 + 34 * 60 + 15;
        let end = super::mapping_wait_end_ist_secs(boot, RIDER_TARGET)
            .expect("the 07:34 boot is inside the stall ceiling and must wait");
        assert_eq!(
            end,
            RIDER_TARGET + 600,
            "the wait must end at the rider's build hour plus its build budget (08:10), not \
             at boot plus the same budget (07:44) — the artifact did not exist at 07:44"
        );
        assert!(
            end > 8 * 3_600,
            "an end at or before 08:00 cannot see an artifact the rider writes at 08:00"
        );

        // 08:09:28 — the rescue boot. It found the artifact in 3.3 s, so it
        // never reaches this function's wait path; the rule must still permit
        // a wait, or a restart at that minute would refuse to look at all.
        let rescue = 8 * 3_600 + 9 * 60 + 28;
        assert!(
            super::mapping_wait_end_ist_secs(rescue, RIDER_TARGET).is_some(),
            "a boot minutes after the build hour must still be willing to wait"
        );
    }

    /// The stall ceiling is what stops the rider extension from holding boot
    /// open until morning, and it must bite on duration alone.
    #[test]
    fn mapping_wait_end_ist_secs_caps_the_rider_extension_at_the_stall_ceiling() {
        // Exactly at the ceiling: an 07:10 boot is 3,600 s from 08:10.
        let at_ceiling = RIDER_TARGET + 600 - super::MAPPING_WAIT_MAX_STALL_SECS;
        assert!(
            super::mapping_wait_end_ist_secs(at_ceiling, RIDER_TARGET).is_some(),
            "a stall of exactly MAPPING_WAIT_MAX_STALL_SECS is permitted — the ceiling is a \
             maximum, not an exclusive bound"
        );
        assert_eq!(
            super::mapping_wait_end_ist_secs(at_ceiling - 1, RIDER_TARGET),
            None,
            "one second over the ceiling must refuse to wait"
        );
    }

    /// The 09:10 cutoff outranks everything, in both directions.
    #[test]
    fn the_pre_open_cutoff_still_wins() {
        assert_eq!(
            super::mapping_wait_end_ist_secs(super::MAPPING_WAIT_NEVER_PAST_IST_SECS, RIDER_TARGET),
            None,
            "a boot at the cutoff must not wait even one poll interval"
        );
        assert_eq!(
            super::mapping_wait_end_ist_secs(9 * 3_600 + 30 * 60, RIDER_TARGET),
            None,
            "a boot after the open must dial immediately — a partial set beats nothing"
        );

        // A misconfigured target LATER than the cutoff must not push the wait
        // past it. The cutoff is the safety property; the target is a hint.
        let late_target = 10 * 3_600;
        let end = super::mapping_wait_end_ist_secs(9 * 3_600, late_target)
            .expect("an 09:00 boot is 10 minutes from the cutoff, inside the ceiling");
        assert_eq!(
            end,
            super::MAPPING_WAIT_NEVER_PAST_IST_SECS,
            "a rider target past 09:10 must be clamped to the cutoff, never followed"
        );
    }

    /// A boot after the build hour keeps the behaviour it already had.
    #[test]
    fn a_boot_after_the_build_hour_is_unchanged() {
        // The scheduled 08:30 morning start: end is boot + the deadline, as
        // before. The rider extension must not lengthen a wait that already
        // starts after the producer could have run.
        let scheduled = 8 * 3_600 + 30 * 60;
        assert_eq!(
            super::mapping_wait_end_ist_secs(scheduled, RIDER_TARGET),
            Some(scheduled + 600),
            "the normal morning boot must keep its boot-relative budget — the rider hour is \
             already behind it, so extending to it would SHORTEN the wait"
        );
    }

    /// Every `WS-GAP-03` error in this module must carry the `source` field the
    /// CloudWatch filter selects on.
    ///
    /// `tv-<env>-errcode-ws-gap-03-universe-collapse` matches
    /// `{ $.code = "WS-GAP-03" && $.level = "ERROR" && $.source = "..." }`.
    /// `WS-GAP-03` has ~50 emit sites across the workspace, so the `source`
    /// term is the only thing keeping that alarm off ordinary reconnect churn
    /// — which makes an emit site WITHOUT it invisible to the alarm rather
    /// than merely under-labelled.
    ///
    /// That is not hypothetical. Until 2026-09-06 the missing-artifact arm of
    /// `resolve_live_universe` omitted it. On 2026-09-05 that arm fired twice,
    /// the session collapsed from ~22,996 instruments to 4, the day captured
    /// ZERO ticks, and `describe-alarm-history` for the collapse alarm is
    /// EMPTY for that entire day.
    ///
    /// Deliberately a SOURCE SCAN rather than a log-capture assertion: what
    /// broke was a missing field in one arm among several, and the only
    /// property worth pinning is "no arm is missing it". A behavioural test
    /// would have to enumerate the arms, which is the thing that went wrong.
    ///
    /// Two arms legitimately have no `source`, and both are labelled at the
    /// site with `COLLAPSE-ALARM-EXEMPT:` plus a reason — the house
    /// `// APPROVED:` shape, so the exemption travels with the code instead of
    /// living in a line-number list that goes stale on the next edit:
    ///
    /// * the NTM and F&O narrowing arms, which fall through to a WIDER set —
    ///   labelling them would page on a session subscribing MORE than asked;
    /// * the three give-up arms of `await_mapping_artifact`, which predict a
    ///   collapse that `resolve_live_universe` then emits, labelled, moments
    ///   later — labelling them would page twice for one event.
    #[test]
    fn every_ws_gap_03_error_in_this_module_carries_the_source_the_alarm_filters_on() {
        let src = include_str!("dhan_live_universe.rs");
        let mut offenders: Vec<String> = Vec::new();
        let mut covered = 0usize;
        let mut exempt = 0usize;

        // The needle is SPLIT on purpose — do not join it back into one
        // literal. Written whole it reads to `error_code_tag_guard` as a real
        // emit site (the guard matches the bare macro name too, so the split has
        // to fall INSIDE the word), and the assertion message below mentions the
        // tracked code, so that guard reports this scanner as a macro call
        // missing its `code` field. It failed CI exactly that way on
        // 2026-09-06. Splitting the needle removes the false match rather than
        // suppressing a true-looking one with the guard's `APPROVED` escape
        // hatch, which the next reader would take to mean "this emit site is
        // allowed to have no code field".
        let needle = concat!("tracing::err", "or!(");
        for (idx, _) in src.match_indices(needle) {
            // Take the macro invocation by matching parens from the opening
            // one, so a nested call or a string containing a paren cannot end
            // the block early.
            let rest = &src[idx..];
            let open = rest.find('(').expect("match_indices guarantees a paren");
            let mut depth = 0i32;
            let mut end = rest.len();
            for (i, c) in rest.char_indices().skip(open) {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = i + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let block = &rest[..end];
            if !block.contains("WsGapConnectionState") {
                continue;
            }
            // Strip line comments before looking for the field. The collapse
            // arm carries a long comment that QUOTES the alarm's filter
            // pattern (`$.source = "fell_back_to_indices"`), so a naive
            // substring check counts that site as covered even after the real
            // field is deleted — proven by bite-test on 2026-09-06, where
            // removing the field left this guard green. A check satisfied by
            // prose about a field is the same false-OK class as the alarm this
            // guard exists to protect.
            let code_only: String = block
                .lines()
                .filter_map(|l| l.split("//").next())
                .collect::<Vec<_>>()
                .join("\n");

            if code_only.contains("source =") {
                covered += 1;
                continue;
            }

            // An exemption counts ONLY when it is the comment block directly
            // above this macro: walk back over contiguous `//` lines and stop
            // at the first line that is not one. A marker on a different site
            // is separated by code and therefore cannot be borrowed.
            // `idx` sits mid-line (after the indentation), so trim back to the
            // last COMPLETE line first — otherwise the walk stops immediately
            // on the macro's own leading whitespace and never sees the comment.
            let head = &src[..idx];
            let head = match head.rfind('\n') {
                Some(p) if !head.ends_with('\n') => &head[..p],
                _ => head,
            };
            let attached_marker = head
                .lines()
                .rev()
                .take_while(|l| l.trim_start().starts_with("//"))
                .any(|l| l.contains("COLLAPSE-ALARM-EXEMPT"));
            if attached_marker {
                exempt += 1;
                continue;
            }

            let line = src[..idx].matches('\n').count() + 1;
            offenders.push(format!(
                "line {line}: {}",
                block
                    .chars()
                    .take(120)
                    .collect::<String>()
                    .replace('\n', " ")
            ));
        }

        assert!(
            offenders.is_empty(),
            "{} WS-GAP-03 error site(s) in dhan_live_universe.rs carry neither a `source` field \
             nor an attached `// COLLAPSE-ALARM-EXEMPT:` reason, so \
             tv-<env>-errcode-ws-gap-03-universe-collapse CANNOT match them — the alarm reads \
             green while the universe collapses:\n  {}\n\nEither add `source = \
             UniverseSource::<variant>.as_str(),` (the site reports an actual collapse) or an \
             attached exemption comment saying why it does not.",
            offenders.len(),
            offenders.join("\n  ")
        );

        // Non-vacuity, both halves. A scanner that stopped matching would pass
        // silently, and so would an edit that quietly exempted every site.
        assert!(
            covered >= 1,
            "no WS-GAP-03 site in this module carries `source` — the collapse alarm has \
             nothing left to match and this guard is passing vacuously"
        );
        assert!(
            exempt >= 1,
            "no exempt site found — the scanner is no longer reaching the widening and \
             wait-give-up arms, so it is no longer guarding them either"
        );
    }

    /// An index listed TWICE in the artifact is subscribed ONCE.
    ///
    /// Duplicates are not exotic in that file: its `mappings` array carries
    /// one row per (index list, stock) membership pair, which is why 4,565
    /// rows resolve to about 870 instruments. The seed-swap used to re-push
    /// every master index row without consulting what was already selected.
    ///
    /// Downstream `dedup_subscribe_set` would have removed it before the wire
    /// — but only after this function counted the duplicate against the
    /// capacity envelope, and an over-count here falls the WHOLE universe back
    /// to four ids. Found by a property test.
    #[test]
    fn an_index_listed_twice_in_the_master_is_subscribed_once() {
        let seeds = [13_u64, 25].map(|security_id| SubscribeInstrument {
            security_id,
            segment: ExchangeSegment::IdxI,
        });
        let master = [
            MasterEntry {
                security_id: 99,
                exchange_segment_code: ExchangeSegment::IdxI as u8,
            },
            MasterEntry {
                security_id: 99,
                exchange_segment_code: ExchangeSegment::IdxI as u8,
            },
        ];
        let got = select_live_universe(&seeds, Some(&master), 50);
        assert_eq!(
            got.instruments,
            vec![SubscribeInstrument {
                security_id: 99,
                segment: ExchangeSegment::IdxI,
            }],
            "the duplicate index row reached the subscription"
        );
    }

    /// A master whose index ids all coincide with the seeds STILL drops the
    /// seeds it does not name.
    ///
    /// The swap used to fire on how many index rows were newly INSERTED, so a
    /// master that named only ids already seeded inserted nothing, the swap
    /// never ran, and the other seeds survived — ids measured receiving zero
    /// packets of any response code, including the code-6 PrevClose Dhan
    /// support confirmed is emitted for IDX_I on any subscription in any mode.
    ///
    /// The trigger is now the master's index SET, which is the question the
    /// surrounding comment always claimed to be asking. Found by the same
    /// property test as the duplicate above, in the same counterexample.
    #[test]
    fn a_master_naming_only_a_seed_index_still_drops_the_other_seeds() {
        let seeds = [13_u64, 25, 51, 21].map(|security_id| SubscribeInstrument {
            security_id,
            segment: ExchangeSegment::IdxI,
        });
        let master = [MasterEntry {
            security_id: 13,
            exchange_segment_code: ExchangeSegment::IdxI as u8,
        }];
        let got = select_live_universe(&seeds, Some(&master), 50);
        assert_eq!(
            got.instruments,
            vec![SubscribeInstrument {
                security_id: 13,
                segment: ExchangeSegment::IdxI,
            }],
            "seeds the master did not name survived"
        );
    }

    /// The three spot-universe sources must produce THREE distinct labels.
    ///
    /// MEASURED 2026-08-25: the box logged `spot_universe: "fno_underlyings",
    /// instruments: 865` while the same session's artifact line read
    /// `ntm_constituents: 746, nse_indices: 119` — which is exactly 865. NTM
    /// was working and the label named a narrowing that had not run, because
    /// the label was a two-arm ternary with no NTM case.
    ///
    /// Source-scanned rather than called: the label is computed inside
    /// `resolve_live_universe`, which needs an artifact on disk. What must not
    /// regress is the SHAPE — a two-arm ternary cannot describe three sources,
    /// and that is checkable without a filesystem.
    #[test]
    fn the_spot_universe_label_has_an_arm_for_every_source() {
        let full = include_str!("dhan_live_universe.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let src = full.split(test_marker).next().unwrap_or(full);

        for label in ["\"ntm\"", "\"fno_underlyings\"", "\"full_master\""] {
            assert!(
                src.contains(label),
                "the label {label} must exist — three sources need three names"
            );
        }
        assert!(
            src.contains("let spot_universe_label ="),
            "the label must be computed once and reused, not re-derived per log site"
        );
        assert_eq!(
            src.matches("spot_universe = spot_universe_label").count(),
            3,
            "the success, the truncated and the fallback log line must use the SAME computed label — \
             two independently-written ternaries are how the NTM arm went missing"
        );
        // The exact shape that was wrong: a `narrowed` ternary choosing
        // between only fno_underlyings and full_master.
        assert!(
            !src.contains("spot_universe = if narrowed {"),
            "the two-arm ternary must not come back — it cannot name the NTM source"
        );
    }

    /// NTM winning must SET the flag the label reads, or the label silently
    /// falls back to naming the F&O narrowing again.
    #[test]
    fn the_ntm_success_path_records_that_ntm_was_used() {
        let full = include_str!("dhan_live_universe.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let src = full.split(test_marker).next().unwrap_or(full);

        let ntm_branch = src
            .find("if cfg.spot_universe_ntm_only {")
            .expect("the NTM branch must exist");
        let fno_branch = src
            .find("if master.is_none() && cfg.spot_universe_fno_underlyings_only {")
            .expect("the F&O branch must exist");
        assert!(
            ntm_branch < fno_branch,
            "NTM is tried FIRST — that ordering IS the 2026-08-22-over-2026-08-21 precedence"
        );
        assert!(
            src[ntm_branch..fno_branch].contains("ntm_used = true"),
            "the NTM success arm must record that it won, or the label names the wrong source"
        );
        assert!(
            !src[fno_branch..].contains("ntm_used = true"),
            "only the NTM arm may set it"
        );
    }
    /// Precedence is expressed by ORDER — the NTM branch runs first and the
    /// F&O branch is guarded by `master.is_none()`. That is a real decision
    /// (operator 2026-08-22 supersedes 2026-08-21) sitting in a form that a
    /// tidy-up could silently reverse, so it is pinned here rather than left
    /// to whichever `if` someone wrote first.
    ///
    /// A source scan and not a behavioural test because `resolve_live_universe`
    /// is `TEST-EXEMPT: filesystem I/O` — the branch it guards reads two dated
    /// artifacts from disk. This asserts the exact property that matters and
    /// that the exemption otherwise leaves unchecked: with both flags on, the
    /// F&O read cannot run once NTM has produced a set.
    /// `MAX_DAILY_UNIVERSE_SIZE` had zero production readers. This is now a
    /// reader, and this test is what stops it becoming decorative again.
    ///
    /// The number itself is the one the scope lock derives: 5 main-feed
    /// connections x 5,000 instruments each. If the capacity ever stops
    /// matching that derivation the constant is lying about the bound, which
    /// is exactly the state the audit found it in.
    #[test]
    fn the_documented_universe_ceiling_is_actually_read_and_matches_its_derivation() {
        use tickvault_common::constants::MAX_DAILY_UNIVERSE_SIZE;

        assert_eq!(
            MAX_DAILY_UNIVERSE_SIZE, 25_000,
            "the ceiling is 5 connections x 5,000 instruments — the main-feed subscription \
             capacity, not a round number"
        );

        // The warn band must leave real room to act. A 1% band on 25,000 is
        // 250 instruments — less than one index option chain, i.e. a warning
        // that arrives after the decision is already made.
        let warn_below = MAX_DAILY_UNIVERSE_SIZE / UNIVERSE_HEADROOM_WARN_DIVISOR;
        assert!(
            warn_below >= 2_000,
            "the headroom warning must fire while at least one full index option chain \
             still fits (2,037 contracts observed for three underlyings); {warn_below} \
             would arrive too late to act on"
        );

        // The measured 2026-08-22 reading — 22,996 of 25,000 — must be inside
        // the warn band. If it is not, this warning would have stayed silent
        // through the exact state that prompted it.
        assert!(
            MAX_DAILY_UNIVERSE_SIZE - 22_996 < warn_below,
            "the live 2026-08-22 universe (22,996 of 25,000) must trip the headroom \
             warning; a band that misses it is decoration"
        );

        // And the function must actually be called on the success path —
        // publishing headroom only when something already broke is the
        // fallback-only-tripwire shape this file rejected for the size gauge.
        let src = include_str!("dhan_live_universe.rs");
        assert!(
            src.contains("report_universe_headroom(selection.instruments.len(), capacity)"),
            "the headroom report must run on the MASTER-SOURCED success path, not only \
             on a failure branch"
        );
    }
    #[test]
    fn ntm_wins_when_both_narrowing_flags_are_on() {
        let src = include_str!("dhan_live_universe.rs");
        let ntm_at = src
            .find("if cfg.spot_universe_ntm_only {")
            .expect("the NTM branch is gone — the 2026-08-22 narrowing no longer exists");
        let fno_at = src
            .find("if master.is_none() && cfg.spot_universe_fno_underlyings_only {")
            .expect(
                "the F&O branch lost its `master.is_none()` guard — with both flags on it \
                 would now overwrite the NTM set and silently narrow to ~335 instruments",
            );
        assert!(
            ntm_at < fno_at,
            "the F&O branch runs before NTM — precedence is inverted against the operator's \
             later dated instruction"
        );
    }

    use super::*;

    fn idx() -> Vec<SubscribeInstrument> {
        vec![
            SubscribeInstrument {
                security_id: 13,
                segment: ExchangeSegment::IdxI,
            },
            SubscribeInstrument {
                security_id: 25,
                segment: ExchangeSegment::IdxI,
            },
        ]
    }

    fn entry(security_id: u64, code: u8) -> MasterEntry {
        MasterEntry {
            security_id,
            exchange_segment_code: code,
        }
    }

    /// The default path must be byte-identical to the pre-existing behaviour.
    /// If this ever diverges, the "ships default-OFF" claim in the scope-lock
    /// is false and the third quote's carve-out has been breached in code.
    /// The 2026-08-20 finding: the four hardcoded seeds are REST Data API ids
    /// reused on the WebSocket, and they drew zero packets of any code on
    /// every recorded day. When the master supplies real index ids they must
    /// REPLACE the seeds, not sit beside them.
    #[test]
    fn master_indices_replace_the_hardcoded_seeds_rather_than_joining_them() {
        let seeds = vec![
            SubscribeInstrument {
                security_id: 13,
                segment: ExchangeSegment::IdxI,
            },
            SubscribeInstrument {
                security_id: 51,
                segment: ExchangeSegment::IdxI,
            },
        ];
        let master = vec![
            MasterEntry {
                security_id: 900,
                exchange_segment_code: 0,
            },
            MasterEntry {
                security_id: 901,
                exchange_segment_code: 0,
            },
            MasterEntry {
                security_id: 500,
                exchange_segment_code: 1,
            },
        ];
        let out = select_live_universe(&seeds, Some(&master), 25_000);
        let idx: Vec<u64> = out
            .instruments
            .iter()
            .filter(|i| i.segment == ExchangeSegment::IdxI)
            .map(|i| i.security_id)
            .collect();
        assert_eq!(
            idx,
            vec![900, 901],
            "the seeds are gone, the master ids remain"
        );
        assert!(
            out.instruments.iter().any(|i| i.security_id == 500),
            "the equity is untouched by the index swap"
        );
    }

    /// The conditional half, and the one that keeps a master problem from
    /// becoming an outage: no INDEX rows means the seeds STAY.
    #[test]
    fn a_master_with_no_indices_leaves_the_hardcoded_seeds_in_place() {
        let seeds = vec![SubscribeInstrument {
            security_id: 13,
            segment: ExchangeSegment::IdxI,
        }];
        let master = vec![MasterEntry {
            security_id: 500,
            exchange_segment_code: 1,
        }];
        let out = select_live_universe(&seeds, Some(&master), 25_000);
        let idx: Vec<u64> = out
            .instruments
            .iter()
            .filter(|i| i.segment == ExchangeSegment::IdxI)
            .map(|i| i.security_id)
            .collect();
        assert_eq!(
            idx,
            vec![13],
            "degrade to the old behaviour, never to zero indices"
        );
    }

    /// A master id that repeats a seed must appear ONCE, not twice.
    #[test]
    fn a_master_index_repeating_a_seed_is_not_subscribed_twice() {
        let seeds = vec![SubscribeInstrument {
            security_id: 13,
            segment: ExchangeSegment::IdxI,
        }];
        let master = vec![MasterEntry {
            security_id: 13,
            exchange_segment_code: 0,
        }];
        let out = select_live_universe(&seeds, Some(&master), 25_000);
        let idx: Vec<u64> = out
            .instruments
            .iter()
            .filter(|i| i.segment == ExchangeSegment::IdxI)
            .map(|i| i.security_id)
            .collect();
        assert_eq!(idx, vec![13]);
    }

    #[test]
    fn test_select_live_universe_without_master_returns_the_index_set_unchanged() {
        let sel = select_live_universe(&idx(), None, 25_000);
        assert_eq!(sel.instruments, idx());
        assert_eq!(sel.source, UniverseSource::HardcodedIndices);
    }

    /// Widening ADDS to the indices rather than replacing them — index spots are
    /// the reference every derivative is priced against.
    #[test]
    fn test_select_live_universe_keeps_the_indices_when_widening() {
        let sel = select_live_universe(&idx(), Some(&[entry(2885, 1)]), 25_000);
        assert_eq!(sel.source, UniverseSource::MasterSourced);
        assert_eq!(sel.instruments.len(), 3);
        assert!(
            sel.instruments
                .iter()
                .any(|i| i.security_id == 13 && i.segment == ExchangeSegment::IdxI),
            "the index SIDs must survive widening"
        );
    }

    /// Dhan's segment numbering has a GAP at 6. Coercing an undecodable byte
    /// would subscribe a real id under the wrong segment, and
    /// `(security_id, segment)` is the composite identity everything keys on.
    #[test]
    fn test_select_live_universe_refuses_the_segment_gap_rather_than_coercing() {
        let sel = select_live_universe(&idx(), Some(&[entry(2885, 6), entry(2886, 99)]), 25_000);
        assert_eq!(sel.refused_unknown_segment, 2);
        assert_eq!(
            sel.instruments.len(),
            idx().len(),
            "nothing undecodable may be subscribed"
        );
    }

    #[test]
    fn test_select_live_universe_refuses_zero_security_ids() {
        let sel = select_live_universe(&idx(), Some(&[entry(0, 1)]), 25_000);
        assert_eq!(sel.refused_zero_id, 1);
        assert_eq!(sel.instruments.len(), idx().len());
    }

    /// Dedup is on the I-P1-11 composite pair, not the bare id — two real
    /// instruments can share a numeric id across segments, and collapsing them
    /// would silently drop one.
    /// The mapping artifact carries ONE ROW PER `(index, symbol)` PAIR, so a
    /// stock in a dozen NSE index lists appears a dozen times. This is the
    /// test that says what the feed does with that.
    ///
    /// It exists because the row count has twice been read as a subscription
    /// count -- once into a scope-lock rule file as "4,565 SIDs in the live
    /// set", and once into a sizing argument as "the boot pass already spends
    /// ~4,565 slots". Neither is what happens: `select_live_universe` dedups on
    /// the I-P1-11 composite key across the WHOLE master, so 4,565 rows of
    /// roughly 750 NIFTY Total Market stocks plus ~120 indices subscribe about
    /// 870 instruments and take ONE main-feed connection, not the whole pool.
    ///
    /// A number that is five times the truth decides the wrong thing about
    /// capacity, so this is pinned rather than left to be re-derived.
    #[test]
    fn a_stock_repeated_across_many_index_lists_is_subscribed_once() {
        // 750 distinct stocks, each appearing in 6 index lists -> 4,500 rows,
        // plus 65 index rows: the artifact shape, in miniature.
        let mut master = Vec::new();
        for list in 0..6u64 {
            for stock in 0..750u64 {
                let _ = list;
                master.push(MasterEntry {
                    security_id: 100_000 + stock,
                    exchange_segment_code: 1, // NSE_EQ
                });
            }
        }
        for idx in 0..65u64 {
            master.push(MasterEntry {
                security_id: 1_000 + idx,
                exchange_segment_code: 0, // IDX_I
            });
        }
        assert_eq!(master.len(), 4_565, "the artifact row count under audit");

        let sel = select_live_universe(&[], Some(&master), 25_000);
        assert_eq!(
            sel.instruments.len(),
            815,
            "750 stocks + 65 indices -- the number of instruments actually dialed"
        );
        assert_eq!(
            sel.deduped, 3_750,
            "every repeat is COUNTED, so the gap between rows and instruments is \
             visible rather than inferred"
        );
        assert_eq!(
            sel.instruments.len() + sel.deduped,
            master.len(),
            "rows in == instruments + dedups; nothing is lost silently"
        );
    }

    #[test]
    fn test_select_live_universe_dedups_on_the_composite_pair_not_the_bare_id() {
        // 13/IdxI duplicates an index entry; 13/NseEquity is a DIFFERENT
        // instrument and must survive.
        let sel = select_live_universe(&idx(), Some(&[entry(13, 0), entry(13, 1)]), 25_000);
        assert_eq!(
            sel.deduped, 1,
            "only the exact (id, segment) repeat is a dup"
        );
        assert!(
            sel.instruments
                .iter()
                .any(|i| i.security_id == 13 && i.segment == ExchangeSegment::NseEquity),
            "a same-id different-segment instrument is not a duplicate"
        );
    }

    /// Audit D3 (owner, 2026-09-26): over the envelope the capacity is FILLED
    /// by priority — indices first, then by `(segment, security_id)` — and the
    /// excess is counted. It must never fall back to the 4 index SIDs.
    #[test]
    fn over_the_envelope_fills_the_capacity_by_priority_indices_first() {
        let mut master: Vec<MasterEntry> = (1..=10).map(|i| entry(1000 + i, 1)).collect();
        // The master's index row comes LAST in the file; priority puts it first.
        master.push(entry(1999, 0));
        let sel = select_live_universe(&idx(), Some(&master), 5);
        assert_eq!(sel.source, UniverseSource::TruncatedToCapacity);
        assert_eq!(
            sel.instruments.len(),
            5,
            "the capacity is filled, not left empty"
        );
        assert_ne!(
            sel.instruments,
            idx(),
            "must never fall back to the index set"
        );
        assert_eq!(
            sel.instruments[0],
            SubscribeInstrument {
                security_id: 1999,
                segment: ExchangeSegment::IdxI,
            },
            "indices come first"
        );
        let stocks: Vec<SecurityId> = sel.instruments[1..].iter().map(|i| i.security_id).collect();
        assert_eq!(
            stocks,
            vec![1001, 1002, 1003, 1004],
            "then by composite key, whatever the row order"
        );
        // 10 stocks + 1 master index (the seeds are replaced) = 11; 5 fit.
        assert_eq!(sel.refused_over_capacity, 6);
    }

    /// Exactly at the envelope is a normal widened session, not a truncation.
    #[test]
    fn exactly_at_the_envelope_is_not_a_truncation() {
        let master: Vec<MasterEntry> = (1..=3).map(|i| entry(1000 + i, 1)).collect();
        let sel = select_live_universe(&idx(), Some(&master), 5);
        assert_eq!(sel.source, UniverseSource::MasterSourced);
        assert_eq!(sel.instruments.len(), 5);
        assert_eq!(sel.refused_over_capacity, 0);
    }

    #[test]
    fn earlier_ist_dates_walks_back_across_a_month_boundary_newest_first() {
        assert_eq!(
            earlier_ist_dates("2026-03-02", 3),
            vec!["2026-03-01", "2026-02-28", "2026-02-27"]
        );
        assert!(earlier_ist_dates("2026-03-02", 0).is_empty());
        assert!(
            earlier_ist_dates("not-a-date", 7).is_empty(),
            "never guess a date"
        );
    }

    /// An in-memory stand-in for the artifact directory: a file is a path and
    /// a row count; anything else reads as missing.
    fn lookup(
        files: &[(std::path::PathBuf, usize)],
        path: &std::path::Path,
    ) -> Result<Vec<MasterEntry>, ArtifactFailure> {
        match files.iter().find(|(f, _)| f == path) {
            Some((_, n)) => Ok((0..*n).map(|i| entry(5000 + i as u64, 1)).collect()),
            None => Err(ArtifactFailure::Unreadable("missing".to_owned())),
        }
    }

    fn full_cfg() -> tickvault_common::config::DhanUniverseConfig {
        tickvault_common::config::DhanUniverseConfig::default()
    }

    /// The newest earlier day wins, and today's own file is never asked for
    /// (the caller already failed to read it).
    #[test]
    fn the_lookback_takes_the_newest_earlier_day_and_never_rereads_today() {
        let files = vec![
            (crate::dhan_universe::mapping_artifact_path("2026-09-23"), 3),
            (crate::dhan_universe::mapping_artifact_path("2026-09-25"), 2),
        ];
        let asked = std::cell::RefCell::new(Vec::new());
        let got = newest_earlier_master_with(&full_cfg(), "2026-09-28", 7, |p| {
            asked.borrow_mut().push(p.to_path_buf());
            lookup(&files, p)
        })
        .expect("an earlier list is on disk");
        assert_eq!(got.date_ist, "2026-09-25");
        assert_eq!(got.kind, "full_master");
        assert_eq!(got.entries.len(), 2);
        assert!(
            asked
                .borrow()
                .iter()
                .all(|p| !p.to_string_lossy().contains("2026-09-28")),
            "today's file is never re-read"
        );
    }

    /// An empty list is skipped: it would resolve to nothing and report a
    /// collapse under another name.
    #[test]
    fn the_lookback_skips_an_empty_list() {
        let files = vec![
            (crate::dhan_universe::mapping_artifact_path("2026-09-27"), 0),
            (crate::dhan_universe::mapping_artifact_path("2026-09-26"), 4),
        ];
        let got = newest_earlier_master_with(&full_cfg(), "2026-09-28", 7, |p| lookup(&files, p))
            .expect("the older, non-empty list is used");
        assert_eq!(got.date_ist, "2026-09-26");
    }

    /// Within one day the same precedence as today's resolve: NTM when its
    /// flag is on, otherwise straight to the full mapping.
    #[test]
    fn the_lookback_keeps_todays_precedence_within_a_day() {
        let files = vec![
            (
                crate::dhan_universe::ntm_spot_artifact_path("2026-09-27"),
                2,
            ),
            (crate::dhan_universe::mapping_artifact_path("2026-09-27"), 9),
        ];
        let with_ntm =
            newest_earlier_master_with(&ntm_cfg(), "2026-09-28", 7, |p| lookup(&files, p))
                .expect("found");
        assert_eq!(with_ntm.kind, "ntm");
        let without =
            newest_earlier_master_with(&full_cfg(), "2026-09-28", 7, |p| lookup(&files, p))
                .expect("found");
        assert_eq!(
            without.kind, "full_master",
            "a flag that is off is never read"
        );
    }

    /// Nothing inside the retention window is nothing: the caller then takes
    /// the old, paged index fallback.
    #[test]
    fn the_lookback_stops_at_the_retention_window() {
        let files = vec![(crate::dhan_universe::mapping_artifact_path("2026-09-20"), 5)];
        assert!(
            newest_earlier_master_with(&full_cfg(), "2026-09-28", 7, |p| lookup(&files, p))
                .is_none(),
            "eight days back is outside a seven-day window"
        );
        assert!(
            newest_earlier_master_with(&full_cfg(), "2026-09-28", 8, |p| lookup(&files, p))
                .is_some()
        );
    }

    /// The lane's lookback and the rider's sweep use one number: a longer
    /// lookback could only find swept files, a shorter one would miss kept ones.
    #[test]
    fn resolve_looks_back_exactly_as_far_as_the_rider_keeps_files() {
        let src = include_str!("dhan_live_universe.rs");
        let resolve = &src[src
            .find(concat!("pub ", "fn resolve_live_universe"))
            .expect("resolve exists")..];
        assert!(resolve.contains("crate::dhan_universe::ARTIFACT_RETENTION_DAYS"));
    }

    /// A master that resolves nothing usable must not be reported as widened —
    /// the instrument count would equal the index set and every downstream
    /// signal would look like an ordinary narrow session.
    #[test]
    fn test_select_live_universe_does_not_claim_widening_when_nothing_was_added() {
        let sel = select_live_universe(&idx(), Some(&[]), 25_000);
        assert_eq!(sel.source, UniverseSource::FellBackToIndices);
    }

    #[test]
    fn test_parse_mapping_artifact_errors_on_garbage_not_empty_list() {
        assert!(parse_mapping_artifact("not json").is_err());
        assert!(
            parse_mapping_artifact(r#"{"resolved":3}"#).is_err(),
            "a body with no `mappings` key is a parse failure, not an empty master"
        );
        assert_eq!(parse_mapping_artifact(r#"{"mappings":[]}"#), Ok(vec![]));
    }

    #[test]
    fn test_parse_mapping_artifact_reads_the_rider_shape() {
        let body = r#"{"mappings":[
            {"index_name":"Nifty 50","symbol":"RELIANCE","isin":"INE002A01018",
             "security_id":2885,"exchange_segment":1}]}"#;
        assert_eq!(
            parse_mapping_artifact(body),
            Ok(vec![entry(2885, 1)]),
            "must decode the exact field names the rider writes"
        );
    }

    /// The reader and the writer must agree on the filename. They share one
    /// function so they cannot drift, and this pins the shared value — a
    /// mismatch here would make the reader fall back forever while looking
    /// exactly like an empty master.
    #[test]
    fn test_mapping_artifact_path_is_shared_by_reader_and_writer() {
        let path = crate::dhan_universe::mapping_artifact_path("2026-08-12");
        assert_eq!(
            path.to_string_lossy(),
            "data/instrument-cache/dhan-nse-mapping-2026-08-12.json"
        );
    }

    #[test]
    fn artifact_failure_labels_name_the_file_and_the_kind() {
        let unreadable = ArtifactFailure::Unreadable("no such file".to_owned());
        let unparseable = ArtifactFailure::Unparseable("not JSON".to_owned());

        // The two artifacts get DIFFERENT labels for the same failure kind,
        // because the consequence is opposite: a broken F&O artifact widens
        // the session, a broken mapping artifact collapses it to the index
        // universe. One shared label would hide that on every dashboard.
        assert_eq!(unreadable.mapping_reason(), "artifact_unreadable");
        assert_eq!(unreadable.fno_reason(), "fno_artifact_unreadable");
        assert_eq!(unparseable.mapping_reason(), "artifact_unparseable");
        assert_eq!(unparseable.fno_reason(), "fno_artifact_unparseable");

        // And the two kinds stay distinguishable within each artifact — a
        // file the rider never wrote and a file the rider wrote wrong are
        // different problems with different owners.
        assert_ne!(unreadable.mapping_reason(), unparseable.mapping_reason());
        assert_ne!(unreadable.fno_reason(), unparseable.fno_reason());

        assert_eq!(unreadable.detail(), "no such file");
        assert_eq!(unparseable.detail(), "not JSON");
    }

    #[test]
    fn read_master_artifact_separates_missing_from_malformed() {
        let dir = std::env::temp_dir().join("tv-read-master-artifact-test");
        std::fs::create_dir_all(&dir).expect("temp dir");

        let missing = dir.join("absent.json");
        let _ = std::fs::remove_file(&missing);
        assert!(
            matches!(
                read_master_artifact(&missing),
                Err(ArtifactFailure::Unreadable(_))
            ),
            "a file the rider never wrote must read as Unreadable, never as an empty list"
        );

        let malformed = dir.join("malformed.json");
        std::fs::write(&malformed, "{\"resolved\":3}").expect("write");
        assert!(
            matches!(
                read_master_artifact(&malformed),
                Err(ArtifactFailure::Unparseable(_))
            ),
            "a body with no `mappings` array must fail LOUD, not silently resolve to zero \
             instruments — an empty universe is what falls back, and it must say why"
        );

        let good = dir.join("good.json");
        std::fs::write(
            &good,
            "{\"count\":1,\"mappings\":[{\"security_id\":11536,\"exchange_segment\":1}]}",
        )
        .expect("write");
        assert_eq!(
            read_master_artifact(&good).ok(),
            Some(vec![MasterEntry {
                security_id: 11536,
                exchange_segment_code: 1,
            }]),
            "the F&O artifact and the mapping artifact share one shape and one parser"
        );

        let _ = std::fs::remove_file(&malformed);
        let _ = std::fs::remove_file(&good);
    }

    // ---------------------------------------------------------------
    // The narrowed-artifact settle (2026-09-13).
    //
    // These pin the RACE, not the happy path. `await_mapping_artifact`
    // waits on the mapping file; `resolve_live_universe` reads the NTM
    // file. The rider writes them milliseconds apart inside ONE call, so
    // the wait can return in the gap and the read then widens silently.
    // ---------------------------------------------------------------

    fn ntm_cfg() -> tickvault_common::config::DhanUniverseConfig {
        tickvault_common::config::DhanUniverseConfig {
            spot_universe_ntm_only: true,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn the_settle_returns_at_once_when_the_narrowed_artifact_is_already_there() {
        let date = "2099-01-02";
        let p = crate::dhan_universe::ntm_spot_artifact_path(date);
        let _ = std::fs::create_dir_all(p.parent().expect("parent"));
        std::fs::write(&p, b"{}").expect("seed the artifact");

        let t0 = std::time::Instant::now();
        settle_for_narrowed_spot_artifact(&ntm_cfg(), date).await;
        let waited = t0.elapsed();

        let _ = std::fs::remove_file(&p);
        assert!(
            waited < std::time::Duration::from_millis(NARROWED_ARTIFACT_SETTLE_MAX_MS / 2),
            "a present artifact must not be waited for; waited {waited:?}"
        );
    }

    #[tokio::test]
    async fn the_settle_waits_for_an_ntm_file_that_lands_just_after_the_mapping() {
        // THE RACE, reproduced: the file is absent when the settle starts and
        // appears a few hundred ms later, exactly as the rider produces it.
        // Without the settle the caller would have read the absent file and
        // widened to the full master-sourced set.
        let date = "2099-01-03";
        let p = crate::dhan_universe::ntm_spot_artifact_path(date);
        let _ = std::fs::create_dir_all(p.parent().expect("parent"));
        let _ = std::fs::remove_file(&p);

        let writer = {
            let p = p.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                std::fs::write(&p, b"{}").expect("late write");
            })
        };

        let t0 = std::time::Instant::now();
        settle_for_narrowed_spot_artifact(&ntm_cfg(), date).await;
        let waited = t0.elapsed();
        writer.await.expect("writer task");

        let present = p.exists();
        let _ = std::fs::remove_file(&p);

        assert!(present, "the test's own writer must have produced the file");
        assert!(
            waited >= std::time::Duration::from_millis(250),
            "the settle must actually have waited for the late file; waited {waited:?}"
        );
        assert!(
            waited < std::time::Duration::from_millis(NARROWED_ARTIFACT_SETTLE_MAX_MS),
            "the settle must return on arrival, not burn the whole budget; waited {waited:?}"
        );
    }

    #[tokio::test]
    async fn the_settle_never_stalls_boot_past_its_budget_when_the_rider_skipped_the_file() {
        // The rider DELIBERATELY does not write the NTM artifact when the list
        // resolves zero constituents. That is correct behaviour, so the settle
        // must cost seconds and hand back -- never stall to the 09:10 cutoff.
        let date = "2099-01-04";
        let p = crate::dhan_universe::ntm_spot_artifact_path(date);
        let _ = std::fs::create_dir_all(p.parent().expect("parent"));
        let _ = std::fs::remove_file(&p);

        let t0 = std::time::Instant::now();
        settle_for_narrowed_spot_artifact(&ntm_cfg(), date).await;
        let waited = t0.elapsed();

        assert!(
            waited >= std::time::Duration::from_millis(NARROWED_ARTIFACT_SETTLE_MAX_MS),
            "an absent file must be given the full settle window; waited {waited:?}"
        );
        assert!(
            waited < std::time::Duration::from_millis(NARROWED_ARTIFACT_SETTLE_MAX_MS * 3),
            "and must then RETURN -- boot may not stall on it; waited {waited:?}"
        );
    }

    #[tokio::test]
    async fn a_boot_that_narrows_nothing_settles_for_nothing() {
        let date = "2099-01-05";
        let t0 = std::time::Instant::now();
        settle_for_narrowed_spot_artifact(
            &tickvault_common::config::DhanUniverseConfig::default(),
            date,
        )
        .await;
        assert!(
            t0.elapsed() < std::time::Duration::from_millis(200),
            "no narrowing flag means no file to wait for"
        );
    }

    #[test]
    fn the_settle_waits_for_the_same_artifact_resolve_live_universe_reads() {
        // Drift guard. `resolve_live_universe` reads NTM first and falls
        // through to F&O; if that precedence is ever reversed there and not
        // here, the settle waits for a file the consumer will not open and the
        // race re-opens silently.
        let src = include_str!("dhan_live_universe.rs");
        let body = src
            .split("#[cfg(test)]")
            .next()
            .expect("production half of the file");

        let resolve = body
            // Split so this literal is not itself read as a declaration: the
            // pub-fn-test guard greps the raw line for that text and would
            // count this search string as a new untested pub fn.
            .find(concat!("pub ", "fn resolve_live_universe"))
            .expect("resolve_live_universe");
        let r_ntm = body[resolve..]
            .find("cfg.spot_universe_ntm_only")
            .expect("resolve checks NTM");
        let r_fno = body[resolve..]
            .find("cfg.spot_universe_fno_underlyings_only")
            .expect("resolve checks F&O");
        assert!(
            r_ntm < r_fno,
            "resolve_live_universe must check NTM before F&O (operator 2026-08-22 > 2026-08-21)"
        );

        let settle = body
            .find("async fn settle_for_narrowed_spot_artifact")
            .expect("settle_for_narrowed_spot_artifact");
        let s_ntm = body[settle..]
            .find("cfg.spot_universe_ntm_only")
            .expect("settle checks NTM");
        let s_fno = body[settle..]
            .find("cfg.spot_universe_fno_underlyings_only")
            .expect("settle checks F&O");
        assert!(
            s_ntm < s_fno,
            "the settle must mirror resolve_live_universe's precedence, NTM before F&O"
        );

        // ⚠ WIRING, not just shape — added 2026-09-18.
        //
        // Until now this guard stopped at the precedence check above, so
        // deleting BOTH production call sites left it GREEN: it pinned that
        // the helper is CORRECT and nothing pinned that it RUNS. That is the
        // vacuous-guard class this repository has recorded repeatedly — a
        // guard satisfied by the very artifact it exists to reject.
        //
        // `await_mapping_artifact` is the ONLY production caller and it
        // returns early on several arms, so the call has to appear on both
        // arms that actually narrow. Assert the CALL FORM inside that
        // function's production body.
        let waiter = body
            // Split for the same pub-fn-guard reason as above.
            .find(concat!("pub ", "async fn await_mapping_artifact"))
            .expect("await_mapping_artifact");
        let call_sites = body[waiter..]
            .matches("settle_for_narrowed_spot_artifact(cfg, date_ist).await")
            .count();
        assert!(
            call_sites >= 2,
            "await_mapping_artifact must CALL the settle on both narrowing arms \
             (NTM and F&O); found {call_sites}. Deleting a call site silently \
             re-opens the boot race the settle exists to close."
        );
    }

    #[tokio::test]
    async fn await_mapping_artifact_returns_at_once_when_master_sourcing_is_off() {
        // The `not_requested` arm. A lane that is not master-sourced has no
        // artifact to wait for, so this must never stall boot -- and it must
        // not consult the filesystem to decide that, because the whole point
        // of the flag is that the rider's output is irrelevant to this boot.
        let cfg = tickvault_common::config::DhanUniverseConfig {
            live_subscription_from_master: false,
            ..Default::default()
        };
        let t0 = std::time::Instant::now();
        await_mapping_artifact(&cfg, "2099-01-06", false).await;
        assert!(
            t0.elapsed() < std::time::Duration::from_millis(200),
            "master sourcing off means nothing to wait for; waited {:?}",
            t0.elapsed()
        );
    }

    #[tokio::test]
    async fn await_mapping_artifact_does_not_wait_when_the_rider_that_writes_it_is_disabled() {
        // The `rider_disabled` arm: master sourcing is REQUESTED but the only
        // writer is switched off, so no amount of waiting can produce the
        // file. Waiting would burn the whole budget to reach the identical
        // fallback -- the two flags disagreeing is a config error, not a race.
        let cfg = tickvault_common::config::DhanUniverseConfig {
            live_subscription_from_master: true,
            enabled: false,
            ..Default::default()
        };
        let t0 = std::time::Instant::now();
        await_mapping_artifact(&cfg, "2099-01-07", false).await;
        assert!(
            t0.elapsed() < std::time::Duration::from_millis(200),
            "a disabled rider must fail fast, not stall boot; waited {:?}",
            t0.elapsed()
        );
    }

    /// The off-session verdict, on the real boot times that paged.
    #[test]
    fn collapse_is_expected_only_off_session() {
        const RIDER: u32 = 8 * 3_600; // [dhan_universe] target_secs_of_day_ist default
        let at = |h: u32, m: u32| h * 3_600 + m * 60;
        // Measured paging boots (IST): 05:08 Wed, 03:42 on a trading day.
        assert!(super::collapse_is_expected_for_this_boot(
            true,
            at(5, 8),
            RIDER
        ));
        assert!(super::collapse_is_expected_for_this_boot(
            true,
            at(3, 42),
            RIDER
        ));
        // Any time on a non-trading day — weekends, holidays.
        assert!(super::collapse_is_expected_for_this_boot(
            false,
            at(10, 0),
            RIDER
        ));
        assert!(super::collapse_is_expected_for_this_boot(
            false,
            at(8, 30),
            RIDER
        ));
        // The scheduled 08:30 start and every mid-session restart must still page.
        assert!(!super::collapse_is_expected_for_this_boot(
            true,
            at(8, 30),
            RIDER
        ));
        assert!(!super::collapse_is_expected_for_this_boot(
            true,
            at(11, 45),
            RIDER
        ));
        // Boundary: AT the rider hour the artifact is due — no longer expected.
        assert!(!super::collapse_is_expected_for_this_boot(
            true, RIDER, RIDER
        ));
        assert!(super::collapse_is_expected_for_this_boot(
            true,
            RIDER - 1,
            RIDER
        ));
    }

    /// The expected-fallback line must be invisible to the collapse alarm, and
    /// must not move the alarmed counter.
    #[test]
    fn the_expected_fallback_cannot_reach_the_collapse_alarm() {
        assert_ne!(
            super::PRE_RIDER_BOOT_SOURCE,
            UniverseSource::FellBackToIndices.as_str(),
            "the expected-fallback source must differ from the one the collapse alarm filters on"
        );
        let tf = include_str!("../../../deploy/aws/terraform/error-code-alarms.tf");
        assert!(
            !tf.contains(super::PRE_RIDER_BOOT_SOURCE),
            "no CloudWatch filter may match the expected pre-rider fallback"
        );

        let src = include_str!("dhan_live_universe.rs");
        let start = src
            .find(concat!(
                "if collapse_",
                "expected {\n                        // EXPECTED"
            ))
            .expect("the expected-fallback arm must exist in resolve_live_universe");
        let arm_end = start
            + src[start..]
                .find("return index_universe;")
                .expect("the expected arm must return the index universe");
        let arm = &src[start..arm_end];
        assert!(
            arm.contains("PRE_RIDER_BOOT_SOURCE"),
            "arm must carry the pre-rider source"
        );
        assert!(
            !arm.contains(concat!("record_master_sourcing_", "fallback(")),
            "the expected arm must NOT increment the alarmed fallback counter"
        );
        assert!(
            !arm.contains(concat!("tracing::err", "or!(")),
            "the expected arm must not log at ERROR"
        );
        // And it must run BEFORE the paging arm, or it is unreachable.
        let paging = src[start..]
            .find(concat!("record_master_sourcing_", "fallback("))
            .expect("the paging arm must still exist");
        assert!(
            paging > arm_end - start,
            "expected arm must precede the paging arm"
        );

        // Audit D3: the EARLIER-list arm has an expected half too, and it must
        // be as quiet as the index one — same source, no counter, no ERROR.
        let early = src
            .find("// Expected (pre-rider or non-trading-day boot):")
            .expect("the expected half of the earlier-list arm must exist");
        let early_end = early
            + src[early..]
                .find("} else {")
                .expect("the expected half ends where the paging half starts");
        let early_arm = &src[early..early_end];
        assert!(early_arm.contains("PRE_RIDER_BOOT_SOURCE"));
        assert!(!early_arm.contains("MASTER_SOURCING_FALLBACK_COUNTER"));
        assert!(!early_arm.contains(concat!("tracing::err", "or!(")));
        assert!(
            !tf.contains(super::EARLIER_ARTIFACT_SOURCE),
            "the collapse alarm must not match a session running on an earlier list"
        );
    }

    /// The wait and the resolve must be judged by the SAME verdict, computed
    /// once — otherwise the wait could predict one outcome and the resolve
    /// report another.
    #[test]
    fn main_passes_one_off_session_verdict_to_both_calls() {
        let main = include_str!("main.rs");
        assert_eq!(
            main.matches("universe_collapse_expected,").count(),
            1,
            "the pre-wait verdict must be passed to await_mapping_artifact only"
        );
        assert_eq!(
            main.matches("universe_collapse_expected_after_wait")
                .count(),
            2,
            "main.rs must re-judge the verdict once after the wait and pass THAT to \
             resolve_live_universe"
        );
        assert!(main.contains("trading_calendar.is_trading_day_today()"));
    }

    /// Audit re-check 7: a boot between 07:10 and the rider hour waits past
    /// the hour, so a verdict frozen before the wait would log a real miss as
    /// the expected pre-rider warning and page nobody.
    #[test]
    fn main_re_judges_the_verdict_after_the_wait_and_can_only_tighten_it() {
        let main = include_str!("main.rs");
        let wait = main
            .find("await_mapping_artifact(")
            .expect("main.rs must wait for the mapping artifact");
        let rejudge = main
            .find("let universe_collapse_expected_after_wait = universe_collapse_expected")
            .expect("the re-judge must AND with the pre-wait verdict");
        let resolve = main
            .find("resolve_live_universe(")
            .expect("main.rs must resolve the live universe");
        assert!(
            wait < rejudge && rejudge < resolve,
            "wait, then re-judge, then resolve"
        );
        let rejudge_block = &main[rejudge..resolve];
        assert!(
            rejudge_block.contains(
                "&& tickvault_app::dhan_live_universe::collapse_is_expected_for_this_boot("
            )
        );
        assert!(rejudge_block.contains("now_ist_secs_of_day()"));
    }

    #[test]
    fn run_degraded_universe_heartbeat_is_spawned_by_main_while_degraded() {
        let main = include_str!("main.rs");
        let resolve = main
            .find("resolve_live_universe(")
            .expect("main.rs must resolve the live universe");
        let tail = &main[resolve..];
        let spawn = tail
            .find("run_degraded_universe_heartbeat(reason)")
            .expect("main.rs must spawn the degraded-universe heartbeat");
        let reason = tail
            .find("live_universe_degraded_reason()")
            .expect("the heartbeat must be gated on the degraded reason");
        assert!(reason < spawn);
        assert!(
            spawn < tail.find("spawn_dhan_feed_stack(").expect("lane spawn"),
            "the heartbeat is armed right after the resolve, before the lane dials"
        );
    }

    #[test]
    fn live_universe_degraded_reason_decodes_every_reason_and_nothing_else() {
        assert_eq!(degraded_reason_from_slot(0), None, "0 means today's list");
        for (index, reason) in MASTER_SOURCING_FALLBACK_REASONS.iter().enumerate() {
            assert_eq!(degraded_reason_from_slot(index + 1), Some(*reason));
        }
        assert_eq!(
            degraded_reason_from_slot(MASTER_SOURCING_FALLBACK_REASONS.len() + 1),
            None
        );
        assert_eq!(degraded_reason_from_slot(usize::MAX), None);
    }

    /// Every paged path goes through `record_master_sourcing_fallback`, so
    /// every paged path also arms the heartbeat. Pinned in source so a new
    /// paged arm cannot count once and go quiet.
    #[test]
    fn every_paged_fallback_arms_the_heartbeat() {
        let src = include_str!("dhan_live_universe.rs");
        let body_start = src
            .find(concat!("fn record_master_sourcing_", "fallback(reason"))
            .expect("the fallback recorder must exist");
        let body_end = body_start
            + src[body_start..]
                .find("\n}\n")
                .expect("the recorder body must close");
        assert!(src[body_start..body_end].contains("mark_live_universe_degraded(reason);"));
        // The earlier-day-list arm counts the mapping reason directly (it keeps
        // the size gauge for the success path), so it must arm the heartbeat
        // itself.
        let earlier = src
            .find(concat!("source = EARLIER_ARTIFACT_", "SOURCE"))
            .expect("the earlier-day-list paging line must exist");
        let armed = src[..earlier]
            .rfind(concat!(
                "mark_live_universe_",
                "degraded(failure.mapping_reason());"
            ))
            .expect("the earlier-day-list arm must arm the heartbeat");
        assert!(
            earlier - armed < 1_200,
            "the heartbeat arming must sit in the earlier-day-list arm itself"
        );
    }
}

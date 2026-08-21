//! The live main-feed subscription set, optionally sourced from the daily
//! resolved instrument master.
//!
//! Authorized to be BUILT by the operator's 2026-08-11 fourth quote; the
//! verbatim quote, the tension with the same day's third quote, and the
//! reasoning for shipping DEFAULT-OFF are recorded in
//! `.claude/rules/project/websocket-connection-scope-lock.md`.
//!
//! # This module changes nothing until a human turns it on
//!
//! With `[dhan_universe] live_subscription_from_master = false` — the default,
//! and the shipped value — [`select_live_universe`] returns the same 4 hardcoded
//! index SIDs the lane has always used. The master-sourced path is code that
//! *can* widen the subscription, sitting behind an off switch, which is what
//! keeps the third quote's carve-out ("re-pointing the lane… must not be
//! smuggled in") honoured in substance rather than only in wording.
//!
//! # Fallback is never silent
//!
//! Every path that cannot produce a trustworthy master-sourced set falls back
//! to the index universe and says so at `error!`. That matters more here than
//! in most places: a fallback that logged nothing would look identical to a
//! successful widening in every metric the lane exposes — the connection count
//! would simply be lower, and nobody reads a connection count expecting it to
//! carry an error.

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
}

impl UniverseSource {
    /// Stable label for logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HardcodedIndices => "hardcoded_indices",
            Self::MasterSourced => "master_sourced",
            Self::FellBackToIndices => "fell_back_to_indices",
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
/// instruments-per-connection). Exceeding it is refused as a whole rather than
/// truncated, because `plan_pool` refuses the ENTIRE pool when a set does not
/// fit — a set one instrument too large would take the main feed down with it,
/// so silently trimming to fit would be the wrong repair and blowing the lane
/// up would be worse. Falling back to the index set keeps the feed alive and
/// makes the problem loud.
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
    let mut master_indices = 0usize;

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
        if segment == ExchangeSegment::IdxI {
            master_indices += 1;
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
    if master_indices > 0 {
        instruments.retain(|i| i.segment != ExchangeSegment::IdxI);
        for entry in master {
            let Some(segment) = ExchangeSegment::from_byte(entry.exchange_segment_code) else {
                continue;
            };
            if segment == ExchangeSegment::IdxI && entry.security_id != 0 {
                instruments.push(SubscribeInstrument {
                    security_id: entry.security_id as SecurityId,
                    segment,
                });
            }
        }
    }

    if instruments.len() > capacity {
        return LiveUniverseSelection {
            instruments: index_universe.to_vec(),
            source: UniverseSource::FellBackToIndices,
            refused_zero_id,
            refused_unknown_segment,
            deduped,
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
    }
}

// The artifact path is deliberately NOT restated here — it comes from
// `dhan_universe::mapping_artifact_path`, the same function the writer uses.
// A second copy of that filename fails silently: the reader looks for a name
// the writer never produces, finds nothing, falls back, and the result is
// indistinguishable from "the master resolved nothing usable".

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
    // Cold path — once per boot at most — so the macro's key build is fine here.
    #[allow(clippy::cast_precision_loss)]
    // APPROVED: instrument counts are bounded by MAX_DAILY_UNIVERSE_SIZE, far below 2^53.
    metrics::gauge!(LIVE_UNIVERSE_SIZE_GAUGE).set(fell_back_to as f64);
}

/// Gauge: how many instruments the live lane actually subscribed.
pub const LIVE_UNIVERSE_SIZE_GAUGE: &str = "tv_dhan_live_universe_instruments";

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
) -> Vec<SubscribeInstrument> {
    if !cfg.live_subscription_from_master {
        tracing::info!(
            instruments = index_universe.len(),
            source = UniverseSource::HardcodedIndices.as_str(),
            "live universe: the 4 hardcoded index SIDs (master sourcing is OFF by \
             default; enabling it is a config change plus a restart)"
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
    if cfg.spot_universe_fno_underlyings_only {
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

    let master = match master {
        Some(m) => m,
        None => {
            let path = crate::dhan_universe::mapping_artifact_path(date_ist);
            match read_master_artifact(&path) {
                Ok(m) => m,
                Err(failure) => {
                    record_master_sourcing_fallback(failure.mapping_reason(), index_universe.len());
                    tracing::error!(
                        code =
                            tickvault_common::error_code::ErrorCode::WsGapConnectionState
                                .code_str(),
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
            tracing::info!(
                instruments = selection.instruments.len(),
                master_entries = master.len(),
                spot_universe = if narrowed {
                    "fno_underlyings"
                } else {
                    "full_master"
                },
                refused_zero_id = selection.refused_zero_id,
                refused_unknown_segment = selection.refused_unknown_segment,
                deduped = selection.deduped,
                capacity,
                source = selection.source.as_str(),
                "live universe: widened from today's resolved master"
            );
        }
        _ => {
            record_master_sourcing_fallback("no_usable_widening", selection.instruments.len());
            tracing::error!(
                code = tickvault_common::error_code::ErrorCode::WsGapConnectionState.code_str(),
                master_entries = master.len(),
                spot_universe = if narrowed {
                    "fno_underlyings"
                } else {
                    "full_master"
                },
                refused_zero_id = selection.refused_zero_id,
                refused_unknown_segment = selection.refused_unknown_segment,
                deduped = selection.deduped,
                capacity,
                source = selection.source.as_str(),
                "live universe: master sourcing was REQUESTED but produced no usable \
                 widening (or exceeded the authorized {capacity}-instrument envelope) — \
                 subscribing the index universe instead. This is NOT a widened session."
            );
        }
    }
    selection.instruments
}

#[cfg(test)]
mod tests {
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

    /// Over capacity, THIS function returns the index universe — the live set
    /// collapses to the index count rather than dialing an oversize pool.
    /// Falling back keeps the feed alive; truncating to fit would silently
    /// subscribe an arbitrary subset, which is the outcome this pins against.
    ///
    /// CORRECTED 2026-08-21: this comment used to credit `plan_pool` with
    /// refusing the whole pool. `plan_pool` does that for sets that REACH it —
    /// but an oversize set never does, because the arm under test returns
    /// first. Same behaviour, wrong mechanism named, and the misattribution
    /// (repeated in `MAX_DAILY_UNIVERSE_SIZE`'s own docs) produced a false
    /// CRITICAL audit finding. The collapse is alarmed via
    /// `tv_dhan_live_universe_fallback_total`.
    #[test]
    fn test_select_live_universe_falls_back_whole_when_over_the_envelope() {
        let master: Vec<MasterEntry> = (1..=10).map(|i| entry(1000 + i, 1)).collect();
        let sel = select_live_universe(&idx(), Some(&master), 5);
        assert_eq!(sel.source, UniverseSource::FellBackToIndices);
        assert_eq!(
            sel.instruments,
            idx(),
            "over-envelope must fall back whole, never truncate to fit"
        );
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

/// Counter: how each boot's wait for the mapping artifact ended.
///
/// `outcome` is one of `not_requested`, `rider_disabled`, `already_present`,
/// `became_ready`, `timed_out`, `pre_open_cutoff`.
///
/// # NOT shipped to CloudWatch — deliberately, and this is a real limitation
///
/// This counter is readable on the box's `/metrics` endpoint and nowhere else.
/// Adding it to the EMF `metric_selectors` list was attempted and REVERTED:
/// `user-data.sh.tftpl` currently renders to **15,870 bytes against a 15,872
/// byte budget**, and the size guard that pins it explicitly forbids buying
/// room by shaving unrelated blocks. Its prescribed remedy — moving content
/// out of user-data into a file copied in after the repo clone — is a larger
/// change than this fix should carry.
///
/// So a boot that timed out waiting is visible in the log line and on
/// `/metrics`, but not in CloudWatch. Shipping it is a follow-up that has to
/// deal with the byte budget first.
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
pub async fn await_mapping_artifact(
    cfg: &tickvault_common::config::DhanUniverseConfig,
    date_ist: &str,
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
        return;
    }

    // The rider is the only writer. With it disabled nobody will ever produce
    // the artifact, so waiting would burn the full deadline to reach the same
    // fallback. Fail fast and say why.
    if !cfg.enabled {
        metrics::counter!(MAPPING_WAIT_COUNTER, "outcome" => "rider_disabled").increment(1);
        tracing::error!(
            code = tickvault_common::error_code::ErrorCode::WsGapConnectionState.code_str(),
            path = %path.display(),
            "live universe: master sourcing is REQUESTED but [dhan_universe] enabled = false, \
             so nothing will ever write today's mapping. Not waiting — the lane will subscribe \
             the 4 index SIDs. Enable the rider or turn master sourcing off; the two flags \
             disagree."
        );
        return;
    }

    tracing::info!(
        path = %path.display(),
        deadline_secs = MAPPING_WAIT_DEADLINE_SECS,
        "live universe: today's mapping artifact is not written yet — waiting for the daily \
         rider before subscribing, so the lane does not lose the boot race and collapse to 4 \
         instruments"
    );

    let started = std::time::Instant::now();
    let deadline = std::time::Duration::from_secs(MAPPING_WAIT_DEADLINE_SECS);
    let interval = std::time::Duration::from_millis(MAPPING_POLL_INTERVAL_MS);

    while started.elapsed() < deadline {
        // Bounded by the CLOCK as well as the duration — see
        // `MAPPING_WAIT_NEVER_PAST_IST_SECS`. Checked first so a boot that is
        // already past the cutoff exits without burning a poll interval.
        if tickvault_common::market_hours::now_ist_secs_of_day() >= MAPPING_WAIT_NEVER_PAST_IST_SECS
        {
            metrics::counter!(MAPPING_WAIT_COUNTER, "outcome" => "pre_open_cutoff").increment(1);
            tracing::error!(
                code = tickvault_common::error_code::ErrorCode::WsGapConnectionState.code_str(),
                waited_secs = started.elapsed().as_secs_f64(),
                path = %path.display(),
                "live universe: stopped waiting for today's mapping at the 09:10 IST pre-open \
                 cutoff — the market opens at 09:15 and dialing a partial set beats dialing \
                 nothing. This session subscribes the 4 index SIDs. It means the box booted \
                 late or the daily rider is failing; the rider keeps retrying, but the lane \
                 reads the artifact once at boot, so only a restart widens this session."
            );
            return;
        }
        tokio::time::sleep(interval).await;
        if path.exists() {
            metrics::counter!(MAPPING_WAIT_COUNTER, "outcome" => "became_ready").increment(1);
            tracing::info!(
                waited_secs = started.elapsed().as_secs_f64(),
                path = %path.display(),
                "live universe: mapping artifact is ready — subscribing the widened set"
            );
            return;
        }
    }

    metrics::counter!(MAPPING_WAIT_COUNTER, "outcome" => "timed_out").increment(1);
    tracing::error!(
        code = tickvault_common::error_code::ErrorCode::WsGapConnectionState.code_str(),
        waited_secs = started.elapsed().as_secs_f64(),
        path = %path.display(),
        "live universe: the daily rider did not produce today's mapping within \
         {MAPPING_WAIT_DEADLINE_SECS}s. Subscribing the 4 index SIDs for this session. The \
         rider keeps retrying, but the lane reads the artifact once at boot — so this session \
         stays at 4 instruments until a restart."
    );
}

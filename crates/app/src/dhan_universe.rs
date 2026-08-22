//! `[dhan_universe]` — the daily Dhan master + NSE India indices download,
//! and the ISIN join between them.
//!
//! Authorized by the operator 2026-08-11, reversing Q3 of the 2026-07-13
//! amendment; the verbatim quote lives in
//! `.claude/rules/project/websocket-connection-scope-lock.md`. What the
//! operator asked for is not either download on its own — it is the
//! **mapping** between them, and that is what this rider produces.
//!
//! Shape mirrors `groww_universe.rs` deliberately: same supervised-respawn
//! outer loop, same per-attempt IST date derivation, same pull-until-success
//! backoff, same atomic file write. A second scheduler with its own
//! idiosyncrasies would be a second thing to reason about at 08:00 on a
//! morning when something has gone wrong.
//!
//! COLD PATH: one build per IST day. Never the tick path, never a WebSocket,
//! never an order path.
//!
//! # The 8 AM problem, and why this is a target rather than a schedule
//!
//! The operator asked for 08:00 IST. The prod box powers on at 08:30
//! (`cron(0 3)` = 03:00 UTC). A plain 08:00 scheduler therefore fires into a
//! machine that is not running — it would never execute, while still
//! appearing scheduled, which is the worst combination available.
//!
//! So the target is treated as "at or after": on a normal boot the rider
//! finds 08:00 already past, sees no build for today, and runs IMMEDIATELY —
//! ~08:30, still 45 minutes clear of the 09:15 open. If the process happens
//! to be alive BEFORE the target it sleeps until the target instead. Either
//! way a day cannot pass without a build attempt, which is the property that
//! actually matters.

use std::time::Duration;

use tickvault_common::config::{DhanUniverseConfig, QuestDbConfig};
use tickvault_common::constants::{
    DHAN_DETAILED_CSV_URL, INDEX_CONSTITUENCY_SLUGS, IST_UTC_OFFSET_SECONDS_I64,
};
use tickvault_core::instrument::master_csv::{
    Constituent, JoinOutcome, NSE_MEMBERSHIP_TOLERANCE, build_isin_index, join_constituents,
    parse_master_csv, split_csv_line,
};
use tickvault_core::instrument::master_download::{
    build_hardened_csv_client, fetch_csv_hardened, nse_index_csv_url, nse_user_agent,
};
use tracing::{error, info, warn};

/// Seconds in a day. Named so the modulo below reads as intent.
const SECS_PER_DAY: i64 = 86_400;

/// How long to wait for the `index_constituency` TRUNCATE migration gate.
///
/// That one-shot migration wipes the WHOLE table — QuestDB has no row-level
/// delete, so it cannot be scoped to one feed. Every writer must therefore
/// wait for it, or its just-written rows are erased by a migration that ran
/// moments later. Bounded so a gate that never opens degrades to a loud skip
/// rather than wedging the rider forever.
const MIGRATION_GATE_WAIT_SECS: u64 = 120;

/// Provenance stamped on every persisted constituent row.
const CONSTITUENCY_SOURCE: &str = "niftyindices";

/// Provenance tag stamped on F&O stock UNDERLYING mappings so a consumer can
/// tell them apart from index-constituent rows without re-parsing the master.
/// Mirrors the `"NSE_INDEX"` literal `nse_index_mappings` already uses — the
/// artifact field exists; only the value is new.
const FNO_UNDERLYING_TAG: &str = "FNO_UNDERLYING";

/// The niftyindices DISPLAY NAME of the Nifty Total Market list.
///
/// This string is a JOIN KEY, not a label: `join_constituents` stamps
/// `index_name` from the display name in [`INDEX_CONSTITUENCY_SLUGS`], and
/// [`ntm_spot_mappings`] selects on it. A typo here does not fail loudly — it
/// selects ZERO constituents and the session quietly carries indices alone,
/// which is why `ntm_display_name_matches_a_real_slug` pins the two together
/// rather than trusting that two copies of a string stay equal.
const NTM_INDEX_NAME: &str = "Nifty Total Market";

/// Backoff before respawning a died rider task. Matches the house sibling
/// (`groww_universe`, `disk_health_watcher`) — short, because the thing that
/// is not happening while we wait is the day's entire instrument mapping.
const RESPAWN_BACKOFF_SECS: u64 = 5;

/// Non-zero means the rider task died and was restarted. In a release build
/// (`panic = "abort"`) the process dies instead, so this is an unwind-build
/// signal — kept because a respawn LOOP is otherwise invisible.
const RESPAWN_COUNTER: &str = "tv_dhan_universe_respawn_total";

/// Daily build outcomes, labelled `ok` / `failed`. Both pre-registered at 0
/// so "no build has ever run" is distinguishable from "no metric reported".
const BUILD_COUNTER: &str = "tv_dhan_universe_builds_total";

/// Constituents resolved in the last successful build.
///
/// One row per `(index, symbol)` PAIR, so a stock in twelve index lists counts
/// twelve times. Read [`DISTINCT_INSTRUMENTS_GAUGE`] for the number that
/// decides how many instruments the feed subscribes.
const RESOLVED_GAUGE: &str = "tv_dhan_universe_resolved_constituents";

/// Distinct instruments in the last successful build — the number the live
/// subscription actually dials, after I-P1-11 dedup.
const DISTINCT_INSTRUMENTS_GAUGE: &str = "tv_dhan_universe_distinct_instruments";

/// Unresolved fraction of the last successful build, in `[0, 1]`.
///
/// The most useful single number here: it moves BEFORE builds start failing
/// outright, so a vendor quietly dropping constituents is visible while the
/// build is still passing its tolerance gate.
const UNRESOLVED_FRACTION_GAUGE: &str = "tv_dhan_universe_unresolved_fraction";

/// Index lists that failed to download or parse in the last build attempt.
///
/// Deliberately separate from the unresolved fraction: that gate judges the
/// joined result, not the download count. This gauge is what makes a partial
/// download visible; [`MAX_FAILED_INDEX_LIST_FRACTION`] is what makes it fatal.
const INDEX_LISTS_FAILED_GAUGE: &str = "tv_dhan_universe_index_lists_failed";

/// How many of the ~49 NSE index lists may fail before the build is REJECTED.
///
/// # Why the unresolved-fraction gate cannot cover this
///
/// That gate divides by the constituents actually DOWNLOADED, so a list that
/// never arrived is not in its denominator — invisible to it by construction.
/// 48 lists failing while the 49th returns clean data yields a fraction of
/// 0.0: a green build carrying ~2% of the intended membership. Worse, because
/// `index_constituency` rows are never evicted, yesterday's rows for the 48
/// missing indices are still in the table, so even a SQL spot-check reads
/// complete. That is precisely the false-OK class the charter forbids, and it
/// needs its own gate over the list COUNT.
///
/// Set to 10% — about 4 of 49 — because individual niftyindices lists do
/// genuinely flake, and rejecting the day over one of them would make the
/// pipeline as reliable as its least reliable list (the same reasoning that
/// makes a single failure non-fatal in the loop below).
const MAX_FAILED_INDEX_LIST_FRACTION: f64 = 0.10;

/// Backoff floor between failed attempts, doubling to the configured cap.
const RETRY_BASE_SECS: u64 = 10;

/// Whole-build deadline. Exceeding it fails the attempt; it never wedges.
///
/// The per-request timeouts (10 s connect / 60 s read) bound each HTTP attempt
/// individually. Nothing bounded their SUM: ~50 sequential fetches at the read
/// timeout is ~50 minutes, which starting at 08:00 IST would run past the 09:15
/// open — and the QuestDB ILP flush at the end has no timeout of its own, so a
/// server that accepts the connection then stops reading blocks forever. Either
/// way the loop stalls with the task still ALIVE, so the supervisor never fires
/// and no counter moves: the rider is silently gone, which is worse than a loud
/// failure.
///
/// 15 minutes is ~10× a healthy build (the master download dominates at ~1–2
/// min) and still leaves room for several retries between the 08:00 target and
/// the 09:15 open. A build slow enough to hit this is not one to keep waiting
/// on — retrying from the top is both faster and louder.
const BUILD_DEADLINE_SECS: u64 = 900;

/// Directory for the resolved mapping artifact.
const MAPPING_DIR: &str = "data/instrument-cache";

/// Path of the day's resolved mapping artifact.
///
/// Shared by the writer here and the reader in `dhan_live_universe`, on
/// purpose. Two copies of this filename would have a silent failure mode: the
/// reader looks for a name the writer never produces, finds nothing, and falls
/// back to the index universe — which is indistinguishable from "the master
/// resolved nothing usable" in every log line and counter.
#[must_use]
pub fn mapping_artifact_path(date_ist: &str) -> std::path::PathBuf {
    std::path::Path::new(MAPPING_DIR).join(format!("dhan-nse-mapping-{date_ist}.json"))
}

/// Path of the day's F&O stock UNDERLYING artifact.
///
/// A SEPARATE file rather than extra rows in the mapping artifact, and that is
/// the whole safety argument: the mapping artifact is what the live lane reads
/// at boot TODAY. Adding rows to it would change the default spot universe the
/// moment this ships, before anyone chose to switch anything on. A file no
/// current consumer opens cannot do that — the narrowed universe arrives only
/// when the flag that reads it is turned on.
///
/// Same single-definition discipline as [`mapping_artifact_path`]: two copies
/// of this filename would let the reader look for a name the writer never
/// produces, find nothing, and fall back — indistinguishable from "the master
/// had no F&O rows" in every log line.
#[must_use]
pub fn fno_underlying_artifact_path(date_ist: &str) -> std::path::PathBuf {
    std::path::Path::new(MAPPING_DIR).join(format!("dhan-fno-underlyings-{date_ist}.json"))
}

/// Path of the day's NSE-indices + Nifty-Total-Market spot artifact.
///
/// A THIRD file, for the same reason the F&O one is a second: the live lane
/// reads the mapping artifact by default, and a set that arrives only when its
/// own flag is on cannot change anybody's universe by merely shipping.
///
/// The filename must not collide with either sibling — a collision would have
/// one writer overwrite the other and the reader would subscribe whichever ran
/// last, silently. Pinned by `ntm_spot_artifact_path_never_collides_with_its_two_siblings`.
#[must_use]
pub fn ntm_spot_artifact_path(date_ist: &str) -> std::path::PathBuf {
    std::path::Path::new(MAPPING_DIR).join(format!("dhan-ntm-spot-{date_ist}.json"))
}

/// Today's IST date as `YYYY-MM-DD`, recomputed per attempt.
///
/// Never frozen at spawn: a retry loop that crosses IST midnight must name
/// the NEW day's artifact, or a vendor outage spanning midnight writes
/// yesterday's filename with today's data.
#[must_use]
pub fn today_ist_date() -> String {
    (chrono::Utc::now() + chrono::TimeDelta::seconds(IST_UTC_OFFSET_SECONDS_I64))
        .format("%Y-%m-%d")
        .to_string()
}

/// Current IST second-of-day, from a UTC epoch second.
///
/// Pure so the scheduling decision below is testable without a clock.
#[must_use]
pub fn ist_secs_of_day(now_utc_secs: i64) -> u32 {
    let ist = now_utc_secs.saturating_add(IST_UTC_OFFSET_SECONDS_I64);
    u32::try_from(ist.rem_euclid(SECS_PER_DAY)).unwrap_or(0)
}

/// How long to sleep before the next build attempt.
///
/// `None` means **run now**. That is the boot-catch-up case and it is the
/// common one in production: the box starts at 08:30, the 08:00 target is
/// already past, and today has no build yet.
///
/// # Why `already_built_today` is a parameter and not an internal flag
///
/// It makes the one decision that can silently double-run testable in
/// isolation. A rider that re-runs after its own successful build would
/// re-download ~15 MB and rewrite the day's mapping on every wake — invisible
/// in logs that only report success.
#[must_use]
pub fn next_wait(
    now_secs_of_day: u32,
    target_secs_of_day: u32,
    already_built_today: bool,
) -> Option<Duration> {
    if already_built_today {
        // Sleep to the next day's target. Computed from the target, not from
        // "24h from now", so a slow build cannot drift the schedule later
        // every day until it walks out of the market window entirely.
        // `saturating_sub`, not `-`. This is a `pub fn` whose caller supplies
        // `now_secs_of_day` freely, the release profile sets
        // `overflow-checks = true` AND `panic = "abort"`, so a value at or
        // past 86_400 would not return a wrong duration — it would KILL the
        // trading process. Every sibling arithmetic in this file already
        // saturates; this one line did not. Saturating yields 0, i.e. "the
        // next target is today's target", which is the safe direction: it
        // wakes early rather than never.
        let secs_to_midnight = u64::from(
            u32::try_from(SECS_PER_DAY)
                .unwrap_or(u32::MAX)
                .saturating_sub(now_secs_of_day),
        );
        return Some(Duration::from_secs(
            secs_to_midnight.saturating_add(u64::from(target_secs_of_day)),
        ));
    }
    if now_secs_of_day >= target_secs_of_day {
        // The target has passed and today has no build — the boot-catch-up
        // case. Run immediately rather than waiting ~23h for the next target,
        // which would skip the day entirely.
        return None;
    }
    Some(Duration::from_secs(u64::from(
        target_secs_of_day - now_secs_of_day,
    )))
}

/// One day's resolved mapping, as written to disk.
/// The F&O stock UNDERLYING artifact.
///
/// Its OWN shape rather than a reuse of [`MappingArtifact`]: that struct
/// carries join diagnostics (`ambiguous_isins`, `constituents_seen`, …) that
/// describe the ISIN join this set never performs. Filling them with zeros
/// would publish seven fields of meaningless provenance, and a later reader
/// would have no way to tell a real zero from a placeholder.
///
/// `count` is redundant with `underlyings.len()` and is written anyway: a
/// truncated file that still parses as JSON is caught by comparing them, which
/// is cheaper than trusting a length nobody checked.
#[derive(serde::Serialize)]
/// The narrowed spot artifact: NSE indices PLUS the F&O stock underlyings.
/// The type and the filename keep the `fno` name for continuity with the
/// path helper and its tests, but the CONTENTS are both halves — see
/// `narrowed_spot_mappings` for why the indices half is not optional.
struct FnoUnderlyingArtifact {
    count: usize,
    /// Named `mappings` -- the SAME key the mapping artifact uses -- so the
    /// consumer reads this file with `parse_mapping_artifact`, the parser
    /// already hardened to fail LOUD on garbage and to distinguish "parsed to
    /// an empty list" from "did not parse". A second bespoke parser for a
    /// second shape would be a second place for that distinction to be got
    /// wrong.
    mappings: Vec<MappingEntry>,
}

#[derive(Debug, serde::Serialize)]
struct MappingArtifact {
    trading_date_ist: String,
    master_rows: usize,
    isin_index_len: usize,
    ambiguous_isins: usize,
    constituents_seen: usize,
    resolved: usize,
    unresolved: usize,
    unresolved_fraction: f64,
    /// Distinct `(security_id, exchange_segment)` pairs across ALL mappings —
    /// i.e. how many instruments the live feed will actually SUBSCRIBE.
    ///
    /// This is NOT `resolved` and the difference is large. `join_constituents`
    /// dedups on `(index_name, security_id, segment)`, so a stock that belongs to
    /// twelve NSE index lists produces TWELVE resolved rows. Summed over the 46
    /// downloaded lists that inflates a real universe of roughly 750 NIFTY Total
    /// Market stocks plus ~120 NSE indices into a `mappings` array of several
    /// thousand entries.
    ///
    /// The subscribe path then dedups on the I-P1-11 composite key
    /// (`dhan_live_universe::select_live_universe`), so the number of sockets and
    /// instruments the lane opens has always followed THIS count, not `resolved`.
    /// Nothing published it, so the only figure available to a reader was the
    /// inflated one — and it was read, and recorded in a rule file, as "SIDs in
    /// the live set". A count that is five times the truth is worse than no count.
    distinct_instruments: usize,
    /// Every unresolved constituent, BY NAME. §31.1 item 4 requires the
    /// operator can see which security failed, not merely how many did.
    unresolved_detail: Vec<String>,
    mappings: Vec<MappingEntry>,
}

#[derive(Debug, serde::Serialize)]
struct MappingEntry {
    index_name: String,
    symbol: String,
    isin: String,
    security_id: u64,
    exchange_segment: u8,
}

/// Distinct `(security_id, exchange_segment)` pairs in a mapping array.
///
/// The subscribe path (`dhan_live_universe::select_live_universe`) dedups on
/// exactly this key, so this is the count of instruments the live feed will
/// dial — the answer to "how many will actually be subscribed?", available at
/// 08:30 from the artifact rather than only after the sockets come up.
fn distinct_instrument_count(mappings: &[MappingEntry]) -> usize {
    mappings
        .iter()
        .map(|m| (m.security_id, m.exchange_segment))
        .collect::<std::collections::HashSet<_>>()
        .len()
}

/// Every NSE index in the master, as mapping entries.
///
/// # Why the ISIN join structurally cannot produce these
///
/// The constituency join answers *"which cash equities make up each index?"*
/// and filters to `NSE && SEGMENT=E && SERIES=EQ`. An index is none of those:
/// it has no ISIN (the master leaves the column empty), no series, and no
/// order book. So the join emits the ~750 CONSTITUENTS of NIFTY 50 and never
/// NIFTY 50 itself — and the live universe, which is built from the join's
/// output, inherited that hole.
///
/// The consequence, measured on the box 2026-08-20: the lane subscribed FOUR
/// indices, and those four came from a hardcoded constant rather than from
/// the master at all. Every other NSE index — and there are dozens, each one
/// a reference price something is quoted against — was absent, not because
/// anything refused it but because nothing ever asked for it.
///
/// These ride the SAME `mappings` array the constituents use, so the live
/// The NSE F&O stock UNDERLYING set — the cash-equity rows that actually have
/// futures or options written on them.
///
/// **Why this exists (operator, 2026-08-21, recorded in
/// `websocket-connection-scope-lock.md`):** the authorized contract set —
/// full NIFTY/BANKNIFTY current-expiry chains plus every F&O stock's
/// current-expiry options at ATM ± 25 — does not fit inside the 25,000
/// subscription capacity alongside the master-sourced spot universe. The
/// code says so itself in `dhan_feed_stack`: ~4,565 spot instruments leave
/// ~20,435 for contracts against an authorized set of ~23,820. Narrowing the
/// spot side to indices + the underlyings we actually trade options on is the
/// lever that makes the operator's stated design fit, and it is the ONLY one
/// that does not change the contract shape he specified.
///
/// **Derived from the daily master, never a hardcoded list.** An F&O list
/// written into Rust goes stale on the next SEBI revision and nothing would
/// notice — the standing no-manual-intervention mandate forbids it. Both
/// passes below read the same master the rest of the build already parsed, so
/// this adds no fetch and no second parse.
///
/// # Complexity
/// Two O(n) passes over the master with O(1)-average hash operations — one to
/// collect the underlying SYMBOLS that derivatives name, one to resolve those
/// symbols to their cash-equity `security_id`. No nested scan: resolving by
/// filtering the master per underlying would be O(underlyings × rows), which
/// at ~220 × ~150,000 is the quadratic shape this codebase has already had to
/// repair three times.
///
/// # What it deliberately does NOT do
/// - It does not invent an underlying for a derivative whose `underlying_symbol`
///   is empty — that row is skipped and counted by the caller, never guessed.
/// - It does not include an underlying whose cash-equity row is absent from the
///   master. A symbol we cannot resolve to a `security_id` cannot be subscribed,
///   and emitting a zero id would subscribe instrument 0 and look healthy.
/// - It does not dedupe on `security_id` alone across segments: every row it
///   emits is NSE cash equity by construction, so the segment half of the
///   I-P1-11 composite is a constant here — stated at the filter rather than
///   assumed, exactly as `nse_index_mappings` does one function below.
fn fno_underlying_mappings(
    master: &[tickvault_core::instrument::master_csv::MasterRow],
) -> Vec<MappingEntry> {
    use tickvault_core::instrument::master_csv::InstrumentClass;

    // Pass 1 — which symbols do NSE stock derivatives name as their underlying?
    let mut wanted: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for row in master {
        if row.exch_id != "NSE" {
            continue;
        }
        if !matches!(
            row.class,
            InstrumentClass::StockFuture | InstrumentClass::StockOption
        ) {
            continue;
        }
        if row.underlying_symbol.is_empty() {
            continue;
        }
        wanted.insert(row.underlying_symbol.as_str());
    }

    // Pass 2 — resolve those symbols to their NSE cash-equity security_id.
    let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for row in master {
        if row.class != InstrumentClass::Equity || row.exch_id != "NSE" {
            continue;
        }
        // `EQ` is the cash series. A non-EQ series (BE, BZ, trade-to-trade)
        // is a different instrument with its own id, and subscribing it in
        // place of the EQ line would price the ATM window off the wrong book.
        if row.series != "EQ" {
            continue;
        }
        if !wanted.contains(row.symbol_name.as_str()) {
            continue;
        }
        // A zero id is the parser's "absent or unusable" answer. Subscribing
        // instrument 0 would look healthy and receive nothing.
        if row.security_id == 0 || !seen.insert(row.security_id) {
            continue;
        }
        out.push(MappingEntry {
            index_name: FNO_UNDERLYING_TAG.to_owned(),
            symbol: row.symbol_name.clone(),
            isin: row.isin.clone(),
            security_id: row.security_id,
            exchange_segment: tickvault_common::types::ExchangeSegment::NseEquity.binary_code(),
        });
    }
    out
}

/// universe widens through the path it already has: no new artifact, no new
/// parser, no new consumer. They carry `IDX_I` (segment code 0), which is
/// what makes them indices rather than equities on the wire.
///
/// `index_name` is the literal `NSE_INDEX` rather than a parent index, because
/// an index is not a constituent OF anything — the field exists to say which
/// list a row came from, and this row came from the master itself.
///
/// Deduped on `security_id` alone, deliberately, and that is NOT an I-P1-11
/// violation: every row here is `IDX_I` by construction, so the segment half
/// of the composite is a constant and a pair would add a field that cannot
/// vary. The equality is documented at the filter rather than assumed.
fn nse_index_mappings(
    master: &[tickvault_core::instrument::master_csv::MasterRow],
) -> Vec<MappingEntry> {
    use tickvault_core::instrument::master_csv::InstrumentClass;

    let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for row in master {
        if row.class != InstrumentClass::Index || row.exch_id != "NSE" {
            continue;
        }
        // A zero id is the parser's "absent or unusable" answer. Subscribing
        // instrument 0 would look healthy and receive nothing.
        if row.security_id == 0 || !seen.insert(row.security_id) {
            continue;
        }
        out.push(MappingEntry {
            index_name: "NSE_INDEX".to_owned(),
            symbol: row.symbol_name.clone(),
            isin: String::new(),
            security_id: row.security_id,
            exchange_segment: tickvault_common::types::ExchangeSegment::IdxI.binary_code(),
        });
    }
    out
}

/// Fraction of index lists that failed, in `[0.0, 1.0]`.
///
/// Routed through `u32` so `f64::from` is lossless — no precision-loss
/// `#[allow]`, and a count that somehow exceeded `u32::MAX` saturates toward
/// REJECTION rather than wrapping into a healthy-looking small number.
///
/// Returns `1.0` when `total` is zero: an empty slug list means nothing was
/// even attempted, and reporting a perfect 0.0 for it is the same false-OK the
/// gate exists to close.
fn failed_list_fraction(failed: usize, total: usize) -> f64 {
    if total == 0 {
        return 1.0;
    }
    let failed = u32::try_from(failed).unwrap_or(u32::MAX);
    let total = u32::try_from(total).unwrap_or(u32::MAX);
    f64::from(failed) / f64::from(total)
}

/// Parses one niftyindices constituent CSV.
///
/// Columns are resolved BY NAME (`Symbol`, `ISIN Code`) for the same reason
/// the master parser does: the layout is the vendor's to change.
///
/// Tokenizing uses the master parser's quote-aware [`split_csv_line`], NOT a
/// bare `split(',')`. The niftyindices layout is
/// `Company Name,Industry,Symbol,Series,ISIN Code` — the company name routinely
/// contains a comma and is therefore quoted, and the join key is the LAST
/// column, so a naive split shifts `ISIN Code` off the end. The failure is
/// silent and passes the tolerance gate: the row still yields a non-empty
/// "ISIN" (the value of some other column), which misses the index and is
/// reported as `IsinNotInMaster` against a garbled symbol.
fn parse_constituent_csv(index_name: &str, csv: &str) -> Vec<Constituent> {
    let csv = csv.strip_prefix('\u{feff}').unwrap_or(csv);
    let mut lines = csv.lines().filter(|l| !l.trim().is_empty());
    let Some(header) = lines.next() else {
        return Vec::new();
    };
    let mut fields: Vec<String> = Vec::new();
    split_csv_line(header.trim_end_matches('\r'), &mut fields);
    let find = |name: &str| fields.iter().position(|c| c.trim() == name);
    let (Some(i_sym), Some(i_isin)) = (find("Symbol"), find("ISIN Code")) else {
        // A list whose header we cannot read yields NOTHING rather than
        // guessing at positions. Zero constituents from one index is visible
        // in the counts; silently reading the wrong column is not.
        return Vec::new();
    };
    let widest = i_sym.max(i_isin);
    let mut out = Vec::new();
    for line in lines {
        split_csv_line(line.trim_end_matches('\r'), &mut fields);
        if fields.len() <= widest {
            continue;
        }
        out.push(Constituent {
            index_name: index_name.to_string(),
            symbol: fields[i_sym].trim().to_uppercase(),
            isin: fields[i_isin].trim().to_uppercase(),
        });
    }
    out
}

/// Runs one full daily build: download both sources, join, persist the map.
///
/// # Errors
/// Any failure that leaves the day without a trustworthy mapping. The caller
/// retries with backoff — it never proceeds on a partial result, because a
/// partial mapping subscribes a partial universe and reports success.
///
/// # Complexity — time AND space
///
/// Time is O(master rows + constituents), every step a single pass or a hash
/// probe; the network wait dominates by orders of magnitude.
///
/// Space is the part worth stating, because this is the process's peak: the
/// downloaded master body (~15 MB, capped at 50 MB), the parsed `Vec<MasterRow>`
/// (~150,000 rows × 5 owned `String`s), the ISIN index over its NSE-cash-equity
/// subset, the accumulated `Vec<Constituent>` across all 49 lists (~37,000
/// rows × 3 `String`s), and then the `JoinOutcome` plus the serialized JSON
/// artifact. Rough peak ~70–90 MB, held for the duration of one daily build on
/// a 32 GiB box and dropped when this function returns. It runs ONCE per day,
/// off the tick path, before the market opens — which is the only reason a
/// peak this shape is acceptable at all.
///
/// The per-list bodies are dropped as the loop advances, so the 50 MB download
/// cap bounds each list individually but NOT the accumulated `constituents`
/// vector — a vendor serving millions of minimal rows across 49 lists could
/// still grow it without limit. Recorded rather than fixed here: the honest
/// bound is a row-count ceiling per list, which needs a number the vendor's
/// real list sizes have to justify.
async fn build_once(date: &str, questdb: &QuestDbConfig) -> anyhow::Result<JoinOutcome> {
    let client = build_hardened_csv_client().map_err(|e| anyhow::anyhow!("{e}"))?;

    // 1. The Dhan master. Logged by LABEL, never by URL (§18 last row).
    info!(
        source = "dhan_master",
        date, "downloading instrument master"
    );
    let master_csv = fetch_csv_hardened(&client, DHAN_DETAILED_CSV_URL, None)
        .await
        .map_err(|e| anyhow::anyhow!("dhan master download: {e}"))?;
    let master = parse_master_csv(&master_csv).map_err(|e| anyhow::anyhow!("{e}"))?;
    let index = build_isin_index(&master);
    info!(
        source = "dhan_master",
        rows = master.len(),
        isins = index.len(),
        ambiguous = index.ambiguous.len(),
        "instrument master parsed"
    );

    // 1b. The derivative subset, written for the contract attach.
    //
    // Written HERE, while the parsed master is still in hand, because it is
    // dropped a few lines below and the attach runs much later — it waits for
    // live prices to locate at-the-money. Without this the attach would have
    // to re-download the same ~15 MB file in the minutes after the open.
    //
    // NON-FATAL by construction: a failure here costs the session its
    // contracts and leaves the spot universe untouched, so it must never
    // abort the mapping build that the whole rider exists for.
    let contract_rows = crate::dhan_contract_universe::contract_rows_from_master(&master);
    match crate::dhan_contract_universe::write_contract_artifact(date, &contract_rows) {
        Ok(()) => info!(
            source = "dhan_master",
            contracts = contract_rows.len(),
            "contract artifact written"
        ),
        Err(err) => error!(
            code = tickvault_common::error_code::ErrorCode::WsGapConnectionState.code_str(),
            %err,
            date,
            "contract artifact could NOT be written — the live lane will carry its spot \
             universe only, with no futures and no option contracts, until this is fixed. \
             The mapping build below is unaffected."
        ),
    }

    // 2. Every NSE India index list. A single list failing does NOT abort the
    // day: the tolerance gate below judges the RESULT, so one flaky index out
    // of ~49 degrades the fraction rather than losing the other 48. Aborting
    // on the first failure would make the whole pipeline as reliable as its
    // least reliable index.
    let mut constituents: Vec<Constituent> = Vec::new();
    let mut failed_lists = 0usize;
    for (display_name, slug) in INDEX_CONSTITUENCY_SLUGS {
        let url = nse_index_csv_url(slug);
        match fetch_csv_hardened(&client, &url, Some(nse_user_agent())).await {
            Ok(body) => {
                let rows = parse_constituent_csv(display_name, &body);
                if rows.is_empty() {
                    failed_lists += 1;
                    warn!(index = display_name, "index list parsed to zero rows");
                } else {
                    constituents.extend(rows);
                }
            }
            Err(e) => {
                failed_lists += 1;
                warn!(index = display_name, error = %e, "index list download failed");
            }
        }
    }
    // Published even on a build that goes on to succeed, so a partial download
    // is visible in the gauge as well as in the gate below.
    metrics::gauge!(INDEX_LISTS_FAILED_GAUGE).set(failed_lists as f64);
    info!(
        lists = INDEX_CONSTITUENCY_SLUGS.len(),
        failed = failed_lists,
        constituents = constituents.len(),
        "NSE India index lists downloaded"
    );

    // 2b. Reject a build that is missing too many lists ENTIRELY. This gate
    // exists because the unresolved-fraction gate below structurally cannot
    // see a missing list — see MAX_FAILED_INDEX_LIST_FRACTION.
    let failed_fraction = failed_list_fraction(failed_lists, INDEX_CONSTITUENCY_SLUGS.len());
    if failed_fraction > MAX_FAILED_INDEX_LIST_FRACTION {
        anyhow::bail!(
            "build REJECTED: {failed_lists} of {} NSE index lists failed ({:.1}%), above the \
             {:.1}% ceiling — the surviving lists would join cleanly and report a healthy day \
             while carrying a fraction of the intended membership",
            INDEX_CONSTITUENCY_SLUGS.len(),
            failed_fraction * 100.0,
            MAX_FAILED_INDEX_LIST_FRACTION * 100.0,
        );
    }

    // 3. The join — the actual deliverable.
    let outcome = join_constituents(&constituents, &index);
    let fraction = outcome.unresolved_fraction();

    // 4. Fail-closed. An empty join reports fraction 1.0 by construction, so
    // "every list failed to download" lands here as a rejection rather than
    // as a flawless zero-mismatch day.
    if fraction > NSE_MEMBERSHIP_TOLERANCE {
        for u in outcome.unresolved.iter().take(50) {
            error!(
                index = %u.index_name,
                symbol = %u.symbol,
                isin = %u.isin,
                reason = ?u.reason,
                "constituent unresolved"
            );
        }
        anyhow::bail!(
            "join REJECTED: {:.3}% of {} constituents unresolved, above the {:.1}% tolerance \
             (resolved={}, unresolved={})",
            fraction * 100.0,
            outcome.resolved.len() + outcome.unresolved.len(),
            NSE_MEMBERSHIP_TOLERANCE * 100.0,
            outcome.resolved.len(),
            outcome.unresolved.len()
        );
    }

    // Disk first, then the table. The artifact is the cheaper, more reliable
    // record; writing it before the network call means a QuestDB outage
    // cannot cost us the day's mapping.
    write_mapping_atomic(date, &master, &index, &outcome)?;

    // The never-delete lifecycle record.
    //
    // Operator directive 2026-08-19: an expired instrument is MARKED expired,
    // never removed, so the table answers "was this tradeable on that day?"
    // for every instrument that has ever existed.
    //
    // NON-FATAL: the mapping build is what the rider exists for, and a
    // lifecycle write that fails must not cost the day its universe.
    //
    // MOVED BELOW `write_mapping_atomic` on 2026-08-21, and the move is the
    // point. This is a ~150,000-row QuestDB round trip (a read of every
    // instrument ever seen, then chunked ILP writes). It used to run BEFORE
    // the mapping artifact — and the live lane blocks at boot waiting for
    // exactly that artifact, bounded by `MAPPING_WAIT_DEADLINE_SECS`. So the
    // lane's whole budget was being spent on a write nothing waits for, and
    // when it ran out the session fell back to 4 index SIDs for the day.
    //
    // The budget was 120 s, derived from a 9-second production build measured
    // on 2026-08-18 — before this write existed at all (it landed 2026-08-20
    // in #1773). Nobody re-derived it, so the number described a build that
    // no longer existed.
    //
    // This is the same "disk first, then the table" rule stated above for
    // `write_mapping_atomic` / `persist_constituents`: the artifact is the
    // cheaper, more reliable record and nothing slow may sit in front of it.
    // Everything here is unchanged except WHEN it runs.
    let today_ymd = crate::dhan_feed_stack::ymd_from_ist_date(date);
    let today_nanos = ist_midnight_nanos(date);
    match crate::dhan_lifecycle::write_dhan_lifecycle(questdb, &master, today_ymd, today_nanos, "")
        .await
    {
        Ok(tally) => info!(
            active = tally.active,
            expired_by_date = tally.expired_by_date,
            expired_by_absence = tally.expired_by_absence,
            "instrument lifecycle recorded"
        ),
        Err(err) => error!(
            code = tickvault_common::error_code::ErrorCode::WsGapConnectionState.code_str(),
            %err,
            date,
            "instrument lifecycle could NOT be written — nothing was deleted (the table is \
             append-and-upsert only), but today's expiry marks are missing, so a query \
             asking which instruments were tradeable today will read yesterday's answer."
        ),
    }
    persist_constituents(questdb, date, &outcome).await;
    Ok(outcome)
}

/// Persists the resolved mapping into the SEBI `index_constituency` table.
///
/// Runs only AFTER the join has passed its tolerance gate, so a rejected
/// build never reaches the table — the artifact on disk and the table can
/// disagree about a failed day, and the table is the one that must stay
/// clean.
///
/// # Why failure here does NOT fail the build
///
/// The mapping is already written to disk and already correct. A QuestDB
/// outage should not send the rider back to re-download 15 MB and redo a
/// join that succeeded; the DEDUP UPSERT keys make tomorrow's write
/// idempotent, so the row lands then. Reported at `error!` because a
/// persistent failure means the SEBI point-in-time history is developing a
/// hole, which is worth waking someone for — just not worth discarding good
/// work over.
async fn persist_constituents(questdb: &QuestDbConfig, date: &str, outcome: &JoinOutcome) {
    if outcome.resolved.is_empty() {
        // Unreachable while the tolerance gate stands (an empty join reports
        // fraction 1.0 and is rejected upstream), but asserted here anyway:
        // if that gate is ever loosened, this must not quietly write nothing
        // and log success.
        warn!("no resolved constituents to persist — skipping (this should be unreachable)");
        return;
    }

    tickvault_storage::index_constituency_persistence::ensure_index_constituency_table(questdb)
        .await;

    // The TRUNCATE migration is not feed-scoped: it wipes every row of every
    // feed. Writing before it opens means writing rows it then erases —
    // silently, since the write itself succeeds.
    let gate =
        tickvault_storage::index_constituency_persistence::index_constituency_migration_gate();
    if !gate
        .wait(Duration::from_secs(MIGRATION_GATE_WAIT_SECS))
        .await
    {
        error!(
            timeout_secs = MIGRATION_GATE_WAIT_SECS,
            "index_constituency migration gate did not open — SKIPPING the persist rather than \
             writing rows a later TRUNCATE would erase"
        );
        return;
    }

    let trading_date_ist_nanos = ist_midnight_nanos(date);
    let rows: Vec<tickvault_storage::index_constituency_persistence::IndexConstituencyRow<'_>> =
        outcome
            .resolved
            .iter()
            .map(|r| {
                tickvault_storage::index_constituency_persistence::IndexConstituencyRow {
                    trading_date_ist_nanos,
                    index_name: &r.index_name,
                    // The table's column is i64; a security_id above i64::MAX
                    // cannot exist in the master (it parsed from a decimal
                    // field), but saturating beats wrapping into a negative id.
                    security_id: i64::try_from(r.security_id).unwrap_or(i64::MAX),
                    exchange_segment: r.exchange_segment.as_str(),
                    symbol_name: &r.symbol,
                    isin: &r.isin,
                    // Every row here came from the ISIN primary key — the join
                    // has no symbol-fallback path. Stamped true honestly, not
                    // by default: if a fallback is ever added, this must
                    // become per-row or the provenance column starts lying.
                    via_isin: true,
                    source: CONSTITUENCY_SOURCE,
                    dry_run: false,
                    feed: tickvault_storage::index_constituency_persistence::INDEX_CONSTITUENCY_FEED_DHAN,
                }
            })
            .collect();

    match tickvault_storage::index_constituency_persistence::append_index_constituency_rows(
        questdb, &rows,
    )
    .await
    {
        Ok(()) => {
            info!(
                rows = rows.len(),
                date, "index_constituency rows persisted (feed=dhan)"
            );
            // ONLY on the success arm. Absence from today's rows is the sole
            // evidence a constituent was dropped at a rebalance — and a FAILED
            // write produces exactly the same absence. Running this after an
            // error would read our own outage as "every stock left its index"
            // and expire the live universe in one pass. The gating is the
            // safety property; nothing inside the expiry call can recover it.
            tickvault_storage::index_constituency_persistence::mark_missing_index_constituents_expired(
                questdb,
                tickvault_storage::index_constituency_persistence::INDEX_CONSTITUENCY_FEED_DHAN,
                false,
                trading_date_ist_nanos,
            )
            .await;
        }
        Err(err) => error!(
            rows = rows.len(),
            date,
            error = %err,
            "index_constituency persist FAILED — the mapping file is still correct on disk and \
             tomorrow's write is DEDUP-idempotent, but today's point-in-time row is missing. \
             The rebalance-expiry pass is deliberately SKIPPED: a failed write is \
             indistinguishable from every constituent being dropped, so running it here \
             would expire the live universe"
        ),
    }
}

/// IST midnight of `date` (`YYYY-MM-DD`) in epoch nanoseconds.
///
/// IST wall-clock stamped as epoch, per the house convention: the tick and
/// audit tables store IST-as-epoch and NEVER add the +5:30 offset a second
/// time. Getting this wrong shifts every row by 5.5 hours into the wrong
/// trading day.
#[must_use]
pub fn ist_midnight_nanos(date: &str) -> i64 {
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .and_then(|dt| dt.and_utc().timestamp_nanos_opt())
        .unwrap_or(0)
}

/// Writes the day's mapping atomically: temp file, then rename.
///
/// A half-written artifact that a later reader parses is worse than no
/// artifact — rename is atomic on the same filesystem, so a reader sees
/// either the previous day's complete file or today's complete file, never a
/// truncated one.
fn write_mapping_atomic(
    date: &str,
    master: &[tickvault_core::instrument::master_csv::MasterRow],
    index: &tickvault_core::instrument::master_csv::IsinIndex,
    outcome: &JoinOutcome,
) -> anyhow::Result<()> {
    // Path-traversal guard BEFORE the filename is built (§18 row 4). The date
    // is derived internally, but a validated input is validated regardless of
    // where it came from — that is what makes it a guard rather than a habit.
    anyhow::ensure!(
        date.len() == 10 && date.bytes().all(|b| b.is_ascii_digit() || b == b'-'),
        "refusing to write a mapping for a malformed date {date:?}"
    );
    let artifact = MappingArtifact {
        trading_date_ist: date.to_string(),
        master_rows: master.len(),
        isin_index_len: index.len(),
        ambiguous_isins: index.ambiguous.len(),
        constituents_seen: outcome.resolved.len() + outcome.unresolved.len(),
        resolved: outcome.resolved.len(),
        unresolved: outcome.unresolved.len(),
        unresolved_fraction: outcome.unresolved_fraction(),
        // Filled below, once the NSE index entries have been appended: the
        // count must cover the WHOLE mappings array, not just the join half.
        distinct_instruments: 0,
        unresolved_detail: outcome
            .unresolved
            .iter()
            .map(|u| format!("{}/{} ({}) {:?}", u.index_name, u.symbol, u.isin, u.reason))
            .collect(),
        mappings: outcome
            .resolved
            .iter()
            .map(|r| MappingEntry {
                index_name: r.index_name.clone(),
                symbol: r.symbol.clone(),
                isin: r.isin.clone(),
                security_id: r.security_id,
                // `binary_code()`, NOT `as u8`: Dhan's wire codes have a GAP
                // (there is no 6, between MCX_COMM=5 and BSE_CURRENCY=7), so
                // declaration order and wire value diverge for BseCurrency
                // (6 vs 7) and BseFno (7 vs 8). The artifact is consumed as
                // Dhan segment codes, so it must carry Dhan's numbering.
                // Latent today — the join only ever emits NseEquity, whose
                // two numberings coincide at 1 — which is exactly why it
                // would have gone unnoticed until the first non-equity row.
                exchange_segment: r.exchange_segment.binary_code(),
            })
            .collect(),
    };
    // The indices go in AFTER the constituents so the join's own output is
    // never reordered by this addition — a diff between two days stays a diff
    // about the market, not about where a block was spliced.
    let mut artifact = artifact;
    let indices = nse_index_mappings(master);
    let index_count = indices.len();
    artifact.mappings.extend(indices);
    artifact.distinct_instruments = distinct_instrument_count(&artifact.mappings);
    metrics::gauge!(DISTINCT_INSTRUMENTS_GAUGE).set(artifact.distinct_instruments as f64);
    std::fs::create_dir_all(MAPPING_DIR)?;
    let path = mapping_artifact_path(date);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&artifact)?)?;
    std::fs::rename(&tmp, &path)?;
    info!(
        path = %path.display(),
        resolved = artifact.resolved,
        unresolved = artifact.unresolved,
        nse_indices = index_count,
        mappings = artifact.mappings.len(),
        distinct_instruments = artifact.distinct_instruments,
        "instrument mapping written"
    );

    // The F&O underlying set, written AFTER the mapping artifact has landed
    // and deliberately NOT allowed to fail this function.
    //
    // Ordering is the point: the mapping artifact is what the live lane blocks
    // on at boot. If deriving or writing the F&O file went wrong, returning
    // `Err` here would report the whole build as failed when the artifact the
    // lane actually needs is already on disk and correct -- trading a real
    // outage for a cosmetic one.
    //
    // The consumer treats an absent file as "narrowing was requested and is
    // NOT in effect" and falls back loudly, so a miss here degrades to today's
    // behaviour rather than to a silently narrower universe. That is the
    // safe direction: too many instruments is a capacity error the lane
    // reports; too few is a coverage hole nothing reports.
    let narrowed = narrowed_spot_mappings(master);
    let fno_count = narrowed
        .iter()
        .filter(|e| e.index_name == FNO_UNDERLYING_TAG)
        .count();
    let index_count_narrowed = narrowed.len() - fno_count;
    match write_fno_underlying_artifact(date, narrowed) {
        Ok(fno_path) => info!(
            path = %fno_path.display(),
            fno_underlyings = fno_count,
            nse_indices = index_count_narrowed,
            "F&O underlying set written"
        ),
        Err(e) => error!(
            code = tickvault_common::error_code::ErrorCode::WsGapConnectionState.code_str(),
            error = %e,
            fno_underlyings = fno_count,
            "F&O underlying artifact could not be written — a narrowed-universe \
             boot will fall back to the master-sourced spot set and say so"
        ),
    }

    // The NTM spot set (NSE indices + Nifty Total Market), written on the same
    // terms as the F&O one directly above and for the same reasons: AFTER the
    // mapping artifact the live lane blocks on, and never able to fail this
    // function. A miss here degrades to today's behaviour — the wider set —
    // not to a silently narrower universe.
    let ntm = ntm_spot_mappings(master, outcome);
    let ntm_constituents = ntm
        .iter()
        .filter(|e| e.index_name == NTM_INDEX_NAME)
        .count();
    let ntm_indices = ntm.len() - ntm_constituents;
    if ntm_constituents == 0 {
        // Counted as an ERROR, not a warning, and deliberately not fatal.
        // Writing the file anyway would hand the reader an indices-only set
        // that looks like a successful narrowing; refusing to write it makes
        // the consumer fall THROUGH to the full master and say so, which is
        // the direction that never loses coverage silently.
        error!(
            code = tickvault_common::error_code::ErrorCode::WsGapConnectionState.code_str(),
            date,
            list = NTM_INDEX_NAME,
            "the Nifty Total Market list resolved ZERO constituents — the NTM spot artifact \
             was NOT written, so an NTM-narrowed boot falls through to the full \
             master-sourced set. Indices alone would have looked like a successful narrowing."
        );
    } else {
        match write_spot_artifact(ntm_spot_artifact_path(date), ntm) {
            Ok(p) => info!(
                path = %p.display(),
                ntm_constituents,
                nse_indices = ntm_indices,
                "NTM spot set written"
            ),
            Err(e) => error!(
                code = tickvault_common::error_code::ErrorCode::WsGapConnectionState.code_str(),
                error = %e,
                ntm_constituents,
                "NTM spot artifact could not be written — an NTM-narrowed boot will fall \
                 back to the master-sourced spot set and say so"
            ),
        }
    }

    Ok(())
}

/// The NTM spot universe: NSE indices PLUS the Nifty Total Market constituents.
///
/// # Why this exists as its own function (operator, 2026-08-22)
///
/// Every one of the ~750 Nifty Total Market rows was ALREADY being resolved —
/// `build_once` downloads `ind_niftytotalmarket_list` with the other 48 lists
/// and `join_constituents` matches each row to the Dhan master by ISIN. What
/// did not exist was anything that took THAT list back out: the join's dedup
/// key is `(index_name, security_id, segment)`, scoped per list, so the
/// artifact carries the UNION of all 49 and the live selector dedupes it to
/// ~4,565 SIDs. The operator asked for one list and got the pile.
///
/// So this is a SELECTION, not a new fetch: no extra download, no second
/// parser, no new failure mode on the network path.
///
/// # Why the indices half is not optional
///
/// Same argument as `narrowed_spot_mappings`, and it is not theoretical:
/// `select_live_universe` REPLACES the four hardcoded index seeds with the
/// artifact's `IDX_I` rows and leaves the seeds standing only when there are
/// none. An artifact of constituents alone would therefore ship 4 indices
/// instead of ~119 while every log line still read "widened".
///
/// # What it deliberately does NOT do
/// - It does not fall back to a different list when NTM resolves to nothing.
///   An empty NTM half means the vendor served us something wrong, and
///   substituting Nifty 500 would answer a different question than the one
///   asked while looking identical downstream. The caller counts and reports
///   it; `resolve_live_universe` falls THROUGH to the full master.
/// - It does not re-filter by exchange. Every resolved constituent is NSE
///   cash equity by construction (`build_isin_index` indexes only that
///   subset), and every index row comes from `nse_index_mappings`, which
///   filters `exch_id == "NSE"` itself. Stated here rather than assumed,
///   because "skip BSE" is an operator lock and a reader must be able to see
///   where it holds.
///
/// # Complexity
///
/// O(indices + resolved constituents) — one pass over the master for the
/// index half and one pass over the join's output for the other, with a
/// string equality per row. Cold path, once per day, off the tick path.
fn ntm_spot_mappings(
    master: &[tickvault_core::instrument::master_csv::MasterRow],
    outcome: &JoinOutcome,
) -> Vec<MappingEntry> {
    let mut out = nse_index_mappings(master);
    for r in &outcome.resolved {
        if r.index_name != NTM_INDEX_NAME {
            continue;
        }
        out.push(MappingEntry {
            index_name: r.index_name.clone(),
            symbol: r.symbol.clone(),
            isin: r.isin.clone(),
            security_id: r.security_id,
            // `binary_code()`, never `as u8`: Dhan's wire codes have a gap at
            // 6, so declaration order and wire value diverge above it. Same
            // reasoning as the mapping artifact's own write.
            exchange_segment: r.exchange_segment.binary_code(),
        });
    }
    out
}
/// The NARROWED spot universe: NSE indices PLUS the F&O stock underlyings.
///
/// The indices half is not optional and is the reason this function exists
/// rather than the write site calling `fno_underlying_mappings` directly.
/// `select_live_universe` REPLACES the hardcoded index seeds with the
/// artifact's index rows and leaves the seeds standing when there are none —
/// so an artifact carrying only underlyings would have produced 4 indices
/// instead of ~119, silently, while every log line still read "widened".
/// Composing both halves in ONE named function is what makes that a testable
/// claim instead of an assumption about a call site.
///
/// Indices go FIRST: the file's whole point is "indices + underlyings", and a
/// reader opening it should meet the anchor set at the top.
fn narrowed_spot_mappings(
    master: &[tickvault_core::instrument::master_csv::MasterRow],
) -> Vec<MappingEntry> {
    let mut out = nse_index_mappings(master);
    out.extend(fno_underlying_mappings(master));
    out
}

/// Serialise the NARROWED SPOT SET — NSE indices plus the F&O stock
/// underlyings — atomically (tmp then rename), so a reader
/// never observes a half-written file. Same shape as the mapping artifact's
/// own write for exactly that reason.
fn write_fno_underlying_artifact(
    date: &str,
    entries: Vec<MappingEntry>,
) -> std::io::Result<std::path::PathBuf> {
    write_spot_artifact(fno_underlying_artifact_path(date), entries)
}

/// Serialise ONE narrowed spot set atomically (tmp then rename) to `path`.
///
/// One function for both narrowed sets rather than two near-identical writers.
/// The duplication it removes is not cosmetic: the tmp-then-rename is the only
/// thing stopping a reader from parsing a half-written file, and a second copy
/// is a second place for that to be got subtly wrong — the exact shape of bug
/// this file has already recorded twice for duplicated filenames.
///
/// The envelope stays [`FnoUnderlyingArtifact`] for both, deliberately: the
/// consumer is `parse_mapping_artifact`, which reads `mappings` and nothing
/// else, so a second envelope type would add a second parser for an identical
/// payload. The type name now under-describes what it carries; renaming it
/// would touch the F&O artifact's on-disk shape, which is a separate change.
fn write_spot_artifact(
    path: std::path::PathBuf,
    entries: Vec<MappingEntry>,
) -> std::io::Result<std::path::PathBuf> {
    std::fs::create_dir_all(MAPPING_DIR)?;
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(&FnoUnderlyingArtifact {
        count: entries.len(),
        mappings: entries,
    })
    .map_err(std::io::Error::other)?;
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Spawns the SUPERVISED daily rider.
///
/// # Why the supervisor exists (added on review of my own first draft)
///
/// The first version of this function was a single `tokio::spawn` around the
/// day loop. A panic anywhere inside — a slice index, an unwrap in a
/// dependency, an allocation failure — would kill the task, and NOTHING would
/// restart it. The daily download would simply stop happening, forever,
/// with no error after the panic line and every other signal still green.
/// That is the precise failure this codebase keeps finding and this rider
/// would have shipped it.
///
/// So the shape now matches `groww_universe.rs` exactly: an outer loop owns
/// an inner `JoinHandle`, and since the inner task is an infinite loop, ANY
/// resolution of that handle is abnormal. Cancellation is the one legitimate
/// exit (graceful shutdown) and returns; everything else counts, logs, backs
/// off and respawns.
///
/// Honest limit: release builds use `panic = "abort"`, so the respawn arm is
/// reachable only in unwind builds. The counter is what makes a panic loop
/// visible in production, where the process dies instead.
#[must_use]
// TEST-EXEMPT: tokio supervisor wiring over an infinite daily control-plane loop driving live
// network + QuestDB I/O. The pure primitives it composes (`next_wait`, `ist_secs_of_day`,
// `parse_constituent_csv`, `ist_midnight_nanos`) are unit-tested in this module.
pub fn spawn_dhan_universe_rider(
    config: DhanUniverseConfig,
    questdb: QuestDbConfig,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if !config.enabled {
            info!("[dhan_universe] disabled — no instrument master download this boot");
            return;
        }
        // Pre-register at 0 so a dashboard distinguishes "never respawned"
        // from "never reported" — the first-sample-baseline discipline the
        // loss counters got today.
        metrics::counter!(RESPAWN_COUNTER, "reason" => "panic").increment(0);
        metrics::counter!(BUILD_COUNTER, "outcome" => "ok").increment(0);
        metrics::counter!(BUILD_COUNTER, "outcome" => "failed").increment(0);
        loop {
            let inner = tokio::spawn(run_dhan_universe_rider(config.clone(), questdb.clone()));
            let result = inner.await;
            if let Err(join_err) = &result
                && join_err.is_cancelled()
            {
                // Graceful shutdown teardown — not an abort.
                return;
            }
            let reason = tickvault_storage::disk_health_watcher::classify_join_exit(&result);
            metrics::counter!(RESPAWN_COUNTER, "reason" => reason).increment(1);
            error!(
                reason,
                backoff_secs = RESPAWN_BACKOFF_SECS,
                "[dhan_universe] daily rider task DIED — respawning. Until this respawn \
                 completes there is no daily instrument mapping being produced."
            );
            tokio::time::sleep(Duration::from_secs(RESPAWN_BACKOFF_SECS)).await;
        }
    })
}

/// The rider loop body (supervised above): one build per IST day, forever.
async fn run_dhan_universe_rider(config: DhanUniverseConfig, questdb: QuestDbConfig) {
    {
        info!(
            target_secs_of_day_ist = config.target_secs_of_day_ist,
            "[dhan_universe] daily instrument master + NSE indices rider armed"
        );
        let mut built_for: Option<String> = None;
        let mut attempt: u32 = 0;
        loop {
            let today = today_ist_date();
            let already = built_for.as_deref() == Some(today.as_str());
            let now_sod = ist_secs_of_day(chrono::Utc::now().timestamp());
            if let Some(wait) = next_wait(now_sod, config.target_secs_of_day_ist, already) {
                tokio::time::sleep(wait).await;
                continue;
            }
            // A whole-build deadline, on top of the per-request timeouts. Those
            // bound each HTTP attempt; nothing bounded the SUM of ~50 sequential
            // fetches plus the parse and the QuestDB flush, and the ILP flush in
            // particular has no timeout of its own — a QuestDB that accepts the
            // connection and then stops reading blocks the writer forever. That
            // wedges this loop with the task still ALIVE, so the supervisor
            // never fires, no counter moves, and the rider is silently gone.
            // The deadline turns that into an ordinary failed attempt that
            // retries on the backoff ladder.
            let build = tokio::time::timeout(
                Duration::from_secs(BUILD_DEADLINE_SECS),
                build_once(&today, &questdb),
            )
            .await
            .unwrap_or_else(|_| {
                Err(anyhow::anyhow!(
                    "build exceeded the {BUILD_DEADLINE_SECS}s deadline — treating as a failed \
                     attempt so the retry ladder runs instead of the rider wedging silently"
                ))
            });
            match build {
                Ok(outcome) => {
                    attempt = 0;
                    built_for = Some(today.clone());
                    metrics::counter!(BUILD_COUNTER, "outcome" => "ok").increment(1);
                    metrics::gauge!(RESOLVED_GAUGE).set(outcome.resolved.len() as f64);
                    // The fraction is the health signal that matters: it moves
                    // BEFORE the build starts failing outright, so a vendor
                    // slowly dropping constituents is visible while it is still
                    // passing the gate.
                    metrics::gauge!(UNRESOLVED_FRACTION_GAUGE).set(outcome.unresolved_fraction());
                    info!(
                        date = %today,
                        resolved = outcome.resolved.len(),
                        unresolved = outcome.unresolved.len(),
                        unresolved_fraction = outcome.unresolved_fraction(),
                        "[dhan_universe] daily build COMPLETE"
                    );
                }
                Err(err) => {
                    metrics::counter!(BUILD_COUNTER, "outcome" => "failed").increment(1);
                    attempt = attempt.saturating_add(1);
                    let backoff = RETRY_BASE_SECS
                        .saturating_mul(1u64 << attempt.min(5))
                        .min(config.retry_backoff_cap_secs);
                    // Pull-until-success: never give up for the day. A vendor
                    // outage that clears at 10:00 must still produce the
                    // mapping; a rider that stopped after N attempts would
                    // leave the day silently unmapped.
                    if attempt <= 3 {
                        warn!(date = %today, attempt, backoff_secs = backoff, error = %err,
                              "[dhan_universe] build failed — retrying");
                    } else {
                        error!(date = %today, attempt, backoff_secs = backoff, error = %err,
                               "[dhan_universe] build STILL failing — the day has no mapping yet");
                    }
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {

    // ---- F&O stock underlying derivation (operator 2026-08-21) ----

    fn mrow(
        id: u64,
        sym: &str,
        class: tickvault_core::instrument::master_csv::InstrumentClass,
        underlying: &str,
        series: &str,
        exch: &str,
    ) -> tickvault_core::instrument::master_csv::MasterRow {
        tickvault_core::instrument::master_csv::MasterRow {
            security_id: id,
            isin: String::new(),
            symbol_name: sym.to_owned(),
            exch_id: exch.to_owned(),
            segment: String::new(),
            series: series.to_owned(),
            class,
            expiry_ymd: 0,
            strike_paise: 0,
            option_leg: tickvault_core::instrument::master_csv::OptionLeg::None,
            underlying_symbol: underlying.to_owned(),
        }
    }

    #[test]
    fn the_narrowed_spot_set_carries_the_indices_and_not_only_the_underlyings() {
        use tickvault_core::instrument::master_csv::InstrumentClass;
        let master = vec![
            mrow(13, "NIFTY 50", InstrumentClass::Index, "", "", "NSE"),
            mrow(25, "NIFTY BANK", InstrumentClass::Index, "", "", "NSE"),
            mrow(500, "RELIANCE", InstrumentClass::Equity, "", "EQ", "NSE"),
            mrow(
                900,
                "RELIANCE28AUGFUT",
                InstrumentClass::StockFuture,
                "RELIANCE",
                "",
                "NSE",
            ),
        ];

        let out = narrowed_spot_mappings(&master);

        // The defect this test exists for: an artifact of underlyings ALONE
        // leaves `select_live_universe` standing on the 4 hardcoded seeds,
        // so ~115 NSE indices vanish while the log still says "widened".
        let indices: Vec<u64> = out
            .iter()
            .filter(|m| {
                m.exchange_segment == tickvault_common::types::ExchangeSegment::IdxI.binary_code()
            })
            .map(|m| m.security_id)
            .collect();
        assert_eq!(
            indices,
            vec![13, 25],
            "every NSE index must be in the narrowed set — it is half of what the operator named"
        );

        let underlyings: Vec<u64> = out
            .iter()
            .filter(|m| m.index_name == FNO_UNDERLYING_TAG)
            .map(|m| m.security_id)
            .collect();
        assert_eq!(underlyings, vec![500], "and the F&O underlying half too");

        assert_eq!(out.len(), 3, "indices + underlyings, nothing else");
    }

    #[test]
    fn a_master_with_no_indices_narrows_to_the_underlyings_without_inventing_any() {
        use tickvault_core::instrument::master_csv::InstrumentClass;
        let master = vec![
            mrow(500, "RELIANCE", InstrumentClass::Equity, "", "EQ", "NSE"),
            mrow(
                900,
                "RELIANCE28AUGFUT",
                InstrumentClass::StockFuture,
                "RELIANCE",
                "",
                "NSE",
            ),
        ];
        let out = narrowed_spot_mappings(&master);
        assert_eq!(out.len(), 1, "no index rows are fabricated to fill a gap");
        assert_eq!(out[0].security_id, 500);
    }

    #[test]
    fn fno_underlying_artifact_path_is_date_stamped_and_never_collides_with_the_mapping_file() {
        let a = fno_underlying_artifact_path("2026-08-21");
        let b = fno_underlying_artifact_path("2026-08-22");
        assert_ne!(
            a, b,
            "the filename must carry the date — one shared name would let a \
             stale day's set be read as today's"
        );
        assert!(
            a.to_string_lossy().contains("2026-08-21"),
            "the date must appear verbatim so an operator can find the file"
        );
        assert_ne!(
            a,
            mapping_artifact_path("2026-08-21"),
            "must NOT collide with the mapping artifact: one overwriting the \
             other would feed the live lane the wrong universe entirely"
        );
        assert_eq!(
            a.parent(),
            mapping_artifact_path("2026-08-21").parent(),
            "same directory, so one cleanup path covers both"
        );
    }

    #[test]
    fn fno_artifact_round_trips_to_a_file_a_reader_can_actually_parse() {
        // The derivation being right is worth nothing if what lands on disk is
        // unreadable. This writes the real file through the real function and
        // parses it back as a reader would.
        let dir = std::path::Path::new(MAPPING_DIR);
        let date = "2099-01-02"; // far future: cannot collide with a real run
        let entries = vec![MappingEntry {
            index_name: FNO_UNDERLYING_TAG.to_owned(),
            symbol: "RELIANCE".to_owned(),
            isin: "INE002A01018".to_owned(),
            security_id: 2885,
            exchange_segment: tickvault_common::types::ExchangeSegment::NseEquity.binary_code(),
        }];
        let path = match write_fno_underlying_artifact(date, entries) {
            Ok(p) => p,
            // A sandbox with no write access must not fail the suite for a
            // reason that has nothing to do with the logic under test.
            Err(_) => return,
        };
        let body = std::fs::read_to_string(&path).expect("written file must be readable");
        let v: serde_json::Value = serde_json::from_str(&body).expect("must be valid JSON");
        assert_eq!(v["count"], 1, "count must match what was written");
        assert_eq!(v["mappings"][0]["security_id"], 2885);
        assert_eq!(
            v["mappings"][0]["index_name"], FNO_UNDERLYING_TAG,
            "the tag is how a consumer tells these from constituent rows"
        );
        assert_eq!(
            v["mappings"].as_array().map(Vec::len),
            Some(v["count"].as_u64().unwrap() as usize),
            "count and list length must agree — that mismatch is how a \
             truncated-but-still-parseable file is caught"
        );
        // No .tmp left behind: a reader that globbed the directory would
        // otherwise find a half-written sibling.
        assert!(
            !path.with_extension("json.tmp").exists(),
            "the temp file must be renamed away, never left beside the real one"
        );
        let _ = std::fs::remove_file(&path);
        let _ = dir; // silence unused in the early-return path
    }

    #[test]
    fn fno_underlyings_resolves_only_stocks_that_actually_have_derivatives() {
        use tickvault_core::instrument::master_csv::InstrumentClass as C;
        let master = vec![
            // RELIANCE has options -> wanted, and its EQ row resolves it.
            mrow(2885, "RELIANCE", C::Equity, "", "EQ", "NSE"),
            mrow(
                50001,
                "RELIANCE24SEP",
                C::StockOption,
                "RELIANCE",
                "",
                "NSE",
            ),
            // TCS has a future -> wanted.
            mrow(11536, "TCS", C::Equity, "", "EQ", "NSE"),
            mrow(50002, "TCS24SEPFUT", C::StockFuture, "TCS", "", "NSE"),
            // ZEEL is cash-only -> must NOT be selected.
            mrow(3812, "ZEEL", C::Equity, "", "EQ", "NSE"),
        ];
        let got = fno_underlying_mappings(&master);
        let ids: Vec<u64> = got.iter().map(|m| m.security_id).collect();
        assert_eq!(
            ids,
            vec![2885, 11536],
            "only underlyings that actually carry derivatives may be subscribed"
        );
        assert!(
            got.iter().all(|m| m.index_name == FNO_UNDERLYING_TAG),
            "every emitted row must be tagged so the consumer can filter without re-parsing"
        );
    }

    #[test]
    fn fno_underlyings_never_emit_a_zero_or_duplicate_id() {
        use tickvault_core::instrument::master_csv::InstrumentClass as C;
        let master = vec![
            mrow(50003, "INFY24SEP", C::StockOption, "INFY", "", "NSE"),
            // id 0 is the parser's "unusable" answer -- subscribing instrument
            // 0 would look healthy and receive nothing.
            mrow(0, "INFY", C::Equity, "", "EQ", "NSE"),
            mrow(1594, "INFY", C::Equity, "", "EQ", "NSE"),
            mrow(1594, "INFY", C::Equity, "", "EQ", "NSE"),
        ];
        let ids: Vec<u64> = fno_underlying_mappings(&master)
            .iter()
            .map(|m| m.security_id)
            .collect();
        assert_eq!(ids, vec![1594], "zero refused, duplicate deduped");
    }

    #[test]
    fn fno_underlyings_take_the_eq_series_not_a_trade_to_trade_line() {
        use tickvault_core::instrument::master_csv::InstrumentClass as C;
        let master = vec![
            mrow(50004, "IDEA24SEP", C::StockOption, "IDEA", "", "NSE"),
            // Same symbol, different series -> a DIFFERENT instrument with its
            // own book. Pricing the ATM window off it would centre the strike
            // window on the wrong price.
            mrow(9999, "IDEA", C::Equity, "", "BE", "NSE"),
            mrow(14366, "IDEA", C::Equity, "", "EQ", "NSE"),
        ];
        let ids: Vec<u64> = fno_underlying_mappings(&master)
            .iter()
            .map(|m| m.security_id)
            .collect();
        assert_eq!(ids, vec![14366], "only the EQ cash line may be subscribed");
    }

    #[test]
    fn fno_underlyings_skip_a_derivative_with_no_underlying_named() {
        use tickvault_core::instrument::master_csv::InstrumentClass as C;
        let master = vec![
            // Empty underlying -> skipped, never guessed from the symbol text.
            mrow(50005, "MYSTERY24SEP", C::StockOption, "", "", "NSE"),
            mrow(4321, "MYSTERY", C::Equity, "", "EQ", "NSE"),
        ];
        assert!(
            fno_underlying_mappings(&master).is_empty(),
            "an underlying we cannot read is never inferred"
        );
    }

    #[test]
    fn fno_underlyings_ignore_other_exchanges_and_index_derivatives() {
        use tickvault_core::instrument::master_csv::InstrumentClass as C;
        let master = vec![
            // BSE stock option -> out of scope for the NSE spot set.
            mrow(60001, "SENSTK24SEP", C::StockOption, "SENSTK", "", "BSE"),
            mrow(60002, "SENSTK", C::Equity, "", "EQ", "BSE"),
            // Index options must not drag an "underlying equity" in -- NIFTY
            // has no cash-equity line, and inventing one would subscribe a
            // wrong id.
            mrow(60003, "NIFTY24SEP", C::IndexOption, "NIFTY", "", "NSE"),
            mrow(60004, "NIFTY", C::Index, "", "", "NSE"),
        ];
        assert!(
            fno_underlying_mappings(&master).is_empty(),
            "NSE cash equities only: no BSE rows, no index legs"
        );
    }

    #[test]
    fn fno_underlyings_do_not_rescan_the_master_per_underlying() {
        // Guards the complexity, not the output. Resolving by filtering the
        // master once per underlying is O(underlyings x rows) -- at ~220 x
        // ~150,000 that is the quadratic shape this codebase has repaired
        // three times. Two linear passes must stay linear: 10x the rows costs
        // ~10x, never ~100x.
        use tickvault_core::instrument::master_csv::InstrumentClass as C;
        let build = |n: u64| -> Vec<_> {
            let mut m = Vec::new();
            for i in 0..n {
                m.push(mrow(
                    100_000 + i,
                    &format!("S{i}OPT"),
                    C::StockOption,
                    &format!("S{i}"),
                    "",
                    "NSE",
                ));
                m.push(mrow(i + 1, &format!("S{i}"), C::Equity, "", "EQ", "NSE"));
            }
            m
        };
        let small = build(50);
        let large = build(500);
        assert_eq!(fno_underlying_mappings(&small).len(), 50);
        assert_eq!(
            fno_underlying_mappings(&large).len(),
            500,
            "10x the underlyings must still resolve every one"
        );
    }
    use super::*;

    #[test]
    fn test_next_wait_runs_immediately_when_the_target_already_passed() {
        // THE production case, and the reason this function exists. The box
        // powers on at 08:30 with an 08:00 target; a scheduler that waited
        // for the next 08:00 would skip the day entirely while looking
        // perfectly scheduled.
        assert_eq!(
            next_wait(8 * 3600 + 1800, 8 * 3600, false),
            None,
            "08:30 with an 08:00 target and no build today must run NOW"
        );
        assert_eq!(
            next_wait(15 * 3600, 8 * 3600, false),
            None,
            "even a late-afternoon boot must still build the day"
        );
    }

    #[test]
    fn test_next_wait_sleeps_to_the_target_when_it_is_still_ahead() {
        assert_eq!(
            next_wait(7 * 3600, 8 * 3600, false),
            Some(Duration::from_secs(3600)),
            "an hour before the target, sleep exactly an hour"
        );
        assert_eq!(
            next_wait(8 * 3600 - 1, 8 * 3600, false),
            Some(Duration::from_secs(1))
        );
    }

    #[test]
    fn test_next_wait_at_exactly_the_target_runs_rather_than_sleeping_a_day() {
        // The boundary. `>` instead of `>=` here would sleep ~24h at exactly
        // the target instant — the one moment the rider is meant to fire.
        assert_eq!(next_wait(8 * 3600, 8 * 3600, false), None);
    }

    #[test]
    fn test_next_wait_after_a_successful_build_sleeps_to_tomorrows_target() {
        // Without this arm the rider re-downloads ~15 MB and rewrites the
        // mapping on every wake, which shows up in logs as nothing but
        // repeated success.
        let wait = next_wait(9 * 3600, 8 * 3600, true).expect("must sleep after a build");
        assert_eq!(
            wait,
            Duration::from_secs((SECS_PER_DAY as u64 - 9 * 3600) + 8 * 3600),
            "sleep must land on TOMORROW's target, not 24h from now"
        );
    }

    #[test]
    fn test_next_wait_survives_an_out_of_range_second_of_day() {
        // `next_wait` is `pub` and its caller supplies this value freely. The
        // release profile is `overflow-checks = true` + `panic = "abort"`, so
        // before 2026-08-19 a value at or past 86_400 did not return a wrong
        // duration — it ABORTED the trading process. These inputs are the
        // reason the arithmetic saturates.
        for bad in [86_400_u32, 86_401, 100_000, u32::MAX] {
            let wait = next_wait(bad, 8 * 3600, true)
                .expect("an out-of-range clock must still yield a wait, never a panic");
            assert_eq!(
                wait.as_secs(),
                8 * 3600,
                "saturating to zero seconds-to-midnight wakes EARLY (at today's \
                 target), which is the safe direction — never late, never never"
            );
        }
        // The exact boundary still behaves normally one second below.
        let ok = next_wait(86_399, 8 * 3600, true).expect("must sleep");
        assert_eq!(ok.as_secs(), 1 + 8 * 3600);
    }

    #[test]
    fn test_next_wait_after_build_does_not_drift_the_schedule() {
        // Anchoring to the target rather than to "now + 24h" is what stops a
        // slow build walking the schedule later every day until it leaves the
        // market window. Two different finish times must land on the same
        // next-target instant.
        let early = next_wait(8 * 3600 + 60, 8 * 3600, true).unwrap();
        let late = next_wait(8 * 3600 + 3600, 8 * 3600, true).unwrap();
        assert_eq!(
            early.as_secs() - late.as_secs(),
            3600 - 60,
            "a build finishing an hour later must sleep an hour less, landing on the same target"
        );
    }

    #[test]
    fn test_ist_secs_of_day_converts_utc_to_ist() {
        // 03:00 UTC is 08:30 IST — the box's actual start instant, and the
        // one that decides whether catch-up fires on a normal morning.
        let utc_0300 = 3 * 3600;
        assert_eq!(ist_secs_of_day(utc_0300), 8 * 3600 + 1800);
    }

    #[test]
    fn test_ist_secs_of_day_wraps_across_utc_midnight() {
        // 20:00 UTC is 01:30 IST the NEXT day. A conversion that did not wrap
        // would return a value past 86400 and every comparison against the
        // target would then be wrong for the 18:30-24:00 UTC window.
        let utc_2000 = 20 * 3600;
        assert_eq!(ist_secs_of_day(utc_2000), 3600 + 1800);
        assert!(ist_secs_of_day(utc_2000) < SECS_PER_DAY as u32);
    }

    #[test]
    fn test_ist_midnight_nanos_stamps_ist_wall_clock_without_a_second_offset() {
        // The house convention (data-integrity.md): IST wall-clock is stored
        // AS the epoch value — the +5:30 is never added a second time. Adding
        // it here would push every constituent row 5.5 hours forward, which
        // on a date boundary files it under the WRONG TRADING DAY. That is
        // invisible in a spot check and wrong in every point-in-time query.
        let nanos = ist_midnight_nanos("2026-08-11");
        let expected = chrono::NaiveDate::from_ymd_opt(2026, 8, 11)
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .and_then(|dt| dt.and_utc().timestamp_nanos_opt())
            .expect("a fixed valid date");
        assert_eq!(
            nanos, expected,
            "the stamp must be IST midnight as-epoch, with NO added offset"
        );
        // Explicitly assert the wrong answer is not produced.
        assert_ne!(
            nanos,
            expected + 19_800 * 1_000_000_000,
            "a second +5:30 offset would shift the row into the wrong day"
        );
    }

    #[test]
    fn test_ist_midnight_nanos_returns_zero_on_a_malformed_date() {
        // Fail-soft rather than panic: the rider must not die on the boot
        // path over a date it derived itself. Zero is an obviously-wrong
        // sentinel that shows up immediately in the table, rather than a
        // plausible-looking wrong timestamp that does not.
        assert_eq!(ist_midnight_nanos("not-a-date"), 0);
        assert_eq!(ist_midnight_nanos(""), 0);
        assert_eq!(
            ist_midnight_nanos("2026-02-30"),
            0,
            "a calendar-invalid date must not silently roll into March"
        );
    }

    #[test]
    fn test_parse_constituent_csv_reads_columns_by_name() {
        let csv = "Company Name,Industry,Symbol,Series,ISIN Code\n\
                   Reliance Industries,Energy,RELIANCE,EQ,INE002A01018";
        let rows = parse_constituent_csv("NIFTY 50", csv);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].symbol, "RELIANCE");
        assert_eq!(rows[0].isin, "INE002A01018");
        assert_eq!(rows[0].index_name, "NIFTY 50");
    }

    #[test]
    fn test_parse_constituent_csv_tolerates_reordered_columns() {
        // The vendor owns this layout. A positional parser would silently
        // read Industry as the symbol.
        let csv = "ISIN Code,Symbol,Company Name\nINE002A01018,RELIANCE,Reliance";
        let rows = parse_constituent_csv("NIFTY 50", csv);
        assert_eq!(rows[0].symbol, "RELIANCE");
        assert_eq!(rows[0].isin, "INE002A01018");
    }

    #[test]
    fn test_parse_constituent_csv_yields_nothing_on_an_unreadable_header() {
        // Zero rows from one index is visible in the counts and degrades the
        // tolerance fraction. Guessing at column positions is not visible at
        // all — it produces confident, wrong mappings.
        assert!(parse_constituent_csv("X", "A,B,C\n1,2,3").is_empty());
        assert!(parse_constituent_csv("X", "").is_empty());
        assert!(
            parse_constituent_csv("X", "Symbol,Company\nRELIANCE,Reliance").is_empty(),
            "a header missing ISIN Code must yield nothing, not symbol-only rows"
        );
    }

    #[test]
    fn test_parse_constituent_csv_strips_bom_and_crlf() {
        let csv = "\u{feff}Symbol,ISIN Code\r\nRELIANCE,INE002A01018\r\n";
        let rows = parse_constituent_csv("NIFTY 50", csv);
        assert_eq!(rows.len(), 1, "BOM + CRLF must not break the header");
        assert_eq!(
            rows[0].isin, "INE002A01018",
            "a trailing CR would make every ISIN fail to match the master"
        );
    }

    #[test]
    fn test_parse_constituent_csv_skips_short_rows_without_shifting() {
        let csv = "Symbol,ISIN Code\nRELIANCE,INE002A01018\nBROKEN";
        let rows = parse_constituent_csv("NIFTY 50", csv);
        assert_eq!(rows.len(), 1, "a short row is skipped, never padded");
    }

    #[test]
    fn test_parse_constituent_csv_survives_a_quoted_comma_in_the_company_name() {
        // REGRESSION (2026-08-11): this parser used a bare `split(',')` while
        // the master parser next door used a quote-aware tokenizer. The real
        // niftyindices layout puts the quoted company name FIRST and the join
        // key LAST, so one comma inside a company name shifted `ISIN Code` off
        // the end of the row. The constituent was then silently lost — and lost
        // QUIETLY, because the garbled row still produced a non-empty "ISIN"
        // that simply missed the master and counted as IsinNotInMaster.
        let csv = "Company Name,Industry,Symbol,Series,ISIN Code\n\
                   \"Foo, Bar Ltd.\",Energy,RELIANCE,EQ,INE002A01018\n\
                   Plain Ltd.,IT,INFY,EQ,INE009A01021";
        let rows = parse_constituent_csv("NIFTY 50", csv);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].symbol, "RELIANCE");
        assert_eq!(
            rows[0].isin, "INE002A01018",
            "a quoted comma must not shift the join key"
        );
        assert_eq!(rows[1].symbol, "INFY");
        assert_eq!(rows[1].isin, "INE009A01021");
    }

    fn master_row(
        security_id: u64,
        symbol: &str,
        exch: &str,
        class: tickvault_core::instrument::master_csv::InstrumentClass,
    ) -> tickvault_core::instrument::master_csv::MasterRow {
        tickvault_core::instrument::master_csv::MasterRow {
            security_id,
            isin: String::new(),
            symbol_name: symbol.to_owned(),
            exch_id: exch.to_owned(),
            segment: "I".into(),
            series: String::new(),
            class,
            expiry_ymd: 0,
            strike_paise: 0,
            option_leg: tickvault_core::instrument::master_csv::OptionLeg::None,
            underlying_symbol: String::new(),
        }
    }

    /// Builds one resolved constituent, tagged with the list it came from.
    fn resolved(
        index_name: &str,
        symbol: &str,
        security_id: u64,
    ) -> tickvault_core::instrument::master_csv::ResolvedConstituent {
        tickvault_core::instrument::master_csv::ResolvedConstituent {
            index_name: index_name.to_owned(),
            symbol: symbol.to_owned(),
            isin: format!("INE{security_id:09}"),
            security_id,
            exchange_segment: tickvault_common::types::ExchangeSegment::NseEquity,
        }
    }

    /// `NTM_INDEX_NAME` is a JOIN KEY, not a label — `join_constituents`
    /// stamps `index_name` from the slug table's display name and
    /// `ntm_spot_mappings` selects on it. A typo would not fail loudly: it
    /// would select ZERO constituents, the artifact would not be written, and
    /// the session would fall through to the full 4,565 set — which is exactly
    /// the state the operator was complaining about, restored silently.
    #[test]
    fn ntm_display_name_matches_a_real_slug() {
        assert!(
            tickvault_common::constants::INDEX_CONSTITUENCY_SLUGS
                .iter()
                .any(|(display, _)| *display == NTM_INDEX_NAME),
            "NTM_INDEX_NAME {NTM_INDEX_NAME:?} is not a display name in \
             INDEX_CONSTITUENCY_SLUGS — the join stamps index_name from that table, so this \
             selector would match nothing and narrow to indices alone"
        );
    }

    /// Three writers, three readers, one directory. A collision would have one
    /// set overwrite another and the lane would subscribe whichever ran last,
    /// with every log line still naming the set that was asked for.
    #[test]
    fn ntm_spot_artifact_path_never_collides_with_its_two_siblings() {
        let d = "2026-08-22";
        let paths = [
            mapping_artifact_path(d),
            fno_underlying_artifact_path(d),
            ntm_spot_artifact_path(d),
        ];
        for (i, a) in paths.iter().enumerate() {
            for b in paths.iter().skip(i + 1) {
                assert_ne!(a, b, "two spot artifacts share a filename: {a:?}");
            }
        }
        // And the date must actually reach the name, or every day overwrites
        // the last and a stale set is served as today's.
        assert!(
            ntm_spot_artifact_path(d).to_string_lossy().contains(d),
            "NTM artifact name drops the date"
        );
    }

    /// The whole point of the 2026-08-22 change: ONE list out of the 49, not
    /// the union. A selector that let a sibling list through would reproduce
    /// the ~4,565 pile under a name that claims to be ~750.
    #[test]
    fn ntm_spot_mappings_selects_only_the_total_market_list() {
        use tickvault_core::instrument::master_csv::{InstrumentClass, JoinOutcome};
        let master = vec![master_row(13, "NIFTY 50", "NSE", InstrumentClass::Index)];
        let outcome = JoinOutcome {
            resolved: vec![
                resolved(NTM_INDEX_NAME, "RELIANCE", 2885),
                resolved(NTM_INDEX_NAME, "TCS", 11536),
                resolved("Nifty 500", "SOMEOTHER", 4444),
                resolved("Nifty Microcap 250", "TINYCO", 5555),
            ],
            unresolved: Vec::new(),
        };
        let out = ntm_spot_mappings(&master, &outcome);
        let ids: Vec<u64> = out
            .iter()
            .filter(|e| e.index_name == NTM_INDEX_NAME)
            .map(|e| e.security_id)
            .collect();
        assert_eq!(ids, vec![2885, 11536], "took rows from a non-NTM list");
        assert!(
            !out.iter()
                .any(|e| e.security_id == 4444 || e.security_id == 5555),
            "a sibling list leaked into the NTM set"
        );
    }

    /// `select_live_universe` REPLACES the four hardcoded index seeds with the
    /// artifact's IDX_I rows whenever there is at least one. An NTM set of
    /// constituents alone would therefore ship 4 indices instead of ~119 while
    /// every log line still read "widened" — so the anchor set is not optional
    /// and this asserts it is present, with the right segment code.
    #[test]
    fn ntm_spot_mappings_carries_the_index_anchor_set() {
        use tickvault_core::instrument::master_csv::{InstrumentClass, JoinOutcome};
        let master = vec![
            master_row(13, "NIFTY 50", "NSE", InstrumentClass::Index),
            master_row(25, "NIFTY BANK", "NSE", InstrumentClass::Index),
            // BSE stays out — the operator lock, asserted where it holds.
            master_row(51, "SENSEX", "BSE", InstrumentClass::Index),
        ];
        let outcome = JoinOutcome {
            resolved: vec![resolved(NTM_INDEX_NAME, "RELIANCE", 2885)],
            unresolved: Vec::new(),
        };
        let out = ntm_spot_mappings(&master, &outcome);
        let idx: Vec<u64> = out
            .iter()
            .filter(|e| {
                e.exchange_segment == tickvault_common::types::ExchangeSegment::IdxI.binary_code()
            })
            .map(|e| e.security_id)
            .collect();
        assert_eq!(
            idx,
            vec![13, 25],
            "index anchor set wrong (BSE must not appear)"
        );
        assert_eq!(out.len(), 3, "expected 2 indices + 1 NTM constituent");
    }

    /// An NTM half that resolves to nothing must NOT be written. Writing it
    /// would hand the reader an indices-only file that parses cleanly and
    /// looks like a successful narrowing; not writing it makes the consumer
    /// fall through to the full master and say so.
    #[test]
    fn ntm_spot_mappings_with_no_constituents_is_indices_only_so_the_caller_can_refuse() {
        use tickvault_core::instrument::master_csv::{InstrumentClass, JoinOutcome};
        let master = vec![master_row(13, "NIFTY 50", "NSE", InstrumentClass::Index)];
        let outcome = JoinOutcome {
            resolved: vec![resolved("Nifty 50", "RELIANCE", 2885)],
            unresolved: Vec::new(),
        };
        let out = ntm_spot_mappings(&master, &outcome);
        assert_eq!(
            out.iter()
                .filter(|e| e.index_name == NTM_INDEX_NAME)
                .count(),
            0,
            "the caller's zero-constituent refusal would never trigger"
        );
    }

    /// The gap this closes: the ISIN join emits CONSTITUENTS, never the
    /// indices themselves, so the live universe had four hardcoded indices
    /// and no path to the rest.
    #[test]
    fn test_nse_index_mappings_collects_every_nse_index_as_idx_i() {
        use tickvault_core::instrument::master_csv::InstrumentClass;
        let master = vec![
            master_row(13, "NIFTY 50", "NSE", InstrumentClass::Index),
            master_row(25, "NIFTY BANK", "NSE", InstrumentClass::Index),
            master_row(999, "NIFTY MIDCAP 150", "NSE", InstrumentClass::Index),
            // Not an index.
            master_row(500, "RELIANCE", "NSE", InstrumentClass::Equity),
            // An index, but not NSE — out of scope per the 2026-08-20 narrowing.
            master_row(51, "SENSEX", "BSE", InstrumentClass::Index),
        ];
        let out = nse_index_mappings(&master);
        assert_eq!(out.len(), 3, "three NSE indices, and only those");
        assert!(
            out.iter().all(|m| m.exchange_segment
                == tickvault_common::types::ExchangeSegment::IdxI.binary_code())
        );
        assert!(out.iter().all(|m| m.index_name == "NSE_INDEX"));
        let ids: Vec<u64> = out.iter().map(|m| m.security_id).collect();
        assert_eq!(ids, vec![13, 25, 999]);
        assert!(
            !ids.contains(&51),
            "SENSEX is BSE — the operator narrowed to NSE alone"
        );
        assert!(!ids.contains(&500), "an equity is not an index");
    }

    /// A zero id is the parser's "unusable" answer. Subscribing instrument 0
    /// would look healthy on every counter and receive nothing.
    #[test]
    fn test_nse_index_mappings_refuses_a_zero_id_and_dedupes() {
        use tickvault_core::instrument::master_csv::InstrumentClass;
        let master = vec![
            master_row(0, "BROKEN", "NSE", InstrumentClass::Index),
            master_row(13, "NIFTY 50", "NSE", InstrumentClass::Index),
            master_row(13, "NIFTY 50", "NSE", InstrumentClass::Index),
        ];
        let out = nse_index_mappings(&master);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].security_id, 13);
    }

    /// An empty answer must stay empty rather than inventing a fallback: a
    /// master with no index rows is a master problem, and quietly substituting
    /// the old hardcoded four would hide it.
    #[test]
    fn test_nse_index_mappings_on_a_master_with_no_indices_is_empty() {
        use tickvault_core::instrument::master_csv::InstrumentClass;
        let master = vec![master_row(500, "RELIANCE", "NSE", InstrumentClass::Equity)];
        assert!(nse_index_mappings(&master).is_empty());
    }

    #[test]
    fn distinct_instrument_count_collapses_a_stock_that_sits_in_many_indices() {
        // The real shape: one stock (id 2885, NSE_EQ) is a member of three of
        // the 46 downloaded index lists, so the join emits three rows for it.
        let e = |index: &str, id: u64, seg: u8| MappingEntry {
            index_name: index.to_owned(),
            symbol: "RELIANCE".to_owned(),
            isin: "INE002A01018".to_owned(),
            security_id: id,
            exchange_segment: seg,
        };
        let mappings = vec![
            e("Nifty 50", 2885, 1),
            e("Nifty 100", 2885, 1),
            e("Nifty Total Market", 2885, 1),
            e("NSE_INDEX", 13, 0),
        ];
        assert_eq!(
            mappings.len(),
            4,
            "the artifact really does carry one row per (index, symbol) pair"
        );
        assert_eq!(
            distinct_instrument_count(&mappings),
            2,
            "one stock plus one index — this is what the feed subscribes"
        );
    }

    #[test]
    fn distinct_instrument_count_keeps_one_id_that_appears_in_two_segments() {
        // I-P1-11: the same numeric id in two segments is two instruments, and
        // collapsing them would under-count the subscription.
        let e = |id: u64, seg: u8| MappingEntry {
            index_name: "x".to_owned(),
            symbol: "x".to_owned(),
            isin: String::new(),
            security_id: id,
            exchange_segment: seg,
        };
        assert_eq!(distinct_instrument_count(&[e(27, 0), e(27, 1)]), 2);
    }

    #[test]
    fn test_mapping_artifact_uses_dhan_wire_segment_codes_not_declaration_order() {
        // The artifact's `exchange_segment` is consumed as a DHAN segment
        // code, and Dhan's numbering has a gap (no 6). `as u8` would give
        // declaration order, which diverges for exactly two variants — and
        // NOT for NseEquity, the only one the join emits today, so the bug
        // would stay invisible until the first non-equity mapping.
        use tickvault_common::types::ExchangeSegment;
        assert_eq!(ExchangeSegment::NseEquity.binary_code(), 1);
        assert_eq!(
            ExchangeSegment::BseCurrency.binary_code(),
            7,
            "declaration order says 6; the wire says 7"
        );
        assert_eq!(
            ExchangeSegment::BseFno.binary_code(),
            8,
            "declaration order says 7; the wire says 8"
        );
        // The divergence is real, so `as u8` is not an acceptable shortcut.
        assert_ne!(
            ExchangeSegment::BseFno as u8,
            ExchangeSegment::BseFno.binary_code()
        );
    }

    #[test]
    fn test_failed_list_fraction_treats_nothing_attempted_as_total_failure() {
        assert!(
            (failed_list_fraction(0, 49) - 0.0).abs() < f64::EPSILON,
            "a clean day is 0.0"
        );
        assert!((failed_list_fraction(49, 49) - 1.0).abs() < f64::EPSILON);
        // 4 of 49 ≈ 8.2% — under the ceiling, so a few flaky lists still build.
        assert!(failed_list_fraction(4, 49) <= MAX_FAILED_INDEX_LIST_FRACTION);
        // 5 of 49 ≈ 10.2% — over it.
        assert!(failed_list_fraction(5, 49) > MAX_FAILED_INDEX_LIST_FRACTION);
        // The false-OK case: an empty slug list means nothing was attempted,
        // which must read as total failure, never as a flawless 0.0.
        assert!(
            (failed_list_fraction(0, 0) - 1.0).abs() < f64::EPSILON,
            "nothing attempted must never render as a perfect score"
        );
    }

    #[test]
    fn test_max_failed_index_list_fraction_rejects_the_48_of_49_false_ok() {
        // The scenario the gate exists for: 48 lists fail, the 49th returns
        // clean data, so the unresolved fraction is a flawless 0.0 over the
        // handful of constituents that did arrive.
        let total = INDEX_CONSTITUENCY_SLUGS.len();
        assert!(total > 1, "the gate is meaningless with one list");
        let fraction = failed_list_fraction(total - 1, total);
        assert!(
            fraction > MAX_FAILED_INDEX_LIST_FRACTION,
            "{} of {total} lists failing ({fraction:.3}) must be REJECTED — a green build \
             here would carry a fraction of the intended membership",
            total - 1
        );
    }
}

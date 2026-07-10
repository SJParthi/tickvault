//! Date-keyed subscription-plan snapshot — instant warm-resubscribe cache.
//!
//! # Why this exists (the disaster-recovery contract)
//!
//! The daily-universe boot (`load_daily_universe_plan`) builds the
//! ~331-SID subscription set from the Dhan Detailed CSV. A full cold build
//! is the batched-seconds path (parse + extract + assemble + the ~219K-row
//! applicable-F&O lifecycle reconcile). That is correct for a *fresh* boot,
//! but it is the wrong cost for a *same-day crash recovery*: if the app
//! process is OOM-killed at 11:30 IST, the operator does not want to wait
//! for the whole cold rebuild before ticks flow again — the universe for
//! TODAY was already computed an hour ago.
//!
//! This module persists the minimal subscription plan to a **date-keyed
//! file on the host disk** (`data/instrument-cache/`), deliberately
//! SEPARATE from the QuestDB volume. That separation is the single most
//! important resilience choice here: a QuestDB volume wipe does NOT remove
//! this snapshot, so instant resubscribe survives a DB wipe.
//!
//! ## Tiered universe source (the recovery ladder)
//!
//! 1. **This snapshot** (host disk, `~1ms` to load + build the plan) — used
//!    only when its `trading_date_ist` matches today. Survives QuestDB wipe.
//! 2. **Cold CSV build** (batched seconds) — when no same-day snapshot
//!    exists (fresh box, first boot of the day, or stale snapshot).
//!
//! Both produce the same subscription plan. After an instant resubscribe
//! from tier 1, the caller still runs the full cold build in the
//! BACKGROUND to write the `instrument_lifecycle` master + refresh this
//! snapshot — so the fast path never skips the audit/lifecycle work, it
//! only DEFERS it past first-tick.
//!
//! ## What is (and isn't) in the snapshot
//!
//! The subscription-plan builder (`build_subscription_plan_from_daily_universe`)
//! reads only three fields per target: `role`, `csv_row.security_id`,
//! `csv_row.symbol_name`. So the snapshot stores exactly those — it is NOT
//! a full instrument-master mirror. The `fno_contracts` master-only rows are
//! intentionally absent: they are never subscribed, and the background
//! reconcile rebuilds them. This keeps the snapshot tiny (~331 rows) and
//! makes it human-inspectable JSON.
//!
//! ## Honest envelope
//!
//! This does NOT detect an intraday change to the universe — it is a
//! same-day warm cache, keyed on the IST trading date. If the operator
//! force-rebuilds the universe mid-day, the snapshot is refreshed by the
//! background reconcile, not by this loader. The loader trusts a same-date
//! snapshot; correctness of "is today's universe still valid" is the
//! background reconcile's job.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tickvault_common::constants::MIN_DAILY_UNIVERSE_SIZE;
use tracing::{debug, warn};

use super::daily_universe::{DailyUniverse, InstrumentRole, SubscriptionTarget};
use crate::instrument::csv_parser::CsvRow;

/// Base directory for the snapshot cache. Shares the `data/instrument-cache/`
/// tree with the CSV raw cache + index prev-close cache, all on the host
/// disk — SEPARATE from the QuestDB volume (`tv-questdb-data`). A QuestDB
/// wipe leaves this intact.
pub const CACHE_BASE_DIR: &str = "data/instrument-cache";

/// File name prefix. The full name is `plan-snapshot-YYYY-MM-DD.json`.
const SNAPSHOT_PREFIX: &str = "plan-snapshot-";

/// Snapshot format version (§36 2026-07-08; §36.7 2026-07-10). Format 2 =
/// carries the `index_future` role + the optional segment/expiry/underlying
/// fields. Format 3 (§36.7) = multiple `index_future` targets per underlying
/// are legal (one per monthly expiry); the loader REJECTS
/// `format < PLAN_SNAPSHOT_FORMAT_CURRENT` → one deterministic cold build on
/// deploy day (a stale nearest-only snapshot never warm-boots a single-future
/// day after the all-months code lands — the same doctrine the v2 bump
/// established). The gate is keyed on FORMAT, never on futures COUNT — a
/// legitimately degraded zero-futures day writes a format-3 snapshot that
/// warm boots ACCEPT (no cold-rebuild loop).
pub const PLAN_SNAPSHOT_FORMAT_CURRENT: u32 = 3;

/// One subscription target, reduced to exactly the fields the plan-builder
/// consumes. `role` is the stable wire-format label from
/// [`InstrumentRole::as_str`] so the JSON is self-describing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotTarget {
    /// `"index"`, `"fno_underlying"`, or `"index_constituent"` — see
    /// [`InstrumentRole::as_str`].
    pub role: String,
    /// Dhan SecurityId as a string (matches `CsvRow::security_id`).
    pub security_id: String,
    /// Tradable symbol (matches `CsvRow::symbol_name`).
    pub symbol_name: String,
    /// Lossless membership flag (§31.1(6)). `#[serde(default)]` so a snapshot
    /// written by pre-Sub-PR-#5 code deserializes with `false` — the `role`
    /// still drives subscription, so the warm plan stays correct.
    #[serde(default)]
    pub is_fno_underlying: bool,
    /// Lossless membership flag (§31.1(6)). See `is_fno_underlying`.
    #[serde(default)]
    pub is_index_constituent: bool,
    /// §36 (2026-07-08): CSV segment for `index_future` targets ONLY
    /// (`"NSE_FNO"` / `"BSE_FNO"`) — segment is NOT derivable from that role
    /// (SENSEX is BSE_FNO). `None` (and skipped on write) for the 3 spot
    /// roles, so their entries stay byte-stable across versions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub segment: Option<String>,
    /// §36: `YYYY-MM-DD` expiry for `index_future` targets ONLY.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiry_date: Option<String>,
    /// §36: underlying symbol for `index_future` targets ONLY.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub underlying_symbol: Option<String>,
}

/// The on-disk snapshot. Keyed by the IST trading date so a stale (previous-
/// day) snapshot is never mistaken for today's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSnapshot {
    /// `YYYY-MM-DD` IST trading date this snapshot was built for.
    pub trading_date_ist: String,
    /// Snapshot format version (§36). `#[serde(default)]` = 0 for snapshots
    /// written by pre-§36 code, which the loader rejects → cold build.
    #[serde(default)]
    pub format: u32,
    /// The ~343 subscription targets (indices + F&O underlyings + the §36.7
    /// all-months index futures, ~12 typical).
    pub targets: Vec<SnapshotTarget>,
}

/// Strictly validate a `YYYY-MM-DD` trading-date string.
///
/// This is the **path-traversal guard**: the date string is used to build a
/// file path, so it MUST contain nothing that could escape `CACHE_BASE_DIR`
/// (no `/`, no `.`, no `..`). We accept ONLY the exact shape
/// `dddd-dd-dd` of ASCII digits — anything else returns `false` and the
/// caller fails closed (no read, no write).
#[must_use]
pub fn is_valid_trading_date(date: &str) -> bool {
    let bytes = date.as_bytes();
    if bytes.len() != 10 {
        return false;
    }
    // Positions 4 and 7 must be '-'; all others must be ASCII digits.
    for (i, &b) in bytes.iter().enumerate() {
        let ok = if i == 4 || i == 7 {
            b == b'-'
        } else {
            b.is_ascii_digit()
        };
        if !ok {
            return false;
        }
    }
    true
}

/// Build the validated snapshot path for a trading date.
///
/// Returns `None` (fail-closed) if `date` is not a strict `YYYY-MM-DD`
/// string — the caller then skips the snapshot entirely rather than risk a
/// path-traversal write/read. As defence-in-depth, the assembled path is
/// re-checked to be a direct child of `CACHE_BASE_DIR`.
#[must_use]
pub fn snapshot_path_for(date: &str) -> Option<PathBuf> {
    if !is_valid_trading_date(date) {
        return None;
    }
    let file_name = format!("{SNAPSHOT_PREFIX}{date}.json");
    let path = PathBuf::from(CACHE_BASE_DIR).join(&file_name);
    // Defence-in-depth: the parent MUST be exactly CACHE_BASE_DIR.
    if path.parent() != Some(std::path::Path::new(CACHE_BASE_DIR)) {
        return None;
    }
    Some(path)
}

/// Convert a [`PlanSnapshot`] back into a [`DailyUniverse`] suitable for the
/// subscription-plan builder.
///
/// **Fail-closed on an unknown role string**: if any target carries a role
/// label that is not a known [`InstrumentRole`], the whole conversion
/// returns `None` so the caller falls through to the cold build rather than
/// silently mis-classifying an instrument's segment/feed-mode. `fno_contracts`
/// is left empty — those master-only rows are rebuilt by the background
/// reconcile, never subscribed.
#[must_use]
pub fn to_universe(snapshot: &PlanSnapshot) -> Option<DailyUniverse> {
    use crate::instrument::index_extractor::canonicalize_index_symbol;
    use crate::instrument::index_futures::INDEX_FUTURES_UNDERLYINGS;

    let mut subscription_targets: Vec<SubscriptionTarget> =
        Vec::with_capacity(snapshot.targets.len());
    // §36 hostile-review round 4 (2026-07-08): per-future dedup state —
    // exact-duplicate entries (same composite (security_id, segment)) are
    // collapsed first-entry-wins so a corrupt snapshot can't fire a spurious
    // planner-drop FUTIDX-01. §36.7 (2026-07-10): the fail-closed corruption
    // unit is per-(canonical, expiry_date) — the writer legitimately emits
    // one contract per (underlying, monthly expiry), so distinct expiries
    // per canonical are LEGAL; a SECOND DISTINCT SID for the SAME
    // (canonical, expiry) is corruption (mirrors AmbiguousDuplicateExpiry).
    let mut seen_future_keys: Vec<(String, String)> = Vec::new();
    let mut seen_future_canonical_expiries: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();
    for t in &snapshot.targets {
        let role = parse_role(&t.role)?;
        let mut csv_row = CsvRow {
            security_id: t.security_id.clone(),
            symbol_name: t.symbol_name.clone(),
            ..CsvRow::default()
        };
        if role == InstrumentRole::IndexFuture {
            // §36: an index-future target MUST carry a known FNO segment —
            // fail-closed otherwise (same doctrine as an unknown role): the
            // planner would otherwise mis-subscribe or silently drop it.
            let segment = t.segment.as_deref()?;
            if segment != "NSE_FNO" && segment != "BSE_FNO" {
                return None;
            }
            csv_row.segment = segment.to_string();
            csv_row.exch_id = if segment == "BSE_FNO" { "BSE" } else { "NSE" }.to_string();
            csv_row.instrument = "FUTIDX".to_string();
            // Hostile-review round 3 (2026-07-08): expiry + underlying are
            // fail-closed like segment — a corrupt/hand-edited snapshot with
            // either missing previously produced a SUBSCRIBED future that
            // was INVISIBLE to the parity recorder (empty expiry → the
            // `dhan_selections_from_universe` parse skips it → a false
            // one-sided FUTIDX-02 page). Whole-snapshot `None` → cold
            // rebuild, same doctrine as an unknown role/segment.
            csv_row.expiry_date = t.expiry_date.clone()?;
            csv_row.underlying_symbol = t.underlying_symbol.clone()?;
            if csv_row.expiry_date.trim().is_empty() || csv_row.underlying_symbol.trim().is_empty()
            {
                return None;
            }
            // Hostile-review round 4 (2026-07-08): non-empty is NOT enough —
            // the exact threat the r3 arm claims to close (subscribed but
            // parity-invisible → false one-sided FUTIDX-02) reproduces with a
            // NON-empty UNPARSABLE expiry ("30-07-2026") or a NON-canonical
            // underlying ("NIFT"): the planner subscribes such a target while
            // `dhan_selections_from_universe` silently skips it. So the
            // expiry MUST parse (`%Y-%m-%d`) and the underlying MUST
            // canonicalize to a §36 entry — the SAME two gates the parity
            // derivation applies — else the whole snapshot fails closed.
            if chrono::NaiveDate::parse_from_str(csv_row.expiry_date.trim(), "%Y-%m-%d").is_err() {
                return None;
            }
            let canonical = canonicalize_index_symbol(&csv_row.underlying_symbol);
            if !INDEX_FUTURES_UNDERLYINGS
                .iter()
                .any(|u| u.canonical == canonical)
            {
                return None;
            }
            // Round 4 dedup (see the state comment above): exact composite
            // duplicate → skip (never a spurious planner-drop FUTIDX-01);
            // distinct SID for an already-seen (canonical, expiry) → fail
            // closed (§36.7: distinct expiries per canonical are LEGAL).
            let key = (csv_row.security_id.clone(), csv_row.segment.clone());
            if seen_future_keys.contains(&key) {
                continue;
            }
            let expiry_trimmed = csv_row.expiry_date.trim().to_string();
            if !seen_future_canonical_expiries.insert((canonical, expiry_trimmed)) {
                return None;
            }
            seen_future_keys.push(key);
        }
        // Derive the membership flags from `role` when the snapshot left them
        // at the serde default (`false`) — a snapshot written by pre-Sub-PR-#5
        // code carries no flag fields, so without this an `fno_underlying` row
        // would round-trip to `is_fno_underlying == false`, breaking the role
        // invariant and `fno_underlying_count()`. The OR preserves the explicit
        // both-case (`FnoUnderlying` + `is_index_constituent == true`).
        subscription_targets.push(SubscriptionTarget {
            role,
            is_fno_underlying: t.is_fno_underlying || role == InstrumentRole::FnoUnderlying,
            is_index_constituent: t.is_index_constituent
                || role == InstrumentRole::IndexConstituent,
            csv_row,
        });
    }
    Some(DailyUniverse {
        subscription_targets,
        fno_contracts: Vec::new(),
    })
}

/// Parse a wire-format role label back into [`InstrumentRole`]. Inverse of
/// [`InstrumentRole::as_str`]. Returns `None` for any unknown label.
fn parse_role(label: &str) -> Option<InstrumentRole> {
    match label {
        "index" => Some(InstrumentRole::Index),
        "fno_underlying" => Some(InstrumentRole::FnoUnderlying),
        "index_constituent" => Some(InstrumentRole::IndexConstituent),
        "index_future" => Some(InstrumentRole::IndexFuture),
        _ => None,
    }
}

/// Build a [`PlanSnapshot`] from a live [`DailyUniverse`] for a trading date.
#[must_use]
pub fn snapshot_from_universe(universe: &DailyUniverse, trading_date_ist: &str) -> PlanSnapshot {
    let targets = universe
        .subscription_targets
        .iter()
        .map(|t| {
            let is_future = t.role == InstrumentRole::IndexFuture;
            SnapshotTarget {
                role: t.role.as_str().to_string(),
                security_id: t.csv_row.security_id.clone(),
                symbol_name: t.csv_row.symbol_name.clone(),
                is_fno_underlying: t.is_fno_underlying,
                is_index_constituent: t.is_index_constituent,
                // §36: written ONLY for index_future targets — existing-role
                // entries stay byte-stable.
                segment: is_future.then(|| t.csv_row.segment.clone()),
                expiry_date: is_future.then(|| t.csv_row.expiry_date.clone()),
                underlying_symbol: is_future.then(|| t.csv_row.underlying_symbol.clone()),
            }
        })
        .collect();
    PlanSnapshot {
        trading_date_ist: trading_date_ist.to_string(),
        format: PLAN_SNAPSHOT_FORMAT_CURRENT,
        targets,
    }
}

/// Atomically write today's subscription plan to the date-keyed snapshot
/// file (best-effort — the source of truth is always the cold rebuild).
///
/// Write is tmp-file + rename so a crash mid-write never leaves a truncated
/// snapshot (a partial JSON would just fail to parse on load → cold path).
///
/// # Errors
///
/// Returns an error if the date is invalid, the cache dir can't be created,
/// or the file I/O fails. Callers log this at `warn!` and proceed with the
/// cold path — a failed cache write is degraded-not-broken.
pub fn write_plan_snapshot(
    universe: &DailyUniverse,
    trading_date_ist: &str,
) -> std::io::Result<()> {
    let path = snapshot_path_for(trading_date_ist).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid trading date for snapshot path: {trading_date_ist:?}"),
        )
    })?;
    let snapshot = snapshot_from_universe(universe, trading_date_ist);
    let json = serde_json::to_vec_pretty(&snapshot)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    std::fs::create_dir_all(CACHE_BASE_DIR)?;
    let tmp_path = path.with_extension("json.tmp");
    std::fs::write(&tmp_path, &json)?;
    std::fs::rename(&tmp_path, &path)?;
    Ok(())
}

/// Load today's subscription plan from the snapshot, if a same-date snapshot
/// exists and parses cleanly.
///
/// Returns `None` (→ caller does the cold build) when:
/// - the date is invalid (fail-closed path guard),
/// - the file does not exist (normal on the first boot of the day — logged
///   at `debug!`, not `warn!`),
/// - the JSON is corrupt (truncated by a crash mid-write, logged `warn!`),
/// - the file's `trading_date_ist` does not match the requested date (a
///   stale previous-day snapshot — never trusted), or
/// - a target carries an unknown role label (fail-closed via [`to_universe`]).
#[must_use]
pub fn load_plan_snapshot_for_today(trading_date_ist: &str) -> Option<DailyUniverse> {
    let path = snapshot_path_for(trading_date_ist)?;
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            debug!(
                date = trading_date_ist,
                "no plan snapshot for today — cold build"
            );
            return None;
        }
        Err(e) => {
            warn!(error = %e, date = trading_date_ist, "plan snapshot read failed — cold build");
            return None;
        }
    };
    let snapshot: PlanSnapshot = match serde_json::from_slice(&bytes) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, date = trading_date_ist, "plan snapshot parse failed — cold build");
            return None;
        }
    };
    if snapshot.trading_date_ist != trading_date_ist {
        warn!(
            file_date = snapshot.trading_date_ist,
            requested = trading_date_ist,
            "plan snapshot date mismatch — cold build"
        );
        return None;
    }
    // §36 (2026-07-08) format gate: a snapshot written by pre-§36 code
    // (format 0) parses fine but silently LACKS the index-future targets —
    // rejecting it forces exactly ONE deterministic cold build on deploy
    // day, after which a format-2 snapshot is written. Keyed on FORMAT,
    // never on futures count (a degraded zero-futures format-2 day is
    // accepted — no rebuild loop).
    if snapshot.format < PLAN_SNAPSHOT_FORMAT_CURRENT {
        warn!(
            format = snapshot.format,
            required = PLAN_SNAPSHOT_FORMAT_CURRENT,
            date = trading_date_ist,
            "plan snapshot format too old (pre-§36) — one-time cold build"
        );
        return None;
    }
    let universe = to_universe(&snapshot)?;
    // Zero-tick-loss guard (2026-06-02): a same-day snapshot that is empty or
    // implausibly small (corruption, truncated/partial write, manual edit) must
    // NOT be warm-resubscribed — that would subscribe to too few / zero SIDs and
    // SILENTLY lose the day's ticks with no alarm. Reject it → return None so the
    // caller falls through to the cold build, which re-fetches the Dhan CSV and
    // enforces the [MIN_DAILY_UNIVERSE_SIZE, MAX_DAILY_UNIVERSE_SIZE] envelope
    // (fail-closed boot halt on an out-of-range size). The cold path is the only
    // sanctioned way to accept a universe.
    if universe.subscription_targets.len() < MIN_DAILY_UNIVERSE_SIZE {
        warn!(
            date = trading_date_ist,
            targets = universe.subscription_targets.len(),
            min = MIN_DAILY_UNIVERSE_SIZE,
            "plan snapshot has too few targets (empty/corrupt?) — rejecting warm \
             path; cold build will rebuild + envelope-check (zero-tick-loss guard)"
        );
        return None;
    }
    debug!(
        date = trading_date_ist,
        targets = universe.subscription_targets.len(),
        "loaded plan snapshot — instant warm resubscribe"
    );
    Some(universe)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(role: InstrumentRole, sid: &str, sym: &str) -> SubscriptionTarget {
        // Flags follow the role invariant (the both-case is covered explicitly
        // by `test_snapshot_flags_round_trip`).
        SubscriptionTarget {
            role,
            is_fno_underlying: role == InstrumentRole::FnoUnderlying,
            is_index_constituent: role == InstrumentRole::IndexConstituent,
            csv_row: CsvRow {
                security_id: sid.to_string(),
                symbol_name: sym.to_string(),
                ..CsvRow::default()
            },
        }
    }

    fn target_future(
        sid: &str,
        underlying: &str,
        segment: &str,
        expiry: &str,
    ) -> SubscriptionTarget {
        SubscriptionTarget {
            role: InstrumentRole::IndexFuture,
            is_fno_underlying: false,
            is_index_constituent: false,
            csv_row: CsvRow {
                security_id: sid.to_string(),
                exch_id: if segment == "BSE_FNO" { "BSE" } else { "NSE" }.to_string(),
                segment: segment.to_string(),
                instrument: "FUTIDX".to_string(),
                symbol_name: format!("{underlying}-Jul2026-FUT"),
                underlying_symbol: underlying.to_string(),
                expiry_date: expiry.to_string(),
                ..CsvRow::default()
            },
        }
    }

    fn sample_universe() -> DailyUniverse {
        DailyUniverse {
            subscription_targets: vec![
                target(InstrumentRole::Index, "13", "NIFTY"),
                target(InstrumentRole::FnoUnderlying, "2885", "RELIANCE"),
            ],
            // fno_contracts deliberately non-empty to prove the snapshot
            // drops them (master-only, never subscribed).
            fno_contracts: vec![CsvRow {
                security_id: "99999".to_string(),
                ..CsvRow::default()
            }],
        }
    }

    /// A universe with >= MIN_DAILY_UNIVERSE_SIZE targets — passes the
    /// zero-tick-loss size guard in `load_plan_snapshot_for_today` (the warm
    /// path rejects anything smaller as empty/corrupt).
    fn large_sample_universe() -> DailyUniverse {
        let mut subscription_targets = Vec::with_capacity(MIN_DAILY_UNIVERSE_SIZE + 20);
        subscription_targets.push(target(InstrumentRole::Index, "13", "NIFTY"));
        for i in 0..(MIN_DAILY_UNIVERSE_SIZE + 19) {
            subscription_targets.push(target(
                InstrumentRole::FnoUnderlying,
                &format!("{}", 10_000 + i),
                "SYM",
            ));
        }
        DailyUniverse {
            subscription_targets,
            fno_contracts: Vec::new(),
        }
    }

    #[test]
    fn test_is_valid_trading_date_accepts_well_formed() {
        assert!(is_valid_trading_date("2026-05-29"));
        assert!(is_valid_trading_date("1999-12-31"));
    }

    #[test]
    fn test_is_valid_trading_date_rejects_traversal_and_malformed() {
        assert!(!is_valid_trading_date("../../etc/pass"));
        assert!(!is_valid_trading_date("2026/05/29"));
        assert!(!is_valid_trading_date("2026-05-29/x"));
        assert!(!is_valid_trading_date("2026-5-9"));
        assert!(!is_valid_trading_date(""));
        assert!(!is_valid_trading_date("2026-05-29 "));
        assert!(!is_valid_trading_date("20260529ab"));
    }

    #[test]
    fn test_snapshot_path_for_valid_date_is_child_of_cache_dir() {
        let p = snapshot_path_for("2026-05-29").expect("valid date");
        assert_eq!(
            p,
            PathBuf::from("data/instrument-cache/plan-snapshot-2026-05-29.json")
        );
    }

    #[test]
    fn test_snapshot_path_for_invalid_date_is_none() {
        assert!(snapshot_path_for("../evil").is_none());
        assert!(snapshot_path_for("2026/05/29").is_none());
    }

    #[test]
    fn test_snapshot_from_universe_drops_fno_contracts_keeps_min_fields() {
        let snap = snapshot_from_universe(&sample_universe(), "2026-05-29");
        assert_eq!(snap.trading_date_ist, "2026-05-29");
        assert_eq!(snap.targets.len(), 2);
        assert_eq!(snap.targets[0].role, "index");
        assert_eq!(snap.targets[0].security_id, "13");
        assert_eq!(snap.targets[0].symbol_name, "NIFTY");
        assert_eq!(snap.targets[1].role, "fno_underlying");
    }

    #[test]
    fn test_to_universe_roundtrip_preserves_targets() {
        let snap = snapshot_from_universe(&sample_universe(), "2026-05-29");
        let uni = to_universe(&snap).expect("known roles round-trip");
        assert_eq!(uni.subscription_targets.len(), 2);
        assert!(uni.fno_contracts.is_empty(), "master-only rows dropped");
        assert_eq!(uni.subscription_targets[0].role, InstrumentRole::Index);
        assert_eq!(uni.subscription_targets[0].csv_row.security_id, "13");
        assert_eq!(
            uni.subscription_targets[1].role,
            InstrumentRole::FnoUnderlying
        );
    }

    #[test]
    fn test_to_universe_fails_closed_on_unknown_role() {
        let snap = PlanSnapshot {
            trading_date_ist: "2026-05-29".to_string(),
            format: PLAN_SNAPSHOT_FORMAT_CURRENT,
            targets: vec![SnapshotTarget {
                role: "depth_underlying".to_string(), // not a known role
                security_id: "13".to_string(),
                symbol_name: "NIFTY".to_string(),
                is_fno_underlying: false,
                is_index_constituent: false,
                segment: None,
                expiry_date: None,
                underlying_symbol: None,
            }],
        };
        assert!(
            to_universe(&snap).is_none(),
            "unknown role must fail closed, not silently mis-classify"
        );
    }

    #[test]
    fn test_parse_role_inverse_of_as_str() {
        assert_eq!(parse_role("index"), Some(InstrumentRole::Index));
        assert_eq!(
            parse_role("fno_underlying"),
            Some(InstrumentRole::FnoUnderlying)
        );
        assert_eq!(
            parse_role("index_constituent"),
            Some(InstrumentRole::IndexConstituent)
        );
        assert_eq!(parse_role("garbage"), None);
    }

    #[test]
    fn test_snapshot_flags_round_trip() {
        // Sub-PR #5: the lossless membership flags survive
        // universe → snapshot → JSON → snapshot → universe, incl. the both-case.
        let universe = DailyUniverse {
            subscription_targets: vec![
                target(InstrumentRole::Index, "13", "NIFTY"),
                // The both-case: F&O underlying that is ALSO an NTM constituent.
                SubscriptionTarget {
                    role: InstrumentRole::FnoUnderlying,
                    is_fno_underlying: true,
                    is_index_constituent: true,
                    csv_row: CsvRow {
                        security_id: "2885".to_string(),
                        symbol_name: "RELIANCE".to_string(),
                        ..CsvRow::default()
                    },
                },
                target(InstrumentRole::IndexConstituent, "1594", "INFY"),
            ],
            fno_contracts: Vec::new(),
        };
        let snap = snapshot_from_universe(&universe, "2026-06-06");
        let json = serde_json::to_string(&snap).expect("serialize");
        let back: PlanSnapshot = serde_json::from_str(&json).expect("deserialize");
        let rebuilt = to_universe(&back).expect("known roles round-trip");
        assert_eq!(rebuilt.fno_underlying_count(), 1, "the both-case stock");
        assert_eq!(
            rebuilt.index_constituent_count(),
            2,
            "both-case + pure constituent"
        );
        let both = &rebuilt.subscription_targets[1];
        assert!(both.is_fno_underlying && both.is_index_constituent);
    }

    #[test]
    fn test_snapshot_old_format_defaults_flags_false() {
        // A snapshot written by pre-Sub-PR-#5 code has no flag fields. serde
        // default ⇒ false; the role still drives subscription.
        let legacy = r#"{
            "trading_date_ist": "2026-06-06",
            "targets": [
                { "role": "fno_underlying", "security_id": "2885", "symbol_name": "RELIANCE" }
            ]
        }"#;
        let snap: PlanSnapshot = serde_json::from_str(legacy).expect("legacy parses");
        // At the serde layer the missing fields default to false …
        assert!(!snap.targets[0].is_fno_underlying, "missing field ⇒ false");
        assert!(!snap.targets[0].is_index_constituent);
        // … but `to_universe` DERIVES the flag from `role`, so the invariant
        // `FnoUnderlying ⇒ is_fno_underlying` holds even for a legacy snapshot
        // and `fno_underlying_count()` stays correct (hostile-review LOW fix).
        let uni = to_universe(&snap).expect("legacy round-trips");
        assert_eq!(
            uni.subscription_targets[0].role,
            InstrumentRole::FnoUnderlying,
            "role still drives subscription for a legacy snapshot"
        );
        assert!(
            uni.subscription_targets[0].is_fno_underlying,
            "flag derived from role on legacy snapshot"
        );
        assert_eq!(uni.fno_underlying_count(), 1, "count correct post-derive");
    }

    #[test]
    fn test_write_plan_snapshot_then_load_plan_snapshot_for_today_roundtrips() {
        // Use a unique date so the test file never collides with a real one
        // and never trusts a stale file. Write → load → assert equality.
        let date = "2099-01-02";
        let path = snapshot_path_for(date).expect("valid date");
        // Clean any leftover from a prior run.
        let _ = std::fs::remove_file(&path);

        // Use a >= MIN_DAILY_UNIVERSE_SIZE universe so the zero-tick-loss size
        // guard accepts the warm load (a 2-SID snapshot is now rejected — see
        // test_load_rejects_too_small_snapshot_zero_tick_loss_guard).
        write_plan_snapshot(&large_sample_universe(), date).expect("write ok");
        let loaded = load_plan_snapshot_for_today(date).expect("load ok");
        assert!(loaded.subscription_targets.len() >= MIN_DAILY_UNIVERSE_SIZE);
        assert_eq!(loaded.subscription_targets[0].csv_row.security_id, "13");

        // Cleanup.
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_load_rejects_too_small_snapshot_zero_tick_loss_guard() {
        // Regression 2026-06-02: a same-day snapshot with too few SIDs
        // (empty/corrupt/truncated) MUST be rejected → cold build rebuilds +
        // envelope-checks. Prevents silent warm-resubscribe to ~0 SIDs, which
        // would lose the entire day's ticks with no alarm.
        let date = "2099-05-06";
        let path = snapshot_path_for(date).expect("valid date");
        let _ = std::fs::remove_file(&path);
        // sample_universe() has only 2 targets — below MIN_DAILY_UNIVERSE_SIZE.
        write_plan_snapshot(&sample_universe(), date).expect("write ok");
        assert!(
            load_plan_snapshot_for_today(date).is_none(),
            "snapshot with < MIN_DAILY_UNIVERSE_SIZE targets must be rejected (cold build)"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_load_plan_snapshot_for_today_missing_is_none() {
        // A date with no file on disk → None (cold path), no panic.
        assert!(load_plan_snapshot_for_today("2099-12-31").is_none());
    }

    #[test]
    fn test_load_date_mismatch_is_none() {
        // Write under one date, then hand-edit the file's internal date to
        // simulate a stale previous-day snapshot, and assert it's rejected.
        let date = "2099-03-04";
        let path = snapshot_path_for(date).expect("valid date");
        let _ = std::fs::remove_file(&path);
        std::fs::create_dir_all(CACHE_BASE_DIR).expect("mkdir");
        let stale = PlanSnapshot {
            trading_date_ist: "2099-03-03".to_string(), // different from file name
            format: PLAN_SNAPSHOT_FORMAT_CURRENT,
            targets: vec![],
        };
        std::fs::write(&path, serde_json::to_vec(&stale).unwrap()).expect("write");
        assert!(
            load_plan_snapshot_for_today(date).is_none(),
            "internal date mismatch must be rejected"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_load_corrupt_json_is_none() {
        let date = "2099-05-06";
        let path = snapshot_path_for(date).expect("valid date");
        std::fs::create_dir_all(CACHE_BASE_DIR).expect("mkdir");
        std::fs::write(&path, b"{ this is not valid json").expect("write");
        assert!(
            load_plan_snapshot_for_today(date).is_none(),
            "corrupt JSON must fall through to cold path"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The correctness ratchet behind PR #884: a subscription plan built from
    /// a snapshot-restored universe must be IDENTICAL to the plan built from
    /// the original universe. If this ever drifts, warm-resubscribe would
    /// silently subscribe to a different set than the cold build — the worst
    /// possible failure mode (looks healthy, wrong data). We assert both the
    /// summary counts AND the exact (security_id, segment) set in the registry.
    #[test]
    fn test_plan_is_identical_across_snapshot_boundary() {
        use crate::instrument::subscription_planner::build_subscription_plan_from_daily_universe;
        use tickvault_common::types::ExchangeSegment;

        // Realistic mixed universe — indices (IDX_I) + F&O underlyings (NSE_EQ).
        let mut targets_vec = vec![
            target(InstrumentRole::Index, "13", "NIFTY"),
            target(InstrumentRole::Index, "25", "BANKNIFTY"),
            target(InstrumentRole::Index, "51", "SENSEX"),
            target(InstrumentRole::FnoUnderlying, "2885", "RELIANCE"),
            target(InstrumentRole::FnoUnderlying, "1333", "HDFCBANK"),
            target(InstrumentRole::FnoUnderlying, "11536", "TCS"),
            // §36: the 4 nearest-expiry index futures ride the same snapshot.
            target_future("35001", "NIFTY", "NSE_FNO", "2026-07-30"),
            target_future("35002", "BANKNIFTY", "NSE_FNO", "2026-07-30"),
            target_future("35003", "MIDCPNIFTY", "NSE_FNO", "2026-07-28"),
            target_future("45001", "SENSEX", "BSE_FNO", "2026-07-31"),
        ];
        // Pad to >= MIN_DAILY_UNIVERSE_SIZE so the snapshot passes the
        // zero-tick-loss size guard (2026-06-02). Plan-A and Plan-B both build
        // from this same padded universe, so the plan-identity assertions below
        // are unaffected; the padding SIDs (20000+) never collide with the named
        // ones and are all FnoUnderlying (index count stays 3).
        for i in 0..MIN_DAILY_UNIVERSE_SIZE {
            targets_vec.push(target(
                InstrumentRole::FnoUnderlying,
                &format!("{}", 20_000 + i),
                "PAD",
            ));
        }
        let original = DailyUniverse {
            subscription_targets: targets_vec,
            fno_contracts: vec![],
        };

        let date = "2099-07-08";
        let path = snapshot_path_for(date).expect("valid date");
        let _ = std::fs::remove_file(&path);

        // Plan A: directly from the original universe.
        let plan_a = build_subscription_plan_from_daily_universe(&original);

        // Round-trip through the on-disk snapshot, then Plan B.
        write_plan_snapshot(&original, date).expect("write ok");
        let restored = load_plan_snapshot_for_today(date).expect("load ok");
        let plan_b = build_subscription_plan_from_daily_universe(&restored);

        // Summary counts must match exactly.
        assert_eq!(plan_a.summary.total, plan_b.summary.total, "total drift");
        assert_eq!(
            plan_a.summary.major_index_values, plan_b.summary.major_index_values,
            "index-count drift"
        );
        assert_eq!(
            plan_a.summary.stock_equities, plan_b.summary.stock_equities,
            "stock-count drift"
        );
        assert_eq!(
            plan_a.summary.index_derivatives, plan_b.summary.index_derivatives,
            "index-future-count drift (§36 boot symmetry)"
        );
        assert_eq!(plan_a.summary.index_derivatives, 4, "all 4 futures planned");
        assert_eq!(
            plan_a.summary.feed_mode, plan_b.summary.feed_mode,
            "feed-mode drift"
        );

        // The exact (security_id, segment) set must match — this is the
        // I-P1-11 composite identity that the WS dispatcher subscribes.
        let mut set_a: Vec<(u64, ExchangeSegment)> = plan_a
            .registry
            .iter()
            .map(|i| (i.security_id, i.exchange_segment))
            .collect();
        let mut set_b: Vec<(u64, ExchangeSegment)> = plan_b
            .registry
            .iter()
            .map(|i| (i.security_id, i.exchange_segment))
            .collect();
        set_a.sort_by_key(|(id, _)| *id);
        set_b.sort_by_key(|(id, _)| *id);
        assert_eq!(
            set_a, set_b,
            "subscription set differs across the snapshot boundary — warm resubscribe would diverge from cold build"
        );
        assert_eq!(
            set_a.len(),
            original.subscription_targets.len(),
            "all instruments must survive the round-trip"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// Production-scale disaster proof: a realistic ~331-SID universe (the live
    /// daily-universe size) must round-trip through the snapshot losslessly —
    /// no SID silently dropped, the plan identical to the cold build. A drop
    /// at scale is the catastrophic "warm boot subscribes to fewer instruments
    /// than the cold build, looks healthy" failure. Runs in CI as a --lib test.
    #[test]
    fn test_warm_resubscribe_lossless_at_production_scale() {
        use crate::instrument::subscription_planner::build_subscription_plan_from_daily_universe;

        const INDEX_COUNT: usize = 3;
        const UNDERLYING_COUNT: usize = 328; // 3 + 328 = 331, the live size
        const FUTURE_COUNT: usize = 12; // §36.7: 3 months × 4 underlyings
        let mut targets = Vec::with_capacity(INDEX_COUNT + UNDERLYING_COUNT + FUTURE_COUNT);
        targets.push(target(InstrumentRole::Index, "13", "NIFTY"));
        targets.push(target(InstrumentRole::Index, "25", "BANKNIFTY"));
        targets.push(target(InstrumentRole::Index, "51", "SENSEX"));
        for (month_idx, (nse_expiry, bse_expiry)) in [
            ("2026-07-30", "2026-07-31"),
            ("2026-08-27", "2026-08-28"),
            ("2026-09-24", "2026-09-25"),
        ]
        .into_iter()
        .enumerate()
        {
            let base = 35_000 + month_idx * 1_000;
            targets.push(target_future(
                &format!("{base}"),
                "NIFTY",
                "NSE_FNO",
                nse_expiry,
            ));
            targets.push(target_future(
                &format!("{}", base + 1),
                "BANKNIFTY",
                "NSE_FNO",
                nse_expiry,
            ));
            targets.push(target_future(
                &format!("{}", base + 2),
                "MIDCPNIFTY",
                "NSE_FNO",
                nse_expiry,
            ));
            targets.push(target_future(
                &format!("{}", 45_000 + month_idx * 1_000),
                "SENSEX",
                "BSE_FNO",
                bse_expiry,
            ));
        }
        for i in 0..UNDERLYING_COUNT {
            // 10000+ keeps ids well clear of the 3 index ids above; all unique.
            let sid = (10_000 + i).to_string();
            let sym = format!("STK{i}");
            targets.push(target(InstrumentRole::FnoUnderlying, &sid, &sym));
        }
        let original = DailyUniverse {
            subscription_targets: targets,
            fno_contracts: vec![],
        };
        let expected = INDEX_COUNT + UNDERLYING_COUNT + FUTURE_COUNT;

        let date = "2099-08-09";
        let path = snapshot_path_for(date).expect("valid date");
        let _ = std::fs::remove_file(&path);

        let plan_a = build_subscription_plan_from_daily_universe(&original);
        write_plan_snapshot(&original, date).expect("write ok");
        let restored = load_plan_snapshot_for_today(date).expect("load ok");
        let plan_b = build_subscription_plan_from_daily_universe(&restored);

        assert_eq!(
            plan_a.summary.total, expected,
            "cold build must plan all {expected} SIDs"
        );
        assert_eq!(
            plan_b.summary.total, expected,
            "warm resubscribe dropped SIDs at production scale"
        );
        assert_eq!(
            restored.subscription_targets.len(),
            expected,
            "snapshot lost targets at scale"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// Crash-during-write disaster: the write is tmp-file + atomic rename, so a
    /// process killed mid-write leaves only a `.tmp` and NO `.json`. The loader
    /// must then return `None` (→ cold path) and must NEVER read the partial
    /// `.tmp`. This proves an OOM/SIGKILL at the exact write instant degrades
    /// to a clean cold build, never a corrupt warm plan.
    #[test]
    fn test_load_ignores_partial_tmp_write() {
        let date = "2099-09-10";
        let path = snapshot_path_for(date).expect("valid date");
        let tmp_path = path.with_extension("json.tmp");
        std::fs::create_dir_all(CACHE_BASE_DIR).expect("mkdir");
        // Simulate a crash mid-write: a half-written .tmp exists, no .json.
        let _ = std::fs::remove_file(&path);
        std::fs::write(&tmp_path, b"{ \"trading_date_ist\": \"2099-09-10\", \"targ")
            .expect("write partial tmp");

        assert!(
            load_plan_snapshot_for_today(date).is_none(),
            "loader must ignore the partial .tmp and fall through to cold build"
        );

        let _ = std::fs::remove_file(&tmp_path);
    }

    // ----- §36 (2026-07-08): format-v2 + index_future role -----

    #[test]
    fn test_parse_role_round_trips_index_future() {
        assert_eq!(
            parse_role(InstrumentRole::IndexFuture.as_str()),
            Some(InstrumentRole::IndexFuture),
            "as_str/parse_role inverse for the new label"
        );
    }

    #[test]
    fn test_snapshot_roundtrip_preserves_index_future_segment_and_expiry() {
        let universe = DailyUniverse {
            subscription_targets: vec![
                target(InstrumentRole::Index, "13", "NIFTY"),
                target_future("45001", "SENSEX", "BSE_FNO", "2026-07-31"),
            ],
            fno_contracts: Vec::new(),
        };
        let snap = snapshot_from_universe(&universe, "2026-07-08");
        assert_eq!(snap.format, PLAN_SNAPSHOT_FORMAT_CURRENT);
        // Spot entries stay byte-stable: no §36 fields serialized.
        let json = serde_json::to_string(&snap).expect("serialize");
        let back: PlanSnapshot = serde_json::from_str(&json).expect("deserialize");
        let rebuilt = to_universe(&back).expect("round-trip");
        let fut = &rebuilt.subscription_targets[1];
        assert_eq!(fut.role, InstrumentRole::IndexFuture);
        assert_eq!(fut.csv_row.segment, "BSE_FNO");
        assert_eq!(fut.csv_row.exch_id, "BSE");
        assert_eq!(fut.csv_row.instrument, "FUTIDX");
        assert_eq!(fut.csv_row.expiry_date, "2026-07-31");
        assert_eq!(fut.csv_row.underlying_symbol, "SENSEX");
        // The spot entry carries NO §36 fields on the wire.
        assert!(!json.contains("\"segment\":null"));
    }

    /// Hostile-review round 3 (2026-07-08): expiry + underlying are
    /// fail-closed like segment — a corrupt snapshot future with either
    /// missing/empty previously subscribed INVISIBLY to the parity recorder
    /// (empty expiry never parses → the Dhan parity entry omits a live
    /// underlying → false one-sided FUTIDX-02).
    #[test]
    fn test_snapshot_index_future_missing_expiry_or_underlying_fails_closed() {
        let base = SnapshotTarget {
            role: "index_future".to_string(),
            security_id: "35001".to_string(),
            symbol_name: "NIFTY-Jul2026-FUT".to_string(),
            is_fno_underlying: false,
            is_index_constituent: false,
            segment: Some("NSE_FNO".to_string()),
            expiry_date: Some("2026-07-30".to_string()),
            underlying_symbol: Some("NIFTY".to_string()),
        };
        let snap_for = |t: SnapshotTarget| PlanSnapshot {
            trading_date_ist: "2026-07-08".to_string(),
            format: PLAN_SNAPSHOT_FORMAT_CURRENT,
            targets: vec![t],
        };
        // Control: the intact entry converts.
        assert!(to_universe(&snap_for(base.clone())).is_some());
        // Missing expiry → whole snapshot fails closed.
        let mut t = base.clone();
        t.expiry_date = None;
        assert!(to_universe(&snap_for(t)).is_none());
        // Empty-string expiry → fails closed.
        let mut t = base.clone();
        t.expiry_date = Some(String::new());
        assert!(to_universe(&snap_for(t)).is_none());
        // Missing underlying → fails closed.
        let mut t = base.clone();
        t.underlying_symbol = None;
        assert!(to_universe(&snap_for(t)).is_none());
        // Whitespace-only underlying → fails closed.
        let mut t = base;
        t.underlying_symbol = Some("  ".to_string());
        assert!(to_universe(&snap_for(t)).is_none());
    }

    /// Hostile-review round 4 (2026-07-08): non-empty is NOT enough — a
    /// NON-empty UNPARSABLE expiry ("30-07-2026") or a NON-canonical
    /// underlying ("NIFT") previously passed the r3 arm, got SUBSCRIBED by
    /// the planner, but was SKIPPED by `dhan_selections_from_universe` —
    /// reproducing the exact false one-sided FUTIDX-02 the r3 fix claims to
    /// close. The expiry must PARSE (%Y-%m-%d) and the underlying must
    /// CANONICALIZE to a §36 entry, else the whole snapshot fails closed.
    #[test]
    fn test_snapshot_index_future_unparsable_expiry_or_noncanonical_underlying_fails_closed() {
        let base = SnapshotTarget {
            role: "index_future".to_string(),
            security_id: "35001".to_string(),
            symbol_name: "NIFTY-Jul2026-FUT".to_string(),
            is_fno_underlying: false,
            is_index_constituent: false,
            segment: Some("NSE_FNO".to_string()),
            expiry_date: Some("2026-07-30".to_string()),
            underlying_symbol: Some("NIFTY".to_string()),
        };
        let snap_for = |t: SnapshotTarget| PlanSnapshot {
            trading_date_ist: "2026-07-08".to_string(),
            format: PLAN_SNAPSHOT_FORMAT_CURRENT,
            targets: vec![t],
        };
        // Control: the intact entry converts.
        assert!(to_universe(&snap_for(base.clone())).is_some());
        // Non-empty but UNPARSABLE expiry (DD-MM-YYYY) → fails closed.
        let mut t = base.clone();
        t.expiry_date = Some("30-07-2026".to_string());
        assert!(
            to_universe(&snap_for(t)).is_none(),
            "unparsable expiry must fail the whole snapshot closed"
        );
        // Garbage expiry → fails closed.
        let mut t = base.clone();
        t.expiry_date = Some("garbage".to_string());
        assert!(to_universe(&snap_for(t)).is_none());
        // Non-canonical underlying ("NIFT") → fails closed.
        let mut t = base.clone();
        t.underlying_symbol = Some("NIFT".to_string());
        assert!(
            to_universe(&snap_for(t)).is_none(),
            "non-canonical underlying must fail the whole snapshot closed"
        );
        // A 5th (non-§36) real index → fails closed too.
        let mut t = base.clone();
        t.underlying_symbol = Some("FINNIFTY".to_string());
        assert!(to_universe(&snap_for(t)).is_none());
        // An ALIAS that canonicalizes to a §36 entry is accepted.
        let mut t = base;
        t.underlying_symbol = Some("NIFTY MIDCAP SELECT".to_string());
        assert!(
            to_universe(&snap_for(t)).is_some(),
            "alias canonicalizing to MIDCPNIFTY is a valid §36 underlying"
        );
    }

    /// Snapshot-fixture future entry builder for the §36.7 dedup tests.
    fn fut_target(sid: &str, expiry: &str) -> SnapshotTarget {
        SnapshotTarget {
            role: "index_future".to_string(),
            security_id: sid.to_string(),
            symbol_name: format!("NIFTY-{expiry}-FUT"),
            is_fno_underlying: false,
            is_index_constituent: false,
            segment: Some("NSE_FNO".to_string()),
            expiry_date: Some(expiry.to_string()),
            underlying_symbol: Some("NIFTY".to_string()),
        }
    }

    /// Hostile-review round 4 (2026-07-08), re-keyed §36.7 (2026-07-10):
    /// duplicate IndexFuture entries in a corrupt snapshot. EXACT duplicates
    /// (same SID + segment) are DEDUPED first-entry-wins — previously they
    /// reached the planner, whose composite-key dedup collapsed them and
    /// fired a spurious planner-drop FUTIDX-01. A DISTINCT SID for an
    /// already-seen (canonical, expiry) is fail-closed (the writer only
    /// ever emits one contract per (underlying, month) — two is corruption).
    #[test]
    fn test_snapshot_index_future_same_expiry_distinct_sids_fail_closed() {
        // EXACT duplicate (same SID) → deduped to ONE target, snapshot OK.
        let snap = PlanSnapshot {
            trading_date_ist: "2026-07-08".to_string(),
            format: PLAN_SNAPSHOT_FORMAT_CURRENT,
            targets: vec![
                fut_target("35001", "2026-07-30"),
                fut_target("35001", "2026-07-30"),
            ],
        };
        let uni = to_universe(&snap).expect("exact duplicate is deduped, not fatal");
        assert_eq!(
            uni.subscription_targets.len(),
            1,
            "exact-duplicate future entries collapse first-entry-wins"
        );
        assert_eq!(uni.subscription_targets[0].csv_row.security_id, "35001");
        // DISTINCT SID for the SAME (canonical, expiry) → fail closed.
        let snap2 = PlanSnapshot {
            trading_date_ist: "2026-07-08".to_string(),
            format: PLAN_SNAPSHOT_FORMAT_CURRENT,
            targets: vec![
                fut_target("35001", "2026-07-30"),
                fut_target("35099", "2026-07-30"),
            ],
        };
        assert!(
            to_universe(&snap2).is_none(),
            "two DISTINCT SIDs for one (underlying, expiry) is corruption — fail closed"
        );
    }

    /// §36.7 (2026-07-10): distinct expiries per canonical are LEGAL — the
    /// direct inversion of the pre-all-months per-canonical fail-closed arm
    /// (which would have rejected EVERY valid multi-month snapshot →
    /// permanent cold-build, warm path dead).
    #[test]
    fn test_snapshot_index_future_distinct_expiries_per_underlying_accepted() {
        let snap = PlanSnapshot {
            trading_date_ist: "2026-07-10".to_string(),
            format: PLAN_SNAPSHOT_FORMAT_CURRENT,
            targets: vec![
                fut_target("35001", "2026-07-30"),
                fut_target("36001", "2026-08-27"),
            ],
        };
        let uni = to_universe(&snap).expect("two months of one underlying are legal (§36.7)");
        assert_eq!(uni.subscription_targets.len(), 2);
        let mut expiries: Vec<&str> = uni
            .subscription_targets
            .iter()
            .map(|t| t.csv_row.expiry_date.as_str())
            .collect();
        expiries.sort_unstable();
        assert_eq!(expiries, vec!["2026-07-30", "2026-08-27"]);
    }

    /// §36.7 (2026-07-10): a format-2 (nearest-only era) snapshot is
    /// REJECTED at the load gate → one deterministic cold build on deploy
    /// day — a stale single-future snapshot never warm-boots an all-months
    /// day. Mirrors the v1→v2 format-gate test above.
    #[test]
    fn test_snapshot_format_2_rejected_after_v3_bump() {
        let date = "2099-07-13";
        let path = snapshot_path_for(date).expect("valid date");
        std::fs::create_dir_all(CACHE_BASE_DIR).expect("mkdir");
        let mut legacy = snapshot_from_universe(&large_sample_universe(), date);
        legacy.format = 2; // simulate the §36 nearest-only writer
        std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).expect("write");
        assert!(
            load_plan_snapshot_for_today(date).is_none(),
            "format 2 < PLAN_SNAPSHOT_FORMAT_CURRENT must be rejected (deploy-day cold build)"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_snapshot_index_future_missing_segment_fails_closed() {
        let snap = PlanSnapshot {
            trading_date_ist: "2026-07-08".to_string(),
            format: PLAN_SNAPSHOT_FORMAT_CURRENT,
            targets: vec![SnapshotTarget {
                role: "index_future".to_string(),
                security_id: "35001".to_string(),
                symbol_name: "NIFTY-Jul2026-FUT".to_string(),
                is_fno_underlying: false,
                is_index_constituent: false,
                segment: None, // hand-edited / corrupt entry
                expiry_date: Some("2026-07-30".to_string()),
                underlying_symbol: Some("NIFTY".to_string()),
            }],
        };
        assert!(
            to_universe(&snap).is_none(),
            "index_future without a segment must fail closed → cold build"
        );
        // Unknown segment value also fails closed.
        let mut snap2 = snap;
        snap2.targets[0].segment = Some("MCX_COMM".to_string());
        assert!(to_universe(&snap2).is_none());
    }

    #[test]
    fn test_snapshot_format_v1_rejected_forces_cold_build_once() {
        // A pre-§36 snapshot (no `format` field ⇒ serde default 0) parses
        // fine but is REJECTED by the loader — one deterministic cold build.
        let date = "2099-07-11";
        let path = snapshot_path_for(date).expect("valid date");
        std::fs::create_dir_all(CACHE_BASE_DIR).expect("mkdir");
        let mut legacy = snapshot_from_universe(&large_sample_universe(), date);
        legacy.format = 0; // simulate the old writer
        std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).expect("write");
        assert!(
            load_plan_snapshot_for_today(date).is_none(),
            "format < PLAN_SNAPSHOT_FORMAT_CURRENT must be rejected (deploy-day cold build)"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_snapshot_format_v2_zero_futures_accepted_no_rebuild_loop() {
        // The gate is keyed on FORMAT, never futures COUNT: a legitimately
        // degraded zero-futures day writes a format-2 snapshot that warm
        // boots ACCEPT — no cold-rebuild loop.
        let date = "2099-07-12";
        let path = snapshot_path_for(date).expect("valid date");
        let _ = std::fs::remove_file(&path);
        // large_sample_universe() has ZERO index futures.
        write_plan_snapshot(&large_sample_universe(), date).expect("write ok");
        let loaded = load_plan_snapshot_for_today(date);
        assert!(loaded.is_some(), "format-2 zero-futures snapshot accepted");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_old_snapshot_without_new_fields_parses_with_empty_defaults() {
        // serde-default tolerance: an entry without the 3 §36 fields (any of
        // the 3 old roles) deserializes with None — parse never fails.
        let legacy = r#"{
            "trading_date_ist": "2026-07-08",
            "targets": [
                { "role": "index", "security_id": "13", "symbol_name": "NIFTY" }
            ]
        }"#;
        let snap: PlanSnapshot = serde_json::from_str(legacy).expect("legacy parses");
        assert_eq!(snap.format, 0, "missing format field defaults to 0");
        assert!(snap.targets[0].segment.is_none());
        assert!(snap.targets[0].expiry_date.is_none());
        assert!(snap.targets[0].underlying_symbol.is_none());
        // The old-role entry still converts (the format gate lives in the
        // LOADER, not in to_universe).
        assert!(to_universe(&snap).is_some());
    }
}

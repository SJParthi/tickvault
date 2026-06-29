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
}

/// The on-disk snapshot. Keyed by the IST trading date so a stale (previous-
/// day) snapshot is never mistaken for today's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSnapshot {
    /// `YYYY-MM-DD` IST trading date this snapshot was built for.
    pub trading_date_ist: String,
    /// The ~331 subscription targets (indices + F&O underlyings).
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
    let mut subscription_targets: Vec<SubscriptionTarget> =
        Vec::with_capacity(snapshot.targets.len());
    for t in &snapshot.targets {
        let role = parse_role(&t.role)?;
        let csv_row = CsvRow {
            security_id: t.security_id.clone(),
            symbol_name: t.symbol_name.clone(),
            ..CsvRow::default()
        };
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
        _ => None,
    }
}

/// Build a [`PlanSnapshot`] from a live [`DailyUniverse`] for a trading date.
#[must_use]
pub fn snapshot_from_universe(universe: &DailyUniverse, trading_date_ist: &str) -> PlanSnapshot {
    let targets = universe
        .subscription_targets
        .iter()
        .map(|t| SnapshotTarget {
            role: t.role.as_str().to_string(),
            security_id: t.csv_row.security_id.clone(),
            symbol_name: t.csv_row.symbol_name.clone(),
            is_fno_underlying: t.is_fno_underlying,
            is_index_constituent: t.is_index_constituent,
        })
        .collect();
    PlanSnapshot {
        trading_date_ist: trading_date_ist.to_string(),
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
            targets: vec![SnapshotTarget {
                role: "depth_underlying".to_string(), // not a known role
                security_id: "13".to_string(),
                symbol_name: "NIFTY".to_string(),
                is_fno_underlying: false,
                is_index_constituent: false,
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
        let mut targets = Vec::with_capacity(INDEX_COUNT + UNDERLYING_COUNT);
        targets.push(target(InstrumentRole::Index, "13", "NIFTY"));
        targets.push(target(InstrumentRole::Index, "25", "BANKNIFTY"));
        targets.push(target(InstrumentRole::Index, "51", "SENSEX"));
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
        let expected = INDEX_COUNT + UNDERLYING_COUNT;

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
}

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

use tickvault_common::config::DhanUniverseConfig;
use tickvault_common::constants::{
    DHAN_DETAILED_CSV_URL, INDEX_CONSTITUENCY_SLUGS, IST_UTC_OFFSET_SECONDS_I64,
};
use tickvault_core::instrument::master_csv::{
    Constituent, JoinOutcome, NSE_MEMBERSHIP_TOLERANCE, build_isin_index, join_constituents,
    parse_master_csv,
};
use tickvault_core::instrument::master_download::{
    build_hardened_csv_client, fetch_csv_hardened, nse_index_csv_url, nse_user_agent,
};
use tracing::{error, info, warn};

/// Seconds in a day. Named so the modulo below reads as intent.
const SECS_PER_DAY: i64 = 86_400;

/// Backoff floor between failed attempts, doubling to the configured cap.
const RETRY_BASE_SECS: u64 = 10;

/// Directory for the resolved mapping artifact.
const MAPPING_DIR: &str = "data/instrument-cache";

/// Today's IST date as `YYYY-MM-DD`, recomputed per attempt.
///
/// Never frozen at spawn: a retry loop that crosses IST midnight must name
/// the NEW day's artifact, or a vendor outage spanning midnight writes
/// yesterday's filename with today's data.
fn today_ist_date() -> String {
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
        let secs_to_midnight = u64::from(SECS_PER_DAY as u32 - now_secs_of_day);
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

/// Parses one niftyindices constituent CSV.
///
/// Columns are resolved BY NAME (`Symbol`, `ISIN Code`) for the same reason
/// the master parser does: the layout is the vendor's to change.
fn parse_constituent_csv(index_name: &str, csv: &str) -> Vec<Constituent> {
    let csv = csv.strip_prefix('\u{feff}').unwrap_or(csv);
    let mut lines = csv.lines().filter(|l| !l.trim().is_empty());
    let Some(header) = lines.next() else {
        return Vec::new();
    };
    let cols: Vec<&str> = header.trim_end_matches('\r').split(',').collect();
    let find = |name: &str| cols.iter().position(|c| c.trim() == name);
    let (Some(i_sym), Some(i_isin)) = (find("Symbol"), find("ISIN Code")) else {
        // A list whose header we cannot read yields NOTHING rather than
        // guessing at positions. Zero constituents from one index is visible
        // in the counts; silently reading the wrong column is not.
        return Vec::new();
    };
    let widest = i_sym.max(i_isin);
    let mut out = Vec::new();
    for line in lines {
        let f: Vec<&str> = line.trim_end_matches('\r').split(',').collect();
        if f.len() <= widest {
            continue;
        }
        out.push(Constituent {
            index_name: index_name.to_string(),
            symbol: f[i_sym].trim().to_uppercase(),
            isin: f[i_isin].trim().to_uppercase(),
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
async fn build_once(date: &str) -> anyhow::Result<JoinOutcome> {
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
    info!(
        lists = INDEX_CONSTITUENCY_SLUGS.len(),
        failed = failed_lists,
        constituents = constituents.len(),
        "NSE India index lists downloaded"
    );

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

    write_mapping_atomic(date, &master, &index, &outcome)?;
    Ok(outcome)
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
                exchange_segment: r.exchange_segment as u8,
            })
            .collect(),
    };
    std::fs::create_dir_all(MAPPING_DIR)?;
    let path = std::path::Path::new(MAPPING_DIR).join(format!("dhan-nse-mapping-{date}.json"));
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&artifact)?)?;
    std::fs::rename(&tmp, &path)?;
    info!(
        path = %path.display(),
        resolved = artifact.resolved,
        unresolved = artifact.unresolved,
        "instrument mapping written"
    );
    Ok(())
}

/// Spawns the supervised daily rider.
///
/// Supervised for the same reason every other rider here is: a silent task
/// death would stop the mapping updating while every other signal stayed
/// green, and a stale mapping is worse than an absent one because nothing
/// looks wrong.
#[must_use]
// TEST-EXEMPT: spawns an endless supervised task whose body is network I/O. Its one piece of
// real logic — WHEN to run, including the boot-catch-up case that makes the 08:00 target work
// against an 08:30 box — is `next_wait`, which is pure and has five tests covering both
// boundaries and schedule drift.
pub fn spawn_dhan_universe_rider(config: DhanUniverseConfig) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if !config.enabled {
            info!("[dhan_universe] disabled — no instrument master download this boot");
            return;
        }
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
            match build_once(&today).await {
                Ok(outcome) => {
                    attempt = 0;
                    built_for = Some(today.clone());
                    info!(
                        date = %today,
                        resolved = outcome.resolved.len(),
                        unresolved = outcome.unresolved.len(),
                        "[dhan_universe] daily build COMPLETE"
                    );
                }
                Err(err) => {
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
    })
}

#[cfg(test)]
mod tests {
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
}

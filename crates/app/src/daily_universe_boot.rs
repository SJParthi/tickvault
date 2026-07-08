//! Outer daily-universe BOOT orchestrator — the top-level glue that the
//! `main.rs` boot path calls (the spawn callsite is the next, final
//! wiring step). Chains the three layers merged across the
//! daily-universe sequence into one fail-closed async entry point:
//!
//! 1. [`run_daily_universe_fetch_runner`] (core) — drives the §4 infinite
//!    -retry CSV fetch + `build_universe_from_bytes`, returning a built
//!    [`DailyUniverse`] once a fetch+build cycle succeeds (boot stays
//!    BLOCKED until then under `max_attempts = None`).
//! 2. [`run_lifecycle_reconcile`] (#850) — load yesterday's rows → diff →
//!    §24 audit-first UPSERT/UPDATE. Fail-closed: a read-back error or a
//!    partial prior read aborts the whole boot.
//! 3. [`record_terminal_success`] (#837) — write the terminal
//!    `instrument_fetch_audit` Success row (counts + provenance).
//!
//! ## CSV provenance (SHA-256)
//!
//! The audit row + every lifecycle row carry `source_csv_sha256` (§3/§9
//! tamper-evidence + L3 day-over-day reconcile). The core fetch loop
//! discards the bytes after building, so this orchestrator WRAPS the
//! caller's `fetch_fn` to capture the SHA-256 + CSV data-row count of each
//! fetched body. On success the last capture is the body that built the
//! universe (fetch→build are sequential per attempt; success returns
//! immediately). No change to the core runner's contract.
//!
//! ## §10 write ordering
//!
//! Reconcile (lifecycle UPSERT/UPDATE + lifecycle audit) runs BEFORE the
//! `instrument_fetch_audit` terminal-success row: a reconcile failure
//! aborts the boot before we record a "clean fetch" outcome. The next
//! idempotent boot re-runs.
//!
//! Single responsibility: this module does NOT spawn a tokio task, parse
//! the `--operator-acknowledge-stale-csv` escape valve, or dispatch
//! Telegram (the runner already emits structured tracing the Loki→Telegram
//! path consumes). Those remaining bits live in the `main.rs` Step-6c
//! wiring PR. Generic over `fetch_fn` so tests drive every branch with a
//! synthetic CSV + no live HTTP / QuestDB.
//!
//! **Feature-gated** under `daily_universe_fetcher` per rule §21 —
//! dormant in the default build.

#![cfg(feature = "daily_universe_fetcher")]

use core::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use tickvault_common::config::QuestDbConfig;
use tickvault_common::instrument_types::IndexConstituencyMap;
use tickvault_core::instrument::csv_downloader::CsvDownloadError;
use tickvault_core::instrument::daily_universe::{DailyUniverse, InstrumentRole};
use tickvault_core::instrument::instr_fetch_loop::LoopOutcome;
use tickvault_core::instrument::instr_fetch_runner::run_daily_universe_fetch_runner;
use tracing::info;

use crate::instr_fetch_audit_writer::record_terminal_success;
use crate::lifecycle_reconcile_orchestrator::{ReconcileRunOutcome, run_lifecycle_reconcile};

/// Outcome of a full daily-universe boot — surfaced to the boot caller for
/// the Telegram summary + any post-boot assertions.
#[derive(Debug, Clone, PartialEq)]
pub struct DailyUniverseBootOutcome {
    /// 1-based fetch attempts that ran before the build succeeded.
    pub attempts_used: u32,
    /// Instruments in the built universe.
    pub universe_size: i64,
    /// CSV data rows (excludes the header) of the body that built.
    pub total_rows: i64,
    /// Lowercase-hex SHA-256 of the CSV body that built the universe.
    pub source_csv_sha256: String,
    /// The reconcile tally (the caller MUST inspect `reconcile.apply.errors`).
    pub reconcile: ReconcileRunOutcome,
    /// Wall-clock of the whole instrument load (fetch→build→reconcile), ms.
    /// Real-time proof of the O(1) warm path: a warm-skip boot is sub-second
    /// regardless of universe size; a full rebuild is the batched seconds path.
    pub elapsed_ms: u64,
    /// Per-phase breakdown of `elapsed_ms` — REAL proof (no guessing) of WHERE
    /// a cold boot spends its seconds, so the operator can see download vs
    /// build vs QuestDB-write at a glance. `download_ms` is the wall-clock of
    /// the LAST (successful) CSV GET that built the universe — it excludes
    /// retry/backoff time from earlier failed attempts. `reconcile_ms` is the
    /// lifecycle UPSERT/UPDATE phase (the bulk QuestDB ILP write; sub-ms on a
    /// warm-skip boot). `build_ms` (parse + extract + assemble) is derived by
    /// the caller as `elapsed_ms − download_ms − reconcile_ms`.
    pub download_ms: u64,
    /// Wall-clock of the lifecycle reconcile phase (QuestDB write), ms.
    pub reconcile_ms: u64,
    /// Hostile-review round 4 (2026-07-08): the IST trading date
    /// (`YYYY-MM-DD`) the SUCCESSFUL build attempt actually selected under —
    /// the LAST date the per-attempt `today_ist_fn` derived (R2-2). A cold
    /// build whose §4 retry loop crosses IST midnight builds for D+1 while
    /// the caller's boot-entry date is still D; snapshot writers MUST stamp
    /// THIS date so the plan-snapshot filename + `trading_date_ist` match
    /// the FUTIDX selection date (never an internally-inconsistent D-labeled
    /// artifact carrying the D+1 front month).
    pub build_trading_date_ist: String,
}

/// Lowercase-hex SHA-256 of the CSV bytes — the `source_csv_sha256`
/// provenance + the §9 L2/L3 tamper-evidence digest. PURE.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    // Build the 64-char hex into one pre-sized String (no per-byte
    // `format!` allocation — keeps the hot-path scanner happy even though
    // this is a cold path).
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Count CSV DATA rows (excluding the header line). PURE. A trailing line
/// without a newline still counts; an empty body is 0; a header-only body
/// is 0.
///
/// The production fetch path body-caps the CSV at 50 MB (§18), so the
/// newline count is far below `i64::MAX`; the saturating conversion below
/// is purely defensive against a pathological input and can never trip in
/// a body-capped boot.
#[must_use]
pub fn count_csv_data_rows(bytes: &[u8]) -> i64 {
    if bytes.is_empty() {
        return 0;
    }
    let newlines = bytes.iter().filter(|&&b| b == b'\n').count();
    // Physical lines = newline count + 1 unless the body ends in newline.
    let trailing = i64::from(bytes[bytes.len() - 1] != b'\n');
    let physical_lines = i64::try_from(newlines)
        .unwrap_or(i64::MAX)
        .saturating_add(trailing);
    // Subtract the header line.
    physical_lines.saturating_sub(1).max(0)
}

/// Fail-closed guard before recording a clean fetch-audit Success. PURE.
/// `run_lifecycle_reconcile` returns `Ok` even when individual lifecycle
/// UPSERT/UPDATE actions failed (counted, not raised) — so a naive boot
/// would write `FetchOutcome::Success` while lifecycle rows are partially
/// unwritten (a false-OK, charter Rule 11). If any apply error OR a
/// missing-detail skip occurred, the boot is NOT clean: refuse to record
/// Success so the idempotent next boot re-runs the failed applies and only
/// then stamps a truthful Success row.
///
/// # Errors
///
/// Returns `Err` when `apply.errors > 0` or `apply.skipped_missing_detail > 0`.
pub fn ensure_reconcile_fully_applied(reconcile: &ReconcileRunOutcome) -> Result<()> {
    let errors = reconcile.apply.errors;
    let skipped = reconcile.apply.skipped_missing_detail;
    if errors > 0 || skipped > 0 {
        anyhow::bail!(
            "reconcile did not fully apply ({errors} write error(s), {skipped} \
             skipped-missing-detail) — refusing to record a clean fetch-audit Success; \
             the idempotent next boot re-runs the failed applies"
        );
    }
    Ok(())
}

/// Run the full daily-universe boot. Cold path — called once at startup.
/// See module docs for the 3-layer chain + fail-closed semantics.
///
/// `fetch_fn` is the §4 retry loop's per-attempt fetcher (production: a
/// closure over `CsvDownloader::fetch_csv`). `max_attempts = None` is the
/// production infinite-retry path; tests pass `Some(n)`.
///
/// # Errors
///
/// Returns `Err` when (a) the runner yields no universe (the test-only
/// `Exhausted` path), (b) provenance capture is missing after a successful
/// fetch (internal invariant), (c) the lifecycle reconcile fails
/// (read-back unavailable / partial — fail-closed), (d) the reconcile did
/// not fully apply (`apply.errors > 0` or a missing-detail skip — see
/// [`ensure_reconcile_fully_applied`]; refuses to record a false-OK
/// Success), or (e) the terminal-success audit write fails.
///
/// On success returns `(outcome, Arc<DailyUniverse>)` — the built universe
/// is handed back so the caller (`load_instruments`) can build the
/// ~250-SID WebSocket subscription plan from it without re-fetching.
pub async fn run_daily_universe_boot<Fetch, FetchFut>(
    questdb_config: &QuestDbConfig,
    fetch_fn: Fetch,
    now_ist_nanos: i64,
    today_ist_nanos: i64,
    dry_run: bool,
    max_attempts: Option<u32>,
    ntm_map: Option<IndexConstituencyMap>,
) -> Result<(DailyUniverseBootOutcome, Arc<DailyUniverse>)>
where
    Fetch: FnMut(u32) -> FetchFut,
    FetchFut: Future<Output = Result<Vec<u8>, CsvDownloadError>>,
{
    // Wall-clock the whole instrument load (fetch→build→reconcile). This is
    // the real-time proof of the O(1) warm path: a warm-skip boot is
    // sub-second regardless of universe size; a full rebuild is the batched
    // seconds path. Surfaced via `elapsed_ms` + Telegram + Prometheus.
    let boot_timer = Instant::now();

    // Capture provenance (sha + data-row count + download wall-clock) of each
    // fetched body. On success the last capture is the body that built the
    // universe. `download_ms` times ONLY the GET future (this attempt), so the
    // per-phase breakdown excludes retry/backoff time from earlier attempts.
    let captured: Arc<Mutex<Option<(String, i64, u64)>>> = Arc::new(Mutex::new(None));
    let mut inner = fetch_fn;
    let cap_for_closure = Arc::clone(&captured);
    let wrapped = move |attempt: u32| {
        let fut = inner(attempt);
        let cap = Arc::clone(&cap_for_closure);
        async move {
            let dl_timer = Instant::now();
            let result = fut.await;
            let download_ms = u64::try_from(dl_timer.elapsed().as_millis()).unwrap_or(u64::MAX);
            if let Ok(bytes) = &result {
                let provenance = (sha256_hex(bytes), count_csv_data_rows(bytes), download_ms);
                if let Ok(mut guard) = cap.lock() {
                    *guard = Some(provenance);
                }
            }
            result
        }
    };

    // §36 (2026-07-08): the IST trading date drives the nearest-expiry
    // FUTIDX selection inside the universe build. Hostile-review round 2
    // (2026-07-08, F5): re-derived from the LIVE wall clock PER BUILD
    // ATTEMPT — a boot stuck in the §4 infinite-retry loop across IST
    // midnight (retry backoff caps at 300s, reachable on a long outage)
    // must select FUTIDX with the NEW date, never the frozen boot-entry
    // date (on the T-0→T+1 expiry crossing the frozen date kept the
    // just-expired contract for the whole next session). The lifecycle
    // reconcile keeps stamping the boot-entry `today_ist_nanos` (its
    // pre-existing per-boot property); the frozen date is retained ONLY as
    // the fallback if the IST offset constant were ever invalid.
    let fallback_today_ist = chrono::DateTime::from_timestamp_nanos(today_ist_nanos).date_naive();
    let live_today_ist_fn = move || match chrono::FixedOffset::east_opt(
        tickvault_common::constants::IST_UTC_OFFSET_SECONDS,
    ) {
        Some(offset) => chrono::Utc::now().with_timezone(&offset).date_naive(),
        None => fallback_today_ist,
    };
    // Hostile-review round 4 (2026-07-08): capture the LAST date the
    // per-attempt closure derived — on success that is the date the built
    // universe's FUTIDX selection actually used (fetch→build are sequential
    // per attempt; success returns immediately — the same argument as the
    // provenance capture above). Surfaced as `build_trading_date_ist` so
    // snapshot writers stamp the SELECTION date, not the frozen entry date.
    let (today_ist_fn, build_date_capture) = recording_date_fn(live_today_ist_fn);
    let (outcome, universe) =
        run_daily_universe_fetch_runner(wrapped, max_attempts, ntm_map.as_ref(), today_ist_fn)
            .await;

    let Some(universe) = universe else {
        anyhow::bail!(
            "daily-universe fetch produced no universe (outcome: {outcome:?}) — boot blocked"
        );
    };
    let LoopOutcome::Success { attempts_used } = outcome else {
        anyhow::bail!("daily-universe runner returned a universe with a non-Success outcome");
    };
    // Share the built universe: reconcile borrows it; the caller gets a clone
    // to build the subscription plan (avoids a second CSV fetch).
    let universe = Arc::new(universe);

    // Recover the captured value even from a poisoned mutex (the data is
    // valid; poison only signals a prior panic) so a poison can't be
    // misattributed as "provenance missing".
    let captured_provenance = match captured.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    let (source_csv_sha256, total_rows, download_ms) = captured_provenance
        .context("CSV provenance missing after a successful fetch (internal invariant)")?;

    let universe_size = i64::try_from(universe.total_count()).unwrap_or(i64::MAX);
    let index_count =
        i64::try_from(universe.count_by_role(InstrumentRole::Index)).unwrap_or(i64::MAX);
    let underlying_count =
        i64::try_from(universe.count_by_role(InstrumentRole::FnoUnderlying)).unwrap_or(i64::MAX);
    // §31 NTM observability — flag-derived counts (include the both-case).
    // `fno_underlying_count` == `count_by_role(FnoUnderlying)` today (NTM fold
    // empty pre-#10), but reads from the lossless flag for forward-correctness.
    let fno_flag_count = i64::try_from(universe.fno_underlying_count()).unwrap_or(i64::MAX);
    let index_constituent_count =
        i64::try_from(universe.index_constituent_count()).unwrap_or(i64::MAX);
    // ROLE-partition count (mutually-exclusive) for the audit-row invariant
    // `universe_size == index + underlying + index_constituent`. Distinct from
    // the flag count above, which includes the FnoUnderlying∩IndexConstituent
    // overlap and would break the 3-role partition sum.
    let index_constituent_role_count =
        i64::try_from(universe.count_by_role(InstrumentRole::IndexConstituent)).unwrap_or(i64::MAX);

    // §10 ordering: reconcile FIRST (lifecycle UPSERT/UPDATE + lifecycle
    // audit), then the instrument_fetch_audit terminal-success row. A
    // fail-closed reconcile error aborts before recording a clean outcome.
    let reconcile_timer = Instant::now();
    let reconcile = run_lifecycle_reconcile(
        questdb_config,
        &universe,
        now_ist_nanos,
        today_ist_nanos,
        &source_csv_sha256,
        dry_run,
    )
    .await
    .context("daily lifecycle reconcile failed")?;
    let reconcile_ms = u64::try_from(reconcile_timer.elapsed().as_millis()).unwrap_or(u64::MAX);

    // Refuse to stamp a clean Success audit row if the reconcile did not
    // fully apply (false-OK guard — the next idempotent boot completes it).
    ensure_reconcile_fully_applied(&reconcile)?;

    record_terminal_success(
        questdb_config,
        &outcome,
        now_ist_nanos,
        today_ist_nanos,
        total_rows,
        universe_size,
        index_count,
        underlying_count,
        index_constituent_role_count,
        &source_csv_sha256,
        dry_run,
    )
    .await
    .context("instrument_fetch_audit terminal-success write failed")?;

    let elapsed_ms = u64::try_from(boot_timer.elapsed().as_millis()).unwrap_or(u64::MAX);
    // build_ms (parse + extract + assemble) is the residual after the two
    // measured phases. Saturating so a clock hiccup can never underflow.
    let build_ms = elapsed_ms
        .saturating_sub(download_ms)
        .saturating_sub(reconcile_ms);
    info!(
        attempts_used,
        universe_size,
        index_count,
        fno_underlying_count = fno_flag_count,
        index_constituent_count,
        total_rows,
        upserted = reconcile.apply.upserted,
        expired = reconcile.apply.expired,
        errors = reconcile.apply.errors,
        warm_skipped = reconcile.warm_skipped,
        elapsed_ms,
        download_ms,
        build_ms,
        reconcile_ms,
        dry_run,
        "daily-universe boot complete (per-phase: download→build→reconcile)"
    );
    // Round 4: the date the SUCCESSFUL build selected under. The build
    // always invokes the date fn, so `None` is unreachable in practice —
    // the boot-entry fallback is purely defensive.
    let build_trading_date = match build_date_capture.lock() {
        Ok(guard) => *guard,
        Err(poisoned) => *poisoned.into_inner(),
    }
    .unwrap_or(fallback_today_ist);
    let outcome = DailyUniverseBootOutcome {
        attempts_used,
        universe_size,
        total_rows,
        source_csv_sha256,
        reconcile,
        elapsed_ms,
        download_ms,
        reconcile_ms,
        build_trading_date_ist: build_trading_date.format("%Y-%m-%d").to_string(),
    };
    Ok((outcome, universe))
}

/// Wrap a date-deriving closure so every invocation records the derived
/// date into the returned shared slot — the LAST recorded value is the date
/// the SUCCESSFUL build attempt used (round 4; mirror of the provenance
/// capture wrapper in [`run_daily_universe_boot`]). PURE apart from the
/// slot write; testable without a live build.
fn recording_date_fn<F>(
    inner: F,
) -> (
    impl Fn() -> chrono::NaiveDate,
    Arc<Mutex<Option<chrono::NaiveDate>>>,
)
where
    F: Fn() -> chrono::NaiveDate,
{
    let slot: Arc<Mutex<Option<chrono::NaiveDate>>> = Arc::new(Mutex::new(None));
    let slot_for_closure = Arc::clone(&slot);
    let wrapped = move || {
        let d = inner();
        // Poison only signals a prior panic; the slot write is still valid.
        match slot_for_closure.lock() {
            Ok(mut guard) => *guard = Some(d),
            Err(poisoned) => *poisoned.into_inner() = Some(d),
        }
        d
    };
    (wrapped, slot)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_unreachable() -> QuestDbConfig {
        // Closed loopback port → connection-refused on a real host, so the
        // read-back fails closed without hitting a live QuestDB. (Matches
        // the unreachable-config convention of the sibling reconcile tests.)
        QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 8812,
            ilp_port: 9009,
        }
    }

    /// A valid Detailed-CSV body that `build_universe_from_bytes` accepts
    /// (131 instruments: 100 NSE_EQ underlyings + 30 NSE indices + 1 BSE
    /// SENSEX + 100 FUTSTK derivatives). Mirrors the core orchestrator's
    /// own end-to-end fixture so the universe clears the [100, 1200]
    /// envelope.
    fn valid_csv() -> Vec<u8> {
        let mut s = String::from(
            "SECURITY_ID,EXCH_ID,SEGMENT,INSTRUMENT,SYMBOL_NAME,UNDERLYING_SECURITY_ID\n",
        );
        for i in 0..100 {
            s.push_str(&format!("28{i:04},NSE,NSE_EQ,EQUITY,STK{i},\n"));
        }
        // Operator lock 2026-06-01 §30: the index allowlist drops fake
        // IDX{i} symbols, so use real allowlisted names (first 30 of 31).
        use tickvault_core::instrument::index_extractor::NSE_INDEX_ALLOWLIST;
        for i in 0..30 {
            let sym = NSE_INDEX_ALLOWLIST[i % NSE_INDEX_ALLOWLIST.len()];
            s.push_str(&format!("{},NSE,IDX_I,INDEX,{sym},\n", 1000 + i));
        }
        s.push_str("51,BSE,IDX_I,INDEX,SENSEX,\n");
        for i in 0..100 {
            s.push_str(&format!(
                "{},NSE,NSE_FNO,FUTSTK,STK{i}FUT,28{i:04}\n",
                38000 + i
            ));
        }
        s.into_bytes()
    }

    // ---- sha256_hex ----

    #[test]
    fn test_sha256_hex_known_vectors() {
        // Standard NIST/RFC test vectors.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn test_sha256_hex_is_64_hex_chars() {
        let h = sha256_hex(b"tickvault");
        assert_eq!(h.len(), 64);
        assert!(
            h.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    // ---- count_csv_data_rows ----

    #[test]
    fn test_count_csv_data_rows_excludes_header() {
        assert_eq!(count_csv_data_rows(b"H1,H2\na,b\nc,d\n"), 2);
    }

    #[test]
    fn test_count_csv_data_rows_no_trailing_newline() {
        assert_eq!(count_csv_data_rows(b"H1,H2\na,b\nc,d"), 2);
    }

    #[test]
    fn test_count_csv_data_rows_header_only_is_zero() {
        assert_eq!(count_csv_data_rows(b"H1,H2\n"), 0);
        assert_eq!(count_csv_data_rows(b"H1,H2"), 0);
    }

    #[test]
    fn test_count_csv_data_rows_empty_is_zero() {
        assert_eq!(count_csv_data_rows(b""), 0);
    }

    // ---- ensure_reconcile_fully_applied ----

    fn reconcile_with(errors: usize, skipped: usize) -> ReconcileRunOutcome {
        use crate::apply_reconcile_plan::ApplyOutcome;
        ReconcileRunOutcome {
            universe_size: 10,
            prev_rows_loaded: 10,
            skipped_unknown_state: 0,
            skipped_malformed: 0,
            plan_size: 1,
            apply: ApplyOutcome {
                upserted: 1,
                expired: 0,
                errors,
                malformed_expiry: 0,
                skipped_missing_detail: skipped,
            },
            warm_skipped: false,
        }
    }

    #[test]
    fn test_ensure_reconcile_fully_applied_ok_when_clean() {
        assert!(ensure_reconcile_fully_applied(&reconcile_with(0, 0)).is_ok());
    }

    #[test]
    fn test_ensure_reconcile_fully_applied_errors_on_apply_errors() {
        assert!(
            ensure_reconcile_fully_applied(&reconcile_with(1, 0)).is_err(),
            "apply errors must block a clean Success audit row (false-OK guard)"
        );
    }

    #[test]
    fn test_ensure_reconcile_fully_applied_errors_on_missing_detail() {
        assert!(
            ensure_reconcile_fully_applied(&reconcile_with(0, 2)).is_err(),
            "a missing-detail skip means lifecycle rows did not fully land"
        );
    }

    // ---- recording_date_fn (round 4: snapshot stamps the SELECTION date) ----

    #[test]
    fn test_recording_date_fn_captures_last_derived_date() {
        use std::cell::Cell;
        let calls = Cell::new(0_u32);
        let dates = [
            chrono::NaiveDate::from_ymd_opt(2026, 7, 8).expect("date"),
            chrono::NaiveDate::from_ymd_opt(2026, 7, 9).expect("date"),
        ];
        let inner = || {
            let d = dates[calls.get() as usize];
            calls.set(calls.get() + 1);
            d
        };
        let (wrapped, slot) = recording_date_fn(inner);
        assert!(
            slot.lock().expect("slot").is_none(),
            "nothing recorded before the first derivation"
        );
        // Attempt 1 derives day D; attempt 2 (midnight crossed) derives D+1.
        assert_eq!(wrapped(), dates[0]);
        assert_eq!(*slot.lock().expect("slot"), Some(dates[0]));
        assert_eq!(wrapped(), dates[1]);
        assert_eq!(
            *slot.lock().expect("slot"),
            Some(dates[1]),
            "the LAST derived date wins — the successful build's selection date"
        );
    }

    /// Round 4 source-order ratchet (main.rs scan, style of
    /// `ratchet_main_rs_uses_bounded_reinject_helper`): BOTH plan-snapshot
    /// write sites must stamp the boot outcome's `build_trading_date_ist`
    /// (the date the successful build's FUTIDX selection actually used) —
    /// never the `today_date` string frozen at `load_daily_universe_plan`
    /// entry. A midnight-crossing cold build otherwise writes a D-labeled
    /// snapshot carrying the D+1 selection (internally inconsistent
    /// forensic artifact).
    #[test]
    fn ratchet_main_rs_snapshot_writes_stamp_the_build_date() {
        let src: String = include_str!("main.rs")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        // Needles split via concat! so this test's own source never
        // satisfies the scan vacuously.
        // No closing paren in the needles — rustfmt may add a trailing comma
        // when it breaks the call across lines.
        let slow_path = concat!(
            "write_plan_snapshot(&daily_universe,",
            "&outcome.build_trading_date_ist"
        );
        let warm_bg = concat!(
            "write_plan_snapshot(&fresh_universe,",
            "&fresh_outcome.build_trading_date_ist"
        );
        let frozen_slow = concat!("write_plan_snapshot(&daily_universe,", "&today_date)");
        let frozen_bg = concat!("date_", "for_bg");
        assert!(
            src.contains(slow_path),
            "slow-path snapshot write must stamp outcome.build_trading_date_ist"
        );
        assert!(
            src.contains(warm_bg),
            "warm-path background-reconcile snapshot write must stamp \
             fresh_outcome.build_trading_date_ist"
        );
        assert!(
            !src.contains(frozen_slow),
            "slow-path snapshot write must not use the frozen boot-entry today_date"
        );
        assert!(
            !src.contains(frozen_bg),
            "the frozen date_for_bg clone must not be reintroduced"
        );
    }

    // ---- run_daily_universe_boot ----

    #[tokio::test]
    async fn test_run_daily_universe_boot_no_universe_bails() {
        // Stub fetch returns a TINY (invalid — below the size envelope)
        // CSV; with max_attempts=Some(1) the runner exhausts → no universe
        // → boot bails.
        let cfg = cfg_unreachable();
        let fetch = |_attempt: u32| async {
            Ok::<_, CsvDownloadError>(
                b"SECURITY_ID,EXCH_ID,SEGMENT,INSTRUMENT,SYMBOL_NAME,UNDERLYING_SECURITY_ID\n51,BSE,IDX_I,INDEX,SENSEX,\n".to_vec(),
            )
        };
        let result = run_daily_universe_boot(&cfg, fetch, 1, 1, false, Some(1), None).await;
        assert!(result.is_err(), "no universe built → boot must bail");
    }

    #[tokio::test]
    async fn test_run_daily_universe_boot_valid_csv_reaches_reconcile_then_fails_closed() {
        // Valid CSV → runner Success → universe built → SHA provenance
        // captured + counts extracted → reconcile load-prev hits the
        // unreachable QuestDB → fail-closed Err carrying the reconcile
        // context. Reaching the `reconcile` context (NOT the no-universe
        // or `provenance missing` bail) proves the full wiring ran:
        // wrapper capture → runner Success → universe → counts → reconcile.
        let cfg = cfg_unreachable();
        let fetch = |_attempt: u32| async { Ok::<_, CsvDownloadError>(valid_csv()) };
        let result = run_daily_universe_boot(&cfg, fetch, 1, 1, false, Some(1), None).await;
        let msg = format!("{:#}", result.expect_err("reconcile must fail-closed"));
        assert!(
            msg.contains("reconcile"),
            "should carry reconcile context: {msg}"
        );
        assert!(
            !msg.contains("provenance missing"),
            "provenance was captured: {msg}"
        );
        assert!(!msg.contains("no universe"), "universe was built: {msg}");
    }
}

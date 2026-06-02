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

    let (outcome, universe) = run_daily_universe_fetch_runner(wrapped, max_attempts).await;

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
    let elapsed_ms = u64::try_from(boot_timer.elapsed().as_millis()).unwrap_or(u64::MAX);
    let outcome = DailyUniverseBootOutcome {
        attempts_used,
        universe_size,
        total_rows,
        source_csv_sha256,
        reconcile,
        elapsed_ms,
        download_ms,
        reconcile_ms,
    };
    Ok((outcome, universe))
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
    /// own end-to-end fixture so the universe clears the [100, 400]
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
        let result = run_daily_universe_boot(&cfg, fetch, 1, 1, false, Some(1)).await;
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
        let result = run_daily_universe_boot(&cfg, fetch, 1, 1, false, Some(1)).await;
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

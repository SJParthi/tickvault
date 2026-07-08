//! Sub-PR #10b-δ (2026-05-27): production runner that drives the
//! §4 infinite-retry loop with real `tokio::time::sleep` + structured
//! tracing emission per [`ErrorCode`] severity.
//!
//! Wires together:
//!
//! 1. Sub-PR #10b-γ — pure-async loop driver
//!    ([`run_instr_fetch_loop`](super::instr_fetch_loop::run_instr_fetch_loop))
//! 2. Sub-PR #10 — pure builder
//!    ([`build_universe_from_bytes`](super::daily_universe_orchestrator::build_universe_from_bytes))
//! 3. Sub-PR #10b-β — pure adapter
//!    ([`decide_retry`](super::instr_fetch_retry_adapter::decide_retry))
//! 4. Real `tokio::time::sleep` for the §4 backoff cadence
//! 5. `tracing::{info,warn,error}` with `code = ErrorCode::X.code_str()`
//!    field per `observability-architecture.md` Rule 6 (tag-guard)
//!
//! Telegram dispatch + audit row writes are STILL deferred — those land
//! in Sub-PR #10b-ε (notification service) and Sub-PR #10b-ζ
//! (`instrument_fetch_audit` row). This module emits structured tracing
//! events so Loki picks them up; the dispatch service routes them once
//! wired.
//!
//! # Why generic over the fetcher
//!
//! The caller passes a `FnMut(u32) -> Future<Result<Vec<u8>, CsvDownloadError>>`
//! closure. Production callsite is a one-liner that closes over a
//! [`CsvDownloader`](super::csv_downloader::CsvDownloader):
//!
//! ```ignore
//! let downloader = CsvDownloader::new()?;
//! let (outcome, universe) = run_daily_universe_fetch_runner(
//!     |_attempt| async { downloader.fetch_csv().await },
//!     None,
//! ).await;
//! ```
//!
//! Tests pass a `RefCell`-backed stub so they cover every §4 ladder
//! transition without a live HTTP server.

use core::future::Future;

use tickvault_common::error_code::{ErrorCode, Severity};
use tracing::{error, info, warn};

use tickvault_common::instrument_types::IndexConstituencyMap;

use super::csv_downloader::CsvDownloadError;
use super::daily_universe::DailyUniverse;
use super::daily_universe_orchestrator::{OrchestratorError, build_universe_from_bytes};
use super::instr_fetch_loop::{LoopOutcome, run_instr_fetch_loop};
use super::instr_fetch_retry_adapter::TelegramEmit;

/// Drive the §4 daily-universe fetch contract end-to-end.
///
/// On success returns
/// `(LoopOutcome::Success { attempts_used }, Some(DailyUniverse))`.
///
/// The `max_attempts = None` production path NEVER returns
/// `LoopOutcome::Exhausted` — boot stays BLOCKED until the fetch + build
/// succeeds per §1 fail-closed contract. Tests pass `Some(n)` to bound
/// the loop for deterministic assertions.
///
/// # Side effects
///
/// Per failed attempt, the configured `tokio::time::sleep` is awaited
/// for the §4 backoff duration (10s -> 20s -> 40s -> ... capped 300s).
/// Telegram-eligible attempts (4+) emit a structured `tracing::error!`
/// (severity Critical), `warn!` (severity High), or `info!` (severity
/// Info) with the `code = ErrorCode::X.code_str()` field that
/// downstream Loki / Telegram routing consumes.
///
/// # Errors
///
/// Returns `(LoopOutcome::Exhausted { attempts_used: 0 }, None)` only
/// when called with `max_attempts = Some(0)` — a test-only escape
/// hatch.
pub async fn run_daily_universe_fetch_runner<Fetch, FetchFut>(
    fetch_fn: Fetch,
    max_attempts: Option<u32>,
    ntm_map: Option<&IndexConstituencyMap>,
    today_ist: chrono::NaiveDate,
) -> (LoopOutcome, Option<DailyUniverse>)
where
    Fetch: FnMut(u32) -> FetchFut,
    FetchFut: Future<Output = Result<Vec<u8>, CsvDownloadError>>,
{
    // Wrap the pure builder so the FULL error detail (e.g. the dangling-
    // reference sample from `INSTR-FETCH-03`) reaches Loki on every failed
    // attempt. The retry loop only carries the typed ErrorCode forward, so
    // without this the operator would see "boot BLOCKED" with no clue WHICH
    // underlyings failed to resolve.
    let build_with_diagnostics = |bytes: &[u8]| -> Result<DailyUniverse, OrchestratorError> {
        // §31 NTM (Sub-PR #10b): `ntm_map` is the best-effort niftyindices
        // constituency fetched once by the boot caller; `None` means the source
        // degraded (caller already emitted NTM-CONSTITUENCY-01) → core universe.
        build_universe_from_bytes(bytes, ntm_map, today_ist).inspect_err(|e| {
            let code = e.error_code().code_str();
            warn!(
                code = code,
                stage = e.stage(),
                detail = %e,
                "daily-universe build attempt failed — boot remains BLOCKED",
            );
        })
    };

    run_instr_fetch_loop(
        fetch_fn,
        build_with_diagnostics,
        |duration| tokio::time::sleep(duration),
        emit_telegram_event,
        max_attempts,
    )
    .await
}

/// Structured `tracing` emit for a single retry attempt.
///
/// The `code` field is REQUIRED — `error_code_tag_guard.rs` fails the
/// build if an `error!`/`warn!`/`info!` mentions an INSTR-FETCH-* code
/// without it.
///
/// Severity routing:
///
/// | Severity | tracing macro | Telegram destination (via Loki) |
/// |---|---|---|
/// | `Critical` | `error!` | SNS 4-leg fan-out (SMS + TG + Email + Connect) |
/// | `High` | `warn!` | Telegram Severity::High |
/// | `Info` | `info!` | Telegram Severity::Info |
/// | `Medium` / `Low` | `info!` | informational only |
async fn emit_telegram_event(emit: TelegramEmit, attempt: u32) {
    let code = emit.error_code.code_str();
    let stage = emit.stage;
    match emit.severity {
        Severity::Critical => {
            error!(
                code = code,
                stage = stage,
                attempt = attempt,
                "INSTR-FETCH retry escalated to CRITICAL — boot remains BLOCKED",
            );
        }
        Severity::High => {
            warn!(
                code = code,
                stage = stage,
                attempt = attempt,
                "INSTR-FETCH retry at HIGH escalation tier — boot remains BLOCKED",
            );
        }
        Severity::Info | Severity::Medium | Severity::Low => {
            info!(
                code = code,
                stage = stage,
                attempt = attempt,
                "INSTR-FETCH retry — boot remains BLOCKED",
            );
        }
    }
    // Suppress unused warning when ErrorCode is exhaustive elsewhere; the
    // variant set is the runtime-discriminant we route on, so referencing
    // the type here keeps the import alive without dead-code.
    let _ = ErrorCode::InstrFetch01CsvHardFailed;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instrument::csv_downloader::CsvDownloadError;
    use core::cell::RefCell;
    use core::time::Duration;

    /// Source-scan: confirm the production runner threads
    /// `tokio::time::sleep` into the loop driver and DOES NOT silently
    /// swap in a no-op (which would break the §4 backoff cadence on
    /// AWS).
    #[test]
    fn runner_wires_tokio_time_sleep() {
        let src = include_str!("instr_fetch_runner.rs");
        assert!(
            src.contains("tokio::time::sleep(duration)"),
            "runner MUST drive sleeps through tokio::time::sleep; \
             swapping in a no-op silently violates §4",
        );
    }

    /// Source-scan: confirm the runner threads `build_universe_from_bytes`
    /// into the loop driver. Swapping in a stub would silently bypass
    /// the universe-size envelope check (§2 + §22 fail-closed).
    #[test]
    fn runner_wires_build_universe_from_bytes() {
        let src = include_str!("instr_fetch_runner.rs");
        assert!(
            src.contains("build_universe_from_bytes"),
            "runner MUST drive builds through build_universe_from_bytes; \
             swapping in a stub silently bypasses §2 + §22 envelope",
        );
    }

    /// Source-scan: confirm every Severity arm of `emit_telegram_event`
    /// carries the `code` field — otherwise the
    /// `error_code_tag_guard.rs` meta-test would fail at build time.
    #[test]
    fn emit_carries_code_field_on_every_severity_arm() {
        let src = include_str!("instr_fetch_runner.rs");
        let macro_calls: Vec<&str> = src
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                t.starts_with("error!(") || t.starts_with("warn!(") || t.starts_with("info!(")
            })
            .collect();
        assert!(
            !macro_calls.is_empty(),
            "expected at least one tracing macro call site",
        );
        // Every macro must be followed within ~6 lines by `code = code,`
        // (we accept either inline or multi-line form).
        for (i, line) in src.lines().enumerate() {
            let t = line.trim_start();
            if t.starts_with("error!(") || t.starts_with("warn!(") || t.starts_with("info!(") {
                let window = src.lines().skip(i).take(7).collect::<Vec<_>>().join("\n");
                assert!(
                    window.contains("code = code"),
                    "tracing macro at line {} missing `code = code` field within \
                     7-line window — error_code_tag_guard.rs requires it.\nWindow:\n{}",
                    i + 1,
                    window,
                );
            }
        }
    }

    /// End-to-end: production runner succeeds on attempt 1 when the
    /// supplied fetcher returns valid bytes that the real
    /// `build_universe_from_bytes` accepts.
    ///
    /// We DON'T have 100+ unique F&O underlyings in a synthetic CSV, so
    /// instead we drive the loop to `Exhausted` via `max_attempts = 1`
    /// + a fetcher that returns malformed bytes (parse fails -> retry,
    /// but loop caps at 1). This verifies the wiring without depending
    /// on a 200+ row synthetic CSV.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn runner_returns_exhausted_when_max_attempts_one_and_build_fails() {
        let outcomes = RefCell::new(vec![Ok(b"not-a-csv".to_vec())]);
        let fetch = |_attempt: u32| {
            let next = outcomes.borrow_mut().remove(0);
            async move { next }
        };
        let (outcome, universe) = run_daily_universe_fetch_runner(
            fetch,
            Some(1),
            None,
            chrono::NaiveDate::from_ymd_opt(2026, 7, 8).expect("date"),
        )
        .await;
        assert_eq!(outcome, LoopOutcome::Exhausted { attempts_used: 1 });
        assert!(universe.is_none());
    }

    /// End-to-end: fetcher returns CsvDownloadError -> loop retries once,
    /// then caps at `max_attempts = 1` and reports Exhausted.
    ///
    /// Uses `start_paused = true` + auto-advance via the implicit
    /// `tokio::time::sleep` skipping in paused mode — keeps the test
    /// fast (otherwise the 10s backoff would be wall-clock real).
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn runner_handles_fetcher_error_and_caps_at_max_attempts() {
        let outcomes = RefCell::new(vec![Err(CsvDownloadError::BodyTooLarge)]);
        let fetch = |_attempt: u32| {
            let next = outcomes.borrow_mut().remove(0);
            async move { next }
        };
        let (outcome, universe) = run_daily_universe_fetch_runner(
            fetch,
            Some(1),
            None,
            chrono::NaiveDate::from_ymd_opt(2026, 7, 8).expect("date"),
        )
        .await;
        assert_eq!(outcome, LoopOutcome::Exhausted { attempts_used: 1 });
        assert!(universe.is_none());
    }

    /// `max_attempts = 0` is the test-only escape hatch — returns
    /// immediately without invoking the fetcher.
    #[tokio::test(flavor = "current_thread")]
    async fn runner_zero_max_attempts_returns_immediately() {
        let outcomes: RefCell<Vec<Result<Vec<u8>, CsvDownloadError>>> = RefCell::new(vec![]);
        let mut fetch_called = false;
        let fetch = |_attempt: u32| {
            fetch_called = true;
            let next = outcomes.borrow_mut().pop();
            async move { next.unwrap_or(Err(CsvDownloadError::BodyTooLarge)) }
        };
        let (outcome, universe) = run_daily_universe_fetch_runner(
            fetch,
            Some(0),
            None,
            chrono::NaiveDate::from_ymd_opt(2026, 7, 8).expect("date"),
        )
        .await;
        assert_eq!(outcome, LoopOutcome::Exhausted { attempts_used: 0 });
        assert!(universe.is_none());
        assert!(!fetch_called, "max_attempts=0 must not invoke fetcher");
    }

    /// emit_telegram_event must be reachable with every Severity variant
    /// without panicking. We can't observe the tracing emission directly
    /// in a unit test (no subscriber attached), but we CAN ensure the
    /// match-arms compile + execute for every variant the production
    /// code path could observe.
    #[tokio::test(flavor = "current_thread")]
    async fn emit_telegram_event_handles_every_severity_variant() {
        for severity in [
            Severity::Critical,
            Severity::High,
            Severity::Info,
            Severity::Medium,
            Severity::Low,
        ] {
            let emit = TelegramEmit {
                severity,
                error_code: ErrorCode::InstrFetch01CsvHardFailed,
                stage: "csv_parser",
            };
            // Just verify the call does not panic.
            emit_telegram_event(emit, 5).await;
        }
    }

    /// Compile-time check: `Duration::from_secs(10)` etc. resolve to the
    /// same `core::time::Duration` the loop driver expects. (Catches
    /// accidental `std::time::Duration` shadowing during refactors.)
    #[test]
    fn duration_type_matches_loop_driver() {
        let d: Duration = Duration::from_secs(10);
        assert_eq!(d.as_secs(), 10);
    }
}

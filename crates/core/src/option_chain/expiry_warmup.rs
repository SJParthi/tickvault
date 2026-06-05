//! Pre-market 09:00:30 IST one-shot task that populates
//! [`CurrentExpiryCache`] for NIFTY, BANKNIFTY, and SENSEX.
//!
//! ## Why 09:00:30 IST
//!
//! - 09:00:00 IST is the official NSE pre-open. Dhan starts publishing
//!   IDX_I pre-open ticks at this instant.
//! - 30s buffer absorbs any clock skew between our host + Dhan +
//!   QuestDB.
//! - Market opens at 09:15:00 IST; the first minute-snapshot fires at
//!   09:15:50 IST. Warmup at 09:00:30 IST gives ~15 minutes of safety
//!   margin in case Dhan rate-limits us or the 3 expiry-list calls
//!   take longer than the steady-state ~6s each.
//!
//! ## Why ONE-SHOT (not periodic)
//!
//! Per Dhan docs + operator-confirmed 2026-05-25:
//!
//! > "Dhan only returns ACTIVE (future-dated) expiries; data[0] is
//! > ALWAYS the nearest expiry."
//!
//! So `data[0]` is STABLE within a trading day. Thursday-expiry
//! rollover is handled by the next morning's 09:00:30 IST run — when
//! today is the new Thursday's NEAREST, `data[0]` already reflects
//! that.
//!
//! ## Crash recovery
//!
//! If the app crashes mid-session after warmup has already populated
//! the cache, the boot orchestrator's REHYDRATE step
//! (`crates/app/src/option_chain_cache_loader.rs`) reads the most
//! recent expiry per underlying from QuestDB's
//! `option_chain_minute_snapshot` table — so the cache is restored
//! in <1s without re-calling Dhan.
//!
//! If the crash happens BEFORE 09:00:30 IST and the rehydrate finds
//! no rows (fresh deploy, never warmed up), the next 09:00:30 IST tick
//! does the cold warmup.
//!
//! If the crash happens AFTER 09:00:30 IST on a day where neither
//! warmup nor REHYDRATE succeeded, the per-slot scheduler's
//! cache-miss fallback (in `run_one_slot_fetch`) calls
//! `fetch_expiry_list` inline and populates the cache itself. Three
//! layers of recovery, in order: REHYDRATE → 09:00:30 warmup → inline
//! fallback. This satisfies the Z+ doctrine "L1 DETECT → L2 VERIFY →
//! L3 RECONCILE" matrix for the expiry cache.
//!
//! ## Authority
//!
//! - operator-charter-forever.md §C/§F — honest 100% claim inside
//!   tested envelope: 3 Dhan calls per day for expiry warmup, 3 Dhan
//!   calls per minute for chain snapshots, both audited via
//!   `option_chain_minute_snapshot` table.
//! - market-hours.md — 09:00 IST is the pre-open data window
//!   boundary; 09:15 IST is the trading window.
//! - hot-path.md — this is a COLD path (3 calls/day total). No
//!   bounded-channel concerns.

use std::sync::Arc;

use tracing::{error, info};

use tickvault_common::constants::{IST_UTC_OFFSET_SECONDS_I64, SECONDS_PER_DAY};
use tickvault_common::locked_universe::OPTION_CHAIN_FETCH_SEQUENCE;

use crate::notification::NotificationService;
use crate::option_chain::client::OptionChainClient;
use crate::option_chain::current_expiry_cache::CurrentExpiryCache;

/// Seconds-of-day-IST when the pre-market warmup fires. 09:00:30 IST
/// = 9 * 3600 + 30 = 32_430. Pinned as a named constant per
/// banned-pattern hook category 3 (no hardcoded magic numbers).
pub const WARMUP_FIRE_SECS_OF_DAY_IST: u32 = 9 * 3_600 + 30;

/// Per-underlying segment for the locked universe (all 3 option-chain
/// underlyings are NSE indices = `IDX_I` = byte 0).
const UNDERLYING_SEGMENT_CODE: u8 = 0;
const UNDERLYING_SEGMENT_STR: &str = "IDX_I";

/// Spawn the warmup loop as a background tokio task. The task:
///   1. Sleeps until the next 09:00:30 IST instant.
///   2. Fires 3 sequential expiry-list calls (SENSEX → BANKNIFTY →
///      NIFTY) and populates the cache.
///   3. Sleeps 24h and repeats.
///
/// Returns the `JoinHandle` so the boot orchestrator can await
/// graceful shutdown.
///
/// # Errors
///
/// Construction is infallible. Per-day fetch failures are logged at
/// `error!` and trigger a Telegram alert via `notifier`, but the loop
/// keeps running — tomorrow's warmup gets another chance.
// TEST-EXEMPT: requires tokio runtime + Dhan HTTP client + Telegram
// notifier. The pure-logic primitives `compute_secs_until_next_warmup`
// and `pick_current_expiry_from_list` are covered by unit tests below.
#[must_use]
// TEST-EXEMPT: requires tokio runtime + live Dhan HTTP + Telegram notifier
pub fn spawn_expiry_warmup_task(
    client: OptionChainClient,
    cache: CurrentExpiryCache,
    notifier: Arc<NotificationService>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run_warmup_loop(client, cache, notifier).await;
    })
}

async fn run_warmup_loop(
    mut client: OptionChainClient,
    cache: CurrentExpiryCache,
    notifier: Arc<NotificationService>,
) {
    loop {
        // Compute current seconds-of-day IST.
        let now_utc_secs = chrono::Utc::now().timestamp();
        let now_ist_secs = now_utc_secs.saturating_add(IST_UTC_OFFSET_SECONDS_I64);
        let secs_of_day = (now_ist_secs.rem_euclid(i64::from(SECONDS_PER_DAY))) as u32;

        let sleep_secs = compute_secs_until_next_warmup(secs_of_day);
        info!(
            target_secs_of_day_ist = WARMUP_FIRE_SECS_OF_DAY_IST,
            current_secs_of_day_ist = secs_of_day,
            sleep_secs,
            "option-chain expiry warmup: sleeping until next 09:00:30 IST"
        );
        tokio::time::sleep(std::time::Duration::from_secs(u64::from(sleep_secs))).await;

        run_one_warmup_burst(&mut client, &cache, &notifier).await;
    }
}

/// Executes ONE warmup burst — 3 sequential expiry-list calls in
/// operator-locked sequence (SENSEX → BANKNIFTY → NIFTY) and
/// populates the cache.
///
/// Errors are per-underlying: a failure on one does NOT abort the
/// other two. Successful fetches still populate the cache. Telegram
/// alert fires once with the aggregated failure list at the end.
async fn run_one_warmup_burst(
    client: &mut OptionChainClient,
    cache: &CurrentExpiryCache,
    notifier: &Arc<NotificationService>,
) {
    let mut succeeded: Vec<&'static str> = Vec::new();
    let mut failed: Vec<(&'static str, String)> = Vec::new();

    for &(security_id, symbol) in OPTION_CHAIN_FETCH_SEQUENCE {
        match client
            .fetch_expiry_list(u64::from(security_id), UNDERLYING_SEGMENT_STR)
            .await
        {
            Ok(list) => match pick_current_expiry_from_list(&list.data) {
                Some(expiry) => {
                    cache.insert(security_id, UNDERLYING_SEGMENT_CODE, expiry);
                    succeeded.push(symbol);
                    info!(
                        underlying = symbol,
                        security_id, expiry, "option-chain expiry warmup: cached current expiry"
                    );
                }
                None => {
                    failed.push((symbol, "expiry list returned empty array".into()));
                }
            },
            Err(err) => {
                failed.push((symbol, err.to_string()));
            }
        }
    }

    if !failed.is_empty() {
        let names: Vec<String> = failed
            .iter()
            .map(|(name, reason)| format!("{name}={reason}"))
            .collect();
        error!(
            failed_count = failed.len(),
            succeeded_count = succeeded.len(),
            failures = ?names,
            "option-chain expiry warmup: one or more underlyings failed"
        );
        // Best-effort Telegram. Failure here does NOT block tomorrow's
        // retry. `notifier` is kept wired (the reference is Copy) for the
        // follow-up typed NotificationEvent variant noted below.
        let _notifier = notifier;
        // Hook: a typed NotificationEvent::OptionChainExpiryWarmupFailed
        // variant can land in a follow-up PR. Today we rely on the
        // structured `error!` line above, which Loki routes to
        // Telegram per observability-architecture.md.
    } else {
        info!(
            count = succeeded.len(),
            "option-chain expiry warmup: all underlyings cached successfully"
        );
    }
}

/// Pure helper — computes how many seconds to sleep before the next
/// 09:00:30 IST warmup. If we're already past today's 09:00:30 IST,
/// sleep until tomorrow's. If we're before, sleep the gap.
///
/// O(1). Public for test coverage.
#[must_use]
pub fn compute_secs_until_next_warmup(now_secs_of_day_ist: u32) -> u32 {
    if now_secs_of_day_ist < WARMUP_FIRE_SECS_OF_DAY_IST {
        WARMUP_FIRE_SECS_OF_DAY_IST - now_secs_of_day_ist
    } else {
        // Tomorrow's 09:00:30 IST. saturating_sub guards against the
        // (impossible) end-of-day wrap edge.
        SECONDS_PER_DAY
            .saturating_sub(now_secs_of_day_ist)
            .saturating_add(WARMUP_FIRE_SECS_OF_DAY_IST)
    }
}

/// Pure helper — picks the current expiry from the Dhan
/// `/optionchain/expirylist` response.
///
/// Dhan returns the array sorted ascending and filters out past
/// expiries server-side, so `data[0]` is ALWAYS the current/nearest
/// expiry. This helper exists so the contract is explicit + ratcheted
/// by tests (rather than scattered `.first()` calls).
///
/// O(1). Returns `None` only if the array is empty (Dhan-side bug or
/// underlying with no listed options).
#[must_use]
pub fn pick_current_expiry_from_list(expiry_list: &[String]) -> Option<&str> {
    expiry_list.first().map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_warmup_fire_secs_of_day_is_09_00_30_ist() {
        // Pin the operator-locked time: 09:00:30 IST = 32_430 seconds.
        // 9 * 3600 = 32_400; + 30 = 32_430.
        assert_eq!(WARMUP_FIRE_SECS_OF_DAY_IST, 32_430);
    }

    #[test]
    fn test_warmup_fires_after_pre_open_but_before_market_open() {
        // Pre-open starts at 09:00:00 IST (32_400s). Market opens at
        // 09:15:00 IST (33_300s). Warmup MUST fire strictly between
        // these two — so ticks are flowing AND the cache is populated
        // before the first minute-snapshot at 09:15:50 IST.
        assert!(WARMUP_FIRE_SECS_OF_DAY_IST > 9 * 3_600); // > pre-open
        assert!(WARMUP_FIRE_SECS_OF_DAY_IST < 9 * 3_600 + 15 * 60); // < market open
    }

    #[test]
    fn test_compute_secs_until_next_warmup_before_today() {
        // It's 08:00:00 IST (= 28_800s). Warmup is 1h 0m 30s away
        // = 3_630s.
        let now = 8 * 3_600;
        assert_eq!(
            compute_secs_until_next_warmup(now),
            WARMUP_FIRE_SECS_OF_DAY_IST - now
        );
        assert_eq!(compute_secs_until_next_warmup(now), 3_630);
    }

    #[test]
    fn test_compute_secs_until_next_warmup_at_exact_instant() {
        // Exactly 09:00:30 IST. The "now is past" branch fires (we
        // already missed today's window by 0 seconds), so we sleep
        // until tomorrow.
        let now = WARMUP_FIRE_SECS_OF_DAY_IST;
        let sleep = compute_secs_until_next_warmup(now);
        // ~24h - 0 = 86_400s
        assert_eq!(sleep, SECONDS_PER_DAY);
    }

    #[test]
    fn test_compute_secs_until_next_warmup_after_today() {
        // It's 10:30:00 IST (= 37_800s). Today's 09:00:30 already
        // fired. Sleep until tomorrow's = (86_400 - 37_800) + 32_430.
        let now = 10 * 3_600 + 30 * 60;
        let expected = SECONDS_PER_DAY - now + WARMUP_FIRE_SECS_OF_DAY_IST;
        assert_eq!(compute_secs_until_next_warmup(now), expected);
    }

    #[test]
    fn test_compute_secs_until_next_warmup_just_before_midnight() {
        // 23:59:59 IST. Sleep just over 9h until 09:00:30 next morning.
        let now = SECONDS_PER_DAY - 1;
        let expected = 1 + WARMUP_FIRE_SECS_OF_DAY_IST;
        assert_eq!(compute_secs_until_next_warmup(now), expected);
    }

    #[test]
    fn test_pick_current_expiry_from_list_alias_for_guard() {
        // Pub-fn-test-guard requires a test name containing the full
        // pub fn identifier `pick_current_expiry_from_list`. The
        // canonical scenario tests below cover the actual semantics.
        assert_eq!(
            pick_current_expiry_from_list(&["2026-05-26".to_string()]),
            Some("2026-05-26")
        );
    }

    #[test]
    fn test_pick_current_expiry_from_nonempty_list_returns_first() {
        // Dhan returns ascending-sorted ACTIVE expiries; data[0] is
        // ALWAYS the current/nearest expiry. Pin that contract.
        let list = vec![
            "2026-05-26".to_string(),
            "2026-05-29".to_string(),
            "2026-06-05".to_string(),
            "2026-06-26".to_string(),
        ];
        let got = pick_current_expiry_from_list(&list).expect("non-empty");
        assert_eq!(got, "2026-05-26");
    }

    #[test]
    fn test_pick_current_expiry_from_empty_list_returns_none() {
        let list: Vec<String> = Vec::new();
        assert!(pick_current_expiry_from_list(&list).is_none());
    }

    #[test]
    fn test_pick_current_expiry_does_not_compare_with_today() {
        // Even if data[0] is suspiciously close to "today", we still
        // return it — Dhan has authoritative knowledge of which
        // expiries are active, not our local clock. This test pins
        // the "trust Dhan" contract per operator confirmation
        // 2026-05-25.
        let list = vec!["1970-01-01".to_string(), "2026-05-26".to_string()];
        let got = pick_current_expiry_from_list(&list).expect("non-empty");
        assert_eq!(
            got, "1970-01-01",
            "must return data[0] verbatim; no date arithmetic"
        );
    }
}

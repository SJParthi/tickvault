//! Option-chain minute-snapshot scheduler (PR #4b of 5 per
//! `.claude/plans/friday-may-15-mega/topic-OPTION-CHAIN-MINUTE-SNAPSHOT.md`).
//!
//! Per-slot tokio task that:
//!   1. Sleeps until the next scheduled second (`HH:MM:53` etc.) inside
//!      market hours `[09:15, 15:30) IST`.
//!   2. Fires `OptionChainClient::fetch_option_chain` for ONE underlying.
//!   3. Writes the result into the shared [`SnapshotCache`] (RAM only —
//!      the QuestDB `option_chain_minute_snapshot` table was dropped
//!      2026-06-23; the strategy reads the RAM cache).
//!   4. Emits 4 Prometheus counters + (on first failure of the minute)
//!      the `OptionChainFetchFailed` Telegram.
//!
//! Modeled on `crates/app/src/greeks_pipeline.rs` — the canonical
//! periodic-fetch pattern in this codebase.
//!
//! ## Adversarial defenses baked in (per operator-charter §E)
//!
//! **hot-path-reviewer concerns:**
//!   * Cache writes are NOT hot path — they happen 3 times/minute at
//!     most. Allocation (e.g. `format!` for error strings) is fine
//!     here.
//!   * Strategy READS from `SnapshotCache` are O(1) papaya lookup —
//!     pinned by `option_chain::snapshot_cache::tests`.
//!
//! **security-reviewer concerns:**
//!   * Tokens are handled inside the existing `OptionChainClient` (uses
//!     `arc-swap` + `Secret<String>`) — this scheduler never touches
//!     `access-token` directly.
//!   * URLs are constants in `OptionChainClient` — no string injection
//!     surface here.
//!   * Error messages truncated to 200 chars before going into Telegram
//!     to prevent operator-readable strings from leaking response
//!     bodies.
//!
//! **hostile-bug-hunt concerns:**
//!   * Market-hours gate per `audit-findings-2026-04-17.md` Rule 3.
//!   * Edge-triggered Telegram per Rule 4 — `HashSet<(symbol, minute)>`
//!     tracks which `(underlying, minute)` already fired.
//!   * Per-day reset of edge-trigger set when minute crosses midnight
//!     so next day's first failure fires correctly (regression-class
//!     2026-05-13).
//!   * No `unwrap()` / `expect()` in production paths.
//!   * Errors use `error!` not `warn!` per Rule 5.

use std::collections::HashSet;
use std::sync::Arc;

use tracing::{debug, error, info, warn};

use tickvault_common::config::{OptionChainMinuteSnapshotConfig, OptionChainUnderlyingEntry};
use tickvault_common::constants::{
    IST_UTC_OFFSET_SECONDS_I64, OPTION_CHAIN_COOLDOWN_LADDER_SECS, OPTION_CHAIN_FETCH_CADENCE_SECS,
    OPTION_CHAIN_STALE_CYCLES_BEFORE_TELEGRAM, SECONDS_PER_DAY,
};
use tickvault_common::market_hours::is_within_market_hours_ist;

use crate::notification::{NotificationEvent, NotificationService};
use crate::option_chain::client::OptionChainClient;
use crate::option_chain::snapshot_cache::{CachedSnapshot, SnapshotCache};

/// Max chars of an error message that get embedded in a Telegram body.
/// Prevents accidental leak of full HTTP response bodies (which could
/// contain truncated JWT prefixes if Dhan ever echoes auth-error
/// detail).
const TELEGRAM_ERROR_REASON_MAX_LEN: usize = 200;

/// Fallback sleep duration when `compute_next_slot` returns `None`
/// (degenerate empty-schedule case — should never happen post-boot
/// validator). Named constant per banned-pattern hook category 3.
const EMPTY_SCHEDULE_BACKOFF_SECS: u64 = 60;

/// The 5 wire-format Prometheus counter names. Pinned as constants so
/// dashboards + alerts can rely on exact-match labels.
pub const METRIC_FETCHES_TOTAL: &str = "tv_option_chain_fetches_total";
pub const METRIC_FETCH_ERRORS_TOTAL: &str = "tv_option_chain_fetch_errors_total";
pub const METRIC_CACHE_AGE_SECS: &str = "tv_option_chain_cache_age_secs";
pub const METRIC_STRATEGY_HALTS_TOTAL: &str = "tv_option_chain_strategy_halts_total";

/// State carried across scheduler iterations. Owns the cache handle +
/// edge-trigger set + the day on which the edge-trigger set was last
/// reset.
struct SchedulerState {
    cache: SnapshotCache,
    /// `(underlying_symbol, minute_of_day_ist)` of every fire this day.
    /// Cleared at IST midnight so tomorrow's first failure fires.
    fired_this_minute: HashSet<(String, u32)>,
    /// IST date the edge-trigger set was last cleared. `i64` so we can
    /// compare against `now_ist / SECONDS_PER_DAY`.
    last_reset_day_ist: i64,
    /// PR #8e — L7 COOLDOWN failure-streak tracker. Drives the
    /// cadence ladder (50 → 60 → 90 → 120s) after consecutive fetch
    /// failures.
    cooldown: OptionChainCooldown,
}

/// PR #8e — L7 COOLDOWN state. Tracks the consecutive-failure streak
/// and computes the effective fetch cadence per
/// `docs/architecture/option-chain-z-plus-heart-piece.md` §3 (L7):
/// after 3 consecutive failed cycles, slow down (60 → 90 → 120s) so we
/// stop hammering a degraded Dhan API and cut operator alert fatigue.
/// The first success snaps the cadence back to the 50s base.
///
/// Pure-logic struct — no clock, no I/O. Fully unit-tested.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct OptionChainCooldown {
    /// Number of consecutive failed fetch cycles. Reset to 0 on the
    /// first success.
    consecutive_failures: u32,
}

impl OptionChainCooldown {
    /// Record one failed fetch cycle. Saturating — never overflows.
    fn record_failure(&mut self) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
    }

    /// Record one successful fetch cycle — resets the streak so the
    /// cadence snaps back to the base.
    fn record_success(&mut self) {
        self.consecutive_failures = 0;
    }

    /// Effective fetch cadence in seconds given the operator-locked
    /// `base` (50s). Below the trigger threshold the base is returned
    /// unchanged; at/above it, the ladder applies, saturating at the
    /// final (120s) rung.
    fn effective_cadence_secs(self, base: u64) -> u64 {
        let trigger = OPTION_CHAIN_STALE_CYCLES_BEFORE_TELEGRAM;
        if self.consecutive_failures < trigger {
            return base;
        }
        // failures == trigger → ladder[0]; trigger+1 → ladder[1]; etc.
        let ladder = OPTION_CHAIN_COOLDOWN_LADDER_SECS;
        let last_idx = ladder.len() - 1;
        let idx = ((self.consecutive_failures - trigger) as usize).min(last_idx);
        ladder[idx]
    }

    /// Extra sleep to add above the slot-based schedule when in
    /// cooldown — `effective_cadence_secs - base`, saturating to 0
    /// when not in cooldown.
    fn extra_cooldown_secs(self, base: u64) -> u64 {
        self.effective_cadence_secs(base).saturating_sub(base)
    }

    /// `true` iff the cooldown ladder is currently slowing the cadence.
    fn is_active(self) -> bool {
        self.consecutive_failures >= OPTION_CHAIN_STALE_CYCLES_BEFORE_TELEGRAM
    }
}

impl SchedulerState {
    fn new(cache: SnapshotCache) -> Self {
        Self {
            cache,
            fired_this_minute: HashSet::with_capacity(16),
            last_reset_day_ist: 0,
            cooldown: OptionChainCooldown::default(),
        }
    }

    /// Clear the edge-trigger set if today's IST date differs from the
    /// last-reset day. Pure logic — testable.
    fn maybe_reset_edge_trigger(&mut self, now_ist_secs: i64) {
        let today_ist_days = now_ist_secs / i64::from(SECONDS_PER_DAY);
        if today_ist_days != self.last_reset_day_ist {
            self.fired_this_minute.clear();
            self.last_reset_day_ist = today_ist_days;
        }
    }

    /// Record + check edge-trigger for one `(underlying, minute)` pair.
    /// Returns `true` if this is the first fire (caller should send the
    /// Telegram), `false` if already fired this minute.
    fn try_fire(&mut self, underlying: &str, minute_of_day_ist: u32) -> bool {
        let key = (underlying.to_string(), minute_of_day_ist);
        self.fired_this_minute.insert(key)
    }
}

/// Compute the wall-clock IST seconds-of-day for the NEXT firing of
/// the operator-locked underlyings list. Pure function.
///
/// Strategy:
///   * Walk the schedule in `slot_sec` order
///   * Within today's current minute, return the next future slot
///   * If all of today's slots have passed, return the FIRST slot
///     of the next minute
///
/// Returns `(target_secs_of_day_ist, underlying_index_into_schedule)`.
#[must_use]
pub fn compute_next_slot(
    schedule: &[OptionChainUnderlyingEntry],
    now_secs_of_day_ist: u32,
) -> Option<(u32, usize)> {
    if schedule.is_empty() {
        return None;
    }
    // Sort slot_sec values to walk in time order regardless of TOML order.
    let mut slots: Vec<(u32, usize)> = schedule
        .iter()
        .enumerate()
        .map(|(idx, e)| (e.slot_sec, idx))
        .collect();
    slots.sort_by_key(|(s, _)| *s);

    let now_sec_in_min = now_secs_of_day_ist % 60;
    let now_min_start = now_secs_of_day_ist - now_sec_in_min;

    // Try to find a slot inside the CURRENT minute that is still future.
    for (slot, idx) in &slots {
        let target = now_min_start.saturating_add(*slot);
        if target > now_secs_of_day_ist {
            return Some((target, *idx));
        }
    }

    // All today's-current-minute slots have passed. Fire the FIRST
    // slot of the NEXT minute. saturating_add prevents end-of-day
    // overflow (max ~86_399 + 60 < u32::MAX by 7 orders of magnitude).
    let (first_slot, first_idx) = slots[0];
    let next_min_start = now_min_start.saturating_add(60);
    Some((next_min_start.saturating_add(first_slot), first_idx))
}

/// Spawn the snapshot scheduler as a background tokio task. Returns
/// the `JoinHandle` so the caller can await graceful shutdown.
///
/// If `config.enabled == false` OR `config.underlyings` is empty, this
/// function returns `None` — boot continues without spawning. The
/// boot-time validator (`option_chain_schedule::validate_*`) already
/// HALTs on invalid TOML; this function is the soft toggle.
///
/// # Errors
///
/// Construction failures (cache handle missing, etc.) are infallible
/// today — the function returns `Option<JoinHandle>` to model the
/// disable case explicitly.
// WIRING-EXEMPT: boot call site lands in PR #5 of the 5-PR rollout.
// This PR (#4b) ships the scheduler module + tests; main.rs spawn
// invocation is deferred so PR #5 can land it alongside the strategy
// cache-read path + verification Make target as a coherent unit.
// Pure helpers `compute_next_slot` + `SchedulerState::try_fire` cover
// the testable logic surface in this PR.
// TEST-EXEMPT: requires tokio runtime + Dhan client + Telegram notifier (PR #5 system test)
pub fn spawn_snapshot_scheduler(
    config: OptionChainMinuteSnapshotConfig,
    client: OptionChainClient,
    cache: SnapshotCache,
    current_expiry_cache: crate::option_chain::current_expiry_cache::CurrentExpiryCache,
    notifier: Arc<NotificationService>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !config.enabled {
        info!("option_chain_minute_snapshot: disabled by config — scheduler NOT spawned");
        return None;
    }
    if config.underlyings.is_empty() {
        warn!(
            "option_chain_minute_snapshot: enabled but underlyings list empty — scheduler NOT spawned"
        );
        return None;
    }

    info!(
        underlyings = config.underlyings.len(),
        cache_max_stale_secs = config.cache_max_stale_secs,
        "spawning option-chain snapshot scheduler"
    );

    Some(tokio::spawn(async move {
        run_snapshot_loop(config, client, cache, current_expiry_cache, notifier).await;
    }))
}

/// The main loop — sleeps to the next slot, fetches, persists into
/// the cache, emits counters + Telegram. Never returns under normal
/// operation; exits cleanly only on tokio runtime shutdown.
async fn run_snapshot_loop(
    config: OptionChainMinuteSnapshotConfig,
    mut client: OptionChainClient,
    cache: SnapshotCache,
    current_expiry_cache: crate::option_chain::current_expiry_cache::CurrentExpiryCache,
    notifier: Arc<NotificationService>,
) {
    let mut state = SchedulerState::new(cache.clone());

    loop {
        // Compute current IST seconds-of-day.
        let now_utc_secs = chrono::Utc::now().timestamp();
        let now_ist_secs = now_utc_secs.saturating_add(IST_UTC_OFFSET_SECONDS_I64);
        let secs_of_day = (now_ist_secs.rem_euclid(i64::from(SECONDS_PER_DAY))) as u32;

        state.maybe_reset_edge_trigger(now_ist_secs);

        // Find the next slot in the operator-locked schedule.
        let Some((target_secs, idx)) = compute_next_slot(&config.underlyings, secs_of_day) else {
            warn!(
                backoff_secs = EMPTY_SCHEDULE_BACKOFF_SECS,
                "option-chain scheduler: empty schedule — backing off"
            );
            tokio::time::sleep(std::time::Duration::from_secs(EMPTY_SCHEDULE_BACKOFF_SECS)).await;
            continue;
        };

        // Sleep until target_secs_of_day_ist. saturating math
        // throughout — we never panic on time going backwards.
        let sleep_secs = u64::from(target_secs.saturating_sub(secs_of_day));
        debug!(
            target_secs,
            secs_of_day, sleep_secs, "option-chain scheduler: sleeping until next slot"
        );
        tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)).await;

        // Re-check market hours AFTER waking (operator-charter Rule 3).
        if !is_within_market_hours_ist() {
            debug!("option-chain scheduler: woke off-hours — skip this slot");
            continue;
        }

        // Resolve the underlying. Index is bounds-checked because
        // compute_next_slot only returns indices it produced from
        // the same slice.
        let entry = match config.underlyings.get(idx) {
            Some(e) => e.clone(),
            None => {
                error!(
                    idx,
                    "option-chain scheduler: index out of bounds — schedule shrank \
                     mid-run, skipping"
                );
                continue;
            }
        };

        let cache_handle = state.cache.clone();
        run_one_slot_fetch(
            &entry,
            &mut client,
            &cache_handle,
            &current_expiry_cache,
            &notifier,
            &mut state,
            &config,
        )
        .await;

        // PR #8e — L7 COOLDOWN: if consecutive failures have crossed
        // the trigger threshold, sleep an extra interval ABOVE the
        // slot-based schedule before the next loop iteration. This
        // stops the scheduler hammering a degraded Dhan API and cuts
        // operator alert fatigue. `extra_cooldown_secs` returns 0 when
        // the streak is below the threshold, so the happy path is a
        // no-op.
        let extra = state
            .cooldown
            .extra_cooldown_secs(OPTION_CHAIN_FETCH_CADENCE_SECS);
        if extra > 0 {
            warn!(
                consecutive_failures = state.cooldown.consecutive_failures,
                extra_cooldown_secs = extra,
                "option-chain L7 cooldown active — extending cadence"
            );
            tokio::time::sleep(std::time::Duration::from_secs(extra)).await;
        }
    }
}

/// Fetch one slot's option chain, persist to RAM cache, emit telemetry.
///
/// Errors are logged + counted but never bubbled up — the scheduler
/// must keep running through transient failures.
/// hostile-bug-hunt 2026-05-16: this scheduler currently issues ONE
/// attempt per slot per the operator-locked staggered schedule
/// (:53/:56/:59). The `OptionChainClient` itself already implements
/// up to 3 internal retries with exponential backoff (`MAX_RETRIES`
/// in client.rs), so the per-slot fetch effectively gets 3 attempts
/// at the network level before this scheduler counts it as a slot
/// failure. The `fetch_retry_max_attempts` config field is RESERVED
/// for a future per-slot inter-attempt budget (PR #5 candidate) —
/// today it is intentionally unused, signalled by the `_config`
/// underscore parameter.
async fn run_one_slot_fetch(
    entry: &OptionChainUnderlyingEntry,
    client: &mut OptionChainClient,
    cache: &SnapshotCache,
    current_expiry_cache: &crate::option_chain::current_expiry_cache::CurrentExpiryCache,
    notifier: &Arc<NotificationService>,
    state: &mut SchedulerState,
    _config: &OptionChainMinuteSnapshotConfig,
) {
    let underlying = entry.symbol.as_str();
    let segment = entry.segment.as_str();
    let security_id = u64::from(entry.security_id);

    // Step 1 — resolve the current expiry. Operator-confirmed
    // 2026-05-25: `data[0]` is ALWAYS the nearest/current expiry per
    // Dhan's `/optionchain/expirylist` contract. The cache is populated
    // by the 09:00:30 IST `expiry_warmup` task and the boot-time
    // REHYDRATE (`option_chain_cache_loader`). Cache hit = skip the
    // expiry-list Dhan call entirely (50% reduction in REST traffic).
    // Cache miss = fall back to inline fetch (Z+ L6 RECOVER).
    let expiry: String = match current_expiry_cache.get(
        entry.security_id,
        tickvault_common::segment::segment_str_to_code(segment).unwrap_or(0),
    ) {
        Some(cached) => (*cached).to_string(),
        None => {
            // Fallback: cache miss. Inline expiry-list fetch.
            // Populates the cache on success so subsequent slots
            // benefit immediately.
            let expiries = match client.fetch_expiry_list(security_id, segment).await {
                Ok(list) => list,
                Err(err) => {
                    record_fetch_failure(underlying, 1, &err.to_string(), notifier, state);
                    return;
                }
            };
            let Some(first) = expiries.data.into_iter().next() else {
                record_fetch_failure(
                    underlying,
                    1,
                    "expiry list returned empty array",
                    notifier,
                    state,
                );
                return;
            };
            current_expiry_cache.insert(
                entry.security_id,
                tickvault_common::segment::segment_str_to_code(segment).unwrap_or(0),
                &first,
            );
            warn!(
                underlying,
                expiry = first.as_str(),
                "option-chain: current-expiry cache MISS — inline fetch + populate"
            );
            first
        }
    };

    // Step 2 — fetch the option chain itself, with intra-minute
    // retry per operator lock 2026-05-25 (PR #787). On failure we
    // retry up to MAX_ATTEMPTS_PER_UNDERLYING_PER_BURST times within
    // the same minute window, respecting Dhan's per-underlying 3s
    // rate limit (the client's internal `enforce_rate_limit` inserts
    // the wait automatically). The retry budget is bounded by the
    // remaining seconds until the :59 hard deadline.
    let burst_deadline_sec = crate::option_chain::burst_orchestrator::BURST_DEADLINE_SEC_OF_MINUTE;
    let mut chain_opt: Option<crate::option_chain::types::OptionChainResponse> = None;
    let mut last_error: String = String::new();
    let mut attempts: u32 = 0;
    while attempts < crate::option_chain::burst_orchestrator::MAX_ATTEMPTS_PER_UNDERLYING_PER_BURST
    {
        attempts = attempts.saturating_add(1);
        match client
            .fetch_option_chain(security_id, segment, &expiry)
            .await
        {
            Ok(c) => {
                if attempts > 1 {
                    info!(
                        underlying,
                        attempts, "option-chain fetch RECOVERED via intra-minute retry"
                    );
                }
                chain_opt = Some(c);
                break;
            }
            Err(err) => {
                last_error = err.to_string();
                // Decide whether to retry: budget + deadline check.
                let now_utc = chrono::Utc::now().timestamp();
                let now_secs_of_day =
                    crate::option_chain::burst_orchestrator::ist_secs_of_day_now(now_utc);
                let sec_in_min = now_secs_of_day % 60;
                // Remaining seconds in this minute window before :59.
                let secs_remaining = burst_deadline_sec.saturating_sub(sec_in_min);
                match crate::option_chain::burst_orchestrator::next_retry_sleep_secs(
                    secs_remaining,
                    attempts,
                ) {
                    Some(sleep_secs) => {
                        warn!(
                            underlying,
                            attempt = attempts,
                            secs_remaining,
                            sleep_secs,
                            error = last_error.as_str(),
                            "option-chain fetch failed — intra-minute retry scheduled"
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)).await;
                    }
                    None => {
                        // No retry budget left — exit and record failure.
                        break;
                    }
                }
            }
        }
    }
    let Some(chain) = chain_opt else {
        record_fetch_failure(underlying, attempts, &last_error, notifier, state);
        return;
    };

    // Step 2.5 — L2 VERIFY (PR #8d, heart-piece §3 row 2). Structurally
    // validate the response BEFORE it reaches the cache. A bad-status /
    // empty / too-few-strikes response is REJECTED — the cache keeps
    // its previous (older but valid) snapshot rather than serving the
    // strategy a malformed chain to strike-select against.
    let l2 = super::l2_verify::verify_option_chain_structure(&chain);
    if !l2.is_valid() {
        record_fetch_failure(underlying, 1, &l2.reason(), notifier, state);
        return;
    }

    // Step 3 — write into the RAM cache. `Instant::now()` AFTER the
    // network call so the cache's `received_at` reflects when the
    // strategy will actually see this data, not when the request
    // started.
    let cached = CachedSnapshot {
        response: Arc::new(chain),
        received_at: std::time::Instant::now(),
        underlying_symbol: underlying.to_string(),
        expiry: expiry.clone(),
    };
    cache.insert((underlying.to_string(), segment.to_string()), cached);

    // Step 4 — telemetry. Static labels per audit-findings Rule 12 (no
    // raw counter rendering — dashboards wrap in `increase()` later).
    metrics::counter!(
        METRIC_FETCHES_TOTAL,
        "underlying" => underlying.to_string(),
        "outcome" => "fresh"
    )
    .increment(1);

    // PR #8e — L7 COOLDOWN: a successful cycle resets the failure
    // streak so the cadence snaps back to the 50s base.
    let was_in_cooldown = state.cooldown.is_active();
    state.cooldown.record_success();
    if was_in_cooldown {
        info!(
            underlying,
            "option-chain L7 cooldown cleared — cadence back to base"
        );
    }

    info!(underlying, expiry, "option-chain snapshot fetched + cached");
}

/// Record a fetch failure: increment error counter, fire edge-triggered
/// Telegram. Pulled out so the (rare) success path stays clean.
fn record_fetch_failure(
    underlying: &str,
    attempts_made: u32,
    reason: &str,
    notifier: &Arc<NotificationService>,
    state: &mut SchedulerState,
) {
    let reason_trunc: String = reason.chars().take(TELEGRAM_ERROR_REASON_MAX_LEN).collect();

    // PR #8e — L7 COOLDOWN: a failed cycle extends the streak. Single
    // chokepoint — every failure return in `run_one_slot_fetch` routes
    // through here, so the streak count stays consistent.
    state.cooldown.record_failure();

    // hot-path-reviewer 2026-05-16: drop the .to_string() on
    // classify_error — it already returns &'static str so the
    // allocation was redundant. The `underlying` label still
    // allocates per call but cardinality is bounded by the
    // operator-locked 3-underlying set; cold path acceptable.
    metrics::counter!(
        METRIC_FETCH_ERRORS_TOTAL,
        "underlying" => underlying.to_string(),
        "error_class" => classify_error(&reason_trunc)
    )
    .increment(1);

    error!(
        underlying,
        attempts_made,
        reason = %reason_trunc,
        "option-chain fetch FAILED"
    );

    // Edge-trigger: only fire Telegram on the FIRST failure of this
    // minute. `now_ist_secs / 60` is the minute-of-day index.
    let now_utc = chrono::Utc::now().timestamp();
    let now_ist = now_utc.saturating_add(IST_UTC_OFFSET_SECONDS_I64);
    let minute_of_day_ist = ((now_ist.rem_euclid(i64::from(SECONDS_PER_DAY))) / 60) as u32;

    if state.try_fire(underlying, minute_of_day_ist) {
        notifier.notify(NotificationEvent::OptionChainFetchFailed {
            underlying: underlying.to_string(),
            attempts_made,
            reason: reason_trunc,
        });
    } else {
        debug!(
            underlying,
            minute_of_day_ist, "option-chain fetch failure suppressed — already fired this minute"
        );
    }
}

/// Coarse error classification for Prometheus label cardinality
/// control. Returns a small finite set of strings so Prometheus
/// doesn't explode on unique error messages.
///
/// HTTP 5xx matching uses explicit status-code substrings — hostile-
/// bug-hunt 2026-05-16 found that the previous
/// `contains("5") && contains("00"|"02"|"03")` rule false-positived
/// on strings like `"got 5 results, attempt 2 of 3"`.
fn classify_error(reason: &str) -> &'static str {
    let lower = reason.to_ascii_lowercase();
    if lower.contains("dh-904") || lower.contains("rate limit") {
        "rate_limit"
    } else if lower.contains("timeout") || lower.contains("timed out") {
        "timeout"
    } else if lower.contains("http 5")
        || lower.contains("status 5")
        || lower.contains("returned 5")
        || lower.contains("code 5")
    {
        // Match status codes ONLY when preceded by a context word
        // (`http`, `status`, `returned`, `code`) so substrings like
        // `"size 500"` don't false-positive. Hostile-bug-hunt
        // 2026-05-16 regression block.
        "server_5xx"
    } else if lower.contains("expiry list returned empty") {
        "empty_expiries"
    } else {
        "other"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        symbol: &str,
        security_id: u32,
        segment: &str,
        slot_sec: u32,
    ) -> OptionChainUnderlyingEntry {
        OptionChainUnderlyingEntry {
            symbol: symbol.to_string(),
            security_id,
            segment: segment.to_string(),
            slot_sec,
        }
    }

    fn locked_three() -> Vec<OptionChainUnderlyingEntry> {
        vec![
            entry("SENSEX", 51, "IDX_I", 53),
            entry("BANKNIFTY", 25, "IDX_I", 56),
            entry("NIFTY", 13, "IDX_I", 59),
        ]
    }

    #[test]
    fn test_compute_next_slot_empty_schedule_returns_none() {
        assert!(compute_next_slot(&[], 0).is_none());
    }

    #[test]
    fn test_compute_next_slot_picks_first_future_in_current_minute() {
        // 09:15:00 IST = 33_300 secs. Next slot is :53 of same minute.
        let now = 33_300;
        let (target, idx) = compute_next_slot(&locked_three(), now).unwrap();
        // First slot for this minute is :53 (SENSEX, idx 0).
        assert_eq!(target, 33_300 + 53);
        assert_eq!(idx, 0);
    }

    #[test]
    fn test_compute_next_slot_picks_middle_slot_when_first_passed() {
        // 09:15:54 IST = 33_354. :53 passed, next is :56 (BANKNIFTY).
        let now = 33_354;
        let (target, idx) = compute_next_slot(&locked_three(), now).unwrap();
        assert_eq!(target, 33_300 + 56);
        assert_eq!(idx, 1);
    }

    #[test]
    fn test_compute_next_slot_picks_last_slot() {
        // 09:15:57 IST = 33_357. :53 + :56 passed, next is :59 (NIFTY).
        let now = 33_357;
        let (target, idx) = compute_next_slot(&locked_three(), now).unwrap();
        assert_eq!(target, 33_300 + 59);
        assert_eq!(idx, 2);
    }

    #[test]
    fn test_compute_next_slot_rolls_to_next_minute() {
        // 09:15:59 IST = 33_359. All three slots passed (well, :59 is
        // strictly greater than 59 so it doesn't fire at exactly :59).
        // Actually 33_359 % 60 == 59, so target > 33_359 means :59 is
        // not future — roll to next minute's :53.
        let now = 33_359;
        let (target, idx) = compute_next_slot(&locked_three(), now).unwrap();
        assert_eq!(target, 33_300 + 60 + 53);
        assert_eq!(idx, 0); // SENSEX :53 of next minute
    }

    #[test]
    fn test_compute_next_slot_respects_unsorted_toml_order() {
        // Even if operator puts entries out of order in TOML, the
        // scheduler walks slots in time order.
        let unsorted = vec![
            entry("NIFTY", 13, "IDX_I", 59),
            entry("SENSEX", 51, "IDX_I", 53),
            entry("BANKNIFTY", 25, "IDX_I", 56),
        ];
        let (target, idx) = compute_next_slot(&unsorted, 33_300).unwrap();
        assert_eq!(target, 33_300 + 53);
        // idx is the position in the ORIGINAL slice, which is 1 (SENSEX).
        assert_eq!(idx, 1);
    }

    #[test]
    fn test_compute_next_slot_single_underlying() {
        let single = vec![entry("NIFTY", 13, "IDX_I", 59)];
        let (target, idx) = compute_next_slot(&single, 33_300).unwrap();
        assert_eq!(target, 33_300 + 59);
        assert_eq!(idx, 0);
    }

    #[test]
    fn test_scheduler_state_edge_trigger_fires_once_per_minute() {
        let cache = SnapshotCache::new();
        let mut state = SchedulerState::new(cache);

        // First fire — should return true (i.e. "send Telegram").
        assert!(state.try_fire("NIFTY", 555));
        // Second fire same minute — should return false (suppressed).
        assert!(!state.try_fire("NIFTY", 555));
        // Different minute — should fire again.
        assert!(state.try_fire("NIFTY", 556));
        // Different underlying same minute — independent.
        assert!(state.try_fire("BANKNIFTY", 555));
    }

    #[test]
    fn test_scheduler_state_edge_trigger_resets_on_new_day() {
        let cache = SnapshotCache::new();
        let mut state = SchedulerState::new(cache);

        // Day 1: fire + suppress.
        let day_1 = 1 * i64::from(SECONDS_PER_DAY) + 33_300;
        state.maybe_reset_edge_trigger(day_1);
        assert!(state.try_fire("NIFTY", 555));
        assert!(!state.try_fire("NIFTY", 555));

        // Day 2: same minute number, but the day rolled over —
        // edge-trigger set should be cleared, so fires again.
        let day_2 = 2 * i64::from(SECONDS_PER_DAY) + 33_300;
        state.maybe_reset_edge_trigger(day_2);
        assert!(state.try_fire("NIFTY", 555));
    }

    #[test]
    fn test_scheduler_state_edge_trigger_does_not_reset_within_day() {
        let cache = SnapshotCache::new();
        let mut state = SchedulerState::new(cache);

        let morning = 1 * i64::from(SECONDS_PER_DAY) + 33_300;
        state.maybe_reset_edge_trigger(morning);
        assert!(state.try_fire("NIFTY", 555));

        // Same day, different time — must NOT reset.
        let afternoon = 1 * i64::from(SECONDS_PER_DAY) + 55_000;
        state.maybe_reset_edge_trigger(afternoon);
        assert!(!state.try_fire("NIFTY", 555), "must remain suppressed");
    }

    #[test]
    fn test_classify_error_buckets_known_classes() {
        assert_eq!(classify_error("DH-904 rate limit"), "rate_limit");
        assert_eq!(classify_error("connection timed out"), "timeout");
        assert_eq!(classify_error("timeout 10s"), "timeout");
        assert_eq!(
            classify_error("expiry list returned empty array"),
            "empty_expiries"
        );
        assert_eq!(classify_error("totally unknown error"), "other");
        // Real HTTP 5xx responses.
        assert_eq!(
            classify_error("HTTP 500 internal server error"),
            "server_5xx"
        );
        assert_eq!(
            classify_error("status 503 service unavailable"),
            "server_5xx"
        );
        assert_eq!(classify_error("API returned 502 bad gateway"), "server_5xx");
    }

    #[test]
    fn test_classify_error_does_not_false_positive_on_digits_in_message() {
        // Hostile-bug-hunt 2026-05-16 regression: the previous rule
        // `contains("5") && (contains("00")|contains("02")|contains("03"))`
        // would have matched these benign messages as `server_5xx`.
        // Explicit-prefix matching now prevents that.
        assert_eq!(
            classify_error("got 5 results, attempt 2 of 3"),
            "other",
            "must NOT match server_5xx on benign '5' + digit substrings"
        );
        assert_eq!(
            classify_error("empty array of size 500"),
            "other",
            "must NOT match server_5xx when '500' appears as a non-status quantity"
        );
        assert_eq!(
            classify_error("waited 5s, retried 3 times, got 0 results"),
            "other",
            "must NOT match server_5xx on '5' + '00' + '03' substring soup"
        );
    }

    #[test]
    fn test_classify_error_label_cardinality_is_bounded() {
        // Even for adversarial inputs, classify_error returns one of
        // a small finite set of labels. Prometheus label cardinality
        // is bounded by this set.
        let labels = [
            "rate_limit",
            "timeout",
            "server_5xx",
            "empty_expiries",
            "other",
        ];
        for input in [
            "asdf",
            "",
            "🔥💥",
            &"a".repeat(10_000),
            "DH-904 hit",
            "request timed out after 30s",
        ] {
            let label = classify_error(input);
            assert!(
                labels.contains(&label),
                "classify_error returned unknown label `{label}` for input `{input}`"
            );
        }
    }

    #[test]
    fn test_metric_constants_stable() {
        // Dashboards + alerts depend on these exact names. Bumping any
        // breaks production observability.
        assert_eq!(METRIC_FETCHES_TOTAL, "tv_option_chain_fetches_total");
        assert_eq!(
            METRIC_FETCH_ERRORS_TOTAL,
            "tv_option_chain_fetch_errors_total"
        );
        assert_eq!(METRIC_CACHE_AGE_SECS, "tv_option_chain_cache_age_secs");
        assert_eq!(
            METRIC_STRATEGY_HALTS_TOTAL,
            "tv_option_chain_strategy_halts_total"
        );
    }

    #[test]
    fn test_telegram_reason_max_len_pinned() {
        // 200 chars is short enough to fit comfortably in a Telegram
        // body but long enough to give the operator real error context.
        // Bumping above ~400 risks pushing the body past Telegram's
        // 4096-char limit on a fully-populated event.
        assert_eq!(TELEGRAM_ERROR_REASON_MAX_LEN, 200);
    }

    // -----------------------------------------------------------------
    // PR #8e — L7 COOLDOWN
    // -----------------------------------------------------------------

    const BASE: u64 = OPTION_CHAIN_FETCH_CADENCE_SECS; // 50

    #[test]
    fn test_cooldown_default_is_zero_failures() {
        let c = OptionChainCooldown::default();
        assert_eq!(c.consecutive_failures, 0);
        assert!(!c.is_active());
    }

    #[test]
    fn test_cooldown_below_threshold_returns_base_cadence() {
        let mut c = OptionChainCooldown::default();
        // 0, 1, 2 failures → base cadence (threshold is 3).
        for _ in 0..OPTION_CHAIN_STALE_CYCLES_BEFORE_TELEGRAM {
            assert_eq!(c.effective_cadence_secs(BASE), BASE);
            assert_eq!(c.extra_cooldown_secs(BASE), 0);
            assert!(!c.is_active());
            c.record_failure();
        }
    }

    #[test]
    fn test_cooldown_ladder_60_90_120() {
        let mut c = OptionChainCooldown::default();
        // Walk to exactly the trigger threshold (3 failures).
        for _ in 0..OPTION_CHAIN_STALE_CYCLES_BEFORE_TELEGRAM {
            c.record_failure();
        }
        // failures == 3 → ladder[0] = 60
        assert_eq!(c.effective_cadence_secs(BASE), 60);
        assert!(c.is_active());
        // failures == 4 → ladder[1] = 90
        c.record_failure();
        assert_eq!(c.effective_cadence_secs(BASE), 90);
        // failures == 5 → ladder[2] = 120
        c.record_failure();
        assert_eq!(c.effective_cadence_secs(BASE), 120);
    }

    #[test]
    fn test_cooldown_saturates_at_final_rung() {
        let mut c = OptionChainCooldown::default();
        for _ in 0..50 {
            c.record_failure();
        }
        // Deep into failure streak — saturates at 120s, no panic.
        assert_eq!(c.effective_cadence_secs(BASE), 120);
        assert_eq!(c.extra_cooldown_secs(BASE), 120 - BASE);
    }

    #[test]
    fn test_cooldown_record_success_resets_streak() {
        let mut c = OptionChainCooldown::default();
        for _ in 0..10 {
            c.record_failure();
        }
        assert!(c.is_active());
        c.record_success();
        assert_eq!(c.consecutive_failures, 0);
        assert!(!c.is_active());
        assert_eq!(c.effective_cadence_secs(BASE), BASE);
        assert_eq!(c.extra_cooldown_secs(BASE), 0);
    }

    #[test]
    fn test_cooldown_extra_secs_is_effective_minus_base() {
        let mut c = OptionChainCooldown::default();
        for _ in 0..OPTION_CHAIN_STALE_CYCLES_BEFORE_TELEGRAM {
            c.record_failure();
        }
        // At 3 failures: effective 60, base 50 → extra 10.
        assert_eq!(c.extra_cooldown_secs(BASE), 60 - BASE);
    }

    #[test]
    fn test_cooldown_record_failure_saturates_no_overflow() {
        let mut c = OptionChainCooldown {
            consecutive_failures: u32::MAX,
        };
        c.record_failure();
        // saturating_add — stays at u32::MAX, no panic.
        assert_eq!(c.consecutive_failures, u32::MAX);
        assert_eq!(c.effective_cadence_secs(BASE), 120);
    }

    #[test]
    fn test_cooldown_failure_then_success_then_failure_cycle() {
        let mut c = OptionChainCooldown::default();
        // 4 failures → cooldown active.
        for _ in 0..4 {
            c.record_failure();
        }
        assert!(c.is_active());
        // One success clears it.
        c.record_success();
        assert!(!c.is_active());
        // A fresh failure streak starts from 0 again.
        for _ in 0..OPTION_CHAIN_STALE_CYCLES_BEFORE_TELEGRAM {
            c.record_failure();
        }
        assert!(c.is_active());
        assert_eq!(c.effective_cadence_secs(BASE), 60);
    }
}

//! Depth-20 dynamic top-150 subscriber (Phase 7 of v3 plan, 2026-04-28).
//!
//! Of the 5 depth-20 connection slots in the Dhan budget:
//! - Slots 1+2 are PINNED to NIFTY ATM±24 + BANKNIFTY ATM±24 (~98 instruments).
//!   These are the static high-value index option chains and are managed by
//!   the existing `depth_rebalancer.rs` (60s ATM-drift swap).
//! - Slots 3/4/5 are DYNAMIC and managed by THIS module: every minute we
//!   query the existing `option_movers` table for the top 150 contracts
//!   sorted by `volume DESC` filtered by `change_pct > 0`, then emit a
//!   `DepthCommand::Swap20` over each slot's mpsc channel containing the
//!   set delta (entrants + leavers) so the existing zero-disconnect
//!   subscribe/unsubscribe path runs.
//!
//! ## Why query `option_movers` and not the new movers_22tf tables?
//!
//! `option_movers` is already populated every 60s by `OptionMoversWriter`
//! and exists in production today. Phase 7 ships BEFORE the 22-tf core
//! (Phases 8-13), so depending on the new `movers_{T}` tables would force
//! Phase 7 to wait. The schema is identical for our needs (`change_pct`,
//! `volume`, `security_id`, `segment`, `ts`).
//!
//! ## Ratchets
//!
//! | # | Test | Pins |
//! |---|---|---|
//! | 48 | `test_depth_20_top_150_selector_returns_at_most_150_rows` | LIMIT 150 enforced |
//! | 49 | `test_depth_20_top_150_selector_filters_change_pct_positive_only` | `WHERE change_pct > 0` (Option B) |
//! | 50 | `test_depth_20_top_150_recompute_interval_is_60_seconds` | constant pinned to 60 |
//! | 51 | `test_depth_20_top_150_delta_swap_emits_correct_unsubscribe_subscribe_pair` | set-diff correctness |
//!
//! ## Boot wiring
//!
//! The runner function `run_depth_20_dynamic_subscriber` is `pub(crate)`
//! and will be called from `crates/app/src/main.rs` in a follow-up commit
//! once the 3 new depth-20 connection slots (3/4/5) are added there.
//! See `.claude/plans/v2-architecture.md` Section I for the boot-wiring
//! design.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tracing::{error, info, warn};

/// Maximum number of contracts to subscribe across the 3 dynamic depth-20
/// slots. Each slot holds 50 contracts (Dhan's per-message + per-connection
/// limit), so 3 × 50 = 150.
pub const DEPTH_20_DYNAMIC_TOP_N: usize = 150;

/// Number of instruments per dynamic depth-20 slot (Dhan's per-message
/// subscribe limit).
pub const DEPTH_20_DYNAMIC_SLOT_CAPACITY: usize = 50;

/// Number of dynamic depth-20 connection slots (slots 3/4/5 of the 5-slot
/// Dhan budget; slots 1+2 stay pinned to NIFTY+BANKNIFTY ATM±24).
pub const DEPTH_20_DYNAMIC_SLOT_COUNT: usize = 3;

/// Recompute interval for the dynamic top-150 selector. Pinned at 60s to
/// match the `option_movers` write cadence (1-minute snapshots) — querying
/// faster than the source updates wastes work without changing results.
pub const DEPTH_20_DYNAMIC_RECOMPUTE_INTERVAL_SECS: u64 = 60;

/// Lower bound on returned count below which the selector emits a
/// `Depth20DynamicTopSetEmpty` Telegram alert. 50 is the per-slot capacity;
/// returning < 50 means at least one of the 3 slots cannot fill.
pub const DEPTH_20_DYNAMIC_EMPTY_THRESHOLD: usize = 50;

/// SQL the dynamic subscriber issues every `DEPTH_20_DYNAMIC_RECOMPUTE_INTERVAL_SECS`
/// against the existing `option_movers` table. Public for ratchet tests
/// and for future MCP-tool query integration.
///
/// The 90-second freshness window (`ts > now() - 90s`) covers the
/// 60s `option_movers` write cadence + 30s grace.
pub const DEPTH_20_DYNAMIC_SELECTOR_SQL: &str = "\
SELECT security_id, segment \
FROM option_movers \
WHERE ts > dateadd('s', -90, now()) AND change_pct > 0 \
ORDER BY volume DESC \
LIMIT 150";

/// A single contract identifier for the dynamic top-150 set. Pairs the
/// numeric `security_id` with the `ExchangeSegment` per I-P1-11
/// composite-key invariant — `security_id` alone is NOT unique.
///
/// Stored as `(u32, char)` rather than `(u32, ExchangeSegment)` because the
/// segment-character form is what `OptionMoversWriter` writes to the
/// `segment` column today. Conversion to/from ExchangeSegment happens at
/// the boundary.
pub type DynamicContractKey = (u32, char);

/// Result of computing the set delta between the previous minute's top-150
/// and the current minute's top-150.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DynamicSwapDelta {
    /// Contracts present in `previous` but missing from `current` — must be
    /// unsubscribed via RequestCode 25.
    pub leavers: Vec<DynamicContractKey>,
    /// Contracts present in `current` but missing from `previous` — must be
    /// subscribed via RequestCode 23.
    pub entrants: Vec<DynamicContractKey>,
    /// Total active contracts after the swap (`current.len()`). For
    /// observability — the operator's "is the slot full?" question.
    pub total_active: usize,
}

/// Pure-function set-diff between two top-150 snapshots. Caller passes the
/// previous and current sets (already deduplicated and sized ≤ 150). This
/// function is hot-path-safe (no allocation beyond the two output Vecs,
/// which the caller can reuse across iterations).
///
/// Sort order of output Vecs is NOT guaranteed — receivers should treat
/// them as unordered.
#[must_use]
pub fn compute_swap_delta(
    previous: &HashSet<DynamicContractKey>,
    current: &HashSet<DynamicContractKey>,
) -> DynamicSwapDelta {
    let leavers: Vec<DynamicContractKey> = previous.difference(current).copied().collect();
    let entrants: Vec<DynamicContractKey> = current.difference(previous).copied().collect();
    DynamicSwapDelta {
        leavers,
        entrants,
        total_active: current.len(),
    }
}

/// Validates a set returned by the selector — used by the runner before
/// emitting a Swap20. Returns `Some(reason)` if the set is too small to
/// cover even one slot, otherwise `None`.
///
/// This is the rising-edge detector for the `Depth20DynamicTopSetEmpty`
/// Telegram alert (audit-findings Rule 4 — edge-triggered alerts).
#[must_use]
pub fn check_set_emptiness(returned: &HashSet<DynamicContractKey>) -> Option<&'static str> {
    if returned.is_empty() {
        Some("zero_results")
    } else if returned.len() < DEPTH_20_DYNAMIC_EMPTY_THRESHOLD {
        Some("below_slot_capacity")
    } else {
        None
    }
}

/// Splits a top-150 set into 3 slot-sized batches of ≤ 50 each, preserving
/// the order an iterator over the HashSet returns (sort order is not
/// preserved — that's a known limitation; if the operator wants
/// deterministic per-slot assignment, the SQL `ORDER BY volume DESC` should
/// be reflected in the iteration order, but HashSet iteration is unordered).
///
/// Currently used only for testing the slot-distribution invariant.
#[must_use]
pub fn split_into_slots(
    contracts: &HashSet<DynamicContractKey>,
) -> [Vec<DynamicContractKey>; DEPTH_20_DYNAMIC_SLOT_COUNT] {
    let mut slots: [Vec<DynamicContractKey>; DEPTH_20_DYNAMIC_SLOT_COUNT] = [
        Vec::with_capacity(DEPTH_20_DYNAMIC_SLOT_CAPACITY),
        Vec::with_capacity(DEPTH_20_DYNAMIC_SLOT_CAPACITY),
        Vec::with_capacity(DEPTH_20_DYNAMIC_SLOT_CAPACITY),
    ];
    for (idx, contract) in contracts.iter().enumerate() {
        let slot = idx / DEPTH_20_DYNAMIC_SLOT_CAPACITY;
        if slot < DEPTH_20_DYNAMIC_SLOT_COUNT {
            slots[slot].push(*contract);
        }
        // Indexes >= 150 are silently dropped by design — the caller already
        // ran `check_set_emptiness` and the SELECT had `LIMIT 150`.
    }
    slots
}

/// Parses a QuestDB `/exec` JSON response body into a HashSet of contract
/// keys. The expected response shape is QuestDB's standard 2-column dataset:
///
/// ```json
/// { "dataset": [[12345, "D"], [67890, "D"], ...] }
/// ```
///
/// Each row carries `[security_id, segment]`. `segment` is a string but we
/// stored `'D'` for NSE_FNO at write time (per `OptionMoversWriter`); here
/// we just take the first character.
///
/// Returns an empty set on malformed JSON; the caller's
/// `check_set_emptiness` will route the empty set to the `Depth20Dyn01`
/// Telegram alert with reason `"zero_results"` or `"below_slot_capacity"`.
///
/// Pure function: no I/O. The HTTP fetch lives in the async runner and
/// passes the body string to this parser.
#[must_use]
pub fn parse_questdb_dataset_json(body: &str) -> HashSet<DynamicContractKey> {
    let parsed: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return HashSet::new(),
    };
    let Some(rows) = parsed.get("dataset").and_then(|v| v.as_array()) else {
        return HashSet::new();
    };
    let mut out: HashSet<DynamicContractKey> = HashSet::with_capacity(DEPTH_20_DYNAMIC_TOP_N);
    for row in rows {
        let Some(arr) = row.as_array() else { continue };
        if arr.len() < 2 {
            continue;
        }
        // QuestDB INT columns serialize as JSON numbers; SYMBOL as strings.
        let Some(sec_id) = arr[0].as_i64() else {
            continue;
        };
        if sec_id <= 0 || sec_id > i64::from(u32::MAX) {
            continue;
        }
        let Some(seg) = arr[1].as_str() else { continue };
        let Some(seg_char) = seg.chars().next() else {
            continue;
        };
        out.insert((sec_id as u32, seg_char));
    }
    out
}

/// Builds the QuestDB `/exec` URL for a given host + http port. Public for
/// tests + main.rs wiring; the async runner uses this once at boot to
/// avoid re-formatting per query.
#[must_use]
pub fn questdb_exec_url(host: &str, http_port: u16) -> String {
    format!("http://{host}:{http_port}/exec")
}

// ---------------------------------------------------------------------------
// Phase 7b-runner — async dynamic-subscriber loop
// ---------------------------------------------------------------------------

/// HTTP timeout for the per-minute QuestDB selector query. Generous (5 s)
/// because a single slow query is preferable to a missed snapshot — the
/// rebalance happens off the hot path.
pub const SELECTOR_QUERY_TIMEOUT_SECS: u64 = 5;

/// Outcome of one selector cycle. Returned by `run_one_selector_cycle` so
/// callers (and tests) can verify each step independently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectorCycleOutcome {
    /// Outside `[09:00, 15:30] IST` — loop continued without query.
    /// Emitted to the falling-edge logger but does not page operator.
    SkippedOffMarketHours,
    /// QuestDB query failed (timeout / 5xx / parse error). Edge-triggered
    /// `Depth20DynamicTopSetEmpty` may fire; previous set retained.
    QueryFailed { reason: String },
    /// Selector returned `< DEPTH_20_DYNAMIC_EMPTY_THRESHOLD` rows.
    /// Edge-triggered alert fires once on rising edge.
    EmptyOrUndersized {
        returned_count: usize,
        reason: &'static str,
    },
    /// Set is healthy — delta computed, Swap20 emitted to slots.
    SwapApplied {
        leavers: usize,
        entrants: usize,
        total_active: usize,
    },
    /// Same set as previous cycle — no Swap20 emitted (no-op path).
    NoChange { total_active: usize },
}

/// Pure-function classifier — given the previous + current set, produces
/// the cycle outcome WITHOUT emitting any Swap20. The runner uses this
/// to decide whether to call its callback. Testable in isolation.
#[must_use]
pub fn classify_cycle_outcome(
    previous: &HashSet<DynamicContractKey>,
    current: &HashSet<DynamicContractKey>,
) -> SelectorCycleOutcome {
    if let Some(reason) = check_set_emptiness(current) {
        return SelectorCycleOutcome::EmptyOrUndersized {
            returned_count: current.len(),
            reason,
        };
    }
    let delta = compute_swap_delta(previous, current);
    if delta.leavers.is_empty() && delta.entrants.is_empty() {
        return SelectorCycleOutcome::NoChange {
            total_active: delta.total_active,
        };
    }
    SelectorCycleOutcome::SwapApplied {
        leavers: delta.leavers.len(),
        entrants: delta.entrants.len(),
        total_active: delta.total_active,
    }
}

/// Edge-triggered alert state for the runner. Mirrors the
/// `currently_stale: HashSet<String>` pattern in `depth_rebalancer.rs`
/// (audit-findings Rule 4) — the runner tracks whether the most-recent
/// cycle was already in the "empty" state, so falling+rising edges
/// each fire exactly once.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EdgeTriggeredEmptyState {
    is_empty_now: bool,
}

impl EdgeTriggeredEmptyState {
    /// Update the state with the latest cycle outcome. Returns
    /// `true` iff a Telegram alert should fire on the rising edge
    /// (was healthy, now empty).
    pub fn observe(&mut self, outcome: &SelectorCycleOutcome) -> bool {
        let now_empty = matches!(outcome, SelectorCycleOutcome::EmptyOrUndersized { .. });
        let rising_edge = now_empty && !self.is_empty_now;
        self.is_empty_now = now_empty;
        rising_edge
    }
}

/// Async runner for the 1-minute dynamic top-150 selector loop. Pure
/// data-flow; the IO callback (`fetch_top_set`) and the swap-emit
/// callback (`emit_swap`) are injected so the runner is testable
/// without QuestDB or mpsc senders.
///
/// Design rationale:
/// - Market-hours gate per audit-findings Rule 3 — outside
///   `[09:00, 15:30] IST` the loop sleeps + clears edge state.
/// - Edge-triggered Telegram via `EdgeTriggeredEmptyState` per Rule 4.
/// - `error!` (not `warn!`) on QueryFailed per Rule 5.
/// - Bounded loop iteration — sleeps `DEPTH_20_DYNAMIC_RECOMPUTE_INTERVAL_SECS`
///   between cycles. Shutdown checked at the start of each iteration.
///
/// The boot-wiring step (Phase 10b-2 follow-up) supplies:
/// - `fetch_top_set` — closure that calls QuestDB `/exec` + parses
///   the response via `parse_questdb_dataset_json`. Real impl uses
///   `reqwest::Client`; tests use deterministic stubs.
/// - `emit_swap` — closure that builds Swap20 messages from the
///   delta (leavers + entrants) and `try_send`s them to the 3 slot
///   mpsc senders. Real impl uses `DepthCommand::Swap20`; tests use
///   counters.
// WIRING-EXEMPT: Phase 10b-2 follow-up wires this into main.rs after the 3 dynamic depth-20 slots are allocated. The runner is fully testable in isolation via the injected callbacks.
pub async fn run_dynamic_subscriber_loop<F, Fut, G>(
    mut fetch_top_set: F,
    mut emit_swap: G,
    is_market_hours: impl Fn() -> bool,
    shutdown: Arc<AtomicBool>,
) where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<HashSet<DynamicContractKey>, String>>,
    G: FnMut(&SelectorCycleOutcome),
{
    let mut previous: HashSet<DynamicContractKey> = HashSet::with_capacity(DEPTH_20_DYNAMIC_TOP_N);
    let mut edge_state = EdgeTriggeredEmptyState::default();
    let interval = Duration::from_secs(DEPTH_20_DYNAMIC_RECOMPUTE_INTERVAL_SECS);

    info!(
        interval_secs = DEPTH_20_DYNAMIC_RECOMPUTE_INTERVAL_SECS,
        top_n = DEPTH_20_DYNAMIC_TOP_N,
        "depth-20 dynamic subscriber loop started"
    );

    loop {
        if shutdown.load(Ordering::Relaxed) {
            info!("depth-20 dynamic subscriber loop shutting down");
            break;
        }

        tokio::time::sleep(interval).await;

        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        // Market-hours gate (audit-findings Rule 3). Outside the window
        // we clear the edge state so the next market-open's first empty
        // detection fires correctly as a rising edge.
        if !is_market_hours() {
            edge_state = EdgeTriggeredEmptyState::default();
            let outcome = SelectorCycleOutcome::SkippedOffMarketHours;
            emit_swap(&outcome);
            continue;
        }

        // Query QuestDB + parse. Failures keep `previous` intact so the
        // next cycle re-tries against the same baseline.
        let current = match fetch_top_set().await {
            Ok(set) => set,
            Err(reason) => {
                let outcome = SelectorCycleOutcome::QueryFailed { reason };
                error!(
                    code = "DEPTH-DYN-01",
                    ?outcome,
                    "depth-20 dynamic top-150 query failed — keeping previous set"
                );
                emit_swap(&outcome);
                continue;
            }
        };

        let outcome = classify_cycle_outcome(&previous, &current);
        let rising_empty_edge = edge_state.observe(&outcome);

        match &outcome {
            SelectorCycleOutcome::EmptyOrUndersized {
                returned_count,
                reason,
            } => {
                if rising_empty_edge {
                    warn!(
                        code = "DEPTH-DYN-01",
                        returned_count,
                        reason,
                        "depth-20 dynamic top-150 set is empty / undersized — rising edge"
                    );
                }
                // Don't update `previous` — keep the last healthy baseline
                // so the next recovery cycle computes a clean delta.
            }
            SelectorCycleOutcome::SwapApplied {
                leavers,
                entrants,
                total_active,
            } => {
                info!(
                    leavers,
                    entrants, total_active, "depth-20 dynamic top-150 swap applied"
                );
                previous = current;
            }
            SelectorCycleOutcome::NoChange { total_active } => {
                info!(total_active, "depth-20 dynamic top-150 unchanged");
                // previous == current already.
            }
            SelectorCycleOutcome::QueryFailed { .. }
            | SelectorCycleOutcome::SkippedOffMarketHours => {
                // Already handled above.
            }
        }

        emit_swap(&outcome);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Phase 7 ratchet 48: the SQL the selector issues MUST clamp at LIMIT 150
    /// (corresponds to the 3 slots × 50 capacity). Pinned via const + SQL
    /// substring check so any change to the constant or query body that
    /// breaks the cap fails the build.
    #[test]
    fn test_depth_20_top_150_selector_returns_at_most_150_rows() {
        assert_eq!(
            DEPTH_20_DYNAMIC_TOP_N, 150,
            "DEPTH_20_DYNAMIC_TOP_N must stay at 150 (3 slots × 50 capacity)"
        );
        assert_eq!(
            DEPTH_20_DYNAMIC_SLOT_CAPACITY * DEPTH_20_DYNAMIC_SLOT_COUNT,
            DEPTH_20_DYNAMIC_TOP_N,
            "slot capacity × slot count must equal top-N"
        );
        assert!(
            DEPTH_20_DYNAMIC_SELECTOR_SQL.contains("LIMIT 150"),
            "selector SQL must clamp at LIMIT 150: {DEPTH_20_DYNAMIC_SELECTOR_SQL}"
        );
    }

    /// Phase 7 ratchet 49: the SQL MUST filter `WHERE change_pct > 0` (Option
    /// B confirmed by Parthiban 2026-04-28). Regressing to "all by volume"
    /// would subscribe to losers, which contradicts the gainers-only design.
    #[test]
    fn test_depth_20_top_150_selector_filters_change_pct_positive_only() {
        assert!(
            DEPTH_20_DYNAMIC_SELECTOR_SQL.contains("change_pct > 0"),
            "selector SQL must filter to gainers only (Option B): \
             {DEPTH_20_DYNAMIC_SELECTOR_SQL}"
        );
        assert!(
            DEPTH_20_DYNAMIC_SELECTOR_SQL.contains("ORDER BY volume DESC"),
            "selector SQL must sort by volume DESC: \
             {DEPTH_20_DYNAMIC_SELECTOR_SQL}"
        );
    }

    /// Phase 7 ratchet 50: the recompute interval is exactly 60s. Faster
    /// would waste work (option_movers writes every 60s); slower would
    /// stale the dynamic top-150.
    #[test]
    fn test_depth_20_top_150_recompute_interval_is_60_seconds() {
        assert_eq!(
            DEPTH_20_DYNAMIC_RECOMPUTE_INTERVAL_SECS, 60,
            "recompute interval must match option_movers cadence (60s)"
        );
    }

    /// Phase 7 ratchet 51: set-diff correctness — given a previous and
    /// current top-set, `compute_swap_delta` must emit `leavers = previous
    /// \\ current` and `entrants = current \\ previous`. This is the
    /// invariant that drives the zero-disconnect Swap20.
    #[test]
    fn test_depth_20_top_150_delta_swap_emits_correct_unsubscribe_subscribe_pair() {
        let mut previous: HashSet<DynamicContractKey> = HashSet::new();
        previous.insert((1001, 'D'));
        previous.insert((1002, 'D'));
        previous.insert((1003, 'D'));
        // Common contract — must NOT appear in either delta vec.
        previous.insert((9999, 'D'));

        let mut current: HashSet<DynamicContractKey> = HashSet::new();
        current.insert((9999, 'D')); // unchanged
        current.insert((2001, 'D')); // entrant
        current.insert((2002, 'D')); // entrant

        let delta = compute_swap_delta(&previous, &current);

        // Leavers: 1001, 1002, 1003 (in previous, not in current)
        assert_eq!(delta.leavers.len(), 3);
        let leaver_ids: HashSet<u32> = delta.leavers.iter().map(|(id, _)| *id).collect();
        assert!(leaver_ids.contains(&1001));
        assert!(leaver_ids.contains(&1002));
        assert!(leaver_ids.contains(&1003));
        assert!(
            !leaver_ids.contains(&9999),
            "9999 unchanged, must NOT be a leaver"
        );

        // Entrants: 2001, 2002 (in current, not in previous)
        assert_eq!(delta.entrants.len(), 2);
        let entrant_ids: HashSet<u32> = delta.entrants.iter().map(|(id, _)| *id).collect();
        assert!(entrant_ids.contains(&2001));
        assert!(entrant_ids.contains(&2002));
        assert!(
            !entrant_ids.contains(&9999),
            "9999 unchanged, must NOT be an entrant"
        );

        // Total active matches current set size.
        assert_eq!(delta.total_active, 3);
    }

    #[test]
    fn test_compute_swap_delta_empty_previous_all_entrants() {
        let previous: HashSet<DynamicContractKey> = HashSet::new();
        let mut current: HashSet<DynamicContractKey> = HashSet::new();
        current.insert((1, 'D'));
        current.insert((2, 'D'));

        let delta = compute_swap_delta(&previous, &current);
        assert_eq!(delta.leavers.len(), 0);
        assert_eq!(delta.entrants.len(), 2);
        assert_eq!(delta.total_active, 2);
    }

    #[test]
    fn test_compute_swap_delta_empty_current_all_leavers() {
        let mut previous: HashSet<DynamicContractKey> = HashSet::new();
        previous.insert((1, 'D'));
        previous.insert((2, 'D'));
        let current: HashSet<DynamicContractKey> = HashSet::new();

        let delta = compute_swap_delta(&previous, &current);
        assert_eq!(delta.leavers.len(), 2);
        assert_eq!(delta.entrants.len(), 0);
        assert_eq!(delta.total_active, 0);
    }

    #[test]
    fn test_compute_swap_delta_identical_sets_no_change() {
        let mut previous: HashSet<DynamicContractKey> = HashSet::new();
        previous.insert((1, 'D'));
        previous.insert((2, 'D'));
        previous.insert((3, 'D'));

        let current = previous.clone();
        let delta = compute_swap_delta(&previous, &current);
        assert_eq!(delta.leavers.len(), 0);
        assert_eq!(delta.entrants.len(), 0);
        assert_eq!(delta.total_active, 3);
    }

    #[test]
    fn test_check_set_emptiness_zero_returns_zero_results_reason() {
        let empty: HashSet<DynamicContractKey> = HashSet::new();
        assert_eq!(check_set_emptiness(&empty), Some("zero_results"));
    }

    #[test]
    fn test_check_set_emptiness_below_threshold_returns_below_slot_capacity() {
        let mut small: HashSet<DynamicContractKey> = HashSet::new();
        for i in 0..(DEPTH_20_DYNAMIC_EMPTY_THRESHOLD - 1) {
            small.insert((i as u32, 'D'));
        }
        assert_eq!(check_set_emptiness(&small), Some("below_slot_capacity"));
    }

    #[test]
    fn test_check_set_emptiness_at_or_above_threshold_returns_none() {
        let mut full: HashSet<DynamicContractKey> = HashSet::new();
        for i in 0..DEPTH_20_DYNAMIC_EMPTY_THRESHOLD {
            full.insert((i as u32, 'D'));
        }
        assert_eq!(check_set_emptiness(&full), None);
    }

    #[test]
    fn test_split_into_slots_distributes_correctly() {
        let mut contracts: HashSet<DynamicContractKey> = HashSet::new();
        for i in 0..150 {
            contracts.insert((i as u32, 'D'));
        }
        let slots = split_into_slots(&contracts);
        let total: usize = slots.iter().map(Vec::len).sum();
        assert_eq!(total, 150, "all 150 contracts must land in some slot");
        for (idx, slot) in slots.iter().enumerate() {
            assert!(
                slot.len() <= DEPTH_20_DYNAMIC_SLOT_CAPACITY,
                "slot {idx} exceeded capacity: {} > {DEPTH_20_DYNAMIC_SLOT_CAPACITY}",
                slot.len()
            );
        }
    }

    #[test]
    fn test_split_into_slots_smaller_set_uses_first_slot_first() {
        let mut contracts: HashSet<DynamicContractKey> = HashSet::new();
        for i in 0..30 {
            contracts.insert((i as u32, 'D'));
        }
        let slots = split_into_slots(&contracts);
        assert_eq!(slots[0].len(), 30, "all 30 must land in slot 0");
        assert_eq!(slots[1].len(), 0);
        assert_eq!(slots[2].len(), 0);
    }

    #[test]
    fn test_dynamic_constants_consistent() {
        // Defensive: if anyone bumps DEPTH_20_DYNAMIC_TOP_N without updating
        // the SQL or vice versa, this catches the drift.
        let extracted_limit_str = "LIMIT 150";
        assert!(
            DEPTH_20_DYNAMIC_SELECTOR_SQL.contains(extracted_limit_str),
            "selector SQL LIMIT must equal DEPTH_20_DYNAMIC_TOP_N ({DEPTH_20_DYNAMIC_TOP_N})"
        );
    }

    /// Phase 7b ratchet: parser handles the canonical QuestDB 2-column
    /// dataset shape.
    #[test]
    fn test_parse_questdb_dataset_json_normal_response() {
        let body = r#"{"query":"...","columns":[{"name":"security_id","type":"INT"},{"name":"segment","type":"SYMBOL"}],"dataset":[[1001,"D"],[1002,"D"],[1003,"D"]],"count":3}"#;
        let parsed = parse_questdb_dataset_json(body);
        assert_eq!(parsed.len(), 3);
        assert!(parsed.contains(&(1001u32, 'D')));
        assert!(parsed.contains(&(1002u32, 'D')));
        assert!(parsed.contains(&(1003u32, 'D')));
    }

    /// Phase 7b ratchet: malformed JSON returns empty set (not panic).
    #[test]
    fn test_parse_questdb_dataset_malformed_json_returns_empty() {
        assert!(parse_questdb_dataset_json("not json").is_empty());
        assert!(parse_questdb_dataset_json("").is_empty());
        assert!(parse_questdb_dataset_json("{}").is_empty());
        assert!(parse_questdb_dataset_json(r#"{"dataset":null}"#).is_empty());
    }

    /// Phase 7b ratchet: rows with missing / invalid security_id are
    /// silently dropped (not crash the loop).
    #[test]
    fn test_parse_questdb_dataset_drops_invalid_rows() {
        let body = r#"{"dataset":[[1001,"D"],[null,"D"],[-5,"D"],[0,"D"],[1002,"D"]]}"#;
        let parsed = parse_questdb_dataset_json(body);
        // Only 1001 and 1002 are valid (positive non-zero u32).
        assert_eq!(parsed.len(), 2);
        assert!(parsed.contains(&(1001u32, 'D')));
        assert!(parsed.contains(&(1002u32, 'D')));
    }

    /// Phase 7b ratchet: dedup by (security_id, segment) per I-P1-11 — if
    /// the same row appears twice the HashSet collapses it.
    #[test]
    fn test_parse_questdb_dataset_dedupes_duplicates() {
        let body = r#"{"dataset":[[1001,"D"],[1001,"D"],[1001,"D"]]}"#;
        let parsed = parse_questdb_dataset_json(body);
        assert_eq!(parsed.len(), 1);
        assert!(parsed.contains(&(1001u32, 'D')));
    }

    /// Phase 7b ratchet: u32 max boundary handled cleanly (no overflow).
    #[test]
    fn test_parse_questdb_dataset_u32_max_boundary() {
        let max_u32 = u32::MAX as u64;
        let body = format!(r#"{{"dataset":[[{max_u32},"D"]]}}"#);
        let parsed = parse_questdb_dataset_json(&body);
        assert_eq!(parsed.len(), 1);
        assert!(parsed.contains(&(u32::MAX, 'D')));
    }

    /// Phase 7b ratchet: questdb_exec_url assembly format.
    #[test]
    fn test_questdb_exec_url_format() {
        assert_eq!(
            questdb_exec_url("tv-questdb", 9000),
            "http://tv-questdb:9000/exec"
        );
        assert_eq!(
            questdb_exec_url("127.0.0.1", 9000),
            "http://127.0.0.1:9000/exec"
        );
    }

    /// Phase 7b-runner ratchet: classifier produces SwapApplied when
    /// previous != current and current is healthy.
    #[test]
    fn test_classify_cycle_outcome_swap_applied() {
        let mut prev: HashSet<DynamicContractKey> = HashSet::new();
        for i in 0..50 {
            prev.insert((i, 'D'));
        }
        let mut curr: HashSet<DynamicContractKey> = HashSet::new();
        for i in 25..75 {
            curr.insert((i, 'D'));
        }
        let outcome = classify_cycle_outcome(&prev, &curr);
        assert_eq!(
            outcome,
            SelectorCycleOutcome::SwapApplied {
                leavers: 25,
                entrants: 25,
                total_active: 50,
            }
        );
    }

    /// Phase 7b-runner ratchet: classifier produces NoChange when
    /// previous == current.
    #[test]
    fn test_classify_cycle_outcome_no_change_when_identical() {
        let mut prev: HashSet<DynamicContractKey> = HashSet::new();
        for i in 0..50 {
            prev.insert((i, 'D'));
        }
        let curr = prev.clone();
        let outcome = classify_cycle_outcome(&prev, &curr);
        assert_eq!(outcome, SelectorCycleOutcome::NoChange { total_active: 50 });
    }

    /// Phase 7b-runner ratchet: classifier produces EmptyOrUndersized
    /// when current.len() < threshold (50).
    #[test]
    fn test_classify_cycle_outcome_empty_when_below_threshold() {
        let prev: HashSet<DynamicContractKey> = HashSet::new();
        let mut curr: HashSet<DynamicContractKey> = HashSet::new();
        for i in 0..30 {
            curr.insert((i, 'D'));
        }
        let outcome = classify_cycle_outcome(&prev, &curr);
        assert_eq!(
            outcome,
            SelectorCycleOutcome::EmptyOrUndersized {
                returned_count: 30,
                reason: "below_slot_capacity",
            }
        );
    }

    /// Phase 7b-runner ratchet: classifier produces EmptyOrUndersized
    /// with `zero_results` when current is fully empty.
    #[test]
    fn test_classify_cycle_outcome_empty_when_zero_results() {
        let prev: HashSet<DynamicContractKey> = HashSet::new();
        let curr: HashSet<DynamicContractKey> = HashSet::new();
        let outcome = classify_cycle_outcome(&prev, &curr);
        assert_eq!(
            outcome,
            SelectorCycleOutcome::EmptyOrUndersized {
                returned_count: 0,
                reason: "zero_results",
            }
        );
    }

    /// Phase 7b-runner ratchet: edge-triggered alert fires ONCE on rising
    /// edge from healthy → empty, NOT on subsequent empty cycles.
    #[test]
    fn test_edge_triggered_empty_state_fires_once_on_rising_edge() {
        let mut state = EdgeTriggeredEmptyState::default();

        // Cycle 1: healthy → no alert
        let healthy = SelectorCycleOutcome::SwapApplied {
            leavers: 10,
            entrants: 10,
            total_active: 50,
        };
        assert!(!state.observe(&healthy));

        // Cycle 2: empty → rising edge → ALERT
        let empty = SelectorCycleOutcome::EmptyOrUndersized {
            returned_count: 0,
            reason: "zero_results",
        };
        assert!(state.observe(&empty), "rising edge must fire");

        // Cycle 3: still empty → no alert (already alerted)
        assert!(!state.observe(&empty), "sustained empty must not re-fire");

        // Cycle 4: empty → still no alert
        assert!(!state.observe(&empty));

        // Cycle 5: healthy → no alert
        assert!(!state.observe(&healthy));

        // Cycle 6: empty again → rising edge → ALERT
        assert!(state.observe(&empty), "second rising edge must fire");
    }

    /// Phase 7b-runner ratchet: SELECTOR_QUERY_TIMEOUT_SECS pinned at 5s.
    #[test]
    fn test_selector_query_timeout_is_5_seconds() {
        assert_eq!(SELECTOR_QUERY_TIMEOUT_SECS, 5);
    }

    /// Phase 7b-runner ratchet: SkippedOffMarketHours and QueryFailed
    /// outcomes do NOT trip the empty-state edge.
    #[test]
    fn test_edge_state_does_not_fire_on_skip_or_query_failure() {
        let mut state = EdgeTriggeredEmptyState::default();
        assert!(!state.observe(&SelectorCycleOutcome::SkippedOffMarketHours));
        assert!(!state.observe(&SelectorCycleOutcome::QueryFailed {
            reason: "test".to_string(),
        }));
    }

    /// Phase 7b-runner ratchet: the loop runs at least once with a
    /// market-hours gate that always returns false (off-market) and
    /// shuts down on the second iteration. Verifies the off-hours
    /// path emits SkippedOffMarketHours via the callback.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_run_dynamic_subscriber_loop_off_hours_emits_skipped() {
        use std::sync::Mutex;
        let outcomes: Arc<Mutex<Vec<SelectorCycleOutcome>>> = Arc::new(Mutex::new(Vec::new()));
        let outcomes_clone = Arc::clone(&outcomes);
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = Arc::clone(&shutdown);

        let fetch = move || async move {
            // Should never be called when off-hours.
            unreachable!("fetch should not run during off-market hours");
        };
        let emit = move |outcome: &SelectorCycleOutcome| {
            outcomes_clone.lock().unwrap().push(outcome.clone());
            // Trigger shutdown after the first outcome so the loop exits.
            shutdown_clone.store(true, Ordering::Relaxed);
        };

        let runner = run_dynamic_subscriber_loop(
            fetch,
            emit,
            || false, // always off-hours
            Arc::clone(&shutdown),
        );

        tokio::time::timeout(Duration::from_secs(120), runner)
            .await
            .expect("loop should exit on shutdown");

        let captured = outcomes.lock().unwrap();
        assert!(!captured.is_empty(), "at least one cycle should run");
        assert!(matches!(
            captured[0],
            SelectorCycleOutcome::SkippedOffMarketHours
        ));
    }
}

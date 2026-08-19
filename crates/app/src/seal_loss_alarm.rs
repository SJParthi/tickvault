//! The one place a sealed candle is declared LOST, and the page that must
//! accompany it.
//!
//! ## Why this module exists
//!
//! The 2026-08-19 no-drop work routed every refused seal to the spill and then
//! the DLQ, so a candle can only be lost when BOTH disk tiers refuse. That was
//! the right change and it is not the whole story: the `Lost` arm returned a
//! typed value and **logged nothing**, so the operator's only signal was
//! `tv_dhan_feed_seals_dropped_total` — which the Dhan noise lock (§2.3a)
//! records as *visible but unpageable*.
//!
//! A same-day hostile audit found the case that makes that fatal. If
//! `SealWriterRunner::new` fails at boot, `main.rs` installs **neither** the
//! seal sender **nor** the overflow escalator: one boot `error!`, then nothing.
//! Every seal for the rest of the session then takes the no-tier path and
//! returns `Lost`. The alarmed drain counter
//! (`tv_seal_writer_drain_total{kind="dropped"}`) lives inside the writer loop
//! that never spawned, so it reads a flat, healthy **zero** all day while an
//! entire session's candles evaporate.
//!
//! That is the exact false-OK shape the charter's Rule 11 forbids, and it was
//! introduced by the very change that was supposed to end silent seal loss.
//!
//! ## What this module guarantees
//!
//! A lost seal ALWAYS produces a coded `AGGREGATOR-DROP-01` (Critical, and
//! alarmed via the error-code metric filter). Never a bare counter.
//!
//! ## Why the log is throttled, and why the counter is not
//!
//! The two loss modes have opposite shapes. Both disk tiers refusing is rare
//! and bursty; the missing-tier case fires on EVERY seal — at ~4,565
//! instruments across 13 emitted timeframes that is tens of thousands of lines
//! per minute, which would bury the very page it is trying to raise and could
//! fill the disk this repo just spent a PR protecting.
//!
//! So the LOG is throttled to powers of two (the `indicator::engine` idiom
//! already used in this workspace) and the COUNTER is incremented every time.
//! The metric stays exact for alarming and arithmetic; the log stays readable
//! and carries the running total, so a throttled line still tells the operator
//! the true magnitude rather than implying it happened once.

use std::sync::atomic::{AtomicU64, Ordering};

use tickvault_common::error_code::ErrorCode;
use tracing::error;

/// Why a sealed candle could not be made durable.
///
/// Kept as two distinct variants rather than one because the operator actions
/// are completely different: one is a full or failing disk, the other is a
/// process that booted wrong and must be restarted. A single "seal lost"
/// message would send the operator to inspect a disk that is fine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SealLossReason {
    /// No durable tier was installed — `SealWriterRunner::new` failed at boot,
    /// so neither the seal sender nor the overflow escalator exists. EVERY
    /// seal is lost for the lifetime of the process. Restart is the fix; the
    /// data for this session is not recoverable from the candle path.
    NoDurableTier,
    /// The spill AND the DLQ both refused the write. The data volume is
    /// unwritable — this is the case `AGGREGATOR-DROP-01` was written for.
    BothDiskTiersFailed,
}

impl SealLossReason {
    /// Plain-English cause, for the log line's `cause` field.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoDurableTier => "no durable tier installed (seal writer failed to boot)",
            Self::BothDiskTiersFailed => "spill and DLQ both refused the write",
        }
    }

    /// What the operator should do. Carried in the log because an operator
    /// reading a Critical at 09:20 should not have to open a runbook to learn
    /// which of two unrelated problems they have.
    #[must_use]
    pub const fn operator_action(self) -> &'static str {
        match self {
            Self::NoDurableTier => {
                "the seal writer did not start this boot — restart the process; \
                 candles for this session are NOT recoverable from the candle path"
            }
            Self::BothDiskTiersFailed => {
                "the data volume is refusing writes — free space or fix the mount; \
                 raw frames may still be recoverable from the write-ahead log"
            }
        }
    }
}

/// Running total of seals lost this process, per reason.
///
/// Separate counters so the throttle of one loss mode cannot suppress the
/// first line of the other — a disk failure that begins after a boot failure
/// would otherwise be invisible until the combined count next hit a power of
/// two, which could be thousands of seals later.
static NO_TIER_LOST: AtomicU64 = AtomicU64::new(0);
static BOTH_TIERS_LOST: AtomicU64 = AtomicU64::new(0);

/// Record one lost sealed candle and page for it.
///
/// Returns the running total for this reason, which the caller may use for
/// its own accounting and which the tests assert on.
///
/// The `security_id` and `exchange_segment_code` identify WHICH instrument
/// lost a bar. That matters more than it looks: the missing-tier case affects
/// every instrument equally, while a disk failure mid-session may show a
/// pattern, and the difference is diagnostic.
pub fn record_lost_seal(
    reason: SealLossReason,
    security_id: u64,
    exchange_segment_code: u8,
    timeframe: &'static str,
) -> u64 {
    let slot = match reason {
        SealLossReason::NoDurableTier => &NO_TIER_LOST,
        SealLossReason::BothDiskTiersFailed => &BOTH_TIERS_LOST,
    };
    // `fetch_add` returns the PREVIOUS value; +1 makes `total` the count
    // including this loss, so the first loss logs "total=1" rather than 0.
    let total = slot.fetch_add(1, Ordering::Relaxed).saturating_add(1);

    // Powers of two: 1, 2, 4, 8, … — the first loss always logs, and the rate
    // decays logarithmically so a storm cannot flood the sink. `total` is
    // carried in the line so a throttled message still states the true
    // magnitude instead of implying a single event.
    if total.is_power_of_two() {
        error!(
            code = ErrorCode::AggregatorDrop01.code_str(),
            security_id,
            exchange_segment_code,
            timeframe,
            cause = reason.as_str(),
            seals_lost_total = total,
            action = reason.operator_action(),
            "sealed candle LOST — it is not in the database and not on disk. This log is \
             throttled to powers of two; seals_lost_total is the true running count."
        );
    }
    total
}

/// Seals lost so far because no durable tier was installed. Test/observability
/// read-out; never resets.
#[must_use]
pub fn no_durable_tier_lost() -> u64 {
    NO_TIER_LOST.load(Ordering::Relaxed)
}

/// Seals lost so far because both disk tiers refused. Test/observability
/// read-out; never resets.
#[must_use]
pub fn both_disk_tiers_lost() -> u64 {
    BOTH_TIERS_LOST.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The loss counters are process-global `AtomicU64`s, so any two tests
    /// that assert on a DELTA will race under the default parallel test
    /// runner — one test's increments land inside another's before/after
    /// window. Serialising them is the honest fix (the same shared-`Mutex`
    /// pattern `tv_api_token_prod_guard` uses for its process-global env
    /// var); weakening the assertions to `>=` would hide a real drift
    /// between the counter and its read-out.
    ///
    /// Poison-safe: a panicking test must not cascade into every sibling.
    static COUNTER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_counters() -> std::sync::MutexGuard<'static, ()> {
        COUNTER_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn test_record_lost_seal_counts_every_loss_not_just_the_logged_ones() {
        let _guard = lock_counters();
        // The counter must be exact even though the log is throttled —
        // alarming and arithmetic depend on the metric, not the line.
        let before = both_disk_tiers_lost();
        for _ in 0..10 {
            record_lost_seal(SealLossReason::BothDiskTiersFailed, 13, 0, "M1");
        }
        assert_eq!(
            both_disk_tiers_lost() - before,
            10,
            "every loss must be counted; throttling applies to the LOG only"
        );
    }

    #[test]
    fn test_record_lost_seal_returns_running_total_including_this_loss() {
        let _guard = lock_counters();
        let before = no_durable_tier_lost();
        let first = record_lost_seal(SealLossReason::NoDurableTier, 25, 0, "M1");
        assert_eq!(
            first,
            before + 1,
            "the first loss must report total=1, not 0 — an operator reading `total=0` \
             beside a loss message would reasonably conclude nothing was lost"
        );
    }

    #[test]
    fn test_the_two_loss_reasons_count_independently() {
        let _guard = lock_counters();
        // A disk failure that starts AFTER a boot failure must be able to log
        // its own first line. A shared counter would suppress it until the
        // combined total next hit a power of two — potentially thousands of
        // seals later.
        let tier_before = no_durable_tier_lost();
        let disk_before = both_disk_tiers_lost();
        record_lost_seal(SealLossReason::NoDurableTier, 51, 0, "M1");
        assert_eq!(
            both_disk_tiers_lost(),
            disk_before,
            "one reason's losses must not advance the other's counter"
        );
        assert_eq!(no_durable_tier_lost(), tier_before + 1);
    }

    #[test]
    fn test_seal_loss_reason_strings_name_the_cause_and_the_action() {
        // The operator actions are genuinely different — one is a restart, the
        // other is a disk. A shared message would send them to the wrong place.
        for reason in [
            SealLossReason::NoDurableTier,
            SealLossReason::BothDiskTiersFailed,
        ] {
            assert!(!reason.as_str().is_empty());
            assert!(!reason.operator_action().is_empty());
        }
        assert_ne!(
            SealLossReason::NoDurableTier.operator_action(),
            SealLossReason::BothDiskTiersFailed.operator_action(),
            "a restart and a full disk must not share an operator action"
        );
        assert!(
            SealLossReason::NoDurableTier
                .operator_action()
                .contains("restart"),
            "the boot-failure case must tell the operator to restart"
        );
    }

    #[test]
    fn test_no_durable_tier_lost_and_both_disk_tiers_lost_read_back_the_totals() {
        let _guard = lock_counters();
        // These accessors are the only way anything outside this module can
        // see the loss totals, so they must track the recorded losses exactly.
        // A read-out that drifts from the counter would be a false-OK of its
        // own: the operator would read a reassuring number while the real
        // count climbed.
        let tier_before = no_durable_tier_lost();
        let disk_before = both_disk_tiers_lost();

        record_lost_seal(SealLossReason::NoDurableTier, 13, 0, "M1");
        record_lost_seal(SealLossReason::NoDurableTier, 25, 0, "M1");
        record_lost_seal(SealLossReason::BothDiskTiersFailed, 51, 0, "M1");

        assert_eq!(no_durable_tier_lost(), tier_before + 2);
        assert_eq!(both_disk_tiers_lost(), disk_before + 1);
    }

    #[test]
    fn test_throttle_fires_on_powers_of_two_only() {
        // Pure check of the predicate the throttle uses, so the intent is
        // pinned even if the logging macro is refactored away from it.
        let logged: Vec<u64> = (1u64..=16).filter(|n| n.is_power_of_two()).collect();
        assert_eq!(
            logged,
            vec![1, 2, 4, 8, 16],
            "the first loss must always log, then decay logarithmically"
        );
    }
}

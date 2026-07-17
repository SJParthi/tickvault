//! Fail-closed order-readiness gate for the LIVE order path.
//!
//! Cold path — the per-order gate is an O(1) RAM read of a cached probe
//! snapshot (RAM-first; NEVER a REST call per order). A background refresher
//! runs the EXISTING pre-market check (dataPlan Active / Derivative segment /
//! >4h token headroom) at 08:45 IST and every 900s in-session, snapshotting the
//! verdict into an atomic state. The dry-run / paper path never consults this
//! gate; a live order with no fresh, valid snapshot is REFUSED.
//!
//! Decoupling: to keep the trading crate free of a production dependency on
//! `tickvault-core` (which it carries only as a dev-dependency — the same
//! reason `TokenProvider` is a trait), the profile check is abstracted behind
//! the `ReadinessProbe` trait. The app-side seam owner implements it over
//! `TokenManager::pre_market_check()` + `seconds_until_expiry()` — the trading
//! crate NEVER re-implements the 3 checks and NEVER mints a token.
//!
//! Ground truth: `dhan-ref/02-authentication.md` rule 10;
//! `.claude/rules/project/order-readiness-error-codes.md`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::task::{JoinError, JoinHandle};
use tracing::{error, info, warn};

use tickvault_common::constants::{
    ORDER_READINESS_MAX_AGE_SECS, ORDER_READINESS_PREMARKET_TRIGGER_SECS_OF_DAY_IST,
    ORDER_READINESS_REFRESH_INTERVAL_SECS, ORDER_READINESS_REFRESHER_RESPAWN_BACKOFF_SECS,
    ORDER_TOKEN_HEADROOM_MIN_SECS, SECONDS_PER_DAY, TICK_PERSIST_END_SECS_OF_DAY_IST,
};
use tickvault_common::error_code::ErrorCode;

/// SeqCst everywhere — cold path. The refresher is the primary writer; the
/// engine's DATA-806 arm is a SECOND writer of `profile_ok` only
/// (`poison_profile`). Correctness (R17, corrected): freshness (`last_ok`) is
/// written LAST and read FIRST, and the headroom + its stamp are a SINGLE
/// packed `AtomicU64` (F-B fix — a reader can never observe an OLD headroom
/// with a NEW stamp and spuriously PASS near the 4h boundary), so a torn read
/// can only produce a spurious REFUSAL, never a spurious pass. The one benign
/// relaxation: a DATA-806 `poison_profile` racing a concurrently-completing OK
/// refresh is last-writer-wins on `profile_ok`; a poisoned gate may re-open one
/// probe early, which self-heals on the next probe (R16 — dataPlan does not
/// truly gate Trading APIs). This is documented, not relied upon for
/// duplicate-order safety.
const ORD: Ordering = Ordering::SeqCst;

/// The outcome of one readiness probe (produced by the app-side `ReadinessProbe`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeOutcome {
    /// `dataPlan == Active && activeSegment contains Derivative && > 4h headroom`
    /// at probe time (the pre-market check returned `Ok`).
    pub profile_ok: bool,
    /// `seconds_until_expiry()` snapshot at probe time.
    pub token_headroom_secs: u64,
}

/// Boxed probe future (keeps the trait dyn-compatible without `async-trait`).
pub type ProbeFuture<'a> = Pin<Box<dyn Future<Output = ProbeOutcome> + Send + 'a>>;

/// Seam trait: the app-side implementor bridges to the EXISTING
/// `TokenManager::pre_market_check()` + `seconds_until_expiry()`. The trading
/// crate NEVER references `tickvault-core` in production code (the
/// `TokenProvider` decoupling precedent).
pub trait ReadinessProbe: Send + Sync {
    /// Run one readiness probe: pre-market check + token-headroom snapshot.
    fn probe(&self) -> ProbeFuture<'_>;
}

/// Fail-closed atomic readiness snapshot. All zeros = never probed = REFUSE.
pub struct OrderReadinessState {
    /// dataPlan Active && Derivative && >4h at the last probe.
    profile_ok: AtomicBool,
    /// Epoch seconds of the last OK probe (0 = never OK). Written LAST.
    last_ok_epoch_s: AtomicU64,
    /// Epoch seconds of the last probe attempt (0 = refresher never ran).
    last_attempt_epoch_s: AtomicU64,
    /// Packed `(headroom_secs << 32) | stamped_epoch_s` — the `seconds_until_expiry()`
    /// snapshot AND the epoch it was taken at, stored as ONE atomic so the
    /// per-order decay pair is always read/written together (F-B fix: no torn
    /// OLD-headroom + NEW-stamp spurious PASS). Both halves are clamped to
    /// `u32::MAX`; epoch seconds fit in u32 until 2106 and token headroom (≤24h)
    /// fits trivially.
    headroom_packed: AtomicU64,
    /// ORDER-READY-01 gate edge-latch (loud once per refusal episode).
    refusal_latched: AtomicBool,
}

/// Pack a `(headroom_secs, stamped_epoch_s)` pair into one `u64`; both halves
/// clamp to `u32::MAX`.
const fn pack_headroom(headroom_secs: u64, stamped_epoch_s: u64) -> u64 {
    const U32_MAX: u64 = u32::MAX as u64;
    let h = if headroom_secs > U32_MAX {
        U32_MAX
    } else {
        headroom_secs
    };
    let s = if stamped_epoch_s > U32_MAX {
        U32_MAX
    } else {
        stamped_epoch_s
    };
    (h << 32) | s
}

/// Unpack a packed headroom word into `(headroom_secs, stamped_epoch_s)`.
const fn unpack_headroom(packed: u64) -> (u64, u64) {
    (packed >> 32, packed & 0xFFFF_FFFF)
}

impl Default for OrderReadinessState {
    fn default() -> Self {
        Self::new()
    }
}

impl OrderReadinessState {
    /// A fresh, fail-closed state (all zeros ⇒ live orders refused until the
    /// first successful probe).
    #[must_use]
    pub fn new() -> Self {
        Self {
            profile_ok: AtomicBool::new(false),
            last_ok_epoch_s: AtomicU64::new(0),
            last_attempt_epoch_s: AtomicU64::new(0),
            headroom_packed: AtomicU64::new(0),
            refusal_latched: AtomicBool::new(false),
        }
    }

    /// DATA-806 cross-poison: flip the profile NOT-OK until the next OK probe
    /// restores it (R16). Called from the engine's 806 arm.
    pub fn poison_profile(&self) {
        self.profile_ok.store(false, ORD);
    }

    /// Returns `true` on the RISING edge of a gate refusal (was clear → now
    /// latched) so the caller alerts exactly once per episode.
    pub(crate) fn latch_refusal(&self) -> bool {
        !self.refusal_latched.swap(true, ORD)
    }

    /// Clear the refusal latch (called on the next OK refresh). Returns `true`
    /// if a refusal episode was in progress.
    pub(crate) fn clear_refusal_latch(&self) -> bool {
        self.refusal_latched.swap(false, ORD)
    }
}

/// The reason a live order was refused by the readiness gate. First failure
/// wins (see `evaluate_order_readiness`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadinessRefusal {
    /// The refresher never produced an OK verdict (or is not wired).
    NeverProbed,
    /// The last OK verdict is older than the staleness bound.
    Stale {
        /// Age of the last OK probe, in seconds.
        age_secs: u64,
    },
    /// The last probe found dataPlan/segment invalid (or DATA-806 poison).
    ProfileInvalid,
    /// The decayed token headroom fell below the minimum.
    TokenHeadroomLow {
        /// Effective (decayed) headroom seconds.
        effective_secs: u64,
    },
}

impl ReadinessRefusal {
    /// Stable metric/log slug.
    #[must_use]
    pub const fn slug(&self) -> &'static str {
        match self {
            Self::NeverProbed => "no_probe",
            Self::Stale { .. } => "stale",
            Self::ProfileInvalid => "profile_invalid",
            Self::TokenHeadroomLow { .. } => "token_headroom",
        }
    }
}

/// PURE per-order gate. O(1), atomics + saturating arithmetic, NO I/O
/// (RAM-first). Evaluation order, first failure wins: probed → fresh
/// (`now − last_ok ≤ MAX_AGE`) → profile_ok → effective headroom
/// (`headroom − (now − stamped) ≥ MIN`, strict `<` refuses). All arithmetic is
/// `saturating_*` (u64-overflow-proof).
///
/// # Errors
/// Returns the first `ReadinessRefusal` that applies; `Ok(())` means the order
/// may proceed on the live path.
pub fn evaluate_order_readiness(
    state: &OrderReadinessState,
    now_epoch_s: u64,
) -> Result<(), ReadinessRefusal> {
    // Freshness FIRST (R17): a torn write can only make us refuse, never pass.
    let last_attempt = state.last_attempt_epoch_s.load(ORD);
    let last_ok = state.last_ok_epoch_s.load(ORD);
    if last_attempt == 0 || last_ok == 0 {
        return Err(ReadinessRefusal::NeverProbed);
    }
    let age = now_epoch_s.saturating_sub(last_ok);
    if age > ORDER_READINESS_MAX_AGE_SECS {
        return Err(ReadinessRefusal::Stale { age_secs: age });
    }
    if !state.profile_ok.load(ORD) {
        return Err(ReadinessRefusal::ProfileInvalid);
    }
    let (headroom, stamped) = unpack_headroom(state.headroom_packed.load(ORD));
    let elapsed = now_epoch_s.saturating_sub(stamped);
    let effective = headroom.saturating_sub(elapsed);
    if effective < ORDER_TOKEN_HEADROOM_MIN_SECS {
        return Err(ReadinessRefusal::TokenHeadroomLow {
            effective_secs: effective,
        });
    }
    Ok(())
}

/// Strict, delimiter-aware check that a Dhan `activeSegment` string GRANTS the
/// Derivative segment. Unlike a naive `activeSegment.contains("D")` (which
/// fail-OPENS on tokens like `"DISABLED"` or `"CURRENCY_D_LEG"`), this splits on
/// the documented list delimiters and requires an EXACT token match against
/// `"D"` or `"Derivative"` (case-insensitive on the token, never a substring).
///
/// Provided for the app-side seam owner: the `ReadinessProbe` implementation
/// bridging `TokenManager::pre_market_check()` MUST use this instead of a
/// `contains` substring test (the fail-OPEN bug flagged in
/// `token_manager.rs::pre_market_check` — a Cluster D auth-gate follow-up).
// WIRING-EXEMPT: seam-handoff helper — the app-side probe owns the call site;
// exercised here by segment_has_derivative unit tests.
#[must_use]
pub fn segment_has_derivative(active_segment: &str) -> bool {
    active_segment
        .split(|c: char| c == ',' || c == ';' || c == '|' || c.is_whitespace())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .any(|t| t.eq_ignore_ascii_case("D") || t.eq_ignore_ascii_case("Derivative"))
}

/// PURE state-mutation core (exhaustively unit-tested). Writes `last_ok` LAST
/// (R17); a non-OK probe advances everything EXCEPT `last_ok` (so the verdict
/// goes `ProfileInvalid` while fresh, then `Stale` naturally).
pub(crate) fn apply_probe_outcome(
    state: &OrderReadinessState,
    outcome: ProbeOutcome,
    now_epoch_s: u64,
) {
    // Reserve 0 for "never" so a probe exactly at epoch 0 is still "attempted".
    let stamp = now_epoch_s.max(1);
    state.last_attempt_epoch_s.store(stamp, ORD);
    // Headroom + its stamp as ONE atomic store (F-B: the decay pair is never
    // torn). Stamped uses the raw `now_epoch_s` (not `stamp`) to preserve the
    // exact per-order decay base.
    state
        .headroom_packed
        .store(pack_headroom(outcome.token_headroom_secs, now_epoch_s), ORD);
    state.profile_ok.store(outcome.profile_ok, ORD);
    if outcome.profile_ok {
        // Written LAST — readers that saw this already saw profile_ok=true.
        state.last_ok_epoch_s.store(stamp, ORD);
    }
}

fn now_epoch_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One probe cycle: run the app-side probe, record it, log the readiness edge,
/// emit metrics. Returns whether the snapshot is now READY. ANY probe failure
/// ⇒ not-ready (fail-closed; the refusal is non-destructive, so this is
/// deliberately blunter than the watchdog's transient split).
// FINANCIAL-TEST-EXEMPT: async I/O shell; the decision math is covered by the
// evaluate_order_readiness + apply_probe_outcome boundary/proptest suites.
pub async fn refresh_order_readiness(
    probe: &dyn ReadinessProbe,
    state: &OrderReadinessState,
) -> bool {
    let now = now_epoch_s();
    let was_ready = evaluate_order_readiness(state, now).is_ok();
    let outcome = probe.probe().await;
    apply_probe_outcome(state, outcome, now);
    let now_ready = evaluate_order_readiness(state, now).is_ok();

    metrics::counter!(
        "tv_order_readiness_probes_total",
        "outcome" => if now_ready { "ok" } else { "failed" },
    )
    .increment(1);
    metrics::gauge!("tv_order_readiness").set(if now_ready { 1.0 } else { 0.0 });

    // Edge-triggered (audit-findings Rule 4): loud only on the transition.
    if was_ready && !now_ready {
        error!(
            code = ErrorCode::OrderReady01GateRefused.code_str(),
            stage = "refresh_failed",
            "the broker order-readiness probe went NOT-READY"
        );
    } else if !was_ready && now_ready {
        state.clear_refusal_latch();
        info!(
            stage = "refresh_recovered",
            "the broker order-readiness probe recovered to READY"
        );
    }
    now_ready
}

/// PURE schedule decision (unit-tested boundaries). Seconds to wait before the
/// next probe: before 08:45 IST → sleep to 08:45; in `[08:45, 15:30)` → 0
/// (probe now); after close → sleep to the next day's 08:45. Uses secs-of-day
/// comparison, never `is_within_market_hours*(now())` (banned Cat-11).
#[must_use]
pub fn next_readiness_probe_wait_secs(now_ist_secs_of_day: u32) -> u64 {
    let trigger = ORDER_READINESS_PREMARKET_TRIGGER_SECS_OF_DAY_IST;
    // 15:30 IST — reuse the persist-window close (same wall-clock instant).
    let close = TICK_PERSIST_END_SECS_OF_DAY_IST;
    if now_ist_secs_of_day < trigger {
        u64::from(trigger - now_ist_secs_of_day)
    } else if now_ist_secs_of_day < close {
        0
    } else {
        u64::from(SECONDS_PER_DAY - now_ist_secs_of_day) + u64::from(trigger)
    }
}

/// Classify how the inner probe-loop task resolved (mirror of the house
/// `spawn_supervised_*` exit classifiers). `panic`/`cancelled` are only
/// distinguishable in unwind builds; production uses `panic = "abort"`, so a
/// panicking probe aborts the process there (WS-GAP-05 honest envelope) and
/// this classifier's `panic` arm fires only under `cfg(test)`/unwind.
fn classify_refresher_exit(res: &Result<(), JoinError>) -> &'static str {
    match res {
        Ok(()) => "clean_exit",
        Err(e) if e.is_panic() => "panic",
        Err(_) => "cancelled",
    }
}

/// The inner probe loop: schedule-gated 08:45 IST + 900s cadence. A panicking
/// app-supplied `probe.probe()` propagates out of this task and is caught by
/// the supervisor (`supervise_order_readiness_refresher`).
async fn run_order_readiness_refresher_loop(
    probe: Arc<dyn ReadinessProbe>,
    state: Arc<OrderReadinessState>,
) {
    loop {
        let wait =
            next_readiness_probe_wait_secs(tickvault_common::market_hours::now_ist_secs_of_day());
        if wait > 0 {
            tokio::time::sleep(Duration::from_secs(wait)).await;
            continue;
        }
        let _ready = refresh_order_readiness(probe.as_ref(), state.as_ref()).await;
        tokio::time::sleep(Duration::from_secs(ORDER_READINESS_REFRESH_INTERVAL_SECS)).await;
    }
}

/// Supervisor loop: spawn the inner probe loop, and if it EVER resolves (a
/// panicking probe, an external cancel, or an unexpected clean return) log a
/// coded `warn!`, count the respawn, back off, and respawn — so the fail-closed
/// gate never silently stops refreshing (C4 fix; mirrors the house
/// `spawn_supervised_*` pattern — WS-GAP-05 / DISK-WATCHER-01).
async fn supervise_order_readiness_refresher(
    probe: Arc<dyn ReadinessProbe>,
    state: Arc<OrderReadinessState>,
) {
    loop {
        let inner = tokio::spawn(run_order_readiness_refresher_loop(
            Arc::clone(&probe),
            Arc::clone(&state),
        ));
        let reason = classify_refresher_exit(&inner.await);
        warn!(
            code = ErrorCode::OrderReady01GateRefused.code_str(),
            stage = "refresher_respawn",
            reason,
            "🔷 DHAN — order-readiness refresher task exited; respawning (fail-closed gate stays REFUSING while down)"
        );
        metrics::counter!("tv_order_readiness_refresher_respawn_total", "reason" => reason)
            .increment(1);
        tokio::time::sleep(Duration::from_secs(
            ORDER_READINESS_REFRESHER_RESPAWN_BACKOFF_SECS,
        ))
        .await;
    }
}

/// SEAM HANDOFF: the trading crate ships this fn + tests; the ONE-LINE app-side
/// spawn (constructing the `Arc<dyn ReadinessProbe>` over `TokenManager` and
/// installing the state via `OrderManagementSystem::set_order_readiness`)
/// belongs to the boot-seam owners (PR body + seam memory). Off-window ticks
/// sleep to the next 08:45 IST; state is untouched off-window (the verdict goes
/// Stale naturally = correct overnight fail-closure). The returned task is a
/// SUPERVISOR: a panicking / exiting probe loop is caught and respawned (C4)
/// rather than silently disabling the gate.
// WIRING-EXEMPT: seam handoff — the app-side boot owns the one-line spawn; the
// trading test suite exercises it via a mock ReadinessProbe.
// FINANCIAL-TEST-EXEMPT: scheduling loop; the pure schedule math is covered by
// next_readiness_probe_wait_secs boundary tests.
pub fn spawn_order_readiness_refresher(
    probe: Arc<dyn ReadinessProbe>,
    state: Arc<OrderReadinessState>,
) -> JoinHandle<()> {
    tokio::spawn(supervise_order_readiness_refresher(probe, state))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready_state(now: u64) -> OrderReadinessState {
        let state = OrderReadinessState::new();
        apply_probe_outcome(
            &state,
            ProbeOutcome {
                profile_ok: true,
                token_headroom_secs: 20_000,
            },
            now,
        );
        state
    }

    #[test]
    fn test_evaluate_order_readiness_never_probed_refuses() {
        let state = OrderReadinessState::new();
        assert_eq!(
            evaluate_order_readiness(&state, 1_000),
            Err(ReadinessRefusal::NeverProbed)
        );
    }

    #[test]
    fn test_evaluate_order_readiness_ran_but_never_ok_refuses() {
        let state = OrderReadinessState::new();
        // A failed probe advances last_attempt but NOT last_ok.
        apply_probe_outcome(
            &state,
            ProbeOutcome {
                profile_ok: false,
                token_headroom_secs: 20_000,
            },
            1_000,
        );
        assert_eq!(
            evaluate_order_readiness(&state, 1_000),
            Err(ReadinessRefusal::NeverProbed)
        );
    }

    #[test]
    fn test_evaluate_order_readiness_not_ready_refuses() {
        let state = ready_state(1_000);
        state.poison_profile();
        assert_eq!(
            evaluate_order_readiness(&state, 1_010),
            Err(ReadinessRefusal::ProfileInvalid)
        );
    }

    #[test]
    fn test_evaluate_order_readiness_ready_fresh_passes() {
        let state = ready_state(1_000);
        assert_eq!(evaluate_order_readiness(&state, 1_100), Ok(()));
    }

    #[test]
    fn test_evaluate_order_readiness_stale_boundary_2100_passes_2101_refuses() {
        let state = ready_state(1_000);
        // age == 2100 passes.
        assert_eq!(evaluate_order_readiness(&state, 1_000 + 2_100), Ok(()));
        // age == 2101 refuses.
        assert_eq!(
            evaluate_order_readiness(&state, 1_000 + 2_101),
            Err(ReadinessRefusal::Stale { age_secs: 2_101 })
        );
    }

    #[test]
    fn test_evaluate_order_readiness_headroom_boundary_14399_refuses_14400_passes() {
        // Stamp headroom exactly at the boundary, no decay.
        let state = OrderReadinessState::new();
        apply_probe_outcome(
            &state,
            ProbeOutcome {
                profile_ok: true,
                token_headroom_secs: 14_400,
            },
            1_000,
        );
        assert_eq!(evaluate_order_readiness(&state, 1_000), Ok(()));

        let low = OrderReadinessState::new();
        apply_probe_outcome(
            &low,
            ProbeOutcome {
                profile_ok: true,
                token_headroom_secs: 14_399,
            },
            1_000,
        );
        assert_eq!(
            evaluate_order_readiness(&low, 1_000),
            Err(ReadinessRefusal::TokenHeadroomLow {
                effective_secs: 14_399
            })
        );
    }

    #[test]
    fn test_evaluate_order_readiness_headroom_decays_between_probes() {
        let state = OrderReadinessState::new();
        apply_probe_outcome(
            &state,
            ProbeOutcome {
                profile_ok: true,
                token_headroom_secs: 15_000,
            },
            1_000,
        );
        // 700s later: effective = 15000 - 700 = 14300 < 14400 -> refuse.
        assert_eq!(
            evaluate_order_readiness(&state, 1_700),
            Err(ReadinessRefusal::TokenHeadroomLow {
                effective_secs: 14_300
            })
        );
        // At +600s: effective = 14400 -> passes.
        assert_eq!(evaluate_order_readiness(&state, 1_600), Ok(()));
    }

    /// F-B fix: headroom + its stamp are packed into ONE atomic word, so a
    /// reader can never observe an OLD headroom paired with a NEW stamp (the
    /// spurious-PASS-near-boundary scenario). Every observable value is exactly
    /// one whole pair; pack/unpack roundtrips losslessly within the u32 range.
    #[test]
    fn test_headroom_pair_is_atomically_packed_no_torn_mix() {
        let state = OrderReadinessState::new();
        // OLD: huge headroom stamped at t=1000.
        apply_probe_outcome(
            &state,
            ProbeOutcome {
                profile_ok: true,
                token_headroom_secs: 80_000,
            },
            1_000,
        );
        assert_eq!(
            unpack_headroom(state.headroom_packed.load(ORD)),
            (80_000, 1_000)
        );
        // NEW: near-boundary headroom stamped at t=2000. With separate atomics a
        // torn read could mix (80_000, 2_000) -> effective 80_000 -> spurious
        // PASS. Packed, the pair is inseparable.
        apply_probe_outcome(
            &state,
            ProbeOutcome {
                profile_ok: true,
                token_headroom_secs: 14_401,
            },
            2_000,
        );
        assert_eq!(
            unpack_headroom(state.headroom_packed.load(ORD)),
            (14_401, 2_000)
        );
        // At t=2000 with the NEW pair, effective = 14_401 (passes); a decayed
        // read a second later drops to 14_400 exactly at the boundary.
        assert_eq!(evaluate_order_readiness(&state, 2_000), Ok(()));
        assert_eq!(evaluate_order_readiness(&state, 2_001), Ok(()));
        assert_eq!(
            evaluate_order_readiness(&state, 2_002),
            Err(ReadinessRefusal::TokenHeadroomLow {
                effective_secs: 14_399
            })
        );
        // Lossless pack/unpack roundtrip across the u32 range.
        for (h, s) in [
            (0_u64, 0_u64),
            (14_400, 1_700),
            (u64::from(u32::MAX), u64::from(u32::MAX)),
        ] {
            assert_eq!(unpack_headroom(pack_headroom(h, s)), (h, s));
        }
        // Over-range halves clamp to u32::MAX (never wrap).
        assert_eq!(
            unpack_headroom(pack_headroom(u64::MAX, u64::MAX)),
            (u64::from(u32::MAX), u64::from(u32::MAX))
        );
    }

    #[test]
    fn test_evaluate_order_readiness_zero_and_u64_max_now_saturate() {
        let state = ready_state(1_000);
        // now = 0 (before stamp): saturating_sub keeps age 0 -> but last_ok=1000
        // fresh; headroom elapsed saturates to 0 -> passes (no panic).
        let _ = evaluate_order_readiness(&state, 0);
        // now = u64::MAX: huge age -> Stale, no overflow panic.
        assert!(matches!(
            evaluate_order_readiness(&state, u64::MAX),
            Err(ReadinessRefusal::Stale { .. })
        ));
    }

    #[test]
    fn test_poison_profile_flips_not_ready_until_next_ok_probe() {
        let state = ready_state(1_000);
        assert_eq!(evaluate_order_readiness(&state, 1_010), Ok(()));
        state.poison_profile();
        assert_eq!(
            evaluate_order_readiness(&state, 1_010),
            Err(ReadinessRefusal::ProfileInvalid)
        );
        // A fresh OK probe restores readiness.
        apply_probe_outcome(
            &state,
            ProbeOutcome {
                profile_ok: true,
                token_headroom_secs: 20_000,
            },
            1_020,
        );
        assert_eq!(evaluate_order_readiness(&state, 1_030), Ok(()));
    }

    #[test]
    fn test_apply_probe_outcome_ok_and_err_roundtrip() {
        let state = OrderReadinessState::new();
        apply_probe_outcome(
            &state,
            ProbeOutcome {
                profile_ok: true,
                token_headroom_secs: 18_000,
            },
            5_000,
        );
        assert_eq!(evaluate_order_readiness(&state, 5_000), Ok(()));
        // A subsequent failed probe keeps last_ok (stays fresh) but flips profile.
        apply_probe_outcome(
            &state,
            ProbeOutcome {
                profile_ok: false,
                token_headroom_secs: 18_000,
            },
            5_100,
        );
        assert_eq!(
            evaluate_order_readiness(&state, 5_100),
            Err(ReadinessRefusal::ProfileInvalid)
        );
    }

    #[test]
    fn test_refusal_latch_fires_once_per_episode_and_rearms_on_ok() {
        let state = OrderReadinessState::new();
        assert!(state.latch_refusal(), "first refusal is a rising edge");
        assert!(
            !state.latch_refusal(),
            "second refusal is not a rising edge"
        );
        assert!(
            state.clear_refusal_latch(),
            "clear reports the episode ended"
        );
        assert!(state.latch_refusal(), "re-arms after a clear");
    }

    #[test]
    fn test_readiness_state_midnight_crossing_no_reset() {
        // The state has no daily reset; an old OK verdict simply goes Stale.
        let state = ready_state(1_000);
        assert!(matches!(
            evaluate_order_readiness(&state, 1_000 + 90_000),
            Err(ReadinessRefusal::Stale { .. })
        ));
    }

    #[test]
    fn test_readiness_refusal_slug_is_stable() {
        assert_eq!(ReadinessRefusal::NeverProbed.slug(), "no_probe");
        assert_eq!(ReadinessRefusal::Stale { age_secs: 1 }.slug(), "stale");
        assert_eq!(ReadinessRefusal::ProfileInvalid.slug(), "profile_invalid");
        assert_eq!(
            ReadinessRefusal::TokenHeadroomLow { effective_secs: 1 }.slug(),
            "token_headroom"
        );
    }

    // --- schedule boundaries ---

    #[test]
    fn test_next_readiness_probe_wait_secs_before_0845() {
        // 08:00 IST = 28800; wait to 08:45 (31500) = 2700.
        assert_eq!(next_readiness_probe_wait_secs(28_800), 2_700);
    }

    #[test]
    fn test_next_readiness_probe_wait_secs_at_0845_boundary() {
        assert_eq!(next_readiness_probe_wait_secs(31_500), 0);
    }

    #[test]
    fn test_next_readiness_probe_wait_secs_in_session() {
        // 12:00 IST = 43200; in [08:45, 15:30) -> probe now.
        assert_eq!(next_readiness_probe_wait_secs(43_200), 0);
    }

    #[test]
    fn test_next_readiness_probe_wait_secs_after_close_sleeps_to_next_0845() {
        // 16:00 IST = 57600; wait = (86400 - 57600) + 31500 = 60300.
        assert_eq!(next_readiness_probe_wait_secs(57_600), 60_300);
        // Exactly at close (15:30 = 55800) is NOT in-session -> sleep to next.
        assert_eq!(
            next_readiness_probe_wait_secs(55_800),
            u64::from(SECONDS_PER_DAY - 55_800) + 31_500
        );
    }

    // --- refresher (mock probe) ---

    struct MockProbe {
        outcome: ProbeOutcome,
    }
    impl ReadinessProbe for MockProbe {
        fn probe(&self) -> ProbeFuture<'_> {
            let outcome = self.outcome;
            Box::pin(async move { outcome })
        }
    }

    #[tokio::test]
    async fn test_refresh_order_readiness_ok_probe_makes_ready() {
        let state = OrderReadinessState::new();
        let probe = MockProbe {
            outcome: ProbeOutcome {
                profile_ok: true,
                token_headroom_secs: 20_000,
            },
        };
        let ready = refresh_order_readiness(&probe, &state).await;
        assert!(ready);
        assert!(evaluate_order_readiness(&state, now_epoch_s()).is_ok());
    }

    #[tokio::test]
    async fn test_refresh_order_readiness_failed_probe_not_ready_and_clears_latch_on_recovery() {
        let state = OrderReadinessState::new();
        // Latch a refusal first.
        state.latch_refusal();
        let bad = MockProbe {
            outcome: ProbeOutcome {
                profile_ok: false,
                token_headroom_secs: 20_000,
            },
        };
        assert!(!refresh_order_readiness(&bad, &state).await);
        // Recovery probe clears the refusal latch.
        let good = MockProbe {
            outcome: ProbeOutcome {
                profile_ok: true,
                token_headroom_secs: 20_000,
            },
        };
        assert!(refresh_order_readiness(&good, &state).await);
        // Recovery cleared the latch: it now reports no episode in progress.
        assert!(
            !state.clear_refusal_latch(),
            "refusal latch must be cleared on the recovery edge"
        );
    }

    #[tokio::test]
    async fn test_spawn_order_readiness_refresher_returns_handle() {
        let probe: Arc<dyn ReadinessProbe> = Arc::new(MockProbe {
            outcome: ProbeOutcome {
                profile_ok: true,
                token_headroom_secs: 20_000,
            },
        });
        let state = Arc::new(OrderReadinessState::new());
        let handle = spawn_order_readiness_refresher(probe, state);
        handle.abort();
        assert!(
            handle.await.is_err(),
            "aborted task resolves to a JoinError"
        );
    }

    /// A probe that panics — used to prove the C4 supervisor catches it.
    struct PanicProbe;
    impl ReadinessProbe for PanicProbe {
        fn probe(&self) -> ProbeFuture<'_> {
            Box::pin(async { panic!("probe boom") })
        }
    }

    /// C4: the supervisor's exit classifier maps a clean return, a panic, and a
    /// cancel to stable respawn-reason slugs.
    #[tokio::test]
    async fn test_classify_refresher_exit_variants() {
        let clean = tokio::spawn(async {});
        assert_eq!(classify_refresher_exit(&clean.await), "clean_exit");

        let panicked = tokio::spawn(async { panic!("boom") });
        assert_eq!(classify_refresher_exit(&panicked.await), "panic");

        let cancelled = tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        });
        cancelled.abort();
        assert_eq!(classify_refresher_exit(&cancelled.await), "cancelled");
    }

    /// C4: a panicking app-supplied probe does NOT kill supervision silently —
    /// the inner task's panic is caught (JoinError::is_panic) exactly as the
    /// supervisor observes it, and the fail-closed gate stays REFUSING.
    #[tokio::test]
    async fn test_refresher_inner_probe_panic_is_caught_as_panic_exit() {
        let probe: Arc<dyn ReadinessProbe> = Arc::new(PanicProbe);
        let state = Arc::new(OrderReadinessState::new());
        let inner = tokio::spawn({
            let p = Arc::clone(&probe);
            let s = Arc::clone(&state);
            async move {
                let _ = refresh_order_readiness(p.as_ref(), s.as_ref()).await;
            }
        });
        assert_eq!(classify_refresher_exit(&inner.await), "panic");
        // Gate never became READY — fail-closed after a probe panic.
        assert!(evaluate_order_readiness(&state, now_epoch_s()).is_err());
    }

    // --- strict Derivative-segment check (seam-handoff helper) ---

    #[test]
    fn test_segment_has_derivative_exact_token_grants() {
        assert!(segment_has_derivative("Derivative"));
        assert!(segment_has_derivative("Equity,Derivative,Currency"));
        assert!(segment_has_derivative("EQ | D | CUR"));
        assert!(segment_has_derivative("D"));
        assert!(segment_has_derivative("derivative")); // case-insensitive token
    }

    #[test]
    fn test_segment_has_derivative_rejects_fail_open_substrings() {
        // The naive `contains("D")` would fail-OPEN on all of these.
        assert!(!segment_has_derivative("DISABLED"));
        assert!(!segment_has_derivative("Equity,Currency"));
        assert!(!segment_has_derivative("CURRENCY_D_LEG"));
        assert!(!segment_has_derivative(""));
        assert!(!segment_has_derivative("Derivatives")); // not the exact token
    }

    #[tokio::test]
    async fn test_concurrent_readers_flapping_writer_no_invalid_outcome() {
        let state = Arc::new(ready_state(now_epoch_s()));
        let mut readers = Vec::new();
        for _ in 0..8 {
            let s = Arc::clone(&state);
            readers.push(tokio::spawn(async move {
                for _ in 0..2_000 {
                    // Must never panic under a concurrently-flapping writer;
                    // torn interleavings can only yield a spurious refusal.
                    let _ = evaluate_order_readiness(&s, now_epoch_s());
                }
            }));
        }
        // Flap the writer concurrently.
        for i in 0..2_000u64 {
            apply_probe_outcome(
                &state,
                ProbeOutcome {
                    profile_ok: i % 2 == 0,
                    token_headroom_secs: 20_000,
                },
                now_epoch_s(),
            );
        }
        for r in readers {
            r.await.expect("reader task must not panic");
        }
    }

    proptest::proptest! {
        #[test]
        fn prop_evaluate_order_readiness_total_no_panic_boundary(
            profile_ok in proptest::prelude::any::<bool>(),
            headroom in proptest::prelude::any::<u64>(),
            stamp in proptest::prelude::any::<u64>(),
            now in proptest::prelude::any::<u64>(),
        ) {
            let state = OrderReadinessState::new();
            apply_probe_outcome(&state, ProbeOutcome { profile_ok, token_headroom_secs: headroom }, stamp);
            // Must never panic for any arithmetic combination.
            let _ = evaluate_order_readiness(&state, now);
        }
    }
}

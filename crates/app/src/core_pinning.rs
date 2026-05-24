//! `core_affinity` worker pinning helpers.
//!
//! The hot-path latency budgets in `quality/benchmark-budgets.toml` (≤10ns
//! tick parse, ≤100ns OMS state transition, ≤50ns map lookup) assume the
//! WS read loop has a dedicated CPU. On AWS t4g.medium (2 vCPUs Graviton,
//! operator-lock 2026-05-18) and on dev Mac mirroring the 2-vCPU layout,
//! this module pins:
//!
//! ```text
//! Core 0  ←  WebSocket read loop (latency-critical)
//! Core 1  ←  Tick processor / pipeline / ILP writer / Other
//!            (kernel multiplexes — none of these are < 1µs sensitive)
//! ```
//!
//! Prior to 2026-05-24 this module assumed a 4-vCPU layout (c7i.xlarge).
//! When the AWS instance was relocked to t4g.medium (2 vCPUs), the layout
//! consolidated: WsRead keeps its dedicated core, the 3 other roles share
//! Core 1. The aggregate parser + ILP + observability hot-path budget is
//! ~10µs (Criterion p99) so multiplexing on a single 2.5 GHz Graviton
//! core is safely within budget.
//!
//! `core_affinity::set_for_current` is best-effort — kernels can ignore
//! the hint, cgroups can override it, and macOS `thread_policy_set` is a
//! soft request. When pinning fails this module emits ErrorCode
//! `CORE-PIN-01` (Severity::High) but the app continues without pinning;
//! latency budgets may not be met, the operator sees that fact via the
//! `tv_core_pinning_workers_pinned_total` gauge.
//!
//! Drift detection (CORE-PIN-02) is currently a stub; the full 60s drift
//! watchdog is a follow-up. The mechanical guards already in place
//! (`tv_core_pinning_drift_total` Prom counter wired in
//! `crates/app/src/observability.rs`) catch regressions when emitted.

use anyhow::Result;
use core_affinity::CoreId;
use tickvault_common::error_code::ErrorCode;
use tracing::{error, info};

/// Pinning role per the t4g.medium 2-vCPU layout. Mapping to a `core_id` is
/// done at runtime by `pin_current_thread_for(role)` so the host doesn't
/// need to know the layout statically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkerRole {
    /// WebSocket main-feed read loop — Core 0.
    WsRead,
    /// Tick processor / parser pipeline — Core 1.
    Parser,
    /// QuestDB ILP writer thread — Core 2.
    IlpWriter,
    /// Main thread / observability / telegram / etc. — Core 3.
    Other,
}

impl WorkerRole {
    /// Returns the canonical core index for this role on the t4g.medium
    /// 2-vCPU layout (operator-lock 2026-05-18). WsRead gets a dedicated
    /// core; the 3 other roles share core 1 — see module doc-comment for
    /// the rationale.
    #[must_use]
    pub const fn core_index(self) -> usize {
        match self {
            Self::WsRead => 0,
            // Parser / IlpWriter / Other all share Core 1. The kernel
            // multiplexes — aggregate budget is ~10µs (Criterion p99),
            // safely within a single 2.5GHz Graviton core's slot.
            Self::Parser | Self::IlpWriter | Self::Other => 1,
        }
    }

    /// Stable label used as Prom counter `worker_kind` label and
    /// tracing field.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::WsRead => "ws_read",
            Self::Parser => "parser",
            Self::IlpWriter => "ilp_writer",
            Self::Other => "other",
        }
    }
}

/// Minimum number of logical cores tickvault expects on the host.
/// Operator-lock 2026-05-18: t4g.medium (2 vCPU). Below this,
/// `pin_current_thread_for` returns an error rather than silently
/// pinning multiple roles onto a single shared core (which would
/// defeat the WS-read latency isolation).
pub const MINIMUM_CORE_COUNT: usize = 2;

/// Pins the current thread to the canonical core for the given role.
///
/// On failure, increments the
/// `tv_core_pinning_workers_pinned_total{worker_kind, outcome="failed"}`
/// counter and returns `Err(_)`. On success the same counter is bumped
/// with `outcome="ok"`. Either way the function NEVER panics — the
/// caller decides whether a failed pin is fatal (today: warn + continue).
///
/// Test coverage: covered by the host-too-small + role-mapping ratchets in
/// `tests::test_*` plus runtime emission of
/// `tv_core_pinning_workers_pinned_total`. Cannot be deterministically
/// unit-tested — depends on host CPU topology + kernel cgroup policy.
// TEST-EXEMPT: host-dependent — see docstring for coverage rationale.
pub fn pin_current_thread_for(role: WorkerRole) -> Result<()> {
    // `core_affinity::get_core_ids()` returns `Option<Vec<CoreId>>`; the
    // `None` case is platform-API failure (sched_getaffinity unavailable
    // / cgroup cpuset readback rejected). Treat as "host-too-small"
    // semantically — same operator action: investigate the deployment.
    let cores = match core_affinity::get_core_ids() {
        Some(c) => c,
        None => {
            metrics::counter!(
                "tv_core_pinning_workers_pinned_total",
                "worker_kind" => role.label(),
                "outcome" => "core_ids_unavailable",
            )
            .increment(1);
            error!(
                code = ErrorCode::CorePin01PinningFailedAtBoot.code_str(),
                worker = role.label(),
                "CORE-PIN-01: core_affinity::get_core_ids() returned None — \
                 platform API unavailable (kernel sched_getaffinity / cgroup \
                 cpuset readback rejected). Pinning skipped."
            );
            anyhow::bail!(
                "core_affinity::get_core_ids() returned None for {}",
                role.label()
            );
        }
    };
    let n = cores.len();
    let target_idx = role.core_index();

    if n < MINIMUM_CORE_COUNT {
        metrics::counter!(
            "tv_core_pinning_workers_pinned_total",
            "worker_kind" => role.label(),
            "outcome" => "host_too_small",
        )
        .increment(1);
        error!(
            code = ErrorCode::CorePin01PinningFailedAtBoot.code_str(),
            worker = role.label(),
            host_cores = n,
            required = MINIMUM_CORE_COUNT,
            "CORE-PIN-01: host has fewer than {MINIMUM_CORE_COUNT} cores — \
             t4g.medium 2-vCPU layout cannot be satisfied; pinning skipped."
        );
        anyhow::bail!(
            "host has {} cores; tickvault requires at least {}",
            n,
            MINIMUM_CORE_COUNT
        );
    }

    let core_id: CoreId = cores[target_idx];
    if core_affinity::set_for_current(core_id) {
        metrics::counter!(
            "tv_core_pinning_workers_pinned_total",
            "worker_kind" => role.label(),
            "outcome" => "ok",
        )
        .increment(1);
        info!(
            worker = role.label(),
            core_id = target_idx,
            "core_affinity: pinned thread for {} to core {}",
            role.label(),
            target_idx
        );
        Ok(())
    } else {
        metrics::counter!(
            "tv_core_pinning_workers_pinned_total",
            "worker_kind" => role.label(),
            "outcome" => "failed",
        )
        .increment(1);
        // Hostile-review fix (was `warn!`): CorePin01 is
        // Severity::High, and the host_too_small / core_ids_unavailable
        // branches above already use `error!`. Splitting the same code
        // across two log levels broke Loki→Telegram routing for the
        // single most common failure mode (kernel rejected hint).
        error!(
            code = ErrorCode::CorePin01PinningFailedAtBoot.code_str(),
            worker = role.label(),
            core_id = target_idx,
            "CORE-PIN-01: core_affinity::set_for_current returned false; \
             continuing without pinning. Latency budgets may not be met."
        );
        anyhow::bail!(
            "core_affinity::set_for_current returned false for {} -> core {}",
            role.label(),
            target_idx
        );
    }
}

/// Best-effort one-shot pin of the calling (main) thread to
/// `WorkerRole::Other`. Used at boot so the main thread doesn't share
/// Core 0 with the WS read loop.
///
/// On failure, `pin_current_thread_for` ALREADY emits `error!` with
/// `code = ErrorCode::CorePin01PinningFailedAtBoot.code_str()` AND
/// increments `tv_core_pinning_workers_pinned_total{outcome="failed"|"host_too_small"|"core_ids_unavailable"}`.
/// We deliberately do NOT re-log here — duplicating the message at WARN
/// level would (a) lack the `code` field (so the tag-guard cannot trace
/// it) and (b) confuse operators reading `errors.jsonl` who would see
/// the same incident twice. Loki routes the inner ERROR to Telegram via
/// the standard severity pipeline; the outer wrapper just consumes the
/// `Result` without obscuring it.
// TEST-EXEMPT: thin wrapper around pin_current_thread_for; same rationale.
pub fn pin_main_thread() {
    // Discard the Result — `pin_current_thread_for` already emits ERROR
    // + CORE-PIN-01 + counter on failure (Loki → Telegram). Re-handling
    // here would double-log without adding signal.
    drop(pin_current_thread_for(WorkerRole::Other));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_worker_role_core_indices_match_t4g_medium_layout() {
        // t4g.medium operator-lock 2026-05-18: 2 vCPUs total.
        // WsRead gets its own core; everything else shares Core 1.
        assert_eq!(WorkerRole::WsRead.core_index(), 0);
        assert_eq!(WorkerRole::Parser.core_index(), 1);
        assert_eq!(WorkerRole::IlpWriter.core_index(), 1);
        assert_eq!(WorkerRole::Other.core_index(), 1);
        // Every role's core_index must be within the host's vCPU budget.
        for r in [
            WorkerRole::WsRead,
            WorkerRole::Parser,
            WorkerRole::IlpWriter,
            WorkerRole::Other,
        ] {
            assert!(
                r.core_index() < MINIMUM_CORE_COUNT,
                "role {r:?} core index {} >= MINIMUM_CORE_COUNT {MINIMUM_CORE_COUNT}",
                r.core_index()
            );
        }
    }

    #[test]
    fn test_ws_read_is_isolated_from_other_roles() {
        // The latency-critical WS read loop MUST NOT share a core with
        // any other role — otherwise the kernel can preempt mid-frame.
        // Any future refactor that puts Parser/IlpWriter/Other on Core 0
        // breaks the budgeting and fails this ratchet.
        let ws_core = WorkerRole::WsRead.core_index();
        assert_ne!(ws_core, WorkerRole::Parser.core_index());
        assert_ne!(ws_core, WorkerRole::IlpWriter.core_index());
        assert_ne!(ws_core, WorkerRole::Other.core_index());
    }

    #[test]
    fn test_worker_role_labels_are_stable_and_distinct() {
        let labels = [
            WorkerRole::WsRead.label(),
            WorkerRole::Parser.label(),
            WorkerRole::IlpWriter.label(),
            WorkerRole::Other.label(),
        ];
        let mut seen = std::collections::HashSet::new();
        for lbl in labels {
            assert!(seen.insert(lbl), "label {lbl} collides");
            // Labels are used as Prom counter dimensions — must be
            // ASCII-lowercase and snake_case.
            assert!(
                lbl.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "label {lbl} must be ASCII lowercase + underscores"
            );
        }
    }

    #[test]
    fn test_minimum_core_count_matches_aws_t4g_medium() {
        // t4g.medium has 2 vCPUs (operator-lock 2026-05-18 in aws-budget.md).
        // Bumping this constant requires updating both CLAUDE.md and
        // aws-budget.md.
        assert_eq!(MINIMUM_CORE_COUNT, 2);
    }
}

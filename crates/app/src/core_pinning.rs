//! Wave 5 Item 6 — `core_affinity` worker pinning helpers.
//!
//! The hot-path latency budgets in `quality/benchmark-budgets.toml` (≤10ns
//! tick parse, ≤100ns OMS state transition, ≤50ns map lookup) assume the
//! WS read loop, parser pipeline, and ILP writer threads each have a
//! dedicated CPU. On AWS c7i.xlarge (4 vCPUs) and on dev Mac mirroring
//! the 4-vCPU layout, this module pins:
//!
//! ```text
//! Core 0  ←  WebSocket read loop
//! Core 1  ←  Tick processor / pipeline
//! Core 2  ←  QuestDB ILP writer
//! Core 3  ←  Other (main thread, observability, telegram, etc.)
//! ```
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
use tracing::{error, info, warn};

/// Pinning role per the Wave 5 4-vCPU layout. Mapping to a `core_id` is
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
    /// Returns the canonical Wave 5 core index for this role on a 4-vCPU
    /// host. Hosts with fewer cores cannot satisfy the layout — see
    /// `pin_current_thread_for` for the runtime guard.
    #[must_use]
    pub const fn core_index(self) -> usize {
        match self {
            Self::WsRead => 0,
            Self::Parser => 1,
            Self::IlpWriter => 2,
            Self::Other => 3,
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

/// Minimum number of logical cores Wave 5 expects on the host. Below
/// this, `pin_current_thread_for` returns an error rather than silently
/// pinning all roles to a smaller set.
pub const MINIMUM_CORE_COUNT: usize = 4;

/// Pins the current thread to the canonical core for the given role.
///
/// On failure, increments the
/// `tv_core_pinning_workers_pinned_total{worker_kind, outcome="failed"}`
/// counter and returns `Err(_)`. On success the same counter is bumped
/// with `outcome="ok"`. Either way the function NEVER panics — the
/// caller decides whether a failed pin is fatal (today: warn + continue).
// TEST-EXEMPT: depends on host CPU topology + kernel cgroup policy; covered
// by the host-too-small + role-mapping ratchets above plus runtime emission
// of `tv_core_pinning_workers_pinned_total`.
pub fn pin_current_thread_for(role: WorkerRole) -> Result<()> {
    let cores = core_affinity::get_core_ids().unwrap_or_default();
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
             Wave 5 4-vCPU layout cannot be satisfied; pinning skipped."
        );
        anyhow::bail!(
            "host has {} cores; Wave 5 requires at least {}",
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
        warn!(
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
// TEST-EXEMPT: thin wrapper around `pin_current_thread_for(Other)`; the
// wrapped function is itself TEST-EXEMPT for the same reasons.
pub fn pin_main_thread() {
    if let Err(err) = pin_current_thread_for(WorkerRole::Other) {
        warn!(
            ?err,
            "main thread pinning failed — operator should investigate \
             tv_core_pinning_workers_pinned_total"
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_worker_role_core_indices_are_distinct_under_4_vcpu_layout() {
        let roles = [
            WorkerRole::WsRead,
            WorkerRole::Parser,
            WorkerRole::IlpWriter,
            WorkerRole::Other,
        ];
        let mut seen = std::collections::HashSet::new();
        for r in roles {
            assert!(
                seen.insert(r.core_index()),
                "role {:?} core index {} collides",
                r,
                r.core_index()
            );
            assert!(r.core_index() < MINIMUM_CORE_COUNT);
        }
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
    fn test_minimum_core_count_matches_aws_c7i_xlarge() {
        // c7i.xlarge has 4 vCPUs; dev Mac mirrors that layout per the
        // Wave 5 plan. Bumping this constant requires updating both
        // CLAUDE.md and aws-budget.md.
        assert_eq!(MINIMUM_CORE_COUNT, 4);
    }
}

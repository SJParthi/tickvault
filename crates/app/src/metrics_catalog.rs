//! Metrics cardinality contract — L126 / L128 (Wave-5 plan §AA).
//!
//! This module is the single source of truth for the **labels** that
//! the in-memory store observability stack (PR #504a) is allowed to
//! emit. Every metric introduced by #504a must appear here; ratchet
//! tests reject regressions in either direction.
//!
//! Locked decisions (from `.claude/plans/active-plan-in-memory-store-aws-instance.md` §AA):
//! - **L121** — `tv_subsystem_memory_bytes` renamed to
//!   `tv_subsystem_memory_estimated_bytes` (no claim of being raw RSS).
//! - **L126** — cardinality contract: `metrics_catalog.rs` enumerates
//!   the allowed label set; reject any addition without explicit
//!   approval. Stops a future `{tf, security_id}` regression cold.
//! - **L128** — TF labels are `&'static str` matched from a compile-
//!   time enum. Ratchet test pins the allowed values so that an
//!   unconstrained `tf` argument cannot be smuggled in via `format!`.
//!
//! Non-goals: this catalog does NOT register the metrics with the
//! `metrics` facade — it only declares the contract. Registration
//! happens in [`crate::subsystem_memory`].
//!
//! # Why a catalog instead of inline `&'static str` constants?
//!
//! The plan-review verdict (§AA) flagged two separate hot-path /
//! cardinality concerns:
//!   * HOT-C2 — `format!()` for TF labels in `metrics::counter!` is a
//!     banned-pattern hit AND allocates per call.
//!   * BUG-H5 — once a `{tf}` label exists, future commits could
//!     silently add `{tf, security_id}` and detonate Prometheus with
//!     ~231K series.
//!
//! Folding both into one catalog gives us:
//!   * shared `TfIndex` type → `display_name()` returns one of ten
//!     `&'static str` constants; no allocation on the hot path;
//!   * `ALLOWED_*` slices → ratcheted by a runtime test in this same
//!     module so any addition has to land here AND in the test.
//!
//! # Retired — Dhan exchange-lag metrics (deleted 2026-07-17, dashboard tidy)
//!
//! The 2026-07-06 silent-feed hardening's Dhan lag trio
//! (`tv_dhan_exchange_lag_p99_seconds` gauge,
//! `tv_dhan_lag_samples_excluded_total`, `tv_dhan_lag_negative_clamped_total`)
//! and the supervisor counter `tv_feed_lag_publisher_respawn_total` were
//! DELETED 2026-07-17 with the dead Dhan-lag ring/publisher chain in
//! `feed_lag_monitor.rs` — the chain lost its tick source + spawn sites
//! with the Dhan live-WS lane deletion (PR-C2, 2026-07-13) and the stage-2
//! dead-WS sweep (2026-07-17), so none of the four could ever emit again.
//! The 2 CloudWatch-exported names left the EMF allowlist (19 → 17) and the
//! `dhan_exchange_lag_p99_high` alarm was retired in the same PR
//! (silent-feed-alarms.tf; cost note in aws-budget.md, 2026-07-17).
//!
//! # Retired — Groww exchange-lag metrics (deleted 2026-07-15)
//!
//! The scoreboard PR-C Groww lag mirror (`tv_groww_exchange_lag_p99_seconds`
//! gauge + its `tv_groww_lag_*` companion counters +
//! `spawn_supervised_groww_lag_publisher`) was DELETED 2026-07-15 with the
//! Groww live feed (operator directive: REST-only for both brokers) — the
//! sidecar capture stream that fed it no longer exists. What SURVIVES: the
//! per-feed DAY lag histograms (in-memory, drained into the
//! `feed_scoreboard_daily` lag columns at 15:45 IST — the Groww slot now
//! reads None/−1, honest) live on in
//! `tickvault_core::pipeline::feed_lag_monitor`.

#![allow(clippy::module_name_repetitions)]

// Scoreboard PR-D presence registry: DELETED 2026-07-18 (stage-4
// dead-producer sweep) — it carried deliberately ZERO metrics, so no
// catalog change accompanies the deletion.

/// Metric name for the per-component memory estimate gauge.
///
/// L121 (renamed from `tv_subsystem_memory_bytes`): the value is a
/// `len() × size_of` *estimate*, not a raw RSS measurement. The
/// `process_resident_memory_bytes` gauge (already exposed by
/// `metrics-exporter-prometheus`) remains the canonical RSS source.
/// We reconcile against it inside ±5% (see
/// `subsystem_memory::reconcile_within_pct`).
pub const SUBSYSTEM_MEMORY_GAUGE_NAME: &str = "tv_subsystem_memory_estimated_bytes";

/// Heartbeat gauge for the sampler task (L125). The Prometheus
/// expression `time() - tv_subsystem_memory_sampler_last_update_seconds`
/// fires the `tv-subsystem-memory-sampler-stale` alert when the
/// sampler hangs.
pub const SAMPLER_HEARTBEAT_GAUGE_NAME: &str = "tv_subsystem_memory_sampler_last_update_seconds";

/// Quiet-hours gate used by every alert that must NOT page outside
/// the trading session (L129 + audit-findings Rule 4 edge-trigger
/// discipline). `1.0` during 09:00–15:30 IST, `0.0` otherwise. The
/// alert expression `... and on() tv_market_hours_active == 1`
/// suppresses pages at IST midnight rebuild + post-close transitions.
pub const MARKET_HOURS_ACTIVE_GAUGE_NAME: &str = "tv_market_hours_active";

/// Counter for in-memory bar-storage evictions per timeframe. The
/// ratchet pins both the metric name AND the `{tf}` label set so
/// future commits cannot silently add `{security_id}` (BUG-H5).
///
/// `count == 0` for every TF at boot (BUG-L13) so PromQL
/// `sum by (tf)` does not silently miss freshly-introduced labels.
pub const IN_MEM_EVICTIONS_COUNTER_NAME: &str = "tv_in_mem_evictions_total";

/// Coarse component labels for the per-component memory gauge.
///
/// SEC-M3 (§AA): names are deliberately coarse so that an attacker
/// who scrapes `/metrics` cannot derive the precise architecture
/// (e.g. "papaya tick map at security_id 13" → "rescue_ring",
/// "bar_storage", "registry", "runtime", "binary_static",
/// "papaya_overhead"). Exactly 6 entries.
///
/// `f64::NAN` is the boot-time value for every component (L124);
/// the sampler overwrites only those components whose source
/// closure has been registered. Components that remain unset stay
/// NaN and are filtered out by the `unless absent_over_time(...)`
/// alert clause.
///
/// `tick_storage` RETIRED 2026-07-19 (dead-code cleanup — BATCH-5): the
/// in-RAM `TickStorage` store was removed with the PrevDayCache/TickStorage
/// sweep, so no source closure ever registered this component — its gauge
/// stayed permanently NaN (sourceless). Dropped from the allowlist.
pub const ALLOWED_SUBSYSTEM_COMPONENTS: &[&str] = &[
    "rescue_ring",
    "bar_storage",
    "registry",
    "runtime",
    "binary_static",
    "papaya_overhead",
];

/// Metrics use the same typed active frame as the candle engine. There is no
/// independent label enum that can continue advertising retired timeframes.
pub use tickvault_trading::candles::TfIndex as Tf;

/// Static labels for the active candle set; cardinality follows TF_COUNT.
#[must_use]
pub fn allowed_tf_labels() -> [&'static str; tickvault_trading::candles::TF_COUNT] {
    std::array::from_fn(|index| Tf::ALL[index].display_name())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn metric_names_use_tv_prefix() {
        for name in [
            SUBSYSTEM_MEMORY_GAUGE_NAME,
            SAMPLER_HEARTBEAT_GAUGE_NAME,
            MARKET_HOURS_ACTIVE_GAUGE_NAME,
            IN_MEM_EVICTIONS_COUNTER_NAME,
        ] {
            assert!(
                name.starts_with("tv_"),
                "metric name {name} must start with tv_"
            );
        }
    }

    #[test]
    fn subsystem_memory_metric_name_uses_estimated_suffix_per_l121() {
        // L121: rename forbade the legacy `tv_subsystem_memory_bytes`.
        // The catalog must point at the renamed gauge.
        assert_eq!(
            SUBSYSTEM_MEMORY_GAUGE_NAME, "tv_subsystem_memory_estimated_bytes",
            "L121 rename: `tv_subsystem_memory_bytes` is BANNED — \
             use `tv_subsystem_memory_estimated_bytes` so the name does \
             not falsely claim raw-RSS semantics."
        );
    }

    #[test]
    fn allowed_components_has_exactly_six_coarse_labels() {
        // L18 + SEC-M3: coarse names only. Pinned at 6 since the sourceless
        // `tick_storage` label was retired 2026-07-19 (BATCH-5) with the
        // in-RAM TickStorage store.
        assert_eq!(
            ALLOWED_SUBSYSTEM_COMPONENTS.len(),
            6,
            "L18 design pins 6 components — adding a 7th requires a \
             plan update plus dashboard / alert / ratchet review."
        );
        let set: HashSet<&&str> = ALLOWED_SUBSYSTEM_COMPONENTS.iter().collect();
        assert_eq!(
            set.len(),
            ALLOWED_SUBSYSTEM_COMPONENTS.len(),
            "component labels must be unique"
        );
        for name in ALLOWED_SUBSYSTEM_COMPONENTS {
            assert!(
                !name.contains(':') && !name.contains('.'),
                "coarse label {name} must not leak architectural punctuation"
            );
        }
    }

    #[test]
    fn allowed_components_do_not_leak_precise_subsystems() {
        // SEC-M3: even though the value is internal, the names must
        // not name precise modules. `tick_processor`, `papaya_pool`,
        // etc. would leak architecture; `bar_storage` / `runtime`
        // are coarse enough.
        let banned_substrings = [
            "tick_processor",
            "depth_connection",
            "questdb_writer",
            "websocket_pool",
        ];
        for &component in ALLOWED_SUBSYSTEM_COMPONENTS {
            for &banned in &banned_substrings {
                assert!(
                    !component.contains(banned),
                    "component label {component} leaks subsystem name {banned}"
                );
            }
        }
    }

    #[test]
    fn tf_labels_match_the_exact_active_candle_set() {
        let expected = [
            "1s", "3s", "5s", "1m", "3m", "5m", "10m", "15m", "30m", "60m",
        ];
        assert_eq!(allowed_tf_labels(), expected);
        assert_eq!(Tf::ALL.len(), expected.len());
        let unique: HashSet<_> = allowed_tf_labels().into_iter().collect();
        assert_eq!(unique.len(), expected.len());
        assert_eq!(
            tickvault_common::config::TimeframesConfig::default_list(),
            expected,
        );
    }

    #[test]
    fn retired_candle_labels_cannot_be_prewarmed() {
        for retired in [
            "2s", "4s", "6s", "7s", "8s", "9s", "10s", "11s", "12s", "13s", "14s", "15s", "30s",
            "2m", "1h", "2h", "3h", "4h", "1d",
        ] {
            assert!(!allowed_tf_labels().contains(&retired), "{retired}");
        }
    }

    #[test]
    fn allowed_tf_labels_follow_canonical_enum_iteration() {
        for (index, tf) in Tf::ALL.iter().enumerate() {
            assert_eq!(allowed_tf_labels()[index], tf.display_name());
        }
    }
}

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
//!   * compile-time `Tf` enum → `as_static_str()` returns one of 21
//!     `&'static str` constants; no allocation on the hot path;
//!   * `ALLOWED_*` slices → ratcheted by a runtime test in this same
//!     module so any addition has to land here AND in the test.
//!
//! # Registered elsewhere — Dhan exchange-lag metrics (silent-feed Item 4)
//!
//! The 2026-07-06 silent-feed hardening added three metrics whose emit
//! sites live in `tickvault_core::pipeline::feed_lag_monitor` (this app
//! crate cannot host their names as consts without dead code — core cannot
//! depend on app). Catalogued here for discoverability:
//!   * `tv_dhan_exchange_lag_p99_seconds` — UNLABELED gauge, trailing-60s
//!     exchange→receive lag p99, published every 10 s ONLY in-session with
//!     ≥50 samples. **Quantization floor: Dhan LTT is a u32 of whole IST
//!     seconds, so the lag has a ≥1 s floor — a healthy p99 reads ~1–2 s
//!     and can NEVER read 0; sub-second wire lag is UNMEASURABLE for
//!     feed=dhan.** The dhan-only NAME (no `feed` label) sidesteps the
//!     CloudWatch EMF host-only dimension label-folding trap.
//!   * `tv_dhan_lag_samples_excluded_total` — /metrics-only counter of
//!     WAL-replay samples excluded by the receipt−capture dwell
//!     discriminator (visible, never silent censoring — Rule 11).
//!   * `tv_dhan_lag_negative_clamped_total` — /metrics-only counter of
//!     negative-lag clamps (host-clock skew vs Dhan whole-second stamps).
//! Plus the supervisor counter `tv_feed_lag_publisher_respawn_total{reason}`
//! emitted by `spawn_supervised_feed_lag_publisher` in `main.rs`
//! (WS-GAP-05/SLO-03 respawn pattern; `reason` labels are the static
//! `classify_join_exit` set — no cardinality risk).

#![allow(clippy::module_name_repetitions)]

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
/// "tick_storage", "bar_storage", "registry", "runtime",
/// "binary_static", "papaya_overhead"). Exactly 7 entries.
///
/// `f64::NAN` is the boot-time value for every component (L124);
/// the sampler overwrites only those components whose source
/// closure has been registered. Components that remain unset stay
/// NaN and are filtered out by the `unless absent_over_time(...)`
/// alert clause.
pub const ALLOWED_SUBSYSTEM_COMPONENTS: &[&str] = &[
    "rescue_ring",
    "tick_storage",
    "bar_storage",
    "registry",
    "runtime",
    "binary_static",
    "papaya_overhead",
];

/// Compile-time enum for the allowed `tf` label values used by the
/// in-memory eviction counter (HOT-C2 + SEC-M1 + L128).
///
/// The exact 21 timeframes match L6 in the in-memory store plan:
/// 1m..15m (every minute) + 30m + 1h..4h (every hour) + 1d. Seconds
/// engines (1s/3s/5s/10s/15s/30s) were retired in L7 of the plan;
/// they are NOT permitted in this enum.
///
/// Routing values into this enum (rather than accepting a `&str`)
/// guarantees that:
///   1. an arbitrary `tf` value cannot be passed via `format!`
///      (rejected at the type level),
///   2. the cardinality of the `tf` label is bounded at compile time
///      (BUG-H5 — exactly 21 series, no growth path),
///   3. the hot path uses a static `&'static str` from
///      [`Tf::as_static_str`] with zero allocation (HOT-C2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tf {
    M1,
    M5,
    M15,
    M30,
    H1,
    H2,
    H3,
    H4,
    D1,
}

impl Tf {
    /// All 9 timeframes, ordered ascending. PR #517 (Wave-5 TF reduction)
    /// retired the 12 sub-15-minute non-canonical timeframes
    /// (M2/M3/M4/M6/M7/M8/M9/M10/M11/M12/M13/M14). Used by the boot-time
    /// counter pre-warm (BUG-L13) and by the cardinality ratchet.
    pub const ALL: [Self; 9] = [
        Self::M1,
        Self::M5,
        Self::M15,
        Self::M30,
        Self::H1,
        Self::H2,
        Self::H3,
        Self::H4,
        Self::D1,
    ];

    /// `&'static str` form for use as a Prometheus label value.
    /// The returned reference has program-lifetime, so the metrics
    /// crate can intern it without allocating.
    #[must_use]
    pub const fn as_static_str(self) -> &'static str {
        match self {
            Self::M1 => "1m",
            Self::M5 => "5m",
            Self::M15 => "15m",
            Self::M30 => "30m",
            Self::H1 => "1h",
            Self::H2 => "2h",
            Self::H3 => "3h",
            Self::H4 => "4h",
            Self::D1 => "1d",
        }
    }
}

/// All `&'static str` TF label values, derived from [`Tf::ALL`].
/// Pinned by the ratchet so adding an entry without updating the
/// enum (or vice versa) fails the build.
#[must_use]
pub fn allowed_tf_labels() -> [&'static str; 9] {
    let mut out: [&'static str; 9] = [""; 9];
    let mut i = 0;
    while i < Tf::ALL.len() {
        out[i] = Tf::ALL[i].as_static_str();
        i += 1;
    }
    out
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
    fn allowed_components_has_exactly_seven_coarse_labels() {
        // L18 + SEC-M3: exactly 7 components, coarse names only.
        assert_eq!(
            ALLOWED_SUBSYSTEM_COMPONENTS.len(),
            7,
            "L18 design pins 7 components — adding an 8th requires a \
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
        // etc. would leak architecture; `tick_storage` / `runtime`
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
    fn tf_enum_has_exactly_9_variants_per_pr517() {
        // PR #517 (Wave-5 TF reduction): 21 → 9 TFs. Retired the 12 sub-15-
        // minute non-canonical timeframes (M2/M3/M4/M6/M7/M8/M9/M10/M11/
        // M12/M13/M14). The active ladder is now 1m / 5m / 15m / 30m /
        // 1h / 2h / 3h / 4h / 1d.
        assert_eq!(
            Tf::ALL.len(),
            9,
            "PR #517 pins 9 timeframes after the sub-15m retirement."
        );
    }

    #[test]
    fn tf_static_str_values_are_unique_and_match_enum() {
        let mut set = HashSet::new();
        for tf in Tf::ALL {
            assert!(set.insert(tf.as_static_str()), "duplicate TF label");
        }
        assert_eq!(set.len(), 9);
    }

    #[test]
    fn tf_static_str_values_are_canonical_pinned_set() {
        // PR #517 ratchet — pin the EXACT set so an unsanctioned addition
        // (e.g. someone re-introduces "2m" or "30s") fails this test.
        let expected: HashSet<&'static str> =
            ["1m", "5m", "15m", "30m", "1h", "2h", "3h", "4h", "1d"]
                .into_iter()
                .collect();
        let actual: HashSet<&'static str> = allowed_tf_labels().into_iter().collect();
        assert_eq!(actual, expected, "PR #517: TF label set drifted");
    }

    #[test]
    fn tf_retired_engines_are_banned() {
        // L7 retired 1s/3s/5s/10s/15s/30s. PR #517 retired 2m/3m/4m/6m/7m/
        // 8m/9m/10m/11m/12m/13m/14m. The ratchet ensures none ever sneak
        // back in.
        for banned in [
            "1s", "3s", "5s", "10s", "15s", "30s", "2m", "3m", "4m", "6m", "7m", "8m", "9m", "10m",
            "11m", "12m", "13m", "14m",
        ] {
            for &allowed in &allowed_tf_labels() {
                assert_ne!(
                    allowed, banned,
                    "Retired timeframe {banned} MUST NOT reappear — re-add via \
                     coordinated PR (config + cascade engine + matview DDL + \
                     enum + symmetry ratchet)."
                );
            }
        }
    }

    #[test]
    fn allowed_tf_labels_helper_matches_enum_iteration() {
        for (i, tf) in Tf::ALL.iter().enumerate() {
            assert_eq!(
                allowed_tf_labels()[i],
                tf.as_static_str(),
                "allowed_tf_labels()[{i}] drifted from Tf::ALL[{i}]"
            );
        }
    }
}

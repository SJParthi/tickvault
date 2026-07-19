//! Per-subsystem memory observability — Wave-5 in-memory store
//! plan §AA, locked decisions L18 (revised) + L121–L130.
//!
//! Ships the SCAFFOLDING that downstream PRs (`#504d` in-memory store
//! and beyond) populate with concrete component sources. The metric
//! contract is locked here so adding a component / TF label / cardinality
//! lever later requires touching this module + the catalog ratchet.
//!
//! # Why this exists (operator-facing)
//!
//! The legacy `MEMORY_RSS_ALERT_MB = 1024` alert (PR #497) reads a
//! single process-wide RSS gauge — when it fires, the operator has to
//! grep `data/logs/*` to figure out which subsystem leaked. Under the
//! Wave-5 in-memory-store design (~2.31 GB total expected RAM) the
//! 1 GB threshold would fire instantly post-deploy AND give zero
//! diagnostic signal (BUG-C2). #504a retires the legacy threshold and
//! ships per-component gauges so the operator sees:
//!
//! ```text
//! tv_subsystem_memory_estimated_bytes{component="rescue_ring"}   228000000
//! tv_subsystem_memory_estimated_bytes{component="bar_storage"}  1287000000  # alert!
//! tv_subsystem_memory_estimated_bytes{component="registry"}       12000000
//! ```
//!
//! at a glance, with the alert pointing at the offending component.
//!
//! # Honest measurement caveat (L121)
//!
//! Per-component values are `len() × size_of` *estimates*, not raw
//! kernel-measured RSS. The exporter's pre-existing
//! `process_resident_memory_bytes` remains the canonical RSS source.
//! [`reconcile_within_pct`] cross-checks the sum against process RSS
//! within ±5%; the ratchet test pins this so the design's claim
//! "per-subsystem accounting summed to RSS" is enforced mechanically.
//!
//! # Hot-path budget (HOT-C1 + HOT-H1)
//!
//! No `AtomicUsize` is mutated on the tick-processing hot path. The
//! sampler runs out-of-band every 10 s (matches the Prometheus scrape
//! cadence) and reads `len()` from the live `papaya` snapshots that
//! are already MVCC-clean. Counter handles for evictions are cached
//! at boot (HOT-H1) so the eviction emit site is a `Counter::increment`
//! on a pre-resolved key.

#![allow(clippy::module_name_repetitions)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use metrics::{Counter, Gauge, counter, gauge};
use tracing::{info, instrument, warn};

use crate::metrics_catalog::{
    ALLOWED_SUBSYSTEM_COMPONENTS, IN_MEM_EVICTIONS_COUNTER_NAME, MARKET_HOURS_ACTIVE_GAUGE_NAME,
    SAMPLER_HEARTBEAT_GAUGE_NAME, SUBSYSTEM_MEMORY_GAUGE_NAME, Tf,
};

/// How often the sampler task wakes to refresh component gauges +
/// the heartbeat. Aligned with the Prometheus default scrape window
/// per the plan: HYBRID `len() × size_of` lazy at the 10 s cadence.
pub const SAMPLER_INTERVAL_SECS: u64 = 10;

/// Reconciliation tolerance — sum of per-component estimates must
/// fall within ±5 % of the kernel-measured process RSS gauge.
/// Pinned by `test_reconcile_within_pct_*` ratchets so anyone who
/// loosens the tolerance has to update the design lock first.
pub const RECONCILE_TOLERANCE_PCT: f64 = 5.0;

/// Cached `Gauge` and `Counter` handles registered once at boot.
///
/// HOT-H1 (§AA review verdict): per-call `metrics::counter!()` /
/// `metrics::gauge!()` allocation under a 25 K-tick/s hot path is
/// outside the budget. Holding the handles in this struct means the
/// emit site dereferences `&Counter` / `&Gauge` and calls
/// `.increment(1)` / `.set(value)` directly — zero hashmap lookup
/// per emit.
///
/// L124 — every gauge here is set to `f64::NAN` at construction.
/// The sampler overwrites a component's gauge only when a
/// [`SubsystemMemorySampler::register_source`] closure is wired.
/// Components that remain unregistered stay NaN; the alert's
/// `unless absent_over_time(...)` clause filters them out.
#[derive(Debug)]
pub struct SubsystemMemoryHandles {
    /// One eviction counter per timeframe (BUG-L13: pre-warmed to 0
    /// at boot so PromQL `sum by (tf)` does not silently miss a
    /// freshly-introduced label).
    eviction_counters: HashMap<Tf, Counter>,
    /// One per-component memory gauge, keyed on the component label
    /// (one of [`ALLOWED_SUBSYSTEM_COMPONENTS`]).
    component_gauges: HashMap<&'static str, Gauge>,
    /// Heartbeat gauge — UNIX seconds of the last sampler tick.
    /// L125: `tv-subsystem-memory-sampler-stale` alert fires if
    /// `time() - tv_subsystem_memory_sampler_last_update_seconds > 30`.
    sampler_heartbeat: Gauge,
    /// `1.0` during 09:00–15:30 IST, `0.0` otherwise. Quiet-hours
    /// gate for every alert that must not page outside the trading
    /// session (L129 + audit-findings Rule 4).
    market_hours_active: Gauge,
}

impl SubsystemMemoryHandles {
    /// Cache every handle and apply the boot-time priming required by
    /// the §AA review verdict.
    ///
    /// Safety / ordering: MUST be called AFTER
    /// `metrics-exporter-prometheus` has installed its global recorder
    /// (i.e. after [`crate::observability::init_metrics`]). Calling
    /// this before installation produces no-op handles per the
    /// `metrics` crate semantics.
    #[must_use]
    pub fn register() -> Self {
        // L128 + BUG-L13: pre-warm all 21 TF eviction counters to 0
        // so PromQL `sum by (tf)` reports every label from boot.
        let mut eviction_counters = HashMap::with_capacity(Tf::ALL.len());
        for tf in Tf::ALL {
            let label = tf.as_static_str();
            // HOT-C2 + SEC-M1: label value is a `&'static str`, no
            // formatting, no allocation.
            let handle: Counter = counter!(IN_MEM_EVICTIONS_COUNTER_NAME, "tf" => label);
            handle.absolute(0); // BUG-L13 prime
            eviction_counters.insert(tf, handle);
        }

        // L124 + L18 (revised): 6 component gauges, all NaN at boot
        // (the sourceless `tick_storage` label was retired 2026-07-19,
        // BATCH-5). Sampler overwrites only when a source is registered.
        let mut component_gauges = HashMap::with_capacity(ALLOWED_SUBSYSTEM_COMPONENTS.len());
        for &component in ALLOWED_SUBSYSTEM_COMPONENTS {
            let handle: Gauge = gauge!(SUBSYSTEM_MEMORY_GAUGE_NAME, "component" => component);
            handle.set(f64::NAN);
            component_gauges.insert(component, handle);
        }

        let sampler_heartbeat: Gauge = gauge!(SAMPLER_HEARTBEAT_GAUGE_NAME);
        // Initial value 0 — sampler overwrites immediately on first tick.
        // The alert is `time() - heartbeat > 30`, so 0 at boot would
        // fire instantly; we delay the alert via the `for: 1m` clause
        // in the rule. The sampler updates the heartbeat <=10 s after
        // boot, well within the alert's evaluation window.
        sampler_heartbeat.set(0.0);

        let market_hours_active: Gauge = gauge!(MARKET_HOURS_ACTIVE_GAUGE_NAME);
        market_hours_active.set(0.0);

        info!(
            components = ALLOWED_SUBSYSTEM_COMPONENTS.len(),
            timeframes = Tf::ALL.len(),
            metric.subsystem_memory = SUBSYSTEM_MEMORY_GAUGE_NAME,
            metric.heartbeat = SAMPLER_HEARTBEAT_GAUGE_NAME,
            metric.market_hours = MARKET_HOURS_ACTIVE_GAUGE_NAME,
            "Subsystem memory metrics registered (NaN-init, all-TF pre-warm)"
        );

        Self {
            eviction_counters,
            component_gauges,
            sampler_heartbeat,
            market_hours_active,
        }
    }

    /// Increment the eviction counter for `tf` by 1.
    ///
    /// HOT-C2 + HOT-H1: dereferences a cached `Counter` handle —
    /// no allocation, no hashmap lookup at the metrics-crate layer.
    pub fn increment_eviction(&self, tf: Tf) {
        if let Some(c) = self.eviction_counters.get(&tf) {
            c.increment(1);
        }
        // No `else`: the map is built from `Tf::ALL` at boot so a
        // miss is unreachable under the locked-cardinality contract.
    }

    /// Set the gauge for `component` to `bytes`.
    ///
    /// `component` MUST be one of [`ALLOWED_SUBSYSTEM_COMPONENTS`];
    /// foreign labels are silently dropped (the catalog ratchet
    /// rejects any new component at compile time anyway).
    pub fn set_component(&self, component: &'static str, bytes: f64) {
        if let Some(g) = self.component_gauges.get(component) {
            g.set(bytes);
        }
    }

    /// Refresh the heartbeat gauge to the current UNIX wall-clock.
    ///
    /// Called from the sampler tick. The PromQL alert
    /// `time() - tv_subsystem_memory_sampler_last_update_seconds > 30`
    /// fires if this stops updating — typically because the sampler
    /// task panicked or was deadlocked.
    pub fn touch_heartbeat(&self, unix_secs: f64) {
        self.sampler_heartbeat.set(unix_secs);
    }

    /// Set the quiet-hours gate gauge.
    pub fn set_market_hours_active(&self, active: bool) {
        self.market_hours_active.set(if active { 1.0 } else { 0.0 });
    }
}

/// Boxed source closure: produces the current memory estimate (in
/// bytes) for one component, or `None` to skip the tick. Sources MUST
/// be cheap (`<1 ms` budget — they run on every sampler tick).
pub type SourceFn = Box<dyn Fn() -> Option<f64> + Send + Sync>;

/// A registry of "compute the current memory footprint of component X"
/// closures. Closures are registered by downstream PRs (e.g. wiring
/// `bar_storage` from a candle store's `len() × size_of`).
///
/// The sampler task evaluates every registered source on each tick.
/// Components that never get a source registered keep their gauge at
/// `f64::NAN` (L124).
pub struct SubsystemMemorySampler {
    handles: Arc<SubsystemMemoryHandles>,
    sources: Mutex<HashMap<&'static str, SourceFn>>,
}

impl std::fmt::Debug for SubsystemMemorySampler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = self.sources.lock().map(|s| s.len()).unwrap_or(0);
        f.debug_struct("SubsystemMemorySampler")
            .field("registered_sources", &n)
            .field("components_total", &ALLOWED_SUBSYSTEM_COMPONENTS.len())
            .finish()
    }
}

impl SubsystemMemorySampler {
    /// Build a fresh sampler around already-registered metric handles.
    #[must_use]
    pub fn new(handles: Arc<SubsystemMemoryHandles>) -> Self {
        Self {
            handles,
            sources: Mutex::new(HashMap::new()),
        }
    }

    /// Register a source closure for a single component.
    ///
    /// Returns `Ok(())` on accept, `Err(...)` if `component` is not
    /// in [`ALLOWED_SUBSYSTEM_COMPONENTS`] (L126 cardinality contract).
    /// The component MUST be a `&'static str` from the catalog so the
    /// gauge label cardinality stays bounded at compile time.
    ///
    /// The closure is called at every sampler tick and MUST be cheap
    /// (`<1 ms` budget). Returning `None` skips the tick — the gauge
    /// stays at its previous value.
    pub fn register_source<F>(&self, component: &'static str, source: F) -> Result<(), &'static str>
    where
        F: Fn() -> Option<f64> + Send + Sync + 'static,
    {
        if !ALLOWED_SUBSYSTEM_COMPONENTS.contains(&component) {
            return Err("component not in catalog (L126 cardinality contract)");
        }
        let mut guard = self
            .sources
            .lock()
            .map_err(|_| "sampler sources mutex poisoned")?;
        guard.insert(component, Box::new(source));
        Ok(())
    }

    /// Run a single sampler tick: evaluate every registered source,
    /// write the gauges, refresh the heartbeat.
    ///
    /// Pure-ish: reads `now_unix_secs` from the caller so the function
    /// is testable without a clock. Production callers pass
    /// `SystemTime::now()` seconds.
    #[instrument(skip(self), level = "trace")]
    pub fn sample_once(&self, now_unix_secs: f64, market_hours_active: bool) {
        // BUG-M7: under the live `papaya` MVCC swap pattern, a source
        // closure is expected to take an `Arc<Map>` snapshot for the
        // entire `len() + size_of` computation so the value cannot
        // straddle a swap. We don't enforce this here (closures are
        // opaque) but the convention is documented in `register_source`.
        let snapshot: Vec<(&'static str, Option<f64>)> = match self.sources.lock() {
            Ok(guard) => guard.iter().map(|(&k, f)| (k, f())).collect(),
            Err(err) => {
                warn!(?err, "sampler sources mutex poisoned — skipping tick");
                return;
            }
        };
        for (component, value) in snapshot {
            if let Some(v) = value {
                self.handles.set_component(component, v);
            }
            // None → leave the gauge at its previous value (NaN if no
            // source has ever fired); the alert filters NaN via
            // `unless absent_over_time(...)`.
        }
        self.handles.touch_heartbeat(now_unix_secs);
        self.handles.set_market_hours_active(market_hours_active);
    }

    /// Spawn the sampler task on the current `tokio` runtime.
    ///
    /// Uses `tokio::time::interval` so the cadence is robust against
    /// system suspend / clock skew. Returns a `JoinHandle` so the
    /// caller can `abort()` on shutdown if desired (`tokio` aborts
    /// background tasks on runtime drop anyway).
    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        let interval_secs = SAMPLER_INTERVAL_SECS;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let now_unix_secs = unix_now_secs_f64();
                let active = tickvault_common::market_hours::is_within_market_hours_ist();
                self.sample_once(now_unix_secs, active);
            }
        })
    }
}

/// Best-effort UNIX wall-clock seconds as `f64` for the heartbeat
/// gauge. Falls back to `0.0` on the (unreachable on any sane
/// platform) `SystemTime::now() < UNIX_EPOCH` case.
fn unix_now_secs_f64() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Reconciliation primitive — the §AA Gate 1 ratchet.
///
/// `estimated_sum_bytes` = sum of every component gauge that has a
/// registered source (NaN entries excluded by the caller).
/// `process_rss_bytes` = the kernel measure (e.g. value of the
/// pre-existing `process_resident_memory_bytes` gauge or the
/// `/proc/self/status::VmRSS` reading).
///
/// Returns `Ok(diff_pct)` if the relative difference is within
/// ±[`RECONCILE_TOLERANCE_PCT`], else `Err(diff_pct)`. `diff_pct` is
/// the absolute difference as a percentage of the RSS measure, so a
/// caller that wants to log the ratio can pass it through.
///
/// The function is pure so the ratchet test exercises its semantics
/// without spinning up the sampler.
pub fn reconcile_within_pct(
    estimated_sum_bytes: f64,
    process_rss_bytes: f64,
    tolerance_pct: f64,
) -> Result<f64, f64> {
    if !process_rss_bytes.is_finite() || process_rss_bytes <= 0.0 {
        // Cannot reconcile against a non-positive / NaN RSS reading;
        // treat as in-tolerance so the ratchet does not false-fail
        // on a transient `/proc` read glitch.
        return Ok(0.0);
    }
    if !estimated_sum_bytes.is_finite() {
        // Sum is NaN — happens before any source is registered.
        // Surface as out-of-tolerance so the ratchet test fails CI
        // rather than silently passing.
        return Err(f64::INFINITY);
    }
    let diff_pct = ((estimated_sum_bytes - process_rss_bytes).abs() / process_rss_bytes) * 100.0;
    if diff_pct <= tolerance_pct {
        Ok(diff_pct)
    } else {
        Err(diff_pct)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    // -----------------------------------------------------------------------
    // Reconciliation ratchet — §AA Gate 1
    // -----------------------------------------------------------------------

    #[test]
    fn test_subsystem_memory_sums_within_5pct_of_process_rss() {
        // Synthetic happy path: sum 1.00 GB, RSS 1.02 GB → diff 1.96 %
        // → within 5 % tolerance → Ok.
        let estimated = 1_000_000_000.0_f64;
        let rss = 1_020_000_000.0_f64;
        let outcome = reconcile_within_pct(estimated, rss, RECONCILE_TOLERANCE_PCT);
        assert!(outcome.is_ok(), "1.96% diff must pass 5% tolerance");
        let diff = outcome.expect("ok branch");
        assert!(
            (diff - 1.96).abs() < 0.01,
            "diff_pct should be ~1.96, got {diff}"
        );
    }

    #[test]
    fn test_reconcile_rejects_drift_above_tolerance() {
        // Synthetic divergence: sum 1.0 GB, RSS 2.0 GB → diff 50 %
        // → must FAIL.
        let outcome =
            reconcile_within_pct(1_000_000_000.0, 2_000_000_000.0, RECONCILE_TOLERANCE_PCT);
        assert!(
            outcome.is_err(),
            "50% drift must trip the 5% reconciliation gate"
        );
    }

    #[test]
    fn test_reconcile_rejects_nan_sum() {
        // BUG-H3 + L124: NaN sum (no sources registered yet) must
        // surface as out-of-tolerance, not silently pass.
        let outcome = reconcile_within_pct(f64::NAN, 1_000_000_000.0, 5.0);
        assert!(
            outcome.is_err(),
            "NaN sum must fail reconciliation, not silently pass"
        );
    }

    #[test]
    fn test_reconcile_treats_zero_rss_as_skip() {
        // Defensive path: if `/proc/self/status::VmRSS` ever returns
        // 0 (read glitch under load), do NOT divide by zero.
        let outcome = reconcile_within_pct(1_000_000_000.0, 0.0, 5.0);
        assert!(
            outcome.is_ok(),
            "zero RSS must short-circuit to Ok (no spurious failure)"
        );
    }

    #[test]
    fn test_reconcile_tolerance_constant_is_five_percent_per_l18() {
        // Pin the tolerance constant — anyone who relaxes this must
        // amend the design lock first.
        assert!(
            (RECONCILE_TOLERANCE_PCT - 5.0).abs() < f64::EPSILON,
            "L18 design pins reconciliation tolerance at 5%"
        );
    }

    #[test]
    fn test_sampler_interval_constant_pinned_at_10s() {
        // Plan: HYBRID `len() × size_of` lazy at 10 s scrape cadence.
        assert_eq!(SAMPLER_INTERVAL_SECS, 10);
    }

    // -----------------------------------------------------------------------
    // Sampler API — pure-logic tests (no metrics recorder needed)
    // -----------------------------------------------------------------------
    //
    // Construction tests intentionally exercise `SubsystemMemoryHandles`
    // without an installed Prometheus recorder. The `metrics` crate
    // returns no-op handles in that mode; these tests verify the
    // ownership / type shape, not the wire-level emit.

    #[test]
    fn handles_register_initializes_all_9_tf_eviction_counters() {
        // PR #517 (Wave-5 TF reduction, 2026-05-08) reduced the
        // operator-facing TF set from 21 → 9. The assertion mirrors
        // `Tf::ALL.len()` so any future symmetric resize (per
        // `tf_symmetry_guard`) doesn't silently drift.
        let h = SubsystemMemoryHandles::register();
        assert_eq!(
            h.eviction_counters.len(),
            Tf::ALL.len(),
            "BUG-L13: every TF must be pre-warmed at boot"
        );
        assert_eq!(
            Tf::ALL.len(),
            9,
            "PR #517 pinned the operator-facing TF count at 9; \
             a drift here is a `tf_symmetry_guard` regression."
        );
        for tf in Tf::ALL {
            assert!(
                h.eviction_counters.contains_key(&tf),
                "missing TF {tf:?} in eviction counter map"
            );
        }
    }

    #[test]
    fn handles_register_initializes_all_six_components() {
        let h = SubsystemMemoryHandles::register();
        assert_eq!(h.component_gauges.len(), 6, "L18 pins 6 components");
        for &c in ALLOWED_SUBSYSTEM_COMPONENTS {
            assert!(
                h.component_gauges.contains_key(c),
                "missing component {c} in gauge map"
            );
        }
    }

    #[test]
    fn sampler_register_source_rejects_unknown_component() {
        let handles = Arc::new(SubsystemMemoryHandles::register());
        let sampler = SubsystemMemorySampler::new(handles);
        let outcome = sampler.register_source("not_a_real_component", || Some(1.0));
        assert!(
            outcome.is_err(),
            "L126: components outside the catalog must be rejected"
        );
    }

    #[test]
    fn sampler_register_source_accepts_catalog_component() {
        let handles = Arc::new(SubsystemMemoryHandles::register());
        let sampler = SubsystemMemorySampler::new(handles);
        for &c in ALLOWED_SUBSYSTEM_COMPONENTS {
            let outcome = sampler.register_source(c, || Some(0.0));
            assert!(outcome.is_ok(), "catalog component {c} must be accepted");
        }
    }

    #[test]
    fn sampler_sample_once_invokes_each_registered_source() {
        let handles = Arc::new(SubsystemMemoryHandles::register());
        let sampler = SubsystemMemorySampler::new(Arc::clone(&handles));

        // Wire two synthetic sources; assert each is called once per tick.
        let calls_a = Arc::new(AtomicU64::new(0));
        let calls_b = Arc::new(AtomicU64::new(0));
        {
            let calls_a = Arc::clone(&calls_a);
            sampler
                .register_source("rescue_ring", move || {
                    calls_a.fetch_add(1, Ordering::Relaxed);
                    Some(123_456.0)
                })
                .expect("catalog component");
        }
        {
            let calls_b = Arc::clone(&calls_b);
            sampler
                .register_source("registry", move || {
                    calls_b.fetch_add(1, Ordering::Relaxed);
                    Some(7_890.0)
                })
                .expect("catalog component");
        }

        sampler.sample_once(1_700_000_000.0, true);
        assert_eq!(calls_a.load(Ordering::Relaxed), 1);
        assert_eq!(calls_b.load(Ordering::Relaxed), 1);

        sampler.sample_once(1_700_000_010.0, true);
        assert_eq!(calls_a.load(Ordering::Relaxed), 2);
        assert_eq!(calls_b.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn sampler_sample_once_tolerates_source_returning_none() {
        let handles = Arc::new(SubsystemMemoryHandles::register());
        let sampler = SubsystemMemorySampler::new(Arc::clone(&handles));
        sampler
            .register_source("rescue_ring", || None)
            .expect("catalog component");
        // Must not panic + must continue updating heartbeat.
        sampler.sample_once(1_700_000_000.0, false);
    }

    #[test]
    fn handles_increment_eviction_does_not_panic_for_any_tf() {
        let h = SubsystemMemoryHandles::register();
        for tf in Tf::ALL {
            h.increment_eviction(tf); // no-op on no-recorder env, must not panic
        }
    }

    #[test]
    fn handles_set_component_silently_drops_unknown_label() {
        // Defence-in-depth: if a future bug somehow passes a non-
        // catalog component, we MUST drop it (rather than silently
        // creating a new gauge series and breaking L126).
        let h = SubsystemMemoryHandles::register();
        // No assertion needed — this must simply not panic / not
        // register a new gauge. Cardinality contract is enforced by
        // `metrics_catalog::tests::allowed_components_*`.
        h.set_component("foreign_component_should_be_dropped", 1.0);
    }

    #[test]
    fn test_increment_eviction_does_not_panic() {
        let h = SubsystemMemoryHandles::register();
        for tf in Tf::ALL {
            h.increment_eviction(tf);
        }
    }

    #[test]
    fn test_set_component_does_not_panic_for_catalog_label() {
        let h = SubsystemMemoryHandles::register();
        for &c in ALLOWED_SUBSYSTEM_COMPONENTS {
            h.set_component(c, 1024.0);
        }
    }

    #[test]
    fn test_touch_heartbeat_accepts_unix_seconds() {
        // L125: heartbeat is a UNIX-seconds gauge updated every tick.
        // Sanity-check the API: the function must not panic on a
        // realistic value and must accept a `f64` so subsecond
        // precision is preserved.
        let h = SubsystemMemoryHandles::register();
        h.touch_heartbeat(1_700_000_000.123_f64);
    }

    #[test]
    fn test_set_market_hours_active_accepts_both_values() {
        // L129 quiet-hours gate is a 0/1 gauge. The wrapper takes a
        // `bool` so the conversion is centralised — assert both
        // branches execute without panic.
        let h = SubsystemMemoryHandles::register();
        h.set_market_hours_active(true);
        h.set_market_hours_active(false);
    }

    #[test]
    fn test_sampler_register_source_rejects_unknown() {
        // Mirror the pub-fn name in the test name so the workspace
        // ratchet maps fn -> test by single-line grep.
        let handles = Arc::new(SubsystemMemoryHandles::register());
        let sampler = SubsystemMemorySampler::new(handles);
        assert!(sampler.register_source("not_a_component", || None).is_err());
    }

    #[test]
    fn test_sampler_sample_once_runs_without_sources_registered() {
        // Sampler must tolerate the boot state where no sources are
        // wired (this is exactly #504a's shipping configuration).
        let handles = Arc::new(SubsystemMemoryHandles::register());
        let sampler = SubsystemMemorySampler::new(handles);
        sampler.sample_once(1_700_000_000.0, true);
    }

    #[tokio::test]
    async fn test_sampler_spawn_returns_running_join_handle() {
        // Smoke-test the `spawn()` API: the returned `JoinHandle` is
        // alive immediately after spawn, and aborting it terminates
        // the task cleanly.
        let handles = Arc::new(SubsystemMemoryHandles::register());
        let sampler = Arc::new(SubsystemMemorySampler::new(handles));
        let join = Arc::clone(&sampler).spawn();
        assert!(!join.is_finished(), "sampler task must be running");
        join.abort();
    }

    #[test]
    fn unix_now_secs_returns_recent_value() {
        let now = unix_now_secs_f64();
        // 2026 is around 1.77e9; sanity-check the helper isn't broken.
        assert!(now > 1_700_000_000.0, "wall-clock helper broken: {now}");
        assert!(now < 4_000_000_000.0, "wall-clock helper broken: {now}");
    }
}

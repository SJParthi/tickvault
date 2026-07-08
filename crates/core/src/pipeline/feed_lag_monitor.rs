//! Dhan exchange→receive lag monitor — `tv_dhan_exchange_lag_p99_seconds`.
//!
//! Silent-feed alerting hardening Item 4 (2026-07-06 incident: the Dhan feed
//! degraded ALL day with exchange→receive lag p99 46s / max 199s and the
//! operator received ZERO pages — no exchange→received metric existed;
//! `tv_wire_to_done_duration_ns` measures receive→done only).
//!
//! # Architecture
//!
//! **Hot path (O(1), zero-alloc):** [`record_dhan_tick`] is called from the
//! two Dhan persist sites in `tick_processor.rs` (Ticker/Quote arm + Full
//! arm). It computes `lag_ns = received_at(IST) − exchange_timestamp(IST)`
//! and writes the sample into a preallocated single-writer 32,768-slot ring
//! (two relaxed atomic stores + one relaxed head bump). 32,768 slots ≈ 65 s
//! of headroom at the measured ~500 ticks/s; above ~546 ticks/s sustained the
//! trailing-60s window becomes best-effort (oldest samples overwritten — the
//! p99 remains a valid sample-based estimate of the window).
//!
//! **Cold path (honestly O(N), NEVER O(1)):** a supervised 10 s publisher
//! task ([`run_dhan_lag_publisher`], spawned from `start_dhan_lane` in
//! `main.rs` next to the SLO publisher) snapshots the trailing-60s window
//! into a preallocated scratch buffer and computes p99 via
//! `select_nth_unstable` — **O(N-window), N ≤ 32,768** — off the tick thread.
//!
//! # Clock alignment (CRITICAL — see `data-integrity.md`)
//!
//! `received_at_nanos` is UTC wall nanos; `exchange_timestamp` is Dhan LTT in
//! **IST** epoch seconds. The lag computation shifts the receive instant to
//! IST (`+ IST_UTC_OFFSET_NANOS` — the same convention as the stored
//! `received_at` column) before subtracting. Comparing UTC-vs-IST raw would
//! clamp every sample to 0 — a permanent "perfect lag" Rule-11 false-OK.
//!
//! # WAL-replay exclusion (exact discriminator, no Rule-11 censoring)
//!
//! A sample is admitted ONLY if
//! `received_at_nanos − capture_seq < REPLAY_EXCLUDE_DWELL_NANOS (60 s)`.
//! `capture_seq` is stamped ONCE at the original WS-read instant
//! (`ws_frame_spill::next_frame_seq` = `max(prev+1, wall_nanos)` ≈ UTC wall
//! nanos) and PRESERVED through WAL re-injection, while `received_at` is
//! RE-stamped at dequeue — so a replayed row shows receipt−capture = downtime
//! (≥ minutes, excluded EXACTLY), while a genuinely-lagged LIVE row (the
//! incident's real 46s/199s) has a FRESH capture instant and is KEPT. Every
//! exclusion increments `tv_dhan_lag_samples_excluded_total` (/metrics-only —
//! visible, never silent).
//!
//! # Publish gating (audit Rules 3 + 11)
//!
//! The gauge is set ONLY when (a) the wall clock is inside the trading
//! session persist window [09:00, 15:30) IST — Rule 3, prevents the
//! stale-gauge-after-close artifact — AND (b) the trailing-60s window holds
//! ≥ [`MIN_LAG_SAMPLES`] samples (an empty/thin window publishes NOTHING —
//! `0` would read as "perfect lag", a Rule-11 false-OK; feed-dead detection
//! is owned by the silent-instruments + WS alarms via `notBreaching`).
//! Honest envelope: when publishing stops, the /metrics exporter keeps
//! serving the LAST set value (the `metrics` facade cannot un-register a
//! gauge) — the CloudWatch window-gate Lambda (09:20–15:35 IST) + the
//! sibling feed-dead alarms own that tail, exactly as for the
//! silent-instruments gauge.
//!
//! # Quantization honesty (≥1 s floor)
//!
//! Dhan LTT is a u32 of WHOLE IST SECONDS (ticker packet bytes 12–15, quote
//! packet bytes 14–17) — the lag has a ≥1 s quantization floor. A healthy
//! p99 reads ~1–2 s and can NEVER read 0; sub-second wire lag is
//! UNMEASURABLE for feed=dhan. The CloudWatch alarm threshold (10 s) sits
//! ~10× above this floor. The gauge name carries `_seconds` because
//! millisecond units would imply false precision.
//!
//! The metric name is deliberately dhan-only (`tv_dhan_…`, unlabeled): the
//! CloudWatch EMF export folds label dimensions under host-only dims, so a
//! per-feed LABEL would silently fold Dhan and Groww series together. A
//! future Groww lag gauge gets its OWN name.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use tickvault_common::constants::{
    IST_UTC_OFFSET_NANOS, IST_UTC_OFFSET_SECONDS_I64, SECONDS_PER_DAY,
    TICK_PERSIST_END_SECS_OF_DAY_IST, TICK_PERSIST_START_SECS_OF_DAY_IST,
};

/// Ring capacity — ~65 s of headroom at the measured ~500 ticks/s Dhan rate.
/// Above ~546 ticks/s sustained the trailing-60s window is best-effort
/// (oldest samples overwritten; the p99 stays a valid sample estimate).
const RING_SLOTS: usize = 32_768;

/// WAL-replay exclusion dwell: a tick whose `received_at − capture_seq`
/// is ≥ this is a re-injected (replayed) row, not a live one. 60 s is far
/// above any live-path dequeue dwell (the frame channel drains in
/// milliseconds; even the incident's worst LIVE lag was 199 s of
/// exchange→receive wire lag with a FRESH capture instant) and far below
/// any real WAL-replay downtime (≥ minutes). Strict `<`: a row dwelling
/// exactly 60.000000000 s is EXCLUDED.
const REPLAY_EXCLUDE_DWELL_NANOS: i64 = 60_000_000_000;

/// Trailing window the publisher aggregates over. Same magnitude as the
/// dwell above but a SEPARATE concern (kept as its own constant).
const LAG_WINDOW_NANOS: i64 = 60_000_000_000;

/// Minimum samples in the trailing window before the gauge is published.
/// Below this, publish NOTHING (Rule 11 — a thin/empty window must not
/// read as "perfect lag").
const MIN_LAG_SAMPLES: usize = 50;

/// Publisher cadence (seconds). Mirrors the SLO publisher's 10 s tick.
const PUBLISH_INTERVAL_SECS: u64 = 10;

const NANOS_PER_SEC: i64 = 1_000_000_000;

/// Outcome of one hot-path lag observation (returned for unit-test
/// visibility; the metrics side effects live in [`record_dhan_tick`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LagRecordOutcome {
    /// Sample admitted into the ring. `clamped` = the raw lag was negative
    /// (host clock behind Dhan's whole-second stamp / ≤2 s BOOT-03 skew)
    /// and was clamped to 0.
    Admitted { clamped: bool },
    /// Sample excluded by the WAL-replay dwell discriminator — the row's
    /// receipt−capture gap says it was re-injected, not live.
    ExcludedReplay,
}

/// Preallocated single-writer lag ring. The ONLY writer is the Dhan tick
/// processor task (one task per process; a D2b lane restart tears the old
/// task down before spawning a new one). The publisher task is the only
/// reader. All slot accesses are relaxed atomics: a racing read can at
/// worst pair a fresh receive-instant with a stale lag value for ONE
/// sample out of ≥50 — statistically negligible for a p99, and a
/// stale/zero-init slot fails the recency filter.
struct FeedLagRing {
    /// Clamped lag samples in nanoseconds, indexed by `head % RING_SLOTS`.
    lag_ns: Box<[AtomicU64]>,
    /// UTC receive instant (nanos) per slot — the trailing-window key.
    /// `0` = never written (zero-init slots are filtered out).
    recv_utc_nanos: Box<[AtomicI64]>,
    /// Monotonic slot counter (relaxed; single writer).
    head: AtomicU64,
}

impl FeedLagRing {
    /// Allocates the two 32,768-slot arrays ONCE (cold path — first use at
    /// boot via the `OnceLock` global). The hot-path `observe` allocates
    /// nothing.
    fn new() -> Self {
        let mut lag = Vec::with_capacity(RING_SLOTS);
        let mut recv = Vec::with_capacity(RING_SLOTS);
        for _ in 0..RING_SLOTS {
            lag.push(AtomicU64::new(0));
            recv.push(AtomicI64::new(0));
        }
        Self {
            lag_ns: lag.into_boxed_slice(),
            recv_utc_nanos: recv.into_boxed_slice(),
            head: AtomicU64::new(0),
        }
    }

    /// Hot-path observation: O(1), zero-alloc (two relaxed stores + one
    /// relaxed head bump). See the module doc for the clock-alignment and
    /// replay-exclusion contracts.
    fn observe(
        &self,
        received_at_utc_nanos: i64,
        capture_seq_nanos: i64,
        exchange_ts_secs: u32,
    ) -> LagRecordOutcome {
        // WAL-replay dwell discriminator (strict `<`: exactly 60 s is
        // EXCLUDED — pinned by test_replay_dwell_boundary_excludes_at_exactly_60s).
        if received_at_utc_nanos.saturating_sub(capture_seq_nanos) >= REPLAY_EXCLUDE_DWELL_NANOS {
            return LagRecordOutcome::ExcludedReplay;
        }
        // Clock alignment: receive instant UTC→IST so both operands are IST
        // (exchange_timestamp is Dhan LTT = IST epoch seconds). Comparing
        // UTC-vs-IST raw would clamp EVERY sample to 0 (false "perfect lag").
        let received_ist_nanos = received_at_utc_nanos.saturating_add(IST_UTC_OFFSET_NANOS);
        let exchange_ist_nanos = i64::from(exchange_ts_secs).saturating_mul(NANOS_PER_SEC);
        let raw_lag = received_ist_nanos.saturating_sub(exchange_ist_nanos);
        let clamped = raw_lag < 0;
        // Negative lag (Dhan whole-second stamping ahead of a skewed host
        // clock, ≤2 s per BOOT-03) clamps to 0 — never a panic, never a
        // negative sample. The clamp is COUNTED by the caller.
        let lag_ns = u64::try_from(raw_lag).unwrap_or(0);

        let head = self.head.load(Ordering::Relaxed);
        // Modulo of a fixed power-of-two capacity — always in bounds.
        #[allow(clippy::cast_possible_truncation)]
        // APPROVED: head % RING_SLOTS < 32_768 always fits usize
        let idx = (head % RING_SLOTS as u64) as usize;
        self.recv_utc_nanos[idx].store(received_at_utc_nanos, Ordering::Relaxed);
        self.lag_ns[idx].store(lag_ns, Ordering::Relaxed);
        self.head.store(head.wrapping_add(1), Ordering::Relaxed);
        LagRecordOutcome::Admitted { clamped }
    }

    /// Snapshot every sample received within the trailing 60 s into
    /// `scratch`.
    ///
    /// # Performance
    /// **O(N), N ≤ 32,768 — NOT O(1); cold path** (runs on the 10 s
    /// publisher task, never on the tick thread). `scratch` is preallocated
    /// with `RING_SLOTS` capacity by the caller, so the pushes never
    /// allocate.
    fn snapshot_window_into(&self, now_utc_nanos: i64, scratch: &mut Vec<u64>) {
        scratch.clear();
        let head = self.head.load(Ordering::Relaxed);
        let filled = usize::try_from(head.min(RING_SLOTS as u64)).unwrap_or(RING_SLOTS);
        for i in 0..filled {
            let recv = self.recv_utc_nanos[i].load(Ordering::Relaxed);
            if recv > 0 && now_utc_nanos.saturating_sub(recv) <= LAG_WINDOW_NANOS {
                scratch.push(self.lag_ns[i].load(Ordering::Relaxed));
            }
        }
    }
}

/// Process-global ring for the Dhan feed (house `OnceLock` global pattern —
/// mirrors `prev_close_writer::GLOBAL` / `global_tick_gap_detector`).
/// A global avoids threading a new parameter through both
/// `run_tick_processor` boot call sites; the module is dhan-only by
/// construction (only the Dhan persist sites call [`record_dhan_tick`]).
static DHAN_LAG_RING: OnceLock<FeedLagRing> = OnceLock::new();

fn global_ring() -> &'static FeedLagRing {
    DHAN_LAG_RING.get_or_init(FeedLagRing::new)
}

/// Hot-path entry point — called from the two Dhan persist sites in
/// `tick_processor.rs` for every LIVE, in-session, dedup-passed tick.
///
/// # Performance
/// O(1), zero-alloc after the one-time ring init (two relaxed atomic
/// stores + one relaxed head bump; DHAT-ratcheted by
/// `crates/core/tests/dhat_feed_lag_ring.rs`, Criterion-budgeted by
/// `feed_lag_record_dhan_tick` in `quality/benchmark-budgets.toml`).
pub fn record_dhan_tick(received_at_utc_nanos: i64, capture_seq_nanos: i64, exchange_ts_secs: u32) {
    match global_ring().observe(received_at_utc_nanos, capture_seq_nanos, exchange_ts_secs) {
        LagRecordOutcome::ExcludedReplay => {
            // Rule 11: exclusions are VISIBLE (/metrics-only counter),
            // never silent censoring.
            metrics::counter!("tv_dhan_lag_samples_excluded_total").increment(1);
        }
        LagRecordOutcome::Admitted { clamped: true } => {
            metrics::counter!("tv_dhan_lag_negative_clamped_total").increment(1);
        }
        LagRecordOutcome::Admitted { clamped: false } => {}
    }
}

/// p99 over a snapshot window via `select_nth_unstable`.
///
/// Returns `None` when the window holds fewer than [`MIN_LAG_SAMPLES`]
/// samples (thin-window Rule-11 gate — the caller must publish NOTHING).
///
/// # Performance
/// **O(N-window), N ≤ 32,768 — NOT O(1); cold path** (10 s publisher
/// cadence, off the tick thread). Reorders `window` in place (that is why
/// it takes `&mut`).
pub fn compute_window_p99_ns(window: &mut [u64]) -> Option<u64> {
    let n = window.len();
    if n < MIN_LAG_SAMPLES {
        return None;
    }
    // p99 rank (1-based ceil(0.99·n)) → 0-based index. n ≤ 32,768 so the
    // multiply cannot overflow.
    let idx = (n * 99).div_ceil(100).saturating_sub(1);
    let (_, value, _) = window.select_nth_unstable(idx);
    Some(*value)
}

/// Rule 3 session gate: true iff the UTC wall clock falls inside the
/// [09:00, 15:30) IST persist window — the same window that gates the
/// producing persist sites, so the gauge can never be armed by data the
/// pipeline itself refuses to persist.
fn is_in_session_ist(now_utc_secs: i64) -> bool {
    let now_ist = now_utc_secs.saturating_add(IST_UTC_OFFSET_SECONDS_I64);
    let sec_of_day = now_ist.rem_euclid(i64::from(SECONDS_PER_DAY));
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    // APPROVED: rem_euclid(86_400) is always in [0, 86_400) — fits u32, non-negative
    let sec_of_day = sec_of_day as u32;
    // O(1) EXEMPT: Range::contains is two integer comparisons, not a Vec scan.
    (TICK_PERSIST_START_SECS_OF_DAY_IST..TICK_PERSIST_END_SECS_OF_DAY_IST).contains(&sec_of_day)
}

/// Pure publish decision: `Some(p99_seconds)` ONLY when in-session AND the
/// window is thick enough; `None` = publish NOTHING (never 0 — Rule 11).
fn compute_publish_value(in_session: bool, window: &mut [u64]) -> Option<f64> {
    if !in_session {
        return None;
    }
    compute_window_p99_ns(window).map(|ns| {
        #[allow(clippy::cast_precision_loss)]
        // APPROVED: realistic lag ns (≤ hours ≈ 1e13) is exactly representable in f64
        let secs = ns as f64 / 1e9;
        secs
    })
}

/// The supervised 10 s publisher loop — spawned from `start_dhan_lane` in
/// `main.rs` (next to the SLO publisher) via
/// `spawn_supervised_feed_lag_publisher`, which respawns it on death
/// (WS-GAP-05 / SLO-03 supervisor pattern) and counts respawns.
///
/// Every decision inside is a unit-tested pure fn
/// ([`compute_window_p99_ns`], `compute_publish_value`, `is_in_session_ist`);
/// the loop itself is a scheduler wrapper. The trailing-window snapshot +
/// p99 are honestly **O(N-window), N ≤ 32,768** per tick of this COLD task —
/// never claimed O(1).
// TEST-EXEMPT: infinite tokio scheduler loop — every decision is a
// unit-tested pure fn above; supervision + boot wiring are pinned by
// `test_feed_lag_publisher_supervisor_is_wired_into_main` (secret_manager.rs).
pub async fn run_dhan_lag_publisher() {
    let mut scratch: Vec<u64> = Vec::with_capacity(RING_SLOTS);
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(PUBLISH_INTERVAL_SECS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let now = chrono::Utc::now();
        let now_utc_nanos = now.timestamp_nanos_opt().unwrap_or(0);
        global_ring().snapshot_window_into(now_utc_nanos, &mut scratch);
        if let Some(p99_secs) =
            compute_publish_value(is_in_session_ist(now.timestamp()), &mut scratch)
        {
            // ≥1 s Dhan LTT quantization floor: a healthy value reads
            // ~1–2 s and can never read 0 (see module doc).
            metrics::gauge!("tv_dhan_exchange_lag_p99_seconds").set(p99_secs);
        }
        // Out-of-session / thin window: publish NOTHING. The exporter keeps
        // serving the last set value — the window-gate Lambda + the
        // silent-instruments/WS alarms own that tail (module doc, Rule 11).
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A capture instant + receive instant pair representing a LIVE tick:
    /// receipt 1 ms after capture (typical in-process dequeue dwell).
    const LIVE_DWELL_NANOS: i64 = 1_000_000;

    /// 2026-07-06 ~10:00 IST expressed as UTC nanos (04:30 UTC).
    /// 2026-07-06 00:00 UTC = 1_782_950_400 epoch secs.
    const T0_UTC_SECS: i64 = 1_782_950_400 + 4 * 3600 + 1800;
    const T0_UTC_NANOS: i64 = T0_UTC_SECS * NANOS_PER_SEC;

    /// IST epoch seconds matching `T0_UTC_SECS` exactly (zero lag).
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    // APPROVED: test constant, value ≈ 1.78e9 fits u32
    const T0_EXCHANGE_IST_SECS: u32 = (T0_UTC_SECS + IST_UTC_OFFSET_SECONDS_I64) as u32;

    #[test]
    fn test_replay_dwell_boundary_excludes_at_exactly_60s() {
        let ring = FeedLagRing::new();
        // receipt − capture == EXACTLY 60 s → EXCLUDED (strict `<`).
        let capture = T0_UTC_NANOS - REPLAY_EXCLUDE_DWELL_NANOS;
        assert_eq!(
            ring.observe(T0_UTC_NANOS, capture, T0_EXCHANGE_IST_SECS),
            LagRecordOutcome::ExcludedReplay,
            "a row dwelling exactly 60.000000000s must be EXCLUDED"
        );
        // One nano under the dwell → ADMITTED.
        assert_eq!(
            ring.observe(T0_UTC_NANOS, capture + 1, T0_EXCHANGE_IST_SECS),
            LagRecordOutcome::Admitted { clamped: false },
            "a row dwelling 59.999999999s must be ADMITTED"
        );
        // Exclusion must NOT write the ring: only the admitted sample lands.
        let mut scratch = Vec::with_capacity(RING_SLOTS);
        ring.snapshot_window_into(T0_UTC_NANOS, &mut scratch);
        assert_eq!(scratch.len(), 1, "excluded sample must not enter the ring");
    }

    #[test]
    fn test_genuine_lag_kept_with_fresh_capture_instant() {
        // The incident's real 46s exchange→receive lag: exchange stamp 46s
        // in the past, but capture instant FRESH (live socket read) → KEPT.
        let ring = FeedLagRing::new();
        let exchange_secs = T0_EXCHANGE_IST_SECS - 46;
        assert_eq!(
            ring.observe(T0_UTC_NANOS, T0_UTC_NANOS - LIVE_DWELL_NANOS, exchange_secs),
            LagRecordOutcome::Admitted { clamped: false },
        );
        let mut scratch = Vec::with_capacity(RING_SLOTS);
        ring.snapshot_window_into(T0_UTC_NANOS, &mut scratch);
        assert_eq!(scratch.as_slice(), &[46 * NANOS_PER_SEC as u64]);
    }

    #[test]
    fn test_negative_lag_clamped() {
        // Exchange stamp 2s AHEAD of the (IST-aligned) receive instant —
        // BOOT-03-class host skew. Clamped to 0, counted, never negative,
        // never a panic.
        let ring = FeedLagRing::new();
        let exchange_ahead_secs = T0_EXCHANGE_IST_SECS + 2;
        assert_eq!(
            ring.observe(
                T0_UTC_NANOS,
                T0_UTC_NANOS - LIVE_DWELL_NANOS,
                exchange_ahead_secs
            ),
            LagRecordOutcome::Admitted { clamped: true },
        );
        let mut scratch = Vec::with_capacity(RING_SLOTS);
        ring.snapshot_window_into(T0_UTC_NANOS, &mut scratch);
        assert_eq!(scratch.as_slice(), &[0], "negative lag must clamp to 0");
    }

    #[test]
    fn test_p99_on_known_distribution() {
        // 100 samples of 1..=100 seconds: p99 rank = ceil(0.99·100) = 99 →
        // 0-based idx 98 → value 99 s.
        let mut window: Vec<u64> = (1..=100u64).map(|s| s * NANOS_PER_SEC as u64).collect();
        assert_eq!(
            compute_window_p99_ns(&mut window),
            Some(99 * NANOS_PER_SEC as u64)
        );
        // Exactly MIN_LAG_SAMPLES uniform samples: p99 idx = ceil(49.5)−1 =
        // 49 → the max of 1..=50 = 50 s.
        let mut window: Vec<u64> = (1..=50u64).map(|s| s * NANOS_PER_SEC as u64).collect();
        assert_eq!(
            compute_window_p99_ns(&mut window),
            Some(50 * NANOS_PER_SEC as u64)
        );
    }

    #[test]
    fn test_thin_window_publishes_nothing() {
        // 49 samples (< MIN_LAG_SAMPLES = 50) → None even in-session.
        let mut thin: Vec<u64> = vec![NANOS_PER_SEC as u64; MIN_LAG_SAMPLES - 1];
        assert_eq!(compute_window_p99_ns(&mut thin), None);
        assert_eq!(
            compute_publish_value(true, &mut thin),
            None,
            "thin window must publish NOTHING — 0 would be a Rule-11 false-OK"
        );
        // Empty window: same.
        let mut empty: Vec<u64> = Vec::new();
        assert_eq!(compute_publish_value(true, &mut empty), None);
        // 50 samples → publishes.
        let mut thick: Vec<u64> = vec![2 * NANOS_PER_SEC as u64; MIN_LAG_SAMPLES];
        assert_eq!(compute_publish_value(true, &mut thick), Some(2.0));
    }

    #[test]
    fn test_out_of_session_publishes_nothing() {
        // A thick window out of session → None (Rule 3 gate).
        let mut thick: Vec<u64> = vec![2 * NANOS_PER_SEC as u64; MIN_LAG_SAMPLES];
        assert_eq!(compute_publish_value(false, &mut thick), None);

        // Session-window boundary checks on the pure gate. 2026-07-06 is a
        // Monday; midnight UTC = 05:30 IST.
        let ist_midnight_utc = 1_782_950_400 - IST_UTC_OFFSET_SECONDS_I64;
        // 08:59:59 IST — out.
        assert!(!is_in_session_ist(ist_midnight_utc + 9 * 3600 - 1));
        // 09:00:00 IST — in (persist-window start).
        assert!(is_in_session_ist(ist_midnight_utc + 9 * 3600));
        // 15:29:59 IST — in.
        assert!(is_in_session_ist(
            ist_midnight_utc + 15 * 3600 + 30 * 60 - 1
        ));
        // 15:30:00 IST — out (exclusive end; the 15:30→16:30 scrape tail is
        // the stale-gauge artifact the window-gate Lambda absorbs).
        assert!(!is_in_session_ist(ist_midnight_utc + 15 * 3600 + 30 * 60));
    }

    #[test]
    fn test_ring_wraparound() {
        let ring = FeedLagRing::new();
        // RING_SLOTS + 100 admitted samples: the first 100 are overwritten.
        // Give the overwriting generation a distinct lag value (2 s vs 1 s).
        let total = RING_SLOTS + 100;
        for i in 0..total {
            let lag_secs: u32 = if i < 100 { 1 } else { 2 };
            let exchange = T0_EXCHANGE_IST_SECS - lag_secs;
            ring.observe(T0_UTC_NANOS, T0_UTC_NANOS - LIVE_DWELL_NANOS, exchange);
        }
        let mut scratch = Vec::with_capacity(RING_SLOTS);
        ring.snapshot_window_into(T0_UTC_NANOS, &mut scratch);
        assert_eq!(
            scratch.len(),
            RING_SLOTS,
            "window is capped at ring capacity (best-effort above ~546 ticks/s)"
        );
        // Every surviving sample is from the 2s generation — the oldest 100
        // (1s) were overwritten in place.
        let two_secs = 2 * NANOS_PER_SEC as u64;
        assert!(
            scratch.iter().all(|&l| l == two_secs),
            "oldest samples must be overwritten on wraparound"
        );
    }

    #[test]
    fn test_snapshot_window_filters_stale_receives() {
        let ring = FeedLagRing::new();
        // One sample received 61 s before "now" → outside the trailing-60s
        // window → filtered; one fresh sample survives.
        let stale_recv = T0_UTC_NANOS - LAG_WINDOW_NANOS - NANOS_PER_SEC;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        // APPROVED: test constant fits u32
        let stale_exchange = ((stale_recv / NANOS_PER_SEC) + IST_UTC_OFFSET_SECONDS_I64) as u32;
        ring.observe(stale_recv, stale_recv - LIVE_DWELL_NANOS, stale_exchange);
        ring.observe(
            T0_UTC_NANOS,
            T0_UTC_NANOS - LIVE_DWELL_NANOS,
            T0_EXCHANGE_IST_SECS - 3,
        );
        let mut scratch = Vec::with_capacity(RING_SLOTS);
        ring.snapshot_window_into(T0_UTC_NANOS, &mut scratch);
        assert_eq!(scratch.as_slice(), &[3 * NANOS_PER_SEC as u64]);
    }

    #[test]
    fn test_record_dhan_tick_smoke_on_global_ring() {
        // Exercises the pub wrapper end-to-end on the process-global ring
        // (no assertions on global contents — other tests never read the
        // global, and the DHAT/Criterion targets run in separate
        // processes). An excluded sample must not panic and must not write.
        record_dhan_tick(
            T0_UTC_NANOS,
            T0_UTC_NANOS - REPLAY_EXCLUDE_DWELL_NANOS,
            T0_EXCHANGE_IST_SECS,
        );
        // Admitted path (fresh capture) must not panic either.
        record_dhan_tick(
            T0_UTC_NANOS,
            T0_UTC_NANOS - LIVE_DWELL_NANOS,
            T0_EXCHANGE_IST_SECS,
        );
    }
}

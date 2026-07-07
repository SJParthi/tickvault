//! Wave 3-B Item 11 — Telegram bucket coalescer.
//!
//! Coalesces bursts of low-severity notification events into a single
//! summary message per `(topic, severity)` bucket per window. Saves
//! operator pager fatigue when a noisy subsystem fires the same event
//! 100× in 60 seconds.
//!
//! # Severity routing (2026-07-07 UX overhaul)
//!
//! - [`Severity::Critical`] — BYPASS (sent immediately, NEVER batched)
//! - [`Severity::High`]     — BYPASS (sent immediately, NEVER batched)
//! - [`Severity::Medium`]   — COALESCE (2026-07-07 operator directive:
//!   Medium chatter joins the in-market digest; escape hatches =
//!   `DispatchPolicy::Immediate` per event or `digest_window_secs = 60`)
//! - [`Severity::Low`]      — COALESCE
//! - [`Severity::Info`]     — COALESCE
//!
//! The lane decision itself lives in [`classify_dispatch`] — in market
//! hours the batching lane is the `digest_window_secs` digest; off-hours
//! it is the legacy 60s per-topic window.
//!
//! # Window
//!
//! Default 60s, configurable via [`CoalescerConfig::window`]. The first
//! event per `(topic, severity)` opens a window; subsequent events into
//! the same bucket fold into it. The background flush task drains all
//! windows whose age ≥ `window` once every `flush_interval`.
//!
//! # Sample cap
//!
//! Each bucket retains up to [`MAX_SAMPLES_PER_BUCKET`] payload strings
//! verbatim for the summary message. The 11th and onwards are counted
//! but their payload is not preserved (counter
//! `tv_telegram_dropped_total{reason="coalesced_sample_capped"}` increments).
//! The `count` field is authoritative across the full window.
//!
//! # Performance
//!
//! Notifications are cold path; this module is NOT hot path per
//! `crates/core/src/notification/service.rs:22`. The bypass path
//! (Critical/High/Medium) is a single comparison + return — DHAT
//! zero-alloc-pinned by `crates/core/tests/dhat_telegram_dispatcher.rs`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arrayvec::ArrayVec;
use metrics::counter;

use super::episode::EpisodeRole;
use super::events::{DispatchPolicy, Severity};

/// Maximum number of verbatim sample payloads retained per bucket per window.
///
/// Beyond this cap, additional events are still counted (the `count` is
/// authoritative) but their payload string is dropped. The summary
/// message indicates this with `+N more (capped)`.
pub const MAX_SAMPLES_PER_BUCKET: usize = 10;

/// Default coalesce window — bursts within this duration fold into one summary.
pub const DEFAULT_WINDOW_SECS: u64 = 60;

/// Default flush-interval — how often the drain task wakes to emit summaries.
///
/// Set to `DEFAULT_WINDOW_SECS / 6` so windows close within ~10s of their
/// nominal age. Tighter than this is noise, looser causes summary lag.
pub const DEFAULT_FLUSH_INTERVAL_SECS: u64 = 10;

/// Default in-market digest window (Telegram UX overhaul 2026-07-07):
/// LOW/MEDIUM chatter batches into ONE digest bubble per 15 minutes during
/// market hours. Overridable via `[notification] digest_window_secs`
/// (clamped [60, 3600]; 60 == legacy behavior).
pub const DEFAULT_DIGEST_WINDOW_SECS: u64 = 900;

/// Maximum per-topic lines rendered in one digest bubble; the remainder is
/// summarized with a `(+M more)` marker.
pub const DIGEST_MAX_TOPIC_LINES: usize = 20;

/// Bucket key — `(topic, severity)` pair. Both Copy → zero allocation
/// per insert / lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BucketKey {
    pub topic: &'static str,
    pub severity: Severity,
}

/// Coalescer configuration.
#[derive(Debug, Clone, Copy)]
pub struct CoalescerConfig {
    /// Window duration OUTSIDE market hours. Bursts within this fold into
    /// one summary message (legacy 60s per-topic behavior).
    pub window: Duration,
    /// How often the background flush task wakes to drain mature windows.
    pub flush_interval: Duration,
    /// Window duration DURING market hours (the LOW/MEDIUM digest —
    /// Telegram UX overhaul 2026-07-07). Sourced from
    /// `[notification] digest_window_secs`, clamped [60, 3600].
    pub market_hours_window: Duration,
}

impl Default for CoalescerConfig {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(DEFAULT_WINDOW_SECS),
            flush_interval: Duration::from_secs(DEFAULT_FLUSH_INTERVAL_SECS),
            market_hours_window: Duration::from_secs(DEFAULT_DIGEST_WINDOW_SECS),
        }
    }
}

/// Which dispatch lane an event takes (Telegram UX overhaul 2026-07-07).
///
/// The routing law, exhaustively pinned by
/// `test_classify_dispatch_high_critical_never_digest_full_matrix`:
/// Critical/High can NEVER land in a batching lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchLane {
    /// Send immediately (the preserved bypass path; SNS-SMS rides it for
    /// High/Critical).
    Immediate,
    /// The event belongs to a live episode bubble — the episode FSM owns it.
    EpisodeEdit,
    /// Batch into the in-market LOW/MEDIUM digest window.
    Digest,
    /// Off-hours: today's legacy 60s per-topic coalescing.
    Coalesce60,
}

/// Pure dispatch-lane classifier.
///
/// - [`DispatchPolicy::Immediate`] → `Immediate` always (green boot pings
///   unchanged).
/// - An episode-mapped event → `EpisodeEdit` (the FSM decides send-vs-edit;
///   its first page IS an immediate send).
/// - `Critical` / `High` → `Immediate` — NEVER a batching lane.
/// - `Medium` / `Low` / `Info` → `Digest` in market hours, `Coalesce60`
///   off-hours.
#[must_use]
pub fn classify_dispatch(
    severity: Severity,
    policy: DispatchPolicy,
    episode: Option<EpisodeRole>,
    in_market_hours: bool,
) -> DispatchLane {
    if matches!(policy, DispatchPolicy::Immediate) {
        return DispatchLane::Immediate;
    }
    if episode.is_some() {
        return DispatchLane::EpisodeEdit;
    }
    match severity {
        Severity::Critical | Severity::High => DispatchLane::Immediate,
        Severity::Medium | Severity::Low | Severity::Info => {
            if in_market_hours {
                DispatchLane::Digest
            } else {
                DispatchLane::Coalesce60
            }
        }
    }
}

/// Pure: the effective drain window for the current market phase.
#[must_use]
pub fn effective_drain_window(in_market_hours: bool, config: &CoalescerConfig) -> Duration {
    if in_market_hours {
        config.market_hours_window
    } else {
        config.window
    }
}

/// Pure: force-drain fires exactly on the in-market → off-market falling
/// edge (the 15:30 IST close boundary) so no digest straddles overnight.
#[must_use]
pub fn should_force_drain(prev_in_market: bool, now_in_market: bool) -> bool {
    prev_in_market && !now_in_market
}

/// Renders ONE in-market digest bubble from the drained summaries.
///
/// Header: `🔵 15-min digest (10:00–10:15 AM IST)`; one `• <topic> xN`
/// line per bucket; capped at [`DIGEST_MAX_TOPIC_LINES`] lines with a
/// `(+M more)` marker. Plain English per the 10 Telegram commandments.
#[must_use]
pub fn render_digest(
    summaries: &[DrainedSummary],
    window_start_ms: u64,
    window_end_ms: u64,
) -> String {
    const MS_PER_MIN: u64 = 60_000;
    let mins = window_end_ms
        .saturating_sub(window_start_ms)
        .checked_div(MS_PER_MIN)
        .unwrap_or(0)
        .max(1);
    let start = super::episode::format_ist_12h(window_start_ms);
    let end = super::episode::format_ist_12h(window_end_ms);
    let mut out = format!("\u{1f535} {mins}-min digest ({start}\u{2013}{end} IST)");
    for summary in summaries.iter().take(DIGEST_MAX_TOPIC_LINES) {
        out.push_str("\n\u{2022} ");
        out.push_str(summary.topic);
        out.push_str(" x");
        out.push_str(&summary.count.to_string());
    }
    if summaries.len() > DIGEST_MAX_TOPIC_LINES {
        let hidden = summaries.len().saturating_sub(DIGEST_MAX_TOPIC_LINES);
        out.push_str("\n(+");
        out.push_str(&hidden.to_string());
        out.push_str(" more)");
    }
    out
}

/// Internal per-bucket state.
#[derive(Debug)]
struct BucketState {
    /// Total events folded into this bucket (all of them, not just retained samples).
    count: u64,
    /// Wall-clock millisecond timestamp of the first event in this window.
    first_ts_ms: u64,
    /// Wall-clock millisecond timestamp of the most recent event in this window.
    last_ts_ms: u64,
    /// Up to MAX_SAMPLES_PER_BUCKET retained payload strings (rendered messages).
    samples: ArrayVec<String, MAX_SAMPLES_PER_BUCKET>,
}

impl BucketState {
    fn new(now_ms: u64, payload: String) -> Self {
        let mut samples = ArrayVec::new();
        // ArrayVec::push panics on overflow; we just constructed it empty.
        samples.push(payload);
        Self {
            count: 1,
            first_ts_ms: now_ms,
            last_ts_ms: now_ms,
            samples,
        }
    }

    fn fold(&mut self, now_ms: u64, payload: String) -> bool {
        self.count = self.count.saturating_add(1);
        self.last_ts_ms = now_ms;
        // Only retain up to MAX_SAMPLES_PER_BUCKET payloads verbatim.
        // Beyond that, the count is authoritative but the string is dropped.
        if self.samples.len() < MAX_SAMPLES_PER_BUCKET {
            // SAFETY: length checked above; try_push handles the cap defensively.
            // The Result is intentionally discarded — the cap is the contract,
            // a full ArrayVec on this path is not an error.
            if self.samples.try_push(payload).is_err() {
                return false;
            }
            true
        } else {
            false
        }
    }

    fn age(&self, now_ms: u64) -> Duration {
        Duration::from_millis(now_ms.saturating_sub(self.first_ts_ms))
    }
}

/// Decision returned by [`TelegramCoalescer::observe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoalesceDecision {
    /// Event severity bypasses coalescing — caller MUST send immediately.
    Bypass,
    /// Event was folded into a coalescing bucket — caller MUST NOT send now.
    /// The drain task will emit the summary when the window closes.
    Coalesced,
}

/// Drained summary record — one per closed window per bucket.
#[derive(Debug, Clone)]
pub struct DrainedSummary {
    pub topic: &'static str,
    pub severity: Severity,
    pub count: u64,
    pub first_ts_ms: u64,
    pub last_ts_ms: u64,
    pub samples: Vec<String>,
    /// `true` when the count exceeded the sample cap (≥ MAX_SAMPLES_PER_BUCKET).
    pub samples_capped: bool,
}

impl DrainedSummary {
    /// Renders a single Telegram-ready summary message body.
    ///
    /// **Two formats** depending on whether the bucket coalesced a burst
    /// or a single event:
    ///
    /// ## `count == 1` (single event — terse)
    ///
    /// ```text
    /// <sample 1>
    /// ```
    ///
    /// The summary wrapper is omitted because there's nothing to summarise.
    /// The dispatcher already prefixes the severity tag (`✅ [LOW]` etc.),
    /// so the operator sees a clean one-liner like `Auth OK — Dhan JWT
    /// acquired` instead of a 6-line `Coalesced summary […]` envelope.
    ///
    /// ## `count > 1` (genuine burst — terse envelope, 2026-05-09)
    ///
    /// ```text
    /// <topic> x<N>
    /// <sample 1>
    /// <sample 2>
    /// ...
    /// (+M more)
    /// ```
    ///
    /// The 2026-05-09 operator complaint ("I don't need this coalesced
    /// summary extra words") drove the redesign: drop the
    /// `Coalesced summary [...] count=N` preamble, drop the
    /// `First: ... — Last: ...` line, drop the `Samples:` keyword.
    /// The operator already sees the severity tag (`✅ [LOW]` etc.)
    /// from the dispatcher, so the topic + multiplier in one short
    /// line is enough envelope. Burst windows are 60 s by default —
    /// timestamps add little signal at that granularity.
    pub fn render_message(&self) -> String {
        // Single-event fast path: no envelope, just the sample.
        // Defensive: count=1 with no samples should be impossible
        // (Telegram02CoalescerStateInconsistency catches it
        // separately) — fall through to the envelope rather than
        // emit an empty message.
        if self.count == 1
            && let Some(sample) = self.samples.first()
        {
            return sample.clone();
        }

        let mut out = String::with_capacity(256);
        out.push_str(self.topic);
        out.push_str(" x");
        out.push_str(&self.count.to_string());
        for sample in &self.samples {
            out.push('\n');
            out.push_str(sample);
        }
        if self.samples_capped {
            let capped = self.count.saturating_sub(self.samples.len() as u64);
            out.push_str("\n(+");
            out.push_str(&capped.to_string());
            out.push_str(" more)");
        }
        out
    }
}

/// Telegram bucket coalescer.
///
/// Cheap to clone (`Arc` internally is fine; this struct itself is intended
/// to be wrapped in `Arc<TelegramCoalescer>`). Use [`Self::observe`] to
/// classify an event and [`Self::drain_mature`] to harvest closed windows.
pub struct TelegramCoalescer {
    config: CoalescerConfig,
    buckets: Mutex<HashMap<BucketKey, BucketState>>,
}

impl TelegramCoalescer {
    /// Creates a new coalescer with the supplied configuration.
    pub fn new(config: CoalescerConfig) -> Self {
        Self {
            config,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Returns the active configuration.
    pub fn config(&self) -> CoalescerConfig {
        self.config
    }

    /// Classifies an event against the coalescer.
    ///
    /// Returns [`CoalesceDecision::Bypass`] for Critical/High — caller
    /// MUST forward to the underlying dispatcher (the never-digest law).
    /// Returns [`CoalesceDecision::Coalesced`] for Medium/Low/Info — the
    /// event has been folded into a bucket; the drain task will emit a
    /// summary when the window closes.
    ///
    /// 2026-07-07 UX overhaul (dated operator directive in the PR body):
    /// Medium moved from Bypass to Coalesced — the lane decision now lives
    /// in [`classify_dispatch`] (the dispatcher only calls `observe` for
    /// the Digest/Coalesce60 lanes); this severity match is the defensive
    /// second layer keeping High/Critical unrepresentable in a bucket.
    ///
    /// `payload` is consumed only when the event is coalesced; for the
    /// Bypass path it is borrowed (no allocation). The string-typed
    /// argument is intentional — the dispatcher already allocated the
    /// rendered message; this method just owns it on coalesce.
    pub fn observe(
        &self,
        topic: &'static str,
        severity: Severity,
        payload: impl FnOnce() -> String,
    ) -> CoalesceDecision {
        match severity {
            Severity::Critical | Severity::High => CoalesceDecision::Bypass,
            Severity::Medium | Severity::Low | Severity::Info => {
                let key = BucketKey { topic, severity };
                let now_ms = now_ms();
                let p = payload();
                let mut guard = match self.buckets.lock() {
                    Ok(g) => g,
                    Err(poisoned) => {
                        // Mutex poisoning means a previous holder panicked.
                        // Recover by taking the inner state — the data is
                        // still consistent (BucketState's invariants are
                        // preserved across panics; ArrayVec push is panic-safe).
                        poisoned.into_inner()
                    }
                };
                guard
                    .entry(key)
                    .and_modify(|state| {
                        let pushed = state.fold(now_ms, p.clone());
                        if !pushed {
                            counter!(
                                "tv_telegram_dropped_total",
                                "reason" => "coalesced_sample_capped",
                            )
                            .increment(1);
                        }
                    })
                    .or_insert_with(|| BucketState::new(now_ms, p));
                CoalesceDecision::Coalesced
            }
        }
    }

    /// Drains every bucket whose age ≥ the DEFAULT (off-hours) window.
    pub fn drain_mature(&self) -> Vec<DrainedSummary> {
        self.drain_mature_with_window(self.config.window)
    }

    /// Drains every bucket whose age ≥ `window`. Returns one
    /// [`DrainedSummary`] per closed bucket. The internal state is reset
    /// for those buckets.
    ///
    /// The drain ticker passes [`effective_drain_window`] so the same
    /// buckets batch for `market_hours_window` in-market and `window`
    /// (legacy 60s) off-hours.
    pub fn drain_mature_with_window(&self, window: Duration) -> Vec<DrainedSummary> {
        let now_ms = now_ms();
        let mut summaries: Vec<DrainedSummary> = Vec::new();
        let mut guard = match self.buckets.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };

        // Two-pass to avoid borrow conflict between iter and remove:
        // collect mature keys, then remove + render.
        let mature_keys: Vec<BucketKey> = guard
            .iter()
            .filter(|(_, state)| state.age(now_ms) >= window)
            .map(|(k, _)| *k)
            .collect();

        for key in mature_keys {
            if let Some(state) = guard.remove(&key) {
                let samples_capped = state.count > state.samples.len() as u64;
                let summary = DrainedSummary {
                    topic: key.topic,
                    severity: key.severity,
                    count: state.count,
                    first_ts_ms: state.first_ts_ms,
                    last_ts_ms: state.last_ts_ms,
                    samples: state.samples.into_iter().collect(),
                    samples_capped,
                };
                summaries.push(summary);
            }
        }

        summaries
    }

    /// Drains EVERY bucket regardless of age. Used at shutdown to flush
    /// pending summaries before the process exits.
    pub fn drain_all(&self) -> Vec<DrainedSummary> {
        let mut summaries: Vec<DrainedSummary> = Vec::new();
        let mut guard = match self.buckets.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        for (key, state) in guard.drain() {
            let samples_capped = state.count > state.samples.len() as u64;
            summaries.push(DrainedSummary {
                topic: key.topic,
                severity: key.severity,
                count: state.count,
                first_ts_ms: state.first_ts_ms,
                last_ts_ms: state.last_ts_ms,
                samples: state.samples.into_iter().collect(),
                samples_capped,
            });
        }
        summaries
    }

    /// Test-only accessor — number of currently-open buckets.
    #[cfg(test)]
    // TEST-EXEMPT: #[cfg(test)]-gated test-only accessor, exercised by this module's coalescer tests.
    pub(crate) fn bucket_count(&self) -> usize {
        self.buckets.lock().map(|g| g.len()).unwrap_or(0)
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_coalescer(window_ms: u64) -> TelegramCoalescer {
        TelegramCoalescer::new(CoalescerConfig {
            window: Duration::from_millis(window_ms),
            flush_interval: Duration::from_millis(window_ms / 4 + 1),
            ..CoalescerConfig::default()
        })
    }

    #[test]
    fn test_critical_bypasses_coalescer() {
        let c = make_coalescer(60_000);
        let decision = c.observe("RiskHalt", Severity::Critical, || {
            panic!("payload closure must not run on bypass")
        });
        assert_eq!(decision, CoalesceDecision::Bypass);
        assert_eq!(c.bucket_count(), 0);
    }

    #[test]
    fn test_high_bypasses_coalescer() {
        let c = make_coalescer(60_000);
        let decision = c.observe("WebSocketDisconnected", Severity::High, || {
            panic!("payload closure must not run on bypass")
        });
        assert_eq!(decision, CoalesceDecision::Bypass);
        assert_eq!(c.bucket_count(), 0);
    }

    // 2026-07-07 UX overhaul: the old `test_medium_bypasses_coalescer`
    // is CONSCIOUSLY re-pinned (dated operator directive 2026-07-07 —
    // "boilerplate diet + LOW/MEDIUM digest"). Medium now coalesces; the
    // lane law lives in `classify_dispatch` and is exhaustively pinned by
    // `test_classify_dispatch_high_critical_never_digest_full_matrix`.
    #[test]
    fn test_medium_coalesces_since_2026_07_07() {
        let c = make_coalescer(60_000);
        let decision = c.observe("QuestDbReconnected", Severity::Medium, || {
            "recovered".to_string()
        });
        assert_eq!(decision, CoalesceDecision::Coalesced);
        assert_eq!(c.bucket_count(), 1);
    }

    #[test]
    fn test_classify_dispatch_high_critical_never_digest_full_matrix() {
        use super::super::episode::EpisodeRole;
        let severities = [
            Severity::Info,
            Severity::Low,
            Severity::Medium,
            Severity::High,
            Severity::Critical,
        ];
        let policies = [DispatchPolicy::Default, DispatchPolicy::Immediate];
        let episodes = [None, Some(EpisodeRole::Open)];
        for sev in severities {
            for policy in policies {
                for episode in episodes {
                    for in_market in [true, false] {
                        let lane = classify_dispatch(sev, policy, episode, in_market);
                        if sev >= Severity::High {
                            assert!(
                                matches!(lane, DispatchLane::Immediate | DispatchLane::EpisodeEdit),
                                "High/Critical must NEVER batch: {sev:?}/{policy:?}/{episode:?}/{in_market} -> {lane:?}"
                            );
                        }
                        if matches!(policy, DispatchPolicy::Immediate) {
                            assert_eq!(
                                lane,
                                DispatchLane::Immediate,
                                "DispatchPolicy::Immediate always wins (green boot pings)"
                            );
                        }
                    }
                }
            }
        }
        // Spot-pins for the batching lanes.
        assert_eq!(
            classify_dispatch(Severity::Medium, DispatchPolicy::Default, None, true),
            DispatchLane::Digest
        );
        assert_eq!(
            classify_dispatch(Severity::Low, DispatchPolicy::Default, None, false),
            DispatchLane::Coalesce60
        );
        assert_eq!(
            classify_dispatch(
                Severity::Low,
                DispatchPolicy::Default,
                Some(EpisodeRole::Open),
                true
            ),
            DispatchLane::EpisodeEdit
        );
    }

    #[test]
    fn test_digest_window_900_market_60_off_and_config_clamped() {
        let cfg = CoalescerConfig::default();
        assert_eq!(
            effective_drain_window(true, &cfg),
            Duration::from_secs(DEFAULT_DIGEST_WINDOW_SECS),
            "in-market drain window is the 900s digest"
        );
        assert_eq!(
            effective_drain_window(false, &cfg),
            Duration::from_secs(DEFAULT_WINDOW_SECS),
            "off-hours drain window is the legacy 60s"
        );
        // The [60, 3600] clamp is the config layer's contract.
        let mut ncfg = tickvault_common::config::NotificationConfig::default();
        ncfg.digest_window_secs = 0;
        assert_eq!(ncfg.digest_window_secs_clamped(), 60);
        ncfg.digest_window_secs = 86_400;
        assert_eq!(ncfg.digest_window_secs_clamped(), 3_600);
    }

    #[test]
    fn test_digest_force_drain_at_market_close_boundary() {
        // Injected market-phase clock: only the in→out falling edge (the
        // 15:30 IST close) force-drains, so no digest straddles overnight.
        assert!(should_force_drain(true, false), "close boundary drains");
        assert!(!should_force_drain(true, true));
        assert!(!should_force_drain(false, false));
        assert!(!should_force_drain(false, true), "open edge never drains");
        // And the force-drain path empties a still-young digest bucket.
        let c = make_coalescer(900_000);
        c.observe("TickGapsSummary", Severity::Low, || "gap".to_string());
        assert!(
            c.drain_mature_with_window(Duration::from_secs(900))
                .is_empty(),
            "young bucket must survive a normal drain"
        );
        let drained = c.drain_all();
        assert_eq!(drained.len(), 1, "force-drain flushes the young bucket");
    }

    #[test]
    fn test_render_digest_header_topic_counts_ist_window() {
        let mk = |topic: &'static str, count: u64| DrainedSummary {
            topic,
            severity: Severity::Low,
            count,
            first_ts_ms: 0,
            last_ts_ms: 0,
            samples: vec![],
            samples_capped: false,
        };
        let start = 1_783_485_120_000_u64; // 10:02 AM IST
        let end = start + 15 * 60_000;
        let digest = render_digest(
            &[mk("TickGapsSummary", 4), mk("TokenRenewed", 1)],
            start,
            end,
        );
        assert!(
            digest.starts_with("\u{1f535} 15-min digest ("),
            "header: {digest:?}"
        );
        assert!(digest.contains(" IST)"), "IST window: {digest:?}");
        assert!(digest.contains("AM") || digest.contains("PM"));
        assert!(digest.contains("\u{2022} TickGapsSummary x4"));
        assert!(digest.contains("\u{2022} TokenRenewed x1"));
        assert!(!digest.contains("(+"), "no cap marker under the line cap");
        // Cap marker when topic lines exceed DIGEST_MAX_TOPIC_LINES.
        let many: Vec<DrainedSummary> = (0..DIGEST_MAX_TOPIC_LINES + 3)
            .map(|_| mk("Spammy", 2))
            .collect();
        let digest = render_digest(&many, start, end);
        assert!(digest.contains("(+3 more)"), "cap marker: {digest:?}");
        assert_eq!(
            digest.matches('\u{2022}').count(),
            DIGEST_MAX_TOPIC_LINES,
            "topic lines capped"
        );
    }

    #[test]
    fn test_low_severity_coalesces_within_window() {
        let c = make_coalescer(60_000);
        for i in 0..5 {
            let decision = c.observe("WebSocketDisconnectedOffHours", Severity::Low, || {
                format!("disconnect #{i}")
            });
            assert_eq!(decision, CoalesceDecision::Coalesced);
        }
        assert_eq!(c.bucket_count(), 1);
    }

    #[test]
    fn test_info_severity_coalesces_within_window() {
        let c = make_coalescer(60_000);
        for _ in 0..3 {
            let decision = c.observe("StartupComplete", Severity::Info, || "started".to_string());
            assert_eq!(decision, CoalesceDecision::Coalesced);
        }
        assert_eq!(c.bucket_count(), 1);
    }

    #[test]
    fn test_summary_includes_count_first_last_samples() {
        // 10ms window so we can drain quickly.
        let c = make_coalescer(10);
        for i in 0..3 {
            c.observe("TickGapsSummary", Severity::Low, || format!("rebal #{i}"));
        }
        // Sleep beyond the window so all buckets are mature.
        std::thread::sleep(Duration::from_millis(20));
        let summaries = c.drain_mature();
        assert_eq!(summaries.len(), 1);
        let s = &summaries[0];
        assert_eq!(s.topic, "TickGapsSummary");
        assert_eq!(s.severity, Severity::Low);
        assert_eq!(s.count, 3);
        assert_eq!(s.samples.len(), 3);
        assert!(s.first_ts_ms <= s.last_ts_ms);
        assert!(!s.samples_capped);

        let msg = s.render_message();
        // Wave-Holiday-Gate (2026-05-09): terse format — no preamble,
        // no IST timestamp line, no "Samples:" keyword.
        assert!(msg.contains("TickGapsSummary x3"));
        assert!(msg.contains("rebal #0"));
        assert!(!msg.contains("Coalesced summary"));
        assert!(!msg.contains("Samples:"));
        assert!(!msg.contains("First:"));
    }

    #[test]
    fn test_drain_clears_buckets() {
        let c = make_coalescer(10);
        c.observe("Foo", Severity::Low, || "x".to_string());
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(c.drain_mature().len(), 1);
        assert_eq!(c.bucket_count(), 0);
        // Next event opens a fresh window.
        c.observe("Foo", Severity::Low, || "y".to_string());
        assert_eq!(c.bucket_count(), 1);
    }

    #[test]
    fn test_sample_cap_at_10() {
        let c = make_coalescer(60_000);
        for i in 0..25 {
            c.observe("Spammy", Severity::Low, || format!("evt #{i}"));
        }
        // Force-drain (window not elapsed) via drain_all.
        let summaries = c.drain_all();
        assert_eq!(summaries.len(), 1);
        let s = &summaries[0];
        assert_eq!(s.count, 25);
        assert_eq!(s.samples.len(), MAX_SAMPLES_PER_BUCKET);
        assert!(s.samples_capped);

        let msg = s.render_message();
        // Wave-Holiday-Gate (2026-05-09): terse format — `<topic> x<count>`
        // with `(+M more)` suffix when sample list is capped.
        assert!(msg.contains("Spammy x25"));
        assert!(msg.contains("(+15 more)"));
        assert!(!msg.contains("not retained"));
    }

    #[test]
    fn test_drain_only_returns_mature_buckets() {
        // Window 10s; insert one event then immediately drain — should
        // return zero summaries (bucket too young).
        let c = TelegramCoalescer::new(CoalescerConfig {
            window: Duration::from_secs(10),
            flush_interval: Duration::from_secs(1),
            ..CoalescerConfig::default()
        });
        c.observe("Foo", Severity::Low, || "x".to_string());
        let summaries = c.drain_mature();
        assert_eq!(summaries.len(), 0);
        assert_eq!(c.bucket_count(), 1);
    }

    #[test]
    fn test_distinct_topics_get_distinct_buckets() {
        let c = make_coalescer(60_000);
        c.observe("A", Severity::Low, || "1".to_string());
        c.observe("B", Severity::Low, || "2".to_string());
        c.observe("A", Severity::Info, || "3".to_string());
        // (A, Low), (B, Low), (A, Info) = 3 distinct buckets.
        assert_eq!(c.bucket_count(), 3);
    }

    #[test]
    fn test_render_message_with_no_cap() {
        let summary = DrainedSummary {
            topic: "X",
            severity: Severity::Low,
            count: 2,
            first_ts_ms: 1_700_000_000_000,
            last_ts_ms: 1_700_000_010_000,
            samples: vec!["sample1".into(), "sample2".into()],
            samples_capped: false,
        };
        let msg = summary.render_message();
        // Wave-Holiday-Gate (2026-05-09): terse format — `<topic> x<count>`.
        assert!(msg.contains("X x2"));
        assert!(msg.contains("sample1"));
        assert!(msg.contains("sample2"));
        assert!(!msg.contains("not retained"));
        assert!(!msg.contains("Coalesced summary"));
    }

    #[test]
    fn test_render_message_count_one_omits_envelope() {
        // Operator complaint 2026-05-02: count=1 events were wrapped in
        // a verbose `Coalesced summary [topic] count=1 / Samples: 1. ...`
        // envelope when there's nothing to summarise. Single-event fast
        // path returns just the sample body.
        // 2026-05-03 fix: samples are stored UNPREFIXED — the dispatcher's
        // `deliver_summaries` adds the severity tag exactly once on drain.
        // Storing the prefixed string here caused the duplicate
        // `✅ [LOW] ✅ [LOW]` Telegram bug observed by Parthiban.
        let summary = DrainedSummary {
            topic: "AuthenticationSuccess",
            severity: Severity::Low,
            count: 1,
            first_ts_ms: 1_700_000_000_000,
            last_ts_ms: 1_700_000_000_000,
            samples: vec!["Auth OK — Dhan JWT acquired".into()],
            samples_capped: false,
        };
        let msg = summary.render_message();
        assert_eq!(msg, "Auth OK — Dhan JWT acquired");
        // The severity tag must NOT be in the rendered body — it is added
        // by the dispatcher when sending to Telegram.
        assert!(!msg.contains("[LOW]"));
        assert!(!msg.contains("Coalesced summary"));
        assert!(!msg.contains("count="));
        assert!(!msg.contains("Samples:"));
        assert!(!msg.contains("First:"));
    }

    #[test]
    fn test_render_message_count_one_with_empty_samples_falls_back_to_envelope() {
        // Defensive: if the impossible state (count=1 + empty samples)
        // somehow occurs, do NOT emit an empty Telegram message — fall
        // through to the envelope so the operator at least sees `count=1
        // [topic]`. Telegram02CoalescerStateInconsistency catches the
        // upstream cause separately.
        let summary = DrainedSummary {
            topic: "Glitch",
            severity: Severity::Low,
            count: 1,
            first_ts_ms: 1_700_000_000_000,
            last_ts_ms: 1_700_000_000_000,
            samples: vec![],
            samples_capped: false,
        };
        let msg = summary.render_message();
        // Wave-Holiday-Gate (2026-05-09): degenerate-state fallback now
        // also uses the terse format so the operator never sees the
        // legacy verbose preamble. The `<topic> x1` body still tells
        // them which event lost its sample.
        assert_eq!(msg, "Glitch x1");
        assert!(!msg.contains("Coalesced summary"));
        assert!(!msg.contains("Samples:"));
    }

    #[test]
    fn test_render_message_count_two_uses_terse_envelope() {
        // Wave-Holiday-Gate (2026-05-09): operator complaint
        // "I clearly told you I don't need this coalesced summary extra
        // words" — the count > 1 envelope is now terse. No
        // "Coalesced summary" preamble, no "First/Last" timestamp line,
        // no "Samples:" keyword.
        let summary = DrainedSummary {
            topic: "T",
            severity: Severity::Low,
            count: 2,
            first_ts_ms: 1_700_000_000_000,
            last_ts_ms: 1_700_000_010_000,
            samples: vec!["a".into(), "b".into()],
            samples_capped: false,
        };
        let msg = summary.render_message();
        assert!(msg.contains("T x2"));
        assert!(msg.contains("a"));
        assert!(msg.contains("b"));
        assert!(!msg.contains("Coalesced summary"));
        assert!(!msg.contains("count="));
        assert!(!msg.contains("Samples:"));
        assert!(!msg.contains("First:"));
    }

    #[test]
    fn test_end_to_end_no_double_prefix_on_coalesced_telegram_body() {
        // Regression guard for the 2026-05-03 operator-visible bug:
        //   ✅ [LOW] ✅ [LOW] Auth OK — Dhan JWT acquired
        //
        // Simulates the full pipeline a Severity::Low event takes through
        // the production dispatcher:
        //   1. dispatch_immediate computes `body = event.to_message()`
        //      (NO severity tag).
        //   2. coalescer.observe stores `body` in the bucket.
        //   3. drain_mature returns DrainedSummary.
        //   4. deliver_summaries renders
        //      `format!("{} {}", severity.tag(), summary.render_message())`.
        //
        // The final Telegram body MUST contain the `[LOW]` tag exactly
        // ONCE. Two occurrences = the bug is back.
        let c = TelegramCoalescer::new(CoalescerConfig {
            window: Duration::from_millis(5),
            flush_interval: Duration::from_millis(2),
            ..CoalescerConfig::default()
        });
        let unprefixed_body = "Auth OK — Dhan JWT acquired".to_string();
        c.observe("AuthenticationSuccess", Severity::Low, || {
            unprefixed_body.clone()
        });
        std::thread::sleep(Duration::from_millis(15));
        let drained = c.drain_mature();
        assert_eq!(drained.len(), 1);
        let summary = &drained[0];

        // Mirror deliver_summaries' format! call.
        let final_body = format!("{} {}", summary.severity.tag(), summary.render_message());
        let low_count = final_body.matches("[LOW]").count();
        assert_eq!(
            low_count, 1,
            "expected exactly one [LOW] tag in final Telegram body, got {low_count}: {final_body:?}"
        );
        assert!(final_body.starts_with("✅ [LOW] "));
        assert!(final_body.contains("Auth OK"));
        // And the inner sample must NOT have the tag (regression guard
        // for the upstream bug at the source).
        assert!(!summary.samples[0].contains("[LOW]"));
    }

    #[test]
    fn test_drain_mature_only_returns_aged_buckets() {
        let c = TelegramCoalescer::new(CoalescerConfig {
            window: Duration::from_millis(10),
            flush_interval: Duration::from_millis(5),
            ..CoalescerConfig::default()
        });
        c.observe("Foo", Severity::Low, || "x".to_string());
        // Immediately drain — bucket too young.
        assert!(c.drain_mature().is_empty());
        // After window elapses, drain returns the bucket.
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(c.drain_mature().len(), 1);
    }

    #[test]
    fn test_drain_all_empties_every_bucket() {
        let c = make_coalescer(60_000);
        c.observe("A", Severity::Low, || "1".into());
        c.observe("B", Severity::Info, || "2".into());
        let summaries = c.drain_all();
        assert_eq!(summaries.len(), 2);
        assert_eq!(c.bucket_count(), 0);
    }

    #[test]
    fn test_observe_returns_bypass_for_high_and_critical() {
        let c = make_coalescer(60_000);
        assert_eq!(
            c.observe("X", Severity::Critical, || "x".to_string()),
            CoalesceDecision::Bypass
        );
        assert_eq!(
            c.observe("X", Severity::High, || "x".to_string()),
            CoalesceDecision::Bypass
        );
    }

    #[test]
    fn test_default_config_sane() {
        let c = CoalescerConfig::default();
        assert_eq!(c.window, Duration::from_secs(60));
        assert!(c.flush_interval < c.window);
        assert!(c.flush_interval > Duration::ZERO);
    }
}

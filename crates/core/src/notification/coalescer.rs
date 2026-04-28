//! Wave 3-B Item 11 — Telegram bucket coalescer.
//!
//! Coalesces bursts of low-severity notification events into a single
//! summary message per `(topic, severity)` bucket per window. Saves
//! operator pager fatigue when a noisy subsystem fires the same event
//! 100× in 60 seconds.
//!
//! # Severity routing
//!
//! - [`Severity::Critical`] — BYPASS (sent immediately)
//! - [`Severity::High`]     — BYPASS (sent immediately)
//! - [`Severity::Medium`]   — BYPASS (sent immediately; conservative
//!   default — Medium events are operator-visible
//!   state changes that shouldn't be coalesced)
//! - [`Severity::Low`]      — COALESCE
//! - [`Severity::Info`]     — COALESCE
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

use super::events::Severity;

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
    /// Window duration. Bursts within this fold into one summary message.
    pub window: Duration,
    /// How often the background flush task wakes to drain mature windows.
    pub flush_interval: Duration,
}

impl Default for CoalescerConfig {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(DEFAULT_WINDOW_SECS),
            flush_interval: Duration::from_secs(DEFAULT_FLUSH_INTERVAL_SECS),
        }
    }
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
    /// Format (deterministic, plain text — Telegram handles formatting via
    /// the existing `<pre>` wrapping done by the dispatcher):
    ///
    /// ```text
    /// Coalesced summary [topic] count=N
    /// Window: <first IST> .. <last IST>
    /// Samples (up to 10):
    ///   1. <sample 1>
    ///   2. <sample 2>
    ///   ...
    /// (+M more not retained — count is authoritative)
    /// ```
    pub fn render_message(&self) -> String {
        let mut out = String::with_capacity(256);
        out.push_str("Coalesced summary [");
        out.push_str(self.topic);
        out.push_str("] count=");
        out.push_str(&self.count.to_string());
        out.push('\n');
        out.push_str("First: ");
        out.push_str(&format_ist_ms(self.first_ts_ms));
        out.push_str(" — Last: ");
        out.push_str(&format_ist_ms(self.last_ts_ms));
        out.push('\n');
        out.push_str("Samples:");
        for (idx, sample) in self.samples.iter().enumerate() {
            out.push_str("\n  ");
            out.push_str(&(idx + 1).to_string());
            out.push_str(". ");
            out.push_str(sample);
        }
        if self.samples_capped {
            let capped = self.count.saturating_sub(self.samples.len() as u64);
            out.push_str("\n(+");
            out.push_str(&capped.to_string());
            out.push_str(" more not retained — count is authoritative)");
        }
        out
    }
}

/// Renders a millisecond UTC timestamp as `HH:MM:SS IST` (no allocation
/// of date — operator only needs the time-of-day for a 60s window).
fn format_ist_ms(ms: u64) -> String {
    // IST = UTC + 5h30m. Compute seconds-of-day in IST without bringing in chrono.
    const IST_OFFSET_SECS: u64 = 5 * 3600 + 30 * 60;
    let total_secs = ms / 1000;
    let ist_secs = total_secs.saturating_add(IST_OFFSET_SECS);
    let sec_of_day = ist_secs % 86_400;
    let h = sec_of_day / 3600;
    let m = (sec_of_day % 3600) / 60;
    let s = sec_of_day % 60;
    format!("{h:02}:{m:02}:{s:02} IST")
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
    /// Returns [`CoalesceDecision::Bypass`] for Critical/High/Medium —
    /// caller MUST forward to the underlying dispatcher.
    /// Returns [`CoalesceDecision::Coalesced`] for Low/Info — the event
    /// has been folded into a bucket; the drain task will emit a summary
    /// when the window closes.
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
            Severity::Critical | Severity::High | Severity::Medium => CoalesceDecision::Bypass,
            Severity::Low | Severity::Info => {
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

    /// Drains every bucket whose age ≥ window. Returns one
    /// [`DrainedSummary`] per closed bucket. The internal state is reset
    /// for those buckets.
    pub fn drain_mature(&self) -> Vec<DrainedSummary> {
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
            .filter(|(_, state)| state.age(now_ms) >= self.config.window)
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

    #[test]
    fn test_medium_bypasses_coalescer() {
        let c = make_coalescer(60_000);
        let decision = c.observe("Phase2Complete", Severity::Medium, || {
            panic!("payload closure must not run on bypass")
        });
        assert_eq!(decision, CoalesceDecision::Bypass);
        assert_eq!(c.bucket_count(), 0);
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
            c.observe("DepthRebalanced", Severity::Low, || format!("rebal #{i}"));
        }
        // Sleep beyond the window so all buckets are mature.
        std::thread::sleep(Duration::from_millis(20));
        let summaries = c.drain_mature();
        assert_eq!(summaries.len(), 1);
        let s = &summaries[0];
        assert_eq!(s.topic, "DepthRebalanced");
        assert_eq!(s.severity, Severity::Low);
        assert_eq!(s.count, 3);
        assert_eq!(s.samples.len(), 3);
        assert!(s.first_ts_ms <= s.last_ts_ms);
        assert!(!s.samples_capped);

        let msg = s.render_message();
        assert!(msg.contains("count=3"));
        assert!(msg.contains("DepthRebalanced"));
        assert!(msg.contains("rebal #0"));
        assert!(msg.contains("IST"));
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
        assert!(msg.contains("count=25"));
        assert!(msg.contains("(+15 more not retained"));
    }

    #[test]
    fn test_drain_only_returns_mature_buckets() {
        // Window 10s; insert one event then immediately drain — should
        // return zero summaries (bucket too young).
        let c = TelegramCoalescer::new(CoalescerConfig {
            window: Duration::from_secs(10),
            flush_interval: Duration::from_secs(1),
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
        assert!(msg.contains("count=2"));
        assert!(msg.contains("sample1"));
        assert!(msg.contains("sample2"));
        assert!(!msg.contains("not retained"));
    }

    #[test]
    fn test_format_ist_ms_handles_midnight_boundary() {
        // 2026-01-01 00:00:00 UTC = 05:30:00 IST
        let ms = 1_767_225_600_000;
        let s = format_ist_ms(ms);
        assert!(s.ends_with(" IST"), "got {s}");
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

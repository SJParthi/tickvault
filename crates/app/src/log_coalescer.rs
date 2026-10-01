//! Coalesces repeated identical coded ERROR lines on the `errors.jsonl`
//! stream (plan item 45d, 2026-10-01).
//!
//! # Why
//!
//! On 2026-09-29 the boot WAL catch-up drain failed a flush every few
//! milliseconds, and each failure wrote the same three coded ERROR lines. The
//! `errors.jsonl` stream took about 8,000 lines in eight minutes, a ~9.5 MB
//! burst, and the CloudWatch agent's pipe for that file froze at 08:32 IST for
//! an hour and a half. Every coded-error alarm reads that stream, so the burst
//! that most needed paging blinded the pager.
//!
//! # What
//!
//! A [`Filter`] on the `errors.jsonl` layer admits at most ONE line per
//! second for each (call site, `code`, `source`). Later identical lines in the
//! same second are left off that stream and counted on
//! [`LOG_LINES_COALESCED_METRIC`]. Every coded alarm counts lines with
//! threshold 1 in a five-minute window, so one line a second still pages
//! exactly when the uncoalesced stream would have.
//!
//! What is NOT coalesced: anything below ERROR, any ERROR without a `code`
//! field, and every other sink. `app.log` and `errors.log` keep every line,
//! so the full record of a burst stays on the box.
//!
//! # Honest limit
//!
//! The plan asked for a repeat count on the next admitted line. A
//! `tracing` filter cannot add a field to an event, and an event emitted from
//! inside the subscriber is dropped by `tracing`'s re-entrancy guard, so the
//! count lives on the counter (labelled by sink), not on the line.
//!
//! # Complexity
//!
//! O(1) per ERROR event and zero allocation: one field visit, an FNV-1a hash
//! over two short strings, and one compare-and-swap on a fixed table of
//! [`COALESCE_SLOTS`] atomics. Each slot packs a 32-bit key tag and the low
//! 32 bits of the second. Two keys that share a slot evict each other and are
//! both admitted; only a full 40-bit hash collision (slot index plus tag)
//! inside the same second could hide a distinct line.

use std::sync::atomic::{AtomicU64, Ordering};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Metadata};
use tracing_subscriber::layer::{Context, Filter};

/// Counter for lines left off a sink because an identical line was already
/// written in the same second. Label `sink`.
pub const LOG_LINES_COALESCED_METRIC: &str = "tv_log_lines_coalesced_total";

/// Number of slots in the coalescing table. A power of two.
pub const COALESCE_SLOTS: usize = 256;

const _: () = assert!(COALESCE_SLOTS.is_power_of_two());

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV-1a over `bytes`, continuing from `hash`. Pure, O(len).
#[must_use]
pub const fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
        i += 1;
    }
    hash
}

/// Reads the `code` and `source` fields of an event into a hash, without
/// allocating.
struct KeyVisitor {
    code: u64,
    source: u64,
    has_code: bool,
}

impl Visit for KeyVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "code" => {
                self.code = fnv1a(FNV_OFFSET, value.as_bytes());
                self.has_code = true;
            }
            "source" => self.source = fnv1a(FNV_OFFSET, value.as_bytes()),
            _ => {}
        }
    }

    // A `code` or `source` recorded as Debug (`?x` / `%x`) is not a plain
    // string we can hash without formatting it, so it does not take part in
    // the key. A coded line always passes `code` as `&str`.
    fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
}

/// The coalescing decision table. One per sink.
pub struct ErrorCoalescer {
    slots: [AtomicU64; COALESCE_SLOTS],
    // Resolved once in `new`, so a suppressed line costs one atomic add and
    // never the allocating label path. `main` installs the metrics recorder
    // before it builds the logging layers, so this binds the real recorder.
    coalesced: metrics::Counter,
}

impl ErrorCoalescer {
    /// A fresh table for `sink` (the label on the coalesced counter). Build it
    /// after the metrics recorder is installed.
    #[must_use]
    pub fn new(sink: &'static str) -> Self {
        let coalesced = metrics::counter!(LOG_LINES_COALESCED_METRIC, "sink" => sink);
        coalesced.increment(0);
        Self {
            slots: std::array::from_fn(|_| AtomicU64::new(0)),
            coalesced,
        }
    }

    /// Admit a line with key hash `key` in second `now_secs`? `true` writes
    /// the line; `false` means an identical line was already written this
    /// second. O(1), lock-free, zero allocation.
    pub fn admit(&self, key: u64, now_secs: u64) -> bool {
        let slot = &self.slots[(key as usize) & (COALESCE_SLOTS - 1)];
        let tag = key >> 32;
        let word = (tag << 32) | (now_secs & 0xFFFF_FFFF);
        let cur = slot.load(Ordering::Relaxed);
        if cur == word {
            return false;
        }
        match slot.compare_exchange(cur, word, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => true,
            // Another thread wrote this exact key and second first: its line
            // is the one for this second.
            Err(actual) if actual == word => false,
            // Another key took the slot: admit, failing toward logging.
            Err(_) => true,
        }
    }

    /// The key of a coded ERROR event, or `None` when the event is not one
    /// this filter coalesces.
    #[must_use]
    pub fn key_of(event: &Event<'_>) -> Option<u64> {
        if *event.metadata().level() != Level::ERROR {
            return None;
        }
        let mut v = KeyVisitor {
            code: 0,
            source: 0,
            has_code: false,
        };
        event.record(&mut v);
        if !v.has_code {
            return None;
        }
        let callsite = event.metadata() as *const Metadata<'_> as usize as u64;
        let mut h = fnv1a(FNV_OFFSET, &callsite.to_le_bytes());
        h = fnv1a(h, &v.code.to_le_bytes());
        h = fnv1a(h, &v.source.to_le_bytes());
        Some(h)
    }
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl<S> Filter<S> for ErrorCoalescer {
    fn enabled(&self, _meta: &Metadata<'_>, _cx: &Context<'_, S>) -> bool {
        true
    }

    fn event_enabled(&self, event: &Event<'_>, _cx: &Context<'_, S>) -> bool {
        let Some(key) = Self::key_of(event) else {
            return true;
        };
        if self.admit(key, now_unix_secs()) {
            true
        } else {
            self.coalesced.increment(1);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::filter::FilterExt as _;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::{Layer, Registry};

    #[test]
    fn same_key_same_second_is_coalesced_and_the_next_second_admits() {
        let c = ErrorCoalescer::new("test");
        assert!(c.admit(42, 100));
        assert!(!c.admit(42, 100));
        assert!(!c.admit(42, 100));
        assert!(c.admit(42, 101));
        assert!(!c.admit(42, 101));
    }

    #[test]
    fn distinct_keys_in_one_slot_are_both_admitted() {
        let c = ErrorCoalescer::new("test");
        // Same slot (low bits), different tags (high bits).
        let a = 7_u64;
        let b = 7_u64 | (1 << 40);
        assert!(c.admit(a, 5));
        assert!(c.admit(b, 5));
        assert!(
            c.admit(a, 5),
            "an evicted key is admitted again, never hidden"
        );
    }

    proptest! {
        // Within one second, each distinct key is admitted at least once.
        #[test]
        fn every_distinct_key_is_admitted_at_least_once_per_second(
            keys in proptest::collection::vec(any::<u64>(), 1..64),
            sec in any::<u64>(),
        ) {
            let c = ErrorCoalescer::new("test");
            let mut admitted = std::collections::HashSet::new();
            for &k in &keys {
                if c.admit(k, sec) {
                    admitted.insert(k);
                }
            }
            for &k in &keys {
                // A key can only be refused if an identical slot word was
                // written: same slot and same 32-bit tag.
                let refused_only_by_twin = admitted.contains(&k)
                    || admitted.iter().any(|&a| {
                        (a as usize & (COALESCE_SLOTS - 1)) == (k as usize & (COALESCE_SLOTS - 1))
                            && a >> 32 == k >> 32
                    });
                prop_assert!(refused_only_by_twin);
            }
        }

        // A repeat of the same key in the same second is never admitted twice
        // in a row.
        #[test]
        fn a_back_to_back_repeat_is_never_admitted(key in any::<u64>(), sec in any::<u64>()) {
            let c = ErrorCoalescer::new("test");
            prop_assert!(c.admit(key, sec));
            prop_assert!(!c.admit(key, sec));
        }
    }

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            if let Ok(mut v) = self.0.lock() {
                v.extend_from_slice(b);
            }
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn lines(buf: &Buf) -> usize {
        buf.0
            .lock()
            .map(|v| v.iter().filter(|&&b| b == b'\n').count())
            .unwrap_or(0)
    }

    #[test]
    fn a_burst_of_identical_coded_errors_writes_one_line() {
        let buf = Buf::default();
        let w = buf.clone();
        let layer = tracing_subscriber::fmt::layer()
            .json()
            .with_writer(move || w.clone())
            .with_filter(
                tracing_subscriber::filter::LevelFilter::ERROR
                    .and(ErrorCoalescer::new("test_sink")),
            );
        let subscriber = Registry::default().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..1_000 {
                tracing::error!(code = "WS-SPILL-01", source = "burst", "same line");
            }
            // Uncoded errors and warnings are never coalesced.
            for _ in 0..3 {
                tracing::error!("uncoded");
            }
            // A different source is a different key.
            tracing::error!(code = "WS-SPILL-01", source = "other", "other line");
        });
        // One coded line (unless the burst straddled a second boundary),
        // three uncoded, one other.
        let n = lines(&buf);
        assert!((5..=6).contains(&n), "wrote {n} lines");
    }

    #[test]
    fn the_errors_jsonl_layer_is_coalesced_in_main() {
        let src = include_str!("main.rs");
        let at = src
            .find("init_errors_jsonl_appender(observability::ERRORS_JSONL_DIR)")
            .unwrap_or(0);
        let block = &src[at..(at + 4_000).min(src.len())];
        assert!(
            block.contains("ErrorCoalescer::new(\"errors_jsonl\")"),
            "the errors.jsonl layer must be filtered by the coalescer"
        );
    }
}

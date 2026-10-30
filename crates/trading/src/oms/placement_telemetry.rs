//! Placement telemetry kept off the order step (2026-10-30).
//!
//! Two owner decisions, Fri 30 Oct 2026 (~16:50 IST), relayed by the
//! coordinator session after the order-path measurement:
//!
//! 1. Placement duration. Q: "Fix it the way we already fixed the tick-delay
//!    measurement: fixed counters set up at startup, same metric name,
//!    nothing new to read?" A: "Yes, fixed counters (Recommended)" — "Same
//!    name on the dashboard and alerts; the internal bucket layout changes
//!    slightly. Removes about 400 ns from the slowest orders."
//! 2. The per-placement "PAPER TRADE" INFO line. A: "Write it after the
//!    order (Recommended)" — "Same log line, same content, but written by a
//!    background helper right after the order is placed, so it no longer
//!    slows the order step. Nothing is lost."
//!
//! # 1. [`PlacementLatency`]: fixed atomic buckets
//!
//! `tv_order_placement_duration_ns` was a recorder histogram. The production
//! recorder (`metrics-exporter-prometheus`) stores each sample in a
//! `metrics_util` `AtomicBucket`: a list of 64-slot blocks that allocates a
//! new block when the tail fills, and stamps each sample with a clock read.
//! Measured 2026-10-30 (x86 dev container): about 120 ns at p50 and 400 ns at
//! p99 of a paper placement. The tick-delay histogram was fixed the same way
//! on 2026-10-05 (`WsLagSlot` in `dhan_feed_stack.rs`): fixed atomic counts on
//! the hot path, republished on a cold path as the same
//! `_bucket{le}` / `_count` / `_sum` lines the histogram rendered.
//!
//! Recording is O(1): a scan of 13 fixed bounds and two relaxed adds, no
//! allocation, recorder or not. Publishing is O(14) loads, once a second from
//! the order runtime's housekeeping tick ([`super::OrderManagementSystem::publish_placement_latency`]),
//! and registers its counters (allocates their keys) once, at the first
//! publish that has a sample.
//!
//! What changes for a reader of `/metrics`: the three series are exposed as
//! counters (`# TYPE ... counter`) rather than one `histogram` family, the
//! `_sum` is an integer number of nanoseconds, and the lines move at the next
//! publish (within a second) rather than at the sample. The names, the `le`
//! values and the counts are the ones the histogram rendered (pinned by
//! `crates/app/tests/order_path_metrics_parity.rs`, which renders both).
//!
//! # 2. [`log_paper_trade`]: the INFO line written by a helper thread
//!
//! With a JSON log subscriber at INFO (production's shape), formatting that
//! one line was about 2.5 µs of a 3.1 µs paper placement (measured 2026-10-30,
//! `order_path_p99_harness`). The placement now hands a fixed-size `Copy`
//! record to a pre-allocated bounded queue (`std::sync::mpsc::sync_channel`,
//! array-backed: every slot allocated at construction) and returns. One
//! thread, `tv-paper-trade-log`, drains it and writes the same line: same
//! message, same fields and values, same target, inside a span with the same
//! name and field as the one `place_order` opens.
//!
//! The drain thread POLLS (every [`PAPER_TRADE_LOG_POLL`]) instead of blocking
//! on `recv`. A blocked receiver makes every `try_send` take the channel's
//! waker lock and wake it with a syscall, which is the cost this exists to
//! remove (the same reason as the WAL writer's `wal_writer_wait`). So a line
//! is written up to one poll interval after its order.
//!
//! Never silent: if the queue is full, or the thread could not start, the line
//! is written inline on the caller (the old behaviour) and counted on
//! `tv_oms_paper_trade_log_inline_total{reason}`.
//!
//! # Honest limits
//!
//! - A line can be written up to [`PAPER_TRADE_LOG_POLL`] after its order, so
//!   it may appear after lines the order runtime logged later. Its log
//!   timestamp is the write time, not the placement time.
//! - The helper thread's span has no parent, so a JSON line no longer lists
//!   the caller's outer spans (the `place_order` span itself is kept).
//! - Records still queued when the process stops abruptly (a kill, a panic
//!   under `panic = "abort"`) are not written: at most one poll interval of
//!   placements. [`flush_paper_trade_log`] writes them on an orderly shutdown
//!   and when an [`super::OrderManagementSystem`] is dropped. The order itself
//!   is unaffected: it is in the order book and the order audit either way.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tickvault_common::order_types::TransactionType;
use tracing::info;

// ---------------------------------------------------------------------------
// 1. Placement duration
// ---------------------------------------------------------------------------

/// Upper bounds, in nanoseconds, of the placement-duration buckets: the
/// production `*_duration_ns` buckets (`TICK_NS_HISTOGRAM_BUCKETS` in the app's
/// observability module), so the published `le` values are the ones the
/// histogram rendered.
pub const PLACEMENT_DURATION_BOUNDS_NS: [u64; 13] = [
    100,
    500,
    1_000,
    5_000,
    10_000,
    50_000,
    100_000,
    500_000,
    1_000_000,
    10_000_000,
    100_000_000,
    1_000_000_000,
    10_000_000_000,
];

/// One count per bound plus the overflow (`+Inf`) bucket.
const PLACEMENT_DURATION_SLOTS: usize = PLACEMENT_DURATION_BOUNDS_NS.len() + 1;

/// The `le` label of each bucket, spelled as the exporter spells it.
const PLACEMENT_DURATION_LE: [&str; PLACEMENT_DURATION_SLOTS] = [
    "100",
    "500",
    "1000",
    "5000",
    "10000",
    "50000",
    "100000",
    "500000",
    "1000000",
    "10000000",
    "100000000",
    "1000000000",
    "10000000000",
    "+Inf",
];

/// The three exposition names a histogram named
/// `tv_order_placement_duration_ns` rendered.
const BUCKET_SERIES: &str = "tv_order_placement_duration_ns_bucket";
const COUNT_SERIES: &str = "tv_order_placement_duration_ns_count";
const SUM_SERIES: &str = "tv_order_placement_duration_ns_sum";

/// Placement duration as fixed atomic bucket counts. See the module docs.
pub struct PlacementLatency {
    /// Non-cumulative counts; `publish` makes them cumulative.
    buckets: [AtomicU64; PLACEMENT_DURATION_SLOTS],
    sum_ns: AtomicU64,
    /// Total samples at the last publish; no new sample means no publish.
    published: AtomicU64,
    /// Exposition counters, registered at the first publish with a sample.
    counters: OnceLock<PlacementLatencyCounters>,
}

struct PlacementLatencyCounters {
    buckets: [metrics::Counter; PLACEMENT_DURATION_SLOTS],
    count: metrics::Counter,
    sum: metrics::Counter,
}

impl Default for PlacementLatency {
    fn default() -> Self {
        Self::new()
    }
}

impl PlacementLatency {
    /// Empty counts. No metric is registered until the first publish.
    pub fn new() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            sum_ns: AtomicU64::new(0),
            published: AtomicU64::new(0),
            counters: OnceLock::new(),
        }
    }

    /// Records one placement duration. `le` semantics: a sample equal to a
    /// bound lands in that bound's bucket, as a Prometheus histogram counts
    /// it.
    ///
    /// # Performance
    /// O(1): a scan of 13 fixed bounds and two relaxed `fetch_add`s. No
    /// allocation.
    #[inline]
    pub fn record_ns(&self, nanos: u64) {
        let index = PLACEMENT_DURATION_BOUNDS_NS
            .iter()
            .position(|&bound| nanos <= bound)
            .unwrap_or(PLACEMENT_DURATION_BOUNDS_NS.len());
        if let Some(bucket) = self.buckets.get(index) {
            bucket.fetch_add(1, Ordering::Relaxed);
        }
        self.sum_ns.fetch_add(nanos, Ordering::Relaxed);
    }

    /// Cumulative bucket counts; the last entry is the total.
    fn cumulative(&self) -> [u64; PLACEMENT_DURATION_SLOTS] {
        let mut out = [0_u64; PLACEMENT_DURATION_SLOTS];
        let mut running = 0_u64;
        for (slot, bucket) in out.iter_mut().zip(&self.buckets) {
            running = running.saturating_add(bucket.load(Ordering::Relaxed));
            *slot = running;
        }
        out
    }

    /// Sets the exposition counters to the current cumulative counts. Cold:
    /// O(14) loads, skipped when nothing was recorded since the last call.
    /// `_count` is the `+Inf` bucket, so the two can never disagree.
    pub fn publish(&self) {
        let cumulative = self.cumulative();
        let total = cumulative[PLACEMENT_DURATION_SLOTS - 1];
        if total == self.published.load(Ordering::Relaxed) {
            return;
        }
        let counters = self.counters.get_or_init(|| PlacementLatencyCounters {
            buckets: std::array::from_fn(
                |i| metrics::counter!(BUCKET_SERIES, "le" => PLACEMENT_DURATION_LE[i]),
            ),
            count: metrics::counter!(COUNT_SERIES),
            sum: metrics::counter!(SUM_SERIES),
        });
        for (counter, value) in counters.buckets.iter().zip(cumulative) {
            counter.absolute(value);
        }
        counters.count.absolute(total);
        counters.sum.absolute(self.sum_ns.load(Ordering::Relaxed));
        self.published.store(total, Ordering::Relaxed);
    }

    /// The sample count the last `publish` wrote (tests only).
    #[cfg(test)]
    pub(crate) fn published_count(&self) -> u64 {
        self.published.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// 2. The "PAPER TRADE" line
// ---------------------------------------------------------------------------

/// Everything the "PAPER TRADE: order simulated" line prints, fixed size.
/// The order id is `PAPER-<counter>`, so the counter alone rebuilds it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaperTradeRecord {
    /// The paper order counter; the order id is `PAPER-<counter>`.
    pub paper_order_counter: u64,
    pub security_id: u64,
    pub transaction_type: TransactionType,
    pub quantity: i64,
    pub price: f64,
}

/// Records the queue holds before a placement writes its line inline.
/// 1,024 × 48 B = 48 KiB, allocated once when the queue is built. Far above
/// any burst the broker allows (10 orders a second) within one poll interval.
pub const PAPER_TRADE_LOG_QUEUE_CAPACITY: usize = 1_024;

/// How often, in milliseconds, the helper thread drains the queue.
pub const PAPER_TRADE_LOG_POLL_MS: u64 = 20;

/// How often the helper thread drains the queue. Also the most a line can
/// trail its order.
pub const PAPER_TRADE_LOG_POLL: Duration = Duration::from_millis(PAPER_TRADE_LOG_POLL_MS);

/// The tracing target the line had when `place_order` wrote it.
const PAPER_TRADE_LOG_TARGET: &str = "tickvault_trading::oms::engine";

const INLINE_COUNTER: &str = "tv_oms_paper_trade_log_inline_total";

struct PaperTradeLog {
    sender: SyncSender<PaperTradeRecord>,
    receiver: Mutex<Receiver<PaperTradeRecord>>,
}

/// `None` when the helper thread could not be started (every line is then
/// written inline and counted).
static PAPER_TRADE_LOG: OnceLock<Option<PaperTradeLog>> = OnceLock::new();

/// Builds the queue and starts the helper thread, once per process. Cold:
/// called from `OrderManagementSystem::new`, so no placement pays for it.
pub fn ensure_paper_trade_logger() {
    let _ = paper_trade_log();
}

fn paper_trade_log() -> Option<&'static PaperTradeLog> {
    PAPER_TRADE_LOG
        .get_or_init(|| {
            let (sender, receiver) = sync_channel(PAPER_TRADE_LOG_QUEUE_CAPACITY);
            let spawned = std::thread::Builder::new()
                .name("tv-paper-trade-log".to_owned())
                .spawn(drain_forever);
            match spawned {
                Ok(_) => Some(PaperTradeLog {
                    sender,
                    receiver: Mutex::new(receiver),
                }),
                Err(err) => {
                    tracing::warn!(
                        %err,
                        "paper-trade log helper thread did not start; every \
                         PAPER TRADE line is written inline (counted)"
                    );
                    None
                }
            }
        })
        .as_ref()
}

/// The helper thread: drain, then sleep one poll interval. Never blocks on
/// the queue (see the module docs for why).
fn drain_forever() {
    loop {
        flush_paper_trade_log();
        std::thread::sleep(PAPER_TRADE_LOG_POLL);
    }
}

/// Writes every queued "PAPER TRADE" line now, on the calling thread. Called
/// by the helper thread each poll, on an orderly shutdown, and when an
/// `OrderManagementSystem` is dropped. Cold.
pub fn flush_paper_trade_log() {
    let Some(log) = PAPER_TRADE_LOG.get().and_then(Option::as_ref) else {
        return;
    };
    // A poisoned lock still holds a usable receiver; writing the lines
    // matters more than the panic that poisoned it.
    let receiver = log
        .receiver
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        match receiver.try_recv() {
            Ok(record) => write_paper_trade_line(&record),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
        }
    }
}

/// Hands the line to the helper thread, or writes it inline (counted) when
/// the queue is full or the helper is not running.
///
/// # Performance
/// O(1): one `OnceLock` read and one `try_send` of a 48-byte `Copy` record
/// into a pre-allocated slot. No allocation, and no syscall while the helper
/// is polling (it never waits on the queue).
pub fn log_paper_trade(record: PaperTradeRecord) {
    let Some(log) = paper_trade_log() else {
        metrics::counter!(INLINE_COUNTER, "reason" => "no_helper").increment(1);
        write_paper_trade_line(&record);
        return;
    };
    match log.sender.try_send(record) {
        Ok(()) => {}
        Err(TrySendError::Full(record)) => {
            metrics::counter!(INLINE_COUNTER, "reason" => "queue_full").increment(1);
            write_paper_trade_line(&record);
        }
        Err(TrySendError::Disconnected(record)) => {
            metrics::counter!(INLINE_COUNTER, "reason" => "no_helper").increment(1);
            write_paper_trade_line(&record);
        }
    }
}

/// `PAPER-<counter>`, printed without building a `String`.
struct PaperOrderId(u64);

impl fmt::Display for PaperOrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PAPER-{}", self.0)
    }
}

/// The line `place_order` wrote inline until 2026-10-30, field for field,
/// inside a span with the name and field of the one `place_order` opens.
fn write_paper_trade_line(record: &PaperTradeRecord) {
    let _span = tracing::info_span!(
        target: PAPER_TRADE_LOG_TARGET,
        "place_order",
        security_id = record.security_id
    )
    .entered();
    info!(
        target: PAPER_TRADE_LOG_TARGET,
        order_id = %PaperOrderId(record.paper_order_counter),
        security_id = record.security_id,
        transaction_type = %record.transaction_type.as_str(),
        quantity = record.quantity,
        price = record.price,
        "PAPER TRADE: order simulated (no HTTP call)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket_of(nanos: u64) -> usize {
        let latency = PlacementLatency::new();
        latency.record_ns(nanos);
        latency
            .buckets
            .iter()
            .position(|b| b.load(Ordering::Relaxed) == 1)
            .unwrap_or(usize::MAX)
    }

    #[test]
    fn test_record_ns_follows_le_semantics_at_every_bound() {
        for (index, &bound) in PLACEMENT_DURATION_BOUNDS_NS.iter().enumerate() {
            assert_eq!(
                bucket_of(bound),
                index,
                "a sample equal to {bound} is <= it"
            );
            assert_eq!(
                bucket_of(bound + 1),
                index + 1,
                "{} is above {bound}",
                bound + 1
            );
        }
        assert_eq!(bucket_of(0), 0);
        assert_eq!(bucket_of(u64::MAX), PLACEMENT_DURATION_SLOTS - 1, "+Inf");
    }

    #[test]
    fn test_cumulative_counts_and_sum() {
        let latency = PlacementLatency::new();
        for nanos in [50, 650, 1_000, 2_000_000] {
            latency.record_ns(nanos);
        }
        let cumulative = latency.cumulative();
        assert_eq!(cumulative[0], 1); // <= 100
        assert_eq!(cumulative[2], 3); // <= 1,000
        assert_eq!(cumulative[PLACEMENT_DURATION_SLOTS - 1], 4);
        assert_eq!(latency.sum_ns.load(Ordering::Relaxed), 2_001_700);
    }

    #[test]
    fn test_le_labels_match_bounds() {
        for (bound, le) in PLACEMENT_DURATION_BOUNDS_NS
            .iter()
            .zip(PLACEMENT_DURATION_LE)
        {
            assert_eq!(bound.to_string(), le);
        }
        assert_eq!(PLACEMENT_DURATION_LE[PLACEMENT_DURATION_SLOTS - 1], "+Inf");
    }

    #[test]
    fn test_publish_without_a_sample_registers_nothing() {
        let latency = PlacementLatency::new();
        latency.publish();
        assert!(latency.counters.get().is_none());
    }

    #[test]
    fn test_paper_order_id_display_matches_the_old_format() {
        for counter in [0_u64, 1, 42, u64::MAX] {
            assert_eq!(
                PaperOrderId(counter).to_string(),
                format!("PAPER-{counter}")
            );
        }
    }

    #[test]
    fn test_log_paper_trade_then_flush_never_panics() {
        ensure_paper_trade_logger();
        log_paper_trade(PaperTradeRecord {
            paper_order_counter: 1,
            security_id: 13,
            transaction_type: TransactionType::Buy,
            quantity: 1,
            price: 0.0,
        });
        flush_paper_trade_log();
    }

    #[test]
    fn test_ensure_paper_trade_logger_starts_once_and_flush_paper_trade_log_empties_the_queue() {
        ensure_paper_trade_logger();
        let first = PAPER_TRADE_LOG.get().map(std::ptr::from_ref);
        ensure_paper_trade_logger();
        let second = PAPER_TRADE_LOG.get().map(std::ptr::from_ref);
        assert!(first.is_some(), "the queue is built on the first call");
        assert_eq!(first, second, "a second call builds nothing new");

        for counter in 0..8 {
            log_paper_trade(PaperTradeRecord {
                paper_order_counter: counter,
                security_id: 13,
                transaction_type: TransactionType::Sell,
                quantity: 2,
                price: 101.5,
            });
        }
        flush_paper_trade_log();
        let log = PAPER_TRADE_LOG
            .get()
            .and_then(Option::as_ref)
            .expect("helper started"); // APPROVED: test
        let receiver = log
            .receiver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            matches!(receiver.try_recv(), Err(TryRecvError::Empty)),
            "a flush leaves nothing queued"
        );
    }

    #[test]
    fn test_publish_placement_latency_marks_every_sample_published() {
        let latency = PlacementLatency::new();
        assert_eq!(latency.published_count(), 0);
        latency.record_ns(650);
        latency.record_ns(4_000);
        latency.publish();
        assert_eq!(latency.published_count(), 2);
        latency.record_ns(u64::MAX);
        latency.publish();
        assert_eq!(latency.published_count(), 3);
    }

    /// Records every span and event it sees as one line of target, level,
    /// name and fields, so two emissions can be compared exactly.
    mod capture {
        use std::fmt::{self, Write as _};
        use std::sync::{Arc, Mutex};

        use tracing::field::{Field, Visit};
        use tracing::span::{Attributes, Id, Record};
        use tracing::{Event, Metadata, Subscriber};

        #[derive(Clone, Default)]
        pub struct Capture(pub Arc<Mutex<Vec<String>>>);

        struct Fields(String);
        impl Visit for Fields {
            fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
                let _ = write!(self.0, " {}={:?}", field.name(), value);
            }
        }

        impl Capture {
            fn push(&self, line: String) {
                if let Ok(mut lines) = self.0.lock() {
                    lines.push(line);
                }
            }
        }

        impl Subscriber for Capture {
            fn enabled(&self, _: &Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, span: &Attributes<'_>) -> Id {
                let meta = span.metadata();
                let mut fields = Fields(format!(
                    "span {} {} {}",
                    meta.target(),
                    meta.level(),
                    meta.name()
                ));
                span.record(&mut fields);
                self.push(fields.0);
                Id::from_u64(1)
            }
            fn record(&self, _: &Id, _: &Record<'_>) {}
            fn record_follows_from(&self, _: &Id, _: &Id) {}
            fn event(&self, event: &Event<'_>) {
                let meta = event.metadata();
                let mut fields = Fields(format!("event {} {}", meta.target(), meta.level()));
                event.record(&mut fields);
                self.push(fields.0);
            }
            fn enter(&self, _: &Id) {}
            fn exit(&self, _: &Id) {}
        }
    }

    /// The span `#[instrument(skip_all, fields(security_id = ...))]` opened on
    /// `place_order` and the `info!` it wrote inline, both verbatim from the
    /// pre-2026-10-30 engine (the oracle for the helper thread's line).
    fn legacy_inline_line(
        paper_order_id: &str,
        security_id: u64,
        transaction_type: TransactionType,
        quantity: i64,
        price: f64,
    ) {
        let _span = tracing::info_span!(
            target: "tickvault_trading::oms::engine",
            "place_order",
            security_id = security_id
        )
        .entered();
        info!(
            target: "tickvault_trading::oms::engine",
            order_id = %paper_order_id,
            security_id = security_id,
            transaction_type = %transaction_type.as_str(),
            quantity = quantity,
            price = price,
            "PAPER TRADE: order simulated (no HTTP call)"
        );
    }

    fn captured(emit: impl FnOnce()) -> Vec<String> {
        let capture = capture::Capture::default();
        let lines = std::sync::Arc::clone(&capture.0);
        tracing::subscriber::with_default(capture, emit);
        lines.lock().map(|l| l.clone()).unwrap_or_default()
    }

    #[test]
    fn test_helper_line_is_identical_to_the_old_inline_line() {
        let cases = [
            (7_u64, 13_u64, TransactionType::Buy, 50_i64, 245.5_f64),
            (u64::MAX, 0, TransactionType::Sell, -1, 0.0),
            (1, u64::MAX, TransactionType::Buy, i64::MAX, f64::MAX),
        ];
        for (counter, security_id, side, quantity, price) in cases {
            let old = captured(|| {
                legacy_inline_line(
                    &format!("PAPER-{counter}"),
                    security_id,
                    side,
                    quantity,
                    price,
                );
            });
            let new = captured(|| {
                write_paper_trade_line(&PaperTradeRecord {
                    paper_order_counter: counter,
                    security_id,
                    transaction_type: side,
                    quantity,
                    price,
                });
            });
            assert_eq!(old.len(), 2, "one span and one event: {old:?}");
            assert_eq!(new, old);
        }
    }

    #[test]
    fn test_record_is_fixed_size_and_copy() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<PaperTradeRecord>();
        assert!(std::mem::size_of::<PaperTradeRecord>() <= 48);
    }
}
